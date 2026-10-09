//! Block-oriented execution over immutable snapshots of authenticated state.
//!
//! The engine keeps every published state as an immutable [`Snapshot`]. A
//! host begins a [`Branch`] from the current head, executes namespace-scoped
//! write operations in order with per-operation budgets, takes checkpoints
//! for transaction rollback, seals the candidate and publishes it. Publication
//! swaps in one complete state, so every namespace change and descriptor of a
//! block becomes visible together, and rejects candidates whose base is no
//! longer the head.
//!
//! Snapshot identities are internal and sequential; they are not block
//! numbers. The host maps blocks to snapshots (see [`crate::host`]) and owns
//! consensus, finality, signature validation and admission policy. The engine
//! trusts the `actor` a host passes with each write and enforces namespace
//! ownership against the branch state at that operation's position.
//!
//! The only metered data-plane entry points are `execute`, `estimate` (and
//! their `_host` variants), `get`, `scan_prefix`, `descriptor`, `get_record`,
//! `query`, `count` and `terms` on [`Branch`] and [`Snapshot`]. No method
//! exposes a catalog, record map or index, and the raw KV facade
//! [`crate::Database`] shares no state with an [`Engine`]. The unmetered
//! statistics (`namespace_count`, `record_count`, `typed_record_count`,
//! `term_count`, `records_shared_with`, `typed_shared_with`) reveal only
//! counts and map-root identity for hosts and tests, and the hidden
//! `index_consistent` test aid rebuilds a namespace's index to check it;
//! none of them returns keys or values and none may be exposed as an RPC
//! read.
//!
//! # State and roots
//!
//! Every state is a set of Merkle Patricia maps (`arkiv_db_trie::MerkleMap`,
//! with the key and leaf encodings of state format 1): the namespace
//! catalog, whose leaf for each namespace holds its descriptor, record
//! allocator and the roots of its raw record, key-binding, row and term
//! maps, and the host-state map. Updates copy one key path per map and
//! share everything else, so every published snapshot stays valid and
//! cheap. [`Snapshot::state_root`] and [`SealedCandidate::state_root`] are
//! the digest of the [`StateHeader`] (see [`crate::state`]); they depend on
//! the logical content only, and computing them hashes only the nodes built
//! since they were last hashed.
//!
//! # Host state
//!
//! Besides the namespaces, every state holds a host-owned map from 32-byte
//! keys to values of at most [`HOST_VALUE_MAX`] bytes, for work the host
//! schedules deterministically (such as expiry due counts). It is part of
//! the state like the catalog: checkpoints and rollbacks restore it, a
//! sealed candidate carries it, every published snapshot keeps its own
//! version, and its root enters the state root. It is reachable only
//! through the unmetered host-internal calls
//! `Branch::{try_host_get, try_host_put, try_host_remove, try_host_range}`
//! and `Snapshot::{try_host_get, try_host_range}`; no write operation, host
//! path, estimate, metered read or statistic reads or changes it, so it
//! never changes a receipt. A host must never serve it to users.
//!
//! # Fallible surface
//!
//! Every [`Branch`] and [`Snapshot`] method that walks state, metered or
//! not, has a `try_` form returning `Result<_, StorageError>` whose `Ok`
//! value is exactly what the infallible method returns. A [`StorageError`]
//! is an availability failure of the storage beneath the state: a map node
//! that is missing, corrupt or unreadable. It is never a rejection, a
//! receipt, a missing record or an empty result, and a host must not record
//! it in a block.
//!
//! An engine built by [`Engine::new`] keeps its state in memory, its maps
//! never hold a stored link, and the `try_` forms never fail; the
//! infallible methods delegate to them. An engine opened from a committed
//! state ([`Engine::from_state`], a test aid until node step N3c adds
//! durable manifests) reads map nodes lazily through an
//! `arkiv_db_storage::versioned::ObjectReader`, and there every `try_` form
//! can fail. On a snapshot a storage error just fails that call. On a
//! branch, a storage error inside an operation restores the state from
//! before the operation, records no receipt, adds nothing to the block's
//! spend, and **poisons** the branch: every later `try_` call returns the
//! poisoning error, and `try_checkpoint`, `rollback_to`, `set_pricing` and
//! `try_seal` return [`EngineError::Poisoned`]. The host discards a poisoned
//! branch. The node calls only the `try_` forms.
//!
//! Durability is not implemented: nothing here survives process
//! termination.

#[path = "explore.rs"]
pub mod explore;

use std::{
    collections::{BTreeMap, HashSet},
    fmt,
    ops::{Bound, Range},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

use arkiv_db_storage::{
    Entry,
    versioned::{ObjectId, ObjectReader, SpaceId, StorageError, StorageErrorKind},
};
use arkiv_db_trie::{ObjectSink, Probe};

use crate::{
    codec::B256,
    layout::{Blob, Catalog, HostMap, NamespaceState, Posting, RawKey, Records},
    meter::{
        Budget, Cost, Meter, OperationClass, Outcome, PhysicalTrace, PriceSchedule, Receipt,
        ReceiptKind, Rejection,
    },
    namespace::{NamespaceDescriptor, NamespaceId, NamespaceName, OwnerId, Revision},
    nodes::{Fault, Nodes, corrupt},
    query::{Cursor, CursorContext, Projection, Query, QueryPage},
    record::{Cell, CellName, Limits, RecordId, RecordKey, Row, TypeTag, Value},
    state::{MapKind, NamespaceRoots, STATE_FORMAT, StagedObject, StateHeader},
    typed::{self, TypedState},
};

/// Default directory capacity: the largest number of namespaces an engine
/// admits. The directory is a Merkle Patricia map over 8-byte big-endian
/// ids, so whatever the capacity a lookup visits at most `2·8 + 1` = 17
/// nodes and a write (lookup plus update path) at most 34, and the constant
/// `DirectoryLookup` price reserves a bounded worst case.
pub const DEFAULT_NAMESPACE_CAP: u64 = 1 << 32;

pub(crate) static NEXT_INSTANCE: AtomicU64 = AtomicU64::new(1);
static NEXT_BRANCH_TOKEN: AtomicU64 = AtomicU64::new(1);

/// Longest value the host-state map accepts, in bytes.
pub const HOST_VALUE_MAX: usize = 256;

/// A key of the host-state map.
pub type HostKey = [u8; 32];

/// A refused host-state write. Nothing was changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostStateError {
    /// The value is longer than [`HOST_VALUE_MAX`] bytes.
    ValueTooLarge {
        /// The value's length.
        len: usize,
    },
}

impl fmt::Display for HostStateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ValueTooLarge { len } => write!(
                formatter,
                "host value of {len} bytes exceeds {HOST_VALUE_MAX} bytes"
            ),
        }
    }
}

impl std::error::Error for HostStateError {}

/// A count held in a `u64`, as the `usize` the statistics return.
fn count(len: u64) -> usize {
    usize::try_from(len).expect("map sizes fit in usize")
}

/// Unwraps the result of a state walk of an engine whose state is entirely
/// in memory, where no storage beneath it can fail. The infallible methods
/// delegate to their `try_` forms through this; on an engine that reads
/// committed nodes (see [`Engine::from_state`]) they panic on a storage
/// error, so such a host calls only the `try_` forms.
pub(crate) fn in_memory<T>(result: Result<T, StorageError>) -> T {
    match result {
        Ok(value) => value,
        Err(error) => panic!("storage error on an infallible engine call: {error}"),
    }
}

/// Internal identity of a published state.
///
/// Sequential from [`SnapshotId::GENESIS`], never reused within an engine
/// instance, and distinct from any block number.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SnapshotId(u64);

impl SnapshotId {
    /// The empty state every engine starts from.
    pub const GENESIS: Self = Self(0);

    /// A stable sequence supplied by a durable manifest lookup. Existence is
    /// checked by the engine, not by this numeric constructor.
    pub const fn from_sequence(sequence: u64) -> Self {
        Self(sequence)
    }

    /// Sequence number of the snapshot.
    pub const fn sequence(self) -> u64 {
        self.0
    }
}

impl fmt::Display for SnapshotId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "snapshot#{}", self.0)
    }
}

/// Lifecycle failures outside metered execution. No receipt is issued.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EngineError {
    /// The candidate or expectation names a base that is no longer head.
    StaleHead {
        /// The base the caller expected.
        expected: SnapshotId,
        /// The current head.
        head: SnapshotId,
    },
    /// The candidate was sealed by a different engine instance.
    ForeignCandidate,
    /// No such snapshot has been published.
    UnknownSnapshot(SnapshotId),
    /// The checkpoint belongs to another branch or was already rolled past.
    InvalidCheckpoint,
    /// A pricing schedule must carry a larger ID than the one it replaces.
    StaleSchedule {
        /// The ID of the schedule currently committed in the candidate.
        current: u32,
    },
    /// The branch saw a storage error and refuses every further operation,
    /// checkpoint, rollback, pricing change and seal; the host must discard
    /// it (see [Fallible surface](self#fallible-surface)).
    Poisoned,
}

impl fmt::Display for EngineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StaleHead { expected, head } => {
                write!(formatter, "stale head: expected {expected}, head is {head}")
            }
            Self::ForeignCandidate => write!(formatter, "candidate from another engine"),
            Self::UnknownSnapshot(id) => write!(formatter, "unknown {id}"),
            Self::InvalidCheckpoint => write!(formatter, "invalid checkpoint"),
            Self::StaleSchedule { current } => {
                write!(formatter, "schedule id must exceed {current}")
            }
            Self::Poisoned => write!(formatter, "branch poisoned by a storage error"),
        }
    }
}

impl std::error::Error for EngineError {}

/// The complete logical state of one candidate or snapshot.
///
/// Cloning is O(1): the catalog and the host map are structurally shared
/// maps and the pricing schedule is shared. This is what checkpoints,
/// per-operation rollback and estimation copy.
#[derive(Clone, Debug)]
pub(crate) struct WorkingState {
    pub(crate) catalog: Catalog,
    pub(crate) next_namespace: u64,
    pub(crate) pricing: Arc<PriceSchedule>,
    /// Host-owned entries, outside every namespace and every data-plane path.
    pub(crate) host: HostMap,
}

impl WorkingState {
    /// Resolves a namespace for an unmetered statistic.
    fn namespace(
        &self,
        nodes: &Nodes,
        namespace: NamespaceId,
    ) -> Result<Option<NamespaceState>, StorageError> {
        let found = self
            .catalog
            .get(&namespace, &nodes.catalog(), &mut Probe::new())
            .map_err(|error| error.0)?;
        found.map(|leaf| checked(leaf, namespace)).transpose()
    }

    fn host_get(&self, nodes: &Nodes, key: &HostKey) -> Result<Option<Vec<u8>>, StorageError> {
        let found = self
            .host
            .get(key, &nodes.host(), &mut Probe::new())
            .map_err(|error| error.0)?;
        Ok(found.map(|value| value.0.to_vec()))
    }

    fn host_range(
        &self,
        nodes: &Nodes,
        from: &HostKey,
        limit: usize,
    ) -> Result<Vec<(HostKey, Vec<u8>)>, StorageError> {
        let loader = nodes.host();
        self.host
            .range_from(from, &loader)
            .take(limit)
            .map(|entry| {
                entry
                    .map(|(key, value)| (key, value.0.to_vec()))
                    .map_err(|error| error.0)
            })
            .collect()
    }

    /// The global part of the state; see [`StateHeader`]. Hashes whatever
    /// map nodes are not hashed yet and reads nothing.
    pub(crate) fn header(&self) -> StateHeader {
        StateHeader {
            format: STATE_FORMAT,
            catalog_root: self.catalog.root_hash(),
            catalog_len: self.catalog.len(),
            next_namespace: self.next_namespace,
            pricing: PriceSchedule::clone(&self.pricing),
            host_root: self.host.root_hash(),
            host_len: self.host.len(),
        }
    }

    /// Test aid: the staging walk over the whole state, see
    /// [`Snapshot::staged_objects`] and [`Snapshot::hashed_nodes`].
    fn staging(&self) -> Staging {
        let mut staging = Staging::default();
        let mut leaves = Vec::new();
        self.catalog.stage_with(
            &mut staging.sink(SpaceId::Catalog, MapKind::Catalog),
            &mut |leaf: &NamespaceState| leaves.push(leaf.clone()),
        );
        for leaf in leaves {
            let space = SpaceId::Namespace(leaf.descriptor.id.ordinal());
            leaf.records
                .stage(&mut staging.sink(space, MapKind::Records));
            leaf.typed
                .keys
                .stage(&mut staging.sink(space, MapKind::Keys));
            leaf.typed
                .rows
                .stage(&mut staging.sink(space, MapKind::Rows));
            let mut postings = Vec::new();
            leaf.typed.terms.stage_with(
                &mut staging.sink(space, MapKind::Terms),
                &mut |posting: &Posting| postings.push(posting.clone()),
            );
            for posting in postings {
                posting.0.stage(&mut staging.sink(space, MapKind::Postings));
            }
        }
        self.host
            .stage(&mut staging.sink(SpaceId::Host, MapKind::Host));
        staging
    }
}

/// Checks that a catalog leaf read under `namespace` describes it.
fn checked(leaf: NamespaceState, namespace: NamespaceId) -> Result<NamespaceState, StorageError> {
    if leaf.descriptor.id == namespace {
        Ok(leaf)
    } else {
        Err(corrupt(format!(
            "the catalog leaf of {namespace} describes {}",
            leaf.descriptor.id
        )))
    }
}

/// Staged objects of several maps, each `(space, map, id)` kept once. The
/// same bytes staged by two maps of one space appear twice; objects are put
/// if absent, so a commit writes them once.
///
/// `recorded` counts, per map, every node the walks recorded before
/// deduplication: identical nodes built separately are each counted.
#[derive(Default)]
struct Staging {
    objects: Vec<StagedObject>,
    seen: HashSet<(SpaceId, MapKind, ObjectId)>,
    recorded: BTreeMap<MapKind, u64>,
}

impl Staging {
    fn sink(&mut self, space: SpaceId, map: MapKind) -> MapSink<'_> {
        MapSink {
            space,
            map,
            staging: self,
        }
    }
}

/// The sink of one map in one object space.
struct MapSink<'s> {
    space: SpaceId,
    map: MapKind,
    staging: &'s mut Staging,
}

impl ObjectSink for MapSink<'_> {
    fn record(&mut self, id: B256, encoding: &[u8]) {
        *self.staging.recorded.entry(self.map).or_default() += 1;
        if self.staging.seen.insert((self.space, self.map, id.0)) {
            self.staging.objects.push(StagedObject {
                space: self.space,
                map: self.map,
                id: id.0,
                bytes: encoding.to_vec(),
            });
        }
    }
}

#[derive(Debug)]
struct SnapshotState {
    id: SnapshotId,
    base: Option<SnapshotId>,
    instance: u64,
    limits: Limits,
    state: WorkingState,
    /// Where the state's committed nodes are read from.
    nodes: Nodes,
    /// The state root, computed on first request.
    root: OnceLock<B256>,
}

/// Retains every published snapshot and advances the head by publication.
#[derive(Debug)]
pub struct Engine {
    instance: u64,
    snapshots: Vec<Arc<SnapshotState>>,
    limits: Limits,
    nodes: Nodes,
}

impl Engine {
    /// Creates an engine whose genesis snapshot has no namespaces and commits
    /// `schedule` as the initial pricing, with [`Limits::DEFAULT`].
    pub fn new(schedule: PriceSchedule) -> Self {
        Self::with_limits(schedule, Limits::DEFAULT)
    }

    /// Creates an engine that admits at most `namespace_cap` namespaces, with
    /// the other limits at their defaults.
    pub fn with_namespace_cap(schedule: PriceSchedule, namespace_cap: u64) -> Self {
        Self::with_limits(
            schedule,
            Limits {
                namespace_cap,
                ..Limits::DEFAULT
            },
        )
    }

    /// Creates an engine with explicit instance parameters. They are fixed
    /// for the engine's lifetime and checked at admission.
    pub fn with_limits(schedule: PriceSchedule, limits: Limits) -> Self {
        let state = WorkingState {
            catalog: Catalog::new(),
            next_namespace: 1,
            pricing: Arc::new(schedule),
            host: HostMap::new(),
        };
        Self::from_genesis(state, limits, Nodes::memory())
    }

    /// Test aid, until node step N3c opens durable manifests: an engine
    /// whose genesis snapshot is the committed state `header` describes,
    /// with every map node read lazily, on demand, through `reader`.
    ///
    /// Nothing is read here; the header is only checked (format and map
    /// handles). Every later read and write may fail with the reader's
    /// storage errors, or with `Missing`/`Corrupt` for absent or damaged
    /// nodes (see [Fallible surface](self#fallible-surface)). The objects
    /// of a state are what [`Snapshot::staged_objects`] returns for it; its
    /// genesis state root is `header.root()`.
    #[doc(hidden)]
    pub fn from_state(
        header: &StateHeader,
        limits: Limits,
        reader: Arc<dyn ObjectReader>,
    ) -> Result<Self, StorageError> {
        if header.format != STATE_FORMAT {
            return Err(StorageError::new(
                StorageErrorKind::Incompatible,
                format!("state format {} is not {STATE_FORMAT}", header.format),
            ));
        }
        let handle = |error: arkiv_db_trie::TrieError| corrupt(format!("state header: {error}"));
        let state = WorkingState {
            catalog: Catalog::from_root(header.catalog_root, header.catalog_len).map_err(handle)?,
            next_namespace: header.next_namespace,
            pricing: Arc::new(header.pricing.clone()),
            host: HostMap::from_root(header.host_root, header.host_len).map_err(handle)?,
        };
        Ok(Self::from_genesis(state, limits, Nodes::stored(reader)))
    }

    fn from_genesis(state: WorkingState, limits: Limits, nodes: Nodes) -> Self {
        let instance = NEXT_INSTANCE.fetch_add(1, Ordering::Relaxed);
        let genesis = SnapshotState {
            id: SnapshotId::GENESIS,
            base: None,
            instance,
            limits,
            state,
            nodes: nodes.clone(),
            root: OnceLock::new(),
        };
        Self {
            instance,
            snapshots: vec![Arc::new(genesis)],
            limits,
            nodes,
        }
    }

    /// The configured directory capacity.
    pub const fn namespace_cap(&self) -> u64 {
        self.limits.namespace_cap
    }

    /// The instance limits every record write and query is checked against.
    pub const fn limits(&self) -> Limits {
        self.limits
    }

    /// The most recently published snapshot.
    pub fn head(&self) -> SnapshotId {
        self.latest().id
    }

    fn latest(&self) -> &Arc<SnapshotState> {
        self.snapshots
            .last()
            .expect("an engine always has a genesis snapshot")
    }

    /// Number of retained snapshots, including genesis.
    pub fn snapshot_count(&self) -> usize {
        self.snapshots.len()
    }

    /// Pins a published snapshot for reading. Snapshots are retained for the
    /// process lifetime, so any published ID resolves.
    pub fn snapshot(&self, id: SnapshotId) -> Result<Snapshot, EngineError> {
        usize::try_from(id.0)
            .ok()
            .and_then(|index| self.snapshots.get(index))
            .map(|state| Snapshot {
                state: Arc::clone(state),
            })
            .ok_or(EngineError::UnknownSnapshot(id))
    }

    /// Pins the current head for reading.
    pub fn head_snapshot(&self) -> Snapshot {
        Snapshot {
            state: Arc::clone(self.latest()),
        }
    }

    /// Starts a candidate on top of `expected_head`, which must be the head.
    ///
    /// `block_budget` bounds the total write cost of the candidate; `None`
    /// bounds it only by the counter width. Several branches may exist at
    /// once; at most one of them can publish.
    pub fn begin(
        &self,
        expected_head: SnapshotId,
        block_budget: Option<Cost>,
    ) -> Result<Branch, EngineError> {
        let head = self.head();
        if expected_head != head {
            return Err(EngineError::StaleHead {
                expected: expected_head,
                head,
            });
        }
        Ok(self.head_snapshot().branch(0, block_budget))
    }

    /// Publishes a sealed candidate as the new head, atomically.
    ///
    /// The candidate's base must still be the head; otherwise nothing changes
    /// and the candidate is rejected. The candidate is borrowed, so a rejected
    /// candidate keeps its receipts. Publication is in-process only: it makes
    /// the state visible to later readers but does not make it durable.
    pub fn publish(&mut self, candidate: &SealedCandidate) -> Result<SnapshotId, EngineError> {
        if candidate.instance != self.instance {
            return Err(EngineError::ForeignCandidate);
        }
        let head = self.head();
        if candidate.base != head {
            return Err(EngineError::StaleHead {
                expected: candidate.base,
                head,
            });
        }
        let id = SnapshotId(u64::try_from(self.snapshots.len()).expect("snapshot count fits u64"));
        let root = OnceLock::new();
        if let Some(computed) = candidate.root.get() {
            let _ = root.set(*computed);
        }
        self.snapshots.push(Arc::new(SnapshotState {
            id,
            base: Some(candidate.base),
            instance: self.instance,
            limits: self.limits,
            state: candidate.state.clone(),
            nodes: self.nodes.clone(),
            root,
        }));
        Ok(id)
    }
}

/// A frozen candidate: no further operations, ready for publication.
///
/// Cloning is O(1) apart from the receipt list.
#[derive(Clone, Debug)]
pub struct SealedCandidate {
    pub(crate) instance: u64,
    pub(crate) generation: u64,
    base: SnapshotId,
    pub(crate) state: WorkingState,
    receipts: Vec<Receipt>,
    spent: Cost,
    root: OnceLock<B256>,
}

impl SealedCandidate {
    /// The snapshot the candidate must be published on top of.
    pub const fn base(&self) -> SnapshotId {
        self.base
    }

    /// Write receipts in execution order, including rejected operations.
    pub fn receipts(&self) -> &[Receipt] {
        &self.receipts
    }

    /// Aggregate write cost of the candidate: the sum of its receipts.
    pub const fn spent(&self) -> Cost {
        self.spent
    }

    /// The global part of the candidate's state; its digest is
    /// [`state_root`](Self::state_root). Hashes only the map nodes the
    /// candidate built that are not hashed yet; never reads storage.
    pub fn state_header(&self) -> StateHeader {
        self.state.header()
    }

    /// The candidate's state root, `digest("arkiv/state/v1",
    /// encode(state_header))`: the root its published snapshot reports.
    /// Computed on first request and kept.
    pub fn state_root(&self) -> B256 {
        *self.root.get_or_init(|| self.state.header().root())
    }

    /// Test aid: see [`Snapshot::staged_objects`].
    #[doc(hidden)]
    pub fn staged_objects(&self) -> Vec<StagedObject> {
        self.state.staging().objects
    }

    /// Test aid: see [`Snapshot::hashed_nodes`].
    #[doc(hidden)]
    pub fn hashed_nodes(&self) -> BTreeMap<MapKind, u64> {
        self.state.staging().recorded
    }
}

/// A read handle that pins one published state.
///
/// Every read consults only this snapshot's catalog and record maps. Later
/// publications, transfers and discarded candidates cannot change its
/// answers, and keys deleted at a later head remain visible here.
#[derive(Clone, Debug)]
pub struct Snapshot {
    state: Arc<SnapshotState>,
}

impl Snapshot {
    pub(crate) fn from_header(
        id: SnapshotId,
        base: Option<SnapshotId>,
        instance: u64,
        limits: Limits,
        header: &StateHeader,
        nodes: Nodes,
    ) -> Result<Self, StorageError> {
        // Strict decoding checks allocator/handle relations as well as format.
        let header: StateHeader =
            crate::decode_exact(&crate::encode(header)).map_err(|e| corrupt(e.to_string()))?;
        let state = WorkingState {
            catalog: Catalog::from_root(header.catalog_root, header.catalog_len)
                .map_err(|e| corrupt(e.to_string()))?,
            next_namespace: header.next_namespace,
            pricing: Arc::new(header.pricing.clone()),
            host: HostMap::from_root(header.host_root, header.host_len)
                .map_err(|e| corrupt(e.to_string()))?,
        };
        let root = OnceLock::from(header.root());
        Ok(Self {
            state: Arc::new(SnapshotState {
                id,
                base,
                instance,
                limits,
                state,
                nodes,
                root,
            }),
        })
    }

    pub(crate) fn branch(&self, generation: u64, block_budget: Option<Cost>) -> Branch {
        let base = Arc::clone(&self.state);
        Branch {
            instance: base.instance,
            generation,
            token: NEXT_BRANCH_TOKEN.fetch_add(1, Ordering::Relaxed),
            limits: base.limits,
            mutations: 0,
            working: base.state.clone(),
            pricing: Arc::clone(&base.state.pricing),
            base,
            checkpoints: Vec::new(),
            next_serial: 1,
            receipts: Vec::new(),
            block_limit: block_budget.unwrap_or(Cost::new(u64::MAX)),
            spent: Cost::ZERO,
            poison: OnceLock::new(),
        }
    }

    /// Produces bounded complete equality evidence for this immutable
    /// state. The host must resolve the request's block identity to this
    /// snapshot; this method independently checks its state root.
    pub fn prove_eq(
        &self,
        request: &crate::proof::EqRequest,
        limits: crate::proof::ProofLimits,
        budget: Budget,
    ) -> Result<crate::proof::GeneratedProof, crate::proof::ProofError> {
        crate::proof::generate(
            &self.state.state.catalog,
            &self.state.nodes,
            self.state_header(),
            self.state.limits,
            request,
            limits,
            budget,
        )
    }

    /// Complete index-backed numeric range, with no pagination or truncation.
    /// All matching terms, postings and rows must fit the caller's proof caps.
    pub fn prove_range(
        &self,
        request: &crate::proof::RangeRequest,
        limits: crate::proof::RangeProofLimits,
        budget: Budget,
    ) -> Result<crate::proof::GeneratedProof, crate::proof::ProofError> {
        crate::proof::generate_range(
            &self.state.state.catalog,
            &self.state.nodes,
            self.state_header(),
            self.state.limits,
            request,
            limits,
            budget,
        )
    }

    /// Bounded host-state membership/nonmembership evidence. Host-internal API;
    /// callers decide which host keys are public.
    pub fn prove_host(&self, key: &HostKey) -> Result<Vec<u8>, StorageError> {
        let path = self
            .state
            .state
            .host
            .prove(key, &self.state.nodes.host(), &mut Probe::new())
            .map_err(|e| e.0)?;
        let mut e = crate::Encoder::new();
        e.u32(1);
        e.put(&self.state_header());
        e.u32(path.len() as u32);
        for node in path {
            e.bytes(&node);
        }
        let bytes = e.into_bytes();
        if bytes.len() > crate::proof::HOST_PROOF_CAP {
            return Err(StorageError::new(
                crate::StorageErrorKind::Corrupt,
                "host proof exceeds structural bound",
            ));
        }
        Ok(bytes)
    }

    /// Identity of the pinned snapshot.
    pub fn id(&self) -> SnapshotId {
        self.state.id
    }

    /// The snapshot this one was published on top of; `None` for genesis.
    pub fn base(&self) -> Option<SnapshotId> {
        self.state.base
    }

    /// The pricing schedule committed in this snapshot. Reads at this
    /// snapshot are priced with it.
    pub fn pricing(&self) -> &PriceSchedule {
        &self.state.state.pricing
    }

    /// The global part of this snapshot's state: catalog and host roots
    /// with their entry counts, the namespace allocator and the pricing.
    /// Its digest is [`state_root`](Self::state_root). Hashes only map nodes
    /// not hashed yet; never reads storage, so it cannot fail.
    pub fn state_header(&self) -> StateHeader {
        self.state.state.header()
    }

    /// The state root, `digest("arkiv/state/v1", encode(state_header))`
    /// (see [`crate::state`]). It depends only on the logical content of
    /// the state, never on snapshot ids, physical node ids or cache state.
    /// Computed on first request and kept.
    pub fn state_root(&self) -> B256 {
        *self
            .state
            .root
            .get_or_init(|| self.state.state.header().root())
    }

    /// Test aid, until node step N3c stages commits: the objects of this
    /// state that exist only in memory, each once per space and map. Every
    /// map is walked from its root, stopping at nodes read from storage; the
    /// catalog's nested namespace maps and the term leaves' posting maps are
    /// staged into their namespace's space. For a memory engine that is every object of
    /// the state; for an engine from [`Engine::from_state`], the objects its
    /// blocks created. Committing them lets [`Engine::from_state`] reopen
    /// the state from its header.
    #[doc(hidden)]
    pub fn staged_objects(&self) -> Vec<StagedObject> {
        self.state.state.staging().objects
    }

    /// Test aid: per map, the nodes of this state that exist only in memory
    /// and carry a hash (every map root and every other node of 32 bytes or
    /// more), which computing the state root hashes once each. The staging
    /// walk counts them where it meets them, before
    /// [`staged_objects`](Self::staged_objects) keeps each `(space, map,
    /// id)` once: identical nodes built separately (the one-entry posting
    /// maps of one record's terms, say) are hashed and counted once each
    /// but staged once, and a node the state reached twice would be counted
    /// twice. So each map's count is at least its staged objects. For an
    /// engine from [`Engine::from_state`], these are the nodes its blocks
    /// built.
    #[doc(hidden)]
    pub fn hashed_nodes(&self) -> BTreeMap<MapKind, u64> {
        self.state.state.staging().recorded
    }

    /// Test aid: the roots of `namespace`'s maps and its record allocator,
    /// as its catalog leaf records them; `None` if it does not exist.
    #[doc(hidden)]
    pub fn try_namespace_roots(
        &self,
        namespace: NamespaceId,
    ) -> Result<Option<NamespaceRoots>, StorageError> {
        let handle = |root: B256, len: u64| (root, len);
        Ok(self
            .state
            .state
            .namespace(&self.state.nodes, namespace)?
            .map(|leaf| NamespaceRoots {
                next_record: leaf.typed.next_record,
                records: handle(leaf.records.root_hash(), leaf.records.len()),
                keys: handle(leaf.typed.keys.root_hash(), leaf.typed.keys.len()),
                rows: handle(leaf.typed.rows.root_hash(), leaf.typed.rows.len()),
                terms: handle(leaf.typed.terms.root_hash(), leaf.typed.terms.len()),
            }))
    }

    /// Number of namespaces in the catalog. An O(1) statistic, not a metered
    /// data-plane read.
    pub fn namespace_count(&self) -> usize {
        in_memory(self.try_namespace_count())
    }

    /// Fallible form of [`namespace_count`](Self::namespace_count); see
    /// [Fallible surface](self#fallible-surface).
    pub fn try_namespace_count(&self) -> Result<usize, StorageError> {
        Ok(count(self.state.state.catalog.len()))
    }

    /// Number of records in a namespace, if it exists. A statistic that
    /// resolves the namespace in the catalog, not a metered data-plane read.
    pub fn record_count(&self, namespace: NamespaceId) -> Option<usize> {
        in_memory(self.try_record_count(namespace))
    }

    /// Fallible form of [`record_count`](Self::record_count).
    pub fn try_record_count(&self, namespace: NamespaceId) -> Result<Option<usize>, StorageError> {
        Ok(self
            .state
            .state
            .namespace(&self.state.nodes, namespace)?
            .map(|state| count(state.records.len())))
    }

    /// Whether `namespace`'s raw record map has the same root in both
    /// snapshots, hence the same contents. For a non-empty namespace this
    /// shows that no record changed between the two states; the check
    /// compares shared nodes or stored ids first and hashes only nodes not
    /// hashed yet. Two empty record maps always count as shared, and
    /// snapshots of different engines are compared by ordinal only.
    pub fn records_shared_with(&self, other: &Self, namespace: NamespaceId) -> bool {
        in_memory(self.try_records_shared_with(other, namespace))
    }

    /// Fallible form of [`records_shared_with`](Self::records_shared_with).
    pub fn try_records_shared_with(
        &self,
        other: &Self,
        namespace: NamespaceId,
    ) -> Result<bool, StorageError> {
        Ok(
            match (
                self.state.state.namespace(&self.state.nodes, namespace)?,
                other.state.state.namespace(&other.state.nodes, namespace)?,
            ) {
                (Some(left), Some(right)) => left.records.same_root(&right.records),
                _ => false,
            },
        )
    }

    /// Number of live typed records in a namespace, if it exists. A
    /// statistic, not a metered data-plane read.
    pub fn typed_record_count(&self, namespace: NamespaceId) -> Option<usize> {
        in_memory(self.try_typed_record_count(namespace))
    }

    /// Fallible form of [`typed_record_count`](Self::typed_record_count).
    pub fn try_typed_record_count(
        &self,
        namespace: NamespaceId,
    ) -> Result<Option<usize>, StorageError> {
        Ok(self
            .state
            .state
            .namespace(&self.state.nodes, namespace)?
            .map(|state| count(state.typed.record_count())))
    }

    /// Number of index terms (value and existence) in a namespace, if it
    /// exists. A statistic, not a metered data-plane read.
    pub fn term_count(&self, namespace: NamespaceId) -> Option<usize> {
        in_memory(self.try_term_count(namespace))
    }

    /// Fallible form of [`term_count`](Self::term_count).
    pub fn try_term_count(&self, namespace: NamespaceId) -> Result<Option<usize>, StorageError> {
        Ok(self
            .state
            .state
            .namespace(&self.state.nodes, namespace)?
            .map(|state| count(state.typed.term_count())))
    }

    /// Whether `namespace`'s typed rows, key bindings and index have the
    /// same roots in both snapshots (see
    /// [`records_shared_with`](Self::records_shared_with)).
    pub fn typed_shared_with(&self, other: &Self, namespace: NamespaceId) -> bool {
        in_memory(self.try_typed_shared_with(other, namespace))
    }

    /// Fallible form of [`typed_shared_with`](Self::typed_shared_with).
    pub fn try_typed_shared_with(
        &self,
        other: &Self,
        namespace: NamespaceId,
    ) -> Result<bool, StorageError> {
        Ok(
            match (
                self.state.state.namespace(&self.state.nodes, namespace)?,
                other.state.state.namespace(&other.state.nodes, namespace)?,
            ) {
                (Some(left), Some(right)) => left.typed.shares_with(&right.typed),
                _ => false,
            },
        )
    }

    /// Test aid: whether `namespace`'s key bindings and index terms are
    /// exactly what its live rows imply. O(rows × cells) and unmetered; not
    /// a data-plane read.
    #[doc(hidden)]
    pub fn index_consistent(&self, namespace: NamespaceId) -> Option<bool> {
        in_memory(self.try_index_consistent(namespace))
    }

    /// Test aid: fallible form of [`index_consistent`](Self::index_consistent).
    #[doc(hidden)]
    pub fn try_index_consistent(
        &self,
        namespace: NamespaceId,
    ) -> Result<Option<bool>, StorageError> {
        index_consistent(&self.state.state, &self.state.nodes, namespace)
    }

    /// Returns the value of `key` in `namespace`, metered as a read.
    /// Keys longer than [`Limits::MAX_RAW_KEY_LEN`] fail admission.
    pub fn get(
        &self,
        namespace: NamespaceId,
        key: &[u8],
        budget: Budget,
    ) -> ReadResult<Option<Vec<u8>>> {
        in_memory(self.try_get(namespace, key, budget))
    }

    /// Fallible form of [`get`](Self::get).
    pub fn try_get(
        &self,
        namespace: NamespaceId,
        key: &[u8],
        budget: Budget,
    ) -> Result<ReadResult<Option<Vec<u8>>>, StorageError> {
        let state = &self.state.state;
        read_get(
            state,
            &self.state.nodes,
            &state.pricing,
            namespace,
            key,
            budget,
        )
    }

    /// Returns the typed record bound to `key`, projected, metered as a read.
    pub fn get_record(
        &self,
        namespace: NamespaceId,
        key: &RecordKey,
        projection: &Projection,
        budget: Budget,
    ) -> ReadResult<Option<Row>> {
        in_memory(self.try_get_record(namespace, key, projection, budget))
    }

    /// Fallible form of [`get_record`](Self::get_record).
    pub fn try_get_record(
        &self,
        namespace: NamespaceId,
        key: &RecordKey,
        projection: &Projection,
        budget: Budget,
    ) -> Result<ReadResult<Option<Row>>, StorageError> {
        let state = &self.state.state;
        read_record(
            state,
            &self.state.nodes,
            &state.pricing,
            &self.state.limits,
            namespace,
            key,
            projection,
            budget,
        )
    }

    /// Counts every live record matching `query`, metered as a read: the
    /// same enumeration as a complete page walk with no row emitted.
    pub fn count(&self, namespace: NamespaceId, query: &Query, budget: Budget) -> ReadResult<u64> {
        in_memory(self.try_count(namespace, query, budget))
    }

    /// Fallible form of [`count`](Self::count).
    pub fn try_count(
        &self,
        namespace: NamespaceId,
        query: &Query,
        budget: Budget,
    ) -> Result<ReadResult<u64>, StorageError> {
        let state = &self.state.state;
        read_count(
            state,
            &self.state.nodes,
            &state.pricing,
            &self.state.limits,
            namespace,
            query,
            budget,
        )
    }

    /// The distinct indexed values of attribute `name` with type `tag` held
    /// by live records, in value order from `lower`, at most `limit` of
    /// them, metered as a read. Positional: only the returned terms and the
    /// seek are charged.
    pub fn terms(
        &self,
        namespace: NamespaceId,
        name: &CellName,
        tag: TypeTag,
        lower: Bound<Value>,
        limit: usize,
        budget: Budget,
    ) -> ReadResult<Vec<Value>> {
        in_memory(self.try_terms(namespace, name, tag, lower, limit, budget))
    }

    /// Fallible form of [`terms`](Self::terms).
    pub fn try_terms(
        &self,
        namespace: NamespaceId,
        name: &CellName,
        tag: TypeTag,
        lower: Bound<Value>,
        limit: usize,
        budget: Budget,
    ) -> Result<ReadResult<Vec<Value>>, StorageError> {
        let state = &self.state.state;
        let request = TermsRequest {
            limits: &self.state.limits,
            namespace,
            name,
            tag,
            lower: &lower,
            limit,
        };
        read_terms(state, &self.state.nodes, &state.pricing, &request, budget)
    }

    /// Evaluates `query` against this snapshot and returns one page of at
    /// most `limit` rows after `cursor`, metered as a read.
    ///
    /// Evaluation is lazy and positioned after the cursor: the charge covers
    /// the candidates scanned to fill the page plus each group's first match
    /// beyond it, which makes `next` an exact claim. The cursor must have
    /// been issued by this snapshot for the same namespace, query and
    /// projection.
    pub fn query(
        &self,
        namespace: NamespaceId,
        query: &Query,
        projection: &Projection,
        limit: usize,
        cursor: Option<&Cursor>,
        budget: Budget,
    ) -> ReadResult<QueryPage> {
        in_memory(self.try_query(namespace, query, projection, limit, cursor, budget))
    }

    /// Fallible form of [`query`](Self::query).
    pub fn try_query(
        &self,
        namespace: NamespaceId,
        query: &Query,
        projection: &Projection,
        limit: usize,
        cursor: Option<&Cursor>,
        budget: Budget,
    ) -> Result<ReadResult<QueryPage>, StorageError> {
        let state = &self.state.state;
        let context = CursorContext::Snapshot {
            instance: self.state.instance,
            id: self.state.id,
        };
        let request = QueryRequest {
            limits: &self.state.limits,
            namespace,
            query,
            projection,
            limit,
            cursor,
            context,
        };
        read_query(state, &self.state.nodes, &state.pricing, &request, budget)
    }

    /// Returns `namespace`'s entries starting with `prefix`, in key order,
    /// metered as a read. An empty prefix scans the whole namespace.
    /// Prefixes longer than [`Limits::MAX_RAW_KEY_LEN`] fail admission.
    pub fn scan_prefix(
        &self,
        namespace: NamespaceId,
        prefix: &[u8],
        budget: Budget,
    ) -> ReadResult<Vec<Entry>> {
        in_memory(self.try_scan_prefix(namespace, prefix, budget))
    }

    /// Fallible form of [`scan_prefix`](Self::scan_prefix).
    pub fn try_scan_prefix(
        &self,
        namespace: NamespaceId,
        prefix: &[u8],
        budget: Budget,
    ) -> Result<ReadResult<Vec<Entry>>, StorageError> {
        let state = &self.state.state;
        read_scan(
            state,
            &self.state.nodes,
            &state.pricing,
            namespace,
            prefix,
            budget,
        )
    }

    /// Returns the namespace descriptor as of this snapshot, metered as a read.
    pub fn descriptor(
        &self,
        namespace: NamespaceId,
        budget: Budget,
    ) -> ReadResult<NamespaceDescriptor> {
        in_memory(self.try_descriptor(namespace, budget))
    }

    /// Fallible form of [`descriptor`](Self::descriptor).
    pub fn try_descriptor(
        &self,
        namespace: NamespaceId,
        budget: Budget,
    ) -> Result<ReadResult<NamespaceDescriptor>, StorageError> {
        let state = &self.state.state;
        read_descriptor(state, &self.state.nodes, &state.pricing, namespace, budget)
    }

    /// The host-state value under `key` in this snapshot. Unmetered and
    /// host-internal; see [Host state](self#host-state).
    pub fn try_host_get(&self, key: &HostKey) -> Result<Option<Vec<u8>>, StorageError> {
        self.state.state.host_get(&self.state.nodes, key)
    }

    /// At most `limit` host-state entries of this snapshot whose keys are at
    /// least `from`, in key order. Unmetered and host-internal; the work is
    /// bounded by `limit`.
    pub fn try_host_range(
        &self,
        from: &HostKey,
        limit: usize,
    ) -> Result<Vec<(HostKey, Vec<u8>)>, StorageError> {
        self.state.state.host_range(&self.state.nodes, from, limit)
    }
}

/// See [`Snapshot::try_index_consistent`].
fn index_consistent(
    state: &WorkingState,
    nodes: &Nodes,
    namespace: NamespaceId,
) -> Result<Option<bool>, StorageError> {
    let Some(leaf) = state.namespace(nodes, namespace)? else {
        return Ok(None);
    };
    let loaders = nodes.namespace(namespace);
    leaf.typed
        .is_consistent(&loaders)
        .map(Some)
        .map_err(typed::storage)
}

/// A checkpoint inside one branch; see [`Branch::checkpoint`].
///
/// The branch token is process-wide unique and the serial is per branch, so a
/// checkpoint is accepted only by the branch that issued it and only while it
/// has not been rolled past.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Checkpoint {
    token: u64,
    serial: u64,
    index: usize,
}

#[derive(Debug)]
struct Saved {
    serial: u64,
    state: WorkingState,
    receipt_count: usize,
}

/// A host-authorized write. The host validates the request and its signature
/// and passes the authenticated actor separately.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WriteOp {
    /// Allocate a namespace owned by the actor. Creation is permissionless
    /// and gated only by its charge and the directory capacity.
    CreateNamespace {
        /// Readable name; registration and uniqueness are host policy.
        name: NamespaceName,
    },
    /// Replace the owner of a namespace the actor currently owns.
    TransferNamespace {
        /// The namespace to transfer.
        namespace: NamespaceId,
        /// The revision the request was built against.
        expected_revision: Revision,
        /// The new owner; may equal the current owner.
        new_owner: OwnerId,
    },
    /// Insert or replace a record in a namespace the actor owns.
    Put {
        /// Target namespace.
        namespace: NamespaceId,
        /// Record key.
        key: Vec<u8>,
        /// Record value; empty values are valid.
        value: Vec<u8>,
    },
    /// Remove a record from a namespace the actor owns; missing keys succeed.
    Delete {
        /// Target namespace.
        namespace: NamespaceId,
        /// Record key.
        key: Vec<u8>,
    },
    /// Create a typed record under a key not currently live in a namespace
    /// the actor owns; the record's attribute cells are indexed at once.
    CreateRecord {
        /// Target namespace.
        namespace: NamespaceId,
        /// Caller-assigned key; `AlreadyExists` if bound.
        key: RecordKey,
        /// Cells in any order; names must be unique.
        cells: Vec<(CellName, Cell)>,
    },
    /// Set and remove cells of a live typed record; kind and type may
    /// change and the index follows in the same operation.
    PatchRecord {
        /// Target namespace.
        namespace: NamespaceId,
        /// Key of the record; `NotFound` if not live.
        key: RecordKey,
        /// Cells to create or replace.
        set: Vec<(CellName, Cell)>,
        /// Names to remove; an absent name is a charged no-op.
        remove: Vec<CellName>,
    },
    /// Remove a live typed record and every index membership it holds.
    DeleteRecord {
        /// Target namespace.
        namespace: NamespaceId,
        /// Key of the record; `NotFound` if not live.
        key: RecordKey,
    },
}

impl WriteOp {
    /// Validates a write's arguments against the instance limits: exactly
    /// the argument checks of admission, which run before any charge and
    /// reject with [`Rejection::InvalidArgument`] without recording
    /// anything. They depend only on the operation and `limits`, never on
    /// state, so a host can apply them before accepting a request.
    pub fn validate(&self, limits: &Limits) -> Result<(), Rejection> {
        let outcome = match self {
            Self::Put { key, .. } | Self::Delete { key, .. }
                if key.len() > Limits::MAX_RAW_KEY_LEN =>
            {
                Err(crate::ArgumentFault::KeyTooLong)
            }
            Self::CreateRecord { cells, .. } => typed::validate_create(limits, cells),
            Self::PatchRecord { set, remove, .. } => typed::validate_patch(limits, set, remove),
            _ => Ok(()),
        };
        outcome.map_err(Rejection::InvalidArgument)
    }
}

/// What an applied write changed.
///
/// Effects are provisional until the candidate is published: a rolled-back
/// or discarded creation releases its ID, which a later creation may reuse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    /// A host-account transfer, represented in GLM base units.
    BalanceTransferred { to: OwnerId, amount: [u8; 32] },
    /// A namespace was allocated with this ID.
    NamespaceCreated(NamespaceId),
    /// The descriptor now carries this revision.
    Transferred {
        /// The new revision.
        revision: Revision,
    },
    /// A record was inserted (`replaced == false`) or replaced.
    Put {
        /// Whether the key previously existed.
        replaced: bool,
    },
    /// A key was removed, or was already absent.
    Deleted {
        /// Whether the key previously existed.
        existed: bool,
    },
    /// A typed record was created.
    RecordCreated {
        /// The minted id.
        id: RecordId,
    },
    /// A typed record was patched.
    RecordPatched {
        /// The record's id.
        id: RecordId,
    },
    /// A typed record was deleted.
    RecordDeleted {
        /// The id the record had.
        id: RecordId,
    },
}

/// Who authorizes a write: a host-authenticated principal checked against
/// the namespace owner, or the host itself for chain-driven transitions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Authority {
    Owner(OwnerId),
    Host,
}

/// A successful write with its receipt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Executed {
    /// The change applied.
    pub effect: Effect,
    /// The canonical cost record.
    pub receipt: Receipt,
    /// Non-canonical physical measurement of the work.
    pub physical: PhysicalTrace,
}

/// A rejected operation: no logical change, but the receipt is still issued.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rejected {
    /// Why the operation stopped.
    pub reason: Rejection,
    /// The canonical cost record of the work charged before it stopped.
    pub receipt: Receipt,
    /// Non-canonical physical measurement of the work.
    pub physical: PhysicalTrace,
}

impl fmt::Display for Rejected {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} (spent {})", self.reason, self.receipt.spent)
    }
}

impl std::error::Error for Rejected {}

/// A successful read with its receipt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Metered<T> {
    /// The read result.
    pub value: T,
    /// The canonical read receipt; never a gas charge.
    pub receipt: Receipt,
    /// Non-canonical physical measurement of the work.
    pub physical: PhysicalTrace,
}

/// Outcome of a write.
pub type WriteResult = Result<Executed, Rejected>;

/// Outcome of a read.
pub type ReadResult<T> = Result<Metered<T>, Rejected>;

/// A candidate block under construction on top of one base snapshot.
///
/// Operations execute in call order against the branch's working state and
/// are priced with the schedule committed in the base snapshot. The branch
/// owns its base, so it stays valid while other candidates are published;
/// it can then no longer be published itself.
///
/// A storage error during any branch call poisons the branch; see
/// [Fallible surface](self#fallible-surface).
#[derive(Debug)]
pub struct Branch {
    instance: u64,
    generation: u64,
    token: u64,
    limits: Limits,
    /// Serial of applied state changes; branch cursors are bound to it.
    mutations: u64,
    base: Arc<SnapshotState>,
    working: WorkingState,
    pricing: Arc<PriceSchedule>,
    checkpoints: Vec<Saved>,
    next_serial: u64,
    receipts: Vec<Receipt>,
    block_limit: Cost,
    spent: Cost,
    /// The first storage error the branch saw; set once, never cleared.
    poison: OnceLock<StorageError>,
}

impl Branch {
    /// The snapshot this candidate is built on.
    pub fn base(&self) -> SnapshotId {
        self.base.id
    }

    /// The pricing pinned for every write, estimate and read in this branch.
    pub fn pricing(&self) -> &PriceSchedule {
        &self.pricing
    }

    /// The block's total write budget.
    pub const fn block_limit(&self) -> Cost {
        self.block_limit
    }

    /// The engine's instance limits.
    pub const fn limits(&self) -> Limits {
        self.limits
    }

    /// Units charged to write receipts so far, including rejected operations.
    pub const fn spent(&self) -> Cost {
        self.spent
    }

    /// Block budget still available to writes.
    pub const fn remaining(&self) -> Cost {
        self.block_limit.saturating_sub(self.spent)
    }

    /// Write receipts in execution order, including rejected operations.
    /// Admission rejections, estimates and reads are never recorded here.
    pub fn receipts(&self) -> &[Receipt] {
        &self.receipts
    }

    /// Whether a storage error has poisoned the branch; see
    /// [Fallible surface](self#fallible-surface).
    pub fn is_poisoned(&self) -> bool {
        self.poison.get().is_some()
    }

    fn nodes(&self) -> &Nodes {
        &self.base.nodes
    }

    /// Refuses every call on a poisoned branch with the poisoning error.
    fn guard(&self) -> Result<(), StorageError> {
        match self.poison.get() {
            None => Ok(()),
            Some(error) => Err(StorageError::new(
                error.kind,
                format!(
                    "branch poisoned by an earlier storage error: {}",
                    error.detail
                ),
            )),
        }
    }

    /// Poisons the branch with its first storage error and returns `error`.
    fn fail(&self, error: StorageError) -> StorageError {
        let _ = self.poison.set(error.clone());
        error
    }

    /// Runs a call that changes nothing on the working state: refused when
    /// poisoned, and poisoning on a storage error.
    fn read<T>(
        &self,
        call: impl FnOnce(&WorkingState, &Nodes) -> Result<T, StorageError>,
    ) -> Result<T, StorageError> {
        self.guard()?;
        call(&self.working, self.nodes()).map_err(|error| self.fail(error))
    }

    /// Number of namespaces in the branch's current state. A statistic, not
    /// a metered read.
    pub fn namespace_count(&self) -> usize {
        in_memory(self.try_namespace_count())
    }

    /// Fallible form of [`namespace_count`](Self::namespace_count); see
    /// [Fallible surface](self#fallible-surface).
    pub fn try_namespace_count(&self) -> Result<usize, StorageError> {
        self.read(|state, _| Ok(count(state.catalog.len())))
    }

    /// Number of records in a namespace at the branch's current state, if it
    /// exists. A statistic, not a metered read.
    pub fn record_count(&self, namespace: NamespaceId) -> Option<usize> {
        in_memory(self.try_record_count(namespace))
    }

    /// Fallible form of [`record_count`](Self::record_count).
    pub fn try_record_count(&self, namespace: NamespaceId) -> Result<Option<usize>, StorageError> {
        self.read(|state, nodes| {
            Ok(state
                .namespace(nodes, namespace)?
                .map(|state| count(state.records.len())))
        })
    }

    /// Number of live typed records in a namespace at the branch's current
    /// state, if it exists. A statistic, not a metered read.
    pub fn typed_record_count(&self, namespace: NamespaceId) -> Option<usize> {
        in_memory(self.try_typed_record_count(namespace))
    }

    /// Fallible form of [`typed_record_count`](Self::typed_record_count).
    pub fn try_typed_record_count(
        &self,
        namespace: NamespaceId,
    ) -> Result<Option<usize>, StorageError> {
        self.read(|state, nodes| {
            Ok(state
                .namespace(nodes, namespace)?
                .map(|state| count(state.typed.record_count())))
        })
    }

    /// Number of index terms in a namespace at the branch's current state,
    /// if it exists. A statistic, not a metered read.
    pub fn term_count(&self, namespace: NamespaceId) -> Option<usize> {
        in_memory(self.try_term_count(namespace))
    }

    /// Fallible form of [`term_count`](Self::term_count).
    pub fn try_term_count(&self, namespace: NamespaceId) -> Result<Option<usize>, StorageError> {
        self.read(|state, nodes| {
            Ok(state
                .namespace(nodes, namespace)?
                .map(|state| count(state.typed.term_count())))
        })
    }

    /// Test aid: see [`Snapshot::index_consistent`].
    #[doc(hidden)]
    pub fn index_consistent(&self, namespace: NamespaceId) -> Option<bool> {
        in_memory(self.try_index_consistent(namespace))
    }

    /// Test aid: fallible form of [`index_consistent`](Self::index_consistent).
    #[doc(hidden)]
    pub fn try_index_consistent(
        &self,
        namespace: NamespaceId,
    ) -> Result<Option<bool>, StorageError> {
        self.read(|state, nodes| index_consistent(state, nodes, namespace))
    }

    /// Executes one write at the current position on behalf of `actor`.
    ///
    /// Admission: malformed arguments are rejected with
    /// [`Rejection::InvalidArgument`] and a limited budget larger than the
    /// block's remaining budget with [`Rejection::ExceedsBlockBudget`], both
    /// before any charge and without recording anything. Otherwise the
    /// effective limit is the budget, or the block's remaining budget when
    /// the operation is unlimited. Charges precede the work they cover. On
    /// rejection the operation's changes are discarded, but its receipt is
    /// recorded and its spent units count against the block.
    pub fn execute(&mut self, actor: OwnerId, op: WriteOp, budget: Budget) -> WriteResult {
        in_memory(self.try_execute(actor, op, budget))
    }

    /// Fallible form of [`execute`](Self::execute). A storage error restores
    /// the state from before the operation, records nothing and poisons the
    /// branch.
    pub fn try_execute(
        &mut self,
        actor: OwnerId,
        op: WriteOp,
        budget: Budget,
    ) -> Result<WriteResult, StorageError> {
        self.execute_as(Authority::Owner(actor), op, budget)
    }

    /// Executes one typed record write with the host's own authority: the
    /// namespace owner check is skipped, everything else (validation,
    /// admission, charges, rollback, receipts) is identical to
    /// [`execute`](Self::execute). Namespace operations and raw byte writes
    /// are refused at admission with
    /// [`crate::ArgumentFault::NotHostOperation`], so descriptors are never
    /// host-writable.
    ///
    /// This is the trusted path for chain-driven transitions such as expiry
    /// deletion; a host must never expose it to user requests.
    pub fn execute_host(&mut self, op: WriteOp, budget: Budget) -> WriteResult {
        in_memory(self.try_execute_host(op, budget))
    }

    /// Fallible form of [`execute_host`](Self::execute_host), with the
    /// storage-error behaviour of [`try_execute`](Self::try_execute).
    pub fn try_execute_host(
        &mut self,
        op: WriteOp,
        budget: Budget,
    ) -> Result<WriteResult, StorageError> {
        self.execute_as(Authority::Host, op, budget)
    }

    /// Charges deterministic host work against the same block budget as writes.
    /// This trusted API records a separate receipt; rollback retains its charge.
    /// Host work has no engine counting ledger. An insufficient budget charges zero.
    pub fn try_charge_host(
        &mut self,
        units: u64,
        budget: Budget,
    ) -> Result<Result<Receipt, Rejected>, StorageError> {
        self.guard()?;
        let reason = if budget
            .limit_units()
            .is_some_and(|limit| limit > self.remaining())
        {
            Some(Rejection::ExceedsBlockBudget {
                remaining: self.remaining(),
            })
        } else if Cost::new(units) > self.remaining()
            || budget
                .limit_units()
                .is_some_and(|limit| Cost::new(units) > limit)
        {
            Some(Rejection::OutOfBudget)
        } else {
            None
        };
        let receipt = Receipt {
            kind: ReceiptKind::Write,
            model_version: crate::MODEL_VERSION,
            schedule_id: self.pricing.id(),
            spent: if reason.is_some() {
                Cost::ZERO
            } else {
                Cost::new(units)
            },
            outcome: reason.map_or(Outcome::Applied, Outcome::Rejected),
            ledger: None,
        };
        self.spent = self
            .spent
            .checked_add(receipt.spent)
            .expect("bounded host charge");
        self.receipts.push(receipt.clone());
        Ok(match reason {
            Some(reason) => Err(Rejected {
                reason,
                receipt,
                physical: PhysicalTrace::default(),
            }),
            None => Ok(receipt),
        })
    }

    fn execute_as(
        &mut self,
        authority: Authority,
        op: WriteOp,
        budget: Budget,
    ) -> Result<WriteResult, StorageError> {
        self.guard()?;
        let remaining = self.remaining();
        if let Some(admission) = admit(
            &self.pricing,
            &self.limits,
            authority,
            &op,
            budget,
            remaining,
        ) {
            return Ok(Err(admission));
        }
        let saved = self.working.clone();
        let mut meter = Meter::new(&self.pricing, budget, Some(remaining));
        let result = apply(
            &mut self.working,
            &self.base.nodes,
            &self.limits,
            authority,
            op,
            &mut meter,
        );
        let reason = match result {
            Err(Fault::Storage(error)) => {
                // Nothing of the operation survives: no state change, no
                // receipt, no spend.
                self.working = saved;
                return Err(self.fail(error));
            }
            Err(Fault::Reject(reason)) => reason,
            Ok(effect) => {
                self.spent = self
                    .spent
                    .checked_add(meter.spent())
                    .expect("the meter never exceeds the remaining block budget");
                self.mutations += 1;
                let (receipt, physical) = meter.finish(ReceiptKind::Write, Outcome::Applied);
                self.receipts.push(receipt.clone());
                return Ok(Ok(Executed {
                    effect,
                    receipt,
                    physical,
                }));
            }
        };
        self.spent = self
            .spent
            .checked_add(meter.spent())
            .expect("the meter never exceeds the remaining block budget");
        self.working = saved;
        let (receipt, physical) = meter.finish(ReceiptKind::Write, Outcome::Rejected(reason));
        self.receipts.push(receipt.clone());
        Ok(Err(Rejected {
            reason,
            receipt,
            physical,
        }))
    }

    /// Runs `op` exactly as [`execute`](Self::execute) would, including the
    /// admission checks and the block's current remaining budget, on a
    /// disposable copy of the current state.
    ///
    /// Nothing is changed, recorded or charged to the block. The result equals
    /// execution only for the same state, operation and pricing.
    pub fn estimate(&self, actor: OwnerId, op: WriteOp, budget: Budget) -> WriteResult {
        in_memory(self.try_estimate(actor, op, budget))
    }

    /// Fallible form of [`estimate`](Self::estimate). A storage error
    /// poisons the branch.
    pub fn try_estimate(
        &self,
        actor: OwnerId,
        op: WriteOp,
        budget: Budget,
    ) -> Result<WriteResult, StorageError> {
        self.estimate_as(Authority::Owner(actor), op, budget)
    }

    /// Estimates with the host's authority; see
    /// [`execute_host`](Self::execute_host).
    pub fn estimate_host(&self, op: WriteOp, budget: Budget) -> WriteResult {
        in_memory(self.try_estimate_host(op, budget))
    }

    /// Fallible form of [`estimate_host`](Self::estimate_host).
    pub fn try_estimate_host(
        &self,
        op: WriteOp,
        budget: Budget,
    ) -> Result<WriteResult, StorageError> {
        self.estimate_as(Authority::Host, op, budget)
    }

    fn estimate_as(
        &self,
        authority: Authority,
        op: WriteOp,
        budget: Budget,
    ) -> Result<WriteResult, StorageError> {
        self.guard()?;
        let remaining = self.remaining();
        if let Some(admission) = admit(
            &self.pricing,
            &self.limits,
            authority,
            &op,
            budget,
            remaining,
        ) {
            return Ok(Err(admission));
        }
        let mut scratch = self.working.clone();
        let mut meter = Meter::new(&self.pricing, budget, Some(remaining));
        match apply(
            &mut scratch,
            self.nodes(),
            &self.limits,
            authority,
            op,
            &mut meter,
        ) {
            Ok(effect) => {
                let (receipt, physical) = meter.finish(ReceiptKind::Write, Outcome::Applied);
                Ok(Ok(Executed {
                    effect,
                    receipt,
                    physical,
                }))
            }
            Err(Fault::Reject(reason)) => {
                let (receipt, physical) =
                    meter.finish(ReceiptKind::Write, Outcome::Rejected(reason));
                Ok(Err(Rejected {
                    reason,
                    receipt,
                    physical,
                }))
            }
            Err(Fault::Storage(error)) => Err(self.fail(error)),
        }
    }

    /// Records the current logical state (records, descriptors, allocator,
    /// pending pricing and host state) so a transaction can be rolled back
    /// to it.
    ///
    /// # Panics
    ///
    /// If the branch is poisoned, which only an engine that reads storage
    /// can cause; such a host calls [`try_checkpoint`](Self::try_checkpoint).
    pub fn checkpoint(&mut self) -> Checkpoint {
        match self.try_checkpoint() {
            Ok(checkpoint) => checkpoint,
            Err(error) => panic!("checkpoint on a poisoned branch: {error}"),
        }
    }

    /// Fallible form of [`checkpoint`](Self::checkpoint):
    /// [`EngineError::Poisoned`] on a poisoned branch.
    pub fn try_checkpoint(&mut self) -> Result<Checkpoint, EngineError> {
        if self.is_poisoned() {
            return Err(EngineError::Poisoned);
        }
        let serial = self.next_serial;
        self.next_serial += 1;
        self.checkpoints.push(Saved {
            serial,
            state: self.working.clone(),
            receipt_count: self.receipts.len(),
        });
        Ok(Checkpoint {
            token: self.token,
            serial,
            index: self.checkpoints.len() - 1,
        })
    }

    /// Restores the state recorded by `checkpoint` and returns the index
    /// range of every receipt issued since the checkpoint, i.e. the
    /// operations whose effects are no longer in the state.
    ///
    /// Changes made after the checkpoint disappear; earlier successful work
    /// stays. Later checkpoints become invalid; this one stays usable, and
    /// rolling back to it again returns the same since-checkpoint range.
    /// Issued receipts and the block's spent units are not restored. A
    /// poisoned branch refuses with [`EngineError::Poisoned`].
    pub fn rollback_to(&mut self, checkpoint: Checkpoint) -> Result<Range<usize>, EngineError> {
        if self.is_poisoned() {
            return Err(EngineError::Poisoned);
        }
        let saved = self
            .checkpoints
            .get(checkpoint.index)
            .filter(|saved| checkpoint.token == self.token && saved.serial == checkpoint.serial)
            .ok_or(EngineError::InvalidCheckpoint)?;
        let discarded = saved.receipt_count..self.receipts.len();
        self.working = saved.state.clone();
        self.checkpoints.truncate(checkpoint.index + 1);
        self.mutations += 1;
        Ok(discarded)
    }

    /// Commits a new pricing schedule into the candidate. This is a host
    /// admin transition: it takes effect for branches based on the published
    /// result, never for this branch, whose pricing stays pinned to its base.
    /// The schedule ID must exceed the one it replaces, so receipts always
    /// name a unique pricing context. A poisoned branch refuses with
    /// [`EngineError::Poisoned`].
    pub fn set_pricing(&mut self, schedule: PriceSchedule) -> Result<(), EngineError> {
        if self.is_poisoned() {
            return Err(EngineError::Poisoned);
        }
        let current = self.working.pricing.id();
        if schedule.id() <= current {
            return Err(EngineError::StaleSchedule { current });
        }
        self.working.pricing = Arc::new(schedule);
        self.mutations += 1;
        Ok(())
    }

    /// Reads `key` from the branch's current state, metered as a read with
    /// the branch's pinned pricing. The key must fit [`Limits::MAX_RAW_KEY_LEN`].
    pub fn get(
        &self,
        namespace: NamespaceId,
        key: &[u8],
        budget: Budget,
    ) -> ReadResult<Option<Vec<u8>>> {
        in_memory(self.try_get(namespace, key, budget))
    }

    /// Fallible form of [`get`](Self::get).
    pub fn try_get(
        &self,
        namespace: NamespaceId,
        key: &[u8],
        budget: Budget,
    ) -> Result<ReadResult<Option<Vec<u8>>>, StorageError> {
        self.read(|state, nodes| read_get(state, nodes, &self.pricing, namespace, key, budget))
    }

    /// Scans the branch's current state, metered as a read.
    /// The prefix must fit [`Limits::MAX_RAW_KEY_LEN`].
    pub fn scan_prefix(
        &self,
        namespace: NamespaceId,
        prefix: &[u8],
        budget: Budget,
    ) -> ReadResult<Vec<Entry>> {
        in_memory(self.try_scan_prefix(namespace, prefix, budget))
    }

    /// Fallible form of [`scan_prefix`](Self::scan_prefix).
    pub fn try_scan_prefix(
        &self,
        namespace: NamespaceId,
        prefix: &[u8],
        budget: Budget,
    ) -> Result<ReadResult<Vec<Entry>>, StorageError> {
        self.read(|state, nodes| read_scan(state, nodes, &self.pricing, namespace, prefix, budget))
    }

    /// Reads the descriptor from the branch's current state, metered as a read.
    pub fn descriptor(
        &self,
        namespace: NamespaceId,
        budget: Budget,
    ) -> ReadResult<NamespaceDescriptor> {
        in_memory(self.try_descriptor(namespace, budget))
    }

    /// Fallible form of [`descriptor`](Self::descriptor).
    pub fn try_descriptor(
        &self,
        namespace: NamespaceId,
        budget: Budget,
    ) -> Result<ReadResult<NamespaceDescriptor>, StorageError> {
        self.read(|state, nodes| read_descriptor(state, nodes, &self.pricing, namespace, budget))
    }

    /// Reads a typed record from the branch's current state, metered as a read.
    pub fn get_record(
        &self,
        namespace: NamespaceId,
        key: &RecordKey,
        projection: &Projection,
        budget: Budget,
    ) -> ReadResult<Option<Row>> {
        in_memory(self.try_get_record(namespace, key, projection, budget))
    }

    /// Fallible form of [`get_record`](Self::get_record).
    pub fn try_get_record(
        &self,
        namespace: NamespaceId,
        key: &RecordKey,
        projection: &Projection,
        budget: Budget,
    ) -> Result<ReadResult<Option<Row>>, StorageError> {
        self.read(|state, nodes| {
            read_record(
                state,
                nodes,
                &self.pricing,
                &self.limits,
                namespace,
                key,
                projection,
                budget,
            )
        })
    }

    /// Counts the matches of `query` in the branch's current state; see
    /// [`Snapshot::count`].
    pub fn count(&self, namespace: NamespaceId, query: &Query, budget: Budget) -> ReadResult<u64> {
        in_memory(self.try_count(namespace, query, budget))
    }

    /// Fallible form of [`count`](Self::count).
    pub fn try_count(
        &self,
        namespace: NamespaceId,
        query: &Query,
        budget: Budget,
    ) -> Result<ReadResult<u64>, StorageError> {
        self.read(|state, nodes| {
            read_count(
                state,
                nodes,
                &self.pricing,
                &self.limits,
                namespace,
                query,
                budget,
            )
        })
    }

    /// The distinct indexed values of an attribute in the branch's current
    /// state; see [`Snapshot::terms`].
    pub fn terms(
        &self,
        namespace: NamespaceId,
        name: &CellName,
        tag: TypeTag,
        lower: Bound<Value>,
        limit: usize,
        budget: Budget,
    ) -> ReadResult<Vec<Value>> {
        in_memory(self.try_terms(namespace, name, tag, lower, limit, budget))
    }

    /// Fallible form of [`terms`](Self::terms).
    pub fn try_terms(
        &self,
        namespace: NamespaceId,
        name: &CellName,
        tag: TypeTag,
        lower: Bound<Value>,
        limit: usize,
        budget: Budget,
    ) -> Result<ReadResult<Vec<Value>>, StorageError> {
        let request = TermsRequest {
            limits: &self.limits,
            namespace,
            name,
            tag,
            lower: &lower,
            limit,
        };
        self.read(|state, nodes| read_terms(state, nodes, &self.pricing, &request, budget))
    }

    /// Evaluates `query` against the branch's current state; see
    /// [`Snapshot::query`]. A branch cursor is bound to this branch and its
    /// current mutation serial: any applied write, rollback, pricing change
    /// or host-state write since the cursor was issued makes it a
    /// `CursorMismatch`.
    pub fn query(
        &self,
        namespace: NamespaceId,
        query: &Query,
        projection: &Projection,
        limit: usize,
        cursor: Option<&Cursor>,
        budget: Budget,
    ) -> ReadResult<QueryPage> {
        in_memory(self.try_query(namespace, query, projection, limit, cursor, budget))
    }

    /// Fallible form of [`query`](Self::query).
    pub fn try_query(
        &self,
        namespace: NamespaceId,
        query: &Query,
        projection: &Projection,
        limit: usize,
        cursor: Option<&Cursor>,
        budget: Budget,
    ) -> Result<ReadResult<QueryPage>, StorageError> {
        let request = QueryRequest {
            limits: &self.limits,
            namespace,
            query,
            projection,
            limit,
            cursor,
            context: CursorContext::Branch {
                token: self.token,
                mutations: self.mutations,
            },
        };
        self.read(|state, nodes| read_query(state, nodes, &self.pricing, &request, budget))
    }

    /// The host-state value under `key` in the branch's current state.
    /// Unmetered and host-internal; see [Host state](self#host-state).
    pub fn try_host_get(&self, key: &HostKey) -> Result<Option<Vec<u8>>, StorageError> {
        self.read(|state, nodes| state.host_get(nodes, key))
    }

    /// Binds `key` to `value` in the host-state map and returns the value it
    /// replaced. Unmetered, recorded in no receipt, and never charged to the
    /// block.
    ///
    /// A value longer than [`HOST_VALUE_MAX`] is refused with
    /// [`HostStateError::ValueTooLarge`] and changes nothing. A successful
    /// put advances the branch's mutation serial, like any applied write,
    /// even when the value is unchanged, so branch cursors issued before it
    /// become `CursorMismatch`. Checkpoints and rollbacks cover host state.
    /// A storage error changes nothing and poisons the branch.
    pub fn try_host_put(
        &mut self,
        key: HostKey,
        value: &[u8],
    ) -> Result<Result<Option<Vec<u8>>, HostStateError>, StorageError> {
        self.guard()?;
        if value.len() > HOST_VALUE_MAX {
            return Ok(Err(HostStateError::ValueTooLarge { len: value.len() }));
        }
        let inserted = self.working.host.insert(
            &key,
            Blob(Arc::from(value)),
            &self.base.nodes.host(),
            &mut Probe::new(),
        );
        let (host, previous) = inserted.map_err(|error| self.fail(error.0))?;
        self.working.host = host;
        self.mutations += 1;
        Ok(Ok(previous.map(|value| value.0.to_vec())))
    }

    /// Removes `key` from the host-state map and returns the value it held.
    /// Unmetered and host-internal. Every call advances the branch's
    /// mutation serial, like a raw delete of a missing key. A storage error
    /// changes nothing and poisons the branch.
    pub fn try_host_remove(&mut self, key: &HostKey) -> Result<Option<Vec<u8>>, StorageError> {
        self.guard()?;
        let removed = self
            .working
            .host
            .remove(key, &self.base.nodes.host(), &mut Probe::new());
        let (host, previous) = removed.map_err(|error| self.fail(error.0))?;
        self.working.host = host;
        self.mutations += 1;
        Ok(previous.map(|value| value.0.to_vec()))
    }

    /// At most `limit` host-state entries of the branch's current state whose
    /// keys are at least `from`, in key order. Unmetered and host-internal;
    /// the work is bounded by `limit`.
    pub fn try_host_range(
        &self,
        from: &HostKey,
        limit: usize,
    ) -> Result<Vec<(HostKey, Vec<u8>)>, StorageError> {
        self.read(|state, nodes| state.host_range(nodes, from, limit))
    }

    /// The state root the branch's current state would seal to, without
    /// consuming the branch: equal to the [`SealedCandidate::state_root`]
    /// of a seal right now. Hashes only the map nodes not hashed yet and
    /// keeps their hashes, so a later seal or further calls redo none of
    /// that work; never reads storage. [`EngineError::Poisoned`] on a
    /// poisoned branch.
    pub fn try_state_root(&self) -> Result<B256, EngineError> {
        if self.is_poisoned() {
            return Err(EngineError::Poisoned);
        }
        Ok(self.working.header().root())
    }

    /// Freezes the candidate. No further operations are possible.
    ///
    /// # Panics
    ///
    /// If the branch is poisoned, which only an engine that reads storage
    /// can cause; such a host calls [`try_seal`](Self::try_seal).
    pub fn seal(self) -> SealedCandidate {
        match self.try_seal() {
            Ok(sealed) => sealed,
            Err(error) => panic!("seal of a poisoned branch: {error}"),
        }
    }

    /// Fallible form of [`seal`](Self::seal): [`EngineError::Poisoned`] on
    /// a poisoned branch, which must be discarded.
    pub fn try_seal(self) -> Result<SealedCandidate, EngineError> {
        if self.is_poisoned() {
            return Err(EngineError::Poisoned);
        }
        Ok(SealedCandidate {
            instance: self.instance,
            generation: self.generation,
            base: self.base.id,
            state: self.working,
            receipts: self.receipts,
            spent: self.spent,
            root: OnceLock::new(),
        })
    }
}

/// A zero-cost rejection before any charge.
fn zero_cost(
    pricing: &PriceSchedule,
    budget: Budget,
    kind: ReceiptKind,
    reason: Rejection,
) -> Rejected {
    let (receipt, physical) =
        Meter::new(pricing, budget, None).finish(kind, Outcome::Rejected(reason));
    Rejected {
        reason,
        receipt,
        physical,
    }
}

/// The admission checks shared by execution and estimation: the host-path
/// restriction, argument validation, then the block-budget check.
fn admit(
    pricing: &PriceSchedule,
    limits: &Limits,
    authority: Authority,
    op: &WriteOp,
    budget: Budget,
    remaining: Cost,
) -> Option<Rejected> {
    if authority == Authority::Host
        && !matches!(
            op,
            WriteOp::CreateRecord { .. }
                | WriteOp::PatchRecord { .. }
                | WriteOp::DeleteRecord { .. }
        )
    {
        let reason = Rejection::InvalidArgument(crate::record::ArgumentFault::NotHostOperation);
        return Some(zero_cost(pricing, budget, ReceiptKind::Write, reason));
    }
    if let Err(reason) = op.validate(limits) {
        return Some(zero_cost(pricing, budget, ReceiptKind::Write, reason));
    }
    let limit = budget.limit_units()?;
    if limit <= remaining {
        return None;
    }
    let reason = Rejection::ExceedsBlockBudget { remaining };
    Some(zero_cost(pricing, budget, ReceiptKind::Write, reason))
}

fn units(length: usize) -> u64 {
    u64::try_from(length).expect("lengths fit in 64 bits")
}

fn raw_key(key: &[u8]) -> RawKey {
    RawKey(Arc::from(key))
}

/// Resolves a namespace through the directory, recording the physical work.
fn resolve(
    state: &WorkingState,
    nodes: &Nodes,
    namespace: NamespaceId,
    meter: &mut Meter<'_>,
) -> Result<NamespaceState, Fault> {
    let mut probe = Probe::new();
    let found = state.catalog.get(&namespace, &nodes.catalog(), &mut probe);
    meter.note_directory_nodes(probe.nodes);
    let leaf = found?.ok_or(Rejection::UnknownNamespace)?;
    Ok(checked(leaf, namespace)?)
}

fn authorize(state: &NamespaceState, authority: Authority) -> Result<(), Rejection> {
    match authority {
        Authority::Host => Ok(()),
        Authority::Owner(actor) if state.descriptor.owner == actor => Ok(()),
        Authority::Owner(_) => Err(Rejection::Unauthorized),
    }
}

/// Stores an updated namespace state in the directory.
fn commit_namespace(
    state: &mut WorkingState,
    nodes: &Nodes,
    namespace: NamespaceId,
    updated: NamespaceState,
    meter: &mut Meter<'_>,
) -> Result<(), Fault> {
    let mut probe = Probe::new();
    let inserted = state
        .catalog
        .insert(&namespace, updated, &nodes.catalog(), &mut probe);
    meter.note_directory_nodes(probe.nodes);
    let (catalog, _) = inserted?;
    state.catalog = catalog;
    Ok(())
}

/// Applies one write to `state`, charging every unit before its work.
///
/// Rejections are checked in a fixed order after the directory charge:
/// unknown namespace, then authorization, then revision, then the
/// operation's own charges. A storage error may leave `state` partly
/// updated; the caller restores it.
fn apply(
    state: &mut WorkingState,
    nodes: &Nodes,
    limits: &Limits,
    authority: Authority,
    op: WriteOp,
    meter: &mut Meter<'_>,
) -> Result<Effect, Fault> {
    match op {
        WriteOp::CreateNamespace { name } => {
            meter.charge(OperationClass::DirectoryLookup, 1)?;
            if state.catalog.len() >= limits.namespace_cap {
                return Err(Rejection::DirectoryFull.into());
            }
            let Authority::Owner(actor) = authority else {
                return Err(Rejection::Unauthorized.into());
            };
            // One logical entry write: the new catalog leaf.
            meter.charge_group(&[
                (OperationClass::NamespaceCreate, 1),
                (OperationClass::RootUpdate, 1),
            ])?;
            let id = NamespaceId::from_ordinal(state.next_namespace)
                .ok_or(Rejection::CounterExhausted)?;
            let next = state
                .next_namespace
                .checked_add(1)
                .ok_or(Rejection::CounterExhausted)?;
            let created = NamespaceState {
                descriptor: NamespaceDescriptor {
                    id,
                    name,
                    owner: actor,
                    revision: Revision::INITIAL,
                },
                records: Records::new(),
                typed: TypedState::new(),
            };
            commit_namespace(state, nodes, id, created, meter)?;
            state.next_namespace = next;
            Ok(Effect::NamespaceCreated(id))
        }
        WriteOp::TransferNamespace {
            namespace,
            expected_revision,
            new_owner,
        } => {
            meter.charge(OperationClass::DirectoryLookup, 1)?;
            let existing = resolve(state, nodes, namespace, meter)?;
            authorize(&existing, authority)?;
            let current = existing.descriptor.revision;
            if current != expected_revision {
                return Err(Rejection::StaleRevision {
                    expected: expected_revision,
                    current,
                }
                .into());
            }
            // One logical entry write: the replaced catalog leaf.
            meter.charge_group(&[
                (OperationClass::NamespaceTransfer, 1),
                (OperationClass::RootUpdate, 1),
            ])?;
            let revision = Revision(
                current
                    .0
                    .checked_add(1)
                    .ok_or(Rejection::CounterExhausted)?,
            );
            // The record maps are shared as-is: no record is visited or copied.
            let updated = NamespaceState {
                descriptor: NamespaceDescriptor {
                    id: namespace,
                    name: existing.descriptor.name,
                    owner: new_owner,
                    revision,
                },
                ..existing
            };
            commit_namespace(state, nodes, namespace, updated, meter)?;
            Ok(Effect::Transferred { revision })
        }
        WriteOp::Put {
            namespace,
            key,
            value,
        } => {
            meter.charge(OperationClass::DirectoryLookup, 1)?;
            let existing = resolve(state, nodes, namespace, meter)?;
            authorize(&existing, authority)?;
            // Two logical entry writes: the raw entry and the catalog leaf.
            meter.charge_group(&[
                (OperationClass::PutRecord, 1),
                (OperationClass::WriteKeyByte, units(key.len())),
                (OperationClass::WriteValueByte, units(value.len())),
                (OperationClass::RootUpdate, 2),
            ])?;
            let mut probe = Probe::new();
            let inserted = existing.records.insert(
                &raw_key(&key),
                Blob(Arc::from(value)),
                &nodes.namespace(namespace).records,
                &mut probe,
            );
            meter.note_record_nodes(probe.nodes);
            let (records, previous) = inserted?;
            let updated = NamespaceState {
                records,
                ..existing
            };
            commit_namespace(state, nodes, namespace, updated, meter)?;
            Ok(Effect::Put {
                replaced: previous.is_some(),
            })
        }
        WriteOp::Delete { namespace, key } => {
            meter.charge(OperationClass::DirectoryLookup, 1)?;
            let existing = resolve(state, nodes, namespace, meter)?;
            authorize(&existing, authority)?;
            // Two logical entry writes (the raw entry and the catalog leaf),
            // charged whether or not the key exists so the charge does not
            // depend on the lookup.
            meter.charge_group(&[
                (OperationClass::DeleteRecord, 1),
                (OperationClass::WriteKeyByte, units(key.len())),
                (OperationClass::RootUpdate, 2),
            ])?;
            let mut probe = Probe::new();
            let removed = existing.records.remove(
                &raw_key(&key),
                &nodes.namespace(namespace).records,
                &mut probe,
            );
            meter.note_record_nodes(probe.nodes);
            let (records, previous) = removed?;
            let updated = NamespaceState {
                records,
                ..existing
            };
            commit_namespace(state, nodes, namespace, updated, meter)?;
            Ok(Effect::Deleted {
                existed: previous.is_some(),
            })
        }
        WriteOp::CreateRecord {
            namespace,
            key,
            cells,
        } => {
            meter.charge(OperationClass::DirectoryLookup, 1)?;
            let existing = resolve(state, nodes, namespace, meter)?;
            authorize(&existing, authority)?;
            let loaders = nodes.namespace(namespace);
            let (typed, id) = typed::create(&existing.typed, &loaders, key, cells, meter)?;
            let updated = NamespaceState { typed, ..existing };
            commit_namespace(state, nodes, namespace, updated, meter)?;
            Ok(Effect::RecordCreated { id })
        }
        WriteOp::PatchRecord {
            namespace,
            key,
            set,
            remove,
        } => {
            meter.charge(OperationClass::DirectoryLookup, 1)?;
            let existing = resolve(state, nodes, namespace, meter)?;
            authorize(&existing, authority)?;
            let loaders = nodes.namespace(namespace);
            let (typed, id) =
                typed::patch(&existing.typed, &loaders, limits, &key, set, remove, meter)?;
            let updated = NamespaceState { typed, ..existing };
            commit_namespace(state, nodes, namespace, updated, meter)?;
            Ok(Effect::RecordPatched { id })
        }
        WriteOp::DeleteRecord { namespace, key } => {
            meter.charge(OperationClass::DirectoryLookup, 1)?;
            let existing = resolve(state, nodes, namespace, meter)?;
            authorize(&existing, authority)?;
            let loaders = nodes.namespace(namespace);
            let (typed, id) = typed::delete(&existing.typed, &loaders, &key, meter)?;
            let updated = NamespaceState { typed, ..existing };
            commit_namespace(state, nodes, namespace, updated, meter)?;
            Ok(Effect::RecordDeleted { id })
        }
    }
}

/// Closes a read's meter: a value or a rejection with its receipt, or the
/// storage error that stopped it (no receipt).
fn finish_read<T>(
    meter: Meter<'_>,
    outcome: Result<T, Fault>,
) -> Result<ReadResult<T>, StorageError> {
    match outcome {
        Ok(value) => {
            let (receipt, physical) = meter.finish(ReceiptKind::Read, Outcome::Applied);
            Ok(Ok(Metered {
                value,
                receipt,
                physical,
            }))
        }
        Err(Fault::Reject(reason)) => {
            let (receipt, physical) = meter.finish(ReceiptKind::Read, Outcome::Rejected(reason));
            Ok(Err(Rejected {
                reason,
                receipt,
                physical,
            }))
        }
        Err(Fault::Storage(error)) => Err(error),
    }
}

fn read_get(
    state: &WorkingState,
    nodes: &Nodes,
    pricing: &PriceSchedule,
    namespace: NamespaceId,
    key: &[u8],
    budget: Budget,
) -> Result<ReadResult<Option<Vec<u8>>>, StorageError> {
    if key.len() > Limits::MAX_RAW_KEY_LEN {
        return Ok(Err(zero_cost(
            pricing,
            budget,
            ReceiptKind::Read,
            Rejection::InvalidArgument(crate::ArgumentFault::KeyTooLong),
        )));
    }
    let mut meter = Meter::new(pricing, budget, None);
    let outcome = get_inner(state, nodes, namespace, key, &mut meter);
    finish_read(meter, outcome)
}

fn get_inner(
    state: &WorkingState,
    nodes: &Nodes,
    namespace: NamespaceId,
    key: &[u8],
    meter: &mut Meter<'_>,
) -> Result<Option<Vec<u8>>, Fault> {
    meter.charge(OperationClass::DirectoryLookup, 1)?;
    let existing = resolve(state, nodes, namespace, meter)?;
    meter.charge(OperationClass::PointRead, 1)?;
    let mut probe = Probe::new();
    let found = existing.records.get(
        &raw_key(key),
        &nodes.namespace(namespace).records,
        &mut probe,
    );
    meter.note_record_nodes(probe.nodes);
    let Some(value) = found? else {
        return Ok(None);
    };
    // The value's length is known only after the lookup; the bytes are
    // charged before they are copied out.
    meter.charge(OperationClass::ReadByte, units(value.0.len()))?;
    Ok(Some(value.0.to_vec()))
}

fn read_scan(
    state: &WorkingState,
    nodes: &Nodes,
    pricing: &PriceSchedule,
    namespace: NamespaceId,
    prefix: &[u8],
    budget: Budget,
) -> Result<ReadResult<Vec<Entry>>, StorageError> {
    if prefix.len() > Limits::MAX_RAW_KEY_LEN {
        return Ok(Err(zero_cost(
            pricing,
            budget,
            ReceiptKind::Read,
            Rejection::InvalidArgument(crate::ArgumentFault::KeyTooLong),
        )));
    }
    let mut meter = Meter::new(pricing, budget, None);
    let outcome = scan_inner(state, nodes, namespace, prefix, &mut meter);
    finish_read(meter, outcome)
}

fn scan_inner(
    state: &WorkingState,
    nodes: &Nodes,
    namespace: NamespaceId,
    prefix: &[u8],
    meter: &mut Meter<'_>,
) -> Result<Vec<Entry>, Fault> {
    meter.charge(OperationClass::DirectoryLookup, 1)?;
    let existing = resolve(state, nodes, namespace, meter)?;
    meter.charge(OperationClass::ScanSeek, 1)?;
    let loaders = nodes.namespace(namespace);
    // The encoding of the prefix sorts at or before every key that extends
    // it and after every smaller key, so it is the seek position.
    let mut range = existing
        .records
        .range_from(&raw_key(prefix), &loaders.records);
    let mut entries = Vec::new();
    let outcome = loop {
        // Stepping to the next entry is the bounded step that reveals its
        // size; the entry is charged before it is materialized. An abort at
        // entry k has charged k - 1 entries; the nodes stepped into to
        // reveal entry k were visited but its entry was neither charged nor
        // returned. The physical count covers every trie node the iterator
        // stepped into (the seek path, then each node once as the walk
        // reaches it), so it is at most the map's node count, not equal to
        // the number of entries.
        let (key, value) = match range.next() {
            None => break Ok(()),
            Some(Err(error)) => break Err(Fault::from(error)),
            Some(Ok(entry)) => entry,
        };
        if !key.0.starts_with(prefix) {
            break Ok(());
        }
        if let Err(reason) = meter.charge_group(&[
            (OperationClass::ScanEntry, 1),
            (OperationClass::ReadByte, units(key.0.len() + value.0.len())),
        ]) {
            break Err(reason.into());
        }
        entries.push(Entry {
            key: key.0.to_vec(),
            value: value.0.to_vec(),
        });
    };
    meter.note_record_nodes(range.probe().nodes);
    outcome.map(|()| entries)
}

fn read_descriptor(
    state: &WorkingState,
    nodes: &Nodes,
    pricing: &PriceSchedule,
    namespace: NamespaceId,
    budget: Budget,
) -> Result<ReadResult<NamespaceDescriptor>, StorageError> {
    let mut meter = Meter::new(pricing, budget, None);
    let outcome = meter
        .charge(OperationClass::DirectoryLookup, 1)
        .map_err(Fault::from)
        .and_then(|()| resolve(state, nodes, namespace, &mut meter).map(|found| found.descriptor));
    finish_read(meter, outcome)
}

#[allow(clippy::too_many_arguments)]
fn read_record(
    state: &WorkingState,
    nodes: &Nodes,
    pricing: &PriceSchedule,
    limits: &Limits,
    namespace: NamespaceId,
    key: &RecordKey,
    projection: &Projection,
    budget: Budget,
) -> Result<ReadResult<Option<Row>>, StorageError> {
    if let Err(fault) = typed::validate_projection(limits, projection) {
        let reason = Rejection::InvalidQuery(fault);
        return Ok(Err(zero_cost(pricing, budget, ReceiptKind::Read, reason)));
    }
    let mut meter = Meter::new(pricing, budget, None);
    let loaders = nodes.namespace(namespace);
    let outcome = meter
        .charge(OperationClass::DirectoryLookup, 1)
        .map_err(Fault::from)
        .and_then(|()| resolve(state, nodes, namespace, &mut meter))
        .and_then(|existing| {
            typed::get_record(&existing.typed, &loaders, key, projection, &mut meter)
        });
    finish_read(meter, outcome)
}

/// One query request with its cursor context.
struct QueryRequest<'r> {
    limits: &'r Limits,
    namespace: NamespaceId,
    query: &'r Query,
    projection: &'r Projection,
    limit: usize,
    cursor: Option<&'r Cursor>,
    context: CursorContext,
}

impl QueryRequest<'_> {
    /// Admission: the query shape and the projection against the limits,
    /// then the cursor binding. All are decided before any charge.
    fn validate(&self) -> Result<Option<RecordId>, Rejection> {
        typed::validate_query(self.limits, self.query, self.limit)
            .map_err(Rejection::InvalidQuery)?;
        typed::validate_projection(self.limits, self.projection)
            .map_err(Rejection::InvalidQuery)?;
        let Some(cursor) = self.cursor else {
            return Ok(None);
        };
        let bound = cursor.context == self.context
            && cursor.namespace == self.namespace
            && *cursor.query == *self.query
            && *cursor.projection == *self.projection;
        if bound {
            Ok(Some(cursor.after))
        } else {
            Err(Rejection::CursorMismatch)
        }
    }

    fn continuation(&self, after: RecordId) -> Cursor {
        let (query, projection) = match self.cursor {
            Some(cursor) => (Arc::clone(&cursor.query), Arc::clone(&cursor.projection)),
            None => (
                Arc::new(self.query.clone()),
                Arc::new(self.projection.clone()),
            ),
        };
        Cursor {
            context: self.context,
            namespace: self.namespace,
            query,
            projection,
            after,
        }
    }
}

fn read_query(
    state: &WorkingState,
    nodes: &Nodes,
    pricing: &PriceSchedule,
    request: &QueryRequest<'_>,
    budget: Budget,
) -> Result<ReadResult<QueryPage>, StorageError> {
    let after = match request.validate() {
        Ok(after) => after,
        Err(reason) => return Ok(Err(zero_cost(pricing, budget, ReceiptKind::Read, reason))),
    };
    let mut meter = Meter::new(pricing, budget, None);
    let loaders = nodes.namespace(request.namespace);
    let outcome = meter
        .charge(OperationClass::DirectoryLookup, 1)
        .map_err(Fault::from)
        .and_then(|()| resolve(state, nodes, request.namespace, &mut meter))
        .and_then(|existing| {
            typed::query(
                &existing.typed,
                &loaders,
                request.query,
                request.projection,
                request.limit,
                after,
                &mut meter,
            )
        })
        .map(|(rows, next)| QueryPage {
            rows,
            next: next.map(|after| request.continuation(after)),
        });
    finish_read(meter, outcome)
}

fn read_count(
    state: &WorkingState,
    nodes: &Nodes,
    pricing: &PriceSchedule,
    limits: &Limits,
    namespace: NamespaceId,
    query: &Query,
    budget: Budget,
) -> Result<ReadResult<u64>, StorageError> {
    if let Err(fault) = typed::validate_query(limits, query, 1) {
        let reason = Rejection::InvalidQuery(fault);
        return Ok(Err(zero_cost(pricing, budget, ReceiptKind::Read, reason)));
    }
    let mut meter = Meter::new(pricing, budget, None);
    let loaders = nodes.namespace(namespace);
    let outcome = meter
        .charge(OperationClass::DirectoryLookup, 1)
        .map_err(Fault::from)
        .and_then(|()| resolve(state, nodes, namespace, &mut meter))
        .and_then(|existing| typed::count(&existing.typed, &loaders, query, &mut meter));
    finish_read(meter, outcome)
}

/// One `terms` request.
struct TermsRequest<'r> {
    limits: &'r Limits,
    namespace: NamespaceId,
    name: &'r CellName,
    tag: TypeTag,
    lower: &'r Bound<Value>,
    limit: usize,
}

fn read_terms(
    state: &WorkingState,
    nodes: &Nodes,
    pricing: &PriceSchedule,
    request: &TermsRequest<'_>,
    budget: Budget,
) -> Result<ReadResult<Vec<Value>>, StorageError> {
    if let Err(fault) =
        typed::validate_terms(request.limits, request.tag, request.lower, request.limit)
    {
        let reason = Rejection::InvalidQuery(fault);
        return Ok(Err(zero_cost(pricing, budget, ReceiptKind::Read, reason)));
    }
    let mut meter = Meter::new(pricing, budget, None);
    let loaders = nodes.namespace(request.namespace);
    let outcome = meter
        .charge(OperationClass::DirectoryLookup, 1)
        .map_err(Fault::from)
        .and_then(|()| resolve(state, nodes, request.namespace, &mut meter))
        .and_then(|existing| {
            typed::terms(
                &existing.typed,
                &loaders,
                request.name,
                request.tag,
                request.lower,
                request.limit,
                &mut meter,
            )
        });
    finish_read(meter, outcome)
}

#[cfg(test)]
mod tests {
    //! What the integration tests cannot see: after a storage error the
    //! branch refuses every call, so only here can the test check that the
    //! failed operation left the working state exactly as it was.

    use std::{
        collections::HashMap,
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;

    /// Committed objects, failing every read after `allowance` reads.
    struct Reader {
        objects: HashMap<(SpaceId, ObjectId), Vec<u8>>,
        allowance: AtomicU64,
        reads: AtomicU64,
    }

    impl ObjectReader for Reader {
        fn read_object(
            &self,
            space: SpaceId,
            id: &ObjectId,
        ) -> Result<Option<Vec<u8>>, StorageError> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            let left = self.allowance.load(Ordering::SeqCst);
            if left == 0 {
                return Err(StorageError::new(StorageErrorKind::Io, "injected"));
            }
            self.allowance.store(left - 1, Ordering::SeqCst);
            Ok(self.objects.get(&(space, *id)).cloned())
        }
    }

    fn name(text: &str) -> CellName {
        CellName::new(text).unwrap()
    }

    fn record(key: &[u8], kind: &str) -> WriteOp {
        WriteOp::CreateRecord {
            namespace: NamespaceId::from_ordinal(1).unwrap(),
            key: RecordKey::new(key).unwrap(),
            cells: vec![
                (name("kind"), Cell::attribute(Value::str(kind))),
                (name("n"), Cell::attribute(Value::I64(7))),
            ],
        }
    }

    /// Hosts share snapshots across threads and move branches and sealed
    /// candidates between them.
    #[test]
    fn engine_handles_are_send_and_sync() {
        fn send_sync<T: Send + Sync>() {}
        send_sync::<Engine>();
        send_sync::<Snapshot>();
        send_sync::<Branch>();
        send_sync::<SealedCandidate>();
    }

    #[test]
    fn a_failed_operation_leaves_the_branch_state_as_it_was() {
        let owner = OwnerId::new([1; 20]);
        let mut memory = Engine::new(PriceSchedule::FIXTURE);
        let mut branch = memory.begin(memory.head(), None).unwrap();
        let create = WriteOp::CreateNamespace {
            name: NamespaceName::new("alpha").unwrap(),
        };
        branch.execute(owner, create, Budget::unlimited()).unwrap();
        for index in 0u8..8 {
            let kind = ["red", "rose"][usize::from(index % 2)];
            let op = record(&[b'r', index], kind);
            branch.execute(owner, op, Budget::unlimited()).unwrap();
        }
        for key in [[1; 32], [2; 32]] {
            branch.try_host_put(key, b"due").unwrap().unwrap();
        }
        memory.publish(&branch.seal()).unwrap();
        let head = memory.head_snapshot();
        let reader = Arc::new(Reader {
            objects: head
                .staged_objects()
                .into_iter()
                .map(|object| ((object.space, object.id), object.bytes))
                .collect(),
            allowance: AtomicU64::new(u64::MAX),
            reads: AtomicU64::new(0),
        });
        let engine =
            Engine::from_state(&head.state_header(), Limits::DEFAULT, reader.clone()).unwrap();

        // The operation under test, after one earlier applied write (to the
        // raw map, so the typed maps stay stored) so the working state
        // differs from the base.
        let failing = record(b"new", "ruby");
        let prepare = |branch: &mut Branch| {
            reader.allowance.store(u64::MAX, Ordering::SeqCst);
            let op = WriteOp::Put {
                namespace: NamespaceId::from_ordinal(1).unwrap(),
                key: b"raw".to_vec(),
                value: b"value".to_vec(),
            };
            branch.execute(owner, op, Budget::unlimited()).unwrap();
        };
        let mut branch = engine.begin(engine.head(), None).unwrap();
        prepare(&mut branch);
        let before = reader.reads.load(Ordering::SeqCst);
        branch
            .execute(owner, failing.clone(), Budget::unlimited())
            .unwrap();
        let needed = reader.reads.load(Ordering::SeqCst) - before;
        assert!(needed > 4, "the create loads the stored typed maps");

        for allowance in 0..needed {
            let mut branch = engine.begin(engine.head(), None).unwrap();
            prepare(&mut branch);
            let state = branch.working.header();
            let mutations = branch.mutations;
            reader.allowance.store(allowance, Ordering::SeqCst);
            let result = branch.try_execute(owner, failing.clone(), Budget::unlimited());
            assert!(result.is_err(), "failing after {allowance} of {needed}");
            assert_eq!(branch.working.header(), state, "after {allowance}");
            assert_eq!(branch.mutations, mutations);
            assert_eq!(branch.receipts.len(), 1, "only the earlier write");
            assert!(branch.is_poisoned());

            // A failing host-state write (the host map is stored) changes
            // nothing either.
            let mut branch = engine.begin(engine.head(), None).unwrap();
            prepare(&mut branch);
            let state = branch.working.header();
            reader.allowance.store(0, Ordering::SeqCst);
            assert!(branch.try_host_put([3; 32], b"later").is_err());
            assert_eq!(branch.working.header(), state);
            assert!(branch.is_poisoned());
        }
    }
}
