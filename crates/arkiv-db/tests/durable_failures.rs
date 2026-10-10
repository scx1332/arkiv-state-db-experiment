//! Adversarial durable-facade lifecycle checks over controlled atomic stores.
//! These are engine boundary tests, not evidence of physical media durability.

use std::{
    collections::BTreeMap,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use arkiv_db::{
    B256, Budget, CellName, Engine, EngineError, MapKind, NamespaceId, NamespaceName, OwnerId,
    PriceSchedule, Projection, Query, RecordKey, SealedCandidate, Snapshot, SnapshotId,
    StateHeader, StorageError, StorageErrorKind, Term, Value, WriteOp, decode_exact,
    durable::{
        CommitInfo, DurableEngine, DurableError, EngineGenesis, EngineManifest, Initialization,
        Options, Publication, Recovery, WriterState,
    },
};
use arkiv_db_memory::MemoryVersionedStore;
use arkiv_db_storage::{
    testing::{CommitFault, FaultyStore, ReadScope},
    versioned::{
        CommitBatch, CommitOutcome, DatabaseId, Head, Health, ObjectId, ObjectReader, SpaceId,
        StoreStats, Table, VersionedStore,
    },
};
use arkiv_db_trie::{EMPTY_ROOT_HASH, MemoryOnly, MerkleMap, Probe, Staged, TrieKey};

const OWNER: OwnerId = OwnerId::new([7; 20]);
type Faults = FaultyStore<MemoryVersionedStore>;

fn genesis() -> EngineGenesis {
    EngineGenesis::new([0x42; 32], PriceSchedule::FIXTURE)
}
fn id(sequence: u64) -> SnapshotId {
    SnapshotId::from_sequence(sequence)
}
fn namespace() -> NamespaceId {
    NamespaceId::from_ordinal(1).unwrap()
}

fn initialize(
    store: Arc<dyn VersionedStore>,
    genesis: EngineGenesis,
    options: Options,
) -> DurableEngine {
    match DurableEngine::initialize(store, genesis, options).unwrap() {
        Initialization::Committed(engine) => engine,
        other => panic!("initialization failed: {other:?}"),
    }
}

fn commit(engine: &mut DurableEngine, candidate: SealedCandidate) -> CommitInfo {
    let info = engine.prepare(candidate).unwrap();
    assert_eq!(engine.commit().unwrap(), Publication::Committed(info));
    info
}

fn put_candidate(engine: &DurableEngine, value: &[u8]) -> SealedCandidate {
    let mut branch = engine.begin(engine.head(), None).unwrap();
    if engine.head() == id(0) {
        branch
            .execute(
                OWNER,
                WriteOp::CreateNamespace {
                    name: NamespaceName::new("alpha").unwrap(),
                },
                Budget::unlimited(),
            )
            .unwrap();
    }
    branch
        .execute(
            OWNER,
            WriteOp::Put {
                namespace: namespace(),
                key: b"key".to_vec(),
                value: value.to_vec(),
            },
            Budget::unlimited(),
        )
        .unwrap();
    branch.try_seal().unwrap()
}

fn value(snapshot: &Snapshot) -> Vec<u8> {
    snapshot
        .try_get(namespace(), b"key", Budget::unlimited())
        .unwrap()
        .unwrap()
        .value
        .unwrap()
}

fn seeded() -> (Arc<Faults>, DurableEngine) {
    let store = Arc::new(FaultyStore::latching(MemoryVersionedStore::new()));
    let mut engine = initialize(store.clone(), genesis(), Options::default());
    let candidate = put_candidate(&engine, b"old");
    commit(&mut engine, candidate);
    (store, engine)
}

fn storage_kind(error: DurableError) -> StorageErrorKind {
    match error {
        DurableError::Storage(error) => error.kind,
        other => panic!("expected storage error, got {other:?}"),
    }
}

#[test]
fn failed_health_cannot_be_bypassed_by_a_warm_cache_and_recovery_clears_it() {
    let (store, mut engine) = seeded();
    let pinned = engine.head_snapshot();
    assert_eq!(value(&pinned), b"old");
    let reads = store.stats().object_reads();
    assert_eq!(value(&pinned), b"old");
    assert_eq!(store.stats().object_reads(), reads);
    assert!(engine.stats().cache.hits > 0);
    assert!(engine.stats().cache.entries > 0);
    let doomed_branch = engine.begin(engine.head(), None).unwrap();
    store.latch(StorageError::new(
        StorageErrorKind::Io,
        "latched after warm reads",
    ));
    assert_eq!(
        pinned
            .try_get(namespace(), b"key", Budget::unlimited())
            .unwrap_err()
            .kind,
        StorageErrorKind::Io
    );
    assert_eq!(
        doomed_branch
            .try_get(namespace(), b"key", Budget::unlimited())
            .unwrap_err()
            .kind,
        StorageErrorKind::Io
    );
    assert!(doomed_branch.is_poisoned());
    assert!(engine.begin(engine.head(), None).is_err());
    assert_eq!(engine.recover().unwrap(), Recovery::Ready);
    assert_eq!(engine.stats().cache.entries, 0);
    assert_eq!(engine.stats().cache.retained_bytes, 0);
    assert_eq!(engine.stats().cache.generation, 1);
    assert_eq!(value(&pinned), b"old");
    assert!(store.stats().object_reads() > reads);
    assert!(
        doomed_branch
            .try_get(namespace(), b"key", Budget::unlimited())
            .is_err()
    );
    assert_eq!(doomed_branch.try_seal().unwrap_err(), EngineError::Poisoned);
}

#[test]
fn failed_recovery_fences_cached_and_uncached_reads_until_validated_recovery() {
    let (store, mut engine) = seeded();
    let pinned = engine.head_snapshot();
    value(&pinned);
    store.fail_next_recoveries(1, StorageErrorKind::Io);
    assert_eq!(
        storage_kind(engine.recover().unwrap_err()),
        StorageErrorKind::Io
    );
    assert_eq!(engine.writer_state(), WriterState::Unavailable);
    assert_eq!(engine.stats().cache.entries, 0);
    assert!(
        pinned
            .try_get(namespace(), b"key", Budget::unlimited())
            .is_err()
    );
    assert!(engine.snapshot(id(1)).is_err());
    assert!(engine.begin(engine.head(), None).is_err());
    assert_eq!(engine.recover().unwrap(), Recovery::Ready);
    assert_eq!(engine.stats().cache.generation, 2);
    assert_eq!(value(&pinned), b"old");
}

#[test]
fn unchanged_parent_reauthorizes_only_the_owned_prepared_retry() {
    for fault in [
        CommitFault::NotCommitted(StorageErrorKind::Full),
        CommitFault::IndeterminateNotApplied(StorageErrorKind::Io),
    ] {
        let (store, mut engine) = seeded();
        let candidate = put_candidate(&engine, b"new");
        let external_clone = candidate.clone();
        let expected = engine.prepare(candidate).unwrap();
        let published = engine.head_info();
        store.push_commit_fault(fault);
        let outcome = engine.commit().unwrap();
        assert!(matches!(
            outcome,
            Publication::NotCommitted(_) | Publication::Indeterminate(_)
        ));
        assert_eq!(engine.head_info(), published);
        assert!(engine.commit().is_err());
        assert!(engine.begin(engine.head(), None).is_err());
        assert_eq!(engine.recover_pending().unwrap(), Recovery::RetryPrepared);
        assert_eq!(engine.head_info(), published);
        assert_eq!(engine.commit().unwrap(), Publication::Committed(expected));
        assert_eq!(value(&engine.head_snapshot()), b"new");
        assert_eq!(
            engine.prepare(external_clone).unwrap_err().error,
            DurableError::StaleGeneration
        );
        assert_eq!(store.stats().committed, 3); // Genesis, seed, exactly one retry.
        assert_eq!(engine.stats().pending_payload_bytes, 0);
    }
}

#[test]
fn healthy_definite_noncommit_retries_the_exact_preparation_without_reexecution() {
    let (store, mut engine) = seeded();
    let candidate = put_candidate(&engine, b"new");
    let info = engine.prepare(candidate).unwrap();
    let before = engine.head_info();
    store.push_commit_fault(CommitFault::NotCommitted(StorageErrorKind::Conflict));
    assert!(
        matches!(engine.commit().unwrap(), Publication::NotCommitted(error) if error.kind == StorageErrorKind::Conflict)
    );
    assert_eq!(store.health(), Health::Available);
    assert_eq!(engine.writer_state(), WriterState::Prepared);
    assert_eq!(engine.head_info(), before);
    assert_eq!(engine.commit().unwrap(), Publication::Committed(info));
    assert_eq!(engine.head(), id(2));
}

#[test]
fn applied_uncertainty_and_lost_ack_never_become_durable_success() {
    for fault in [
        CommitFault::IndeterminateApplied(StorageErrorKind::Io),
        CommitFault::AckLost,
    ] {
        let (store, mut engine) = seeded();
        let candidate = put_candidate(&engine, b"new");
        let info = engine.prepare(candidate).unwrap();
        let before = engine.head_info();
        store.push_commit_fault(fault);
        assert!(matches!(
            engine.commit().unwrap(),
            Publication::Indeterminate(_)
        ));
        assert_eq!(engine.head_info(), before);
        assert_eq!(
            engine.recover_pending().unwrap(),
            Recovery::ObservedCommittedDurabilityUnknown(info)
        );
        assert_eq!(engine.writer_state(), WriterState::Uncertain);
        assert_eq!(engine.head_info(), before);
        assert_eq!(value(&engine.head_snapshot()), b"old");
        assert!(engine.commit().is_err());
        assert!(engine.discard_prepared().is_err());
        assert!(engine.begin(engine.head(), None).is_err());
        assert!(matches!(
            engine.snapshot(info.snapshot),
            Err(DurableError::Engine(EngineError::UnknownSnapshot(_)))
        ));
        // This volatile adapter models visibility only: no medium assurance is
        // inferred from the separate open below.
        let reopened = DurableEngine::open(store.clone(), genesis(), Options::default()).unwrap();
        assert_eq!(reopened.head_info(), info);
        assert_eq!(value(&reopened.head_snapshot()), b"new");
    }
}

#[test]
fn reader_recovery_rejects_old_candidates_but_keeps_pinned_snapshots_readable() {
    let (store, mut engine) = seeded();
    let candidate = put_candidate(&engine, b"old generation");
    let pinned = engine.head_snapshot();
    store.fail_reads(ReadScope::Records, StorageErrorKind::Io, Some(1));
    assert!(engine.snapshot(engine.head()).is_err());
    assert_eq!(engine.recover().unwrap(), Recovery::Ready);
    assert_eq!(
        engine.prepare(candidate).unwrap_err().error,
        DurableError::StaleGeneration
    );
    assert_eq!(value(&pinned), b"old");
    let candidate = put_candidate(&engine, b"new generation");
    commit(&mut engine, candidate);
    assert_eq!(value(&pinned), b"old");
}

#[test]
fn foreign_stale_and_reopened_candidates_are_rejected_without_a_commit() {
    let (store, mut engine) = seeded();
    let mut memory = Engine::new(PriceSchedule::FIXTURE);
    let foreign = memory.begin(memory.head(), None).unwrap().seal();
    assert_eq!(
        engine.prepare(foreign).unwrap_err().error,
        DurableError::Engine(EngineError::ForeignCandidate)
    );
    let stale = put_candidate(&engine, b"stale");
    let current = put_candidate(&engine, b"current");
    commit(&mut engine, current);
    assert!(matches!(
        engine.prepare(stale).unwrap_err().error,
        DurableError::Engine(EngineError::StaleHead { .. })
    ));
    let prereopen = put_candidate(&engine, b"prereopen");
    drop(engine);
    let mut reopened = DurableEngine::open(store.clone(), genesis(), Options::default()).unwrap();
    let commits = store.stats().committed;
    assert_eq!(
        reopened.prepare(prereopen).unwrap_err().error,
        DurableError::Engine(EngineError::ForeignCandidate)
    );
    assert_eq!(store.stats().committed, commits);
    // Keep the memory facade's ordinary publication path covered unchanged.
    let candidate = memory.begin(memory.head(), None).unwrap().seal();
    memory.publish(&candidate).unwrap();
}

#[test]
fn initialization_failure_and_uncertainty_expose_only_recovery_tokens() {
    for fault in [
        CommitFault::NotCommitted(StorageErrorKind::Io),
        CommitFault::IndeterminateNotApplied(StorageErrorKind::Io),
    ] {
        let store = Arc::new(FaultyStore::latching(MemoryVersionedStore::new()));
        store.push_commit_fault(fault);
        let mut pending = match DurableEngine::initialize(
            store.clone(),
            genesis(),
            Options::default(),
        )
        .unwrap()
        {
            Initialization::NotCommitted { pending, .. }
            | Initialization::Indeterminate { pending, .. } => pending,
            other => panic!("unexpected success: {other:?}"),
        };
        assert!(DurableEngine::open(store.clone(), genesis(), Options::default()).is_err());
        assert!(pending.recover().unwrap());
        let engine = match pending.commit().unwrap() {
            Initialization::Committed(engine) => engine,
            other => panic!("retry failed: {other:?}"),
        };
        assert_eq!(engine.head(), id(0));
        assert_eq!(store.stats().committed, 1);
    }
    for fault in [
        CommitFault::IndeterminateApplied(StorageErrorKind::Io),
        CommitFault::AckLost,
    ] {
        let store = Arc::new(FaultyStore::latching(MemoryVersionedStore::new()));
        store.push_commit_fault(fault);
        let mut pending = match DurableEngine::initialize(
            store.clone(),
            genesis(),
            Options::default(),
        )
        .unwrap()
        {
            Initialization::Indeterminate { pending, .. } => pending,
            other => panic!("unexpected outcome: {other:?}"),
        };
        assert!(!pending.recover().unwrap());
        assert!(matches!(
            pending.commit(),
            Err(DurableError::WriterUnavailable)
        ));
        // The token was consumed/dropped above. This separate open checks only
        // the volatile adapter's retained complete genesis, not disk durability.
        let reopened = DurableEngine::open(store, genesis(), Options::default()).unwrap();
        assert_eq!(reopened.head(), id(0));
    }
}

type RecordOverrides = BTreeMap<(Table, Vec<u8>), Option<Vec<u8>>>;
type ObjectOverrides = BTreeMap<(SpaceId, ObjectId), Option<Vec<u8>>>;

/// Mutates reads without weakening the adapter's normal atomic write path.
/// Its bounded methods check lengths before copying, and unbounded calls panic
/// so a facade regression cannot accidentally exercise a post-copy size check.
#[derive(Default)]
struct Corruptible {
    inner: MemoryVersionedStore,
    records: Mutex<RecordOverrides>,
    objects: Mutex<ObjectOverrides>,
    head: Mutex<Option<Option<Head>>>,
}

fn copy_bounded(bytes: Option<&Vec<u8>>, max: usize) -> Result<Option<Vec<u8>>, StorageError> {
    if let Some(bytes) = bytes {
        if bytes.len() > max {
            return Err(StorageError::new(
                StorageErrorKind::Corrupt,
                "oversize before copy",
            ));
        }
        Ok(Some(bytes.clone()))
    } else {
        Ok(None)
    }
}

impl ObjectReader for Corruptible {
    fn read_object(&self, _: SpaceId, _: &ObjectId) -> Result<Option<Vec<u8>>, StorageError> {
        panic!("unbounded object read")
    }
    fn read_object_bounded(
        &self,
        space: SpaceId,
        id: &ObjectId,
        max: usize,
    ) -> Result<Option<Vec<u8>>, StorageError> {
        if let Some(bytes) = self.objects.lock().unwrap().get(&(space, *id)) {
            return copy_bounded(bytes.as_ref(), max);
        }
        self.inner.read_object_bounded(space, id, max)
    }
}

impl VersionedStore for Corruptible {
    fn database_id(&self) -> DatabaseId {
        self.inner.database_id()
    }
    fn head(&self) -> Result<Option<Head>, StorageError> {
        match *self.head.lock().unwrap() {
            Some(head) => Ok(head),
            None => self.inner.head(),
        }
    }
    fn read_record(&self, _: Table, _: &[u8]) -> Result<Option<Vec<u8>>, StorageError> {
        panic!("unbounded record read")
    }
    fn read_record_bounded(
        &self,
        table: Table,
        key: &[u8],
        max: usize,
    ) -> Result<Option<Vec<u8>>, StorageError> {
        if let Some(bytes) = self.records.lock().unwrap().get(&(table, key.to_vec())) {
            return copy_bounded(bytes.as_ref(), max);
        }
        self.inner.read_record_bounded(table, key, max)
    }
    fn commit(&self, batch: &CommitBatch) -> CommitOutcome {
        self.inner.commit(batch)
    }
    fn health(&self) -> Health {
        self.inner.health()
    }
    fn recover(&self) -> Result<Option<Head>, StorageError> {
        self.head()
    }
    fn stats(&self) -> StoreStats {
        self.inner.stats()
    }
}

fn corruptible() -> (Arc<Corruptible>, DurableEngine) {
    let store = Arc::new(Corruptible::default());
    let mut engine = initialize(store.clone(), genesis(), Options::default());
    for text in [b"first".as_slice(), b"second"] {
        let candidate = put_candidate(&engine, text);
        commit(&mut engine, candidate);
    }
    (store, engine)
}

#[test]
fn missing_old_manifest_is_corruption_while_future_snapshot_is_unknown() {
    let (store, engine) = corruptible();
    store
        .records
        .lock()
        .unwrap()
        .insert((Table::Manifests, 1_u64.to_be_bytes().to_vec()), None);
    assert_eq!(
        storage_kind(engine.snapshot(id(1)).unwrap_err()),
        StorageErrorKind::Corrupt
    );
    assert!(matches!(
        engine.snapshot(id(3)),
        Err(DurableError::Engine(EngineError::UnknownSnapshot(_)))
    ));
    assert_eq!(value(&engine.head_snapshot()), b"second");
}

#[test]
fn metadata_and_manifest_damage_is_rejected_on_open() {
    let (store, engine) = corruptible();
    let head = engine.head_info();
    let manifest_key = head.snapshot.sequence().to_be_bytes().to_vec();
    let original_manifest = store
        .inner
        .read_record(Table::Manifests, &manifest_key)
        .unwrap()
        .unwrap();
    let original_genesis = store
        .inner
        .read_record(Table::Meta, b"engine/genesis")
        .unwrap()
        .unwrap();
    let mut bad_digest = original_manifest.clone();
    *bad_digest.last_mut().unwrap() ^= 1;
    let mut wrong_genesis = original_genesis.clone();
    *wrong_genesis.last_mut().unwrap() ^= 1;
    let cases = [
        (Table::Meta, b"engine/profile".to_vec(), None),
        (
            Table::Meta,
            b"engine/profile".to_vec(),
            Some(b"ARKENG01".to_vec()),
        ),
        (Table::Meta, b"engine/profile".to_vec(), Some(vec![0; 13])),
        (Table::Meta, b"engine/genesis".to_vec(), Some(wrong_genesis)),
        (
            Table::Meta,
            b"engine/genesis".to_vec(),
            Some(original_genesis[..original_genesis.len() - 1].to_vec()),
        ),
        (Table::Manifests, manifest_key.clone(), None),
        (Table::Manifests, manifest_key.clone(), Some(bad_digest)),
        (
            Table::Manifests,
            manifest_key.clone(),
            Some(original_manifest[..original_manifest.len() - 1].to_vec()),
        ),
        (
            Table::Manifests,
            manifest_key,
            Some([original_manifest.as_slice(), &[0]].concat()),
        ),
        (Table::BlockIndex, head.commit_id.as_slice().to_vec(), None),
        (
            Table::BlockIndex,
            head.commit_id.as_slice().to_vec(),
            Some(99_u64.to_be_bytes().to_vec()),
        ),
    ];
    for (table, key, bytes) in cases {
        store
            .records
            .lock()
            .unwrap()
            .insert((table, key.clone()), bytes);
        assert_eq!(
            storage_kind(
                DurableEngine::open(store.clone(), genesis(), Options::default()).unwrap_err()
            ),
            StorageErrorKind::Corrupt,
            "{table:?}/{key:?}"
        );
        store.records.lock().unwrap().clear();
    }
    *store.head.lock().unwrap() = Some(Some(Head {
        height: 2,
        hash: [0x99; 32],
    }));
    assert_eq!(
        storage_kind(
            DurableEngine::open(store.clone(), genesis(), Options::default()).unwrap_err()
        ),
        StorageErrorKind::Corrupt
    );
}

#[test]
fn expected_identity_and_unknown_complete_versions_are_incompatible() {
    let (store, _) = corruptible();
    let mut expected = genesis();
    expected.deployment[0] ^= 1;
    assert_eq!(
        storage_kind(DurableEngine::open(store.clone(), expected, Options::default()).unwrap_err()),
        StorageErrorKind::Incompatible
    );
    let mut expected = genesis();
    expected.limits.max_cells += 1;
    assert_eq!(
        storage_kind(DurableEngine::open(store.clone(), expected, Options::default()).unwrap_err()),
        StorageErrorKind::Incompatible
    );
    let mut bytes = b"ARKENG01".to_vec();
    bytes.extend_from_slice(&2_u32.to_be_bytes());
    store
        .records
        .lock()
        .unwrap()
        .insert((Table::Meta, b"engine/profile".to_vec()), Some(bytes));
    assert_eq!(
        storage_kind(
            DurableEngine::open(store.clone(), genesis(), Options::default()).unwrap_err()
        ),
        StorageErrorKind::Incompatible
    );
    assert_eq!(
        storage_kind(
            DurableEngine::initialize(store.clone(), genesis(), Options::default()).unwrap_err()
        ),
        StorageErrorKind::Incompatible
    );
}

#[test]
fn missing_corrupt_and_oversized_objects_are_never_absence_answers() {
    for corruption in [None, Some(vec![0; 20]), Some(vec![0; 1024 * 1024 + 1])] {
        let (store, engine) = corruptible();
        let root = engine.head_snapshot().state_header().catalog_root.0;
        let cached_before = engine.stats().cache.entries;
        store
            .objects
            .lock()
            .unwrap()
            .insert((SpaceId::Catalog, root), corruption.clone());
        let error = engine
            .head_snapshot()
            .try_get(namespace(), b"key", Budget::unlimited())
            .unwrap_err();
        assert_eq!(
            error.kind,
            if corruption.is_none() {
                StorageErrorKind::Missing
            } else {
                StorageErrorKind::Corrupt
            }
        );
        assert_eq!(engine.stats().cache.entries, cached_before);
    }
}

#[test]
fn unexpected_head_during_recovery_is_fenced_and_cannot_rebase_pending_work() {
    let (store, mut engine) = seeded();
    let candidate = put_candidate(&engine, b"pending");
    engine.prepare(candidate).unwrap();
    store.push_commit_fault(CommitFault::NotCommitted(StorageErrorKind::Io));
    engine.commit().unwrap();
    let parent = store.inner().head().unwrap().unwrap();
    // Deliberately violate exclusive-writer ownership to test the CAS fence.
    assert_eq!(
        store.inner().commit(&CommitBatch::new(
            Some(parent),
            Head {
                height: parent.height + 7,
                hash: [0xcc; 32]
            }
        )),
        CommitOutcome::Committed
    );
    assert!(engine.recover_pending().is_err());
    assert_eq!(engine.writer_state(), WriterState::Unavailable);
    assert_eq!(engine.head(), id(1));
    assert!(engine.commit().is_err());
    assert!(
        engine
            .head_snapshot()
            .try_get(namespace(), b"key", Budget::unlimited())
            .is_err()
    );
}

#[test]
fn preparation_caps_fail_before_commit_and_return_the_candidate() {
    let store = Arc::new(MemoryVersionedStore::new());
    let engine = initialize(store.clone(), genesis(), Options::default());
    drop(engine);
    for options in [
        Options {
            prepare_objects: 0,
            ..Options::default()
        },
        Options {
            prepare_visits: 0,
            ..Options::default()
        },
        Options {
            prepare_bytes: 0,
            ..Options::default()
        },
    ] {
        let mut engine = DurableEngine::open(store.clone(), genesis(), options).unwrap();
        let candidate = put_candidate(&engine, b"new");
        let receipts = candidate.receipts().to_vec();
        let before = store.stats();
        let error = engine.prepare(candidate).unwrap_err();
        assert!(matches!(error.error, DurableError::ResourceLimit(_)));
        assert_eq!(error.candidate.receipts(), receipts);
        assert_eq!(engine.head(), id(0));
        assert_eq!(engine.writer_state(), WriterState::Ready);
        assert_eq!(engine.stats().pending_payload_bytes, 0);
        assert_eq!(store.stats().committed, before.committed);
        assert_eq!(store.stats().not_committed, before.not_committed);
        assert_eq!(store.stats().object_reads(), before.object_reads());
    }
}

#[test]
fn oversized_raw_value_is_rejected_without_storage_work_or_state_publication() {
    let store = Arc::new(MemoryVersionedStore::new());
    let mut profile = genesis();
    profile.max_object_bytes = 1024;
    let mut engine = initialize(store.clone(), profile, Options::default());
    let candidate = put_candidate(&engine, &vec![0x55; 1024]);
    let receipts = candidate.receipts().to_vec();
    let before = store.stats();
    let failure = engine.prepare(candidate).unwrap_err();
    assert!(matches!(failure.error, DurableError::ResourceLimit(_)));
    assert_eq!(failure.candidate.receipts(), receipts);
    assert_eq!(store.stats().committed, before.committed);
    assert_eq!(store.stats().object_reads(), before.object_reads());
    assert_eq!(engine.head(), id(0));
    assert_eq!(engine.stats().last_preparation.preflight_visits, 0);
}

#[test]
fn self_consistent_manifest_digests_do_not_bypass_semantic_validation() {
    let (store, engine) = corruptible();
    let key = engine.head().sequence().to_be_bytes().to_vec();
    let bytes = store
        .inner
        .read_record(Table::Manifests, &key)
        .unwrap()
        .unwrap();
    let original: EngineManifest = decode_exact(&bytes[..bytes.len() - 32]).unwrap();
    let mut cases = Vec::new();
    let mut modified = original.clone();
    modified.state_root = B256::ZERO;
    cases.push(modified);
    let mut modified = original.clone();
    modified.genesis = B256::ZERO;
    cases.push(modified);
    let mut modified = original.clone();
    modified.parent = None;
    cases.push(modified);
    let mut modified = original.clone();
    modified.parent.as_mut().unwrap().0 = 0;
    cases.push(modified);
    let mut modified = original.clone();
    modified.parent.as_mut().unwrap().1 = engine.snapshot(id(0)).unwrap().state_root();
    cases.push(modified);
    let mut modified = original.clone();
    modified.state.next_namespace = 0;
    modified.state_root = modified.state.root();
    cases.push(modified);
    let mut modified = original;
    modified.state.catalog_len = 0;
    modified.state_root = modified.state.root();
    cases.push(modified);
    for manifest in cases {
        store
            .records
            .lock()
            .unwrap()
            .insert((Table::Manifests, key.clone()), Some(manifest.envelope()));
        assert_eq!(
            storage_kind(
                DurableEngine::open(store.clone(), genesis(), Options::default()).unwrap_err()
            ),
            StorageErrorKind::Corrupt
        );
    }
}

#[test]
fn impossible_persisted_genesis_is_corrupt_but_invalid_requested_config_is_incompatible() {
    let (store, _) = corruptible();
    let mut invalid = genesis();
    invalid.max_object_bytes = 0;
    store.records.lock().unwrap().insert(
        (Table::Meta, b"engine/genesis".to_vec()),
        Some(invalid.envelope()),
    );
    assert_eq!(
        storage_kind(
            DurableEngine::open(store.clone(), genesis(), Options::default()).unwrap_err()
        ),
        StorageErrorKind::Corrupt
    );
    assert_eq!(
        storage_kind(DurableEngine::open(store, invalid, Options::default()).unwrap_err()),
        StorageErrorKind::Incompatible
    );
    for max in [0, u64::MAX] {
        let store = Arc::new(MemoryVersionedStore::new());
        let mut invalid = genesis();
        invalid.max_object_bytes = max;
        assert_eq!(
            storage_kind(
                DurableEngine::initialize(store.clone(), invalid, Options::default()).unwrap_err()
            ),
            StorageErrorKind::Incompatible
        );
        assert_eq!(store.stats().committed, 0);
        assert_eq!(store.head().unwrap(), None);
    }
}

#[test]
fn partial_initialization_records_are_not_treated_as_a_fresh_store() {
    let store = Arc::new(Corruptible::default());
    store.records.lock().unwrap().insert(
        (Table::Meta, b"engine/profile".to_vec()),
        Some([b"ARKENG01".as_slice(), &1_u32.to_be_bytes()].concat()),
    );
    assert_eq!(
        storage_kind(
            DurableEngine::initialize(store.clone(), genesis(), Options::default()).unwrap_err()
        ),
        StorageErrorKind::Incompatible
    );
    assert_eq!(
        storage_kind(
            DurableEngine::open(store.clone(), genesis(), Options::default()).unwrap_err()
        ),
        StorageErrorKind::Corrupt
    );
    assert_eq!(store.stats().committed, 0);
}

#[test]
fn exact_preparation_boundaries_accept_and_one_less_rejects() {
    let store = Arc::new(MemoryVersionedStore::new());
    let mut engine = initialize(store.clone(), genesis(), Options::default());
    let candidate = put_candidate(&engine, &vec![0x55; 1024]);
    let expected = engine.prepare(candidate).unwrap();
    let stats = engine.stats().last_preparation;
    engine.discard_prepared().unwrap();
    drop(engine);
    for deficit in [0, 1] {
        for options in [
            Options {
                prepare_bytes: stats.payload_bytes - deficit,
                ..Options::default()
            },
            Options {
                prepare_objects: stats.unique_objects - deficit,
                ..Options::default()
            },
            Options {
                prepare_visits: stats.preflight_visits - deficit as u64,
                ..Options::default()
            },
        ] {
            let mut engine = DurableEngine::open(store.clone(), genesis(), options).unwrap();
            let candidate = put_candidate(&engine, &vec![0x55; 1024]);
            let prepared = engine.prepare(candidate);
            if deficit == 0 {
                assert_eq!(prepared.unwrap(), expected);
            } else {
                assert!(matches!(
                    prepared.unwrap_err().error,
                    DurableError::ResourceLimit(_)
                ));
            }
            assert_eq!(engine.head(), id(0));
            assert_eq!(store.stats().committed, 1);
        }
    }
}

#[test]
fn encoded_object_ceiling_includes_trie_framing_and_persists_on_reopen() {
    let baseline = Arc::new(MemoryVersionedStore::new());
    let engine = initialize(baseline, genesis(), Options::default());
    let candidate = put_candidate(&engine, &vec![0x55; 1024]);
    let largest = candidate
        .staged_objects()
        .iter()
        .map(|object| object.bytes.len())
        .max()
        .unwrap();
    assert!(largest > 1024);
    for deficit in [0, 1] {
        let store = Arc::new(MemoryVersionedStore::new());
        let mut profile = genesis();
        profile.max_object_bytes = (largest - deficit) as u64;
        let mut engine = initialize(store.clone(), profile.clone(), Options::default());
        let candidate = put_candidate(&engine, &vec![0x55; 1024]);
        if deficit == 0 {
            commit(&mut engine, candidate);
            drop(engine);
            let reopened = DurableEngine::open(store, profile, Options::default()).unwrap();
            assert_eq!(value(&reopened.head_snapshot()), vec![0x55; 1024]);
        } else {
            assert!(matches!(
                engine.prepare(candidate).unwrap_err().error,
                DurableError::ResourceLimit(_)
            ));
            assert_eq!(store.stats().committed, 1);
        }
    }
}

#[test]
fn cache_payload_entries_and_container_metadata_stay_within_configured_bounds() {
    // Key, erased node, charge and reference bit of one queue slot.
    let entry_bytes = std::mem::size_of::<(
        (SpaceId, MapKind, ObjectId),
        Arc<dyn std::any::Any + Send + Sync>,
        usize,
        bool,
    )>();
    for (bytes, entries) in [(0, 0), (512, 1), (512, 3), (2048, 17)] {
        let store = Arc::new(MemoryVersionedStore::new());
        let options = Options {
            cache_bytes: bytes,
            cache_entries: entries,
            ..Options::default()
        };
        let mut engine = initialize(store, genesis(), options);
        let mut branch = engine.begin(engine.head(), None).unwrap();
        branch
            .execute(
                OWNER,
                WriteOp::CreateNamespace {
                    name: NamespaceName::new("alpha").unwrap(),
                },
                Budget::unlimited(),
            )
            .unwrap();
        for i in 0_u8..50 {
            branch
                .execute(
                    OWNER,
                    WriteOp::Put {
                        namespace: namespace(),
                        key: vec![i],
                        value: vec![i; 64],
                    },
                    Budget::unlimited(),
                )
                .unwrap();
        }
        commit(&mut engine, branch.seal());
        let snapshot = engine.head_snapshot();
        for i in (0_u8..50).chain((0_u8..50).rev()) {
            assert_eq!(
                snapshot
                    .try_get(namespace(), &[i], Budget::unlimited())
                    .unwrap()
                    .unwrap()
                    .value,
                Some(vec![i; 64])
            );
            let stats = engine.stats().cache;
            assert!(stats.entries <= entries);
            assert!(stats.peak_entries <= entries);
            assert!(stats.retained_bytes <= bytes);
            assert!(stats.peak_retained_bytes <= bytes);
            assert!(stats.container_bytes <= entries * entry_bytes);
        }
    }
}

#[test]
fn identical_object_hashes_in_other_namespaces_cannot_fill_a_missing_object() {
    let store = Arc::new(Corruptible::default());
    let mut engine = initialize(store.clone(), genesis(), Options::default());
    let mut branch = engine.begin(engine.head(), None).unwrap();
    for (number, name) in [(1, "alpha"), (2, "beta")] {
        branch
            .execute(
                OWNER,
                WriteOp::CreateNamespace {
                    name: NamespaceName::new(name).unwrap(),
                },
                Budget::unlimited(),
            )
            .unwrap();
        branch
            .execute(
                OWNER,
                WriteOp::Put {
                    namespace: NamespaceId::from_ordinal(number).unwrap(),
                    key: b"key".to_vec(),
                    value: b"same".to_vec(),
                },
                Budget::unlimited(),
            )
            .unwrap();
    }
    commit(&mut engine, branch.seal());
    let snapshot = engine.head_snapshot();
    let second = NamespaceId::from_ordinal(2).unwrap();
    let first_root = snapshot
        .try_namespace_roots(namespace())
        .unwrap()
        .unwrap()
        .records
        .0;
    let second_root = snapshot
        .try_namespace_roots(second)
        .unwrap()
        .unwrap()
        .records
        .0;
    assert_eq!(first_root, second_root);
    assert_eq!(value(&snapshot), b"same");
    store
        .objects
        .lock()
        .unwrap()
        .insert((SpaceId::Namespace(2), second_root.0), None);
    assert_eq!(
        snapshot
            .try_get(second, b"key", Budget::unlimited())
            .unwrap_err()
            .kind,
        StorageErrorKind::Missing
    );
}

fn raw_map<K: TrieKey>(key: &K, value: Vec<u8>) -> MerkleMap<K, Vec<u8>> {
    MerkleMap::new()
        .insert(key, value, &MemoryOnly, &mut Probe::new())
        .unwrap()
        .0
}

fn objects<K: TrieKey>(
    map: &MerkleMap<K, Vec<u8>>,
    space: SpaceId,
) -> Vec<(SpaceId, ObjectId, Vec<u8>)> {
    let mut staged = Staged::new();
    map.stage(&mut staged);
    staged
        .into_objects()
        .into_iter()
        .map(|(id, bytes)| (space, id.0, bytes))
        .collect()
}

fn crafted_snapshot(header: StateHeader, objects: Vec<(SpaceId, ObjectId, Vec<u8>)>) -> Snapshot {
    crafted_engine(header, objects, Options::default()).head_snapshot()
}

fn crafted_engine(
    header: StateHeader,
    objects: Vec<(SpaceId, ObjectId, Vec<u8>)>,
    options: Options,
) -> DurableEngine {
    let store = Arc::new(MemoryVersionedStore::new());
    let engine = initialize(store.clone(), genesis(), Options::default());
    let parent = engine.head_info();
    drop(engine);
    let manifest = EngineManifest {
        genesis: genesis().id(),
        sequence: 1,
        parent: Some((0, parent.commit_id)),
        state_root: header.root(),
        state: header,
    };
    let mut batch = CommitBatch::new(
        store.head().unwrap(),
        Head {
            height: 1,
            hash: manifest.id().0,
        },
    );
    batch.objects = objects;
    batch.put_record(Table::Manifests, 1_u64.to_be_bytes(), manifest.envelope());
    batch.put_record(Table::BlockIndex, manifest.id().0, 1_u64.to_be_bytes());
    assert_eq!(store.commit(&batch), CommitOutcome::Committed);
    let before = store.stats().object_reads();
    let engine = DurableEngine::open(store.clone(), genesis(), options).unwrap();
    assert_eq!(store.stats().object_reads(), before);
    engine
}

#[test]
fn zero_cache_bytes_never_retains_even_hash_valid_empty_objects() {
    let empty_hash = alloy_primitives::keccak256(b"");
    let mut header = genesis().initial_header();
    header.host_root = empty_hash;
    header.host_len = 1;
    let engine = crafted_engine(
        header,
        vec![(SpaceId::Host, empty_hash.0, Vec::new())],
        Options {
            cache_bytes: 0,
            ..Options::default()
        },
    );
    // The content hash passes, but an empty byte string is no trie node.
    // Disabling retention must cover this path before typed decoding fails.
    assert_eq!(
        engine
            .head_snapshot()
            .try_host_get(&[0; 32])
            .unwrap_err()
            .kind,
        StorageErrorKind::Corrupt
    );
    let stats = engine.stats().cache;
    assert_eq!(stats.entries, 0);
    assert_eq!(stats.container_bytes, 0);
    assert_eq!(stats.retained_bytes, 0);
}

fn catalog(handles: [(B256, u64); 4]) -> MerkleMap<u64, Vec<u8>> {
    let mut leaf = 1_u64.to_be_bytes().to_vec();
    leaf.extend(5_u32.to_be_bytes());
    leaf.extend(b"alpha");
    leaf.extend(OWNER.as_bytes());
    leaf.extend(0_u64.to_be_bytes());
    leaf.extend(2_u64.to_be_bytes());
    for (root, len) in handles {
        leaf.extend(root);
        leaf.extend(len.to_be_bytes());
    }
    raw_map(&1_u64, leaf)
}

#[test]
fn content_addressed_but_malformed_host_row_and_posting_values_are_corruption() {
    let host = raw_map(&[0_u8; 32], vec![0, 0, 0, 1]); // Blob length 1, no byte.
    let mut header = genesis().initial_header();
    header.host_root = host.root_hash();
    header.host_len = 1;
    let snapshot = crafted_snapshot(header, objects(&host, SpaceId::Host));
    assert_eq!(
        snapshot.try_host_get(&[0; 32]).unwrap_err().kind,
        StorageErrorKind::Corrupt
    );

    let key = RecordKey::new(b"r").unwrap();
    let keys = raw_map(&key.encoded(), 1_u64.to_be_bytes().to_vec());
    let rows = raw_map(&1_u64, vec![1, b'r', 0, 0, 0, 1]); // One cell with no encoding.
    let directory = catalog([
        (EMPTY_ROOT_HASH, 0),
        (keys.root_hash(), 1),
        (rows.root_hash(), 1),
        (EMPTY_ROOT_HASH, 0),
    ]);
    let mut header = genesis().initial_header();
    header.catalog_root = directory.root_hash();
    header.catalog_len = 1;
    header.next_namespace = 2;
    let mut all = objects(&directory, SpaceId::Catalog);
    all.extend(objects(&keys, SpaceId::Namespace(1)));
    all.extend(objects(&rows, SpaceId::Namespace(1)));
    let snapshot = crafted_snapshot(header, all);
    assert_eq!(
        snapshot
            .try_get_record(namespace(), &key, &Projection::ALL, Budget::unlimited())
            .unwrap_err()
            .kind,
        StorageErrorKind::Corrupt
    );

    let postings = raw_map(&1_u64, vec![2]); // A posting member must be exactly 01.
    let mut term_key = [0_u8; 88];
    term_key[..4].copy_from_slice(b"kind");
    term_key[20] = 4;
    term_key[21] = Value::U64(1).tag().id();
    term_key[22] = 1;
    term_key[23..31].copy_from_slice(&1_u64.to_be_bytes());
    term_key[87] = 8;
    let mut handle = postings.root_hash().to_vec();
    handle.extend(1_u64.to_be_bytes());
    let terms = raw_map(&term_key, handle);
    let directory = catalog([
        (EMPTY_ROOT_HASH, 0),
        (EMPTY_ROOT_HASH, 0),
        (EMPTY_ROOT_HASH, 0),
        (terms.root_hash(), 1),
    ]);
    let mut header = genesis().initial_header();
    header.catalog_root = directory.root_hash();
    header.catalog_len = 1;
    header.next_namespace = 2;
    let mut all = objects(&directory, SpaceId::Catalog);
    all.extend(objects(&terms, SpaceId::Namespace(1)));
    all.extend(objects(&postings, SpaceId::Namespace(1)));
    let snapshot = crafted_snapshot(header, all);
    let query = Query::all(vec![arkiv_db::Literal::of(Term::eq(
        CellName::new("kind").unwrap(),
        Value::U64(1),
    ))]);
    assert_eq!(
        snapshot
            .try_count(namespace(), &query, Budget::unlimited())
            .unwrap_err()
            .kind,
        StorageErrorKind::Corrupt
    );
}

#[test]
fn sequence_exhaustion_fails_before_hashing_staging_or_commit() {
    let (store, engine) = corruptible();
    let state = engine.head_snapshot().state_header();
    drop(engine);
    // Point-read fixture for the terminal numeric sequence; no history scan
    // or 2^64 physical publications are needed to test overflow admission.
    let parent_id = B256::repeat_byte(0xab);
    let manifest = EngineManifest {
        genesis: genesis().id(),
        sequence: u64::MAX,
        parent: Some((u64::MAX - 1, parent_id)),
        state_root: state.root(),
        state,
    };
    let mut records = store.records.lock().unwrap();
    records.insert(
        (Table::Manifests, u64::MAX.to_be_bytes().to_vec()),
        Some(manifest.envelope()),
    );
    records.insert(
        (Table::BlockIndex, manifest.id().to_vec()),
        Some(u64::MAX.to_be_bytes().to_vec()),
    );
    records.insert(
        (Table::BlockIndex, parent_id.to_vec()),
        Some((u64::MAX - 1).to_be_bytes().to_vec()),
    );
    drop(records);
    *store.head.lock().unwrap() = Some(Some(Head {
        height: u64::MAX,
        hash: manifest.id().0,
    }));
    let mut engine = DurableEngine::open(store.clone(), genesis(), Options::default()).unwrap();
    assert_eq!(
        engine.snapshot_count(),
        Err(DurableError::SequenceExhausted)
    );
    let candidate = engine.begin(engine.head(), None).unwrap().seal();
    let before = store.stats();
    assert_eq!(
        engine.prepare(candidate).unwrap_err().error,
        DurableError::SequenceExhausted
    );
    assert_eq!(store.stats().object_reads(), before.object_reads());
    assert_eq!(store.stats().committed, before.committed);
    assert_eq!(engine.head(), id(u64::MAX));
}

// ---------------------------------------------------------------------------
// Concurrent reads through the durable context
// ---------------------------------------------------------------------------

type ReadHook = Arc<dyn Fn() + Send + Sync>;

/// A latching faulty store whose object reads first run a test hook, and
/// which logs when recovery reaches the store.
struct Hooked {
    inner: Faults,
    hook: Mutex<Option<ReadHook>>,
    events: Mutex<Vec<&'static str>>,
}

impl Hooked {
    fn new() -> Self {
        Self {
            inner: FaultyStore::latching(MemoryVersionedStore::new()),
            hook: Mutex::new(None),
            events: Mutex::new(Vec::new()),
        }
    }

    fn set_hook(&self, hook: Option<ReadHook>) {
        *self.hook.lock().unwrap() = hook;
    }

    fn log(&self, event: &'static str) {
        self.events.lock().unwrap().push(event);
    }
}

impl ObjectReader for Hooked {
    fn read_object(&self, space: SpaceId, id: &ObjectId) -> Result<Option<Vec<u8>>, StorageError> {
        self.read_object_bounded(space, id, usize::MAX)
    }
    fn read_object_bounded(
        &self,
        space: SpaceId,
        id: &ObjectId,
        max: usize,
    ) -> Result<Option<Vec<u8>>, StorageError> {
        let hook = self.hook.lock().unwrap().clone();
        if let Some(hook) = hook {
            hook();
        }
        self.inner.read_object_bounded(space, id, max)
    }
}

impl VersionedStore for Hooked {
    fn database_id(&self) -> DatabaseId {
        self.inner.database_id()
    }
    fn head(&self) -> Result<Option<Head>, StorageError> {
        self.inner.head()
    }
    fn read_record(&self, table: Table, key: &[u8]) -> Result<Option<Vec<u8>>, StorageError> {
        self.inner.read_record(table, key)
    }
    fn read_record_bounded(
        &self,
        table: Table,
        key: &[u8],
        max: usize,
    ) -> Result<Option<Vec<u8>>, StorageError> {
        self.inner.read_record_bounded(table, key, max)
    }
    fn commit(&self, batch: &CommitBatch) -> CommitOutcome {
        self.inner.commit(batch)
    }
    fn health(&self) -> Health {
        self.inner.health()
    }
    fn recover(&self) -> Result<Option<Head>, StorageError> {
        self.log("store recover");
        self.inner.recover()
    }
    fn stats(&self) -> StoreStats {
        self.inner.stats()
    }
}

/// An engine over a [`Hooked`] store holding `keys` raw records, with a
/// cold cache.
fn hooked(keys: u8) -> (Arc<Hooked>, DurableEngine) {
    let store = Arc::new(Hooked::new());
    let mut engine = initialize(store.clone(), genesis(), Options::default());
    let mut branch = engine.begin(engine.head(), None).unwrap();
    branch
        .execute(
            OWNER,
            WriteOp::CreateNamespace {
                name: NamespaceName::new("alpha").unwrap(),
            },
            Budget::unlimited(),
        )
        .unwrap();
    for i in 0..keys {
        branch
            .execute(
                OWNER,
                WriteOp::Put {
                    namespace: namespace(),
                    key: vec![i],
                    value: vec![i; 48],
                },
                Budget::unlimited(),
            )
            .unwrap();
    }
    commit(&mut engine, branch.seal());
    assert_eq!(engine.recover().unwrap(), Recovery::Ready);
    assert_eq!(engine.stats().cache.entries, 0);
    (store, engine)
}

fn get(snapshot: &Snapshot, key: u8) -> Result<Vec<u8>, StorageErrorKind> {
    snapshot
        .try_get(namespace(), &[key], Budget::unlimited())
        .map(|record| record.unwrap().value.unwrap())
        .map_err(|error| error.kind)
}

/// Waits until `count` threads arrived, or `timeout` passed; true if all did.
struct Rendezvous {
    arrived: Mutex<usize>,
    all: Condvar,
    count: usize,
}

impl Rendezvous {
    fn new(count: usize) -> Arc<Self> {
        Arc::new(Self {
            arrived: Mutex::new(0),
            all: Condvar::new(),
            count,
        })
    }

    fn arrive(&self, timeout: Duration) -> bool {
        let mut arrived = self.arrived.lock().unwrap();
        *arrived += 1;
        self.all.notify_all();
        let (arrived, _) = self
            .all
            .wait_timeout_while(arrived, timeout, |arrived| *arrived < self.count)
            .unwrap();
        *arrived >= self.count
    }
}

#[test]
fn concurrent_readers_are_inside_the_store_read_at_the_same_time() {
    let (store, engine) = hooked(16);
    let snapshot = engine.head_snapshot();
    let rendezvous = Rendezvous::new(2);
    let met = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(AtomicUsize::new(0));
    let hook = {
        let (rendezvous, met, entered) = (rendezvous.clone(), met.clone(), entered.clone());
        // Only each thread's first read waits: a serialized context would
        // let the second reader in only after the first gave up.
        move || {
            thread_local!(static WAITED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) });
            if !WAITED.with(|waited| waited.replace(true)) {
                entered.fetch_add(1, Ordering::SeqCst);
                if rendezvous.arrive(Duration::from_secs(10)) {
                    met.fetch_add(1, Ordering::SeqCst);
                }
            }
        }
    };
    store.set_hook(Some(Arc::new(hook)));
    thread::scope(|scope| {
        let readers: Vec<_> = [3_u8, 11]
            .into_iter()
            .map(|key| {
                let snapshot = snapshot.clone();
                scope.spawn(move || get(&snapshot, key))
            })
            .collect();
        for (reader, key) in readers.into_iter().zip([3_u8, 11]) {
            assert_eq!(reader.join().unwrap(), Ok(vec![key; 48]));
        }
    });
    store.set_hook(None);
    assert_eq!(entered.load(Ordering::SeqCst), 2);
    assert_eq!(
        met.load(Ordering::SeqCst),
        2,
        "both readers were inside a store read at once"
    );
    // Racing misses of one node keep one retained copy.
    let stats = engine.stats().cache;
    assert!(stats.entries > 0 && stats.hits + stats.misses > 0);
}

#[test]
fn recovery_waits_for_an_in_flight_reader() {
    let (store, mut engine) = hooked(16);
    store.events.lock().unwrap().clear();
    let snapshot = engine.head_snapshot();
    let entered = Rendezvous::new(2);
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let blocked = Arc::new(AtomicBool::new(false));
    let hook = {
        let (entered, release, blocked, store_events) = (
            entered.clone(),
            release.clone(),
            blocked.clone(),
            store.clone(),
        );
        move || {
            // Only the first read blocks.
            if blocked.swap(true, Ordering::SeqCst) {
                return;
            }
            entered.arrive(Duration::from_secs(10));
            let (released, wake) = &*release;
            let guard = released.lock().unwrap();
            let _guard = wake
                .wait_timeout_while(guard, Duration::from_secs(10), |released| !*released)
                .unwrap();
            store_events.log("blocked read returns");
        }
    };
    store.set_hook(Some(Arc::new(hook)));
    let recovered = AtomicBool::new(false);
    thread::scope(|scope| {
        let reader = scope.spawn(|| get(&snapshot, 5));
        assert!(
            entered.arrive(Duration::from_secs(10)),
            "reader entered the store"
        );
        let recovery = scope.spawn(|| {
            let outcome = engine.recover();
            recovered.store(true, Ordering::SeqCst);
            outcome
        });
        thread::sleep(Duration::from_millis(200));
        assert!(
            !recovered.load(Ordering::SeqCst),
            "recovery completed while a load was in flight"
        );
        let (released, wake) = &*release;
        *released.lock().unwrap() = true;
        wake.notify_all();
        assert_eq!(recovery.join().unwrap().unwrap(), Recovery::Ready);
        assert_eq!(reader.join().unwrap(), Ok(vec![5; 48]));
    });
    store.set_hook(None);
    let events = store.events.lock().unwrap().clone();
    assert_eq!(events, ["blocked read returns", "store recover"]);
    let stats = engine.stats().cache;
    assert_eq!(stats.generation, 2);
    // The node of the blocked read (the catalog root, read first) was
    // retained before the recovery and cleared by it, so reading again
    // misses it. (The reader's later loads may run on either side of the
    // recovery.)
    assert_eq!(get(&snapshot, 5), Ok(vec![5; 48]));
    let catalog = MapKind::Catalog.index();
    let again = engine.stats().cache;
    assert_eq!(stats.misses_by_map[catalog], 1);
    assert_eq!(again.misses_by_map[catalog], 2);
}

#[test]
fn a_latch_in_a_burst_of_readers_fails_every_later_read_until_recovery() {
    let (store, mut engine) = hooked(64);
    let snapshot = engine.head_snapshot();
    let generation = engine.stats().cache.generation;
    let latched = AtomicBool::new(false);
    let reads = AtomicUsize::new(0);
    let deadline = Instant::now() + Duration::from_secs(20);
    thread::scope(|scope| {
        for reader in 0..8_u8 {
            let (snapshot, latched, reads) = (snapshot.clone(), &latched, &reads);
            scope.spawn(move || {
                let mut key = reader;
                loop {
                    let after_latch = latched.load(Ordering::SeqCst);
                    let result = get(&snapshot, key % 64);
                    reads.fetch_add(1, Ordering::SeqCst);
                    if after_latch {
                        assert_eq!(result, Err(StorageErrorKind::Io), "read after the latch");
                        return;
                    }
                    if let Ok(value) = result {
                        assert_eq!(value, vec![key % 64; 48]);
                    }
                    assert!(Instant::now() < deadline);
                    key = key.wrapping_add(7);
                }
            });
        }
        while reads.load(Ordering::SeqCst) < 200 {
            thread::yield_now();
        }
        store
            .inner
            .latch(StorageError::new(StorageErrorKind::Io, "latched mid-burst"));
        latched.store(true, Ordering::SeqCst);
    });
    assert!(
        engine.stats().cache.entries > 0,
        "the burst warmed the cache"
    );
    assert_eq!(get(&snapshot, 1), Err(StorageErrorKind::Io));
    assert_eq!(engine.recover().unwrap(), Recovery::Ready);
    let stats = engine.stats().cache;
    assert_eq!((stats.entries, stats.retained_bytes), (0, 0));
    assert_eq!(stats.generation, generation + 1);
    assert_eq!(get(&snapshot, 1), Ok(vec![1; 48]));
}

#[test]
fn cold_warm_and_disabled_caches_report_identical_logical_work() {
    let store = Arc::new(MemoryVersionedStore::new());
    let mut engine = initialize(store.clone(), genesis(), Options::default());
    let candidate = put_candidate(&engine, b"value");
    commit(&mut engine, candidate);
    let disabled = DurableEngine::open(
        store.clone(),
        genesis(),
        Options {
            cache_bytes: 0,
            ..Options::default()
        },
    )
    .unwrap();
    let probe = |snapshot: &Snapshot| {
        let read = snapshot
            .try_get(namespace(), b"key", Budget::unlimited().traced())
            .unwrap()
            .unwrap();
        (read.value, read.receipt)
    };
    assert_eq!(engine.recover().unwrap(), Recovery::Ready);
    let cold = probe(&engine.head_snapshot());
    let warm = probe(&engine.head_snapshot());
    assert!(engine.stats().cache.hits > 0);
    let uncached = probe(&disabled.head_snapshot());
    assert_eq!(disabled.stats().cache.entries, 0);
    assert_eq!(cold, warm);
    assert_eq!(cold, uncached);
}
