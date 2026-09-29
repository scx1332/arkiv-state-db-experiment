#![allow(dead_code)]

use std::sync::Arc;

use arkiv_db::{
    B256, Branch, Budget, Cell, CellName, Engine, NamespaceId, NamespaceName, OwnerId,
    PriceSchedule, RecordKey, Snapshot, SnapshotId, Value, WriteOp,
    durable::{DurableEngine, EngineGenesis, Initialization, Options, Publication},
    proof::{EqQuery, EqRequest, ProofLimits, SnapshotRef, VerifiedEq, verify_eq},
};
use arkiv_db_redb::{RedbConfig, RedbStore, testing::CrashDisk};
use arkiv_db_storage::versioned::VersionedStore;

pub const OWNER: OwnerId = OwnerId::new([7; 20]);
pub const HOST_KEY: [u8; 32] = [19; 32];
pub fn ns(n: u64) -> NamespaceId {
    NamespaceId::from_ordinal(n).unwrap()
}
pub fn name(s: &str) -> CellName {
    CellName::new(s).unwrap()
}
pub fn key(n: u64) -> RecordKey {
    RecordKey::new(&n.to_be_bytes()).unwrap()
}
pub fn genesis() -> EngineGenesis {
    EngineGenesis::new([0x71; 32], PriceSchedule::FIXTURE)
}
pub fn options(cache_bytes: usize) -> Options {
    Options {
        cache_bytes,
        ..Options::default()
    }
}
pub fn config() -> RedbConfig {
    RedbConfig::with_cache_size(32 << 10)
}

pub fn disk_store(disk: &CrashDisk) -> Arc<RedbStore> {
    let disk = disk.clone();
    Arc::new(RedbStore::with_backend(move || Ok(disk.backend()), config()).unwrap())
}
pub fn initialize(store: Arc<dyn VersionedStore>, cache_bytes: usize) -> DurableEngine {
    match DurableEngine::initialize(store, genesis(), options(cache_bytes)).unwrap() {
        Initialization::Committed(engine) => engine,
        other => panic!("initialization: {other:?}"),
    }
}
pub fn execute(branch: &mut Branch, op: WriteOp) {
    branch
        .try_execute(OWNER, op, Budget::unlimited())
        .unwrap()
        .unwrap();
}
pub fn namespace(branch: &mut Branch, text: &str) {
    execute(
        branch,
        WriteOp::CreateNamespace {
            name: NamespaceName::new(text).unwrap(),
        },
    );
}
pub fn row(branch: &mut Branch, namespace: NamespaceId, n: u64) {
    execute(
        branch,
        WriteOp::CreateRecord {
            namespace,
            key: key(n),
            cells: vec![
                (name("kind"), Cell::attribute(Value::U64(n % 16))),
                (name("payload"), Cell::field(Value::bytes(&[n as u8; 64]))),
            ],
        },
    );
}
pub fn put(branch: &mut Branch, namespace: NamespaceId, key: &[u8], value: &[u8]) {
    execute(
        branch,
        WriteOp::Put {
            namespace,
            key: key.to_vec(),
            value: value.to_vec(),
        },
    );
}
pub fn base(branch: &mut Branch) {
    namespace(branch, "target");
    namespace(branch, "other");
    put(branch, ns(1), b"version", b"parent");
    put(branch, ns(2), b"untouched", b"isolated");
    for n in 0..16 {
        row(branch, ns(1), n);
    }
    branch
        .try_host_put(HOST_KEY, b"parent-host")
        .unwrap()
        .unwrap();
}
pub fn candidate(branch: &mut Branch, large: bool) {
    namespace(branch, "newspace");
    put(branch, ns(1), b"version", b"candidate");
    row(branch, ns(1), 16);
    let count = if large { 192 } else { 4 };
    let len = if large { 9000 } else { 256 };
    for n in 0u64..count {
        put(branch, ns(3), &n.to_be_bytes(), &vec![n as u8; len]);
    }
    branch
        .try_host_put(HOST_KEY, b"candidate-host")
        .unwrap()
        .unwrap();
}
pub fn publish(engine: &mut DurableEngine, branch: Branch) {
    let info = engine.prepare(branch.try_seal().unwrap()).unwrap();
    assert_eq!(engine.commit().unwrap(), Publication::Committed(info));
}
pub fn memory_snapshots(large: bool) -> (Snapshot, Snapshot) {
    let mut engine = Engine::new(PriceSchedule::FIXTURE);
    let mut branch = engine.begin(engine.head(), None).unwrap();
    base(&mut branch);
    engine.publish(&branch.seal()).unwrap();
    let parent = engine.head_snapshot();
    let mut branch = engine.begin(engine.head(), None).unwrap();
    candidate(&mut branch, large);
    engine.publish(&branch.seal()).unwrap();
    (parent, engine.head_snapshot())
}
pub fn proof_request(snapshot: &Snapshot, namespace: NamespaceId) -> EqRequest {
    EqRequest {
        snapshot: SnapshotRef {
            // Test-only caller-resolved block identities, not fabricated node headers.
            genesis_hash: B256::repeat_byte(11),
            height: snapshot.id().sequence(),
            block_hash: B256::repeat_byte(12),
            state_root: snapshot.state_root(),
        },
        query: EqQuery {
            namespace,
            attribute: name("kind"),
            value: Value::U64(0),
        },
        limit: 64,
        continuation: None,
    }
}
pub fn proof(snapshot: &Snapshot, namespace: NamespaceId) -> Vec<u8> {
    let request = proof_request(snapshot, namespace);
    let generated = snapshot
        .prove_eq(&request, ProofLimits::default(), Budget::unlimited())
        .unwrap();
    let verified = verify_eq(
        &request,
        None,
        &generated.bytes,
        ProofLimits::default(),
        genesis().limits,
    )
    .unwrap();
    assert!(matches!(verified, VerifiedEq::Page(_)));
    generated.bytes
}
pub fn check(engine: &DurableEngine, parent: &Snapshot, candidate: &Snapshot, large: bool) {
    assert!(engine.head().sequence() == 1 || engine.head().sequence() == 2);
    let old = engine.snapshot(SnapshotId::from_sequence(1)).unwrap();
    assert_eq!(old.state_header(), parent.state_header());
    assert_eq!(old.try_index_consistent(ns(1)).unwrap(), Some(true));
    assert_eq!(
        old.try_host_get(&HOST_KEY).unwrap(),
        Some(b"parent-host".to_vec())
    );
    assert_eq!(proof(&old, ns(1)), proof(parent, ns(1)));
    let head = engine.head_snapshot();
    let changed = head.id().sequence() == 2;
    let expected = if changed { candidate } else { parent };
    assert_eq!(head.state_header(), expected.state_header());
    assert_eq!(head.try_index_consistent(ns(1)).unwrap(), Some(true));
    for n in 0..if changed { 17 } else { 16 } {
        let got = head
            .try_get_record(
                ns(1),
                &key(n),
                &arkiv_db::Projection::ALL,
                Budget::unlimited(),
            )
            .unwrap()
            .unwrap();
        let want = expected
            .try_get_record(
                ns(1),
                &key(n),
                &arkiv_db::Projection::ALL,
                Budget::unlimited(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(got.value, want.value);
        assert_eq!(got.receipt, want.receipt);
    }
    assert_eq!(
        head.try_host_get(&HOST_KEY).unwrap(),
        expected.try_host_get(&HOST_KEY).unwrap()
    );
    assert_eq!(proof(&head, ns(1)), proof(expected, ns(1)));
    for (namespace, key) in [(ns(1), b"version".as_slice()), (ns(2), b"untouched")] {
        assert_eq!(
            head.try_get(namespace, key, Budget::unlimited()).unwrap(),
            expected
                .try_get(namespace, key, Budget::unlimited())
                .unwrap()
        );
    }
    if changed {
        for n in 0u64..if large { 192 } else { 4 } {
            assert_eq!(
                head.try_get(ns(3), &n.to_be_bytes(), Budget::unlimited())
                    .unwrap()
                    .unwrap()
                    .value,
                Some(vec![n as u8; if large { 9000 } else { 256 }])
            );
        }
    }
    assert!(
        head.staged_objects().is_empty(),
        "committed head owns stored roots only"
    );
}
