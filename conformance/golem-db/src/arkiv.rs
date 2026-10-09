//! `golemdb_api::Api` implemented on the Arkiv engine
//! (`arkiv-state-db-experiment`'s `arkiv-db` crate).
//!
//! Mapping:
//! - one fixed namespace (`golemdb`), created by the adapter in the first
//!   engine commit; golem-db commit N is engine snapshot N + 1;
//! - `RecordKey` (32 bytes) -> engine `RecordKey` (1..=32 bytes);
//! - attributes / fields -> engine `Cell::attribute` / `Cell::field`. The
//!   engine's type ids are golem's cell type ids and its fixed-width term
//!   bytes are golem's stored payloads, so a cell converts by id and payload
//!   (see [`to_value`] and [`from_cell`]);
//! - golem's `Genesis` (as loaded from its YAML) becomes the engine's
//!   genesis: `cell_limits` map onto the engine's [`Limits`], which the
//!   engine enforces, and opening returns golem's [`OpenInfo`]. Only
//!   keccak-256 is supported: a Blake3 genesis is refused at open;
//! - `BranchId` -> an adapter registry slot holding an engine `Branch` plus
//!   the log of the slot's applied writes. Checkpoint/rollback frames are
//!   positions in that log (golem's journal-frame rules), backed by engine
//!   `Checkpoint`s;
//! - `BranchInfo::version` (golem's undo-entry count) is derived from the
//!   log when asked, so writes never read a record for it;
//! - stale base -> `HandleInvalid` on every branch call, `Conflict` on commit
//!   (golem's rule: a stale handle is consumed when touched);
//! - storage failures: `StoreFull` for a full medium, otherwise `Internal`.
//!   As in golem a failed call leaves the branch as it was and can be
//!   retried: a branch the engine poisoned is rebuilt from its log, and the
//!   durable engine is recovered first.
//!
//! Two storage backends: the volatile in-memory `Engine` and the durable
//! `DurableEngine` on a `RedbStore` file.

use std::collections::{BTreeMap, HashMap};
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use arkiv_db::durable::{
    DurableEngine, DurableError, EngineGenesis, Initialization, Options, Publication, Recovery,
    WriterState,
};
use arkiv_db::{
    Branch, Budget, Cell, CellKind as ArkivKind, CellName as ArkivName, Checkpoint, Effect, Engine,
    EngineError, Limits, NamespaceId, NamespaceName, OwnerId, PriceSchedule, Projection,
    RecordKey as ArkivKey, Rejection, Row, Snapshot, SnapshotId, StorageError, TypeTag, Value,
    WriteOp,
};
use arkiv_db_redb::{RedbConfig, RedbStore};
use arkiv_db_storage::versioned::{Health, SpaceId, StorageErrorKind, Table, VersionedStore};
use golemdb_api::{
    Api, ApiError, BranchId, BranchInfo, CellKind, CellName, CellNameRef, CellValue, CommitId,
    Genesis, HashAlgorithm, ImmutableDataAddress, ImmutableDataKey, ImmutableDataOrdinal,
    ImmutableDataRow, OpenConfig, OpenError, OpenInfo, OpenMode, OpenResult, ReadTarget, Record,
    RecordKey, RecordOp, Result, SealInfo, op,
};
use golemdb_cells::system;

const ACTOR: OwnerId = OwnerId::new([0xa1; 20]);
const NAMESPACE: &str = "golemdb";
/// The durable backend's file inside the database directory.
pub const FILE: &str = "arkiv.redb";
/// Engine genesis identity of every database this adapter creates.
const DEPLOYMENT: [u8; 32] = [0x67; 32];
/// The engine's genesis metadata key (`durable::format::GENESIS_KEY`), read
/// only to tell a genesis mismatch from another incompatibility.
const GENESIS_KEY: &[u8] = b"engine/genesis";

/// Branch ids are process-wide, as in golem's branch manager, so handles of
/// independent databases never alias.
static NEXT_BRANCH: AtomicU64 = AtomicU64::new(0);

/// Runtime options of the durable backend; none changes logical state.
#[derive(Clone, Copy, Debug)]
pub struct ArkivOptions {
    /// redb page cache, bytes.
    pub redb_cache: usize,
    /// Engine encoded-object cache, bytes and entries.
    pub object_cache: usize,
    pub object_cache_entries: usize,
}

impl Default for ArkivOptions {
    fn default() -> Self {
        Self {
            redb_cache: 256 << 20,
            // The engine's own defaults (durable::Options::default()).
            object_cache: 8 << 20,
            object_cache_entries: 16_384,
        }
    }
}

#[derive(Debug)]
struct AdapterError(String);
impl std::fmt::Display for AdapterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for AdapterError {}

fn internal(message: impl Into<String>) -> ApiError {
    ApiError::internal(AdapterError(message.into()))
}

/// golem maps a full store to `StoreFull` and every other storage failure
/// to `Internal`, keeping the storage error as the source.
fn storage(error: StorageError) -> ApiError {
    if error.kind == StorageErrorKind::Full {
        ApiError::StoreFull
    } else {
        ApiError::internal(error)
    }
}

fn durable(error: DurableError) -> ApiError {
    match error {
        DurableError::Storage(error) => storage(error),
        other => ApiError::internal(other),
    }
}

fn engine(error: EngineError) -> ApiError {
    ApiError::internal(error)
}

/// golem reports a poisoned lock as an internal error instead of panicking.
fn lock<'a, T>(mutex: &'a Mutex<T>, what: &str) -> Result<MutexGuard<'a, T>> {
    mutex
        .lock()
        .map_err(|_| internal(format!("{what} lock poisoned")))
}

struct Durable {
    engine: DurableEngine,
    store: Arc<RedbStore>,
    path: PathBuf,
    genesis: EngineGenesis,
    options: Options,
}

enum Backend {
    Memory(Engine),
    Durable(Box<Durable>),
}

impl Backend {
    fn head(&self) -> SnapshotId {
        match self {
            Self::Memory(e) => e.head(),
            Self::Durable(d) => d.engine.head(),
        }
    }
    fn head_snapshot(&self) -> Snapshot {
        match self {
            Self::Memory(e) => e.head_snapshot(),
            Self::Durable(d) => d.engine.head_snapshot(),
        }
    }
    fn snapshot(&self, id: SnapshotId) -> Result<Snapshot> {
        match self {
            Self::Memory(e) => e.snapshot(id).map_err(engine),
            Self::Durable(d) => d.engine.snapshot(id).map_err(durable),
        }
    }
    fn begin(&self, base: SnapshotId) -> Result<Branch> {
        match self {
            Self::Memory(e) => e.begin(base, None).map_err(engine),
            Self::Durable(d) => d.engine.begin(base, None).map_err(durable),
        }
    }
    fn state_root(&self) -> [u8; 32] {
        match self {
            Self::Memory(e) => e.head_snapshot().state_root().0,
            Self::Durable(d) => d.engine.head_info().state_root.0,
        }
    }
}

/// The engine branch of a slot, valid for one engine epoch.
struct Live {
    branch: Branch,
    /// One engine checkpoint per frame of the slot.
    checkpoints: Vec<Checkpoint>,
    epoch: u64,
}

struct Slot {
    base: SnapshotId,
    /// Every write applied and retained (not rolled back), in order.
    log: Vec<WriteOp>,
    /// golem's journal frames: the log length at the start of each frame;
    /// the first frame starts at 0 and is never popped.
    frames: Vec<usize>,
    /// golem's `Journal::rolled_back`.
    rolled_back: bool,
    /// Set once sealed; a sealed branch stays readable.
    sealed: Option<SealInfo>,
    /// `None` after a poisoning failure, a panic or a commit attempt: the
    /// branch is rebuilt from the log before its next use.
    live: Option<Live>,
    /// Memo of `BranchInfo::version` for the current log.
    version: Option<u64>,
}

type Entry = Arc<Mutex<Option<Slot>>>;

pub struct ArkivDb {
    backend: Mutex<Backend>,
    /// Sequence of the engine head; read without the engine lock.
    head: AtomicU64,
    /// Bumped (under the engine lock) whenever engine branches made before
    /// can no longer publish: after a store recovery or an engine reopen.
    epoch: AtomicU64,
    /// The durable writer needs recovery before its next use.
    unhealthy: AtomicBool,
    /// Durable backend: the store, for health checks without the engine lock.
    store: Option<Arc<RedbStore>>,
    branches: Mutex<HashMap<u64, Entry>>,
    namespace: NamespaceId,
    /// Engine snapshot sequence of golem-db commit 0.
    offset: u64,
    genesis: Genesis,
    info: OpenInfo,
    pub description: String,
}

/// golem's genesis -> the engine's genesis.
///
/// The engine has one hash (keccak-256), so a Blake3 genesis is refused, as
/// golem refuses settings its backend cannot hold. golem's three cell limits
/// map one to one. golem indexes a `str` attribute up to `max_str_len`, so
/// that is also the engine's indexed-string limit; a value above the
/// engine's caps (32-byte names, 128-byte indexed values) is refused rather
/// than silently narrowed. golem has no per-record cell count: the engine's
/// default (64) applies.
pub fn engine_genesis(genesis: &Genesis) -> OpenResult<EngineGenesis> {
    if genesis.hash_function != HashAlgorithm::Keccak256 {
        return Err(OpenError::InvalidConfig(format!(
            "unsupported hash function {:?}: the arkiv engine supports only keccak-256",
            genesis.hash_function
        )));
    }
    let golem = genesis.cell_limits;
    if golem.max_str_len as usize > Value::MAX_INDEXED_LEN {
        return Err(OpenError::InvalidConfig(format!(
            "max_str_len {} exceeds the arkiv engine's indexed-value cap of {} bytes \
             (golem indexes str attributes up to max_str_len)",
            golem.max_str_len,
            Value::MAX_INDEXED_LEN
        )));
    }
    let limits = Limits {
        max_cell_name_len: golem.max_cell_name_len as usize,
        max_str_len: golem.max_str_len as usize,
        max_bytes_len: golem.max_bytes_len as usize,
        max_indexed_len: golem.max_str_len as usize,
        ..Limits::DEFAULT
    };
    limits
        .validate()
        .map_err(|reason| OpenError::InvalidConfig(format!("arkiv engine limits: {reason}")))?;
    let mut engine_genesis = EngineGenesis::new(DEPLOYMENT, PriceSchedule::FIXTURE);
    engine_genesis.limits = limits;
    // A stored object must hold the largest admissible row.
    let row = limits.max_cells as u64 * (limits.max_payload_len() as u64 + 64);
    engine_genesis.max_object_bytes = (16 << 20).max(2 * row);
    engine_genesis
        .validate()
        .map_err(|e| OpenError::InvalidConfig(format!("arkiv engine genesis: {e}")))?;
    Ok(engine_genesis)
}

/// A storage failure while opening, in golem's opening-error terms.
fn open_storage(error: StorageError) -> OpenError {
    match error.kind {
        StorageErrorKind::Corrupt => OpenError::CorruptState("arkiv store: damaged data"),
        StorageErrorKind::Missing => OpenError::CorruptState("arkiv store: missing data"),
        StorageErrorKind::Incompatible => {
            OpenError::CorruptState("arkiv store: not an arkiv engine database")
        }
        _ => OpenError::Io(std::io::Error::other(error)),
    }
}

/// `DurableEngine::open` refused the store: a different genesis is golem's
/// `GenesisMismatch`; anything else is unusable state.
fn open_existing(
    store: &dyn VersionedStore,
    expected: &EngineGenesis,
    error: DurableError,
) -> OpenError {
    match error {
        DurableError::Storage(error) if error.kind == StorageErrorKind::Incompatible => {
            match store.read_record_bounded(Table::Meta, GENESIS_KEY, 1 << 16) {
                Ok(Some(stored)) if stored != expected.envelope() => OpenError::GenesisMismatch,
                _ => OpenError::CorruptState("arkiv store: incompatible engine profile"),
            }
        }
        DurableError::Storage(error) => open_storage(error),
        _ => OpenError::CorruptState("arkiv engine: stored state does not open"),
    }
}

fn durable_options(options: &ArkivOptions) -> Options {
    Options {
        cache_bytes: options.object_cache,
        cache_entries: options.object_cache_entries,
        prepare_bytes: 1 << 30,
        prepare_objects: 4_000_000,
        prepare_visits: 100_000_000,
    }
}

impl ArkivDb {
    /// Volatile engine; all history retained in memory.
    pub fn open_memory(genesis: &Genesis, _options: ArkivOptions) -> OpenResult<Self> {
        let engine_genesis = engine_genesis(genesis)?;
        let engine = Engine::with_limits(PriceSchedule::FIXTURE, engine_genesis.limits);
        Self::setup(
            Backend::Memory(engine),
            None,
            genesis,
            &engine_genesis,
            "arkiv memory Engine".into(),
        )
    }

    /// Durable engine in `dir/arkiv.redb`, with golem's opening modes:
    /// `CreateIfMissing` initializes pristine storage or validates and
    /// reopens it, `ExistingOnly` never initializes, `CreateNew` requires
    /// pristine storage. Reopening checks the genesis (`GenesisMismatch`).
    pub fn open(dir: &Path, config: &OpenConfig, options: ArkivOptions) -> OpenResult<Self> {
        engine_genesis(&config.genesis)?;
        let path = dir.join(FILE);
        let present = std::fs::metadata(&path).is_ok_and(|m| m.len() > 0);
        let redb = RedbConfig::with_cache_size(options.redb_cache);
        let store = if present {
            RedbStore::open(&path, redb).map_err(open_storage)?
        } else {
            // Like golem, ExistingOnly never creates a missing directory.
            if config.mode == OpenMode::ExistingOnly {
                return Err(OpenError::NotInitialized);
            }
            std::fs::create_dir_all(dir).map_err(OpenError::Io)?;
            RedbStore::create(&path, redb).map_err(open_storage)?
        };
        Self::open_store(Arc::new(store), path, config, options)
    }

    /// The durable engine on an already opened store (the ported tests use
    /// it to inject storage faults).
    pub fn open_store(
        store: Arc<RedbStore>,
        path: PathBuf,
        config: &OpenConfig,
        options: ArkivOptions,
    ) -> OpenResult<Self> {
        let engine_genesis = engine_genesis(&config.genesis)?;
        let engine_options = durable_options(&options);
        let versioned: Arc<dyn VersionedStore> = store.clone();
        let description = format!(
            "arkiv DurableEngine on redb ({}, redb cache {} MiB, object cache {} MiB / {} entries)",
            path.display(),
            options.redb_cache >> 20,
            options.object_cache >> 20,
            options.object_cache_entries
        );
        let pristine = versioned.head().map_err(open_storage)?.is_none();
        if pristine {
            if config.mode == OpenMode::ExistingOnly {
                return Err(OpenError::NotInitialized);
            }
            let initialized = DurableEngine::initialize(
                versioned,
                engine_genesis.clone(),
                engine_options.clone(),
            )
            .map_err(|e| open_existing(store.as_ref(), &engine_genesis, e))?;
            let engine = match initialized {
                Initialization::Committed(engine) => engine,
                Initialization::NotCommitted { error, .. }
                | Initialization::Indeterminate { error, .. } => return Err(open_storage(error)),
            };
            let backend = Backend::Durable(Box::new(Durable {
                engine,
                store: store.clone(),
                path,
                genesis: engine_genesis.clone(),
                options: engine_options,
            }));
            return Self::setup(
                backend,
                Some(store),
                &config.genesis,
                &engine_genesis,
                description,
            );
        }
        let engine = DurableEngine::open(
            versioned.clone(),
            engine_genesis.clone(),
            engine_options.clone(),
        )
        .map_err(|e| open_existing(versioned.as_ref(), &engine_genesis, e))?;
        // golem checks the genesis before the mode.
        if config.mode == OpenMode::CreateNew {
            return Err(OpenError::AlreadyInitialized);
        }
        // The adapter's own genesis commit: the namespace, as snapshot 1.
        let namespaces = engine
            .head_snapshot()
            .try_namespace_count()
            .map_err(open_storage)?;
        if engine.head().sequence() < 1 || namespaces != 1 {
            return Err(OpenError::CorruptState(
                "arkiv adapter: the genesis namespace commit is missing",
            ));
        }
        let backend = Backend::Durable(Box::new(Durable {
            engine,
            store: store.clone(),
            path,
            genesis: engine_genesis.clone(),
            options: engine_options,
        }));
        Ok(Self::new(
            backend,
            Some(store),
            NamespaceId::from_ordinal(1).expect("nonzero"),
            1,
            config.genesis,
            &engine_genesis,
            false,
            description,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn new(
        backend: Backend,
        store: Option<Arc<RedbStore>>,
        namespace: NamespaceId,
        offset: u64,
        genesis: Genesis,
        engine_genesis: &EngineGenesis,
        created: bool,
        description: String,
    ) -> Self {
        let head = backend.head();
        let info = OpenInfo {
            created,
            commit_id: CommitId::new(head.sequence() - offset),
            state_root: backend.state_root(),
            // The index is part of the namespace state; there is no
            // separate index root (seals report zeros too).
            index_root: [0; 32],
            genesis_id: engine_genesis.id().0,
        };
        Self {
            backend: Mutex::new(backend),
            head: AtomicU64::new(head.sequence()),
            epoch: AtomicU64::new(0),
            unhealthy: AtomicBool::new(false),
            store,
            branches: Mutex::new(HashMap::new()),
            namespace,
            offset,
            genesis,
            info,
            description,
        }
    }

    /// golem commit 0: the engine commit creating the adapter's namespace.
    fn setup(
        mut backend: Backend,
        store: Option<Arc<RedbStore>>,
        genesis: &Genesis,
        engine_genesis: &EngineGenesis,
        description: String,
    ) -> OpenResult<Self> {
        let failed = |e: ApiError| OpenError::Io(std::io::Error::other(e));
        let mut branch = backend.begin(backend.head()).map_err(failed)?;
        let executed = branch
            .try_execute(
                ACTOR,
                WriteOp::CreateNamespace {
                    name: NamespaceName::new(NAMESPACE).expect("valid namespace name"),
                },
                Budget::unlimited(),
            )
            .map_err(open_storage)?
            .map_err(|r| failed(internal(format!("create namespace: {r}"))))?;
        let Effect::NamespaceCreated(namespace) = executed.effect else {
            return Err(failed(internal("unexpected namespace effect")));
        };
        let candidate = branch.try_seal().map_err(|e| failed(engine(e)))?;
        match &mut backend {
            Backend::Memory(e) => {
                e.publish(&candidate).map_err(|e| failed(engine(e)))?;
            }
            Backend::Durable(d) => {
                d.engine
                    .prepare(candidate)
                    .map_err(|f| failed(durable(f.error)))?;
                match d.engine.commit().map_err(|e| failed(durable(e)))? {
                    Publication::Committed(_) => {}
                    Publication::NotCommitted(e) | Publication::Indeterminate(e) => {
                        return Err(open_storage(e));
                    }
                }
            }
        }
        let offset = backend.head().sequence();
        Ok(Self::new(
            backend,
            store,
            namespace,
            offset,
            *genesis,
            engine_genesis,
            true,
            description,
        ))
    }

    /// golem's `Database::info`: a snapshot of opening.
    pub fn info(&self) -> &OpenInfo {
        &self.info
    }

    /// golem's `Database::genesis`.
    pub fn genesis(&self) -> &Genesis {
        &self.genesis
    }

    /// The harness's `table_log` for this backend: CSV lines in the
    /// harness's `table,entries,depth,pages,mib` shape. Durable: redb's
    /// object tables per engine space, the other tables, unaccounted and
    /// free file space, then the engine's object cache; both backends: live
    /// records, index terms and retained snapshots (entries only).
    pub fn table_stats(&self) -> Result<Vec<String>> {
        let backend = lock(&self.backend, "engine")?;
        let snapshot = backend.head_snapshot();
        let mut lines = Vec::new();
        let mib = |bytes: u64| bytes as f64 / (1u64 << 20) as f64;
        let snapshots = match &*backend {
            Backend::Memory(e) => e.snapshot_count() as u64,
            Backend::Durable(d) => {
                let spaces = [
                    ("catalog", SpaceId::Catalog),
                    ("host", SpaceId::Host),
                    ("namespace", SpaceId::Namespace(self.namespace.ordinal())),
                ];
                let layout = d
                    .store
                    .layout(&spaces.map(|(_, space)| space))
                    .map_err(storage)?;
                let data = d.store.data_stats().map_err(storage)?;
                let page = layout.page_size as u64;
                let (mut entries, mut pages) = (0, 0);
                for (name, space) in spaces {
                    if let Some(table) = layout.spaces.get(&space) {
                        let used = table.leaf_pages + table.branch_pages;
                        entries += table.entries;
                        pages += used;
                        lines.push(format!(
                            "objects:{name},{},{},{used},{:.3}",
                            table.entries,
                            table.height,
                            mib(used * page)
                        ));
                    }
                }
                // Metadata, manifests and the block index.
                let tables = data.leaf_pages + data.branch_pages;
                let records = tables.saturating_sub(pages);
                lines.push(format!(
                    "records,{},,{records},{:.3}",
                    data.entries.saturating_sub(entries),
                    mib(records * page)
                ));
                let file = std::fs::metadata(&d.path).map_or(0, |m| m.len()) / page.max(1);
                // unaccounted: redb's table directory, allocator state and
                // pages freed by recent commits; free: file space not in use.
                let unaccounted = layout.allocated_pages.saturating_sub(tables);
                let free = file.saturating_sub(layout.allocated_pages);
                for (name, count) in [("unaccounted", unaccounted), ("free", free), ("file", file)]
                {
                    lines.push(format!("{name},,,{count},{:.3}", mib(count * page)));
                }
                let stats = d.engine.stats();
                lines.push(format!(
                    "object-cache,{},,,{:.3}",
                    stats.cache.entries,
                    mib(stats.cache.payload_capacity as u64)
                ));
                d.engine.snapshot_count().map_err(durable)?
            }
        };
        let count = |n: Option<usize>| n.unwrap_or(0);
        lines.push(format!(
            "live:records,{},,,",
            count(
                snapshot
                    .try_typed_record_count(self.namespace)
                    .map_err(storage)?
            )
        ));
        lines.push(format!(
            "live:terms,{},,,",
            count(snapshot.try_term_count(self.namespace).map_err(storage)?)
        ));
        lines.push(format!("snapshots,{snapshots},,,"));
        Ok(lines)
    }

    fn commit_id(&self, snapshot: SnapshotId) -> CommitId {
        CommitId::new(snapshot.sequence() - self.offset)
    }

    fn head_id(&self) -> SnapshotId {
        SnapshotId::from_sequence(self.head.load(Ordering::Acquire))
    }

    fn entry(&self, branch: BranchId) -> Result<Entry> {
        lock(&self.branches, "registry")?
            .get(&branch.get())
            .cloned()
            .ok_or(ApiError::HandleInvalid)
    }

    fn remove(&self, branch: BranchId) -> Result<()> {
        lock(&self.branches, "registry")?.remove(&branch.get());
        Ok(())
    }

    /// Whether engine branches can be used without first recovering.
    fn healthy(&self) -> bool {
        !self.unhealthy.load(Ordering::Acquire)
            && self
                .store
                .as_ref()
                .is_none_or(|store| matches!(store.health(), Health::Available))
    }

    /// Brings the durable writer back to `Ready` after a storage failure:
    /// recovers the store, resolves a prepared or uncertain publication and,
    /// if a publication was observed committed, reopens the engine. Any
    /// recovery makes older engine branches unpublishable (`epoch`).
    fn heal(&self, backend: &mut Backend) -> Result<()> {
        let Backend::Durable(d) = backend else {
            return Ok(());
        };
        let result = self.heal_durable(d);
        let ready = d.engine.writer_state() == WriterState::Ready;
        self.unhealthy
            .store(result.is_err() || !ready, Ordering::Release);
        result
    }

    fn heal_durable(&self, d: &mut Durable) -> Result<()> {
        match d.engine.writer_state() {
            WriterState::Ready => {
                if let Health::Failed(_) = d.store.health() {
                    d.engine.recover().map_err(durable)?;
                    self.epoch.fetch_add(1, Ordering::AcqRel);
                }
            }
            WriterState::Prepared => d.engine.discard_prepared().map_err(durable)?,
            WriterState::Unavailable | WriterState::Uncertain => {
                match d.engine.recover_pending() {
                    Ok(Recovery::RetryPrepared) => {
                        // The slot that prepared it re-prepares on retry.
                        self.epoch.fetch_add(1, Ordering::AcqRel);
                        d.engine.discard_prepared().map_err(durable)?;
                    }
                    Ok(Recovery::ObservedCommittedDurabilityUnknown(_)) => self.reopen(d)?,
                    Ok(Recovery::Ready) => {}
                    Err(DurableError::NoPrepared) => {
                        d.engine.recover().map_err(durable)?;
                        self.epoch.fetch_add(1, Ordering::AcqRel);
                    }
                    Err(error) => return Err(durable(error)),
                }
            }
        }
        Ok(())
    }

    /// A new engine handle over the same store, at the stored head.
    fn reopen(&self, d: &mut Durable) -> Result<()> {
        self.epoch.fetch_add(1, Ordering::AcqRel);
        let store: Arc<dyn VersionedStore> = d.store.clone();
        d.engine =
            DurableEngine::open(store, d.genesis.clone(), d.options.clone()).map_err(durable)?;
        self.head
            .store(d.engine.head().sequence(), Ordering::Release);
        Ok(())
    }

    /// The slot's engine branch, rebuilt from the log if it was dropped or
    /// made under an older epoch, after healing the engine if needed.
    fn live<'a>(&self, slot: &'a mut Slot) -> Result<&'a mut Live> {
        let epoch = self.epoch.load(Ordering::Acquire);
        let fresh = slot.live.as_ref().is_some_and(|live| live.epoch == epoch);
        if !fresh || !self.healthy() {
            let mut backend = lock(&self.backend, "engine")?;
            self.rebuild(slot, &mut backend)?;
        }
        Ok(slot.live.as_mut().expect("built"))
    }

    fn rebuild(&self, slot: &mut Slot, backend: &mut Backend) -> Result<()> {
        self.heal(backend)?;
        let epoch = self.epoch.load(Ordering::Acquire);
        if slot.live.as_ref().is_some_and(|live| live.epoch == epoch) {
            return Ok(());
        }
        slot.live = None;
        let mut branch = backend.begin(slot.base)?;
        let mut checkpoints = Vec::with_capacity(slot.frames.len());
        let mut frames = slot.frames.iter().peekable();
        for (at, op) in slot.log.iter().enumerate() {
            while frames.next_if(|&&start| start == at).is_some() {
                checkpoints.push(branch.try_checkpoint().map_err(engine)?);
            }
            branch
                .try_execute(ACTOR, op.clone(), Budget::unlimited())
                .map_err(storage)?
                .map_err(|r| internal(format!("branch replay diverged: {r}")))?;
        }
        for _ in frames {
            checkpoints.push(branch.try_checkpoint().map_err(engine)?);
        }
        slot.live = Some(Live {
            branch,
            checkpoints,
            epoch,
        });
        Ok(())
    }

    /// golem's `with_slot`: lock the branch, then reject (and reclaim) a
    /// stale or consumed handle with `stale`. A panic inside `f` drops the
    /// engine branch (the log restores it), releases the lock unpoisoned
    /// and resumes, as golem's branch manager does.
    fn with_slot<T>(
        &self,
        branch: BranchId,
        stale: fn() -> ApiError,
        f: impl FnOnce(&mut Option<Slot>) -> Result<T>,
    ) -> Result<T> {
        let entry = self.entry(branch)?;
        let mut guard = lock(&entry, "branch")?;
        let base = guard.as_ref().ok_or(ApiError::HandleInvalid)?.base;
        if base != self.head_id() {
            *guard = None;
            drop(guard);
            self.remove(branch)?;
            return Err(stale());
        }
        match catch_unwind(AssertUnwindSafe(|| f(&mut guard))) {
            Ok(result) => result,
            Err(panic) => {
                if let Some(slot) = guard.as_mut() {
                    slot.live = None;
                    slot.version = None;
                }
                drop(guard);
                resume_unwind(panic)
            }
        }
    }

    /// A live, open (unsealed) slot.
    fn with_open<T>(&self, branch: BranchId, f: impl FnOnce(&mut Slot) -> Result<T>) -> Result<T> {
        self.with_slot(
            branch,
            || ApiError::HandleInvalid,
            |slot| {
                let slot = slot.as_mut().ok_or(ApiError::HandleInvalid)?;
                if slot.sealed.is_some() {
                    return Err(ApiError::Sealed);
                }
                f(slot)
            },
        )
    }

    /// Executes `op` on the slot's branch and logs it. A rejected write
    /// changes nothing; a storage failure poisons the engine branch, which
    /// is then dropped and rebuilt from the log, so the failed write leaves
    /// no trace (golem rewinds a failed write group). `resets` is whether
    /// the write reactivates a rolled-back frame (golem: any journal entry).
    fn execute(&self, slot: &mut Slot, op: WriteOp, resets: bool) -> Result<()> {
        let live = self.live(slot)?;
        let result = live
            .branch
            .try_execute(ACTOR, op.clone(), Budget::unlimited());
        let poisoned = live.branch.is_poisoned();
        match result {
            Err(error) => {
                if poisoned {
                    slot.live = None;
                }
                Err(storage(error))
            }
            Ok(Err(rejected)) => Err(rejection(rejected)),
            Ok(Ok(_)) => {
                slot.log.push(op);
                slot.version = None;
                if resets {
                    slot.rolled_back = false;
                }
                Ok(())
            }
        }
    }

    /// golem's name grammar, then the engine's name type. Length limits are
    /// the engine's: its 32-byte name type and the configured
    /// `max_cell_name_len`, checked at admission.
    fn check_name(&self, name: &CellName) -> Result<ArkivName> {
        let text = CellNameRef::parse_user(name.as_bytes(), usize::MAX)
            .map_err(|e| ApiError::invalid_argument(e.to_string()))?;
        let text = std::str::from_utf8(text.as_bytes()).expect("grammar is ASCII");
        ArkivName::new(text)
            .map_err(|e| ApiError::invalid_argument(format!("arkiv cell name {text:?}: {e}")))
    }

    /// Value limits are the engine's, checked at admission.
    fn check_value(&self, value: &CellValue) -> Result<Cell> {
        let converted = to_value(value)?;
        Ok(match value.kind() {
            CellKind::Attribute => Cell::attribute(converted),
            CellKind::Field => Cell::field(converted),
        })
    }

    fn read(&self, source: Source<'_>, key: &ArkivKey) -> Result<Option<Row>> {
        let result = match source {
            Source::Branch(branch) => {
                branch.try_get_record(self.namespace, key, &Projection::ALL, Budget::unlimited())
            }
            Source::Snapshot(snapshot) => {
                snapshot.try_get_record(self.namespace, key, &Projection::ALL, Budget::unlimited())
            }
        };
        let metered = result
            .map_err(storage)?
            .map_err(|r| internal(format!("get_record: {r}")))?;
        Ok(metered.value)
    }

    /// A read through the slot's branch (sealed branches stay readable).
    fn read_branch(&self, slot: &mut Slot, key: &ArkivKey) -> Result<Option<Row>> {
        let live = self.live(slot)?;
        let result = self.read(Source::Branch(&live.branch), key);
        if live.branch.is_poisoned() {
            slot.live = None;
        }
        result
    }

    /// golem's `BranchInfo::version`: the undo entries the retained writes
    /// hold. Derived from the log against the base when asked, so writes
    /// never read a record for it. create: its cells, `#key`, the key
    /// binding and the allocator; patch: each cell whose value changes or
    /// that is removed while present; delete: its cells, `#key` and binding.
    fn version(&self, slot: &mut Slot) -> Result<u64> {
        if let Some(version) = slot.version {
            return Ok(version);
        }
        type Cells = Vec<(ArkivName, Cell)>;
        let mut known: HashMap<ArkivKey, Option<Cells>> = HashMap::new();
        // The base (== head here), taken only if a record of it is needed.
        let mut base: Option<Snapshot> = None;
        let mut current =
            |known: &HashMap<ArkivKey, Option<Cells>>, key: &ArkivKey| -> Result<Option<Cells>> {
                if let Some(cells) = known.get(key) {
                    return Ok(cells.clone());
                }
                if base.is_none() {
                    let mut backend = lock(&self.backend, "engine")?;
                    if !self.healthy() {
                        self.heal(&mut backend)?;
                    }
                    base = Some(backend.head_snapshot());
                }
                let snapshot = base.as_ref().expect("taken");
                Ok(self
                    .read(Source::Snapshot(snapshot), key)?
                    .map(|row| row.cells))
            };
        let mut version = 0u64;
        for op in &slot.log {
            match op {
                WriteOp::CreateRecord { key, cells, .. } => {
                    version += cells.len() as u64 + 3;
                    known.insert(key.clone(), Some(cells.clone()));
                }
                WriteOp::PatchRecord {
                    key, set, remove, ..
                } => {
                    let mut cells: Cells = current(&known, key)?
                        .ok_or_else(|| internal("logged patch of a missing record"))?;
                    for (name, cell) in set {
                        match cells.iter_mut().find(|(n, _)| n == name) {
                            Some((_, old)) if old == cell => {}
                            Some((_, old)) => {
                                *old = cell.clone();
                                version += 1;
                            }
                            None => {
                                cells.push((name.clone(), cell.clone()));
                                version += 1;
                            }
                        }
                    }
                    for name in remove {
                        if let Some(at) = cells.iter().position(|(n, _)| n == name) {
                            cells.remove(at);
                            version += 1;
                        }
                    }
                    known.insert(key.clone(), Some(cells));
                }
                WriteOp::DeleteRecord { key, .. } => {
                    let cells: Cells = current(&known, key)?
                        .ok_or_else(|| internal("logged delete of a missing record"))?;
                    version += cells.len() as u64 + 2;
                    known.insert(key.clone(), None);
                }
                _ => {}
            }
        }
        slot.version = Some(version);
        Ok(version)
    }

    /// Seals an open slot (idempotent), returning golem's seal result. A
    /// failed seal leaves the branch open, as in golem.
    fn seal_slot(&self, slot: &mut Slot) -> Result<SealInfo> {
        if let Some(info) = slot.sealed {
            return Ok(info);
        }
        let live = self.live(slot)?;
        // The root without consuming the branch: sealed branches stay
        // readable, and the hashes it computes are kept for the seal at
        // commit.
        let root = live.branch.try_state_root();
        if live.branch.is_poisoned() {
            slot.live = None;
        }
        let root = root.map_err(engine)?;
        let info = SealInfo {
            commit_id: CommitId::new(self.commit_id(slot.base).get() + 1),
            state_root: root.0,
            index_root: [0; 32],
        };
        slot.sealed = Some(info);
        Ok(info)
    }

    /// Publishes on the durable engine. A failed publication keeps nothing
    /// prepared that would block other writers: the committing slot
    /// rebuilds and re-prepares on retry.
    fn publish_durable(
        &self,
        d: &mut Durable,
        candidate: arkiv_db::SealedCandidate,
    ) -> Result<SnapshotId> {
        if let Err(failure) = d.engine.prepare(candidate) {
            return Err(match failure.error {
                DurableError::Engine(EngineError::StaleHead { .. }) => ApiError::Conflict,
                error => durable(error),
            });
        }
        match d.engine.commit() {
            Ok(Publication::Committed(info)) => Ok(info.snapshot),
            Ok(Publication::NotCommitted(error)) => {
                if d.engine.writer_state() == WriterState::Prepared {
                    d.engine.discard_prepared().map_err(durable)?;
                } else {
                    self.unhealthy.store(true, Ordering::Release);
                }
                Err(storage(error))
            }
            Ok(Publication::Indeterminate(error)) => match d.engine.recover_pending() {
                // It did commit: report it, with a fresh engine handle.
                Ok(Recovery::ObservedCommittedDurabilityUnknown(info)) => {
                    self.reopen(d)?;
                    Ok(info.snapshot)
                }
                Ok(Recovery::RetryPrepared) => {
                    self.epoch.fetch_add(1, Ordering::AcqRel);
                    d.engine.discard_prepared().map_err(durable)?;
                    Err(storage(error))
                }
                _ => {
                    self.unhealthy.store(true, Ordering::Release);
                    Err(storage(error))
                }
            },
            Err(error) => {
                self.unhealthy.store(true, Ordering::Release);
                Err(durable(error))
            }
        }
    }
}

enum Source<'a> {
    Branch(&'a Branch),
    Snapshot(&'a Snapshot),
}

fn arkiv_key(key: RecordKey) -> ArkivKey {
    ArkivKey::new(&key.0).expect("32-byte keys are valid")
}

fn reject_reserved(key: RecordKey) -> Result<()> {
    if system::by_key(&key.0).is_some() {
        Err(ApiError::Reserved)
    } else {
        Ok(())
    }
}

fn rejection(rejected: arkiv_db::Rejected) -> ApiError {
    match rejected.reason {
        Rejection::AlreadyExists => ApiError::AlreadyExists,
        Rejection::NotFound => ApiError::NotFound,
        Rejection::InvalidArgument(fault) => {
            ApiError::invalid_argument(format!("arkiv engine: {fault}"))
        }
        Rejection::RecordFull => ApiError::invalid_argument("arkiv engine: record full"),
        other => internal(format!("arkiv engine rejection: {other}")),
    }
}

/// golem cell -> engine value: the same type id, and golem's payload is
/// the engine's term bytes for every fixed-width type.
fn to_value(value: &CellValue) -> Result<Value> {
    let mismatch = || internal(format!("golem {:?} payload", value.cell_type()));
    let tag = TypeTag::from_id(value.cell_type().id()).ok_or_else(mismatch)?;
    Ok(match tag {
        TypeTag::Str => Value::str(value.as_str().ok_or_else(mismatch)?),
        TypeTag::Bytes => Value::bytes(value.as_bytes().ok_or_else(mismatch)?),
        fixed => Value::from_term_bytes(fixed, value.value()).ok_or_else(mismatch)?,
    })
}

/// Engine cell -> golem cell, the inverse of [`to_value`]: golem's encoded
/// cell is `type id | attribute bit ‖ payload`. The engine's `Key` (an
/// Arkiv entity reference, id 64) has no golem type and reads back as
/// golem `bytes32`.
fn from_cell(cell: &Cell) -> Result<CellValue> {
    let (id, payload) = match &cell.value {
        Value::Str(text) => (TypeTag::Str.id(), text.as_bytes().to_vec()),
        Value::Bytes(bytes) => (TypeTag::Bytes.id(), bytes.to_vec()),
        Value::Key(bytes) => (TypeTag::Bytes32.id(), bytes.to_vec()),
        fixed => (
            fixed.tag().id(),
            fixed
                .term_bytes()
                .expect("fixed-width values have term bytes"),
        ),
    };
    let attribute = match cell.kind {
        ArkivKind::Attribute => 0x80,
        ArkivKind::Field => 0,
    };
    let mut encoded = vec![id | attribute];
    encoded.extend(payload);
    CellValue::parse(encoded).map_err(|e| internal(format!("stored cell: {e}")))
}

fn golem_name(name: &str) -> CellName {
    CellName::from(CellNameRef::raw(name.as_bytes()))
}

/// golem's record shape: user cells plus the `#key` identity cell, then
/// golem's projection rules (missing names omitted, `#key` selectable).
fn to_record(key: RecordKey, row: Row, names: Option<&[CellName]>) -> Result<Record> {
    let mut cells = BTreeMap::new();
    cells.insert(golem_name("#key"), CellValue::from_bytes32(key.0));
    for (name, cell) in &row.cells {
        cells.insert(golem_name(name.as_str()), from_cell(cell)?);
    }
    if let Some(names) = names {
        cells.retain(|name, _| names.contains(name));
    }
    Ok(Record { key, cells })
}

/// golem system records are not engine records; answer with their identity
/// (and the deployment limits for `#params`) so reads succeed as in golem.
fn system_record(genesis: &Genesis, key: RecordKey, names: Option<&[CellName]>) -> Record {
    let limits = genesis.cell_limits;
    let mut cells = BTreeMap::new();
    cells.insert(golem_name("#key"), CellValue::from_bytes32(key.0));
    if key.0 == system::PARAMS.key {
        cells.insert(
            golem_name("#maxCellNameLen"),
            CellValue::from_u32(limits.max_cell_name_len),
        );
        cells.insert(
            golem_name("#maxStrLen"),
            CellValue::from_u32(limits.max_str_len),
        );
        cells.insert(
            golem_name("#maxBytesLen"),
            CellValue::from_u32(limits.max_bytes_len),
        );
    }
    if let Some(names) = names {
        cells.retain(|name, _| names.contains(name));
    }
    Record { key, cells }
}

impl Api for ArkivDb {
    fn create(&self, branch: BranchId, operation: RecordOp<op::Create>) -> Result<RecordKey> {
        let key = operation.record_key();
        reject_reserved(key)?;
        self.with_open(branch, |slot| {
            let mut cells = Vec::with_capacity(operation.cells().len());
            for (name, value) in operation.cells() {
                cells.push((self.check_name(name)?, self.check_value(value)?));
            }
            let op = WriteOp::CreateRecord {
                namespace: self.namespace,
                key: arkiv_key(key),
                cells,
            };
            self.execute(slot, op, true)?;
            Ok(key)
        })
    }

    fn get(&self, target: ReadTarget, operation: RecordOp<op::Get>) -> Result<Record> {
        let key = operation.record_key();
        let names = operation.names();
        let system = system::by_key(&key.0).is_some();
        let row = match target {
            ReadTarget::Branch(branch) => self.with_slot(
                branch,
                || ApiError::HandleInvalid,
                |slot| {
                    let slot = slot.as_mut().ok_or(ApiError::HandleInvalid)?;
                    if system {
                        return Ok(None);
                    }
                    self.read_branch(slot, &arkiv_key(key))
                },
            )?,
            ReadTarget::Head | ReadTarget::Commit(_) => {
                let snapshot = {
                    let mut backend = lock(&self.backend, "engine")?;
                    if !self.healthy() {
                        self.heal(&mut backend)?;
                    }
                    let head = backend.head();
                    match target {
                        ReadTarget::Commit(requested) => {
                            match requested.get().checked_add(self.offset) {
                                Some(seq) if seq == head.sequence() => backend.head_snapshot(),
                                // Unlike golem-db, the engine keeps history.
                                Some(seq) if seq < head.sequence() => {
                                    backend.snapshot(SnapshotId::from_sequence(seq))?
                                }
                                _ => {
                                    return Err(ApiError::CommitUnavailable {
                                        requested,
                                        head: self.commit_id(head),
                                    });
                                }
                            }
                        }
                        _ => backend.head_snapshot(),
                    }
                };
                if system {
                    None
                } else {
                    self.read(Source::Snapshot(&snapshot), &arkiv_key(key))?
                }
            }
        };
        if system {
            return Ok(system_record(&self.genesis, key, names));
        }
        let row = row.ok_or(ApiError::NotFound)?;
        to_record(key, row, names)
    }

    fn patch(&self, branch: BranchId, operation: RecordOp<op::Patch>) -> Result<()> {
        let key = operation.record_key();
        reject_reserved(key)?;
        self.with_open(branch, |slot| {
            let mut set = Vec::new();
            let mut remove = Vec::new();
            for (name, change) in operation.changes() {
                let arkiv_name = self.check_name(name)?;
                match change {
                    Some(value) => set.push((arkiv_name, self.check_value(value)?)),
                    None => remove.push(arkiv_name),
                }
            }
            let key = arkiv_key(key);
            let empty = set.is_empty() && remove.is_empty();
            // Whether the patch reactivates a rolled-back frame, i.e. adds a
            // golem journal entry. Only then does it matter, and only then
            // does the adapter read the record first.
            let resets = if empty {
                false
            } else if slot.rolled_back {
                let row = self.read_branch(slot, &key)?.ok_or(ApiError::NotFound)?;
                let current =
                    |name: &ArkivName| row.cells.iter().find(|(n, _)| n == name).map(|(_, c)| c);
                set.iter().any(|(name, cell)| current(name) != Some(cell))
                    || remove.iter().any(|name| current(name).is_some())
            } else {
                true
            };
            let op = WriteOp::PatchRecord {
                namespace: self.namespace,
                key,
                set,
                remove,
            };
            if empty {
                // The engine implements golem's empty patch: NotFound for a
                // missing record, otherwise a no-op. Nothing to log.
                let live = self.live(slot)?;
                let result = live.branch.try_execute(ACTOR, op, Budget::unlimited());
                if live.branch.is_poisoned() {
                    slot.live = None;
                }
                return result.map_err(storage)?.map(drop).map_err(rejection);
            }
            self.execute(slot, op, resets)
        })
    }

    fn delete(&self, branch: BranchId, operation: RecordOp<op::Delete>) -> Result<()> {
        let key = operation.record_key();
        reject_reserved(key)?;
        self.with_open(branch, |slot| {
            let op = WriteOp::DeleteRecord {
                namespace: self.namespace,
                key: arkiv_key(key),
            };
            self.execute(slot, op, true)
        })
    }

    fn head(&self) -> Result<CommitId> {
        Ok(self.commit_id(self.head_id()))
    }

    fn begin(&self) -> Result<BranchId> {
        let live = {
            let mut backend = lock(&self.backend, "engine")?;
            if !self.healthy() {
                self.heal(&mut backend)?;
            }
            let mut branch = backend.begin(backend.head())?;
            let checkpoint = branch.try_checkpoint().map_err(engine)?;
            Live {
                branch,
                checkpoints: vec![checkpoint],
                epoch: self.epoch.load(Ordering::Acquire),
            }
        };
        let id = NEXT_BRANCH
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| internal("branch id space exhausted"))?;
        let slot = Slot {
            base: live.branch.base(),
            log: Vec::new(),
            frames: vec![0],
            rolled_back: false,
            sealed: None,
            live: Some(live),
            version: Some(0),
        };
        lock(&self.branches, "registry")?.insert(id, Arc::new(Mutex::new(Some(slot))));
        Ok(BranchId::new(id))
    }

    fn branch_info(&self, branch: BranchId) -> Result<BranchInfo> {
        self.with_slot(
            branch,
            || ApiError::HandleInvalid,
            |slot| {
                let slot = slot.as_mut().ok_or(ApiError::HandleInvalid)?;
                Ok(BranchInfo {
                    commit_id: self.commit_id(slot.base),
                    branch_id: branch,
                    version: self.version(slot)?,
                    sealed: slot.sealed.is_some(),
                })
            },
        )
    }

    fn checkpoint(&self, branch: BranchId) -> Result<()> {
        self.with_open(branch, |slot| {
            // A rolled-back frame has been consumed: reuse its marker.
            if !slot.rolled_back {
                slot.frames.push(slot.log.len());
                if let Some(live) = &mut slot.live {
                    match live.branch.try_checkpoint() {
                        Ok(checkpoint) => live.checkpoints.push(checkpoint),
                        // Poisoned: the log keeps the frame; rebuild later.
                        Err(_) => slot.live = None,
                    }
                }
            }
            slot.rolled_back = false;
            Ok(())
        })
    }

    fn rollback(&self, branch: BranchId) -> Result<()> {
        self.with_open(branch, |slot| {
            if slot.rolled_back {
                if slot.frames.len() == 1 {
                    return Err(ApiError::NoFrameToRollback);
                }
                slot.frames.pop();
                if let Some(live) = &mut slot.live {
                    live.checkpoints.pop();
                }
            }
            let start = *slot.frames.last().expect("the first frame is never popped");
            slot.log.truncate(start);
            slot.version = None;
            if let Some(live) = &mut slot.live {
                let checkpoint = *live.checkpoints.last().expect("one per frame");
                if live.branch.rollback_to(checkpoint).is_err() {
                    slot.live = None;
                }
            }
            slot.rolled_back = true;
            Ok(())
        })
    }

    fn seal(&self, branch: BranchId) -> Result<SealInfo> {
        self.with_slot(
            branch,
            || ApiError::HandleInvalid,
            |slot| {
                let slot = slot.as_mut().ok_or(ApiError::HandleInvalid)?;
                self.seal_slot(slot)
            },
        )
    }

    fn commit(&self, branch: BranchId) -> Result<CommitId> {
        self.with_slot(
            branch,
            || ApiError::Conflict,
            |slot_opt| {
                let slot = slot_opt.as_mut().ok_or(ApiError::HandleInvalid)?;
                self.seal_slot(slot)?;
                let mut backend = lock(&self.backend, "engine")?;
                self.rebuild(slot, &mut backend)?;
                // From here the slot keeps only its log and seal: a failed
                // publication is retried by rebuilding the branch.
                let live = slot.live.take().expect("rebuilt");
                let candidate = live.branch.try_seal().map_err(engine)?;
                let published = match &mut *backend {
                    Backend::Memory(e) => match e.publish(&candidate) {
                        Ok(id) => Ok(id),
                        Err(EngineError::StaleHead { .. }) => Err(ApiError::Conflict),
                        Err(error) => Err(engine(error)),
                    },
                    Backend::Durable(d) => self.publish_durable(d, candidate),
                };
                match published {
                    Ok(head) => {
                        self.head.store(head.sequence(), Ordering::Release);
                        drop(backend);
                        *slot_opt = None;
                        // Publication succeeded; registry housekeeping must
                        // not turn it into an apparent failure.
                        let _ = self.remove(branch);
                        Ok(self.commit_id(head))
                    }
                    Err(ApiError::Conflict) => {
                        drop(backend);
                        *slot_opt = None;
                        self.remove(branch)?;
                        Err(ApiError::Conflict)
                    }
                    Err(error) => Err(error),
                }
            },
        )
    }

    fn discard(&self, branch: BranchId) -> Result<()> {
        self.with_slot(
            branch,
            || ApiError::HandleInvalid,
            |slot| {
                *slot = None;
                Ok(())
            },
        )?;
        self.remove(branch)
    }

    fn immutable_data_append(
        &self,
        _: BranchId,
        _: &str,
        _: Option<ImmutableDataKey>,
        _: ImmutableDataRow,
    ) -> Result<ImmutableDataOrdinal> {
        Err(ApiError::NotImplemented {
            operation: "immutable_data_append",
        })
    }
    fn immutable_data_get(&self, _: &str, _: ImmutableDataAddress) -> Result<ImmutableDataRow> {
        Err(ApiError::NotImplemented {
            operation: "immutable_data_get",
        })
    }
    fn immutable_data_range_of(
        &self,
        _: &str,
        _: CommitId,
    ) -> Result<std::ops::Range<ImmutableDataOrdinal>> {
        Err(ApiError::NotImplemented {
            operation: "immutable_data_range_of",
        })
    }
    fn immutable_data_rows_of(&self, _: &str, _: CommitId) -> Result<Vec<ImmutableDataRow>> {
        Err(ApiError::NotImplemented {
            operation: "immutable_data_rows_of",
        })
    }
}
