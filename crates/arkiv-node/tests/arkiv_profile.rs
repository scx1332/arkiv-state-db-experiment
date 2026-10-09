//! Isolated v2 producer/full/light acceptance. No running service or network.
#![cfg(feature = "arkiv-v2")]
use arkiv_db::{
    durable::Options,
    encode,
    proof::{VerifiedEq, verify_eq},
};
use arkiv_db_memory::MemoryVersionedStore;
use arkiv_node::simulator::{
    host::{Host, RunConfig},
    light::{Light, VerifiedRequest},
    wire::*,
    workload::Workload,
};
use std::sync::Arc;

fn run() -> RunConfig {
    RunConfig {
        genesis: arkiv_node::demo::workload::genesis(9002, 0),
        workload: Workload::default(),
    }
}
fn request(
    height: u64,
    name: &str,
    ty: &str,
    value: serde_json::Value,
    limit: u32,
) -> VerifiedRequest {
    VerifiedRequest {
        height: height.to_string(),
        namespace: "1".into(),
        attribute: name.into(),
        value_type: ty.into(),
        value,
        limit,
        cursor: None,
    }
}
fn inspect(host: &Host, light: &mut Light, r: &VerifiedRequest) -> serde_json::Value {
    let (q, p) = light.request(r).unwrap();
    let proof = host.proof(&q).unwrap();
    let value = light.inspect(r, &q, p.as_ref(), &proof).unwrap();
    assert_eq!(value["inspection"]["version"], arkiv_db::LAYOUT_VERSION);
    assert_eq!(value["inspection"]["proofProfile"], "eq-page-v2");
    let mut changed = proof.clone();
    let last = changed.len() - 1;
    changed[last] ^= 1;
    assert!(light.inspect(r, &q, p.as_ref(), &changed).is_err());
    assert!(matches!(
        verify_eq(
            &q,
            p.as_ref(),
            &proof,
            proof_limits(),
            host.run.genesis.limits
        )
        .unwrap(),
        VerifiedEq::Page(_)
    ));
    value
}
#[test]
fn lifecycle_history_full_replay_header_only_restart_and_typed_proofs() {
    let store = Arc::new(MemoryVersionedStore::new());
    let mut host =
        Host::create(store.clone(), run(), [1; 16], [2; 16], Options::default()).unwrap();
    let full_store = Arc::new(MemoryVersionedStore::new());
    let mut full = Host::create(
        full_store.clone(),
        run(),
        [1; 16],
        [2; 16],
        Options::default(),
    )
    .unwrap();
    let light_store = Arc::new(MemoryVersionedStore::new());
    let mut light = Light::create(light_store.clone(), host.durable_identity.clone()).unwrap();
    for height in 1..=20 {
        host.produce(None).unwrap();
        let block = host.block(height).unwrap();
        assert!(
            block
                .block
                .outcomes
                .txs
                .iter()
                .all(|t| t.status == arkiv_node::TxStatus::Committed),
            "block {height}: {:?}",
            block.block.outcomes
        );
        assert!(
            !String::from_utf8(json_bytes(&block.feed, FEED_CAP).unwrap())
                .unwrap()
                .contains(&"S".repeat(32))
        );
        full.import(block).unwrap();
        light.append(&[host.head], &host.head).unwrap();
        assert_eq!(host.head, full.head);
        assert_eq!(
            host.engine.head_snapshot().state_root(),
            full.engine.head_snapshot().state_root()
        );
    }
    let alice = hex(&[0xa1; 20]);
    let bob = hex(&[0xb0; 20]);
    for (height, name, ty, value, count) in [
        (3, "$owner", "addr", alice.clone(), 5),
        (5, "$owner", "addr", alice.clone(), 4),
        (5, "$owner", "addr", bob, 1),
        (5, "$creator", "addr", alice, 5),
        (5, "$createdAt", "u64", "2".into(), 5),
        (5, "$updatedAt", "u64", "5".into(), 1),
        (6, "$updatedAt", "u64", "6".into(), 1),
        (13, "$expiresAt", "u64", "14".into(), 1),
        (14, "$expiresAt", "u64", "14".into(), 0),
        (3, "$contentType", "str", "application/json".into(), 5),
        (3, "counter", "i32", i32::MIN.to_string(), 5),
        (
            3,
            "quantity",
            "u256",
            alloy_primitives::U256::MAX.to_string(),
            5,
        ),
        (3, "price", "dec", "-12.34567890123456789".into(), 5),
        (3, "checksum", "bytes32", hex(&[0x42; 32]), 5),
        (3, "reference", "key", hex(&[0x77; 32]), 5),
    ] {
        let v = inspect(
            &host,
            &mut light,
            &request(height, name, ty, value.into(), 64),
        );
        assert_eq!(v["postingCount"], count, "{height} {name}");
    }
    let mut paged = request(3, "group", "u64", "1".into(), 3);
    let first = inspect(&host, &mut light, &paged);
    assert_eq!(first["rows"].as_array().unwrap().len(), 3);
    paged.cursor = first["continuation"].as_str().map(str::to_owned);
    let second = inspect(&host, &mut light, &paged);
    assert_eq!(second["rows"].as_array().unwrap().len(), 2);
    paged.height = "4".into();
    assert!(light.request(&paged).is_err());
    for (name, ty, value) in [
        ("$owner", "key", hex(&[0xa1; 32])),
        ("$payload", "str", "".into()),
        ("$expiration", "u64", "14".into()),
        ("counter", "i64", "-1".into()),
    ] {
        assert!(
            light
                .request(&request(3, name, ty, value.into(), 64))
                .is_err()
        );
    }
    let id = host.identity.clone();
    let head = host.head;
    drop(host);
    drop(full);
    drop(light);
    let host = Host::open(store, Some(&id), Options::default()).unwrap();
    let full = Host::open(full_store, Some(&id), Options::default()).unwrap();
    let mut light = Light::open(light_store, &id).unwrap();
    assert_eq!(encode(&host.head), encode(&head));
    assert_eq!(full.head, head);
    assert_eq!(light.head, head);
    assert_eq!(
        inspect(
            &host,
            &mut light,
            &request(3, "group", "u64", "1".into(), 64)
        )["postingCount"],
        5
    );
}

#[test]
fn maximum_payload_is_provable_without_leaking_into_explorer_feed() {
    let mut config = run();
    config.workload.payload_bytes = 128 * 1024;
    let mut host = Host::create(
        Arc::new(MemoryVersionedStore::new()),
        config,
        [3; 16],
        [4; 16],
        Options::default(),
    )
    .unwrap();
    let mut light = Light::create(
        Arc::new(MemoryVersionedStore::new()),
        host.durable_identity.clone(),
    )
    .unwrap();
    for _ in 0..2 {
        host.produce(None).unwrap();
        light.append(&[host.head], &host.head).unwrap();
    }
    assert!(
        host.block(2)
            .unwrap()
            .block
            .outcomes
            .txs
            .iter()
            .all(|t| t.status == arkiv_node::TxStatus::Committed)
    );
    let feed = json_bytes(&host.block(2).unwrap().feed, FEED_CAP).unwrap();
    assert!(feed.len() < 20_000);
    assert!(!String::from_utf8(feed).unwrap().contains(&"S".repeat(32)));
    let page = inspect(
        &host,
        &mut light,
        &request(2, "group", "u64", "1".into(), 64),
    );
    assert_eq!(
        page["entities"][0]["payload"].as_str().unwrap().len(),
        2 + 2 * 128 * 1024
    );
    assert!(json_bytes(&page, RESPONSE_CAP).is_ok());
    let (q, previous) = light
        .request(&request(2, "group", "u64", "1".into(), 64))
        .unwrap();
    let mut caps = proof_limits();
    caps.max_proof_bytes = 1024;
    assert!(
        host.engine
            .head_snapshot()
            .prove_eq(&q, caps, arkiv_db::Budget::unlimited())
            .is_err()
    );
    let proof = host.proof(&q).unwrap();
    assert!(verify_eq(&q, previous.as_ref(), &proof, caps, host.run.genesis.limits).is_err());
}

#[tokio::test]
async fn http_light_inspection_is_typed_bounded_and_independently_verifiable() {
    use arkiv_node::simulator::runtime::{self, Node, Runtime};
    async fn server(router: axum::Router) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        (
            url,
            tokio::spawn(async move { axum::serve(listener, router).await.unwrap() }),
        )
    }
    let mut host = Host::create(
        Arc::new(MemoryVersionedStore::new()),
        run(),
        [5; 16],
        [6; 16],
        Options::default(),
    )
    .unwrap();
    for _ in 0..15 {
        host.produce(None).unwrap();
    }
    let limits = host.run.genesis.limits;
    let mut light = Light::create(
        Arc::new(MemoryVersionedStore::new()),
        host.durable_identity.clone(),
    )
    .unwrap();
    light
        .append(
            &(1..=15)
                .map(|h| host.header(h).unwrap())
                .collect::<Vec<_>>(),
            &host.head,
        )
        .unwrap();
    let (source, source_task) = server(runtime::public_router(
        Runtime::new(Node::Full(Box::new(host)), None, None).unwrap(),
    ))
    .await;
    let (local, local_task) = server(runtime::public_router(
        Runtime::new(Node::Light(Box::new(light)), Some(source.clone()), None).unwrap(),
    ))
    .await;
    let client = reqwest::Client::new();
    let status: serde_json::Value = client
        .get(format!("{local}/sim/v1/status"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["capabilities"]["profile"], "arkiv-entity-v2");
    assert_eq!(status["protocolVersion"], 3);
    let r = request(5, "$owner", "addr", hex(&[0xb0; 20]).into(), 64);
    let response = client
        .post(format!("{local}/sim/v1/query/inspect"))
        .json(&r)
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    let value: serde_json::Value = response.json().await.unwrap();
    assert_eq!(value["entities"][0]["owner"], hex(&[0xb0; 20]));
    let query = unpack(value["canonicalRequest"].as_str().unwrap(), REQUEST_CAP).unwrap();
    let bytes = unhex(value["canonicalProof"].as_str().unwrap(), RESPONSE_CAP).unwrap();
    assert!(verify_eq(&query, None, &bytes, proof_limits(), limits).is_ok());
    let unsupported = request(5, "$owner", "bytes32", hex(&[0xb0; 32]).into(), 64);
    let response = client
        .post(format!("{local}/sim/v1/query/inspect"))
        .json(&unsupported)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    let error: serde_json::Value = response.json().await.unwrap();
    assert!(error.get("rows").is_none());
    let oversized = client
        .post(format!("{local}/sim/v1/query/inspect"))
        .header("content-type", "application/json")
        .body(" ".repeat(REQUEST_CAP + 1))
        .send()
        .await
        .unwrap();
    assert_eq!(oversized.status(), 400);
    let error: serde_json::Value = oversized.json().await.unwrap();
    assert!(error.get("rows").is_none());
    // Text classification is independent of execution and never labels a
    // syntactically eligible Eq as already verified.
    for (query_text, language, execution) in [
        ("group = u64(1)", "valid", "supported"),
        ("group >= u64(1)", "valid", "unsupported"),
        ("NOT group = u64(1)", "valid", "unsupported"),
        (
            "group = u64(1) AND counter = i32(1)",
            "valid",
            "unsupported",
        ),
        ("group =", "invalid", "unsupported"),
    ] {
        let c: serde_json::Value = client
            .post(format!("{local}/sim/v1/query/classify"))
            .json(&serde_json::json!({"query":query_text}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(c["language"]["status"], language, "{query_text}: {c}");
        assert_eq!(c["execution"]["status"], execution, "{query_text}: {c}");
        assert!(c.get("verification").is_none());
    }
    for require_proof in [true, false] {
        let mut body = serde_json::json!({"query":"((group = u64(1)))", "requireProof":require_proof,
            "height":"3","namespace":"1","limit":2,"cursor":null});
        let response = client
            .post(format!("{local}/sim/v1/query/text"))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success());
        let page: serde_json::Value = response.json().await.unwrap();
        assert_eq!(page["verificationStatus"]["status"], "verified");
        assert_eq!(page["rows"].as_array().unwrap().len(), 2);
        let query = unpack(page["canonicalRequest"].as_str().unwrap(), REQUEST_CAP).unwrap();
        let proof = unhex(page["canonicalProof"].as_str().unwrap(), RESPONSE_CAP).unwrap();
        assert!(verify_eq(&query, None, &proof, proof_limits(), limits).is_ok());
        body["query"] = "group >= u64(1)".into();
        let response = client
            .post(format!("{local}/sim/v1/query/text"))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 422);
        let error: serde_json::Value = response.json().await.unwrap();
        assert_eq!(
            error["error"],
            if require_proof {
                "UnsupportedProof"
            } else {
                "UnsupportedQueryExecution"
            }
        );
        assert_eq!(error["classification"]["language"]["status"], "valid");
        assert_eq!(error["verificationStatus"]["status"], "not_executed");
        assert!(error.get("rows").is_none());
    }
    let response = client
        .post(format!("{source}/sim/v1/query/text"))
        .json(
            &serde_json::json!({"query":"group = u64(1)","requireProof":true,
            "height":"3","namespace":"1","limit":2}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 422);
    let error: serde_json::Value = response.json().await.unwrap();
    assert_eq!(error["error"], "UnsupportedNodeRole");
    let response = client
        .post(format!("{local}/sim/v1/query/text"))
        .json(&serde_json::json!({"query":"group = u64(1)",
            "height":"3","namespace":"1","limit":2}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400); // requireProof is an explicit decision.
    source_task.abort();
    local_task.abort();
}

#[tokio::test]
async fn text_query_reports_runtime_proof_limits_without_downgrade() {
    use arkiv_node::simulator::runtime::{self, Node, Runtime};
    for code in ["ProofLimitExceeded", "BudgetExceeded", "InvalidProof"] {
        let mut host = Host::create(
            Arc::new(MemoryVersionedStore::new()),
            run(),
            [8; 16],
            [9; 16],
            Options::default(),
        )
        .unwrap();
        for _ in 0..3 {
            host.produce(None).unwrap();
        }
        let mut light = Light::create(
            Arc::new(MemoryVersionedStore::new()),
            host.durable_identity.clone(),
        )
        .unwrap();
        light
            .append(
                &(1..=3).map(|h| host.header(h).unwrap()).collect::<Vec<_>>(),
                &host.head,
            )
            .unwrap();
        let peer = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let peer_url = format!("http://{}", peer.local_addr().unwrap());
        let router = axum::Router::new().route(
            "/sim/v1/query/eq",
            axum::routing::post(move || async move {
                (
                    axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                    axum::Json(serde_json::json!({"error":code})),
                )
            }),
        );
        let peer_task = tokio::spawn(async move { axum::serve(peer, router).await.unwrap() });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let router = runtime::public_router(
            Runtime::new(Node::Light(Box::new(light)), Some(peer_url), None).unwrap(),
        );
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let response = reqwest::Client::new().post(format!("{url}/sim/v1/query/text"))
            .json(&serde_json::json!({"query":"group = u64(1)","requireProof":false,"height":"3","namespace":"1","limit":2}))
            .send().await.unwrap();
        assert_eq!(response.status(), 422);
        let error: serde_json::Value = response.json().await.unwrap();
        assert_eq!(error["classification"]["proof"]["status"], "supported");
        assert_eq!(
            error["verificationStatus"]["status"],
            if code == "InvalidProof" {
                "failed"
            } else {
                "resource_limit"
            }
        );
        assert!(error.get("rows").is_none());
        assert!(error.get("canonicalProof").is_none());
        task.abort();
        peer_task.abort();
    }
}
