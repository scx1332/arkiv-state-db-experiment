//! The executor's failure paths and host-side rules that the fixtures do
//! not reach through valid chains: an inconsistent due schedule in host
//! state, a malformed `$expiresAt` cell, a branch that was not begun for
//! the chain, blocks that do not follow their parent (height, timestamp and
//! chain id; a wrong parent hash is refused by the chain, see `replay.rs`),
//! and the timestamp's irrelevance to execution.
//! Every failure is an `ExecError`: no candidate, no outcome. Also the
//! host's expiry read at the edge of the costs a schedule may have.

mod support;

use arkiv_db::{
    Branch, Budget, Cell, Cost, Engine, Limits, OperationClass, Outcome, PriceSchedule, Receipt,
    ReceiptKind, RecordKey, Rejection, Value, WriteOp,
};
use arkiv_node::{
    AdminOutcome, AdminRejection, BlockFault, BlockInputs, ChainParams, ChainSpec, ExecError,
    Header, Invariant, MemoryChain, OpOutcome, ScheduleFault, SuccessorFault, TxOutcome, TxStatus,
    execute_block,
};
use support::*;

fn spec() -> ChainSpec {
    ChainSpec::new(genesis()).expect("valid genesis")
}

fn branch(chain: &MemoryChain) -> Branch {
    let engine = chain.engine();
    engine
        .begin(engine.head(), Some(Cost::new(PARAMS.block_budget)))
        .expect("head")
}

/// A chain at height 2 whose namespace 2 holds `e` expiring at height 3
/// (so its schedule is `D(3, 2) = 1`, `T(3) = 1`). Namespace 1 is the Arkiv
/// entity namespace, which generic operations may not write; every fixture
/// creates it first and writes generic records to namespace 2.
fn chain_with_due_record() -> MemoryChain {
    let mut chain = MemoryChain::new(spec());
    chain
        .produce(
            2_000,
            txs(vec![tx(
                ALICE,
                1,
                1_000,
                vec![
                    create_namespace("entities"),
                    create_namespace("alpha"),
                    create(2, b"e", vec![], Some(until(3))),
                ],
            )]),
        )
        .expect("block 1");
    chain
        .produce(3_000, BlockInputs::default())
        .expect("block 2");
    chain
}

fn run(chain: &MemoryChain, branch: Branch, inputs: &BlockInputs) -> Result<(), ExecError> {
    let parent = *chain.head();
    execute_block(
        chain.spec(),
        branch,
        &parent,
        parent.height + 1,
        parent.timestamp_ms,
        inputs,
    )
    .map(|_| ())
}

fn count(value: u64) -> Vec<u8> {
    value.to_be_bytes().to_vec()
}

fn put_host(branch: &mut Branch, key: [u8; 32], value: &[u8]) {
    branch
        .try_host_put(key, value)
        .expect("memory engine")
        .expect("small value");
}

#[test]
fn a_due_count_without_records_is_inconsistent() {
    let chain = chain_with_due_record();
    let mut branch = branch(&chain);
    // Two due at height 3 in namespace 2, but only one record expires.
    put_host(&mut branch, due_key(3, 2), &count(2));
    put_host(&mut branch, total_key(3), &count(2));
    assert_eq!(
        run(&chain, branch, &BlockInputs::default()),
        Err(ExecError::ScheduleInconsistent(
            ScheduleFault::CountMismatch {
                height: 3,
                namespace: ns(2),
                due: 2,
                deleted: 1,
            }
        ))
    );
}

#[test]
fn a_total_that_disagrees_with_its_counts_is_inconsistent() {
    let chain = chain_with_due_record();
    let mut branch = branch(&chain);
    put_host(&mut branch, total_key(3), &count(5));
    assert_eq!(
        run(&chain, branch, &BlockInputs::default()),
        Err(ExecError::ScheduleInconsistent(
            ScheduleFault::TotalMismatch {
                height: 3,
                total: 5,
                due: 1,
            }
        ))
    );
}

#[test]
fn a_total_without_counts_is_inconsistent() {
    let chain = chain_with_due_record();
    let mut branch = branch(&chain);
    put_host(&mut branch, total_key(2), &count(1));
    assert_eq!(
        run(&chain, branch, &BlockInputs::default()),
        Err(ExecError::ScheduleInconsistent(
            ScheduleFault::OrphanTotal { height: 2 }
        ))
    );
}

#[test]
fn malformed_schedule_entries_are_inconsistent() {
    let chain = chain_with_due_record();
    for value in [vec![1, 2, 3], count(0), vec![0; 9]] {
        let mut branch = branch(&chain);
        put_host(&mut branch, due_key(3, 2), &value);
        assert_eq!(
            run(&chain, branch, &BlockInputs::default()),
            Err(ExecError::ScheduleInconsistent(
                ScheduleFault::MalformedEntry { key: due_key(3, 2) }
            ))
        );
    }
    // A due key with nonzero padding, and one naming namespace 0.
    let mut padded = due_key(3, 2);
    padded[31] = 1;
    let mut zero = due_key(3, 2);
    zero[16] = 0;
    for key in [padded, zero] {
        let mut branch = branch(&chain);
        put_host(&mut branch, key, &count(1));
        assert_eq!(
            run(&chain, branch, &BlockInputs::default()),
            Err(ExecError::ScheduleInconsistent(
                ScheduleFault::MalformedEntry { key }
            ))
        );
    }
}

/// Entries due after this block are not swept, and the sweep never reads a
/// namespace without due entries.
#[test]
fn later_due_entries_are_left_alone() {
    let chain = chain_with_due_record();
    let mut branch = branch(&chain);
    // Namespace 9 does not exist; its entries are due later.
    put_host(&mut branch, due_key(4, 9), &count(1));
    put_host(&mut branch, total_key(4), &count(1));
    let parent = *chain.head();
    let executed = execute_block(
        chain.spec(),
        branch,
        &parent,
        3,
        parent.timestamp_ms,
        &BlockInputs::default(),
    )
    .expect("the later entries are not read");
    assert_eq!(executed.outcomes.expiry.len(), 1);
}

/// A record the host path created with `$expiresAt` but no due entry: a
/// user deletion finds no count to decrement.
#[test]
fn a_missing_due_count_underflows() {
    let spec = spec();
    let mut engine = Engine::with_limits(PriceSchedule::FIXTURE, LIMITS);
    let mut setup = engine.begin(engine.head(), None).expect("head");
    for name in ["entities", "alpha"] {
        setup
            .try_execute(
                ALICE,
                WriteOp::CreateNamespace {
                    name: ns_name(name),
                },
                Budget::unlimited(),
            )
            .expect("memory engine")
            .expect("created");
    }
    setup
        .try_execute_host(
            WriteOp::CreateRecord {
                namespace: ns(2),
                key: key(b"h"),
                cells: vec![(name("$expiresAt"), Cell::attribute(Value::U64(50)))],
            },
            Budget::unlimited(),
        )
        .expect("memory engine")
        .expect("created");
    engine.publish(&setup.seal()).expect("published");
    let branch = engine
        .begin(engine.head(), Some(Cost::new(PARAMS.block_budget)))
        .expect("head");
    let parent = *spec.genesis_header();
    let inputs = txs(vec![tx(ALICE, 1, 1_000, vec![delete(2, b"h")])]);
    assert_eq!(
        execute_block(&spec, branch, &parent, 1, 2_000, &inputs).err(),
        Some(ExecError::ScheduleInconsistent(ScheduleFault::Underflow {
            height: 50,
            namespace: ns(2),
        }))
    );
}

/// A `$expiresAt` that is not a `U64` attribute cannot come from users; the
/// host's pre-read refuses to interpret it.
#[test]
fn a_malformed_expiry_cell_is_an_invariant_error() {
    let spec = spec();
    for cell in [
        Cell::field(Value::U64(50)),
        Cell::attribute(Value::str("50")),
    ] {
        let mut engine = Engine::with_limits(PriceSchedule::FIXTURE, LIMITS);
        let mut setup = engine.begin(engine.head(), None).expect("head");
        for name in ["entities", "alpha"] {
            setup
                .try_execute(
                    ALICE,
                    WriteOp::CreateNamespace {
                        name: ns_name(name),
                    },
                    Budget::unlimited(),
                )
                .expect("memory engine")
                .expect("created");
        }
        setup
            .try_execute_host(
                WriteOp::CreateRecord {
                    namespace: ns(2),
                    key: RecordKey::new(b"h").expect("key"),
                    cells: vec![(name("$expiresAt"), cell)],
                },
                Budget::unlimited(),
            )
            .expect("memory engine")
            .expect("created");
        engine.publish(&setup.seal()).expect("published");
        let branch = engine
            .begin(engine.head(), Some(Cost::new(PARAMS.block_budget)))
            .expect("head");
        let inputs = txs(vec![tx(
            ALICE,
            1,
            1_000,
            vec![patch(
                2,
                b"h",
                vec![("x", attr(Value::U64(1)))],
                vec![],
                None,
            )],
        )]);
        assert_eq!(
            execute_block(&spec, branch, spec.genesis_header(), 1, 2_000, &inputs).err(),
            Some(ExecError::Invariant(Invariant::MalformedExpiryCell {
                namespace: ns(2)
            }))
        );
    }
}

#[test]
fn the_branch_must_be_fresh_and_sized_for_the_chain() {
    let chain = MemoryChain::new(spec());
    let engine = chain.engine();
    let unbounded = engine.begin(engine.head(), None).expect("head");
    assert_eq!(
        run(&chain, unbounded, &BlockInputs::default()),
        Err(ExecError::Invariant(Invariant::Branch))
    );
    let mut used = branch(&chain);
    used.try_execute(
        ALICE,
        WriteOp::CreateNamespace {
            name: ns_name("alpha"),
        },
        Budget::unlimited(),
    )
    .expect("memory engine")
    .expect("created");
    assert_eq!(
        run(&chain, used, &BlockInputs::default()),
        Err(ExecError::Invariant(Invariant::Branch))
    );
    let other_limits = Engine::with_limits(
        PriceSchedule::FIXTURE,
        arkiv_db::Limits {
            max_cells: 9,
            ..LIMITS
        },
    );
    let foreign = other_limits
        .begin(other_limits.head(), Some(Cost::new(PARAMS.block_budget)))
        .expect("head");
    assert_eq!(
        run(&chain, foreign, &BlockInputs::default()),
        Err(ExecError::Invariant(Invariant::Branch))
    );
}

#[test]
fn blocks_must_follow_their_parent() {
    let chain = MemoryChain::new(spec());
    let parent = *chain.head();
    let invalid = |fault| Err(ExecError::InvalidBlock(BlockFault::Successor(fault)));
    let attempt = |parent: &Header, height, timestamp| {
        execute_block(
            chain.spec(),
            branch(&chain),
            parent,
            height,
            timestamp,
            &BlockInputs::default(),
        )
        .map(|_| ())
    };
    assert_eq!(attempt(&parent, 2, 2_000), invalid(SuccessorFault::Height));
    assert_eq!(attempt(&parent, 0, 2_000), invalid(SuccessorFault::Height));
    assert_eq!(attempt(&parent, 1, 999), invalid(SuccessorFault::Timestamp));
    let foreign = Header {
        chain_id: CHAIN + 1,
        ..parent
    };
    assert_eq!(
        attempt(&foreign, 1, 2_000),
        invalid(SuccessorFault::ChainId)
    );
    let last = Header {
        height: u64::MAX,
        ..parent
    };
    assert_eq!(attempt(&last, 0, 2_000), invalid(SuccessorFault::Height));
    assert_eq!(attempt(&parent, 1, 1_000), Ok(()));
}

/// The timestamp is a header field only: the same inputs at two timestamps
/// give identical outcomes and headers that differ only in the timestamp.
#[test]
fn execution_never_reads_the_timestamp() {
    let chain = chain_with_due_record();
    let parent = *chain.head();
    let inputs = txs(vec![tx(
        ALICE,
        2,
        1_000,
        vec![create(2, b"f", vec![], Some(until(5)))],
    )]);
    let early =
        execute_block(chain.spec(), branch(&chain), &parent, 3, 3_000, &inputs).expect("executes");
    let late = execute_block(chain.spec(), branch(&chain), &parent, 3, 9_999_999, &inputs)
        .expect("executes");
    assert_eq!(early.outcomes, late.outcomes);
    assert_eq!(
        Header {
            timestamp_ms: 3_000,
            ..late.header
        },
        early.header
    );
}

/// The host's expiry pre-read runs with an unlimited budget before the
/// engine sees an operation, whoever its actor is, so every schedule the
/// chain accepts must keep its cost representable. A schedule whose
/// pre-read costs exactly `u64::MAX` applies and the pre-read runs under
/// it; one unit dearer is `ReservationOverflow`.
#[test]
fn the_host_expiry_read_cannot_overflow() {
    let params = ChainParams {
        block_budget: u64::MAX,
        expiry_per_block: 1,
        ..PARAMS
    };
    let limits = Limits {
        max_page: 1,
        ..LIMITS
    };
    let mut chain = MemoryChain::new(
        ChainSpec::new(genesis_with(params, limits, PriceSchedule::FIXTURE)).expect("valid"),
    );
    // Pre-read of a record with `$expiresAt`: DirectoryLookup 1 + PointRead
    // (2^63 − 20) + ResultRow 2^63 + 18 × ReadByte 1 = 2^64 − 1. The
    // deletion reserve (2^63 + 199) and the sweep bound (2^63 + 44) fit.
    let edge = PriceSchedule::FIXTURE
        .with_id(2)
        .with_price(OperationClass::PointRead, (1 << 63) - 20)
        .with_price(OperationClass::ResultRow, 1 << 63);
    let over = edge
        .clone()
        .with_id(3)
        .with_price(OperationClass::PointRead, (1 << 63) - 19);
    let block = chain
        .produce(
            2_000,
            BlockInputs {
                admin: vec![set_pricing(1, edge), set_pricing(2, over)],
                txs: vec![tx(
                    ALICE,
                    1,
                    1_000,
                    vec![
                        create_namespace("entities"),
                        create_namespace("alpha"),
                        create(2, b"k", vec![], Some(until(50))),
                    ],
                )],
            },
        )
        .expect("block 1");
    assert_eq!(
        block.outcomes.admin,
        vec![
            AdminOutcome::Applied,
            AdminOutcome::Rejected(AdminRejection::ReservationOverflow),
        ]
    );
    assert_eq!(block.outcomes.txs[0].status, TxStatus::Committed);
    // Under `edge` both deletions pre-read `k` in full. Bob's: DirectoryLookup
    // 1, then Unauthorized. Alice's: DirectoryLookup 1, then PointRead
    // exceeds her budget of 1,000: OutOfBudget.
    let block = chain
        .produce(
            3_000,
            txs(vec![
                tx(BOB, 1, 1_000, vec![delete(2, b"k")]),
                tx(ALICE, 2, 1_000, vec![delete(2, b"k")]),
            ]),
        )
        .expect("the pre-reads do not overflow");
    let refused = |reason| TxOutcome {
        status: TxStatus::Failed { op_index: 0 },
        ops: vec![OpOutcome::Rejected {
            reason,
            receipt: Receipt {
                kind: ReceiptKind::Write,
                model_version: 3,
                schedule_id: 2,
                spent: Cost::new(1),
                outcome: Outcome::Rejected(reason),
                ledger: None,
            },
        }],
        spent: 1,
    };
    assert_eq!(
        block.outcomes.txs,
        vec![
            refused(Rejection::Unauthorized),
            refused(Rejection::OutOfBudget)
        ]
    );
    assert_eq!(block.outcomes.spent, 2);
}
