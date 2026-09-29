//! Block lifecycle and publication: ordered candidates over retained
//! snapshots, atomic publication, stale and foreign candidates, and the
//! host's block-to-snapshot mapping.
//!
//! Every expected cost below is a literal computed by hand from the FIXTURE
//! weights (DirectoryLookup 1, NamespaceCreate 50, NamespaceTransfer 20,
//! PutRecord 10, DeleteRecord 5, WriteKeyByte 2, WriteValueByte 1,
//! PointRead 3, ScanSeek 4, ScanEntry 2, ReadByte 1, RootUpdate 2) and the
//! model-version-3 charge points:
//!
//! | operation  | units                                      |
//! | ---------- | ------------------------------------------ |
//! | create     | 1 + 50 + RootUpdate 1*2 = 53               |
//! | transfer   | 1 + 20 + RootUpdate 1*2 = 23               |
//! | put        | 1 + 10 + 2 * key_len + value_len + 2*2     |
//! | delete     | 1 + 5 + 2 * key_len + 2*2                  |
//! | get        | 1 + 3 + value_len (present), 4 (missing)   |
//! | scan       | 1 + 4 + sum over entries (2 + key + value) |
//! | descriptor | 1                                          |
//!
//! A rejection after the directory lookup (unknown namespace, unauthorized,
//! out of budget on the first group) has spent exactly 1.

use arkiv_db::{
    BlockMap, BlockNumber, Branch, Budget, Cost, Effect, Engine, EngineError, Entry, Executed,
    HostError, NamespaceDescriptor, NamespaceId, OperationClass, Outcome, OwnerId, PriceSchedule,
    ReadResult, Receipt, ReceiptKind, Rejection, Revision, Snapshot, SnapshotId, WriteOp,
    WriteResult,
};

const ALICE: OwnerId = OwnerId::new([0xa1; 20]);
const BOB: OwnerId = OwnerId::new([0xb0; 20]);
const CAROL: OwnerId = OwnerId::new([0xc0; 20]);
const DAVE: OwnerId = OwnerId::new([0xd0; 20]);
const ERIN: OwnerId = OwnerId::new([0xe0; 20]);

/// Unlimited, untraced budget: the cost is asserted through the receipt.
const ALL: Budget = Budget::unlimited();

// --- identities -------------------------------------------------------------

/// The namespace with allocation ordinal `ordinal`, built through the public
/// byte encoding so expected IDs never come from the engine.
fn ns(ordinal: u64) -> NamespaceId {
    NamespaceId::from_bytes(ordinal.to_be_bytes()).expect("ordinals start at 1")
}

fn descriptor(id: NamespaceId, owner: OwnerId, revision: u64) -> NamespaceDescriptor {
    NamespaceDescriptor {
        name: arkiv_db::NamespaceName::new("test").unwrap(),
        id,
        owner,
        revision: Revision(revision),
    }
}

// --- operations -------------------------------------------------------------

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

// --- receipts ---------------------------------------------------------------

fn receipt(kind: ReceiptKind, schedule_id: u32, spent: u64, outcome: Outcome) -> Receipt {
    Receipt {
        kind,
        model_version: 3,
        schedule_id,
        spent: Cost::new(spent),
        outcome,
        ledger: None,
    }
}

/// An applied write receipt under the FIXTURE schedule (id 1).
fn applied(spent: u64) -> Receipt {
    receipt(ReceiptKind::Write, 1, spent, Outcome::Applied)
}

/// A rejected write receipt under the FIXTURE schedule (id 1).
fn rejected(spent: u64, reason: Rejection) -> Receipt {
    receipt(ReceiptKind::Write, 1, spent, Outcome::Rejected(reason))
}

/// An applied read receipt under the FIXTURE schedule (id 1).
fn read(spent: u64) -> Receipt {
    receipt(ReceiptKind::Read, 1, spent, Outcome::Applied)
}

/// A rejected read receipt under the FIXTURE schedule (id 1).
fn read_rejected(spent: u64, reason: Rejection) -> Receipt {
    receipt(ReceiptKind::Read, 1, spent, Outcome::Rejected(reason))
}

// --- results ----------------------------------------------------------------

/// The canonical part of a read: the value (or rejection) and the receipt,
/// with the non-canonical physical trace dropped.
type Logical<T> = Result<(T, Receipt), (Rejection, Receipt)>;

fn logical<T>(result: ReadResult<T>) -> Logical<T> {
    match result {
        Ok(metered) => Ok((metered.value, metered.receipt)),
        Err(rejected) => Err((rejected.reason, rejected.receipt)),
    }
}

fn value<T>(result: ReadResult<T>) -> T {
    result
        .unwrap_or_else(|rejected| panic!("read rejected: {rejected}"))
        .value
}

fn rejection(result: WriteResult) -> (Rejection, Receipt) {
    match result {
        Ok(executed) => panic!("expected a rejection, got {executed:?}"),
        Err(rejected) => (rejected.reason, rejected.receipt),
    }
}

fn some(bytes: &[u8]) -> Option<Vec<u8>> {
    Some(bytes.to_vec())
}

fn entry(key: &[u8], value: &[u8]) -> Entry {
    Entry {
        key: key.to_vec(),
        value: value.to_vec(),
    }
}

// --- driving the engine -----------------------------------------------------

fn branch(engine: &Engine, block_budget: Option<u64>) -> Branch {
    engine
        .begin(engine.head(), block_budget.map(Cost::new))
        .expect("the head is current")
}

fn publish(engine: &mut Engine, candidate: Branch) -> SnapshotId {
    let sealed = candidate.seal();
    engine
        .publish(&sealed)
        .expect("the candidate is built on the head")
}

/// Executes `op` with an unlimited budget and requires it to apply.
fn execute(candidate: &mut Branch, actor: OwnerId, op: WriteOp) -> Executed {
    let description = format!("{op:?}");
    candidate
        .execute(actor, op, ALL)
        .unwrap_or_else(|rejected| panic!("{description} rejected: {rejected}"))
}

fn create(candidate: &mut Branch, actor: OwnerId) -> NamespaceId {
    match execute(
        candidate,
        actor,
        WriteOp::CreateNamespace {
            name: arkiv_db::NamespaceName::new("test").unwrap(),
        },
    )
    .effect
    {
        Effect::NamespaceCreated(id) => id,
        other => panic!("create namespace produced {other:?}"),
    }
}

// --- reading a snapshot or a branch through one interface -------------------

trait Reader {
    fn read_get(
        &self,
        namespace: NamespaceId,
        key: &[u8],
        budget: Budget,
    ) -> ReadResult<Option<Vec<u8>>>;
    fn read_scan(
        &self,
        namespace: NamespaceId,
        prefix: &[u8],
        budget: Budget,
    ) -> ReadResult<Vec<Entry>>;
    fn read_descriptor(
        &self,
        namespace: NamespaceId,
        budget: Budget,
    ) -> ReadResult<NamespaceDescriptor>;
    fn namespaces(&self) -> usize;
}

macro_rules! impl_reader {
    ($($reader:ty),*) => {$(
        impl Reader for $reader {
            fn read_get(
                &self,
                namespace: NamespaceId,
                key: &[u8],
                budget: Budget,
            ) -> ReadResult<Option<Vec<u8>>> {
                self.get(namespace, key, budget)
            }

            fn read_scan(
                &self,
                namespace: NamespaceId,
                prefix: &[u8],
                budget: Budget,
            ) -> ReadResult<Vec<Entry>> {
                self.scan_prefix(namespace, prefix, budget)
            }

            fn read_descriptor(
                &self,
                namespace: NamespaceId,
                budget: Budget,
            ) -> ReadResult<NamespaceDescriptor> {
                self.descriptor(namespace, budget)
            }

            fn namespaces(&self) -> usize {
                self.namespace_count()
            }
        }
    )*};
}

impl_reader!(Snapshot, Branch);

/// Everything a reader reports about one namespace, receipts included
/// (traced, so ledgers take part in the comparison).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Observation {
    namespaces: usize,
    descriptor: Logical<NamespaceDescriptor>,
    gets: Vec<Logical<Option<Vec<u8>>>>,
    scan: Logical<Vec<Entry>>,
}

fn observe(reader: &impl Reader, namespace: NamespaceId, keys: &[&[u8]]) -> Observation {
    let traced = Budget::unlimited().traced();
    Observation {
        namespaces: reader.namespaces(),
        descriptor: logical(reader.read_descriptor(namespace, traced)),
        gets: keys
            .iter()
            .map(|key| logical(reader.read_get(namespace, key, traced)))
            .collect(),
        scan: logical(reader.read_scan(namespace, b"", traced)),
    }
}

// --- 1. the six-step deliverable flow ---------------------------------------

#[test]
fn six_step_flow_publishes_ordered_blocks_over_two_namespaces() {
    let mut engine = Engine::new(PriceSchedule::FIXTURE);
    let mut blocks = BlockMap::new();
    blocks.record(BlockNumber(0), engine.head()).unwrap();
    assert_eq!(engine.head(), SnapshotId::GENESIS);

    // Step 1, block 1: one namespace per owner. create = 1 + 50 + 2 = 53 each.
    let mut candidate = branch(&engine, Some(1_000));
    let shop = execute(
        &mut candidate,
        ALICE,
        WriteOp::CreateNamespace {
            name: arkiv_db::NamespaceName::new("test").unwrap(),
        },
    );
    let blog = execute(
        &mut candidate,
        BOB,
        WriteOp::CreateNamespace {
            name: arkiv_db::NamespaceName::new("test").unwrap(),
        },
    );
    assert_eq!(
        (shop.effect, shop.receipt),
        (Effect::NamespaceCreated(ns(1)), applied(53))
    );
    assert_eq!(
        (blog.effect, blog.receipt),
        (Effect::NamespaceCreated(ns(2)), applied(53))
    );
    let (shop, blog) = (ns(1), ns(2));
    let sealed = candidate.seal();
    assert_eq!(sealed.spent(), Cost::new(106), "53 + 53");
    assert_eq!(sealed.receipts(), &[applied(53), applied(53)]);
    let snapshot_1 = engine.publish(&sealed).unwrap();
    assert_eq!(snapshot_1.sequence(), 1);
    blocks.record(BlockNumber(1), snapshot_1).unwrap();
    let after_block_1 = engine.head_snapshot();
    assert_eq!(after_block_1.id(), snapshot_1);
    assert_eq!(after_block_1.namespace_count(), 2);
    assert_eq!(
        value(after_block_1.descriptor(shop, ALL)),
        descriptor(shop, ALICE, 0)
    );
    assert_eq!(
        value(after_block_1.descriptor(blog, ALL)),
        descriptor(blog, BOB, 0)
    );
    assert_eq!(after_block_1.record_count(shop), Some(0));

    // Step 2, block 2: ordered operations, including put/delete/put on one
    // key inside the block. Only the last write survives; head never shows
    // an intermediate value while the candidate is open.
    let mut candidate = branch(&engine, Some(1_000));
    /// Actor, operation, expected effect, expected cost, expected value of
    /// `k` in the branch afterwards.
    type Step<'a> = (OwnerId, WriteOp, Effect, u64, Option<&'a [u8]>);
    let steps: [Step<'_>; 6] = [
        // put k=v1: 1 + 10 + 2*1 + 2 + 4 = 19
        (
            ALICE,
            put(shop, b"k", b"v1"),
            Effect::Put { replaced: false },
            19,
            Some(b"v1"),
        ),
        // delete k: 1 + 5 + 2*1 + 4 = 12
        (
            ALICE,
            delete(shop, b"k"),
            Effect::Deleted { existed: true },
            12,
            None,
        ),
        // put k=final: 1 + 10 + 2*1 + 5 + 4 = 22
        (
            ALICE,
            put(shop, b"k", b"final"),
            Effect::Put { replaced: false },
            22,
            Some(b"final"),
        ),
        // put other=x: 1 + 10 + 2*5 + 1 + 4 = 26
        (
            ALICE,
            put(shop, b"other", b"x"),
            Effect::Put { replaced: false },
            26,
            Some(b"final"),
        ),
        // put other=y (replacement, same size): 26 again
        (
            ALICE,
            put(shop, b"other", b"y"),
            Effect::Put { replaced: true },
            26,
            Some(b"final"),
        ),
        // bob's put post=hello: 1 + 10 + 2*4 + 5 + 4 = 28
        (
            BOB,
            put(blog, b"post", b"hello"),
            Effect::Put { replaced: false },
            28,
            Some(b"final"),
        ),
    ];
    for (index, (actor, op, effect, cost, k_in_branch)) in steps.into_iter().enumerate() {
        let executed = execute(&mut candidate, actor, op);
        assert_eq!(executed.effect, effect, "step {index}");
        assert_eq!(executed.receipt, applied(cost), "step {index}");
        assert_eq!(
            value(candidate.get(shop, b"k", ALL)),
            k_in_branch.map(<[u8]>::to_vec),
            "branch view of shop/k after step {index}"
        );
        assert_eq!(
            value(after_block_1.get(shop, b"k", ALL)),
            None,
            "retained block-1 handle after step {index}"
        );
        assert_eq!(
            value(engine.head_snapshot().get(shop, b"k", ALL)),
            None,
            "head after step {index}"
        );
        assert_eq!(engine.head(), snapshot_1, "head id after step {index}");
    }
    let sealed = candidate.seal();
    assert_eq!(
        sealed.receipts(),
        &[
            applied(19),
            applied(12),
            applied(22),
            applied(26),
            applied(26),
            applied(28)
        ]
    );
    assert_eq!(
        sealed.spent(),
        Cost::new(133),
        "19 + 12 + 22 + 26 + 26 + 28"
    );
    let snapshot_2 = engine.publish(&sealed).unwrap();
    assert_eq!(snapshot_2.sequence(), 2);
    blocks.record(BlockNumber(2), snapshot_2).unwrap();
    let after_block_2 = engine.head_snapshot();
    assert_eq!(after_block_2.id(), snapshot_2);
    assert_eq!(after_block_2.base(), Some(snapshot_1));
    assert_eq!(value(after_block_2.get(shop, b"k", ALL)), some(b"final"));
    assert_eq!(value(after_block_2.get(shop, b"other", ALL)), some(b"y"));
    assert_eq!(
        value(after_block_2.scan_prefix(shop, b"", ALL)),
        vec![entry(b"k", b"final"), entry(b"other", b"y")],
        "the published snapshot shows only the last write per key"
    );
    assert_eq!(after_block_2.record_count(shop), Some(2));
    assert_eq!(value(after_block_2.get(blog, b"post", ALL)), some(b"hello"));
    assert_eq!(after_block_2.record_count(blog), Some(1));
    let shop_keys: &[&[u8]] = &[b"k", b"other", b"k3"];
    let shop_at_block_2 = observe(&after_block_2, shop, shop_keys);

    // Step 3, block 3: transfer the shop to bob without moving any record,
    // then reject an unauthorized put, an underfunded put, and a put whose
    // budget exceeds the block's remainder. Block budget 32.
    let mut candidate = branch(&engine, Some(32));
    let transferred = execute(&mut candidate, ALICE, transfer(shop, 0, BOB));
    assert_eq!(
        transferred.effect,
        Effect::Transferred {
            revision: Revision(1)
        }
    );
    assert_eq!(transferred.receipt, applied(23), "1 + 20 + 2");
    assert_eq!(
        transferred.physical.record_nodes, 0,
        "a transfer visits no record node"
    );
    assert_eq!(candidate.remaining(), Cost::new(9), "32 - 23");
    assert_eq!(
        value(candidate.descriptor(shop, ALL)),
        descriptor(shop, BOB, 1)
    );
    let shop_after_transfer = observe(&candidate, shop, shop_keys);

    // Alice no longer owns the shop: rejected after the directory lookup.
    let (reason, receipt) =
        rejection(candidate.execute(ALICE, put(shop, b"k3", b"unauthorized"), ALL));
    assert_eq!(reason, Rejection::Unauthorized);
    assert_eq!(receipt, rejected(1, Rejection::Unauthorized));
    assert_eq!(candidate.remaining(), Cost::new(8), "32 - 23 - 1");

    // Bob's put costs 1 + 10 + 2*2 + 4 + 4 = 23; with a 5-unit budget the
    // lookup is charged and the write group (22) is refused as a whole.
    let (reason, receipt) =
        rejection(candidate.execute(BOB, put(shop, b"k3", b"over"), Budget::limit(5)));
    assert_eq!(reason, Rejection::OutOfBudget);
    assert_eq!(receipt, rejected(1, Rejection::OutOfBudget));
    assert_eq!(candidate.remaining(), Cost::new(7), "32 - 23 - 1 - 1");

    // A budget above the block's remainder is refused at admission: no charge
    // and no receipt.
    let (reason, receipt) =
        rejection(candidate.execute(BOB, put(shop, b"k3", b"x"), Budget::limit(8)));
    assert_eq!(
        reason,
        Rejection::ExceedsBlockBudget {
            remaining: Cost::new(7)
        }
    );
    assert_eq!(
        receipt,
        rejected(
            0,
            Rejection::ExceedsBlockBudget {
                remaining: Cost::new(7)
            }
        )
    );
    assert_eq!(candidate.remaining(), Cost::new(7));
    assert_eq!(candidate.spent(), Cost::new(25), "23 + 1 + 1");
    assert_eq!(candidate.receipts().len(), 3);
    assert_eq!(
        observe(&candidate, shop, shop_keys),
        shop_after_transfer,
        "rejected operations change nothing"
    );

    let sealed = candidate.seal();
    assert_eq!(
        sealed.receipts(),
        &[
            applied(23),
            rejected(1, Rejection::Unauthorized),
            rejected(1, Rejection::OutOfBudget)
        ]
    );
    assert_eq!(sealed.spent(), Cost::new(25));
    let snapshot_3 = engine.publish(&sealed).unwrap();
    assert_eq!(snapshot_3.sequence(), 3);
    blocks.record(BlockNumber(3), snapshot_3).unwrap();
    let after_block_3 = engine.head_snapshot();
    assert_eq!(
        value(after_block_3.descriptor(shop, ALL)),
        descriptor(shop, BOB, 1)
    );
    assert_eq!(after_block_3.record_count(shop), Some(2));
    assert!(
        after_block_2.records_shared_with(&after_block_3, shop),
        "the transfer left the shop's record map physically untouched"
    );
    assert!(after_block_2.records_shared_with(&after_block_3, blog));
    assert_eq!(value(after_block_3.get(shop, b"k3", ALL)), None);

    // Step 5: the state after block 2 is unchanged however it is reached.
    let by_id = engine.snapshot(snapshot_2).unwrap();
    let by_block = blocks.at(&engine, BlockNumber(2)).unwrap();
    for (label, snapshot) in [
        ("retained handle", &after_block_2),
        ("engine.snapshot", &by_id),
        ("blocks.at", &by_block),
    ] {
        assert_eq!(snapshot.id(), snapshot_2, "{label}");
        assert_eq!(
            value(snapshot.descriptor(shop, ALL)),
            descriptor(shop, ALICE, 0),
            "{label}: alice still owns the shop at block 2"
        );
        assert_eq!(
            observe(snapshot, shop, shop_keys),
            shop_at_block_2,
            "{label}"
        );
    }
    assert_eq!(
        value(engine.head_snapshot().descriptor(shop, ALL)),
        descriptor(shop, BOB, 1)
    );

    // Step 6, block 4: one block touches both namespaces; readers see all or
    // nothing of it.
    let before = engine.head_snapshot();
    let mut candidate = branch(&engine, Some(1_000));
    let deleted = execute(&mut candidate, BOB, delete(shop, b"k"));
    assert_eq!(
        (deleted.effect, deleted.receipt),
        (Effect::Deleted { existed: true }, applied(12)),
        "1 + 5 + 2*1 + 4"
    );
    let posted = execute(&mut candidate, BOB, put(blog, b"post2", b"world"));
    assert_eq!(
        (posted.effect, posted.receipt),
        (Effect::Put { replaced: false }, applied(30)),
        "1 + 10 + 2*5 + 5 + 4"
    );
    let sealed = candidate.seal();
    assert_eq!(sealed.spent(), Cost::new(42), "12 + 30");
    let staged = engine.head_snapshot();
    assert_eq!(value(staged.get(shop, b"k", ALL)), some(b"final"));
    assert_eq!(value(staged.get(blog, b"post2", ALL)), None);
    let snapshot_4 = engine.publish(&sealed).unwrap();
    assert_eq!(snapshot_4.sequence(), 4);
    blocks.record(BlockNumber(4), snapshot_4).unwrap();
    let after = engine.head_snapshot();
    assert_eq!(after.id(), snapshot_4);
    assert_eq!(after.base(), Some(snapshot_3));
    assert_eq!(
        (
            value(before.get(shop, b"k", ALL)),
            value(before.get(blog, b"post2", ALL))
        ),
        (some(b"final"), None)
    );
    assert_eq!(
        (
            value(after.get(shop, b"k", ALL)),
            value(after.get(blog, b"post2", ALL))
        ),
        (None, some(b"world"))
    );
    assert_eq!(
        value(before.scan_prefix(shop, b"", ALL)),
        vec![entry(b"k", b"final"), entry(b"other", b"y")]
    );
    assert_eq!(
        value(after.scan_prefix(shop, b"", ALL)),
        vec![entry(b"other", b"y")]
    );
    assert_eq!(
        value(after.scan_prefix(blog, b"", ALL)),
        vec![entry(b"post", b"hello"), entry(b"post2", b"world")]
    );
    assert_eq!(
        (before.record_count(shop), after.record_count(shop)),
        (Some(2), Some(1))
    );
    assert!(!before.records_shared_with(&after, shop));
    assert!(!before.records_shared_with(&after, blog));

    for block in 0..=4 {
        assert_eq!(
            blocks.resolve(BlockNumber(block)).map(SnapshotId::sequence),
            Some(block),
            "one publication per block, so block {block} is snapshot {block}"
        );
    }
    assert_eq!(blocks.latest(), Some((BlockNumber(4), snapshot_4)));
    assert_eq!(blocks.len(), 5);
    assert_eq!(engine.snapshot_count(), 5);
}

// --- 2. empty blocks --------------------------------------------------------

#[test]
fn empty_blocks_publish_new_snapshots_with_identical_answers() {
    let mut engine = Engine::new(PriceSchedule::FIXTURE);
    let mut setup = branch(&engine, None);
    let store = create(&mut setup, ALICE);
    execute(&mut setup, ALICE, put(store, b"a", b"1"));
    execute(&mut setup, ALICE, put(store, b"bb", b"22"));
    let filled = publish(&mut engine, setup);
    let before = engine.head_snapshot();
    let keys: &[&[u8]] = &[b"a", b"bb", b"zz"];
    let baseline = observe(&before, store, keys);
    // Literal expectations for the baseline, so the equalities below are not
    // vacuous.
    assert_eq!(
        logical(before.get(store, b"a", ALL)),
        Ok((some(b"1"), read(5))),
        "1 + 3 + 1"
    );
    assert_eq!(
        logical(before.get(store, b"bb", ALL)),
        Ok((some(b"22"), read(6))),
        "1 + 3 + 2"
    );
    assert_eq!(
        logical(before.get(store, b"zz", ALL)),
        Ok((None, read(4))),
        "1 + 3, nothing to copy"
    );
    assert_eq!(
        logical(before.scan_prefix(store, b"", ALL)),
        Ok((vec![entry(b"a", b"1"), entry(b"bb", b"22")], read(15))),
        "1 + 4 + (2 + 1 + 1) + (2 + 2 + 2)"
    );
    assert_eq!(
        logical(before.descriptor(store, ALL)),
        Ok((descriptor(store, ALICE, 0), read(1)))
    );

    // A candidate with no operations, under a zero block budget, still
    // publishes a fresh snapshot.
    let empty = branch(&engine, Some(0)).seal();
    assert!(empty.receipts().is_empty());
    assert_eq!(empty.spent(), Cost::ZERO);
    assert_eq!(empty.base(), filled);
    let first_empty = engine.publish(&empty).unwrap();
    assert_eq!(first_empty.sequence(), filled.sequence() + 1);
    assert_eq!(engine.head(), first_empty);
    let after = engine.snapshot(first_empty).unwrap();
    assert_eq!(after.id(), first_empty);
    assert_eq!(after.base(), Some(filled));
    assert_eq!(observe(&after, store, keys), baseline);
    assert_eq!(after.namespace_count(), before.namespace_count());
    assert_eq!(after.record_count(store), before.record_count(store));
    assert_eq!(after.record_count(store), Some(2));
    assert!(after.records_shared_with(&before, store));
    assert_eq!(after.pricing(), before.pricing());

    // Several consecutive empty blocks.
    let mut previous = first_empty;
    for round in 1..=5 {
        let empty = branch(&engine, None);
        let id = publish(&mut engine, empty);
        assert_eq!(id.sequence(), previous.sequence() + 1, "round {round}");
        let snapshot = engine.snapshot(id).unwrap();
        assert_eq!(snapshot.base(), Some(previous), "round {round}");
        assert_eq!(observe(&snapshot, store, keys), baseline, "round {round}");
        assert_eq!(snapshot.record_count(store), Some(2), "round {round}");
        assert!(
            snapshot.records_shared_with(&before, store),
            "round {round}"
        );
        previous = id;
    }
    assert_eq!(engine.head(), previous);
    assert_eq!(
        engine.snapshot_count(),
        8,
        "genesis, one filled and six empty"
    );
    assert_eq!(
        observe(&before, store, keys),
        baseline,
        "the old handle is unchanged"
    );
}

// --- 3. atomic mixed-namespace publication ----------------------------------

struct Quartet {
    a: NamespaceId,
    b: NamespaceId,
    c: NamespaceId,
    d: NamespaceId,
}

/// The four facts a mixed block changes, as one reader reports them.
#[derive(Clone, Debug, PartialEq, Eq)]
struct MixedView {
    a_k: Option<Vec<u8>>,
    b_k: Option<Vec<u8>>,
    c: Logical<NamespaceDescriptor>,
    d: Logical<NamespaceDescriptor>,
    namespaces: usize,
}

fn mixed_view(reader: &impl Reader, q: &Quartet) -> MixedView {
    MixedView {
        a_k: value(reader.read_get(q.a, b"k", ALL)),
        b_k: value(reader.read_get(q.b, b"k", ALL)),
        c: logical(reader.read_descriptor(q.c, ALL)),
        d: logical(reader.read_descriptor(q.d, ALL)),
        namespaces: reader.namespaces(),
    }
}

fn before_mixed_block(q: &Quartet) -> MixedView {
    MixedView {
        a_k: some(b"a0"),
        b_k: some(b"b0"),
        c: Ok((descriptor(q.c, CAROL, 0), read(1))),
        d: Err((
            Rejection::UnknownNamespace,
            read_rejected(1, Rejection::UnknownNamespace),
        )),
        namespaces: 3,
    }
}

fn after_mixed_block(q: &Quartet) -> MixedView {
    MixedView {
        a_k: some(b"a1"),
        b_k: None,
        c: Ok((descriptor(q.c, DAVE, 1), read(1))),
        d: Ok((descriptor(q.d, ERIN, 0), read(1))),
        namespaces: 4,
    }
}

#[test]
fn a_block_touching_four_namespaces_becomes_visible_all_at_once() {
    let mut engine = Engine::new(PriceSchedule::FIXTURE);
    let mut setup = branch(&engine, None);
    let q = Quartet {
        a: create(&mut setup, ALICE),
        b: create(&mut setup, BOB),
        c: create(&mut setup, CAROL),
        d: ns(4),
    };
    assert_eq!((q.a, q.b, q.c), (ns(1), ns(2), ns(3)));
    execute(&mut setup, ALICE, put(q.a, b"k", b"a0"));
    execute(&mut setup, BOB, put(q.b, b"k", b"b0"));
    let base = publish(&mut engine, setup);
    let h = engine.head_snapshot();
    assert_eq!(h.id(), base);
    let before = before_mixed_block(&q);
    let after = after_mixed_block(&q);
    assert_eq!(mixed_view(&h, &q), before);

    let mut candidate = branch(&engine, Some(1_000));
    let steps: [(OwnerId, WriteOp, Effect, u64); 4] = [
        // put A/k=a1: 1 + 10 + 2*1 + 2 + 4 = 19
        (
            ALICE,
            put(q.a, b"k", b"a1"),
            Effect::Put { replaced: true },
            19,
        ),
        // delete B/k: 1 + 5 + 2*1 + 4 = 12
        (
            BOB,
            delete(q.b, b"k"),
            Effect::Deleted { existed: true },
            12,
        ),
        // transfer C: 1 + 20 + 2 = 23
        (
            CAROL,
            transfer(q.c, 0, DAVE),
            Effect::Transferred {
                revision: Revision(1),
            },
            23,
        ),
        // create D: 1 + 50 + 2 = 53
        (
            ERIN,
            WriteOp::CreateNamespace {
                name: arkiv_db::NamespaceName::new("test").unwrap(),
            },
            Effect::NamespaceCreated(q.d),
            53,
        ),
    ];
    for (index, (actor, op, effect, cost)) in steps.into_iter().enumerate() {
        let executed = execute(&mut candidate, actor, op);
        assert_eq!(executed.effect, effect, "step {index}");
        assert_eq!(executed.receipt, applied(cost), "step {index}");
        assert_eq!(
            mixed_view(&h, &q),
            before,
            "retained head handle after step {index}"
        );
        assert_eq!(
            mixed_view(&engine.head_snapshot(), &q),
            before,
            "fresh head handle after step {index}"
        );
        assert_eq!(engine.head(), base, "head id after step {index}");
        let expected_branch = MixedView {
            a_k: after.a_k.clone(),
            b_k: if index >= 1 {
                after.b_k.clone()
            } else {
                before.b_k.clone()
            },
            c: if index >= 2 {
                after.c.clone()
            } else {
                before.c.clone()
            },
            d: if index >= 3 {
                after.d.clone()
            } else {
                before.d.clone()
            },
            namespaces: if index >= 3 { 4 } else { 3 },
        };
        assert_eq!(
            mixed_view(&candidate, &q),
            expected_branch,
            "branch view after step {index}"
        );
    }
    assert_eq!(mixed_view(&candidate, &q), after);

    let sealed = candidate.seal();
    assert_eq!(
        sealed.receipts(),
        &[applied(19), applied(12), applied(23), applied(53)]
    );
    assert_eq!(sealed.spent(), Cost::new(107), "19 + 12 + 23 + 53");
    assert_eq!(mixed_view(&h, &q), before, "retained handle after seal");
    assert_eq!(
        mixed_view(&engine.head_snapshot(), &q),
        before,
        "fresh head after seal"
    );
    assert_eq!(engine.head(), base);

    let published_id = engine.publish(&sealed).unwrap();
    assert_eq!(published_id.sequence(), base.sequence() + 1);
    let published = engine.snapshot(published_id).unwrap();
    assert_eq!(published.base(), Some(base));
    assert_eq!(
        mixed_view(&published, &q),
        after,
        "all four changes together"
    );
    assert_eq!(mixed_view(&engine.head_snapshot(), &q), after);
    assert_eq!(
        mixed_view(&h, &q),
        before,
        "the old handle shows none of them"
    );
    assert_eq!(
        mixed_view(&engine.snapshot(base).unwrap(), &q),
        before,
        "the old snapshot shows none of them"
    );
    assert_eq!(
        (h.record_count(q.b), published.record_count(q.b)),
        (Some(1), Some(0))
    );
    assert_eq!(
        (h.record_count(q.d), published.record_count(q.d)),
        (None, Some(0))
    );
    // C holds no records, so shared-root identity is vacuous here; the
    // populated case is covered by the six-step flow's shop namespace.
    assert!(
        published.records_shared_with(&h, q.c),
        "C was only transferred"
    );
    assert!(!published.records_shared_with(&h, q.a));
    assert!(!published.records_shared_with(&h, q.b));
}

// --- 4. competing and stale candidates --------------------------------------

#[test]
fn a_stale_candidate_loses_and_releases_its_namespace_ordinal() {
    let mut engine = Engine::new(PriceSchedule::FIXTURE);
    let mut setup = branch(&engine, None);
    let a = create(&mut setup, ALICE);
    let c = create(&mut setup, CAROL);
    execute(&mut setup, ALICE, put(a, b"k", b"a0"));
    let h = publish(&mut engine, setup);

    let c1 = branch(&engine, None);
    let mut c2 = branch(&engine, None);
    assert_eq!((c1.base(), c2.base()), (h, h));
    // put A/k=a1: 1 + 10 + 2*1 + 2 + 4 = 19; transfer C: 23; create: 53.
    execute(&mut c2, ALICE, put(a, b"k", b"a1"));
    execute(&mut c2, CAROL, transfer(c, 0, DAVE));
    let n = create(&mut c2, ERIN);
    assert_eq!(n, ns(3));
    let branch_receipts = c2.receipts().to_vec();
    let branch_spent = c2.spent();
    assert_eq!(branch_receipts, vec![applied(19), applied(23), applied(53)]);
    assert_eq!(branch_spent, Cost::new(95), "19 + 23 + 53");
    assert_eq!(c2.namespace_count(), 3);
    let loser = c2.seal();
    let winner = c1.seal();

    let h1 = engine.publish(&winner).unwrap();
    assert_eq!(h1.sequence(), h.sequence() + 1);
    assert_eq!(
        engine.publish(&loser),
        Err(EngineError::StaleHead {
            expected: h,
            head: h1
        })
    );
    assert_eq!(engine.head(), h1);
    assert_eq!(engine.snapshot_count(), 3);
    let head = engine.head_snapshot();
    assert_eq!(value(head.get(a, b"k", ALL)), some(b"a0"));
    assert_eq!(value(head.descriptor(c, ALL)), descriptor(c, CAROL, 0));
    assert_eq!(head.namespace_count(), 2);
    assert_eq!(
        logical(head.descriptor(n, ALL)),
        Err((
            Rejection::UnknownNamespace,
            read_rejected(1, Rejection::UnknownNamespace)
        ))
    );

    // The rejected candidate keeps what it charged.
    assert_eq!(loser.base(), h);
    assert_eq!(loser.receipts(), branch_receipts.as_slice());
    assert_eq!(loser.spent(), branch_spent);

    // The loser never held ordinal 3: a fresh candidate on the new head
    // allocates it.
    let mut fresh = branch(&engine, None);
    assert_eq!(fresh.base(), h1);
    assert_eq!(create(&mut fresh, ERIN), n);
    assert_eq!(
        engine.begin(h, None).map(|_| ()),
        Err(EngineError::StaleHead {
            expected: h,
            head: h1
        })
    );
    let fresh = fresh.seal();
    let h2 = engine.publish(&fresh).unwrap();
    assert_eq!(h2.sequence(), h.sequence() + 2);
    assert_eq!(
        engine.publish(&fresh),
        Err(EngineError::StaleHead {
            expected: h1,
            head: h2
        }),
        "the same sealed candidate cannot publish twice"
    );
    assert_eq!(engine.head(), h2);
    assert_eq!(engine.snapshot_count(), 4);
    assert_eq!(engine.head_snapshot().namespace_count(), 3);
    assert_eq!(
        value(engine.head_snapshot().descriptor(n, ALL)),
        descriptor(n, ERIN, 0)
    );
    assert_eq!(
        engine.publish(&loser),
        Err(EngineError::StaleHead {
            expected: h,
            head: h2
        }),
        "the loser reports the current head each time"
    );
}

// --- 5. block identity vs snapshot identity ---------------------------------

#[test]
fn block_numbers_and_snapshot_ids_are_distinct_sequences() {
    let mut engine = Engine::new(PriceSchedule::FIXTURE);
    let mut blocks = BlockMap::new();
    assert!(blocks.is_empty());
    assert_eq!(blocks.next_block(), None);
    assert_eq!(blocks.latest(), None);

    // Block 1, stage 1: an admin candidate commits schedule 2, which reprices
    // PointRead from 3 to 6 and keeps every other FIXTURE weight.
    let repriced = PriceSchedule::FIXTURE
        .with_id(2)
        .with_price(OperationClass::PointRead, 6);
    let mut admin = branch(&engine, None);
    assert_eq!(admin.set_pricing(repriced.clone()), Ok(()));
    assert_eq!(
        admin.pricing().id(),
        1,
        "the admin branch stays pinned to its base schedule"
    );
    let admin_snapshot = publish(&mut engine, admin);
    assert_eq!(admin_snapshot.sequence(), 1);

    // Block 1, stage 2: a data candidate priced with schedule 2.
    let mut data = branch(&engine, None);
    assert_eq!(data.pricing(), &repriced);
    let store = create(&mut data, ALICE);
    assert_eq!(store, ns(1));
    let written = execute(&mut data, ALICE, put(store, b"k", b"v"));
    assert_eq!(
        written.receipt,
        receipt(ReceiptKind::Write, 2, 18, Outcome::Applied),
        "1 + 10 + 2*1 + 1 + 4 under schedule 2"
    );
    let sealed = data.seal();
    assert_eq!(
        sealed.receipts(),
        &[
            receipt(ReceiptKind::Write, 2, 53, Outcome::Applied),
            receipt(ReceiptKind::Write, 2, 18, Outcome::Applied)
        ]
    );
    let data_snapshot = engine.publish(&sealed).unwrap();
    assert_eq!(data_snapshot.sequence(), 2);

    // Only the block's terminal snapshot is recorded.
    blocks.record(BlockNumber(1), data_snapshot).unwrap();
    assert_eq!(blocks.resolve(BlockNumber(1)), Some(data_snapshot));
    assert_eq!(blocks.resolve(BlockNumber(2)), None);
    assert_eq!(blocks.resolve(BlockNumber(0)), None);
    assert_eq!(
        blocks
            .at(&engine, BlockNumber(2))
            .map(|snapshot| snapshot.id()),
        Err(HostError::UnknownBlock(BlockNumber(2)))
    );
    assert_eq!(
        blocks.at(&engine, BlockNumber(1)).unwrap().id(),
        data_snapshot
    );
    assert_eq!(blocks.latest(), Some((BlockNumber(1), data_snapshot)));
    assert_eq!(blocks.next_block(), Some(BlockNumber(2)));
    assert_eq!(blocks.len(), 1);

    // The intermediate snapshot is not a block, but it is still readable.
    let intermediate = engine.snapshot(admin_snapshot).unwrap();
    assert_eq!(intermediate.id(), admin_snapshot);
    assert_eq!(intermediate.base(), Some(SnapshotId::GENESIS));
    assert_eq!(intermediate.namespace_count(), 0);
    assert_eq!(intermediate.pricing(), &repriced);
    assert_eq!(
        logical(intermediate.descriptor(store, ALL)),
        Err((
            Rejection::UnknownNamespace,
            receipt(
                ReceiptKind::Read,
                2,
                1,
                Outcome::Rejected(Rejection::UnknownNamespace)
            )
        ))
    );
    assert_eq!(
        engine.snapshot(SnapshotId::GENESIS).unwrap().pricing(),
        &PriceSchedule::FIXTURE
    );
    let terminal = engine.snapshot(data_snapshot).unwrap();
    assert_eq!(terminal.base(), Some(admin_snapshot));
    assert_eq!(
        logical(terminal.get(store, b"k", ALL)),
        Ok((
            some(b"v"),
            receipt(ReceiptKind::Read, 2, 8, Outcome::Applied)
        )),
        "1 + 6 + 1 under schedule 2"
    );

    // Gaps, repeats and non-monotonic snapshots are refused.
    assert_eq!(
        blocks.record(BlockNumber(3), data_snapshot),
        Err(HostError::NonConsecutiveBlock {
            block: BlockNumber(3),
            expected: BlockNumber(2)
        })
    );
    assert_eq!(
        blocks.record(BlockNumber(1), data_snapshot),
        Err(HostError::NonConsecutiveBlock {
            block: BlockNumber(1),
            expected: BlockNumber(2)
        })
    );
    assert_eq!(
        blocks.record(BlockNumber(2), admin_snapshot),
        Err(HostError::NonMonotonicSnapshot {
            snapshot: admin_snapshot,
            previous: data_snapshot
        })
    );
    assert_eq!(
        blocks.record(BlockNumber(2), data_snapshot),
        Err(HostError::NonMonotonicSnapshot {
            snapshot: data_snapshot,
            previous: data_snapshot
        })
    );
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks.latest(), Some((BlockNumber(1), data_snapshot)));

    // Block 2 is empty and still gets a snapshot of its own.
    let empty = branch(&engine, None);
    let empty_snapshot = publish(&mut engine, empty);
    assert_eq!(empty_snapshot.sequence(), 3);
    blocks.record(BlockNumber(2), empty_snapshot).unwrap();
    assert_eq!(blocks.resolve(BlockNumber(2)), Some(empty_snapshot));
    assert_ne!(
        blocks.resolve(BlockNumber(2)),
        blocks.resolve(BlockNumber(1))
    );
    assert_eq!(blocks.latest(), Some((BlockNumber(2), empty_snapshot)));
    assert_eq!(blocks.len(), 2);
    assert_eq!(engine.snapshot_count(), 4);
    assert_eq!(
        value(
            blocks
                .at(&engine, BlockNumber(2))
                .unwrap()
                .get(store, b"k", ALL)
        ),
        some(b"v")
    );

    // Block numbers are not snapshot sequences.
    for block in [1, 2] {
        let snapshot = blocks.resolve(BlockNumber(block)).unwrap();
        assert_ne!(snapshot.sequence(), block, "block {block}");
        assert_eq!(snapshot.sequence(), block + 1, "block {block}");
    }
}

// --- 6. foreign candidates --------------------------------------------------

#[test]
fn candidates_sealed_by_another_engine_are_foreign_even_at_genesis() {
    let mut x = Engine::new(PriceSchedule::FIXTURE);
    let mut y = Engine::new(PriceSchedule::FIXTURE);
    assert_eq!(
        (x.head(), y.head()),
        (SnapshotId::GENESIS, SnapshotId::GENESIS)
    );

    let empty = x.begin(SnapshotId::GENESIS, None).unwrap().seal();
    let mut creating = x.begin(SnapshotId::GENESIS, None).unwrap();
    assert_eq!(create(&mut creating, ALICE), ns(1));
    let creating = creating.seal();

    assert_eq!(y.publish(&empty), Err(EngineError::ForeignCandidate));
    assert_eq!(y.publish(&creating), Err(EngineError::ForeignCandidate));
    assert_eq!(y.head(), SnapshotId::GENESIS);
    assert_eq!(y.snapshot_count(), 1);
    assert_eq!(y.head_snapshot().namespace_count(), 0);

    // X accepts its own candidate; it stays foreign to Y, and the instance
    // check comes before the stale-head check.
    assert_eq!(x.publish(&creating).map(SnapshotId::sequence), Ok(1));
    assert_eq!(y.publish(&creating), Err(EngineError::ForeignCandidate));
    let x_head = x.head();
    assert_eq!(
        x.publish(&empty),
        Err(EngineError::StaleHead {
            expected: SnapshotId::GENESIS,
            head: x_head
        })
    );

    // The relation is symmetric, even with both engines at the same sequence.
    let own = y.begin(SnapshotId::GENESIS, None).unwrap().seal();
    assert_eq!(y.publish(&own).map(SnapshotId::sequence), Ok(1));
    assert_eq!(x.head(), y.head());
    assert_eq!(x.publish(&own), Err(EngineError::ForeignCandidate));
    assert_eq!(x.snapshot_count(), 2);
    assert_eq!(x.head_snapshot().namespace_count(), 1);
    assert_eq!(y.head_snapshot().namespace_count(), 0);
}

// --- 7. retained snapshots --------------------------------------------------

#[test]
fn every_one_of_fifty_published_blocks_stays_readable_and_unchanged() {
    let mut engine = Engine::new(PriceSchedule::FIXTURE);
    let mut blocks = BlockMap::new();
    blocks.record(BlockNumber(0), engine.head()).unwrap();
    let store = ns(1);
    let mut retained = vec![engine.head_snapshot()];
    // Per block: "k" is deleted every fifth block and otherwise set to v{n};
    // "seq" always becomes the block number.
    let mut expected_k: Vec<Option<Vec<u8>>> = vec![None];
    for block in 1..=50u64 {
        let mut candidate = branch(&engine, None);
        if block == 1 {
            assert_eq!(create(&mut candidate, ALICE), store);
        }
        let k = if block % 5 == 0 {
            execute(&mut candidate, ALICE, delete(store, b"k"));
            None
        } else {
            let value = format!("v{block}").into_bytes();
            execute(&mut candidate, ALICE, put(store, b"k", &value));
            Some(value)
        };
        execute(
            &mut candidate,
            ALICE,
            put(store, b"seq", block.to_string().as_bytes()),
        );
        let id = publish(&mut engine, candidate);
        assert_eq!(id.sequence(), block);
        blocks.record(BlockNumber(block), id).unwrap();
        retained.push(engine.head_snapshot());
        expected_k.push(k);
    }
    assert_eq!(engine.snapshot_count(), 51);
    assert_eq!(blocks.len(), 51);
    assert_eq!(expected_k[1], some(b"v1"));
    assert_eq!(expected_k[4], some(b"v4"));
    assert_eq!(expected_k[5], None);
    assert_eq!(expected_k[49], some(b"v49"));
    assert_eq!(expected_k[50], None);

    for block in 0..=50u64 {
        let index = usize::try_from(block).unwrap();
        let by_block = blocks.at(&engine, BlockNumber(block)).unwrap();
        assert_eq!(by_block.id().sequence(), block);
        let by_id = engine.snapshot(by_block.id()).unwrap();
        for (label, snapshot) in [
            ("retained", &retained[index]),
            ("engine.snapshot", &by_id),
            ("blocks.at", &by_block),
        ] {
            assert_eq!(snapshot.id().sequence(), block, "{label} {block}");
            if block == 0 {
                assert_eq!(snapshot.base(), None, "{label} genesis");
                assert_eq!(snapshot.namespace_count(), 0, "{label} genesis");
                assert_eq!(
                    logical(snapshot.get(store, b"k", ALL)),
                    Err((
                        Rejection::UnknownNamespace,
                        read_rejected(1, Rejection::UnknownNamespace)
                    )),
                    "{label} genesis"
                );
                continue;
            }
            assert_eq!(
                snapshot.base().map(SnapshotId::sequence),
                Some(block - 1),
                "{label} {block}"
            );
            assert_eq!(snapshot.namespace_count(), 1, "{label} {block}");
            assert_eq!(
                value(snapshot.get(store, b"k", ALL)),
                expected_k[index],
                "{label} {block}: k"
            );
            assert_eq!(
                value(snapshot.get(store, b"seq", ALL)),
                Some(block.to_string().into_bytes()),
                "{label} {block}: seq"
            );
            assert_eq!(
                snapshot.record_count(store),
                Some(if expected_k[index].is_some() { 2 } else { 1 }),
                "{label} {block}: record count"
            );
        }
    }
}

// --- 8. branch reads --------------------------------------------------------

#[test]
fn branch_reads_see_uncommitted_writes_that_head_never_shows() {
    let mut engine = Engine::new(PriceSchedule::FIXTURE);
    let mut setup = branch(&engine, None);
    let store = create(&mut setup, ALICE);
    execute(&mut setup, ALICE, put(store, b"k", b"old"));
    execute(&mut setup, ALICE, put(store, b"gone", b"bye"));
    let base = publish(&mut engine, setup);
    let keys: &[&[u8]] = &[b"k", b"gone", b"fresh"];
    let committed = observe(&engine.head_snapshot(), store, keys);
    let committed_other = observe(&engine.head_snapshot(), ns(2), keys);

    let mut candidate = branch(&engine, Some(1_000));
    // put k=new: 1 + 10 + 2*1 + 3 + 4 = 20
    let replaced = execute(&mut candidate, ALICE, put(store, b"k", b"new"));
    assert_eq!(
        (replaced.effect, replaced.receipt),
        (Effect::Put { replaced: true }, applied(20))
    );
    // delete gone: 1 + 5 + 2*4 + 4 = 18
    let removed = execute(&mut candidate, ALICE, delete(store, b"gone"));
    assert_eq!(
        (removed.effect, removed.receipt),
        (Effect::Deleted { existed: true }, applied(18))
    );
    // put fresh=!: 1 + 10 + 2*5 + 1 + 4 = 26
    let inserted = execute(&mut candidate, ALICE, put(store, b"fresh", b"!"));
    assert_eq!(
        (inserted.effect, inserted.receipt),
        (Effect::Put { replaced: false }, applied(26))
    );
    // create: 53
    assert_eq!(create(&mut candidate, BOB), ns(2));
    assert_eq!(candidate.spent(), Cost::new(117), "20 + 18 + 26 + 53");

    // The branch answers from its working state.
    assert_eq!(
        logical(candidate.get(store, b"k", ALL)),
        Ok((some(b"new"), read(7))),
        "1 + 3 + 3"
    );
    assert_eq!(
        logical(candidate.get(store, b"gone", ALL)),
        Ok((None, read(4)))
    );
    assert_eq!(
        logical(candidate.get(store, b"fresh", ALL)),
        Ok((some(b"!"), read(5))),
        "1 + 3 + 1"
    );
    assert_eq!(
        logical(candidate.scan_prefix(store, b"", ALL)),
        Ok((vec![entry(b"fresh", b"!"), entry(b"k", b"new")], read(19))),
        "1 + 4 + (2 + 5 + 1) + (2 + 1 + 3)"
    );
    assert_eq!(
        logical(candidate.descriptor(ns(2), ALL)),
        Ok((descriptor(ns(2), BOB, 0), read(1)))
    );
    assert_eq!(candidate.namespace_count(), 2);

    // The head answers from the published state.
    let head = engine.head_snapshot();
    assert_eq!(head.id(), base);
    assert_eq!(
        logical(head.get(store, b"k", ALL)),
        Ok((some(b"old"), read(7)))
    );
    assert_eq!(
        logical(head.get(store, b"gone", ALL)),
        Ok((some(b"bye"), read(7)))
    );
    assert_eq!(logical(head.get(store, b"fresh", ALL)), Ok((None, read(4))));
    assert_eq!(
        logical(head.scan_prefix(store, b"", ALL)),
        Ok((vec![entry(b"gone", b"bye"), entry(b"k", b"old")], read(20))),
        "1 + 4 + (2 + 4 + 3) + (2 + 1 + 3)"
    );
    assert_eq!(
        logical(head.descriptor(ns(2), ALL)),
        Err((
            Rejection::UnknownNamespace,
            read_rejected(1, Rejection::UnknownNamespace)
        ))
    );
    assert_eq!(head.namespace_count(), 1);
    assert_eq!(observe(&head, store, keys), committed);

    // Reads are not block work.
    assert_eq!(candidate.spent(), Cost::new(117));
    assert_eq!(candidate.remaining(), Cost::new(883));
    assert_eq!(candidate.receipts().len(), 4);

    // Dropping the unsealed branch changes nothing.
    drop(candidate);
    assert_eq!(engine.head(), base);
    assert_eq!(engine.snapshot_count(), 2);
    let head = engine.head_snapshot();
    assert_eq!(observe(&head, store, keys), committed);
    assert_eq!(observe(&head, ns(2), keys), committed_other);
    assert_eq!(head.record_count(store), Some(2));
    assert_eq!(head.record_count(ns(2)), None);

    // The discarded creation released its ordinal.
    let mut again = branch(&engine, None);
    assert_eq!(create(&mut again, CAROL), ns(2));
    drop(again);
    assert_eq!(engine.head_snapshot().namespace_count(), 1);
    assert_eq!(engine.head(), base);
}
