//! Real-socket signed submission, body replication and light proof verification.
use alloy_primitives::U256;
use arkiv_db::{
    CellName, Value as DbValue,
    durable::Options,
    proof::{EqRequest, VerifiedEq, verify_eq},
};
use arkiv_db_memory::MemoryVersionedStore;
use arkiv_node::{
    Expiry, TxEnvelope, TxOp,
    accounts::AccountConfig,
    arkiv::{ArkivOperation, entity_namespace},
    signed::{
        crypto::Signer,
        history::History,
        host::Host,
        light::Light,
        protocol::*,
        runtime::{self, Node, Runtime, Shared},
    },
    simulator::wire::{REQUEST_CAP, RESPONSE_CAP, hex, pack, proof_limits, unhex, unpack},
};
use axum::{
    Json, Router,
    extract::{Request, State},
    middleware,
    routing::post as route_post,
};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const TOKEN: &str = "signed-test-replication-token-0123456789";
struct Server {
    url: String,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn serve(router: Router) -> Server {
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", socket.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(socket, router).await.unwrap() });
    Server { url, task }
}
fn signer(who: u8) -> Signer {
    let mut key = [0; 32];
    key[31] = who;
    Signer::from_bytes(key).unwrap()
}
fn genesis() -> SignedGenesis {
    let funded = [1, 2, 3].map(|who| {
        (
            signer(who).address(),
            U256::from(10u64).pow(U256::from(24u64)),
        )
    });
    SignedGenesis::new(
        default_native_genesis(7, 1_000),
        [0x42; 16],
        *signer(1).address().as_bytes(),
        250,
        AccountConfig::new(funded.to_vec()).unwrap(),
    )
    .unwrap()
}
fn host(producer: bool) -> Host {
    let genesis = genesis();
    let cert = SignedHeader::sign(&genesis, genesis.header(), &signer(1)).unwrap();
    Host::create(
        Arc::new(MemoryVersionedStore::new()),
        History::create(Arc::new(MemoryVersionedStore::new()), genesis.hash()).unwrap(),
        genesis,
        [1; 16],
        cert,
        producer.then(|| signer(1)),
        Options::default(),
    )
    .unwrap()
}
fn create(who: u8, nonce: u64, salt: u128) -> SignedTx {
    let signer = signer(who);
    let mut request = [0; 32];
    request[0] = who;
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
                    salt,
                    flags: 0,
                    expiry: Expiry {
                        expires_at: 0,
                        min_lifetime: 1_000,
                    },
                    attributes: vec![
                        (CellName::new("group").unwrap(), DbValue::U64(1)),
                        (
                            CellName::new("$payload").unwrap(),
                            DbValue::bytes(b"private canonical payload"),
                        ),
                    ],
                },
            }],
        },
        &signer,
    )
    .unwrap()
}
async fn post(client: &reqwest::Client, url: &str, path: &str, body: Value) -> reqwest::Response {
    client
        .post(format!("{url}/signed/v1/{path}"))
        .json(&body)
        .send()
        .await
        .unwrap()
}
async fn get(client: &reqwest::Client, url: &str, path: &str) -> Value {
    let r = client
        .get(format!("{url}/signed/v1/{path}"))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "GET {path}: {}", r.status());
    r.json().await.unwrap()
}
async fn step(client: &reqwest::Client, url: &str) {
    let r = client
        .post(format!("{url}/signed/v1/control"))
        .bearer_auth(TOKEN)
        .json(&json!({"action":"step"}))
        .send()
        .await
        .unwrap();
    let status = r.status();
    let body: Value = r.json().await.unwrap();
    assert!(status.is_success(), "{body}");
}
fn head(rt: &Shared) -> arkiv_node::Header {
    rt.node.lock().unwrap().head()
}
fn query() -> Value {
    json!({"height":"2","namespace":"1","attribute":"group","valueType":"u64","value":"1","limit":1,"cursor":null})
}
fn light_rt(source: &str, producer: &Shared) -> Shared {
    let node = producer.node.lock().unwrap();
    let light = Light::create(
        Arc::new(MemoryVersionedStore::new()),
        genesis(),
        node.certificate(0).unwrap(),
    )
    .unwrap();
    Runtime::new(
        Node::Light {
            light: Box::new(light),
            identity: node.identity().clone(),
        },
        Some(source.into()),
        None,
        None,
    )
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn external_admission_full_replay_and_light_paginated_proofs() {
    let producer = Runtime::new(
        Node::Full(Box::new(host(true))),
        None,
        None,
        Some(TOKEN.into()),
    )
    .unwrap();
    let public = serve(
        runtime::public_router(producer.clone()).layer(middleware::from_fn(
            |r: Request, next: middleware::Next| async move {
                if r.uri().path().ends_with("query/eq") {
                    assert!(!r.headers().contains_key("authorization"));
                    assert!(!r.headers().contains_key("cookie"));
                }
                next.run(r).await
            },
        )),
    )
    .await;
    let bodies = Arc::new(AtomicUsize::new(0));
    let count = bodies.clone();
    let private = serve(
        runtime::private_router(producer.clone()).layer(middleware::from_fn(
            move |r: Request, next: middleware::Next| {
                let count = count.clone();
                async move {
                    if r.uri().path().contains("replication/blocks") {
                        count.fetch_add(1, Ordering::SeqCst);
                    }
                    next.run(r).await
                }
            },
        )),
    )
    .await;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let status = get(&client, &public.url, "status").await;
    assert_eq!(status["authentication"], "signed-proposer-v1");
    assert_eq!(status["consensus"], false);
    assert_eq!(status["finality"], false);
    assert_eq!(status["capabilities"]["signatures"], true);
    assert_eq!(
        status["capabilities"]["proofTrust"],
        "pinned-single-proposer-secp256k1"
    );
    let capabilities = &status["capabilities"];
    assert_eq!(capabilities["gas"]["profile"], GAS_PROFILE);
    assert_eq!(capabilities["gas"]["feesDebited"], true);
    assert_eq!(capabilities["genesisPrefill"], false);
    assert_eq!(
        capabilities["valueTypes"].as_array().unwrap().len(),
        arkiv_db::TypeTag::ALL.len()
    );
    assert!(capabilities.get("scalarTypes").is_none());
    assert!(capabilities.get("queryLanguage").is_none());
    let a = create(2, 0, 9);
    let b = create(2, 1, 10);
    let c = create(1, 1, 11);
    let rejected = post(
        &client,
        &public.url,
        "transactions",
        json!({"transactionBytes":pack(&a)}),
    )
    .await;
    assert_eq!(
        rejected.json::<Value>().await.unwrap()["error"],
        "ChainInitializing"
    );
    let response: runtime::GenesisResponse = client
        .get(format!("{}/signed/v1/genesis", public.url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    response
        .validate(genesis().hash(), genesis().proposer, 7)
        .unwrap();
    assert!(
        response
            .validate(genesis().hash(), *signer(2).address().as_bytes(), 7)
            .is_err()
    );
    step(&client, &private.url).await;
    for tx in [&b, &a, &c] {
        let reply = post(
            &client,
            &public.url,
            "transactions",
            json!({"transactionBytes":pack(tx)}),
        )
        .await;
        let status = reply.status();
        let body: Value = reply.json().await.unwrap();
        assert!(status.is_success(), "{body}");
        assert_eq!(body["status"], "pending");
        assert_eq!(body["transactionHash"], hex(tx.hash().as_slice()));
    }
    let duplicate = post(
        &client,
        &public.url,
        "transactions",
        json!({"transactionBytes":pack(&a)}),
    )
    .await
    .json::<Value>()
    .await
    .unwrap();
    assert_eq!(duplicate["status"], "pending");
    let nonce = get(
        &client,
        &public.url,
        &format!("accounts/{}/nonce", hex(signer(2).address().as_bytes())),
    )
    .await;
    assert_eq!(nonce["committedNextNonce"], "0");
    assert_eq!(nonce["pendingNextNonce"], "2");
    let mut forged = a.clone();
    forged.tx.actor = signer(3).address();
    assert!(
        !post(
            &client,
            &public.url,
            "transactions",
            json!({"transactionBytes":pack(&forged)})
        )
        .await
        .status()
        .is_success()
    );
    assert!(
        !post(
            &client,
            &public.url,
            "transactions",
            json!({"transactionBytes":pack(&a),"actor":hex(signer(3).address().as_bytes())})
        )
        .await
        .status()
        .is_success()
    );
    step(&client, &private.url).await;
    assert_eq!(head(&producer).height, 2);
    let included = post(
        &client,
        &public.url,
        "transactions",
        json!({"transactionBytes":pack(&a)}),
    )
    .await
    .json::<Value>()
    .await
    .unwrap();
    assert_eq!(included["status"], "included");
    assert_eq!(included["executionStatus"], "committed");
    let request = get(
        &client,
        &public.url,
        &format!(
            "requests/{}/{}",
            hex(a.tx.actor.as_bytes()),
            hex(&a.tx.request_id)
        ),
    )
    .await;
    assert_eq!(request["transactionHash"], hex(a.hash().as_slice()));
    let full = Runtime::new(
        Node::Full(Box::new(host(false))),
        Some(public.url.clone()),
        Some(private.url.clone()),
        Some(TOKEN.into()),
    )
    .unwrap();
    runtime::sync_once(full.clone()).await.unwrap();
    runtime::sync_once(full.clone()).await.unwrap();
    assert_eq!(head(&full), head(&producer));
    assert_eq!(bodies.load(Ordering::SeqCst), 2);
    let full_public = serve(runtime::public_router(full.clone())).await;
    let feed = get(&client, &full_public.url, "feed/blocks/2").await;
    assert_eq!(feed["header"]["height"], "2");
    assert_eq!(
        feed["header"]["stateRoot"],
        hex(head(&full).state_root.as_slice())
    );
    assert!(feed["transactions"].is_array());
    assert_eq!(
        post(
            &client,
            &full_public.url,
            "transactions",
            json!({"transactionBytes":pack(&a)})
        )
        .await
        .json::<Value>()
        .await
        .unwrap()["error"],
        "ProducerOnly"
    );
    let light = light_rt(&public.url, &producer);
    runtime::sync_once(light.clone()).await.unwrap();
    assert_eq!(head(&light), head(&producer));
    assert_eq!(bodies.load(Ordering::SeqCst), 2);
    let local = serve(runtime::public_router(light.clone())).await;
    assert_eq!(
        post(
            &client,
            &local.url,
            "transactions",
            json!({"transactionBytes":pack(&a)})
        )
        .await
        .json::<Value>()
        .await
        .unwrap()["error"],
        "ProducerOnly"
    );
    let mut q = query();
    let mut total = 0;
    let mut first = Value::Null;
    loop {
        let r = client
            .post(format!("{}/signed/v1/query/inspect", local.url))
            .bearer_auth("never-forward-this")
            .header("cookie", "private=1")
            .json(&q)
            .send()
            .await
            .unwrap();
        let status = r.status();
        let page: Value = r.json().await.unwrap();
        assert!(status.is_success(), "{page}");
        assert_eq!(page["snapshot"]["height"], "2");
        assert_eq!(page["postingCount"], 3);
        for field in ["namespace", "attribute", "valueType", "value", "limit"] {
            assert_eq!(page["query"][field], q[field]);
        }
        assert!(page["entities"][0]["creationFlags"]["readonly"].is_boolean());
        assert_eq!(page["inspection"]["version"], arkiv_db::LAYOUT_VERSION);
        let certificate: SignedHeader =
            unpack(page["certificateBytes"].as_str().unwrap(), 512).unwrap();
        certificate.authenticate(&genesis()).unwrap();
        let request: EqRequest = unpack(page["canonicalRequest"].as_str().unwrap(), 4096).unwrap();
        assert_eq!(request.snapshot.state_root, certificate.header.state_root);
        assert_eq!(request.snapshot.genesis_hash, genesis().hash());
        if total == 0 {
            let proof = unhex(page["canonicalProof"].as_str().unwrap(), RESPONSE_CAP).unwrap();
            assert!(matches!(
                verify_eq(
                    &request,
                    None,
                    &proof,
                    proof_limits(),
                    genesis().genesis.limits
                )
                .unwrap(),
                VerifiedEq::Page(_)
            ));
            first = page.clone();
        }
        total += page["rows"].as_array().unwrap().len();
        q["cursor"] = page["continuation"].clone();
        if q["cursor"].is_null() {
            break;
        }
    }
    assert_eq!(total, 3);
    assert_eq!(bodies.load(Ordering::SeqCst), 2);
    let mut changed = query();
    changed["cursor"] = first["continuation"].clone();
    changed["value"] = json!("2");
    let rejected = post(&client, &local.url, "query/verified", changed)
        .await
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(rejected["error"], "CursorMismatch");
    for (text, valid, supported) in [
        ("group = u64(1)", "valid", "supported"),
        ("group >= u64(1)", "valid", "unsupported"),
        ("group =", "invalid", "unsupported"),
    ] {
        let v = post(&client, &local.url, "query/classify", json!({"query":text}))
            .await
            .json::<Value>()
            .await
            .unwrap();
        assert_eq!(v["language"]["status"], valid);
        assert_eq!(v["execution"]["status"], supported);
        assert!(v.get("verification").is_none());
    }
    let text=post(&client,&local.url,"query/text",json!({"query":"group = u64(1)","requireProof":true,"height":"2","namespace":"1","limit":2,"cursor":null})).await.json::<Value>().await.unwrap();
    assert_eq!(text["verificationStatus"]["status"], "verified");
    assert_eq!(text["rows"].as_array().unwrap().len(), 2);
    let unsupported=post(&client,&local.url,"query/text",json!({"query":"group >= u64(1)","requireProof":true,"height":"2","namespace":"1","limit":2,"cursor":null})).await.json::<Value>().await.unwrap();
    assert_eq!(unsupported["error"], "UnsupportedProof");
    assert_eq!(unsupported["verificationStatus"]["status"], "not_executed");
    assert!(unsupported.get("rows").is_none());
    step(&client, &private.url).await;
    runtime::sync_once(full.clone()).await.unwrap();
    runtime::sync_once(light.clone()).await.unwrap();
    assert_eq!(head(&full), head(&light));
    assert_eq!(head(&light).height, 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn streamed_submission_caps_private_auth_and_bounded_jobs() {
    let producer = Runtime::new(
        Node::Full(Box::new(host(true))),
        None,
        None,
        Some(TOKEN.into()),
    )
    .unwrap();
    let public = serve(runtime::public_router(producer.clone())).await;
    let private = serve(runtime::private_router(producer.clone())).await;
    let client = reqwest::Client::new();
    let denied = client
        .get(format!("{}/signed/v1/replication/blocks/0", private.url))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 401);
    let denied = client
        .post(format!("{}/signed/v1/control", private.url))
        .bearer_auth("wrong-token")
        .json(&json!({"action":"step"}))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 401);
    assert_eq!(head(&producer).height, 0);
    let body = client
        .get(format!("{}/signed/v1/replication/blocks/0", private.url))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    let v: Value = body.json().await.unwrap();
    let block: SignedBlock = unpack(v["blockBytes"].as_str().unwrap(), BODY_CAP as usize).unwrap();
    block.authenticate(&genesis()).unwrap();
    let denied = client
        .get(format!("{}/signed/v1/replication/blocks/0", public.url))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert!(!denied.status().is_success());
    for (path, cap) in [
        ("transactions", 2 * TX_CAP as usize + 4096),
        ("query/eq", REQUEST_CAP),
    ] {
        let address = public.url.strip_prefix("http://").unwrap();
        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let prefix = format!(
            "POST /signed/v1/{path} HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n",
            cap + 1
        );
        stream.write_all(prefix.as_bytes()).await.unwrap();
        stream.write_all(&vec![b' '; cap + 1]).await.unwrap();
        stream.write_all(b"\r\n0\r\n\r\n").await.unwrap();
        let mut result = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut result))
            .await
            .unwrap()
            .unwrap();
        assert!(String::from_utf8_lossy(&result).starts_with("HTTP/1.1 422"));
    }
    // Four unfinished reads retain all admission permits; the fifth is refused immediately.
    let address = public.url.strip_prefix("http://").unwrap();
    let hold = || async move {
        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        stream.write_all(format!("POST /signed/v1/transactions HTTP/1.1\r\nHost: {address}\r\nContent-Length: 1000\r\nContent-Type: application/json\r\n\r\n{{").as_bytes()).await.unwrap();
        stream
    };
    let mut held = Vec::new();
    for _ in 0..4 {
        held.push(hold().await);
    }
    let mut saturated = false;
    for _ in 0..100 {
        if client
            .get(format!("{}/signed/v1/status", public.url))
            .send()
            .await
            .unwrap()
            .status()
            == 429
        {
            saturated = true;
            break;
        }
        // A status probe can win the race for a permit against a held
        // request whose headers the server has not processed yet; that held
        // request is then answered (refused) instead of waiting for its body.
        // Replace answered held requests so exactly four stay unfinished.
        for stream in &mut held {
            let mut byte = [0u8; 1];
            if let Ok(Ok(_)) =
                tokio::time::timeout(Duration::from_millis(1), stream.peek(&mut byte)).await
            {
                *stream = hold().await;
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        saturated,
        "all four incomplete requests acquired admission permits"
    );
    let rejected = post(
        &client,
        &public.url,
        "transactions",
        json!({"transactionBytes":pack(&create(2,0,2))}),
    )
    .await;
    assert_eq!(rejected.status(), 429);
    assert_eq!(
        rejected.json::<Value>().await.unwrap()["error"],
        "ServerBusy"
    );
    drop(held);
    assert_eq!(head(&producer).height, 0);
}

async fn mutate(State(source): State<String>, Json(body): Json<Value>) -> Json<Value> {
    let mut v: Value = runtime::fetch(
        &reqwest::Client::new(),
        &source,
        "/signed/v1/query/eq",
        Some(body),
        None,
        RESPONSE_CAP,
    )
    .await
    .unwrap();
    let mut proof = unhex(v["proof"].as_str().unwrap(), RESPONSE_CAP).unwrap();
    let midpoint = proof.len() / 2;
    proof[midpoint] ^= 1;
    v["proof"] = json!(hex(&proof));
    Json(v)
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn altered_or_truncated_proof_never_returns_verified_rows() {
    let mut h = host(true);
    h.produce().unwrap();
    h.submit(create(2, 0, 9)).unwrap();
    h.produce().unwrap();
    let producer = Runtime::new(Node::Full(Box::new(h)), None, None, None).unwrap();
    let source = serve(runtime::public_router(producer.clone())).await;
    let mutator = serve(
        Router::new()
            .route("/signed/v1/query/eq", route_post(mutate))
            .with_state(source.url.clone()),
    )
    .await;
    let dropper = serve(Router::new().route(
        "/signed/v1/query/eq",
        route_post(|| async {
            (
                axum::http::StatusCode::OK,
                [("content-type", "application/json")],
                "{",
            )
        }),
    ))
    .await;
    for bad in [&mutator, &dropper] {
        let light = light_rt(&bad.url, &producer);
        {
            let source = producer.node.lock().unwrap();
            let mut node = light.node.lock().unwrap();
            let Node::Light { light, .. } = &mut *node else {
                panic!()
            };
            light
                .append(
                    &[
                        source.certificate(1).unwrap(),
                        source.certificate(2).unwrap(),
                    ],
                    &source.certificate(2).unwrap(),
                )
                .unwrap();
        }
        let local = serve(runtime::public_router(light)).await;
        for endpoint in ["query/verified", "query/inspect"] {
            let r = post(&reqwest::Client::new(), &local.url, endpoint, query()).await;
            assert!(!r.status().is_success());
            let v: Value = r.json().await.unwrap();
            for field in ["rows", "verification", "inspection", "canonicalProof"] {
                assert!(v.get(field).is_none());
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn authentic_conflict_in_sampled_header_freezes_full_without_body_fetch() {
    let mut producer = host(true);
    producer.produce().unwrap();
    let mut full = host(false);
    full.import(producer.block(1).unwrap()).unwrap();
    let mut changed = full.head;
    changed.inputs_digest = arkiv_db::B256::repeat_byte(7);
    let conflict = SignedHeader::sign(&genesis(), changed, &signer(1)).unwrap();
    let response = json!({"identity":full.identity,"certificates":[],"sampled":pack(&conflict)});
    let source = serve(Router::new().route(
        "/signed/v1/headers",
        axum::routing::get(move || {
            let response = response.clone();
            async move { Json(response) }
        }),
    ))
    .await;
    let rt = Runtime::new(
        Node::Full(Box::new(full)),
        Some(source.url.clone()),
        None,
        None,
    )
    .unwrap();
    assert_eq!(
        runtime::sync_once(rt.clone()).await,
        Err(arkiv_node::signed::Error("ChainConflict"))
    );
    let node = rt.node.lock().unwrap();
    assert_eq!(runtime::status(&node)["health"], "chain-conflict");
    assert_eq!(node.head(), producer.head);
}

fn full_with_retained_history() -> (Host, Arc<MemoryVersionedStore>) {
    let genesis = genesis();
    let certificate = SignedHeader::sign(&genesis, genesis.header(), &signer(1)).unwrap();
    let store = Arc::new(MemoryVersionedStore::new());
    let history = History::create(store.clone(), genesis.hash()).unwrap();
    let full = Host::create(
        Arc::new(MemoryVersionedStore::new()),
        history,
        genesis,
        [1; 16],
        certificate,
        None,
        Options::default(),
    )
    .unwrap();
    (full, store)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn future_sampled_and_advertised_conflict_freezes_before_replication() {
    let mut producer = host(true);
    producer.produce().unwrap();
    let block = producer.block(1).unwrap();
    let mut changed = block.header.header;
    changed.inputs_digest = arkiv_db::B256::repeat_byte(17);
    let sampled = SignedHeader::sign(&genesis(), changed, &signer(1)).unwrap();
    let (full, history_store) = full_with_retained_history();
    assert_eq!(full.head.height, 0);
    let headers = json!({"identity":full.identity,"certificates":[pack(&block.header)],"sampled":pack(&sampled)});
    let bodies = Arc::new(AtomicUsize::new(0));
    let count = bodies.clone();
    let source = serve(
        Router::new()
            .route(
                "/signed/v1/headers",
                axum::routing::get(move || {
                    let headers = headers.clone();
                    async move { Json(headers) }
                }),
            )
            .route(
                "/signed/v1/replication/blocks/1",
                axum::routing::get(move || {
                    let count = count.clone();
                    let bytes = pack(&block);
                    async move {
                        count.fetch_add(1, Ordering::SeqCst);
                        Json(json!({"blockBytes":bytes}))
                    }
                }),
            ),
    )
    .await;
    let rt = Runtime::new(
        Node::Full(Box::new(full)),
        Some(source.url.clone()),
        Some(source.url.clone()),
        None,
    )
    .unwrap();
    assert_eq!(
        runtime::sync_once(rt.clone()).await,
        Err(arkiv_node::signed::Error("ChainConflict"))
    );
    assert_eq!(bodies.load(Ordering::SeqCst), 0);
    assert_eq!(head(&rt), genesis().header());
    assert_eq!(
        runtime::status(&rt.node.lock().unwrap())["health"],
        "chain-conflict"
    );
    drop(rt);
    assert!(matches!(
        History::open(history_store, genesis().hash()),
        Err(arkiv_node::signed::Error("ChainConflict"))
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn authentic_replication_certificate_conflict_persists_freeze_before_execution() {
    let mut producer = host(true);
    producer.produce().unwrap();
    let block = producer.block(1).unwrap();
    block.authenticate(&genesis()).unwrap();
    let mut changed = block.header.header;
    changed.inputs_digest = arkiv_db::B256::repeat_byte(18);
    let advertised = SignedHeader::sign(&genesis(), changed, &signer(1)).unwrap();
    let (full, history_store) = full_with_retained_history();
    let headers = json!({"identity":full.identity,"certificates":[pack(&advertised)],"sampled":pack(&advertised)});
    let bodies = Arc::new(AtomicUsize::new(0));
    let count = bodies.clone();
    let source = serve(
        Router::new()
            .route(
                "/signed/v1/headers",
                axum::routing::get(move || {
                    let headers = headers.clone();
                    async move { Json(headers) }
                }),
            )
            .route(
                "/signed/v1/replication/blocks/1",
                axum::routing::get(move || {
                    let count = count.clone();
                    let bytes = pack(&block);
                    async move {
                        count.fetch_add(1, Ordering::SeqCst);
                        Json(json!({"blockBytes":bytes}))
                    }
                }),
            ),
    )
    .await;
    let rt = Runtime::new(
        Node::Full(Box::new(full)),
        Some(source.url.clone()),
        Some(source.url.clone()),
        None,
    )
    .unwrap();
    assert_eq!(
        runtime::sync_once(rt.clone()).await,
        Err(arkiv_node::signed::Error("ChainConflict"))
    );
    assert_eq!(bodies.load(Ordering::SeqCst), 1);
    {
        let node = rt.node.lock().unwrap();
        assert_eq!(node.head(), genesis().header());
        assert_eq!(runtime::status(&node)["health"], "chain-conflict");
        let Node::Full(full) = &*node else { panic!() };
        assert_eq!(full.committed_nonce(signer(1).address()).unwrap(), 0);
    }
    drop(rt);
    assert!(matches!(
        History::open(history_store, genesis().hash()),
        Err(arkiv_node::signed::Error("ChainConflict"))
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn advertised_header_survives_failed_body_fetch_and_detects_later_conflict() {
    let mut producer = host(true);
    producer.produce().unwrap();
    let advertised = producer.certificate(1).unwrap();
    producer.produce().unwrap();
    let sampled = producer.certificate(2).unwrap();
    let mut changed = advertised.header;
    changed.inputs_digest = arkiv_db::B256::repeat_byte(29);
    let conflicting = SignedHeader::sign(&genesis(), changed, &signer(1)).unwrap();
    let (full, history_store) = full_with_retained_history();
    let first = json!({"identity":full.identity,"certificates":[pack(&advertised)],"sampled":pack(&sampled)});
    let second = json!({"identity":full.identity,"certificates":[pack(&conflicting)],"sampled":pack(&sampled)});
    let polls = Arc::new(AtomicUsize::new(0));
    let bodies = Arc::new(AtomicUsize::new(0));
    let count = bodies.clone();
    let source = serve(
        Router::new()
            .route(
                "/signed/v1/headers",
                axum::routing::get(move || {
                    let response = if polls.fetch_add(1, Ordering::SeqCst) == 0 {
                        first.clone()
                    } else {
                        second.clone()
                    };
                    async move { Json(response) }
                }),
            )
            .route(
                "/signed/v1/replication/blocks/1",
                axum::routing::get(move || {
                    count.fetch_add(1, Ordering::SeqCst);
                    async { axum::http::StatusCode::NOT_FOUND }
                }),
            ),
    )
    .await;
    let rt = Runtime::new(
        Node::Full(Box::new(full)),
        Some(source.url.clone()),
        Some(source.url.clone()),
        None,
    )
    .unwrap();
    assert!(runtime::sync_once(rt.clone()).await.is_err());
    assert_eq!(head(&rt), genesis().header());
    let fetches = bodies.load(Ordering::SeqCst);
    assert!(fetches > 0);
    assert_eq!(
        runtime::sync_once(rt.clone()).await,
        Err(arkiv_node::signed::Error("ChainConflict"))
    );
    assert_eq!(bodies.load(Ordering::SeqCst), fetches);
    assert_eq!(
        runtime::status(&rt.node.lock().unwrap())["health"],
        "chain-conflict"
    );
    drop(rt);
    assert!(matches!(
        History::open(history_store, genesis().hash()),
        Err(arkiv_node::signed::Error("ChainConflict"))
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn range_inspection_exposes_only_complete_authenticated_branching_witnesses() {
    use arkiv_db::proof::{RangeRequest, verify_range_inspected};
    let mut source = host(true);
    source.produce().unwrap();
    for (nonce, price) in [9, 10, 15, 20, 21].into_iter().enumerate() {
        let mut tx = create(2, nonce as u64, nonce as u128).tx;
        let TxOp::Arkiv {
            operation: ArkivOperation::Create { attributes, .. },
            ..
        } = &mut tx.ops[0]
        else {
            panic!()
        };
        attributes[0] = (CellName::new("price").unwrap(), DbValue::U64(price));
        source
            .submit(SignedTx::sign(&genesis(), nonce as u64, tx, &signer(2)).unwrap())
            .unwrap();
    }
    source.produce().unwrap();
    let producer = Runtime::new(Node::Full(Box::new(source)), None, None, None).unwrap();
    let public = serve(runtime::public_router(producer.clone())).await;
    let light = light_rt(&public.url, &producer);
    runtime::sync_once(light.clone()).await.unwrap();
    let local = serve(runtime::public_router(light)).await;
    let client = reqwest::Client::new();
    let query = json!({"height":"2","namespace":"1","attribute":"price","valueType":"u64",
        "lower":{"value":"10","inclusive":true},"upper":{"value":"20","inclusive":false}});
    let response = post(&client, &local.url, "query/range/inspect", query.clone()).await;
    let status = response.status();
    let result: Value = response.json().await.unwrap();
    assert!(status.is_success(), "{result}");
    assert_eq!(result["termCount"], 2);
    assert_eq!(result["postingCount"], 2);
    assert_eq!(result["complete"], true);
    assert_eq!(result["proofProfile"], "range-complete-v1");
    let certificate: SignedHeader =
        unpack(result["certificateBytes"].as_str().unwrap(), 512).unwrap();
    certificate.authenticate(&genesis()).unwrap();
    let canonical: RangeRequest =
        unpack(result["canonicalRequest"].as_str().unwrap(), 4096).unwrap();
    assert_eq!(canonical.snapshot.state_root, certificate.header.state_root);
    let bytes = unhex(result["canonicalProof"].as_str().unwrap(), RESPONSE_CAP).unwrap();
    verify_range_inspected(
        &canonical,
        &bytes,
        arkiv_node::simulator::wire::range_proof_limits().unwrap(),
        genesis().genesis.limits,
    )
    .unwrap();
    let normal = post(&client, &local.url, "query/range/verified", query.clone())
        .await
        .json::<Value>()
        .await
        .unwrap();
    for key in [
        "rows",
        "termCount",
        "postingCount",
        "snapshot",
        "certificateBytes",
    ] {
        assert_eq!(result[key], normal[key]);
    }
    assert!(normal.get("inspection").is_none());
    assert!(normal.get("canonicalProof").is_none());
    let raw = post(
        &client,
        &public.url,
        "query/range",
        json!({"request":result["canonicalRequest"]}),
    )
    .await
    .json::<Value>()
    .await
    .unwrap();
    assert_eq!(raw["proof"], result["canonicalProof"]);
    let inspection = &result["inspection"];
    assert_eq!(inspection["version"], arkiv_db::LAYOUT_VERSION);
    assert_eq!(
        inspection["stateComposition"]["stateRoot"],
        result["snapshot"]["stateRoot"]
    );
    assert_eq!(inspection["pointPaths"].as_array().unwrap().len(), 5);
    assert_eq!(inspection["postingSets"].as_array().unwrap().len(), 2);
    let interval = &inspection["interval"];
    assert_eq!(
        interval["lowerKeyNibbles"],
        &interval["lowerKey"].as_str().unwrap()[2..]
    );
    let nodes = interval["nodes"].as_array().unwrap();
    // The excluded endpoint 20 is never included; with whole-key exclusive
    // endpoints (state layout 2) its leaf is not even opened.
    assert!(
        !nodes
            .iter()
            .any(|node| node["leaf"]["value"] == "20" && node["leaf"]["included"] == true)
    );
    assert!(
        !nodes
            .iter()
            .any(|node| node["leaf"]["value"] == "9" || node["leaf"]["value"] == "21")
    );
    for (index, node) in nodes.iter().enumerate() {
        assert_eq!(node["index"], index);
        for child in node["children"].as_array().unwrap() {
            if child["decision"] == "open" {
                let next = child["childIndex"].as_u64().unwrap() as usize;
                assert_eq!(nodes[next]["parentIndex"], index);
                assert_eq!(nodes[next]["parentSlot"], child["slot"]);
            } else {
                assert!(child["childIndex"].is_null());
            }
        }
    }
    for change in [
        json!({"lower":{"value":"20","inclusive":true},"upper":{"value":"10","inclusive":true}}),
        json!({"lower":{"value":"10","inclusive":false},"upper":{"value":"15","inclusive":false}}),
        json!({"lower":null,"upper":{"value":"9","inclusive":false}}),
    ] {
        let mut empty = query.clone();
        for (key, value) in change.as_object().unwrap() {
            empty[key] = value.clone();
        }
        let response = post(&client, &local.url, "query/range/inspect", empty).await;
        assert!(response.status().is_success());
        let empty: Value = response.json().await.unwrap();
        assert_eq!(empty["rows"], json!([]));
        assert_eq!(empty["inspection"]["postingSets"], json!([]));
    }
    // Correctly authenticated headers cannot authorize a malformed witness.
    let bad = serve(Router::new().route(
        "/signed/v1/query/range",
        route_post(|| async { Json(json!({"proof":"0x00"})) }),
    ))
    .await;
    let light = light_rt(&bad.url, &producer);
    {
        let source = producer.node.lock().unwrap();
        let mut node = light.node.lock().unwrap();
        let Node::Light { light, .. } = &mut *node else {
            panic!()
        };
        light
            .append(
                &[
                    source.certificate(1).unwrap(),
                    source.certificate(2).unwrap(),
                ],
                &source.certificate(2).unwrap(),
            )
            .unwrap();
    }
    let rejected = serve(runtime::public_router(light)).await;
    let response = post(&client, &rejected.url, "query/range/inspect", query.clone()).await;
    assert!(!response.status().is_success());
    let rejected: Value = response.json().await.unwrap();
    for field in ["rows", "verification", "inspection", "canonicalProof"] {
        assert!(rejected.get(field).is_none());
    }
    let wrong_role = post(&client, &public.url, "query/range/inspect", query).await;
    assert_eq!(
        wrong_role.json::<Value>().await.unwrap()["error"],
        "UnsupportedNodeRole"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn state_explorer_walks_pinned_real_nodes_and_nested_postings() {
    let mut source = host(true);
    source.produce().unwrap();
    for nonce in 0..2 {
        source.submit(create(2, nonce, nonce as u128)).unwrap();
    }
    source.produce().unwrap();
    let producer = Runtime::new(Node::Full(Box::new(source)), None, None, None).unwrap();
    let public = serve(runtime::public_router(producer.clone())).await;
    let client = reqwest::Client::new();
    let read = |map: &str, path: Value, term_path: Value| json!({"height":"2","map":map,"namespace":if map=="catalog"{Value::Null}else{json!("1")},"path":path,"termPath":term_path});
    let response = post(
        &client,
        &public.url,
        "state/explore",
        read("catalog", json!([]), json!([])),
    )
    .await;
    assert!(response.status().is_success());
    let catalog: Value = response.json().await.unwrap();
    assert_eq!(catalog["verification"]["status"], "exploratory");
    assert_eq!(
        catalog["snapshot"]["stateRoot"],
        catalog["stateComposition"]["stateRoot"]
    );
    let header: arkiv_db::StateHeader = unpack(
        catalog["stateComposition"]["encodedHeader"]
            .as_str()
            .unwrap(),
        65536,
    )
    .unwrap();
    assert_eq!(
        hex(header.root().as_slice()),
        catalog["snapshot"]["stateRoot"]
    );
    assert_eq!(catalog["node"]["hash"], catalog["map"]["root"]);
    assert_eq!(catalog["node"]["decoded"]["namespace"], "1");
    let maps = catalog["node"]["decoded"]["maps"].as_array().unwrap();
    for map in maps {
        let r = post(
            &client,
            &public.url,
            "state/explore",
            read(map["kind"].as_str().unwrap(), json!([]), json!([])),
        )
        .await;
        assert!(r.status().is_success());
        let r: Value = r.json().await.unwrap();
        assert_eq!(r["map"]["root"], map["root"]);
    }
    let mut pending = vec![(json!([]), None)];
    let mut term = None;
    while let Some((path, expected)) = pending.pop() {
        let r = post(
            &client,
            &public.url,
            "state/explore",
            read("terms", path, json!([])),
        )
        .await;
        assert!(r.status().is_success());
        let r: Value = r.json().await.unwrap();
        let n = &r["node"];
        assert_eq!(r["snapshot"], catalog["snapshot"]);
        let bytes = unhex(n["rlp"].as_str().unwrap(), 1024 * 1024).unwrap();
        assert_eq!(
            hex(alloy_primitives::keccak256(bytes).as_slice()),
            n["hash"]
        );
        if let Some(expected) = expected {
            assert_eq!(n["hash"], expected);
        }
        if n["decoded"]["attribute"] == "group" && n["decoded"]["existence"] == false {
            term = Some(n.clone());
            break;
        }
        for c in n["children"].as_array().unwrap() {
            if c["kind"] != "empty" {
                pending.push((c["path"].clone(), Some(c["hash"].clone())));
            }
        }
    }
    let term = term.unwrap();
    let mut pending = vec![(json!([]), term["decoded"]["postingRoot"].clone())];
    let mut ids = vec![];
    while let Some((path, expected)) = pending.pop() {
        let r = post(
            &client,
            &public.url,
            "state/explore",
            read("postings", path, term["path"].clone()),
        )
        .await;
        assert!(r.status().is_success());
        let r: Value = r.json().await.unwrap();
        let n = &r["node"];
        assert_eq!(n["hash"], expected);
        if let Some(id) = n["decoded"]["recordId"].as_str() {
            ids.push(id.to_owned());
        }
        for c in n["children"].as_array().unwrap() {
            if c["kind"] != "empty" {
                pending.push((c["path"].clone(), c["hash"].clone()));
            }
        }
    }
    ids.sort();
    assert_eq!(ids, ["1", "2"]);
    for request in [
        json!({"height":"999","map":"catalog"}),
        json!({"height":"2","map":"host"}),
        json!({"height":"2","map":"catalog","path":[17]}),
        json!({"height":"2","map":"catalog","path":vec![0;329]}),
        json!({"height":"2","map":"catalog","path":[16]}),
        json!({"height":"2","map":"rows","namespace":"999"}),
        json!({"height":"2","map":"postings","namespace":"1","termPath":[]}),
    ] {
        let r = post(&client, &public.url, "state/explore", request).await;
        assert!(!r.status().is_success());
    }
    let light = light_rt(&public.url, &producer);
    runtime::sync_once(light.clone()).await.unwrap();
    let local = serve(runtime::public_router(light)).await;
    let r = post(
        &client,
        &local.url,
        "state/explore",
        read("catalog", json!([]), json!([])),
    )
    .await;
    assert_eq!(
        r.json::<Value>().await.unwrap()["error"],
        "UnsupportedNodeRole"
    );
    let old = post(&client, &public.url, "state/explore", json!({"height":"0"}))
        .await
        .json::<Value>()
        .await
        .unwrap();
    assert!(old["node"].is_null());
    assert_eq!(old["map"]["count"], "0");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn state_api_documentation_is_public_and_independent_of_storage_lock() {
    let rt = Runtime::new(Node::Full(Box::new(host(true))), None, None, None).unwrap();
    let public = serve(runtime::public_router(rt.clone())).await;
    let client = reqwest::Client::new();
    // Documentation remains available even while the node is doing storage
    // work: another thread holds the node lock until the checks are done.
    let (locked_tx, locked_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let holder = {
        let rt = rt.clone();
        std::thread::spawn(move || {
            let _locked = rt.node.lock().unwrap();
            locked_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        })
    };
    locked_rx.recv().unwrap();
    for path in ["/docs", "/docs/"] {
        let response = client
            .get(format!("{}{path}", public.url))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert!(
            response.headers()["content-type"]
                .to_str()
                .unwrap()
                .starts_with("text/html")
        );
        let html = response.text().await.unwrap();
        assert!(html.contains("@scalar/api-reference@1.72.2/"));
        assert!(html.contains("url: '/openapi.json'"));
        assert!(!html.contains("proxy.scalar.com"));
    }
    let response = client
        .get(format!("{}/openapi.json", public.url))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "application/json");
    let spec: Value = response.json().await.unwrap();
    release_tx.send(()).unwrap();
    holder.join().unwrap();
    assert_eq!(spec["openapi"], "3.1.0");
    assert_eq!(spec["servers"][0]["url"], "/");
    assert_eq!(
        spec["components"]["schemas"]["Path"]["maxItems"],
        arkiv_db::engine::explore::MAX_TREE_PATH
    );
    let operation = &spec["paths"]["/signed/v1/state/explore"]["post"];
    let example =
        &operation["requestBody"]["content"]["application/json"]["examples"]["catalog"]["value"];
    let response = post(&client, &public.url, "state/explore", example.clone()).await;
    assert_eq!(response.status(), 200);
    let result: Value = response.json().await.unwrap();
    assert_eq!(result["snapshot"]["height"], example["height"]);
    assert!(result["node"].is_null());
    let error = post(
        &client,
        &public.url,
        "state/explore",
        json!({"height":"0", "map":"invalid"}),
    )
    .await;
    assert_eq!(error.status(), 422);
    assert_eq!(
        error.json::<Value>().await.unwrap(),
        operation["responses"]["422"]["content"]["application/json"]["examples"]["InvalidRequest"]
            ["value"]
    );
}
