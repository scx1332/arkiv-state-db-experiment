//! Signed-chain security checks with real independent keys and explicit preimages.
use alloy_primitives::{Keccak256, Signature, U256};
use arkiv_db::{
    B256, CellName, Decode, Encode, NamespaceName, OwnerId, Value, decode_exact, encode,
};
use arkiv_node::{
    BlockOutcomes, Expiry, TxEnvelope, TxOp,
    accounts::AccountConfig,
    arkiv::ArkivOperation,
    signed::{
        crypto::{Signer, recover},
        protocol::*,
    },
};

fn signer(n: u8) -> Signer {
    let mut key = [0; 32];
    key[31] = n;
    Signer::from_bytes(key).unwrap()
}
fn glm() -> U256 {
    U256::from(1_000_000_000_000_000_000u64)
}
fn genesis() -> SignedGenesis {
    SignedGenesis::new(
        default_native_genesis(7, 1_000, 250),
        [0x42; 16],
        *signer(1).address().as_bytes(),
        250,
        AccountConfig::new(vec![
            (signer(1).address(), glm()),
            (signer(2).address(), glm()),
        ])
        .unwrap(),
    )
    .unwrap()
}
fn envelope(who: &Signer) -> TxEnvelope {
    TxEnvelope {
        chain_id: 7,
        actor: who.address(),
        request_id: [0x11; 32],
        tx_budget: 100_000,
        ops: vec![TxOp::CreateNamespace {
            name: NamespaceName::new("entities").unwrap(),
        }],
    }
}
fn signed() -> SignedTx {
    SignedTx::sign(&genesis(), 0, envelope(&signer(1)), &signer(1)).unwrap()
}

#[test]
fn signed_genesis_bounds_native_state_and_reserves_complete_outcomes() {
    let mut g = genesis();
    g.limits.max_body_bytes = g.limits.max_input_bytes + OUTCOME_RESERVE - 1;
    assert!(g.validate().is_err());
    let mut g = genesis();
    g.genesis.params.expiry_per_block = 17;
    assert!(g.validate().is_err());
    let mut g = genesis();
    g.genesis.limits.max_bytes_len = 128 * 1024 + 1;
    assert!(g.validate().is_err());
    // 32 user attributes always fit beside the 7 system cells: a signed
    // genesis admits 39 to 64 cells per record, whatever its prefill.
    for (cells, valid) in [
        (32, false),
        (38, false),
        (39, true),
        (64, true),
        (65, false),
    ] {
        let mut g = genesis();
        g.genesis.limits.max_cells = cells;
        assert_eq!(g.validate().is_ok(), valid, "{cells} cells");
    }
    assert_eq!(
        arkiv_node::arkiv::MIN_ENTITY_CELLS,
        arkiv_node::arkiv::SYSTEM_CELLS + arkiv_node::arkiv::MAX_ATTRIBUTES
    );
    assert_eq!(
        arkiv_node::signed::protocol::default_native_genesis(7, 0, 250)
            .limits
            .max_cells,
        arkiv_node::arkiv::MIN_ENTITY_CELLS
    );
}
fn independent_digest(domain: &str, bytes: &[u8]) -> B256 {
    let mut h = Keccak256::new();
    h.update([domain.len() as u8]);
    h.update(domain);
    h.update(bytes);
    h.finalize()
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn unhex<const N: usize>(s: &str) -> [u8; N] {
    assert_eq!(s.len(), N * 2);
    let mut out = [0; N];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap();
    }
    out
}

#[test]
fn preimages_are_explicit_and_golden() {
    let g = genesis();
    let tx = signed();
    // These bytes are built independently of the new types' encoders.
    let mut genesis_bytes = 1u32.to_be_bytes().to_vec();
    genesis_bytes.extend(encode(&g.genesis));
    genesis_bytes.extend([0x42; 16]);
    genesis_bytes.extend(g.proposer);
    genesis_bytes.extend(250u64.to_be_bytes());
    for cap in [TX_CAP, INPUT_CAP, BODY_CAP] {
        genesis_bytes.extend(cap.to_be_bytes());
    }
    genesis_bytes.extend(1_000_000_000u64.to_be_bytes());
    genesis_bytes.extend(2u32.to_be_bytes());
    let mut funded = [signer(1).address(), signer(2).address()];
    funded.sort();
    for actor in funded {
        genesis_bytes.extend(actor.as_bytes());
        genesis_bytes.extend(glm().to_be_bytes::<32>());
    }
    genesis_bytes.push(0);
    assert_eq!(encode(&g), genesis_bytes);
    assert_eq!(
        g.header().inputs_digest,
        independent_digest("arkiv/signed-genesis/v1", &genesis_bytes)
    );
    assert_ne!(g.hash(), g.genesis.hash());
    assert_eq!(
        hex(signer(1).address().as_bytes()),
        "7e5f4552091a69125d5dfcb7b8c2659029395bdf"
    );
    let mut intent = 1u32.to_be_bytes().to_vec();
    intent.extend(g.hash());
    intent.extend(0u64.to_be_bytes());
    intent.extend(7u64.to_be_bytes());
    intent.extend(signer(1).address().as_bytes());
    intent.extend([0x11; 32]);
    intent.extend(100_000u64.to_be_bytes());
    intent.extend(1u32.to_be_bytes());
    intent.push(1);
    intent.extend(8u32.to_be_bytes());
    intent.extend(b"entities");
    assert_eq!(
        tx.intent_digest(),
        independent_digest("arkiv/signed-tx-intent/v1", &intent)
    );
    let mut tx_bytes = intent;
    tx_bytes.extend(tx.signature);
    assert_eq!(encode(&tx), tx_bytes);
    assert_eq!(
        tx.hash(),
        independent_digest("arkiv/signed-tx/v1", &tx_bytes)
    );
    let inputs = SignedInputs {
        txs: vec![tx.clone()],
    };
    let mut input_bytes = 1u32.to_be_bytes().to_vec();
    input_bytes.extend(1u32.to_be_bytes());
    input_bytes.extend(tx_bytes);
    assert_eq!(encode(&inputs), input_bytes);
    assert_eq!(
        inputs.digest(),
        independent_digest("arkiv/signed-inputs/v1", &input_bytes)
    );
    let header = SignedHeader::sign(&g, g.header(), &signer(1)).unwrap();
    let mut header_intent = 1u32.to_be_bytes().to_vec();
    header_intent.extend(g.hash());
    header_intent.extend(g.header().hash());
    assert_eq!(
        header.intent_digest(),
        independent_digest("arkiv/signed-header/v1", &header_intent)
    );
    assert_eq!(
        recover(tx.intent_digest(), &tx.signature).unwrap(),
        signer(1).address()
    );
    assert_ne!(tx.intent_digest(), tx.hash());
    assert_eq!(signed(), tx); // deterministic RFC6979
    // Explicit byte-layout vectors pinned across dependency changes.
    assert_eq!(
        hex(g.hash().as_slice()),
        "0ab95aaae316933ab5ee4205d16f59e8c9203dd1621961ac561cb838b4aa96ba"
    );
    assert_eq!(
        hex(tx.intent_digest().as_slice()),
        "2ca8780041f75c9d3c16754731d14501e6cd318f120181fcb3f5cb50315138cd"
    );
    assert_eq!(
        hex(tx.hash().as_slice()),
        "802766df3cbf8847b10f8042172cb780a0c491a80e6d45b756486f63166f970f"
    );
    assert_eq!(
        hex(inputs.digest().as_slice()),
        "c54c5e63cae405cbd5d079d5e3672b404e9c9e5fd0cc954f997bd2f0422c1295"
    );
    assert_eq!(
        hex(header.intent_digest().as_slice()),
        "fd0ecccfa60dfc13734386725d7f5ec269a23661e7b4a0f68b98ec55d540ba49"
    );
    assert_eq!(
        hex(&tx.signature),
        "b6626f51a2eae1c0116beafeb8a2d53cf52caf0f7ece8671cb49595c4ccd3f4174502d22fe279e40f00e816cf00ce59c9369c9196de350b17263ecd98dec050000"
    );
    assert_eq!(
        hex(&header.signature),
        "fccafc617fd68aae65308cb58efa41e5439e60e6e04a17fc40a0e27c303268e02b11a63389075b9e602e51597481c9c2349570bf8511c4eb6b8afcf081f16c2b01"
    );
}

fn roundtrip<T: Encode + Decode + std::fmt::Debug + PartialEq>(value: T) {
    let bytes = encode(&value);
    assert_eq!(decode_exact::<T>(&bytes).unwrap(), value);
    for length in 0..bytes.len() {
        assert!(
            decode_exact::<T>(&bytes[..length]).is_err(),
            "truncation {length}"
        );
    }
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(decode_exact::<T>(&trailing).is_err());
    for version in [0u32, 2, u32::MAX] {
        let mut changed = bytes.clone();
        changed[..4].copy_from_slice(&version.to_be_bytes());
        assert!(decode_exact::<T>(&changed).is_err());
    }
}
#[test]
fn exact_decoding_versions_truncations_and_hostile_counts() {
    let g = genesis();
    let header = SignedHeader::sign(&g, g.header(), &signer(1)).unwrap();
    roundtrip(g.clone());
    roundtrip(signed());
    roundtrip(SignedInputs {
        txs: vec![signed()],
    });
    roundtrip(header.clone());
    roundtrip(SignedBlock {
        header,
        inputs: SignedInputs::default(),
        outcomes: BlockOutcomes::default(),
    });
    for count in [17u32, 1_000, u32::MAX] {
        let mut bytes = 1u32.to_be_bytes().to_vec();
        bytes.extend(count.to_be_bytes());
        assert!(decode_exact::<SignedInputs>(&bytes).is_err());
    }
    let mut ops = encode(&signed());
    ops[112..116].copy_from_slice(&u32::MAX.to_be_bytes());
    assert!(SignedTx::from_bytes(&ops).is_err());
    assert!(decode_exact::<SignedTx>(&encode(&signed().tx)).is_err());
    assert!(decode_exact::<SignedGenesis>(&encode(&g.genesis)).is_err());
    assert!(SignedTx::from_bytes(&vec![0; TX_CAP as usize + 1]).is_err());
    assert!(SignedBlock::from_bytes(&vec![0; BODY_CAP as usize + 1]).is_err());
}

#[test]
fn strict_parity_scalars_and_high_s_are_checked_before_recovery() {
    let tx = signed();
    for parity in [2, 3, 27, 28, 35, 36, 255] {
        let mut bytes = tx.signature;
        bytes[64] = parity;
        assert!(recover(tx.intent_digest(), &bytes).is_err());
    }
    let order = U256::from_be_bytes(unhex::<32>(
        "fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141",
    ));
    for scalar in [U256::ZERO, order, U256::MAX] {
        for offset in [0, 32] {
            let mut bytes = tx.signature;
            bytes[offset..offset + 32].copy_from_slice(&scalar.to_be_bytes::<32>());
            assert!(recover(tx.intent_digest(), &bytes).is_err());
        }
    }
    let mut high = tx.signature;
    high[32..64]
        .copy_from_slice(&(order - U256::from_be_slice(&tx.signature[32..64])).to_be_bytes::<32>());
    high[64] ^= 1;
    // The malleated form really is recoverable by Alloy's normalizing API.
    let alloy = Signature::from_raw_array(&high).unwrap();
    assert_eq!(
        alloy
            .recover_address_from_prehash(&tx.intent_digest())
            .unwrap()
            .as_slice(),
        tx.tx.actor.as_bytes()
    );
    assert!(recover(tx.intent_digest(), &high).is_err());
    let mut altered = tx.clone();
    altered.signature = high;
    assert!(decode_exact::<SignedTx>(&encode(&altered)).is_err());
    for length in [0, 1, 64, 66, 70] {
        let mut bytes = encode(&tx);
        bytes.truncate(bytes.len() - 65);
        bytes.extend(vec![1; length]);
        assert!(SignedTx::from_bytes(&bytes).is_err());
    }
    assert!(Signer::from_bytes([0; 32]).is_err());
    assert!(Signer::from_bytes([255; 32]).is_err());
}

#[test]
fn known_external_recovery_vector() {
    // Public Alloy secp256k1 recovery fixture, converted to canonical low-S.
    let original = unhex::<65>(
        "48b55bfa915ac795c431978d8a6a992b628d557da5ff759b307d495a36649353efffd310ac743f371de3b9f7f9cb56c0b28ad43601b4ab949f53faa07bd2c8041b",
    );
    let canonical = Signature::from_raw_array(&original)
        .unwrap()
        .normalized_s()
        .as_rsy();
    let hash = B256::from(unhex::<32>(
        "5eb4f5a33c621f32a8622d5f943b6b102994dfe4e5aebbefe69bb1b2aa0fc93e",
    ));
    assert_eq!(
        recover(hash, &canonical).unwrap(),
        OwnerId::new(unhex("0f65fe9276bc9a24ae7083ae28e2660ef72df99e"))
    );
}

#[test]
fn every_authority_field_and_operation_is_signed() {
    let g = genesis();
    let who = signer(1);
    let mut env = envelope(&who);
    env.ops = vec![
        TxOp::Arkiv {
            namespace: arkiv_node::arkiv::entity_namespace(),
            operation: ArkivOperation::Create {
                salt: 91,
                flags: 1,
                expiry: Expiry::PERMANENT,
                attributes: vec![
                    (
                        CellName::new("group").unwrap(),
                        arkiv_db::Cell::attribute(Value::U64(2)),
                    ),
                    (
                        CellName::new("body").unwrap(),
                        arkiv_db::Cell::field(Value::bytes(b"signed payload")),
                    ),
                ],
            },
        },
        TxOp::Arkiv {
            namespace: arkiv_node::arkiv::entity_namespace(),
            operation: ArkivOperation::Delete {
                key: arkiv_db::RecordKey::new(&[7; 32]).unwrap(),
            },
        },
    ];
    let tx = SignedTx::sign(&g, 4, env, &who).unwrap();
    let mut variants = Vec::new();
    let mut v = tx.clone();
    v.genesis_hash = B256::ZERO;
    variants.push(v);
    let mut v = tx.clone();
    v.tx.chain_id += 1;
    variants.push(v);
    let mut v = tx.clone();
    v.tx.actor = signer(2).address();
    variants.push(v);
    let mut v = tx.clone();
    v.nonce += 1;
    variants.push(v);
    let mut v = tx.clone();
    v.tx.request_id[0] ^= 1;
    variants.push(v);
    let mut v = tx.clone();
    v.tx.tx_budget += 1;
    variants.push(v);
    let mut v = tx.clone();
    v.tx.ops.reverse();
    variants.push(v);
    let mut v = tx.clone();
    if let TxOp::Arkiv { namespace, .. } = &mut v.tx.ops[0] {
        *namespace = arkiv_db::NamespaceId::from_ordinal(2).unwrap();
    }
    variants.push(v);
    let mut v = tx.clone();
    if let TxOp::Arkiv {
        operation: ArkivOperation::Create { salt, .. },
        ..
    } = &mut v.tx.ops[0]
    {
        *salt += 1;
    }
    variants.push(v);
    let mut v = tx.clone();
    if let TxOp::Arkiv {
        operation: ArkivOperation::Create { flags, .. },
        ..
    } = &mut v.tx.ops[0]
    {
        *flags = 2;
    }
    variants.push(v);
    let mut v = tx.clone();
    if let TxOp::Arkiv {
        operation: ArkivOperation::Create { attributes, .. },
        ..
    } = &mut v.tx.ops[0]
    {
        attributes[0].1 = arkiv_db::Cell::attribute(Value::U64(3));
    }
    variants.push(v);
    // The indexed flag is part of the signed operation.
    let mut v = tx.clone();
    if let TxOp::Arkiv {
        operation: ArkivOperation::Create { attributes, .. },
        ..
    } = &mut v.tx.ops[0]
    {
        attributes[0].1 = arkiv_db::Cell::field(Value::U64(2));
    }
    variants.push(v);
    let mut v = tx.clone();
    if let TxOp::Arkiv {
        operation: ArkivOperation::Create { attributes, .. },
        ..
    } = &mut v.tx.ops[0]
    {
        attributes[1].1 = arkiv_db::Cell::field(Value::bytes(b"altered payload"));
    }
    variants.push(v);
    let mut v = tx.clone();
    if let TxOp::Arkiv {
        operation: ArkivOperation::Delete { key },
        ..
    } = &mut v.tx.ops[1]
    {
        *key = arkiv_db::RecordKey::new(&[8; 32]).unwrap();
    }
    variants.push(v);
    for v in variants {
        assert!(v.validate(&g).is_err());
    }
    let mut other = g.clone();
    other.run_id[0] ^= 1;
    assert!(tx.validate(&other).is_err());
    let mut substituted = tx.clone();
    substituted.signature = signed().signature;
    assert!(substituted.validate(&g).is_err());
    assert!(SignedTx::sign(&g, u64::MAX, envelope(&who), &who).is_err());
    assert!(SignedTx::sign(&g, u64::MAX - 1, envelope(&who), &who).is_ok());
}

#[test]
fn bootstrap_and_unsigned_admin_rules_fail_closed() {
    let g = genesis();
    SignedInputs {
        txs: vec![signed()],
    }
    .validate(&g, 1)
    .unwrap();
    assert!(SignedInputs::default().validate(&g, 1).is_err());
    let who = signer(1);
    let mut env = envelope(&who);
    env.ops = vec![TxOp::CreateNamespace {
        name: NamespaceName::new("wrong").unwrap(),
    }];
    let changed = SignedTx::sign(&g, 0, env.clone(), &who).unwrap();
    assert!(
        SignedInputs {
            txs: vec![changed.clone()]
        }
        .validate(&g, 1)
        .is_err()
    );
    SignedInputs { txs: vec![changed] }.validate(&g, 2).unwrap();
    let other = signer(2);
    let mut env = envelope(&other);
    env.request_id[0] = 3;
    assert!(
        SignedInputs {
            txs: vec![SignedTx::sign(&g, 0, env, &other).unwrap()]
        }
        .validate(&g, 1)
        .is_err()
    );
    assert!(
        SignedInputs {
            txs: vec![signed(), signed()]
        }
        .validate(&g, 2)
        .is_err()
    );
    let mut bad = g.clone();
    bad.genesis.admins.push(who.address());
    assert!(bad.validate().is_err());
    let mut bad = g.clone();
    bad.genesis.params.max_admin_per_block = 1;
    assert!(bad.validate().is_err());
    let mut bad = g.clone();
    bad.genesis.protocol_version = arkiv_node::PROTOCOL_VERSION + 1;
    assert!(bad.validate().is_err());
    let mut bad = g.clone();
    bad.block_period_ms = 0;
    assert!(bad.validate().is_err());
    let mut bad = g.clone();
    bad.genesis.timestamp_ms = u64::MAX;
    assert!(bad.validate().is_err());
    assert!(g.timestamp(u64::MAX).is_err());
}

#[test]
fn header_certificates_authenticate_all_native_fields_and_genesis() {
    let g = genesis();
    let who = signer(1);
    let cert = SignedHeader::sign(&g, g.header(), &who).unwrap();
    cert.authenticate(&g).unwrap();
    assert!(SignedHeader::sign(&g, g.header(), &signer(2)).is_err());
    let mut changes = Vec::new();
    let mut h = cert.clone();
    h.genesis_hash = B256::ZERO;
    changes.push(h);
    let mut h = cert.clone();
    h.header.chain_id += 1;
    changes.push(h);
    let mut h = cert.clone();
    h.header.height += 1;
    changes.push(h);
    let mut h = cert.clone();
    h.header.parent_hash = B256::repeat_byte(1);
    changes.push(h);
    let mut h = cert.clone();
    h.header.timestamp_ms += 1;
    changes.push(h);
    let mut h = cert.clone();
    h.header.inputs_digest = B256::ZERO;
    changes.push(h);
    let mut h = cert.clone();
    h.header.outcomes_digest = B256::ZERO;
    changes.push(h);
    let mut h = cert.clone();
    h.header.state_root = B256::ZERO;
    changes.push(h);
    for h in changes {
        assert!(h.authenticate(&g).is_err());
    }
    let body = SignedBlock {
        header: cert,
        inputs: SignedInputs::default(),
        outcomes: BlockOutcomes::default(),
    };
    body.authenticate(&g).unwrap();
    let mut altered = body;
    altered.inputs.txs.push(signed());
    assert!(altered.authenticate(&g).is_err());
}
