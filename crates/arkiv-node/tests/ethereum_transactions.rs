//! Independent viem 2.x signed Ethereum fixtures; never resigned by the native host.
//! `fixtures/ethereum-sdk-vectors.mjs` regenerates them.
use alloy_primitives::{U256, keccak256};
use arkiv_db::{Cell, CellName, Value, encode};
use arkiv_node::{
    TxOp,
    arkiv::ArkivOperation,
    signed::{crypto::Signer, ethereum, protocol::*},
};
mod signed_support;
fn genesis() -> SignedGenesis {
    signed_support::genesis(9300, 1000, [7; 16], 2, 1000, &[2])
}
fn bytes(s: &str) -> Vec<u8> {
    alloy_primitives::hex::decode(s).unwrap()
}
fn fixture(name: &str) -> Vec<u8> {
    let json: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/ethereum-sdk-vectors.json")).unwrap();
    bytes(json[name].as_str().unwrap())
}
#[test]
fn independent_sdk_legacy_and_1559_authentication_and_exact_native_mapping() {
    let g = genesis();
    for name in ["legacy", "eip1559"] {
        let raw = fixture(name);
        let tx = SignedTx::from_ethereum(&g, &raw).unwrap();
        assert_eq!(tx.hash(), keccak256(&raw));
        assert_eq!(tx.nonce, 0);
        assert_eq!(tx.tx.actor, Signer::from_bytes([2; 32]).unwrap().address());
        assert_eq!(tx.tx.tx_budget, 100_000);
        assert_eq!(tx.ethereum_raw.as_deref(), Some(raw.as_slice()));
        let TxOp::Arkiv {
            operation:
                ArkivOperation::Create {
                    salt,
                    expiry,
                    attributes,
                    ..
                },
            ..
        } = &tx.tx.ops[0]
        else {
            panic!("create")
        };
        assert_eq!(*salt, 9);
        assert_eq!(expiry.min_lifetime, 20);
        assert_eq!(expiry.expires_at, 0);
        // The SDK's create for a named schema "posts": the indexed tag
        // (0x88), unindexed bytes (0x07), an indexed u64 (0x83) and an
        // unindexed str (0x08), strictly ascending by name.
        assert_eq!(attributes, &udc_attributes());
        let wire = encode(&tx);
        assert_eq!(&wire[..4], &2u32.to_be_bytes());
        let restored = SignedTx::from_bytes(&wire).unwrap();
        restored.validate(&g).unwrap();
        assert_eq!(restored, tx);
        for mutation in 0..4 {
            let mut forged = tx.clone();
            match mutation {
                0 => forged.nonce += 1,
                1 => forged.tx.tx_budget += 1,
                2 => forged.tx.request_id[0] ^= 1,
                _ => forged.tx.ops.clear(),
            }
            assert!(
                forged.validate(&g).is_err(),
                "mutated mapped field {mutation}"
            );
        }
        let mut wrong_chain = g.clone();
        wrong_chain.genesis.chain_id += 1;
        assert!(SignedTx::from_ethereum(&wrong_chain, &raw).is_err());
        let mut trailing = raw.clone();
        trailing.push(0);
        assert!(ethereum::parse(&trailing).is_err());
        let mut broken = raw.clone();
        *broken.last_mut().unwrap() ^= 1;
        if let Ok(tx2) = SignedTx::from_ethereum(&g, &broken) {
            assert_ne!(tx2.tx.actor, tx.tx.actor);
        }
    }
}
fn udc_attributes() -> Vec<(CellName, Cell)> {
    vec![
        (
            CellName::new("arkiv.schema").unwrap(),
            Cell::attribute(Value::str("posts")),
        ),
        (
            CellName::new("body").unwrap(),
            Cell::field(Value::bytes(&[1, 2, 3])),
        ),
        (
            CellName::new("score").unwrap(),
            Cell::attribute(Value::U64(42)),
        ),
        (
            CellName::new("title").unwrap(),
            Cell::field(Value::str("hello")),
        ),
    ]
}
#[test]
fn calldata_rejects_trailing_aliases_and_unknown_selector() {
    let calldata = fixture("data");
    assert!(ethereum::operations(&calldata).is_ok());
    let mut trailing = calldata.clone();
    trailing.extend([0; 32]);
    assert!(ethereum::operations(&trailing).is_err());
    let mut selector = calldata;
    selector[0] ^= 1;
    assert!(ethereum::operations(&selector).is_err());
    for raw in [
        vec![],
        vec![1, 0xc0],
        vec![3, 0xc0],
        vec![0xc0],
        vec![2, 0xc0],
    ] {
        assert!(ethereum::parse(&raw).is_err());
    }
}

#[test]
fn sdk_all_remaining_operations_preserve_atomic_order_and_expiry_pair() {
    let ops = ethereum::operations(&fixture("batch")).unwrap();
    assert_eq!(ops.len(), 4);
    let ArkivOperation::Patch { key, set, remove } = &ops[0] else {
        panic!("patch")
    };
    assert_eq!(key.as_bytes(), &[0xab; 32]);
    assert!(set.is_empty());
    assert_eq!(remove, &[CellName::new("title").unwrap()]);
    let ArkivOperation::ExtendExpiry { expiry, .. } = &ops[1] else {
        panic!("extend")
    };
    assert_eq!(expiry.expires_at, 123);
    assert_eq!(expiry.min_lifetime, 30);
    let ArkivOperation::TransferOwnership { owner, .. } = &ops[2] else {
        panic!("ownership")
    };
    assert_eq!(
        owner,
        Signer::from_bytes([2; 32]).unwrap().address().as_bytes()
    );
    assert!(matches!(&ops[3], ArkivOperation::Delete { .. }));
}

#[test]
fn legacy_transfer_maps_value_and_unsupported_semantics_are_rejected() {
    let g = genesis();
    let tx = SignedTx::from_ethereum(&g, &fixture("transfer")).unwrap();
    assert!(
        matches!(tx.tx.ops.as_slice(), [TxOp::Transfer { amount, .. }] if *amount == U256::from(123))
    );
    for name in [
        "lowFee",
        "unprotected",
        "contractCreation",
        "wrongDestination",
        "arkivValue",
    ] {
        assert!(
            SignedTx::from_ethereum(&g, &fixture(name)).is_err(),
            "{name}"
        );
    }
}

#[test]
fn ethereum_original_auth_survives_mining_and_independent_full_replay() {
    use arkiv_db::{decode_exact, durable::Options};
    use arkiv_db_memory::MemoryVersionedStore;
    use arkiv_node::signed::{history::History, host::Host};
    use std::sync::Arc;
    let proposer = Signer::from_bytes([1; 32]).unwrap();
    let g = signed_support::genesis(9300, 1000, [7; 16], 1, 1000, &[2]);
    let make_host = |producer: bool| {
        Host::create(
            Arc::new(MemoryVersionedStore::new()),
            History::create(Arc::new(MemoryVersionedStore::new()), g.hash()).unwrap(),
            g.clone(),
            [9; 16],
            SignedHeader::sign(&g, g.header(), &proposer).unwrap(),
            producer.then(|| Signer::from_bytes([1; 32]).unwrap()),
            Options::default(),
        )
        .unwrap()
    };
    let mut producer = make_host(true);
    let mut follower = make_host(false);
    producer.produce().unwrap();
    follower.import(producer.block(1).unwrap()).unwrap();
    let raw = fixture("eip1559");
    let tx = SignedTx::from_ethereum(&g, &raw).unwrap();
    producer.submit(tx.clone()).unwrap();
    producer.produce().unwrap();
    let block = producer.block(2).unwrap();
    assert_eq!(
        block.outcomes.txs[0].status,
        arkiv_node::TxStatus::Committed
    );
    let stored: SignedBlock = decode_exact(&encode(&block)).unwrap();
    assert_eq!(
        stored.inputs.txs[0].ethereum_raw.as_deref(),
        Some(raw.as_slice())
    );
    assert_eq!(stored.inputs.txs[0].hash(), keccak256(&raw));
    follower.import(stored).unwrap();
    assert_eq!(producer.head, follower.head);
    let actor = Signer::from_bytes([2; 32]).unwrap().address();
    assert_eq!(producer.committed_nonce(actor).unwrap(), 1);
    assert_eq!(
        producer.account(2, actor).unwrap(),
        follower.account(2, actor).unwrap()
    );
    assert_eq!(
        producer.request_status(actor, tx.tx.request_id).unwrap()["status"],
        "included"
    );
    assert_eq!(
        producer.submit(tx.clone()).unwrap(),
        producer.request_status(actor, tx.tx.request_id).unwrap()
    );
    // Query the UDC entity back the way the SDK does.
    let key = arkiv_node::arkiv::derive_entity_key(9300, actor.as_bytes(), 0, 9);
    let select = serde_json::json!({"key":true,"owner":true,"creator":true,"createdAt":true,"updatedAt":true,"expiresAt":true,"creationFlags":true,"attributeSchema":true,"attributes":true});
    let query = |host: &Host, source: &str| {
        arkiv_node::signed::rpc_reads::dispatch(
            host,
            "arkiv_query",
            &serde_json::json!([source, {"select": select, "limit": "0x1"}]),
        )
        .unwrap()
        .unwrap()
    };
    for host in [&producer, &follower] {
        let page = query(host, "arkiv.schema = str('posts') AND EXISTS(body)");
        assert_eq!(
            page["data"],
            serde_json::json!([]),
            "bytes are never indexed"
        );
        let page = query(host, "arkiv.schema = str('posts') AND score >= u64(10)");
        let entity = &page["data"][0];
        assert_eq!(entity["key"], arkiv_node::simulator::wire::hex(&key));
        assert_eq!(
            entity["attributeSchema"],
            serde_json::json!([
                {"name":"arkiv.schema","type":"str","indexed":true},
                {"name":"body","type":"bytes","indexed":false},
                {"name":"score","type":"u64","indexed":true},
                {"name":"title","type":"str","indexed":false},
            ])
        );
        assert_eq!(
            entity["attributes"],
            serde_json::json!([
                {"name":"arkiv.schema","type":"str","value":"posts"},
                {"name":"body","type":"bytes","value":"0x010203"},
                {"name":"score","type":"u64","value":"0x2a"},
                {"name":"title","type":"str","value":"hello"},
            ])
        );
        assert!(entity.get("payload").is_none() && entity.get("contentType").is_none());
        // The unindexed str is returned but never matched.
        assert_eq!(
            query(host, "title = str('hello')")["data"],
            serde_json::json!([])
        );
        assert_eq!(
            query(
                host,
                &format!("$key = key({})", arkiv_node::simulator::wire::hex(&key))
            )["data"][0]["attributes"],
            entity["attributes"]
        );
    }
}

#[test]
fn hostile_dynamic_array_counts_and_aliases_fail_before_expansion() {
    let mut huge = fixture("data");
    huge[36..68].fill(0xff);
    assert!(ethereum::operations(&huge).is_err());
    let mut aliased = fixture("batch");
    let first: [u8; 32] = aliased[68..100].try_into().unwrap();
    aliased[100..132].copy_from_slice(&first);
    assert!(ethereum::operations(&aliased).is_err());
}
