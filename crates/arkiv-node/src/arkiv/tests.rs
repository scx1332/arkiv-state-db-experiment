use super::*;
use arkiv_db::{Engine, Limits, NamespaceName, OwnerId, PriceSchedule, Term};
fn setup() -> (Engine, Branch, ArkivExecutionContext) {
    let engine = Engine::with_limits(
        PriceSchedule::FIXTURE,
        Limits {
            max_str_len: 128 * 1024,
            max_bytes_len: 128 * 1024,
            ..Limits::DEFAULT
        },
    );
    let mut branch = engine.begin(engine.head(), None).unwrap();
    branch
        .try_execute(
            OwnerId::new([1; 20]),
            WriteOp::CreateNamespace {
                name: NamespaceName::new("entities").unwrap(),
            },
            Budget::unlimited(),
        )
        .unwrap()
        .unwrap();
    let ctx = ArkivExecutionContext {
        caller: [1; 20],
        block_height: 1,
        chain_id: 1337,
        max_lifetime: 1000,
        expiry_per_block: 100,
        allow_permanent_create: false,
    };
    (engine, branch, ctx)
}
fn create(flags: u8) -> ArkivOperation {
    ArkivOperation::Create {
        salt: 7,
        flags,
        expiry: Expiry {
            expires_at: 10,
            min_lifetime: 0,
        },
        attributes: vec![(name("score"), Cell::attribute(Value::I32(-1)))],
    }
}
fn key(c: &ArkivExecutionContext, nonce: u64) -> RecordKey {
    RecordKey::new(&derive_entity_key(c.chain_id, &c.caller, nonce, 7)).unwrap()
}
fn run(b: &mut Branch, c: &ArkivExecutionContext, op: &ArkivOperation) -> OpOutcome {
    execute_operation(b, entity_namespace(), c, op, Budget::unlimited()).unwrap()
}
fn read(b: &Branch, k: &RecordKey) -> ArkivEntityProjection {
    project(
        &b.try_get_record(entity_namespace(), k, &Projection::ALL, Budget::unlimited())
            .unwrap()
            .unwrap()
            .value
            .unwrap(),
    )
    .unwrap()
}
fn applied(o: OpOutcome) {
    assert!(matches!(o, OpOutcome::Applied { .. }), "{o:?}")
}
#[test]
fn lifecycle_is_committed_owner_authority() {
    let (_e, mut b, mut c) = setup();
    applied(run(&mut b, &c, &create(0)));
    let k = key(&c, 0);
    let original = read(&b, &k);
    assert_eq!(original.creator, [1; 20]);
    // No payload or content type: the entity holds exactly what was written.
    assert_eq!(
        original.attributes,
        vec![(name("score"), Cell::attribute(Value::I32(-1)))]
    );
    c.block_height = 2;
    applied(run(
        &mut b,
        &c,
        &ArkivOperation::TransferOwnership {
            key: k.clone(),
            owner: [2; 20],
        },
    ));
    assert_eq!(
        run(&mut b, &c, &ArkivOperation::Delete { key: k.clone() }),
        rejected(ArkivFault::NotOwner)
    );
    c.caller = [2; 20];
    c.block_height = 3;
    applied(run(
        &mut b,
        &c,
        &ArkivOperation::ExtendExpiry {
            key: k.clone(),
            expiry: Expiry {
                expires_at: 10,
                min_lifetime: 0,
            },
        },
    ));
    let p = read(&b, &k);
    assert_eq!(
        (p.owner, p.creator, p.created_at, p.updated_at, p.expires_at),
        ([2; 20], [1; 20], 1, 3, 10)
    );
    assert_eq!(count(&b, &nonce_key(&[1; 20])).unwrap(), 1);
    applied(run(&mut b, &c, &ArkivOperation::Delete { key: k }));
    c.caller = [1; 20];
    applied(run(&mut b, &c, &create(0)));
    assert_ne!(key(&c, 0), key(&c, 1));
}
#[test]
fn batch_failure_rolls_back_cells_indexes_nonce_and_schedule() {
    let (_e, mut b, c) = setup();
    let ops = [
        create(0),
        ArkivOperation::Delete {
            key: RecordKey::new(&[99; 32]).unwrap(),
        },
    ];
    let outcomes = execute_batch(&mut b, entity_namespace(), &c, &ops, 1_000_000).unwrap();
    assert!(
        matches!(outcomes[0], OpOutcome::RolledBack { .. }),
        "{outcomes:?}"
    );
    assert_eq!(count(&b, &nonce_key(&c.caller)).unwrap(), 0);
    assert_eq!(count(&b, &crate::exec::total_key(10)).unwrap(), 0);
    assert!(
        b.try_get_record(
            entity_namespace(),
            &key(&c, 0),
            &Projection::ALL,
            Budget::unlimited()
        )
        .unwrap()
        .unwrap()
        .value
        .is_none()
    );
    applied(run(&mut b, &c, &create(0)));
    assert_eq!(
        read(&b, &key(&c, 0)).key,
        derive_entity_key(c.chain_id, &c.caller, 0, 7)
    );
}
#[test]
fn readonly_permissionless_and_exact_expiry() {
    let (_e, mut b, mut c) = setup();
    applied(run(&mut b, &c, &create(3)));
    let k = key(&c, 0);
    assert_eq!(
        run(
            &mut b,
            &c,
            &ArkivOperation::Patch {
                key: k.clone(),
                set: vec![],
                remove: vec![]
            }
        ),
        rejected(ArkivFault::Readonly)
    );
    c.caller = [3; 20];
    c.block_height = 9;
    applied(run(
        &mut b,
        &c,
        &ArkivOperation::ExtendExpiry {
            key: k.clone(),
            expiry: Expiry {
                expires_at: 10,
                min_lifetime: 0,
            },
        },
    ));
    c.block_height = 10;
    assert_eq!(
        run(
            &mut b,
            &c,
            &ArkivOperation::ExtendExpiry {
                key: k.clone(),
                expiry: Expiry::PERMANENT
            }
        ),
        rejected(ArkivFault::Expired)
    );
    let expired = arkiv_db::host::try_expire_records(
        &mut b,
        entity_namespace(),
        &name("$expiresAt"),
        arkiv_db::BlockNumber(10),
        Budget::unlimited(),
        Budget::unlimited(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(expired.len(), 1);
}
#[test]
fn authority_and_projection_reject_corrupt_system_cells() {
    let (_e, mut b, c) = setup();
    let mut op = create(0);
    if let ArkivOperation::Create { attributes, .. } = &mut op {
        attributes.push((name("$owner"), Cell::attribute(Value::Addr([2; 20]))))
    }
    assert_eq!(run(&mut b, &c, &op), rejected(ArkivFault::Name));
    applied(run(&mut b, &c, &create(0)));
    let mut row = b
        .try_get_record(
            entity_namespace(),
            &key(&c, 0),
            &Projection::ALL,
            Budget::unlimited(),
        )
        .unwrap()
        .unwrap()
        .value
        .unwrap();
    row.cells
        .iter_mut()
        .find(|(n, _)| n.as_str() == "$key")
        .unwrap()
        .1
        .value = Value::Key([0; 32]);
    assert_eq!(validate_proven_row(&row), Err(ArkivFault::Corrupt));
}
#[test]
fn decimal_exact_extremes_and_sdk_names() {
    for (input, output) in [
        ("+0001.230000", "1.23"),
        ("-0.0", "0"),
        ("-0.000000000000000001", "-0.000000000000000001"),
    ] {
        assert_eq!(format_decimal(&parse_decimal(input).unwrap()), output)
    }
    for bad in ["1e2", ".5", "1.", "1.0000000000000000000", " 1", "--1"] {
        assert!(parse_decimal(bad).is_err(), "{bad}")
    }
    for word in [[0x7f; 32], [0x80; 32], [0xff; 32]] {
        assert_eq!(parse_decimal(&format_decimal(&word)).unwrap(), word)
    }
    let mut min = [0; 32];
    min[0] = 128;
    assert_eq!(parse_decimal(&format_decimal(&min)).unwrap(), min);
    assert!(!sdk_safe_name("not"));
    assert!(protocol_name("not"));
    assert!(!sdk_safe_name("a--b"));
    assert!(!sdk_safe_name("a:b"));
    assert!(sdk_safe_name(&"a".repeat(32)));
    assert!(!sdk_safe_name(&"a".repeat(33)));
}
#[test]
fn sdk_abi_and_tombstone_validation() {
    for value in [
        Value::Bool(true),
        Value::I32(i32::MIN),
        Value::U64(u64::MAX),
        Value::U256([255; 32]),
        Value::Dec([255; 32]),
        Value::Bytes32([3; 32]),
        Value::bytes(&[0, 255]),
        Value::str("é"),
        Value::Addr([4; 20]),
        Value::Key([5; 32]),
    ] {
        let abi = encode_abi_value(&value).unwrap();
        assert_eq!(
            decode_abi_value(ArkivTypeId::of(&value).unwrap() as u8, &abi).unwrap(),
            value
        );
    }
    assert!(decode_abi_value(1, &[2; 32]).is_err());
    assert!(decode_abi_value(2, &[1; 32]).is_err());
    assert!(decode_abi_value(9, &[1; 32]).is_err());
    let mut n = [0; 32];
    n[0] = b'a';
    let tomb = AbiAttribute {
        name: n,
        type_id: 0,
        value: vec![],
    };
    assert!(decode_protocol_triples(std::slice::from_ref(&tomb), false).is_err());
    assert!(decode_protocol_triples(std::slice::from_ref(&tomb), true).is_ok());
    assert!(decode_protocol_triples(&[tomb.clone(), tomb], true).is_err());
}
#[test]
fn nonce_exhaustion_and_expiry_overflow_do_not_write() {
    let (_e, mut b, mut c) = setup();
    put_count(&mut b, nonce_key(&c.caller), u64::MAX).unwrap();
    assert_eq!(
        run(&mut b, &c, &create(0)),
        rejected(ArkivFault::NonceExhausted)
    );
    c.block_height = u64::MAX - 1;
    let op = ArkivOperation::Create {
        salt: 0,
        flags: 0,
        expiry: Expiry {
            expires_at: u64::MAX,
            min_lifetime: 2,
        },
        attributes: vec![],
    };
    assert!(matches!(
        run(&mut b, &c, &op),
        OpOutcome::HostRejected {
            fault: HostFault::ExpiryInvalid(crate::ExpiryFault::Overflow)
        }
    ));
}
#[test]
fn sdk_generated_key_and_decimal_fixtures() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../arkiv-db/tests/fixtures/arkiv/sdk-host.json"
    ))
    .unwrap();
    for v in fixture["keys"].as_array().unwrap() {
        let caller =
            alloy_primitives::Address::parse_checksummed(v["owner"].as_str().unwrap(), None)
                .unwrap();
        let actual = derive_entity_key(
            v["chainId"].as_u64().unwrap(),
            &caller.0.0,
            v["nonce"].as_str().unwrap().parse().unwrap(),
            v["salt"].as_str().unwrap().parse().unwrap(),
        );
        assert_eq!(
            format!("{:#x}", alloy_primitives::B256::from(actual)),
            v["key"].as_str().unwrap()
        );
    }
    for v in fixture["decimals"].as_array().unwrap() {
        assert_eq!(
            format_decimal(&parse_decimal(v["input"].as_str().unwrap()).unwrap()),
            v["canonical"].as_str().unwrap()
        )
    }
}
#[test]
fn scalar_types_stay_distinct() {
    assert!(ArkivTypeId::of(&Value::I64(1)).is_err());
    assert_ne!(
        ArkivTypeId::of(&Value::Bytes32([0; 32])),
        ArkivTypeId::of(&Value::Key([0; 32]))
    );
}

fn triple(name_text: &str, type_id: u8, value: Vec<u8>) -> AbiAttribute {
    let mut n = [0; 32];
    n[..name_text.len()].copy_from_slice(name_text.as_bytes());
    AbiAttribute {
        name: n,
        type_id,
        value,
    }
}
/// The ABI `typeId` of a stored cell: its [`ArkivTypeId`], with
/// [`INDEXED_FLAG`] when it is indexed.
fn type_id(cell: &Cell) -> Result<u8, ArkivFault> {
    let base = ArkivTypeId::of(&cell.value)? as u8;
    Ok(if cell.is_attribute() {
        base | INDEXED_FLAG
    } else {
        base
    })
}
/// The ABI triple of a stored user attribute, the inverse of
/// [`decode_protocol_triples`] for one non-tombstone triple.
fn encode_protocol_triple(name: &CellName, cell: &Cell) -> Result<AbiAttribute, ArkivFault> {
    let mut word = [0u8; 32];
    word[..name.len()].copy_from_slice(name.as_str().as_bytes());
    Ok(AbiAttribute {
        name: word,
        type_id: type_id(cell)?,
        value: encode_abi_value(&cell.value)?,
    })
}
fn every_type() -> [Value; 10] {
    [
        Value::Bool(true),
        Value::I32(i32::MIN),
        Value::U64(u64::MAX),
        Value::U256([255; 32]),
        Value::Dec([255; 32]),
        Value::Bytes32([3; 32]),
        Value::bytes(&[0, 255]),
        Value::str("é"),
        Value::Addr([4; 20]),
        Value::Key([5; 32]),
    ]
}

#[test]
fn every_type_id_decodes_with_and_without_the_indexed_flag() {
    for value in every_type() {
        let base = ArkivTypeId::of(&value).unwrap() as u8;
        let abi = encode_abi_value(&value).unwrap();
        for patch in [false, true] {
            let unindexed =
                decode_protocol_triples(&[triple("a", base, abi.clone())], patch).unwrap();
            assert_eq!(
                unindexed,
                vec![(name("a"), Some(Cell::field(value.clone())))],
                "{value:?}"
            );
            let flagged =
                decode_protocol_triples(&[triple("a", base | INDEXED_FLAG, abi.clone())], patch);
            if matches!(value, Value::Bytes(_)) {
                // 0x87: bytes are stored, never indexed.
                assert_eq!(flagged, Err(ArkivFault::Value));
            } else {
                assert_eq!(
                    flagged.unwrap(),
                    vec![(name("a"), Some(Cell::attribute(value.clone())))],
                    "{value:?}"
                );
            }
        }
        // The stored cell encodes back to the same triple, flag included.
        for cell in [Cell::field(value.clone()), Cell::attribute(value.clone())] {
            if cell.is_attribute() && matches!(value, Value::Bytes(_)) {
                continue;
            }
            let encoded = encode_protocol_triple(&name("a"), &cell).unwrap();
            assert_eq!(encoded.type_id, type_id(&cell).unwrap());
            assert_eq!(encoded.type_id & INDEXED_FLAG != 0, cell.is_attribute());
            assert_eq!(
                decode_protocol_triples(&[encoded], false).unwrap(),
                vec![(name("a"), Some(cell))]
            );
        }
    }
    assert_eq!(
        decode_protocol_triples(&[triple("a", 0x87, vec![1])], false),
        Err(ArkivFault::Value)
    );
    // Types 11..=127, flagged or not, are unknown.
    for unknown in [11, 0x7f, 0x8b, 0xff] {
        assert_eq!(
            decode_protocol_triples(&[triple("a", unknown, vec![0; 32])], true),
            Err(ArkivFault::Value)
        );
    }
}

#[test]
fn tombstones_are_unflagged_empty_and_patch_only() {
    assert_eq!(
        decode_protocol_triples(&[triple("a", 0, vec![])], true).unwrap(),
        vec![(name("a"), None)]
    );
    for (type_id, value, patch) in [
        (0, vec![], false),
        (0, vec![0], true),
        // A flagged tombstone carries no type, so it is not a tombstone.
        (INDEXED_FLAG, vec![], true),
        (INDEXED_FLAG, vec![], false),
    ] {
        assert_eq!(
            decode_protocol_triples(&[triple("a", type_id, value)], patch),
            Err(ArkivFault::Value),
            "{type_id:#x}"
        );
    }
}

#[test]
fn payload_and_content_type_are_gone_from_every_write_path() {
    // Not protocol names on the wire, whatever their type.
    for (n, type_id, value) in [
        ("$payload", 7, vec![1, 2, 3]),
        ("$contentType", 8, b"text/plain".to_vec()),
        ("$contentType", 0x88, b"text/plain".to_vec()),
    ] {
        assert_eq!(
            decode_protocol_triples(&[triple(n, type_id, value)], false),
            Err(ArkivFault::Name)
        );
        assert_eq!(
            decode_protocol_triples(&[triple(n, 0, vec![])], true),
            Err(ArkivFault::Name)
        );
    }
    let (_e, mut b, c) = setup();
    for (n, cell) in [
        ("$payload", Cell::field(Value::bytes(&[1]))),
        ("$contentType", Cell::attribute(Value::str("text/plain"))),
        ("$contentType", Cell::field(Value::str("text/plain"))),
    ] {
        let op = ArkivOperation::Create {
            salt: 1,
            flags: 0,
            expiry: Expiry {
                expires_at: 10,
                min_lifetime: 0,
            },
            attributes: vec![(name(n), cell)],
        };
        assert_eq!(op.validate(), Err(ArkivFault::Name));
        assert_eq!(run(&mut b, &c, &op), rejected(ArkivFault::Name));
    }
    applied(run(&mut b, &c, &create(0)));
    let k = key(&c, 0);
    for removed in ["$payload", "$contentType"] {
        let op = ArkivOperation::Patch {
            key: k.clone(),
            set: vec![],
            remove: vec![name(removed)],
        };
        assert_eq!(run(&mut b, &c, &op), rejected(ArkivFault::Name));
    }
    let row = b
        .try_get_record(
            entity_namespace(),
            &k,
            &Projection::ALL,
            Budget::unlimited(),
        )
        .unwrap()
        .unwrap()
        .value
        .unwrap();
    assert_eq!(row.cells.len(), SYSTEM_CELLS + 1);
    assert!(
        row.cells
            .iter()
            .all(|(n, _)| !matches!(n.as_str(), "$payload" | "$contentType"))
    );
}

#[test]
fn indexed_and_unindexed_attributes_are_attributes_and_fields() {
    let (_e, mut b, c) = setup();
    let attributes = vec![
        (name("arkiv.schema"), Cell::attribute(Value::str("posts"))),
        (name("body"), Cell::field(Value::bytes(&[1, 2, 3]))),
        (name("score"), Cell::attribute(Value::U64(42))),
        (name("title"), Cell::field(Value::str("hello"))),
    ];
    let op = ArkivOperation::Create {
        salt: 7,
        flags: 0,
        expiry: Expiry {
            expires_at: 10,
            min_lifetime: 0,
        },
        attributes: attributes.clone(),
    };
    applied(run(&mut b, &c, &op));
    let k = key(&c, 0);
    assert_eq!(read(&b, &k).attributes, attributes);
    // Only indexed cells reach the index: the unindexed str is not found.
    let count = |b: &Branch, term: Term| {
        b.try_count(
            entity_namespace(),
            &arkiv_db::Query::term(term),
            Budget::unlimited(),
        )
        .unwrap()
        .unwrap()
        .value
    };
    assert_eq!(count(&b, Term::eq(name("score"), Value::U64(42))), 1);
    assert_eq!(
        count(&b, Term::eq(name("arkiv.schema"), Value::str("posts"))),
        1
    );
    assert_eq!(count(&b, Term::eq(name("title"), Value::str("hello"))), 0);
    // A user `bytes` value on a user name, but never indexed.
    let mut indexed_bytes = op.clone();
    if let ArkivOperation::Create { attributes, .. } = &mut indexed_bytes {
        attributes[1].1 = Cell::attribute(Value::bytes(&[1, 2, 3]));
    }
    assert_eq!(indexed_bytes.validate(), Err(ArkivFault::Value));
}

#[test]
fn a_patch_writes_each_attribute_with_the_flag_of_its_new_triple() {
    let (_e, mut b, c) = setup();
    applied(run(&mut b, &c, &create(0)));
    let k = key(&c, 0);
    let indexed = |b: &Branch| {
        b.try_count(
            entity_namespace(),
            &arkiv_db::Query::term(Term::eq(name("score"), Value::I32(-1))),
            Budget::unlimited(),
        )
        .unwrap()
        .unwrap()
        .value
    };
    assert_eq!(indexed(&b), 1);
    for cell in [
        Cell::field(Value::I32(-1)),
        Cell::attribute(Value::I32(-1)),
        Cell::field(Value::I32(-1)),
    ] {
        applied(run(
            &mut b,
            &c,
            &ArkivOperation::Patch {
                key: k.clone(),
                set: vec![(name("score"), cell.clone())],
                remove: vec![],
            },
        ));
        assert_eq!(read(&b, &k).attributes, vec![(name("score"), cell.clone())]);
        assert_eq!(indexed(&b), u64::from(cell.is_attribute()));
    }
}

#[test]
fn the_entity_bytes_cap_holds_on_create_and_across_patches() {
    let (_e, mut b, c) = setup();
    let blob = |n: usize| Cell::field(Value::bytes(&vec![7; n]));
    let create_with = |attributes| ArkivOperation::Create {
        salt: 7,
        flags: 0,
        expiry: Expiry {
            expires_at: 10,
            min_lifetime: 0,
        },
        attributes,
    };
    // Two values within the per-value cap that exceed the entity cap.
    let over = create_with(vec![
        (name("a"), blob(64 * 1024)),
        (name("b"), blob(64 * 1024 + 1)),
    ]);
    assert_eq!(over.validate(), Err(ArkivFault::EntityTooLarge));
    assert_eq!(run(&mut b, &c, &over), rejected(ArkivFault::EntityTooLarge));
    // Exactly the cap is accepted.
    applied(run(
        &mut b,
        &c,
        &create_with(vec![
            (name("a"), blob(64 * 1024)),
            (name("b"), blob(64 * 1024)),
        ]),
    ));
    let k = key(&c, 0);
    let patch = |set: Vec<(CellName, Cell)>, remove: Vec<CellName>| ArkivOperation::Patch {
        key: k.clone(),
        set,
        remove,
    };
    // Each patch is small; the entity it would leave is not.
    let grow = patch(vec![(name("c"), blob(1))], vec![]);
    assert!(grow.validate().is_ok());
    assert_eq!(run(&mut b, &c, &grow), rejected(ArkivFault::EntityTooLarge));
    // Replacing a value counts only the new one.
    applied(run(
        &mut b,
        &c,
        &patch(vec![(name("a"), blob(1024))], vec![]),
    ));
    applied(run(
        &mut b,
        &c,
        &patch(vec![(name("c"), blob(63 * 1024))], vec![]),
    ));
    assert_eq!(entity_bytes(&read(&b, &k).attributes), 128 * 1024);
    assert_eq!(
        run(&mut b, &c, &patch(vec![(name("d"), blob(1))], vec![])),
        rejected(ArkivFault::EntityTooLarge)
    );
    // Removing frees room in the same patch.
    applied(run(
        &mut b,
        &c,
        &patch(vec![(name("d"), blob(64 * 1024))], vec![name("b")]),
    ));
    assert_eq!(entity_bytes(&read(&b, &k).attributes), 128 * 1024);
    // Non-bytes values do not count.
    applied(run(
        &mut b,
        &c,
        &patch(
            vec![(name("note"), Cell::field(Value::str(&"x".repeat(128))))],
            vec![],
        ),
    ));
}

#[test]
fn system_cells_leave_room_for_every_user_attribute() {
    let engine = Engine::with_limits(
        PriceSchedule::FIXTURE,
        Limits {
            max_cells: MIN_ENTITY_CELLS,
            ..Limits::DEFAULT
        },
    );
    let mut b = engine.begin(engine.head(), None).unwrap();
    b.try_execute(
        OwnerId::new([1; 20]),
        WriteOp::CreateNamespace {
            name: NamespaceName::new("entities").unwrap(),
        },
        Budget::unlimited(),
    )
    .unwrap()
    .unwrap();
    let (_e, _b, c) = setup();
    let attributes = |n: usize| {
        (0..n)
            .map(|i| {
                (
                    name(&format!("attr{i:02}")),
                    Cell::attribute(Value::U64(i as u64)),
                )
            })
            .collect::<Vec<_>>()
    };
    let create_with = |attributes| ArkivOperation::Create {
        salt: 7,
        flags: 0,
        expiry: Expiry {
            expires_at: 10,
            min_lifetime: 0,
        },
        attributes,
    };
    applied(run(&mut b, &c, &create_with(attributes(MAX_ATTRIBUTES))));
    assert_eq!(read(&b, &key(&c, 0)).attributes.len(), MAX_ATTRIBUTES);
    assert_eq!(
        create_with(attributes(MAX_ATTRIBUTES + 1)).validate(),
        Err(ArkivFault::Value)
    );
}

#[test]
fn natural_storage_is_not_abi_or_signed_order_encoding() {
    let negative = Value::I32(-1);
    assert_eq!(encode_natural_value(&negative).unwrap(), vec![255; 4]);
    assert_eq!(encode_abi_value(&negative).unwrap(), vec![255; 32]);
    assert_eq!(negative.term_bytes().unwrap(), vec![127, 255, 255, 255]);
    assert_eq!(decode_natural_value(2, &[255; 4]).unwrap(), negative);
    assert!(decode_natural_value(2, &[255; 32]).is_err());
    assert!(decode_abi_value(2, &[255; 4]).is_err());
    assert!(decode_natural_value(1, &[2]).is_err());
}

#[test]
fn authenticated_engine_rows_with_forged_system_types_fail_arkiv_verification() {
    use arkiv_db::proof::{EqQuery, EqRequest, ProofLimits, SnapshotRef, VerifiedEq, verify_eq};
    for (field, cell) in [
        ("$owner", Cell::attribute(Value::Bytes32([2; 32]))),
        ("$owner", Cell::field(Value::Addr([2; 20]))),
        ("$key", Cell::attribute(Value::Key([0; 32]))),
        ("$creationFlags", Cell::field(Value::U64(4))),
    ] {
        let (mut engine, mut branch, c) = setup();
        applied(run(&mut branch, &c, &create(0)));
        let k = key(&c, 0);
        // Deliberately use the generic privileged backend to construct a valid
        // Merkle commitment to a state that Arkiv execution could never create.
        branch
            .try_execute_host(
                WriteOp::PatchRecord {
                    namespace: entity_namespace(),
                    key: k,
                    set: vec![(name(field), cell)],
                    remove: vec![],
                },
                Budget::unlimited(),
            )
            .unwrap()
            .unwrap();
        let sealed = branch.try_seal().unwrap();
        let root = sealed.state_root();
        let id = engine.publish(&sealed).unwrap();
        let snapshot = engine.snapshot(id).unwrap();
        let request = EqRequest {
            snapshot: SnapshotRef {
                genesis_hash: arkiv_db::B256::ZERO,
                height: 1,
                block_hash: arkiv_db::B256::ZERO,
                state_root: root,
            },
            query: EqQuery {
                namespace: entity_namespace(),
                attribute: name("score"),
                value: Value::I32(-1),
            },
            limit: 1,
            continuation: None,
        };
        let caps = ProofLimits::default();
        let proof = snapshot
            .prove_eq(&request, caps, Budget::unlimited())
            .unwrap();
        let VerifiedEq::Page(page) =
            verify_eq(&request, None, &proof.bytes, caps, engine.limits()).unwrap()
        else {
            panic!("namespace present")
        };
        assert_eq!(page.rows.len(), 1);
        assert_eq!(
            validate_proven_row(&page.rows[0]),
            Err(ArkivFault::Corrupt),
            "{field}"
        );
    }
}

#[test]
fn generic_namespaces_without_glm_cannot_receive_arkiv_entities() {
    let (_engine, mut branch, context) = setup();
    let namespace = NamespaceId::from_bytes(2u64.to_be_bytes()).unwrap();
    branch
        .try_execute(
            OwnerId::new(context.caller),
            WriteOp::CreateNamespace {
                name: NamespaceName::new("bricks").unwrap(),
            },
            Budget::unlimited(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        execute_operation(
            &mut branch,
            namespace,
            &context,
            &create(0),
            Budget::unlimited()
        )
        .unwrap(),
        rejected(ArkivFault::Namespace)
    );
    let spec = crate::ChainSpec::new_unchecked(crate::demo::workload::genesis(1337, 0));
    assert!(
        crate::validate::check_op(
            &spec,
            &crate::TxOp::Arkiv {
                namespace,
                operation: create(0)
            }
        )
        .is_err()
    );
}

/// A GLM-priced branch holding one permanent entity created the way a
/// permanent-prefill genesis creates it (height 0, `allow_permanent_create`),
/// plus the live context of block 5.
fn glm_permanent() -> (Engine, Branch, ArkivExecutionContext, RecordKey) {
    let engine = Engine::with_limits(crate::gas::schedule(), Limits::DEFAULT);
    let mut branch = engine.begin(engine.head(), None).unwrap();
    branch
        .try_execute(
            OwnerId::new([1; 20]),
            WriteOp::CreateNamespace {
                name: NamespaceName::new("entities").unwrap(),
            },
            Budget::unlimited(),
        )
        .unwrap()
        .unwrap();
    let mut genesis = ArkivExecutionContext {
        caller: [1; 20],
        block_height: 0,
        chain_id: 1337,
        max_lifetime: 1000,
        expiry_per_block: 1,
        allow_permanent_create: true,
    };
    let mut op = create(2);
    if let ArkivOperation::Create { expiry, .. } = &mut op {
        *expiry = Expiry::PERMANENT;
    }
    let applied_create = run(&mut branch, &genesis, &op);
    let OpOutcome::Applied { receipt, .. } = &applied_create else {
        panic!("{applied_create:?}")
    };
    // The genesis exemption removes rent only; execution is still metered.
    assert!(receipt.spent.units() > 0);
    let key = key(&genesis, 0);
    assert_eq!(read(&branch, &key).expires_at, u64::MAX);
    // The flag only waives rent for creations: a second, finite creation
    // pays and schedules exactly as without it.
    let finite = run(&mut branch, &genesis, &create(0));
    assert!(matches!(finite, OpOutcome::Applied { .. }), "{finite:?}");
    assert_eq!(count(&branch, &crate::exec::total_key(10)).unwrap(), 1);
    assert_eq!(
        count(&branch, &crate::exec::total_key(u64::MAX)).unwrap(),
        0
    );
    assert_eq!(
        count(&branch, &crate::exec::due_key(u64::MAX, entity_namespace())).unwrap(),
        0
    );
    genesis.block_height = 5;
    genesis.allow_permanent_create = false;
    (engine, branch, genesis, key)
}

#[test]
fn glm_rejects_live_permanence_and_updates_of_permanent_entities() {
    let (_engine, mut b, mut live, k) = glm_permanent();
    let mut permanent = create(0);
    if let ArkivOperation::Create { expiry, .. } = &mut permanent {
        *expiry = Expiry::PERMANENT;
    }
    // A live creation is never permanent under GLM rent.
    assert_eq!(
        run(&mut b, &live, &permanent),
        rejected(ArkivFault::PermanentStorageUnsupported)
    );
    // Updates that would keep it permanent need unbounded rent: rejected.
    for op in [
        ArkivOperation::Patch {
            key: k.clone(),
            set: vec![(name("score"), Cell::attribute(Value::I32(7)))],
            remove: vec![],
        },
        ArkivOperation::Patch {
            key: k.clone(),
            set: vec![],
            remove: vec![name("score")],
        },
        ArkivOperation::TransferOwnership {
            key: k.clone(),
            owner: [2; 20],
        },
        ArkivOperation::ExtendExpiry {
            key: k.clone(),
            expiry: Expiry::PERMANENT,
        },
    ] {
        assert_eq!(
            run(&mut b, &live, &op),
            rejected(ArkivFault::PermanentStorageUnsupported),
            "{op:?}"
        );
    }
    // A permanent entity is never shortened to a finite lifetime, including
    // by a permissionless extender (creation flag 2).
    for caller in [[1; 20], [3; 20]] {
        live.caller = caller;
        assert_eq!(
            run(
                &mut b,
                &live,
                &ArkivOperation::ExtendExpiry {
                    key: k.clone(),
                    expiry: Expiry {
                        expires_at: 10,
                        min_lifetime: 0,
                    },
                },
            ),
            OpOutcome::HostRejected {
                fault: HostFault::ExpiryShortened
            }
        );
    }
    live.caller = [1; 20];
    let p = read(&b, &k);
    assert_eq!(
        (p.expires_at, p.owner, p.updated_at),
        (u64::MAX, [1; 20], 0)
    );
    // The rejected updates left the schedule as genesis wrote it: only the
    // finite row is due (at 10); nothing is ever due at u64::MAX.
    let schedule = |b: &Branch| {
        [
            crate::exec::total_key(10),
            crate::exec::due_key(10, entity_namespace()),
            crate::exec::total_key(u64::MAX),
            crate::exec::due_key(u64::MAX, entity_namespace()),
        ]
        .map(|key| count(b, &key).unwrap())
    };
    assert_eq!(schedule(&b), [1, 1, 0, 0]);
    // The owner can still delete it; no expiry count is touched (a decrement
    // of an absent count would fail as corrupt state).
    applied(run(
        &mut b,
        &live,
        &ArkivOperation::Delete { key: k.clone() },
    ));
    assert!(
        b.try_get_record(
            entity_namespace(),
            &k,
            &Projection::ALL,
            Budget::unlimited()
        )
        .unwrap()
        .unwrap()
        .value
        .is_none()
    );
    assert_eq!(schedule(&b), [1, 1, 0, 0]);
}
