//! Durable engine-only publications over a [`VersionedStore`].
//!
//! All retained states live in storage. The facade retains one stored-root
//! head and at most one bounded prepared batch, never a history vector.
//! Callers grant it exclusive writer/recovery authority over the store.
//! This profile stores no node blocks, receipts or signing state.
//!
//! State history is retained according to [`Retention`] (archive by
//! default); see the [`retention`] module for the floor and pin semantics.

pub(crate) mod cache;
mod format;
mod prepare;
pub mod reachability;
pub mod retention;
pub mod simulator;

use crate::{B256, Branch, Cost, EngineError, SealedCandidate, Snapshot, SnapshotId, nodes::Nodes};
use arkiv_db_storage::versioned::{
    CommitBatch, CommitOutcome, Head, Health, StorageError, StorageErrorKind, Table, VersionedStore,
};
pub use cache::CacheStats;
use cache::Context;
pub use format::{EngineGenesis, EngineManifest};
use format::{GENESIS_KEY, GENESIS_MAX_BYTES, MANIFEST_MAX_BYTES, PROFILE_KEY, profile_bytes};
pub use prepare::PreparationStats;
use retention::RetentionState;
pub use retention::{Retention, RetentionStatus};
use std::{
    fmt,
    sync::{Arc, atomic::Ordering},
};

/// Runtime policy; none of these options changes logical state.
/// `retention` decides which historical snapshots this node serves; it is a
/// node-local choice, not part of genesis.
#[derive(Clone, Debug)]
pub struct Options {
    pub cache_bytes: usize,
    pub cache_entries: usize,
    pub prepare_bytes: usize,
    pub prepare_objects: usize,
    pub prepare_visits: u64,
    pub retention: Retention,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            cache_bytes: 8 * 1024 * 1024,
            cache_entries: 16_384,
            prepare_bytes: 32 * 1024 * 1024,
            prepare_objects: 100_000,
            prepare_visits: 1_000_000,
            retention: Retention::Archive,
        }
    }
}
impl Options {
    fn validate(&self) -> Result<(), DurableError> {
        for (count, width) in [(self.cache_entries, 128usize), (self.prepare_objects, 128)] {
            let bytes = count
                .checked_mul(width)
                .ok_or_else(|| incompatible("container limit overflow"))?;
            if bytes > isize::MAX as usize {
                return Err(incompatible(
                    "container limit exceeds addressable allocation",
                ));
            }
        }
        if self.cache_bytes > isize::MAX as usize || self.prepare_bytes > isize::MAX as usize {
            return Err(incompatible("payload limit exceeds addressable allocation"));
        }
        Ok(())
    }
}

/// Lifecycle failures issue no deterministic engine receipt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DurableError {
    Engine(EngineError),
    Storage(StorageError),
    ResourceLimit(String),
    SequenceExhausted,
    StaleGeneration,
    WriterUnavailable,
    AlreadyPrepared,
    NoPrepared,
}
impl fmt::Display for DurableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Engine(e) => e.fmt(f),
            Self::Storage(e) => e.fmt(f),
            Self::ResourceLimit(s) => write!(f, "storage preparation limit: {s}"),
            other => write!(f, "durable lifecycle: {other:?}"),
        }
    }
}
impl std::error::Error for DurableError {}
impl From<StorageError> for DurableError {
    fn from(e: StorageError) -> Self {
        Self::Storage(e)
    }
}
impl From<EngineError> for DurableError {
    fn from(e: EngineError) -> Self {
        Self::Engine(e)
    }
}
pub(crate) fn corrupt(s: impl Into<String>) -> DurableError {
    StorageError::new(StorageErrorKind::Corrupt, s).into()
}
pub(crate) fn incompatible(s: impl Into<String>) -> DurableError {
    StorageError::new(StorageErrorKind::Incompatible, s).into()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriterState {
    Ready,
    Prepared,
    Uncertain,
    Unavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommitInfo {
    pub snapshot: SnapshotId,
    pub commit_id: B256,
    pub state_root: B256,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Publication {
    Committed(CommitInfo),
    NotCommitted(StorageError),
    Indeterminate(StorageError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recovery {
    Ready,
    RetryPrepared,
    /// Exact application observed after uncertainty. Reading it back does
    /// not certify medium durability; this handle may not write again.
    ObservedCommittedDurabilityUnknown(CommitInfo),
}

/// The original candidate is returned on every precommit preparation failure.
#[derive(Debug)]
pub struct PrepareFailure {
    pub error: DurableError,
    pub candidate: Box<SealedCandidate>,
}
impl fmt::Display for PrepareFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(f)
    }
}
impl std::error::Error for PrepareFailure {}

struct Prepared {
    batch: CommitBatch,
    manifest: EngineManifest,
    snapshot: Snapshot,
    info: CommitInfo,
    generation: u64,
    /// The state floor this publication records.
    floor: u64,
}

/// Noncanonical resident-state and preparation counters; backend/caller
/// transient ownership is reported separately, not hidden in cache bytes.
#[derive(Clone, Debug)]
pub struct DurableStats {
    pub cache: CacheStats,
    pub last_preparation: PreparationStats,
    pub published_sequence: u64,
    pub resident_manifests: usize,
    pub pending_payload_bytes: u64,
    pub retention: RetentionStatus,
}

pub struct DurableEngine {
    context: Arc<Context>,
    genesis: EngineGenesis,
    options: Options,
    instance: u64,
    head: Snapshot,
    manifest: EngineManifest,
    pending: Option<Prepared>,
    state: WriterState,
    last_preparation: PreparationStats,
    simulator: Option<simulator::SimulatorIdentity>,
    retention: Arc<RetentionState>,
}
impl fmt::Debug for DurableEngine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DurableEngine")
            .field("head", &self.head())
            .field("writer", &self.state)
            .finish_non_exhaustive()
    }
}

fn required(
    store: &dyn VersionedStore,
    table: Table,
    key: &[u8],
    cap: usize,
) -> Result<Vec<u8>, DurableError> {
    store
        .read_record_bounded(table, key, cap)?
        .ok_or_else(|| corrupt(format!("required {table:?} record missing")))
}
fn head_of(manifest: &EngineManifest) -> Head {
    Head {
        height: manifest.sequence,
        hash: manifest.id().0,
    }
}

fn read_manifest(
    store: &dyn VersionedStore,
    genesis: &EngineGenesis,
    sequence: u64,
) -> Result<EngineManifest, DurableError> {
    let bytes = required(
        store,
        Table::Manifests,
        &sequence.to_be_bytes(),
        MANIFEST_MAX_BYTES,
    )?;
    let manifest = EngineManifest::from_envelope(&bytes)?;
    if manifest.sequence != sequence || manifest.genesis != genesis.id() {
        return Err(corrupt("manifest sequence/genesis association"));
    }
    let index = required(store, Table::BlockIndex, manifest.id().as_slice(), 8)?;
    if index != sequence.to_be_bytes() {
        return Err(corrupt("manifest index association"));
    }
    if let Some((parent_sequence, id)) = manifest.parent {
        let index = required(store, Table::BlockIndex, id.as_slice(), 8)?;
        if index != parent_sequence.to_be_bytes() {
            return Err(corrupt("manifest parent index association"));
        }
    } else if manifest.state != genesis.initial_header() {
        return Err(corrupt(
            "genesis state does not match configured initial state",
        ));
    }
    Ok(manifest)
}

fn read_open(
    store: &dyn VersionedStore,
    expected: &EngineGenesis,
    simulator: Option<&simulator::SimulatorIdentity>,
) -> Result<EngineManifest, DurableError> {
    if let Health::Failed(e) = store.health() {
        return Err(e.into());
    }
    let profile = required(store, Table::Meta, PROFILE_KEY, 12)?;
    if profile.len() != 12 {
        return Err(corrupt("malformed engine profile length"));
    }
    let expected_profile = simulator.map_or_else(profile_bytes, |_| simulator::profile_bytes());
    if profile != expected_profile {
        return Err(incompatible("unknown engine profile/version"));
    }
    if let Some(identity) = simulator {
        let found = simulator::read_identity(store)?;
        if found != *identity {
            return Err(incompatible(
                "simulator identity/config does not match file",
            ));
        }
    }
    let bytes = required(store, Table::Meta, GENESIS_KEY, GENESIS_MAX_BYTES + 32)?;
    let genesis = EngineGenesis::from_envelope(&bytes)?;
    if genesis != *expected {
        return Err(incompatible(
            "expected engine genesis/config does not match file",
        ));
    }
    let head = store
        .head()?
        .ok_or_else(|| corrupt("engine head missing"))?;
    let zero = read_manifest(store, expected, 0)?;
    let manifest = if head.height == 0 {
        zero
    } else {
        read_manifest(store, expected, head.height)?
    };
    if head_of(&manifest) != head {
        return Err(corrupt("head/manifest association"));
    }
    if simulator.is_some() {
        simulator::read_block(store, &manifest)?;
    }
    Ok(manifest)
}

/// The state floor recorded with the commit of `sequence`; zero if none.
fn read_floor(store: &dyn VersionedStore, sequence: u64) -> Result<u64, DurableError> {
    let Some(bytes) =
        store.read_record_bounded(Table::Meta, retention::floor_key(sequence).as_bytes(), 8)?
    else {
        return Ok(0);
    };
    let floor = <[u8; 8]>::try_from(bytes.as_slice())
        .map(u64::from_be_bytes)
        .map_err(|_| corrupt("malformed state floor record"))?;
    if floor == 0 || floor > sequence {
        return Err(corrupt("state floor record outside the published range"));
    }
    Ok(floor)
}

fn manifest_batch(parent: Option<Head>, manifest: &EngineManifest) -> CommitBatch {
    let mut batch = CommitBatch::new(parent, head_of(manifest));
    batch
        .put_record(
            Table::Manifests,
            manifest.sequence.to_be_bytes(),
            manifest.envelope(),
        )
        .put_record(
            Table::BlockIndex,
            manifest.id().0,
            manifest.sequence.to_be_bytes().to_vec(),
        );
    batch
}

/// Prove that genesis is self-contained using only staged objects. Normal
/// commits may stop staging at stored links; an initializer must not inherit
/// those links from another database. Iteration retains only traversal stacks,
/// and the shared allowance bounds both object loads and decoded entries.
fn verify_genesis_objects(
    candidate: &SealedCandidate,
    genesis: &EngineGenesis,
    options: &Options,
    objects: &[(arkiv_db_storage::versioned::SpaceId, [u8; 32], Vec<u8>)],
) -> Result<(), DurableError> {
    use arkiv_db_storage::versioned::SpaceId;
    use arkiv_db_trie::{MerkleMap, NodeLoader, TrieKey, TrieValue};
    use std::{cell::Cell, collections::BTreeMap};

    struct Loader<'a> {
        objects: &'a BTreeMap<(SpaceId, [u8; 32]), &'a [u8]>,
        space: SpaceId,
        remaining: &'a Cell<u64>,
    }
    struct VerificationError(DurableError);
    impl From<arkiv_db_trie::TrieError> for VerificationError {
        fn from(error: arkiv_db_trie::TrieError) -> Self {
            Self(incompatible(format!(
                "genesis objects are not self-contained: {error}"
            )))
        }
    }
    impl Loader<'_> {
        fn spend(&self) -> Result<(), DurableError> {
            let remaining = self.remaining.get().checked_sub(1).ok_or_else(|| {
                DurableError::ResourceLimit("genesis verification visit cap".into())
            })?;
            self.remaining.set(remaining);
            Ok(())
        }
    }
    impl<V: TrieValue> NodeLoader<V> for Loader<'_> {
        type Error = VerificationError;
        fn read(&self, id: &B256) -> Result<Option<Vec<u8>>, Self::Error> {
            self.spend().map_err(VerificationError)?;
            Ok(self
                .objects
                .get(&(self.space, id.0))
                .map(|bytes| bytes.to_vec()))
        }
    }
    fn check_map<K: TrieKey, V: TrieValue>(
        map: &MerkleMap<K, V>,
        loader: &Loader<'_>,
        mut visit: impl FnMut(K, V) -> Result<(), DurableError>,
    ) -> Result<(), DurableError> {
        let stored = map.to_stored();
        let mut count = 0u64;
        for entry in stored.iter(loader) {
            loader.spend()?;
            let (key, value) = entry.map_err(|error| error.0)?;
            visit(key, value)?;
            count += 1;
        }
        if count != map.len() {
            return Err(incompatible("genesis map entry count mismatch"));
        }
        Ok(())
    }

    let objects: BTreeMap<_, _> = objects
        .iter()
        .map(|(space, id, bytes)| ((*space, *id), bytes.as_slice()))
        .collect();
    let remaining = Cell::new(options.prepare_visits);
    let loader = |space| Loader {
        objects: &objects,
        space,
        remaining: &remaining,
    };
    check_map(
        &candidate.state.catalog,
        &loader(SpaceId::Catalog),
        |id, leaf| {
            if leaf.descriptor.id != id || id.ordinal() >= candidate.state.next_namespace {
                return Err(incompatible("genesis namespace association/allocator"));
            }
            let loader = loader(SpaceId::Namespace(id.ordinal()));
            // Raw keys have a fixed codec bound; raw values intentionally have
            // no Limits value-length admission rule. Preflight has checked their
            // codec width and the genesis object-byte ceiling.
            check_map(&leaf.records, &loader, |_, _| Ok(()))?;
            check_map(&leaf.typed.keys, &loader, |_, _| Ok(()))?;
            check_map(&leaf.typed.rows, &loader, |_, row| {
                crate::typed::validate_create(&genesis.limits, &row.cells)
                    .map_err(|_| incompatible("genesis row exceeds engine admission limits"))
            })?;
            check_map(&leaf.typed.terms, &loader, |_, posting| {
                check_map(&posting.0, &loader, |_, _| Ok(()))
            })
        },
    )?;
    check_map(&candidate.state.host, &loader(SpaceId::Host), |_, blob| {
        if blob.0.len() > crate::HOST_VALUE_MAX {
            return Err(incompatible("genesis host value exceeds engine limit"));
        }
        Ok(())
    })
}

impl DurableEngine {
    /// Explicit initialization. Uncertainty returns a recovery token and no
    /// engine/head. Never call this as a fallback for an open error.
    pub fn initialize(
        store: Arc<dyn VersionedStore>,
        genesis: EngineGenesis,
        options: Options,
    ) -> Result<Initialization, DurableError> {
        Self::initialize_inner(store, genesis, options, None, None, None)
    }

    fn initialize_inner(
        store: Arc<dyn VersionedStore>,
        genesis: EngineGenesis,
        options: Options,
        simulator: Option<simulator::SimulatorIdentity>,
        packet: Option<simulator::SimulatorPacket>,
        candidate: Option<SealedCandidate>,
    ) -> Result<Initialization, DurableError> {
        genesis.validate()?;
        options.validate()?;
        if genesis.initial_state.is_some() != candidate.is_some() {
            return Err(incompatible(
                "explicit initial state requires a prefilled genesis candidate",
            ));
        }
        if simulator.is_some() != packet.is_some() {
            return Err(incompatible(
                "simulator initialization requires a host packet",
            ));
        }
        if let Some(packet) = &packet
            && packet.reserved_bytes()? > options.prepare_bytes
        {
            return Err(DurableError::ResourceLimit(
                "host packet exceeds preparation cap".into(),
            ));
        }
        if let Some(identity) = &simulator {
            identity.validate(&genesis)?;
            if store
                .read_record_bounded(
                    Table::Meta,
                    simulator::IDENTITY_KEY,
                    simulator::IDENTITY_MAX_BYTES,
                )?
                .is_some()
            {
                return Err(incompatible("store already contains simulator metadata"));
            }
        }
        if store.head()?.is_some()
            || store
                .read_record_bounded(Table::Meta, PROFILE_KEY, 12)?
                .is_some()
            || store
                .read_record_bounded(Table::Meta, GENESIS_KEY, GENESIS_MAX_BYTES + 32)?
                .is_some()
        {
            return Err(incompatible(
                "store already contains a head or engine metadata",
            ));
        }
        let visits = if let Some(candidate) = &candidate {
            if candidate.base().sequence() != 0 || candidate.generation != 0 {
                return Err(incompatible(
                    "prefilled candidate must originate at genesis",
                ));
            }
            // Bound traversal and object encodings before computing the root.
            let visits = prepare::preflight(
                &candidate.state,
                genesis.max_object_bytes as usize,
                options.prepare_visits,
            )?;
            if candidate.state_header() != genesis.initial_header() {
                return Err(incompatible(
                    "prefilled candidate/initial state association",
                ));
            }
            visits
        } else {
            0
        };
        let header = genesis.initial_header();
        let manifest = EngineManifest {
            genesis: genesis.id(),
            sequence: 0,
            parent: None,
            state_root: header.root(),
            state: header,
        };
        let mut batch = manifest_batch(None, &manifest);
        let profile = simulator
            .as_ref()
            .map_or_else(profile_bytes, |_| simulator::profile_bytes());
        batch
            .put_record(Table::Meta, PROFILE_KEY, profile)
            .put_record(Table::Meta, GENESIS_KEY, genesis.envelope());
        if let Some(identity) = &simulator {
            batch.put_record(Table::Meta, simulator::IDENTITY_KEY, identity.envelope()?);
        }
        if let Some(packet) = packet {
            packet.append(&mut batch, &manifest)?;
        }
        if batch.logical_bytes() > options.prepare_bytes as u64 {
            return Err(DurableError::ResourceLimit(
                "genesis preparation bytes".into(),
            ));
        }
        let preparation = if let Some(candidate) = &candidate {
            let staging = prepare::Staging::collect(
                &candidate.state,
                &options,
                batch.logical_bytes() as usize,
                visits,
            )?;
            // A genesis has no earlier objects to inherit. Verify every root
            // and nested map using only this bounded batch before publishing.
            verify_genesis_objects(candidate, &genesis, &options, &staging.objects)?;
            batch.objects = staging.objects;
            staging.stats
        } else {
            PreparationStats::default()
        };
        let mut engine = Self::from_manifest(store, genesis, options, manifest, simulator)?;
        engine.last_preparation = preparation;
        InitializationPending {
            engine,
            batch,
            retry_allowed: true,
        }
        .commit()
    }

    /// Reads a bounded set of metadata/manifests/index entries and zero trie
    /// objects. The adapter must have completed its startup recovery first.
    pub fn open(
        store: Arc<dyn VersionedStore>,
        genesis: EngineGenesis,
        options: Options,
    ) -> Result<Self, DurableError> {
        genesis.validate()?;
        options.validate()?;
        let manifest = read_open(store.as_ref(), &genesis, None)?;
        Self::from_manifest(store, genesis, options, manifest, None)
    }

    fn from_manifest(
        store: Arc<dyn VersionedStore>,
        genesis: EngineGenesis,
        options: Options,
        manifest: EngineManifest,
        simulator: Option<simulator::SimulatorIdentity>,
    ) -> Result<Self, DurableError> {
        let durable = read_floor(store.as_ref(), manifest.sequence)?;
        if durable > 0 && options.retention == Retention::Archive {
            return Err(incompatible(format!(
                "state history below snapshot {durable} is declared pruned; \
                 archive retention cannot be restored, configure a window"
            )));
        }
        let retention = RetentionState::new(
            durable,
            durable.max(options.retention.floor_at(manifest.sequence)),
        );
        let instance = crate::engine::NEXT_INSTANCE.fetch_add(1, Ordering::Relaxed);
        let context = Arc::new(Context::new(
            store,
            genesis.max_object_bytes as usize,
            &options,
        ));
        let head = Snapshot::from_header(
            SnapshotId::from_sequence(manifest.sequence),
            manifest.parent.map(|p| SnapshotId::from_sequence(p.0)),
            instance,
            genesis.limits,
            &manifest.state,
            Nodes::durable(context.clone()),
            Some(retention.pin(manifest.sequence)),
        )?;
        Ok(Self {
            context,
            genesis,
            options,
            instance,
            head,
            manifest,
            pending: None,
            state: WriterState::Ready,
            last_preparation: PreparationStats::default(),
            simulator,
            retention,
        })
    }

    pub fn head(&self) -> SnapshotId {
        self.head.id()
    }
    pub fn head_snapshot(&self) -> Snapshot {
        self.head.clone()
    }
    pub fn head_info(&self) -> CommitInfo {
        CommitInfo {
            snapshot: self.head(),
            commit_id: self.manifest.id(),
            state_root: self.manifest.state_root,
        }
    }
    pub fn writer_state(&self) -> WriterState {
        self.state
    }
    pub fn genesis(&self) -> &EngineGenesis {
        &self.genesis
    }
    pub fn snapshot_count(&self) -> Result<u64, DurableError> {
        self.manifest
            .sequence
            .checked_add(1)
            .ok_or(DurableError::SequenceExhausted)
    }

    /// The lowest snapshot this engine serves. Every earlier one is refused
    /// with [`EngineError::HistoryPruned`]. Zero under archive retention.
    pub fn state_floor(&self) -> SnapshotId {
        SnapshotId::from_sequence(self.retention.floor())
    }

    /// The retention policy, floors and pins.
    pub fn retention_status(&self) -> RetentionStatus {
        RetentionStatus {
            retention: self.options.retention,
            floor: self.retention.floor(),
            durable_floor: self.retention.durable(),
            lowest_pinned: self.retention.lowest_pinned(),
            pinned_handles: self.retention.pinned_handles(),
        }
    }

    /// Fails with [`EngineError::HistoryPruned`] if `id` is below the floor,
    /// or [`EngineError::UnknownSnapshot`] if it is above the head. Reads
    /// nothing from storage.
    pub fn check_retained(&self, id: SnapshotId) -> Result<(), DurableError> {
        if id > self.head() {
            return Err(EngineError::UnknownSnapshot(id).into());
        }
        let floor = self.state_floor();
        if id < floor {
            return Err(EngineError::HistoryPruned {
                requested: id,
                floor,
            }
            .into());
        }
        Ok(())
    }

    /// Opens a retained snapshot. The pin on `id` is registered first and
    /// the floor is checked after it, before any manifest, object or cache
    /// access (a refusal releases the pin). This order matters for physical
    /// deletion, which is bounded by `min(durable floor, lowest pin)`: a
    /// floor that rises after the check can no longer pass an accepted
    /// reader unnoticed. The handle keeps the pin until its last clone is
    /// dropped and keeps reading even if the floor passes it meanwhile.
    pub fn snapshot(&self, id: SnapshotId) -> Result<Snapshot, DurableError> {
        self.snapshot_hooked(id, &mut || {})
    }

    /// [`Self::snapshot`] with a hook run between pinning and the floor
    /// check (tests assert the ordering through it).
    fn snapshot_hooked(
        &self,
        id: SnapshotId,
        pinned: &mut dyn FnMut(),
    ) -> Result<Snapshot, DurableError> {
        self.context.generation()?;
        if id > self.head() {
            return Err(EngineError::UnknownSnapshot(id).into());
        }
        let pin = self.retention.pin(id.sequence());
        pinned();
        // On refusal the pin is dropped with the error return.
        self.check_retained(id)?;
        let manifest = read_manifest(self.context.store.as_ref(), &self.genesis, id.sequence())?;
        Snapshot::from_header(
            id,
            manifest.parent.map(|p| SnapshotId::from_sequence(p.0)),
            self.instance,
            self.genesis.limits,
            &manifest.state,
            Nodes::durable(self.context.clone()),
            Some(pin),
        )
        .map_err(Into::into)
    }

    pub fn begin(
        &self,
        expected: SnapshotId,
        budget: Option<Cost>,
    ) -> Result<Branch, DurableError> {
        if self.state != WriterState::Ready {
            return Err(DurableError::WriterUnavailable);
        }
        let generation = self.context.generation()?;
        if expected != self.head() {
            return Err(EngineError::StaleHead {
                expected,
                head: self.head(),
            }
            .into());
        }
        Ok(self.head.branch(generation, budget))
    }

    pub fn prepare(&mut self, candidate: SealedCandidate) -> Result<CommitInfo, PrepareFailure> {
        self.prepare_packet(candidate, None)
    }

    fn prepare_packet(
        &mut self,
        candidate: SealedCandidate,
        packet: Option<simulator::SimulatorPacket>,
    ) -> Result<CommitInfo, PrepareFailure> {
        match self.prepare_inner(&candidate, packet.as_ref()) {
            Ok(prepared) => {
                let info = prepared.info;
                self.pending = Some(prepared);
                self.state = WriterState::Prepared;
                Ok(info)
            }
            Err(error) => Err(PrepareFailure {
                error,
                candidate: Box::new(candidate),
            }),
        }
    }

    fn prepare_inner(
        &mut self,
        candidate: &SealedCandidate,
        packet: Option<&simulator::SimulatorPacket>,
    ) -> Result<Prepared, DurableError> {
        if self.simulator.is_some() != packet.is_some() {
            return Err(incompatible(
                "publication API does not match durable profile",
            ));
        }
        if let Some(packet) = packet {
            packet.validate()?;
            if packet.reserved_bytes()? > self.options.prepare_bytes {
                return Err(DurableError::ResourceLimit(
                    "host packet exceeds preparation cap".into(),
                ));
            }
        }
        if self.pending.is_some() {
            return Err(DurableError::AlreadyPrepared);
        }
        if self.state != WriterState::Ready {
            return Err(DurableError::WriterUnavailable);
        }
        let generation = self.context.generation()?;
        if candidate.instance != self.instance {
            return Err(EngineError::ForeignCandidate.into());
        }
        if candidate.generation != generation {
            return Err(DurableError::StaleGeneration);
        }
        if candidate.base() != self.head() {
            return Err(EngineError::StaleHead {
                expected: candidate.base(),
                head: self.head(),
            }
            .into());
        }
        let sequence = self
            .manifest
            .sequence
            .checked_add(1)
            .ok_or(DurableError::SequenceExhausted)?;
        // No state_header/root/leaf encoding until every reachable new node
        // and nested map has passed the shared bounded preflight.
        let visits = prepare::preflight(
            &candidate.state,
            self.genesis.max_object_bytes as usize,
            self.options.prepare_visits,
        )?;
        let header = candidate.state_header();
        let manifest = EngineManifest {
            genesis: self.genesis.id(),
            sequence,
            parent: Some((self.manifest.sequence, self.manifest.id())),
            state_root: header.root(),
            state: header,
        };
        let mut batch = manifest_batch(Some(head_of(&self.manifest)), &manifest);
        let floor = self
            .retention
            .floor()
            .max(self.options.retention.floor_at(sequence));
        if floor > 0 {
            batch.put_record(
                Table::Meta,
                retention::floor_key(sequence).into_bytes(),
                floor.to_be_bytes().to_vec(),
            );
        }
        if let Some(packet) = packet {
            packet.append(&mut batch, &manifest)?;
        }
        let staging = prepare::Staging::collect(
            &candidate.state,
            &self.options,
            batch.logical_bytes() as usize,
            visits,
        )?;
        batch.objects = staging.objects;
        let info = CommitInfo {
            snapshot: SnapshotId::from_sequence(sequence),
            commit_id: manifest.id(),
            state_root: manifest.state_root,
        };
        let snapshot = Snapshot::from_header(
            info.snapshot,
            Some(self.head()),
            self.instance,
            self.genesis.limits,
            &manifest.state,
            Nodes::durable(self.context.clone()),
            Some(self.retention.pin(sequence)),
        )?;
        self.last_preparation = staging.stats;
        Ok(Prepared {
            batch,
            manifest,
            snapshot,
            info,
            generation,
            floor,
        })
    }

    /// Commit the sole prepared batch. On failure it stays owned here for
    /// retry/recovery; operations are never executed again.
    pub fn commit(&mut self) -> Result<Publication, DurableError> {
        if self.state != WriterState::Prepared {
            return Err(DurableError::WriterUnavailable);
        }
        let generation = self.context.generation()?;
        let pending = self.pending.as_ref().ok_or(DurableError::NoPrepared)?;
        if pending.generation != generation {
            return Err(DurableError::StaleGeneration);
        }
        match self.context.store.commit(&pending.batch) {
            CommitOutcome::Committed => {
                let p = self.pending.take().expect("prepared checked");
                self.head = p.snapshot;
                self.manifest = p.manifest;
                self.retention.committed(p.floor);
                self.state = WriterState::Ready;
                Ok(Publication::Committed(p.info))
            }
            CommitOutcome::NotCommitted(e) => {
                if matches!(self.context.store.health(), Health::Failed(_))
                    || e.kind == StorageErrorKind::HeadMismatch
                {
                    self.state = WriterState::Unavailable;
                }
                Ok(Publication::NotCommitted(e))
            }
            CommitOutcome::Indeterminate(e) => {
                self.state = WriterState::Uncertain;
                Ok(Publication::Indeterminate(e))
            }
        }
    }

    /// Discard a prepared definite failure; uncertain writes cannot be forgotten.
    pub fn discard_prepared(&mut self) -> Result<(), DurableError> {
        if self.state != WriterState::Prepared {
            return Err(DurableError::WriterUnavailable);
        }
        self.pending = None;
        self.state = WriterState::Ready;
        Ok(())
    }

    pub fn recover_pending(&mut self) -> Result<Recovery, DurableError> {
        let pending = self.pending.as_ref().ok_or(DurableError::NoPrepared)?;
        self.state = WriterState::Unavailable;
        let actual = self.context.recover(|| {
            let manifest = read_open(
                self.context.store.as_ref(),
                &self.genesis,
                self.simulator.as_ref(),
            )?;
            let actual = head_of(&manifest);
            if actual == pending.batch.new_head {
                for (table, key, bytes) in &pending.batch.records {
                    if required(self.context.store.as_ref(), *table, key, bytes.len())? != *bytes {
                        return Err(corrupt("pending record mismatch"));
                    }
                }
                Ok(true)
            } else if Some(actual) == pending.batch.expected_head && manifest == self.manifest {
                for (table, key, bytes) in &pending.batch.records {
                    if self
                        .context
                        .store
                        .read_record_bounded(*table, key, bytes.len())?
                        .is_some()
                    {
                        return Err(corrupt("pending record exists under unchanged parent"));
                    }
                }
                if head_of(&pending.manifest) != pending.batch.new_head {
                    return Err(corrupt("prepared identity mismatch"));
                }
                Ok(false)
            } else {
                Err(corrupt("unexpected head during pending recovery"))
            }
        })?;
        if actual {
            self.state = WriterState::Uncertain;
            Ok(Recovery::ObservedCommittedDurabilityUnknown(pending.info))
        } else {
            self.pending.as_mut().expect("pending retained").generation =
                self.context.generation()?;
            self.state = WriterState::Prepared;
            Ok(Recovery::RetryPrepared)
        }
    }

    pub fn recover(&mut self) -> Result<Recovery, DurableError> {
        if self.pending.is_some() {
            return Err(DurableError::AlreadyPrepared);
        }
        self.state = WriterState::Unavailable;
        self.context.recover(|| {
            if read_open(
                self.context.store.as_ref(),
                &self.genesis,
                self.simulator.as_ref(),
            )? != self.manifest
            {
                return Err(corrupt("unexpected head during reader recovery"));
            }
            Ok(())
        })?;
        self.state = WriterState::Ready;
        Ok(Recovery::Ready)
    }

    pub fn stats(&self) -> DurableStats {
        DurableStats {
            cache: self.context.stats(),
            last_preparation: self.last_preparation.clone(),
            published_sequence: self.manifest.sequence,
            resident_manifests: 1 + usize::from(self.pending.is_some()),
            pending_payload_bytes: self.pending.as_ref().map_or(0, |p| p.batch.logical_bytes()),
            retention: self.retention_status(),
        }
    }
}

/// Initialization can fail or be uncertain just like any later publication.
#[derive(Debug)]
pub enum Initialization {
    Committed(DurableEngine),
    NotCommitted {
        error: StorageError,
        pending: InitializationPending,
    },
    Indeterminate {
        error: StorageError,
        pending: InitializationPending,
    },
}

/// Owns one bounded genesis preparation. No public engine/head is exposed.
pub struct InitializationPending {
    engine: DurableEngine,
    batch: CommitBatch,
    retry_allowed: bool,
}
impl fmt::Debug for InitializationPending {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InitializationPending")
            .field("retry_allowed", &self.retry_allowed)
            .finish_non_exhaustive()
    }
}
impl InitializationPending {
    pub fn commit(self) -> Result<Initialization, DurableError> {
        if !self.retry_allowed {
            return Err(DurableError::WriterUnavailable);
        }
        match self.engine.context.store.commit(&self.batch) {
            CommitOutcome::Committed => Ok(Initialization::Committed(self.engine)),
            CommitOutcome::NotCommitted(error) => Ok(Initialization::NotCommitted {
                error,
                pending: self,
            }),
            CommitOutcome::Indeterminate(error) => Ok(Initialization::Indeterminate {
                error,
                pending: Self {
                    retry_allowed: false,
                    ..self
                },
            }),
        }
    }

    /// True authorizes retry of the exact absent genesis. False means exact
    /// application was observed but durability is still unknown; no engine is
    /// returned. A fresh open requires an operational medium/recovery decision.
    pub fn recover(&mut self) -> Result<bool, DurableError> {
        self.retry_allowed = false;
        let absent = self
            .engine
            .context
            .recover(|| match self.engine.context.store.head()? {
                None => {
                    for (table, key, bytes) in &self.batch.records {
                        if self
                            .engine
                            .context
                            .store
                            .read_record_bounded(*table, key, bytes.len())?
                            .is_some()
                        {
                            return Err(corrupt("partial initialization metadata"));
                        }
                    }
                    Ok(true)
                }
                Some(head) if head == self.batch.new_head => {
                    if read_open(
                        self.engine.context.store.as_ref(),
                        &self.engine.genesis,
                        self.engine.simulator.as_ref(),
                    )? != self.engine.manifest
                    {
                        return Err(corrupt("initialization identity mismatch"));
                    }
                    for (table, key, bytes) in &self.batch.records {
                        if required(self.engine.context.store.as_ref(), *table, key, bytes.len())?
                            != *bytes
                        {
                            return Err(corrupt("initialization host packet mismatch"));
                        }
                    }
                    for (space, id, bytes) in &self.batch.objects {
                        if self
                            .engine
                            .context
                            .store
                            .load_object_bounded(*space, id, bytes.len())?
                            != *bytes
                        {
                            return Err(corrupt("initialization object mismatch"));
                        }
                    }
                    Ok(false)
                }
                Some(_) => Err(corrupt("unexpected initialization head")),
            })?;
        self.retry_allowed = absent;
        Ok(absent)
    }
}

#[cfg(test)]
mod ownership_tests {
    use super::*;
    use crate::{
        Budget, NamespaceId, NamespaceName, OwnerId, PriceSchedule, WriteOp, layout::RawKey,
    };
    use arkiv_db_memory::MemoryVersionedStore;
    use arkiv_db_trie::{MemoryOnly, Probe};

    #[test]
    fn prepared_and_committed_facade_release_dirty_values_after_callers_drop_clones() {
        let store = Arc::new(MemoryVersionedStore::new());
        let Initialization::Committed(mut engine) = DurableEngine::initialize(
            store,
            EngineGenesis::new([1; 32], PriceSchedule::FIXTURE),
            Options::default(),
        )
        .unwrap() else {
            panic!("memory initialization commits")
        };
        let owner = OwnerId::new([1; 20]);
        let namespace = NamespaceId::from_ordinal(1).unwrap();
        let mut branch = engine.begin(engine.head(), None).unwrap();
        branch
            .execute(
                owner,
                WriteOp::CreateNamespace {
                    name: NamespaceName::new("drop").unwrap(),
                },
                Budget::unlimited(),
            )
            .unwrap();
        branch
            .execute(
                owner,
                WriteOp::Put {
                    namespace,
                    key: b"key".to_vec(),
                    value: vec![5; 4096],
                },
                Budget::unlimited(),
            )
            .unwrap();
        let candidate = branch.seal();
        let leaf = candidate
            .state
            .catalog
            .get(&namespace, &MemoryOnly, &mut Probe::new())
            .unwrap()
            .unwrap();
        let value = leaf
            .records
            .get(
                &RawKey(Arc::from(&b"key"[..])),
                &MemoryOnly,
                &mut Probe::new(),
            )
            .unwrap()
            .unwrap();
        let weak = Arc::downgrade(&value.0);
        drop(value);
        drop(leaf);
        let caller_clone = candidate.clone();
        engine.prepare(candidate).unwrap();
        assert!(
            weak.upgrade().is_some(),
            "the explicitly retained caller clone owns dirty data"
        );
        drop(caller_clone);
        assert!(
            weak.upgrade().is_none(),
            "preparation retains bytes and stored roots, not dirty nodes"
        );
        engine.commit().unwrap();
        assert!(weak.upgrade().is_none());
        let handles: Vec<_> = (0..128).map(|_| engine.head_snapshot()).collect();
        for handle in &handles {
            assert_eq!(
                handle
                    .try_get(namespace, b"key", Budget::unlimited())
                    .unwrap()
                    .unwrap()
                    .value,
                Some(vec![5; 4096])
            );
            assert!(handle.staged_objects().is_empty());
        }
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn snapshot_pins_before_checking_the_floor_and_releases_on_refusal() {
        let store = Arc::new(MemoryVersionedStore::new());
        let options = Options {
            retention: Retention::Window(1),
            ..Options::default()
        };
        let Initialization::Committed(mut engine) = DurableEngine::initialize(
            store,
            EngineGenesis::new([2; 32], PriceSchedule::FIXTURE),
            options,
        )
        .unwrap() else {
            panic!("memory initialization commits")
        };
        for _ in 0..3 {
            let branch = engine.begin(engine.head(), None).unwrap();
            engine.prepare(branch.seal()).unwrap();
            engine.commit().unwrap();
        }
        assert_eq!(engine.state_floor(), SnapshotId::from_sequence(2));
        for (sequence, accepted) in [(2, true), (1, false)] {
            let mut seen = None;
            let result = engine.snapshot_hooked(SnapshotId::from_sequence(sequence), &mut || {
                seen = engine.retention.lowest_pinned();
            });
            // The pin was visible before the floor was read.
            assert_eq!(seen, Some(sequence));
            assert_eq!(result.is_ok(), accepted);
            drop(result);
            assert_eq!(engine.retention_status().lowest_pinned, Some(3));
        }
    }
}
