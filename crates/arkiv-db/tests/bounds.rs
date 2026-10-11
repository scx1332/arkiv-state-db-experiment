//! Admission boundaries applied to real block packets and historical states.
use std::ops::Bound;

use arkiv_db::*;

const ALICE: OwnerId = OwnerId::new([1; 20]);
const BOB: OwnerId = OwnerId::new([2; 20]);

fn create_namespace(branch: &mut Branch, name: &str) -> NamespaceId {
    let result = branch
        .execute(
            ALICE,
            WriteOp::CreateNamespace {
                name: NamespaceName::new(name).unwrap(),
            },
            Budget::unlimited(),
        )
        .unwrap();
    let Effect::NamespaceCreated(id) = result.effect else {
        panic!("namespace expected")
    };
    id
}

fn assert_admission(error: Rejected, reason: Rejection) {
    assert_eq!(error.reason, reason);
    assert_eq!(error.receipt.spent, Cost::ZERO);
    assert_eq!(error.physical, PhysicalTrace::default());
}

#[test]
fn names_are_bounded_and_follow_namespace_history_and_transfers() {
    let name = "n".repeat(20);
    for invalid in [
        "n".repeat(CellName::MAX_LEN + 1),
        "é".repeat(20),
        String::new(),
    ] {
        assert!(NamespaceName::new(&invalid).is_err());
        assert!(CellName::new(&invalid).is_err());
    }
    assert_eq!(CellName::new(&name).unwrap().as_str(), name);
    assert_eq!(NamespaceName::new(&name).unwrap().as_str(), name);
    let mut engine = Engine::new(PriceSchedule::FIXTURE);
    let mut branch = engine.begin(engine.head(), None).unwrap();
    let ns = create_namespace(&mut branch, &name);
    branch
        .execute(
            ALICE,
            WriteOp::Put {
                namespace: ns,
                key: b"data".to_vec(),
                value: b"kept".to_vec(),
            },
            Budget::unlimited(),
        )
        .unwrap();
    let checkpoint = branch.checkpoint();
    let discarded = create_namespace(&mut branch, "discarded");
    branch.rollback_to(checkpoint).unwrap();
    let replacement = create_namespace(&mut branch, "replacement");
    assert_eq!(discarded, replacement);
    // Name uniqueness is host registration policy; labels are not identities.
    let duplicate = create_namespace(&mut branch, &name);
    assert_ne!(duplicate, ns);
    engine.publish(&branch.seal()).unwrap();
    let old = engine.head_snapshot();
    assert_eq!(
        old.descriptor(replacement, Budget::unlimited())
            .unwrap()
            .value
            .name
            .as_str(),
        "replacement"
    );

    let mut branch = engine.begin(engine.head(), None).unwrap();
    branch
        .execute(
            ALICE,
            WriteOp::TransferNamespace {
                namespace: ns,
                expected_revision: Revision::INITIAL,
                new_owner: BOB,
            },
            Budget::unlimited(),
        )
        .unwrap();
    engine.publish(&branch.seal()).unwrap();
    let current = engine.head_snapshot();
    let before = old.descriptor(ns, Budget::unlimited()).unwrap().value;
    let after = current.descriptor(ns, Budget::unlimited()).unwrap().value;
    assert_eq!(before.name, after.name);
    assert_eq!(after.name.as_str(), name);
    assert_eq!((before.owner, after.owner), (ALICE, BOB));
    assert!(current.records_shared_with(&old, ns));
    assert_eq!(
        current.get(ns, b"data", Budget::unlimited()).unwrap().value,
        Some(b"kept".to_vec())
    );
}

#[test]
fn raw_keys_and_prefixes_are_rejected_before_any_lookup_or_write_charge() {
    let mut engine = Engine::new(PriceSchedule::FIXTURE);
    let mut branch = engine.begin(engine.head(), None).unwrap();
    let ns = create_namespace(&mut branch, "shop");
    let key = vec![b'x'; 20];
    branch
        .execute(
            ALICE,
            WriteOp::Put {
                namespace: ns,
                key: key.clone(),
                value: Vec::new(),
            },
            Budget::unlimited(),
        )
        .unwrap();
    assert_eq!(
        branch.get(ns, &key, Budget::limit(4)).unwrap().value,
        Some(Vec::new())
    );
    assert_eq!(
        branch
            .scan_prefix(ns, &key, Budget::unlimited())
            .unwrap()
            .value
            .len(),
        1
    );
    let spent = branch.spent();
    let receipts = branch.receipts().len();
    let reason = Rejection::InvalidArgument(ArgumentFault::KeyTooLong);
    for length in [21, 8 * 1024 * 1024] {
        let long = vec![b'x'; length];
        for op in [
            WriteOp::Put {
                namespace: ns,
                key: long.clone(),
                value: Vec::new(),
            },
            WriteOp::Delete {
                namespace: ns,
                key: long.clone(),
            },
        ] {
            assert_admission(
                branch
                    .estimate(ALICE, op.clone(), Budget::limit(0))
                    .unwrap_err(),
                reason,
            );
            assert_admission(
                branch.execute(ALICE, op, Budget::unlimited()).unwrap_err(),
                reason,
            );
        }
        assert_admission(branch.get(ns, &long, Budget::limit(4)).unwrap_err(), reason);
        assert_admission(
            branch
                .scan_prefix(ns, &long, Budget::unlimited())
                .unwrap_err(),
            reason,
        );
    }
    assert_eq!(branch.spent(), spent);
    assert_eq!(branch.receipts().len(), receipts);
    engine.publish(&branch.seal()).unwrap();
    let snapshot = engine.head_snapshot();
    assert_eq!(
        snapshot.get(ns, &key, Budget::limit(4)).unwrap().value,
        Some(Vec::new())
    );
    let long = vec![b'x'; 21];
    assert_admission(
        snapshot.get(ns, &long, Budget::unlimited()).unwrap_err(),
        reason,
    );
    assert_admission(
        snapshot
            .scan_prefix(ns, &long, Budget::unlimited())
            .unwrap_err(),
        reason,
    );
}

#[test]
fn index_values_have_a_hard_byte_cap_while_payloads_keep_their_own_limit() {
    // Raising the payload limit cannot raise the indexed-value cap.
    let mut engine = Engine::with_limits(
        PriceSchedule::FIXTURE,
        Limits {
            max_str_len: 8192,
            max_bytes_len: 8192,
            ..Limits::DEFAULT
        },
    );
    let mut branch = engine.begin(engine.head(), None).unwrap();
    let ns = create_namespace(&mut branch, "shop");
    let name = CellName::new(&"a".repeat(20)).unwrap();
    let payload = CellName::new("payload").unwrap();
    let value = Value::str(&"é".repeat(arkiv_db::Limits::DEFAULT_INDEXED_LEN / 2)); // 32 characters, 64 UTF-8 bytes.
    let large = Value::bytes(&vec![0; 8192]);
    let key = RecordKey::new(b"row").unwrap();
    branch
        .execute(
            ALICE,
            WriteOp::CreateRecord {
                namespace: ns,
                key: key.clone(),
                cells: vec![
                    (name.clone(), Cell::attribute(value.clone())),
                    (payload.clone(), Cell::field(large.clone())),
                ],
            },
            Budget::unlimited(),
        )
        .unwrap();
    let spent = branch.spent();
    let receipts = branch.receipts().len();
    let too_long = Value::str(&format!(
        "{}x",
        "é".repeat(arkiv_db::Limits::DEFAULT_INDEXED_LEN / 2)
    ));
    for op in [
        WriteOp::CreateRecord {
            namespace: ns,
            key: RecordKey::new(b"bad").unwrap(),
            cells: vec![(name.clone(), Cell::attribute(too_long.clone()))],
        },
        WriteOp::PatchRecord {
            namespace: ns,
            key: key.clone(),
            set: vec![(name.clone(), Cell::attribute(too_long.clone()))],
            remove: vec![],
        },
    ] {
        let reason = Rejection::InvalidArgument(ArgumentFault::ValueTooLarge);
        assert_admission(
            branch
                .estimate(ALICE, op.clone(), Budget::unlimited())
                .unwrap_err(),
            reason,
        );
        assert_admission(
            branch
                .execute(ALICE, op.clone(), Budget::unlimited())
                .unwrap_err(),
            reason,
        );
        assert_admission(
            branch.execute_host(op, Budget::unlimited()).unwrap_err(),
            reason,
        );
    }
    assert_eq!(branch.spent(), spent);
    assert_eq!(branch.receipts().len(), receipts);
    assert_eq!(branch.index_consistent(ns), Some(true));

    let invalid_queries = [
        Query::term(Term::eq(name.clone(), too_long.clone())),
        Query::term(Term::not_eq(name.clone(), too_long.clone())),
        Query::term(Term::starts_with(
            name.clone(),
            &"x".repeat(arkiv_db::Limits::DEFAULT_INDEXED_LEN + 1),
        )),
    ];
    let reason = Rejection::InvalidQuery(QueryFault::ValueTooLarge);
    for query in &invalid_queries {
        assert_admission(
            branch
                .query(ns, query, &Projection::ALL, 1, None, Budget::unlimited())
                .unwrap_err(),
            reason,
        );
        assert_admission(
            branch.count(ns, query, Budget::unlimited()).unwrap_err(),
            reason,
        );
    }
    for lower in [
        Bound::Included(too_long.clone()),
        Bound::Excluded(too_long.clone()),
    ] {
        assert_admission(
            branch
                .terms(ns, &name, TypeTag::Str, lower, 1, Budget::unlimited())
                .unwrap_err(),
            reason,
        );
    }
    engine.publish(&branch.seal()).unwrap();
    let snapshot = engine.head_snapshot();
    for query in &invalid_queries {
        assert_admission(
            snapshot
                .query(ns, query, &Projection::ALL, 1, None, Budget::unlimited())
                .unwrap_err(),
            reason,
        );
        assert_admission(
            snapshot.count(ns, query, Budget::unlimited()).unwrap_err(),
            reason,
        );
    }
    assert_admission(
        snapshot
            .terms(
                ns,
                &name,
                TypeTag::Str,
                Bound::Included(too_long),
                1,
                Budget::unlimited(),
            )
            .unwrap_err(),
        reason,
    );
    let row = snapshot
        .get_record(ns, &key, &Projection::ALL, Budget::unlimited())
        .unwrap()
        .value
        .unwrap();
    assert!(row.cells.contains(&(payload, Cell::field(large))));
    for term in [
        Term::eq(name.clone(), value.clone()),
        Term::starts_with(
            name.clone(),
            &"é".repeat(arkiv_db::Limits::DEFAULT_INDEXED_LEN / 2),
        ),
    ] {
        assert_eq!(
            snapshot
                .count(ns, &Query::term(term), Budget::unlimited())
                .unwrap()
                .value,
            1
        );
    }
    assert_eq!(
        snapshot
            .terms(
                ns,
                &name,
                TypeTag::Str,
                Bound::Included(value.clone()),
                1,
                Budget::unlimited()
            )
            .unwrap()
            .value,
        vec![value]
    );
}
