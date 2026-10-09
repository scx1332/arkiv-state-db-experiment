//! Storage failures beneath engine state (node step N3b).
//!
//! Every engine map reads its committed nodes through a loader. A memory
//! engine never has a stored node; here a state is staged
//! (`Snapshot::staged_objects`), kept in a test object store, and reopened
//! from its state header with the hidden `Engine::from_state` test aid, so
//! every read loads nodes lazily through a reader this file controls: it
//! counts reads, fails every read after a chosen number with a chosen
//! error kind, and can lose or damage single objects.
//!
//! What this file establishes:
//! - A reopened state reads nothing at open, has the original state root,
//!   and answers every read (values, receipts **and** physical node
//!   visits) exactly as the memory engine that built it, while loading
//!   nodes on demand.
//! - On a snapshot, a failed, missing or corrupt node makes the read return
//!   the storage error (`Io`, `Full`, `Missing`, `Corrupt`), never an
//!   absent record, an empty result or a rejection; the snapshot answers
//!   again once the store heals. Failing the k-th node load of a read, for
//!   every k up to the loads it needs and with `Io` or `Full`, always
//!   yields that error.
//! - On a branch, a storage error at any read of an operation (every k, for
//!   raw and typed writes, namespace operations, host-state writes,
//!   estimates, reads and the expiry sweep) returns the error, records no
//!   receipt, adds nothing to the block's spend and poisons the branch:
//!   every later call returns a storage error even after the store heals,
//!   and `try_checkpoint`, `rollback_to`, `set_pricing`, `try_state_root`
//!   and `try_seal` return `EngineError::Poisoned`. An operation given
//!   enough reads succeeds with the receipt the memory engine issues.
//! - Leaves that decode but contradict the key they sit under (a catalog
//!   leaf describing another namespace, a key binding naming a row that
//!   holds another key) are `Corrupt`, and so is a row whose attribute no
//!   admitted write can hold (a `Bytes` value, or one over 64 bytes), for
//!   reads and for the deletes and patches that would index it.
//! - A state header of another format is `Incompatible`, and a map handle
//!   that no map has is `Corrupt`.
//!
//! The unit test `engine::tests` checks that a failed branch operation
//! leaves the branch's state as it was before the operation. What this file
//! does not establish: durability, caching or manifests (node step N3c).

#[path = "../../arkiv-db-trie/tests/support/mod.rs"]
mod trie_support;

use std::{
    collections::HashMap,
    ops::Bound,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use alloy_primitives::{B256, keccak256};
use alloy_trie::EMPTY_ROOT_HASH;
use arkiv_db::{
    BlockNumber, Branch, Budget, Cell, CellName, Engine, EngineError, Limits, MapKind, NamespaceId,
    NamespaceName, OwnerId, PriceSchedule, Projection, Query, RecordKey, Revision, Snapshot,
    StagedObject, StateHeader, StorageError, StorageErrorKind, Term, TypeTag, Value, WriteOp,
    try_expire_records,
};
use arkiv_db_storage::versioned::{ObjectId, ObjectReader, SpaceId};
use trie_support::reference;

const ALICE: OwnerId = OwnerId::new([0xa1; 20]);
const BOB: OwnerId = OwnerId::new([0xb0; 20]);

// ---------------------------------------------------------------------------
// The test object store
// ---------------------------------------------------------------------------

/// Objects by space and id, with read counting and fault injection.
struct TestStore {
    objects: Mutex<HashMap<(SpaceId, ObjectId), Vec<u8>>>,
    reads: AtomicU64,
    /// Reads that may still succeed; `u64::MAX` means no limit.
    allowance: AtomicU64,
    /// The kind of the injected failure.
    kind: Mutex<StorageErrorKind>,
}

impl TestStore {
    fn new(objects: Vec<StagedObject>) -> Arc<Self> {
        Arc::new(Self {
            objects: Mutex::new(
                objects
                    .into_iter()
                    .map(|object| ((object.space, object.id), object.bytes))
                    .collect(),
            ),
            reads: AtomicU64::new(0),
            allowance: AtomicU64::new(u64::MAX),
            kind: Mutex::new(StorageErrorKind::Io),
        })
    }

    fn add(&self, objects: Vec<StagedObject>) {
        let mut stored = self.objects.lock().unwrap();
        for object in objects {
            assert_eq!(
                keccak256(&object.bytes).0,
                object.id,
                "objects are content addressed"
            );
            stored.insert((object.space, object.id), object.bytes);
        }
    }

    fn reads(&self) -> u64 {
        self.reads.load(Ordering::SeqCst)
    }

    /// Every read after the next `reads` fails with `kind`.
    fn fail_after(&self, reads: u64, kind: StorageErrorKind) {
        *self.kind.lock().unwrap() = kind;
        self.allowance.store(reads, Ordering::SeqCst);
    }

    fn heal(&self) {
        self.allowance.store(u64::MAX, Ordering::SeqCst);
    }
}

impl ObjectReader for TestStore {
    fn read_object(&self, space: SpaceId, id: &ObjectId) -> Result<Option<Vec<u8>>, StorageError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        let allowed = self
            .allowance
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| match left {
                0 => None,
                u64::MAX => Some(u64::MAX),
                left => Some(left - 1),
            });
        if allowed.is_err() {
            let kind = *self.kind.lock().unwrap();
            return Err(StorageError::new(kind, "injected by the test store"));
        }
        Ok(self.objects.lock().unwrap().get(&(space, *id)).cloned())
    }

    fn read_object_bounded(
        &self,
        space: SpaceId,
        id: &ObjectId,
        _max_bytes: usize,
    ) -> Result<Option<Vec<u8>>, StorageError> {
        self.read_object(space, id)
    }
}

// ---------------------------------------------------------------------------
// The fixture: a small state built in memory, then reopened from its objects
// ---------------------------------------------------------------------------

fn ns(ordinal: u64) -> NamespaceId {
    NamespaceId::from_ordinal(ordinal).unwrap()
}

fn name(text: &str) -> CellName {
    CellName::new(text).unwrap()
}

fn key(bytes: &[u8]) -> RecordKey {
    RecordKey::new(bytes).unwrap()
}

fn record(namespace: u64, record: &[u8], kind: &str, height: u64) -> WriteOp {
    WriteOp::CreateRecord {
        namespace: ns(namespace),
        key: key(record),
        cells: vec![
            (name("kind"), Cell::attribute(Value::str(kind))),
            (name("h"), Cell::attribute(Value::U64(height))),
            (name("note"), Cell::field(Value::bytes(b"note"))),
        ],
    }
}

fn put(namespace: u64, raw: &[u8], value: &[u8]) -> WriteOp {
    WriteOp::Put {
        namespace: ns(namespace),
        key: raw.to_vec(),
        value: value.to_vec(),
    }
}

/// Two namespaces with raw and typed records and two host entries.
fn memory_engine() -> Engine {
    let mut engine = Engine::new(PriceSchedule::FIXTURE);
    let mut branch = engine.begin(engine.head(), None).unwrap();
    let unlimited = Budget::unlimited();
    for text in ["alpha", "beta"] {
        let create = WriteOp::CreateNamespace {
            name: NamespaceName::new(text).unwrap(),
        };
        branch.execute(ALICE, create, unlimited).unwrap();
    }
    for index in 0u8..12 {
        let raw = [b'k', b'0' + index / 10, b'0' + index % 10];
        branch
            .execute(ALICE, put(1, &raw, &[index; 3]), unlimited)
            .unwrap();
        let kind = ["red", "rose", "ruby"][usize::from(index % 3)];
        let op = record(1, &[b'r', index], kind, u64::from(index % 4) + 2);
        branch.execute(ALICE, op, unlimited).unwrap();
    }
    branch
        .execute(ALICE, record(2, b"other", "red", 3), unlimited)
        .unwrap();
    branch.try_host_put([1; 32], b"due").unwrap().unwrap();
    branch.try_host_put([2; 32], b"total").unwrap().unwrap();
    engine.publish(&branch.seal()).unwrap();
    engine
}

/// The memory engine's head, staged into a fresh store and reopened.
fn reopened() -> (Engine, Engine, Arc<TestStore>) {
    let memory = memory_engine();
    let head = memory.head_snapshot();
    let store = TestStore::new(head.staged_objects());
    let engine = Engine::from_state(&head.state_header(), Limits::DEFAULT, store.clone()).unwrap();
    (memory, engine, store)
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

/// A read of a snapshot, returning its outcome as debug text.
type SnapshotRead = fn(&Snapshot) -> Result<String, StorageError>;

fn text<T: std::fmt::Debug>(result: Result<T, StorageError>) -> Result<String, StorageError> {
    result.map(|value| format!("{value:?}"))
}

fn traced() -> Budget {
    Budget::unlimited().traced()
}

fn red() -> Query {
    Query::term(Term::eq(name("kind"), Value::str("red")))
}

fn mixed() -> Query {
    Query {
        groups: vec![
            vec![arkiv_db::Literal::of(Term::starts_with(name("kind"), "r"))],
            vec![arkiv_db::Literal::not(Term::range(
                name("h"),
                Bound::Included(Value::U64(3)),
                Bound::Unbounded,
            ))],
        ],
    }
}

/// Every metered read of the fixture and the statistics, traced, so the
/// debug text holds values, receipts with ledgers and physical traces.
fn snapshot_reads() -> Vec<(&'static str, SnapshotRead)> {
    vec![
        ("get", |s| text(s.try_get(ns(1), b"k07", traced()))),
        ("get absent", |s| text(s.try_get(ns(1), b"k99", traced()))),
        ("scan", |s| text(s.try_scan_prefix(ns(1), b"k0", traced()))),
        ("descriptor", |s| text(s.try_descriptor(ns(2), traced()))),
        ("get record", |s| {
            text(s.try_get_record(ns(1), &key(&[b'r', 4]), &Projection::ALL, traced()))
        }),
        ("query", |s| {
            text(s.try_query(ns(1), &red(), &Projection::KEY_ONLY, 5, None, traced()))
        }),
        ("mixed query", |s| {
            text(s.try_query(ns(1), &mixed(), &Projection::ALL, 20, None, traced()))
        }),
        ("count", |s| text(s.try_count(ns(1), &mixed(), traced()))),
        ("terms", |s| {
            text(s.try_terms(
                ns(1),
                &name("h"),
                TypeTag::U64,
                Bound::Unbounded,
                10,
                traced(),
            ))
        }),
        ("host get", |s| text(s.try_host_get(&[2; 32]))),
        ("host range", |s| text(s.try_host_range(&[0; 32], 10))),
        ("record count", |s| text(s.try_record_count(ns(1)))),
        ("term count", |s| text(s.try_term_count(ns(1)))),
        ("index consistent", |s| text(s.try_index_consistent(ns(1)))),
    ]
}

fn reads(snapshot: &Snapshot) -> Vec<Result<String, StorageError>> {
    snapshot_reads()
        .into_iter()
        .map(|(_, read)| read(snapshot))
        .collect()
}

#[test]
fn a_state_reopened_from_its_objects_reads_the_same_and_loads_lazily() {
    let (memory, engine, store) = reopened();
    assert_eq!(store.reads(), 0, "opening reads nothing");
    let original = memory.head_snapshot();
    let snapshot = engine.head_snapshot();
    assert_eq!(snapshot.state_root(), original.state_root());
    assert_eq!(snapshot.state_header(), original.state_header());
    assert_eq!(store.reads(), 0, "the root needs no read");
    // Values, receipts and physical node visits: every Debug field.
    let expected = reads(&original);
    assert!(expected.iter().all(Result::is_ok));
    assert_eq!(reads(&snapshot), expected);
    let after_one_pass = store.reads();
    assert!(after_one_pass > 0, "reads load nodes");
    // Nothing is cached in the handles: a second pass loads again, and
    // answers (visits included) the same.
    assert_eq!(reads(&snapshot), expected);
    assert_eq!(store.reads(), 2 * after_one_pass);
    // Nothing of a memory state is from the store; nothing of an unchanged
    // reopened state needs staging.
    assert!(snapshot.staged_objects().is_empty());
}

#[test]
fn snapshot_reads_fail_with_storage_errors_and_recover_after_the_store_heals() {
    let (memory, engine, store) = reopened();
    let snapshot = engine.head_snapshot();
    let expected = reads(&memory.head_snapshot());
    for kind in [StorageErrorKind::Io, StorageErrorKind::Full] {
        store.fail_after(0, kind);
        for (index, got) in reads(&snapshot).into_iter().enumerate() {
            // Statistics that need no node (none here) could still answer;
            // every read below touches the catalog.
            let error = got.expect_err("every read loads a node first");
            assert_eq!(error.kind, kind, "read {index}");
        }
        store.heal();
        assert_eq!(reads(&snapshot), expected, "the snapshot is not poisoned");
    }
    // Failing the k-th read of each read, for every k below what it needs
    // and with either kind, is that error, never another answer.
    for ((what, read), want) in snapshot_reads().into_iter().zip(&expected) {
        let before = store.reads();
        assert_eq!(&read(&snapshot), want, "{what}");
        let needed = store.reads() - before;
        assert!(needed > 0, "{what} loads nodes");
        for kind in [StorageErrorKind::Io, StorageErrorKind::Full] {
            for allowance in 0..needed {
                store.fail_after(allowance, kind);
                let got = read(&snapshot);
                store.heal();
                assert_eq!(
                    got.map_err(|error| error.kind),
                    Err(kind),
                    "{what} failing after {allowance} of {needed} loads"
                );
            }
        }
    }
}

#[test]
fn missing_and_corrupt_objects_are_availability_errors() {
    let memory = memory_engine();
    let head = memory.head_snapshot();
    let objects = head.staged_objects();
    let header = head.state_header();
    let traced = Budget::unlimited().traced();
    // Namespace 2's objects: its row map is one leaf (one record).
    let ns2: Vec<_> = objects
        .iter()
        .filter(|object| object.space == SpaceId::Namespace(2))
        .map(|object| (object.space, object.id, object.bytes.clone()))
        .collect();
    assert!(!ns2.is_empty());
    for (space, id, bytes) in ns2 {
        for damage in ["missing", "corrupt"] {
            let store = TestStore::new(objects.clone());
            {
                let mut stored = store.objects.lock().unwrap();
                match damage {
                    "missing" => {
                        stored.remove(&(space, id));
                    }
                    _ => {
                        let mut damaged = bytes.clone();
                        let last = damaged.len() - 1;
                        damaged[last] ^= 1;
                        stored.insert((space, id), damaged);
                    }
                }
            }
            let engine = Engine::from_state(&header, Limits::DEFAULT, store.clone()).unwrap();
            let snapshot = engine.head_snapshot();
            // Namespace 1 never reads namespace 2's objects.
            assert_eq!(reads(&snapshot), reads(&memory.head_snapshot()));
            // A full walk of namespace 2 reaches the damaged object.
            let walk = snapshot.try_query(
                ns(2),
                &Query::match_all(),
                &Projection::ALL,
                10,
                None,
                traced,
            );
            let consistent = snapshot.try_index_consistent(ns(2));
            let failures: Vec<StorageErrorKind> = [walk.err(), consistent.err()]
                .into_iter()
                .flatten()
                .map(|error| error.kind)
                .collect();
            let kind = match damage {
                "missing" => StorageErrorKind::Missing,
                _ => StorageErrorKind::Corrupt,
            };
            assert!(
                !failures.is_empty() && failures.iter().all(|got| *got == kind),
                "{damage} {id:02x?}: {failures:?}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Branches
// ---------------------------------------------------------------------------

/// An operation on a branch, returning its outcome as debug text.
type BranchCall = fn(&mut Branch) -> Result<String, StorageError>;

fn calls() -> Vec<(&'static str, BranchCall)> {
    vec![
        ("put", |branch| {
            branch
                .try_execute(ALICE, put(1, b"k03", b"new"), Budget::unlimited().traced())
                .map(|r| format!("{r:?}"))
        }),
        ("delete", |branch| {
            let op = WriteOp::Delete {
                namespace: ns(1),
                key: b"k04".to_vec(),
            };
            branch
                .try_execute(ALICE, op, Budget::unlimited().traced())
                .map(|r| format!("{r:?}"))
        }),
        ("create record", |branch| {
            branch
                .try_execute(
                    ALICE,
                    record(1, b"new", "rust", 9),
                    Budget::unlimited().traced(),
                )
                .map(|r| format!("{r:?}"))
        }),
        ("patch record", |branch| {
            let op = WriteOp::PatchRecord {
                namespace: ns(1),
                key: key(&[b'r', 5]),
                set: vec![(name("kind"), Cell::attribute(Value::I64(-1)))],
                remove: vec![name("h")],
            };
            branch
                .try_execute(ALICE, op, Budget::unlimited().traced())
                .map(|r| format!("{r:?}"))
        }),
        ("delete record (host)", |branch| {
            let op = WriteOp::DeleteRecord {
                namespace: ns(1),
                key: key(&[b'r', 6]),
            };
            branch
                .try_execute_host(op, Budget::unlimited().traced())
                .map(|r| format!("{r:?}"))
        }),
        ("create namespace", |branch| {
            let op = WriteOp::CreateNamespace {
                name: NamespaceName::new("gamma").unwrap(),
            };
            branch
                .try_execute(BOB, op, Budget::unlimited().traced())
                .map(|r| format!("{r:?}"))
        }),
        ("transfer", |branch| {
            let op = WriteOp::TransferNamespace {
                namespace: ns(2),
                expected_revision: Revision(0),
                new_owner: BOB,
            };
            branch
                .try_execute(ALICE, op, Budget::unlimited().traced())
                .map(|r| format!("{r:?}"))
        }),
        ("unauthorized put", |branch| {
            branch
                .try_execute(BOB, put(1, b"k", b"v"), Budget::unlimited().traced())
                .map(|r| format!("{r:?}"))
        }),
        ("estimate", |branch| {
            branch
                .try_estimate(
                    ALICE,
                    record(1, b"est", "red", 2),
                    Budget::unlimited().traced(),
                )
                .map(|r| format!("{r:?}"))
        }),
        ("host put", |branch| {
            branch
                .try_host_put([1; 32], b"changed")
                .map(|r| format!("{r:?}"))
        }),
        ("host remove", |branch| {
            branch.try_host_remove(&[2; 32]).map(|r| format!("{r:?}"))
        }),
        ("query", |branch| {
            let query = Query::term(Term::starts_with(name("kind"), "ru"));
            branch
                .try_query(
                    ns(1),
                    &query,
                    &Projection::ALL,
                    10,
                    None,
                    Budget::unlimited(),
                )
                .map(|r| format!("{r:?}"))
        }),
        ("expiry sweep", |branch| {
            try_expire_records(
                branch,
                ns(1),
                &name("h"),
                BlockNumber(3),
                Budget::unlimited(),
                Budget::unlimited(),
            )
            .map(|r| format!("{r:?}"))
        }),
    ]
}

/// Every later call on a poisoned branch fails, whatever the store does.
fn assert_poisoned(branch: &mut Branch, context: &str) {
    assert!(branch.is_poisoned(), "{context}");
    for (what, call) in calls() {
        let error = call(branch).expect_err(what);
        assert!(
            error.detail.contains("poisoned"),
            "{context}: {what}: {error}"
        );
    }
    assert!(branch.try_get(ns(1), b"k01", Budget::unlimited()).is_err());
    assert!(branch.try_namespace_count().is_err());
    assert_eq!(
        branch.try_checkpoint(),
        Err(EngineError::Poisoned),
        "{context}"
    );
    assert_eq!(
        branch.try_state_root(),
        Err(EngineError::Poisoned),
        "{context}"
    );
    assert_eq!(
        branch.set_pricing(PriceSchedule::FIXTURE.with_id(9)),
        Err(EngineError::Poisoned),
        "{context}"
    );
}

#[test]
fn a_storage_error_poisons_the_branch_and_records_nothing() {
    let (memory, engine, store) = reopened();
    for (what, call) in calls() {
        // What the memory engine does: the reference outcome.
        let mut reference = memory.begin(memory.head(), None).unwrap();
        let want = call(&mut reference).expect("memory engines never fail");
        let want_receipts = reference.receipts().to_vec();
        let want_spent = reference.spent();

        // Enough reads: the same outcome, receipts included.
        let mut branch = engine.begin(engine.head(), None).unwrap();
        let before = store.reads();
        assert_eq!(call(&mut branch).as_ref(), Ok(&want), "{what}");
        let needed = store.reads() - before;
        assert!(needed > 0, "{what} loads nodes");
        assert_eq!(branch.receipts(), want_receipts, "{what}");
        assert_eq!(branch.spent(), want_spent, "{what}");
        assert!(branch.try_seal().is_ok(), "{what}");

        // One read short at every point: an error and a poisoned branch.
        for allowance in 0..needed {
            let mut branch = engine.begin(engine.head(), None).unwrap();
            let checkpoint = branch.try_checkpoint().unwrap();
            store.fail_after(allowance, StorageErrorKind::Io);
            let error = call(&mut branch).expect_err(what);
            store.heal();
            let context = format!("{what} failing after {allowance} of {needed} reads");
            assert_eq!(error.kind, StorageErrorKind::Io, "{context}");
            // The failed operation recorded nothing and spent nothing. A
            // call made of several operations (the expiry sweep) keeps the
            // receipts of those that completed before it, as the memory
            // engine issued them; the poisoned branch can never be sealed.
            let kept = branch.receipts();
            assert!(kept.len() <= want_receipts.len(), "{context}");
            assert_eq!(kept, &want_receipts[..kept.len()], "{context}");
            let kept_spent: u64 = kept.iter().map(|receipt| receipt.spent.units()).sum();
            assert_eq!(branch.spent().units(), kept_spent, "{context}: spend");
            if what != "expiry sweep" {
                assert!(kept.is_empty(), "{context}: a single operation");
            }
            assert_poisoned(&mut branch, &context);
            assert_eq!(
                branch.rollback_to(checkpoint),
                Err(EngineError::Poisoned),
                "{context}"
            );
            assert_eq!(branch.try_seal().err(), Some(EngineError::Poisoned));
        }
    }
    // The published state is untouched by all of it.
    assert_eq!(
        engine.head_snapshot().state_root(),
        memory.head_snapshot().state_root()
    );
}

// ---------------------------------------------------------------------------
// Leaves that contradict their keys, and invalid headers
// ---------------------------------------------------------------------------

/// The encoded single-leaf catalog node for 8-byte key `id` and `value`:
/// `[hex_prefix(16 nibbles, leaf), value]`.
fn single_leaf(id: u64, value: &[u8]) -> Vec<u8> {
    let mut path = vec![0x20];
    path.extend(id.to_be_bytes());
    reference::list(&[reference::string(&path), reference::string(value)])
}

/// A catalog leaf for namespace `descriptor_id` with the given map handles.
fn catalog_value(descriptor_id: u64, next_record: u64, handles: [(B256, u64); 4]) -> Vec<u8> {
    let mut leaf = descriptor_id.to_be_bytes().to_vec();
    leaf.extend(5u32.to_be_bytes());
    leaf.extend(b"alpha");
    leaf.extend(ALICE.as_bytes());
    leaf.extend(0u64.to_be_bytes());
    leaf.extend(next_record.to_be_bytes());
    for (root, len) in handles {
        leaf.extend(root.0);
        leaf.extend(len.to_be_bytes());
    }
    leaf
}

/// A state with one catalog entry under key `id` holding `value`.
fn crafted(store: &Arc<TestStore>, id: u64, value: Vec<u8>) -> Result<Engine, StorageError> {
    let node = single_leaf(id, &value);
    let root = keccak256(&node);
    store.add(vec![StagedObject {
        space: SpaceId::Catalog,
        map: MapKind::Catalog,
        id: root.0,
        bytes: node,
    }]);
    let header = StateHeader {
        format: arkiv_db::STATE_FORMAT,
        catalog_root: root,
        catalog_len: 1,
        next_namespace: id + 1,
        pricing: PriceSchedule::FIXTURE,
        host_root: EMPTY_ROOT_HASH,
        host_len: 0,
    };
    Engine::from_state(&header, Limits::DEFAULT, store.clone())
}

/// One namespace holding records created in `order`, the first deleted
/// again, so the survivor has id 2.
fn one_survivor(first: &[u8], survivor: &[u8]) -> Snapshot {
    let mut engine = Engine::new(PriceSchedule::FIXTURE);
    let mut branch = engine.begin(engine.head(), None).unwrap();
    let create = WriteOp::CreateNamespace {
        name: NamespaceName::new("alpha").unwrap(),
    };
    branch.execute(ALICE, create, Budget::unlimited()).unwrap();
    for bytes in [first, survivor] {
        branch
            .execute(ALICE, record(1, bytes, "red", 2), Budget::unlimited())
            .unwrap();
    }
    let delete = WriteOp::DeleteRecord {
        namespace: ns(1),
        key: key(first),
    };
    branch.execute(ALICE, delete, Budget::unlimited()).unwrap();
    engine.publish(&branch.seal()).unwrap();
    engine.head_snapshot()
}

#[test]
fn leaves_that_contradict_their_keys_are_corrupt() {
    let traced = Budget::unlimited().traced();
    // `a` bound to id 2 in one state; id 2's row holds `b` in another.
    let with_a = one_survivor(b"x", b"a");
    let with_b = one_survivor(b"x", b"b");
    let roots_a = with_a.try_namespace_roots(ns(1)).unwrap().unwrap();
    let roots_b = with_b.try_namespace_roots(ns(1)).unwrap().unwrap();
    let store = TestStore::new(with_a.staged_objects());
    store.add(with_b.staged_objects());

    // A catalog leaf describing namespace 1, stored under key 2.
    let honest = catalog_value(
        1,
        3,
        [roots_a.records, roots_a.keys, roots_a.rows, roots_a.terms],
    );
    let engine = crafted(&store, 2, honest.clone()).unwrap();
    let snapshot = engine.head_snapshot();
    for error in [
        snapshot.try_descriptor(ns(2), traced).err(),
        snapshot.try_get(ns(2), b"k", traced).err(),
        snapshot.try_record_count(ns(2)).err(),
    ] {
        assert_eq!(
            error.map(|error| error.kind),
            Some(StorageErrorKind::Corrupt)
        );
    }
    // The same leaf under its own key reads fine.
    let engine = crafted(&store, 1, honest).unwrap();
    let got = engine
        .head_snapshot()
        .try_get_record(ns(1), &key(b"a"), &Projection::KEY_ONLY, traced)
        .unwrap()
        .unwrap();
    assert_eq!(got.value.unwrap().key, Some(key(b"a")));

    // `a`'s binding (id 2) from one state, the rows (id 2 holds `b`) from
    // the other: the row read by key holds another key.
    let mixed = catalog_value(
        1,
        3,
        [roots_a.records, roots_a.keys, roots_b.rows, roots_a.terms],
    );
    let engine = crafted(&store, 1, mixed).unwrap();
    let error = engine
        .head_snapshot()
        .try_get_record(ns(1), &key(b"a"), &Projection::KEY_ONLY, traced)
        .unwrap_err();
    assert_eq!(error.kind, StorageErrorKind::Corrupt, "{error}");

    // A leaf whose key and row counts differ does not decode.
    let (root, _) = roots_a.keys;
    let uneven = catalog_value(
        1,
        3,
        [roots_a.records, (root, 2), roots_a.rows, roots_a.terms],
    );
    let engine = crafted(&store, 1, uneven).unwrap();
    let error = engine
        .head_snapshot()
        .try_descriptor(ns(1), traced)
        .unwrap_err();
    assert_eq!(error.kind, StorageErrorKind::Corrupt, "{error}");
}

/// A row leaf for key `a` with one cell `c` of kind `kind` (1 attribute, 2
/// field) holding `value` (`tag ‖ payload`), encoded by hand: `u8 len ‖
/// key ‖ u32 cell count ‖ (u32 len ‖ name ‖ kind ‖ value)`.
fn row_value(kind: u8, value: &[u8]) -> Vec<u8> {
    let mut leaf = vec![1, b'a'];
    leaf.extend(1u32.to_be_bytes());
    leaf.extend(1u32.to_be_bytes());
    leaf.push(b'c');
    leaf.push(kind);
    leaf.extend(value);
    leaf
}

#[test]
fn rows_whose_attributes_cannot_be_indexed_are_corrupt() {
    let traced = Budget::unlimited().traced();
    // An honest namespace with one record `a` (id 1) holding only a field,
    // so its term map is empty; its row map is replaced below.
    let mut engine = Engine::new(PriceSchedule::FIXTURE);
    let mut branch = engine.begin(engine.head(), None).unwrap();
    let create = WriteOp::CreateNamespace {
        name: NamespaceName::new("alpha").unwrap(),
    };
    branch.execute(ALICE, create, Budget::unlimited()).unwrap();
    let op = WriteOp::CreateRecord {
        namespace: ns(1),
        key: key(b"a"),
        cells: vec![(name("c"), Cell::field(Value::bytes(b"b")))],
    };
    branch.execute(ALICE, op, Budget::unlimited()).unwrap();
    engine.publish(&branch.seal()).unwrap();
    let honest = engine.head_snapshot();
    let roots = honest.try_namespace_roots(ns(1)).unwrap().unwrap();
    assert_eq!(roots.terms, (EMPTY_ROOT_HASH, 0));

    let bytes_value = [&[3u8, 0, 0, 0, 1][..], b"b"].concat();
    let length = Value::MAX_INDEXED_LEN + 1;
    let long_str = [
        &[2u8][..],
        &(length as u32).to_be_bytes(),
        &vec![b'x'; length],
    ]
    .concat();
    let cases = [
        ("bytes field", 2, bytes_value.clone(), true),
        ("long str field", 2, long_str.clone(), true),
        ("bytes attribute", 1, bytes_value, false),
        ("long str attribute", 1, long_str, false),
    ];
    for (what, kind, value, readable) in cases {
        let store = TestStore::new(honest.staged_objects());
        // A single-leaf row map: id 1 → the crafted row.
        let node = single_leaf(1, &row_value(kind, &value));
        let rows_root = keccak256(&node);
        store.add(vec![StagedObject {
            space: SpaceId::Namespace(1),
            map: MapKind::Rows,
            id: rows_root.0,
            bytes: node,
        }]);
        let leaf = catalog_value(
            1,
            2,
            [roots.records, roots.keys, (rows_root, 1), roots.terms],
        );
        let engine = crafted(&store, 1, leaf).unwrap();
        let snapshot = engine.head_snapshot();
        let read = snapshot.try_get_record(ns(1), &key(b"a"), &Projection::ALL, traced);
        if readable {
            // The construction is sound: as a field, the same value reads.
            let row = read.unwrap().unwrap().value.unwrap();
            assert_eq!(row.key, Some(key(b"a")), "{what}");
            continue;
        }
        assert_eq!(
            read.map(|_| ()).map_err(|error| error.kind),
            Err(StorageErrorKind::Corrupt),
            "{what}"
        );
        // Deleting or patching the record would build the attribute's
        // terms: the row load fails first, and the branch is poisoned.
        let writes = [
            WriteOp::DeleteRecord {
                namespace: ns(1),
                key: key(b"a"),
            },
            WriteOp::PatchRecord {
                namespace: ns(1),
                key: key(b"a"),
                set: vec![(name("d"), Cell::attribute(Value::U64(1)))],
                remove: vec![name("c")],
            },
        ];
        for op in writes {
            let mut branch = engine.begin(engine.head(), None).unwrap();
            let error = branch.try_execute(ALICE, op, traced).unwrap_err();
            assert_eq!(error.kind, StorageErrorKind::Corrupt, "{what}: {error}");
            assert!(branch.is_poisoned(), "{what}");
            assert!(branch.receipts().is_empty(), "{what}");
        }
    }
}

#[test]
fn invalid_state_headers_are_refused() {
    let head = memory_engine().head_snapshot();
    let store = TestStore::new(head.staged_objects());
    let mut header = head.state_header();
    header.format = 999;
    let error = Engine::from_state(&header, Limits::DEFAULT, store.clone()).unwrap_err();
    assert_eq!(error.kind, StorageErrorKind::Incompatible);
    let mut header = head.state_header();
    header.host_len = 0;
    let error = Engine::from_state(&header, Limits::DEFAULT, store.clone()).unwrap_err();
    assert_eq!(error.kind, StorageErrorKind::Corrupt);
    let mut header = head.state_header();
    header.catalog_root = EMPTY_ROOT_HASH;
    let error = Engine::from_state(&header, Limits::DEFAULT, store).unwrap_err();
    assert_eq!(error.kind, StorageErrorKind::Corrupt);
    // The header codec refuses the same shapes.
    let mut header = head.state_header();
    header.catalog_len = 0;
    let bytes = arkiv_db::encode(&header);
    assert!(arkiv_db::decode_exact::<StateHeader>(&bytes).is_err());
    let honest = head.state_header();
    assert_eq!(
        arkiv_db::decode_exact::<StateHeader>(&arkiv_db::encode(&honest)),
        Ok(honest)
    );
}
