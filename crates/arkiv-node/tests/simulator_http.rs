//! Real sockets: private controls/body access, caps, full replay and local proof badge.
use arkiv_db::durable::Options;
use arkiv_db_memory::MemoryVersionedStore;
use arkiv_node::simulator::{
    host::{Host, RunConfig},
    light::Light,
    runtime::{self, Node, Runtime, Shared},
    wire::*,
    workload::Workload,
};
use axum::{
    Json, Router,
    extract::{Request, State},
    middleware,
    routing::post,
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
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    Server { url, task }
}
fn host() -> Host {
    Host::create(
        Arc::new(MemoryVersionedStore::new()),
        RunConfig {
            genesis: arkiv_node::demo::workload::genesis(9001, 1700000000000),
            workload: Workload::default(),
        },
        [1; 16],
        [7; 16],
        Options::default(),
    )
    .unwrap()
}
fn query() -> Value {
    json!({"height":"20","namespace":"1","attribute":"group","valueType":"u64","value":"1","limit":1,"cursor":null})
}
async fn get(client: &reqwest::Client, url: &str, path: &str) -> Value {
    client
        .get(format!("{url}/sim/v1/{path}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}
async fn request(
    client: &reqwest::Client,
    url: &str,
    path: &str,
    body: &Value,
) -> reqwest::Response {
    client
        .post(format!("{url}/sim/v1/{path}"))
        .json(body)
        .send()
        .await
        .unwrap()
}
fn height(rt: &Shared) -> u64 {
    let n = rt.node.lock().unwrap();
    match &*n {
        Node::Full(h) => h.head.height,
        Node::Light(h) => h.head.height,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_reports_storage_bytes_only_for_a_known_readable_file() {
    let client = reqwest::Client::new();
    let unknown = serve(runtime::public_router(
        Runtime::new(Node::Full(Box::new(host())), None, None).unwrap(),
    ))
    .await;
    assert!(
        get(&client, &unknown.url, "status")
            .await
            .get("storage")
            .is_none()
    );
    let directory =
        std::env::temp_dir().join(format!("arkiv-sim-status-storage-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let file = directory.join("node.redb");
    std::fs::write(&file, vec![0u8; 12_345]).unwrap();
    let mut known = Runtime::new(Node::Full(Box::new(host())), None, None).unwrap();
    Arc::get_mut(&mut known).unwrap().storage_path = Some(file.clone());
    let server = serve(runtime::public_router(known)).await;
    assert_eq!(
        get(&client, &server.url, "status").await["storage"]["fileBytes"],
        "12345"
    );
    std::fs::remove_file(&file).unwrap();
    // A file that disappears reports no storage rather than a stale or zero size.
    assert!(
        get(&client, &server.url, "status")
            .await
            .get("storage")
            .is_none()
    );
    std::fs::remove_dir_all(&directory).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_and_light_use_disjoint_endpoints_control_auth_and_chunked_limit() {
    let token = "private-test-bearer-secret-1234567890";
    let mut producer = Runtime::new(Node::Full(Box::new(host())), None, None).unwrap();
    Arc::get_mut(&mut producer).unwrap().admin_token = Some(token.into());
    let public = serve(runtime::public_router(producer.clone())).await;
    let bodies = Arc::new(AtomicUsize::new(0));
    let count = bodies.clone();
    let private = serve(
        runtime::control_router(producer.clone()).layer(middleware::from_fn(
            move |r: Request, next: middleware::Next| {
                let count = count.clone();
                async move {
                    if r.uri().path().contains("replication") {
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
    let command = json!({"commandId":hex(&[1;32]),"runId":status["runId"],"expectedRevision":"0","expectedHeight":"0","action":"step","config":null});
    assert_eq!(
        request(&client, &private.url, "control", &command)
            .await
            .status(),
        401
    );
    assert!(
        !request(&client, &public.url, "control", &command)
            .await
            .status()
            .is_success()
    );
    let response = client
        .post(format!("{}/sim/v1/control", private.url))
        .bearer_auth(token)
        .json(&command)
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    assert_eq!(response.json::<Value>().await.unwrap()["resultHeight"], "1");
    let repeat = client
        .post(format!("{}/sim/v1/control", private.url))
        .bearer_auth(token)
        .json(&command)
        .send()
        .await
        .unwrap();
    assert!(repeat.status().is_success());
    assert_eq!(height(&producer), 1);
    assert!(
        !client
            .get(format!("{}/sim/v1/replication/blocks/1", public.url))
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    {
        let mut n = producer.node.lock().unwrap();
        let Node::Full(h) = &mut *n else {
            unreachable!()
        };
        for _ in 1..40 {
            h.produce(None).unwrap();
        }
    }
    let id = {
        let n = producer.node.lock().unwrap();
        let Node::Full(h) = &*n else { unreachable!() };
        h.durable_identity.clone()
    };
    let light = Runtime::new(
        Node::Light(Box::new(
            Light::create(Arc::new(MemoryVersionedStore::new()), id).unwrap(),
        )),
        Some(public.url.clone()),
        None,
    )
    .unwrap();
    runtime::sync_once(light.clone()).await.unwrap();
    assert_eq!(height(&light), 40);
    assert_eq!(bodies.load(Ordering::SeqCst), 0);
    let local = serve(runtime::public_router(light.clone())).await;
    let mut q = query();
    let mut count = 0;
    loop {
        let r = request(&client, &local.url, "query/verified", &q).await;
        assert!(r.status().is_success());
        let v = r.json::<Value>().await.unwrap();
        assert_eq!(v["snapshot"]["height"], "20");
        assert_eq!(
            v["verification"],
            "proof verified against trusted simulator root; unsigned source"
        );
        count += v["rows"].as_array().unwrap().len();
        // Telemetry about the complete proof this light process fetched and
        // verified itself; present on every verified page.
        let diagnostics = &v["diagnostics"];
        assert_eq!(diagnostics["verifier"], "light-follower");
        assert!(
            diagnostics["proofBytes"]
                .as_str()
                .unwrap()
                .parse::<u64>()
                .unwrap()
                > 0
        );
        assert!(
            diagnostics["fetchMs"]
                .as_str()
                .unwrap()
                .parse::<u64>()
                .is_ok()
        );
        assert!(
            diagnostics["verifyMs"]
                .as_str()
                .unwrap()
                .parse::<u64>()
                .is_ok()
        );
        q["cursor"] = v["continuation"].clone();
        if q["cursor"].is_null() {
            break;
        }
    }
    assert_eq!(count, 3);
    assert_eq!(bodies.load(Ordering::SeqCst), 0);
    let mut follower = host();
    follower.role = "full";
    follower.paused = false;
    let mut full = Runtime::new(
        Node::Full(Box::new(follower)),
        Some(public.url.clone()),
        Some(private.url.clone()),
    )
    .unwrap();
    Arc::get_mut(&mut full).unwrap().replication_token = Some(token.into());
    for _ in 0..40 {
        runtime::sync_once(full.clone()).await.unwrap();
    }
    assert_eq!(height(&full), 40);
    assert_eq!(bodies.load(Ordering::SeqCst), 40);
    let address = public.url.strip_prefix("http://").unwrap();
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let prefix = format!(
        "POST /sim/v1/query/eq HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n",
        REQUEST_CAP + 1
    );
    stream.write_all(prefix.as_bytes()).await.unwrap();
    stream
        .write_all(&vec![b'x'; REQUEST_CAP + 1])
        .await
        .unwrap();
    stream.write_all(b"\r\n0\r\n\r\n").await.unwrap();
    let mut result = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut result))
        .await
        .unwrap()
        .unwrap();
    assert!(String::from_utf8_lossy(&result).starts_with("HTTP/1.1 400"));
    assert_eq!(height(&producer), 40);
}

async fn mutate(State(source): State<String>, Json(body): Json<Value>) -> Json<Value> {
    let mut v: Value = runtime::fetch(
        &reqwest::Client::new(),
        &source,
        "/sim/v1/query/eq",
        Some(body),
        RESPONSE_CAP,
    )
    .await
    .unwrap();
    let proof = v["proof"].as_str().unwrap();
    let mut b = unhex(proof, RESPONSE_CAP).unwrap();
    let p = b.len() / 2;
    b[p] ^= 1;
    v["proof"] = json!(hex(&b));
    Json(v)
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn response_mutation_and_dropped_body_never_produce_verified_rows() {
    let mut h = host();
    for _ in 0..40 {
        h.produce(None).unwrap();
    }
    let id = h.durable_identity.clone();
    let headers = (1..=40).map(|n| h.header(n).unwrap()).collect::<Vec<_>>();
    let head = h.head;
    let producer = Runtime::new(Node::Full(Box::new(h)), None, None).unwrap();
    let source = serve(runtime::public_router(producer)).await;
    let mutator = serve(
        Router::new()
            .route("/sim/v1/query/eq", post(mutate))
            .with_state(source.url.clone()),
    )
    .await;
    let dropper = serve(Router::new().route(
        "/sim/v1/query/eq",
        post(|| async {
            (
                axum::http::StatusCode::OK,
                [("content-type", "application/json")],
                "{",
            )
        }),
    ))
    .await;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    for proxy in [&mutator, &dropper] {
        let mut l = Light::create(Arc::new(MemoryVersionedStore::new()), id.clone()).unwrap();
        l.append(&headers, &head).unwrap();
        let rt = Runtime::new(Node::Light(Box::new(l)), Some(proxy.url.clone()), None).unwrap();
        let server = serve(runtime::public_router(rt)).await;
        for endpoint in ["query/verified", "query/inspect"] {
            let result = request(&client, &server.url, endpoint, &query()).await;
            assert!(!result.status().is_success());
            let body = result.json::<Value>().await.unwrap();
            assert!(body.get("rows").is_none());
            assert!(body.get("verification").is_none());
            assert!(body.get("inspection").is_none());
            assert!(body.get("canonicalProof").is_none());
        }
    }
}

struct Child(std::process::Child);
impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn free_address() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().to_string()
}
#[test]
fn private_network_http_is_explicit_and_unknown_configuration_fails_closed() {
    assert!(runtime::validate_peer("http://producer:9400").is_err());
    assert!(runtime::validate_peer_with_policy("http://producer:9400", true).is_ok());
    assert!(runtime::validate_peer("https://example.test").is_ok());
    assert!(runtime::validate_peer("http://127.0.0.1:9400").is_ok());
    for endpoint in [
        "http://user:secret@producer:9400",
        "http://producer:9400/path",
        "http://producer:9400?peer=other",
        "ftp://producer:9400",
    ] {
        assert!(runtime::validate_peer_with_policy(endpoint, true).is_err());
    }
    assert!(!runtime::parse_private_http(None).unwrap());
    assert!(runtime::parse_private_http(Some("true")).unwrap());
    assert!(!runtime::parse_private_http(Some("false")).unwrap());
    assert!(runtime::parse_private_http(Some("1")).is_err());
    assert!(runtime::parse_private_http(Some("TRUE")).is_err());
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_and_busy_bootstrap_does_not_poison_directory_and_reopen_needs_no_genesis_fetch() {
    let h = host();
    let id = h.identity.clone();
    let good = json!({"sourceId":id.source_id,"runId":id.run_id,"genesisHash":id.genesis_hash,"chainId":id.chain_id,"genesis":pack(&h.run.genesis),"header":pack(&h.run.genesis.header()),"durableIdentity":hex(&h.durable_identity.envelope().unwrap())});
    let mode = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let attempts = Arc::new(AtomicUsize::new(0));
    let (m, c, a) = (mode.clone(), calls.clone(), attempts.clone());
    let source = serve(Router::new().route(
        "/sim/v1/genesis",
        axum::routing::get(move || {
            let (mode, calls, attempts, good) = (m.clone(), c.clone(), a.clone(), good.clone());
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                if mode.load(Ordering::SeqCst) == 0 {
                    let mut bad = good;
                    bad["runId"] = json!("11".repeat(16));
                    return (axum::http::StatusCode::OK, Json(bad));
                }
                if attempts.fetch_add(1, Ordering::SeqCst) < 2 {
                    return (
                        axum::http::StatusCode::TOO_MANY_REQUESTS,
                        Json(json!({"error":"ServerBusy"})),
                    );
                }
                (axum::http::StatusCode::OK, Json(good))
            }
        }),
    ))
    .await;
    let path = std::env::temp_dir().join(format!(
        "arkiv-simulator-bootstrap-{}-{}",
        std::process::id(),
        free_address().replace(':', "-")
    ));
    std::fs::create_dir_all(&path).unwrap();
    let listen = free_address();
    let spawn = || {
        Child(
            std::process::Command::new(env!("CARGO_BIN_EXE_arkiv-simulator"))
                .args([
                    "light",
                    "--unsigned-simulator",
                    "--run-dir",
                    path.to_str().unwrap(),
                    "--listen",
                    &listen,
                    "--peer",
                    &source.url,
                    "--source-id",
                    &id.source_id,
                    "--run-id",
                    &id.run_id,
                    "--genesis-hash",
                    &id.genesis_hash,
                    "--chain-id",
                    &id.chain_id,
                ])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        )
    };
    let mut child = spawn();
    let mut ended = false;
    for _ in 0..100 {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(!status.success());
            ended = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(ended);
    drop(child);
    assert!(!path.join("headers.redb").exists());
    // A valid adapter created by an earlier failed initializer is also recoverable.
    drop(
        arkiv_db_redb::RedbStore::create(
            path.join("headers.redb"),
            arkiv_db_redb::RedbConfig::default(),
        )
        .unwrap(),
    );
    mode.store(1, Ordering::SeqCst);
    let child = spawn();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(1))
        .build()
        .unwrap();
    let url = format!("http://{listen}");
    let mut ready = false;
    for _ in 0..150 {
        if let Ok(response) = client.get(format!("{url}/sim/v1/status")).send().await
            && response.status().is_success()
        {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(ready);
    assert!(attempts.load(Ordering::SeqCst) >= 3);
    drop(child);
    let before = calls.load(Ordering::SeqCst);
    mode.store(0, Ordering::SeqCst);
    let child = spawn();
    let mut ready = false;
    for _ in 0..150 {
        if let Ok(response) = client.get(format!("{url}/sim/v1/status")).send().await
            && response.status().is_success()
        {
            let s = response.json::<Value>().await.unwrap();
            assert_eq!(s["runId"], id.run_id);
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(ready);
    assert_eq!(calls.load(Ordering::SeqCst), before);
    drop(child);
    std::fs::remove_dir_all(path).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inspection_returns_exact_verified_witness_and_historical_results_with_bounds() {
    use arkiv_db::{
        decode_exact,
        proof::{EqRequest, VerifiedEq, verify_eq},
    };
    let mut h = host();
    for _ in 0..20 {
        h.produce(None).unwrap();
    }
    let limits = h.run.genesis.limits;
    let id = h.durable_identity.clone();
    let expected_id = h.identity.clone();
    let headers = (1..=20).map(|n| h.header(n).unwrap()).collect::<Vec<_>>();
    let head = h.head;
    let source = serve(
        runtime::public_router(Runtime::new(Node::Full(Box::new(h)), None, None).unwrap()).layer(
            middleware::from_fn(|r: Request, next: middleware::Next| async move {
                assert!(!r.headers().contains_key("authorization"));
                assert!(!r.headers().contains_key("cookie"));
                next.run(r).await
            }),
        ),
    )
    .await;
    let mut l = Light::create(Arc::new(MemoryVersionedStore::new()), id).unwrap();
    l.append(&headers, &head).unwrap();
    let rt = Runtime::new(Node::Light(Box::new(l)), Some(source.url.clone()), None).unwrap();
    let local = serve(runtime::public_router(rt.clone())).await;
    let client = reqwest::Client::new();
    let mut first = Value::Null;
    for (height, ty, value, count) in [
        // The entity fixture creates five group-1 entities at height 2,
        // deletes one at 8 and expires one at 14.
        (20, "u64", "1", 3),
        (20, "u64", "999999", 0),
        (2, "u64", "1", 5),
        (10, "str", "1", 0),
        (13, "u64", "1", 4),
        (14, "u64", "1", 3),
    ] {
        let mut q = query();
        q["height"] = json!(height.to_string());
        q["valueType"] = json!(ty);
        q["value"] = json!(value);
        let response = client
            .post(format!("{}/sim/v1/query/inspect", local.url))
            .bearer_auth("must-never-be-forwarded")
            .header("cookie", "secret=not-forwarded")
            .json(&q)
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success());
        let bytes = response.bytes().await.unwrap();
        assert!(bytes.len() < RESPONSE_CAP);
        let page: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(page["sourceId"], expected_id.source_id);
        assert_eq!(page["runId"], expected_id.run_id);
        assert_eq!(page["postingCount"], count);
        let request: EqRequest =
            decode_exact(&unhex(page["canonicalRequest"].as_str().unwrap(), 4096).unwrap())
                .unwrap();
        assert_eq!(request.snapshot.height, height);
        let witness = unhex(page["canonicalProof"].as_str().unwrap(), 1024 * 1024).unwrap();
        // The returned canonical witness is independently reaccepted by the
        // ordinary stateless verifier, with caller-selected identity.
        assert_eq!(
            request.snapshot.state_root,
            hash(page["snapshot"]["stateRoot"].as_str().unwrap()).unwrap()
        );
        let VerifiedEq::Page(verified) =
            verify_eq(&request, None, &witness, proof_limits(), limits).unwrap()
        else {
            panic!()
        };
        assert_eq!(verified.posting_count, count);
        assert_eq!(verified.rows.len(), page["rows"].as_array().unwrap().len());
        let inspection = &page["inspection"];
        assert_eq!(inspection["version"], 1);
        assert_eq!(
            inspection["queryDigest"],
            hex(request.query.digest().as_slice())
        );
        assert_eq!(
            inspection["stateComposition"]["stateRoot"],
            page["snapshot"]["stateRoot"]
        );
        assert_eq!(inspection["postingSet"]["count"], count);
        assert_eq!(
            inspection["pointPaths"].as_array().unwrap().len(),
            2 + 2 * verified.rows.len()
        );
        let term = &inspection["pointPaths"][1];
        assert_eq!(term["map"], "terms");
        assert_eq!(
            term["terminal"]["kind"],
            if count == 0 { "absence" } else { "inclusion" }
        );
        assert_eq!(inspection["postingSet"]["termPresent"], count > 0);
        if count > 0 {
            assert_eq!(
                inspection["postingSet"]["authenticatedRoot"],
                inspection["postingSet"]["reconstructedRoot"]
            );
        }
        if height == 20 && count == 3 {
            first = page;
        }
    }
    let mut next = query();
    next["cursor"] = first["continuation"].clone();
    let response = request(&client, &local.url, "query/inspect", &next).await;
    assert!(response.status().is_success());
    let second = response.json::<Value>().await.unwrap();
    assert_ne!(second["rows"][0]["recordId"], first["rows"][0]["recordId"]);
    assert_eq!(
        second["inspection"]["postingSet"]["recordIds"],
        first["inspection"]["postingSet"]["recordIds"]
    );
    next["value"] = json!("2");
    let response = request(&client, &local.url, "query/inspect", &next).await;
    assert_eq!(response.status(), 409);
    assert_eq!(
        response.json::<Value>().await.unwrap(),
        json!({"error":"CursorMismatch"})
    );
    let plain = request(&client, &local.url, "query/verified", &query())
        .await
        .json::<Value>()
        .await
        .unwrap();
    assert!(plain.get("inspection").is_none());
    assert!(plain.get("canonicalProof").is_none());
    let _permits = rt
        .reads
        .clone()
        .acquire_many_owned(READ_CONCURRENCY as u32)
        .await
        .unwrap();
    let busy = request(&client, &local.url, "query/inspect", &query()).await;
    assert_eq!(busy.status(), 429);
    assert_eq!(
        busy.json::<Value>().await.unwrap(),
        json!({"error":"ServerBusy"})
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inspection_rejects_oversized_witness_before_returning_any_proof_data() {
    let mut h = host();
    for _ in 0..20 {
        h.produce(None).unwrap();
    }
    let mut l = Light::create(
        Arc::new(MemoryVersionedStore::new()),
        h.durable_identity.clone(),
    )
    .unwrap();
    l.append(
        &(1..=20).map(|n| h.header(n).unwrap()).collect::<Vec<_>>(),
        &h.head,
    )
    .unwrap();
    let source = serve(Router::new().route(
        "/sim/v1/query/eq",
        post(|| async { Json(json!({"proof":format!("0x{}","00".repeat(1024*1024+1))})) }),
    ))
    .await;
    let local = serve(runtime::public_router(
        Runtime::new(Node::Light(Box::new(l)), Some(source.url.clone()), None).unwrap(),
    ))
    .await;
    let response = request(
        &reqwest::Client::new(),
        &local.url,
        "query/inspect",
        &query(),
    )
    .await;
    assert_eq!(response.status(), 422);
    assert_eq!(
        response.json::<Value>().await.unwrap(),
        json!({"error":"ProofLimitExceeded"})
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn state_retention_refuses_pruned_proofs_with_410_on_full_and_light() {
    use arkiv_db::durable::Retention;
    let mut h = Host::create(
        Arc::new(MemoryVersionedStore::new()),
        RunConfig {
            genesis: arkiv_node::demo::workload::genesis(9001, 1700000000000),
            workload: Workload::default(),
        },
        [1; 16],
        [7; 16],
        Options {
            retention: Retention::Window(5),
            ..Options::default()
        },
    )
    .unwrap();
    for _ in 0..20 {
        h.produce(None).unwrap();
    }
    let id = h.durable_identity.clone();
    let headers = (1..=20).map(|n| h.header(n).unwrap()).collect::<Vec<_>>();
    let head = h.head;
    let source = serve(runtime::public_router(
        Runtime::new(Node::Full(Box::new(h)), None, None).unwrap(),
    ))
    .await;
    let client = reqwest::Client::new();
    let status = get(&client, &source.url, "status").await;
    assert_eq!(status["stateRetention"], "window");
    assert_eq!(status["stateRetentionWindow"], "5");
    assert_eq!(status["stateFloor"], "15");
    assert_eq!(status["receiptFloor"], "16");
    for (height, pruned) in [(14, true), (15, false), (20, false)] {
        let mut q = query();
        q["height"] = json!(height.to_string());
        let resolved: Value = request(&client, &source.url, "query/resolve", &q)
            .await
            .json()
            .await
            .unwrap();
        let r = request(
            &client,
            &source.url,
            "query/eq",
            &json!({"request":resolved["request"]}),
        )
        .await;
        if pruned {
            assert_eq!(r.status(), 410);
            assert_eq!(
                r.json::<Value>().await.unwrap(),
                json!({"error":"HistoryPruned","requestedHeight":"14","stateFloor":"15"})
            );
        } else {
            assert!(r.status().is_success());
        }
    }
    // A light follower passes the peer's refusal through explicitly.
    let mut l = Light::create(Arc::new(MemoryVersionedStore::new()), id).unwrap();
    l.append(&headers, &head).unwrap();
    let rt = Runtime::new(Node::Light(Box::new(l)), Some(source.url.clone()), None).unwrap();
    let local = serve(runtime::public_router(rt)).await;
    let light_status = get(&client, &local.url, "status").await;
    assert_eq!(light_status["stateFloor"], Value::Null);
    let mut q = query();
    q["height"] = json!("14");
    let r = request(&client, &local.url, "query/verified", &q).await;
    assert_eq!(r.status(), 410);
    assert_eq!(
        r.json::<Value>().await.unwrap(),
        json!({"error":"HistoryPruned"})
    );
    q["height"] = json!("15");
    assert!(
        request(&client, &local.url, "query/verified", &q)
            .await
            .status()
            .is_success()
    );
}
