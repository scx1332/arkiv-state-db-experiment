//! Detached reads (`arkiv_node::signed::reads`) over real sockets: heavy
//! reads evaluated outside the node lock, bounded in concurrency and time.
//!
//! The node's state store adds a fixed delay to every object read of a
//! namespace's maps, and the decoded-node cache is disabled, so a count that
//! must probe every row (a negated prefix: no posting answers it, so even
//! the planned evaluation walks the row map) over the 256 prefilled
//! entities takes seconds, a key lookup reads a few dozen objects and
//! status, `eth_blockNumber`, admission and empty blocks read none. (A
//! match-all count is answered from the stored row count.)
//!
//! What this file establishes:
//! - While a heavy count runs, status, `eth_blockNumber`, a key lookup and
//!   block production answer promptly; the count returns the entity count
//!   of the height it was admitted at.
//! - With every read permit taken, another detached read is refused at once
//!   with `ServerBusy` (JSON-RPC code -32010 and HTTP 429 for a single
//!   request, per-entry in a batch; REST 429), while transaction admission
//!   and production proceed.
//! - A read past its deadline is answered `ReadDeadlineExceeded`, and one
//!   whose client disconnects is abandoned; either way the walk stops at its
//!   next charge and releases its permit long before it would have finished.
//! - Concurrent reads while blocks are produced each see one published
//!   height: every query page and count equals the in-lock read of the
//!   height it reports, and unpinned counts equal the count of some height.
//! - Cursor pages of concurrent walks join to the single-page result.
use alloy_primitives::U256;
use arkiv_db::{CellName, Value as DbValue, durable::Options};
use arkiv_db_memory::MemoryVersionedStore;
use arkiv_db_storage::versioned::{
    CommitBatch, CommitOutcome, DatabaseId, Head, Health, ObjectId, ObjectReader, SpaceId,
    StorageError, StoreStats, Table, VersionedStore,
};
use arkiv_node::{
    Expiry, TxEnvelope, TxOp,
    accounts::AccountConfig,
    arkiv::{ArkivOperation, derive_entity_key, entity_namespace},
    genesis_bundle::{
        MAX_CANONICAL_BYTES, MAX_STORAGE_UNITS, PrefillEntity, PrefillLimits, build_genesis,
        encode_stream,
    },
    signed::{
        crypto::Signer,
        history::History,
        host::Host,
        protocol::{SignedGenesis, SignedHeader, SignedTx, default_native_genesis},
        reads::ReadLimits,
        rpc_reads,
        runtime::{self, Node, Runtime, Shared},
    },
    simulator::wire::{hex, pack},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering::Relaxed},
    },
    time::{Duration, Instant},
};
use tokio::io::AsyncWriteExt;

const CHAIN: u64 = 9515;
const ENTITIES: u64 = 256;
const TOKEN: &str = "signed-test-replication-token-0123456789";

/// A memory store whose namespace object reads take `delay_us` each.
#[derive(Default)]
struct SlowStore {
    inner: MemoryVersionedStore,
    delay_us: AtomicU64,
    namespace_reads: AtomicU64,
}
impl SlowStore {
    fn slow(&self, space: SpaceId) {
        if matches!(space, SpaceId::Namespace(_)) {
            self.namespace_reads.fetch_add(1, Relaxed);
            let delay = self.delay_us.load(Relaxed);
            if delay > 0 {
                std::thread::sleep(Duration::from_micros(delay));
            }
        }
    }
    fn reads(&self) -> u64 {
        self.namespace_reads.load(Relaxed)
    }
}
impl ObjectReader for SlowStore {
    fn read_object(&self, space: SpaceId, id: &ObjectId) -> Result<Option<Vec<u8>>, StorageError> {
        self.slow(space);
        self.inner.read_object(space, id)
    }
    fn read_object_bounded(
        &self,
        space: SpaceId,
        id: &ObjectId,
        max_bytes: usize,
    ) -> Result<Option<Vec<u8>>, StorageError> {
        self.slow(space);
        self.inner.read_object_bounded(space, id, max_bytes)
    }
}
impl VersionedStore for SlowStore {
    fn database_id(&self) -> DatabaseId {
        self.inner.database_id()
    }
    fn head(&self) -> Result<Option<Head>, StorageError> {
        self.inner.head()
    }
    fn read_record(&self, table: Table, key: &[u8]) -> Result<Option<Vec<u8>>, StorageError> {
        self.inner.read_record(table, key)
    }
    fn read_record_bounded(
        &self,
        table: Table,
        key: &[u8],
        max_bytes: usize,
    ) -> Result<Option<Vec<u8>>, StorageError> {
        self.inner.read_record_bounded(table, key, max_bytes)
    }
    fn commit(&self, batch: &CommitBatch) -> CommitOutcome {
        self.inner.commit(batch)
    }
    fn stage_objects(&self, objects: &[(SpaceId, ObjectId, Vec<u8>)]) -> CommitOutcome {
        self.inner.stage_objects(objects)
    }
    fn health(&self) -> Health {
        self.inner.health()
    }
    fn recover(&self) -> Result<Option<Head>, StorageError> {
        self.inner.recover()
    }
    fn stats(&self) -> StoreStats {
        self.inner.stats()
    }
}

fn signer() -> Signer {
    Signer::from_bytes([1; 32]).unwrap()
}
fn creator() -> [u8; 20] {
    *signer().address().as_bytes()
}
/// Prefilled entity `n`: attribute `n` holds `n % 4`.
fn entity_key(nonce: u64) -> [u8; 32] {
    derive_entity_key(CHAIN, &creator(), nonce, nonce as u128)
}

struct Node2 {
    rt: Shared,
    store: Arc<SlowStore>,
    public: Server,
    private: Server,
}
struct Server {
    url: String,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn serve(router: axum::Router) -> Server {
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", socket.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(socket, router).await.unwrap() });
    Server { url, task }
}

/// A producer over [`SlowStore`] with the prefilled entities at height 0.
async fn node(limits: ReadLimits) -> Node2 {
    let signer = signer();
    let base = SignedGenesis {
        genesis: default_native_genesis(CHAIN, 1_000),
        run_id: [7; 16],
        proposer: creator(),
        block_period_ms: 1_000,
        limits: Default::default(),
        accounts: AccountConfig::new(vec![(
            signer.address(),
            U256::from(10u64).pow(U256::from(24u64)),
        )])
        .unwrap(),
        prefill: None,
    };
    let rows = (0..ENTITIES)
        .map(|n| PrefillEntity {
            creator: creator(),
            create_nonce: n,
            salt: n as u128,
            expires_at_height: 100_000 + n,
            flags: 0,
            attributes: vec![
                (
                    CellName::new("body").unwrap(),
                    arkiv_db::Cell::field(DbValue::bytes(format!("row{n}").as_bytes())),
                ),
                (
                    CellName::new("n").unwrap(),
                    arkiv_db::Cell::attribute(DbValue::U64(n % 4)),
                ),
            ],
        })
        .collect::<Vec<_>>();
    let genesis = build_genesis(
        base,
        PrefillLimits {
            max_entities: ENTITIES,
            max_canonical_bytes: MAX_CANONICAL_BYTES,
            max_storage_units: MAX_STORAGE_UNITS,
        },
        creator(),
        &rows,
    )
    .unwrap();
    let store = Arc::new(SlowStore::default());
    let host = Host::create_with_prefill(
        store.clone(),
        History::create(Arc::new(MemoryVersionedStore::new()), genesis.hash()).unwrap(),
        genesis.clone(),
        [3; 16],
        SignedHeader::sign(&genesis, genesis.header(), &signer).unwrap(),
        Some(signer),
        // No decoded-node cache: every walk reads the store.
        Options {
            cache_bytes: 0,
            cache_entries: 0,
            ..Options::default()
        },
        Some(encode_stream(&rows).unwrap()),
    )
    .unwrap();
    let rt = Runtime::with_read_limits(
        Node::Full(Box::new(host)),
        None,
        None,
        Some(TOKEN.into()),
        limits,
    )
    .unwrap();
    let public = serve(runtime::public_router(rt.clone())).await;
    let private = serve(runtime::private_router(rt.clone())).await;
    Node2 {
        rt,
        store,
        public,
        private,
    }
}
fn limits(concurrency: usize, deadline_secs: u64) -> ReadLimits {
    ReadLimits {
        concurrency,
        deadline: Duration::from_secs(deadline_secs),
        ..ReadLimits::default()
    }
}

/// A signed create of one entity with attribute `n = value`, at account
/// nonce `nonce`.
fn create(genesis: &SignedGenesis, nonce: u64, value: u64) -> SignedTx {
    let mut request_id = [0; 32];
    request_id[24..].copy_from_slice(&nonce.to_be_bytes());
    let tx = TxEnvelope {
        chain_id: CHAIN,
        actor: signer().address(),
        request_id,
        tx_budget: 2_000_000,
        ops: vec![TxOp::Arkiv {
            namespace: entity_namespace(),
            operation: ArkivOperation::Create {
                salt: 1_000 + nonce as u128,
                flags: 0,
                expiry: Expiry {
                    expires_at: 0,
                    min_lifetime: 1_000,
                },
                attributes: vec![(
                    CellName::new("n").unwrap(),
                    arkiv_db::Cell::attribute(DbValue::U64(value)),
                )],
            },
        }],
    };
    SignedTx::sign(genesis, nonce, tx, &signer()).unwrap()
}
fn genesis_of(rt: &Shared) -> SignedGenesis {
    rt.node.lock().unwrap().genesis().clone()
}
fn head(rt: &Shared) -> u64 {
    rt.node.lock().unwrap().head().height
}
/// Submits a create and produces the block holding it, through the API.
async fn produce_create(
    client: &reqwest::Client,
    node: &Node2,
    genesis: &SignedGenesis,
    nonce: u64,
    value: u64,
) {
    submit(client, node, genesis, nonce, value).await;
    step(client, node).await;
}
async fn submit(
    client: &reqwest::Client,
    node: &Node2,
    genesis: &SignedGenesis,
    nonce: u64,
    value: u64,
) {
    let body = json!({"transactionBytes":pack(&create(genesis, nonce, value))});
    let url = format!("{}/signed/v1/transactions", node.public.url);
    until_admitted(|| client.post(&url).json(&body)).await;
}
async fn step(client: &reqwest::Client, node: &Node2) {
    let url = format!("{}/signed/v1/control", node.private.url);
    until_admitted(|| {
        client
            .post(&url)
            .bearer_auth(TOKEN)
            .json(&json!({"action":"step"}))
    })
    .await;
}
/// Writers take the node lock without waiting, as before detached reads,
/// and answer `ServerBusy` while any request holds it; clients retry.
async fn until_admitted(request: impl Fn() -> reqwest::RequestBuilder) {
    let started = Instant::now();
    loop {
        let response = request().send().await.unwrap();
        let status = response.status();
        if status == 429 && started.elapsed() < Duration::from_secs(10) {
            tokio::time::sleep(Duration::from_millis(5)).await;
            continue;
        }
        assert_eq!(status, 200, "{}", response.text().await.unwrap());
        return;
    }
}

async fn rpc(client: &reqwest::Client, url: &str, method: &str, params: Value) -> (u16, Value) {
    let response = client
        .post(url)
        .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    (status, response.json().await.unwrap())
}
async fn timed<T>(f: impl std::future::Future<Output = T>) -> (T, Duration) {
    let started = Instant::now();
    let value = f.await;
    (value, started.elapsed())
}
async fn wait_for(what: &str, limit: Duration, mut condition: impl FnMut() -> bool) {
    let started = Instant::now();
    while !condition() {
        assert!(started.elapsed() < limit, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}
/// The parameters of a count whose walk loads and probes every live row.
fn heavy_count() -> Value {
    json!([{"query":"NOT (n STARTSWITH str('x'))"}])
}
fn key_query(nonce: u64) -> String {
    format!("$key = key({})", hex(&entity_key(nonce)))
}
/// The same read served the old way: planned and run under the node lock.
fn in_lock(rt: &Shared, method: &str, params: Value) -> Value {
    let node = rt.node.lock().unwrap();
    let Node::Full(host) = &*node else {
        panic!("full node")
    };
    rpc_reads::dispatch(host, method, &params).unwrap().unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn status_and_light_reads_answer_while_a_heavy_count_runs() {
    let node = node(ReadLimits::default()).await;
    let client = reqwest::Client::new();
    let genesis = genesis_of(&node.rt);
    let select = json!({"key":true,"owner":true,"attributes":true});
    let expected = in_lock(
        &node.rt,
        "arkiv_query",
        json!([key_query(5), {"select":select}]),
    );
    assert_eq!(expected["data"].as_array().unwrap().len(), 1);
    assert_eq!(
        in_lock(&node.rt, "arkiv_getEntityCount", heavy_count()),
        json!(ENTITIES)
    );
    node.store.delay_us.store(10_000, Relaxed);
    let heavy = tokio::spawn({
        let (client, url) = (client.clone(), node.public.url.clone());
        async move { timed(rpc(&client, &url, "arkiv_getEntityCount", heavy_count())).await }
    });
    wait_for("the count's admission", Duration::from_secs(5), || {
        node.rt.reads_in_flight() == 1
    })
    .await;
    let (status, status_time) = timed(async {
        let response = client
            .get(format!("{}/signed/v1/status", node.public.url))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        response.json::<Value>().await.unwrap()
    })
    .await;
    assert_eq!(status["head"]["height"], "0");
    let ((code, number), number_time) =
        timed(rpc(&client, &node.public.url, "eth_blockNumber", json!([]))).await;
    assert_eq!((code, &number["result"]), (200, &json!("0x0")));
    let ((code, lookup), lookup_time) = timed(rpc(
        &client,
        &node.public.url,
        "arkiv_query",
        json!([key_query(5), {"select":select}]),
    ))
    .await;
    assert_eq!((code, &lookup["result"]), (200, &expected));
    // Writers: an (empty) block, then a transaction admitted to the pool.
    let (_, produce_time) = timed(step(&client, &node)).await;
    assert_eq!(head(&node.rt), 1);
    let (_, submit_time) = timed(submit(&client, &node, &genesis, 0, 1)).await;
    assert_eq!(node.rt.reads_in_flight(), 1, "the count is still running");
    let ((code, count), count_time) = heavy.await.unwrap();
    assert_eq!((code, &count["result"]), (200, &json!(ENTITIES)));
    eprintln!(
        "during a {count_time:?} count: status {status_time:?}, eth_blockNumber \
         {number_time:?}, key lookup {lookup_time:?}, produce {produce_time:?}, submit \
         {submit_time:?}"
    );
    for (what, latency) in [
        ("status", status_time),
        ("eth_blockNumber", number_time),
        ("key lookup", lookup_time),
        ("produce", produce_time),
        ("submit", submit_time),
    ] {
        assert!(latency < count_time / 4, "{what} took {latency:?}");
    }
    // Admitted at height 1, a count never sees the entity a later block adds.
    node.store.delay_us.store(10_000, Relaxed);
    let heavy = tokio::spawn({
        let (client, url) = (client.clone(), node.public.url.clone());
        async move { rpc(&client, &url, "arkiv_getEntityCount", heavy_count()).await }
    });
    wait_for("the count's admission", Duration::from_secs(5), || {
        node.rt.reads_in_flight() == 1
    })
    .await;
    step(&client, &node).await;
    assert_eq!(head(&node.rt), 2);
    assert_eq!(heavy.await.unwrap().1["result"], json!(ENTITIES));
    node.store.delay_us.store(0, Relaxed);
    assert_eq!(
        rpc(&client, &node.public.url, "arkiv_getEntityCount", json!([]))
            .await
            .1["result"],
        json!(ENTITIES + 1)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn saturated_reads_are_refused_at_once_without_blocking_writes() {
    let node = node(limits(1, 30)).await;
    let client = reqwest::Client::new();
    let genesis = genesis_of(&node.rt);
    node.store.delay_us.store(10_000, Relaxed);
    let heavy = tokio::spawn({
        let (client, url) = (client.clone(), node.public.url.clone());
        async move { rpc(&client, &url, "arkiv_getEntityCount", heavy_count()).await }
    });
    wait_for("the count's admission", Duration::from_secs(5), || {
        node.rt.reads_in_flight() == 1
    })
    .await;
    for (method, params) in [
        ("arkiv_query", json!(["*"])),
        ("arkiv_getEntityCount", json!([{"query":"n = u64(1)"}])),
    ] {
        let ((code, busy), elapsed) = timed(rpc(&client, &node.public.url, method, params)).await;
        assert_eq!(code, 429);
        assert_eq!(
            busy,
            json!({"jsonrpc":"2.0","id":1,"error":{"code":arkiv_node::signed::rpc::SERVER_BUSY_CODE,"message":"ServerBusy"}})
        );
        assert!(elapsed < Duration::from_millis(500), "{elapsed:?}");
    }
    let batch: Value = client
        .post(&node.public.url)
        .json(&json!([
            {"jsonrpc":"2.0","id":1,"method":"arkiv_getEntityCount","params":[]},
            {"jsonrpc":"2.0","id":2,"method":"eth_blockNumber","params":[]}
        ]))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(batch[0]["error"]["message"], "ServerBusy");
    assert_eq!(batch[1]["result"], "0x0");
    let (explore, elapsed) = timed(
        client
            .post(format!("{}/signed/v1/state/explore", node.public.url))
            .json(&json!({"height":"0"}))
            .send(),
    )
    .await;
    let explore = explore.unwrap();
    assert_eq!(explore.status(), 429);
    assert_eq!(
        explore.json::<Value>().await.unwrap(),
        json!({"error":"ServerBusy"})
    );
    assert!(elapsed < Duration::from_millis(500), "{elapsed:?}");
    // Writers never wait for the reads: admission and production proceed.
    let (_, submit_time) = timed(submit(&client, &node, &genesis, 0, 1)).await;
    let (_, produce_time) = timed(step(&client, &node)).await;
    assert_eq!(head(&node.rt), 1);
    assert_eq!(node.rt.reads_in_flight(), 1, "the count is still running");
    eprintln!("beside a saturated read gate: submit {submit_time:?}, produce {produce_time:?}");
    let (code, count) = heavy.await.unwrap();
    assert_eq!((code, &count["result"]), (200, &json!(ENTITIES)));
    // The permit is back: the next read is admitted at the new head.
    node.store.delay_us.store(0, Relaxed);
    let (code, count) = rpc(&client, &node.public.url, "arkiv_getEntityCount", json!([])).await;
    assert_eq!((code, &count["result"]), (200, &json!(ENTITIES + 1)));
    let explored = client
        .post(format!("{}/signed/v1/state/explore", node.public.url))
        .json(&json!({"height":"1"}))
        .send()
        .await
        .unwrap();
    assert_eq!(explored.status(), 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deadlines_and_departed_clients_stop_the_walk() {
    let node = node(limits(2, 1)).await;
    let client = reqwest::Client::new();
    let before = node.store.reads();
    assert_eq!(
        in_lock(&node.rt, "arkiv_getEntityCount", heavy_count()),
        json!(ENTITIES)
    );
    let full_walk = node.store.reads() - before;
    node.store.delay_us.store(10_000, Relaxed);
    let started = node.store.reads();
    let ((code, answer), elapsed) = timed(rpc(
        &client,
        &node.public.url,
        "arkiv_getEntityCount",
        heavy_count(),
    ))
    .await;
    assert_eq!(code, 200);
    assert_eq!(
        answer,
        json!({"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"ReadDeadlineExceeded"}})
    );
    assert!(
        elapsed >= Duration::from_secs(1) && elapsed < Duration::from_secs(2),
        "{elapsed:?}"
    );
    // The walk stops at its next charge and returns its permit.
    let (_, release) = timed(wait_for(
        "the abandoned walk to stop",
        Duration::from_millis(500),
        || node.rt.reads_in_flight() == 0,
    ))
    .await;
    let stopped = node.store.reads();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(node.store.reads(), stopped, "the walk kept reading");
    let deadline_reads = stopped - started;
    assert!(
        deadline_reads < full_walk,
        "{deadline_reads} of {full_walk}"
    );
    // A client that disconnects abandons its read the same way.
    let address = node.public.url.strip_prefix("http://").unwrap();
    let body =
        json!({"jsonrpc":"2.0","id":9,"method":"arkiv_getEntityCount","params":heavy_count()})
            .to_string();
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    stream
        .write_all(
            format!(
                "POST / HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    wait_for("the count's admission", Duration::from_secs(5), || {
        node.rt.reads_in_flight() == 1
    })
    .await;
    let started = node.store.reads();
    tokio::time::sleep(Duration::from_millis(200)).await;
    drop(stream);
    let (_, cancel) = timed(wait_for(
        "the departed client's walk to stop",
        Duration::from_millis(900),
        || node.rt.reads_in_flight() == 0,
    ))
    .await;
    let stopped = node.store.reads();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(node.store.reads(), stopped, "the walk kept reading");
    let departed_reads = stopped - started;
    assert!(
        departed_reads < full_walk,
        "{departed_reads} of {full_walk}"
    );
    eprintln!(
        "full walk {full_walk} namespace reads; deadline answered after {elapsed:?}, permit \
         back {release:?} later after {deadline_reads} reads; disconnect: permit back \
         {cancel:?} after the hang-up, {departed_reads} reads"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_reads_see_one_height_while_blocks_are_produced() {
    let node = Arc::new(node(limits(4, 30)).await);
    let client = reqwest::Client::new();
    let genesis = genesis_of(&node.rt);
    node.store.delay_us.store(1_000, Relaxed);
    const BLOCKS: u64 = 8;
    let producer = tokio::spawn({
        let (node, client) = (node.clone(), client.clone());
        async move {
            for nonce in 0..BLOCKS {
                produce_create(&client, &node, &genesis, nonce, nonce % 4).await;
            }
        }
    });
    let select = json!({"key":true,"attributes":true});
    let mut readers = tokio::task::JoinSet::new();
    for reader in 0..4u64 {
        let (node, client, select) = (node.clone(), client.clone(), select.clone());
        readers.spawn(async move {
            let mut seen = Vec::new();
            while head(&node.rt) < BLOCKS {
                let at = head(&node.rt);
                let (method, params) = match reader {
                    0 => (
                        "arkiv_query",
                        json!(["n = u64(1)", {"select":select,"limit":200}]),
                    ),
                    1 => ("arkiv_getEntityCount", json!([{"block":at}])),
                    2 => ("arkiv_getEntityCount", json!([])),
                    _ => ("arkiv_getEntityCount", json!([{"query":"n = u64(2)"}])),
                };
                let (code, answer) = rpc(&client, &node.public.url, method, params.clone()).await;
                assert_eq!(code, 200, "{answer}");
                seen.push((at, params, answer["result"].clone()));
            }
            (reader, seen)
        });
    }
    producer.await.unwrap();
    node.store.delay_us.store(0, Relaxed);
    let counts = |query: Option<&str>| -> BTreeSet<u64> {
        (0..=BLOCKS)
            .map(|h| {
                let mut options = json!({"block":h});
                if let Some(q) = query {
                    options["query"] = json!(q);
                }
                in_lock(&node.rt, "arkiv_getEntityCount", json!([options]))
                    .as_u64()
                    .unwrap()
            })
            .collect()
    };
    let all_heights = counts(None);
    let group_two = counts(Some("n = u64(2)"));
    assert_eq!(all_heights.len() as u64, BLOCKS + 1, "every block adds one");
    let mut reads = 0;
    while let Some(joined) = readers.join_next().await {
        let (reader, seen) = joined.unwrap();
        assert!(!seen.is_empty());
        reads += seen.len();
        for (at, mut params, result) in seen {
            match reader {
                0 => {
                    // The page reports its height; the in-lock read there agrees.
                    params[1]["atBlock"] = result["blockNumber"].clone();
                    assert_eq!(in_lock(&node.rt, "arkiv_query", params), result);
                }
                1 => assert_eq!(in_lock(&node.rt, "arkiv_getEntityCount", params), result),
                2 => assert!(
                    all_heights.contains(&result.as_u64().unwrap()),
                    "{result} at {at}"
                ),
                _ => assert!(
                    group_two.contains(&result.as_u64().unwrap()),
                    "{result} at {at}"
                ),
            }
        }
    }
    eprintln!("{reads} detached reads checked across {BLOCKS} produced blocks");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cursor_walks_join_to_the_single_page_while_others_run() {
    let node = Arc::new(node(limits(4, 30)).await);
    let client = reqwest::Client::new();
    let genesis = genesis_of(&node.rt);
    for nonce in 0..2 {
        produce_create(&client, &node, &genesis, nonce, 1).await;
    }
    let select = json!({"key":true,"attributes":true});
    let full = in_lock(
        &node.rt,
        "arkiv_query",
        json!(["n = u64(1)", {"atBlock":"0x1","limit":200,"select":select}]),
    );
    let full = full["data"].as_array().unwrap().clone();
    assert_eq!(full.len() as u64, ENTITIES / 4 + 1);
    node.store.delay_us.store(500, Relaxed);
    let producer = tokio::spawn({
        let (node, client) = (node.clone(), client.clone());
        async move {
            for nonce in 2..5 {
                produce_create(&client, &node, &genesis, nonce, 1).await;
            }
        }
    });
    let mut walkers = tokio::task::JoinSet::new();
    for limit in [5u64, 7, 11] {
        let (node, client, select) = (node.clone(), client.clone(), select.clone());
        walkers.spawn(async move {
            let mut rows = Vec::new();
            let mut cursor: Option<Value> = None;
            loop {
                let mut options = json!({"atBlock":"0x1","limit":limit,"select":select});
                if let Some(token) = &cursor {
                    options["cursor"] = token.clone();
                }
                let (code, page) = rpc(
                    &client,
                    &node.public.url,
                    "arkiv_query",
                    json!(["n = u64(1)", options]),
                )
                .await;
                assert_eq!(code, 200, "{page}");
                let page = &page["result"];
                assert_eq!(page["blockNumber"], "0x1");
                rows.extend(page["data"].as_array().unwrap().iter().cloned());
                match page.get("cursor") {
                    Some(token) => cursor = Some(token.clone()),
                    None => return (limit, rows),
                }
            }
        });
    }
    while let Some(joined) = walkers.join_next().await {
        let (limit, rows) = joined.unwrap();
        assert_eq!(rows, full, "walk with limit {limit}");
    }
    producer.await.unwrap();
    assert_eq!(head(&node.rt), 5);
}
