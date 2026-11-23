//! Detached reads (`arkiv_node::signed::reads`) over real sockets: heavy
//! reads evaluated outside the node lock, bounded in concurrency and time.
//!
//! The node's state store adds a fixed delay to every object read of a
//! namespace's maps, and the decoded-node cache is disabled, so a count that
//! must probe every row (a negated prefix: no posting answers it, so even
//! the planned evaluation walks the row map) over the 256 prefilled
//! entities takes seconds, a key lookup reads a few dozen objects and
//! status, `eth_blockNumber`, admission and empty blocks read none. (A
//! match-all count is answered from the stored row count.) The store can
//! also raise an [`Interrupt`] at a chosen read, to see where a walk stops.

mod signed_support;

use arkiv_db::{CellName, Interrupt, Value as DbValue, durable::Options};
use arkiv_db_memory::MemoryVersionedStore;
use arkiv_db_storage::versioned::{
    CommitBatch, CommitOutcome, DatabaseId, Head, Health, ObjectId, ObjectReader, SpaceId,
    StorageError, StoreStats, Table, VersionedStore,
};
use arkiv_node::{
    Expiry,
    arkiv::{ArkivOperation, derive_entity_key},
    genesis_bundle::{
        MAX_CANONICAL_BYTES, MAX_STORAGE_UNITS, PrefillEntity, PrefillLimits, build_genesis,
        encode_stream,
    },
    signed::{
        host::{Host, Prefill},
        protocol::{SignedGenesis, SignedTx, default_native_genesis},
        reads::ReadLimits,
        rpc_reads,
        runtime::{self, Node, Runtime, Shared},
    },
    simulator::wire::{hex, pack},
};
use serde_json::{Value, json};
use signed_support::{
    arkiv_ops,
    http::{Server, serve},
    launch, sign_tx, signer,
};
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

/// A memory store whose namespace object reads take `delay_us` each, and
/// which raises `raise.1` at namespace read number `raise.0`.
#[derive(Default)]
struct SlowStore {
    inner: MemoryVersionedStore,
    delay_us: AtomicU64,
    namespace_reads: AtomicU64,
    raise: std::sync::Mutex<Option<(u64, Interrupt)>>,
}

impl SlowStore {
    fn slow(&self, space: SpaceId) {
        if matches!(space, SpaceId::Namespace(_)) {
            let read = self.namespace_reads.fetch_add(1, Relaxed) + 1;
            if let Some((at, interrupt)) = &*self.raise.lock().unwrap()
                && read >= *at
            {
                interrupt.raise();
            }
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

fn creator() -> [u8; 20] {
    *signer(1).address().as_bytes()
}

/// The key of prefilled entity `nonce`.
fn entity_key(nonce: u64) -> [u8; 32] {
    derive_entity_key(CHAIN, &creator(), nonce, nonce as u128)
}

struct Node2 {
    rt: Shared,
    store: Arc<SlowStore>,
    public: Server,
    private: Server,
}

/// A producer over [`SlowStore`] with the prefilled entities at height 0.
async fn node(limits: ReadLimits) -> Node2 {
    let signer = signer(1);
    let base = SignedGenesis {
        genesis: default_native_genesis(CHAIN, 1_000, 1_000),
        run_id: [7; 16],
        proposer: creator(),
        block_period_ms: 1_000,
        limits: Default::default(),
        accounts: signed_support::allocations(&[signer.address()]),
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
                // One term per entity: a range over it is a long run.
                (
                    CellName::new("id").unwrap(),
                    arkiv_db::Cell::attribute(DbValue::U64(n)),
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
    let host = Host::create(
        store.clone(), // No decoded-node cache: every walk reads the store.
        Options {
            cache_bytes: 0,
            cache_entries: 0,
            ..Options::default()
        },
        launch(
            &genesis,
            signer,
            [3; 16],
            Arc::new(MemoryVersionedStore::new()),
            true,
        ),
        Prefill::Stream(encode_stream(&rows).unwrap()),
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
    let create = ArkivOperation::Create {
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
    };
    let ops = arkiv_ops(vec![create]);
    sign_tx(genesis, &signer(1), nonce, request_id, 2_000_000, ops)
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

/// Writers answer `ServerBusy` while a request holds the node lock; clients
/// retry.
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

/// A client whose requests fail after a minute instead of waiting forever.
fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .unwrap()
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

/// The reference answer: the same read planned and run under the node lock.
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
    let client = client();
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
        async move { rpc(&client, &url, "arkiv_getEntityCount", heavy_count()).await }
    });
    wait_for("the count's admission", Duration::from_secs(5), || {
        node.rt.reads_in_flight() == 1
    })
    .await;
    let response = client
        .get(format!("{}/signed/v1/status", node.public.url))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let status = response.json::<Value>().await.unwrap();
    assert_eq!(status["head"]["height"], "0");
    let (code, number) = rpc(&client, &node.public.url, "eth_blockNumber", json!([])).await;
    assert_eq!((code, &number["result"]), (200, &json!("0x0")));
    let (code, lookup) = rpc(
        &client,
        &node.public.url,
        "arkiv_query",
        json!([key_query(5), {"select":select}]),
    )
    .await;
    assert_eq!((code, &lookup["result"]), (200, &expected));
    // Writers: an (empty) block, then a transaction admitted to the pool.
    step(&client, &node).await;
    assert_eq!(head(&node.rt), 1);
    submit(&client, &node, &genesis, 0, 1).await;
    assert_eq!(node.rt.reads_in_flight(), 1, "the count is still running");
    let (code, count) = heavy.await.unwrap();
    assert_eq!((code, &count["result"]), (200, &json!(ENTITIES)));
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
    let client = client();
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
        let (code, busy) = rpc(&client, &node.public.url, method, params).await;
        assert_eq!(code, 429);
        assert_eq!(
            busy,
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "error": {
                    "code": arkiv_node::signed::rpc::SERVER_BUSY_CODE,
                    "message": "ServerBusy",
                },
            })
        );
    }
    let batch: Value = client
        .post(&node.public.url)
        .json(&json!([
            {"jsonrpc":"2.0", "id":1, "method":"arkiv_getEntityCount", "params":[]},
            {"jsonrpc":"2.0", "id":2, "method":"eth_blockNumber", "params":[]}
        ]))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(batch[0]["error"]["message"], "ServerBusy");
    assert_eq!(batch[1]["result"], "0x0");
    let explore = client
        .post(format!("{}/signed/v1/state/explore", node.public.url))
        .json(&json!({"height":"0"}))
        .send()
        .await
        .unwrap();
    assert_eq!(explore.status(), 429);
    assert_eq!(
        explore.json::<Value>().await.unwrap(),
        json!({"error":"ServerBusy"})
    );
    // Writers never wait for the reads: admission and production proceed.
    submit(&client, &node, &genesis, 0, 1).await;
    step(&client, &node).await;
    assert_eq!(head(&node.rt), 1);
    assert_eq!(node.rt.reads_in_flight(), 1, "the count is still running");
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
    let client = client();
    let before = node.store.reads();
    assert_eq!(
        in_lock(&node.rt, "arkiv_getEntityCount", heavy_count()),
        json!(ENTITIES)
    );
    let full_walk = node.store.reads() - before;
    node.store.delay_us.store(10_000, Relaxed);
    let started = node.store.reads();
    let (code, answer) = rpc(
        &client,
        &node.public.url,
        "arkiv_getEntityCount",
        heavy_count(),
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(
        answer,
        json!({"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"ReadDeadlineExceeded"}})
    );
    // The walk stops at its next charge and returns its permit.
    wait_for(
        "the abandoned walk to stop",
        Duration::from_secs(30),
        || node.rt.reads_in_flight() == 0,
    )
    .await;
    let stopped = node.store.reads();
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
                "POST / HTTP/1.1\r\n\
                 Host: {address}\r\n\
                 Content-Type: application/json\r\n\
                 Content-Length: {}\r\n\
                 \r\n\
                 {body}",
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
    drop(stream);
    wait_for(
        "the departed client's walk to stop",
        Duration::from_secs(30),
        || node.rt.reads_in_flight() == 0,
    )
    .await;
    let stopped = node.store.reads();
    let departed_reads = stopped - started;
    assert!(
        departed_reads < full_walk,
        "{departed_reads} of {full_walk}"
    );
}

/// A detached read stops within a few object reads of its interrupt
/// wherever in its walk the interrupt comes: planning, positioning a run's
/// postings (which no charge covers), stepping a source, probing, loading
/// rows. The reads are planned under the lock and run in an interrupt scope,
/// as `reads::Admitted::run` runs them; the store raises the interrupt at
/// every 3rd of the reads an undisturbed run makes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_interrupt_stops_every_phase_of_a_walk_within_a_few_reads() {
    let node = node(ReadLimits::default()).await;
    let plan = |method: &str, params: &Value| {
        let guard = node.rt.node.lock().unwrap();
        let Node::Full(host) = &*guard else {
            panic!("full node")
        };
        rpc_reads::plan(host, method, params, ReadLimits::default())
            .unwrap()
            .unwrap()
    };
    // A count whose 256-term run is walked and then positioned (`n != 1`
    // leaves an absent posting, so the sizes do not decide it), a sorted
    // page, an offset walk and a count that loads every row.
    for (method, params, answer) in [
        (
            "arkiv_getEntityCount",
            json!([{"query":"id >= u64(0) AND n != u64(1)"}]),
            json!(ENTITIES / 4 * 3),
        ),
        (
            "arkiv_query",
            json!(["n != u64(1) ORDER BY id:u64 DESC", {"limit": 2, "select": {"key": true}}]),
            json!([{"key": hex(&entity_key(ENTITIES - 1))}, {"key": hex(&entity_key(ENTITIES - 2))}]),
        ),
        (
            "arkiv_query",
            json!(["n != u64(1)", {"limit": 1, "offset": 150, "select": {"key": true}}]),
            json!([{"key": hex(&entity_key(200))}]),
        ),
        ("arkiv_getEntityCount", heavy_count(), json!(ENTITIES)),
    ] {
        let planned = plan(method, &params);
        let before = node.store.reads();
        let result = planned.run().unwrap();
        let total = node.store.reads() - before;
        let result = if method == "arkiv_query" {
            result["data"].clone()
        } else {
            result
        };
        assert_eq!(result, answer, "{params}");
        let mut worst = 0;
        for at in (1..=total).step_by(3) {
            let planned = plan(method, &params);
            let interrupt = Interrupt::new();
            let start = node.store.reads();
            *node.store.raise.lock().unwrap() = Some((start + at, interrupt.clone()));
            let stopped = interrupt.scope(|| planned.run());
            *node.store.raise.lock().unwrap() = None;
            let after = node.store.reads() - start - at;
            // Stopped short of the full walk: the read answers no result.
            assert!(after == total - at || stopped.is_err(), "{params} at {at}");
            worst = worst.max(after);
        }
        eprintln!("{params}: at most {worst} reads after an interrupt, of {total}");
        assert!(
            worst <= 16,
            "{params}: {worst} reads after an interrupt, of {total}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_reads_see_one_height_while_blocks_are_produced() {
    let node = Arc::new(node(limits(4, 30)).await);
    let client = client();
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
    while let Some(joined) = readers.join_next().await {
        let (reader, seen) = joined.unwrap();
        assert!(!seen.is_empty());
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
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cursor_walks_join_to_the_single_page_while_others_run() {
    let node = Arc::new(node(limits(4, 30)).await);
    let client = client();
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
