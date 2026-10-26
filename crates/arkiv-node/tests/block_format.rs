//! Golden vectors for block format version 1.
//!
//! Every expected byte string is assembled here from the specification in
//! `docs/block-format.md` (big-endian integers, `u32 length ‖ bytes`,
//! `u32 count ‖ items`, `00`/`01` options and the tag tables), never
//! produced by the encoder under test. Each vector must encode to exactly
//! those bytes, decode back to the value, be rejected with a trailing byte
//! and at every truncation. Every digest is `keccak256` (from
//! `alloy-primitives`, not the codec's `digest`) over a hand-assembled
//! preimage `u8 len(domain) ‖ domain ‖ bytes`.

mod support;

use std::fmt::Debug;

use alloy_primitives::{U256, keccak256};
use arkiv_db::{
    B256, Cost, Decode, DecodeError, Effect, Encode, Outcome, PriceSchedule, Receipt, ReceiptKind,
    RecordId, RecordKey, Rejection, Revision, Value, decode_exact, encode,
};
use arkiv_node::{
    AdminAction, AdminInput, AdminOutcome, AdminRejection, Block, BlockInputs, BlockOutcomes,
    ChainSpec, Expiry, ExpiryDeletion, ExpiryFault, Genesis, Header, HostFault, Inclusion,
    MAX_ADMIN_BYTES, MemoryChain, OpOutcome, RequestIdentity, Scope, TxEnvelope, TxOp, TxOutcome,
    TxStatus,
    accounts::AccountFault,
    arkiv::{ArkivFault, ArkivOperation},
};
use support::*;

// ---------------------------------------------------------------------------
// Byte builders, written from the codec rules.

fn be32(value: u32) -> Vec<u8> {
    value.to_be_bytes().to_vec()
}

fn be64(value: u64) -> Vec<u8> {
    value.to_be_bytes().to_vec()
}

/// `u32 length ‖ bytes`.
fn lp(bytes: &[u8]) -> Vec<u8> {
    let mut out = be32(u32::try_from(bytes.len()).unwrap());
    out.extend_from_slice(bytes);
    out
}

fn cat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

/// `u32 count ‖ items`.
fn seq(items: &[Vec<u8>]) -> Vec<u8> {
    let mut out = be32(u32::try_from(items.len()).unwrap());
    for item in items {
        out.extend_from_slice(item);
    }
    out
}

/// `digest(domain, bytes)` recomputed independently.
fn digest_of(domain: &str, bytes: &[u8]) -> B256 {
    let mut preimage = vec![u8::try_from(domain.len()).unwrap()];
    preimage.extend_from_slice(domain.as_bytes());
    preimage.extend_from_slice(bytes);
    keccak256(&preimage)
}

/// The vector checks: exact encoding, decoding back, a trailing byte and
/// every truncation rejected.
fn golden<T: Encode + Decode + PartialEq + Debug>(value: &T, expected: &[u8]) {
    assert_eq!(hex(&encode(value)), hex(expected), "encoding of {value:?}");
    assert_eq!(decode_exact::<T>(expected).as_ref(), Ok(value));
    let mut trailing = expected.to_vec();
    trailing.push(0);
    assert_eq!(
        decode_exact::<T>(&trailing),
        Err(DecodeError::TrailingBytes { remaining: 1 })
    );
    for cut in 0..expected.len() {
        assert!(
            decode_exact::<T>(&expected[..cut]).is_err(),
            "{value:?} decoded from {cut} of {} bytes",
            expected.len()
        );
    }
}

fn rejects<T: Decode + Debug>(bytes: &[u8], error: DecodeError) {
    assert_eq!(decode_exact::<T>(bytes).err(), Some(error));
}

fn unknown_tag<T: Decode + Debug>(kind: &'static str, tag: u8, rest: &[u8]) {
    let mut bytes = vec![tag];
    bytes.extend_from_slice(rest);
    rejects::<T>(&bytes, DecodeError::UnknownTag { kind, tag });
}

// ---------------------------------------------------------------------------
// Pieces reused by several vectors.

/// `PriceSchedule::FIXTURE`: id 1, 31 classes, the weights in
/// `OperationClass::ALL` order transcribed from the charge table.
fn fixture_schedule_bytes() -> Vec<u8> {
    let weights: [u64; 31] = [
        1, 50, 20, 10, 5, 2, 1, 3, 4, 2, 1, 20, 10, 20, 4, 1, 4, 6, 30, 6, 5, 4, 3, 1, 1, 2, 3, 2,
        1, 2, 1,
    ];
    let mut out = cat(&[&be32(1), &be32(31)]);
    for weight in weights {
        out.extend_from_slice(&be64(weight));
    }
    out
}

/// `support::LIMITS`: namespace cap 2^32, 8 cells, 32-byte names, 64-byte
/// strings and bytes, 128-byte indexed strings, pages of 4, 8 groups, 8
/// literals.
fn limits_bytes() -> Vec<u8> {
    cat(&[
        &be64(1 << 32),
        &be64(8),
        &be64(32),
        &be64(64),
        &be64(64),
        &be64(128),
        &be64(4),
        &be64(8),
        &be64(8),
    ])
}

/// `support::PARAMS`.
fn params_bytes() -> Vec<u8> {
    cat(&[
        &be64(100_888),
        &be64(100_000),
        &be32(4),
        &be32(2),
        &be32(16),
        &be32(8),
        &be32(64),
        &be32(4096),
        &be32(8 + 2 * 309 + 4096),
        &be64(100),
    ])
}

fn genesis_bytes() -> Vec<u8> {
    cat(&[
        &be32(1),
        // Protocol 2: the user-defined-columns entity rules.
        &be32(2),
        &be64(CHAIN),
        &be64(1_000),
        &limits_bytes(),
        &fixture_schedule_bytes(),
        &params_bytes(),
        &seq(&[vec![0xad; 20]]),
    ])
}

fn receipt(spent: u64, outcome: Outcome) -> Receipt {
    Receipt {
        kind: ReceiptKind::Write,
        model_version: 3,
        schedule_id: 1,
        spent: Cost::new(spent),
        outcome,
        ledger: None,
    }
}

/// An applied write receipt: `01 ‖ model 3 ‖ schedule 1 ‖ spent ‖ 01`.
fn applied_receipt_bytes(spent: u64) -> Vec<u8> {
    cat(&[&[1], &be32(3), &be32(1), &be64(spent), &[1]])
}

fn hash(byte: u8) -> B256 {
    B256::new([byte; 32])
}

// ---------------------------------------------------------------------------

#[test]
fn chain_params_and_genesis() {
    golden(&PARAMS, &params_bytes());
    assert_eq!(params_bytes().len(), 52);
    let genesis = genesis();
    let bytes = genesis_bytes();
    // 4 + 4 + 8 + 8 + 72 + 256 + 52 + 4 + 20.
    assert_eq!(bytes.len(), 428);
    golden(&genesis, &bytes);
    assert_eq!(genesis.digest(), digest_of("arkiv/genesis/v1", &bytes));
}

#[test]
fn genesis_versions_are_checked() {
    let mut bytes = genesis_bytes();
    for version in [0u32, 2, u32::MAX] {
        bytes[..4].copy_from_slice(&version.to_be_bytes());
        rejects::<Genesis>(
            &bytes,
            DecodeError::UnsupportedVersion {
                kind: "block format",
                version,
            },
        );
    }
    let mut bytes = genesis_bytes();
    // Protocol 1 is the retired payload entity model.
    for version in [0u32, 1, 3, u32::MAX] {
        bytes[4..8].copy_from_slice(&version.to_be_bytes());
        rejects::<Genesis>(
            &bytes,
            DecodeError::UnsupportedVersion {
                kind: "protocol",
                version,
            },
        );
    }
    // A schedule with 30 classes (the count sits after 4 + 4 + 8 + 8 + 72
    // bytes and the 4-byte schedule id).
    let mut bytes = genesis_bytes();
    bytes[100..104].copy_from_slice(&30u32.to_be_bytes());
    rejects::<Genesis>(
        &bytes,
        DecodeError::ClassCountMismatch {
            expected: 31,
            found: 30,
        },
    );
}

#[test]
fn headers_and_block_hashes() {
    let header = Header {
        chain_id: CHAIN,
        height: 5,
        parent_hash: hash(0x11),
        timestamp_ms: 6_000,
        inputs_digest: hash(0x22),
        outcomes_digest: hash(0x33),
        state_root: hash(0x44),
    };
    let bytes = cat(&[
        &be32(1),
        &be64(CHAIN),
        &be64(5),
        &[0x11; 32],
        &be64(6_000),
        &[0x22; 32],
        &[0x33; 32],
        &[0x44; 32],
    ]);
    assert_eq!(bytes.len(), 156);
    golden(&header, &bytes);
    assert_eq!(header.hash(), digest_of("arkiv/header/v1", &bytes));
    for version in [0u32, 2, u32::MAX] {
        let mut other = bytes.clone();
        other[..4].copy_from_slice(&version.to_be_bytes());
        rejects::<Header>(
            &other,
            DecodeError::UnsupportedVersion {
                kind: "header",
                version,
            },
        );
    }
}

/// The genesis header commits to the root of the empty initial state, which
/// the engine computes independently of the block format.
#[test]
fn genesis_block_header() {
    let genesis = genesis();
    let header = genesis.header();
    let empty_outcomes = cat(&[&be32(0), &be32(0), &be32(0), &be64(0)]);
    let empty_root = MemoryChain::new(ChainSpec::new(genesis.clone()).unwrap())
        .snapshot(0)
        .unwrap()
        .state_root();
    let expected = Header {
        chain_id: CHAIN,
        height: 0,
        parent_hash: B256::ZERO,
        timestamp_ms: 1_000,
        inputs_digest: digest_of("arkiv/genesis/v1", &genesis_bytes()),
        outcomes_digest: digest_of("arkiv/outcomes/v1", &empty_outcomes),
        state_root: empty_root,
    };
    assert_eq!(header, expected);
    let header_bytes = cat(&[
        &be32(1),
        &be64(CHAIN),
        &be64(0),
        &[0; 32],
        &be64(1_000),
        expected.inputs_digest.as_slice(),
        expected.outcomes_digest.as_slice(),
        empty_root.as_slice(),
    ]);
    assert_eq!(genesis.hash(), digest_of("arkiv/header/v1", &header_bytes));
    assert_eq!(
        hex(genesis.hash().as_slice()),
        GENESIS_HASH,
        "the support genesis hash"
    );
}

/// The hash of `support::genesis()`, pinned so a change to any genesis or
/// header byte is deliberate.
const GENESIS_HASH: &str = "dcae870e9fb324f007644ee4de69eb0306f04cc4a9e1078ebb5a0dad5e0d3ae9";

#[test]
fn admin_inputs() {
    let schedule = PriceSchedule::FIXTURE;
    let action = AdminAction::SetPricing(schedule);
    let action_bytes = cat(&[&[1], &fixture_schedule_bytes()]);
    golden(&action, &action_bytes);
    let input = AdminInput {
        actor: ADMIN,
        request_id: request(3),
        action,
    };
    let input_bytes = cat(&[&[0xad; 20], &request(3), &action_bytes]);
    golden(&input, &input_bytes);
    assert_eq!(input_bytes.len() as u64, MAX_ADMIN_BYTES);
    assert_eq!(MAX_ADMIN_BYTES, 309);
    assert_eq!(input.digest(), digest_of("arkiv/admin/v1", &input_bytes));
    unknown_tag::<AdminAction>("AdminAction", 0, &fixture_schedule_bytes());
    unknown_tag::<AdminAction>("AdminAction", 2, &fixture_schedule_bytes());
}

/// Every transaction operation, with and without an expiry.
fn ops() -> Vec<(TxOp, Vec<u8>)> {
    let ns1 = be64(1);
    let expiry = Expiry {
        expires_at: 9,
        min_lifetime: 2,
    };
    let expiry_bytes = cat(&[&be64(9), &be64(2)]);
    let price = cat(&[&lp(b"price"), &[1, 13], &be64(7)]);
    let note = cat(&[&lp(b"note"), &[2, 2], &lp(b"hi")]);
    vec![
        (create_namespace("alpha"), cat(&[&[1], &lp(b"alpha")])),
        (
            TxOp::TransferNamespace {
                namespace: ns(1),
                expected_revision: Revision(4),
                new_owner: BOB,
            },
            cat(&[&[2], &ns1, &be64(4), &[0xb0; 20]]),
        ),
        (put(1, b"k", b""), cat(&[&[3], &ns1, &lp(b"k"), &lp(b"")])),
        (
            TxOp::Delete {
                namespace: ns(1),
                key: b"k".to_vec(),
            },
            cat(&[&[4], &ns1, &lp(b"k")]),
        ),
        (
            create(
                1,
                b"a",
                vec![
                    ("price", attr(Value::U64(7))),
                    ("note", field(Value::str("hi"))),
                ],
                Some(expiry),
            ),
            cat(&[
                &[5],
                &ns1,
                &[1, b'a'],
                &seq(&[price.clone(), note]),
                &[1],
                &expiry_bytes,
            ]),
        ),
        (
            create(1, b"a", vec![], None),
            cat(&[&[5], &ns1, &[1, b'a'], &be32(0), &[0]]),
        ),
        (
            patch(
                1,
                b"a",
                vec![("price", attr(Value::U64(7)))],
                vec!["old"],
                None,
            ),
            cat(&[
                &[6],
                &ns1,
                &[1, b'a'],
                &seq(&[price]),
                &seq(&[lp(b"old")]),
                &[0],
            ]),
        ),
        (
            patch(1, b"a", vec![], vec![], Some(Expiry::PERMANENT)),
            cat(&[
                &[6],
                &ns1,
                &[1, b'a'],
                &be32(0),
                &be32(0),
                &[1],
                &be64(u64::MAX),
                &be64(0),
            ]),
        ),
        (
            delete(1, &[0x5a; 32]),
            cat(&[&[7], &ns1, &[32], &[0x5a; 32]]),
        ),
        (
            TxOp::Arkiv {
                namespace: ns(1),
                operation: ArkivOperation::Delete {
                    key: RecordKey::new(&[0x5a; 32]).unwrap(),
                },
            },
            cat(&[&[8], &ns1, &[5], &[32], &[0x5a; 32]]),
        ),
        (
            TxOp::Transfer {
                to: BOB,
                amount: U256::from(7u64),
            },
            cat(&[&[9], &[0xb0; 20], &U256::from(7u64).to_be_bytes::<32>()]),
        ),
    ]
}

#[test]
fn transaction_operations_and_envelopes() {
    golden(
        &Expiry {
            expires_at: 9,
            min_lifetime: 2,
        },
        &cat(&[&be64(9), &be64(2)]),
    );
    for (op, bytes) in ops() {
        golden(&op, &bytes);
    }
    unknown_tag::<TxOp>("TxOp", 0, &lp(b"alpha"));
    unknown_tag::<TxOp>("TxOp", 10, &lp(b"alpha"));
    // An expiry option byte other than 00/01.
    rejects::<TxOp>(
        &cat(&[&[5], &be64(1), &[1, b'a'], &be32(0), &[2]]),
        DecodeError::InvalidOption(2),
    );

    let (ops, op_bytes): (Vec<TxOp>, Vec<Vec<u8>>) = ops().into_iter().take(3).unzip();
    let envelope = TxEnvelope {
        chain_id: CHAIN,
        actor: ALICE,
        request_id: request(42),
        tx_budget: 5_000,
        ops,
    };
    let envelope_bytes = cat(&[
        &be64(CHAIN),
        &[0xa1; 20],
        &request(42),
        &be64(5_000),
        &seq(&op_bytes),
    ]);
    golden(&envelope, &envelope_bytes);
    assert_eq!(envelope.digest(), digest_of("arkiv/tx/v1", &envelope_bytes));

    let empty = BlockInputs::default();
    golden(&empty, &[0; 8]);
    assert_eq!(empty.digest(), digest_of("arkiv/inputs/v1", &[0; 8]));
    let admin = AdminInput {
        actor: ADMIN,
        request_id: request(3),
        action: AdminAction::SetPricing(PriceSchedule::FIXTURE),
    };
    let admin_bytes = cat(&[&[0xad; 20], &request(3), &[1], &fixture_schedule_bytes()]);
    let inputs = BlockInputs {
        admin: vec![admin],
        txs: vec![envelope],
    };
    let inputs_bytes = cat(&[&seq(&[admin_bytes]), &seq(&[envelope_bytes])]);
    golden(&inputs, &inputs_bytes);
    assert_eq!(inputs.digest(), digest_of("arkiv/inputs/v1", &inputs_bytes));
}

#[test]
fn admin_outcomes() {
    golden(&AdminRejection::ReservationViolated, &[1]);
    golden(&AdminRejection::ReservationOverflow, &[2]);
    golden(
        &AdminRejection::StaleSchedule { current: 5 },
        &cat(&[&[3], &be32(5)]),
    );
    golden(&AdminOutcome::Applied, &[1]);
    golden(
        &AdminOutcome::Rejected(AdminRejection::StaleSchedule { current: 5 }),
        &cat(&[&[2, 3], &be32(5)]),
    );
    unknown_tag::<AdminRejection>("AdminRejection", 0, &[]);
    golden(&AdminRejection::GasProfileLocked, &[4]);
    unknown_tag::<AdminRejection>("AdminRejection", 5, &[]);
    unknown_tag::<AdminOutcome>("AdminOutcome", 0, &[]);
    unknown_tag::<AdminOutcome>("AdminOutcome", 3, &[]);
}

#[test]
fn host_faults() {
    golden(&ExpiryFault::Overflow, &[1]);
    golden(&ExpiryFault::DeadOnArrival, &[2]);
    golden(&ExpiryFault::TooFar, &[3]);
    golden(&HostFault::ExpiryCapacityFull, &[1]);
    golden(&HostFault::ExpiryInvalid(ExpiryFault::Overflow), &[2, 1]);
    golden(
        &HostFault::ExpiryInvalid(ExpiryFault::DeadOnArrival),
        &[2, 2],
    );
    golden(&HostFault::ExpiryInvalid(ExpiryFault::TooFar), &[2, 3]);
    golden(&HostFault::ExpiryShortened, &[3]);
    golden(&HostFault::Arkiv(ArkivFault::NotFound), &[4, 4]);
    golden(
        &HostFault::Account(AccountFault::InsufficientBalance),
        &[5, 1],
    );
    unknown_tag::<ExpiryFault>("ExpiryFault", 0, &[]);
    unknown_tag::<ExpiryFault>("ExpiryFault", 4, &[]);
    unknown_tag::<HostFault>("HostFault", 0, &[]);
    unknown_tag::<HostFault>("HostFault", 6, &[]);
    rejects::<HostFault>(
        &[2, 4],
        DecodeError::UnknownTag {
            kind: "ExpiryFault",
            tag: 4,
        },
    );
}

fn id(ordinal: u64) -> RecordId {
    RecordId::from_ordinal(ordinal).unwrap()
}

/// Every operation outcome.
fn op_outcomes() -> Vec<(OpOutcome, Vec<u8>)> {
    let created = cat(&[&[5], &be64(1)]);
    vec![
        (
            OpOutcome::Applied {
                effect: Effect::RecordCreated { id: id(1) },
                receipt: receipt(132, Outcome::Applied),
            },
            cat(&[&[1], &created, &applied_receipt_bytes(132)]),
        ),
        (
            OpOutcome::RolledBack {
                effect: Effect::RecordCreated { id: id(1) },
                receipt: receipt(132, Outcome::Applied),
            },
            cat(&[&[2], &created, &applied_receipt_bytes(132)]),
        ),
        (
            OpOutcome::Rejected {
                reason: Rejection::AlreadyExists,
                receipt: receipt(4, Outcome::Rejected(Rejection::AlreadyExists)),
            },
            // AlreadyExists is Rejection tag 9; a rejected receipt ends in
            // `02 ‖ rejection`.
            cat(&[&[3, 9], &[1], &be32(3), &be32(1), &be64(4), &[2, 9]]),
        ),
        (
            OpOutcome::AdmissionRejected {
                reason: Rejection::ExceedsBlockBudget {
                    remaining: Cost::new(100),
                },
            },
            cat(&[&[4, 4], &be64(100)]),
        ),
        (
            OpOutcome::HostRejected {
                fault: HostFault::ExpiryShortened,
            },
            vec![5, 3],
        ),
        (OpOutcome::NotExecuted, vec![6]),
        (
            OpOutcome::AccountRejected {
                fault: AccountFault::InsufficientBalance,
                receipt: receipt(21, Outcome::Applied),
            },
            cat(&[&[7, 1], &applied_receipt_bytes(21)]),
        ),
    ]
}

#[test]
fn transaction_outcomes() {
    for (outcome, bytes) in op_outcomes() {
        golden(&outcome, &bytes);
    }
    unknown_tag::<OpOutcome>("OpOutcome", 0, &[]);
    unknown_tag::<OpOutcome>("OpOutcome", 8, &[]);
    golden(&TxStatus::Committed, &[1]);
    golden(&TxStatus::Failed { op_index: 2 }, &cat(&[&[2], &be32(2)]));
    unknown_tag::<TxStatus>("TxStatus", 0, &[]);
    unknown_tag::<TxStatus>("TxStatus", 3, &[]);

    let (ops, op_bytes): (Vec<OpOutcome>, Vec<Vec<u8>>) = op_outcomes()
        .into_iter()
        .filter(|(outcome, _)| {
            matches!(
                outcome,
                OpOutcome::RolledBack { .. } | OpOutcome::Rejected { .. } | OpOutcome::NotExecuted
            )
        })
        .unzip();
    let failed = TxOutcome {
        status: TxStatus::Failed { op_index: 1 },
        ops,
        spent: 136,
    };
    let failed_bytes = cat(&[&[2], &be32(1), &seq(&op_bytes), &be64(136)]);
    golden(&failed, &failed_bytes);

    let deletion = ExpiryDeletion {
        namespace: ns(2),
        record_id: id(9),
        receipt: receipt(78, Outcome::Applied),
    };
    let deletion_bytes = cat(&[&be64(2), &be64(9), &applied_receipt_bytes(78)]);
    golden(&deletion, &deletion_bytes);

    let empty = BlockOutcomes::default();
    let empty_bytes = cat(&[&be32(0), &be32(0), &be32(0), &be64(0)]);
    golden(&empty, &empty_bytes);
    assert_eq!(empty.digest(), digest_of("arkiv/outcomes/v1", &empty_bytes));
    let outcomes = BlockOutcomes {
        admin: vec![AdminOutcome::Applied],
        expiry: vec![deletion],
        txs: vec![failed],
        spent: 78 + 136,
    };
    let outcomes_bytes = cat(&[
        &seq(&[vec![1]]),
        &seq(&[deletion_bytes]),
        &seq(&[failed_bytes]),
        &be64(214),
    ]);
    golden(&outcomes, &outcomes_bytes);
    assert_eq!(
        outcomes.digest(),
        digest_of("arkiv/outcomes/v1", &outcomes_bytes)
    );
}

#[test]
fn blocks() {
    let header = Header {
        chain_id: CHAIN,
        height: 1,
        parent_hash: hash(0x11),
        timestamp_ms: 2_000,
        inputs_digest: digest_of("arkiv/inputs/v1", &[0; 8]),
        outcomes_digest: digest_of("arkiv/outcomes/v1", &[0; 20]),
        state_root: hash(0x44),
    };
    let header_bytes = cat(&[
        &be32(1),
        &be64(CHAIN),
        &be64(1),
        &[0x11; 32],
        &be64(2_000),
        header.inputs_digest.as_slice(),
        header.outcomes_digest.as_slice(),
        &[0x44; 32],
    ]);
    let block = Block {
        header,
        inputs: BlockInputs::default(),
        outcomes: BlockOutcomes::default(),
    };
    let block_bytes = cat(&[&header_bytes, &[0; 8], &[0; 20]]);
    golden(&block, &block_bytes);
    assert_eq!(block.hash(), digest_of("arkiv/header/v1", &header_bytes));
}

#[test]
fn request_identities_and_inclusions() {
    golden(&Scope::Tx, &[0]);
    golden(&Scope::Admin, &[1]);
    rejects::<Scope>(
        &[2],
        DecodeError::UnknownTag {
            kind: "Scope",
            tag: 2,
        },
    );
    let identity = RequestIdentity {
        scope: Scope::Admin,
        chain_id: CHAIN,
        actor: ADMIN,
        request_id: request(3),
    };
    golden(
        &identity,
        &cat(&[&[1], &be64(CHAIN), &[0xad; 20], &request(3)]),
    );
    let inclusion = Inclusion {
        height: 12,
        scope: Scope::Tx,
        index: 3,
        digest: hash(0x77),
    };
    golden(&inclusion, &cat(&[&be64(12), &[0], &be32(3), &[0x77; 32]]));
}

/// Hostile counts fail before allocating: a block inputs claiming four
/// billion admin inputs with eight bytes of input.
#[test]
fn hostile_counts_are_truncated() {
    let bytes = cat(&[&be32(u32::MAX), &be32(0)]);
    assert!(matches!(
        decode_exact::<BlockInputs>(&bytes),
        Err(DecodeError::Truncated { .. })
    ));
    let bytes = cat(&[&be32(0), &be32(u32::MAX)]);
    assert!(matches!(
        decode_exact::<BlockInputs>(&bytes),
        Err(DecodeError::Truncated { .. })
    ));
    let bytes = cat(&[&be32(u32::MAX), &be32(0), &be32(0), &be64(0)]);
    assert!(matches!(
        decode_exact::<BlockOutcomes>(&bytes),
        Err(DecodeError::Truncated { .. })
    ));
}

/// Engine types inside blocks keep their own strict decoding: a zero
/// record id and an invalid cell name inside an operation are rejected.
#[test]
fn nested_engine_types_stay_strict() {
    rejects::<ExpiryDeletion>(
        &cat(&[&be64(1), &be64(0), &applied_receipt_bytes(1)]),
        DecodeError::ZeroId { kind: "RecordId" },
    );
    let bad_name = cat(&[
        &[5],
        &be64(1),
        &[1, b'a'],
        &be32(1),
        &lp(b"1bad"),
        &[1, 13],
        &be64(7),
        &[0],
    ]);
    assert!(matches!(
        decode_exact::<TxOp>(&bad_name),
        Err(DecodeError::InvalidName(_))
    ));
}
