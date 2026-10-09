//! golem-db cell type grid conformance: every golem scalar type has an
//! engine type whose term bytes equal golem's stored value bytes, values
//! round-trip as attributes and fields, ordered types answer range queries
//! in value order, and an empty patch is golem's no-op.
//!
//! Runs under both profiles: every type id is admitted by both.

use std::ops::Bound::{Excluded, Included, Unbounded};

use arkiv_db::{
    Budget, Cell, CellName, Effect, Engine, Float32, Float64, NamespaceId, NamespaceName, OwnerId,
    PriceSchedule, Projection, Query, RecordKey, Rejection, Term, TypeTag, Value, WriteOp,
    decode_exact, encode,
};

const OWNER: OwnerId = OwnerId::new([1; 20]);

fn name(text: &str) -> CellName {
    CellName::new(text).unwrap()
}

fn i256(value: i128) -> [u8; 32] {
    let mut out = if value < 0 { [0xff; 32] } else { [0; 32] };
    out[16..].copy_from_slice(&value.to_be_bytes());
    out
}

fn u256(value: u128) -> [u8; 32] {
    let mut out = [0; 32];
    out[16..].copy_from_slice(&value.to_be_bytes());
    out
}

fn f32v(value: f32) -> Value {
    Value::f32(value).unwrap()
}

fn f64v(value: f64) -> Value {
    Value::f64(value).unwrap()
}

/// Strictly increasing samples of every ordered type.
fn ordered_samples() -> Vec<Vec<Value>> {
    let mut i256_min = [0; 32];
    i256_min[0] = 0x80;
    let mut i256_max = [0xff; 32];
    i256_max[0] = 0x7f;
    vec![
        [i32::MIN, -1, 0, 1, i32::MAX].map(Value::I32).to_vec(),
        [i64::MIN, -1, 0, 1, i64::MAX].map(Value::I64).to_vec(),
        [i128::MIN, -1, 0, 1, i128::MAX].map(Value::I128).to_vec(),
        [i256_min, i256(-2), i256(-1), i256(0), i256(1), i256_max]
            .map(Value::I256)
            .to_vec(),
        [0, 1, 255, 256, u32::MAX].map(Value::U32).to_vec(),
        [0, 1, 255, 256, u64::MAX].map(Value::U64).to_vec(),
        [0, 1, 255, 256, u128::MAX].map(Value::U128).to_vec(),
        [u256(0), u256(1), u256(256), [0xff; 32]]
            .map(Value::U256)
            .to_vec(),
        [i32::MIN, -10_000, 0, 1, i32::MAX]
            .map(Value::Dec32)
            .to_vec(),
        [i64::MIN, -1_000_000, 0, 1, i64::MAX]
            .map(Value::Dec64)
            .to_vec(),
        [i128::MIN, -1, 0, 1, i128::MAX].map(Value::Dec128).to_vec(),
        [i256_min, i256(-1), i256(0), i256(1), i256_max]
            .map(Value::Dec)
            .to_vec(),
        [
            f32::NEG_INFINITY,
            f32::MIN,
            -2.0,
            -1.0,
            -f32::MIN_POSITIVE,
            0.0,
        ]
        .into_iter()
        .chain([f32::from_bits(1), 1.0, 2.0, f32::MAX, f32::INFINITY])
        .map(f32v)
        .collect(),
        [
            f64::NEG_INFINITY,
            f64::MIN,
            -2.0,
            -1.0,
            -f64::MIN_POSITIVE,
            0.0,
        ]
        .into_iter()
        .chain([f64::from_bits(1), 1.0, 2.0, f64::MAX, f64::INFINITY])
        .map(f64v)
        .collect(),
        [i32::MIN, -1, 0, 19_000, i32::MAX]
            .map(Value::Date32)
            .to_vec(),
        [i64::MIN, -1, 0, 1_700_000_000_000_000, i64::MAX]
            .map(Value::Timestamp64)
            .to_vec(),
    ]
}

/// One value of every type, for round trips.
fn one_of_each() -> Vec<Value> {
    vec![
        Value::Bool(true),
        Value::str("golem"),
        Value::bytes(&[0, 1, 2]),
        Value::Addr([0x20; 20]),
        Value::Bytes4([4; 4]),
        Value::Bytes8([8; 8]),
        Value::Bytes16([16; 16]),
        Value::Bytes32([32; 32]),
        Value::Key([0x4b; 32]),
        Value::U32(u32::MAX - 1),
        Value::U64(u64::MAX - 1),
        Value::U128(u128::MAX - 1),
        Value::U256(u256(7)),
        Value::I32(-32),
        Value::I64(-64),
        Value::I128(-128),
        Value::I256(i256(-256)),
        Value::Dec32(-123_450),
        Value::Dec64(1_500_000),
        Value::Dec128(-1),
        Value::Dec(i256(-12_345_678_901_234_567_890)),
        f32v(-1.5),
        f64v(f64::NEG_INFINITY),
        Value::Date32(-719_162),
        Value::Timestamp64(-1),
    ]
}

#[test]
fn every_type_id_is_admitted_and_named() {
    assert_eq!(TypeTag::ALL.len(), 25);
    for (index, tag) in TypeTag::ALL.into_iter().enumerate() {
        assert_eq!(usize::from(tag.id()), index + 1);
        assert!(tag.is_supported());
        assert_eq!(TypeTag::from_id(tag.id()), Some(tag));
        assert_eq!(TypeTag::from_name(tag.name()), Some(tag));
        assert_eq!(tag.is_indexable(), tag != TypeTag::Bytes);
    }
    assert_eq!(TypeTag::from_id(0), None);
    assert_eq!(TypeTag::from_id(26), None);
    let mut seen: Vec<_> = one_of_each().iter().map(Value::tag).collect();
    seen.sort();
    assert_eq!(seen, TypeTag::ALL.to_vec());
}

#[test]
fn ordered_types_sort_by_term_bytes_and_cmp_ordered() {
    for samples in ordered_samples() {
        let tag = samples[0].tag();
        assert!(tag.is_ordered(), "{tag}");
        for pair in samples.windows(2) {
            let (low, high) = (&pair[0], &pair[1]);
            assert!(
                low.term_bytes().unwrap() < high.term_bytes().unwrap(),
                "{low:?} < {high:?}"
            );
            assert_eq!(low.cmp_ordered(high), Some(std::cmp::Ordering::Less));
            assert_eq!(high.cmp_ordered(low), Some(std::cmp::Ordering::Greater));
        }
        for value in &samples {
            let term = value.term_bytes().unwrap();
            assert_eq!(term.len(), value.encoded_len());
            assert_eq!(Value::from_term_bytes(tag, &term).as_ref(), Some(value));
            assert_eq!(decode_exact::<Value>(&encode(value)).as_ref(), Ok(value));
            assert_eq!(value.cmp_ordered(value), Some(std::cmp::Ordering::Equal));
        }
    }
    for tag in [
        TypeTag::Bool,
        TypeTag::Str,
        TypeTag::Bytes,
        TypeTag::Addr,
        TypeTag::Bytes4,
        TypeTag::Bytes8,
        TypeTag::Bytes16,
        TypeTag::Bytes32,
        TypeTag::Key,
    ] {
        assert!(!tag.is_ordered(), "{tag}");
    }
    assert_eq!(Value::U32(1).cmp_ordered(&Value::U64(1)), None);
    assert_eq!(Value::Dec32(1).cmp_ordered(&Value::I32(1)), None);
}

/// Term bytes are golem-db's stored value bytes (golem `order.rs` vectors).
#[test]
fn term_bytes_are_golem_stored_forms() {
    let cases: Vec<(Value, Vec<u8>)> = vec![
        (Value::I32(i32::MIN), vec![0, 0, 0, 0]),
        (Value::I32(-1), vec![0x7f, 0xff, 0xff, 0xff]),
        (Value::I32(0), vec![0x80, 0, 0, 0]),
        (Value::I32(i32::MAX), vec![0xff; 4]),
        (f32v(-2.0), vec![0x3f, 0xff, 0xff, 0xff]),
        (f32v(-1.0), vec![0x40, 0x7f, 0xff, 0xff]),
        (f32v(0.0), vec![0x80, 0, 0, 0]),
        (f32v(-0.0), vec![0x80, 0, 0, 0]),
        (f32v(1.0), vec![0xbf, 0x80, 0, 0]),
        (f64v(1.0), vec![0xbf, 0xf0, 0, 0, 0, 0, 0, 0]),
        (Value::U32(258), vec![0, 0, 1, 2]),
        (Value::Dec32(-1), vec![0x7f, 0xff, 0xff, 0xff]),
        (Value::Date32(0), vec![0x80, 0, 0, 0]),
        (
            Value::Timestamp64(-2),
            vec![0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xfe],
        ),
        (Value::Bytes4([1, 2, 3, 4]), vec![1, 2, 3, 4]),
        (Value::I128(0), [&[0x80][..], &[0; 15]].concat()),
        (Value::I256(i256(-1)), [&[0x7f][..], &[0xff; 31]].concat()),
    ];
    for (value, term) in cases {
        assert_eq!(value.term_bytes(), Some(term), "{value:?}");
    }
    // NaN and negative zero have no term form: their encodings never decode.
    // Positive NaN 7fc00000 stores as ffc00000; negative NaN ffc00000 as
    // 003fffff; negative zero 80000000 as 7fffffff.
    for order in [0xffc0_0000u32, 0x003f_ffff, 0x7fff_ffff] {
        assert_eq!(
            Value::from_term_bytes(TypeTag::F32, &order.to_be_bytes()),
            None,
            "{order:08x}"
        );
    }
    for order in [
        0xfff8_0000_0000_0000u64,
        0x0007_ffff_ffff_ffff,
        0x7fff_ffff_ffff_ffff,
    ] {
        assert_eq!(
            Value::from_term_bytes(TypeTag::F64, &order.to_be_bytes()),
            None,
            "{order:016x}"
        );
    }
    assert_eq!(Float32::from_bits((-0.0f32).to_bits()), None);
    assert_eq!(Float64::from_bits((-0.0f64).to_bits()), None);
    assert_eq!(Float64::from_bits(f64::NAN.to_bits()), None);
    assert_eq!(Float64::new(f64::NAN), None);
    assert_eq!(Float64::new(-0.0).unwrap().to_bits(), 0);
}

fn engine_with_namespace() -> (Engine, arkiv_db::Branch, NamespaceId) {
    let engine = Engine::new(PriceSchedule::FIXTURE);
    let mut branch = engine.begin(engine.head(), None).unwrap();
    let executed = branch
        .execute(
            OWNER,
            WriteOp::CreateNamespace {
                name: NamespaceName::new("golem").unwrap(),
            },
            Budget::unlimited(),
        )
        .unwrap();
    let Effect::NamespaceCreated(namespace) = executed.effect else {
        panic!("namespace effect");
    };
    (engine, branch, namespace)
}

fn create(
    branch: &mut arkiv_db::Branch,
    ns: NamespaceId,
    key: &[u8],
    cells: Vec<(CellName, Cell)>,
) {
    branch
        .execute(
            OWNER,
            WriteOp::CreateRecord {
                namespace: ns,
                key: RecordKey::new(key).unwrap(),
                cells,
            },
            Budget::unlimited(),
        )
        .unwrap();
}

fn matches(branch: &arkiv_db::Branch, ns: NamespaceId, query: &Query) -> Vec<Vec<u8>> {
    let page = branch
        .query(ns, query, &Projection::ALL, 200, None, Budget::unlimited())
        .unwrap();
    page.value
        .rows
        .into_iter()
        .map(|row| row.key.unwrap().as_bytes().to_vec())
        .collect()
}

#[test]
fn every_type_round_trips_as_attribute_and_field_and_matches_equality() {
    let (mut engine, mut branch, ns) = engine_with_namespace();
    let values = one_of_each();
    for (index, value) in values.iter().enumerate() {
        let attribute = if value.tag().is_indexable() {
            Cell::attribute(value.clone())
        } else {
            Cell::field(value.clone())
        };
        create(
            &mut branch,
            ns,
            &[index as u8],
            vec![
                (name("a"), attribute),
                (name("f"), Cell::field(value.clone())),
            ],
        );
    }
    engine.publish(&branch.seal()).unwrap();
    let snapshot = engine.head_snapshot();
    let branch = engine.begin(engine.head(), None).unwrap();
    for (index, value) in values.iter().enumerate() {
        let key = RecordKey::new(&[index as u8]).unwrap();
        let row = snapshot
            .get_record(ns, &key, &Projection::ALL, Budget::unlimited())
            .unwrap()
            .value
            .expect("record exists");
        let cells: Vec<_> = row
            .cells
            .iter()
            .map(|(_, cell)| cell.value.clone())
            .collect();
        assert_eq!(cells, vec![value.clone(), value.clone()], "{value:?}");
        if !value.tag().is_indexable() {
            continue;
        }
        assert_eq!(
            matches(
                &branch,
                ns,
                &Query::term(Term::eq(name("a"), value.clone()))
            ),
            vec![vec![index as u8]],
            "{value:?}"
        );
        // Fields are never indexed.
        assert!(
            matches(
                &branch,
                ns,
                &Query::term(Term::eq(name("f"), value.clone()))
            )
            .is_empty()
        );
        assert_eq!(
            matches(
                &branch,
                ns,
                &Query::term(Term::exists(name("a"), value.tag()))
            ),
            vec![vec![index as u8]],
            "{value:?}"
        );
    }
}

#[test]
fn ordered_types_answer_range_queries_in_value_order() {
    let (_engine, mut branch, ns) = engine_with_namespace();
    let samples = ordered_samples();
    let mut ordinal = 0u16;
    let mut keys = Vec::new();
    for values in &samples {
        let mut type_keys = Vec::new();
        for value in values {
            ordinal += 1;
            let key = ordinal.to_be_bytes().to_vec();
            create(
                &mut branch,
                ns,
                &key,
                vec![(name("v"), Cell::attribute(value.clone()))],
            );
            type_keys.push(key);
        }
        keys.push(type_keys);
    }
    for (values, type_keys) in samples.iter().zip(&keys) {
        let n = values.len();
        let mid = n / 2;
        // [mid, max]: exactly the upper half, nothing of other types.
        assert_eq!(
            matches(
                &branch,
                ns,
                &Query::term(Term::range(
                    name("v"),
                    Included(values[mid].clone()),
                    Unbounded
                ))
            ),
            type_keys[mid..].to_vec(),
            "{:?}",
            values[mid]
        );
        // (min, mid): strictly between.
        assert_eq!(
            matches(
                &branch,
                ns,
                &Query::term(Term::range(
                    name("v"),
                    Excluded(values[0].clone()),
                    Excluded(values[mid].clone())
                ))
            ),
            type_keys[1..mid].to_vec(),
            "{:?}",
            values[mid]
        );
    }
}

#[test]
fn empty_patch_is_a_noop_on_an_existing_record_and_not_found_otherwise() {
    let (_engine, mut branch, ns) = engine_with_namespace();
    create(
        &mut branch,
        ns,
        b"a",
        vec![
            (
                name("x"),
                Cell::attribute(Value::F64(Float64::new(2.5).unwrap())),
            ),
            (name("y"), Cell::field(Value::Date32(1))),
        ],
    );
    let root = branch.try_state_root().unwrap();
    let key = RecordKey::new(b"a").unwrap();
    let before = branch
        .get_record(ns, &key, &Projection::ALL, Budget::unlimited())
        .unwrap()
        .value;
    let receipts = branch.receipts().len();
    let applied = branch
        .execute(
            OWNER,
            WriteOp::PatchRecord {
                namespace: ns,
                key: key.clone(),
                set: Vec::new(),
                remove: Vec::new(),
            },
            Budget::unlimited(),
        )
        .expect("an empty patch of an existing record applies");
    assert!(matches!(applied.effect, Effect::RecordPatched { .. }));
    assert_eq!(branch.receipts().len(), receipts + 1);
    assert_eq!(branch.try_state_root().unwrap(), root);
    let after = branch
        .get_record(ns, &key, &Projection::ALL, Budget::unlimited())
        .unwrap()
        .value;
    assert_eq!(after, before);

    let missing = branch
        .execute(
            OWNER,
            WriteOp::PatchRecord {
                namespace: ns,
                key: RecordKey::new(b"missing").unwrap(),
                set: Vec::new(),
                remove: Vec::new(),
            },
            Budget::unlimited(),
        )
        .expect_err("an empty patch of a missing record is not found");
    assert_eq!(missing.reason, Rejection::NotFound);
    assert_eq!(branch.try_state_root().unwrap(), root);
}
