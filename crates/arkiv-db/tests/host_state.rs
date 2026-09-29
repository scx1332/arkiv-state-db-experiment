//! The engine's host-state map (node step N0b-2).
//!
//! What this file establishes:
//! - A fresh state has no host entries. `try_host_put` binds a 32-byte key
//!   to a value of at most 256 bytes (the empty value included) and returns
//!   the value it replaced; `try_host_remove` returns the removed value;
//!   `try_host_get` reads it back. A 257-byte value is refused with
//!   `HostStateError::ValueTooLarge` and changes nothing: not the entry, not
//!   the other entries, not the branch's mutation serial.
//! - `try_host_range(from, limit)` returns at most `limit` entries with keys
//!   at least `from`, in bytewise key order, on branches and snapshots.
//! - Checkpoints and rollbacks restore host state with the rest of the
//!   state; a sealed candidate carries it; every published snapshot keeps
//!   its own version while later blocks change it; discarded and rolled-back
//!   host writes never reach a snapshot. Host writes record no receipt and
//!   spend nothing.
//! - Every successful host put or remove advances the branch's mutation
//!   serial, so a branch cursor issued before it is a `CursorMismatch`.
//! - Isolation, both ways: two engines run the same workload (namespace
//!   writes, raw puts and deletes, typed creates, patches and deletes,
//!   owner and host paths, rejected operations, a transfer, estimates and
//!   an expiry sweep), one of them with host entries written between every
//!   step under keys and values that spell the workload's own namespace
//!   ids, raw keys and record keys. Every write result (effect or
//!   rejection, receipt, physical trace), every estimate, the block's
//!   receipts and spend, every metered read and statistic of both branches
//!   and of the published snapshots are identical, and the host map holds
//!   exactly what a `BTreeMap` model of the host writes holds: no data-plane
//!   operation reads or changes it, and no namespace data appears in it.
//!
//! Expected answers come from the `BTreeMap` model and from the engine run
//! without host state; no cost is asserted here beyond equality, because
//! the claim is that host state changes nothing.

use std::{collections::BTreeMap, ops::Bound};

use arkiv_db::{
    BlockNumber, Branch, Budget, Cell, CellName, Cursor, Engine, EngineError, HOST_VALUE_MAX,
    HostKey, HostStateError, NamespaceId, NamespaceName, OwnerId, PriceSchedule, Projection, Query,
    QueryPage, ReadResult, RecordKey, Rejection, Revision, Snapshot, TypeTag, Value, WriteOp,
    expire_records,
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

/// A host key: `first` followed by 31 zero bytes.
fn hk(first: u8) -> HostKey {
    let mut key = [0u8; 32];
    key[0] = first;
    key
}

/// A host key spelling `bytes`, zero-padded to 32 bytes.
fn spelled(bytes: &[u8]) -> HostKey {
    let mut key = [0u8; 32];
    key[..bytes.len()].copy_from_slice(bytes);
    key
}

fn put(branch: &mut Branch, key: HostKey, value: &[u8]) -> Option<Vec<u8>> {
    branch
        .try_host_put(key, value)
        .expect("in-memory host state never fails")
        .expect("the value fits")
}

fn get(branch: &Branch, key: &HostKey) -> Option<Vec<u8>> {
    branch
        .try_host_get(key)
        .expect("in-memory host state never fails")
}

fn all(branch: &Branch) -> Vec<(HostKey, Vec<u8>)> {
    branch
        .try_host_range(&[0; 32], usize::MAX)
        .expect("in-memory host state never fails")
}

fn all_at(snapshot: &Snapshot) -> Vec<(HostKey, Vec<u8>)> {
    snapshot
        .try_host_range(&[0; 32], usize::MAX)
        .expect("in-memory host state never fails")
}

fn entries(pairs: &[(HostKey, &[u8])]) -> Vec<(HostKey, Vec<u8>)> {
    pairs
        .iter()
        .map(|(key, value)| (*key, value.to_vec()))
        .collect()
}

fn create_namespace(branch: &mut Branch, text: &str) {
    let op = WriteOp::CreateNamespace {
        name: NamespaceName::new(text).unwrap(),
    };
    branch.execute(ALICE, op, Budget::unlimited()).unwrap();
}

fn create_record(branch: &mut Branch, namespace: NamespaceId, record: &[u8], height: u64) {
    let op = WriteOp::CreateRecord {
        namespace,
        key: key(record),
        cells: vec![(name("h"), Cell::attribute(Value::U64(height)))],
    };
    branch.execute(ALICE, op, Budget::unlimited()).unwrap();
}

#[test]
fn a_fresh_state_has_no_host_entries() {
    let engine = Engine::new(PriceSchedule::FIXTURE);
    let genesis = engine.head_snapshot();
    assert_eq!(genesis.try_host_get(&hk(1)), Ok(None));
    assert_eq!(all_at(&genesis), Vec::new());
    let branch = engine.begin(engine.head(), None).unwrap();
    assert_eq!(get(&branch, &hk(1)), None);
    assert_eq!(all(&branch), Vec::new());
}

#[test]
fn puts_replace_and_removes_return_the_previous_value() {
    let engine = Engine::new(PriceSchedule::FIXTURE);
    let mut branch = engine.begin(engine.head(), None).unwrap();
    assert_eq!(put(&mut branch, hk(1), b"one"), None);
    assert_eq!(get(&branch, &hk(1)), Some(b"one".to_vec()));
    assert_eq!(put(&mut branch, hk(1), b"uno"), Some(b"one".to_vec()));
    assert_eq!(get(&branch, &hk(1)), Some(b"uno".to_vec()));
    assert_eq!(branch.try_host_remove(&hk(1)), Ok(Some(b"uno".to_vec())));
    assert_eq!(branch.try_host_remove(&hk(1)), Ok(None));
    assert_eq!(get(&branch, &hk(1)), None);

    // The empty value and a value of exactly HOST_VALUE_MAX bytes fit.
    assert_eq!(HOST_VALUE_MAX, 256);
    assert_eq!(put(&mut branch, hk(2), b""), None);
    assert_eq!(get(&branch, &hk(2)), Some(Vec::new()));
    let largest = [0x5a; 256];
    assert_eq!(put(&mut branch, hk(3), &largest), None);
    assert_eq!(get(&branch, &hk(3)), Some(largest.to_vec()));

    // Nothing the host writes is a receipt or a charge.
    assert!(branch.receipts().is_empty());
    assert_eq!(branch.spent().units(), 0);
}

#[test]
fn oversize_values_are_refused_and_change_nothing() {
    let engine = Engine::new(PriceSchedule::FIXTURE);
    let mut branch = engine.begin(engine.head(), None).unwrap();
    create_namespace(&mut branch, "alpha");
    create_record(&mut branch, ns(1), b"a", 1);
    create_record(&mut branch, ns(1), b"b", 2);
    put(&mut branch, hk(1), b"kept");
    put(&mut branch, hk(2), b"other");
    let before = all(&branch);

    // A branch cursor issued now stays valid across the refused writes.
    let query = Query::match_all();
    let page = branch
        .query(
            ns(1),
            &query,
            &Projection::ID_ONLY,
            1,
            None,
            Budget::unlimited(),
        )
        .unwrap();
    let cursor = page.value.next.expect("a second record follows");

    for len in [257, 300, 4096] {
        let value = vec![0x33; len];
        assert_eq!(
            branch.try_host_put(hk(1), &value),
            Ok(Err(HostStateError::ValueTooLarge { len }))
        );
        assert_eq!(
            branch.try_host_put(hk(9), &value),
            Ok(Err(HostStateError::ValueTooLarge { len }))
        );
    }
    assert_eq!(all(&branch), before);
    assert_eq!(get(&branch, &hk(1)), Some(b"kept".to_vec()));
    assert_eq!(get(&branch, &hk(9)), None);
    let next = branch.query(
        ns(1),
        &query,
        &Projection::ID_ONLY,
        1,
        Some(&cursor),
        Budget::unlimited(),
    );
    assert!(next.is_ok(), "a refused host write changed the serial");
    assert_eq!(
        HostStateError::ValueTooLarge { len: 257 }.to_string(),
        "host value of 257 bytes exceeds 256 bytes"
    );
}

#[test]
fn ranges_are_bounded_ordered_and_start_at_from() {
    let engine = Engine::new(PriceSchedule::FIXTURE);
    let mut branch = engine.begin(engine.head(), None).unwrap();
    let mut one_then_one = hk(1);
    one_then_one[31] = 1;
    let mut one_then_two = hk(1);
    one_then_two[31] = 2;
    // Inserted out of order.
    for (key, value) in [
        (hk(0x80), b"80".as_slice()),
        ([0xff; 32], b"ff"),
        (hk(1), b"01"),
        ([0; 32], b"00"),
        (one_then_one, b"0101"),
    ] {
        put(&mut branch, key, value);
    }
    let everything = entries(&[
        ([0; 32], b"00"),
        (hk(1), b"01"),
        (one_then_one, b"0101"),
        (hk(0x80), b"80"),
        ([0xff; 32], b"ff"),
    ]);
    assert_eq!(all(&branch), everything);
    assert_eq!(branch.try_host_range(&[0; 32], 5), Ok(everything.clone()));
    assert_eq!(
        branch.try_host_range(&[0; 32], 2),
        Ok(everything[..2].to_vec())
    );
    assert_eq!(
        branch.try_host_range(&hk(1), 2),
        Ok(entries(&[(hk(1), b"01"), (one_then_one, b"0101")]))
    );
    // `from` need not be present.
    assert_eq!(
        branch.try_host_range(&one_then_two, 10),
        Ok(entries(&[(hk(0x80), b"80"), ([0xff; 32], b"ff")]))
    );
    assert_eq!(
        branch.try_host_range(&[0xff; 32], 10),
        Ok(entries(&[([0xff; 32], b"ff")]))
    );
    assert_eq!(branch.try_host_range(&[0; 32], 0), Ok(Vec::new()));

    let mut engine = engine;
    let sealed = branch.seal();
    let id = engine.publish(&sealed).unwrap();
    let snapshot = engine.snapshot(id).unwrap();
    assert_eq!(all_at(&snapshot), everything);
    assert_eq!(
        snapshot.try_host_range(&hk(1), 1),
        Ok(entries(&[(hk(1), b"01")]))
    );
    assert_eq!(snapshot.try_host_range(&hk(0x81), 0), Ok(Vec::new()));
    assert_eq!(
        snapshot.try_host_get(&one_then_one),
        Ok(Some(b"0101".to_vec()))
    );
    assert_eq!(snapshot.try_host_get(&one_then_two), Ok(None));
}

#[test]
fn checkpoints_and_rollbacks_restore_host_state() {
    let engine = Engine::new(PriceSchedule::FIXTURE);
    let mut branch = engine.begin(engine.head(), None).unwrap();
    put(&mut branch, hk(1), b"a1");
    put(&mut branch, hk(3), b"c1");
    let first = branch.checkpoint();
    put(&mut branch, hk(1), b"a2");
    put(&mut branch, hk(2), b"b2");
    assert_eq!(branch.try_host_remove(&hk(3)), Ok(Some(b"c1".to_vec())));
    let second = branch.checkpoint();
    put(&mut branch, hk(4), b"d");
    create_namespace(&mut branch, "alpha");

    assert_eq!(branch.rollback_to(second), Ok(0..1));
    assert_eq!(all(&branch), entries(&[(hk(1), b"a2"), (hk(2), b"b2")]));
    assert_eq!(branch.namespace_count(), 0);

    assert_eq!(branch.rollback_to(first), Ok(0..1));
    assert_eq!(all(&branch), entries(&[(hk(1), b"a1"), (hk(3), b"c1")]));
    assert_eq!(
        branch.rollback_to(second),
        Err(EngineError::InvalidCheckpoint)
    );
    // Rolling back to the same checkpoint again restores the same state.
    put(&mut branch, hk(5), b"e");
    assert_eq!(branch.rollback_to(first), Ok(0..1));
    assert_eq!(all(&branch), entries(&[(hk(1), b"a1"), (hk(3), b"c1")]));
}

#[test]
fn snapshots_keep_their_own_host_state() {
    let mut engine = Engine::new(PriceSchedule::FIXTURE);

    let mut first = engine.begin(engine.head(), None).unwrap();
    put(&mut first, hk(1), b"a1");
    put(&mut first, hk(2), b"b1");
    let one = engine.publish(&first.seal()).unwrap();

    let mut second = engine.begin(one, None).unwrap();
    assert_eq!(get(&second, &hk(1)), Some(b"a1".to_vec()));
    put(&mut second, hk(1), b"a2");
    second.try_host_remove(&hk(2)).unwrap();
    put(&mut second, hk(3), b"c2");
    // A competing candidate on the same base loses and changes nothing.
    let mut competitor = engine.begin(one, None).unwrap();
    put(&mut competitor, hk(9), b"lost");
    let two = engine.publish(&second.seal()).unwrap();
    assert_eq!(
        engine.publish(&competitor.seal()),
        Err(EngineError::StaleHead {
            expected: one,
            head: two
        })
    );

    // A rolled-back write and a discarded candidate never reach a snapshot.
    let mut third = engine.begin(two, None).unwrap();
    let checkpoint = third.checkpoint();
    put(&mut third, hk(4), b"rolled back");
    third.rollback_to(checkpoint).unwrap();
    put(&mut third, hk(5), b"e3");
    let three = engine.publish(&third.seal()).unwrap();
    let mut discarded = engine.begin(three, None).unwrap();
    put(&mut discarded, hk(1), b"discarded");
    drop(discarded);

    let at = |id| all_at(&engine.snapshot(id).unwrap());
    assert_eq!(at(arkiv_db::SnapshotId::GENESIS), Vec::new());
    assert_eq!(at(one), entries(&[(hk(1), b"a1"), (hk(2), b"b1")]));
    assert_eq!(at(two), entries(&[(hk(1), b"a2"), (hk(3), b"c2")]));
    assert_eq!(
        at(three),
        entries(&[(hk(1), b"a2"), (hk(3), b"c2"), (hk(5), b"e3")])
    );
    assert_eq!(
        engine.head_snapshot().try_host_get(&hk(1)),
        Ok(Some(b"a2".to_vec()))
    );
    assert_eq!(engine.head_snapshot().try_host_get(&hk(4)), Ok(None));
}

#[test]
fn host_writes_advance_the_branch_mutation_serial() {
    let engine = Engine::new(PriceSchedule::FIXTURE);
    let mut branch = engine.begin(engine.head(), None).unwrap();
    create_namespace(&mut branch, "alpha");
    create_record(&mut branch, ns(1), b"a", 1);
    create_record(&mut branch, ns(1), b"b", 2);
    let query = Query::match_all();
    let cursor = |branch: &Branch| -> Cursor {
        branch
            .query(
                ns(1),
                &query,
                &Projection::ID_ONLY,
                1,
                None,
                Budget::unlimited(),
            )
            .unwrap()
            .value
            .next
            .expect("a second record follows")
    };
    let resume = |branch: &Branch, cursor: &Cursor| {
        branch
            .query(
                ns(1),
                &query,
                &Projection::ID_ONLY,
                1,
                Some(cursor),
                Budget::unlimited(),
            )
            .map(|_| ())
            .map_err(|rejected| rejected.reason)
    };

    let issued = cursor(&branch);
    assert_eq!(resume(&branch, &issued), Ok(()));
    put(&mut branch, hk(1), b"x");
    assert_eq!(resume(&branch, &issued), Err(Rejection::CursorMismatch));

    let issued = cursor(&branch);
    put(&mut branch, hk(1), b"x");
    assert_eq!(
        resume(&branch, &issued),
        Err(Rejection::CursorMismatch),
        "rewriting the same value is still a write"
    );

    let issued = cursor(&branch);
    assert_eq!(branch.try_host_remove(&hk(7)), Ok(None));
    assert_eq!(
        resume(&branch, &issued),
        Err(Rejection::CursorMismatch),
        "removing an absent key advances the serial like a raw delete"
    );

    // Reads of host state change nothing.
    let issued = cursor(&branch);
    get(&branch, &hk(1));
    branch.try_host_range(&[0; 32], 10).unwrap();
    assert_eq!(resume(&branch, &issued), Ok(()));
}

// ---------------------------------------------------------------------------
// Isolation: the same workload with and without host state.
// ---------------------------------------------------------------------------

/// One workload step: an owner write, or a host-path write.
enum Step {
    Owner(OwnerId, WriteOp),
    Host(WriteOp),
}

fn raw_put(namespace: u64, raw: &[u8], value: &[u8]) -> WriteOp {
    WriteOp::Put {
        namespace: ns(namespace),
        key: raw.to_vec(),
        value: value.to_vec(),
    }
}

fn workload() -> Vec<Step> {
    let create = |text: &str| WriteOp::CreateNamespace {
        name: NamespaceName::new(text).unwrap(),
    };
    let record = |record: &[u8], cells: Vec<(CellName, Cell)>| WriteOp::CreateRecord {
        namespace: ns(1),
        key: key(record),
        cells,
    };
    vec![
        Step::Owner(ALICE, create("alpha")),
        Step::Owner(ALICE, create("beta")),
        Step::Owner(ALICE, raw_put(1, b"k1", b"v1")),
        Step::Owner(ALICE, raw_put(1, b"k2", b"v2")),
        Step::Owner(ALICE, raw_put(2, b"k1", b"other")),
        Step::Owner(
            ALICE,
            record(
                b"r1",
                vec![
                    (name("h"), Cell::attribute(Value::U64(5))),
                    (name("t"), Cell::attribute(Value::str("x"))),
                    (name("f"), Cell::field(Value::bytes(&[1, 2]))),
                ],
            ),
        ),
        Step::Owner(
            ALICE,
            record(
                b"r2",
                vec![
                    (name("h"), Cell::attribute(Value::U64(3))),
                    (name("t"), Cell::attribute(Value::str("y"))),
                ],
            ),
        ),
        Step::Owner(
            ALICE,
            record(b"r3", vec![(name("h"), Cell::attribute(Value::U64(9)))]),
        ),
        Step::Owner(
            ALICE,
            WriteOp::PatchRecord {
                namespace: ns(1),
                key: key(b"r2"),
                set: vec![(name("t"), Cell::attribute(Value::str("z")))],
                remove: Vec::new(),
            },
        ),
        Step::Owner(
            ALICE,
            WriteOp::Delete {
                namespace: ns(1),
                key: b"k2".to_vec(),
            },
        ),
        Step::Owner(
            ALICE,
            WriteOp::DeleteRecord {
                namespace: ns(1),
                key: key(b"r3"),
            },
        ),
        Step::Host(WriteOp::PatchRecord {
            namespace: ns(1),
            key: key(b"r1"),
            set: vec![(name("g"), Cell::attribute(Value::I64(-4)))],
            remove: vec![name("f")],
        }),
        Step::Owner(
            ALICE,
            WriteOp::TransferNamespace {
                namespace: ns(2),
                expected_revision: Revision(0),
                new_owner: BOB,
            },
        ),
        // Rejections: unauthorized, duplicate key, oversized raw key, a raw
        // write on the host path, an unknown namespace.
        Step::Owner(ALICE, raw_put(2, b"k9", b"no")),
        Step::Owner(ALICE, record(b"r2", Vec::new())),
        Step::Owner(ALICE, raw_put(1, &[7; 21], b"")),
        Step::Host(raw_put(1, b"k1", b"host")),
        Step::Owner(ALICE, raw_put(9, b"k1", b"v")),
        Step::Owner(BOB, raw_put(2, b"k3", b"bob")),
    ]
}

/// Host entries whose keys and values spell the workload's own
/// identities: namespace ids, raw keys, record keys, an all-zero and an
/// all-ones key.
fn host_writes(step: usize) -> Vec<(HostKey, Vec<u8>)> {
    let index = u8::try_from(step).unwrap();
    vec![
        (spelled(&[0, 0, 0, 0, 0, 0, 0, 1]), vec![index]),
        (spelled(b"k1"), b"v1".to_vec()),
        (spelled(b"r1"), vec![index; usize::from(index) + 1]),
        ([0; 32], b"alpha".to_vec()),
        ([0xff; 32], vec![0xff; 256]),
        (hk(0x40 + index), b"h".to_vec()),
    ]
}

/// Every metered read and statistic of a branch, with query cursors reduced
/// to their positions (cursor contexts name the engine instance).
fn observe_branch(branch: &Branch) -> Vec<String> {
    let traced = Budget::unlimited().traced();
    let mut seen = vec![format!("{}", branch.namespace_count())];
    for ordinal in 1..=3 {
        let namespace = ns(ordinal);
        seen.push(format!("{:?}", branch.get(namespace, b"k1", traced)));
        seen.push(format!("{:?}", branch.get(namespace, b"k2", traced)));
        seen.push(format!("{:?}", branch.scan_prefix(namespace, b"", traced)));
        seen.push(format!("{:?}", branch.descriptor(namespace, traced)));
        for record in [b"r1".as_slice(), b"r2", b"r3"] {
            let row = branch.get_record(namespace, &key(record), &Projection::ALL, traced);
            seen.push(format!("{row:?}"));
        }
        for query in queries() {
            seen.push(page_view(branch.query(
                namespace,
                &query,
                &Projection::ALL,
                1,
                None,
                traced,
            )));
            seen.push(format!("{:?}", branch.count(namespace, &query, traced)));
        }
        seen.push(format!(
            "{:?}",
            branch.terms(
                namespace,
                &name("h"),
                TypeTag::U64,
                Bound::Unbounded,
                10,
                traced
            )
        ));
        seen.push(format!(
            "{:?} {:?} {:?} {:?}",
            branch.record_count(namespace),
            branch.typed_record_count(namespace),
            branch.term_count(namespace),
            branch.index_consistent(namespace)
        ));
    }
    seen
}

/// The same observation of a published snapshot, plus a full keyset walk.
fn observe_snapshot(snapshot: &Snapshot) -> Vec<String> {
    let traced = Budget::unlimited().traced();
    let mut seen = vec![format!("{}", snapshot.namespace_count())];
    for ordinal in 1..=3 {
        let namespace = ns(ordinal);
        seen.push(format!("{:?}", snapshot.get(namespace, b"k1", traced)));
        seen.push(format!(
            "{:?}",
            snapshot.scan_prefix(namespace, b"", traced)
        ));
        seen.push(format!("{:?}", snapshot.descriptor(namespace, traced)));
        for record in [b"r1".as_slice(), b"r2", b"r3"] {
            let row = snapshot.get_record(namespace, &key(record), &Projection::ALL, traced);
            seen.push(format!("{row:?}"));
        }
        for query in queries() {
            let mut cursor = None;
            loop {
                let page = snapshot.query(
                    namespace,
                    &query,
                    &Projection::ALL,
                    1,
                    cursor.as_ref(),
                    traced,
                );
                seen.push(page_view(page.clone()));
                cursor = page.ok().and_then(|page| page.value.next);
                if cursor.is_none() {
                    break;
                }
            }
            seen.push(format!("{:?}", snapshot.count(namespace, &query, traced)));
        }
        seen.push(format!(
            "{:?}",
            snapshot.terms(
                namespace,
                &name("t"),
                TypeTag::Str,
                Bound::Unbounded,
                10,
                traced
            )
        ));
        seen.push(format!(
            "{:?} {:?} {:?} {:?}",
            snapshot.record_count(namespace),
            snapshot.typed_record_count(namespace),
            snapshot.term_count(namespace),
            snapshot.index_consistent(namespace)
        ));
    }
    seen
}

fn queries() -> Vec<Query> {
    vec![
        Query::match_all(),
        Query::term(arkiv_db::Term::range(
            name("h"),
            Bound::Included(Value::U64(0)),
            Bound::Unbounded,
        )),
        Query::term(arkiv_db::Term::starts_with(name("t"), "")),
    ]
}

fn page_view(result: ReadResult<QueryPage>) -> String {
    match result {
        Ok(metered) => format!(
            "{:?} next={:?} {:?} {:?}",
            metered.value.rows,
            metered.value.next.as_ref().map(Cursor::after),
            metered.receipt,
            metered.physical
        ),
        Err(rejected) => format!("{rejected:?}"),
    }
}

#[test]
fn the_data_plane_never_sees_host_state() {
    let mut plain = Engine::new(PriceSchedule::FIXTURE);
    let mut hosted = Engine::new(PriceSchedule::FIXTURE);
    let mut left = plain
        .begin(plain.head(), Some(arkiv_db::Cost::new(100_000)))
        .unwrap();
    let mut right = hosted
        .begin(hosted.head(), Some(arkiv_db::Cost::new(100_000)))
        .unwrap();
    let mut model: BTreeMap<HostKey, Vec<u8>> = BTreeMap::new();
    let traced = Budget::unlimited().traced();
    let mut rejections = Vec::new();

    for (index, step) in workload().into_iter().enumerate() {
        for (key, value) in host_writes(index) {
            put(&mut right, key, &value);
            model.insert(key, value);
        }
        if index % 3 == 2 {
            let removed = hk(0x40 + u8::try_from(index - 1).unwrap());
            right.try_host_remove(&removed).unwrap();
            model.remove(&removed);
        }
        let host_before = all(&right);
        let (left_result, right_result) = match step {
            Step::Owner(actor, op) => {
                assert_eq!(
                    left.estimate(actor, op.clone(), traced),
                    right.estimate(actor, op.clone(), traced),
                    "estimate of step {index}"
                );
                assert_eq!(all(&right), host_before, "an estimate changed host state");
                (
                    left.execute(actor, op.clone(), traced),
                    right.execute(actor, op, traced),
                )
            }
            Step::Host(op) => {
                assert_eq!(
                    left.estimate_host(op.clone(), traced),
                    right.estimate_host(op.clone(), traced),
                    "host estimate of step {index}"
                );
                assert_eq!(all(&right), host_before, "an estimate changed host state");
                (
                    left.execute_host(op.clone(), traced),
                    right.execute_host(op, traced),
                )
            }
        };
        assert_eq!(left_result, right_result, "step {index}");
        if let Err(rejected) = right_result {
            rejections.push((index, rejected.reason));
        }
        assert_eq!(left.receipts(), right.receipts());
        assert_eq!(left.spent(), right.spent());
        assert_eq!(
            observe_branch(&left),
            observe_branch(&right),
            "after step {index}"
        );
        let expected: Vec<(HostKey, Vec<u8>)> = model.clone().into_iter().collect();
        assert_eq!(all(&right), expected, "host state after step {index}");
        assert_eq!(
            all(&left),
            Vec::new(),
            "data-plane state leaked into host state"
        );
    }

    // Steps 13 to 17 are rejected, every other step applies.
    assert_eq!(
        rejections,
        vec![
            (13, Rejection::Unauthorized),
            (14, Rejection::AlreadyExists),
            (
                15,
                Rejection::InvalidArgument(arkiv_db::ArgumentFault::KeyTooLong)
            ),
            (
                16,
                Rejection::InvalidArgument(arkiv_db::ArgumentFault::NotHostOperation)
            ),
            (17, Rejection::UnknownNamespace),
        ]
    );

    // The expiry sweep deletes the same records with the same receipts:
    // r2 (height 3, record id 2); r1 (height 5) stays.
    let h = name("h");
    let block = BlockNumber(4);
    let left_expired = expire_records(&mut left, ns(1), &h, block, traced, traced);
    let right_expired = expire_records(&mut right, ns(1), &h, block, traced, traced);
    assert_eq!(left_expired, right_expired);
    let expired: Vec<_> = right_expired
        .unwrap()
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert_eq!(expired, vec![arkiv_db::RecordId::from_ordinal(2).unwrap()]);
    assert_eq!(left.receipts(), right.receipts());
    let expected: Vec<(HostKey, Vec<u8>)> = model.into_iter().collect();
    assert_eq!(all(&right), expected);

    let left_sealed = left.seal();
    let right_sealed = right.seal();
    assert_eq!(left_sealed.receipts(), right_sealed.receipts());
    assert_eq!(left_sealed.spent(), right_sealed.spent());
    let left_id = plain.publish(&left_sealed).unwrap();
    let right_id = hosted.publish(&right_sealed).unwrap();
    let left_snapshot = plain.snapshot(left_id).unwrap();
    let right_snapshot = hosted.snapshot(right_id).unwrap();
    assert_eq!(
        observe_snapshot(&left_snapshot),
        observe_snapshot(&right_snapshot)
    );
    assert_eq!(all_at(&right_snapshot), expected);
    assert_eq!(all_at(&left_snapshot), Vec::new());
}
