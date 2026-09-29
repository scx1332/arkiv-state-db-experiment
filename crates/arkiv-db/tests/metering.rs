//! Hand-counted metering vectors for the milestone-1 engine: per-operation
//! charge points under every budget, ledgers, charge ordering, block budgets,
//! overflow, estimation parity, determinism and pricing pinning.
//!
//! Every expected number in this file is a literal derived by hand from the
//! FIXTURE weights (DirectoryLookup 1, NamespaceCreate 50, NamespaceTransfer
//! 20, PutRecord 10, DeleteRecord 5, WriteKeyByte 2, WriteValueByte 1,
//! PointRead 3, ScanSeek 4, ScanEntry 2, ReadByte 1, RootUpdate 2) and the
//! model-version-3 charge points of the raw operations: the version-1 points
//! plus `RootUpdate` 1 in the namespace create/transfer group and
//! `RootUpdate` 2 in the put/delete group. Nothing here asks the engine or a
//! schedule what an operation should cost.

use std::fmt::Debug;

use arkiv_db::{
    Branch, Budget, Cost, Effect, Engine, EngineError, Entry, Ledger, LedgerEntry,
    NamespaceDescriptor, NamespaceId, OperationClass, Outcome, OwnerId, PriceSchedule, ReadResult,
    Receipt, ReceiptKind, Rejection, Revision, SnapshotId, WriteOp, WriteResult,
};

const ALICE: OwnerId = OwnerId::new([0xa1; 20]);
const BOB: OwnerId = OwnerId::new([0xb0; 20]);

/// Fixture records; the names carry the byte lengths used in the derivations.
const K3: &[u8] = b"abc";
const V4: &[u8] = b"wxyz";
const K3B: &[u8] = b"abd";
const V1: &[u8] = b"q";

/// Counting-rule version recorded in every receipt.
const MODEL_VERSION: u32 = 3;
/// The id `PriceSchedule::FIXTURE` carries.
const FIXTURE_ID: u32 = 1;

const OUT_OF_BUDGET: Outcome = Outcome::Rejected(Rejection::OutOfBudget);
const COST_OVERFLOW: Outcome = Outcome::Rejected(Rejection::CostOverflow);

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn ns_id(ordinal: u64) -> NamespaceId {
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

fn receipt(kind: ReceiptKind, schedule_id: u32, spent: u64, outcome: Outcome) -> Receipt {
    Receipt {
        kind,
        model_version: MODEL_VERSION,
        schedule_id,
        spent: Cost::new(spent),
        outcome,
        ledger: None,
    }
}

fn write_receipt(spent: u64, outcome: Outcome) -> Receipt {
    receipt(ReceiptKind::Write, FIXTURE_ID, spent, outcome)
}

fn read_receipt(spent: u64, outcome: Outcome) -> Receipt {
    receipt(ReceiptKind::Read, FIXTURE_ID, spent, outcome)
}

/// Ledger lines as `(class, count, unit_price, subtotal)`.
fn ledger(lines: &[(OperationClass, u64, u64, u64)]) -> Ledger {
    Ledger {
        entries: lines
            .iter()
            .map(|&(class, count, unit_price, subtotal)| LedgerEntry {
                class,
                count,
                unit_price,
                subtotal: Cost::new(subtotal),
            })
            .collect(),
    }
}

fn traced(receipt: Receipt, lines: &[(OperationClass, u64, u64, u64)]) -> Receipt {
    Receipt {
        ledger: Some(ledger(lines)),
        ..receipt
    }
}

fn without_ledger(receipt: &Receipt) -> Receipt {
    Receipt {
        ledger: None,
        ..receipt.clone()
    }
}

/// The canonical part of a write result: effect or reason plus the receipt.
/// The physical trace is deliberately dropped.
type Canonical = Result<(Effect, Receipt), (Rejection, Receipt)>;

fn canonical(result: &WriteResult) -> Canonical {
    match result {
        Ok(executed) => Ok((executed.effect, executed.receipt.clone())),
        Err(rejected) => Err((rejected.reason, rejected.receipt.clone())),
    }
}

/// The fixture: snapshot 1 holds one namespace (ordinal 1, owned by ALICE)
/// with `K3 -> V4` and `K3B -> V1`, priced with FIXTURE (id 1).
fn fixture() -> (Engine, NamespaceId) {
    let mut engine = Engine::new(PriceSchedule::FIXTURE);
    let mut branch = engine
        .begin(engine.head(), None)
        .expect("genesis is the head");
    let created = branch
        .execute(
            ALICE,
            WriteOp::CreateNamespace {
                name: arkiv_db::NamespaceName::new("test").unwrap(),
            },
            Budget::unlimited(),
        )
        .expect("an unlimited creation applies");
    let Effect::NamespaceCreated(ns) = created.effect else {
        panic!("creation reports the new id, got {:?}", created.effect);
    };
    assert_eq!(ns, ns_id(1), "the first namespace has ordinal 1");
    for (key, value) in [(K3, V4), (K3B, V1)] {
        branch
            .execute(ALICE, put(ns, key, value), Budget::unlimited())
            .expect("fixture puts apply");
    }
    let published = engine
        .publish(&branch.seal())
        .expect("the fixture publishes on genesis");
    assert_eq!(published.sequence(), 1, "the fixture is snapshot 1");
    (engine, ns)
}

fn fresh_branch(engine: &Engine, block_budget: Option<u64>) -> Branch {
    engine
        .begin(engine.head(), block_budget.map(Cost::new))
        .expect("begin on the current head")
}

/// Everything a metered read can observe about one namespace on a branch.
#[derive(Debug, PartialEq, Eq)]
struct Observed {
    namespaces: usize,
    descriptor: NamespaceDescriptor,
    records: Vec<Entry>,
}

fn observe(branch: &Branch, ns: NamespaceId) -> Observed {
    Observed {
        namespaces: branch.namespace_count(),
        descriptor: branch
            .descriptor(ns, Budget::unlimited())
            .expect("the fixture namespace exists")
            .value,
        records: branch
            .scan_prefix(ns, b"", Budget::unlimited())
            .expect("the fixture namespace exists")
            .value,
    }
}

fn fixture_descriptor(ns: NamespaceId) -> NamespaceDescriptor {
    NamespaceDescriptor {
        name: arkiv_db::NamespaceName::new("test").unwrap(),
        id: ns,
        owner: ALICE,
        revision: Revision(0),
    }
}

fn fixture_records() -> Vec<Entry> {
    vec![entry(K3, V4), entry(K3B, V1)]
}

/// What an operation has spent when its budget `b` runs out: the largest
/// cumulative charge point that still fits `b` (zero before the first one).
fn largest_prefix_sum_at_most(prefix_sums: &[u64], budget: u64) -> u64 {
    prefix_sums
        .iter()
        .copied()
        .filter(|&point| point <= budget)
        .max()
        .unwrap_or(0)
}

/// Runs `op` on a fresh branch of the fixture under every budget in
/// `0..=total` and asserts the whole receipt, the effect on success, and an
/// unchanged state on rejection. `prefix_sums` are the cumulative charge
/// points; the last one is the total.
fn assert_write_vector(
    engine: &Engine,
    ns: NamespaceId,
    name: &str,
    op: &WriteOp,
    prefix_sums: &[u64],
    effect: Effect,
) {
    let total = *prefix_sums.last().expect("a vector has a total");
    let baseline = observe(&fresh_branch(engine, None), ns);
    for budget in 0..=total {
        let mut branch = fresh_branch(engine, None);
        let result = branch.execute(ALICE, op.clone(), Budget::limit(budget));
        let expected_spent = if budget >= total {
            total
        } else {
            largest_prefix_sum_at_most(prefix_sums, budget)
        };
        match result {
            Ok(executed) => {
                assert!(
                    budget >= total,
                    "{name}: applied at budget {budget} below the total {total}"
                );
                assert_eq!(executed.effect, effect, "{name}: effect at budget {budget}");
                assert_eq!(
                    executed.receipt,
                    write_receipt(total, Outcome::Applied),
                    "{name}: receipt at budget {budget}"
                );
            }
            Err(rejected) => {
                assert!(
                    budget < total,
                    "{name}: rejected at budget {budget} >= total {total}: {rejected}"
                );
                assert_eq!(
                    rejected.reason,
                    Rejection::OutOfBudget,
                    "{name}: reason at budget {budget}"
                );
                assert_eq!(
                    rejected.receipt,
                    write_receipt(expected_spent, OUT_OF_BUDGET),
                    "{name}: receipt at budget {budget}"
                );
                assert_eq!(
                    observe(&branch, ns),
                    baseline,
                    "{name}: state changed after a rejection at budget {budget}"
                );
            }
        }
        assert_eq!(
            branch.receipts().len(),
            1,
            "{name}: one receipt is recorded at budget {budget}"
        );
        assert_eq!(
            branch.spent(),
            Cost::new(expected_spent),
            "{name}: block spent at budget {budget}"
        );
    }
    let mut branch = fresh_branch(engine, None);
    let executed = branch
        .execute(ALICE, op.clone(), Budget::unlimited())
        .unwrap_or_else(|rejected| panic!("{name}: unlimited budget rejected: {rejected}"));
    assert_eq!(executed.effect, effect, "{name}: unlimited effect");
    assert_eq!(
        executed.receipt,
        write_receipt(total, Outcome::Applied),
        "{name}: unlimited receipt"
    );
}

/// Runs a read under every budget in `0..=total` plus unlimited and asserts
/// the whole receipt and, on success, the returned value.
fn assert_read_vector<T: Debug + PartialEq>(
    name: &str,
    prefix_sums: &[u64],
    expected: &T,
    read: impl Fn(Budget) -> ReadResult<T>,
) {
    let total = *prefix_sums.last().expect("a vector has a total");
    let budgets = (0..=total).map(Budget::limit).chain([Budget::unlimited()]);
    for budget in budgets {
        let limit = budget.limit_units().map_or(u64::MAX, Cost::units);
        match read(budget) {
            Ok(metered) => {
                assert!(
                    limit >= total,
                    "{name}: applied at budget {limit} below the total {total}"
                );
                assert_eq!(&metered.value, expected, "{name}: value at budget {limit}");
                assert_eq!(
                    metered.receipt,
                    read_receipt(total, Outcome::Applied),
                    "{name}: receipt at budget {limit}"
                );
            }
            Err(rejected) => {
                assert!(
                    limit < total,
                    "{name}: rejected at budget {limit} >= total {total}: {rejected}"
                );
                let spent = largest_prefix_sum_at_most(prefix_sums, limit);
                assert_eq!(
                    rejected.receipt,
                    read_receipt(spent, OUT_OF_BUDGET),
                    "{name}: receipt at budget {limit}"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 1. Per-operation vectors under every budget
// ---------------------------------------------------------------------------

#[test]
fn put_replacing_a_record_charges_lookup_then_the_record_group() {
    let (engine, ns) = fixture();
    // Put(k3, v4): DirectoryLookup 1 -> 1; group PutRecord 10 + WriteKeyByte
    // 3*2 + WriteValueByte 4*1 + RootUpdate 2*2 = 24 -> 25.
    assert_write_vector(
        &engine,
        ns,
        "put replace",
        &put(ns, K3, V4),
        &[1, 25],
        Effect::Put { replaced: true },
    );
}

#[test]
fn put_inserting_a_record_charges_the_same_and_reports_no_replacement() {
    let (engine, ns) = fixture();
    // Put("abe", v4): identical lengths to Put(k3, v4), so 1 then 1 + 24 = 25.
    assert_write_vector(
        &engine,
        ns,
        "put insert",
        &put(ns, b"abe", V4),
        &[1, 25],
        Effect::Put { replaced: false },
    );
}

#[test]
fn delete_of_a_present_key_charges_lookup_then_the_delete_group() {
    let (engine, ns) = fixture();
    // Delete(k3): DirectoryLookup 1 -> 1; group DeleteRecord 5 + WriteKeyByte
    // 3*2 + RootUpdate 2*2 = 15 -> 16.
    assert_write_vector(
        &engine,
        ns,
        "delete present",
        &delete(ns, K3),
        &[1, 16],
        Effect::Deleted { existed: true },
    );
}

#[test]
fn delete_of_a_missing_key_costs_the_same_and_reports_existed_false() {
    let (engine, ns) = fixture();
    // Delete("zzz"): same 3-byte key, so 1 then 1 + 5 + 6 + 4 = 16; nothing
    // to remove, and RootUpdate is charged all the same.
    assert_write_vector(
        &engine,
        ns,
        "delete missing",
        &delete(ns, b"zzz"),
        &[1, 16],
        Effect::Deleted { existed: false },
    );
}

#[test]
fn transfer_charges_lookup_then_the_transfer_group() {
    let (engine, ns) = fixture();
    // Transfer: DirectoryLookup 1 -> 1; group NamespaceTransfer 20 +
    // RootUpdate 1*2 = 22 -> 23.
    assert_write_vector(
        &engine,
        ns,
        "transfer",
        &transfer(ns, 0, BOB),
        &[1, 23],
        Effect::Transferred {
            revision: Revision(1),
        },
    );
}

#[test]
fn create_namespace_charges_lookup_then_the_create_group() {
    let (engine, _ns) = fixture();
    // CreateNamespace: DirectoryLookup 1 -> 1; group NamespaceCreate 50 +
    // RootUpdate 1*2 = 52 -> 53. The fixture holds ordinal 1, so the new
    // namespace is ordinal 2.
    assert_write_vector(
        &engine,
        ns_id(1),
        "create",
        &WriteOp::CreateNamespace {
            name: arkiv_db::NamespaceName::new("test").unwrap(),
        },
        &[1, 53],
        Effect::NamespaceCreated(ns_id(2)),
    );
}

#[test]
fn get_of_a_present_value_charges_lookup_point_read_then_value_bytes() {
    let (engine, ns) = fixture();
    let snapshot = engine.head_snapshot();
    let branch = fresh_branch(&engine, None);
    // get(k3): DirectoryLookup 1 -> 1; PointRead 3 -> 4; ReadByte 4*1 -> 8.
    let expected = Some(V4.to_vec());
    assert_read_vector("snapshot get present", &[1, 4, 8], &expected, |budget| {
        snapshot.get(ns, K3, budget)
    });
    assert_read_vector("branch get present", &[1, 4, 8], &expected, |budget| {
        branch.get(ns, K3, budget)
    });
}

#[test]
fn get_of_a_missing_key_charges_lookup_and_point_read_only() {
    let (engine, ns) = fixture();
    let snapshot = engine.head_snapshot();
    let branch = fresh_branch(&engine, None);
    // get("zzz"): DirectoryLookup 1 -> 1; PointRead 3 -> 4; no bytes returned.
    assert_read_vector("snapshot get missing", &[1, 4], &None, |budget| {
        snapshot.get(ns, b"zzz", budget)
    });
    assert_read_vector("branch get missing", &[1, 4], &None, |budget| {
        branch.get(ns, b"zzz", budget)
    });
}

#[test]
fn scan_prefix_charges_seek_then_one_group_per_entry() {
    let (engine, ns) = fixture();
    let snapshot = engine.head_snapshot();
    let branch = fresh_branch(&engine, None);
    // scan("ab"): DirectoryLookup 1 -> 1; ScanSeek 4 -> 5; entry (abc, wxyz):
    // ScanEntry 2 + ReadByte (3 + 4) = 9 -> 14; entry (abd, q): ScanEntry 2 +
    // ReadByte (3 + 1) = 6 -> 20.
    let expected = fixture_records();
    assert_read_vector("snapshot scan", &[1, 5, 14, 20], &expected, |budget| {
        snapshot.scan_prefix(ns, b"ab", budget)
    });
    assert_read_vector("branch scan", &[1, 5, 14, 20], &expected, |budget| {
        branch.scan_prefix(ns, b"ab", budget)
    });
}

#[test]
fn scan_prefix_matching_nothing_charges_lookup_and_seek_only() {
    let (engine, ns) = fixture();
    let snapshot = engine.head_snapshot();
    let branch = fresh_branch(&engine, None);
    // scan("zz"): DirectoryLookup 1 -> 1; ScanSeek 4 -> 5; no entries.
    let expected: Vec<Entry> = Vec::new();
    assert_read_vector("snapshot empty scan", &[1, 5], &expected, |budget| {
        snapshot.scan_prefix(ns, b"zz", budget)
    });
    assert_read_vector("branch empty scan", &[1, 5], &expected, |budget| {
        branch.scan_prefix(ns, b"zz", budget)
    });
}

#[test]
fn descriptor_charges_one_lookup() {
    let (engine, ns) = fixture();
    let snapshot = engine.head_snapshot();
    let branch = fresh_branch(&engine, None);
    // descriptor: DirectoryLookup 1 -> 1.
    let expected = fixture_descriptor(ns);
    assert_read_vector("snapshot descriptor", &[1], &expected, |budget| {
        snapshot.descriptor(ns, budget)
    });
    assert_read_vector("branch descriptor", &[1], &expected, |budget| {
        branch.descriptor(ns, budget)
    });
}

#[test]
fn put_with_empty_key_and_value_costs_fifteen_and_omits_zero_count_classes() {
    let (engine, ns) = fixture();
    // Put("", ""): DirectoryLookup 1 -> 1; group PutRecord 10 + WriteKeyByte
    // 0*2 + WriteValueByte 0*1 + RootUpdate 2*2 = 14 -> 15.
    assert_write_vector(
        &engine,
        ns,
        "put empty",
        &put(ns, b"", b""),
        &[1, 15],
        Effect::Put { replaced: false },
    );
    let mut branch = fresh_branch(&engine, None);
    let executed = branch
        .execute(ALICE, put(ns, b"", b""), Budget::limit(15).traced())
        .expect("an exact budget succeeds");
    assert_eq!(
        executed.receipt,
        traced(
            write_receipt(15, Outcome::Applied),
            &[
                (OperationClass::DirectoryLookup, 1, 1, 1),
                (OperationClass::PutRecord, 1, 10, 10),
                (OperationClass::RootUpdate, 2, 2, 4),
            ]
        ),
        "zero-count byte classes are absent from the ledger"
    );
    let stored = branch
        .get(ns, b"", Budget::unlimited())
        .expect("the empty key is readable");
    assert_eq!(stored.value, Some(Vec::new()));
    // get(""): DirectoryLookup 1; PointRead 3; ReadByte 0 -> 4.
    assert_eq!(stored.receipt, read_receipt(4, Outcome::Applied));
}

// ---------------------------------------------------------------------------
// 2. Ledger vectors
// ---------------------------------------------------------------------------

#[test]
fn traced_put_ledger_lists_every_charged_class_once() {
    let (engine, ns) = fixture();
    let mut branch = fresh_branch(&engine, None);
    let executed = branch
        .execute(ALICE, put(ns, K3, V4), Budget::unlimited().traced())
        .expect("an unlimited put applies");
    // Put(k3, v4): DirectoryLookup 1x1 = 1; PutRecord 1x10 = 10; WriteKeyByte
    // 3x2 = 6; WriteValueByte 4x1 = 4; RootUpdate 2x2 = 4; total 25.
    assert_eq!(
        executed.receipt,
        traced(
            write_receipt(25, Outcome::Applied),
            &[
                (OperationClass::DirectoryLookup, 1, 1, 1),
                (OperationClass::PutRecord, 1, 10, 10),
                (OperationClass::WriteKeyByte, 3, 2, 6),
                (OperationClass::WriteValueByte, 4, 1, 4),
                (OperationClass::RootUpdate, 2, 2, 4),
            ]
        )
    );
    // A rejected traced put keeps only the lookup: the 24-unit group is not
    // charged when 1 + 24 > 24.
    let rejected = branch
        .execute(ALICE, put(ns, K3, V4), Budget::limit(24).traced())
        .expect_err("24 is one short of 25");
    assert_eq!(
        rejected.receipt,
        traced(
            write_receipt(1, OUT_OF_BUDGET),
            &[(OperationClass::DirectoryLookup, 1, 1, 1)]
        )
    );
}

#[test]
fn traced_scan_ledger_aggregates_per_class_in_first_charge_order() {
    let (engine, ns) = fixture();
    let snapshot = engine.head_snapshot();
    let scanned = snapshot
        .scan_prefix(ns, b"ab", Budget::unlimited().traced())
        .expect("an unlimited scan applies");
    // scan("ab"): DirectoryLookup 1x1 = 1; ScanSeek 1x4 = 4; two entries each
    // ScanEntry 1 -> 2x2 = 4; ReadByte (3 + 4) + (3 + 1) = 11x1 = 11; total 20.
    assert_eq!(scanned.value, fixture_records());
    assert_eq!(
        scanned.receipt,
        traced(
            read_receipt(20, Outcome::Applied),
            &[
                (OperationClass::DirectoryLookup, 1, 1, 1),
                (OperationClass::ScanSeek, 1, 4, 4),
                (OperationClass::ScanEntry, 2, 2, 4),
                (OperationClass::ReadByte, 11, 1, 11),
            ]
        )
    );
}

#[test]
fn untraced_receipts_have_no_ledger_and_otherwise_equal_traced_ones() {
    let (engine, ns) = fixture();
    let snapshot = engine.head_snapshot();

    let mut untraced_branch = fresh_branch(&engine, Some(1_000));
    let mut traced_branch = fresh_branch(&engine, Some(1_000));
    let untraced_put = untraced_branch
        .execute(ALICE, put(ns, K3, V4), Budget::limit(25))
        .expect("25 is exact");
    let traced_put = traced_branch
        .execute(ALICE, put(ns, K3, V4), Budget::limit(25).traced())
        .expect("25 is exact");
    assert_eq!(untraced_put.receipt.ledger, None);
    assert!(traced_put.receipt.ledger.is_some());
    assert_eq!(untraced_put.receipt, write_receipt(25, Outcome::Applied));
    assert_eq!(without_ledger(&traced_put.receipt), untraced_put.receipt);
    assert_eq!(traced_put.effect, untraced_put.effect);
    assert_eq!(untraced_branch.spent(), traced_branch.spent());

    let untraced_scan = snapshot
        .scan_prefix(ns, b"ab", Budget::limit(20))
        .expect("20 is exact");
    let traced_scan = snapshot
        .scan_prefix(ns, b"ab", Budget::limit(20).traced())
        .expect("20 is exact");
    assert_eq!(untraced_scan.receipt.ledger, None);
    assert_eq!(untraced_scan.receipt, read_receipt(20, Outcome::Applied));
    assert_eq!(without_ledger(&traced_scan.receipt), untraced_scan.receipt);
    assert_eq!(traced_scan.value, untraced_scan.value);

    // The same holds for rejections: an unknown namespace costs the lookup.
    let untraced_miss = snapshot
        .descriptor(ns_id(9), Budget::unlimited())
        .expect_err("ordinal 9 does not exist");
    let traced_miss = snapshot
        .descriptor(ns_id(9), Budget::unlimited().traced())
        .expect_err("ordinal 9 does not exist");
    assert_eq!(
        untraced_miss.receipt,
        read_receipt(1, Outcome::Rejected(Rejection::UnknownNamespace))
    );
    assert_eq!(
        traced_miss.receipt,
        traced(
            read_receipt(1, Outcome::Rejected(Rejection::UnknownNamespace)),
            &[(OperationClass::DirectoryLookup, 1, 1, 1)]
        )
    );
}

// ---------------------------------------------------------------------------
// 3. Charge ordering
// ---------------------------------------------------------------------------

#[test]
fn unknown_namespace_is_charged_the_lookup_before_it_is_reported() {
    let (engine, _ns) = fixture();
    let unknown = ns_id(99);
    let mut branch = fresh_branch(&engine, None);

    // Budget 0: the 1-unit lookup does not fit, so nothing is resolved.
    let rejected = branch
        .execute(ALICE, put(unknown, K3, V4), Budget::limit(0))
        .expect_err("nothing fits a zero budget");
    assert_eq!(rejected.reason, Rejection::OutOfBudget);
    assert_eq!(rejected.receipt, write_receipt(0, OUT_OF_BUDGET));

    // Budget 1: the lookup is charged and then fails to resolve.
    for budget in [Budget::limit(1), Budget::limit(500), Budget::unlimited()] {
        let rejected = branch
            .execute(ALICE, put(unknown, K3, V4), budget)
            .expect_err("ordinal 99 does not exist");
        assert_eq!(rejected.reason, Rejection::UnknownNamespace, "{budget:?}");
        assert_eq!(
            rejected.receipt,
            write_receipt(1, Outcome::Rejected(Rejection::UnknownNamespace)),
            "{budget:?}"
        );
    }
    // Reads follow the same order.
    let snapshot = engine.head_snapshot();
    let rejected = snapshot
        .get(unknown, K3, Budget::limit(0))
        .expect_err("nothing fits a zero budget");
    assert_eq!(rejected.receipt, read_receipt(0, OUT_OF_BUDGET));
    let rejected = snapshot
        .get(unknown, K3, Budget::limit(1))
        .expect_err("ordinal 99 does not exist");
    assert_eq!(
        rejected.receipt,
        read_receipt(1, Outcome::Rejected(Rejection::UnknownNamespace))
    );
    // Four rejected writes were recorded: 0 + 1 + 1 + 1 = 3 units.
    assert_eq!(branch.receipts().len(), 4);
    assert_eq!(branch.spent(), Cost::new(3));
}

#[test]
fn unauthorized_put_spends_exactly_the_lookup_under_any_budget() {
    let (engine, ns) = fixture();
    let mut branch = fresh_branch(&engine, None);
    let baseline = observe(&branch, ns);
    let budgets = [
        Budget::limit(1),
        Budget::limit(2),
        Budget::limit(25),
        Budget::limit(1_000),
        Budget::unlimited(),
    ];
    for budget in budgets {
        let rejected = branch
            .execute(BOB, put(ns, K3, V4), budget)
            .expect_err("BOB does not own the namespace");
        assert_eq!(rejected.reason, Rejection::Unauthorized, "{budget:?}");
        assert_eq!(
            rejected.receipt,
            write_receipt(1, Outcome::Rejected(Rejection::Unauthorized)),
            "{budget:?}"
        );
    }
    assert_eq!(observe(&branch, ns), baseline);
    // Five rejections at 1 unit each.
    assert_eq!(branch.spent(), Cost::new(5));
    assert_eq!(branch.receipts().len(), 5);
}

#[test]
fn transfer_one_unit_short_spends_only_the_lookup_and_changes_nothing() {
    let (engine, ns) = fixture();
    let mut branch = fresh_branch(&engine, None);
    // Transfer: lookup 1 fits 22; the group NamespaceTransfer 20 +
    // RootUpdate 2 would reach 23 > 22.
    let rejected = branch
        .execute(ALICE, transfer(ns, 0, BOB), Budget::limit(22))
        .expect_err("22 is one short of 23");
    assert_eq!(rejected.reason, Rejection::OutOfBudget);
    assert_eq!(rejected.receipt, write_receipt(1, OUT_OF_BUDGET));
    let descriptor = branch
        .descriptor(ns, Budget::unlimited())
        .expect("the namespace exists")
        .value;
    assert_eq!(descriptor, fixture_descriptor(ns));
}

#[test]
fn scan_aborting_between_entries_keeps_earlier_charges_and_returns_nothing() {
    let (engine, ns) = fixture();
    let snapshot = engine.head_snapshot();
    // Charge points: 1 (lookup), 5 (seek), 14 (abc/wxyz), 20 (abd/q).
    // Budget 14 admits the first entry group exactly; the second (6) does
    // not fit, so the scan stops with 14 spent and no partial result.
    let rejected = snapshot
        .scan_prefix(ns, b"ab", Budget::limit(14).traced())
        .expect_err("the second entry does not fit");
    assert_eq!(rejected.reason, Rejection::OutOfBudget);
    assert_eq!(
        rejected.receipt,
        traced(
            read_receipt(14, OUT_OF_BUDGET),
            &[
                (OperationClass::DirectoryLookup, 1, 1, 1),
                (OperationClass::ScanSeek, 1, 4, 4),
                (OperationClass::ScanEntry, 1, 2, 2),
                (OperationClass::ReadByte, 7, 1, 7),
            ]
        )
    );
    // Budget 19: still one short of the second group; spent stays at 14.
    let rejected = snapshot
        .scan_prefix(ns, b"ab", Budget::limit(19))
        .expect_err("19 is one short of 20");
    assert_eq!(rejected.receipt, read_receipt(14, OUT_OF_BUDGET));
    // Budget 20: exact, both entries.
    let scanned = snapshot
        .scan_prefix(ns, b"ab", Budget::limit(20))
        .expect("20 is exact");
    assert_eq!(scanned.value, fixture_records());
    assert_eq!(scanned.receipt, read_receipt(20, Outcome::Applied));
    // Budget 13: the first entry group (9) does not fit after 5; spent 5.
    let rejected = snapshot
        .scan_prefix(ns, b"ab", Budget::limit(13))
        .expect_err("the first entry does not fit");
    assert_eq!(rejected.receipt, read_receipt(5, OUT_OF_BUDGET));
}

#[test]
fn rejections_are_checked_unknown_then_unauthorized_then_stale_revision() {
    let (engine, ns) = fixture();
    let mut branch = fresh_branch(&engine, None);
    // A wrong actor with a wrong revision is Unauthorized, not StaleRevision.
    let rejected = branch
        .execute(BOB, transfer(ns, 7, BOB), Budget::unlimited())
        .expect_err("BOB is not the owner");
    assert_eq!(rejected.reason, Rejection::Unauthorized);
    assert_eq!(
        rejected.receipt,
        write_receipt(1, Outcome::Rejected(Rejection::Unauthorized))
    );
    // The owner with a wrong revision is StaleRevision, still at 1 unit.
    let rejected = branch
        .execute(ALICE, transfer(ns, 7, BOB), Budget::unlimited())
        .expect_err("revision 7 is not current");
    let stale = Rejection::StaleRevision {
        expected: Revision(7),
        current: Revision(0),
    };
    assert_eq!(rejected.reason, stale);
    assert_eq!(rejected.receipt, write_receipt(1, Outcome::Rejected(stale)));
    // An unknown namespace wins over everything else.
    let rejected = branch
        .execute(BOB, transfer(ns_id(42), 7, BOB), Budget::unlimited())
        .expect_err("ordinal 42 does not exist");
    assert_eq!(rejected.reason, Rejection::UnknownNamespace);
    assert_eq!(
        rejected.receipt,
        write_receipt(1, Outcome::Rejected(Rejection::UnknownNamespace))
    );
    assert_eq!(observe(&branch, ns).descriptor, fixture_descriptor(ns));
    assert_eq!(branch.spent(), Cost::new(3));
}

// ---------------------------------------------------------------------------
// 4. Block budget vector
// ---------------------------------------------------------------------------

#[test]
fn block_budget_vector_admits_charges_and_records_exactly_as_specified() {
    let (engine, ns) = fixture();
    let mut branch = fresh_branch(&engine, Some(34));
    assert_eq!(branch.block_limit(), Cost::new(34));
    assert_eq!(branch.remaining(), Cost::new(34));

    // op1: Put(k3, v4) = 1 + (10 + 6 + 4 + 4) = 25 <= 34 -> Applied;
    // remaining 9.
    let op1 = branch
        .execute(ALICE, put(ns, K3, V4), Budget::unlimited())
        .expect("25 fits the block's 34");
    assert_eq!(op1.receipt, write_receipt(25, Outcome::Applied));
    assert_eq!(branch.remaining(), Cost::new(9));

    // op2: lookup 1 fits 9; the 24-unit group does not fit the remaining 8.
    let op2 = branch
        .execute(ALICE, put(ns, K3, V4), Budget::unlimited())
        .expect_err("24 does not fit 8");
    assert_eq!(op2.receipt, write_receipt(1, OUT_OF_BUDGET));
    assert_eq!(branch.remaining(), Cost::new(8));

    // op3: Transfer lookup 1 fits 8; the group NamespaceTransfer 20 +
    // RootUpdate 2 = 22 does not fit 7.
    let op3 = branch
        .execute(ALICE, transfer(ns, 0, BOB), Budget::unlimited())
        .expect_err("22 does not fit 7");
    assert_eq!(op3.receipt, write_receipt(1, OUT_OF_BUDGET));
    assert_eq!(branch.remaining(), Cost::new(7));
    assert_eq!(
        branch
            .descriptor(ns, Budget::unlimited())
            .expect("the namespace exists")
            .value,
        fixture_descriptor(ns),
        "owner and revision are unchanged by the rejected transfer"
    );
    assert_eq!(branch.receipts().len(), 3);
    assert_eq!(branch.spent(), Cost::new(27));

    // op4: a 100-unit budget exceeds the remaining 7 -> admission rejection,
    // nothing charged, nothing recorded.
    let op4 = branch
        .execute(
            ALICE,
            WriteOp::CreateNamespace {
                name: arkiv_db::NamespaceName::new("test").unwrap(),
            },
            Budget::limit(100),
        )
        .expect_err("100 > 7 is not admitted");
    let exceeds = Rejection::ExceedsBlockBudget {
        remaining: Cost::new(7),
    };
    assert_eq!(op4.reason, exceeds);
    assert_eq!(op4.receipt, write_receipt(0, Outcome::Rejected(exceeds)));
    assert_eq!(
        branch.receipts().len(),
        3,
        "admission rejections are not recorded"
    );
    assert_eq!(branch.spent(), Cost::new(27));
    assert_eq!(branch.remaining(), Cost::new(7));
    assert_eq!(branch.namespace_count(), 1);
    // Admission uses the op limit alone: 8 is also rejected, 7 is admitted.
    let op4b = branch
        .execute(
            ALICE,
            WriteOp::CreateNamespace {
                name: arkiv_db::NamespaceName::new("test").unwrap(),
            },
            Budget::limit(8),
        )
        .expect_err("8 > 7 is not admitted");
    assert_eq!(op4b.reason, exceeds);
    assert_eq!(branch.receipts().len(), 3);

    // op5: a read with an unlimited budget is priced but never deducted.
    let op5 = branch
        .get(ns, K3, Budget::unlimited())
        .expect("the record exists");
    assert_eq!(op5.value, Some(V4.to_vec()));
    assert_eq!(op5.receipt, read_receipt(8, Outcome::Applied));
    assert_eq!(branch.receipts().len(), 3);
    assert_eq!(branch.remaining(), Cost::new(7));

    // op6: Delete(k3) with limit 7 <= remaining 7 is admitted; the lookup 1
    // fits; the group 5 + 6 + 4 = 15 would reach 16 > 7 -> OutOfBudget
    // spent 1.
    let op6 = branch
        .execute(ALICE, delete(ns, K3), Budget::limit(7))
        .expect_err("16 does not fit 7");
    assert_eq!(op6.receipt, write_receipt(1, OUT_OF_BUDGET));
    // 25 + 1 + 1 + 1 = 28.
    assert_eq!(branch.spent(), Cost::new(28));
    assert_eq!(branch.remaining(), Cost::new(6));
    assert_eq!(branch.receipts().len(), 4);
    assert_eq!(
        branch
            .get(ns, K3, Budget::unlimited())
            .expect("the record still exists")
            .value,
        Some(V4.to_vec())
    );

    // Estimates obey the same admission and the same block remaining.
    let estimate = branch
        .estimate(ALICE, delete(ns, K3), Budget::limit(7))
        .expect_err("7 > 6 is not admitted");
    assert_eq!(
        estimate.reason,
        Rejection::ExceedsBlockBudget {
            remaining: Cost::new(6)
        }
    );
    let estimate = branch
        .estimate(ALICE, delete(ns, K3), Budget::limit(6))
        .expect_err("16 does not fit 6");
    assert_eq!(estimate.receipt, write_receipt(1, OUT_OF_BUDGET));
    let estimate = branch
        .estimate(ALICE, put(ns, K3, V4), Budget::unlimited())
        .expect_err("25 does not fit the remaining 6");
    assert_eq!(estimate.receipt, write_receipt(1, OUT_OF_BUDGET));
    assert_eq!(branch.receipts().len(), 4, "estimates are not recorded");
    assert_eq!(branch.spent(), Cost::new(28));

    let sealed = branch.seal();
    assert_eq!(sealed.spent(), Cost::new(28));
    assert_eq!(
        sealed.receipts(),
        &[
            write_receipt(25, Outcome::Applied),
            write_receipt(1, OUT_OF_BUDGET),
            write_receipt(1, OUT_OF_BUDGET),
            write_receipt(1, OUT_OF_BUDGET),
        ]
    );
    assert!(
        sealed
            .receipts()
            .iter()
            .all(|receipt| receipt.kind == ReceiptKind::Write)
    );
}

// ---------------------------------------------------------------------------
// 5. Exact and zero budgets
// ---------------------------------------------------------------------------

#[test]
fn exact_budget_succeeds_while_one_short_and_zero_fail() {
    let (engine, ns) = fixture();
    let mut branch = fresh_branch(&engine, None);
    // Put(k3, v4) = 25.
    let executed = branch
        .execute(ALICE, put(ns, K3, V4), Budget::limit(25))
        .expect("an exact budget succeeds");
    assert_eq!(executed.receipt, write_receipt(25, Outcome::Applied));

    let rejected = branch
        .execute(ALICE, put(ns, K3, V4), Budget::limit(24))
        .expect_err("one short fails");
    assert_eq!(rejected.reason, Rejection::OutOfBudget);
    assert_eq!(rejected.receipt, write_receipt(1, OUT_OF_BUDGET));

    let rejected = branch
        .execute(ALICE, put(ns, K3, V4), Budget::limit(0))
        .expect_err("zero fails at the first charge");
    assert_eq!(rejected.receipt.outcome, OUT_OF_BUDGET);
    assert_eq!(rejected.receipt, write_receipt(0, OUT_OF_BUDGET));

    // Exact budgets for the other ops: Delete(k3) 16, Transfer 23, Create 53.
    let deleted = branch
        .execute(ALICE, delete(ns, K3), Budget::limit(16))
        .expect("16 is exact");
    assert_eq!(deleted.receipt, write_receipt(16, Outcome::Applied));
    let transferred = branch
        .execute(ALICE, transfer(ns, 0, BOB), Budget::limit(23))
        .expect("23 is exact");
    assert_eq!(transferred.receipt, write_receipt(23, Outcome::Applied));
    let created = branch
        .execute(
            BOB,
            WriteOp::CreateNamespace {
                name: arkiv_db::NamespaceName::new("test").unwrap(),
            },
            Budget::limit(53),
        )
        .expect("53 is exact");
    assert_eq!(created.receipt, write_receipt(53, Outcome::Applied));
    let rejected = branch
        .execute(
            BOB,
            WriteOp::CreateNamespace {
                name: arkiv_db::NamespaceName::new("test").unwrap(),
            },
            Budget::limit(52),
        )
        .expect_err("52 is one short of 53");
    assert_eq!(rejected.receipt, write_receipt(1, OUT_OF_BUDGET));
    // 25 + 1 + 0 + 16 + 23 + 53 + 1 = 119.
    assert_eq!(branch.spent(), Cost::new(119));
}

// ---------------------------------------------------------------------------
// 6. Overflow
// ---------------------------------------------------------------------------

#[test]
fn write_group_overflow_fails_atomically_after_the_lookup() {
    // FIXTURE with WriteValueByte priced at u64::MAX, id 7. Creation and
    // deletion never charge that class, so they keep their FIXTURE totals.
    let schedule = PriceSchedule::FIXTURE
        .with_price(OperationClass::WriteValueByte, u64::MAX)
        .with_id(7);
    let mut engine = Engine::new(schedule);
    let mut branch = engine.begin(engine.head(), None).expect("genesis head");
    let created = branch
        .execute(
            ALICE,
            WriteOp::CreateNamespace {
                name: arkiv_db::NamespaceName::new("test").unwrap(),
            },
            Budget::unlimited(),
        )
        .expect("creation does not price value bytes");
    assert_eq!(
        created.receipt,
        receipt(ReceiptKind::Write, 7, 53, Outcome::Applied)
    );
    let ns = ns_id(1);
    assert_eq!(created.effect, Effect::NamespaceCreated(ns));

    // Put("", "ab"): lookup 1; group PutRecord 10 + WriteKeyByte 0 +
    // WriteValueByte 2 * u64::MAX -> the subtotal overflows; nothing of the
    // group is charged.
    let rejected = branch
        .execute(ALICE, put(ns, b"", b"ab"), Budget::unlimited())
        .expect_err("2 * u64::MAX overflows");
    assert_eq!(rejected.reason, Rejection::CostOverflow);
    assert_eq!(rejected.receipt.outcome, COST_OVERFLOW);
    assert_eq!(
        rejected.receipt,
        receipt(ReceiptKind::Write, 7, 1, COST_OVERFLOW)
    );

    // Put("", "a"): the subtotal 1 * u64::MAX fits, but 10 + u64::MAX does not.
    let rejected = branch
        .execute(ALICE, put(ns, b"", b"a"), Budget::unlimited().traced())
        .expect_err("10 + u64::MAX overflows");
    assert_eq!(
        rejected.receipt,
        traced(
            receipt(ReceiptKind::Write, 7, 1, COST_OVERFLOW),
            &[(OperationClass::DirectoryLookup, 1, 1, 1)]
        )
    );

    // State unchanged: the namespace still has no records.
    let scanned = branch
        .scan_prefix(ns, b"", Budget::unlimited())
        .expect("the namespace exists");
    assert_eq!(scanned.value, Vec::<Entry>::new());
    assert_eq!(engine.head_snapshot().record_count(ns), None);

    // A zero-length value never multiplies the huge price: Put(k3, "") =
    // 1 + 10 + 6 + 0 + RootUpdate 4 = 21.
    let executed = branch
        .execute(ALICE, put(ns, K3, b""), Budget::unlimited())
        .expect("no value bytes are charged");
    assert_eq!(
        executed.receipt,
        receipt(ReceiptKind::Write, 7, 21, Outcome::Applied)
    );
    // 53 + 1 + 1 + 21 = 76.
    assert_eq!(branch.spent(), Cost::new(76));
    let published = engine.publish(&branch.seal()).expect("publish on genesis");
    assert_eq!(
        engine.snapshot(published).unwrap().record_count(ns),
        Some(1)
    );
}

#[test]
fn read_overflow_after_a_maximal_first_charge() {
    // The fixture is published under FIXTURE; a second block commits
    // uniform(7, u64::MAX) so that snapshot 2 prices every class at u64::MAX.
    let (mut engine, ns) = fixture();
    let mut admin = fresh_branch(&engine, None);
    admin
        .set_pricing(PriceSchedule::uniform(7, u64::MAX))
        .expect("7 exceeds the committed id 1");
    let id = engine
        .publish(&admin.seal())
        .expect("publish on snapshot 1");
    let snapshot = engine.snapshot(id).expect("snapshot 2 exists");
    assert_eq!(snapshot.pricing().id(), 7);

    // get: DirectoryLookup 1 * u64::MAX = u64::MAX fits an unlimited budget;
    // PointRead would make the running total u64::MAX + u64::MAX.
    let rejected = snapshot
        .get(ns, K3, Budget::unlimited())
        .expect_err("the second charge overflows");
    assert_eq!(rejected.reason, Rejection::CostOverflow);
    assert_eq!(
        rejected.receipt,
        receipt(ReceiptKind::Read, 7, u64::MAX, COST_OVERFLOW)
    );
    // scan: the seek overflows in the same way.
    let rejected = snapshot
        .scan_prefix(ns, b"", Budget::unlimited())
        .expect_err("the seek charge overflows");
    assert_eq!(
        rejected.receipt,
        receipt(ReceiptKind::Read, 7, u64::MAX, COST_OVERFLOW)
    );
    // descriptor: a single charge of u64::MAX fits; an exact limit of
    // u64::MAX also succeeds, and one unit less stops the first charge.
    let described = snapshot
        .descriptor(ns, Budget::unlimited())
        .expect("one maximal charge fits");
    assert_eq!(described.value, fixture_descriptor(ns));
    assert_eq!(
        described.receipt,
        receipt(ReceiptKind::Read, 7, u64::MAX, Outcome::Applied)
    );
    let described = snapshot
        .descriptor(ns, Budget::limit(u64::MAX))
        .expect("an exact budget succeeds");
    assert_eq!(
        described.receipt,
        receipt(ReceiptKind::Read, 7, u64::MAX, Outcome::Applied)
    );
    let rejected = snapshot
        .descriptor(ns, Budget::limit(u64::MAX - 1))
        .expect_err("one short of the first charge");
    assert_eq!(
        rejected.receipt,
        receipt(ReceiptKind::Read, 7, 0, OUT_OF_BUDGET)
    );
    // The fixture snapshot is unaffected: get(k3) = 8 under schedule 1.
    let old = engine.snapshot(SnapshotId::GENESIS).expect("genesis");
    assert_eq!(old.pricing().id(), FIXTURE_ID);
    let fixture_snapshot = engine.snapshot(id).unwrap().base().expect("has a base");
    let fixture_snapshot = engine.snapshot(fixture_snapshot).unwrap();
    assert_eq!(
        fixture_snapshot
            .get(ns, K3, Budget::unlimited())
            .expect("priced with FIXTURE")
            .receipt,
        read_receipt(8, Outcome::Applied)
    );
}

#[test]
fn first_charge_overflow_and_block_aggregate_saturation() {
    // Snapshot 2 commits FIXTURE with DirectoryLookup at u64::MAX, id 8.
    let (mut engine, ns) = fixture();
    let mut admin = fresh_branch(&engine, None);
    admin
        .set_pricing(
            PriceSchedule::FIXTURE
                .with_price(OperationClass::DirectoryLookup, u64::MAX)
                .with_id(8),
        )
        .expect("8 exceeds the committed id 1");
    let id = engine
        .publish(&admin.seal())
        .expect("publish on snapshot 1");

    // Block budget None: remaining is u64::MAX.
    let mut branch = engine.begin(id, None).expect("snapshot 2 is the head");
    assert_eq!(branch.pricing().id(), 8);
    assert_eq!(branch.remaining(), Cost::new(u64::MAX));

    // descriptor: one charge of 1 * u64::MAX fits an unlimited budget.
    let described = branch
        .descriptor(ns, Budget::unlimited())
        .expect("one maximal charge fits");
    assert_eq!(
        described.receipt,
        receipt(ReceiptKind::Read, 8, u64::MAX, Outcome::Applied)
    );

    // Put(k3, v4): the lookup u64::MAX fits the block remaining exactly; the
    // group 10 + 6 + 4 + 4 = 24 then overflows the running total.
    let rejected = branch
        .execute(ALICE, put(ns, K3, V4), Budget::unlimited())
        .expect_err("u64::MAX + 24 overflows");
    assert_eq!(rejected.reason, Rejection::CostOverflow);
    assert_eq!(
        rejected.receipt,
        receipt(ReceiptKind::Write, 8, u64::MAX, COST_OVERFLOW)
    );
    assert_eq!(branch.spent(), Cost::new(u64::MAX));
    assert_eq!(branch.remaining(), Cost::ZERO);
    assert_eq!(
        branch
            .get(ns, K3, Budget::unlimited())
            .expect_err("PointRead overflows after the maximal lookup")
            .receipt,
        receipt(ReceiptKind::Read, 8, u64::MAX, COST_OVERFLOW)
    );

    // The block aggregate never overflows: with 0 remaining the next put
    // cannot even afford the lookup and is OutOfBudget at 0 units.
    let rejected = branch
        .execute(ALICE, put(ns, K3, V4), Budget::unlimited())
        .expect_err("nothing fits a remaining of 0");
    assert_eq!(rejected.reason, Rejection::OutOfBudget);
    assert_eq!(
        rejected.receipt,
        receipt(ReceiptKind::Write, 8, 0, OUT_OF_BUDGET)
    );
    assert_eq!(branch.spent(), Cost::new(u64::MAX));
    // A limited budget of 1 is not admitted; 0 is admitted and fails at 0.
    let rejected = branch
        .execute(ALICE, delete(ns, K3), Budget::limit(1))
        .expect_err("1 > 0 is not admitted");
    assert_eq!(
        rejected.reason,
        Rejection::ExceedsBlockBudget {
            remaining: Cost::ZERO
        }
    );
    let rejected = branch
        .execute(ALICE, delete(ns, K3), Budget::limit(0))
        .expect_err("0 is admitted but affords nothing");
    assert_eq!(
        rejected.receipt,
        receipt(ReceiptKind::Write, 8, 0, OUT_OF_BUDGET)
    );
    // Reads are unaffected by the exhausted block.
    assert_eq!(
        branch
            .descriptor(ns, Budget::unlimited())
            .expect("reads never deduct from the block")
            .receipt,
        receipt(ReceiptKind::Read, 8, u64::MAX, Outcome::Applied)
    );
    let sealed = branch.seal();
    assert_eq!(sealed.spent(), Cost::new(u64::MAX));
    assert_eq!(sealed.receipts().len(), 3);
    assert_eq!(
        sealed.receipts()[0],
        receipt(ReceiptKind::Write, 8, u64::MAX, COST_OVERFLOW)
    );
    // The overflowing and the OutOfBudget puts changed nothing.
    let published = engine.publish(&sealed).expect("publish on snapshot 2");
    let head = engine.snapshot(published).expect("snapshot 3 exists");
    assert_eq!(head.record_count(ns), Some(2));
}

// ---------------------------------------------------------------------------
// 7. Estimation
// ---------------------------------------------------------------------------

#[test]
fn estimate_matches_execute_for_a_mixed_operation_list() {
    let (engine, ns) = fixture();
    let mut branch = fresh_branch(&engine, Some(500));
    let unauthorized = Outcome::Rejected(Rejection::Unauthorized);
    let stale = Rejection::StaleRevision {
        expected: Revision(5),
        current: Revision(0),
    };
    // Running block spent after each step: 25, 48, 64, 82, 135, 136, 137,
    // 138, 139, 162, 163; remaining before the last step: 500 - 163 = 337.
    let exceeds = Rejection::ExceedsBlockBudget {
        remaining: Cost::new(337),
    };
    let script: Vec<(&str, OwnerId, WriteOp, Budget, Canonical)> = vec![
        (
            // Put("abe", v4) = 1 + 10 + 6 + 4 + 4 = 25.
            "insert",
            ALICE,
            put(ns, b"abe", V4),
            Budget::unlimited(),
            Ok((
                Effect::Put { replaced: false },
                write_receipt(25, Outcome::Applied),
            )),
        ),
        (
            // Put(k3, "zz") = 1 + 10 + 6 + 2 + 4 = 23, traced.
            "replace",
            ALICE,
            put(ns, K3, b"zz"),
            Budget::limit(100).traced(),
            Ok((
                Effect::Put { replaced: true },
                traced(
                    write_receipt(23, Outcome::Applied),
                    &[
                        (OperationClass::DirectoryLookup, 1, 1, 1),
                        (OperationClass::PutRecord, 1, 10, 10),
                        (OperationClass::WriteKeyByte, 3, 2, 6),
                        (OperationClass::WriteValueByte, 2, 1, 2),
                        (OperationClass::RootUpdate, 2, 2, 4),
                    ],
                ),
            )),
        ),
        (
            // Delete(k3b) = 1 + 5 + 6 + 4 = 16, exact budget.
            "delete present",
            ALICE,
            delete(ns, K3B),
            Budget::limit(16),
            Ok((
                Effect::Deleted { existed: true },
                write_receipt(16, Outcome::Applied),
            )),
        ),
        (
            // Delete("nope") = 1 + 5 + 8 + 4 = 18.
            "delete missing",
            ALICE,
            delete(ns, b"nope"),
            Budget::unlimited(),
            Ok((
                Effect::Deleted { existed: false },
                write_receipt(18, Outcome::Applied),
            )),
        ),
        (
            // CreateNamespace = 1 + 50 + 2 = 53 -> ordinal 2.
            "create",
            ALICE,
            WriteOp::CreateNamespace {
                name: arkiv_db::NamespaceName::new("test").unwrap(),
            },
            Budget::limit(53),
            Ok((
                Effect::NamespaceCreated(ns_id(2)),
                write_receipt(53, Outcome::Applied),
            )),
        ),
        (
            "unauthorized",
            BOB,
            put(ns, K3, V4),
            Budget::unlimited(),
            Err((Rejection::Unauthorized, write_receipt(1, unauthorized))),
        ),
        (
            "unknown",
            ALICE,
            put(ns_id(99), K3, V4),
            Budget::unlimited(),
            Err((
                Rejection::UnknownNamespace,
                write_receipt(1, Outcome::Rejected(Rejection::UnknownNamespace)),
            )),
        ),
        (
            "stale revision",
            ALICE,
            transfer(ns, 5, BOB),
            Budget::unlimited(),
            Err((stale, write_receipt(1, Outcome::Rejected(stale)))),
        ),
        (
            // Put(k3, v4) = 25 under a budget of 24: the lookup, then stop.
            "tight",
            ALICE,
            put(ns, K3, V4),
            Budget::limit(24),
            Err((Rejection::OutOfBudget, write_receipt(1, OUT_OF_BUDGET))),
        ),
        (
            "transfer",
            ALICE,
            transfer(ns, 0, BOB),
            Budget::unlimited(),
            Ok((
                Effect::Transferred {
                    revision: Revision(1),
                },
                write_receipt(23, Outcome::Applied),
            )),
        ),
        (
            "after transfer",
            ALICE,
            put(ns, K3, V4),
            Budget::unlimited(),
            Err((Rejection::Unauthorized, write_receipt(1, unauthorized))),
        ),
        (
            "oversized",
            BOB,
            put(ns, K3, V4),
            Budget::limit(1_000),
            Err((exceeds, write_receipt(0, Outcome::Rejected(exceeds)))),
        ),
    ];
    for (name, actor, op, budget, expected) in script {
        let estimated = canonical(&branch.estimate(actor, op.clone(), budget));
        let executed = canonical(&branch.execute(actor, op, budget));
        assert_eq!(estimated, executed, "{name}: estimate differs from execute");
        assert_eq!(
            executed, expected,
            "{name}: execute differs from the vector"
        );
    }
    assert_eq!(branch.spent(), Cost::new(163));
    assert_eq!(
        branch.receipts().len(),
        11,
        "the admission rejection is not recorded"
    );
}

#[test]
fn estimate_has_no_side_effects() {
    let (engine, ns) = fixture();
    let mut branch = fresh_branch(&engine, Some(1_000));
    let before = observe(&branch, ns);

    // Put(k3, "new") = 1 + 10 + 6 + 3 + 4 = 24; Delete(k3) = 1 + 5 + 6 + 4 =
    // 16; Transfer = 1 + 20 + 2 = 23.
    let estimated = branch
        .estimate(ALICE, put(ns, K3, b"new"), Budget::unlimited())
        .expect("the estimate applies");
    assert_eq!(estimated.effect, Effect::Put { replaced: true });
    assert_eq!(estimated.receipt, write_receipt(24, Outcome::Applied));
    let estimated = branch
        .estimate(ALICE, delete(ns, K3), Budget::unlimited())
        .expect("the estimate applies");
    assert_eq!(estimated.receipt, write_receipt(16, Outcome::Applied));
    let estimated = branch
        .estimate(ALICE, transfer(ns, 0, BOB), Budget::unlimited())
        .expect("the estimate applies");
    assert_eq!(estimated.receipt, write_receipt(23, Outcome::Applied));

    assert_eq!(branch.receipts().len(), 0);
    assert_eq!(branch.spent(), Cost::ZERO);
    assert_eq!(branch.remaining(), Cost::new(1_000));
    assert_eq!(observe(&branch, ns), before);

    // Creation estimates do not consume ordinals.
    let first = branch
        .estimate(
            ALICE,
            WriteOp::CreateNamespace {
                name: arkiv_db::NamespaceName::new("test").unwrap(),
            },
            Budget::unlimited(),
        )
        .expect("the estimate applies");
    let second = branch
        .estimate(
            ALICE,
            WriteOp::CreateNamespace {
                name: arkiv_db::NamespaceName::new("test").unwrap(),
            },
            Budget::unlimited(),
        )
        .expect("the estimate applies");
    assert_eq!(first.effect, Effect::NamespaceCreated(ns_id(2)));
    assert_eq!(second.effect, Effect::NamespaceCreated(ns_id(2)));
    assert_eq!(first.receipt, write_receipt(53, Outcome::Applied));
    assert_eq!(second.receipt, first.receipt);
    assert_eq!(branch.namespace_count(), 1);

    let executed = branch
        .execute(
            ALICE,
            WriteOp::CreateNamespace {
                name: arkiv_db::NamespaceName::new("test").unwrap(),
            },
            Budget::unlimited(),
        )
        .expect("creation applies");
    assert_eq!(executed.effect, Effect::NamespaceCreated(ns_id(2)));
    assert_eq!(executed.receipt, write_receipt(53, Outcome::Applied));
    let third = branch
        .estimate(
            ALICE,
            WriteOp::CreateNamespace {
                name: arkiv_db::NamespaceName::new("test").unwrap(),
            },
            Budget::unlimited(),
        )
        .expect("the estimate applies");
    assert_eq!(third.effect, Effect::NamespaceCreated(ns_id(3)));
    assert_eq!(branch.namespace_count(), 2);
    assert_eq!(branch.receipts().len(), 1);
    assert_eq!(branch.spent(), Cost::new(53));
}

#[test]
fn estimates_are_state_bound_not_cached() {
    let (engine, ns) = fixture();
    let mut branch = fresh_branch(&engine, Some(34));

    // With 34 remaining, Put(k3, v4) = 25 fits.
    let estimated = branch
        .estimate(ALICE, put(ns, K3, V4), Budget::unlimited())
        .expect("25 fits 34");
    assert_eq!(estimated.receipt, write_receipt(25, Outcome::Applied));
    assert_eq!(branch.remaining(), Cost::new(34));

    // A different 25-unit op consumes the room: remaining 9.
    let other = branch
        .execute(ALICE, put(ns, b"abe", V4), Budget::unlimited())
        .expect("25 fits 34");
    assert_eq!(other.receipt, write_receipt(25, Outcome::Applied));
    assert_eq!(branch.remaining(), Cost::new(9));

    // Parity while the block remaining binds: lookup 1 fits 9, the group 24
    // does not fit 8, for both the estimate and the execution.
    let estimated = branch
        .estimate(ALICE, put(ns, K3, V4), Budget::unlimited())
        .expect_err("24 does not fit 8");
    assert_eq!(estimated.receipt, write_receipt(1, OUT_OF_BUDGET));
    let executed = branch
        .execute(ALICE, put(ns, K3, V4), Budget::unlimited())
        .expect_err("24 does not fit 8");
    assert_eq!(executed.receipt, write_receipt(1, OUT_OF_BUDGET));
    assert_eq!(canonical(&Err(estimated)), canonical(&Err(executed)));
    assert_eq!(branch.remaining(), Cost::new(8));
    assert_eq!(branch.spent(), Cost::new(26));
}

#[test]
fn estimate_runs_on_scratch_state_that_never_leaks() {
    let (engine, ns) = fixture();
    // `estimate` takes `&self`; the runtime checks cover the scratch copy.
    let branch = fresh_branch(&engine, None);
    let before = observe(&branch, ns);

    branch
        .estimate(ALICE, put(ns, K3, b"leak"), Budget::unlimited())
        .expect("the estimate applies");
    assert_eq!(
        branch.get(ns, K3, Budget::unlimited()).unwrap().value,
        Some(V4.to_vec()),
        "an estimated put leaves the old value"
    );
    branch
        .estimate(ALICE, delete(ns, K3B), Budget::unlimited())
        .expect("the estimate applies");
    assert_eq!(
        branch.get(ns, K3B, Budget::unlimited()).unwrap().value,
        Some(V1.to_vec()),
        "an estimated delete leaves the record"
    );
    branch
        .estimate(ALICE, transfer(ns, 0, BOB), Budget::unlimited())
        .expect("the estimate applies");
    branch
        .estimate(
            ALICE,
            WriteOp::CreateNamespace {
                name: arkiv_db::NamespaceName::new("test").unwrap(),
            },
            Budget::unlimited(),
        )
        .expect("the estimate applies");
    assert_eq!(observe(&branch, ns), before);
    assert_eq!(branch.namespace_count(), 1);
    assert_eq!(
        branch
            .descriptor(ns_id(2), Budget::unlimited())
            .expect_err("the estimated namespace does not exist")
            .reason,
        Rejection::UnknownNamespace
    );
    // The published snapshot underneath is untouched as well.
    let snapshot = engine.head_snapshot();
    assert_eq!(snapshot.namespace_count(), 1);
    assert_eq!(
        snapshot
            .scan_prefix(ns, b"", Budget::unlimited())
            .unwrap()
            .value,
        fixture_records()
    );
}

// ---------------------------------------------------------------------------
// 8. Determinism
// ---------------------------------------------------------------------------

#[test]
fn repeated_reads_on_a_snapshot_yield_identical_receipts() {
    let (engine, ns) = fixture();
    let snapshot = engine.head_snapshot();
    let gets: Vec<_> = (0..5)
        .map(|_| snapshot.get(ns, K3, Budget::limit(8)).expect("8 is exact"))
        .collect();
    for (index, metered) in gets.iter().enumerate() {
        assert_eq!(metered.value, Some(V4.to_vec()), "get #{index}");
        assert_eq!(
            metered.receipt,
            read_receipt(8, Outcome::Applied),
            "get #{index}"
        );
        assert_eq!(metered.physical, gets[0].physical, "get #{index}");
    }
    let scans: Vec<_> = (0..5)
        .map(|_| {
            snapshot
                .scan_prefix(ns, b"ab", Budget::unlimited().traced())
                .expect("an unlimited scan applies")
        })
        .collect();
    let expected = traced(
        read_receipt(20, Outcome::Applied),
        &[
            (OperationClass::DirectoryLookup, 1, 1, 1),
            (OperationClass::ScanSeek, 1, 4, 4),
            (OperationClass::ScanEntry, 2, 2, 4),
            (OperationClass::ReadByte, 11, 1, 11),
        ],
    );
    for (index, metered) in scans.iter().enumerate() {
        assert_eq!(metered.value, fixture_records(), "scan #{index}");
        assert_eq!(metered.receipt, expected, "scan #{index}");
        assert_eq!(metered.physical, scans[0].physical, "scan #{index}");
    }
}

/// Fixed-seed xorshift, as in `model.rs`: no randomness between runs.
struct Generator(u32);

impl Generator {
    fn next(&mut self) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        self.0
    }

    fn bytes(&mut self, max_len: u32) -> Vec<u8> {
        let len = self.next() % (max_len + 1);
        (0..len).map(|_| b'a' + (self.next() % 4) as u8).collect()
    }
}

type BlockRecord = (SnapshotId, Cost, Vec<Receipt>, Vec<Canonical>);

/// Two blocks of scripted work, returning everything canonical that the
/// engine produced plus a final traced read of the head.
fn run_script(engine: &mut Engine) -> (Vec<BlockRecord>, Vec<Entry>, Receipt) {
    let mut generator = Generator(0x9e37_79b9);
    let (ns1, ns2) = (ns_id(1), ns_id(2));
    let mut blocks = Vec::new();

    let mut branch = engine.begin(engine.head(), Some(Cost::new(200))).unwrap();
    let outcomes = vec![
        canonical(&branch.execute(
            ALICE,
            WriteOp::CreateNamespace {
                name: arkiv_db::NamespaceName::new("test").unwrap(),
            },
            Budget::limit(60),
        )),
        canonical(&branch.execute(
            BOB,
            WriteOp::CreateNamespace {
                name: arkiv_db::NamespaceName::new("test").unwrap(),
            },
            Budget::limit(60).traced(),
        )),
        canonical(&branch.execute(
            BOB,
            WriteOp::CreateNamespace {
                name: arkiv_db::NamespaceName::new("test").unwrap(),
            },
            Budget::limit(60),
        )),
    ];
    let sealed = branch.seal();
    let id = engine.publish(&sealed).unwrap();
    blocks.push((id, sealed.spent(), sealed.receipts().to_vec(), outcomes));

    let mut branch = engine.begin(engine.head(), Some(Cost::new(900))).unwrap();
    let mut outcomes = Vec::new();
    for step in 0..40 {
        let (actor, ns) = match (generator.next() % 4, step % 3) {
            (0, _) => (BOB, ns1),
            (_, 0) => (BOB, ns2),
            _ => (ALICE, ns1),
        };
        let key = generator.bytes(6);
        let op = if generator.next().is_multiple_of(5) {
            delete(ns, &key)
        } else {
            let value = generator.bytes(12);
            put(ns, &key, &value)
        };
        let budget = match generator.next() % 3 {
            0 => Budget::unlimited(),
            1 => Budget::limit(u64::from(generator.next() % 40)),
            _ => Budget::limit(60).traced(),
        };
        outcomes.push(canonical(&branch.execute(actor, op, budget)));
    }
    outcomes.push(canonical(&branch.execute(
        ALICE,
        transfer(ns1, 0, BOB),
        Budget::unlimited(),
    )));
    let sealed = branch.seal();
    let id = engine.publish(&sealed).unwrap();
    blocks.push((id, sealed.spent(), sealed.receipts().to_vec(), outcomes));

    let head = engine.head_snapshot();
    let scanned = head
        .scan_prefix(ns1, b"", Budget::unlimited().traced())
        .expect("namespace 1 exists");
    (blocks, scanned.value, scanned.receipt)
}

#[test]
fn identical_op_sequences_on_fresh_engines_are_identical() {
    let mut first = Engine::new(PriceSchedule::FIXTURE);
    let mut second = Engine::new(PriceSchedule::FIXTURE);
    let left = run_script(&mut first);
    let right = run_script(&mut second);
    assert_eq!(left, right);
    assert_eq!(first.head(), second.head());
    assert_eq!(first.snapshot_count(), 3);
    assert_eq!(second.snapshot_count(), 3);

    // Sanity: the script is not degenerate.
    let (blocks, records, _) = &left;
    assert_eq!(blocks[0].0.sequence(), 1);
    assert_eq!(blocks[1].0.sequence(), 2);
    // Block 1: three creations at 53 each = 159.
    assert_eq!(blocks[0].1, Cost::new(159));
    assert_eq!(blocks[1].3.len(), 41);
    assert!(blocks[1].3.iter().any(Result::is_ok), "some writes applied");
    assert!(
        blocks[1].3.iter().any(Result::is_err),
        "some writes were rejected"
    );
    assert!(!records.is_empty(), "namespace 1 holds records");
}

#[test]
fn tracing_never_changes_charges() {
    let (engine, ns) = fixture();
    let ops: Vec<(OwnerId, WriteOp, Budget)> = vec![
        (ALICE, put(ns, K3, V4), Budget::unlimited()),
        (ALICE, put(ns, b"", b""), Budget::limit(15)),
        (ALICE, delete(ns, K3), Budget::limit(16)),
        (ALICE, delete(ns, b"zzz"), Budget::unlimited()),
        (ALICE, transfer(ns, 0, BOB), Budget::unlimited()),
        (
            ALICE,
            WriteOp::CreateNamespace {
                name: arkiv_db::NamespaceName::new("test").unwrap(),
            },
            Budget::limit(53),
        ),
        (BOB, put(ns, K3, V4), Budget::unlimited()),
        (ALICE, put(ns_id(9), K3, V4), Budget::unlimited()),
        (ALICE, put(ns, K3, V4), Budget::limit(24)),
        (ALICE, put(ns, K3, V4), Budget::limit(0)),
        (ALICE, put(ns, K3, V4), Budget::limit(200)),
    ];
    for (actor, op, budget) in ops {
        let untraced =
            canonical(&fresh_branch(&engine, Some(100)).execute(actor, op.clone(), budget));
        let traced = canonical(&fresh_branch(&engine, Some(100)).execute(
            actor,
            op.clone(),
            budget.traced(),
        ));
        let stripped = match &traced {
            Ok((effect, receipt)) => {
                assert!(receipt.ledger.is_some(), "{op:?}: traced");
                Ok((*effect, without_ledger(receipt)))
            }
            Err((reason, receipt)) => {
                assert!(receipt.ledger.is_some(), "{op:?}: traced");
                Err((*reason, without_ledger(receipt)))
            }
        };
        assert_eq!(stripped, untraced, "{op:?}");
    }
    let snapshot = engine.head_snapshot();
    let untraced = snapshot.get(ns, K3, Budget::limit(7)).expect_err("7 < 8");
    let traced = snapshot
        .get(ns, K3, Budget::limit(7).traced())
        .expect_err("7 < 8");
    assert_eq!(untraced.receipt, read_receipt(4, OUT_OF_BUDGET));
    assert_eq!(without_ledger(&traced.receipt), untraced.receipt);
}

// ---------------------------------------------------------------------------
// 9. Pricing pinning
// ---------------------------------------------------------------------------

#[test]
fn pricing_is_pinned_to_the_base_and_applies_only_after_publication() {
    let (mut engine, ns) = fixture();
    let fixture_snapshot = engine.head_snapshot();
    assert_eq!(fixture_snapshot.pricing().id(), FIXTURE_ID);

    let mut b1 = fresh_branch(&engine, None);
    assert_eq!(b1.pricing().id(), FIXTURE_ID);
    // Put(k3, v4) = 25 under FIXTURE.
    let before = b1
        .execute(ALICE, put(ns, K3, V4), Budget::unlimited())
        .expect("applies");
    assert_eq!(before.receipt, write_receipt(25, Outcome::Applied));

    b1.set_pricing(PriceSchedule::uniform(2, 100))
        .expect("2 exceeds the committed id 1");
    // Still pinned to FIXTURE: 25 with schedule id 1, for writes and reads.
    let after = b1
        .execute(ALICE, put(ns, K3, V4), Budget::unlimited())
        .expect("applies");
    assert_eq!(after.receipt, write_receipt(25, Outcome::Applied));
    assert_eq!(b1.pricing().id(), FIXTURE_ID);
    // get missing = 1 + 3 = 4 under FIXTURE.
    let miss = b1.get(ns, b"zzz", Budget::unlimited()).expect("applies");
    assert_eq!(miss.receipt, read_receipt(4, Outcome::Applied));
    let estimated = b1
        .estimate(ALICE, put(ns, K3, V4), Budget::unlimited())
        .expect("applies");
    assert_eq!(estimated.receipt, write_receipt(25, Outcome::Applied));

    // The pending id is 2, so 1 and 2 are stale.
    assert_eq!(
        b1.set_pricing(PriceSchedule::uniform(1, 5)),
        Err(EngineError::StaleSchedule { current: 2 })
    );
    assert_eq!(
        b1.set_pricing(PriceSchedule::uniform(2, 7)),
        Err(EngineError::StaleSchedule { current: 2 })
    );

    let published = engine.publish(&b1.seal()).expect("publish on snapshot 1");
    let snapshot = engine.snapshot(published).expect("snapshot 2 exists");
    assert_eq!(snapshot.pricing().id(), 2);
    // Reads at snapshot 2 use uniform 100: get missing = 100 + 100 = 200;
    // get(k3) = 100 + 100 + 4 * 100 = 600; descriptor = 100.
    assert_eq!(
        snapshot
            .get(ns, b"zzz", Budget::unlimited().traced())
            .expect("applies")
            .receipt,
        traced(
            receipt(ReceiptKind::Read, 2, 200, Outcome::Applied),
            &[
                (OperationClass::DirectoryLookup, 1, 100, 100),
                (OperationClass::PointRead, 1, 100, 100),
            ]
        )
    );
    assert_eq!(
        snapshot
            .get(ns, K3, Budget::unlimited())
            .expect("applies")
            .receipt,
        receipt(ReceiptKind::Read, 2, 600, Outcome::Applied)
    );
    assert_eq!(
        snapshot
            .descriptor(ns, Budget::unlimited())
            .expect("applies")
            .receipt,
        receipt(ReceiptKind::Read, 2, 100, Outcome::Applied)
    );

    // A new branch on snapshot 2: Put(k3, v4) = 100 * (1 + 1 + 3 + 4 + 2) =
    // 1100 (DirectoryLookup, PutRecord, WriteKeyByte, WriteValueByte,
    // RootUpdate counts).
    let mut b2 = engine
        .begin(published, None)
        .expect("snapshot 2 is the head");
    assert_eq!(b2.pricing().id(), 2);
    let repriced = b2
        .execute(ALICE, put(ns, K3, V4), Budget::unlimited())
        .expect("applies");
    assert_eq!(
        repriced.receipt,
        receipt(ReceiptKind::Write, 2, 1100, Outcome::Applied)
    );
    let rejected = b2
        .execute(ALICE, put(ns, K3, V4), Budget::limit(1099))
        .expect_err("1099 is one short of 1100");
    assert_eq!(
        rejected.receipt,
        receipt(ReceiptKind::Write, 2, 100, OUT_OF_BUDGET)
    );

    // The earlier snapshot still prices with schedule 1.
    assert_eq!(fixture_snapshot.pricing().id(), FIXTURE_ID);
    assert_eq!(
        fixture_snapshot
            .get(ns, b"zzz", Budget::unlimited())
            .expect("applies")
            .receipt,
        read_receipt(4, Outcome::Applied)
    );
    assert_eq!(
        engine
            .snapshot(SnapshotId::GENESIS)
            .expect("genesis")
            .pricing()
            .id(),
        FIXTURE_ID
    );
}

#[test]
fn rollback_restores_pending_pricing() {
    let (mut engine, ns) = fixture();
    let mut branch = fresh_branch(&engine, None);
    let checkpoint = branch.checkpoint();
    branch
        .set_pricing(PriceSchedule::uniform(3, 1))
        .expect("3 exceeds 1");
    assert_eq!(
        branch.set_pricing(PriceSchedule::uniform(3, 2)),
        Err(EngineError::StaleSchedule { current: 3 })
    );
    branch
        .rollback_to(checkpoint)
        .expect("the checkpoint is valid");
    // The pending schedule is back to id 1, so id 2 is accepted again.
    branch
        .set_pricing(PriceSchedule::uniform(2, 50))
        .expect("pending pricing was rolled back to id 1");
    assert_eq!(
        branch.set_pricing(PriceSchedule::uniform(1, 5)),
        Err(EngineError::StaleSchedule { current: 2 })
    );
    let published = engine
        .publish(&branch.seal())
        .expect("publish on snapshot 1");
    let snapshot = engine.snapshot(published).expect("snapshot 2 exists");
    assert_eq!(snapshot.pricing().id(), 2);
    // get missing under uniform 50 = 50 + 50 = 100.
    assert_eq!(
        snapshot
            .get(ns, b"zzz", Budget::unlimited())
            .expect("applies")
            .receipt,
        receipt(ReceiptKind::Read, 2, 100, Outcome::Applied)
    );
}

// ---------------------------------------------------------------------------
// 10. Receipt kinds
// ---------------------------------------------------------------------------

#[test]
fn read_receipts_are_never_recorded_and_sealed_receipts_are_all_writes() {
    let (engine, ns) = fixture();
    let snapshot = engine.head_snapshot();
    let mut branch = fresh_branch(&engine, Some(100));

    let reads = [
        branch
            .get(ns, K3, Budget::unlimited())
            .expect("applies")
            .receipt,
        branch
            .scan_prefix(ns, b"ab", Budget::unlimited())
            .expect("applies")
            .receipt,
        branch
            .descriptor(ns, Budget::unlimited())
            .expect("applies")
            .receipt,
        branch
            .get(ns, K3, Budget::limit(0))
            .expect_err("nothing fits 0")
            .receipt,
        branch
            .descriptor(ns_id(9), Budget::unlimited())
            .expect_err("ordinal 9 does not exist")
            .receipt,
        snapshot
            .get(ns, K3, Budget::unlimited())
            .expect("applies")
            .receipt,
        snapshot
            .scan_prefix(ns, b"zz", Budget::limit(4))
            .expect_err("the seek does not fit 4")
            .receipt,
    ];
    assert_eq!(
        reads,
        [
            read_receipt(8, Outcome::Applied),
            read_receipt(20, Outcome::Applied),
            read_receipt(1, Outcome::Applied),
            read_receipt(0, OUT_OF_BUDGET),
            read_receipt(1, Outcome::Rejected(Rejection::UnknownNamespace)),
            read_receipt(8, Outcome::Applied),
            read_receipt(1, OUT_OF_BUDGET),
        ]
    );
    assert!(
        reads
            .iter()
            .all(|receipt| receipt.kind == ReceiptKind::Read)
    );
    assert!(branch.receipts().is_empty(), "reads are never recorded");
    assert_eq!(branch.spent(), Cost::ZERO);
    assert_eq!(branch.remaining(), Cost::new(100));

    let applied = branch
        .execute(ALICE, put(ns, K3, V4), Budget::unlimited())
        .expect("applies");
    let rejected = branch
        .execute(BOB, put(ns, K3, V4), Budget::unlimited())
        .expect_err("BOB is not the owner");
    let admission = branch
        .execute(ALICE, put(ns, K3, V4), Budget::limit(1_000))
        .expect_err("1000 > 74 is not admitted");
    assert_eq!(applied.receipt.kind, ReceiptKind::Write);
    assert_eq!(rejected.receipt.kind, ReceiptKind::Write);
    assert_eq!(admission.receipt.kind, ReceiptKind::Write);
    // 100 - (25 + 1) = 74.
    assert_eq!(
        admission.reason,
        Rejection::ExceedsBlockBudget {
            remaining: Cost::new(74)
        }
    );

    let sealed = branch.seal();
    assert_eq!(
        sealed.receipts(),
        &[
            write_receipt(25, Outcome::Applied),
            write_receipt(1, Outcome::Rejected(Rejection::Unauthorized)),
        ]
    );
    assert!(
        sealed
            .receipts()
            .iter()
            .all(|receipt| receipt.kind == ReceiptKind::Write)
    );
    assert_eq!(sealed.spent(), Cost::new(26));
}
