//! State retention on a signed full node: every state consumer refuses
//! heights below the floor with an explicit HistoryPruned error, serves the
//! floor and the head, and archive (the default) is unchanged.
use alloy_primitives::{U256, keccak256};
use arkiv_db::{
    CellName, Value as DbValue,
    durable::{Options, Retention},
};
use arkiv_db_memory::MemoryVersionedStore;
use arkiv_node::{
    Expiry, TxEnvelope, TxOp,
    accounts::AccountConfig,
    arkiv::{ARKIV_ADDRESS, ArkivOperation, entity_namespace},
    signed::{
        crypto::Signer,
        history::History,
        host::Host,
        protocol::*,
        rpc,
        runtime::{self, Node, Runtime, Shared},
    },
    simulator::wire::hex,
};
use serde_json::{Value, json};
use std::sync::Arc;

fn signer(who: u8) -> Signer {
    let mut key = [0; 32];
    key[31] = who;
    Signer::from_bytes(key).unwrap()
}
fn genesis() -> SignedGenesis {
    let funded = [1, 2].map(|who| {
        (
            signer(who).address(),
            U256::from(10u64).pow(U256::from(24u64)),
        )
    });
    SignedGenesis::new(
        default_native_genesis(7, 1_000),
        [0x43; 16],
        *signer(1).address().as_bytes(),
        250,
        AccountConfig::new(funded.to_vec()).unwrap(),
    )
    .unwrap()
}
fn create(nonce: u64) -> SignedTx {
    let signer = signer(2);
    let mut request = [0; 32];
    request[24..].copy_from_slice(&nonce.to_be_bytes());
    SignedTx::sign(
        &genesis(),
        nonce,
        TxEnvelope {
            chain_id: 7,
            actor: signer.address(),
            request_id: request,
            tx_budget: 2_000_000,
            ops: vec![TxOp::Arkiv {
                namespace: entity_namespace(),
                operation: ArkivOperation::Create {
                    salt: nonce as u128,
                    flags: 0,
                    expiry: Expiry {
                        expires_at: 0,
                        min_lifetime: 1_000,
                    },
                    attributes: vec![(CellName::new("group").unwrap(), DbValue::U64(1))],
                },
            }],
        },
        &signer,
    )
    .unwrap()
}

/// Head 6: block 1 bootstrap, creates in blocks 2 and 3, then empty blocks.
fn node(retention: Retention) -> (Shared, String) {
    let genesis = genesis();
    let cert = SignedHeader::sign(&genesis, genesis.header(), &signer(1)).unwrap();
    let mut host = Host::create(
        Arc::new(MemoryVersionedStore::new()),
        History::create(Arc::new(MemoryVersionedStore::new()), genesis.hash()).unwrap(),
        genesis,
        [1; 16],
        cert,
        Some(signer(1)),
        Options {
            retention,
            ..Options::default()
        },
    )
    .unwrap();
    host.produce().unwrap();
    let tx = create(0);
    let hash = hex(tx.hash().as_slice());
    host.submit(tx).unwrap();
    host.produce().unwrap();
    host.submit(create(1)).unwrap();
    host.produce().unwrap();
    for _ in 0..3 {
        host.produce().unwrap();
    }
    assert_eq!(host.head.height, 6);
    (
        Runtime::new(Node::Full(Box::new(host)), None, None, None).unwrap(),
        hash,
    )
}

async fn call(rt: &Shared, method: &str, params: Value) -> Value {
    rpc::handle_request(
        rt,
        json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}),
    )
    .await
}
fn ok(answer: &Value) -> &Value {
    assert!(answer.get("error").is_none(), "{answer}");
    &answer["result"]
}
fn pruned(answer: &Value, requested: u64, floor: u64) {
    assert_eq!(
        answer["error"],
        json!({"code":rpc::HISTORY_PRUNED_CODE,"message":format!("history pruned: requested {requested}, floor {floor}"),"data":{"requested":format!("0x{requested:x}"),"floor":format!("0x{floor:x}")}}),
        "{answer}"
    );
}
fn address() -> String {
    hex(signer(2).address().as_bytes())
}
fn entity_nonce_call() -> Value {
    let mut data = keccak256(b"entityNonce(address)")[..4].to_vec();
    data.extend([0; 12]);
    data.extend(signer(2).address().as_bytes());
    json!({"to":hex(&ARKIV_ADDRESS),"data":hex(&data)})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn json_rpc_refuses_state_below_the_floor() {
    assert_eq!(rpc::HISTORY_PRUNED_CODE, -32001);
    let (rt, tx_in_block_2) = node(Retention::Window(2));
    // Floor 4; receipts/logs need state h-1, so their floor is 5.
    for method in ["eth_getBalance", "eth_getTransactionCount"] {
        pruned(&call(&rt, method, json!([address(), "0x3"])).await, 3, 4);
        ok(&call(&rt, method, json!([address(), "0x4"])).await);
        ok(&call(&rt, method, json!([address(), "latest"])).await);
        pruned(
            &call(&rt, method, json!([address(), "earliest"])).await,
            0,
            4,
        );
    }
    pruned(
        &call(&rt, "eth_call", json!([entity_nonce_call(), "0x3"])).await,
        3,
        4,
    );
    assert_eq!(
        ok(&call(&rt, "eth_call", json!([entity_nonce_call(), "0x4"])).await)
            .as_str()
            .unwrap()
            .chars()
            .last(),
        Some('2')
    );
    pruned(
        &call(&rt, "arkiv_getEntityCount", json!([{"block":3}])).await,
        3,
        4,
    );
    assert_eq!(
        ok(&call(&rt, "arkiv_getEntityCount", json!([{"block":4}])).await),
        &json!(2)
    );
    pruned(
        &call(&rt, "arkiv_query", json!(["*", {"atBlock":"0x3"}])).await,
        3,
        4,
    );
    // Logs, receipts and block JSON (logsBloom) below the receipt floor.
    pruned(
        &call(
            &rt,
            "eth_getLogs",
            json!([{"fromBlock":"0x4","toBlock":"0x6"}]),
        )
        .await,
        4,
        5,
    );
    pruned(
        &call(
            &rt,
            "eth_getLogs",
            json!([{"fromBlock":"earliest","toBlock":"0x6"}]),
        )
        .await,
        1,
        5,
    );
    ok(&call(
        &rt,
        "eth_getLogs",
        json!([{"fromBlock":"0x5","toBlock":"latest"}]),
    )
    .await);
    pruned(
        &call(&rt, "eth_getBlockByNumber", json!(["0x4", false])).await,
        4,
        5,
    );
    ok(&call(&rt, "eth_getBlockByNumber", json!(["0x5", false])).await);
    // Block 0 has no logs and needs no state.
    ok(&call(&rt, "eth_getBlockByNumber", json!(["0x0", false])).await);
    pruned(
        &call(&rt, "eth_getTransactionReceipt", json!([tx_in_block_2])).await,
        2,
        5,
    );
    // Body-only reads stay available below the floor.
    assert_eq!(
        ok(&call(&rt, "eth_getTransactionByHash", json!([tx_in_block_2])).await)["blockNumber"],
        "0x2"
    );
    assert_eq!(
        ok(&call(&rt, "eth_getBlockTransactionCountByNumber", json!(["0x2"])).await),
        "0x1"
    );
    ok(&call(&rt, "eth_feeHistory", json!(["0x4", "0x3", []])).await);
    // A filter from below the floor is refused on read, not silently empty.
    let filter = ok(&call(&rt, "eth_newFilter", json!([{"fromBlock":"0x2"}])).await).clone();
    pruned(&call(&rt, "eth_getFilterLogs", json!([filter])).await, 2, 5);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn query_cursor_is_not_a_lease() {
    let (rt, _) = node(Retention::Window(2));
    let page = ok(&call(
        &rt,
        "arkiv_query",
        json!(["*", {"atBlock":"0x4","limit":"0x1"}]),
    )
    .await)
    .clone();
    let cursor = page["cursor"].as_str().unwrap().to_owned();
    // The floor passes height 4 between pages.
    {
        let mut node = rt.node.lock().unwrap();
        let Node::Full(host) = &mut *node else {
            unreachable!()
        };
        host.produce().unwrap();
    }
    pruned(
        &call(
            &rt,
            "arkiv_query",
            json!(["*", {"atBlock":"0x4","limit":"0x1","cursor":cursor}]),
        )
        .await,
        4,
        5,
    );
}

async fn serve(rt: Shared) -> (String, tokio::task::JoinHandle<()>) {
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", socket.local_addr().unwrap());
    let router = runtime::public_router(rt);
    let task = tokio::spawn(async move { axum::serve(socket, router).await.unwrap() });
    (url, task)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rest_answers_410_with_floor_details() {
    let (rt, _) = node(Retention::Window(2));
    let (url, task) = serve(rt).await;
    let client = reqwest::Client::new();
    let status: Value = client
        .get(format!("{url}/signed/v1/status"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["bodyRetention"], "all");
    assert_eq!(status["stateRetention"], "window");
    assert_eq!(status["stateRetentionWindow"], "2");
    assert_eq!(status["stateFloor"], "4");
    assert_eq!(status["receiptFloor"], "5");
    let gone = |height: u64| json!({"error":"HistoryPruned","requestedHeight":height.to_string(),"stateFloor":"4"});
    for path in ["", "/proof"] {
        let r = client
            .get(format!(
                "{url}/signed/v1/accounts/{}{path}?height=3",
                address()
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 410);
        assert_eq!(r.json::<Value>().await.unwrap(), gone(3));
        let r = client
            .get(format!(
                "{url}/signed/v1/accounts/{}{path}?height=4",
                address()
            ))
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success());
    }
    let explore = |height: &str| json!({"height":height,"map":"catalog"});
    let r = client
        .post(format!("{url}/signed/v1/state/explore"))
        .json(&explore("3"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 410);
    assert_eq!(r.json::<Value>().await.unwrap(), gone(3));
    let r = client
        .post(format!("{url}/signed/v1/state/explore"))
        .json(&explore("4"))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    // An equality proof request resolved at a pruned height.
    for (height, expect) in [("3", Some(gone(3))), ("6", None)] {
        let resolved: Value = client
            .post(format!("{url}/signed/v1/query/resolve"))
            .json(&json!({"height":height,"namespace":"1","attribute":"group","valueType":"u64","value":"1","limit":1,"cursor":null}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let r = client
            .post(format!("{url}/signed/v1/query/eq"))
            .json(&json!({"request":resolved["request"]}))
            .send()
            .await
            .unwrap();
        match expect {
            Some(body) => {
                assert_eq!(r.status(), 410);
                assert_eq!(r.json::<Value>().await.unwrap(), body);
            }
            None => assert!(r.status().is_success()),
        }
    }
    // Headers and bodies are retained in full.
    let r = client
        .get(format!("{url}/signed/v1/feed/blocks/1"))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn archive_default_serves_every_height() {
    assert_eq!(Options::default().retention, Retention::Archive);
    let (rt, tx) = node(Retention::Archive);
    ok(&call(&rt, "eth_getBalance", json!([address(), "0x0"])).await);
    ok(&call(
        &rt,
        "eth_getLogs",
        json!([{"fromBlock":"earliest","toBlock":"0x6"}]),
    )
    .await);
    ok(&call(&rt, "eth_getTransactionReceipt", json!([tx])).await);
    let status = runtime::status(&rt.node.lock().unwrap());
    assert_eq!(status["stateRetention"], "archive");
    assert_eq!(status["stateRetentionWindow"], Value::Null);
    assert_eq!(status["stateFloor"], "0");
    assert_eq!(status["receiptFloor"], "0");
}
