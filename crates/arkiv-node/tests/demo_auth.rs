use arkiv_db::{B256, decode_exact, encode};
use arkiv_node::demo::{
    DemoError,
    auth::{HeaderChain, SignedHeader, TrustConfig, signing_message},
    runtime::{Node, Role},
    wire::{self, Blocks, RpcResponse},
    workload,
};
use arkiv_node::{BlockInputs, ChainSpec, MemoryChain};
use ed25519_dalek::SigningKey;

fn fixture() -> (TrustConfig, SigningKey) {
    let key = SigningKey::from_bytes(&[0x42; 32]);
    let genesis = workload::genesis(7, 1000);
    (
        TrustConfig {
            genesis_hash: genesis.hash(),
            public_key: key.verifying_key().to_bytes(),
            genesis,
        },
        key,
    )
}
fn header_chain(trust: &TrustConfig, key: &SigningKey) -> HeaderChain {
    HeaderChain::new(
        trust.clone(),
        SignedHeader::sign(trust.genesis_hash, trust.genesis.header(), key),
        1000,
    )
    .unwrap()
}
#[test]
fn empty_genesis_root_and_prepared_publication() {
    let (trust, _) = fixture();
    let mut chain = MemoryChain::new(trust.validate().unwrap());
    assert_eq!(
        chain.head().state_root,
        chain.snapshot(0).unwrap().state_root()
    );
    let initial = *chain.head();
    let prepared = chain.prepare(2000, BlockInputs::default()).unwrap();
    assert_eq!(*chain.head(), initial);
    assert_eq!(prepared.block().header.state_root, initial.state_root);
    drop(prepared);
    assert_eq!(*chain.head(), initial);
    let block = chain.produce(2000, BlockInputs::default()).unwrap();
    let mut follower = MemoryChain::new(trust.validate().unwrap());
    let mut wrong = block.clone();
    wrong.header.state_root = B256::ZERO;
    assert!(follower.apply(&wrong).is_err());
    assert_eq!(follower.height(), 0);
    follower.apply(&block).unwrap();
    assert_eq!(chain.head(), follower.head());
    let mut other = trust.genesis;
    other.protocol_version += 1;
    assert!(ChainSpec::new(other).is_err());
}
#[test]
fn literal_authentication_layout_vectors() {
    let (trust, key) = fixture();
    let header = trust.genesis.header();
    let signed = SignedHeader::sign(trust.genesis_hash, header, &key);
    // Hand-built widths/layout, independent of the authentication encoder.
    let mut header_bytes = Vec::new();
    // Block format 4.
    header_bytes.extend(4u32.to_be_bytes());
    header_bytes.extend(7u64.to_be_bytes());
    header_bytes.extend(0u64.to_be_bytes());
    header_bytes.extend([0; 32]);
    header_bytes.extend(1000u64.to_be_bytes());
    header_bytes.extend(header.inputs_digest.as_slice());
    header_bytes.extend(header.outcomes_digest.as_slice());
    header_bytes.extend(header.state_root.as_slice());
    assert_eq!(header_bytes, encode(&header));
    let mut envelope = vec![0, 0, 0, 1];
    envelope.extend(trust.genesis_hash.as_slice());
    envelope.extend(&header_bytes);
    envelope.extend(signed.signature);
    assert_eq!(envelope, encode(&signed));
    let mut trust_bytes = vec![0, 0, 0, 1];
    let genesis_bytes = encode(&trust.genesis);
    trust_bytes.extend((genesis_bytes.len() as u32).to_be_bytes());
    trust_bytes.extend(genesis_bytes);
    trust_bytes.extend(trust.genesis_hash.as_slice());
    trust_bytes.extend(trust.public_key);
    assert_eq!(trust_bytes, encode(&trust));
    let domain = b"arkiv/demo-sign/v1";
    let mut preimage = vec![domain.len() as u8];
    preimage.extend(domain);
    preimage.extend(1u32.to_be_bytes());
    preimage.extend(trust.genesis_hash.as_slice());
    preimage.extend(header.hash().as_slice());
    assert_eq!(
        alloy_primitives::keccak256(preimage),
        signing_message(trust.genesis_hash, &header)
    );
    assert_eq!(
        trust.genesis_hash.to_string(),
        "0xa856e3073474017e5b84ae367864c5e24d79b274eb82c207ab3705b7e1212c22"
    );
    assert_eq!(
        signing_message(trust.genesis_hash, &header).to_string(),
        "0x6cc297a52b1091b7190d2d2f71800238eb657b2ac3b1111194c67e7f52aa4cdb"
    );
    assert_eq!(
        wire::hex(&signed.signature),
        "0x0b190e8b90b1f9ee39f47ed48801051a5a5f85673fcda156655953e76dcbca728ac6a811d9304d88273ead7fa15e52bb6cdadaba87fbd82ec5090e308cefad0c"
    );
    assert_eq!(decode_exact::<SignedHeader>(&envelope).unwrap(), signed);
    let mut bad = envelope.clone();
    bad.push(0);
    assert!(decode_exact::<SignedHeader>(&bad).is_err());
    bad = envelope;
    bad[3] = 2;
    assert!(decode_exact::<SignedHeader>(&bad).is_err());
}
#[test]
fn signature_linkage_equivocation_and_false_root_trust_boundary() {
    let (trust, key) = fixture();
    let mut producer = Node::producer(trust.clone(), key.clone(), 1000, Some(7)).unwrap();
    let body = producer.produce().unwrap();
    let valid: SignedHeader = wire::unpack(&body.signed_header, 1024).unwrap();
    let mut chain = header_chain(&trust, &key);
    assert!(chain.accept(valid.clone()).unwrap());
    assert!(!chain.accept(valid.clone()).unwrap());
    for bad in [
        SignedHeader::sign(
            trust.genesis_hash,
            valid.header,
            &SigningKey::from_bytes(&[9; 32]),
        ),
        SignedHeader::sign(B256::ZERO, valid.header, &key),
        {
            let mut h = valid.header;
            h.chain_id += 1;
            SignedHeader::sign(trust.genesis_hash, h, &key)
        },
        {
            let mut h = valid.header;
            h.height = 2;
            SignedHeader::sign(trust.genesis_hash, h, &key)
        },
        {
            let mut h = valid.header;
            h.timestamp_ms = 0;
            SignedHeader::sign(trust.genesis_hash, h, &key)
        },
        {
            let mut h = valid.clone();
            h.signature[0] ^= 1;
            h
        },
    ] {
        assert_eq!(
            header_chain(&trust, &key).accept(bad),
            Err(DemoError::InvalidHeader)
        );
    }
    let mut bad_parent = valid.header;
    bad_parent.parent_hash = B256::ZERO;
    let mut c = header_chain(&trust, &key);
    assert_eq!(
        c.accept(SignedHeader::sign(trust.genesis_hash, bad_parent, &key)),
        Err(DemoError::ChainConflict)
    );
    assert_eq!(c.conflicts().len(), 2);
    let mut false_header = valid.header;
    false_header.state_root = B256::ZERO;
    let false_signed = SignedHeader::sign(trust.genesis_hash, false_header, &key);
    // Authentication alone endorses a signed false root. This is explicit,
    // not described as consensus or execution verification.
    assert!(
        header_chain(&trust, &key)
            .accept(false_signed.clone())
            .unwrap()
    );
    assert_eq!(
        chain.accept(false_signed.clone()),
        Err(DemoError::ChainConflict)
    );
    assert_eq!(chain.accept(valid.clone()), Err(DemoError::ChainConflict));
    assert_eq!(chain.conflicts().len(), 2);
    let mut false_body = body.clone();
    let mut block: arkiv_node::Block = wire::unpack(&body.block, wire::RESPONSE_CAP).unwrap();
    block.header = false_header;
    false_body.block = wire::packed(&block);
    false_body.signed_header = wire::packed(&false_signed);
    let mut full = Node::follower(
        Role::Full,
        trust.clone(),
        SignedHeader::sign(trust.genesis_hash, trust.genesis.header(), &key),
        1000,
        "unused".into(),
    )
    .unwrap();
    assert_eq!(full.apply_body(false_body), Err(DemoError::InvalidHeader));
    assert_eq!(full.height(), 0);
    // Raw body tampering cannot advance a full follower either.
    let mut full = Node::follower(
        Role::Full,
        trust.clone(),
        SignedHeader::sign(trust.genesis_hash, trust.genesis.header(), &key),
        1000,
        "unused".into(),
    )
    .unwrap();
    let mut tampered = body;
    block.inputs = BlockInputs::default();
    tampered.block = wire::packed(&block);
    assert_eq!(full.apply_body(tampered), Err(DemoError::InvalidHeader));
    assert_eq!(full.height(), 0);
}
#[test]
fn unservable_candidate_and_capacity_never_publish() {
    let (trust, key) = fixture();
    let mut producer = Node::producer(trust.clone(), key.clone(), 3, Some(7)).unwrap();
    producer.response_cap = 1;
    assert_eq!(
        producer.produce().unwrap_err(),
        DemoError::ProofLimitExceeded
    );
    assert_eq!(producer.height(), 0);
    producer.response_cap = wire::RESPONSE_CAP;
    let first = producer.produce().unwrap();
    let response = RpcResponse::from_result(wire::json(&Blocks {
        blocks: vec![first],
        sampled_height: "1".into(),
        sampled_hash: producer.headers.head().header.hash().to_string(),
    }));
    assert!(serde_json::to_vec(&response).unwrap().len() < wire::RESPONSE_CAP);
    producer.produce().unwrap();
    assert_eq!(producer.produce().unwrap_err(), DemoError::CapacityReached);
    assert_eq!(producer.height(), 2);
    assert!(producer.chain.as_ref().unwrap().snapshot(0).is_some());
    assert!(producer.chain.as_ref().unwrap().snapshot(1).is_some());
}
#[test]
fn strict_wire_unknown_fields_versions_hex_and_decimal() {
    assert!(
        serde_json::from_str::<wire::RpcRequest>(
            r#"{"rpcVersion":1,"unknown":0,"request":{"method":"status"}}"#
        )
        .is_err()
    );
    assert!(
        serde_json::from_str::<wire::RpcRequest>(
            r#"{"rpcVersion":1,"request":{"method":"status","unknown":0}}"#
        )
        .is_err()
    );
    assert!(serde_json::from_str::<wire::RpcRequest>(r#"{"rpcVersion":1,"request":{"method":"headers","afterHeight":1,"afterHash":"0x00","limit":1}}"#).is_err());
    for text in ["1", "0Xab", "0xAB", "0xa", "0xgg"] {
        assert!(wire::unhex(text, 100).is_err());
    }
    for text in ["01", "-1", "+1", "18446744073709551616"] {
        assert!(wire::number(text).is_err());
    }
    assert_eq!(wire::number("18446744073709551615").unwrap(), u64::MAX);
    assert!(wire::unhex("0x0000", 1).is_err());
}

#[test]
fn largest_admitted_body_queue_identity_and_prepublication_failure() {
    use arkiv_db::{NamespaceId, NamespaceName};
    use arkiv_node::{RequestIdentity, Scope, TxEnvelope, TxOp};
    use wire::Request;
    let (_, key) = fixture();
    let mut genesis = workload::genesis(7, 1000);
    let tx = |id: u8, op| TxEnvelope {
        chain_id: 7,
        actor: workload::ALICE,
        request_id: [id; 32],
        tx_budget: 10000,
        ops: vec![op],
    };
    // Namespace 1 is the host-controlled entity namespace; the raw write
    // targets namespace 2.
    let entities = tx(
        4,
        TxOp::CreateNamespace {
            name: NamespaceName::new("entities").unwrap(),
        },
    );
    let create = tx(
        1,
        TxOp::CreateNamespace {
            name: NamespaceName::new("cap").unwrap(),
        },
    );
    let put = tx(
        2,
        TxOp::Put {
            namespace: NamespaceId::from_ordinal(2).unwrap(),
            key: vec![1; 20],
            value: vec![2; 1024],
        },
    );
    genesis.params.max_admin_per_block = 0;
    genesis.params.max_txs_per_block = 1;
    genesis.params.max_ops_per_tx = 1;
    genesis.params.max_ops_per_block = 1;
    genesis.params.max_tx_bytes = encode(&put).len() as u32;
    genesis.params.max_block_input_bytes = 8 + genesis.params.max_tx_bytes;
    let trust = TrustConfig {
        genesis_hash: genesis.hash(),
        public_key: key.verifying_key().to_bytes(),
        genesis,
    };
    let mut producer = Node::producer(trust.clone(), key.clone(), 100, None).unwrap();
    producer.submit(entities).unwrap();
    producer.produce().unwrap();
    assert_eq!(
        producer.submit(create.clone()).unwrap()["status"],
        "pending"
    );
    assert_eq!(
        producer.submit(create.clone()).unwrap()["status"],
        "pending"
    );
    let mut conflict = create.clone();
    conflict.tx_budget += 1;
    assert_eq!(
        producer.submit(conflict).unwrap_err(),
        DemoError::RequestConflict
    );
    producer.response_cap = 1;
    assert_eq!(
        producer.produce().unwrap_err(),
        DemoError::ProofLimitExceeded
    );
    assert_eq!(producer.status().pending, "1");
    assert_eq!(producer.height(), 1);
    producer.response_cap = wire::RESPONSE_CAP;
    producer.produce().unwrap();
    assert_eq!(producer.submit(create).unwrap()["status"], "included");
    producer.submit(put.clone()).unwrap();
    let body = producer.produce().unwrap();
    let block: arkiv_node::Block = wire::unpack(&body.block, wire::RESPONSE_CAP).unwrap();
    assert_eq!(
        encode(&block.inputs).len(),
        trust.genesis.params.max_block_input_bytes as usize
    );
    let returned = producer
        .handle(Request::Blocks {
            from_height: "3".into(),
            limit: 16,
        })
        .unwrap();
    assert_eq!(returned["blocks"][0]["block"], body.block);
    let signed = SignedHeader::sign(trust.genesis_hash, trust.genesis.header(), &key);
    let mut full = Node::follower(Role::Full, trust, signed, 100, "unused".into()).unwrap();
    let batch: Blocks = wire::parse(
        producer
            .handle(Request::Blocks {
                from_height: "1".into(),
                limit: 16,
            })
            .unwrap(),
    )
    .unwrap();
    for body in batch.blocks {
        full.apply_body(body).unwrap();
    }
    assert_eq!(full.height(), 3);
    assert!(!full.apply_body(body.clone()).unwrap());
    let mut duplicate = body;
    let mut tampered_block: arkiv_node::Block =
        wire::unpack(&duplicate.block, wire::RESPONSE_CAP).unwrap();
    tampered_block.outcomes.spent += 1;
    duplicate.block = wire::packed(&tampered_block);
    assert_eq!(full.apply_body(duplicate), Err(DemoError::InvalidHeader));
    assert_eq!(full.height(), 3);
    let identity = RequestIdentity {
        scope: Scope::Tx,
        chain_id: 7,
        actor: workload::ALICE,
        request_id: [2; 32],
    };
    let status = producer
        .handle(Request::RequestStatus {
            identity: wire::packed(&identity),
        })
        .unwrap();
    assert_eq!(status["status"], "included");
    assert_eq!(status["height"], "3");
    let mut too_big = put;
    too_big.request_id = [3; 32];
    if let TxOp::Put { value, .. } = &mut too_big.ops[0] {
        value.push(3);
    }
    assert_eq!(
        producer.submit(too_big).unwrap_err(),
        DemoError::InvalidRequest
    );
}

#[test]
fn queue_count_is_bounded_and_duplicate_retries_do_not_consume_capacity() {
    use arkiv_node::{TxEnvelope, TxOp};
    let (trust, key) = fixture();
    let mut producer = Node::producer(trust, key, 100, None).unwrap();
    for n in 0u64..1024 {
        let mut id = [0; 32];
        id[24..].copy_from_slice(&n.to_be_bytes());
        let tx = TxEnvelope {
            chain_id: 7,
            actor: workload::ALICE,
            request_id: id,
            tx_budget: 1000,
            ops: vec![TxOp::CreateNamespace {
                name: arkiv_db::NamespaceName::new("queue").unwrap(),
            }],
        };
        producer.submit(tx.clone()).unwrap();
        producer.submit(tx).unwrap();
    }
    assert_eq!(producer.status().pending, "1024");
    let tx = TxEnvelope {
        chain_id: 7,
        actor: workload::ALICE,
        request_id: [0xff; 32],
        tx_budget: 1000,
        ops: vec![TxOp::CreateNamespace {
            name: arkiv_db::NamespaceName::new("queue").unwrap(),
        }],
    };
    assert_eq!(producer.submit(tx).unwrap_err(), DemoError::QueueFull);
    assert_eq!(producer.status().pending, "1024");
}

#[test]
fn workload_keys_remain_bounded_for_the_full_seed_and_height_width() {
    for height in [999_998, u64::MAX - 13, u64::MAX] {
        let inputs = workload::inputs(7, height, u64::MAX);
        for tx in inputs.txs {
            for op in tx.ops {
                if let arkiv_node::TxOp::CreateRecord { key, .. } = op {
                    assert!(key.as_bytes().len() <= 32);
                }
            }
        }
    }
}
