use super::*;
use arkiv_db::{Engine, Limits, NamespaceName, OwnerId, PriceSchedule};
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
        attributes: vec![(name("score"), Value::I32(-1))],
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
    assert_eq!(original.content_type, "");
    assert!(original.payload.is_empty());
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
        attributes.push((name("$owner"), Value::Addr([2; 20])))
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
fn system_tombstones_restore_empty_and_scalar_types_stay_distinct() {
    let (_e, mut b, c) = setup();
    applied(run(&mut b, &c, &create(0)));
    let k = key(&c, 0);
    applied(run(
        &mut b,
        &c,
        &ArkivOperation::Patch {
            key: k.clone(),
            set: vec![
                (name("$payload"), Value::bytes(&[1, 2])),
                (name("$contentType"), Value::str("text/plain")),
                (name("ref"), Value::Key([99; 32])),
            ],
            remove: vec![],
        },
    ));
    applied(run(
        &mut b,
        &c,
        &ArkivOperation::Patch {
            key: k.clone(),
            set: vec![],
            remove: vec![name("$payload"), name("$contentType")],
        },
    ));
    let p = read(&b, &k);
    assert!(p.payload.is_empty());
    assert_eq!(p.content_type, "");
    assert!(
        p.attributes
            .iter()
            .any(|(n, v)| n.as_str() == "ref" && *v == Value::Key([99; 32]))
    );
    assert!(ArkivTypeId::of(&Value::I64(1)).is_err());
    assert_ne!(
        ArkivTypeId::of(&Value::Bytes32([0; 32])),
        ArkivTypeId::of(&Value::Key([0; 32]))
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
