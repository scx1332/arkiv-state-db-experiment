//! Atomic simulator packet composition without changing canonical engine state.
use arkiv_db::{
    B256, Budget, Cell, CellName, Engine, NamespaceId, NamespaceName, OwnerId, PriceSchedule,
    Projection, RecordKey, SealedCandidate, SnapshotId, StorageError, StorageErrorKind, Value,
    WriteOp, decode_exact,
    durable::{
        DurableEngine, DurableError, EngineGenesis, EngineManifest, Initialization, Options,
        Publication, Recovery,
        simulator::{
            BLOCK_FRAMING_BYTES, BLOCK_MAX_BYTES, SimulatorIdentity, SimulatorInclusion,
            SimulatorPacket,
        },
    },
    encode,
};
use arkiv_db_memory::MemoryVersionedStore;
use arkiv_db_storage::{
    testing::{CommitFault, FaultyStore},
    versioned::*,
};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

fn identity() -> SimulatorIdentity {
    SimulatorIdentity {
        source_id: [1; 16],
        run_id: [2; 16],
        node_genesis_hash: B256::repeat_byte(3),
        metadata: b"canonical host genesis fixture".to_vec(),
    }
}
fn genesis() -> EngineGenesis {
    EngineGenesis::new(identity().deployment(), PriceSchedule::FIXTURE)
}
fn packet(height: u64, root: B256) -> SimulatorPacket {
    SimulatorPacket {
        height,
        state_root: root,
        bytes: height.to_be_bytes().to_vec(),
        inclusions: if height == 0 {
            vec![]
        } else {
            vec![SimulatorInclusion {
                key: [vec![2], vec![height as u8; 32]].concat(),
                value: height.to_be_bytes().to_vec(),
            }]
        },
    }
}
fn initialize(store: Arc<dyn VersionedStore>) -> DurableEngine {
    let g = genesis();
    let Initialization::Committed(engine) = DurableEngine::initialize_simulator(
        store,
        g.clone(),
        Options::default(),
        identity(),
        packet(0, g.initial_header().root()),
    )
    .unwrap() else {
        panic!("initialization")
    };
    engine
}
fn candidate(engine: &DurableEngine) -> SealedCandidate {
    let mut b = engine.begin(engine.head(), None).unwrap();
    if engine.head().sequence() == 0 {
        b.execute(
            OwnerId::new([7; 20]),
            WriteOp::CreateNamespace {
                name: NamespaceName::new("sim").unwrap(),
            },
            Budget::unlimited(),
        )
        .unwrap();
    }
    b.try_seal().unwrap()
}
fn hash(domain: &str, bytes: &[u8]) -> B256 {
    let mut p = vec![domain.len() as u8];
    p.extend(domain.as_bytes());
    p.extend(bytes);
    alloy_primitives::keccak256(p)
}
fn seal(domain: &str, mut bytes: Vec<u8>) -> Vec<u8> {
    let h = hash(domain, &bytes);
    bytes.extend(h);
    bytes
}

#[test]
fn hand_built_profile_identity_block_and_inclusion_vectors() {
    let i = identity();
    let mut preimage = vec![3; 32];
    preimage.extend([2; 16]);
    assert_eq!(
        i.deployment(),
        hash("arkiv/simulator-engine/v1", &preimage).0
    );
    let mut body = b"ARKSIMI1".to_vec();
    body.extend([1; 16]);
    body.extend([2; 16]);
    body.extend([3; 32]);
    body.extend((i.metadata.len() as u32).to_be_bytes());
    body.extend(&i.metadata);
    let expected = seal("arkiv/simulator-profile/v1", body);
    assert_eq!(i.envelope().unwrap(), expected);
    assert_eq!(SimulatorIdentity::from_envelope(&expected).unwrap(), i);
    for n in 0..expected.len() {
        assert!(SimulatorIdentity::from_envelope(&expected[..n]).is_err());
    }
    assert!(SimulatorIdentity::from_envelope(&[expected.as_slice(), &[0]].concat()).is_err());
    let store = Arc::new(MemoryVersionedStore::new());
    let mut engine = initialize(store.clone());
    assert_eq!(
        store
            .read_record(Table::Meta, b"engine/profile")
            .unwrap()
            .unwrap(),
        [b"ARKSIM01".as_slice(), &1_u32.to_be_bytes()].concat()
    );
    let c = candidate(&engine);
    let p = packet(1, c.state_root());
    let inclusion = p.inclusions[0].clone();
    let payload = p.bytes.clone();
    let info = engine.prepare_simulator(c, p).unwrap();
    engine.commit().unwrap();
    let mut block = b"ARKSIMB1".to_vec();
    block.extend(1_u64.to_be_bytes());
    block.extend(info.commit_id);
    block.extend(info.state_root);
    block.extend(8_u32.to_be_bytes());
    block.extend(payload);
    assert_eq!(
        store
            .read_record(Table::Blocks, &1_u64.to_be_bytes())
            .unwrap()
            .unwrap(),
        seal("arkiv/simulator-node-record/v1", block)
    );
    let mut record = b"ARKSIMR1".to_vec();
    record.extend(1_u64.to_be_bytes());
    record.extend(info.commit_id);
    record.push(33);
    record.extend(&inclusion.key);
    record.extend(8_u32.to_be_bytes());
    record.extend(inclusion.value);
    assert_eq!(
        store
            .read_record(Table::Inclusions, &inclusion.key)
            .unwrap()
            .unwrap(),
        seal("arkiv/simulator-inclusion/v1", record)
    );
}

#[test]
fn host_publication_is_atomic_lazy_and_profile_separated() {
    let store = Arc::new(MemoryVersionedStore::new());
    let mut e = initialize(store.clone());
    assert!(DurableEngine::open(store.clone(), genesis(), Options::default()).is_err());
    assert_eq!(
        DurableEngine::read_simulator_identity(store.as_ref()).unwrap(),
        identity()
    );
    let c = candidate(&e);
    assert!(e.prepare(c.clone()).is_err());
    let p = packet(1, c.state_root());
    let key = p.inclusions[0].key.clone();
    let info = e.prepare_simulator(c, p).unwrap();
    assert_eq!(e.head().sequence(), 0);
    assert!(e.simulator_block(1).is_err());
    assert!(e.simulator_inclusion(&key).unwrap().is_none());
    assert!(
        store
            .read_record(Table::Blocks, &1_u64.to_be_bytes())
            .unwrap()
            .is_none()
    );
    assert_eq!(e.commit().unwrap(), Publication::Committed(info));
    assert_eq!(e.simulator_block(1).unwrap().bytes, 1_u64.to_be_bytes());
    assert_eq!(
        e.simulator_inclusion(&key).unwrap(),
        Some(1_u64.to_be_bytes().to_vec())
    );
    assert!(e.head_snapshot().staged_objects().is_empty());
    drop(e);
    let before = store.stats();
    let e = DurableEngine::open_simulator(store.clone(), genesis(), Options::default(), identity())
        .unwrap();
    let delta = store.stats().since(&before);
    assert_eq!(delta.object_reads(), 0);
    assert_eq!(delta.record_reads, 9);
    assert_eq!(delta.head_reads, 1);
    assert_eq!(e.head_info(), info);
    assert_eq!(e.simulator_block(0).unwrap().bytes, [0; 8]);
    let mut wrong = identity();
    wrong.source_id[0] ^= 1;
    assert!(
        DurableEngine::open_simulator(store.clone(), genesis(), Options::default(), wrong).is_err()
    );
    let ordinary = Arc::new(MemoryVersionedStore::new());
    DurableEngine::initialize(ordinary.clone(), genesis(), Options::default()).unwrap();
    assert!(
        DurableEngine::open_simulator(ordinary, genesis(), Options::default(), identity()).is_err()
    );
}

#[test]
fn exact_host_packet_retry_and_uncertain_exposure() {
    for fault in [
        CommitFault::NotCommitted(StorageErrorKind::Full),
        CommitFault::IndeterminateNotApplied(StorageErrorKind::Io),
        CommitFault::IndeterminateApplied(StorageErrorKind::Io),
        CommitFault::AckLost,
    ] {
        let store = Arc::new(FaultyStore::latching(MemoryVersionedStore::new()));
        let mut e = initialize(store.clone());
        let c = candidate(&e);
        let p = packet(1, c.state_root());
        let key = p.inclusions[0].key.clone();
        let info = e.prepare_simulator(c, p).unwrap();
        store.push_commit_fault(fault);
        assert!(!matches!(e.commit().unwrap(), Publication::Committed(_)));
        assert_eq!(e.head().sequence(), 0);
        let recovery = e.recover_pending().unwrap();
        if matches!(
            fault,
            CommitFault::IndeterminateApplied(_) | CommitFault::AckLost
        ) {
            assert_eq!(recovery, Recovery::ObservedCommittedDurabilityUnknown(info));
            assert!(e.commit().is_err());
            assert!(e.simulator_block(1).is_err());
            assert!(e.simulator_inclusion(&key).unwrap().is_none());
        } else {
            assert_eq!(recovery, Recovery::RetryPrepared);
            assert_eq!(e.commit().unwrap(), Publication::Committed(info));
            assert_eq!(
                e.simulator_inclusion(&key).unwrap(),
                Some(1_u64.to_be_bytes().to_vec())
            );
        }
        let reopened =
            DurableEngine::open_simulator(store, genesis(), Options::default(), identity())
                .unwrap();
        assert_eq!(reopened.simulator_block(1).unwrap().info, info);
    }
}

#[test]
fn packet_caps_identity_and_state_association_fail_before_commit() {
    let store = Arc::new(MemoryVersionedStore::new());
    let mut e = initialize(store.clone());
    let c = candidate(&e);
    let base = packet(1, c.state_root());
    let mut variants = Vec::new();
    let mut p = base.clone();
    p.height = 2;
    variants.push(p);
    let mut p = base.clone();
    p.state_root = B256::ZERO;
    variants.push(p);
    let mut p = base.clone();
    p.inclusions[0].key[0] = 3;
    variants.push(p);
    let mut p = base.clone();
    p.inclusions.push(p.inclusions[0].clone());
    variants.push(p);
    let mut p = base.clone();
    p.inclusions[0].value = vec![0; 1025];
    variants.push(p);
    let mut p = base.clone();
    p.bytes = vec![0; BLOCK_MAX_BYTES - BLOCK_FRAMING_BYTES + 1];
    variants.push(p);
    for p in variants {
        assert!(e.prepare_simulator(c.clone(), p).is_err());
        assert_eq!(e.head().sequence(), 0);
        assert!(
            store
                .read_record(Table::Blocks, &1_u64.to_be_bytes())
                .unwrap()
                .is_none()
        );
    }
    let cap = 600;
    drop(e);
    let mut e = DurableEngine::open_simulator(
        store.clone(),
        genesis(),
        Options {
            prepare_bytes: cap,
            ..Options::default()
        },
        identity(),
    )
    .unwrap();
    let c = candidate(&e);
    let p = packet(1, c.state_root());
    assert!(e.prepare_simulator(c, p).is_err());
    assert_eq!(e.head().sequence(), 0);
}

type RecordOverrides = BTreeMap<(Table, Vec<u8>), Option<Vec<u8>>>;
#[derive(Default)]
struct Overlay {
    inner: MemoryVersionedStore,
    records: Mutex<RecordOverrides>,
}
impl ObjectReader for Overlay {
    fn read_object(&self, s: SpaceId, id: &ObjectId) -> Result<Option<Vec<u8>>, StorageError> {
        self.inner.read_object(s, id)
    }
    fn read_object_bounded(
        &self,
        s: SpaceId,
        id: &ObjectId,
        n: usize,
    ) -> Result<Option<Vec<u8>>, StorageError> {
        self.inner.read_object_bounded(s, id, n)
    }
}
impl VersionedStore for Overlay {
    fn database_id(&self) -> DatabaseId {
        self.inner.database_id()
    }
    fn head(&self) -> Result<Option<Head>, StorageError> {
        self.inner.head()
    }
    fn read_record(&self, _: Table, _: &[u8]) -> Result<Option<Vec<u8>>, StorageError> {
        panic!("unbounded host read")
    }
    fn read_record_bounded(
        &self,
        t: Table,
        k: &[u8],
        n: usize,
    ) -> Result<Option<Vec<u8>>, StorageError> {
        if let Some(v) = self.records.lock().unwrap().get(&(t, k.to_vec())) {
            if v.as_ref().is_some_and(|v| v.len() > n) {
                return Err(StorageError::new(StorageErrorKind::Corrupt, "oversize"));
            }
            return Ok(v.clone());
        }
        self.inner.read_record_bounded(t, k, n)
    }
    fn commit(&self, b: &CommitBatch) -> CommitOutcome {
        self.inner.commit(b)
    }
    fn health(&self) -> Health {
        self.inner.health()
    }
    fn recover(&self) -> Result<Option<Head>, StorageError> {
        self.inner.recover()
    }
    fn stats(&self) -> StoreStats {
        self.inner.stats()
    }
}

#[test]
fn missing_tampered_or_wrongly_associated_host_packet_is_corruption() {
    let store = Arc::new(Overlay::default());
    let mut e = initialize(store.clone());
    let c = candidate(&e);
    let p = packet(1, c.state_root());
    e.prepare_simulator(c, p).unwrap();
    e.commit().unwrap();
    let key = 1_u64.to_be_bytes().to_vec();
    let bytes = store
        .inner
        .read_record(Table::Blocks, &key)
        .unwrap()
        .unwrap();
    let mut corrupt = bytes.clone();
    corrupt[10] ^= 1;
    let mut wrong = bytes[..bytes.len() - 32].to_vec();
    wrong[20] ^= 1;
    let wrong = seal("arkiv/simulator-node-record/v1", wrong);
    for value in [
        None,
        Some(vec![0; BLOCK_MAX_BYTES + 1]),
        Some(bytes[..50].to_vec()),
        Some(corrupt),
        Some(wrong),
    ] {
        store
            .records
            .lock()
            .unwrap()
            .insert((Table::Blocks, key.clone()), value);
        assert!(matches!(
            DurableEngine::open_simulator(store.clone(), genesis(), Options::default(), identity()),
            Err(DurableError::Storage(StorageError {
                kind: StorageErrorKind::Corrupt,
                ..
            }))
        ));
        assert!(e.simulator_block(1).is_err());
    }
}

#[test]
fn long_history_keeps_one_manifest_and_reopens_without_trie_reads() {
    let store = Arc::new(MemoryVersionedStore::new());
    let mut e = initialize(store.clone());
    for height in 1..=1000 {
        let c = candidate(&e);
        let mut p = packet(height, c.state_root());
        p.inclusions.clear();
        e.prepare_simulator(c, p).unwrap();
        e.commit().unwrap();
        assert_eq!(e.stats().resident_manifests, 1);
        assert_eq!(e.stats().pending_payload_bytes, 0);
        assert!(e.head_snapshot().staged_objects().is_empty());
    }
    drop(e);
    let before = store.stats();
    let e = DurableEngine::open_simulator(store.clone(), genesis(), Options::default(), identity())
        .unwrap();
    let delta = store.stats().since(&before);
    assert_eq!(delta.record_reads, 9);
    assert_eq!(delta.object_reads(), 0);
    assert_eq!(e.head(), SnapshotId::from_sequence(1000));
    for h in [0, 1, 17, 999, 1000] {
        assert_eq!(e.simulator_block(h).unwrap().bytes, h.to_be_bytes());
    }
}

fn prefilled() -> (EngineGenesis, SealedCandidate) {
    let engine = Engine::new(PriceSchedule::FIXTURE);
    let mut branch = engine.begin(engine.head(), None).unwrap();
    let owner = OwnerId::new([7; 20]);
    let namespace = NamespaceId::from_ordinal(1).unwrap();
    branch
        .execute(
            owner,
            WriteOp::CreateNamespace {
                name: NamespaceName::new("initial").unwrap(),
            },
            Budget::unlimited(),
        )
        .unwrap();
    for i in 0..3u8 {
        branch
            .execute(
                owner,
                WriteOp::CreateRecord {
                    namespace,
                    key: RecordKey::new(&[i + 1]).unwrap(),
                    cells: vec![
                        (
                            CellName::new("count").unwrap(),
                            Cell::attribute(Value::U64(i.into())),
                        ),
                        (
                            CellName::new("body").unwrap(),
                            Cell::field(Value::Bytes(vec![i; 96].into())),
                        ),
                    ],
                },
                Budget::unlimited(),
            )
            .unwrap();
    }
    branch
        .execute(
            owner,
            WriteOp::Put {
                namespace,
                key: b"raw".to_vec(),
                value: vec![9; 512],
            },
            Budget::unlimited(),
        )
        .unwrap();
    branch
        .try_host_put([8; 32], b"initial host value")
        .unwrap()
        .unwrap();
    let candidate = branch.try_seal().unwrap();
    let mut genesis = genesis();
    genesis.initial_state = Some(candidate.state_header());
    (genesis, candidate)
}

fn initialize_prefilled(
    store: Arc<dyn VersionedStore>,
    genesis: EngineGenesis,
    candidate: SealedCandidate,
    options: Options,
) -> Result<Initialization, DurableError> {
    let packet = packet(0, genesis.initial_header().root());
    DurableEngine::initialize_simulator_prefilled(
        store,
        genesis,
        options,
        identity(),
        packet,
        candidate,
    )
}

fn assert_prefilled(engine: &DurableEngine) {
    assert_eq!(engine.head().sequence(), 0);
    assert_eq!(
        engine.head_info().state_root,
        engine.genesis().initial_header().root()
    );
    assert_eq!(engine.simulator_block(0).unwrap().info, engine.head_info());
    assert_eq!(engine.simulator_block(0).unwrap().bytes, vec![0; 8]);
    let snapshot = engine.head_snapshot();
    let namespace = NamespaceId::from_ordinal(1).unwrap();
    assert_eq!(
        snapshot.try_host_get(&[8; 32]).unwrap(),
        Some(b"initial host value".to_vec())
    );
    assert_eq!(snapshot.try_typed_record_count(namespace).unwrap(), Some(3));
    assert_eq!(
        snapshot.try_index_consistent(namespace).unwrap(),
        Some(true)
    );
    assert_eq!(
        snapshot
            .try_get(namespace, b"raw", Budget::unlimited())
            .unwrap()
            .unwrap()
            .value,
        Some(vec![9; 512])
    );
    for i in 0..3u8 {
        let row = snapshot
            .try_get_record(
                namespace,
                &RecordKey::new(&[i + 1]).unwrap(),
                &Projection::named(
                    true,
                    [
                        CellName::new("body").unwrap(),
                        CellName::new("count").unwrap(),
                    ],
                ),
                Budget::unlimited(),
            )
            .unwrap()
            .unwrap()
            .value
            .unwrap();
        assert_eq!(row.cells.len(), 2);
    }
    assert!(snapshot.staged_objects().is_empty());
}

#[test]
fn prefilled_genesis_has_distinct_encoding_and_one_complete_height_zero_commit() {
    let (g, c) = prefilled();
    let empty = genesis();
    assert_eq!(empty.initial_state, None);
    assert_eq!(
        decode_exact::<EngineGenesis>(&encode(&empty)).unwrap(),
        empty
    );
    // The empty genesis ends in the absent-option byte; a prefilled one
    // replaces it with `01 ‖ header`.
    let mut expected = encode(&empty);
    assert_eq!(expected.pop(), Some(0));
    expected.push(1);
    expected.extend(encode(g.initial_state.as_ref().unwrap()));
    assert_eq!(encode(&g), expected);
    assert_eq!(decode_exact::<EngineGenesis>(&expected).unwrap(), g);
    assert_ne!(g.id(), empty.id());
    for len in 0..expected.len() {
        assert!(decode_exact::<EngineGenesis>(&expected[..len]).is_err());
    }
    let store = Arc::new(MemoryVersionedStore::new());
    let Initialization::Committed(engine) =
        initialize_prefilled(store.clone(), g.clone(), c, Options::default()).unwrap()
    else {
        panic!("initialization");
    };
    assert_prefilled(&engine);
    assert_eq!(store.stats().committed, 1);
    assert_eq!(store.stats().records_written, 6);
    assert!(engine.stats().last_preparation.unique_objects > 0);
    assert!(engine.stats().last_preparation.preflight_visits > 0);
    assert!(engine.stats().last_preparation.payload_bytes > 0);
    let before = store.stats();
    let mut reopened =
        DurableEngine::open_simulator(store.clone(), g.clone(), Options::default(), identity())
            .unwrap();
    assert_eq!(store.stats().since(&before).object_reads(), 0);
    assert_prefilled(&reopened);
    assert!(
        DurableEngine::open_simulator(store.clone(), empty, Options::default(), identity())
            .is_err()
    );
    let c = reopened
        .begin(reopened.head(), None)
        .unwrap()
        .try_seal()
        .unwrap();
    let p = packet(1, c.state_root());
    reopened.prepare_simulator(c, p).unwrap();
    assert!(matches!(
        reopened.commit().unwrap(),
        Publication::Committed(_)
    ));
    assert_eq!(
        reopened
            .snapshot(SnapshotId::from_sequence(0))
            .unwrap()
            .state_root(),
        g.initial_header().root()
    );
    assert_eq!(
        DurableEngine::open_simulator(store, g, Options::default(), identity())
            .unwrap()
            .head()
            .sequence(),
        1
    );
}

#[test]
fn prefilled_genesis_limits_and_invariants_fail_without_writes() {
    let (g, c) = prefilled();
    let mut variants = vec![];
    let mut changed = g.clone();
    changed.initial_state = None;
    variants.push((changed, Options::default()));
    let mut changed = g.clone();
    changed.initial_state.as_mut().unwrap().host_root = B256::repeat_byte(77);
    variants.push((changed, Options::default()));
    let mut changed = g.clone();
    changed.pricing = PriceSchedule::uniform(2, 3);
    variants.push((changed, Options::default()));
    let mut changed = g.clone();
    changed.limits.namespace_cap = 0;
    variants.push((changed, Options::default()));
    let mut changed = g.clone();
    changed.limits.max_cells = 1;
    variants.push((changed, Options::default()));
    let mut changed = g.clone();
    changed.limits = changed.limits.with_value_len(95);
    variants.push((changed, Options::default()));
    let mut changed = g.clone();
    changed.max_object_bytes = 500;
    variants.push((changed, Options::default()));
    variants.push((
        g.clone(),
        Options {
            prepare_bytes: 2000,
            ..Options::default()
        },
    ));
    variants.push((
        g.clone(),
        Options {
            prepare_objects: 1,
            ..Options::default()
        },
    ));
    variants.push((
        g.clone(),
        Options {
            prepare_visits: 1,
            ..Options::default()
        },
    ));
    for (genesis, options) in variants {
        let store = Arc::new(MemoryVersionedStore::new());
        assert!(initialize_prefilled(store.clone(), genesis, c.clone(), options).is_err());
        assert_eq!(store.head().unwrap(), None);
        assert_eq!(store.stats().committed, 0);
        assert_eq!(store.stats().records_written, 0);
        assert!(
            store
                .stats()
                .spaces
                .values()
                .all(|s| s.objects_written == 0)
        );
    }
    let store = Arc::new(MemoryVersionedStore::new());
    assert!(
        DurableEngine::initialize_simulator(
            store.clone(),
            g.clone(),
            Options::default(),
            identity(),
            packet(0, c.state_root())
        )
        .is_err()
    );
    assert_eq!(store.head().unwrap(), None);
    let Initialization::Committed(source) =
        initialize_prefilled(store, g.clone(), c, Options::default()).unwrap()
    else {
        panic!("initialization");
    };
    let inherited = source
        .begin(source.head(), None)
        .unwrap()
        .try_seal()
        .unwrap();
    let target = Arc::new(MemoryVersionedStore::new());
    assert!(initialize_prefilled(target.clone(), g, inherited, Options::default()).is_err());
    assert_eq!(target.head().unwrap(), None);
    assert_eq!(target.stats().records_written, 0);
}

#[test]
fn prefilled_genesis_atomic_faults_retry_exact_batch_or_require_reopen() {
    for fault in [
        CommitFault::NotCommitted(StorageErrorKind::Full),
        CommitFault::IndeterminateNotApplied(StorageErrorKind::Io),
        CommitFault::IndeterminateApplied(StorageErrorKind::Io),
        CommitFault::AckLost,
    ] {
        let store = Arc::new(FaultyStore::latching(MemoryVersionedStore::new()));
        let (g, c) = prefilled();
        store.push_commit_fault(fault);
        let outcome =
            initialize_prefilled(store.clone(), g.clone(), c, Options::default()).unwrap();
        let mut pending = match outcome {
            Initialization::NotCommitted { pending, .. }
            | Initialization::Indeterminate { pending, .. } => pending,
            Initialization::Committed(_) => panic!("fault must not expose an engine"),
        };
        let applied = matches!(
            fault,
            CommitFault::IndeterminateApplied(_) | CommitFault::AckLost
        );
        assert_eq!(pending.recover().unwrap(), !applied);
        if applied {
            assert!(matches!(
                pending.commit(),
                Err(DurableError::WriterUnavailable)
            ));
        } else {
            assert_eq!(store.head().unwrap(), None);
            assert_eq!(store.stats().records_written, 0);
            assert!(
                store
                    .stats()
                    .spaces
                    .values()
                    .all(|s| s.objects_written == 0)
            );
            let Initialization::Committed(engine) = pending.commit().unwrap() else {
                panic!("exact retry");
            };
            assert_prefilled(&engine);
        }
        let engine =
            DurableEngine::open_simulator(store, g, Options::default(), identity()).unwrap();
        assert_prefilled(&engine);
    }
}

#[test]
fn prefilled_genesis_preparation_inclusive_caps_and_packet_association() {
    let (g, c) = prefilled();
    let Initialization::Committed(engine) = initialize_prefilled(
        Arc::new(MemoryVersionedStore::new()),
        g.clone(),
        c.clone(),
        Options::default(),
    )
    .unwrap() else {
        panic!("initialization");
    };
    let stats = engine.stats().last_preparation;
    let exact = Options {
        prepare_bytes: stats.payload_bytes,
        prepare_objects: stats.unique_objects,
        ..Options::default()
    };
    assert!(matches!(
        initialize_prefilled(
            Arc::new(MemoryVersionedStore::new()),
            g.clone(),
            c.clone(),
            exact.clone(),
        )
        .unwrap(),
        Initialization::Committed(_)
    ));
    for options in [
        Options {
            prepare_bytes: exact.prepare_bytes - 1,
            ..exact.clone()
        },
        Options {
            prepare_objects: exact.prepare_objects - 1,
            ..exact
        },
    ] {
        let store = Arc::new(MemoryVersionedStore::new());
        assert!(matches!(
            initialize_prefilled(store.clone(), g.clone(), c.clone(), options),
            Err(DurableError::ResourceLimit(_))
        ));
        assert_eq!(store.head().unwrap(), None);
        assert_eq!(store.stats().records_written, 0);
    }
    let valid = packet(0, c.state_root());
    let wrong_height = SimulatorPacket {
        height: 1,
        ..valid.clone()
    };
    let wrong_root = SimulatorPacket {
        state_root: B256::ZERO,
        ..valid.clone()
    };
    let inclusions = SimulatorPacket {
        inclusions: packet(1, c.state_root()).inclusions,
        ..valid
    };
    for packet in [wrong_height, wrong_root, inclusions] {
        let store = Arc::new(MemoryVersionedStore::new());
        assert!(
            DurableEngine::initialize_simulator_prefilled(
                store.clone(),
                g.clone(),
                Options::default(),
                identity(),
                packet,
                c.clone()
            )
            .is_err()
        );
        assert_eq!(store.head().unwrap(), None);
        assert_eq!(store.stats().records_written, 0);
    }
}

#[test]
fn prefilled_genesis_open_checks_height_zero_root_even_under_later_head() {
    let store = Arc::new(Overlay::default());
    let (g, c) = prefilled();
    let Initialization::Committed(mut engine) =
        initialize_prefilled(store.clone(), g.clone(), c, Options::default()).unwrap()
    else {
        panic!("initialization");
    };
    let candidate = engine
        .begin(engine.head(), None)
        .unwrap()
        .try_seal()
        .unwrap();
    let packet = packet(1, candidate.state_root());
    engine.prepare_simulator(candidate, packet).unwrap();
    engine.commit().unwrap();
    let wrong = EngineManifest {
        genesis: g.id(),
        sequence: 0,
        parent: None,
        state: genesis().initial_header(),
        state_root: genesis().initial_header().root(),
    };
    let mut overrides = store.records.lock().unwrap();
    overrides.insert(
        (Table::Manifests, 0u64.to_be_bytes().to_vec()),
        Some(wrong.envelope()),
    );
    overrides.insert(
        (Table::BlockIndex, wrong.id().to_vec()),
        Some(0u64.to_be_bytes().to_vec()),
    );
    drop(overrides);
    let error =
        DurableEngine::open_simulator(store, g, Options::default(), identity()).unwrap_err();
    assert!(
        matches!(error, DurableError::Storage(StorageError { kind: StorageErrorKind::Corrupt, detail }) if detail.contains("configured initial state"))
    );
}
