//! The fallible `try_` surface (node step N0b-2) on the in-memory engine.
//!
//! What this file establishes: every `try_` form of a `Branch` or
//! `Snapshot` method returns `Ok` holding exactly what the infallible
//! method returns, on a small workload with applied and rejected writes:
//! writes and estimates on both paths (effects, rejections, receipts,
//! physical traces, the block's receipts and spend, run in lockstep on two
//! engines), every metered read (`get`, `scan_prefix`, `descriptor`,
//! `get_record`, `query` pages and their cursors, `count`, `terms`),
//! every statistic and the hidden index test aid, and the expiry sweep
//! (`try_expire_records` against `expire_records`, completed and halted).
//! `StorageError` is the storage contract's type, re-exported.
//!
//! What it does not establish: behaviour under storage failures. The
//! in-memory engine has none; an engine reading committed nodes (node step
//! N3b's `Engine::from_state` test aid) makes these forms fail, and
//! `storage_faults.rs` tests that, including branch poisoning.

use std::ops::Bound;

use arkiv_db::{
    BlockNumber, Branch, Budget, Cell, CellName, Cost, Engine, NamespaceId, NamespaceName, OwnerId,
    PriceSchedule, Projection, Query, RecordKey, Revision, Snapshot, StorageError,
    StorageErrorKind, Term, TypeTag, Value, WriteOp, expire_records, try_expire_records,
};

const ALICE: OwnerId = OwnerId::new([0xa1; 20]);
const BOB: OwnerId = OwnerId::new([0xb0; 20]);

fn ns(ordinal: u64) -> NamespaceId {
    NamespaceId::from_ordinal(ordinal).expect("ordinals start at 1")
}

fn name(text: &str) -> CellName {
    CellName::new(text).expect("fixture names are valid")
}

fn key(bytes: &[u8]) -> RecordKey {
    RecordKey::new(bytes).expect("fixture keys are 1..=32 bytes")
}

/// The `Ok` of a `try_` form; the in-memory engine never fails.
#[track_caller]
fn ok<T>(result: Result<T, StorageError>) -> T {
    result.expect("the in-memory engine never reports a storage error")
}

/// A write on the owner path, or on the host path.
enum Step {
    Owner(OwnerId, WriteOp),
    Host(WriteOp),
}

fn record(record: &[u8], height: u64, tag: &str) -> WriteOp {
    WriteOp::CreateRecord {
        namespace: ns(1),
        key: key(record),
        cells: vec![
            (name("h"), Cell::attribute(Value::U64(height))),
            (name("t"), Cell::attribute(Value::str(tag))),
            (name("f"), Cell::field(Value::bytes(&[7]))),
        ],
    }
}

fn workload() -> Vec<Step> {
    let create = |text: &str| WriteOp::CreateNamespace {
        name: NamespaceName::new(text).unwrap(),
    };
    let put = |namespace: u64, raw: &[u8], value: &[u8]| WriteOp::Put {
        namespace: ns(namespace),
        key: raw.to_vec(),
        value: value.to_vec(),
    };
    vec![
        Step::Owner(ALICE, create("alpha")),
        Step::Owner(ALICE, create("beta")),
        Step::Owner(ALICE, put(1, b"k1", b"v1")),
        Step::Owner(ALICE, put(1, b"k2", b"v2")),
        Step::Owner(ALICE, record(b"r1", 2, "x")),
        Step::Owner(ALICE, record(b"r2", 2, "y")),
        Step::Owner(ALICE, record(b"r3", 6, "x")),
        Step::Owner(
            ALICE,
            WriteOp::PatchRecord {
                namespace: ns(1),
                key: key(b"r3"),
                set: vec![(name("t"), Cell::attribute(Value::str("z")))],
                remove: vec![name("f")],
            },
        ),
        Step::Host(WriteOp::DeleteRecord {
            namespace: ns(1),
            key: key(b"r2"),
        }),
        Step::Owner(
            ALICE,
            WriteOp::Delete {
                namespace: ns(1),
                key: b"k2".to_vec(),
            },
        ),
        Step::Owner(
            ALICE,
            WriteOp::TransferNamespace {
                namespace: ns(2),
                expected_revision: Revision(0),
                new_owner: BOB,
            },
        ),
        // Rejected: unauthorized, duplicate, invalid argument, host raw write.
        Step::Owner(ALICE, put(2, b"k", b"v")),
        Step::Owner(ALICE, record(b"r1", 1, "dup")),
        Step::Owner(ALICE, put(1, &[1; 21], b"")),
        Step::Host(put(1, b"k", b"v")),
        Step::Owner(ALICE, record(b"r4", 3, "w")),
        Step::Owner(ALICE, record(b"r5", 3, "x")),
    ]
}

/// Runs the workload on two branches, one through the infallible methods
/// and one through the `try_` forms, comparing every result.
fn run_in_lockstep(infallible: &mut Branch, fallible: &mut Branch) {
    let traced = Budget::unlimited().traced();
    let mut rejected = 0;
    for (index, step) in workload().into_iter().enumerate() {
        let (left, right) = match step {
            Step::Owner(actor, op) => {
                assert_eq!(
                    infallible.estimate(actor, op.clone(), traced),
                    ok(fallible.try_estimate(actor, op.clone(), traced)),
                    "estimate of step {index}"
                );
                (
                    infallible.execute(actor, op.clone(), traced),
                    ok(fallible.try_execute(actor, op, traced)),
                )
            }
            Step::Host(op) => {
                assert_eq!(
                    infallible.estimate_host(op.clone(), traced),
                    ok(fallible.try_estimate_host(op.clone(), traced)),
                    "host estimate of step {index}"
                );
                (
                    infallible.execute_host(op.clone(), traced),
                    ok(fallible.try_execute_host(op, traced)),
                )
            }
        };
        rejected += usize::from(right.is_err());
        assert_eq!(left, right, "step {index}");
        assert_eq!(infallible.receipts(), fallible.receipts());
        assert_eq!(infallible.spent(), fallible.spent());
    }
    assert_eq!(rejected, 4, "the workload's four rejections");
}

fn queries() -> Vec<Query> {
    vec![
        Query::match_all(),
        Query::term(Term::eq(name("t"), Value::str("x"))),
        Query::term(Term::range(
            name("h"),
            Bound::Included(Value::U64(2)),
            Bound::Excluded(Value::U64(10)),
        )),
        Query::all(vec![arkiv_db::Literal::not(Term::eq(
            name("t"),
            Value::str("x"),
        ))]),
        // Rejected at admission: no groups.
        Query { groups: Vec::new() },
    ]
}

/// Compares every read of a branch through both forms, including cursors
/// handed from one form to the other.
fn compare_branch_reads(branch: &Branch) {
    let traced = Budget::unlimited().traced();
    let starved = Budget::limit(3);
    assert_eq!(ok(branch.try_namespace_count()), branch.namespace_count());
    for ordinal in 1..=3 {
        let namespace = ns(ordinal);
        for budget in [traced, starved] {
            for raw in [b"k1".as_slice(), b"k2", &[0; 21]] {
                assert_eq!(
                    ok(branch.try_get(namespace, raw, budget)),
                    branch.get(namespace, raw, budget)
                );
            }
            for prefix in [b"".as_slice(), b"k", &[0; 21]] {
                assert_eq!(
                    ok(branch.try_scan_prefix(namespace, prefix, budget)),
                    branch.scan_prefix(namespace, prefix, budget)
                );
            }
            assert_eq!(
                ok(branch.try_descriptor(namespace, budget)),
                branch.descriptor(namespace, budget)
            );
            for record in [b"r1".as_slice(), b"r2", b"r3"] {
                for projection in [Projection::ALL, Projection::KEY_ONLY] {
                    assert_eq!(
                        ok(branch.try_get_record(namespace, &key(record), &projection, budget)),
                        branch.get_record(namespace, &key(record), &projection, budget)
                    );
                }
            }
            for query in queries() {
                assert_eq!(
                    ok(branch.try_count(namespace, &query, budget)),
                    branch.count(namespace, &query, budget)
                );
                let first =
                    ok(branch.try_query(namespace, &query, &Projection::ALL, 1, None, budget));
                assert_eq!(
                    first,
                    branch.query(namespace, &query, &Projection::ALL, 1, None, budget)
                );
                if let Ok(page) = first
                    && let Some(cursor) = page.value.next
                {
                    assert_eq!(
                        ok(branch.try_query(
                            namespace,
                            &query,
                            &Projection::ALL,
                            2,
                            Some(&cursor),
                            budget
                        )),
                        branch.query(
                            namespace,
                            &query,
                            &Projection::ALL,
                            2,
                            Some(&cursor),
                            budget
                        )
                    );
                }
            }
            for (tag, lower) in [
                (TypeTag::U64, Bound::Unbounded),
                (TypeTag::Str, Bound::Excluded(Value::str("x"))),
                (TypeTag::Bytes, Bound::Unbounded),
            ] {
                assert_eq!(
                    ok(branch.try_terms(namespace, &name("t"), tag, lower.clone(), 5, budget)),
                    branch.terms(namespace, &name("t"), tag, lower, 5, budget)
                );
            }
        }
        assert_eq!(
            ok(branch.try_record_count(namespace)),
            branch.record_count(namespace)
        );
        assert_eq!(
            ok(branch.try_typed_record_count(namespace)),
            branch.typed_record_count(namespace)
        );
        assert_eq!(
            ok(branch.try_term_count(namespace)),
            branch.term_count(namespace)
        );
        assert_eq!(
            ok(branch.try_index_consistent(namespace)),
            branch.index_consistent(namespace)
        );
    }
}

/// The same comparison for a snapshot, plus the sharing statistics against
/// `other`.
fn compare_snapshot_reads(snapshot: &Snapshot, other: &Snapshot) {
    let traced = Budget::unlimited().traced();
    let starved = Budget::limit(3);
    assert_eq!(
        ok(snapshot.try_namespace_count()),
        snapshot.namespace_count()
    );
    for ordinal in 1..=3 {
        let namespace = ns(ordinal);
        for budget in [traced, starved] {
            assert_eq!(
                ok(snapshot.try_get(namespace, b"k1", budget)),
                snapshot.get(namespace, b"k1", budget)
            );
            assert_eq!(
                ok(snapshot.try_scan_prefix(namespace, b"", budget)),
                snapshot.scan_prefix(namespace, b"", budget)
            );
            assert_eq!(
                ok(snapshot.try_descriptor(namespace, budget)),
                snapshot.descriptor(namespace, budget)
            );
            for record in [b"r1".as_slice(), b"r3", b"r9"] {
                assert_eq!(
                    ok(snapshot.try_get_record(namespace, &key(record), &Projection::ALL, budget)),
                    snapshot.get_record(namespace, &key(record), &Projection::ALL, budget)
                );
            }
            for query in queries() {
                assert_eq!(
                    ok(snapshot.try_count(namespace, &query, budget)),
                    snapshot.count(namespace, &query, budget)
                );
                // A full walk, alternating the forms page by page.
                let mut cursor = None;
                for page_index in 0.. {
                    let fallible = ok(snapshot.try_query(
                        namespace,
                        &query,
                        &Projection::ALL,
                        1,
                        cursor.as_ref(),
                        budget,
                    ));
                    let infallible = snapshot.query(
                        namespace,
                        &query,
                        &Projection::ALL,
                        1,
                        cursor.as_ref(),
                        budget,
                    );
                    assert_eq!(fallible, infallible, "page {page_index}");
                    cursor = match (page_index % 2, fallible, infallible) {
                        (0, Ok(page), _) | (_, _, Ok(page)) => page.value.next,
                        _ => None,
                    };
                    if cursor.is_none() {
                        break;
                    }
                }
            }
            assert_eq!(
                ok(snapshot.try_terms(
                    namespace,
                    &name("h"),
                    TypeTag::U64,
                    Bound::Unbounded,
                    9,
                    budget
                )),
                snapshot.terms(
                    namespace,
                    &name("h"),
                    TypeTag::U64,
                    Bound::Unbounded,
                    9,
                    budget
                )
            );
        }
        assert_eq!(
            ok(snapshot.try_record_count(namespace)),
            snapshot.record_count(namespace)
        );
        assert_eq!(
            ok(snapshot.try_typed_record_count(namespace)),
            snapshot.typed_record_count(namespace)
        );
        assert_eq!(
            ok(snapshot.try_term_count(namespace)),
            snapshot.term_count(namespace)
        );
        assert_eq!(
            ok(snapshot.try_index_consistent(namespace)),
            snapshot.index_consistent(namespace)
        );
        assert_eq!(
            ok(snapshot.try_records_shared_with(other, namespace)),
            snapshot.records_shared_with(other, namespace)
        );
        assert_eq!(
            ok(snapshot.try_typed_shared_with(other, namespace)),
            snapshot.typed_shared_with(other, namespace)
        );
    }
}

#[test]
fn writes_estimates_reads_and_statistics_match_the_infallible_forms() {
    let mut left_engine = Engine::new(PriceSchedule::FIXTURE);
    let mut right_engine = Engine::new(PriceSchedule::FIXTURE);
    let block = Some(Cost::new(50_000));
    let mut left = left_engine.begin(left_engine.head(), block).unwrap();
    let mut right = right_engine.begin(right_engine.head(), block).unwrap();
    run_in_lockstep(&mut left, &mut right);
    compare_branch_reads(&right);

    let left_sealed = left.seal();
    let right_sealed = right.seal();
    assert_eq!(left_sealed.receipts(), right_sealed.receipts());
    assert_eq!(left_sealed.spent(), right_sealed.spent());
    left_engine.publish(&left_sealed).unwrap();
    let one = right_engine.publish(&right_sealed).unwrap();

    // A second block that leaves namespace 2 untouched, so the sharing
    // statistics see both answers.
    let mut next = right_engine.begin(one, None).unwrap();
    let patch = WriteOp::PatchRecord {
        namespace: ns(1),
        key: key(b"r1"),
        set: vec![(name("t"), Cell::attribute(Value::str("q")))],
        remove: Vec::new(),
    };
    ok(next.try_execute(ALICE, patch, Budget::unlimited())).unwrap();
    let two = right_engine.publish(&next.seal()).unwrap();
    let first = right_engine.snapshot(one).unwrap();
    let second = right_engine.snapshot(two).unwrap();
    assert!(second.records_shared_with(&first, ns(1)));
    assert!(!second.typed_shared_with(&first, ns(1)));
    assert!(second.typed_shared_with(&first, ns(2)));
    compare_snapshot_reads(&first, &second);
    compare_snapshot_reads(&second, &first);
}

#[test]
fn the_expiry_sweep_matches_its_fallible_form() {
    for (delete_budget, halts) in [(Budget::unlimited(), false), (Budget::limit(10), true)] {
        let mut left_engine = Engine::new(PriceSchedule::FIXTURE);
        let mut right_engine = Engine::new(PriceSchedule::FIXTURE);
        let mut left = left_engine.begin(left_engine.head(), None).unwrap();
        let mut right = right_engine.begin(right_engine.head(), None).unwrap();
        run_in_lockstep(&mut left, &mut right);
        let height = name("h");
        let query_budget = Budget::unlimited();
        let expected = expire_records(
            &mut left,
            ns(1),
            &height,
            BlockNumber(3),
            query_budget,
            delete_budget,
        );
        let actual = ok(try_expire_records(
            &mut right,
            ns(1),
            &height,
            BlockNumber(3),
            query_budget,
            delete_budget,
        ));
        assert_eq!(actual.is_err(), halts);
        assert_eq!(actual, expected);
        assert_eq!(left.receipts(), right.receipts());
        compare_branch_reads(&right);
        left_engine.publish(&left.seal()).unwrap();
        right_engine.publish(&right.seal()).unwrap();
    }
}

#[test]
fn storage_errors_are_the_storage_contract_type() {
    let error: arkiv_db_storage::versioned::StorageError =
        StorageError::new(StorageErrorKind::Corrupt, "hash mismatch");
    assert_eq!(
        error.kind,
        arkiv_db_storage::versioned::StorageErrorKind::Corrupt
    );
    assert_eq!(error.to_string(), "Corrupt: hash mismatch");
}
