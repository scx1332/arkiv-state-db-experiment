//! State layout 2: unpadded, prefix-free term keys at the engine level.
//!
//! Strings chosen to break variable-length encodings (the empty string,
//! embedded and trailing NUL and U+0001, multibyte UTF-8, strings that are
//! prefixes of each other, maximum lengths) and every indexable scalar type
//! are written as attributes; equality, range, prefix and `terms` reads, and
//! complete equality and range proofs, are compared with an independent row
//! model (never `Engine::query` or the proof producer). Also: the default
//! profile keeps the 128-byte indexed-string limit while a generic profile
//! admits longer strings up to the 512-byte ceiling, consistently for
//! writes, equality values, range bounds and prefixes; and tampered or
//! misdirected proofs never verify to a different result.
use std::ops::Bound::{self, Excluded, Included, Unbounded};

use arkiv_db::{
    B256, Budget, Cell, CellName, Engine, Limits, NamespaceId, NamespaceName, OwnerId,
    PriceSchedule, Projection, Query, RecordId, RecordKey, Row, Snapshot, Term, TypeTag, Value,
    WriteOp,
    proof::{
        EqQuery, EqRequest, ProofLimits, RangeProofLimits, RangeQuery, RangeRequest, SnapshotRef,
        VerifiedEq, VerifiedRange, verify_eq, verify_range,
    },
};

const OWNER: OwnerId = OwnerId::new([7; 20]);

fn ns(n: u64) -> NamespaceId {
    NamespaceId::from_ordinal(n).unwrap()
}
fn name(text: &str) -> CellName {
    CellName::new(text).unwrap()
}

/// Strings whose naive encodings collide or misorder.
fn tricky_strings(max: usize) -> Vec<String> {
    let mut strings: Vec<String> = [
        "",
        "\0",
        "\0\0",
        "\u{1}",
        "\u{1}\0",
        "a",
        "a\0",
        "a\0\0",
        "a\0b",
        "a\u{1}",
        "aa",
        "ab",
        "a\u{7f}",
        "b",
        "é",
        "éa",
        "e\u{301}",
        "€",
        "\u{10ffff}",
        "z",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    strings.push("x".repeat(max));
    strings.push("x".repeat(max - 1));
    strings.push(format!("{}\0", "x".repeat(max - 1)));
    strings.push("\0".repeat(max));
    strings.push("é".repeat(max / 2));
    strings.sort();
    strings.dedup();
    strings
}

/// One namespace, one record per value, all under attribute `v`.
fn fixture(limits: Limits, values: &[Value]) -> (Engine, Vec<Row>) {
    let mut engine = Engine::with_limits(PriceSchedule::FIXTURE, limits);
    let mut branch = engine.begin(engine.head(), None).unwrap();
    branch
        .execute(
            OWNER,
            WriteOp::CreateNamespace {
                name: NamespaceName::new("terms").unwrap(),
            },
            Budget::unlimited(),
        )
        .unwrap();
    let mut rows = Vec::new();
    for (i, value) in values.iter().enumerate() {
        let key = RecordKey::new(&(i as u64).to_be_bytes()).unwrap();
        let cells = vec![(name("v"), Cell::attribute(value.clone()))];
        branch
            .execute(
                OWNER,
                WriteOp::CreateRecord {
                    namespace: ns(1),
                    key: key.clone(),
                    cells: cells.clone(),
                },
                Budget::unlimited(),
            )
            .unwrap();
        rows.push(Row {
            id: RecordId::from_ordinal(i as u64 + 1).unwrap(),
            key: Some(key),
            cells,
        });
    }
    engine.publish(&branch.seal()).unwrap();
    (engine, rows)
}

fn query_ids(snapshot: &Snapshot, term: Term) -> Vec<RecordId> {
    let mut ids = Vec::new();
    let mut cursor = None;
    loop {
        let page = snapshot
            .query(
                ns(1),
                &Query::term(term.clone()),
                &Projection::ALL,
                7,
                cursor.as_ref(),
                Budget::unlimited(),
            )
            .unwrap()
            .value;
        ids.extend(page.rows.iter().map(|row| row.id));
        match page.next {
            Some(next) => cursor = Some(next),
            None => return ids,
        }
    }
}

fn value_of(row: &Row) -> &Value {
    &row.cells[0].1.value
}

fn in_bounds(value: &Value, lower: &Bound<Value>, upper: &Bound<Value>) -> bool {
    use std::cmp::Ordering::*;
    (match lower {
        Unbounded => true,
        Included(v) => matches!(value.cmp_ordered(v), Some(Equal | Greater)),
        Excluded(v) => value.cmp_ordered(v) == Some(Greater),
    }) && (match upper {
        Unbounded => true,
        Included(v) => matches!(value.cmp_ordered(v), Some(Equal | Less)),
        Excluded(v) => value.cmp_ordered(v) == Some(Less),
    })
}

fn snapshot_ref(snapshot: &Snapshot) -> SnapshotRef {
    SnapshotRef {
        genesis_hash: B256::repeat_byte(1),
        height: 3,
        block_hash: B256::repeat_byte(2),
        state_root: snapshot.state_root(),
    }
}

fn range_rows(
    snapshot: &Snapshot,
    limits: Limits,
    lower: Bound<Value>,
    upper: Bound<Value>,
) -> Vec<Row> {
    let request = RangeRequest {
        snapshot: snapshot_ref(snapshot),
        query: RangeQuery {
            namespace: ns(1),
            attribute: name("v"),
            lower,
            upper,
        },
    };
    let bytes = snapshot
        .prove_range(&request, RangeProofLimits::default(), Budget::unlimited())
        .unwrap()
        .bytes;
    let VerifiedRange::Complete(result) =
        verify_range(&request, &bytes, RangeProofLimits::default(), limits).unwrap()
    else {
        panic!("namespace absent")
    };
    result.rows
}

fn eq_request(snapshot: &Snapshot, attribute: &str, value: Value) -> EqRequest {
    EqRequest {
        snapshot: snapshot_ref(snapshot),
        query: EqQuery {
            namespace: ns(1),
            attribute: name(attribute),
            value,
        },
        limit: 64,
        continuation: None,
    }
}

fn eq_rows(snapshot: &Snapshot, limits: Limits, value: &Value) -> Vec<Row> {
    let request = eq_request(snapshot, "v", value.clone());
    let bytes = snapshot
        .prove_eq(&request, ProofLimits::default(), Budget::unlimited())
        .unwrap()
        .bytes;
    match verify_eq(&request, None, &bytes, ProofLimits::default(), limits).unwrap() {
        VerifiedEq::Page(page) => page.rows,
        VerifiedEq::NamespaceAbsent => panic!("namespace absent"),
    }
}

fn check_strings(limits: Limits) {
    let max = limits.indexed_value_len();
    let strings = tricky_strings(max);
    let values: Vec<Value> = strings.iter().map(|s| Value::str(s)).collect();
    let (engine, model) = fixture(limits, &values);
    let snapshot = engine.snapshot(engine.head()).unwrap();
    // `terms` lists every distinct value in byte order.
    let listed = snapshot
        .terms(
            ns(1),
            &name("v"),
            TypeTag::Str,
            Unbounded,
            200,
            Budget::unlimited(),
        )
        .unwrap()
        .value;
    assert_eq!(listed, values);
    for (index, text) in strings.iter().enumerate() {
        let value = Value::str(text);
        // Equality: exactly one record, by query and by proof.
        assert_eq!(
            query_ids(&snapshot, Term::eq(name("v"), value.clone())),
            vec![model[index].id]
        );
        assert_eq!(
            eq_rows(&snapshot, limits, &value),
            vec![model[index].clone()]
        );
        // Prefix: every string starting with this one.
        let expected: Vec<RecordId> = model
            .iter()
            .filter(|row| matches!(value_of(row), Value::Str(s) if s.starts_with(text.as_str())))
            .map(|row| row.id)
            .collect();
        assert_eq!(
            query_ids(&snapshot, Term::starts_with(name("v"), text)),
            expected,
            "prefix {text:?}"
        );
        // `terms` from an inclusive and exclusive lower bound.
        for (lower, skip) in [(Included(value.clone()), 0), (Excluded(value.clone()), 1)] {
            let listed = snapshot
                .terms(
                    ns(1),
                    &name("v"),
                    TypeTag::Str,
                    lower,
                    200,
                    Budget::unlimited(),
                )
                .unwrap()
                .value;
            assert_eq!(listed, values[index + skip..].to_vec());
        }
    }
    // Ranges over every pair of inclusive/exclusive endpoints from a sample.
    let sample: Vec<Value> = ["", "\0", "a", "a\0", "aa", "é", "z"]
        .into_iter()
        .map(Value::str)
        .chain([values.last().unwrap().clone()])
        .collect();
    let mut bounds = vec![Unbounded];
    for value in &sample {
        bounds.push(Included(value.clone()));
        bounds.push(Excluded(value.clone()));
    }
    for lower in &bounds {
        for upper in &bounds {
            // A range needs at least one bound (queries and proofs alike).
            if *lower == Unbounded && *upper == Unbounded {
                continue;
            }
            let expected: Vec<Row> = model
                .iter()
                .filter(|row| in_bounds(value_of(row), lower, upper))
                .cloned()
                .collect();
            let ids: Vec<RecordId> = expected.iter().map(|row| row.id).collect();
            assert_eq!(
                query_ids(
                    &snapshot,
                    Term::range(name("v"), lower.clone(), upper.clone())
                ),
                ids,
                "{lower:?}..{upper:?}"
            );
            assert_eq!(
                range_rows(&snapshot, limits, lower.clone(), upper.clone()),
                expected,
                "proof {lower:?}..{upper:?}"
            );
        }
    }
}

#[test]
fn default_profile_strings_query_and_prove_like_the_model() {
    check_strings(Limits::DEFAULT);
}

#[test]
fn generic_profile_above_128_bytes_queries_and_proves_like_the_model() {
    let generic = Limits {
        max_indexed_len: 300,
        ..Limits::DEFAULT
    };
    assert_eq!(generic.validate(), Ok(()));
    assert_eq!(generic.indexed_value_len(), 300);
    check_strings(generic);
}

#[test]
fn indexed_string_admission_is_consistent_per_profile() {
    let generic = Limits {
        max_indexed_len: Value::INDEXED_LEN_CEILING,
        ..Limits::DEFAULT
    };
    for (limits, admitted) in [
        (Limits::DEFAULT, 128),
        (generic, Value::INDEXED_LEN_CEILING),
    ] {
        let (engine, _) = fixture(limits, &[]);
        let snapshot = engine.snapshot(engine.head()).unwrap();
        for (len, accepted) in [(admitted, true), (admitted + 1, false)] {
            // UTF-8 bytes, not characters: a two-byte character over the edge.
            for text in ["x".repeat(len), format!("{}é", "x".repeat(len - 2))] {
                let value = Value::str(&text);
                let writer = Engine::with_limits(PriceSchedule::FIXTURE, limits);
                let mut branch = writer.begin(writer.head(), None).unwrap();
                branch
                    .execute(
                        OWNER,
                        WriteOp::CreateNamespace {
                            name: NamespaceName::new("w").unwrap(),
                        },
                        Budget::unlimited(),
                    )
                    .unwrap();
                let write = branch.execute(
                    OWNER,
                    WriteOp::CreateRecord {
                        namespace: ns(1),
                        key: RecordKey::new(b"k").unwrap(),
                        cells: vec![(name("v"), Cell::attribute(value.clone()))],
                    },
                    Budget::unlimited(),
                );
                assert_eq!(write.is_ok(), accepted, "write {} bytes", text.len());
                for term in [
                    Term::eq(name("v"), value.clone()),
                    Term::starts_with(name("v"), &text),
                    Term::range(name("v"), Included(value.clone()), Unbounded),
                ] {
                    let read = snapshot.query(
                        ns(1),
                        &Query::term(term),
                        &Projection::ALL,
                        10,
                        None,
                        Budget::unlimited(),
                    );
                    assert_eq!(read.is_ok(), accepted, "query {} bytes", text.len());
                }
                let eq = eq_request(&snapshot, "v", value.clone());
                assert_eq!(
                    snapshot
                        .prove_eq(&eq, ProofLimits::default(), Budget::unlimited())
                        .is_ok(),
                    accepted
                );
                let range = RangeRequest {
                    snapshot: snapshot_ref(&snapshot),
                    query: RangeQuery {
                        namespace: ns(1),
                        attribute: name("v"),
                        lower: Included(value.clone()),
                        upper: Unbounded,
                    },
                };
                assert_eq!(
                    snapshot
                        .prove_range(&range, RangeProofLimits::default(), Budget::unlimited())
                        .is_ok(),
                    accepted
                );
            }
        }
    }
    // Above the ceiling no instance can be configured.
    let over = Limits {
        max_indexed_len: Value::INDEXED_LEN_CEILING + 1,
        ..Limits::DEFAULT
    };
    assert!(over.validate().is_err());
}

#[test]
fn every_scalar_type_orders_and_proves_like_the_model() {
    let mut min_dec = [0; 32];
    min_dec[0] = 0x80;
    let mut max_dec = [0xff; 32];
    max_dec[0] = 0x7f;
    let groups: Vec<Vec<Value>> = vec![
        vec![Value::Bool(true), Value::Bool(false)],
        vec![
            Value::I64(i64::MIN),
            Value::I64(-1),
            Value::I64(0),
            Value::I64(i64::MAX),
        ],
        vec![
            Value::U64(0),
            Value::U64(1),
            Value::U64(256),
            Value::U64(u64::MAX),
        ],
        vec![Value::I32(-5), Value::I32(5), Value::I32(i32::MIN)],
        vec![Value::U32(0), Value::U32(u32::MAX)],
        vec![Value::U128(0), Value::U128(u128::MAX), Value::U128(1 << 64)],
        vec![
            Value::I128(i128::MIN),
            Value::I128(0),
            Value::I128(i128::MAX),
        ],
        vec![
            Value::U256([0; 32]),
            Value::U256([0xff; 32]),
            Value::U256([1; 32]),
        ],
        vec![
            Value::I256(min_dec),
            Value::I256([0; 32]),
            Value::I256(max_dec),
        ],
        vec![
            Value::Dec(min_dec),
            Value::Dec([0xff; 32]),
            Value::Dec(max_dec),
        ],
        vec![Value::Dec32(-1), Value::Dec32(1)],
        vec![Value::Dec64(i64::MIN), Value::Dec64(7)],
        vec![Value::Dec128(-7), Value::Dec128(7)],
        vec![
            Value::f64(-1.5).unwrap(),
            Value::f64(0.0).unwrap(),
            Value::f64(f64::INFINITY).unwrap(),
            Value::f64(f64::NEG_INFINITY).unwrap(),
        ],
        vec![Value::f32(-0.5).unwrap(), Value::f32(2.0).unwrap()],
        vec![Value::Date32(-1), Value::Date32(1)],
        vec![Value::Timestamp64(-1), Value::Timestamp64(i64::MAX)],
        vec![Value::Addr([0; 20]), Value::Addr([0xff; 20])],
        vec![Value::Bytes4([0; 4]), Value::Bytes4([0, 0, 0, 1])],
        vec![Value::Bytes8([9; 8]), Value::Bytes8([0; 8])],
        vec![Value::Bytes16([0xff; 16]), Value::Bytes16([0; 16])],
        vec![Value::Bytes32([0; 32]), Value::Bytes32([0xfe; 32])],
        vec![Value::Key([1; 32]), Value::Key([0; 32])],
    ];
    for values in groups {
        let (engine, model) = fixture(Limits::DEFAULT, &values);
        let snapshot = engine.snapshot(engine.head()).unwrap();
        let tag = values[0].tag();
        let mut sorted = values.clone();
        sorted.sort_by(|a, b| a.cmp_ordered(b).unwrap());
        let listed = snapshot
            .terms(ns(1), &name("v"), tag, Unbounded, 100, Budget::unlimited())
            .unwrap()
            .value;
        assert_eq!(listed, sorted, "{tag:?}");
        for value in &values {
            let expected: Vec<Row> = model
                .iter()
                .filter(|row| value_of(row) == value)
                .cloned()
                .collect();
            assert_eq!(eq_rows(&snapshot, Limits::DEFAULT, value), expected);
            for (lower, upper) in [
                (Included(value.clone()), Unbounded),
                (Excluded(value.clone()), Unbounded),
                (Unbounded, Excluded(value.clone())),
                (Unbounded, Included(value.clone())),
            ] {
                let expected: Vec<Row> = model
                    .iter()
                    .filter(|row| in_bounds(value_of(row), &lower, &upper))
                    .cloned()
                    .collect();
                assert_eq!(
                    range_rows(&snapshot, Limits::DEFAULT, lower.clone(), upper.clone()),
                    expected,
                    "{tag:?} {lower:?}..{upper:?}"
                );
            }
        }
    }
}

#[test]
fn tampered_or_misdirected_proofs_never_verify_to_another_result() {
    let values: Vec<Value> = ["a", "a\0", "aa", "ab", "b"]
        .into_iter()
        .map(Value::str)
        .collect();
    let (engine, model) = fixture(Limits::DEFAULT, &values);
    let snapshot = engine.snapshot(engine.head()).unwrap();
    // Equality for "a": a different attribute, type or value must not accept
    // the proof of another request.
    let honest = eq_request(&snapshot, "v", Value::str("a"));
    let bytes = snapshot
        .prove_eq(&honest, ProofLimits::default(), Budget::unlimited())
        .unwrap()
        .bytes;
    let verified = verify_eq(
        &honest,
        None,
        &bytes,
        ProofLimits::default(),
        Limits::DEFAULT,
    )
    .unwrap();
    let VerifiedEq::Page(page) = &verified else {
        panic!("namespace absent")
    };
    assert_eq!(page.rows, vec![model[0].clone()]);
    for other in [
        eq_request(&snapshot, "w", Value::str("a")),
        eq_request(&snapshot, "v", Value::str("aa")),
        eq_request(&snapshot, "v", Value::str("a\0")),
        eq_request(&snapshot, "v", Value::Bool(true)),
        EqRequest {
            query: EqQuery {
                namespace: ns(2),
                ..honest.query.clone()
            },
            ..honest.clone()
        },
    ] {
        assert!(
            verify_eq(
                &other,
                None,
                &bytes,
                ProofLimits::default(),
                Limits::DEFAULT
            )
            .is_err()
        );
    }
    // Every single-byte change either fails or gives the honest answer.
    for index in 0..bytes.len() {
        let mut mutated = bytes.clone();
        mutated[index] ^= 0x01;
        if let Ok(result) = verify_eq(
            &honest,
            None,
            &mutated,
            ProofLimits::default(),
            Limits::DEFAULT,
        ) {
            assert_eq!(result, verified, "byte {index}");
        }
    }
    // A complete range ["a", "aa") holds exactly "a" and "a\0"; dropping or
    // altering any witness byte never yields another successful answer.
    let request = RangeRequest {
        snapshot: snapshot_ref(&snapshot),
        query: RangeQuery {
            namespace: ns(1),
            attribute: name("v"),
            lower: Included(Value::str("a")),
            upper: Excluded(Value::str("aa")),
        },
    };
    let bytes = snapshot
        .prove_range(&request, RangeProofLimits::default(), Budget::unlimited())
        .unwrap()
        .bytes;
    let verified = verify_range(
        &request,
        &bytes,
        RangeProofLimits::default(),
        Limits::DEFAULT,
    )
    .unwrap();
    let VerifiedRange::Complete(rows) = &verified else {
        panic!("namespace absent")
    };
    assert_eq!(rows.rows, model[..2].to_vec());
    for index in 0..bytes.len() {
        let mut mutated = bytes.clone();
        mutated[index] ^= 0x80;
        if let Ok(result) = verify_range(
            &request,
            &mutated,
            RangeProofLimits::default(),
            Limits::DEFAULT,
        ) {
            assert_eq!(result, verified, "byte {index}");
        }
        let mut truncated = bytes.clone();
        truncated.truncate(index);
        assert!(
            verify_range(
                &request,
                &truncated,
                RangeProofLimits::default(),
                Limits::DEFAULT
            )
            .is_err()
        );
    }
    // The same witness never verifies for a wider, narrower or retyped range.
    for (lower, upper) in [
        (Included(Value::str("a")), Included(Value::str("aa"))),
        (Excluded(Value::str("a")), Excluded(Value::str("aa"))),
        (Included(Value::str("")), Excluded(Value::str("aa"))),
        (Included(Value::Bool(false)), Unbounded),
    ] {
        let other = RangeRequest {
            query: RangeQuery {
                lower,
                upper,
                ..request.query.clone()
            },
            ..request.clone()
        };
        assert!(
            verify_range(&other, &bytes, RangeProofLimits::default(), Limits::DEFAULT).is_err()
        );
    }
}
