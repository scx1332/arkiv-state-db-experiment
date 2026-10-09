//! `golemdb_api::Api` implemented on the Arkiv engine
//! (`arkiv-state-db-experiment`'s `arkiv-db` crate).
//!
//! Mapping:
//! - one fixed namespace (`golemdb`), created by the adapter in the first
//!   engine commit; golem-db commit N is engine snapshot N + 1;
//! - `RecordKey` (32 bytes) -> engine `RecordKey` (1..=32 bytes);
//! - attributes / fields -> engine `Cell::attribute` / `Cell::field`, typed
//!   natively: every golem cell type has an engine type with the same value
//!   domain, order and term bytes (see [`to_value`] and [`from_cell`]);
//! - `BranchId` -> an adapter registry slot holding an engine `Branch`;
//!   checkpoint/rollback frames are emulated on engine `Checkpoint`s with
//!   golem's journal-frame rules;
//! - stale base -> `HandleInvalid` on every branch call, `Conflict` on commit
//!   (golem's rule: a stale handle is consumed when touched).
//!
//! Two storage backends: the volatile in-memory `Engine` and the durable
//! `DurableEngine` on a `RedbStore` file.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use arkiv_db::durable::{DurableEngine, EngineGenesis, Initialization, Options, Publication};
use arkiv_db::{
    Branch, Budget, Cell, CellKind as ArkivKind, CellName as ArkivName, Checkpoint, Effect, Engine,
    EngineError, Limits, NamespaceId, NamespaceName, OwnerId, PriceSchedule, Projection,
    RecordKey as ArkivKey, Rejection, Row, SealedCandidate, Snapshot, SnapshotId, StorageError,
    Value, WriteOp,
};
use arkiv_db_redb::{RedbConfig, RedbStore};
use golemdb_api::{
    Api, ApiError, BranchId, BranchInfo, CellKind, CellLimits, CellName, CellNameRef, CellType,
    CellValue, CommitId, FloatWidth, ImmutableDataAddress, ImmutableDataKey, ImmutableDataOrdinal,
    ImmutableDataRow, ReadTarget, Record, RecordKey, RecordOp, Result, SealInfo, Width, op,
};
use golemdb_cells::system;

const ACTOR: OwnerId = OwnerId::new([0xa1; 20]);
const NAMESPACE: &str = "golemdb";

/// Adapter policy.
#[derive(Clone, Copy, Debug)]
pub struct ArkivOptions {
    /// Emulate golem-db's `BranchInfo::version` (number of cell undo
    /// entries) exactly. Costs one extra record read per patch and delete.
    /// Off for load runs: version then counts successful mutations.
    pub exact_version: bool,
    /// Durable backend: redb page cache and engine object cache, bytes.
    pub redb_cache: usize,
    pub object_cache: usize,
    pub object_cache_entries: usize,
}

impl Default for ArkivOptions {
    fn default() -> Self {
        Self {
            exact_version: true,
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
fn storage(error: StorageError) -> ApiError {
    internal(format!("arkiv storage: {error}"))
}

enum Backend {
    Memory(Engine),
    Durable(Box<DurableEngine>),
}

impl Backend {
    fn head(&self) -> SnapshotId {
        match self {
            Self::Memory(e) => e.head(),
            Self::Durable(e) => e.head(),
        }
    }
    fn head_snapshot(&self) -> Snapshot {
        match self {
            Self::Memory(e) => e.head_snapshot(),
            Self::Durable(e) => e.head_snapshot(),
        }
    }
    fn snapshot(&self, id: SnapshotId) -> std::result::Result<Snapshot, String> {
        match self {
            Self::Memory(e) => e.snapshot(id).map_err(|e| e.to_string()),
            Self::Durable(e) => e.snapshot(id).map_err(|e| e.to_string()),
        }
    }
    fn begin(&self) -> std::result::Result<Branch, String> {
        match self {
            Self::Memory(e) => e.begin(e.head(), None).map_err(|e| e.to_string()),
            Self::Durable(e) => e.begin(e.head(), None).map_err(|e| e.to_string()),
        }
    }
    /// Publishes; `Ok(None)` when the candidate's base is no longer head.
    fn publish(&mut self, candidate: &SealedCandidate) -> Result<Option<SnapshotId>> {
        match self {
            Self::Memory(e) => match e.publish(candidate) {
                Ok(id) => Ok(Some(id)),
                Err(EngineError::StaleHead { .. }) => Ok(None),
                Err(error) => Err(internal(format!("publish: {error}"))),
            },
            Self::Durable(e) => {
                // prepare consumes the candidate; the caller keeps its own
                // clone so a failed publication can be retried.
                match e.prepare(candidate.clone()) {
                    Ok(_) => {}
                    Err(failure) => {
                        return match failure.error {
                            arkiv_db::durable::DurableError::Engine(EngineError::StaleHead {
                                ..
                            }) => Ok(None),
                            error => Err(internal(format!("prepare: {error}"))),
                        };
                    }
                }
                match e.commit() {
                    Ok(Publication::Committed(info)) => Ok(Some(info.snapshot)),
                    Ok(Publication::NotCommitted(error)) => {
                        let _ = e.discard_prepared();
                        Err(storage(error))
                    }
                    Ok(Publication::Indeterminate(error)) => Err(storage(error)),
                    Err(error) => Err(internal(format!("commit: {error}"))),
                }
            }
        }
    }
}

/// One golem journal frame: the engine checkpoint at its start and the
/// golem version (undo-entry count) to restore.
struct Frame {
    checkpoint: Checkpoint,
    version: u64,
}

enum State {
    Open {
        branch: Branch,
        frames: Vec<Frame>,
        /// golem's `Journal::rolled_back`.
        rolled_back: bool,
    },
    /// Sealed: the root comes from `Branch::try_state_root`, so the branch
    /// stays readable; it is turned into a candidate at commit.
    SealedBranch { branch: Branch, info: SealInfo },
    /// Being committed: only the candidate remains (kept so a failed
    /// durable commit can be retried), so branch reads are unavailable.
    Sealed {
        candidate: SealedCandidate,
        info: SealInfo,
    },
    /// Transient, while the branch is moved out during sealing.
    Taken,
}

struct Slot {
    base: SnapshotId,
    version: u64,
    state: State,
}

type Entry = Arc<Mutex<Option<Slot>>>;

pub struct ArkivDb {
    backend: Mutex<Backend>,
    /// Sequence of the engine head; read without the writer lock.
    head: AtomicU64,
    branches: Mutex<HashMap<u64, Entry>>,
    next_branch: AtomicU64,
    namespace: NamespaceId,
    /// Engine snapshot sequence of golem-db commit 0.
    offset: u64,
    limits: CellLimits,
    options: ArkivOptions,
    pub description: String,
}

fn engine_limits(golem: &CellLimits) -> Limits {
    Limits {
        max_value_len: (golem.max_bytes_len.max(golem.max_str_len) as usize).max(4096),
        ..Limits::DEFAULT
    }
}

impl ArkivDb {
    /// Volatile engine; all history retained in memory.
    pub fn open_memory(limits: CellLimits, options: ArkivOptions) -> Result<Self> {
        let engine = Engine::with_limits(PriceSchedule::FIXTURE, engine_limits(&limits));
        Self::setup(Backend::Memory(engine), limits, options, "arkiv memory Engine".into())
    }

    fn durable_genesis(limits: &CellLimits) -> EngineGenesis {
        let mut genesis = EngineGenesis::new([0x67; 32], PriceSchedule::FIXTURE);
        genesis.limits = engine_limits(limits);
        genesis.max_object_bytes = 16 << 20;
        genesis
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

    /// Reopens a durable database created by [`open_durable`] with the same
    /// limits. Pending branches of the earlier handle are gone.
    /// Used by the ported golem tests only.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn reopen_durable(dir: &Path, limits: CellLimits, options: ArkivOptions) -> Result<Self> {
        let path = dir.join("arkiv.redb");
        let store = RedbStore::open(&path, RedbConfig::with_cache_size(options.redb_cache))
            .map_err(storage)?;
        let store: Arc<dyn arkiv_db_storage::versioned::VersionedStore> = Arc::new(store);
        let engine = DurableEngine::open(
            store,
            Self::durable_genesis(&limits),
            Self::durable_options(&options),
        )
        .map_err(|e| internal(format!("open: {e}")))?;
        let head = engine.head();
        Ok(Self {
            backend: Mutex::new(Backend::Durable(Box::new(engine))),
            head: AtomicU64::new(head.sequence()),
            branches: Mutex::new(HashMap::new()),
            next_branch: AtomicU64::new(1),
            // open_durable's setup commit creates the only namespace (ordinal 1)
            // as engine snapshot 1.
            namespace: NamespaceId::from_ordinal(1).expect("nonzero"),
            offset: 1,
            limits,
            options,
            description: format!("arkiv DurableEngine reopened from {}", path.display()),
        })
    }

    /// Durable engine over a fresh redb file `dir/arkiv.redb`.
    pub fn open_durable(dir: &Path, limits: CellLimits, options: ArkivOptions) -> Result<Self> {
        let path = dir.join("arkiv.redb");
        let store = RedbStore::create(&path, RedbConfig::with_cache_size(options.redb_cache))
            .map_err(storage)?;
        let store: Arc<dyn arkiv_db_storage::versioned::VersionedStore> = Arc::new(store);
        let genesis = Self::durable_genesis(&limits);
        let engine_options = Self::durable_options(&options);
        let engine = match DurableEngine::initialize(store, genesis, engine_options)
            .map_err(|e| internal(format!("initialize: {e}")))?
        {
            Initialization::Committed(engine) => engine,
            _ => return Err(internal("durable initialization did not commit")),
        };
        Self::setup(
            Backend::Durable(Box::new(engine)),
            limits,
            options,
            format!(
                "arkiv DurableEngine on redb ({}, redb cache {} MiB, object cache {} MiB / {} entries)",
                path.display(),
                options.redb_cache >> 20,
                options.object_cache >> 20,
                options.object_cache_entries
            ),
        )
    }

    fn setup(
        mut backend: Backend,
        limits: CellLimits,
        options: ArkivOptions,
        description: String,
    ) -> Result<Self> {
        let mut branch = backend.begin().map_err(internal)?;
        let executed = branch
            .try_execute(
                ACTOR,
                WriteOp::CreateNamespace {
                    name: NamespaceName::new(NAMESPACE).expect("valid namespace name"),
                },
                Budget::unlimited(),
            )
            .map_err(storage)?
            .map_err(|r| internal(format!("create namespace: {r}")))?;
        let Effect::NamespaceCreated(namespace) = executed.effect else {
            return Err(internal("unexpected namespace effect"));
        };
        let candidate = branch.try_seal().map_err(|e| internal(e.to_string()))?;
        let head = backend
            .publish(&candidate)?
            .ok_or_else(|| internal("setup lost a race"))?;
        Ok(Self {
            backend: Mutex::new(backend),
            head: AtomicU64::new(head.sequence()),
            branches: Mutex::new(HashMap::new()),
            next_branch: AtomicU64::new(1),
            namespace,
            offset: head.sequence(),
            limits,
            options,
            description,
        })
    }

    fn commit_id(&self, snapshot: SnapshotId) -> CommitId {
        CommitId::new(snapshot.sequence() - self.offset)
    }

    fn head_id(&self) -> SnapshotId {
        SnapshotId::from_sequence(self.head.load(Ordering::Acquire))
    }

    fn entry(&self, branch: BranchId) -> Result<Entry> {
        self.branches
            .lock()
            .unwrap()
            .get(&branch.get())
            .cloned()
            .ok_or(ApiError::HandleInvalid)
    }

    fn remove(&self, branch: BranchId) {
        self.branches.lock().unwrap().remove(&branch.get());
    }

    /// golem's `with_slot`: lock the branch, then reject (and reclaim) a
    /// stale or consumed handle with `stale`.
    fn with_slot<T>(
        &self,
        branch: BranchId,
        stale: fn() -> ApiError,
        f: impl FnOnce(&mut Option<Slot>) -> Result<T>,
    ) -> Result<T> {
        let entry = self.entry(branch)?;
        let mut slot = entry.lock().unwrap();
        let base = slot.as_ref().ok_or(ApiError::HandleInvalid)?.base;
        if base != self.head_id() {
            *slot = None;
            drop(slot);
            self.remove(branch);
            return Err(stale());
        }
        f(&mut slot)
    }

    fn with_open<T>(
        &self,
        branch: BranchId,
        f: impl FnOnce(&mut Branch, &mut u64, &mut bool) -> Result<T>,
    ) -> Result<T> {
        self.with_slot(
            branch,
            || ApiError::HandleInvalid,
            |slot| {
                let slot = slot.as_mut().ok_or(ApiError::HandleInvalid)?;
                match &mut slot.state {
                    State::Open {
                        branch,
                        rolled_back,
                        ..
                    } => f(branch, &mut slot.version, rolled_back),
                    _ => Err(ApiError::Sealed),
                }
            },
        )
    }

    fn check_name(&self, name: &CellName) -> Result<ArkivName> {
        let text = self
            .limits
            .parse_user_name(name.as_bytes())
            .map_err(|e| ApiError::invalid_argument(e.to_string()))?;
        let text = std::str::from_utf8(text.as_bytes()).expect("grammar is ASCII");
        ArkivName::new(text)
            .map_err(|e| ApiError::invalid_argument(format!("arkiv cell name {text:?}: {e}")))
    }

    fn check_value(&self, value: &CellValue) -> Result<Cell> {
        self.limits
            .validate_value(value.as_view())
            .map_err(|e| ApiError::invalid_argument(e.to_string()))?;
        let converted = to_value(value)?;
        Ok(match value.kind() {
            CellKind::Attribute => Cell::attribute(converted),
            CellKind::Field => Cell::field(converted),
        })
    }

    fn read(
        &self,
        source: Source<'_>,
        key: RecordKey,
    ) -> Result<Option<Row>> {
        let arkiv_key = arkiv_key(key);
        let result = match source {
            Source::Branch(branch) => branch.try_get_record(
                self.namespace,
                &arkiv_key,
                &Projection::ALL,
                Budget::unlimited(),
            ),
            Source::Snapshot(snapshot) => snapshot.try_get_record(
                self.namespace,
                &arkiv_key,
                &Projection::ALL,
                Budget::unlimited(),
            ),
        };
        let metered = result
            .map_err(storage)?
            .map_err(|r| internal(format!("get_record: {r}")))?;
        Ok(metered.value)
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

/// golem cell -> engine value. Every golem type maps natively, in both
/// engine profiles; the accessors cannot fail for the matching type.
fn to_value(value: &CellValue) -> Result<Value> {
    let mismatch = || internal(format!("golem {:?} accessor failed", value.cell_type()));
    Ok(match value.cell_type() {
        CellType::Bool => Value::Bool(value.as_bool().ok_or_else(mismatch)?),
        CellType::Str => Value::str(value.as_str().ok_or_else(mismatch)?),
        CellType::Bytes => Value::bytes(value.as_bytes().ok_or_else(mismatch)?),
        CellType::Bytes20 => Value::Addr(value.as_bytes20().ok_or_else(mismatch)?),
        CellType::FixedBytes(Width::W4) => Value::Bytes4(value.as_bytes4().ok_or_else(mismatch)?),
        CellType::FixedBytes(Width::W8) => Value::Bytes8(value.as_bytes8().ok_or_else(mismatch)?),
        CellType::FixedBytes(Width::W16) => {
            Value::Bytes16(value.as_bytes16().ok_or_else(mismatch)?)
        }
        CellType::FixedBytes(Width::W32) => {
            Value::Bytes32(value.as_bytes32().ok_or_else(mismatch)?)
        }
        CellType::Uint(Width::W4) => Value::U32(value.as_u32().ok_or_else(mismatch)?),
        CellType::Uint(Width::W8) => Value::U64(value.as_u64().ok_or_else(mismatch)?),
        CellType::Uint(Width::W16) => Value::U128(value.as_u128().ok_or_else(mismatch)?),
        CellType::Uint(Width::W32) => Value::U256(value.as_u256_be().ok_or_else(mismatch)?),
        CellType::Int(Width::W4) => Value::I32(value.as_i32().ok_or_else(mismatch)?),
        CellType::Int(Width::W8) => Value::I64(value.as_i64().ok_or_else(mismatch)?),
        CellType::Int(Width::W16) => Value::I128(value.as_i128().ok_or_else(mismatch)?),
        CellType::Int(Width::W32) => Value::I256(value.as_i256_be().ok_or_else(mismatch)?),
        CellType::Decimal(Width::W4) => {
            Value::Dec32(value.as_dec32_unscaled().ok_or_else(mismatch)?)
        }
        CellType::Decimal(Width::W8) => {
            Value::Dec64(value.as_dec64_unscaled().ok_or_else(mismatch)?)
        }
        CellType::Decimal(Width::W16) => {
            Value::Dec128(value.as_dec128_unscaled().ok_or_else(mismatch)?)
        }
        CellType::Decimal(Width::W32) => {
            Value::Dec(value.as_dec256_unscaled_be().ok_or_else(mismatch)?)
        }
        // golem's codec already rejected NaN and negative zero.
        CellType::Float(FloatWidth::F32) => {
            Value::f32(value.as_f32().ok_or_else(mismatch)?).ok_or_else(mismatch)?
        }
        CellType::Float(FloatWidth::F64) => {
            Value::f64(value.as_f64().ok_or_else(mismatch)?).ok_or_else(mismatch)?
        }
        CellType::Date32 => Value::Date32(value.as_date32().ok_or_else(mismatch)?),
        CellType::Timestamp64 => Value::Timestamp64(value.as_timestamp64().ok_or_else(mismatch)?),
    })
}

/// Engine cell -> golem cell. The inverse of [`to_value`]; the engine's
/// `Key` (an Arkiv entity reference) has no golem counterpart and reads back
/// as golem `bytes32`.
fn from_cell(cell: &Cell) -> Result<CellValue> {
    let value = match &cell.value {
        Value::Bool(v) => CellValue::from_bool(*v),
        Value::Str(v) => CellValue::from_str(v),
        Value::Bytes(v) => CellValue::from_bytes(v),
        Value::Addr(v) => CellValue::from_bytes20(*v),
        Value::Bytes4(v) => CellValue::from_bytes4(*v),
        Value::Bytes8(v) => CellValue::from_bytes8(*v),
        Value::Bytes16(v) => CellValue::from_bytes16(*v),
        Value::Bytes32(v) | Value::Key(v) => CellValue::from_bytes32(*v),
        Value::U32(v) => CellValue::from_u32(*v),
        Value::U64(v) => CellValue::from_u64(*v),
        Value::U128(v) => CellValue::from_u128(*v),
        Value::U256(v) => CellValue::from_u256_be(*v),
        Value::I32(v) => CellValue::from_i32(*v),
        Value::I64(v) => CellValue::from_i64(*v),
        Value::I128(v) => CellValue::from_i128(*v),
        Value::I256(v) => CellValue::from_i256_be(*v),
        Value::Dec32(v) => CellValue::from_dec32_unscaled(*v),
        Value::Dec64(v) => CellValue::from_dec64_unscaled(*v),
        Value::Dec128(v) => CellValue::from_dec128_unscaled(*v),
        Value::Dec(v) => CellValue::from_dec256_unscaled_be(*v),
        Value::F32(v) => CellValue::from_f32(v.get())
            .map_err(|e| internal(format!("stored f32: {e}")))?,
        Value::F64(v) => CellValue::from_f64(v.get())
            .map_err(|e| internal(format!("stored f64: {e}")))?,
        Value::Date32(v) => CellValue::from_date32(*v),
        Value::Timestamp64(v) => CellValue::from_timestamp64(*v),
    };
    let kind = match cell.kind {
        ArkivKind::Attribute => CellKind::Attribute,
        ArkivKind::Field => CellKind::Field,
    };
    value
        .with_kind(kind)
        .map_err(|e| internal(format!("stored cell kind: {e}")))
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
fn system_record(limits: &CellLimits, key: RecordKey, names: Option<&[CellName]>) -> Record {
    let mut cells = BTreeMap::new();
    cells.insert(golem_name("#key"), CellValue::from_bytes32(key.0));
    if key.0 == system::PARAMS.key {
        cells.insert(golem_name("#maxCellNameLen"), CellValue::from_u32(limits.max_cell_name_len));
        cells.insert(golem_name("#maxStrLen"), CellValue::from_u32(limits.max_str_len));
        cells.insert(golem_name("#maxBytesLen"), CellValue::from_u32(limits.max_bytes_len));
    }
    if let Some(names) = names {
        cells.retain(|name, _| names.contains(name));
    }
    Record { key, cells }
}

/// The golem undo entries a patch would add against the current row.
fn patch_changes(row: &Row, set: &[(ArkivName, Cell)], remove: &[ArkivName]) -> u64 {
    let current = |name: &ArkivName| row.cells.iter().find(|(n, _)| n == name).map(|(_, c)| c);
    let sets = set.iter().filter(|(n, c)| current(n) != Some(c)).count();
    let removes = remove.iter().filter(|n| current(n).is_some()).count();
    (sets + removes) as u64
}

impl Api for ArkivDb {
    fn create(&self, branch: BranchId, operation: RecordOp<op::Create>) -> Result<RecordKey> {
        let key = operation.record_key();
        reject_reserved(key)?;
        self.with_open(branch, |engine_branch, version, rolled_back| {
            let mut cells = Vec::with_capacity(operation.cells().len());
            for (name, value) in operation.cells() {
                cells.push((self.check_name(name)?, self.check_value(value)?));
            }
            let entries = cells.len() as u64 + 3; // cells, #key, binding, allocator
            engine_branch
                .try_execute(
                    ACTOR,
                    WriteOp::CreateRecord {
                        namespace: self.namespace,
                        key: arkiv_key(key),
                        cells,
                    },
                    Budget::unlimited(),
                )
                .map_err(storage)?
                .map_err(rejection)?;
            *version += entries;
            *rolled_back = false;
            Ok(key)
        })
    }

    fn get(&self, target: ReadTarget, operation: RecordOp<op::Get>) -> Result<Record> {
        let key = operation.record_key();
        let names = operation.names();
        let system = system::by_key(&key.0).is_some();
        let row = match target {
            ReadTarget::Branch(branch) => {
                let entry = self.entry(branch)?;
                let mut slot = entry.lock().unwrap();
                let base = slot.as_ref().ok_or(ApiError::HandleInvalid)?.base;
                if base != self.head_id() {
                    *slot = None;
                    drop(slot);
                    self.remove(branch);
                    return Err(ApiError::HandleInvalid);
                }
                if system {
                    return Ok(system_record(&self.limits, key, names));
                }
                match &slot.as_ref().expect("checked").state {
                    State::Open { branch, .. } => self.read(Source::Branch(branch), key)?,
                    State::SealedBranch { branch, .. } => self.read(Source::Branch(branch), key)?,
                    State::Taken => return Err(internal("branch lost during a failed seal")),
                    State::Sealed { .. } => {
                        return Err(ApiError::NotImplemented {
                            operation: "get on a sealed branch (arkiv: SealedCandidate has no read API)",
                        });
                    }
                }
            }
            ReadTarget::Head | ReadTarget::Commit(_) => {
                let snapshot = {
                    let backend = self.backend.lock().unwrap();
                    let head = backend.head();
                    match target {
                        ReadTarget::Commit(requested) => {
                            let wanted = requested.get().checked_add(self.offset);
                            match wanted {
                                Some(seq) if seq <= head.sequence() => {
                                    if seq == head.sequence() {
                                        backend.head_snapshot()
                                    } else {
                                        // Unlike golem-db, the engine keeps history.
                                        backend
                                            .snapshot(SnapshotId::from_sequence(seq))
                                            .map_err(internal)?
                                    }
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
                    return Ok(system_record(&self.limits, key, names));
                }
                self.read(Source::Snapshot(&snapshot), key)?
            }
        };
        let row = row.ok_or(ApiError::NotFound)?;
        to_record(key, row, names)
    }

    fn patch(&self, branch: BranchId, operation: RecordOp<op::Patch>) -> Result<()> {
        let key = operation.record_key();
        reject_reserved(key)?;
        self.with_open(branch, |engine_branch, version, rolled_back| {
            let mut set = Vec::new();
            let mut remove = Vec::new();
            let mut checked = Vec::new();
            for (name, change) in operation.changes() {
                checked.push((name, change));
            }
            // golem resolves the record before validating limits. The engine
            // itself treats an empty patch as golem's no-op (NotFound for a
            // missing record), so only exact version counting reads here.
            let current = if self.options.exact_version {
                Some(
                    self.read(Source::Branch(engine_branch), key)?
                        .ok_or(ApiError::NotFound)?,
                )
            } else {
                None
            };
            for (name, change) in checked {
                let arkiv_name = self.check_name(name)?;
                match change {
                    Some(value) => set.push((arkiv_name, self.check_value(value)?)),
                    None => remove.push(arkiv_name),
                }
            }
            let entries = match &current {
                Some(row) => patch_changes(row, &set, &remove),
                None => u64::from(!set.is_empty() || !remove.is_empty()),
            };
            if entries == 0 && current.is_some() {
                return Ok(()); // golem: a no-op patch adds no journal entry
            }
            engine_branch
                .try_execute(
                    ACTOR,
                    WriteOp::PatchRecord {
                        namespace: self.namespace,
                        key: arkiv_key(key),
                        set,
                        remove,
                    },
                    Budget::unlimited(),
                )
                .map_err(storage)?
                .map_err(rejection)?;
            if entries > 0 {
                *version += entries;
                *rolled_back = false;
            }
            Ok(())
        })
    }

    fn delete(&self, branch: BranchId, operation: RecordOp<op::Delete>) -> Result<()> {
        let key = operation.record_key();
        reject_reserved(key)?;
        self.with_open(branch, |engine_branch, version, rolled_back| {
            let entries = if self.options.exact_version {
                let row = self
                    .read(Source::Branch(engine_branch), key)?
                    .ok_or(ApiError::NotFound)?;
                row.cells.len() as u64 + 2 // cells, #key, binding
            } else {
                1
            };
            engine_branch
                .try_execute(
                    ACTOR,
                    WriteOp::DeleteRecord {
                        namespace: self.namespace,
                        key: arkiv_key(key),
                    },
                    Budget::unlimited(),
                )
                .map_err(storage)?
                .map_err(rejection)?;
            *version += entries;
            *rolled_back = false;
            Ok(())
        })
    }

    fn head(&self) -> Result<CommitId> {
        Ok(self.commit_id(self.head_id()))
    }

    fn begin(&self) -> Result<BranchId> {
        let mut branch = {
            let backend = self.backend.lock().unwrap();
            backend.begin().map_err(internal)?
        };
        let base = branch.base();
        let checkpoint = branch
            .try_checkpoint()
            .map_err(|e| internal(e.to_string()))?;
        let id = BranchId::new(self.next_branch.fetch_add(1, Ordering::Relaxed));
        let slot = Slot {
            base,
            version: 0,
            state: State::Open {
                branch,
                frames: vec![Frame {
                    checkpoint,
                    version: 0,
                }],
                rolled_back: false,
            },
        };
        self.branches
            .lock()
            .unwrap()
            .insert(id.get(), Arc::new(Mutex::new(Some(slot))));
        Ok(id)
    }

    fn branch_info(&self, branch: BranchId) -> Result<BranchInfo> {
        self.with_slot(
            branch,
            || ApiError::HandleInvalid,
            |slot| {
                let slot = slot.as_ref().ok_or(ApiError::HandleInvalid)?;
                Ok(BranchInfo {
                    commit_id: self.commit_id(slot.base),
                    branch_id: branch,
                    version: slot.version,
                    sealed: !matches!(slot.state, State::Open { .. }),
                })
            },
        )
    }

    fn checkpoint(&self, branch: BranchId) -> Result<()> {
        self.with_slot(
            branch,
            || ApiError::HandleInvalid,
            |slot| {
                let slot = slot.as_mut().ok_or(ApiError::HandleInvalid)?;
                let State::Open {
                    branch,
                    frames,
                    rolled_back,
                } = &mut slot.state
                else {
                    return Err(ApiError::Sealed);
                };
                if !*rolled_back {
                    let checkpoint = branch
                        .try_checkpoint()
                        .map_err(|e| internal(e.to_string()))?;
                    frames.push(Frame {
                        checkpoint,
                        version: slot.version,
                    });
                }
                *rolled_back = false;
                Ok(())
            },
        )
    }

    fn rollback(&self, branch: BranchId) -> Result<()> {
        self.with_slot(
            branch,
            || ApiError::HandleInvalid,
            |slot| {
                let slot = slot.as_mut().ok_or(ApiError::HandleInvalid)?;
                let State::Open {
                    branch,
                    frames,
                    rolled_back,
                } = &mut slot.state
                else {
                    return Err(ApiError::Sealed);
                };
                if *rolled_back {
                    if frames.len() == 1 {
                        return Err(ApiError::NoFrameToRollback);
                    }
                    frames.pop();
                }
                let frame = frames.last().expect("the first frame is never popped");
                branch
                    .rollback_to(frame.checkpoint)
                    .map_err(|e| internal(e.to_string()))?;
                slot.version = frame.version;
                *rolled_back = true;
                Ok(())
            },
        )
    }

    fn seal(&self, branch: BranchId) -> Result<SealInfo> {
        self.with_slot(
            branch,
            || ApiError::HandleInvalid,
            |slot| {
                let slot = slot.as_mut().ok_or(ApiError::HandleInvalid)?;
                seal_slot(slot, self.commit_id(slot.base))
            },
        )
    }

    fn commit(&self, branch: BranchId) -> Result<CommitId> {
        self.with_slot(
            branch,
            || ApiError::Conflict,
            |slot_opt| {
                let slot = slot_opt.as_mut().ok_or(ApiError::HandleInvalid)?;
                seal_slot(slot, self.commit_id(slot.base))?;
                if matches!(slot.state, State::SealedBranch { .. }) {
                    let State::SealedBranch { branch, info } =
                        std::mem::replace(&mut slot.state, State::Taken)
                    else {
                        unreachable!()
                    };
                    let candidate = branch.try_seal().map_err(|e| internal(e.to_string()))?;
                    slot.state = State::Sealed { candidate, info };
                }
                let State::Sealed { candidate, .. } = &slot.state else {
                    unreachable!("sealed above")
                };
                let mut backend = self.backend.lock().unwrap();
                match backend.publish(candidate)? {
                    Some(head) => {
                        self.head.store(head.sequence(), Ordering::Release);
                        drop(backend);
                        *slot_opt = None;
                        self.remove(branch);
                        Ok(self.commit_id(head))
                    }
                    None => {
                        drop(backend);
                        *slot_opt = None;
                        self.remove(branch);
                        Err(ApiError::Conflict)
                    }
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
        self.remove(branch);
        Ok(())
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

/// Seals an open slot (idempotent), returning golem's seal result.
fn seal_slot(slot: &mut Slot, base: CommitId) -> Result<SealInfo> {
    match &slot.state {
        State::Sealed { info, .. } => return Ok(*info),
        State::SealedBranch { info, .. } => return Ok(*info),
        State::Open { .. } => {}
        State::Taken => return Err(internal("branch lost during an earlier failed seal")),
    }
    let commit_id = CommitId::new(base.get() + 1);
    // Move the branch out; a failed seal leaves the slot Taken (only a
    // poisoned engine branch can fail here).
    let State::Open { branch, .. } = std::mem::replace(&mut slot.state, State::Taken) else {
        unreachable!()
    };
    // The root without consuming the branch: sealed branches stay readable,
    // and the hashes it computes are kept for the seal at commit.
    let root = branch
        .try_state_root()
        .map_err(|e| internal(e.to_string()))?;
    let info = SealInfo {
        commit_id,
        state_root: root.0,
        index_root: [0; 32],
    };
    slot.state = State::SealedBranch { branch, info };
    Ok(info)
}
