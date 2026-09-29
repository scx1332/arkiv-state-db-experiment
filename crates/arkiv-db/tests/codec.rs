//! Golden vectors for the canonical codec, version 1 (`arkiv_db::codec`).
//!
//! What this file establishes:
//! - For every covered type, the encoding of hand-picked values equals bytes
//!   assembled in this file from the specification's pieces (`u32`/`u64`
//!   big-endian, `u32 length ‖ bytes`, `00`/`01` booleans and options,
//!   `u32 count ‖ items`, one tag byte per enum with the tag tables of
//!   `docs/block-format.md`, `u8 len ‖ bytes` record keys). The expected
//!   bytes are literals or concatenations of literal pieces; they are never
//!   produced by the codec under test.
//! - Each vector decodes back to its value with `decode_exact`, re-encodes
//!   to the same bytes, is rejected with one trailing byte, and every strict
//!   prefix is rejected as truncated.
//! - Non-canonical inputs are rejected with the specific error: bool and
//!   option bytes other than `00`/`01`, unknown tags of every enum, invalid
//!   names (grammar, empty, too long, non-UTF-8), zero namespace and record
//!   ids, record-key lengths 0 and above 32, a price-schedule class count
//!   other than 31, and huge sequence counts with little input (rejected
//!   before anything is reserved).
//! - Exhaustively over every 1-byte input for 12 types and every 2-byte
//!   input for 9 types, and over 1,000 seeded mutations of each of 17 seed
//!   vectors (the 8 `WriteOp` vectors and 9 others): whatever is accepted
//!   re-encodes to exactly the input (`encode(decode(x)) == x`), and the
//!   number of accepted short inputs equals the count derived by hand from
//!   the tag tables.
//! - `digest(domain, bytes)` equals `keccak256` (a trusted primitive,
//!   pinned here by its empty-input vector) over `u8 len(domain) ‖ domain ‖
//!   bytes` assembled by hand, domains of different lengths never collide,
//!   and invalid domains panic.
//! - The codec enforces type invariants only: an operation that violates an
//!   engine admission bound decodes and is then rejected by the engine.
//!
//! What it does not establish: block formats (node step N0b-3), trie leaf
//! layouts or roots (`roots.rs` and the `layout` unit tests cover those
//! since N3b), or anything about storage.

use std::{fmt::Debug, sync::Arc};

use alloy_primitives::{hex, keccak256};
use arkiv_db::{
    ArgumentFault, B256, Budget, Cell, CellKind, CellName, Cost, Decode, DecodeError, Effect,
    Encode, Engine, Ledger, LedgerEntry, Limits, NameFault, NamespaceDescriptor, NamespaceId,
    NamespaceName, OperationClass, Outcome, OwnerId, PriceSchedule, QueryFault, Receipt,
    ReceiptKind, RecordId, RecordKey, Rejection, Revision, TypeTag, Value, WriteOp, decode_exact,
    digest, encode,
};

// ---------------------------------------------------------------------------
// Specification pieces, written out by hand.
// ---------------------------------------------------------------------------

/// `u32`, big-endian: the length and count prefix.
fn be32(value: u32) -> Vec<u8> {
    value.to_be_bytes().to_vec()
}

/// `u64`, big-endian.
fn be64(value: u64) -> Vec<u8> {
    value.to_be_bytes().to_vec()
}

/// `u32 length ‖ bytes`.
fn lp(bytes: &[u8]) -> Vec<u8> {
    let len = u32::try_from(bytes.len()).expect("test strings are short");
    [be32(len), bytes.to_vec()].concat()
}

/// Concatenation of pieces.
fn cat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

/// Namespace 1: its ordinal as eight big-endian bytes.
const NS1: [u8; 8] = [0, 0, 0, 0, 0, 0, 0, 1];

// ---------------------------------------------------------------------------
// Values under test.
// ---------------------------------------------------------------------------

fn ns(ordinal: u64) -> NamespaceId {
    NamespaceId::from_ordinal(ordinal).expect("ordinals start at 1")
}

fn rid(ordinal: u64) -> RecordId {
    RecordId::from_ordinal(ordinal).expect("ids start at 1")
}

fn name(text: &str) -> CellName {
    CellName::new(text).expect("fixture names are valid")
}

fn key(bytes: &[u8]) -> RecordKey {
    RecordKey::new(bytes).expect("fixture keys are 1..=32 bytes")
}

/// The FIXTURE weights in `OperationClass::ALL` order, transcribed from the
/// charge table.
const FIXTURE_WEIGHTS: [u64; 31] = [
    1, 50, 20, 10, 5, 2, 1, 3, 4, 2, 1, 20, 10, 20, 4, 1, 4, 6, 30, 6, 5, 4, 3, 1, 1, 2, 3, 2, 1,
    2, 1,
];

// ---------------------------------------------------------------------------
// Checks.
// ---------------------------------------------------------------------------

/// `value` encodes to exactly `expected`, which decodes back to `value`,
/// re-encodes to itself, and is rejected with a trailing byte or cut short.
#[track_caller]
fn golden<T: Encode + Decode + PartialEq + Debug>(value: &T, expected: &[u8]) {
    assert_eq!(encode(value), expected, "encoding of {value:?}");
    let decoded = decode_exact::<T>(expected)
        .unwrap_or_else(|error| panic!("decoding {expected:02x?} as {value:?}: {error}"));
    assert_eq!(&decoded, value);
    assert_eq!(encode(&decoded), expected);
    let mut longer = expected.to_vec();
    longer.push(0);
    assert_eq!(
        decode_exact::<T>(&longer).err(),
        Some(DecodeError::TrailingBytes { remaining: 1 }),
        "trailing byte after {value:?}"
    );
    for cut in 0..expected.len() {
        match decode_exact::<T>(&expected[..cut]) {
            Err(DecodeError::Truncated { .. }) => {}
            other => panic!("{value:?} cut at {cut}: {other:?}"),
        }
    }
}

/// `bytes` is rejected as `T` with exactly `error`.
#[track_caller]
fn rejects<T: Decode + Debug>(bytes: &[u8], error: DecodeError) {
    assert_eq!(decode_exact::<T>(bytes).err(), Some(error), "{bytes:02x?}");
}

/// Whether `bytes` decodes as `T`; if it does, it must re-encode to itself.
#[track_caller]
fn accepted<T: Encode + Decode + Debug>(bytes: &[u8]) -> bool {
    match decode_exact::<T>(bytes) {
        Ok(value) => {
            assert_eq!(encode(&value), bytes, "{value:?} is not canonical");
            true
        }
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------
// Primitives and containers.
// ---------------------------------------------------------------------------

#[test]
fn integers_are_fixed_width_big_endian() {
    assert_eq!(
        arkiv_db::CODEC_VERSION,
        if cfg!(feature = "arkiv-v2") { 2 } else { 1 }
    );
    golden(&7u8, &[0x07]);
    golden(&0x0102_0304u32, &[1, 2, 3, 4]);
    golden(&258u64, &[0, 0, 0, 0, 0, 0, 1, 2]);
    golden(&u64::MAX, &[0xff; 8]);
    golden(&-2i64, &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xfe]);
    golden(&i64::MIN, &[0x80, 0, 0, 0, 0, 0, 0, 0]);
    golden(&i64::MAX, &[0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);
    rejects::<u64>(
        &[1, 2, 3],
        DecodeError::Truncated {
            needed: 8,
            remaining: 3,
        },
    );
}

#[test]
fn booleans_and_options_accept_only_zero_and_one() {
    golden(&false, &[0x00]);
    golden(&true, &[0x01]);
    rejects::<bool>(&[0x02], DecodeError::InvalidBool(2));
    rejects::<bool>(&[0xff], DecodeError::InvalidBool(0xff));

    golden(&None::<u32>, &[0x00]);
    golden(&Some(5u32), &[0x01, 0, 0, 0, 5]);
    golden(&Some(None::<bool>), &[0x01, 0x00]);
    golden(&Some(Some(true)), &[0x01, 0x01, 0x01]);
    rejects::<Option<u32>>(&[0x02, 0, 0, 0, 5], DecodeError::InvalidOption(2));
    rejects::<Option<bool>>(&[0x01, 0x02], DecodeError::InvalidBool(2));
}

#[test]
fn text_is_length_prefixed_utf8() {
    golden(&String::from("ab"), &[0, 0, 0, 2, b'a', b'b']);
    golden(&String::new(), &[0, 0, 0, 0]);
    golden(&String::from("hé"), &[0, 0, 0, 3, b'h', 0xc3, 0xa9]);
    rejects::<String>(&[0, 0, 0, 1, 0xff], DecodeError::InvalidUtf8);
    rejects::<String>(
        &[0, 0, 0, 5, b'a'],
        DecodeError::Truncated {
            needed: 5,
            remaining: 1,
        },
    );
}

#[test]
fn sequences_pairs_and_arrays() {
    golden(&Vec::<u32>::new(), &[0, 0, 0, 0]);
    golden(
        &vec![1u32, 0x0a0b_0c0d],
        &[0, 0, 0, 2, 0, 0, 0, 1, 0x0a, 0x0b, 0x0c, 0x0d],
    );
    // A byte vector is a sequence of one-byte items: the same bytes as a
    // length-prefixed string.
    golden(&vec![9u8, 8], &[0, 0, 0, 2, 9, 8]);
    golden(&(7u8, true), &[7, 1]);
    golden(&vec![(1u8, false), (2u8, true)], &[0, 0, 0, 2, 1, 0, 2, 1]);
    golden(&[0x11u8; 32], &[0x11; 32]);
    golden(&B256::repeat_byte(0x22), &[0x22; 32]);
    golden(
        &vec![vec![1u8], vec![]],
        &[0, 0, 0, 2, 0, 0, 0, 1, 1, 0, 0, 0, 0],
    );
}

#[test]
fn huge_counts_fail_before_anything_is_reserved() {
    // Four billion one-byte items with no input left.
    rejects::<Vec<u8>>(
        &[0xff, 0xff, 0xff, 0xff],
        DecodeError::Truncated {
            needed: 0xffff_ffff,
            remaining: 0,
        },
    );
    // Items of at least 8 bytes: the count alone promises 8 × count bytes.
    rejects::<Vec<u64>>(
        &[0xff, 0xff, 0xff, 0xff, 1, 2, 3],
        DecodeError::Truncated {
            needed: 0xffff_ffff * 8,
            remaining: 3,
        },
    );
    rejects::<Vec<u64>>(
        &[0, 0, 0, 1, 1, 2, 3, 4],
        DecodeError::Truncated {
            needed: 8,
            remaining: 4,
        },
    );
    // Large items (a price schedule is at least 256 bytes) and nested
    // sequences: reserving these counts would ask for terabytes, so a
    // decoder that trusted the count would abort instead of returning.
    for input in [
        [0xff, 0xff, 0xff, 0xff, 0, 0, 0, 0, 0, 0].as_slice(),
        &[0x7f, 0xff, 0xff, 0xff, 0, 0],
    ] {
        assert!(matches!(
            decode_exact::<Vec<PriceSchedule>>(input),
            Err(DecodeError::Truncated { .. })
        ));
        assert!(matches!(
            decode_exact::<Vec<WriteOp>>(input),
            Err(DecodeError::Truncated { .. })
        ));
        assert!(matches!(
            decode_exact::<Vec<Receipt>>(input),
            Err(DecodeError::Truncated { .. })
        ));
    }
    // Three operations need at least 3 × 6 bytes (the shortest is a
    // `CreateNamespace` with a one-character name): 17 bytes holding two
    // whole operations and a cut third are rejected by the count.
    rejects::<Vec<WriteOp>>(
        &cat(&[
            &[0, 0, 0, 3],
            &[1, 0, 0, 0, 1, b'a'],
            &[1, 0, 0, 0, 1, b'b'],
            &[1, 0, 0, 0, 1],
        ]),
        DecodeError::Truncated {
            needed: 18,
            remaining: 17,
        },
    );
    // An inner sequence with a huge count inside an outer one.
    assert!(matches!(
        decode_exact::<Vec<Vec<Cell>>>(&[0, 0, 0, 1, 0xff, 0xff, 0xff, 0xff, 1, 3]),
        Err(DecodeError::Truncated { .. })
    ));
    // A huge string length.
    rejects::<String>(
        &[0xff, 0xff, 0xff, 0xff, b'a'],
        DecodeError::Truncated {
            needed: 0xffff_ffff,
            remaining: 1,
        },
    );
}

// ---------------------------------------------------------------------------
// Identities and names.
// ---------------------------------------------------------------------------

#[test]
fn ids_owners_revisions_and_costs() {
    golden(&ns(258), &[0, 0, 0, 0, 0, 0, 1, 2]);
    golden(&ns(1), &NS1);
    rejects::<NamespaceId>(
        &[0; 8],
        DecodeError::ZeroId {
            kind: "NamespaceId",
        },
    );

    golden(&OwnerId::new([0xab; 20]), &[0xab; 20]);
    golden(&Revision(7), &[0, 0, 0, 0, 0, 0, 0, 7]);
    golden(&Revision(0), &[0; 8]);
    // 379 = 0x017b.
    golden(&Cost::new(379), &[0, 0, 0, 0, 0, 0, 0x01, 0x7b]);

    golden(&rid(7), &[0, 0, 0, 0, 0, 0, 0, 7]);
    rejects::<RecordId>(&[0; 8], DecodeError::ZeroId { kind: "RecordId" });
}

#[test]
fn names_are_validated_text() {
    let blog = NamespaceName::new("blog").unwrap();
    golden(&blog, &[0, 0, 0, 4, b'b', b'l', b'o', b'g']);
    golden(&name("$expiresAt"), &cat(&[&[0, 0, 0, 10], b"$expiresAt"]));
    golden(&name("x_y-z.w:v"), &lp(b"x_y-z.w:v"));
    let twenty = "n".repeat(20);
    golden(&name(&twenty), &lp(twenty.as_bytes()));
    golden(
        &NamespaceName::new(&twenty).unwrap(),
        &lp(twenty.as_bytes()),
    );

    for (bytes, fault) in [
        (lp(b""), NameFault::Empty),
        (lp(b"1ab"), NameFault::Grammar),
        (lp(b"a b"), NameFault::Grammar),
        (lp(b"$"), NameFault::Grammar),
        (lp("é".as_bytes()), NameFault::Grammar),
        (lp(b"a\0"), NameFault::Grammar),
        (
            lp("n".repeat(CellName::MAX_LEN + 1).as_bytes()),
            NameFault::TooLong,
        ),
    ] {
        rejects::<CellName>(&bytes, DecodeError::InvalidName(fault));
        rejects::<NamespaceName>(&bytes, DecodeError::InvalidName(fault));
    }
    rejects::<CellName>(&lp(&[0xff]), DecodeError::InvalidUtf8);
    rejects::<NamespaceName>(&lp(&[b'a', 0xc3]), DecodeError::InvalidUtf8);
}

#[test]
fn namespace_descriptors() {
    let descriptor = NamespaceDescriptor {
        id: ns(1),
        name: NamespaceName::new("blog").unwrap(),
        owner: OwnerId::new([0xa1; 20]),
        revision: Revision(2),
    };
    golden(
        &descriptor,
        &cat(&[&NS1, &lp(b"blog"), &[0xa1; 20], &[0, 0, 0, 0, 0, 0, 0, 2]]),
    );
    rejects::<NamespaceDescriptor>(
        &cat(&[&[0; 8], &lp(b"blog"), &[0xa1; 20], &[0; 8]]),
        DecodeError::ZeroId {
            kind: "NamespaceId",
        },
    );
}

#[test]
fn record_keys_are_a_length_byte_and_1_to_32_bytes() {
    golden(&key(b"a"), &[0x01, b'a']);
    let digest_key: Vec<u8> = (0xa0..=0xbf).collect();
    golden(&key(&digest_key), &cat(&[&[0x20], &digest_key]));
    golden(&key(&[0; 31]), &cat(&[&[31], &[0; 31]]));

    rejects::<RecordKey>(&[0x00], DecodeError::InvalidKeyLength(0));
    rejects::<RecordKey>(&cat(&[&[33], &[7; 33]]), DecodeError::InvalidKeyLength(33));
    rejects::<RecordKey>(&[0xff], DecodeError::InvalidKeyLength(0xff));
    rejects::<RecordKey>(
        &[0x02, b'a'],
        DecodeError::Truncated {
            needed: 2,
            remaining: 1,
        },
    );
}

// ---------------------------------------------------------------------------
// Types, values and cells.
// ---------------------------------------------------------------------------

#[test]
fn type_tags_and_values() {
    golden(&TypeTag::Bool, &[1]);
    golden(&TypeTag::I64, &[2]);
    golden(&TypeTag::U64, &[3]);
    golden(&TypeTag::Str, &[4]);
    golden(&TypeTag::Bytes, &[5]);
    for tag in [0, if cfg!(feature = "arkiv-v2") { 12 } else { 6 }, 0xff] {
        rejects::<TypeTag>(
            &[tag],
            DecodeError::UnknownTag {
                kind: "TypeTag",
                tag,
            },
        );
    }

    golden(&Value::Bool(false), &[1, 0]);
    golden(&Value::Bool(true), &[1, 1]);
    // Two's complement, not the sign-flipped term form.
    golden(
        &Value::I64(-2),
        &[2, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xfe],
    );
    golden(&Value::I64(i64::MIN), &[2, 0x80, 0, 0, 0, 0, 0, 0, 0]);
    golden(&Value::I64(1), &[2, 0, 0, 0, 0, 0, 0, 0, 1]);
    golden(&Value::U64(258), &[3, 0, 0, 0, 0, 0, 0, 1, 2]);
    golden(&Value::U64(u64::MAX), &cat(&[&[3], &[0xff; 8]]));
    golden(&Value::str("hé"), &[4, 0, 0, 0, 3, b'h', 0xc3, 0xa9]);
    golden(&Value::str(""), &[4, 0, 0, 0, 0]);
    golden(&Value::bytes(&[0x00, 0xff]), &[5, 0, 0, 0, 2, 0x00, 0xff]);
    golden(&Value::bytes(&[]), &[5, 0, 0, 0, 0]);

    rejects::<Value>(&[1, 2], DecodeError::InvalidBool(2));
    rejects::<Value>(&[4, 0, 0, 0, 1, 0xff], DecodeError::InvalidUtf8);
    for tag in [0, if cfg!(feature = "arkiv-v2") { 12 } else { 6 }, 0x80] {
        rejects::<Value>(&[tag, 0], DecodeError::UnknownTag { kind: "Value", tag });
    }
}

#[test]
fn cell_kinds_and_cells() {
    golden(&CellKind::Attribute, &[1]);
    golden(&CellKind::Field, &[2]);
    for tag in [0, 3] {
        rejects::<CellKind>(
            &[tag],
            DecodeError::UnknownTag {
                kind: "CellKind",
                tag,
            },
        );
    }
    golden(
        &Cell::attribute(Value::U64(7)),
        &[1, 3, 0, 0, 0, 0, 0, 0, 0, 7],
    );
    golden(&Cell::field(Value::bytes(&[])), &[2, 5, 0, 0, 0, 0]);
    golden(
        &(name("title"), Cell::field(Value::str("hi"))),
        &cat(&[&lp(b"title"), &[2, 4, 0, 0, 0, 2, b'h', b'i']]),
    );
}

// ---------------------------------------------------------------------------
// Writes and effects.
// ---------------------------------------------------------------------------

fn write_op_vectors() -> Vec<(WriteOp, Vec<u8>)> {
    vec![
        (
            WriteOp::CreateNamespace {
                name: NamespaceName::new("blog").unwrap(),
            },
            cat(&[&[1], &lp(b"blog")]),
        ),
        (
            WriteOp::TransferNamespace {
                namespace: ns(1),
                expected_revision: Revision(3),
                new_owner: OwnerId::new([0xb0; 20]),
            },
            cat(&[&[2], &NS1, &[0, 0, 0, 0, 0, 0, 0, 3], &[0xb0; 20]]),
        ),
        (
            WriteOp::Put {
                namespace: ns(1),
                key: b"k".to_vec(),
                value: Vec::new(),
            },
            cat(&[&[3], &NS1, &[0, 0, 0, 1, b'k'], &[0, 0, 0, 0]]),
        ),
        (
            WriteOp::Delete {
                namespace: ns(1),
                key: Vec::new(),
            },
            cat(&[&[4], &NS1, &[0, 0, 0, 0]]),
        ),
        (
            WriteOp::CreateRecord {
                namespace: ns(1),
                key: key(b"a"),
                cells: vec![
                    (name("price"), Cell::attribute(Value::U64(7))),
                    (name("note"), Cell::field(Value::str("hi"))),
                ],
            },
            cat(&[
                &[5],
                &NS1,
                &[1, b'a'],
                &[0, 0, 0, 2],
                &lp(b"price"),
                &[1, 3, 0, 0, 0, 0, 0, 0, 0, 7],
                &lp(b"note"),
                &[2, 4, 0, 0, 0, 2, b'h', b'i'],
            ]),
        ),
        (
            WriteOp::PatchRecord {
                namespace: ns(1),
                key: key(b"a"),
                set: vec![(name("price"), Cell::attribute(Value::I64(-1)))],
                remove: vec![name("note")],
            },
            cat(&[
                &[6],
                &NS1,
                &[1, b'a'],
                &[0, 0, 0, 1],
                &lp(b"price"),
                &[1, 2],
                &[0xff; 8],
                &[0, 0, 0, 1],
                &lp(b"note"),
            ]),
        ),
        (
            WriteOp::PatchRecord {
                namespace: ns(2),
                key: key(b"b"),
                set: Vec::new(),
                remove: Vec::new(),
            },
            cat(&[
                &[6],
                &[0, 0, 0, 0, 0, 0, 0, 2],
                &[1, b'b'],
                &[0, 0, 0, 0],
                &[0, 0, 0, 0],
            ]),
        ),
        (
            WriteOp::DeleteRecord {
                namespace: ns(1),
                key: key(b"a"),
            },
            cat(&[&[7], &NS1, &[1, b'a']]),
        ),
    ]
}

#[test]
fn write_ops() {
    for (op, bytes) in write_op_vectors() {
        golden(&op, &bytes);
    }
    for tag in [0, 8, 0xff] {
        rejects::<WriteOp>(
            &cat(&[&[tag], &NS1]),
            DecodeError::UnknownTag {
                kind: "WriteOp",
                tag,
            },
        );
    }
    rejects::<WriteOp>(
        &cat(&[&[7], &[0; 8], &[1, b'a']]),
        DecodeError::ZeroId {
            kind: "NamespaceId",
        },
    );
    rejects::<WriteOp>(
        &cat(&[&[1], &lp(b"no space")]),
        DecodeError::InvalidName(NameFault::Grammar),
    );
    rejects::<WriteOp>(
        &cat(&[
            &[5],
            &NS1,
            &[1, b'a'],
            &[0, 0, 0, 1],
            &lp(b"x"),
            &[3, 3, 0, 0, 0, 0, 0, 0, 0, 7],
        ]),
        DecodeError::UnknownTag {
            kind: "CellKind",
            tag: 3,
        },
    );
}

#[test]
fn engine_admission_bounds_are_left_to_the_engine() {
    // The codec checks the invariants of the types; a raw key over the
    // engine's 20-byte bound, a Bytes attribute or duplicate names decode,
    // and the engine then rejects them at admission.
    let long_key = cat(&[&[3], &NS1, &lp(&[7; 21]), &lp(b"")]);
    let op: WriteOp = decode_exact(&long_key).unwrap();
    let bytes_attribute = cat(&[
        &[5],
        &NS1,
        &[1, b'a'],
        &[0, 0, 0, 1],
        &lp(b"x"),
        &[1, 5, 0, 0, 0, 0],
    ]);
    let attribute: WriteOp = decode_exact(&bytes_attribute).unwrap();
    let duplicate = cat(&[
        &[5],
        &NS1,
        &[1, b'a'],
        &[0, 0, 0, 2],
        &lp(b"x"),
        &[2, 1, 0],
        &lp(b"x"),
        &[2, 1, 1],
    ]);
    let duplicated: WriteOp = decode_exact(&duplicate).unwrap();

    let engine = Engine::new(PriceSchedule::FIXTURE);
    let mut branch = engine.begin(engine.head(), None).unwrap();
    let owner = OwnerId::new([0xa1; 20]);
    let created = WriteOp::CreateNamespace {
        name: NamespaceName::new("blog").unwrap(),
    };
    branch.execute(owner, created, Budget::unlimited()).unwrap();
    for (op, fault) in [
        (op, ArgumentFault::KeyTooLong),
        (attribute, ArgumentFault::BytesNotIndexable),
        (duplicated, ArgumentFault::DuplicateCell),
    ] {
        let rejected = branch.execute(owner, op, Budget::unlimited()).unwrap_err();
        assert_eq!(rejected.reason, Rejection::InvalidArgument(fault));
    }
}

#[test]
fn effects() {
    golden(&Effect::NamespaceCreated(ns(1)), &cat(&[&[1], &NS1]));
    golden(
        &Effect::Transferred {
            revision: Revision(1),
        },
        &[2, 0, 0, 0, 0, 0, 0, 0, 1],
    );
    golden(&Effect::Put { replaced: true }, &[3, 1]);
    golden(&Effect::Put { replaced: false }, &[3, 0]);
    golden(&Effect::Deleted { existed: false }, &[4, 0]);
    golden(&Effect::Deleted { existed: true }, &[4, 1]);
    golden(
        &Effect::RecordCreated { id: rid(7) },
        &[5, 0, 0, 0, 0, 0, 0, 0, 7],
    );
    golden(
        &Effect::RecordPatched { id: rid(7) },
        &[6, 0, 0, 0, 0, 0, 0, 0, 7],
    );
    golden(
        &Effect::RecordDeleted { id: rid(7) },
        &[7, 0, 0, 0, 0, 0, 0, 0, 7],
    );
    golden(
        &Effect::BalanceTransferred {
            to: OwnerId::new([0xa1; 20]),
            amount: [0x42; 32],
        },
        &cat(&[&[8], &[0xa1; 20], &[0x42; 32]]),
    );
    for tag in [0, 9] {
        rejects::<Effect>(
            &[tag, 0],
            DecodeError::UnknownTag {
                kind: "Effect",
                tag,
            },
        );
    }
    rejects::<Effect>(&[3, 2], DecodeError::InvalidBool(2));
    rejects::<Effect>(
        &cat(&[&[5], &[0; 8]]),
        DecodeError::ZeroId { kind: "RecordId" },
    );
}

// ---------------------------------------------------------------------------
// Rejections, outcomes, receipts.
// ---------------------------------------------------------------------------

#[test]
fn argument_and_query_faults() {
    let arguments = [
        (ArgumentFault::KeyTooLong, 1),
        (ArgumentFault::TooManyCells, 2),
        (ArgumentFault::DuplicateCell, 3),
        (ArgumentFault::SetAndRemoveOverlap, 4),
        (ArgumentFault::ValueTooLarge, 5),
        (ArgumentFault::BytesNotIndexable, 6),
        (ArgumentFault::EmptyPatch, 7),
        (ArgumentFault::NotHostOperation, 8),
        (ArgumentFault::UnsupportedType, 9),
    ];
    for (fault, tag) in arguments {
        golden(&fault, &[tag]);
    }
    for tag in [0, 10] {
        rejects::<ArgumentFault>(
            &[tag],
            DecodeError::UnknownTag {
                kind: "ArgumentFault",
                tag,
            },
        );
    }
    let queries = [
        (QueryFault::NoGroups, 1),
        (QueryFault::TooManyGroups, 2),
        (QueryFault::TooManyLiterals, 3),
        (QueryFault::ZeroLimit, 4),
        (QueryFault::LimitTooLarge, 5),
        (QueryFault::NotIndexable, 6),
        (QueryFault::ValueTooLarge, 7),
        (QueryFault::UnboundedRange, 8),
        (QueryFault::MixedBounds, 9),
        (QueryFault::NotOrdered, 10),
        (QueryFault::ProjectionTooLarge, 11),
    ];
    for (fault, tag) in queries {
        golden(&fault, &[tag]);
    }
    for tag in [0, 12] {
        rejects::<QueryFault>(
            &[tag],
            DecodeError::UnknownTag {
                kind: "QueryFault",
                tag,
            },
        );
    }
}

#[test]
fn rejections() {
    golden(&Rejection::UnknownNamespace, &[1]);
    golden(&Rejection::Unauthorized, &[2]);
    golden(
        &Rejection::StaleRevision {
            expected: Revision(1),
            current: Revision(2),
        },
        &[3, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 2],
    );
    golden(
        &Rejection::ExceedsBlockBudget {
            remaining: Cost::new(100),
        },
        &[4, 0, 0, 0, 0, 0, 0, 0, 0x64],
    );
    golden(&Rejection::OutOfBudget, &[5]);
    golden(&Rejection::CostOverflow, &[6]);
    golden(&Rejection::DirectoryFull, &[7]);
    golden(&Rejection::CounterExhausted, &[8]);
    golden(&Rejection::AlreadyExists, &[9]);
    golden(&Rejection::NotFound, &[10]);
    golden(&Rejection::RecordFull, &[11]);
    golden(
        &Rejection::InvalidArgument(ArgumentFault::KeyTooLong),
        &[12, 1],
    );
    golden(
        &Rejection::InvalidArgument(ArgumentFault::NotHostOperation),
        &[12, 8],
    );
    golden(
        &Rejection::InvalidQuery(QueryFault::ProjectionTooLarge),
        &[13, 11],
    );
    golden(&Rejection::CursorMismatch, &[14]);
    for tag in [0, 15] {
        rejects::<Rejection>(
            &[tag],
            DecodeError::UnknownTag {
                kind: "Rejection",
                tag,
            },
        );
    }
    rejects::<Rejection>(
        &[12, 10],
        DecodeError::UnknownTag {
            kind: "ArgumentFault",
            tag: 10,
        },
    );
    rejects::<Rejection>(
        &[13, 0],
        DecodeError::UnknownTag {
            kind: "QueryFault",
            tag: 0,
        },
    );
}

#[test]
fn outcomes_and_receipt_kinds() {
    golden(&Outcome::Applied, &[1]);
    golden(&Outcome::Rejected(Rejection::NotFound), &[2, 10]);
    golden(&Outcome::Rejected(Rejection::OutOfBudget), &[2, 5]);
    for tag in [0, 3] {
        rejects::<Outcome>(
            &[tag],
            DecodeError::UnknownTag {
                kind: "Outcome",
                tag,
            },
        );
    }
    golden(&ReceiptKind::Write, &[1]);
    golden(&ReceiptKind::Read, &[2]);
    for tag in [0, 3] {
        rejects::<ReceiptKind>(
            &[tag],
            DecodeError::UnknownTag {
                kind: "ReceiptKind",
                tag,
            },
        );
    }
}

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

/// The applied write receipt of 379 units under schedule 1:
/// kind 01 ‖ model 00000003 ‖ schedule 00000001 ‖ spent 000000000000017b ‖
/// outcome 01.
const APPLIED_379: [u8; 18] = [1, 0, 0, 0, 3, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0x01, 0x7b, 1];

#[test]
fn receipts_encode_without_their_ledger() {
    let applied = receipt(ReceiptKind::Write, 1, 379, Outcome::Applied);
    golden(&applied, &APPLIED_379);
    golden(
        &receipt(
            ReceiptKind::Read,
            7,
            0,
            Outcome::Rejected(Rejection::StaleRevision {
                expected: Revision(0),
                current: Revision(1),
            }),
        ),
        &cat(&[
            &[2],
            &[0, 0, 0, 3],
            &[0, 0, 0, 7],
            &[0; 8],
            &[2, 3],
            &[0; 8],
            &[0, 0, 0, 0, 0, 0, 0, 1],
        ]),
    );

    // A traced receipt has the same encoding; decoding yields no ledger.
    let traced = Receipt {
        ledger: Some(Ledger {
            entries: vec![LedgerEntry {
                class: OperationClass::DirectoryLookup,
                count: 1,
                unit_price: 1,
                subtotal: Cost::new(1),
            }],
        }),
        ..applied.clone()
    };
    assert_eq!(encode(&traced), APPLIED_379);
    assert_eq!(decode_exact::<Receipt>(&APPLIED_379), Ok(applied));
    rejects::<Receipt>(
        &cat(&[&[3], &APPLIED_379[1..]]),
        DecodeError::UnknownTag {
            kind: "ReceiptKind",
            tag: 3,
        },
    );
}

// ---------------------------------------------------------------------------
// Pricing and limits.
// ---------------------------------------------------------------------------

fn weights_bytes(weights: &[u64]) -> Vec<u8> {
    weights.iter().flat_map(|weight| be64(*weight)).collect()
}

#[test]
fn price_schedules_carry_exactly_31_weights() {
    assert_eq!(
        PriceSchedule::new(1, FIXTURE_WEIGHTS),
        PriceSchedule::FIXTURE
    );
    let fixture = cat(&[
        &[0, 0, 0, 1],
        &[0, 0, 0, 31],
        &weights_bytes(&FIXTURE_WEIGHTS),
    ]);
    assert_eq!(fixture.len(), 4 + 4 + 31 * 8);
    // The first two weights (DirectoryLookup 1, NamespaceCreate 50) and the
    // last (ProofByte 1).
    assert_eq!(fixture[8..16], [0, 0, 0, 0, 0, 0, 0, 1]);
    assert_eq!(fixture[16..24], [0, 0, 0, 0, 0, 0, 0, 50]);
    assert_eq!(fixture[248..256], [0, 0, 0, 0, 0, 0, 0, 1]);
    golden(&PriceSchedule::FIXTURE, &fixture);

    let mut weights = [2u64; 31];
    weights[30] = 9;
    golden(
        &PriceSchedule::uniform(5, 2).with_price(OperationClass::ProofByte, 9),
        &cat(&[&[0, 0, 0, 5], &[0, 0, 0, 31], &weights_bytes(&weights)]),
    );

    for count in [0u32, 30, 32, u32::MAX] {
        rejects::<PriceSchedule>(
            &cat(&[
                &[0, 0, 0, 1],
                &be32(count),
                &weights_bytes(&FIXTURE_WEIGHTS),
            ]),
            DecodeError::ClassCountMismatch {
                expected: 31,
                found: count,
            },
        );
    }
    // Model-2 schedules had 27 classes: never silently padded.
    rejects::<PriceSchedule>(
        &cat(&[&[0, 0, 0, 1], &[0, 0, 0, 27], &weights_bytes(&[1; 27])]),
        DecodeError::ClassCountMismatch {
            expected: 31,
            found: 27,
        },
    );
}

#[test]
fn limits_are_six_u64_fields() {
    golden(
        &Limits::DEFAULT,
        &cat(&[
            &[0, 0, 0, 1, 0, 0, 0, 0],
            &be64(64),
            &be64(4096),
            &be64(200),
            &be64(8),
            &be64(8),
        ]),
    );
    golden(
        &Limits {
            namespace_cap: 3,
            max_cells: 2,
            max_value_len: 1,
            max_page: 5,
            max_groups: 7,
            max_literals: 11,
        },
        &cat(&[&be64(3), &be64(2), &be64(1), &be64(5), &be64(7), &be64(11)]),
    );
}

// ---------------------------------------------------------------------------
// Canonicity over every short input and over mutations.
// ---------------------------------------------------------------------------

#[test]
fn every_accepted_short_input_is_canonical() {
    fn count<T: Encode + Decode + Debug>(inputs: &[Vec<u8>]) -> usize {
        inputs.iter().filter(|input| accepted::<T>(input)).count()
    }
    let singles: Vec<Vec<u8>> = (0..=255u8).map(|byte| vec![byte]).collect();
    let pairs: Vec<Vec<u8>> = (0..=0xffffu16)
        .map(|pair| pair.to_be_bytes().to_vec())
        .collect();

    // Single bytes, counted from the tag tables: bool 00/01; option 00;
    // five type ids; two cell kinds; eight argument and eleven query
    // faults; ten field-less rejections (1, 2, 5–11, 14); outcome 01; two
    // receipt kinds.
    assert_eq!(count::<bool>(&singles), 2);
    assert_eq!(count::<Option<bool>>(&singles), 1);
    assert_eq!(
        count::<TypeTag>(&singles),
        if cfg!(feature = "arkiv-v2") { 11 } else { 5 }
    );
    assert_eq!(count::<CellKind>(&singles), 2);
    assert_eq!(count::<ArgumentFault>(&singles), 9);
    assert_eq!(count::<QueryFault>(&singles), 11);
    assert_eq!(count::<Rejection>(&singles), 10);
    assert_eq!(count::<Outcome>(&singles), 1);
    assert_eq!(count::<ReceiptKind>(&singles), 2);
    assert_eq!(count::<Value>(&singles), 0);
    assert_eq!(count::<Effect>(&singles), 0);
    assert_eq!(count::<RecordKey>(&singles), 0);

    // Two bytes: `01 xx` keys; Bool values; Put/Deleted effects; argument
    // (12, 1–8) and query (13, 1–11) rejections; rejected outcomes with a
    // field-less rejection; `01 00`/`01 01` options.
    assert_eq!(count::<RecordKey>(&pairs), 256);
    assert_eq!(count::<Value>(&pairs), 2);
    assert_eq!(count::<Effect>(&pairs), 4);
    assert_eq!(count::<Rejection>(&pairs), 9 + 11);
    assert_eq!(count::<Outcome>(&pairs), 10);
    assert_eq!(count::<Option<bool>>(&pairs), 2);
    assert_eq!(count::<(bool, bool)>(&pairs), 4);
    assert_eq!(count::<Cell>(&pairs), 0);
    assert_eq!(count::<u32>(&pairs), 0);
}

/// A small deterministic generator (xorshift64).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: usize) -> usize {
        usize::try_from(self.next() % u64::try_from(bound).unwrap()).unwrap()
    }
}

/// Mutates `bytes` in one of five ways: overwrite, insert, delete,
/// truncate, or change a length-prefix-sized window.
fn mutate(rng: &mut Rng, bytes: &[u8]) -> Vec<u8> {
    let mut out = bytes.to_vec();
    let position = rng.below(out.len() + 1);
    let byte = u8::try_from(rng.next() & 0xff).unwrap();
    match rng.below(5) {
        0 if !out.is_empty() => {
            let last = out.len() - 1;
            out[position.min(last)] = byte;
        }
        1 => out.insert(position, byte),
        2 if !out.is_empty() => {
            out.remove(position.min(out.len() - 1));
        }
        3 => out.truncate(position),
        _ => {
            for offset in 0..4 {
                if let Some(slot) = out.get_mut(position + offset) {
                    *slot = if offset == 3 { byte } else { 0 };
                }
            }
        }
    }
    out
}

#[test]
fn every_accepted_mutation_is_canonical() {
    /// Applies one mutation (every fourth round, two) to `seed` 1,000
    /// times; every accepted result must re-encode to itself. Returns how
    /// many were accepted, which must be at least one for the accept path
    /// to have been exercised (an overwritten id, weight or text byte of
    /// each seed stays valid).
    fn fuzz<T: Encode + Decode + Debug>(rng: &mut Rng, seed: &[u8]) -> usize {
        let mut accepted_count = 0;
        for round in 0..1000 {
            let mut input = mutate(rng, seed);
            if round % 4 == 3 {
                input = mutate(rng, &input);
            }
            if accepted::<T>(&input) {
                accepted_count += 1;
            }
        }
        assert!(
            accepted_count > 0,
            "no mutation of {seed:02x?} was accepted"
        );
        accepted_count
    }
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for (_, bytes) in write_op_vectors() {
        fuzz::<WriteOp>(&mut rng, &bytes);
    }
    fuzz::<Receipt>(&mut rng, &APPLIED_379);
    fuzz::<Value>(&mut rng, &[4, 0, 0, 0, 3, b'h', 0xc3, 0xa9]);
    fuzz::<Effect>(&mut rng, &[5, 0, 0, 0, 0, 0, 0, 0, 7]);
    fuzz::<Rejection>(
        &mut rng,
        &[3, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 2],
    );
    fuzz::<Limits>(&mut rng, &[1; 48]);
    fuzz::<Vec<(CellName, Cell)>>(
        &mut rng,
        &cat(&[&[0, 0, 0, 1], &lp(b"x"), &[1, 3], &[0; 8]]),
    );
    fuzz::<NamespaceDescriptor>(&mut rng, &cat(&[&NS1, &lp(b"blog"), &[0xa1; 20], &[0; 8]]));
    fuzz::<Option<RecordKey>>(&mut rng, &[1, 2, b'a', b'b']);
    fuzz::<PriceSchedule>(
        &mut rng,
        &cat(&[
            &[0, 0, 0, 1],
            &[0, 0, 0, 31],
            &weights_bytes(&FIXTURE_WEIGHTS),
        ]),
    );
}

// ---------------------------------------------------------------------------
// Digests.
// ---------------------------------------------------------------------------

#[test]
fn keccak_is_the_trusted_primitive() {
    assert_eq!(
        keccak256(b"").0,
        hex!("c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470")
    );
}

#[test]
fn digests_hash_a_length_prefixed_domain_then_the_bytes() {
    // "arkiv/test/v1" is 13 bytes.
    assert_eq!(
        digest("arkiv/test/v1", b""),
        keccak256(cat(&[&[13], b"arkiv/test/v1"]))
    );
    // "arkiv/header/v1" is 15 bytes.
    assert_eq!(
        digest("arkiv/header/v1", &[1, 2, 3]),
        keccak256(cat(&[&[15], b"arkiv/header/v1", &[1, 2, 3]]))
    );
    // The length prefix separates domains that share a concatenation.
    let a = digest("a", b"bc");
    let ab = digest("ab", b"c");
    assert_eq!(a, keccak256([1, b'a', b'b', b'c']));
    assert_eq!(ab, keccak256([2, b'a', b'b', b'c']));
    assert_ne!(a, ab);
    // A digest of canonical bytes: "arkiv/receipt/v1" is 16 bytes.
    let applied = receipt(ReceiptKind::Write, 1, 379, Outcome::Applied);
    assert_eq!(
        digest("arkiv/receipt/v1", &encode(&applied)),
        keccak256(cat(&[&[16], b"arkiv/receipt/v1", &APPLIED_379]))
    );
    // The longest domain: 255 bytes.
    let longest = "a".repeat(255);
    assert_eq!(
        digest(&longest, b"x"),
        keccak256(cat(&[&[255], longest.as_bytes(), b"x"]))
    );
    // Changing a byte of the input changes the digest.
    assert_ne!(digest("arkiv/test/v1", b"x"), digest("arkiv/test/v1", b"y"));
}

#[test]
#[should_panic(expected = "invalid digest domain")]
fn an_empty_domain_panics() {
    digest("", b"x");
}

#[test]
#[should_panic(expected = "invalid digest domain")]
fn a_256_byte_domain_panics() {
    digest(&"a".repeat(256), b"x");
}

#[test]
#[should_panic(expected = "invalid digest domain")]
fn a_domain_with_a_space_panics() {
    digest("arkiv header/v1", b"x");
}

#[test]
#[should_panic(expected = "invalid digest domain")]
fn a_non_ascii_domain_panics() {
    digest("arkiv/é/v1", b"x");
}

#[test]
fn values_round_trip_through_shared_storage() {
    // Str and Bytes values share their buffers; decoding yields equal
    // values with fresh buffers.
    let text: Arc<str> = Arc::from("shared");
    let value = Value::Str(Arc::clone(&text));
    let decoded: Value = decode_exact(&encode(&value)).unwrap();
    assert_eq!(decoded, value);
    assert_eq!(encode(&decoded), cat(&[&[4], &lp(b"shared")]));
}
