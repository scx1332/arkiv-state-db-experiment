//! Host interrupts of metered reads (`arkiv_db::interrupt`).
//!
//! What this file establishes:
//! - A raised interrupt changes nothing for a read whose budget did not opt
//!   in, nor for an interruptible read outside the flag's scope: counts,
//!   pages and receipts equal those of an undisturbed read. Writes on a
//!   branch inside a raised scope keep their receipts and state root.
//! - An interruptible snapshot or branch read inside the scope of a raised
//!   flag stops at its first charge with `OutOfBudget` and spends nothing.
//! - Opting in leaves the charges of an unraised read unchanged.
//!
//! Stopping part way through a long walk on durable storage is exercised by
//! the node's detached-read tests.

use arkiv_db::{
    Budget, Cell, CellName, Effect, Engine, Interrupt, NamespaceId, OwnerId, PriceSchedule,
    Projection, Query, RecordKey, Rejection, Term, Value, WriteOp,
};

const ALICE: OwnerId = OwnerId::new([0xa1; 20]);

fn fixture() -> (Engine, NamespaceId) {
    let mut engine = Engine::new(PriceSchedule::FIXTURE);
    let mut branch = engine.begin(engine.head(), None).unwrap();
    let created = branch
        .execute(
            ALICE,
            WriteOp::CreateNamespace {
                name: arkiv_db::NamespaceName::new("walk").unwrap(),
            },
            Budget::unlimited(),
        )
        .unwrap();
    let Effect::NamespaceCreated(namespace) = created.effect else {
        panic!("namespace creation reports its id");
    };
    for i in 0..300u64 {
        branch
            .execute(ALICE, create(namespace, i), Budget::unlimited())
            .unwrap();
    }
    engine.publish(&branch.seal()).unwrap();
    (engine, namespace)
}

fn create(namespace: NamespaceId, i: u64) -> WriteOp {
    WriteOp::CreateRecord {
        namespace,
        key: RecordKey::new(format!("r{i:04}").as_bytes()).unwrap(),
        cells: vec![(
            CellName::new("group").unwrap(),
            Cell::attribute(Value::U64(i % 3)),
        )],
    }
}

fn group(value: u64) -> Query {
    Query::term(Term::eq(CellName::new("group").unwrap(), Value::U64(value)))
}

#[test]
fn raised_interrupts_stop_only_opted_in_reads_inside_their_scope() {
    let (engine, namespace) = fixture();
    let snapshot = engine.head_snapshot();
    let all = Query::match_all();
    let count = snapshot
        .count(namespace, &all, Budget::unlimited())
        .unwrap();
    assert_eq!(count.value, 300);
    let page = snapshot
        .query(
            namespace,
            &group(1),
            &Projection::ALL,
            50,
            None,
            Budget::unlimited(),
        )
        .unwrap();
    let interrupt = Interrupt::new();
    // Opting in alone changes no charge.
    let opted = interrupt.scope(|| {
        snapshot
            .count(namespace, &all, Budget::unlimited().interruptible())
            .unwrap()
    });
    assert_eq!(opted, count);
    interrupt.raise();
    assert!(interrupt.is_raised());
    interrupt.scope(|| {
        // No opt-in: identical value and receipt.
        assert_eq!(
            snapshot
                .count(namespace, &all, Budget::unlimited())
                .unwrap(),
            count
        );
        assert_eq!(
            snapshot
                .query(
                    namespace,
                    &group(1),
                    &Projection::ALL,
                    50,
                    None,
                    Budget::unlimited()
                )
                .unwrap(),
            page
        );
        // Opted in: stopped at the first charge, before any work.
        for stopped in [
            snapshot
                .count(namespace, &all, Budget::limit(1_000_000).interruptible())
                .unwrap_err(),
            snapshot
                .query(
                    namespace,
                    &group(1),
                    &Projection::ALL,
                    50,
                    None,
                    Budget::unlimited().interruptible(),
                )
                .unwrap_err(),
        ] {
            assert_eq!(stopped.reason, Rejection::OutOfBudget);
            assert_eq!(stopped.receipt.spent.units(), 0);
        }
    });
    // Outside the scope the raised flag is not observed.
    assert_eq!(
        snapshot
            .count(namespace, &all, Budget::unlimited().interruptible())
            .unwrap(),
        count
    );
}

#[test]
fn block_execution_inside_a_raised_scope_is_unchanged() {
    let (engine, namespace) = fixture();
    let run = |scoped: Option<&Interrupt>| {
        let mut branch = engine.begin(engine.head(), None).unwrap();
        let mut receipts = Vec::new();
        let mut work = || {
            for i in 300..340 {
                receipts.push(
                    branch
                        .execute(ALICE, create(namespace, i), Budget::limit(100_000))
                        .unwrap()
                        .receipt,
                );
            }
            let counted = branch
                .count(namespace, &group(0), Budget::unlimited())
                .unwrap();
            receipts.push(counted.receipt);
            counted.value
        };
        let value = match scoped {
            Some(interrupt) => interrupt.scope(work),
            None => work(),
        };
        (value, receipts, branch.seal().state_root())
    };
    let interrupt = Interrupt::new();
    interrupt.raise();
    let quiet = run(None);
    assert_eq!(quiet.0, 114);
    assert_eq!(run(Some(&interrupt)), quiet);
    // A branch read that opts in is stopped like a snapshot read.
    let branch = engine.begin(engine.head(), None).unwrap();
    let stopped = interrupt
        .scope(|| branch.count(namespace, &group(0), Budget::unlimited().interruptible()))
        .unwrap_err();
    assert_eq!(stopped.reason, Rejection::OutOfBudget);
    assert_eq!(stopped.receipt.spent.units(), 0);
}
