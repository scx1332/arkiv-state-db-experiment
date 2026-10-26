//! Deterministic block fixtures: each is produced on a memory chain, checked
//! against hand-derived outcomes, replayed on a fresh engine to identical
//! bytes, and pinned by golden block hashes.
//!
//! The expected receipts are derived by hand from the model-3 charge table
//! under `PriceSchedule::FIXTURE` (derivations in the comments; weights:
//! DirectoryLookup 1, NamespaceCreate 50, NamespaceTransfer 20, PutRecord
//! 10, WriteKeyByte 2, WriteValueByte 1, PointRead 3, RecordCreate 20,
//! RecordPatch 10, RecordDelete 20, CellWrite 4, CellByte 1, CellRemove 4,
//! IndexInsert 6, IndexTermCreate 30, IndexRemove 6, RootUpdate 2, RowByte
//! 1). An index insert costs 6 + 2·2 = 10, plus 30 when its term is new; an
//! index removal costs 6 + 2·2 = 10. The injected `$expiresAt` cell (name 10
//! bytes, value 8) adds 18 cell bytes. The golden hashes are literals
//! recorded from a run: they pin determinism (a change to any byte of any
//! header, input or outcome changes them), not correctness, which comes
//! from the hand-derived outcomes here and from the independent oracle of
//! `block_model.rs`.
//!
//! Namespace 1 is the Arkiv entity namespace, which generic operations may
//! not write: every fixture creates it (`entities`) first, so the generic
//! namespaces are 2 and up.

mod support;

use arkiv_db::{
    Cost, Effect, Outcome, PriceSchedule, Receipt, ReceiptKind, RecordId, Rejection, Revision,
    Value, decode_exact, encode,
};
use arkiv_node::{
    AdminOutcome, AdminRejection, Block, BlockFault, BlockInputs, BlockOutcomes, ChainError,
    ChainParams, ChainSpec, ExecError, Expiry, ExpiryDeletion, ExpiryFault, Genesis, HostFault,
    Invariant, MemoryChain, OpOutcome, ReplayError, Scope, SuccessorFault, TxFault, TxOp,
    TxOutcome, TxStatus, replay,
};
use support::*;

fn receipt(schedule_id: u32, spent: u64, outcome: Outcome) -> Receipt {
    Receipt {
        kind: ReceiptKind::Write,
        model_version: 3,
        schedule_id,
        spent: Cost::new(spent),
        outcome,
        ledger: None,
    }
}

fn applied(effect: Effect, spent: u64) -> OpOutcome {
    OpOutcome::Applied {
        effect,
        receipt: receipt(1, spent, Outcome::Applied),
    }
}

fn rejected(reason: Rejection, spent: u64) -> OpOutcome {
    OpOutcome::Rejected {
        reason,
        receipt: receipt(1, spent, Outcome::Rejected(reason)),
    }
}

/// A committed transaction; `spent` is the hand-derived sum of its
/// operations' receipts.
fn committed(spent: u64, ops: Vec<OpOutcome>) -> TxOutcome {
    TxOutcome {
        status: TxStatus::Committed,
        ops,
        spent,
    }
}

/// A transaction failed at `op_index`; `spent` is the hand-derived sum of
/// its operations' receipts (rolled back and rejected ones included).
fn failed(op_index: u32, spent: u64, ops: Vec<OpOutcome>) -> TxOutcome {
    TxOutcome {
        status: TxStatus::Failed { op_index },
        ops,
        spent,
    }
}

fn id(ordinal: u64) -> RecordId {
    RecordId::from_ordinal(ordinal).expect("nonzero")
}

fn deletion(namespace: u64, record: u64, schedule_id: u32, spent: u64) -> ExpiryDeletion {
    ExpiryDeletion {
        namespace: ns(namespace),
        record_id: id(record),
        receipt: receipt(schedule_id, spent, Outcome::Applied),
    }
}

fn chain_of(genesis: &Genesis) -> MemoryChain {
    MemoryChain::new(ChainSpec::new(genesis.clone()).expect("valid genesis"))
}

/// A fixture run: the chain, its genesis and every block produced.
struct Fixture {
    genesis: Genesis,
    chain: MemoryChain,
    blocks: Vec<Block>,
}

impl Fixture {
    fn new(genesis: Genesis) -> Self {
        let chain = chain_of(&genesis);
        Self {
            genesis,
            chain,
            blocks: Vec::new(),
        }
    }

    /// Produces the next block one second after its parent.
    fn produce(&mut self, inputs: BlockInputs) -> &Block {
        let timestamp = self.chain.head().timestamp_ms + 1_000;
        let block = self
            .chain
            .produce(timestamp, inputs)
            .expect("block produced");
        self.blocks.push(block);
        self.blocks.last().expect("just pushed")
    }

    fn empty(&mut self) -> &Block {
        self.produce(BlockInputs::default())
    }

    fn snapshot(&self, height: u64) -> arkiv_db::Snapshot {
        self.chain.snapshot(height).expect("block exists")
    }

    /// Replays every block on a fresh engine: identical headers, outcomes
    /// and terminal states; every block round-trips through the codec; the
    /// block hashes equal the golden literals.
    fn finish(self, golden: &[&str]) {
        let replayed = replay(&self.genesis, &self.blocks).expect("replay reproduces the chain");
        assert_eq!(replayed.height(), self.chain.height());
        for block in &self.blocks {
            let height = block.header.height;
            assert_eq!(replayed.header(height), Some(&block.header));
            let bytes = encode(block);
            assert_eq!(decode_exact::<Block>(&bytes).as_ref(), Ok(block));
        }
        let hashes: Vec<String> = self
            .blocks
            .iter()
            .map(|block| hex(block.hash().as_slice()))
            .collect();
        let golden: Vec<String> = golden.iter().map(|hash| (*hash).to_owned()).collect();
        assert_eq!(hashes, golden, "golden block hashes");
    }
}

fn outcomes(txs: Vec<TxOutcome>) -> BlockOutcomes {
    let spent = txs.iter().map(|tx| tx.spent).sum();
    BlockOutcomes {
        admin: Vec::new(),
        expiry: Vec::new(),
        txs,
        spent,
    }
}

// ---------------------------------------------------------------------------

/// One transaction of five operations across the raw and typed keyspaces,
/// with an expiry, all committed in one block.
#[test]
fn multi_operation_transaction() {
    let mut fixture = Fixture::new(genesis());
    let block = fixture.produce(txs(vec![tx(
        ALICE,
        1,
        10_000,
        vec![
            create_namespace("entities"),
            create_namespace("alpha"),
            put(2, b"k1", b"hello"),
            create(2, b"r1", vec![("color", attr(Value::str("red")))], None),
            create(
                2,
                b"r2",
                vec![
                    ("color", attr(Value::str("red"))),
                    ("note", field(Value::str("hi"))),
                ],
                Some(until(10)),
            ),
            patch(2, b"r1", vec![("size", attr(Value::U64(7)))], vec![], None),
        ],
    )]));
    // Each CreateNamespace (entities, then alpha): DirectoryLookup 1 +
    // {NamespaceCreate 50 + RootUpdate 2} = 53.
    // Put k1 (2 bytes) = hello (5): 1 + {10 + 2·2 + 5 + 2·2} = 24.
    // CreateRecord r1 {color: "red"}: b = 5 + 3 = 8, n = 2 + 8 = 10;
    // 1 + PointRead 3 + {20 + 4 + 8 + 10 + 6} + two new terms 2·40 = 132.
    // CreateRecord r2 {color: "red", note: field "hi", $expiresAt: 10}:
    // b = 8 + 6 + 18 = 32, c = 3, n = 34; 1 + 3 + {20 + 12 + 32 + 34 + 6}
    // + $expiresAt value and existence terms new (2·40) + color's two terms
    // held by r1 (2·10) = 208.
    // PatchRecord r1 set {size: 7}: b = 4 + 8 = 12, row {color, size}
    // n = 2 + 8 + 12 = 22; 1 + 3 + {10 + 4 + 12 + 0 + 22 + 4} + two new
    // terms 2·40 = 136.
    let expected = outcomes(vec![committed(
        606,
        vec![
            applied(Effect::NamespaceCreated(ns(1)), 53),
            applied(Effect::NamespaceCreated(ns(2)), 53),
            applied(Effect::Put { replaced: false }, 24),
            applied(Effect::RecordCreated { id: id(1) }, 132),
            applied(Effect::RecordCreated { id: id(2) }, 208),
            applied(Effect::RecordPatched { id: id(1) }, 136),
        ],
    )]);
    assert_eq!(block.outcomes, expected);
    assert_eq!(block.outcomes.spent, 606);
    let snapshot = fixture.snapshot(1);
    assert_eq!(raw(&snapshot, 2, b"k1"), Some(b"hello".to_vec()));
    let r1 = record(&snapshot, 2, b"r1").expect("r1 is live");
    assert_eq!(
        r1.cells,
        vec![
            (name("color"), attr(Value::str("red"))),
            (name("size"), attr(Value::U64(7))),
        ]
    );
    let r2 = record(&snapshot, 2, b"r2").expect("r2 is live");
    assert_eq!(
        r2.cells,
        vec![
            (name("$expiresAt"), attr(Value::U64(10))),
            (name("color"), attr(Value::str("red"))),
            (name("note"), field(Value::str("hi"))),
        ]
    );
    // The due schedule: one record of namespace 2 at height 10.
    assert_eq!(
        host_entries(&snapshot),
        vec![
            (due_key(10, 2), 1u64.to_be_bytes().to_vec()),
            (total_key(10), 1u64.to_be_bytes().to_vec()),
        ]
    );
    fixture.finish(&[GOLDEN_MULTI_OP]);
}

const GOLDEN_MULTI_OP: &str = "a059fa03837f44ad0c7c4f15498259f31b81f5ea7d9e8d3082d6582e5d17cb57";

/// A transaction failing mid-way is rolled back with its receipts kept,
/// its later operations not executed, and the block continues; exact and
/// one-short transaction budgets.
#[test]
fn failing_transaction_rolls_back() {
    let mut fixture = Fixture::new(genesis());
    fixture.produce(txs(vec![tx(
        ALICE,
        1,
        1_000,
        vec![create_namespace("entities"), create_namespace("alpha")],
    )]));
    let blue = || vec![("color", attr(Value::str("blue")))];
    let block = fixture.produce(txs(vec![
        tx(
            ALICE,
            2,
            1_000,
            vec![
                create(2, b"r1", blue(), None),
                create(2, b"r1", blue(), None),
                put(2, b"k", b"v"),
            ],
        ),
        tx(ALICE, 3, 134, vec![create(2, b"r1", blue(), None)]),
        tx(ALICE, 4, 73, vec![create(2, b"r2", blue(), None)]),
        tx(ALICE, 5, 74, vec![create(2, b"r2", blue(), None)]),
    ]));
    // Create r1 {color: "blue"}: b = 5 + 4 = 9, n = 11; 1 + 3 + {20 + 4 +
    // 9 + 11 + 6} + two new terms 2·40 = 134. The second create of r1:
    // DirectoryLookup 1 + PointRead 3, then AlreadyExists: 4.
    // After the rollback the same create costs 134 again and mints id 1
    // again (the allocator was rolled back too).
    // Create r2 {color: "blue"} with both terms now held by r1: 1 + 3 + 50
    // + 10 + 10 = 74. With 73 the last insert does not fit: 64 charged.
    let expected = outcomes(vec![
        failed(
            1,
            138,
            vec![
                OpOutcome::RolledBack {
                    effect: Effect::RecordCreated { id: id(1) },
                    receipt: receipt(1, 134, Outcome::Applied),
                },
                rejected(Rejection::AlreadyExists, 4),
                OpOutcome::NotExecuted,
            ],
        ),
        committed(134, vec![applied(Effect::RecordCreated { id: id(1) }, 134)]),
        failed(0, 64, vec![rejected(Rejection::OutOfBudget, 64)]),
        committed(74, vec![applied(Effect::RecordCreated { id: id(2) }, 74)]),
    ]);
    assert_eq!(block.outcomes, expected);
    assert_eq!(block.outcomes.spent, 138 + 134 + 64 + 74);
    let snapshot = fixture.snapshot(2);
    assert_eq!(record(&snapshot, 2, b"r1").map(|row| row.id), Some(id(1)));
    assert_eq!(record(&snapshot, 2, b"r2").map(|row| row.id), Some(id(2)));
    assert_eq!(raw(&snapshot, 2, b"k"), None);
    fixture.finish(&[GOLDEN_ROLLBACK_1, GOLDEN_ROLLBACK_2]);
}

const GOLDEN_ROLLBACK_1: &str = "bf07ce06d4119777a58bf75f67817c56bee64a085696e8d0a2221433fb9eea5a";
const GOLDEN_ROLLBACK_2: &str = "dd4e659221481b13af8172417a7cd4357efff25772c87b8fdc3700dbc17d0e54";

/// A transfer takes effect for the next transaction of the same block:
/// the old owner is refused, the new owner writes; a stale revision is
/// refused. Request ids are scoped by actor: bob's request 1 is not
/// alice's.
#[test]
fn same_block_ownership_transfer() {
    let mut fixture = Fixture::new(genesis());
    fixture.produce(txs(vec![tx(
        ALICE,
        1,
        1_000,
        vec![create_namespace("entities"), create_namespace("alpha")],
    )]));
    let block = fixture.produce(txs(vec![
        tx(
            ALICE,
            2,
            1_000,
            vec![TxOp::TransferNamespace {
                namespace: ns(2),
                expected_revision: Revision(0),
                new_owner: BOB,
            }],
        ),
        tx(ALICE, 3, 1_000, vec![put(2, b"k", b"v")]),
        tx(BOB, 1, 1_000, vec![put(2, b"k", b"v")]),
        tx(
            BOB,
            2,
            1_000,
            vec![TxOp::TransferNamespace {
                namespace: ns(2),
                expected_revision: Revision(0),
                new_owner: ALICE,
            }],
        ),
    ]));
    // Transfer: 1 + {NamespaceTransfer 20 + RootUpdate 2} = 23. Alice's put
    // after it: DirectoryLookup 1, then Unauthorized. Bob's put of k = v:
    // 1 + {10 + 2 + 1 + 4} = 18. Bob's transfer naming revision 0:
    // DirectoryLookup 1, then StaleRevision (current 1).
    let expected = outcomes(vec![
        committed(
            23,
            vec![applied(
                Effect::Transferred {
                    revision: Revision(1),
                },
                23,
            )],
        ),
        failed(0, 1, vec![rejected(Rejection::Unauthorized, 1)]),
        committed(18, vec![applied(Effect::Put { replaced: false }, 18)]),
        failed(
            0,
            1,
            vec![rejected(
                Rejection::StaleRevision {
                    expected: Revision(0),
                    current: Revision(1),
                },
                1,
            )],
        ),
    ]);
    assert_eq!(block.outcomes, expected);
    let before = descriptor(&fixture.snapshot(1), 2).expect("namespace 2");
    let after = descriptor(&fixture.snapshot(2), 2).expect("namespace 2");
    assert_eq!((before.owner, before.revision), (ALICE, Revision(0)));
    assert_eq!((after.owner, after.revision), (BOB, Revision(1)));
    assert_eq!(raw(&fixture.snapshot(2), 2, b"k"), Some(b"v".to_vec()));
    fixture.finish(&[GOLDEN_TRANSFER_1, GOLDEN_TRANSFER_2]);
}

const GOLDEN_TRANSFER_1: &str = "bf07ce06d4119777a58bf75f67817c56bee64a085696e8d0a2221433fb9eea5a";
const GOLDEN_TRANSFER_2: &str = "6b1fcf19a5c06aa785365c5a1a77763db00e933ed2634e5d732c8592d3966af8";

/// Records expiring at height 3 are present at height 2 and deleted in
/// block 3, before any user transaction: block 3's transactions already
/// find them gone. A record expiring at 4 survives block 3.
/// `min_lifetime` resolves the height as `max(expires_at, height +
/// min_lifetime)`.
#[test]
fn expiry_at_the_boundary() {
    let mut fixture = Fixture::new(genesis());
    let tag = || vec![("tag", attr(Value::Bool(true)))];
    let block = fixture.produce(txs(vec![tx(
        ALICE,
        1,
        10_000,
        vec![
            create_namespace("entities"),
            create_namespace("alpha"),
            create(2, b"e1", tag(), Some(until(3))),
            create(
                2,
                b"e2",
                tag(),
                Some(Expiry {
                    expires_at: 0,
                    min_lifetime: 2,
                }),
            ),
            create(2, b"e3", tag(), Some(until(4))),
        ],
    )]));
    // Each record {tag: true, $expiresAt}: b = (3 + 1) + 18 = 22, c = 2,
    // n = 24; group {20 + 8 + 22 + 24 + 6} = 80. e1: four new terms, 1 + 3
    // + 80 + 160 = 244. e2 (height 1 + 2 = 3): all four terms held,
    // 1 + 3 + 80 + 40 = 124. e3: the U64(4) value term is new, 1 + 3 + 80
    // + 40 + 3·10 = 154.
    assert_eq!(
        block.outcomes,
        outcomes(vec![committed(
            628,
            vec![
                applied(Effect::NamespaceCreated(ns(1)), 53),
                applied(Effect::NamespaceCreated(ns(2)), 53),
                applied(Effect::RecordCreated { id: id(1) }, 244),
                applied(Effect::RecordCreated { id: id(2) }, 124),
                applied(Effect::RecordCreated { id: id(3) }, 154),
            ],
        )])
    );
    let two = u64::to_be_bytes(2).to_vec();
    let one = u64::to_be_bytes(1).to_vec();
    fixture.empty();
    let at_two = fixture.snapshot(2);
    assert!(record(&at_two, 2, b"e1").is_some());
    assert!(record(&at_two, 2, b"e2").is_some());
    assert_eq!(host(&at_two, &due_key(3, 2)), Some(two.clone()));
    assert_eq!(host(&at_two, &total_key(3)), Some(two));
    // Block 3 sweeps first, so its transactions see e1 and e2 gone: a
    // patch of e1 is NotFound and a creation of e2 mints a fresh id.
    let three = fixture
        .produce(txs(vec![
            tx(
                ALICE,
                2,
                1_000,
                vec![patch(
                    2,
                    b"e1",
                    vec![("tag", attr(Value::Bool(false)))],
                    vec![],
                    None,
                )],
            ),
            tx(ALICE, 3, 1_000, vec![create(2, b"e2", tag(), None)]),
        ]))
        .outcomes
        .clone();
    // Each deletion: 1 + 3 + {RecordDelete 20 + 2 cells × CellRemove 4 +
    // RootUpdate 6} + 4 term removals × 10 = 78, in record-id order.
    // Patch of the missing e1: DirectoryLookup 1 + PointRead 3, then
    // NotFound: 4. Create e2 {tag: true} without expiry: b = 3 + 1 = 4,
    // c = 1, n = 2 + 4 = 6; 1 + 3 + {20 + 4 + 4 + 6 + 6} + both terms held
    // by e3 2·10 = 64, as record 4 (ids are never reused).
    assert_eq!(
        three,
        BlockOutcomes {
            admin: Vec::new(),
            expiry: vec![deletion(2, 1, 1, 78), deletion(2, 2, 1, 78)],
            txs: vec![
                failed(0, 4, vec![rejected(Rejection::NotFound, 4)]),
                committed(64, vec![applied(Effect::RecordCreated { id: id(4) }, 64)]),
            ],
            spent: 156 + 4 + 64,
        }
    );
    let at_three = fixture.snapshot(3);
    assert!(record(&at_three, 2, b"e1").is_none());
    assert_eq!(record(&at_three, 2, b"e2").map(|row| row.id), Some(id(4)));
    assert!(record(&at_three, 2, b"e3").is_some());
    assert_eq!(
        host_entries(&at_three),
        vec![(due_key(4, 2), one.clone()), (total_key(4), one)]
    );
    let four = fixture.empty().outcomes.clone();
    assert_eq!(four.expiry, vec![deletion(2, 3, 1, 78)]);
    assert!(record(&fixture.snapshot(4), 2, b"e3").is_none());
    assert!(host_entries(&fixture.snapshot(4)).is_empty());
    // The history keeps what each height had.
    assert!(record(&fixture.snapshot(2), 2, b"e1").is_some());
    fixture.finish(&[
        GOLDEN_BOUNDARY_1,
        GOLDEN_BOUNDARY_2,
        GOLDEN_BOUNDARY_3,
        GOLDEN_BOUNDARY_4,
    ]);
}

const GOLDEN_BOUNDARY_1: &str = "a53b66d77f70f030e60f19b73b5d88ee3a7dcb1fb677c3a1569157618cc52d52";
const GOLDEN_BOUNDARY_2: &str = "bd2c25ea771e19213f39079ac5ab2e592ef66c763ad2646b532d98a4c09870a0";
const GOLDEN_BOUNDARY_3: &str = "983ee851f03141194b593f7a968c5c6de3d639071e48c79f3f7ef1ac84ccfadf";
const GOLDEN_BOUNDARY_4: &str = "0a478eecb993c364f297107c18ee78b7abd241e47c2c98508b5b37f54e67689f";

/// Eight cells, all attributes, under a 32-byte key: the most expensive
/// record to expire.
fn max_cell_record(namespace: u64, index: u8, expiry: Expiry) -> TxOp {
    let key_bytes = [index; 32];
    let cells = ["a0", "a1", "a2", "a3", "a4", "a5", "a6"]
        .into_iter()
        .map(|text| (text, attr(Value::U64(u64::from(index)))))
        .collect();
    create(namespace, &key_bytes, cells, Some(expiry))
}

/// The expiry capacity: four max-cell records with 32-byte keys across
/// three namespaces expire at one height, under a schedule whose
/// reservation is exact (the block budget is `user_budget + 4 × 222`); a
/// fifth record at that height is refused, a patch keeping its record's
/// height is not; the sweep reads one-row pages; the block of the sweep
/// still gives a transaction the whole user budget. A schedule one unit
/// more expensive per deletion is refused.
#[test]
fn expiry_at_capacity() {
    let limits = arkiv_db::Limits {
        max_page: 1,
        ..LIMITS
    };
    let mut fixture = Fixture::new(genesis_with(PARAMS, limits, PriceSchedule::FIXTURE));
    let dearer = PriceSchedule::FIXTURE
        .with_id(2)
        .with_price(arkiv_db::OperationClass::RecordDelete, 21);
    let block = fixture.produce(BlockInputs {
        admin: vec![set_pricing(1, dearer)],
        txs: vec![tx(
            ALICE,
            1,
            1_000,
            vec![
                create_namespace("entities"),
                create_namespace("alpha"),
                create_namespace("beta"),
                create_namespace("gamma"),
            ],
        )],
    });
    // One unit more per deletion: 100,000 + 4 × 223 = 100,892 > 100,888.
    assert_eq!(
        block.outcomes.admin,
        vec![AdminOutcome::Rejected(AdminRejection::ReservationViolated)]
    );
    let block = fixture.produce(txs(vec![
        tx(
            ALICE,
            2,
            10_000,
            vec![
                max_cell_record(2, 1, until(4)),
                max_cell_record(2, 2, until(4)),
                max_cell_record(3, 3, until(4)),
                max_cell_record(4, 4, until(4)),
            ],
        ),
        tx(ALICE, 3, 10_000, vec![max_cell_record(4, 5, until(4))]),
        tx(
            ALICE,
            4,
            10_000,
            vec![patch(
                2,
                &[1; 32],
                vec![("a0", attr(Value::U64(100)))],
                vec![],
                Some(until(4)),
            )],
        ),
    ]));
    assert!(matches!(block.outcomes.txs[0].status, TxStatus::Committed));
    assert_eq!(
        block.outcomes.txs[1],
        failed(
            0,
            0,
            vec![OpOutcome::HostRejected {
                fault: HostFault::ExpiryCapacityFull
            }]
        )
    );
    assert!(matches!(block.outcomes.txs[2].status, TxStatus::Committed));
    let four = u64::to_be_bytes(4).to_vec();
    assert_eq!(host(&fixture.snapshot(2), &total_key(4)), Some(four));
    fixture.empty();
    // The sweep block also carries a transaction with the whole user budget.
    let block = fixture.produce(txs(vec![tx(
        ALICE,
        5,
        100_000,
        vec![create(2, b"after", vec![], None)],
    )]));
    // Each deletion: 1 + 3 + {20 + 8·4 + 6} + 16 removals × 10 = 222, in
    // (namespace, record id) order.
    assert_eq!(
        block.outcomes.expiry,
        vec![
            deletion(2, 1, 1, 222),
            deletion(2, 2, 1, 222),
            deletion(3, 1, 1, 222),
            deletion(4, 1, 1, 222),
        ]
    );
    // Create {} under a 5-byte key: 1 + 3 + {20 + 0 + 0 + 5 + 6} = 35.
    assert_eq!(
        block.outcomes.txs,
        vec![committed(
            35,
            vec![applied(Effect::RecordCreated { id: id(3) }, 35)]
        )]
    );
    assert_eq!(block.outcomes.spent, 888 + 35);
    let snapshot = fixture.snapshot(4);
    for (namespace, index) in [(1, 1), (1, 2), (2, 3), (3, 4)] {
        assert!(record(&snapshot, namespace, &[index; 32]).is_none());
    }
    assert!(host_entries(&snapshot).is_empty());
    fixture.finish(&[
        GOLDEN_CAPACITY_1,
        GOLDEN_CAPACITY_2,
        GOLDEN_CAPACITY_3,
        GOLDEN_CAPACITY_4,
    ]);
}

const GOLDEN_CAPACITY_1: &str = "08426b5595a71a3196f5eb839a527753909af4f42850e861ef9373cc933922a7";
const GOLDEN_CAPACITY_2: &str = "43ab9a4c096eccce08f4919b254b6cd1b50fa22633cf1d680831b986633adfa9";
const GOLDEN_CAPACITY_3: &str = "9b9d1bb433a54c98693f4d9f57c4ceaadaaf9b8d4de7c5eb19d5d8920e773e8a";
const GOLDEN_CAPACITY_4: &str = "11750528f136aa8f44ff032295ba7eea2ed9b6f73473db88484379490d4e4b23";

/// Every read weight ×100 (DirectoryLookup, PointRead, ScanSeek, ScanEntry,
/// ReadByte, QueryGroup, TermSeek, TermVisit, PostingEntry, RowVisit,
/// RecordProbe, ResultRow).
fn reads_times_100() -> PriceSchedule {
    use arkiv_db::OperationClass::*;
    let fixture = PriceSchedule::FIXTURE;
    [
        DirectoryLookup,
        PointRead,
        ScanSeek,
        ScanEntry,
        ReadByte,
        QueryGroup,
        TermSeek,
        TermVisit,
        PostingEntry,
        RowVisit,
        RecordProbe,
        ResultRow,
    ]
    .into_iter()
    .fold(fixture.clone().with_id(2), |schedule, class| {
        schedule.with_price(class, fixture.price(class) * 100)
    })
}

/// A schedule raising every read weight 100× takes effect in the next
/// block; expiry still completes under it; a stale schedule id is refused.
#[test]
fn set_pricing_raises_read_weights() {
    // Under the new schedule a deletion reserves 100 + 300 + 20 + 32 + 6 +
    // 160 = 618 per record, 2,472 for four: the block budget must cover
    // 102,472.
    let params = ChainParams {
        block_budget: 110_000,
        ..PARAMS
    };
    let mut fixture = Fixture::new(genesis_with(params, LIMITS, PriceSchedule::FIXTURE));
    let tag = || vec![("tag", attr(Value::Bool(true)))];
    let block = fixture.produce(BlockInputs {
        admin: vec![set_pricing(1, reads_times_100())],
        txs: vec![tx(
            ALICE,
            1,
            10_000,
            vec![
                create_namespace("entities"),
                create_namespace("alpha"),
                create(2, b"e1", tag(), Some(until(3))),
            ],
        )],
    });
    assert_eq!(block.outcomes.admin, vec![AdminOutcome::Applied]);
    // Block 1 still runs on schedule 1: 2 × 53 + 244.
    assert_eq!(block.outcomes.spent, 350);
    let block = fixture.produce(BlockInputs {
        admin: vec![set_pricing(2, PriceSchedule::FIXTURE)],
        txs: vec![tx(
            ALICE,
            2,
            10_000,
            vec![create(2, b"e2", tag(), Some(until(3)))],
        )],
    });
    assert_eq!(
        block.outcomes.admin,
        vec![AdminOutcome::Rejected(AdminRejection::StaleSchedule {
            current: 2
        })]
    );
    // Under schedule 2: DirectoryLookup 100 + PointRead 300 + {80} + four
    // held terms 4·10 = 520.
    assert_eq!(
        block.outcomes.txs,
        vec![TxOutcome {
            status: TxStatus::Committed,
            ops: vec![OpOutcome::Applied {
                effect: Effect::RecordCreated { id: id(2) },
                receipt: receipt(2, 520, Outcome::Applied),
            }],
            spent: 520,
        }]
    );
    // The sweep of block 3 reads with the ×100 bound (15,500 units) and
    // deletes both: 100 + 300 + {20 + 8 + 6} + 4·10 = 474 each.
    let three = fixture.empty().outcomes.clone();
    assert_eq!(
        three.expiry,
        vec![deletion(2, 1, 2, 474), deletion(2, 2, 2, 474)]
    );
    assert_eq!(fixture.snapshot(3).pricing(), &reads_times_100());
    assert!(record(&fixture.snapshot(3), 2, b"e1").is_none());
    fixture.finish(&[GOLDEN_PRICING_1, GOLDEN_PRICING_2, GOLDEN_PRICING_3]);
}

const GOLDEN_PRICING_1: &str = "cf77ecc630c8b46245feabb22520fe58854eaf549ebc281088ca37a85117f379";
const GOLDEN_PRICING_2: &str = "81ddd970607f524c801a402f2e06f65a6cbfbab1db40369a96e021f804161f07";
const GOLDEN_PRICING_3: &str = "ae22315cf5d84eea841c60886d6f199cd5e472904e4f0b3d702b63243bf23114";

/// Permanent records: `expires_at = u64::MAX` stores `U64(u64::MAX)` and
/// schedules nothing; extending a finite record to permanent removes its
/// due entry; a record without `$expiresAt` is permanent, so any finite
/// expiry would shorten it.
#[test]
fn permanent_expiry() {
    let mut fixture = Fixture::new(genesis());
    let tag = || vec![("tag", attr(Value::Bool(true)))];
    fixture.produce(txs(vec![tx(
        ALICE,
        1,
        10_000,
        vec![
            create_namespace("entities"),
            create_namespace("alpha"),
            create(2, b"p1", tag(), Some(Expiry::PERMANENT)),
            create(2, b"p2", tag(), Some(until(3))),
            create(2, b"p3", tag(), None),
        ],
    )]));
    let snapshot = fixture.snapshot(1);
    assert_eq!(
        record(&snapshot, 2, b"p1").expect("p1").cells[0],
        (name("$expiresAt"), attr(Value::U64(u64::MAX)))
    );
    assert_eq!(
        host_entries(&snapshot),
        vec![
            (due_key(3, 2), 1u64.to_be_bytes().to_vec()),
            (total_key(3), 1u64.to_be_bytes().to_vec()),
        ]
    );
    let block = fixture.produce(txs(vec![
        tx(
            ALICE,
            2,
            10_000,
            vec![patch(2, b"p2", vec![], vec![], Some(Expiry::PERMANENT))],
        ),
        tx(
            ALICE,
            3,
            10_000,
            vec![patch(2, b"p3", vec![], vec![], Some(until(50)))],
        ),
        tx(
            ALICE,
            4,
            10_000,
            vec![patch(2, b"p1", vec![], vec![], Some(Expiry::PERMANENT))],
        ),
    ]));
    assert!(matches!(block.outcomes.txs[0].status, TxStatus::Committed));
    assert_eq!(
        block.outcomes.txs[1].ops,
        vec![OpOutcome::HostRejected {
            fault: HostFault::ExpiryShortened
        }]
    );
    assert!(matches!(block.outcomes.txs[2].status, TxStatus::Committed));
    assert!(host_entries(&fixture.snapshot(2)).is_empty());
    let three = fixture.empty().outcomes.clone();
    assert!(three.expiry.is_empty());
    let snapshot = fixture.snapshot(3);
    for key_bytes in [b"p1", b"p2", b"p3"] {
        assert!(record(&snapshot, 2, key_bytes).is_some());
    }
    assert_eq!(
        record(&snapshot, 2, b"p2").expect("p2").cells[0],
        (name("$expiresAt"), attr(Value::U64(u64::MAX)))
    );
    fixture.finish(&[GOLDEN_PERMANENT_1, GOLDEN_PERMANENT_2, GOLDEN_PERMANENT_3]);
}

const GOLDEN_PERMANENT_1: &str = "25544718ff86b27a078f23bcdf952b1f29e64382be4c8f79b025db6665216c09";
const GOLDEN_PERMANENT_2: &str = "d7495e74e2270eaeb3c6c2f55da1cbfe7fff1526a65671a69d642915bbf45ee8";
const GOLDEN_PERMANENT_3: &str = "2060a06fbfef675a1673c3365d533420102a8a19947274fd7581b84bb7d802bd";

/// Expiry only extends: an earlier height is refused, the same height is
/// accepted without moving the schedule, a later height moves it.
#[test]
fn expiry_extends_only() {
    let mut fixture = Fixture::new(genesis());
    let tag = || vec![("tag", attr(Value::Bool(true)))];
    fixture.produce(txs(vec![tx(
        ALICE,
        1,
        10_000,
        vec![
            create_namespace("entities"),
            create_namespace("alpha"),
            create(2, b"x1", tag(), Some(until(5))),
        ],
    )]));
    let block = fixture.produce(txs(vec![
        tx(
            ALICE,
            2,
            10_000,
            vec![
                patch(2, b"x1", vec![("n", attr(Value::I64(-1)))], vec![], None),
                patch(2, b"x1", vec![], vec![], Some(until(4))),
            ],
        ),
        tx(
            ALICE,
            3,
            10_000,
            vec![patch(2, b"x1", vec![], vec![], Some(until(5)))],
        ),
        tx(
            ALICE,
            4,
            10_000,
            vec![patch(2, b"x1", vec![], vec![], Some(until(7)))],
        ),
    ]));
    let outcome = &block.outcomes.txs[0];
    assert_eq!(outcome.status, TxStatus::Failed { op_index: 1 });
    assert!(matches!(outcome.ops[0], OpOutcome::RolledBack { .. }));
    assert_eq!(
        outcome.ops[1],
        OpOutcome::HostRejected {
            fault: HostFault::ExpiryShortened
        }
    );
    // Patch {$expiresAt: 5} of x1 {tag, $expiresAt: 5}: b = 18, row n = 2 +
    // 4 + 18 = 24; 1 + 3 + {10 + 4 + 18 + 0 + 24 + 4} = 64, and no index
    // change because the value is unchanged.
    assert_eq!(
        block.outcomes.txs[1],
        committed(64, vec![applied(Effect::RecordPatched { id: id(1) }, 64)])
    );
    // To 7: the same 64, plus removing the U64(5) value term and inserting
    // the new U64(7) value term: 64 + 10 + 40 = 114.
    assert_eq!(
        block.outcomes.txs[2],
        committed(114, vec![applied(Effect::RecordPatched { id: id(1) }, 114)])
    );
    let one = u64::to_be_bytes(1).to_vec();
    assert_eq!(
        host_entries(&fixture.snapshot(2)),
        vec![(due_key(7, 2), one.clone()), (total_key(7), one)]
    );
    for _ in 3..=6 {
        assert!(fixture.empty().outcomes.expiry.is_empty());
    }
    assert!(record(&fixture.snapshot(6), 2, b"x1").is_some());
    let seven = fixture.empty().outcomes.clone();
    assert_eq!(seven.expiry.len(), 1);
    assert!(record(&fixture.snapshot(7), 2, b"x1").is_none());
    fixture.finish(&[
        GOLDEN_EXTEND_1,
        GOLDEN_EXTEND_2,
        GOLDEN_EXTEND_3,
        GOLDEN_EXTEND_4,
        GOLDEN_EXTEND_5,
        GOLDEN_EXTEND_6,
        GOLDEN_EXTEND_7,
    ]);
}

const GOLDEN_EXTEND_1: &str = "08debc69c1fe67b67b0fec95b47271b1b7b5461452ac7a88a790ca2340bd0267";
const GOLDEN_EXTEND_2: &str = "f767714923f0c1c6b59f216f02733e17f7ad9839b1ad461f040853a86b02bd17";
const GOLDEN_EXTEND_3: &str = "aad4023cce0f6b8576c7f9e20e0db851263b74001c5c6c83c6d50922d1612381";
const GOLDEN_EXTEND_4: &str = "da0b1a7d670dcde48e717b987dbc89d9977fdc0560a0e5197471324d8fbe550d";
const GOLDEN_EXTEND_5: &str = "8a0ce1f072bf8c871d97f1dc8cf729814970388284e558e852190cc6c8b7a3ed";
const GOLDEN_EXTEND_6: &str = "e4a8af7320ca508d4d0721f298e4bfde2e9f943fbdc1141f77347d7cb75bc13c";
const GOLDEN_EXTEND_7: &str = "9ff43e9a5515c0498e8eb6242856462a7bd806a761759093b5a9bb953868086f";

/// Expiry pairs that do not resolve: dead on arrival, too far, and an
/// overflowing `height + min_lifetime` (reachable only with a huge
/// `max_lifetime`).
#[test]
fn unresolvable_expiry_is_host_rejected() {
    let params = ChainParams {
        max_lifetime: u64::MAX,
        max_rent_lifetime: arkiv_node::gas::DEFAULT_RENT_CAP_BLOCKS,
        ..PARAMS
    };
    let mut fixture = Fixture::new(genesis_with(params, LIMITS, PriceSchedule::FIXTURE));
    fixture.produce(txs(vec![tx(
        ALICE,
        1,
        1_000,
        vec![create_namespace("entities"), create_namespace("alpha")],
    )]));
    let tag = || vec![("tag", attr(Value::Bool(true)))];
    let block = fixture.produce(txs(vec![
        tx(
            ALICE,
            2,
            1_000,
            vec![create(2, b"d", tag(), Some(until(2)))],
        ),
        tx(
            ALICE,
            3,
            1_000,
            vec![create(
                2,
                b"o",
                tag(),
                Some(Expiry {
                    expires_at: 5,
                    min_lifetime: u64::MAX,
                }),
            )],
        ),
    ]));
    let host_rejected = |fault| {
        failed(
            0,
            0,
            vec![OpOutcome::HostRejected {
                fault: HostFault::ExpiryInvalid(fault),
            }],
        )
    };
    assert_eq!(
        block.outcomes,
        outcomes(vec![
            host_rejected(ExpiryFault::DeadOnArrival),
            host_rejected(ExpiryFault::Overflow),
        ])
    );
    fixture.finish(&[GOLDEN_FAULTS_1, GOLDEN_FAULTS_2]);

    // Too far: at height 2 with max_lifetime 100, 103 is refused and 102
    // accepted.
    let mut fixture = Fixture::new(genesis());
    fixture.produce(txs(vec![tx(
        ALICE,
        1,
        1_000,
        vec![create_namespace("entities"), create_namespace("alpha")],
    )]));
    let block = fixture.produce(txs(vec![
        tx(
            ALICE,
            2,
            1_000,
            vec![create(2, b"f", tag(), Some(until(103)))],
        ),
        tx(
            ALICE,
            3,
            1_000,
            vec![create(2, b"g", tag(), Some(until(102)))],
        ),
    ]));
    // g {tag: true, $expiresAt: 102}: b = 4 + 18 = 22, c = 2, n = 1 + 22 =
    // 23; 1 + 3 + {20 + 8 + 22 + 23 + 6} + four new terms 4·40 = 243.
    assert_eq!(
        block.outcomes,
        outcomes(vec![
            host_rejected(ExpiryFault::TooFar),
            committed(243, vec![applied(Effect::RecordCreated { id: id(1) }, 243)]),
        ])
    );
    fixture.finish(&[GOLDEN_TOO_FAR_1, GOLDEN_TOO_FAR_2]);
}

const GOLDEN_FAULTS_1: &str = "cf637461fcad2657e2c394d59a76bbc4dc92ea65a166ef2be2c32ef5dec7838f";
const GOLDEN_FAULTS_2: &str = "0a080157234d4881693c03b0e365150cbc8f124aba89deb8450f2d6b07c6852b";
const GOLDEN_TOO_FAR_1: &str = "bf07ce06d4119777a58bf75f67817c56bee64a085696e8d0a2221433fb9eea5a";
const GOLDEN_TOO_FAR_2: &str = "f858a82df6f4eec4e346e3a6f4fab1dc5fb1972fff1e8e44af8442b6e0653e29";

/// Empty blocks still have headers, digests and outcomes; the timestamp may
/// repeat the parent's.
#[test]
fn empty_blocks() {
    let mut fixture = Fixture::new(genesis());
    let first = fixture.empty().clone();
    assert_eq!(first.outcomes, BlockOutcomes::default());
    assert_eq!(first.header.height, 1);
    assert_eq!(
        first.header.parent_hash,
        fixture.chain.spec().genesis_hash()
    );
    let parent = *fixture.chain.head();
    let second = fixture
        .chain
        .produce(parent.timestamp_ms, BlockInputs::default())
        .expect("same timestamp is allowed");
    fixture.blocks.push(second.clone());
    assert_eq!(second.header.parent_hash, first.hash());
    assert_ne!(second.hash(), first.hash());
    fixture.empty();
    fixture.finish(&[GOLDEN_EMPTY_1, GOLDEN_EMPTY_2, GOLDEN_EMPTY_3]);
}

const GOLDEN_EMPTY_1: &str = "50d8d7f13895dff00dba1815cf63b4291d54ac92fa0972ea8d068b87e0e5647b";
const GOLDEN_EMPTY_2: &str = "aaadddac10d4ce3201a5c2722b7caacd96dc2b6aaef4686ac2afda70c668771e";
const GOLDEN_EMPTY_3: &str = "8010792e671e59d243aaa041805679ea44c69de00118e718a87445d6b6bd01cd";

/// A recorded block whose header follows the chain but whose inputs repeat
/// an identity: within the block, or one included earlier. Replay refuses
/// it before executing anything; so does production. Scopes are separate:
/// an admin input may reuse a transaction's actor and request id.
#[test]
fn replay_rejects_duplicate_identities() {
    let genesis = genesis();
    let mut fixture = Fixture::new(genesis.clone());
    fixture.produce(BlockInputs {
        admin: vec![set_pricing(1, PriceSchedule::FIXTURE.with_id(2))],
        txs: vec![tx(
            ADMIN,
            1,
            1_000,
            vec![create_namespace("entities"), create_namespace("alpha")],
        )],
    });
    let good = fixture.blocks.clone();
    let header = |chain: &MemoryChain| arkiv_node::Header {
        chain_id: CHAIN,
        height: chain.height() + 1,
        parent_hash: chain.head().hash(),
        timestamp_ms: chain.head().timestamp_ms + 1,
        inputs_digest: Default::default(),
        outcomes_digest: Default::default(),
        state_root: Default::default(),
    };
    // Already included at height 1.
    let again = BlockInputs {
        admin: Vec::new(),
        txs: vec![tx(ADMIN, 1, 1_000, vec![create_namespace("beta")])],
    };
    // Twice in one block, with different contents.
    let twice = BlockInputs {
        admin: Vec::new(),
        txs: vec![
            tx(ALICE, 9, 1_000, vec![create_namespace("beta")]),
            tx(ALICE, 9, 1_000, vec![create_namespace("gamma")]),
        ],
    };
    for (inputs, fault) in [
        (
            again,
            BlockFault::AlreadyIncluded {
                scope: Scope::Tx,
                index: 0,
            },
        ),
        (
            twice,
            BlockFault::DuplicateIdentity {
                scope: Scope::Tx,
                index: 1,
            },
        ),
    ] {
        let recorded = Block {
            header: header(&fixture.chain),
            inputs: inputs.clone(),
            outcomes: BlockOutcomes::default(),
        };
        let mut blocks = good.clone();
        blocks.push(recorded);
        assert_eq!(
            replay(&genesis, &blocks).err(),
            Some(ReplayError::Block {
                height: 2,
                error: ChainError::InvalidBlock(fault),
            })
        );
        let height = fixture.chain.height();
        assert_eq!(
            fixture.chain.produce(10_000, inputs).err(),
            Some(ChainError::InvalidBlock(fault))
        );
        assert_eq!(fixture.chain.height(), height);
    }
    fixture.finish(&[GOLDEN_IDENTITY_1]);
}

const GOLDEN_IDENTITY_1: &str = "c47d63381db9b74098f946ff26ddae2b2ef668aabbd58632445ae9e6d250ebb5";

/// Replay refuses a recorded block with a statically invalid request, a
/// broken selection limit, a wrong parent, or outcomes or a header that
/// re-execution does not reproduce.
#[test]
fn replay_rejects_invalid_blocks() {
    let genesis = genesis();
    let mut fixture = Fixture::new(genesis.clone());
    fixture.produce(txs(vec![tx(
        ALICE,
        1,
        1_000,
        vec![create_namespace("entities"), create_namespace("alpha")],
    )]));
    let block = fixture
        .produce(txs(vec![tx(
            ALICE,
            2,
            1_000,
            vec![create(2, b"r", vec![("x", attr(Value::U64(1)))], None)],
        )]))
        .clone();
    let first = fixture.blocks[0].clone();
    let replay_second = |second: Block| replay(&genesis, &[first.clone(), second]).err();
    let invalid = |fault| {
        Some(ReplayError::Block {
            height: 2,
            error: ChainError::Exec(ExecError::InvalidBlock(fault)),
        })
    };

    // A `$` cell in a user operation.
    let mut tampered = block.clone();
    tampered.inputs.txs[0].ops[0] = create(2, b"r", vec![("$x", attr(Value::U64(1)))], None);
    assert_eq!(
        replay_second(tampered),
        invalid(BlockFault::Tx {
            index: 0,
            fault: TxFault::HostOwnedCell { op: 0 },
        })
    );
    // A budget above the user budget, and a zero budget.
    for budget in [100_001, 0] {
        let mut tampered = block.clone();
        tampered.inputs.txs[0].tx_budget = budget;
        assert_eq!(
            replay_second(tampered),
            invalid(BlockFault::Tx {
                index: 0,
                fault: TxFault::Budget,
            })
        );
    }
    // An admin input from a non-admin.
    let mut tampered = block.clone();
    let mut input = set_pricing(7, PriceSchedule::FIXTURE.with_id(9));
    input.actor = ALICE;
    tampered.inputs.admin.push(input);
    assert_eq!(
        replay_second(tampered),
        invalid(BlockFault::Admin {
            index: 0,
            fault: arkiv_node::AdminFault::NotAdmin,
        })
    );
    // Budgets summing past the user budget.
    let mut tampered = block.clone();
    tampered.inputs.txs = vec![
        tx(ALICE, 3, 60_000, vec![create_namespace("b")]),
        tx(ALICE, 4, 60_000, vec![create_namespace("c")]),
    ];
    assert_eq!(
        replay_second(tampered),
        invalid(BlockFault::Selection(arkiv_node::SelectionFault::Budget))
    );
    // A wrong parent hash.
    let mut tampered = block.clone();
    tampered.header.parent_hash = Default::default();
    assert_eq!(
        replay_second(tampered),
        Some(ReplayError::Block {
            height: 2,
            error: ChainError::InvalidBlock(BlockFault::Successor(SuccessorFault::ParentHash)),
        })
    );
    // A timestamp before the parent's.
    let mut tampered = block.clone();
    tampered.header.timestamp_ms = first.header.timestamp_ms - 1;
    assert_eq!(
        replay_second(tampered),
        Some(ReplayError::Block {
            height: 2,
            error: ChainError::InvalidBlock(BlockFault::Successor(SuccessorFault::Timestamp)),
        })
    );
    // Outcomes that re-execution does not reproduce.
    let mut tampered = block.clone();
    tampered.outcomes.spent += 1;
    assert_eq!(
        replay_second(tampered),
        Some(ReplayError::Block {
            height: 2,
            error: ChainError::OutcomesDiffer,
        })
    );
    // A header digest that does not match the inputs.
    let mut tampered = block.clone();
    tampered.header.inputs_digest = Default::default();
    let recorded = tampered.hash();
    assert_eq!(
        replay_second(tampered),
        Some(ReplayError::Block {
            height: 2,
            error: ChainError::HeaderDiffers {
                produced: block.hash(),
                recorded,
            },
        })
    );
    // The untampered chain replays.
    fixture.finish(&[GOLDEN_INVALID_1, GOLDEN_INVALID_2]);
}

const GOLDEN_INVALID_1: &str = "bf07ce06d4119777a58bf75f67817c56bee64a085696e8d0a2221433fb9eea5a";
const GOLDEN_INVALID_2: &str = "9674d1d5e1bbb9f5e71d4f7368f14351ff64b0ef7f0f3f3563ee78cfd1c7ab7d";

/// A statically valid operation refused at engine admission is a broken
/// invariant, never an outcome: here the (unchecked) genesis reserves less
/// block budget than a transaction may declare, so the operation meets
/// `ExceedsBlockBudget`. Nothing is published.
#[test]
fn admission_rejection_is_an_invariant_error() {
    let params = ChainParams {
        block_budget: 100,
        user_budget: 1_000,
        ..PARAMS
    };
    let spec = ChainSpec::new_unchecked(genesis_with(params, LIMITS, PriceSchedule::FIXTURE));
    assert!(ChainSpec::new(spec.genesis().clone()).is_err());
    let mut chain = MemoryChain::new(spec);
    let result = chain.produce(
        2_000,
        txs(vec![tx(
            ALICE,
            1,
            1_000,
            vec![create_namespace("entities"), create_namespace("alpha")],
        )]),
    );
    assert_eq!(
        result.err(),
        Some(ChainError::Exec(ExecError::Invariant(
            Invariant::AdmissionRejected {
                tx: 0,
                op: 0,
                reason: Rejection::ExceedsBlockBudget {
                    remaining: Cost::new(100),
                },
            }
        )))
    );
    assert_eq!(chain.height(), 0);
}

/// An expiry sweep that cannot finish aborts the candidate: here the
/// (unchecked) block budget of 300 covers three 78-unit deletions but not
/// four. The block is not published and the records stay live.
#[test]
fn halted_expiry_aborts_the_candidate() {
    let params = ChainParams {
        block_budget: 300,
        user_budget: 300,
        ..PARAMS
    };
    let spec = ChainSpec::new_unchecked(genesis_with(params, LIMITS, PriceSchedule::FIXTURE));
    let mut chain = MemoryChain::new(spec);
    let tag = || vec![("tag", attr(Value::Bool(true)))];
    // Two namespace creations, 2 × 53; then e1 with its four new terms 244,
    // and 124 per later record (every term already held).
    chain
        .produce(
            2_000,
            txs(vec![tx(
                ALICE,
                1,
                106,
                vec![create_namespace("entities"), create_namespace("alpha")],
            )]),
        )
        .expect("block 1");
    for (n, key_bytes, budget) in [
        (2, b"e1", 244),
        (3, b"e2", 124),
        (4, b"e3", 124),
        (5, b"e4", 124),
    ] {
        chain
            .produce(
                2_000 + n,
                txs(vec![tx(
                    ALICE,
                    n,
                    budget,
                    vec![create(2, key_bytes, tag(), Some(until(6)))],
                )]),
            )
            .expect("record block");
    }
    assert_eq!(chain.height(), 5);
    assert_eq!(
        chain.produce(3_000, BlockInputs::default()).err(),
        Some(ChainError::Exec(ExecError::ExpiryHalted {
            height: 6,
            namespace: ns(2),
            deleted: 3,
            reason: Rejection::OutOfBudget,
        }))
    );
    assert_eq!(chain.height(), 5);
    assert!(record(&chain.snapshot(5).expect("head"), 2, b"e1").is_some());
}

/// A header encodes in 156 bytes: the format version first, the state root
/// last.
#[test]
fn headers_carry_the_format_version_and_the_state_root() {
    let header = genesis().header();
    let bytes = encode(&header);
    assert_eq!(bytes.len(), 156);
    assert_eq!(bytes[..4], [0, 0, 0, 3]);
    assert_eq!(bytes[124..], header.state_root[..]);
}
