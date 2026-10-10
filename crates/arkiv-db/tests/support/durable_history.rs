//! One seeded engine/storage history used unchanged by memory and real redb tests.
use arkiv_db::{
    B256, Branch, Budget, Cell, CellName, Engine, Limits, NamespaceId, NamespaceName, OwnerId,
    PriceSchedule, Projection, Query, RecordKey, Revision, Snapshot, SnapshotId, Value, WriteOp,
    durable::{DurableEngine, EngineGenesis, Initialization, Options, Publication},
    proof::{EqQuery, EqRequest, ProofLimits, SnapshotRef, VerifiedEq, verify_eq},
};
use arkiv_db_storage::versioned::VersionedStore;
use std::sync::Arc;
const OWNER: OwnerId = OwnerId::new([7; 20]);
fn ns(n: u64) -> NamespaceId {
    NamespaceId::from_ordinal(n).unwrap()
}
fn name(n: &str) -> CellName {
    CellName::new(n).unwrap()
}
fn key(n: u64) -> RecordKey {
    RecordKey::new(&n.to_be_bytes()).unwrap()
}
fn genesis() -> EngineGenesis {
    EngineGenesis::new([9; 32], PriceSchedule::FIXTURE)
}
fn initialize(store: Arc<dyn VersionedStore>, options: Options) -> DurableEngine {
    match DurableEngine::initialize(store, genesis(), options).unwrap() {
        Initialization::Committed(engine) => engine,
        other => panic!("{other:?}"),
    }
}
fn apply_pair(a: &mut Branch, b: &mut Branch, op: WriteOp) {
    assert_eq!(
        a.execute(OWNER, op.clone(), Budget::unlimited()),
        b.try_execute(OWNER, op, Budget::unlimited()).unwrap()
    );
}
fn proof_parity(a: &Snapshot, b: &Snapshot, value: Value) {
    let mut request = EqRequest {
        snapshot: SnapshotRef {
            genesis_hash: B256::repeat_byte(1),
            height: a.id().sequence(),
            block_hash: B256::repeat_byte(2),
            state_root: a.state_root(),
        },
        query: EqQuery {
            namespace: ns(1),
            attribute: name("kind"),
            value,
        },
        limit: 3,
        continuation: None,
    };
    let mut previous = None;
    loop {
        let left = a
            .prove_eq(&request, ProofLimits::default(), Budget::unlimited())
            .unwrap();
        let right = b
            .prove_eq(&request, ProofLimits::default(), Budget::unlimited())
            .unwrap();
        assert_eq!(left.bytes, right.bytes);
        assert_eq!(left.receipt, right.receipt);
        let verified = verify_eq(
            &request,
            previous.as_ref(),
            &right.bytes,
            ProofLimits::default(),
            Limits::DEFAULT,
        )
        .unwrap();
        let VerifiedEq::Page(page) = verified else {
            break;
        };
        previous = page.next;
        request.continuation = previous.as_ref().map(|p| p.cursor().clone());
        if previous.is_none() {
            break;
        }
    }
}

fn compare(a: &Snapshot, b: &Snapshot) {
    assert_eq!(a.state_header(), b.state_header());
    assert_eq!(a.state_root(), b.state_root());
    assert_eq!(
        a.try_host_range(&[0; 32], 100),
        b.try_host_range(&[0; 32], 100)
    );
    for namespace in [ns(1), ns(2)] {
        assert_eq!(
            a.try_namespace_roots(namespace).unwrap(),
            b.try_namespace_roots(namespace).unwrap()
        );
        assert_eq!(
            a.scan_prefix(namespace, b"", Budget::unlimited()),
            b.scan_prefix(namespace, b"", Budget::unlimited())
        );
        assert_eq!(
            a.descriptor(namespace, Budget::unlimited()),
            b.descriptor(namespace, Budget::unlimited())
        );
        let query = Query::all(vec![]);
        let projection = Projection::ALL;
        assert_eq!(
            a.count(namespace, &query, Budget::unlimited()),
            b.count(namespace, &query, Budget::unlimited())
        );
        let (mut ca, mut cb) = (None, None);
        loop {
            let x = a
                .query(
                    namespace,
                    &query,
                    &projection,
                    3,
                    ca.as_ref(),
                    Budget::unlimited(),
                )
                .unwrap();
            let y = b
                .query(
                    namespace,
                    &query,
                    &projection,
                    3,
                    cb.as_ref(),
                    Budget::unlimited(),
                )
                .unwrap();
            assert_eq!(x.value.rows, y.value.rows);
            assert_eq!(x.receipt, y.receipt);
            assert_eq!(x.physical, y.physical);
            ca = x.value.next;
            cb = y.value.next;
            assert_eq!(ca.is_some(), cb.is_some());
            if ca.is_none() {
                break;
            }
        }
        for i in 0..25 {
            assert_eq!(
                a.get_record(namespace, &key(i), &projection, Budget::unlimited()),
                b.get_record(namespace, &key(i), &projection, Budget::unlimited())
            );
        }
    }
    for value in [
        Value::str("book"),
        Value::U64(7),
        Value::Bool(true),
        Value::I64(-3),
        Value::str("absent"),
    ] {
        proof_parity(a, b, value);
    }
}

/// Drops every initial adapter Arc before calling reopen; disk tests really
/// close/reopen the file, while memory tests retain the backing reference store.
pub fn run_seeded_history(
    store: Arc<dyn VersionedStore>,
    mut reopen: impl FnMut() -> Arc<dyn VersionedStore>,
    seed: u64,
) {
    let mut durable = initialize(store.clone(), Options::default());
    let mut memory = Engine::new(PriceSchedule::FIXTURE);
    let mut rng = seed;
    for height in 1..=65u64 {
        let mut a = memory.begin(memory.head(), None).unwrap();
        let mut b = durable.begin(durable.head(), None).unwrap();
        if height == 1 {
            for _ in 0..2 {
                apply_pair(
                    &mut a,
                    &mut b,
                    WriteOp::CreateNamespace {
                        name: NamespaceName::new("project").unwrap(),
                    },
                );
            }
        }
        if height % 11 != 0 {
            for _ in 0..9 {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                let i = rng % 25;
                let namespace = ns(1 + rng % 2);
                let value = match rng % 4 {
                    0 => Value::str("book"),
                    1 => Value::U64(7),
                    2 => Value::Bool(true),
                    _ => Value::I64(-3),
                };
                let op = match (rng >> 8) % 6 {
                    0 => WriteOp::Put {
                        namespace,
                        key: i.to_be_bytes().to_vec(),
                        value: vec![(height % 256) as u8; 37],
                    },
                    1 => WriteOp::Delete {
                        namespace,
                        key: i.to_be_bytes().to_vec(),
                    },
                    2 | 3 => WriteOp::CreateRecord {
                        namespace,
                        key: key(i),
                        cells: vec![
                            (name("kind"), Cell::attribute(value)),
                            (name("payload"), Cell::field(Value::bytes(b"payload"))),
                        ],
                    },
                    4 => WriteOp::PatchRecord {
                        namespace,
                        key: key(i),
                        set: vec![
                            (name("kind"), Cell::attribute(value)),
                            (name("text"), Cell::field(Value::str("hello"))),
                        ],
                        remove: vec![name("payload")],
                    },
                    _ => WriteOp::DeleteRecord {
                        namespace,
                        key: key(i),
                    },
                };
                apply_pair(&mut a, &mut b, op);
            }
            let checkpoint_a = a.checkpoint();
            let checkpoint_b = b.checkpoint();
            apply_pair(
                &mut a,
                &mut b,
                WriteOp::Put {
                    namespace: ns(1),
                    key: b"rolled back".to_vec(),
                    value: vec![4; 10000],
                },
            );
            a.rollback_to(checkpoint_a).unwrap();
            b.rollback_to(checkpoint_b).unwrap();
            let hostkey = [height as u8; 32];
            assert_eq!(
                a.try_host_put(hostkey, &[height as u8; 31]).unwrap(),
                b.try_host_put(hostkey, &[height as u8; 31]).unwrap()
            );
            if height > 5 {
                let old = [(height - 5) as u8; 32];
                assert_eq!(
                    a.try_host_remove(&old).unwrap(),
                    b.try_host_remove(&old).unwrap()
                );
            }
        }
        if height == 40 {
            let next_owner = OwnerId::new([8; 20]);
            apply_pair(
                &mut a,
                &mut b,
                WriteOp::TransferNamespace {
                    namespace: ns(1),
                    expected_revision: Revision(0),
                    new_owner: next_owner,
                },
            );
            let op = WriteOp::TransferNamespace {
                namespace: ns(1),
                expected_revision: Revision(1),
                new_owner: OWNER,
            };
            assert_eq!(
                a.execute(next_owner, op.clone(), Budget::unlimited()),
                b.try_execute(next_owner, op, Budget::unlimited()).unwrap()
            );
        }
        if height == 30 {
            let price = PriceSchedule::uniform(2, 3);
            a.set_pricing(price.clone()).unwrap();
            b.set_pricing(price).unwrap();
        }
        let ca = a.seal();
        let cb = b.seal();
        assert_eq!(ca.receipts(), cb.receipts());
        assert_eq!(ca.spent(), cb.spent());
        memory.publish(&ca).unwrap();
        let before = store.stats();
        durable.prepare(cb).unwrap();
        assert_eq!(
            store.stats().since(&before).object_reads(),
            0,
            "preparation must not load committed nodes"
        );
        assert!(matches!(
            durable.commit().unwrap(),
            Publication::Committed(_)
        ));
        assert_eq!(
            memory.head_snapshot().state_root(),
            durable.head_snapshot().state_root()
        );
        assert!(
            durable.head_snapshot().staged_objects().is_empty(),
            "committed head must be stored roots"
        );
        if height % 11 == 0 {
            assert_eq!(durable.stats().last_preparation.unique_objects, 0);
        }
    }
    drop(durable);
    drop(store);
    for cap in [0, 64 * 1024, 8 * 1024 * 1024] {
        let store = reopen();
        let before = store.stats();
        let engine = DurableEngine::open(
            store.clone(),
            genesis(),
            Options {
                cache_bytes: cap,
                ..Options::default()
            },
        )
        .unwrap();
        let delta = store.stats().since(&before);
        assert_eq!(delta.object_reads(), 0);
        assert!(delta.record_reads <= 9);
        for h in [1, 2, 11, 30, 48, 65] {
            let id = SnapshotId::from_sequence(h);
            let before = store.stats();
            let snapshot = engine.snapshot(id).unwrap();
            assert_eq!(store.stats().since(&before).object_reads(), 0);
            compare(&memory.snapshot(id).unwrap(), &snapshot);
            compare(&memory.snapshot(id).unwrap(), &snapshot);
            assert!(snapshot.staged_objects().is_empty());
            assert!(engine.stats().cache.retained_bytes <= cap);
            assert!(engine.stats().cache.entries <= Options::default().cache_entries);
        }
    }
}
