//! Per-operation atomicity, checkpoints and transaction rollback on a branch.
//!
//! Every expected cost below is a literal derived by hand from the FIXTURE
//! weights (DirectoryLookup 1, NamespaceCreate 50, NamespaceTransfer 20,
//! PutRecord 10, DeleteRecord 5, WriteKeyByte 2, WriteValueByte 1,
//! RootUpdate 2) and the model-version-3 charge points:
//!
//! | operation                              | cost                   |
//! | -------------------------------------- | ---------------------- |
//! | CreateNamespace                        | 1 + 50 + 2 = 53        |
//! | TransferNamespace                      | 1 + 20 + 2 = 23        |
//! | Put (k key bytes, v value bytes)       | 1 + 10 + 2k + v + 4    |
//! | Delete (k key bytes)                   | 1 + 5 + 2k + 4         |
//! | any rejection after the directory hop  | 1                  |
//! | admission rejection (ExceedsBlockBudget) | 0, never recorded |

use arkiv_db::{
    Branch, Budget, Cost, Effect, Engine, EngineError, Entry, Executed, Ledger, LedgerEntry,
    NamespaceDescriptor, NamespaceId, OperationClass, Outcome, OwnerId, PriceSchedule, Receipt,
    ReceiptKind, Rejected, Rejection, Revision, Snapshot, WriteOp,
};

const ALICE: OwnerId = OwnerId::new([0xa1; 20]);
const BOB: OwnerId = OwnerId::new([0xb0; 20]);
const CAROL: OwnerId = OwnerId::new([0xc0; 20]);
const DAVE: OwnerId = OwnerId::new([0xd0; 20]);

/// Ordinals a state view probes. Larger than any namespace a test creates,
/// so a creation that should have been undone shows up in the comparison.
const PROBED_ORDINALS: u64 = 6;

fn ns(ordinal: u64) -> NamespaceId {
    NamespaceId::from_bytes(ordinal.to_be_bytes()).expect("ordinals start at 1")
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

fn entry(key: &[u8], value: &[u8]) -> Entry {
    Entry {
        key: key.to_vec(),
        value: value.to_vec(),
    }
}

fn descriptor(ordinal: u64, owner: OwnerId, revision: u64) -> NamespaceDescriptor {
    NamespaceDescriptor {
        name: arkiv_db::NamespaceName::new("test").unwrap(),
        id: ns(ordinal),
        owner,
        revision: Revision(revision),
    }
}

/// The canonical receipt of an applied, untraced write under schedule 1.
fn applied(spent: u64) -> Receipt {
    Receipt {
        kind: ReceiptKind::Write,
        model_version: 3,
        schedule_id: 1,
        spent: Cost::new(spent),
        outcome: Outcome::Applied,
        ledger: None,
    }
}

/// The canonical receipt of a rejected, untraced write under schedule 1.
fn rejected(spent: u64, reason: Rejection) -> Receipt {
    Receipt {
        outcome: Outcome::Rejected(reason),
        ..applied(spent)
    }
}

/// Publishes one namespace per owner on a fresh engine: the n-th owner in
/// `owners` receives ordinal n. The published head is snapshot 1.
fn engine_with_namespaces(owners: &[OwnerId]) -> Engine {
    let mut engine = Engine::new(PriceSchedule::FIXTURE);
    let mut branch = engine.begin(engine.head(), None).expect("genesis is head");
    for (index, &owner) in owners.iter().enumerate() {
        let created = apply(
            &mut branch,
            owner,
            WriteOp::CreateNamespace {
                name: arkiv_db::NamespaceName::new("test").unwrap(),
            },
        );
        let ordinal = u64::try_from(index).expect("small") + 1;
        assert_eq!(created.effect, Effect::NamespaceCreated(ns(ordinal)));
    }
    engine
        .publish(&branch.seal())
        .expect("setup block publishes");
    engine
}

/// Executes `op` with an unlimited budget and requires it to apply.
fn apply(branch: &mut Branch, actor: OwnerId, op: WriteOp) -> Executed {
    branch
        .execute(actor, op.clone(), Budget::unlimited())
        .unwrap_or_else(|rejected| panic!("{op:?} by {actor:?} was rejected: {rejected}"))
}

/// Executes `op` under `budget` and requires it to be rejected.
fn reject(branch: &mut Branch, actor: OwnerId, op: WriteOp, budget: Budget) -> Rejected {
    match branch.execute(actor, op.clone(), budget) {
        Err(rejected) => rejected,
        Ok(executed) => panic!("{op:?} by {actor:?} applied unexpectedly: {executed:?}"),
    }
}

/// The metered reads a branch and a snapshot share, with unlimited budgets.
trait View {
    fn value(&self, namespace: NamespaceId, key: &[u8]) -> Result<Option<Vec<u8>>, Rejection>;
    fn entries(&self, namespace: NamespaceId) -> Result<Vec<Entry>, Rejection>;
    fn desc(&self, namespace: NamespaceId) -> Result<NamespaceDescriptor, Rejection>;
    fn count(&self) -> usize;
}

impl View for Branch {
    fn value(&self, namespace: NamespaceId, key: &[u8]) -> Result<Option<Vec<u8>>, Rejection> {
        self.get(namespace, key, Budget::unlimited())
            .map(|read| read.value)
            .map_err(|rejected| rejected.reason)
    }

    fn entries(&self, namespace: NamespaceId) -> Result<Vec<Entry>, Rejection> {
        self.scan_prefix(namespace, b"", Budget::unlimited())
            .map(|read| read.value)
            .map_err(|rejected| rejected.reason)
    }

    fn desc(&self, namespace: NamespaceId) -> Result<NamespaceDescriptor, Rejection> {
        self.descriptor(namespace, Budget::unlimited())
            .map(|read| read.value)
            .map_err(|rejected| rejected.reason)
    }

    fn count(&self) -> usize {
        self.namespace_count()
    }
}

impl View for Snapshot {
    fn value(&self, namespace: NamespaceId, key: &[u8]) -> Result<Option<Vec<u8>>, Rejection> {
        self.get(namespace, key, Budget::unlimited())
            .map(|read| read.value)
            .map_err(|rejected| rejected.reason)
    }

    fn entries(&self, namespace: NamespaceId) -> Result<Vec<Entry>, Rejection> {
        self.scan_prefix(namespace, b"", Budget::unlimited())
            .map(|read| read.value)
            .map_err(|rejected| rejected.reason)
    }

    fn desc(&self, namespace: NamespaceId) -> Result<NamespaceDescriptor, Rejection> {
        self.descriptor(namespace, Budget::unlimited())
            .map(|read| read.value)
            .map_err(|rejected| rejected.reason)
    }

    fn count(&self) -> usize {
        self.namespace_count()
    }
}

/// The value of an existing namespace's key.
fn value(view: &impl View, namespace: NamespaceId, key: &[u8]) -> Option<Vec<u8>> {
    view.value(namespace, key)
        .unwrap_or_else(|reason| panic!("reading {namespace} failed: {reason}"))
}

/// What one probed ordinal looks like: its descriptor and all of its records,
/// or `UnknownNamespace` for both when it does not exist.
type NamespaceView = (
    Result<NamespaceDescriptor, Rejection>,
    Result<Vec<Entry>, Rejection>,
);

/// Everything a test compares before and after an operation: the namespace
/// count plus the view of every probed ordinal.
#[derive(Debug, PartialEq, Eq)]
struct StateView {
    namespace_count: usize,
    namespaces: Vec<NamespaceView>,
}

fn observe(view: &impl View) -> StateView {
    StateView {
        namespace_count: view.count(),
        namespaces: (1..=PROBED_ORDINALS)
            .map(|ordinal| (view.desc(ns(ordinal)), view.entries(ns(ordinal))))
            .collect(),
    }
}

/// The view of a state holding exactly `namespaces`, given in ordinal order
/// starting at 1.
fn expected_view(namespaces: &[(NamespaceDescriptor, Vec<Entry>)]) -> StateView {
    StateView {
        namespace_count: namespaces.len(),
        namespaces: (0..PROBED_ORDINALS)
            .map(
                |index| match namespaces.get(usize::try_from(index).expect("small")) {
                    Some((descriptor, entries)) => (Ok(*descriptor), Ok(entries.clone())),
                    None => (
                        Err(Rejection::UnknownNamespace),
                        Err(Rejection::UnknownNamespace),
                    ),
                },
            )
            .collect(),
    }
}

// ---------------------------------------------------------------------------
// 1. Per-operation atomicity
// ---------------------------------------------------------------------------

/// Runs one rejected write in a branch that already holds applied work and
/// checks that nothing changed: not the base data, not the earlier applied
/// put, not the descriptors, not the namespace count. The check runs on the
/// branch and again on the published snapshot.
///
/// Base (snapshot 1 + block 2): A = ns 1 (alice) with k1 = "one"; B = ns 2
/// (bob), empty. The branch under test first applies A/k2 = "two".
fn assert_rejection_is_atomic(actor: OwnerId, op: WriteOp, budget: Budget, expected: Receipt) {
    let mut engine = engine_with_namespaces(&[ALICE, BOB]);
    let (a, b) = (ns(1), ns(2));
    let mut setup = engine.begin(engine.head(), None).expect("head");
    apply(&mut setup, ALICE, put(a, b"k1", b"one"));
    engine
        .publish(&setup.seal())
        .expect("setup block publishes");

    let unchanged = expected_view(&[
        (
            descriptor(1, ALICE, 0),
            vec![entry(b"k1", b"one"), entry(b"k2", b"two")],
        ),
        (descriptor(2, BOB, 0), vec![]),
    ]);

    let mut branch = engine
        .begin(engine.head(), Some(Cost::new(1_000)))
        .expect("head");
    // put A/k2 = "two": 1 + 10 + 2*2 + 3 + 4 = 22.
    assert_eq!(
        apply(&mut branch, ALICE, put(a, b"k2", b"two")).receipt,
        applied(22)
    );
    assert_eq!(
        observe(&branch),
        unchanged,
        "precondition: the applied put is visible"
    );

    let rejection = reject(&mut branch, actor, op.clone(), budget);
    assert_eq!(
        rejection.receipt, expected,
        "receipt of the rejected {op:?}"
    );
    assert_eq!(Outcome::Rejected(rejection.reason), expected.outcome);

    assert_eq!(
        observe(&branch),
        unchanged,
        "branch state after rejecting {op:?}"
    );
    assert_eq!(value(&branch, a, b"k1"), Some(b"one".to_vec()));
    assert_eq!(value(&branch, b, b"k1"), None);
    assert_eq!(branch.namespace_count(), 2);
    assert_eq!(
        branch.receipts().len(),
        2,
        "the rejection is still recorded"
    );
    assert_eq!(branch.spent(), Cost::new(22 + expected.spent.units()));

    let id = engine.publish(&branch.seal()).expect("candidate publishes");
    let snapshot = engine.snapshot(id).expect("published");
    assert_eq!(
        observe(&snapshot),
        unchanged,
        "published state after rejecting {op:?}"
    );
    assert_eq!(snapshot.namespace_count(), 2);
    assert_eq!(snapshot.record_count(a), Some(2));
    assert_eq!(snapshot.record_count(b), Some(0));
}

#[test]
fn put_out_of_budget_after_the_directory_lookup_changes_nothing() {
    // put A/k1 = "new" costs 1 + (10 + 2*2 + 3 + 4) = 22. A limit of 5 admits
    // the directory lookup (1) but not the put group (21): rejected with
    // spent 1 and the existing "one" stays in place.
    assert_rejection_is_atomic(
        ALICE,
        put(ns(1), b"k1", b"new"),
        Budget::limit(5),
        rejected(1, Rejection::OutOfBudget),
    );
}

#[test]
fn unauthorized_put_changes_nothing() {
    // Bob is not A's owner: rejected after the directory lookup, spent 1.
    assert_rejection_is_atomic(
        BOB,
        put(ns(1), b"k1", b"new"),
        Budget::limit(100),
        rejected(1, Rejection::Unauthorized),
    );
}

#[test]
fn put_to_an_unknown_namespace_changes_nothing() {
    // Ordinal 9 was never allocated: rejected after the lookup, spent 1.
    assert_rejection_is_atomic(
        ALICE,
        put(ns(9), b"k1", b"new"),
        Budget::limit(100),
        rejected(1, Rejection::UnknownNamespace),
    );
}

#[test]
fn stale_revision_transfer_changes_nothing() {
    // A is at revision 0; a transfer built against revision 7 is rejected
    // after the lookup and owner check, spent 1, and A keeps its owner.
    assert_rejection_is_atomic(
        ALICE,
        transfer(ns(1), 7, BOB),
        Budget::limit(100),
        rejected(
            1,
            Rejection::StaleRevision {
                expected: Revision(7),
                current: Revision(0),
            },
        ),
    );
}

#[test]
fn create_rejected_out_of_budget_does_not_consume_an_ordinal() {
    let mut engine = engine_with_namespaces(&[ALICE, BOB]);
    let mut branch = engine.begin(engine.head(), None).expect("head");

    // Limit 1 covers only the directory lookup; the group NamespaceCreate 50
    // + RootUpdate 2 does not fit, so the create is rejected with spent 1.
    let rejection = reject(
        &mut branch,
        CAROL,
        WriteOp::CreateNamespace {
            name: arkiv_db::NamespaceName::new("test").unwrap(),
        },
        Budget::limit(1),
    );
    assert_eq!(rejection.receipt, rejected(1, Rejection::OutOfBudget));
    assert_eq!(branch.namespace_count(), 2);
    assert_eq!(branch.desc(ns(3)), Err(Rejection::UnknownNamespace));

    // Limit 52 is also one unit short of 1 + 50 + 2.
    let rejection = reject(
        &mut branch,
        CAROL,
        WriteOp::CreateNamespace {
            name: arkiv_db::NamespaceName::new("test").unwrap(),
        },
        Budget::limit(52),
    );
    assert_eq!(rejection.receipt, rejected(1, Rejection::OutOfBudget));
    assert_eq!(branch.namespace_count(), 2);

    // The next successful create receives the ordinal the rejections did not
    // consume; the exact budget of 53 is enough.
    let created = branch
        .execute(
            CAROL,
            WriteOp::CreateNamespace {
                name: arkiv_db::NamespaceName::new("test").unwrap(),
            },
            Budget::limit(53),
        )
        .expect("exact budget applies");
    assert_eq!(created.effect, Effect::NamespaceCreated(ns(3)));
    assert_eq!(created.receipt, applied(53));
    assert_eq!(branch.namespace_count(), 3);
    assert_eq!(branch.desc(ns(3)), Ok(descriptor(3, CAROL, 0)));
    assert_eq!(branch.desc(ns(4)), Err(Rejection::UnknownNamespace));

    // 1 + 1 + 53.
    assert_eq!(branch.spent(), Cost::new(55));
    assert_eq!(branch.receipts().len(), 3);

    let id = engine.publish(&branch.seal()).expect("candidate publishes");
    let snapshot = engine.snapshot(id).expect("published");
    assert_eq!(snapshot.namespace_count(), 3);
    assert_eq!(snapshot.desc(ns(3)), Ok(descriptor(3, CAROL, 0)));
    assert_eq!(snapshot.desc(ns(4)), Err(Rejection::UnknownNamespace));
}

// ---------------------------------------------------------------------------
// 2. Nested checkpoints
// ---------------------------------------------------------------------------

#[test]
fn nested_checkpoints_roll_back_the_inner_then_the_outer_transaction() {
    // Base: A = ns 1 (alice), B = ns 2 (bob), C = ns 3 (carol). The block
    // limit of 243 is tuned so that the last put lands exactly on the
    // remaining budget.
    //
    // | # | operation                                 | cost | running spent |
    // | 0 | put A/k1 = "one"    1 + 10 + 2*2 + 3 + 4  |  22  |  22           |
    // | 1 | transfer C carol -> dave, rev 0 -> 1      |  23  |  45           | cp2
    // | 2 | put A/k1 = "two"    1 + 10 + 2*2 + 3 + 4  |  22  |  67           |
    // | 3 | put B/k2 = "beta"   1 + 10 + 2*2 + 4 + 4  |  23  |  90           |
    // | 4 | create -> ns 4                            |  53  | 143           |
    // | 5 | put A/k3 = "x", limit 5 -> OutOfBudget    |   1  | 144           | rollback_to(cp2)
    // | 6 | create -> ns 4 again                      |  53  | 197           | rollback_to(cp2), rollback_to(cp1)
    // | 7 | transfer C carol -> dave, rev 0 -> 1      |  23  | 220           |
    // | 8 | put B/k2 = "beta" with limit 23           |  23  | 243           | == block limit
    let mut engine = engine_with_namespaces(&[ALICE, BOB, CAROL]);
    let (a, b, c) = (ns(1), ns(2), ns(3));
    let mut branch = engine
        .begin(engine.head(), Some(Cost::new(243)))
        .expect("head");

    let cp1 = branch.checkpoint();
    assert_eq!(
        apply(&mut branch, ALICE, put(a, b"k1", b"one")).receipt,
        applied(22)
    );
    let transferred = apply(&mut branch, CAROL, transfer(c, 0, DAVE));
    assert_eq!(
        transferred.effect,
        Effect::Transferred {
            revision: Revision(1)
        }
    );
    assert_eq!(transferred.receipt, applied(23));
    let cp2 = branch.checkpoint();

    let replaced = apply(&mut branch, ALICE, put(a, b"k1", b"two"));
    assert_eq!(replaced.effect, Effect::Put { replaced: true });
    assert_eq!(replaced.receipt, applied(22));
    assert_eq!(
        apply(&mut branch, BOB, put(b, b"k2", b"beta")).receipt,
        applied(23)
    );
    let created = apply(
        &mut branch,
        ALICE,
        WriteOp::CreateNamespace {
            name: arkiv_db::NamespaceName::new("test").unwrap(),
        },
    );
    assert_eq!(created.effect, Effect::NamespaceCreated(ns(4)));
    assert_eq!(created.receipt, applied(53));
    let rejection = reject(&mut branch, ALICE, put(a, b"k3", b"x"), Budget::limit(5));
    assert_eq!(rejection.receipt, rejected(1, Rejection::OutOfBudget));
    assert_eq!(branch.spent(), Cost::new(144));
    assert_eq!(branch.namespace_count(), 4);

    // Inner rollback: exactly tx2's receipts (applied and rejected) are covered.
    assert_eq!(branch.rollback_to(cp2), Ok(2..6));
    let after_tx1 = expected_view(&[
        (descriptor(1, ALICE, 0), vec![entry(b"k1", b"one")]),
        (descriptor(2, BOB, 0), vec![]),
        (descriptor(3, DAVE, 1), vec![]),
    ]);
    assert_eq!(observe(&branch), after_tx1, "state after rolling back tx2");
    assert_eq!(value(&branch, a, b"k1"), Some(b"one".to_vec()));
    assert_eq!(value(&branch, b, b"k2"), None);
    assert_eq!(branch.desc(c), Ok(descriptor(3, DAVE, 1)));
    assert_eq!(branch.desc(ns(4)), Err(Rejection::UnknownNamespace));
    assert_eq!(branch.receipts().len(), 6, "receipts are never withdrawn");
    assert_eq!(branch.spent(), Cost::new(144), "spent is never restored");
    assert_eq!(branch.remaining(), Cost::new(243 - 144));

    // The released ordinal is handed out again.
    let created = apply(
        &mut branch,
        ALICE,
        WriteOp::CreateNamespace {
            name: arkiv_db::NamespaceName::new("test").unwrap(),
        },
    );
    assert_eq!(created.effect, Effect::NamespaceCreated(ns(4)));
    assert_eq!(created.receipt, applied(53));
    assert_eq!(branch.desc(ns(4)), Ok(descriptor(4, ALICE, 0)));

    // The rollback target stays valid. The range always starts at the
    // checkpoint's receipt position: it names every receipt issued since the
    // checkpoint, including those the first rollback to cp2 already covered.
    assert_eq!(branch.rollback_to(cp2), Ok(2..7));
    assert_eq!(
        observe(&branch),
        after_tx1,
        "state after the second rollback to cp2"
    );
    assert_eq!(branch.spent(), Cost::new(197));

    // Outer rollback: everything from tx1 is gone as well.
    assert_eq!(branch.rollback_to(cp1), Ok(0..7));
    let base_view = expected_view(&[
        (descriptor(1, ALICE, 0), vec![]),
        (descriptor(2, BOB, 0), vec![]),
        (descriptor(3, CAROL, 0), vec![]),
    ]);
    assert_eq!(
        observe(&branch),
        base_view,
        "state after rolling back to cp1"
    );
    assert_eq!(observe(&engine.head_snapshot()), base_view);
    let transferred = apply(&mut branch, CAROL, transfer(c, 0, DAVE));
    assert_eq!(
        transferred.effect,
        Effect::Transferred {
            revision: Revision(1)
        }
    );
    assert_eq!(transferred.receipt, applied(23));
    assert_eq!(branch.spent(), Cost::new(220));

    // cp2 was rolled past and is now invalid; the failure changes nothing.
    let before = observe(&branch);
    assert_eq!(branch.rollback_to(cp2), Err(EngineError::InvalidCheckpoint));
    assert_eq!(observe(&branch), before);
    assert_eq!(branch.receipts().len(), 8);

    // remaining() is 23: an op costing exactly 23 applies and drains the block.
    assert_eq!(branch.remaining(), Cost::new(23));
    let exact = branch
        .execute(BOB, put(b, b"k2", b"beta"), Budget::limit(23))
        .expect("an operation costing exactly the remaining budget applies");
    assert_eq!(exact.receipt, applied(23));
    assert_eq!(branch.remaining(), Cost::ZERO);
    assert_eq!(branch.spent(), Cost::new(243));
    let admission = reject(&mut branch, BOB, put(b, b"k3", b""), Budget::limit(1));
    assert_eq!(
        admission.receipt,
        rejected(
            0,
            Rejection::ExceedsBlockBudget {
                remaining: Cost::ZERO
            }
        )
    );
    assert_eq!(
        branch.receipts().len(),
        9,
        "admission rejections are not recorded"
    );

    let sealed = branch.seal();
    assert_eq!(sealed.spent(), Cost::new(243));
    let id = engine.publish(&sealed).expect("candidate publishes");
    let snapshot = engine.snapshot(id).expect("published");
    assert_eq!(
        observe(&snapshot),
        expected_view(&[
            (descriptor(1, ALICE, 0), vec![]),
            (descriptor(2, BOB, 0), vec![entry(b"k2", b"beta")]),
            (descriptor(3, DAVE, 1), vec![]),
        ]),
        "only the work after the last rollback is published"
    );
}

// ---------------------------------------------------------------------------
// 3. ABA safety
// ---------------------------------------------------------------------------

#[test]
fn a_checkpoint_rolled_past_stays_invalid_when_its_slot_is_reused() {
    let engine = engine_with_namespaces(&[ALICE]);
    let a = ns(1);
    let mut branch = engine.begin(engine.head(), None).expect("head");

    let cp1 = branch.checkpoint();
    apply(&mut branch, ALICE, put(a, b"k", b"one")); // receipt 0
    let cp2 = branch.checkpoint();
    apply(&mut branch, ALICE, put(a, b"k", b"two")); // receipt 1
    assert_eq!(branch.rollback_to(cp1), Ok(0..2));
    assert_eq!(value(&branch, a, b"k"), None);

    apply(&mut branch, ALICE, put(a, b"k", b"three")); // receipt 2
    // cp3 occupies the stack index cp2 used to have.
    let cp3 = branch.checkpoint();
    assert_ne!(cp2, cp3, "a reissued checkpoint is a distinct handle");
    apply(&mut branch, ALICE, put(a, b"k", b"four")); // receipt 3

    let before = observe(&branch);
    assert_eq!(branch.rollback_to(cp2), Err(EngineError::InvalidCheckpoint));
    assert_eq!(
        observe(&branch),
        before,
        "a rejected rollback changes nothing"
    );
    assert_eq!(value(&branch, a, b"k"), Some(b"four".to_vec()));

    assert_eq!(branch.rollback_to(cp3), Ok(3..4));
    assert_eq!(value(&branch, a, b"k"), Some(b"three".to_vec()));

    // The outer checkpoint is still valid; rolling back to it invalidates cp3.
    assert_eq!(branch.rollback_to(cp1), Ok(0..4));
    assert_eq!(value(&branch, a, b"k"), None);
    assert_eq!(branch.rollback_to(cp3), Err(EngineError::InvalidCheckpoint));
    assert_eq!(branch.rollback_to(cp2), Err(EngineError::InvalidCheckpoint));
    // A repeated rollback reports the whole span since cp1 again.
    assert_eq!(branch.rollback_to(cp1), Ok(0..4));
    assert_eq!(value(&branch, a, b"k"), None);
}

// ---------------------------------------------------------------------------
// 4. Cross-branch checkpoints
// ---------------------------------------------------------------------------

#[test]
fn a_checkpoint_from_another_branch_is_invalid() {
    let engine = engine_with_namespaces(&[ALICE]);
    let a = ns(1);
    let mut x = engine.begin(engine.head(), None).expect("head");
    let mut y = engine.begin(engine.head(), None).expect("head");
    let cp_x = x.checkpoint();
    let cp_y = y.checkpoint();
    apply(&mut x, ALICE, put(a, b"k", b"x"));
    apply(&mut y, ALICE, put(a, b"k", b"y"));

    let before = observe(&y);
    assert_eq!(y.rollback_to(cp_x), Err(EngineError::InvalidCheckpoint));
    assert_eq!(observe(&y), before, "y is unchanged by x's checkpoint");
    assert_eq!(x.rollback_to(cp_y), Err(EngineError::InvalidCheckpoint));
    assert_eq!(value(&x, a, b"k"), Some(b"x".to_vec()));

    // Each branch still honours its own checkpoint, independently.
    assert_eq!(y.rollback_to(cp_y), Ok(0..1));
    assert_eq!(value(&y, a, b"k"), None);
    assert_eq!(value(&x, a, b"k"), Some(b"x".to_vec()));
    assert_eq!(x.rollback_to(cp_x), Ok(0..1));
    assert_eq!(value(&x, a, b"k"), None);
}

#[test]
fn a_checkpoint_from_another_engines_branch_is_invalid() {
    // Both engines have the same history, so both branches are the second
    // branch of their engine, both checkpoints are the first of their branch
    // and both sit at stack index 0: only the branch identity tells them
    // apart, which a per-engine branch counter would not.
    let engine = engine_with_namespaces(&[ALICE]);
    let other = engine_with_namespaces(&[ALICE]);
    let a = ns(1);
    let mut branch = engine.begin(engine.head(), None).expect("head");
    let mut foreign = other.begin(other.head(), None).expect("head");
    let own = branch.checkpoint();
    let cp_foreign = foreign.checkpoint();
    apply(&mut branch, ALICE, put(a, b"k", b"mine"));
    apply(&mut foreign, ALICE, put(a, b"k", b"theirs"));

    let before = observe(&branch);
    assert_eq!(
        branch.rollback_to(cp_foreign),
        Err(EngineError::InvalidCheckpoint),
        "a checkpoint from a branch of another engine must be rejected"
    );
    assert_eq!(
        observe(&branch),
        before,
        "a rejected rollback changes nothing"
    );
    assert_eq!(value(&branch, a, b"k"), Some(b"mine".to_vec()));
    assert_eq!(branch.rollback_to(own), Ok(0..1));
    assert_eq!(value(&branch, a, b"k"), None);
    assert_eq!(value(&foreign, a, b"k"), Some(b"theirs".to_vec()));
}

// ---------------------------------------------------------------------------
// 5. Rollback restores owner, content, allocator and pending pricing together
// ---------------------------------------------------------------------------

#[test]
fn rollback_restores_owner_records_allocator_and_pending_pricing_together() {
    let mut engine = engine_with_namespaces(&[ALICE, BOB]);
    let (a, b) = (ns(1), ns(2));
    let mut branch = engine.begin(engine.head(), None).expect("head");
    let schedule_2 = PriceSchedule::uniform(2, 7);

    let cp = branch.checkpoint();
    let transferred = apply(&mut branch, ALICE, transfer(a, 0, BOB)); // receipt 0
    assert_eq!(
        transferred.effect,
        Effect::Transferred {
            revision: Revision(1)
        }
    );
    apply(&mut branch, BOB, put(b, b"k", b"v")); // receipt 1
    let created = apply(
        &mut branch,
        ALICE,
        WriteOp::CreateNamespace {
            name: arkiv_db::NamespaceName::new("test").unwrap(),
        },
    ); // receipt 2
    assert_eq!(created.effect, Effect::NamespaceCreated(ns(3)));
    assert_eq!(branch.set_pricing(schedule_2.clone()), Ok(()));
    assert_eq!(
        branch.set_pricing(schedule_2.clone()),
        Err(EngineError::StaleSchedule { current: 2 }),
        "precondition: schedule 2 is pending"
    );
    assert_eq!(
        branch.pricing(),
        &PriceSchedule::FIXTURE,
        "pricing stays pinned"
    );
    assert_eq!(
        observe(&branch),
        expected_view(&[
            (descriptor(1, BOB, 1), vec![]),
            (descriptor(2, BOB, 0), vec![entry(b"k", b"v")]),
            (descriptor(3, ALICE, 0), vec![]),
        ]),
        "precondition: every change is visible before the rollback"
    );

    assert_eq!(branch.rollback_to(cp), Ok(0..3));
    assert_eq!(
        branch.desc(a),
        Ok(descriptor(1, ALICE, 0)),
        "owner and revision restored"
    );
    assert_eq!(value(&branch, b, b"k"), None, "record removed");
    assert_eq!(branch.namespace_count(), 2);
    assert_eq!(branch.desc(ns(3)), Err(Rejection::UnknownNamespace));
    assert_eq!(
        branch.set_pricing(schedule_2.clone()),
        Ok(()),
        "the pending pricing was restored to id 1, so id 2 is fresh again"
    );
    assert_eq!(branch.pricing(), &PriceSchedule::FIXTURE);

    // Roll back again (the target stays valid; the range restarts at the
    // checkpoint's receipt position) so the published result carries the base
    // pricing, then prove the allocator and the owner were restored.
    assert_eq!(branch.rollback_to(cp), Ok(0..3));
    let created = apply(
        &mut branch,
        CAROL,
        WriteOp::CreateNamespace {
            name: arkiv_db::NamespaceName::new("test").unwrap(),
        },
    );
    assert_eq!(
        created.effect,
        Effect::NamespaceCreated(ns(3)),
        "the released ordinal is reused"
    );
    let transferred = apply(&mut branch, ALICE, transfer(a, 0, CAROL));
    assert_eq!(
        transferred.effect,
        Effect::Transferred {
            revision: Revision(1)
        }
    );

    let id = engine.publish(&branch.seal()).expect("candidate publishes");
    let snapshot = engine.snapshot(id).expect("published");
    assert_eq!(snapshot.pricing().id(), 1);
    assert_eq!(snapshot.pricing(), &PriceSchedule::FIXTURE);
    assert_eq!(
        observe(&snapshot),
        expected_view(&[
            (descriptor(1, CAROL, 1), vec![]),
            (descriptor(2, BOB, 0), vec![]),
            (descriptor(3, CAROL, 0), vec![]),
        ])
    );
    let next = engine.begin(engine.head(), None).expect("head");
    assert_eq!(
        next.pricing().id(),
        1,
        "the next branch still pins schedule 1"
    );
}

#[test]
fn rollback_restores_the_pending_pricing_recorded_at_the_checkpoint() {
    let mut engine = engine_with_namespaces(&[ALICE]);
    let mut branch = engine.begin(engine.head(), None).expect("head");

    assert_eq!(branch.set_pricing(PriceSchedule::uniform(2, 7)), Ok(()));
    let cp = branch.checkpoint();
    assert_eq!(branch.set_pricing(PriceSchedule::uniform(3, 8)), Ok(()));
    assert_eq!(
        branch.set_pricing(PriceSchedule::uniform(3, 8)),
        Err(EngineError::StaleSchedule { current: 3 })
    );

    assert_eq!(branch.rollback_to(cp), Ok(0..0));
    assert_eq!(
        branch.set_pricing(PriceSchedule::uniform(2, 9)),
        Err(EngineError::StaleSchedule { current: 2 }),
        "schedule 2, pending at the checkpoint, is back"
    );
    assert_eq!(branch.set_pricing(PriceSchedule::uniform(3, 8)), Ok(()));
    assert_eq!(branch.rollback_to(cp), Ok(0..0));

    let id = engine.publish(&branch.seal()).expect("candidate publishes");
    let snapshot = engine.snapshot(id).expect("published");
    assert_eq!(snapshot.pricing(), &PriceSchedule::uniform(2, 7));
    assert_eq!(
        engine
            .begin(engine.head(), None)
            .expect("head")
            .pricing()
            .id(),
        2
    );
}

// ---------------------------------------------------------------------------
// 6. Receipts survive rollback and discard
// ---------------------------------------------------------------------------

#[test]
fn receipts_and_spent_survive_rollback_and_a_discarded_candidate() {
    // Block limit 1000. Receipts, in order:
    //
    // | # | operation                                    | cost | running spent |
    // | 0 | put A/k1 = "one"      1 + 10 + 2*2 + 3 + 4   |  22  |  22           | cp
    // | 1 | put A/k1 = "two", traced (same charges)      |  22  |  44           |
    // | 2 | delete A/k1           1 + 5 + 2*2 + 4        |  14  |  58           |
    // | 3 | create -> ns 3        1 + 50 + 2             |  53  | 111           |
    // | 4 | put A/k9 = "v" by bob -> Unauthorized        |   1  | 112           |
    // | 5 | transfer A rev 3 -> StaleRevision            |   1  | 113           |
    // | 6 | put A/k1 = "three", limit 3 -> OutOfBudget   |   1  | 114           |
    // | - | put A/z = "z", limit 887 > remaining 886     |   0  | not recorded  |
    let mut engine = engine_with_namespaces(&[ALICE, BOB]);
    let a = ns(1);
    let mut branch = engine
        .begin(engine.head(), Some(Cost::new(1_000)))
        .expect("head");

    apply(&mut branch, ALICE, put(a, b"k1", b"one"));
    let cp = branch.checkpoint();
    branch
        .execute(ALICE, put(a, b"k1", b"two"), Budget::unlimited().traced())
        .expect("traced put applies");
    apply(&mut branch, ALICE, delete(a, b"k1"));
    apply(
        &mut branch,
        ALICE,
        WriteOp::CreateNamespace {
            name: arkiv_db::NamespaceName::new("test").unwrap(),
        },
    );
    reject(&mut branch, BOB, put(a, b"k9", b"v"), Budget::unlimited());
    reject(&mut branch, ALICE, transfer(a, 3, BOB), Budget::unlimited());
    reject(
        &mut branch,
        ALICE,
        put(a, b"k1", b"three"),
        Budget::limit(3),
    );
    let admission = reject(&mut branch, ALICE, put(a, b"z", b"z"), Budget::limit(887));
    assert_eq!(
        admission.receipt,
        rejected(
            0,
            Rejection::ExceedsBlockBudget {
                remaining: Cost::new(886)
            }
        )
    );

    let ledger_entry = |class, count, unit_price, subtotal| LedgerEntry {
        class,
        count,
        unit_price,
        subtotal: Cost::new(subtotal),
    };
    let expected = vec![
        applied(22),
        Receipt {
            ledger: Some(Ledger {
                entries: vec![
                    ledger_entry(OperationClass::DirectoryLookup, 1, 1, 1),
                    ledger_entry(OperationClass::PutRecord, 1, 10, 10),
                    ledger_entry(OperationClass::WriteKeyByte, 2, 2, 4),
                    ledger_entry(OperationClass::WriteValueByte, 3, 1, 3),
                    ledger_entry(OperationClass::RootUpdate, 2, 2, 4),
                ],
            }),
            ..applied(22)
        },
        applied(14),
        applied(53),
        rejected(1, Rejection::Unauthorized),
        rejected(
            1,
            Rejection::StaleRevision {
                expected: Revision(3),
                current: Revision(0),
            },
        ),
        rejected(1, Rejection::OutOfBudget),
    ];
    let before = branch.receipts().to_vec();
    assert_eq!(before, expected);
    assert_eq!(branch.spent(), Cost::new(114));

    assert_eq!(branch.rollback_to(cp), Ok(1..7));
    assert_eq!(
        branch.receipts(),
        before.as_slice(),
        "receipts are never withdrawn"
    );
    assert_eq!(format!("{:?}", branch.receipts()), format!("{before:?}"));
    assert_eq!(branch.spent(), Cost::new(114));
    assert_eq!(branch.remaining(), Cost::new(886));
    assert_eq!(value(&branch, a, b"k1"), Some(b"one".to_vec()));
    assert_eq!(branch.namespace_count(), 2);

    let sealed = branch.seal();
    assert_eq!(sealed.receipts(), before.as_slice());
    assert_eq!(sealed.spent(), Cost::new(114));

    // Discard the candidate by publishing a competitor first; the stale
    // candidate keeps its receipts.
    let competitor = engine.begin(engine.head(), None).expect("head").seal();
    let base = sealed.base();
    let head = engine.publish(&competitor).expect("competitor publishes");
    assert_eq!(
        engine.publish(&sealed),
        Err(EngineError::StaleHead {
            expected: base,
            head
        })
    );
    assert_eq!(sealed.receipts(), before.as_slice());
    assert_eq!(sealed.spent(), Cost::new(114));
    assert_eq!(
        engine.head_snapshot().value(a, b"k1"),
        Ok(None),
        "nothing from the discarded candidate was published"
    );
}

// ---------------------------------------------------------------------------
// 7. Rollback composes with rejected operations and with nothing at all
// ---------------------------------------------------------------------------

#[test]
fn rollback_after_a_rejected_op_matches_rollback_alone() {
    let engine = engine_with_namespaces(&[ALICE]);
    let a = ns(1);
    let base = observe(&engine.head_snapshot());

    // put A/k = "one": 1 + 10 + 2*1 + 3 + 4 = 20; the unauthorized put spends 1.
    let mut with_rejection = engine.begin(engine.head(), None).expect("head");
    let cp = with_rejection.checkpoint();
    apply(&mut with_rejection, ALICE, put(a, b"k", b"one"));
    let rejection = reject(
        &mut with_rejection,
        BOB,
        put(a, b"k", b"two"),
        Budget::unlimited(),
    );
    assert_eq!(rejection.receipt, rejected(1, Rejection::Unauthorized));
    assert_eq!(with_rejection.rollback_to(cp), Ok(0..2));

    let mut plain = engine.begin(engine.head(), None).expect("head");
    let cp = plain.checkpoint();
    apply(&mut plain, ALICE, put(a, b"k", b"one"));
    assert_eq!(plain.rollback_to(cp), Ok(0..1));

    assert_eq!(observe(&with_rejection), observe(&plain));
    assert_eq!(observe(&plain), base);
    assert_eq!(with_rejection.spent(), Cost::new(21));
    assert_eq!(plain.spent(), Cost::new(20));

    // A checkpoint taken right before a rejection covers just that receipt.
    let cp = plain.checkpoint();
    reject(&mut plain, BOB, put(a, b"k", b"two"), Budget::unlimited());
    assert_eq!(plain.rollback_to(cp), Ok(1..2));
    assert_eq!(observe(&plain), base);
    assert_eq!(plain.spent(), Cost::new(21));
}

#[test]
fn rollback_with_nothing_since_the_checkpoint_is_a_no_op() {
    let engine = engine_with_namespaces(&[ALICE]);
    let a = ns(1);
    let mut branch = engine
        .begin(engine.head(), Some(Cost::new(500)))
        .expect("head");
    apply(&mut branch, ALICE, put(a, b"k", b"one")); // receipt 0, cost 20

    let cp = branch.checkpoint();
    let before = observe(&branch);
    let receipts = branch.receipts().to_vec();
    assert_eq!(branch.rollback_to(cp), Ok(1..1));
    assert_eq!(observe(&branch), before);
    assert_eq!(branch.receipts(), receipts.as_slice());
    assert_eq!(branch.spent(), Cost::new(20));
    assert_eq!(branch.remaining(), Cost::new(480));
    assert_eq!(
        branch.rollback_to(cp),
        Ok(1..1),
        "still a no-op the second time"
    );
    assert_eq!(observe(&branch), before);

    // The checkpoint keeps working once there is something to roll back.
    apply(&mut branch, ALICE, put(a, b"k", b"two")); // receipt 1
    assert_eq!(branch.rollback_to(cp), Ok(1..2));
    assert_eq!(observe(&branch), before);
    assert_eq!(value(&branch, a, b"k"), Some(b"one".to_vec()));
}

// ---------------------------------------------------------------------------
// 8. Rolled-back work never reaches the published snapshot
// ---------------------------------------------------------------------------

#[test]
fn published_snapshot_omits_rolled_back_ops_and_keeps_later_ones() {
    // | # | operation                                     | cost |
    // | 0 | put A/before = "1"      1 + 10 + 2*6 + 1 + 4  |  28  |
    // |   | cp                                            |      |
    // | 1 | put A/rolled = "2"      1 + 10 + 2*6 + 1 + 4  |  28  |
    // | 2 | put B/rolled = "3"      1 + 10 + 2*6 + 1 + 4  |  28  |
    // | 3 | delete A/before         1 + 5 + 2*6 + 4       |  22  |
    // | 4 | create -> ns 3 (bob)    1 + 50 + 2            |  53  |
    // | 5 | transfer A alice -> bob 1 + 20 + 2            |  23  |
    // |   | rollback_to(cp)                               |      |
    // | 6 | put A/after = "4"       1 + 10 + 2*5 + 1 + 4  |  26  |
    // | 7 | create -> ns 3 (carol)                        |  53  |
    // | 8 | put B/after = "5"       1 + 10 + 2*5 + 1 + 4  |  26  |
    // total 28 + 28 + 28 + 22 + 53 + 23 + 26 + 53 + 26 = 287
    let mut engine = engine_with_namespaces(&[ALICE, BOB]);
    let (a, b) = (ns(1), ns(2));
    let base = engine.head_snapshot();
    let mut branch = engine.begin(engine.head(), None).expect("head");

    apply(&mut branch, ALICE, put(a, b"before", b"1"));
    let cp = branch.checkpoint();
    apply(&mut branch, ALICE, put(a, b"rolled", b"2"));
    apply(&mut branch, BOB, put(b, b"rolled", b"3"));
    assert_eq!(
        apply(&mut branch, ALICE, delete(a, b"before")).effect,
        Effect::Deleted { existed: true }
    );
    assert_eq!(
        apply(
            &mut branch,
            BOB,
            WriteOp::CreateNamespace {
                name: arkiv_db::NamespaceName::new("test").unwrap()
            }
        )
        .effect,
        Effect::NamespaceCreated(ns(3))
    );
    apply(&mut branch, ALICE, transfer(a, 0, BOB));
    assert_eq!(branch.rollback_to(cp), Ok(1..6));

    // Alice still owns A and ordinal 3 is free again.
    apply(&mut branch, ALICE, put(a, b"after", b"4"));
    assert_eq!(
        apply(
            &mut branch,
            CAROL,
            WriteOp::CreateNamespace {
                name: arkiv_db::NamespaceName::new("test").unwrap()
            }
        )
        .effect,
        Effect::NamespaceCreated(ns(3))
    );
    apply(&mut branch, BOB, put(b, b"after", b"5"));

    let sealed = branch.seal();
    assert_eq!(sealed.receipts().len(), 9);
    assert_eq!(sealed.spent(), Cost::new(287));
    let id = engine.publish(&sealed).expect("candidate publishes");
    let snapshot = engine.snapshot(id).expect("published");
    assert_eq!(snapshot.base(), Some(base.id()));

    let published = expected_view(&[
        (
            descriptor(1, ALICE, 0),
            vec![entry(b"after", b"4"), entry(b"before", b"1")],
        ),
        (descriptor(2, BOB, 0), vec![entry(b"after", b"5")]),
        (descriptor(3, CAROL, 0), vec![]),
    ]);
    assert_eq!(observe(&snapshot), published);
    assert_eq!(observe(&engine.head_snapshot()), published);
    assert_eq!(value(&snapshot, a, b"rolled"), None);
    assert_eq!(value(&snapshot, b, b"rolled"), None);
    assert_eq!(snapshot.record_count(a), Some(2));
    assert_eq!(snapshot.record_count(b), Some(1));
    assert_eq!(snapshot.record_count(ns(3)), Some(0));

    // The base snapshot is untouched by any of it.
    assert_eq!(
        observe(&base),
        expected_view(&[
            (descriptor(1, ALICE, 0), vec![]),
            (descriptor(2, BOB, 0), vec![]),
        ])
    );
}
