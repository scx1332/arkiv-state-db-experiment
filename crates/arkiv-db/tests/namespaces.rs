//! Namespace identity, aliasing, ownership and authorization at the engine
//! boundary.
//!
//! Every expected cost below is a literal computed by hand from the FIXTURE
//! weights (DirectoryLookup 1, NamespaceCreate 50, NamespaceTransfer 20,
//! PutRecord 10, DeleteRecord 5, WriteKeyByte 2, WriteValueByte 1, PointRead 3,
//! ScanSeek 4, ScanEntry 2, ReadByte 1, RootUpdate 2) and the model-3 charge
//! points:
//!
//! - create: 1 + 50 + RootUpdate 1 * 2 = 53
//! - transfer: 1 + 20 + RootUpdate 1 * 2 = 23
//! - put: 1 + 10 + 2 * key.len() + value.len() + RootUpdate 2 * 2, i.e.
//!   15 + 2 * key.len() + value.len()
//! - delete: 1 + 5 + 2 * key.len() + RootUpdate 2 * 2
//! - get: 1 + 3 (+ value.len() when present), so a miss costs 4
//! - scan: 1 + 4 + sum over matches of (2 + key.len() + value.len())
//! - descriptor: 1
//! - any UnknownNamespace / Unauthorized / StaleRevision / DirectoryFull
//!   rejection: exactly the directory charge, 1
//!
//! Receipts carry model version 3 and FIXTURE's schedule id 1.

use arkiv_db::{
    Branch, Budget, Cost, Effect, Engine, EngineError, Entry, Executed, Ledger, LedgerEntry,
    NamespaceDescriptor, NamespaceId, OperationClass, Outcome, OwnerId, PriceSchedule, ReadResult,
    Receipt, ReceiptKind, Rejection, Revision, Snapshot, SnapshotId, WriteOp, WriteResult,
};

const ALICE: OwnerId = OwnerId::new([0xa1; 20]);
const BOB: OwnerId = OwnerId::new([0xb0; 20]);
const CAROL: OwnerId = OwnerId::new([0xca; 20]);
const DAVE: OwnerId = OwnerId::new([0xda; 20]);

const CREATE_COST: u64 = 53;
const TRANSFER_COST: u64 = 23;
const DIRECTORY_CHARGE: u64 = 1;
const MISSING_KEY_COST: u64 = 4;
const EMPTY_SCAN_COST: u64 = 5;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn ns(ordinal: u64) -> NamespaceId {
    NamespaceId::from_bytes(ordinal.to_be_bytes()).expect("ordinal is non-zero")
}

fn descriptor(id: NamespaceId, owner: OwnerId, revision: u64) -> NamespaceDescriptor {
    NamespaceDescriptor {
        name: arkiv_db::NamespaceName::new("test").unwrap(),
        id,
        owner,
        revision: Revision(revision),
    }
}

fn entry(key: &[u8], value: &[u8]) -> Entry {
    Entry {
        key: key.to_vec(),
        value: value.to_vec(),
    }
}

fn receipt(kind: ReceiptKind, spent: u64, outcome: Outcome) -> Receipt {
    Receipt {
        kind,
        model_version: 3,
        schedule_id: 1,
        spent: Cost::new(spent),
        outcome,
        ledger: None,
    }
}

fn applied(spent: u64) -> Receipt {
    receipt(ReceiptKind::Write, spent, Outcome::Applied)
}

fn rejected(spent: u64, reason: Rejection) -> Receipt {
    receipt(ReceiptKind::Write, spent, Outcome::Rejected(reason))
}

fn read(spent: u64) -> Receipt {
    receipt(ReceiptKind::Read, spent, Outcome::Applied)
}

fn read_rejected(spent: u64, reason: Rejection) -> Receipt {
    receipt(ReceiptKind::Read, spent, Outcome::Rejected(reason))
}

/// A write refused right after the directory charge: spent exactly 1.
fn refused(reason: Rejection) -> Result<(Effect, Receipt), (Rejection, Receipt)> {
    Err((reason, rejected(DIRECTORY_CHARGE, reason)))
}

/// A read refused right after the directory charge: spent exactly 1.
fn read_refused<T>(reason: Rejection) -> Result<(T, Receipt), (Rejection, Receipt)> {
    Err((reason, read_rejected(DIRECTORY_CHARGE, reason)))
}

fn stale(expected: u64, current: u64) -> Rejection {
    Rejection::StaleRevision {
        expected: Revision(expected),
        current: Revision(current),
    }
}

/// Attaches a ledger of `(class, count, unit_price, subtotal)` lines.
fn traced(mut receipt: Receipt, lines: &[(OperationClass, u64, u64, u64)]) -> Receipt {
    receipt.ledger = Some(Ledger {
        entries: lines
            .iter()
            .map(|&(class, count, unit_price, subtotal)| LedgerEntry {
                class,
                count,
                unit_price,
                subtotal: Cost::new(subtotal),
            })
            .collect(),
    });
    receipt
}

fn put(namespace: NamespaceId, key: &[u8], value: &[u8]) -> WriteOp {
    WriteOp::Put {
        namespace,
        key: key.to_vec(),
        value: value.to_vec(),
    }
}

fn delete(namespace: NamespaceId, key: &[u8]) -> WriteOp {
    WriteOp::Delete {
        namespace,
        key: key.to_vec(),
    }
}

fn transfer(namespace: NamespaceId, expected_revision: u64, new_owner: OwnerId) -> WriteOp {
    WriteOp::TransferNamespace {
        namespace,
        expected_revision: Revision(expected_revision),
        new_owner,
    }
}

/// The canonical part of a write outcome: the physical trace is dropped so
/// whole outcomes can be compared with `==`.
fn logical(result: WriteResult) -> Result<(Effect, Receipt), (Rejection, Receipt)> {
    result
        .map(|executed| (executed.effect, executed.receipt))
        .map_err(|rejected| (rejected.reason, rejected.receipt))
}

/// The canonical part of a read outcome.
fn value<T>(result: ReadResult<T>) -> Result<(T, Receipt), (Rejection, Receipt)> {
    result
        .map(|metered| (metered.value, metered.receipt))
        .map_err(|rejected| (rejected.reason, rejected.receipt))
}

fn begin(engine: &Engine) -> Branch {
    engine
        .begin(engine.head(), None)
        .expect("the branch starts at the current head")
}

fn publish(engine: &mut Engine, branch: Branch) -> Snapshot {
    let sealed = branch.seal();
    let id = engine
        .publish(&sealed)
        .expect("the candidate is based on head");
    engine.snapshot(id).expect("a published snapshot resolves")
}

/// Creates a namespace for `actor`, checking the canonical receipt.
fn create(branch: &mut Branch, actor: OwnerId) -> NamespaceId {
    match branch.execute(
        actor,
        WriteOp::CreateNamespace {
            name: arkiv_db::NamespaceName::new("test").unwrap(),
        },
        Budget::unlimited(),
    ) {
        Ok(Executed {
            effect: Effect::NamespaceCreated(id),
            receipt,
            ..
        }) => {
            assert_eq!(
                receipt,
                applied(CREATE_COST),
                "create receipt for {actor:?}"
            );
            id
        }
        other => panic!("creating a namespace for {actor:?} failed: {other:?}"),
    }
}

fn apply(branch: &mut Branch, actor: OwnerId, op: WriteOp) -> Executed {
    branch
        .execute(actor, op.clone(), Budget::unlimited())
        .unwrap_or_else(|rejected| panic!("{actor:?} executing {op:?} was rejected: {rejected}"))
}

/// Unlimited reads shared by branches and snapshots, so the same assertions
/// can run against the working state and the published state.
trait Reads {
    fn lookup(&self, namespace: NamespaceId, key: &[u8]) -> ReadResult<Option<Vec<u8>>>;
    fn scan(&self, namespace: NamespaceId, prefix: &[u8]) -> ReadResult<Vec<Entry>>;
    fn describe(&self, namespace: NamespaceId) -> ReadResult<NamespaceDescriptor>;
}

impl Reads for Branch {
    fn lookup(&self, namespace: NamespaceId, key: &[u8]) -> ReadResult<Option<Vec<u8>>> {
        self.get(namespace, key, Budget::unlimited())
    }

    fn scan(&self, namespace: NamespaceId, prefix: &[u8]) -> ReadResult<Vec<Entry>> {
        self.scan_prefix(namespace, prefix, Budget::unlimited())
    }

    fn describe(&self, namespace: NamespaceId) -> ReadResult<NamespaceDescriptor> {
        self.descriptor(namespace, Budget::unlimited())
    }
}

impl Reads for Snapshot {
    fn lookup(&self, namespace: NamespaceId, key: &[u8]) -> ReadResult<Option<Vec<u8>>> {
        self.get(namespace, key, Budget::unlimited())
    }

    fn scan(&self, namespace: NamespaceId, prefix: &[u8]) -> ReadResult<Vec<Entry>> {
        self.scan_prefix(namespace, prefix, Budget::unlimited())
    }

    fn describe(&self, namespace: NamespaceId) -> ReadResult<NamespaceDescriptor> {
        self.descriptor(namespace, Budget::unlimited())
    }
}

fn owner_at(view: &impl Reads, namespace: NamespaceId) -> NamespaceDescriptor {
    value(view.describe(namespace))
        .unwrap_or_else(|(reason, _)| panic!("descriptor of {namespace} rejected: {reason}"))
        .0
}

// ---------------------------------------------------------------------------
// 1. Identical keys never alias
// ---------------------------------------------------------------------------

/// A key written to both namespaces with different values.
struct SharedKey {
    key: &'static [u8],
    a_value: &'static [u8],
    b_value: &'static [u8],
    /// Put cost 15 + 2 * key.len() + value.len(), for A and for B.
    a_put: u64,
    b_put: u64,
    /// Get cost 1 + 3 + value.len(), for A and for B.
    a_get: u64,
    b_get: u64,
}

const SHARED_KEYS: [SharedKey; 4] = [
    SharedKey {
        key: b"",
        a_value: b"alpha",
        b_value: b"beta",
        a_put: 20, // 15 + 0 + 5
        b_put: 19, // 15 + 0 + 4
        a_get: 9,  // 4 + 5
        b_get: 8,  // 4 + 4
    },
    SharedKey {
        key: b"\0",
        a_value: b"a0",
        b_value: b"b0!",
        a_put: 19, // 15 + 2 + 2
        b_put: 20, // 15 + 2 + 3
        a_get: 6,  // 4 + 2
        b_get: 7,  // 4 + 3
    },
    SharedKey {
        key: b"a\0b",
        a_value: b"ab",
        b_value: b"bab",
        a_put: 23, // 15 + 6 + 2
        b_put: 24, // 15 + 6 + 3
        a_get: 6,  // 4 + 2
        b_get: 7,  // 4 + 3
    },
    SharedKey {
        key: b"\xff",
        a_value: b"aff",
        b_value: b"bff!",
        a_put: 20, // 15 + 2 + 3
        b_put: 21, // 15 + 2 + 4
        a_get: 7,  // 4 + 3
        b_get: 8,  // 4 + 4
    },
];

/// The key deleted from A after being written to both namespaces.
const DELETED_IN_A: &[u8] = b"\xff";
/// A key written to B only.
const ONLY_IN_B: (&[u8], &[u8]) = (b"b/only", b"solo");

fn assert_namespaces_do_not_alias(view: &impl Reads, a: NamespaceId, b: NamespaceId, at: &str) {
    for shared in &SHARED_KEYS {
        let key = shared.key;
        let expected_a = if key == DELETED_IN_A {
            Ok((None, read(MISSING_KEY_COST)))
        } else {
            Ok((Some(shared.a_value.to_vec()), read(shared.a_get)))
        };
        assert_eq!(
            value(view.lookup(a, key)),
            expected_a,
            "{at}: get A {key:?}"
        );
        assert_eq!(
            value(view.lookup(b, key)),
            Ok((Some(shared.b_value.to_vec()), read(shared.b_get))),
            "{at}: get B {key:?}"
        );
    }
    // A key present only in B misses through A at the missing-key cost.
    assert_eq!(
        value(view.lookup(a, ONLY_IN_B.0)),
        Ok((None, read(MISSING_KEY_COST))),
        "{at}: B-only key through A"
    );
    assert_eq!(
        value(view.lookup(b, ONLY_IN_B.0)),
        Ok((Some(ONLY_IN_B.1.to_vec()), read(8))), // 4 + 4
        "{at}: B-only key through B"
    );

    // Bytewise order: "" < "\0" < "a\0b" < "b/only" < "\xff".
    let a_all = vec![
        entry(b"", b"alpha"),
        entry(b"\0", b"a0"),
        entry(b"a\0b", b"ab"),
    ];
    let b_all = vec![
        entry(b"", b"beta"),
        entry(b"\0", b"b0!"),
        entry(b"a\0b", b"bab"),
        entry(b"b/only", b"solo"),
        entry(b"\xff", b"bff!"),
    ];
    // A: 5 + (2+0+5) + (2+1+2) + (2+3+2) = 24.
    assert_eq!(
        value(view.scan(a, b"")),
        Ok((a_all, read(24))),
        "{at}: full scan of A"
    );
    // B: 5 + (2+0+4) + (2+1+3) + (2+3+3) + (2+6+4) + (2+1+4) = 44.
    assert_eq!(
        value(view.scan(b, b"")),
        Ok((b_all, read(44))),
        "{at}: full scan of B"
    );

    // Prefixes matching keys only in B: the B-only key and the key deleted in A.
    assert_eq!(
        value(view.scan(a, b"b/")),
        Ok((vec![], read(EMPTY_SCAN_COST))),
        "{at}: scan A b/"
    );
    assert_eq!(
        value(view.scan(b, b"b/")),
        Ok((vec![entry(b"b/only", b"solo")], read(17))), // 5 + (2+6+4)
        "{at}: scan B b/"
    );
    assert_eq!(
        value(view.scan(a, b"\xff")),
        Ok((vec![], read(EMPTY_SCAN_COST))),
        "{at}: scan A \\xff"
    );
    assert_eq!(
        value(view.scan(b, b"\xff")),
        Ok((vec![entry(b"\xff", b"bff!")], read(12))), // 5 + (2+1+4)
        "{at}: scan B \\xff"
    );
    // A prefix matching one key in each namespace returns that namespace's value.
    assert_eq!(
        value(view.scan(a, b"a")),
        Ok((vec![entry(b"a\0b", b"ab")], read(12))), // 5 + (2+3+2)
        "{at}: scan A a"
    );
    assert_eq!(
        value(view.scan(b, b"a")),
        Ok((vec![entry(b"a\0b", b"bab")], read(13))), // 5 + (2+3+3)
        "{at}: scan B a"
    );
}

#[test]
fn identical_keys_in_different_namespaces_never_alias() {
    let mut engine = Engine::new(PriceSchedule::FIXTURE);
    let mut branch = begin(&engine);
    let a = create(&mut branch, ALICE);
    let b = create(&mut branch, BOB);
    assert_ne!(a, b);
    let created = publish(&mut engine, branch);

    let mut branch = begin(&engine);
    for shared in &SHARED_KEYS {
        assert_eq!(
            logical(branch.execute(
                ALICE,
                put(a, shared.key, shared.a_value),
                Budget::unlimited()
            )),
            Ok((Effect::Put { replaced: false }, applied(shared.a_put))),
            "put A {:?}",
            shared.key
        );
    }
    // B is written in the opposite order, plus its private key first.
    assert_eq!(
        logical(branch.execute(BOB, put(b, ONLY_IN_B.0, ONLY_IN_B.1), Budget::unlimited())),
        Ok((Effect::Put { replaced: false }, applied(31))), // 15 + 12 + 4
    );
    for shared in SHARED_KEYS.iter().rev() {
        assert_eq!(
            logical(branch.execute(BOB, put(b, shared.key, shared.b_value), Budget::unlimited())),
            Ok((Effect::Put { replaced: false }, applied(shared.b_put))),
            "put B {:?}",
            shared.key
        );
    }
    // Deleting the key in A does not touch B's copy.
    assert_eq!(
        logical(branch.execute(ALICE, delete(a, DELETED_IN_A), Budget::unlimited())),
        Ok((Effect::Deleted { existed: true }, applied(12))), // 1 + 5 + 2 + 4
    );

    assert_namespaces_do_not_alias(&branch, a, b, "branch");
    let published = publish(&mut engine, branch);
    assert_namespaces_do_not_alias(&published, a, b, "snapshot");

    assert_eq!(published.record_count(a), Some(3), "A keeps 4 - 1 records");
    assert_eq!(published.record_count(b), Some(5), "B keeps 4 + 1 records");
    assert_eq!(created.record_count(a), Some(0), "base snapshot unchanged");
    assert_eq!(created.record_count(b), Some(0), "base snapshot unchanged");
    assert_eq!(
        value(created.scan(b, b"")),
        Ok((vec![], read(EMPTY_SCAN_COST))),
        "base snapshot B stays empty"
    );
}

// ---------------------------------------------------------------------------
// 2. Unknown namespace
// ---------------------------------------------------------------------------

#[test]
fn unknown_namespaces_are_rejected_after_the_directory_charge() {
    // Encoding: 8-byte big-endian ordinal; zero is not an ID.
    assert_eq!(NamespaceId::ENCODED_LEN, 8);
    assert_eq!(NamespaceId::from_bytes([0; 8]), None);
    let ghost = ns(999);
    assert_eq!(ghost.ordinal(), 999);
    assert_eq!(ghost.to_bytes(), [0, 0, 0, 0, 0, 0, 0x03, 0xe7]);
    assert_eq!(NamespaceId::from_bytes(ghost.to_bytes()), Some(ghost));
    assert_eq!(ghost.to_string(), "ns:00000000000003e7");
    assert_eq!(format!("{ghost}"), "ns:00000000000003e7");
    assert_eq!(
        NamespaceId::from_bytes([0xff; 8]).map(NamespaceId::ordinal),
        Some(u64::MAX)
    );

    let mut engine = Engine::new(PriceSchedule::FIXTURE);
    let genesis = engine.head_snapshot();
    assert_eq!(
        value(genesis.describe(ns(1))),
        read_refused(Rejection::UnknownNamespace)
    );
    assert_eq!(genesis.record_count(ns(1)), None);

    let mut branch = begin(&engine);
    let first = create(&mut branch, ALICE);
    assert_eq!(first, ns(1));
    assert_eq!(first.to_string(), "ns:0000000000000001");
    assert!(first < ghost);
    assert!(first.to_bytes() < ghost.to_bytes());
    let published = publish(&mut engine, branch);

    let mut branch = engine
        .begin(engine.head(), Some(Cost::new(30)))
        .expect("head is current");
    for (view, at) in [(&branch as &dyn Reads, "branch"), (&published, "snapshot")] {
        assert_eq!(
            value(view.lookup(ghost, b"k")),
            read_refused(Rejection::UnknownNamespace),
            "{at}: get"
        );
        assert_eq!(
            value(view.scan(ghost, b"")),
            read_refused(Rejection::UnknownNamespace),
            "{at}: scan"
        );
        assert_eq!(
            value(view.describe(ghost)),
            read_refused(Rejection::UnknownNamespace),
            "{at}: descriptor"
        );
    }
    assert_eq!(published.record_count(ghost), None);

    let unknown_write = refused(Rejection::UnknownNamespace);
    let budget = Budget::limit(10);
    assert_eq!(
        logical(branch.execute(ALICE, put(ghost, b"k", b"v"), budget)),
        unknown_write,
        "put"
    );
    assert_eq!(
        logical(branch.execute(ALICE, delete(ghost, b"k"), budget)),
        unknown_write,
        "delete"
    );
    assert_eq!(
        logical(branch.execute(ALICE, transfer(ghost, 0, BOB), budget)),
        unknown_write,
        "transfer"
    );
    // Every rejection was recorded and charged to the block, nothing was created.
    assert_eq!(
        branch.receipts(),
        vec![rejected(1, Rejection::UnknownNamespace); 3]
    );
    assert_eq!(branch.spent(), Cost::new(3));
    assert_eq!(branch.remaining(), Cost::new(27));
    assert_eq!(branch.namespace_count(), 1);
    let after = publish(&mut engine, branch);
    assert_eq!(after.namespace_count(), 1);
    assert_eq!(
        value(after.describe(ghost)),
        read_refused(Rejection::UnknownNamespace)
    );
    assert_eq!(owner_at(&after, first), descriptor(first, ALICE, 0));
}

// ---------------------------------------------------------------------------
// 3. Authorization and the order of checks
// ---------------------------------------------------------------------------

#[test]
fn only_the_owner_writes_and_the_actor_is_checked_before_the_revision() {
    let mut engine = Engine::new(PriceSchedule::FIXTURE);
    let mut branch = begin(&engine);
    let a = create(&mut branch, ALICE);
    apply(&mut branch, ALICE, put(a, b"k", b"v"));
    let created = publish(&mut engine, branch);

    let mut branch = begin(&engine);
    let unauthorized = refused(Rejection::Unauthorized);
    assert_eq!(
        logical(branch.execute(BOB, put(a, b"k", b"x"), Budget::unlimited())),
        unauthorized,
        "non-owner put"
    );
    assert_eq!(
        logical(branch.execute(BOB, delete(a, b"k"), Budget::unlimited())),
        unauthorized,
        "non-owner delete"
    );
    assert_eq!(
        logical(branch.execute(BOB, transfer(a, 0, BOB), Budget::unlimited())),
        unauthorized,
        "non-owner transfer"
    );
    // Nothing changed: the record and descriptor are as published.
    assert_eq!(
        value(branch.lookup(a, b"k")),
        Ok((Some(b"v".to_vec()), read(5))) // 4 + 1
    );
    assert_eq!(owner_at(&branch, a), descriptor(a, ALICE, 0));

    // The owner with a wrong revision: StaleRevision, owner unchanged.
    assert_eq!(
        logical(branch.execute(ALICE, transfer(a, 7, BOB), Budget::unlimited())),
        refused(stale(7, 0)),
        "owner transfer with a stale revision"
    );
    assert_eq!(owner_at(&branch, a), descriptor(a, ALICE, 0));
    // Wrong actor AND wrong revision: the actor check comes first.
    assert_eq!(
        logical(branch.execute(BOB, transfer(a, 7, BOB), Budget::unlimited())),
        unauthorized,
        "non-owner transfer with a stale revision"
    );
    assert_eq!(owner_at(&branch, a), descriptor(a, ALICE, 0));

    // The owner succeeds at every operation.
    assert_eq!(
        logical(branch.execute(ALICE, put(a, b"k", b"w"), Budget::unlimited())),
        Ok((Effect::Put { replaced: true }, applied(18))), // 15 + 2 + 1
    );
    assert_eq!(
        logical(branch.execute(ALICE, delete(a, b"k"), Budget::unlimited())),
        Ok((Effect::Deleted { existed: true }, applied(12))), // 10 + 2
    );
    assert_eq!(
        logical(branch.execute(ALICE, transfer(a, 0, BOB), Budget::unlimited())),
        Ok((
            Effect::Transferred {
                revision: Revision(1)
            },
            applied(TRANSFER_COST)
        )),
    );
    assert_eq!(owner_at(&branch, a), descriptor(a, BOB, 1));
    assert_eq!(
        branch.receipts(),
        vec![
            rejected(1, Rejection::Unauthorized),
            rejected(1, Rejection::Unauthorized),
            rejected(1, Rejection::Unauthorized),
            rejected(1, stale(7, 0)),
            rejected(1, Rejection::Unauthorized),
            applied(18),
            applied(12),
            applied(23),
        ]
    );
    assert_eq!(branch.spent(), Cost::new(58)); // 5 * 1 + 18 + 12 + 23

    let after = publish(&mut engine, branch);
    assert_eq!(owner_at(&created, a), descriptor(a, ALICE, 0));
    assert_eq!(owner_at(&after, a), descriptor(a, BOB, 1));
    assert_eq!(
        value(created.lookup(a, b"k")),
        Ok((Some(b"v".to_vec()), read(5)))
    );
    assert_eq!(
        value(after.lookup(a, b"k")),
        Ok((None, read(MISSING_KEY_COST)))
    );
}

// ---------------------------------------------------------------------------
// 4. Same-block authority change
// ---------------------------------------------------------------------------

#[test]
fn authority_changes_take_effect_immediately_within_a_block() {
    let mut engine = Engine::new(PriceSchedule::FIXTURE);
    let mut branch = begin(&engine);
    let a = create(&mut branch, ALICE);
    let before = publish(&mut engine, branch);

    let mut branch = begin(&engine);
    let unauthorized = refused(Rejection::Unauthorized);
    let transferred = |revision| {
        Ok((
            Effect::Transferred {
                revision: Revision(revision),
            },
            applied(TRANSFER_COST),
        ))
    };
    assert_eq!(
        logical(branch.execute(ALICE, transfer(a, 0, BOB), Budget::unlimited())),
        transferred(1)
    );
    assert_eq!(owner_at(&branch, a), descriptor(a, BOB, 1));
    assert_eq!(
        logical(branch.execute(ALICE, put(a, b"k", b"v"), Budget::unlimited())),
        unauthorized,
        "alice lost authority in the same block"
    );
    assert_eq!(
        logical(branch.execute(BOB, put(a, b"k", b"v"), Budget::unlimited())),
        Ok((Effect::Put { replaced: false }, applied(18))), // 15 + 2 + 1
        "bob gained authority in the same block"
    );
    assert_eq!(
        logical(branch.execute(ALICE, transfer(a, 1, ALICE), Budget::unlimited())),
        unauthorized,
        "alice cannot take the namespace back"
    );
    assert_eq!(
        logical(branch.execute(BOB, transfer(a, 0, ALICE), Budget::unlimited())),
        refused(stale(0, 1)),
        "bob must name the revision produced by the earlier transfer"
    );
    assert_eq!(owner_at(&branch, a), descriptor(a, BOB, 1));
    assert_eq!(
        logical(branch.execute(BOB, transfer(a, 1, ALICE), Budget::unlimited())),
        transferred(2)
    );
    assert_eq!(owner_at(&branch, a), descriptor(a, ALICE, 2));
    assert_eq!(
        logical(branch.execute(ALICE, put(a, b"k2", b"w"), Budget::unlimited())),
        Ok((Effect::Put { replaced: false }, applied(20))), // 15 + 4 + 1
        "alice regained authority immediately"
    );
    assert_eq!(
        logical(branch.execute(BOB, put(a, b"k3", b"x"), Budget::unlimited())),
        unauthorized,
        "bob lost authority immediately"
    );
    assert_eq!(branch.spent(), Cost::new(88)); // 23 + 1 + 18 + 1 + 1 + 23 + 20 + 1
    let after = publish(&mut engine, branch);

    // Descriptor history: only published states are visible, whole blocks at once.
    assert_eq!(owner_at(&before, a), descriptor(a, ALICE, 0));
    assert_eq!(owner_at(&after, a), descriptor(a, ALICE, 2));
    assert_eq!(
        value(before.lookup(a, b"k")),
        Ok((None, read(MISSING_KEY_COST)))
    );
    // Bob's record survives even though bob no longer owns the namespace.
    assert_eq!(
        value(after.scan(a, b"")),
        Ok((
            vec![entry(b"k", b"v"), entry(b"k2", b"w")],
            read(14) // 5 + (2+1+1) + (2+2+1)
        ))
    );
}

// ---------------------------------------------------------------------------
// 5. Transfer to self
// ---------------------------------------------------------------------------

#[test]
fn transfer_to_self_is_permitted_and_bumps_the_revision() {
    let mut engine = Engine::new(PriceSchedule::FIXTURE);
    let mut branch = begin(&engine);
    let a = create(&mut branch, ALICE);
    publish(&mut engine, branch);

    let mut branch = begin(&engine);
    assert_eq!(
        logical(branch.execute(ALICE, transfer(a, 0, ALICE), Budget::unlimited())),
        Ok((
            Effect::Transferred {
                revision: Revision(1)
            },
            applied(TRANSFER_COST)
        ))
    );
    assert_eq!(owner_at(&branch, a), descriptor(a, ALICE, 1));
    // The bumped revision is enforced like any other.
    assert_eq!(
        logical(branch.execute(ALICE, transfer(a, 0, ALICE), Budget::unlimited())),
        refused(stale(0, 1))
    );
    assert_eq!(
        logical(branch.execute(ALICE, transfer(a, 1, ALICE), Budget::unlimited())),
        Ok((
            Effect::Transferred {
                revision: Revision(2)
            },
            applied(TRANSFER_COST)
        ))
    );
    // Alice still owns it and can write.
    assert_eq!(
        logical(branch.execute(ALICE, put(a, b"k", b"v"), Budget::unlimited())),
        Ok((Effect::Put { replaced: false }, applied(18))) // 15 + 2 + 1
    );
    let after = publish(&mut engine, branch);
    assert_eq!(owner_at(&after, a), descriptor(a, ALICE, 2));
}

// ---------------------------------------------------------------------------
// 6. Transfer cost is independent of namespace size
// ---------------------------------------------------------------------------

#[test]
fn transfer_is_a_metadata_operation_independent_of_namespace_size() {
    const RECORDS: usize = 2_000;
    const PER_BLOCK: usize = 500;
    // Each record: 8-byte key "rec/NNNN", 16-byte zero-padded value.
    let record = |i: usize| (format!("rec/{i:04}"), format!("{i:016}"));

    let mut engine = Engine::new(PriceSchedule::FIXTURE);
    let mut branch = begin(&engine);
    let a = create(&mut branch, ALICE);
    let b = create(&mut branch, ALICE);
    publish(&mut engine, branch);

    for block in 0..RECORDS / PER_BLOCK {
        let mut branch = begin(&engine);
        for i in block * PER_BLOCK..(block + 1) * PER_BLOCK {
            let (key, value) = record(i);
            let executed = apply(&mut branch, ALICE, put(b, key.as_bytes(), value.as_bytes()));
            assert_eq!(executed.receipt, applied(47), "put {key}"); // 15 + 16 + 16
        }
        // 500 puts at 47 each.
        assert_eq!(branch.spent(), Cost::new(23_500), "block {block}");
        publish(&mut engine, branch);
    }
    let before = engine.head_snapshot();
    assert_eq!(before.record_count(a), Some(0));
    assert_eq!(before.record_count(b), Some(RECORDS));

    let expected_entries: Vec<Entry> = (0..RECORDS)
        .map(|i| {
            let (key, value) = record(i);
            entry(key.as_bytes(), value.as_bytes())
        })
        .collect();
    // 5 + 2000 * (2 + 8 + 16) = 52_005.
    let scan_before = value(before.scan(b, b""));
    assert_eq!(scan_before, Ok((expected_entries, read(52_005))));

    let mut branch = begin(&engine);
    let budget = Budget::limit(100).traced();
    let empty = branch
        .execute(ALICE, transfer(a, 0, BOB), budget)
        .expect("transfer of the empty namespace");
    let large = branch
        .execute(ALICE, transfer(b, 0, BOB), budget)
        .expect("transfer of the 2000-record namespace");
    let expected_receipt = traced(
        applied(TRANSFER_COST),
        &[
            (OperationClass::DirectoryLookup, 1, 1, 1),
            (OperationClass::NamespaceTransfer, 1, 20, 20),
            (OperationClass::RootUpdate, 1, 2, 2),
        ],
    );
    assert_eq!(empty.receipt, expected_receipt);
    assert_eq!(large.receipt, expected_receipt);
    assert_eq!(empty.receipt, large.receipt);
    assert_eq!(
        empty.effect,
        Effect::Transferred {
            revision: Revision(1)
        }
    );
    assert_eq!(large.effect, empty.effect);
    assert_eq!(empty.physical.record_nodes, 0, "transfer visits no record");
    assert_eq!(large.physical.record_nodes, 0, "transfer visits no record");
    assert_eq!(branch.spent(), Cost::new(46));
    let after = publish(&mut engine, branch);

    assert!(
        before.records_shared_with(&after, b),
        "B's records were not copied"
    );
    // A is empty, so this identity check is vacuous; the populated B above
    // and the equal receipts are the evidence for size independence.
    assert!(
        before.records_shared_with(&after, a),
        "A's (empty) record map is reported as shared"
    );
    assert_eq!(after.record_count(a), Some(0));
    assert_eq!(after.record_count(b), Some(RECORDS));
    assert_eq!(owner_at(&before, b), descriptor(b, ALICE, 0));
    assert_eq!(owner_at(&after, b), descriptor(b, BOB, 1));
    assert_eq!(owner_at(&after, a), descriptor(a, BOB, 1));
    assert_eq!(
        value(after.scan(b, b"")),
        scan_before,
        "the transferred namespace reads identically"
    );
    // The new owner writes, the old one does not.
    let mut branch = begin(&engine);
    assert_eq!(
        logical(branch.execute(ALICE, put(b, b"k", b"v"), Budget::unlimited())),
        refused(Rejection::Unauthorized)
    );
    assert_eq!(
        logical(branch.execute(BOB, put(b, b"k", b"v"), Budget::unlimited())),
        Ok((Effect::Put { replaced: false }, applied(18)))
    );
}

// ---------------------------------------------------------------------------
// 7. ID allocation
// ---------------------------------------------------------------------------

#[test]
fn ids_are_creation_ordinals_and_discarded_ordinals_are_reused() {
    let mut engine = Engine::new(PriceSchedule::FIXTURE);
    let genesis = engine.head();

    let mut branch = begin(&engine);
    assert_eq!(create(&mut branch, ALICE), ns(1));
    let first = publish(&mut engine, branch);
    let mut branch = begin(&engine);
    assert_eq!(create(&mut branch, BOB), ns(2));
    assert_eq!(create(&mut branch, ALICE), ns(3));
    let second = publish(&mut engine, branch);
    assert_eq!(second.namespace_count(), 3);

    // Block 3, candidate X: create -> rollback -> create reuses ordinal 4.
    let mut candidate = begin(&engine);
    let checkpoint = candidate.checkpoint();
    assert_eq!(create(&mut candidate, CAROL), ns(4));
    assert_eq!(owner_at(&candidate, ns(4)), descriptor(ns(4), CAROL, 0));
    assert_eq!(candidate.rollback_to(checkpoint), Ok(0..1));
    assert_eq!(candidate.namespace_count(), 3);
    assert_eq!(
        value(candidate.describe(ns(4))),
        read_refused(Rejection::UnknownNamespace),
        "the rolled-back creation is gone"
    );
    assert_eq!(create(&mut candidate, BOB), ns(4), "the ordinal is reused");
    assert_eq!(owner_at(&candidate, ns(4)), descriptor(ns(4), BOB, 0));
    // The rolled-back creation was still charged.
    assert_eq!(candidate.receipts(), vec![applied(53), applied(53)]);
    assert_eq!(candidate.spent(), Cost::new(106));
    let candidate = candidate.seal();

    // A competing candidate on the same base allocates the same ordinal.
    let mut competitor = begin(&engine);
    assert_eq!(create(&mut competitor, DAVE), ns(4));
    assert_eq!(owner_at(&competitor, ns(4)), descriptor(ns(4), DAVE, 0));
    let competitor = competitor.seal();

    let third_id = engine.publish(&candidate).expect("first publish wins");
    assert_eq!(
        engine.publish(&competitor),
        Err(EngineError::StaleHead {
            expected: second.id(),
            head: third_id,
        }),
        "the loser is rejected without changing anything"
    );
    let third = engine.snapshot(third_id).expect("published");
    assert_eq!(third.namespace_count(), 4);
    assert_eq!(owner_at(&third, ns(4)), descriptor(ns(4), BOB, 0));

    // Retrying on the new head gets the next ordinal.
    let mut retry = begin(&engine);
    assert_eq!(create(&mut retry, DAVE), ns(5));
    let fourth = publish(&mut engine, retry);
    assert_eq!(owner_at(&fourth, ns(5)), descriptor(ns(5), DAVE, 0));

    // A later transfer extends ordinal 4's lineage; earlier snapshots keep theirs.
    let mut branch = begin(&engine);
    apply(&mut branch, BOB, transfer(ns(4), 0, ALICE));
    let fifth = publish(&mut engine, branch);

    assert_eq!(engine.snapshot_count(), 6);
    let expected_lineage = [
        (genesis, None),
        (first.id(), None),
        (second.id(), None),
        (third.id(), Some((BOB, 0))),
        (fourth.id(), Some((BOB, 0))),
        (fifth.id(), Some((ALICE, 1))),
    ];
    for (id, expected) in expected_lineage {
        let snapshot = engine.snapshot(id).expect("retained");
        let expected = match expected {
            Some((owner, revision)) => Ok((descriptor(ns(4), owner, revision), read(1))),
            None => read_refused(Rejection::UnknownNamespace),
        };
        assert_eq!(
            value(snapshot.describe(ns(4))),
            expected,
            "ordinal 4 at {id}"
        );
        assert_ne!(
            value(snapshot.describe(ns(4)))
                .ok()
                .map(|(found, _)| found.owner),
            Some(CAROL),
            "the rolled-back owner never appears at {id}"
        );
        assert_ne!(
            value(snapshot.describe(ns(4)))
                .ok()
                .map(|(found, _)| found.owner),
            Some(DAVE),
            "the losing candidate's owner never appears at {id}"
        );
        let has_five = id.sequence() >= fourth.id().sequence();
        assert_eq!(
            value(snapshot.describe(ns(5))).ok(),
            has_five.then(|| (descriptor(ns(5), DAVE, 0), read(1))),
            "ordinal 5 at {id}"
        );
    }
    // Ordinals 1..3 are stable across every snapshot that has them.
    for (ordinal, owner) in [(1, ALICE), (2, BOB), (3, ALICE)] {
        for snapshot in [&second, &third, &fourth, &fifth] {
            assert_eq!(
                owner_at(snapshot, ns(ordinal)),
                descriptor(ns(ordinal), owner, 0),
                "ordinal {ordinal} at {}",
                snapshot.id()
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 8. Namespace cap
// ---------------------------------------------------------------------------

#[test]
fn namespace_cap_rejects_creation_after_the_directory_charge_and_rollback_frees_it() {
    let mut engine = Engine::with_namespace_cap(PriceSchedule::FIXTURE, 2);
    assert_eq!(engine.namespace_cap(), 2);
    let full = refused(Rejection::DirectoryFull);
    // With tracing the ledger shows the single directory charge.
    let full_traced = Err((
        Rejection::DirectoryFull,
        traced(
            rejected(DIRECTORY_CHARGE, Rejection::DirectoryFull),
            &[(OperationClass::DirectoryLookup, 1, 1, 1)],
        ),
    ));

    let mut branch = begin(&engine);
    assert_eq!(create(&mut branch, ALICE), ns(1));
    let checkpoint = branch.checkpoint();
    assert_eq!(create(&mut branch, BOB), ns(2));
    assert_eq!(branch.namespace_count(), 2);
    assert_eq!(
        logical(branch.execute(
            CAROL,
            WriteOp::CreateNamespace {
                name: arkiv_db::NamespaceName::new("test").unwrap()
            },
            Budget::limit(100).traced()
        )),
        full_traced,
        "the third creation hits the cap"
    );
    assert_eq!(branch.namespace_count(), 2);
    assert_eq!(
        logical(branch.estimate(
            CAROL,
            WriteOp::CreateNamespace {
                name: arkiv_db::NamespaceName::new("test").unwrap()
            },
            Budget::unlimited()
        )),
        full,
        "an estimate agrees"
    );
    assert_eq!(
        value(branch.describe(ns(3))),
        read_refused(Rejection::UnknownNamespace)
    );

    // Rolling back the second creation frees a slot.
    assert_eq!(branch.rollback_to(checkpoint), Ok(1..3));
    assert_eq!(branch.namespace_count(), 1);
    assert_eq!(
        create(&mut branch, CAROL),
        ns(2),
        "the freed slot and ordinal"
    );
    assert_eq!(
        logical(branch.execute(
            DAVE,
            WriteOp::CreateNamespace {
                name: arkiv_db::NamespaceName::new("test").unwrap()
            },
            Budget::unlimited()
        )),
        full,
        "the cap applies again"
    );
    assert_eq!(branch.namespace_count(), 2);
    assert_eq!(
        branch.receipts(),
        vec![
            applied(53),
            applied(53),
            traced(
                rejected(1, Rejection::DirectoryFull),
                &[(OperationClass::DirectoryLookup, 1, 1, 1)]
            ),
            applied(53),
            rejected(1, Rejection::DirectoryFull),
        ]
    );
    assert_eq!(branch.spent(), Cost::new(161)); // 53 + 53 + 1 + 53 + 1
    let published = publish(&mut engine, branch);
    assert_eq!(published.namespace_count(), 2);
    assert_eq!(owner_at(&published, ns(1)), descriptor(ns(1), ALICE, 0));
    assert_eq!(owner_at(&published, ns(2)), descriptor(ns(2), CAROL, 0));
    assert_eq!(
        value(published.describe(ns(3))),
        read_refused(Rejection::UnknownNamespace)
    );

    // The committed directory is full for later blocks too.
    let mut branch = begin(&engine);
    assert_eq!(
        logical(branch.execute(
            ALICE,
            WriteOp::CreateNamespace {
                name: arkiv_db::NamespaceName::new("test").unwrap()
            },
            Budget::unlimited()
        )),
        full
    );
    assert_eq!(branch.namespace_count(), 2);
    // Existing namespaces keep working at the cap.
    assert_eq!(
        logical(branch.execute(CAROL, put(ns(2), b"k", b"v"), Budget::unlimited())),
        Ok((Effect::Put { replaced: false }, applied(18)))
    );
}

// ---------------------------------------------------------------------------
// 9. Descriptor history
// ---------------------------------------------------------------------------

#[test]
fn descriptor_history_reflects_published_transfers_only() {
    let mut engine = Engine::new(PriceSchedule::FIXTURE);
    let genesis = engine.head_snapshot();

    let mut branch = begin(&engine);
    let a = create(&mut branch, ALICE);
    let created = publish(&mut engine, branch);

    let mut branch = begin(&engine);
    apply(&mut branch, ALICE, transfer(a, 0, BOB));
    let to_bob = publish(&mut engine, branch);

    let mut branch = begin(&engine);
    apply(&mut branch, BOB, transfer(a, 1, CAROL));
    let to_carol = publish(&mut engine, branch);

    // Block 4: a transfer that is rolled back, then an unrelated write.
    let mut branch = begin(&engine);
    let checkpoint = branch.checkpoint();
    assert_eq!(
        logical(branch.execute(CAROL, transfer(a, 2, DAVE), Budget::unlimited())),
        Ok((
            Effect::Transferred {
                revision: Revision(3)
            },
            applied(TRANSFER_COST)
        ))
    );
    assert_eq!(owner_at(&branch, a), descriptor(a, DAVE, 3));
    assert_eq!(branch.rollback_to(checkpoint), Ok(0..1));
    assert_eq!(owner_at(&branch, a), descriptor(a, CAROL, 2));
    assert_eq!(
        logical(branch.execute(CAROL, transfer(a, 3, DAVE), Budget::unlimited())),
        refused(stale(3, 2)),
        "the rolled-back revision is not current"
    );
    apply(&mut branch, CAROL, put(a, b"k", b"v"));
    let rolled_back = publish(&mut engine, branch);

    // A discarded candidate never reaches any snapshot.
    let mut discarded = begin(&engine);
    apply(&mut discarded, CAROL, transfer(a, 2, DAVE));
    drop(discarded);

    let history = [
        (&genesis, None),
        (&created, Some((ALICE, 0))),
        (&to_bob, Some((BOB, 1))),
        (&to_carol, Some((CAROL, 2))),
        (&rolled_back, Some((CAROL, 2))),
        (&engine.head_snapshot(), Some((CAROL, 2))),
    ];
    for (snapshot, expected) in history {
        let expected = match expected {
            Some((owner, revision)) => Ok((descriptor(a, owner, revision), read(1))),
            None => read_refused(Rejection::UnknownNamespace),
        };
        assert_eq!(
            value(snapshot.describe(a)),
            expected,
            "at {}",
            snapshot.id()
        );
    }
    assert_eq!(genesis.id(), SnapshotId::GENESIS);
    assert_eq!(rolled_back.base(), Some(to_carol.id()));
    assert_eq!(genesis.namespace_count(), 0);
    assert_eq!(genesis.record_count(a), None);
}

// ---------------------------------------------------------------------------
// 10. Ownership is per namespace
// ---------------------------------------------------------------------------

#[test]
fn owning_one_namespace_grants_nothing_in_another() {
    let mut engine = Engine::new(PriceSchedule::FIXTURE);
    let mut branch = begin(&engine);
    let a = create(&mut branch, ALICE);
    let b = create(&mut branch, ALICE);
    apply(&mut branch, ALICE, put(a, b"k", b"a"));
    publish(&mut engine, branch);

    let mut branch = begin(&engine);
    apply(&mut branch, ALICE, transfer(b, 0, BOB));
    let transferred = publish(&mut engine, branch);
    assert_eq!(owner_at(&transferred, a), descriptor(a, ALICE, 0));
    assert_eq!(owner_at(&transferred, b), descriptor(b, BOB, 1));

    let mut branch = begin(&engine);
    let unauthorized = refused(Rejection::Unauthorized);
    assert_eq!(
        logical(branch.execute(BOB, put(a, b"k", b"b"), Budget::unlimited())),
        unauthorized,
        "bob's ownership of B does not reach A"
    );
    assert_eq!(
        logical(branch.execute(BOB, delete(a, b"k"), Budget::unlimited())),
        unauthorized
    );
    assert_eq!(
        logical(branch.execute(BOB, transfer(a, 0, BOB), Budget::unlimited())),
        unauthorized
    );
    assert_eq!(
        logical(branch.execute(BOB, put(b, b"k", b"b"), Budget::unlimited())),
        Ok((Effect::Put { replaced: false }, applied(18))), // 15 + 2 + 1
        "bob writes the namespace he owns"
    );
    assert_eq!(
        logical(branch.execute(ALICE, put(b, b"k", b"a"), Budget::unlimited())),
        unauthorized,
        "alice's ownership of A does not reach B"
    );
    assert_eq!(
        logical(branch.execute(ALICE, put(a, b"k", b"a2"), Budget::unlimited())),
        Ok((Effect::Put { replaced: true }, applied(19))), // 15 + 2 + 2
        "alice still writes A"
    );
    let after = publish(&mut engine, branch);
    assert_eq!(owner_at(&after, a), descriptor(a, ALICE, 0));
    assert_eq!(owner_at(&after, b), descriptor(b, BOB, 1));
    assert_eq!(
        value(after.lookup(a, b"k")),
        Ok((Some(b"a2".to_vec()), read(6))) // 4 + 2
    );
    assert_eq!(
        value(after.lookup(b, b"k")),
        Ok((Some(b"b".to_vec()), read(5))) // 4 + 1
    );
}
