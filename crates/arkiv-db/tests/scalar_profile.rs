//! The single engine profile: hard caps, scalar encodings, and adversarial proofs.
use arkiv_db::{CellName, NamespaceName, TypeTag, Value, decode_exact, encode};

#[test]
fn profile_limits_are_explicit_and_namespaces_stay_independent() {
    assert_eq!(arkiv_db::LAYOUT_VERSION, 1);
    assert_eq!(arkiv_db::CODEC_VERSION, arkiv_db::LAYOUT_VERSION);
    assert_eq!(arkiv_db::state::STATE_FORMAT, arkiv_db::LAYOUT_VERSION);
    assert_eq!(arkiv_db::proof::PROOF_VERSION, arkiv_db::LAYOUT_VERSION);
    assert_eq!(CellName::MAX_LEN, 32);
    assert_eq!(Value::MAX_INDEXED_LEN, 128);
    for len in [0, 20, 21, 32, 33] {
        assert_eq!(
            CellName::new(&"a".repeat(len)).is_ok(),
            len > 0 && len <= CellName::MAX_LEN
        );
        assert_eq!(
            NamespaceName::new(&"a".repeat(len)).is_ok(),
            len > 0 && len <= 20
        );
    }
}

fn values() -> Vec<Value> {
    vec![
        Value::I32(i32::MIN),
        Value::I32(-1),
        Value::I32(0),
        Value::I32(i32::MAX),
        Value::U256([255; 32]),
        Value::Dec([255; 32]),
        Value::Bytes32([255; 32]),
        Value::Addr([255; 20]),
        Value::Key([255; 32]),
        Value::Bytes4([255; 4]),
        Value::Bytes8([255; 8]),
        Value::Bytes16([255; 16]),
        Value::U32(u32::MAX),
        Value::U128(u128::MAX),
        Value::I128(i128::MIN),
        Value::I256([255; 32]),
        Value::Dec32(-1),
        Value::Dec64(i64::MAX),
        Value::Dec128(-1),
        Value::f32(-0.5).unwrap(),
        Value::f64(f64::INFINITY).unwrap(),
        Value::Date32(-1),
        Value::Timestamp64(i64::MIN),
    ]
}

#[test]
fn scalar_widths_and_tags_round_trip() {
    for v in values() {
        let wire = encode(&v);
        assert_eq!(wire.len(), 1 + v.encoded_len());
        assert_eq!(decode_exact::<TypeTag>(&[v.tag().id()]).unwrap(), v.tag());
        assert_eq!(decode_exact::<Value>(&wire).unwrap(), v);
        assert_eq!(
            Value::from_term_bytes(v.tag(), &v.term_bytes().unwrap()),
            Some(v.clone())
        );
        for n in 0..wire.len() {
            assert!(decode_exact::<Value>(&wire[..n]).is_err());
        }
        let mut long = wire.clone();
        long.push(0);
        assert!(decode_exact::<Value>(&long).is_err());
        assert!(Value::from_term_bytes(v.tag(), &vec![0; v.encoded_len() - 1]).is_none());
        assert!(Value::from_term_bytes(v.tag(), &vec![0; v.encoded_len() + 1]).is_none());
    }
    // `type id ‖ term bytes`: i32 is golem id 16, stored sign-flipped.
    assert_eq!(encode(&Value::I32(-1)), vec![16, 127, 255, 255, 255]);
    assert_eq!(
        Value::I32(-1).term_bytes().unwrap(),
        vec![127, 255, 255, 255]
    );
    let mut tags: Vec<_> = values().iter().map(|v| v.tag().id()).collect();
    tags.sort_unstable();
    tags.dedup();
    assert_eq!(
        tags,
        [4, 8, 9, 10, 11, 12, 14, 15, 16, 18, 19, 20, 21, 22, 23, 24, 25, 28, 29, 64]
    );
    assert_ne!(
        encode(&Value::U256([0; 32])),
        encode(&Value::Bytes32([0; 32]))
    );
    assert_ne!(encode(&Value::Dec([0; 32])), encode(&Value::Key([0; 32])));
}

#[test]
fn signed_and_unsigned_order_vectors() {
    let signed = [i32::MIN, -100, -1, 0, 1, 100, i32::MAX];
    for pair in signed.windows(2) {
        assert!(Value::I32(pair[0]).term_bytes() < Value::I32(pair[1]).term_bytes());
    }
    let mut min = [0; 32];
    min[0] = 128;
    let mut max = [255; 32];
    max[0] = 127;
    let mut one = [0; 32];
    one[31] = 1;
    let signed = [min, [255; 32], [0; 32], one, max];
    for pair in signed.windows(2) {
        assert!(Value::Dec(pair[0]).term_bytes() < Value::Dec(pair[1]).term_bytes());
        assert_eq!(
            Value::Dec(pair[0]).cmp_ordered(&Value::Dec(pair[1])),
            Some(std::cmp::Ordering::Less)
        );
    }
    for pair in [[0; 32], one, max, min, [255; 32]].windows(2) {
        assert!(Value::U256(pair[0]).term_bytes() < Value::U256(pair[1]).term_bytes());
    }
}

#[test]
fn all_scalars_prove_equality_and_reject_typed_substitution() {
    use arkiv_db::proof::{EqQuery, EqRequest, ProofLimits, SnapshotRef, verify_eq};
    use arkiv_db::{
        B256, Budget, Cell, Engine, Limits, NamespaceId, OwnerId, PriceSchedule, RecordKey, WriteOp,
    };
    let owner = OwnerId::new([1; 20]);
    let ns = NamespaceId::from_ordinal(1).unwrap();
    let mut engine = Engine::new(PriceSchedule::FIXTURE);
    let mut branch = engine.begin(engine.head(), None).unwrap();
    branch
        .execute(
            owner,
            WriteOp::CreateNamespace {
                name: NamespaceName::new("scalars").unwrap(),
            },
            Budget::unlimited(),
        )
        .unwrap();
    let mut all = values();
    all.extend([
        Value::Bool(true),
        Value::U64(u64::MAX),
        Value::I64(i64::MIN),
        Value::str(&"é".repeat(Value::MAX_INDEXED_LEN / 2)),
    ]);
    let attr = CellName::new(&"a".repeat(CellName::MAX_LEN)).unwrap();
    for (n, value) in all.iter().enumerate() {
        branch
            .execute(
                owner,
                WriteOp::CreateRecord {
                    namespace: ns,
                    key: RecordKey::new(&[n as u8]).unwrap(),
                    cells: vec![(attr.clone(), Cell::attribute(value.clone()))],
                },
                Budget::unlimited(),
            )
            .unwrap();
    }
    engine.publish(&branch.seal()).unwrap();
    let snapshot = engine.head_snapshot();
    for value in all {
        let request = EqRequest {
            snapshot: SnapshotRef {
                genesis_hash: B256::ZERO,
                height: 1,
                block_hash: B256::ZERO,
                state_root: snapshot.state_root(),
            },
            query: EqQuery {
                namespace: ns,
                attribute: attr.clone(),
                value,
            },
            limit: 20,
            continuation: None,
        };
        let proof = snapshot
            .prove_eq(&request, ProofLimits::default(), Budget::unlimited())
            .unwrap()
            .bytes;
        let arkiv_db::proof::VerifiedEq::Page(verified) = verify_eq(
            &request,
            None,
            &proof,
            ProofLimits::default(),
            Limits::DEFAULT,
        )
        .unwrap() else {
            panic!("expected page")
        };
        assert_eq!(verified.rows.len(), 1);
        let mut substituted = request.clone();
        substituted.query.value = Value::Addr([0; 20]);
        assert!(
            verify_eq(
                &substituted,
                None,
                &proof,
                ProofLimits::default(),
                Limits::DEFAULT
            )
            .is_err()
        );
        for n in [0, 4, proof.len() / 2, proof.len() - 1] {
            assert!(
                verify_eq(
                    &request,
                    None,
                    &proof[..n],
                    ProofLimits::default(),
                    Limits::DEFAULT
                )
                .is_err()
            );
        }
        let mut old = proof.clone();
        old[..4].copy_from_slice(&(arkiv_db::proof::PROOF_VERSION ^ 3).to_be_bytes());
        assert!(
            verify_eq(
                &request,
                None,
                &old,
                ProofLimits::default(),
                Limits::DEFAULT
            )
            .is_err()
        );
    }
}

#[test]
fn foreign_genesis_magic_and_state_format_are_rejected() {
    use arkiv_db::{PriceSchedule, StateHeader, durable::EngineGenesis};
    let genesis = EngineGenesis::new([3; 32], PriceSchedule::FIXTURE);
    let mut wire = encode(&genesis);
    wire[7] = b'2';
    assert!(decode_exact::<EngineGenesis>(&wire).is_err());
    let mut state = encode(&genesis.initial_header());
    state[..4].copy_from_slice(&2u32.to_be_bytes());
    assert!(decode_exact::<StateHeader>(&state).is_err());
}

#[test]
fn scalar_and_string_admission_obeys_the_caps() {
    use arkiv_db::{Budget, Cell, Engine, NamespaceId, OwnerId, PriceSchedule, RecordKey, WriteOp};
    let mut engine = Engine::new(PriceSchedule::FIXTURE);
    let mut branch = engine.begin(engine.head(), None).unwrap();
    let owner = OwnerId::new([1; 20]);
    branch
        .execute(
            owner,
            WriteOp::CreateNamespace {
                name: NamespaceName::new("bounds").unwrap(),
            },
            Budget::unlimited(),
        )
        .unwrap();
    let ns = NamespaceId::from_ordinal(1).unwrap();
    let mut ordinal = 0u8;
    for (value, accepted) in [
        (Value::str(&"a".repeat(Value::MAX_INDEXED_LEN - 1)), true),
        (Value::str(&"a".repeat(Value::MAX_INDEXED_LEN)), true),
        (Value::str(&"a".repeat(Value::MAX_INDEXED_LEN + 1)), false),
        (Value::str(&"é".repeat(Value::MAX_INDEXED_LEN / 2)), true),
        (
            Value::str(&format!("{}x", "é".repeat(Value::MAX_INDEXED_LEN / 2))),
            false,
        ),
        (Value::I32(1), true),
        (Value::Bytes32([1; 32]), true),
        (Value::Bytes16([1; 16]), true),
        (Value::f64(1.0).unwrap(), true),
        (Value::Timestamp64(1), true),
        (Value::bytes(&[1]), false),
    ] {
        ordinal += 1;
        let result = branch.execute(
            owner,
            WriteOp::CreateRecord {
                namespace: ns,
                key: RecordKey::new(&[ordinal]).unwrap(),
                cells: vec![(CellName::new("v").unwrap(), Cell::attribute(value))],
            },
            Budget::unlimited(),
        );
        assert_eq!(result.is_ok(), accepted);
    }
    engine.publish(&branch.seal()).unwrap();
}

#[test]
fn genesis_and_query_have_literal_profile_vectors() {
    use arkiv_db::{NamespaceId, PriceSchedule, durable::EngineGenesis, proof::EqQuery};
    let genesis = EngineGenesis::new([3; 32], PriceSchedule::FIXTURE);
    let wire = encode(&genesis);
    let mut prefix = b"ARKENG01".to_vec();
    for version in [1u32, 1, 1, 3, 2, 1] {
        prefix.extend(version.to_be_bytes());
    }
    assert_eq!(&wire[..32], &prefix);
    assert_eq!(decode_exact::<EngineGenesis>(&wire).unwrap(), genesis);
    let query = EqQuery {
        namespace: NamespaceId::from_ordinal(1).unwrap(),
        attribute: CellName::new("a").unwrap(),
        value: Value::I32(-2),
    };
    let mut expected = vec![
        0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 1, b'a', 16, 127, 255, 255, 254,
    ];
    // Projection flags bind both the structural key and all committed cells.
    expected.extend([1, 1]);
    assert_eq!(encode(&query), expected);
    assert_eq!(decode_exact::<EqQuery>(&expected).unwrap(), query);
    expected[3] = 2;
    assert!(decode_exact::<EqQuery>(&expected).is_err());
}
