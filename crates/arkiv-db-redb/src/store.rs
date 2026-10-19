//! [`RedbStore`]: the versioned storage contract over one redb file.

use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions, TryLockError},
    io,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard, PoisonError, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use alloy_primitives::keccak256;
use arkiv_db_storage::versioned::{
    BackendStats, CommitBatch, CommitOutcome, DatabaseId, Head, Health, ObjectId, ObjectReader,
    SpaceId, StatsRecorder, StorageError, StorageErrorKind, StoreStats, Table, VersionedStore,
    head_mismatch, object_conflict, record_conflict,
};
use redb::{
    Builder, Database, Durability, ReadableDatabase, ReadableTable, ReadableTableMetadata,
    StorageBackend, TableDefinition, WriteTransaction, backends::FileBackend,
};

use crate::{
    counting::{CountingBackend, IoProbe},
    error::{Fault, latches},
    metrics::{OperationTiming, OperationTimingSnapshot},
};

/// The version of this store's table layout, kept under `store/format`.
pub const STORE_FORMAT_VERSION: u32 = 1;

/// redb page-cache size used unless configured otherwise: 8 MiB.
///
/// Small on purpose: correctness must not depend on the cache, and the
/// experiment measures behaviour with little of the file cached. redb's own
/// default is 1 GiB.
pub const DEFAULT_CACHE_SIZE: usize = 8 << 20;

const FORMAT_KEY: &str = "store/format";
const DATABASE_ID_KEY: &str = "store/database_id";
const HEAD_KEY: &str = "store/head";

const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
const BLOCKS: TableDefinition<u64, &[u8]> = TableDefinition::new("blocks");
const MANIFESTS: TableDefinition<u64, &[u8]> = TableDefinition::new("manifests");
const INCLUSIONS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("inclusions");
const BLOCK_INDEX: TableDefinition<&[u8; 32], u64> = TableDefinition::new("block_index");

/// The redb table holding the objects of `space`: `objects_catalog`,
/// `objects_host`, or `objects_ns_` followed by the namespace id as 16
/// lowercase hex digits.
pub fn object_table_name(space: SpaceId) -> String {
    match space {
        SpaceId::Catalog => "objects_catalog".to_owned(),
        SpaceId::Host => "objects_host".to_owned(),
        SpaceId::Namespace(id) => format!("objects_ns_{id:016x}"),
    }
}

fn object_table(name: &str) -> TableDefinition<'_, &'static [u8; 32], &'static [u8]> {
    TableDefinition::new(name)
}

/// Settings of a [`RedbStore`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RedbConfig {
    /// redb page-cache size in bytes ([`DEFAULT_CACHE_SIZE`] by default).
    pub cache_size: usize,
    /// Whether commits save redb's allocator state (quick repair, with
    /// two-phase commit). On by default and required for the store's
    /// recovery claims; turning it off exists only to measure the
    /// difference.
    pub quick_repair: bool,
}

impl Default for RedbConfig {
    fn default() -> Self {
        Self {
            cache_size: DEFAULT_CACHE_SIZE,
            quick_repair: true,
        }
    }
}

impl RedbConfig {
    /// The default configuration with another cache size.
    pub fn with_cache_size(cache_size: usize) -> Self {
        Self {
            cache_size,
            ..Self::default()
        }
    }
}

/// What the last open (at creation or by `recover`) did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OpenReport {
    /// redb ran its full repair (walking the whole file) instead of loading a
    /// saved allocator state.
    pub full_repair: bool,
    /// The store initialized an empty file.
    pub initialized: bool,
    /// Wall-clock time of the open, repair and metadata check included.
    pub duration: Duration,
    /// Backend I/O during the open.
    pub backend: BackendStats,
    /// Store-level reads the open performed (metadata entries).
    pub meta_reads: u32,
}

/// Physical shape of the file, for isolation measurements.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LayoutStats {
    /// Height of redb's table directory: the B-tree from table names to
    /// table roots, which every table open descends.
    pub directory_height: u32,
    /// Number of tables in the file.
    pub tables: u64,
    /// Pages allocated in the file.
    pub allocated_pages: u64,
    /// Bytes per page.
    pub page_size: usize,
    /// Per requested space: its table's shape (absent if it has no table).
    pub spaces: BTreeMap<SpaceId, TableLayout>,
}

/// Physical shape of one object table.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TableLayout {
    /// Pages from the table root to its deepest leaf.
    pub height: u32,
    /// Leaf pages.
    pub leaf_pages: u64,
    /// Branch pages.
    pub branch_pages: u64,
    /// Key and value bytes stored.
    pub stored_bytes: u64,
    /// Entries.
    pub entries: u64,
}

/// Aggregate statistics of user tables in a single committed snapshot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DataStats {
    /// Number of user tables.
    pub tables: u64,
    /// Number of key/value entries.
    pub entries: u64,
    /// Encoded key and value bytes, including application indexes and history.
    pub stored_bytes: u64,
    /// Table B-tree indexing and metadata bytes.
    pub metadata_bytes: u64,
    /// Unused bytes within table pages (not free database pages).
    pub fragmented_bytes: u64,
    /// Table leaf pages.
    pub leaf_pages: u64,
    /// Table branch pages.
    pub branch_pages: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OpenMode {
    /// Initialize an empty file; open an existing store.
    Create,
    /// Open an existing store only.
    Existing,
}

/// What checking or creating the store metadata of an opened database found.
struct Prepared {
    id: DatabaseId,
    /// Metadata entries read.
    meta_reads: u32,
    initialized: bool,
}

/// Checks or creates the store metadata of an opened database ([`prepare`]).
type Prepare<'a> = &'a dyn Fn(&Database) -> Result<Prepared, Fault>;

/// Opens the database and prepares its metadata, in the order crash safety
/// requires (see [`create_file`]).
type Opener = Box<
    dyn Fn(OpenMode, &Builder, &Arc<IoProbe>, Prepare<'_>) -> Result<(Database, Prepared), Fault>
        + Send
        + Sync,
>;

struct Slot {
    db: Option<Database>,
    epoch: u64,
}

/// The versioned storage contract over one redb file (see the crate
/// documentation for the layout and the mechanism behind each guarantee).
pub struct RedbStore {
    opener: Opener,
    config: RedbConfig,
    database_id: DatabaseId,
    slot: RwLock<Slot>,
    failure: Mutex<Option<StorageError>>,
    /// Whether `failure` holds an error: lets the healthy path of
    /// [`VersionedStore::health`] and every read skip the mutex. Set after
    /// the error is stored and cleared before it is removed, both under the
    /// mutex, so a set flag always finds the latched error.
    failed: AtomicBool,
    writer: Mutex<()>,
    stats: StatsRecorder,
    timings: [OperationTiming; 5],
    probe: Arc<IoProbe>,
    repairs: Arc<AtomicU64>,
    last_open: Mutex<OpenReport>,
}

impl std::fmt::Debug for RedbStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedbStore")
            .field("database_id", &self.database_id)
            .field("config", &self.config)
            .field("epoch", &self.epoch())
            .field("health", &self.health())
            .finish_non_exhaustive()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn closed() -> StorageError {
    StorageError::new(StorageErrorKind::Closed, "the store has no open database")
}

/// The temporary file [`RedbStore::create`] builds a new store in: the
/// store's path with `.init` appended to its file name.
///
/// It exists only while a `create` runs or after one was interrupted; the
/// next `create` of the same path reuses and replaces it.
pub fn initialization_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".init");
    path.with_file_name(name)
}

impl RedbStore {
    /// Opens the store in the file at `path`, creating and initializing the
    /// file if it does not exist or is empty.
    ///
    /// A new store is built in [`initialization_path`]`(path)` — redb's file
    /// creation and the store's initialization commit, both synced — then
    /// renamed to `path`, and the directory is synced. A crash during
    /// creation therefore never leaves a partial file under `path` (redb
    /// writes its magic number last, and a file without it is refused); it
    /// leaves at most the temporary file, which the next `create` discards.
    /// A concurrent `create` of the same path is `Locked` or opens the store
    /// the other one finished.
    ///
    /// A file holding a redb database without this store's metadata is
    /// initialized in place, in one redb transaction; a file with another
    /// store format, or one that is not a redb database, is `Incompatible`,
    /// and a file another process has open is `Locked`.
    pub fn create(path: impl AsRef<Path>, config: RedbConfig) -> Result<Self, StorageError> {
        Self::open_with(
            path_opener(path.as_ref().to_path_buf()),
            OpenMode::Create,
            config,
        )
    }

    /// Opens an existing store at `path`: a missing or empty file is
    /// `Missing`, a file without this store's metadata `Incompatible`.
    pub fn open(path: impl AsRef<Path>, config: RedbConfig) -> Result<Self, StorageError> {
        Self::open_with(
            path_opener(path.as_ref().to_path_buf()),
            OpenMode::Existing,
            config,
        )
    }

    /// Opens a store on a custom redb backend, creating it if the backend is
    /// empty. `open_backend` is called for the first open and again by every
    /// [`recover`](VersionedStore::recover); each call must return a handle
    /// on the same underlying storage.
    ///
    /// An empty backend is initialized in place: a crash during that first
    /// initialization can leave a backend that redb refuses (`Incompatible`),
    /// because redb writes its magic number last. [`RedbStore::create`]
    /// avoids this for files.
    pub fn with_backend<B, F>(open_backend: F, config: RedbConfig) -> Result<Self, StorageError>
    where
        B: StorageBackend,
        F: Fn() -> io::Result<B> + Send + Sync + 'static,
    {
        let opener: Opener = Box::new(move |mode, builder, probe, prepare| {
            let backend = open_backend()?;
            if mode == OpenMode::Existing && backend.len()? == 0 {
                return Err(Fault::new(
                    StorageErrorKind::Missing,
                    "the backend holds no database",
                ));
            }
            let db =
                builder.create_with_backend(CountingBackend::with_probe(backend, probe.clone()))?;
            let prepared = prepare(&db)?;
            Ok((db, prepared))
        });
        Self::open_with(opener, OpenMode::Create, config)
    }

    fn open_with(opener: Opener, mode: OpenMode, config: RedbConfig) -> Result<Self, StorageError> {
        let probe = IoProbe::new();
        let repairs = Arc::new(AtomicU64::new(0));
        let (db, database_id, report) = open_database(&opener, mode, &config, &probe, &repairs)?;
        Ok(Self {
            opener,
            config,
            database_id,
            slot: RwLock::new(Slot {
                db: Some(db),
                epoch: 0,
            }),
            failure: Mutex::new(None),
            failed: AtomicBool::new(false),
            writer: Mutex::new(()),
            stats: StatsRecorder::new(),
            timings: Default::default(),
            probe,
            repairs,
            last_open: Mutex::new(report),
        })
    }

    /// The configuration.
    pub fn config(&self) -> RedbConfig {
        self.config
    }

    /// What the most recent successful open did.
    pub fn last_open(&self) -> OpenReport {
        *lock(&self.last_open)
    }

    /// The backend counters and kill switch of this store.
    pub fn probe(&self) -> &Arc<IoProbe> {
        &self.probe
    }

    /// Timings of all attempted store operations, including validation failures.
    pub fn operation_timings(&self) -> [OperationTimingSnapshot; 5] {
        std::array::from_fn(|i| {
            self.timings[i].snapshot(["read_object", "read_record", "head", "commit", "recover"][i])
        })
    }

    /// Aggregates current user table statistics using only a read transaction.
    /// This walks table pages, so sample sparingly on a background thread. It
    /// never takes the writer lock, but pins its snapshot until the walk ends.
    /// Excludes free pages, the table directory and allocator metadata.
    pub fn data_stats(&self) -> Result<DataStats, StorageError> {
        self.with_db(|db| {
            let read = db.begin_read()?;
            let mut totals = DataStats::default();
            for handle in read.list_tables()? {
                let table = read.open_untyped_table(handle)?;
                let stats = table.stats()?;
                totals.tables += 1;
                totals.entries += table.len()?;
                totals.stored_bytes += stats.stored_bytes();
                totals.metadata_bytes += stats.metadata_bytes();
                totals.fragmented_bytes += stats.fragmented_bytes();
                totals.leaf_pages += stats.leaf_pages();
                totals.branch_pages += stats.branch_pages();
            }
            Ok(totals)
        })
    }

    /// How many times the database handle has been replaced by `recover`.
    pub fn epoch(&self) -> u64 {
        self.slot
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .epoch
    }

    /// The latched failure. The flag is read first, so the healthy path
    /// takes no lock; the mutex is consulted only once a failure latched.
    fn failure(&self) -> Option<StorageError> {
        if !self.failed.load(Ordering::Acquire) {
            return None;
        }
        lock(&self.failure).clone()
    }

    /// Latches `error` unless a failure is already latched.
    fn latch(&self, error: &StorageError) {
        let mut failure = lock(&self.failure);
        failure.get_or_insert_with(|| error.clone());
        self.failed.store(true, Ordering::Release);
    }

    /// Replaces the latched failure (recovery's outcome).
    fn set_failure(&self, error: Option<StorageError>) {
        let mut failure = lock(&self.failure);
        self.failed.store(error.is_some(), Ordering::Release);
        *failure = error;
    }

    /// Latches `error` if it is an I/O failure and nothing latched yet.
    fn observe(&self, error: &StorageError) {
        if latches(error) {
            self.latch(error);
        }
    }

    /// Runs `read` on the live database, unless a failure has latched.
    fn with_db<T>(
        &self,
        read: impl FnOnce(&Database) -> Result<T, Fault>,
    ) -> Result<T, StorageError> {
        if let Some(error) = self.failure() {
            return Err(error);
        }
        let slot = self.slot.read().unwrap_or_else(PoisonError::into_inner);
        let Some(db) = slot.db.as_ref() else {
            return Err(self.failure().unwrap_or_else(closed));
        };
        read(db).map_err(|fault| {
            self.observe(&fault.0);
            fault.0
        })
    }

    /// Measures the physical layout: the table directory height and the
    /// shape of each requested space's table. Diagnostics only; it runs a
    /// read transaction and an aborted write transaction.
    pub fn layout(&self, spaces: &[SpaceId]) -> Result<LayoutStats, StorageError> {
        let _writer = lock(&self.writer);
        self.with_db(|db| {
            let read = db.begin_read()?;
            let mut deepest_table = 0;
            let mut tables = 0;
            for handle in read.list_tables()? {
                let table = read.open_untyped_table(handle)?;
                deepest_table = deepest_table.max(table.stats()?.tree_height());
                tables += 1;
            }
            let mut per_space = BTreeMap::new();
            for space in spaces {
                let name = object_table_name(*space);
                match read.open_table(object_table(&name)) {
                    Ok(table) => {
                        let stats = table.stats()?;
                        per_space.insert(
                            *space,
                            TableLayout {
                                height: stats.tree_height(),
                                leaf_pages: stats.leaf_pages(),
                                branch_pages: stats.branch_pages(),
                                stored_bytes: stats.stored_bytes(),
                                entries: table.len()?,
                            },
                        );
                    }
                    Err(redb::TableError::TableDoesNotExist(_)) => {}
                    Err(error) => return Err(error.into()),
                }
            }
            drop(read);
            let write = db.begin_write()?;
            let stats = write.stats()?;
            write.abort()?;
            Ok(LayoutStats {
                // redb reports the directory height plus the deepest table.
                directory_height: stats.tree_height() - deepest_table,
                tables,
                allocated_pages: stats.allocated_pages(),
                page_size: stats.page_size(),
                spaces: per_space,
            })
        })
    }

    fn commit_batch(
        &self,
        batch: &CommitBatch,
        headless: bool,
    ) -> (CommitOutcome, Vec<(SpaceId, usize)>, u64) {
        let _writer = lock(&self.writer);
        // A latched failure answers every call, even a malformed batch.
        if let Some(error) = self.failure() {
            return (CommitOutcome::NotCommitted(error), Vec::new(), 0);
        }
        if let Err(error) = batch.validate() {
            return (CommitOutcome::NotCommitted(error), Vec::new(), 0);
        }
        let slot = self.slot.read().unwrap_or_else(PoisonError::into_inner);
        let Some(db) = slot.db.as_ref() else {
            let error = self.failure().unwrap_or_else(closed);
            return (CommitOutcome::NotCommitted(error), Vec::new(), 0);
        };
        let staged = (|| -> Result<_, Fault> {
            let mut txn = db.begin_write()?;
            txn.set_durability(Durability::Immediate)?;
            txn.set_quick_repair(self.config.quick_repair);
            let (objects, records) = stage(&txn, batch, headless)?;
            Ok((txn, objects, records))
        })();
        let (txn, objects, records) = match staged {
            Ok(staged) => staged,
            Err(fault) => {
                // The transaction was dropped, which aborts it.
                self.observe(&fault.0);
                return (CommitOutcome::NotCommitted(fault.0), Vec::new(), 0);
            }
        };
        match txn.commit() {
            Ok(()) => (CommitOutcome::Committed, objects, records),
            Err(redb::CommitError::TransactionPoisoned) => {
                // redb rolled the transaction back.
                let error = StorageError::new(
                    StorageErrorKind::Io,
                    "redb transaction poisoned and rolled back",
                );
                self.observe(&error);
                (CommitOutcome::NotCommitted(error), Vec::new(), 0)
            }
            Err(error) => {
                // redb refuses further writes on this handle whatever the
                // cause, so the store fails until it is reopened.
                let error = Fault::from(error).0;
                self.latch(&error);
                (CommitOutcome::Indeterminate(error), Vec::new(), 0)
            }
        }
    }
}

/// Stages a whole batch in `txn`: head check, objects, records, new head.
fn stage(
    txn: &WriteTransaction,
    batch: &CommitBatch,
    headless: bool,
) -> Result<(Vec<(SpaceId, usize)>, u64), Fault> {
    let mut meta = txn.open_table(META)?;
    let current = match meta.get(HEAD_KEY)? {
        Some(guard) => Some(Head::decode(guard.value())?),
        None => None,
    };
    if current != batch.expected_head {
        return Err(head_mismatch(batch.expected_head, current).into());
    }

    let mut by_space: BTreeMap<SpaceId, Vec<(&ObjectId, &[u8])>> = BTreeMap::new();
    for (space, id, bytes) in &batch.objects {
        by_space.entry(*space).or_default().push((id, bytes));
    }
    let mut new_objects = Vec::new();
    for (space, objects) in by_space {
        let name = object_table_name(space);
        let mut table = txn.open_table(object_table(&name))?;
        for (id, bytes) in objects {
            let existing = table.get(id)?.map(|stored| stored.value() == bytes);
            match existing {
                Some(true) => {}
                Some(false) => return Err(object_conflict(space, id).into()),
                None => {
                    table.insert(id, bytes)?;
                    new_objects.push((space, bytes.len()));
                }
            }
        }
    }

    let mut new_records = 0;
    for table in Table::ALL {
        let mut entries = batch
            .records
            .iter()
            .filter(|(of, _, _)| *of == table)
            .map(|(_, key, value)| (key.as_slice(), value.as_slice()))
            .peekable();
        if entries.peek().is_none() {
            continue;
        }
        // Shapes were checked by `CommitBatch::validate`.
        match table {
            Table::Meta => {
                for (key, value) in entries {
                    let name = std::str::from_utf8(key).map_err(|_| shape(table))?;
                    let existing = meta.get(name)?.map(|stored| stored.value() == value);
                    new_records +=
                        insert_only(existing, table, key, || meta.insert(name, value).map(drop))?;
                }
            }
            Table::Blocks | Table::Manifests => {
                let mut records = txn.open_table(if table == Table::Blocks {
                    BLOCKS
                } else {
                    MANIFESTS
                })?;
                for (key, value) in entries {
                    let height = u64::from_be_bytes(key.try_into().map_err(|_| shape(table))?);
                    let existing = records.get(height)?.map(|stored| stored.value() == value);
                    new_records += insert_only(existing, table, key, || {
                        records.insert(height, value).map(drop)
                    })?;
                }
            }
            Table::Inclusions => {
                let mut records = txn.open_table(INCLUSIONS)?;
                for (key, value) in entries {
                    let existing = records.get(key)?.map(|stored| stored.value() == value);
                    new_records += insert_only(existing, table, key, || {
                        records.insert(key, value).map(drop)
                    })?;
                }
            }
            Table::BlockIndex => {
                let mut records = txn.open_table(BLOCK_INDEX)?;
                for (key, value) in entries {
                    let hash: &[u8; 32] = key.try_into().map_err(|_| shape(table))?;
                    let height = u64::from_be_bytes(value.try_into().map_err(|_| shape(table))?);
                    let existing = records.get(hash)?.map(|stored| stored.value() == height);
                    new_records += insert_only(existing, table, key, || {
                        records.insert(hash, height).map(drop)
                    })?;
                }
            }
        }
    }

    if !headless {
        meta.insert(HEAD_KEY, batch.new_head.encode().as_slice())?;
    }
    Ok((new_objects, new_records))
}

fn shape(table: Table) -> Fault {
    Fault::new(
        StorageErrorKind::Incompatible,
        format!("{} record has the wrong shape", table.name()),
    )
}

/// Applies the insert-only rule given whether an existing record matches:
/// inserts when absent (returning 1), skips an identical record (0) and
/// refuses a different one.
fn insert_only(
    existing: Option<bool>,
    table: Table,
    key: &[u8],
    insert: impl FnOnce() -> Result<(), redb::StorageError>,
) -> Result<u64, Fault> {
    match existing {
        Some(true) => Ok(0),
        Some(false) => Err(record_conflict(table, key).into()),
        None => {
            insert()?;
            Ok(1)
        }
    }
}

fn path_opener(path: PathBuf) -> Opener {
    Box::new(move |mode, builder, probe, prepare| {
        if mode == OpenMode::Create
            && !has_contents(&path)?
            && let Some(created) = create_file(&path, builder, probe, prepare)?
        {
            return Ok(created);
        }
        // An existing store, or one another `create` just finished.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|error| {
                if error.kind() == io::ErrorKind::NotFound {
                    Fault::new(
                        StorageErrorKind::Missing,
                        format!("no database at {}", path.display()),
                    )
                } else {
                    error.into()
                }
            })?;
        if file.metadata()?.len() == 0 {
            return Err(Fault::new(
                StorageErrorKind::Missing,
                format!("{} is empty", path.display()),
            ));
        }
        let backend = FileBackend::new(file)?;
        let db =
            builder.create_with_backend(CountingBackend::with_probe(backend, probe.clone()))?;
        let prepared = prepare(&db)?;
        Ok((db, prepared))
    })
}

/// Whether `path` names a non-empty file (or anything else that is not an
/// absent or empty file, which the open then reports).
fn has_contents(path: &Path) -> io::Result<bool> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(metadata.len() > 0),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Creates a new store file at `path` without ever exposing a partial file
/// under that name; `None` if another process created it meanwhile.
///
/// redb initializes a file in place in several synced steps and writes its
/// magic number last, and the store's metadata commit follows. A power cut
/// in between leaves a non-empty file that redb refuses to open. So the
/// store is built in [`initialization_path`]: redb's creation and the
/// store's initialization commit, both synced before they return, and only
/// then is the file renamed to `path` and the directory synced. Before the
/// rename `path` does not exist, and the next `create` truncates the stale
/// temporary file and starts over; after it, `path` names a complete store.
///
/// The temporary file is locked (`flock`) from before it is truncated until
/// the returned handle closes, so concurrent `create`s of one path cannot
/// initialize the same file twice: a second one is `Locked`, or finds `path`
/// created and opens it. Only the lock holder renames or removes the
/// temporary name.
fn create_file(
    path: &Path,
    builder: &Builder,
    probe: &Arc<IoProbe>,
    prepare: Prepare<'_>,
) -> Result<Option<(Database, Prepared)>, Fault> {
    let temp = initialization_path(path);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&temp)?;
    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            return Err(Fault::new(
                StorageErrorKind::Locked,
                format!("another process is creating {}", path.display()),
            ));
        }
        // Without file locks (some file systems) creation is unguarded
        // against a concurrent `create`, as redb's own open is.
        Err(TryLockError::Error(error)) if error.kind() == io::ErrorKind::Unsupported => {}
        Err(TryLockError::Error(error)) => return Err(error.into()),
    }
    if has_contents(path)? {
        // A `create` that held the lock before us finished. A temporary file
        // we created after its rename is ours to remove.
        if names_file(&temp, &file) {
            fs::remove_file(&temp)?;
        }
        return Ok(None);
    }
    // Whatever an interrupted initialization left is discarded. The
    // truncation need not be durable: a crash leaves a stale file again.
    file.set_len(0)?;
    // redb takes its own locks on the same open file, which already holds
    // ours; they are released together when the database closes.
    let backend = FileBackend::new(file)?;
    let db = builder.create_with_backend(CountingBackend::with_probe(backend, probe.clone()))?;
    let prepared = prepare(&db)?;
    fs::rename(&temp, path)?;
    sync_parent_directory(path)?;
    Ok(Some((db, prepared)))
}

/// Whether `path` names the open `file` (same device and inode).
#[cfg(unix)]
fn names_file(path: &Path, file: &File) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (fs::metadata(path), file.metadata()) {
        (Ok(named), Ok(open)) => named.dev() == open.dev() && named.ino() == open.ino(),
        _ => false,
    }
}

/// Without inode numbers a leftover temporary file is kept (it is empty).
#[cfg(not(unix))]
fn names_file(_path: &Path, _file: &File) -> bool {
    false
}

/// Makes a name created or renamed in `path`'s directory durable by syncing
/// the directory.
#[cfg(unix)]
fn sync_parent_directory(path: &Path) -> io::Result<()> {
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    File::open(parent)?.sync_all()
}

/// Directories cannot be opened for syncing on this platform; the rename's
/// durability is left to the file system.
#[cfg(not(unix))]
fn sync_parent_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// Opens the database, runs redb's repair if needed, and checks or creates
/// the store metadata.
fn open_database(
    opener: &Opener,
    mode: OpenMode,
    config: &RedbConfig,
    probe: &Arc<IoProbe>,
    repairs: &Arc<AtomicU64>,
) -> Result<(Database, DatabaseId, OpenReport), StorageError> {
    let started = Instant::now();
    let before = probe.stats();
    let repairs_before = repairs.load(Ordering::SeqCst);
    let mut builder = Builder::new();
    builder.set_cache_size(config.cache_size);
    let counter = Arc::clone(repairs);
    builder.set_repair_callback(move |_| {
        counter.fetch_add(1, Ordering::SeqCst);
    });
    let check = |db: &Database| prepare(db, mode, config);
    let (db, prepared) = opener(mode, &builder, probe, &check)?;
    let report = OpenReport {
        full_repair: repairs.load(Ordering::SeqCst) > repairs_before,
        initialized: prepared.initialized,
        duration: started.elapsed(),
        backend: probe.stats().since(&before),
        meta_reads: prepared.meta_reads,
    };
    Ok((db, prepared.id, report))
}

/// Reads and checks the store metadata, or initializes an empty database in
/// one durable, quick-repair transaction.
fn prepare(db: &Database, mode: OpenMode, config: &RedbConfig) -> Result<Prepared, Fault> {
    {
        let read = db.begin_read()?;
        match read.open_table(META) {
            Ok(meta) => {
                // These fixed-size adapter records are checked directly
                // from guards; malformed lengths never allocate a copy.
                let format = meta.get(FORMAT_KEY)?;
                let id = meta.get(DATABASE_ID_KEY)?;
                return Ok(Prepared {
                    id: check_meta(
                        format.as_ref().map(|value| value.value()),
                        id.as_ref().map(|value| value.value()),
                    )?,
                    meta_reads: 2,
                    initialized: false,
                });
            }
            Err(redb::TableError::TableDoesNotExist(_)) => {}
            Err(error) => return Err(error.into()),
        }
    }
    if mode == OpenMode::Existing {
        return Err(Fault::new(
            StorageErrorKind::Incompatible,
            "the database has no arkiv store metadata",
        ));
    }
    let id = DatabaseId::random();
    let mut txn = db.begin_write()?;
    txn.set_durability(Durability::Immediate)?;
    txn.set_quick_repair(config.quick_repair);
    {
        let mut meta = txn.open_table(META)?;
        meta.insert(FORMAT_KEY, STORE_FORMAT_VERSION.to_be_bytes().as_slice())?;
        meta.insert(DATABASE_ID_KEY, id.0.as_slice())?;
        txn.open_table(BLOCKS)?;
        txn.open_table(MANIFESTS)?;
        txn.open_table(INCLUSIONS)?;
        txn.open_table(BLOCK_INDEX)?;
        txn.open_table(object_table(&object_table_name(SpaceId::Catalog)))?;
        txn.open_table(object_table(&object_table_name(SpaceId::Host)))?;
    }
    txn.commit()?;
    Ok(Prepared {
        id,
        meta_reads: 0,
        initialized: true,
    })
}

fn check_meta(format: Option<&[u8]>, id: Option<&[u8]>) -> Result<DatabaseId, Fault> {
    let format = format
        .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok())
        .map(u32::from_be_bytes);
    if format != Some(STORE_FORMAT_VERSION) {
        return Err(Fault::new(
            StorageErrorKind::Incompatible,
            format!("store format {format:?}, expected {STORE_FORMAT_VERSION}"),
        ));
    }
    let id = id
        .and_then(|bytes| <[u8; 16]>::try_from(bytes).ok())
        .ok_or_else(|| {
            Fault::new(
                StorageErrorKind::Corrupt,
                "database id missing or malformed",
            )
        })?;
    Ok(DatabaseId(id))
}

impl ObjectReader for RedbStore {
    fn read_object(&self, space: SpaceId, id: &ObjectId) -> Result<Option<Vec<u8>>, StorageError> {
        self.read_object_bounded(space, id, usize::MAX)
    }

    fn read_object_bounded(
        &self,
        space: SpaceId,
        id: &ObjectId,
        max_bytes: usize,
    ) -> Result<Option<Vec<u8>>, StorageError> {
        let mut timing = self.timings[0].start();
        let found = self.with_db(|db| {
            let txn = db.begin_read()?;
            let name = object_table_name(space);
            let table = match txn.open_table(object_table(&name)) {
                Ok(table) => table,
                // A namespace's table is created by its first object; the
                // catalog and host tables exist from initialization, so
                // their absence is damage (`Corrupt`, from the conversion).
                Err(redb::TableError::TableDoesNotExist(_))
                    if matches!(space, SpaceId::Namespace(_)) =>
                {
                    return Ok(None);
                }
                Err(error) => return Err(error.into()),
            };
            table
                .get(id)?
                .map(|value| copy_bounded(value.value(), max_bytes))
                .transpose()
        })?;
        if let Some(bytes) = &found
            && keccak256(bytes).0 != *id
        {
            return Err(StorageError::new(
                StorageErrorKind::Corrupt,
                format!("object in {space:?} does not hash to its id"),
            ));
        }
        self.stats.object_read(space, found.as_ref().map(Vec::len));
        timing.success();
        Ok(found)
    }
}

impl VersionedStore for RedbStore {
    fn database_id(&self) -> DatabaseId {
        self.database_id
    }

    fn head(&self) -> Result<Option<Head>, StorageError> {
        let mut timing = self.timings[2].start();
        let head = self.with_db(|db| {
            let txn = db.begin_read()?;
            let meta = txn.open_table(META)?;
            let head = match meta.get(HEAD_KEY)? {
                Some(value) => Some(Head::decode(value.value())?),
                None => None,
            };
            Ok(head)
        })?;
        self.stats.head_read();
        timing.success();
        Ok(head)
    }

    fn read_record(&self, table: Table, key: &[u8]) -> Result<Option<Vec<u8>>, StorageError> {
        self.read_record_bounded(table, key, usize::MAX)
    }

    fn read_record_bounded(
        &self,
        table: Table,
        key: &[u8],
        max_bytes: usize,
    ) -> Result<Option<Vec<u8>>, StorageError> {
        let mut timing = self.timings[1].start();
        let found = self.with_db(|db| {
            let txn = db.begin_read()?;
            // A key that cannot have the table's shape is absent by
            // construction.
            match table {
                Table::Meta => match std::str::from_utf8(key) {
                    Ok(name) => txn
                        .open_table(META)?
                        .get(name)?
                        .map(|value| copy_bounded(value.value(), max_bytes)),
                    Err(_) => None,
                },
                Table::Blocks | Table::Manifests => match <[u8; 8]>::try_from(key) {
                    Ok(height) => txn
                        .open_table(if table == Table::Blocks {
                            BLOCKS
                        } else {
                            MANIFESTS
                        })?
                        .get(u64::from_be_bytes(height))?
                        .map(|value| copy_bounded(value.value(), max_bytes)),
                    Err(_) => None,
                },
                Table::Inclusions => txn
                    .open_table(INCLUSIONS)?
                    .get(key)?
                    .map(|value| copy_bounded(value.value(), max_bytes)),
                Table::BlockIndex => match <&[u8; 32]>::try_from(key) {
                    Ok(hash) => txn
                        .open_table(BLOCK_INDEX)?
                        .get(hash)?
                        .map(|value| copy_bounded(&value.value().to_be_bytes(), max_bytes)),
                    Err(_) => None,
                },
            }
            .transpose()
        })?;
        self.stats.record_read(found.as_ref().map(Vec::len));
        timing.success();
        Ok(found)
    }

    fn commit(&self, batch: &CommitBatch) -> CommitOutcome {
        let mut timing = self.timings[3].start();
        let (outcome, objects, records) = self.commit_batch(batch, false);
        self.stats.commit(&outcome, &objects, records);
        if matches!(outcome, CommitOutcome::Committed) {
            timing.success();
        }
        outcome
    }

    fn stage_objects(&self, objects: &[(SpaceId, ObjectId, Vec<u8>)]) -> CommitOutcome {
        let mut batch = CommitBatch::new(None, Head::default());
        batch.objects = objects.to_vec();
        let (outcome, objects, records) = self.commit_batch(&batch, true);
        self.stats.commit(&outcome, &objects, records);
        outcome
    }

    fn health(&self) -> Health {
        match self.failure() {
            Some(error) => Health::Failed(error),
            None => Health::Available,
        }
    }

    fn recover(&self) -> Result<Option<Head>, StorageError> {
        let mut timing = self.timings[4].start();
        // Writer first, then the slot: the same order as `commit`. Taking the
        // slot exclusively waits for every in-flight read.
        let _writer = lock(&self.writer);
        let mut slot = self.slot.write().unwrap_or_else(PoisonError::into_inner);
        // Dropping the handle closes redb; after an I/O failure redb skips
        // its clean-close writes and the reopen repairs instead.
        slot.db = None;
        let reopened = open_database(
            &self.opener,
            OpenMode::Existing,
            &self.config,
            &self.probe,
            &self.repairs,
        )
        .and_then(|(db, id, report)| {
            if id == self.database_id {
                Ok((db, report))
            } else {
                Err(StorageError::new(
                    StorageErrorKind::Incompatible,
                    format!(
                        "reopened database {id} is not this store's database {}",
                        self.database_id
                    ),
                ))
            }
        });
        match reopened {
            Ok((db, report)) => {
                slot.db = Some(db);
                slot.epoch += 1;
                *lock(&self.last_open) = report;
                self.set_failure(None);
            }
            Err(error) => {
                self.set_failure(Some(error.clone()));
                return Err(error);
            }
        }
        drop(slot);
        let head = self.head()?;
        self.stats.recovered();
        timing.success();
        Ok(head)
    }

    fn stats(&self) -> StoreStats {
        self.stats.snapshot(Some(self.probe.stats()))
    }
}

/// The redb value guard is still borrowed here: reject its length before
/// making the returned value copy. Pager allocations are outside this cap.
fn copy_bounded(bytes: &[u8], max_bytes: usize) -> Result<Vec<u8>, Fault> {
    if bytes.len() > max_bytes {
        return Err(Fault::new(
            StorageErrorKind::Corrupt,
            format!(
                "stored value has {} bytes, exceeds read limit {max_bytes}",
                bytes.len()
            ),
        ));
    }
    Ok(bytes.to_vec())
}

#[cfg(test)]
mod metrics_tests {
    use super::*;
    use crate::testing::{CrashDisk, DiskOp};

    #[test]
    fn timings_include_bounded_rejections_conflicts_and_backend_failures() {
        let disk = CrashDisk::new();
        let device = disk.clone();
        let store =
            RedbStore::with_backend(move || Ok(device.backend()), RedbConfig::with_cache_size(0))
                .unwrap();
        let head = Head {
            height: 0,
            hash: [1; 32],
        };
        let bytes = vec![42; 1024];
        let id = keccak256(&bytes).0;
        let mut batch = CommitBatch::new(None, head);
        batch.put_object(SpaceId::Host, id, bytes.clone());
        assert!(matches!(store.commit(&batch), CommitOutcome::Committed));
        assert!(matches!(
            store.commit(&batch),
            CommitOutcome::NotCommitted(_)
        ));
        assert!(store.read_object_bounded(SpaceId::Host, &id, 1).is_err());
        assert_eq!(store.read_object(SpaceId::Host, &id).unwrap(), Some(bytes));
        assert!(
            store
                .read_record_bounded(Table::Meta, FORMAT_KEY.as_bytes(), 0)
                .is_err()
        );
        let before = store.probe().operation_timings()[0];
        disk.fail(DiskOp::Read, 0, io::ErrorKind::Other);
        assert!(store.head().is_err());
        let after = store.probe().operation_timings()[0];
        assert_eq!(after.errors, before.errors + 1);
        assert!(after.count > before.count);
        assert_eq!(store.recover().unwrap(), Some(head));
        let timings = store.operation_timings();
        assert_eq!((timings[0].count, timings[0].errors), (2, 1));
        assert_eq!((timings[1].count, timings[1].errors), (1, 1));
        assert_eq!((timings[2].count, timings[2].errors), (2, 1));
        assert_eq!((timings[3].count, timings[3].errors), (2, 1));
        assert_eq!((timings[4].count, timings[4].errors), (1, 0));
    }

    #[test]
    fn data_stats_do_not_write_or_wait_for_writer_lock() {
        let disk = CrashDisk::new();
        let store =
            RedbStore::with_backend(move || Ok(disk.backend()), RedbConfig::default()).unwrap();
        // Holding the writer mutex would deadlock a diagnostic that uses it.
        let _writer = lock(&store.writer);
        let before = store.probe().stats();
        let stats = store.data_stats().unwrap();
        let after = store.probe().stats();
        assert_eq!(stats.tables, 7);
        assert_eq!(stats.entries, 2);
        assert_eq!(
            stats.stored_bytes,
            (FORMAT_KEY.len() + 4 + DATABASE_ID_KEY.len() + 16) as u64
        );
        assert!(stats.metadata_bytes > 0);
        assert!(stats.leaf_pages > 0);
        assert_eq!(after.writes, before.writes);
        assert_eq!(after.syncs, before.syncs);
    }
}
