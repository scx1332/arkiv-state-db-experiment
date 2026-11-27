//! golem-db's `Api` on the arkiv engine (`crates/arkiv-db`).
//!
//! Mapping:
//! - one fixed namespace (`golemdb`), created by the adapter in the first
//!   engine commit; golem-db commit N is engine snapshot N + 1;
//! - `RecordKey` (32 bytes) -> engine `RecordKey` (1..=32 bytes);
//! - attributes / fields -> engine `Cell::attribute` / `Cell::field`. The
//!   engine's type ids are golem's cell type ids and its fixed-width term
//!   bytes are golem's stored payloads, so a cell converts by id and payload
//!   (see [`to_value`] and [`from_cell`]);
//! - golem's `Genesis` becomes the engine's genesis: `cell_limits` map onto
//!   the engine's [`Limits`]; the engine deployment id is golem's genesis
//!   identity (its `golemdb/genesis/v1` formula over the genesis cells), so
//!   every genesis setting takes part in `GenesisMismatch`. Only keccak-256
//!   is supported: a Blake3 genesis is refused at open;
//! - writes: golem's admission ([`crate::admission`]), then existence, then
//!   golem's per-record caps, then the engine write. A patch reads the
//!   record first and sends the engine only the cells that change;
//! - reads add the `#key` and `#meta` cells golem stores per record, and ask
//!   the engine only for the cells a projection names;
//! - reserved records (`#params`, `#alloc`, `#roots`, `#recordKeys`,
//!   `#rootIndex`, `@meteringModel`, `@modelWeight`) are derived on read:
//!   from the genesis cells, the engine's record allocator and ids, and the
//!   state roots of the retained snapshots;
//! - `query` / `count` run on the engine's query form ([`crate::query`]);
//! - receipts: `priced_at` and `details` are golem's; `cost` is 0, the
//!   ledger is empty and budgets are not enforced (no pricing model);
//! - `BranchId` -> an adapter registry slot holding an engine `Branch` plus
//!   the log of the slot's applied writes. Checkpoint/rollback frames are
//!   positions in that log (golem's journal-frame rules), backed by engine
//!   `Checkpoint`s;
//! - `BranchInfo::version` (golem's undo-entry count) is the sum of the
//!   undo entries each logged write stands for;
//! - stale base -> `HandleInvalid` on every branch call, `Conflict` on commit
//!   (golem's rule: a stale handle is consumed when touched);
//! - storage failures: `StoreFull` for a full medium, otherwise `Internal`.
//!   As in golem a failed call leaves the branch as it was and can be
//!   retried: a branch the engine poisoned is rebuilt from its log, and the
//!   durable engine is recovered first;
//! - `SealInfo::index_root` / `OpenInfo::index_root` are zeros (the term
//!   index is part of the state the state root commits to), and the
//!   immutable-data calls are not served.
//!
//! Two storage backends: the volatile in-memory `Engine` and the durable
//! `DurableEngine` on a `RedbStore` file.
//!
//! History: golem-db `main` serves the head only, so `ReadTarget::Commit(n)`
//! and `query(Some(n))` for another commit are `CommitUnavailable` unless
//! `ArkivOptions::serve_history` (`ARKIV_SERVE_HISTORY=1`) is set; then every
//! commit the engine's state retention keeps is served
//! (`ArkivOptions::retention`, `ARKIV_STATE_RETENTION=archive|<N>`, or
//! golem's `OpenConfig::pruning` window).

use std::collections::{BTreeMap, HashMap};
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use arkiv_db::durable::{
    DurableEngine, DurableError, EngineGenesis, Initialization, Options, Publication, Recovery,
    Retention, WriterState,
};
use arkiv_db::{
    Branch, Budget, Cell, CellKind as ArkivKind, CellName as ArkivName, Checkpoint, Effect, Engine,
    EngineError, Limits, NamespaceId, NamespaceName, OwnerId, PriceSchedule, Projection,
    RecordKey as ArkivKey, Rejection, Row, Snapshot, SnapshotId, StorageError, TypeTag, Value,
    WriteOp,
};
use arkiv_db_redb::{RedbConfig, RedbStore};
use arkiv_db_storage::versioned::{Health, SpaceId, StorageErrorKind, Table, VersionedStore};
use golemdb_cells::{CellKey, reserved, system};
use golemdb_merkle::{HashProvider, Keccak256Hasher};
use golemdb_metering::{ModelMetadata, model_cells};
use real::{
    Api, ApiError, BranchId, BranchInfo, CellKind, CellName, CellNameRef, CellType, CellValue,
    CommitId, CountOp, Details, Genesis, HashAlgorithm, ImmutableDataAddress, ImmutableDataKey,
    ImmutableDataOrdinal, ImmutableDataReadOptions, ImmutableDataRow, ImmutableDataSchema, Metered,
    MeteringModelInfo, MeteringModelMetadata, ModelWeights, OpenConfig, OpenError, OpenInfo,
    OpenMode, OpenResult, QueryOp, QueryPage, ReadTarget, Record, RecordKey, RecordOp, Result,
    SealInfo, op,
};

use crate::admission::{Cells, Change, admit_create, admit_patch, check_caps, effect, meta_of};
use crate::immutable::{ImmutableStore, Staging, api_error};
use crate::query::{self, Counters, Request};

const ACTOR: OwnerId = OwnerId::new([0xa1; 20]);
const NAMESPACE: &str = "golemdb";
/// The durable backend's file inside the database directory.
pub const FILE: &str = "arkiv.redb";
/// golem-db's stored-format ids, hashed into the genesis identity.
const FORMAT: u32 = 2;
const ROARING: u16 = 1;
const KECCAK_ID: u16 = 1;
/// The engine's per-record cell cap is a `usize` it sizes objects by; golem's
/// `max_cells` above this is enforced by the adapter only up to here.
const ENGINE_MAX_CELLS: u64 = 1024;
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
    /// Whether every commit also saves redb's allocator state, so that the
    /// file opens without a repair after an unclean stop (the node's
    /// setting, and the default).
    pub redb_quick_repair: bool,
    /// Engine encoded-object cache, bytes and entries.
    pub object_cache: usize,
    pub object_cache_entries: usize,
    /// State-history retention of the durable backend (archive by default).
    pub retention: Retention,
    /// Whether `ReadTarget::Commit(n)` and `query(Some(n))` serve retained
    /// commits other than the head. golem-db `main` serves the head only
    /// (`CommitUnavailable` otherwise), so the default is `false`.
    pub serve_history: bool,
}

impl ArkivOptions {
    /// `ARKIV_STATE_RETENTION` (`archive` or a window in commits) if set and
    /// nonempty, else archive.
    pub fn retention_from_env() -> Retention {
        match std::env::var("ARKIV_STATE_RETENTION") {
            Ok(value) if !value.is_empty() => value
                .parse()
                .unwrap_or_else(|e| panic!("ARKIV_STATE_RETENTION: {e}")),
            _ => Retention::Archive,
        }
    }

    /// `ARKIV_SERVE_HISTORY=1` serves retained non-head commits.
    pub fn serve_history_from_env() -> bool {
        std::env::var("ARKIV_SERVE_HISTORY").is_ok_and(|v| v == "1")
    }

    /// The defaults with every `ARKIV_*` variable applied: the two above,
    /// `ARKIV_REDB_CACHE_MIB`, `ARKIV_REDB_QUICK_REPAIR` (`0` or `1`),
    /// `ARKIV_OBJECT_CACHE_MIB` and `ARKIV_OBJECT_CACHE_ENTRIES`.
    pub fn from_env() -> Self {
        fn number(name: &str) -> Option<usize> {
            let value = std::env::var(name).ok().filter(|v| !v.is_empty())?;
            Some(
                value
                    .parse()
                    .unwrap_or_else(|e| panic!("{name}: {value:?}: {e}")),
            )
        }
        let mut options = Self {
            retention: Self::retention_from_env(),
            serve_history: Self::serve_history_from_env(),
            ..Self::default()
        };
        if let Some(mib) = number("ARKIV_REDB_CACHE_MIB") {
            options.redb_cache = mib << 20;
        }
        if let Some(flag) = number("ARKIV_REDB_QUICK_REPAIR") {
            options.redb_quick_repair = flag != 0;
        }
        if let Some(mib) = number("ARKIV_OBJECT_CACHE_MIB") {
            options.object_cache = mib << 20;
        }
        if let Some(entries) = number("ARKIV_OBJECT_CACHE_ENTRIES") {
            options.object_cache_entries = entries;
        }
        options
    }
}

impl Default for ArkivOptions {
    fn default() -> Self {
        Self {
            redb_cache: 256 << 20,
            redb_quick_repair: true,
            // The engine's own defaults (durable::Options::default()).
            object_cache: 8 << 20,
            object_cache_entries: 16_384,
            retention: Retention::Archive,
            serve_history: false,
        }
    }
}

/// `ARKIV_STATS_LOG=<file>`: a diagnostic log of what the store wrote, one
/// `C` line per commit and the size of every table (`T` lines) every
/// `ARKIV_STATS_EVERY` commits (default 500; the table walk is not free).
///
/// `C,commit,file bytes,backend writes,backend bytes written,syncs,records
/// written,entries removed,` then `objects written,object bytes` for the
/// catalog, host and namespace spaces, all cumulative since the open.
/// `T,commit,table,entries,stored bytes,page bytes`.
fn stats_log(d: &Durable) {
    use std::io::Write;
    static LOG: std::sync::OnceLock<Option<(Mutex<std::fs::File>, u64)>> =
        std::sync::OnceLock::new();
    static COMMITS: AtomicU64 = AtomicU64::new(0);
    let Some((file, every)) = LOG.get_or_init(|| {
        let path = std::env::var("ARKIV_STATS_LOG")
            .ok()
            .filter(|p| !p.is_empty())?;
        let every = std::env::var("ARKIV_STATS_EVERY")
            .ok()
            .and_then(|n| n.parse().ok())
            .filter(|n| *n > 0)
            .unwrap_or(500);
        let file =
            std::fs::File::create(&path).unwrap_or_else(|e| panic!("ARKIV_STATS_LOG {path}: {e}"));
        Some((Mutex::new(file), every))
    }) else {
        return;
    };
    let commit = COMMITS.fetch_add(1, Ordering::Relaxed) + 1;
    let stats = d.store.stats();
    let backend = stats.backend.unwrap_or_default();
    let mut line = format!(
        "C,{commit},{},{},{},{},{},{}",
        std::fs::metadata(&d.path).map_or(0, |m| m.len()),
        backend.writes,
        backend.write_bytes,
        backend.syncs,
        stats.records_written,
        stats.removed,
    );
    let mut namespace = (0, 0);
    for (space, counters) in &stats.spaces {
        if matches!(space, SpaceId::Namespace(_)) {
            namespace.0 += counters.objects_written;
            namespace.1 += counters.bytes_written;
        }
    }
    for (objects, bytes) in [
        {
            let s = stats.space(SpaceId::Catalog);
            (s.objects_written, s.bytes_written)
        },
        {
            let s = stats.space(SpaceId::Host);
            (s.objects_written, s.bytes_written)
        },
        namespace,
    ] {
        line.push_str(&format!(",{objects},{bytes}"));
    }
    line.push('\n');
    if commit.is_multiple_of(*every)
        && let Ok(tables) = d.store.table_stats()
    {
        for (name, entries, stored, pages) in tables {
            line.push_str(&format!("T,{commit},{name},{entries},{stored},{pages}\n"));
        }
    }
    if let Ok(mut file) = file.lock() {
        let _ = file.write_all(line.as_bytes());
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

pub(crate) fn internal(message: impl Into<String>) -> ApiError {
    ApiError::internal(AdapterError(message.into()))
}

/// golem maps a full store to `StoreFull` and every other storage failure
/// to `Internal`, keeping the storage error as the source.
pub(crate) fn storage(error: StorageError) -> ApiError {
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
    /// The volatile engine keeps every snapshot; the adapter applies the
    /// retention floor to its reads so both backends answer alike.
    Memory(Engine, Retention),
    Durable(Box<Durable>),
}

impl Backend {
    fn head(&self) -> SnapshotId {
        match self {
            Self::Memory(e, _) => e.head(),
            Self::Durable(d) => d.engine.head(),
        }
    }
    fn head_snapshot(&self) -> Snapshot {
        match self {
            Self::Memory(e, _) => e.head_snapshot(),
            Self::Durable(d) => d.engine.head_snapshot(),
        }
    }
    fn snapshot(&self, id: SnapshotId) -> Result<Snapshot> {
        match self {
            Self::Memory(e, _) => e.snapshot(id).map_err(engine),
            Self::Durable(d) => d.engine.snapshot(id).map_err(durable),
        }
    }
    /// The lowest snapshot the backend serves.
    fn floor(&self) -> u64 {
        match self {
            Self::Memory(e, retention) => retention.floor_at(e.head().sequence()),
            Self::Durable(d) => d.engine.state_floor().sequence(),
        }
    }
    fn begin(&self, base: SnapshotId) -> Result<Branch> {
        match self {
            Self::Memory(e, _) => e.begin(base, None).map_err(engine),
            Self::Durable(d) => d.engine.begin(base, None).map_err(durable),
        }
    }
    fn state_root(&self) -> [u8; 32] {
        match self {
            Self::Memory(e, _) => e.head_snapshot().state_root().0,
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
    /// golem undo entries of each logged write; their sum is
    /// `BranchInfo::version`.
    undo: Vec<u64>,
    /// Immutable rows staged on the branch. They are no part of the log:
    /// frames, rollback and the seal leave them alone.
    immutable: Staging,
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
    pub(crate) namespace: NamespaceId,
    /// Engine snapshot sequence of golem-db commit 0.
    offset: u64,
    genesis: Genesis,
    /// golem-db's genesis cells, from which reserved records are served.
    cells: BTreeMap<CellKey, CellValue>,
    serve_history: bool,
    /// The immutable-data store, when the genesis declares segments.
    immutable: Option<ImmutableStore>,
    pub(crate) counters: Counters,
    info: OpenInfo,
    pub description: String,
}

/// What golem-db's genesis fixes for one database.
struct Deployment {
    genesis: Genesis,
    /// golem-db's genesis identity.
    id: [u8; 32],
    cells: BTreeMap<CellKey, CellValue>,
    serve_history: bool,
}

impl Deployment {
    fn new(genesis: &Genesis, id: [u8; 32], options: &ArkivOptions) -> OpenResult<Self> {
        Ok(Self {
            genesis: genesis.clone(),
            id,
            cells: genesis_cells(genesis)?,
            serve_history: options.serve_history,
        })
    }
}

/// golem-db's genesis cells, as `golemdb-api`'s `genesis::initial_cells`
/// writes them at commit 0. The adapter serves the reserved records from
/// them and hashes them into the genesis identity.
fn genesis_cells(config: &Genesis) -> OpenResult<BTreeMap<CellKey, CellValue>> {
    let mut cells = BTreeMap::new();
    for record in system::ALL {
        cells.insert(
            CellKey::new(record.id, reserved::KEY),
            CellValue::from_bytes32(record.key),
        );
        cells.insert(
            CellKey::new(system::RECORD_KEYS.id, CellNameRef::raw(&record.key)),
            CellValue::from_u64(record.id),
        );
    }
    let limits = config.cell_limits;
    for (name, value) in [
        (reserved::MAX_CELL_NAME_LEN, limits.max_cell_name_len),
        (reserved::MAX_STR_LEN, limits.max_str_len),
        (reserved::MAX_BYTES_LEN, limits.max_bytes_len),
    ] {
        cells.insert(
            CellKey::new(system::PARAMS.id, name),
            CellValue::from_u32(value),
        );
    }
    for (name, value) in [
        (reserved::MAX_RECORD_CELLS, config.record_limits.max_cells),
        (
            reserved::MAX_RECORD_INDEXED_CELLS,
            config.record_limits.max_indexed_cells,
        ),
    ] {
        cells.insert(
            CellKey::new(system::PARAMS.id, name),
            CellValue::from_u64(value),
        );
    }
    cells.insert(
        CellKey::new(system::ALLOC.id, reserved::NEXT_RECORD_ID),
        CellValue::from_u64(system::FIRST_USER_RECORD_ID),
    );
    cells.extend(model_cells(
        &ModelMetadata {
            version: 1,
            activation_commit: 0,
            golemdb_code_version: config.metering.golemdb_code_version,
            host_code_version: config.metering.host_code_version,
        },
        &config.metering.weights,
    ));
    if let Some(immutable_data) = &config.immutable_data {
        cells.insert(
            CellKey::new(system::PARAMS.id, reserved::SHARD_SPAN),
            CellValue::from_u64(immutable_data.shard_span),
        );
        let segments = immutable_data
            .encode_segments()
            .map_err(|error| OpenError::InvalidConfig(error.to_string()))?;
        cells.insert(
            CellKey::new(system::PARAMS.id, reserved::IMMUTABLE_DATA_SEGMENTS),
            CellValue::from_bytes(&segments),
        );
    }
    Ok(cells)
}

/// golem-db's genesis identity formula (`golemdb/genesis/v1`), keccak-256.
fn genesis_id(cells: &BTreeMap<CellKey, CellValue>) -> [u8; 32] {
    let mut bytes = b"golemdb/genesis/v1\0".to_vec();
    bytes.extend_from_slice(&FORMAT.to_be_bytes());
    bytes.extend_from_slice(&KECCAK_ID.to_be_bytes());
    bytes.extend_from_slice(&ROARING.to_be_bytes());
    bytes.extend_from_slice(&(cells.len() as u32).to_be_bytes());
    for (key, value) in cells {
        let key = key.encode();
        bytes.extend_from_slice(&(key.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&key);
        let value = value.encoded_bytes();
        bytes.extend_from_slice(&(value.len() as u32).to_be_bytes());
        bytes.extend_from_slice(value);
    }
    Keccak256Hasher.hash(&bytes)
}

/// golem's genesis -> the engine's genesis.
///
/// The engine has one hash (keccak-256), so a Blake3 genesis is refused, as
/// golem refuses settings its backend cannot hold. golem's three cell limits
/// map one to one. golem indexes a `str` attribute up to `max_str_len`, so
/// that is also the engine's indexed-string limit; a value above the
/// engine's caps (32-byte names, 512-byte indexed values) is refused rather
/// than silently narrowed. The engine's deployment id is golem's genesis
/// identity, which covers every genesis setting (record caps, pricing and
/// immutable-data declarations included), so reopening with any of them
/// changed is a `GenesisMismatch`. golem's per-record caps are enforced by
/// the adapter; the engine's own cell cap is set at or above `max_cells`.
pub fn engine_genesis(genesis: &Genesis) -> OpenResult<(EngineGenesis, [u8; 32])> {
    if genesis.hash_function != HashAlgorithm::Keccak256 {
        return Err(OpenError::InvalidConfig(format!(
            "unsupported hash function {:?}: the arkiv engine supports only keccak-256",
            genesis.hash_function
        )));
    }
    if let Some(config) = &genesis.immutable_data {
        config
            .validate()
            .map_err(|error| OpenError::InvalidConfig(error.to_string()))?;
        if config.segments.is_empty() {
            return Err(OpenError::InvalidConfig(
                "immutable_data needs at least one segment; use None or omit it to disable".into(),
            ));
        }
    }
    let golem = genesis.cell_limits;
    if golem.max_str_len as usize > Value::INDEXED_LEN_CEILING {
        return Err(OpenError::InvalidConfig(format!(
            "max_str_len {} exceeds the arkiv engine's indexed-value cap of {} bytes \
             (golem indexes str attributes up to max_str_len)",
            golem.max_str_len,
            Value::INDEXED_LEN_CEILING
        )));
    }
    let limits = Limits {
        max_cells: engine_max_cells(genesis),
        max_cell_name_len: golem.max_cell_name_len as usize,
        max_str_len: golem.max_str_len as usize,
        max_bytes_len: golem.max_bytes_len as usize,
        max_indexed_len: golem.max_str_len as usize,
        ..Limits::DEFAULT
    };
    limits
        .validate()
        .map_err(|reason| OpenError::InvalidConfig(format!("arkiv engine limits: {reason}")))?;
    let id = genesis_id(&genesis_cells(genesis)?);
    let mut engine_genesis = EngineGenesis::new(id, PriceSchedule::FIXTURE);
    engine_genesis.limits = limits;
    // A stored object must hold the largest admissible row.
    let row = limits.max_cells as u64 * (limits.max_payload_len() as u64 + 64);
    engine_genesis.max_object_bytes = (16 << 20).max(2 * row);
    engine_genesis
        .validate()
        .map_err(|e| OpenError::InvalidConfig(format!("arkiv engine genesis: {e}")))?;
    Ok((engine_genesis, id))
}

fn engine_max_cells(genesis: &Genesis) -> usize {
    genesis.record_limits.max_cells.clamp(1, ENGINE_MAX_CELLS) as usize
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
        retention: options.retention,
        ..Options::default()
    }
}

impl ArkivDb {
    /// Volatile engine; all history retained in memory.
    pub fn open_memory(genesis: &Genesis, options: ArkivOptions) -> OpenResult<Self> {
        let (engine_genesis, id) = engine_genesis(genesis)?;
        genesis.metering.validate(&[])?;
        let engine = Engine::with_limits(PriceSchedule::FIXTURE, engine_genesis.limits);
        Self::setup(
            Backend::Memory(engine, options.retention),
            None,
            Deployment::new(genesis, id, &options)?,
            "arkiv memory Engine".into(),
        )?
        .with_immutable_data(None)
    }

    /// Durable engine in `dir/arkiv.redb`, with golem's opening modes:
    /// `CreateIfMissing` initializes pristine storage or validates and
    /// reopens it, `ExistingOnly` never initializes, `CreateNew` requires
    /// pristine storage. Reopening checks the genesis (`GenesisMismatch`).
    ///
    /// golem's node-local `OpenConfig::pruning` window is the engine's state
    /// retention window: the head and that many commits before it.
    pub fn open(dir: &Path, config: &OpenConfig, mut options: ArkivOptions) -> OpenResult<Self> {
        engine_genesis(&config.genesis)?;
        if let Some(pruning) = config.pruning {
            options.retention = Retention::Window(pruning.window());
        }
        let path = dir.join(FILE);
        let present = std::fs::metadata(&path).is_ok_and(|m| m.len() > 0);
        let redb = RedbConfig {
            cache_size: options.redb_cache,
            quick_repair: options.redb_quick_repair,
        };
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
        // golem's default: the files live beside the database.
        let directory = config
            .immutable_data_directory
            .clone()
            .unwrap_or_else(|| dir.join("immutable-data"));
        Self::open_engine(Arc::new(store), path, config, options)?
            .with_immutable_data(Some(&directory))
    }

    /// The immutable-data store of a freshly opened database: in
    /// `directory`, or in memory without one; none when the genesis declares
    /// no segment (and then no directory is created). Rows of a commit the
    /// engine did not publish are dropped before the store is used.
    fn with_immutable_data(mut self, directory: Option<&Path>) -> OpenResult<Self> {
        let Some(config) = &self.genesis.immutable_data else {
            return Ok(self);
        };
        let id = self.info.genesis_id;
        let store = match directory {
            Some(directory) => ImmutableStore::open(config, id, directory)?,
            None => ImmutableStore::in_memory(config, id)?,
        };
        store
            .recover(self.info.commit_id.get())
            .map_err(OpenError::ImmutableData)?;
        self.immutable = Some(store);
        Ok(self)
    }

    /// The durable engine on an already opened store (the kit's tests use
    /// it to inject storage faults).
    ///
    /// Immutable data goes to `config.immutable_data_directory`, or stays in
    /// memory when there is none.
    pub fn open_store(
        store: Arc<RedbStore>,
        path: PathBuf,
        config: &OpenConfig,
        options: ArkivOptions,
    ) -> OpenResult<Self> {
        Self::open_engine(store, path, config, options)?
            .with_immutable_data(config.immutable_data_directory.as_deref())
    }

    fn open_engine(
        store: Arc<RedbStore>,
        path: PathBuf,
        config: &OpenConfig,
        options: ArkivOptions,
    ) -> OpenResult<Self> {
        let (engine_genesis, id) = engine_genesis(&config.genesis)?;
        let deployment = Deployment::new(&config.genesis, id, &options)?;
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
            // As golem: only a new deployment validates its genesis model.
            config.genesis.metering.validate(&config.host_schemas)?;
            let initialized = DurableEngine::initialize(
                versioned,
                engine_genesis.clone(),
                engine_options.clone(),
            )
            .map_err(|e| open_existing(store.as_ref(), &engine_genesis, e))?;
            let mut engine = match initialized {
                Initialization::Committed(engine) => engine,
                Initialization::NotCommitted { error, .. }
                | Initialization::Indeterminate { error, .. } => return Err(open_storage(error)),
            };
            // With a retention window the store deletes what no retained
            // commit reaches, as the node's does. It is turned on here, on
            // the empty state, because turning it on later means a scan of
            // every object; a file created under archive retention and
            // reopened with a window keeps everything, like the node's.
            if engine_options.retention.window().is_some() {
                engine
                    .enable_gc(None)
                    .map_err(|e| open_existing(store.as_ref(), &engine_genesis, e))?;
            }
            let backend = Backend::Durable(Box::new(Durable {
                engine,
                store: store.clone(),
                path,
                genesis: engine_genesis.clone(),
                options: engine_options,
            }));
            return Self::setup(backend, Some(store), deployment, description);
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
            deployment,
            false,
            description,
        ))
    }

    fn new(
        backend: Backend,
        store: Option<Arc<RedbStore>>,
        namespace: NamespaceId,
        offset: u64,
        deployment: Deployment,
        created: bool,
        description: String,
    ) -> Self {
        let head = backend.head();
        let info = OpenInfo {
            created,
            commit_id: CommitId::new(head.sequence() - offset),
            state_root: backend.state_root(),
            index_root: INDEX_ROOT,
            genesis_id: deployment.id,
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
            genesis: deployment.genesis,
            cells: deployment.cells,
            serve_history: deployment.serve_history,
            immutable: None,
            counters: Counters::default(),
            info,
            description,
        }
    }

    /// golem commit 0: the engine commit creating the adapter's namespace.
    fn setup(
        mut backend: Backend,
        store: Option<Arc<RedbStore>>,
        deployment: Deployment,
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
            Backend::Memory(e, _) => {
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
            deployment,
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

    /// The engine's per-record cell cap, which also bounds the names of a
    /// named projection.
    pub(crate) fn engine_max_cells(&self) -> usize {
        engine_max_cells(&self.genesis)
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
            Backend::Memory(e, retention) => {
                e.snapshot_count() as u64 - retention.floor_at(e.head().sequence())
            }
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
                    mib(stats.cache.retained_bytes as u64)
                ));
                // Retained snapshots: the head down to the state floor.
                d.engine.snapshot_count().map_err(durable)? - d.engine.state_floor().sequence()
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

    /// Executes `op` on the slot's branch and logs it with the number of
    /// golem undo entries it stands for. A rejected write changes nothing; a
    /// storage failure poisons the engine branch, which is then dropped and
    /// rebuilt from the log, so the failed write leaves no trace (golem
    /// rewinds a failed write group). Any applied write reactivates a
    /// rolled-back frame (golem: any journal entry).
    fn execute(&self, slot: &mut Slot, op: WriteOp, undo: u64) -> Result<()> {
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
                slot.undo.push(undo);
                slot.rolled_back = false;
                Ok(())
            }
        }
    }

    /// The engine name of an admitted golem name. golem's and the engine's
    /// grammars agree; the engine's hard 32-byte cap is refused at genesis.
    fn engine_name(&self, name: &CellName) -> Result<ArkivName> {
        let text = std::str::from_utf8(name.as_bytes())
            .map_err(|_| ApiError::invalid_argument("arkiv cell name: not UTF-8"))?;
        ArkivName::new(text)
            .map_err(|e| ApiError::invalid_argument(format!("arkiv cell name {text:?}: {e}")))
    }

    fn engine_cell(&self, value: &CellValue) -> Result<Cell> {
        let converted = to_value(value)?;
        Ok(match value.kind() {
            CellKind::Attribute => Cell::attribute(converted),
            CellKind::Field => Cell::field(converted),
        })
    }

    fn read(
        &self,
        source: Source<'_>,
        key: &ArkivKey,
        projection: &Projection,
    ) -> Result<Option<Row>> {
        let result = match source {
            Source::Branch(branch) => {
                branch.try_get_record(self.namespace, key, projection, Budget::unlimited())
            }
            Source::Snapshot(snapshot) => {
                snapshot.try_get_record(self.namespace, key, projection, Budget::unlimited())
            }
        };
        let metered = result
            .map_err(storage)?
            .map_err(|r| internal(format!("get_record: {r}")))?;
        Ok(metered.value)
    }

    /// A read through the slot's branch (sealed branches stay readable).
    fn read_branch(
        &self,
        slot: &mut Slot,
        key: &ArkivKey,
        projection: &Projection,
    ) -> Result<Option<Row>> {
        let live = self.live(slot)?;
        let result = self.read(Source::Branch(&live.branch), key, projection);
        if live.branch.is_poisoned() {
            slot.live = None;
        }
        result
    }

    /// The committed state `commit` names (`None`: the head) and its commit
    /// id, under golem's rule: the head only, unless history is served.
    pub(crate) fn committed(
        &self,
        commit: Option<CommitId>,
        priced_at: &mut Option<CommitId>,
    ) -> Result<(Snapshot, CommitId)> {
        let mut backend = lock(&self.backend, "engine")?;
        if !self.healthy() {
            self.heal(&mut backend)?;
        }
        let head = backend.head();
        let head_commit = self.commit_id(head);
        let Some(requested) = commit else {
            *priced_at = Some(head_commit);
            return Ok((backend.head_snapshot(), head_commit));
        };
        match requested.get().checked_add(self.offset) {
            Some(seq) if seq == head.sequence() => {
                *priced_at = Some(head_commit);
                Ok((backend.head_snapshot(), head_commit))
            }
            Some(seq) if self.serve_history && seq < head.sequence() && seq >= backend.floor() => {
                *priced_at = Some(requested);
                Ok((backend.snapshot(SnapshotId::from_sequence(seq))?, requested))
            }
            _ => {
                *priced_at = Some(head_commit);
                Err(ApiError::CommitUnavailable {
                    requested,
                    head: head_commit,
                })
            }
        }
    }

    /// The id the next created record receives in `snapshot`, as golem
    /// numbers records (user ids start at 64).
    fn next_record_id(&self, snapshot: &Snapshot) -> Result<u64> {
        let roots = snapshot
            .try_namespace_roots(self.namespace)
            .map_err(storage)?
            .ok_or_else(|| internal("the adapter namespace is missing"))?;
        Ok(golem_id(roots.next_record))
    }

    /// A reserved record at one view. golem stores these as cells; the
    /// adapter derives them: genesis cells, the allocator from the engine's,
    /// key bindings from the engine's record ids, and `#roots` from the
    /// retained snapshots below the view's commit.
    fn reserved_record(
        &self,
        record: system::SystemRecord,
        view: &mut View<'_>,
        names: Option<&[CellName]>,
    ) -> Result<Record> {
        let wanted =
            |name: &[u8]| names.is_none_or(|names| names.iter().any(|n| n.as_bytes() == name));
        let mut cells = BTreeMap::new();
        for (key, value) in &self.cells {
            if key.record_id() == record.id && wanted(key.name().as_bytes()) {
                cells.insert(CellName::from(key.name()), value.clone());
            }
        }
        if record.id == system::ALLOC.id && wanted(reserved::NEXT_RECORD_ID.as_bytes()) {
            let next = self.next_record_id(view.base)? + view.created;
            cells.insert(reserved::NEXT_RECORD_ID.into(), CellValue::from_u64(next));
        }
        if record.id == system::RECORD_KEYS.id {
            let binding = |id: u64| CellValue::from_u64(golem_id(id));
            match names {
                Some(names) => {
                    for name in names {
                        let Ok(key) = <[u8; 32]>::try_from(name.as_bytes()) else {
                            continue;
                        };
                        if system::by_key(&key).is_some() {
                            continue;
                        }
                        let key = arkiv_key(RecordKey(key));
                        let row = match &mut view.slot {
                            Some(slot) => self.read_branch(slot, &key, &Projection::ID_ONLY)?,
                            None => {
                                self.read(Source::Snapshot(view.base), &key, &Projection::ID_ONLY)?
                            }
                        };
                        if let Some(row) = row {
                            cells.insert(name.clone(), binding(row.id.ordinal()));
                        }
                    }
                }
                None => {
                    let rows = match &mut view.slot {
                        Some(slot) => {
                            let live = self.live(slot)?;
                            self.branch_keys(&live.branch)?
                        }
                        None => self.snapshot_keys(view.base)?,
                    };
                    for (id, key) in rows {
                        cells.insert(CellNameRef::raw(key.as_bytes()).into(), binding(id));
                    }
                }
            }
        }
        if record.id == system::ROOTS.id {
            let backend = lock(&self.backend, "engine")?;
            for commit in 0..view.commit.get() {
                let name = commit.to_be_bytes();
                let seq = commit + self.offset;
                if !wanted(&name) || seq < backend.floor() {
                    continue;
                }
                let snapshot = backend.snapshot(SnapshotId::from_sequence(seq))?;
                let mut roots = snapshot.state_root().0.to_vec();
                roots.extend_from_slice(&INDEX_ROOT);
                cells.insert(
                    CellNameRef::raw(&name).into(),
                    CellValue::from_bytes(&roots),
                );
            }
        }
        Ok(Record {
            key: RecordKey(record.key),
            cells,
        })
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
            index_root: INDEX_ROOT,
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
            Ok(Publication::Committed(info)) => {
                stats_log(d);
                Ok(info.snapshot)
            }
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

/// What a reserved record is derived from: the committed state under the
/// view, its commit, and for a branch its slot and the records it created.
struct View<'a> {
    base: &'a Snapshot,
    commit: CommitId,
    created: u64,
    slot: Option<&'a mut Slot>,
}

/// The engine has no separate index root: the term index is part of the
/// namespace state the state root commits to.
const INDEX_ROOT: [u8; 32] = [0; 32];

/// golem numbers user records from 64; the engine's ordinals start at 1.
fn golem_id(ordinal: u64) -> u64 {
    system::FIRST_USER_RECORD_ID - 1 + ordinal
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

pub(crate) fn type_tag(ty: CellType) -> Result<TypeTag> {
    TypeTag::from_id(ty.id()).ok_or_else(|| internal(format!("golem type {ty:?}")))
}

/// golem cell -> engine value: the same type id, and golem's payload is
/// the engine's term bytes for every fixed-width type.
pub(crate) fn to_value(value: &CellValue) -> Result<Value> {
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

/// The user cells of an engine row, as golem cells.
fn row_cells(row: &Row) -> Result<Cells> {
    row.cells
        .iter()
        .map(|(name, cell)| Ok((golem_name(name.as_str()), from_cell(cell)?)))
        .collect()
}

/// golem's record shape: user cells plus the `#key` identity cell and the
/// `#meta` counters, then golem's projection (missing names omitted).
///
/// `row` was read with [`query::projection`] of the same `names`, so it
/// holds every cell whenever `#meta`, which is derived from all of them, is
/// part of the result.
pub(crate) fn to_record(key: RecordKey, row: &Row, names: Option<&[CellName]>) -> Result<Record> {
    let mut cells = row_cells(row)?;
    let wanted = |name: &[u8]| names.is_none_or(|names| names.iter().any(|n| n.as_bytes() == name));
    if wanted(reserved::META.as_bytes()) {
        let meta = meta_of(&cells);
        cells.insert(reserved::META.into(), meta.encode());
    }
    cells.insert(reserved::KEY.into(), CellValue::from_bytes32(key.0));
    if let Some(names) = names {
        cells.retain(|name, _| names.contains(name));
    }
    Ok(Record { key, cells })
}

/// A zero-cost receipt: the adapter does not run golem's pricing model.
fn metered<T>(result: Result<T>, priced_at: Option<CommitId>, details: Details) -> Metered<T> {
    let mut metered = Metered::unmetered(result, priced_at);
    if metered.result.is_ok() {
        metered.receipt.details = details;
    }
    metered
}

impl Api for ArkivDb {
    fn create(&self, branch: BranchId, operation: RecordOp<op::Create>) -> Metered<RecordKey> {
        let key = operation.record_key();
        let mut priced_at = None;
        let mut details = Details::default();
        let result = self.with_open(branch, |slot| {
            priced_at = Some(self.commit_id(slot.base));
            reject_reserved(key)?;
            let cells = admit_create(&self.genesis.cell_limits, operation.entries())?;
            let engine_key = arkiv_key(key);
            if self
                .read_branch(slot, &engine_key, &Projection::ID_ONLY)?
                .is_some()
            {
                return Err(ApiError::AlreadyExists);
            }
            let mut effects = Details::default();
            for (name, value) in &cells {
                effect(&mut effects, name, None, Some(value));
            }
            check_caps(self.genesis.record_limits, meta_of(&cells))?;
            let mut engine_cells = Vec::with_capacity(cells.len());
            for (name, value) in &cells {
                engine_cells.push((self.engine_name(name)?, self.engine_cell(value)?));
            }
            let op = WriteOp::CreateRecord {
                namespace: self.namespace,
                key: engine_key,
                cells: engine_cells,
            };
            // Undo entries: the cells, `#meta`, `#key`, the binding and the
            // allocator.
            self.execute(slot, op, cells.len() as u64 + 4)?;
            details = effects;
            Ok(key)
        });
        metered(result, priced_at, details)
    }

    fn get(&self, target: ReadTarget, operation: RecordOp<op::Get>) -> Metered<Record> {
        let key = operation.record_key();
        let names = operation.names();
        let reserved = system::by_key(&key.0);
        // Only the cells the projection names are read from the engine.
        let shape = query::projection(names, self.engine_max_cells());
        let mut priced_at = None;
        let result = match target {
            ReadTarget::Branch(branch) => self.with_slot(
                branch,
                || ApiError::HandleInvalid,
                |slot| {
                    let slot = slot.as_mut().ok_or(ApiError::HandleInvalid)?;
                    let commit = self.commit_id(slot.base);
                    priced_at = Some(commit);
                    if let Some(record) = reserved {
                        let base = lock(&self.backend, "engine")?.snapshot(slot.base)?;
                        let created = slot
                            .log
                            .iter()
                            .filter(|op| matches!(op, WriteOp::CreateRecord { .. }))
                            .count() as u64;
                        let mut view = View {
                            base: &base,
                            commit,
                            created,
                            slot: Some(slot),
                        };
                        return self.reserved_record(record, &mut view, names);
                    }
                    let row = self
                        .read_branch(slot, &arkiv_key(key), &shape)?
                        .ok_or(ApiError::NotFound)?;
                    to_record(key, &row, names)
                },
            ),
            ReadTarget::Head | ReadTarget::Commit(_) => {
                let commit = match target {
                    ReadTarget::Commit(commit) => Some(commit),
                    _ => None,
                };
                self.committed(commit, &mut priced_at)
                    .and_then(|(snapshot, commit)| {
                        if let Some(record) = reserved {
                            let mut view = View {
                                base: &snapshot,
                                commit,
                                created: 0,
                                slot: None,
                            };
                            return self.reserved_record(record, &mut view, names);
                        }
                        let row = self
                            .read(Source::Snapshot(&snapshot), &arkiv_key(key), &shape)?
                            .ok_or(ApiError::NotFound)?;
                        to_record(key, &row, names)
                    })
            }
        };
        metered(result, priced_at, Details::default())
    }

    fn patch(&self, branch: BranchId, operation: RecordOp<op::Patch>) -> Metered<()> {
        let key = operation.record_key();
        let mut priced_at = None;
        let mut details = Details::default();
        let result = self.with_open(branch, |slot| {
            priced_at = Some(self.commit_id(slot.base));
            reject_reserved(key)?;
            let changes = admit_patch(&self.genesis.cell_limits, operation.entries())?;
            let engine_key = arkiv_key(key);
            let row = self
                .read_branch(slot, &engine_key, &Projection::ALL)?
                .ok_or(ApiError::NotFound)?;
            let before = row_cells(&row)?;
            let mut after = before.clone();
            let mut effects = Details::default();
            let (mut set, mut remove) = (Vec::new(), Vec::new());
            for (name, change) in changes {
                let old = before.get(&name);
                let new = match change {
                    Change::Write(value) => Some(value),
                    Change::Set(value) => {
                        let old =
                            old.ok_or_else(|| ApiError::CellNotFound { name: name.clone() })?;
                        Some(value.with_kind(old.kind()).map_err(|source| {
                            ApiError::InvalidArgument {
                                message: format!(
                                    "set({name:?}): invalid cell value for stored kind"
                                ),
                                source: Some(Box::new(source)),
                            }
                        })?)
                    }
                    Change::Remove => None,
                };
                if old == new.as_ref() {
                    continue;
                }
                effect(&mut effects, &name, old, new.as_ref());
                match new {
                    Some(value) => {
                        set.push((self.engine_name(&name)?, self.engine_cell(&value)?));
                        after.insert(name, value);
                    }
                    None => {
                        remove.push(self.engine_name(&name)?);
                        after.remove(&name);
                    }
                }
            }
            let (meta_before, meta_after) = (meta_of(&before), meta_of(&after));
            check_caps(self.genesis.record_limits, meta_after)?;
            let changed = (set.len() + remove.len()) as u64;
            if changed == 0 {
                // golem: a no-op adds no journal entry.
                return Ok(());
            }
            let op = WriteOp::PatchRecord {
                namespace: self.namespace,
                key: engine_key,
                set,
                remove,
            };
            // Undo entries: each changed cell, and `#meta` if its counts move.
            self.execute(slot, op, changed + u64::from(meta_after != meta_before))?;
            details = effects;
            Ok(())
        });
        metered(result, priced_at, details)
    }

    fn delete(&self, branch: BranchId, operation: RecordOp<op::Delete>) -> Metered<()> {
        let key = operation.record_key();
        let mut priced_at = None;
        let mut details = Details::default();
        let result = self.with_open(branch, |slot| {
            priced_at = Some(self.commit_id(slot.base));
            reject_reserved(key)?;
            let engine_key = arkiv_key(key);
            let row = self
                .read_branch(slot, &engine_key, &Projection::ALL)?
                .ok_or(ApiError::NotFound)?;
            let cells = row_cells(&row)?;
            let mut effects = Details::default();
            for (name, value) in &cells {
                effect(&mut effects, name, Some(value), None);
            }
            let op = WriteOp::DeleteRecord {
                namespace: self.namespace,
                key: engine_key,
            };
            // Undo entries: the cells, `#meta`, `#key` and the binding.
            self.execute(slot, op, cells.len() as u64 + 3)?;
            details = effects;
            Ok(())
        });
        metered(result, priced_at, details)
    }

    fn query(&self, commit_id: Option<CommitId>, operation: QueryOp) -> Metered<QueryPage> {
        let mut priced_at = None;
        let request = operation.request();
        let request = Request {
            filter: &request.filter,
            sort: &request.sort,
            projection: request.projection.as_deref(),
            limit: request.limit,
            offset: request.offset,
            with_total: request.with_total,
        };
        let result = self.run_query(commit_id, &request, &mut priced_at);
        metered(result, priced_at, Details::default())
    }

    fn count(&self, commit_id: Option<CommitId>, operation: CountOp) -> Metered<u64> {
        let mut priced_at = None;
        let result = self.run_count(commit_id, operation.predicate(), &mut priced_at);
        metered(result, priced_at, Details::default())
    }

    /// The genesis model: the adapter has no model activation.
    fn metering_model(&self, commit_id: Option<CommitId>) -> Result<MeteringModelInfo> {
        let (_, commit) = self.committed(commit_id, &mut None)?;
        Ok(MeteringModelInfo {
            commit_id: commit,
            active: MeteringModelMetadata {
                version: 1,
                activation_commit: CommitId::GENESIS,
                golemdb_code_version: self.genesis.metering.golemdb_code_version,
                host_code_version: self.genesis.metering.host_code_version,
            },
            pending: None,
        })
    }

    fn model_weights(
        &self,
        commit_id: Option<CommitId>,
        version: Option<u64>,
    ) -> Result<ModelWeights> {
        let (_, commit) = self.committed(commit_id, &mut None)?;
        let version = version.unwrap_or(1);
        if version != 1 {
            return Err(ApiError::NotFound);
        }
        Ok(ModelWeights {
            commit_id: commit,
            version,
            weights: Arc::new(self.genesis.metering.weights.clone()),
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
            undo: Vec::new(),
            immutable: Staging::default(),
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
                    version: slot.undo.iter().sum(),
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
            slot.undo.truncate(start);
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
                // The engine's own head decides, after any recovery: an
                // earlier publication whose outcome was unknown may have
                // moved it, and then this branch has lost.
                self.heal(&mut backend)?;
                if backend.head() != slot.base {
                    self.head
                        .store(backend.head().sequence(), Ordering::Release);
                    drop(backend);
                    *slot_opt = None;
                    self.remove(branch)?;
                    return Err(ApiError::Conflict);
                }
                self.rebuild(slot, &mut backend)?;
                // Immutable rows first, as the rows of the commit about to
                // be published; they stay invisible until the engine's head
                // moves (see `crate::immutable`).
                if let Some(store) = &self.immutable {
                    let commit = self.commit_id(slot.base).get() + 1;
                    store.publish(commit, &slot.immutable).map_err(api_error)?;
                }
                // From here the slot keeps only its log, its staged rows and
                // its seal: a failed publication is retried by rebuilding
                // the branch.
                let live = slot.live.take().expect("rebuilt");
                let candidate = live.branch.try_seal().map_err(engine)?;
                let published = match &mut *backend {
                    Backend::Memory(e, _) => match e.publish(&candidate) {
                        Ok(id) => Ok(id),
                        Err(EngineError::StaleHead { .. }) => Err(ApiError::Conflict),
                        Err(error) => Err(engine(error)),
                    },
                    Backend::Durable(d) => self.publish_durable(d, candidate),
                };
                match published {
                    Ok(head) => {
                        if let Some(store) = &self.immutable {
                            store.published();
                        }
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

    fn immutable_data_schema(&self, segment: &str) -> Result<ImmutableDataSchema> {
        self.immutable()?.schema(segment).map_err(api_error)
    }

    fn immutable_data_append(
        &self,
        branch: BranchId,
        segment: &str,
        key: Option<ImmutableDataKey>,
        row: ImmutableDataRow,
    ) -> Result<ImmutableDataOrdinal> {
        self.with_staging(branch, |store, staging, origin| {
            store.append(staging, origin, segment, key, row)
        })
    }

    fn immutable_data_truncate(
        &self,
        branch: BranchId,
        segment: &str,
        from_ordinal: ImmutableDataOrdinal,
    ) -> Result<()> {
        self.with_staging(branch, |store, staging, origin| {
            store.truncate(staging, origin, segment, from_ordinal)
        })
    }

    fn immutable_data_get(
        &self,
        segment: &str,
        address: ImmutableDataAddress,
        options: &ImmutableDataReadOptions,
    ) -> Result<ImmutableDataRow> {
        let store = self.immutable()?;
        let head = self.commit_id(self.head_id()).get();
        store
            .get(head, segment, address, options)
            .map_err(api_error)
    }

    fn immutable_data_rows_of(
        &self,
        segment: &str,
        commit: CommitId,
        options: &ImmutableDataReadOptions,
    ) -> Result<Vec<ImmutableDataRow>> {
        let store = self.immutable()?;
        let head = self.commit_id(self.head_id()).get();
        store
            .rows_of(head, segment, commit.get(), options)
            .map_err(api_error)
    }
}

impl ArkivDb {
    /// The store of committed immutable rows; `NotFound` when the genesis
    /// declares no segment, as on golem-db.
    fn immutable(&self) -> Result<&ImmutableStore> {
        self.immutable.as_ref().ok_or(ApiError::NotFound)
    }

    /// Runs a staging call on a live branch, sealed or not: the handle is
    /// checked first, then the segment.
    fn with_staging<T>(
        &self,
        branch: BranchId,
        call: impl FnOnce(
            &ImmutableStore,
            &mut Staging,
            u64,
        )
            -> std::result::Result<T, golemdb_immutable_data::ImmutableDataStorageError>,
    ) -> Result<T> {
        self.with_slot(
            branch,
            || ApiError::HandleInvalid,
            |slot| {
                let slot = slot.as_mut().ok_or(ApiError::HandleInvalid)?;
                let store = self.immutable()?;
                let origin = self.commit_id(slot.base).get();
                call(store, &mut slot.immutable, origin).map_err(api_error)
            },
        )
    }
}
