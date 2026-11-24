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
use arkiv_db_storage::{
    gc::{
        self as collect, GC_STATE_KEY, GC_STORE_FORMAT, GcCommit, GcError, GcReport, GcState,
        ObjectGraph, Seeder, gc_key,
    },
    versioned::{
        BackendStats, CommitBatch, CommitOutcome, DatabaseId, GcDump, Head, Health, ObjectId,
        ObjectReader, RESERVED_META_PREFIX, Relabel, SpaceId, StatsRecorder, StorageError,
        StorageErrorKind, StoreStats, Table, VersionedStore, Written, copy_bounded,
        gc_write_refused, head_mismatch, object_conflict, record_conflict, relabel_mismatch,
        validate_objects,
    },
};
use redb::{
    Builder, Database, Durability, ReadableDatabase, ReadableTable, ReadableTableMetadata,
    StorageBackend, TableDefinition, WriteTransaction, backends::FileBackend,
};

use crate::{
    counting::{CountingBackend, IoProbe},
    error::{Fault, latches},
    gc::{self as tables, TxnGc},
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

pub(crate) const FORMAT_KEY: &str = "store/format";
const DATABASE_ID_KEY: &str = "store/database_id";
const HEAD_KEY: &str = "store/head";

pub(crate) const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
const BLOCKS: TableDefinition<u64, &[u8]> = TableDefinition::new("blocks");
const MANIFESTS: TableDefinition<u64, &[u8]> = TableDefinition::new("manifests");
const INCLUSIONS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("inclusions");
const BLOCK_INDEX: TableDefinition<&[u8; 32], u64> = TableDefinition::new("block_index");

/// The redb table holding the objects of `space`: `objects_catalog`,
/// `objects_host`, or `objects_ns_` followed by the namespace id as 16
/// lowercase hex digits.
pub(crate) fn object_table_name(space: SpaceId) -> String {
    match space {
        SpaceId::Catalog => "objects_catalog".to_owned(),
        SpaceId::Host => "objects_host".to_owned(),
        SpaceId::Namespace(id) => format!("objects_ns_{id:016x}"),
    }
}

pub(crate) fn object_table(name: &str) -> TableDefinition<'_, &'static [u8; 32], &'static [u8]> {
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
    /// The GC state of a store that collects garbage.
    gc: Option<GcState>,
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
    /// The GC state as of the last commit, open or recovery; `None` for a
    /// store that keeps every object.
    gc: Mutex<Option<GcState>>,
    gc_report: Mutex<Option<GcReport>>,
    /// Wall time of the garbage collection work inside commits (see
    /// [`Self::gc_seconds`]).
    gc_nanos: AtomicU64,
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
    /// A new store is built in [`initialization_path`]`(path)` and renamed
    /// to `path` once it is durable, so a crash during creation never leaves
    /// a partial file under `path`; it leaves at most the temporary file,
    /// which the next `create` discards. A concurrent `create` of the same
    /// path is `Locked` or opens the store the other one finished.
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
        let (db, database_id, report, gc) =
            open_database(&opener, mode, &config, &probe, &repairs)?;
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
            gc: Mutex::new(gc),
            gc_report: Mutex::new(None),
            gc_nanos: AtomicU64::new(0),
        })
    }

    /// What the most recent successful open did.
    pub fn last_open(&self) -> OpenReport {
        *lock(&self.last_open)
    }

    /// The backend counters and kill switch of this store.
    pub fn probe(&self) -> &Arc<IoProbe> {
        &self.probe
    }

    /// Timings of reads, `commit` and `relabel` (one series) and `recover`,
    /// including validation failures.
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

    /// Whether the store collects garbage.
    fn collects(&self) -> bool {
        lock(&self.gc).is_some()
    }

    /// Wall time of the garbage collection work inside commits, since the
    /// store was opened: reading the GC state, planning (counting the new
    /// head's references, releasing the old head's, the release cascade and
    /// choosing what to delete) and applying the plan's deletions and GC
    /// table changes. Failed and retried attempts count.
    ///
    /// It excludes what a store without garbage collection does too:
    /// checking and storing the batch's own objects and records. It also
    /// excludes redb's commit (writing the dirty pages and the durability
    /// barriers), where the pages the collection dirtied are written
    /// together with the batch's and cannot be told apart. Compare it with
    /// the `commit` operation timing, which is the whole call.
    pub fn gc_seconds(&self) -> f64 {
        self.gc_nanos.load(Ordering::Relaxed) as f64 / 1e9
    }

    /// Per user table: name, entries, stored key and value bytes, and the
    /// bytes of its pages (stored, metadata and unused bytes within them). Walks every table in one read transaction, like
    /// [`Self::data_stats`]: offline diagnostics only.
    pub fn table_stats(&self) -> Result<Vec<(String, u64, u64, u64)>, StorageError> {
        use redb::TableHandle as _;
        self.with_db(|db| {
            let read = db.begin_read()?;
            let mut out = Vec::new();
            for handle in read.list_tables()? {
                let name = handle.name().to_owned();
                let table = read.open_untyped_table(handle)?;
                let stats = table.stats()?;
                let pages =
                    stats.stored_bytes() + stats.metadata_bytes() + stats.fragmented_bytes();
                out.push((name, table.len()?, stats.stored_bytes(), pages));
            }
            Ok(out)
        })
    }

    /// Visits every record of `table` in key order, in one read
    /// transaction, until `visit` returns `false`. Keys have the shape of
    /// [`Table::check_record`] (heights big-endian, `BlockIndex` values as 8
    /// bytes). Offline migrations only: the read transaction pins every page
    /// freed while it runs.
    pub fn for_each_record(
        &self,
        table: Table,
        mut visit: impl FnMut(&[u8], &[u8]) -> Result<bool, StorageError>,
    ) -> Result<(), StorageError> {
        self.with_db(|db| {
            let read = db.begin_read()?;
            match table {
                Table::Meta => {
                    for entry in read.open_table(META)?.iter()? {
                        let (key, value) = entry?;
                        if !visit(key.value().as_bytes(), value.value())? {
                            break;
                        }
                    }
                }
                Table::Blocks | Table::Manifests => {
                    let records = read.open_table(if table == Table::Blocks {
                        BLOCKS
                    } else {
                        MANIFESTS
                    })?;
                    for entry in records.iter()? {
                        let (key, value) = entry?;
                        if !visit(&key.value().to_be_bytes(), value.value())? {
                            break;
                        }
                    }
                }
                Table::Inclusions => {
                    for entry in read.open_table(INCLUSIONS)?.iter()? {
                        let (key, value) = entry?;
                        if !visit(key.value(), value.value())? {
                            break;
                        }
                    }
                }
                Table::BlockIndex => {
                    for entry in read.open_table(BLOCK_INDEX)?.iter()? {
                        let (key, value) = entry?;
                        if !visit(key.value(), &value.value().to_be_bytes())? {
                            break;
                        }
                    }
                }
            }
            Ok(())
        })
    }

    /// Inserts `records` (insert-only rules) without moving the head or
    /// touching garbage-collection state: offline migrations of a stopped
    /// node only, for records no published head depends on yet.
    pub fn insert_records_offline(
        &self,
        records: &[(Table, Vec<u8>, Vec<u8>)],
    ) -> Result<u64, StorageError> {
        self.rewrite_records_offline(&[], records)
    }

    /// [`Self::insert_records_offline`] after removing `remove`, in one
    /// transaction. Returns the records inserted.
    pub fn rewrite_records_offline(
        &self,
        remove: &[(Table, Vec<u8>)],
        records: &[(Table, Vec<u8>, Vec<u8>)],
    ) -> Result<u64, StorageError> {
        let head = self.head()?;
        let mut check = CommitBatch::new(None, Head::default());
        check.records = records.to_vec();
        check.remove_records = remove.to_vec();
        let inserted = std::cell::Cell::new(0);
        match self.transact(
            || check.validate(),
            |txn| {
                let written = stage_removing(txn, head, &[], remove, &[], records, None)?;
                inserted.set(written.records);
                Ok(written)
            },
        ) {
            CommitOutcome::Committed => Ok(inserted.get()),
            CommitOutcome::NotCommitted(error) | CommitOutcome::Indeterminate(error) => Err(error),
        }
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

    /// Runs `apply` in one durable write transaction after `validate`, under
    /// the writer lock, maps redb's outcome to a [`CommitOutcome`] and
    /// counts it.
    fn transact(
        &self,
        validate: impl FnOnce() -> Result<(), StorageError>,
        apply: impl FnOnce(&WriteTransaction) -> Result<Written, Fault>,
    ) -> CommitOutcome {
        let _writer = lock(&self.writer);
        self.stats.write(self.transact_locked(validate, apply))
    }

    fn transact_locked(
        &self,
        validate: impl FnOnce() -> Result<(), StorageError>,
        apply: impl FnOnce(&WriteTransaction) -> Result<Written, Fault>,
    ) -> Result<Written, CommitOutcome> {
        // A latched failure answers every call, even a malformed batch.
        if let Some(error) = self.failure() {
            return Err(CommitOutcome::NotCommitted(error));
        }
        validate().map_err(CommitOutcome::NotCommitted)?;
        let slot = self.slot.read().unwrap_or_else(PoisonError::into_inner);
        let Some(db) = slot.db.as_ref() else {
            let error = self.failure().unwrap_or_else(closed);
            return Err(CommitOutcome::NotCommitted(error));
        };
        let staged = (|| -> Result<_, Fault> {
            let mut txn = db.begin_write()?;
            txn.set_durability(Durability::Immediate)?;
            txn.set_quick_repair(self.config.quick_repair);
            let written = apply(&txn)?;
            Ok((txn, written))
        })();
        let (txn, written) = match staged {
            Ok(staged) => staged,
            Err(fault) => {
                // The transaction was dropped, which aborts it.
                self.observe(&fault.0);
                return Err(CommitOutcome::NotCommitted(fault.0));
            }
        };
        match txn.commit() {
            Ok(()) => Ok(written),
            Err(redb::CommitError::TransactionPoisoned) => {
                // redb rolled the transaction back.
                let error = StorageError::new(
                    StorageErrorKind::Io,
                    "redb transaction poisoned and rolled back",
                );
                self.observe(&error);
                Err(CommitOutcome::NotCommitted(error))
            }
            Err(error) => {
                // redb refuses further writes on this handle whatever the
                // cause, so the store fails until it is reopened.
                let error = Fault::from(error).0;
                self.latch(&error);
                Err(CommitOutcome::Indeterminate(error))
            }
        }
    }
}

/// Stages a write in `txn`: head check, objects, records, and the new head
/// if there is one.
fn stage(
    txn: &WriteTransaction,
    expected_head: Option<Head>,
    objects: &[(SpaceId, ObjectId, Vec<u8>)],
    records: &[(Table, Vec<u8>, Vec<u8>)],
    new_head: Option<Head>,
) -> Result<Written, Fault> {
    stage_removing(txn, expected_head, &[], &[], objects, records, new_head)
}

/// [`stage`] after removing `remove_objects` and `remove_records` (history
/// expiry), in the same transaction.
fn stage_removing(
    txn: &WriteTransaction,
    expected_head: Option<Head>,
    remove_objects: &[(SpaceId, ObjectId)],
    remove_records: &[(Table, Vec<u8>)],
    objects: &[(SpaceId, ObjectId, Vec<u8>)],
    records: &[(Table, Vec<u8>, Vec<u8>)],
    new_head: Option<Head>,
) -> Result<Written, Fault> {
    let mut meta = txn.open_table(META)?;
    let current = match meta.get(HEAD_KEY)? {
        Some(guard) => Some(Head::decode(guard.value())?),
        None => None,
    };
    if current != expected_head {
        return Err(head_mismatch(expected_head, current).into());
    }
    let mut removed = 0u64;
    if !remove_objects.is_empty() {
        let mut by_space: BTreeMap<SpaceId, Vec<&ObjectId>> = BTreeMap::new();
        for (space, id) in remove_objects {
            by_space.entry(*space).or_default().push(id);
        }
        for (space, ids) in by_space {
            let name = object_table_name(space);
            let mut table = txn.open_table(object_table(&name))?;
            for id in ids {
                removed += u64::from(table.remove(id)?.is_some());
            }
        }
    }
    for (table, key) in remove_records {
        // A key that cannot have the table's shape is absent by
        // construction.
        removed += u64::from(match table {
            Table::Meta => match std::str::from_utf8(key) {
                Ok(name) => meta.remove(name)?.is_some(),
                Err(_) => false,
            },
            Table::Blocks | Table::Manifests => match <[u8; 8]>::try_from(key.as_slice()) {
                Ok(height) => txn
                    .open_table(if *table == Table::Blocks {
                        BLOCKS
                    } else {
                        MANIFESTS
                    })?
                    .remove(u64::from_be_bytes(height))?
                    .is_some(),
                Err(_) => false,
            },
            Table::Inclusions => txn
                .open_table(INCLUSIONS)?
                .remove(key.as_slice())?
                .is_some(),
            Table::BlockIndex => match <&[u8; 32]>::try_from(key.as_slice()) {
                Ok(hash) => txn.open_table(BLOCK_INDEX)?.remove(hash)?.is_some(),
                Err(_) => false,
            },
        });
    }

    let mut by_space: BTreeMap<SpaceId, Vec<(&ObjectId, &[u8])>> = BTreeMap::new();
    for (space, id, bytes) in objects {
        by_space.entry(*space).or_default().push((id, bytes));
    }
    let mut written = Written::default();
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
                    written.objects.push((space, bytes.len()));
                }
            }
        }
    }

    for table in Table::ALL {
        let mut entries = records
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
                    written.records +=
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
                    written.records += insert_only(existing, table, key, || {
                        records.insert(height, value).map(drop)
                    })?;
                }
            }
            Table::Inclusions => {
                let mut records = txn.open_table(INCLUSIONS)?;
                for (key, value) in entries {
                    let existing = records.get(key)?.map(|stored| stored.value() == value);
                    written.records += insert_only(existing, table, key, || {
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
                    written.records += insert_only(existing, table, key, || {
                        records.insert(hash, height).map(drop)
                    })?;
                }
            }
        }
    }

    if let Some(head) = new_head {
        meta.insert(HEAD_KEY, head.encode().as_slice())?;
    }
    written.removed = removed;
    Ok(written)
}

/// Checks that the store holds exactly `relabel`'s head and retired records,
/// removes them all, then stages its batch as a first commit, in `txn`.
fn stage_relabel(txn: &WriteTransaction, relabel: &Relabel) -> Result<Written, Fault> {
    {
        let mut meta = txn.open_table(META)?;
        let current = match meta.get(HEAD_KEY)? {
            Some(guard) => Some(Head::decode(guard.value())?),
            None => None,
        };
        if current != Some(relabel.expected_head) {
            return Err(head_mismatch(Some(relabel.expected_head), current).into());
        }
        let retired = |table: Table| {
            relabel
                .retired
                .iter()
                .filter(move |(of, _, _)| *of == table)
                .map(|(_, key, value)| (key.as_slice(), value.as_slice()))
        };
        // The caller's metadata, the store's own records aside, must be
        // exactly the retired set: every name is checked, not only counted.
        let mut names = Vec::new();
        for entry in meta.iter()? {
            let (name, value) = entry?;
            let name = name.value();
            if name.starts_with(RESERVED_META_PREFIX) {
                continue;
            }
            if retired(Table::Meta).all(|(key, _)| key != name.as_bytes()) {
                return Err(relabel_mismatch(format!("unexpected meta record {name}")).into());
            }
            names.push((name.to_owned(), value.value().to_vec()));
        }
        if names.len() != retired(Table::Meta).count() {
            return Err(relabel_mismatch("meta records missing").into());
        }
        for (key, value) in retired(Table::Meta) {
            let name = std::str::from_utf8(key).map_err(|_| shape(Table::Meta))?;
            if !names.iter().any(|(n, v)| n == name && v == value) {
                return Err(relabel_mismatch(format!("meta record {name} differs")).into());
            }
            meta.remove(name)?;
        }
        meta.remove(HEAD_KEY)?;
        for table in [Table::Blocks, Table::Manifests] {
            let mut records = txn.open_table(if table == Table::Blocks {
                BLOCKS
            } else {
                MANIFESTS
            })?;
            if records.len()? != retired(table).count() as u64 {
                return Err(relabel_mismatch(format!("{} record count", table.name())).into());
            }
            for (key, value) in retired(table) {
                let height = u64::from_be_bytes(key.try_into().map_err(|_| shape(table))?);
                if records.remove(height)?.map(|v| v.value() == value) != Some(true) {
                    return Err(relabel_mismatch(format!("{} record differs", table.name())).into());
                }
            }
        }
        let mut inclusions = txn.open_table(INCLUSIONS)?;
        if inclusions.len()? != retired(Table::Inclusions).count() as u64 {
            return Err(relabel_mismatch("inclusions record count").into());
        }
        for (key, value) in retired(Table::Inclusions) {
            if inclusions.remove(key)?.map(|v| v.value() == value) != Some(true) {
                return Err(relabel_mismatch("inclusions record differs").into());
            }
        }
        let mut index = txn.open_table(BLOCK_INDEX)?;
        if index.len()? != retired(Table::BlockIndex).count() as u64 {
            return Err(relabel_mismatch("block_index record count").into());
        }
        for (key, value) in retired(Table::BlockIndex) {
            let hash: &[u8; 32] = key.try_into().map_err(|_| shape(Table::BlockIndex))?;
            let height =
                u64::from_be_bytes(value.try_into().map_err(|_| shape(Table::BlockIndex))?);
            if index.remove(hash)?.map(|v| v.value() == height) != Some(true) {
                return Err(relabel_mismatch("block_index record differs").into());
            }
        }
    }
    // Every table is empty of caller records and the head is gone, so the
    // replacement stages exactly like an initial commit.
    stage_batch(txn, &relabel.batch)
}

fn stage_batch(txn: &WriteTransaction, batch: &CommitBatch) -> Result<Written, Fault> {
    stage_removing(
        txn,
        batch.expected_head,
        &batch.remove_objects,
        &batch.remove_records,
        &batch.objects,
        &batch.records,
        Some(batch.new_head),
    )
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
) -> Result<(Database, DatabaseId, OpenReport, Option<GcState>), StorageError> {
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
    Ok((db, prepared.id, report, prepared.gc))
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
                let (id, collects) = check_meta(
                    format.as_ref().map(|value| value.value()),
                    id.as_ref().map(|value| value.value()),
                )?;
                let gc = if collects {
                    let state = meta.get(GC_STATE_KEY)?.ok_or_else(|| {
                        Fault::new(
                            StorageErrorKind::Corrupt,
                            "a garbage-collecting store without its GC state",
                        )
                    })?;
                    Some(GcState::decode(state.value())?)
                } else {
                    None
                };
                return Ok(Prepared {
                    id,
                    meta_reads: 2 + u32::from(collects),
                    initialized: false,
                    gc,
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
        gc: None,
    })
}

/// The database id, and whether the store collects garbage (format
/// [`GC_STORE_FORMAT`]).
fn check_meta(format: Option<&[u8]>, id: Option<&[u8]>) -> Result<(DatabaseId, bool), Fault> {
    let format = format
        .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok())
        .map(u32::from_be_bytes);
    if format != Some(STORE_FORMAT_VERSION) && format != Some(GC_STORE_FORMAT) {
        return Err(Fault::new(
            StorageErrorKind::Incompatible,
            format!(
                "store format {format:?}, expected {STORE_FORMAT_VERSION} or {GC_STORE_FORMAT}"
            ),
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
    Ok((DatabaseId(id), format == Some(GC_STORE_FORMAT)))
}

/// The head of the store in the file at `path`, read through a read-only
/// open of the file, which never writes to it (opening a [`RedbStore`]
/// does: redb marks the file open and rewrites its header when it closes).
/// For checking a copy whose bytes must stay as they are.
///
/// `Locked` while a store has the file open; an error for a file that was
/// not closed cleanly, which only a writable open can repair, and for a
/// file without this store's metadata.
pub fn read_only_head(path: impl AsRef<Path>) -> Result<Option<Head>, StorageError> {
    let read = || -> Result<Option<Head>, Fault> {
        let mut builder = Builder::new();
        builder.set_cache_size(DEFAULT_CACHE_SIZE);
        let db = builder.open_read_only(path.as_ref())?;
        let txn = db.begin_read()?;
        let meta = txn.open_table(META)?;
        let format = meta.get(FORMAT_KEY)?;
        let id = meta.get(DATABASE_ID_KEY)?;
        check_meta(
            format.as_ref().map(|value| value.value()),
            id.as_ref().map(|value| value.value()),
        )?;
        Ok(match meta.get(HEAD_KEY)? {
            Some(value) => Some(Head::decode(value.value())?),
            None => None,
        })
    };
    Ok(read()?)
}

impl ObjectReader for RedbStore {
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
            Ok(table
                .get(id)?
                .map(|value| copy_bounded(value.value(), max_bytes))
                .transpose()?)
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
            let found = match table {
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
            };
            Ok(found.transpose()?)
        })?;
        self.stats.record_read(found.as_ref().map(Vec::len));
        timing.success();
        Ok(found)
    }

    fn commit(&self, batch: &CommitBatch) -> CommitOutcome {
        let mut timing = self.timings[3].start();
        let outcome = self.transact(
            || match self.collects() {
                true => Err(gc_write_refused("a plain commit")),
                false => batch.validate(),
            },
            |txn| stage_batch(txn, batch),
        );
        if matches!(outcome, CommitOutcome::Committed) {
            timing.success();
        }
        outcome
    }

    fn stage_objects(&self, objects: &[(SpaceId, ObjectId, Vec<u8>)]) -> CommitOutcome {
        self.transact(
            || match self.collects() {
                true => Err(gc_write_refused("object staging")),
                false => validate_objects(objects),
            },
            |txn| stage(txn, None, objects, &[], None),
        )
    }

    fn relabel(&self, relabel: &Relabel) -> CommitOutcome {
        let mut timing = self.timings[3].start();
        let outcome = self.transact(
            || match self.collects() {
                true => Err(gc_write_refused("relabelling")),
                false => relabel.validate(),
            },
            |txn| stage_relabel(txn, relabel),
        );
        if matches!(outcome, CommitOutcome::Committed) {
            timing.success();
        }
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
        .and_then(|(db, id, report, gc)| {
            if id == self.database_id {
                Ok((db, report, gc))
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
            Ok((db, report, gc)) => {
                slot.db = Some(db);
                slot.epoch += 1;
                *lock(&self.last_open) = report;
                *lock(&self.gc) = gc;
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

    fn gc_state(&self) -> Result<Option<GcState>, StorageError> {
        Ok(*lock(&self.gc))
    }

    fn commit_gc(
        &self,
        batch: &CommitBatch,
        gc: &GcCommit,
        graph: &dyn ObjectGraph,
    ) -> CommitOutcome {
        let mut timing = self.timings[3].start();
        let _writer = lock(&self.writer);
        let invariant = std::cell::Cell::new(None::<String>);
        let planned = std::cell::Cell::new(None::<(GcState, GcReport)>);
        let collecting = std::cell::Cell::new(Duration::ZERO);
        let validate = || match batch.remove_objects.is_empty() {
            true => batch.validate(),
            false => Err(gc_write_refused("an object removal")),
        };
        let mut outcome = self.transact_locked(validate, |txn| {
            stage_gc(txn, batch, gc, graph, &invariant, &planned, &collecting)
        });
        if let Some(reason) = invariant.take() {
            // The counts are wrong: commit without collection, suspended,
            // so production continues and an operator rebuilds them.
            eprintln!("garbage collection suspended: {reason}");
            outcome = self.transact_locked(validate, |txn| {
                let written = stage_batch(txn, batch)?;
                let mut state = tables::read_state(txn)?.ok_or_else(|| {
                    Fault::new(
                        StorageErrorKind::Incompatible,
                        "the store does not collect garbage",
                    )
                })?;
                state.suspended = true;
                tables::write_state(txn, &state)?;
                planned.set(Some((state, GcReport::default())));
                Ok(written)
            });
        }
        self.gc_nanos.fetch_add(
            collecting.get().as_nanos().min(u128::from(u64::MAX)) as u64,
            Ordering::Relaxed,
        );
        if outcome.is_ok() {
            if let Some((state, report)) = planned.take() {
                *lock(&self.gc) = Some(state);
                *lock(&self.gc_report) = Some(report);
            }
            timing.success();
        }
        self.stats.write(outcome)
    }

    fn gc_report(&self) -> Option<GcReport> {
        *lock(&self.gc_report)
    }

    fn gc_enable(
        &self,
        roots: &[(SpaceId, ObjectId)],
        graph: &dyn ObjectGraph,
        spill: Option<&Path>,
    ) -> Result<GcState, StorageError> {
        let _writer = lock(&self.writer);
        let state = self.with_db(|db| {
            let quick_repair = self.config.quick_repair;
            // A rebuild starts from a plain store, so an interrupted one
            // leaves a plain store, never stale counts.
            let previous = {
                let mut txn = db.begin_write()?;
                txn.set_durability(Durability::Immediate)?;
                txn.set_quick_repair(quick_repair);
                let previous = tables::read_state(&txn)?;
                tables::clear(&txn)?;
                txn.commit()?;
                previous
            };
            *lock(&self.gc) = None;
            let read = db.begin_read()?;
            let head = {
                let meta = read.open_table(META)?;
                let head = meta.get(HEAD_KEY)?;
                head.map(|value| Head::decode(value.value())).transpose()?
            }
            .ok_or_else(|| {
                Fault::new(
                    StorageErrorKind::Incompatible,
                    "garbage collection needs a head",
                )
            })?;
            let mut seeder = Seeder::new(graph, roots, spill)?;
            for space in tables::object_spaces(&read)? {
                let name = object_table_name(space);
                let table = read.open_table(object_table(&name))?;
                for entry in table.iter()? {
                    let (id, bytes) = entry?;
                    seeder
                        .object(space, id.value(), bytes.value())
                        .map_err(seed_fault)?;
                }
            }
            drop(read);
            let epoch = previous.map_or(1, |state| state.epoch + 1);
            let mut writer = tables::SeedWriter::new(db, quick_repair);
            let state = seeder.finish(&mut writer, epoch).map_err(seed_fault)?;
            writer.flush()?;
            let mut txn = db.begin_write()?;
            txn.set_durability(Durability::Immediate)?;
            txn.set_quick_repair(quick_repair);
            {
                let mut epochs = txn.open_table(tables::GC_EPOCHS)?;
                epochs.insert(head.height, epoch)?;
                // The other tables exist from here on even when empty.
                txn.open_table(tables::GC_SHARED)?;
                txn.open_table(tables::GC_DEAD)?;
                txn.open_table(tables::GC_PHANTOM)?;
                txn.open_table(tables::GC_QUEUE)?;
                txn.open_table(tables::GC_RELEASE)?;
                txn.open_table(tables::GC_EPOCH_COUNTS)?;
            }
            tables::write_state(&txn, &state)?;
            txn.commit()?;
            Ok(state)
        })?;
        *lock(&self.gc) = Some(state);
        Ok(state)
    }

    fn gc_dump(&self) -> Result<Option<GcDump>, StorageError> {
        let Some(state) = *lock(&self.gc) else {
            return Ok(None);
        };
        self.with_db(|db| {
            let read = db.begin_read()?;
            let mut objects = std::collections::BTreeMap::new();
            for space in tables::object_spaces(&read)? {
                let name = object_table_name(space);
                let table = read.open_table(object_table(&name))?;
                for entry in table.iter()? {
                    let (id, bytes) = entry?;
                    objects.insert(gc_key(space, id.value()), bytes.value().to_vec());
                }
            }
            let tables = tables::dump_tables(&read)?;
            let state = {
                let meta = read.open_table(META)?;
                let found = meta.get(GC_STATE_KEY)?;
                match found {
                    Some(bytes) => GcState::decode(bytes.value())?,
                    None => state,
                }
            };
            Ok(Some(GcDump {
                objects,
                tables,
                state,
            }))
        })
    }
}

/// An invariant failure while seeding means the store's objects do not
/// form the graph: damage.
fn seed_fault(error: GcError) -> Fault {
    match error {
        GcError::Storage(error) => Fault(error),
        GcError::Invariant(detail) => {
            Fault::new(StorageErrorKind::Corrupt, format!("gc seeding: {detail}"))
        }
    }
}

/// Stages a garbage-collecting commit in `txn`: the head check, records and
/// head as [`stage`] does, then the planned GC changes and the new state.
/// An invariant failure is reported through `invariant` and aborts.
fn stage_gc(
    txn: &WriteTransaction,
    batch: &CommitBatch,
    gc: &GcCommit,
    graph: &dyn ObjectGraph,
    invariant: &std::cell::Cell<Option<String>>,
    planned: &std::cell::Cell<Option<(GcState, GcReport)>>,
    collecting: &std::cell::Cell<Duration>,
) -> Result<Written, Fault> {
    // `collecting` accumulates the time of the garbage collection work
    // only ([`RedbStore::gc_seconds`]): the spans below that a store
    // without collection would not run.
    let spent = |time: Duration| collecting.set(collecting.get() + time);
    let started = Instant::now();
    let state = tables::read_state(txn)?.ok_or_else(|| {
        Fault::new(
            StorageErrorKind::Incompatible,
            "this store does not collect garbage",
        )
    })?;
    spent(started.elapsed());
    if state.suspended {
        let written = stage_batch(txn, batch)?;
        planned.set(Some((state, GcReport::default())));
        return Ok(written);
    }
    let mut written = stage_removing(
        txn,
        batch.expected_head,
        &[],
        &batch.remove_records,
        &[],
        &batch.records,
        Some(batch.new_head),
    )?;
    let started = Instant::now();
    let mut view = TxnGc::open(txn)?;
    let plan = collect::plan(
        &mut view,
        graph,
        &state,
        &batch.objects,
        gc,
        batch.new_head.height,
    );
    // The planner also checks whether each of the batch's objects is
    // already stored, as every commit must.
    spent(started.elapsed().saturating_sub(view.matching));
    let plan = match plan {
        Ok(plan) => plan,
        Err(GcError::Storage(error)) => return Err(error.into()),
        Err(GcError::Invariant(reason)) => {
            invariant.set(Some(reason.clone()));
            return Err(Fault::new(StorageErrorKind::Corrupt, reason));
        }
    };
    plan.apply_inserts(&mut view)?;
    let started = Instant::now();
    plan.apply_collection(&mut view)?;
    drop(view);
    tables::write_state(txn, &plan.state)?;
    spent(started.elapsed());
    for (space, _, bytes) in &plan.inserts {
        written.objects.push((*space, bytes.len()));
    }
    planned.set(Some((plan.state, plan.report)));
    Ok(written)
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
