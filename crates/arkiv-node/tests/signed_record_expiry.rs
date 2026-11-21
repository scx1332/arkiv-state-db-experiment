//! Per-height records of a signed node expire with the state window: feeds,
//! blocks, receipts, inclusions and hash lookups below the record floor are
//! gone (`HistoryPruned`, or `null` for a hash), duplicate transactions stay
//! impossible (nonces) and request identities stay unique for
//! `REQUEST_IDENTITY_WINDOW` blocks, restart reads only the retained blocks,
//! followers still replay from genesis while bodies are kept, and archive
//! nodes keep everything.

use arkiv_db::durable::{Options, Retention};
use arkiv_db_memory::MemoryVersionedStore;
use arkiv_db_storage::versioned::{Table, VersionedStore};
use arkiv_node::{
    Expiry,
    arkiv::ArkivOperation,
    signed::{
        history::History,
        host::{Host, Prefill},
        protocol::{REQUEST_IDENTITY_WINDOW, SignedBlock, SignedGenesis, SignedTx},
        runtime::{self, Node, Runtime},
    },
    simulator::wire::hex,
};
use serde_json::{Value, json};
use std::sync::Arc;

mod signed_support;
use signed_support::{
    arkiv_ops,
    http::{call, serve},
    launch, sign_tx, signer,
};

fn genesis() -> SignedGenesis {
    signed_support::genesis(9100, 1000, [7; 16], 1, 100, &[1, 2, 3])
}

fn options(retention: Retention) -> Options {
    Options {
        retention,
        ..Options::default()
    }
}

struct Stores {
    state: Arc<dyn VersionedStore>,
    history: Arc<dyn VersionedStore>,
}

impl Stores {
    fn new() -> Self {
        Self {
            state: Arc::new(MemoryVersionedStore::new()),
            history: Arc::new(MemoryVersionedStore::new()),
        }
    }

    fn create(&self, retention: Retention, producer: bool) -> Host {
        let launch = launch(
            &genesis(),
            signer(1),
            [3; 16],
            self.history.clone(),
            producer,
        );
        Host::create(
            self.state.clone(),
            options(retention),
            launch,
            Prefill::None,
        )
        .unwrap()
    }

    fn reopen(&self, retention: Retention, producer: bool) -> Host {
        let genesis = genesis();
        Host::open(
            self.state.clone(),
            History::open(self.history.clone(), genesis.hash()).unwrap(),
            &genesis,
            producer.then(|| signer(1)),
            options(retention),
        )
        .unwrap()
    }
}

fn create_tx(key: u8, nonce: u64, request: u8, salt: u128) -> SignedTx {
    let ops = arkiv_ops(vec![ArkivOperation::Create {
        salt,
        flags: 0,
        expiry: Expiry {
            expires_at: 0,
            min_lifetime: 1_000,
        },
        attributes: vec![(
            arkiv_db::CellName::new("group").unwrap(),
            arkiv_db::Cell::attribute(arkiv_db::Value::U64(1)),
        )],
    }]);
    sign_tx(
        &genesis(),
        &signer(key),
        nonce,
        [request; 32],
        10_000_000,
        ops,
    )
}

fn code<T: std::fmt::Debug>(result: arkiv_node::signed::Result<T>) -> &'static str {
    result.expect_err("expected a refusal").code.as_str()
}

fn produce_to(host: &mut Host, height: u64) {
    while host.head.height < height {
        host.produce().unwrap();
    }
}

/// Head 2 holds `early` (actor 2, nonce 0, request 1); the chain then runs
/// until block 2 is `REQUEST_IDENTITY_WINDOW + 4` blocks old.
fn chain(stores: &Stores, retention: Retention) -> (Host, SignedTx, SignedBlock) {
    let mut host = stores.create(retention, true);
    host.produce().unwrap();
    let early = create_tx(2, 0, 1, 1);
    host.submit(early.clone()).unwrap();
    host.produce().unwrap();
    let block = host.block(2).unwrap();
    assert_eq!(block.inputs.txs, vec![early.clone()]);
    produce_to(&mut host, 2 + REQUEST_IDENTITY_WINDOW + 4);
    (host, early, block)
}

#[test]
fn records_expire_at_the_record_floor_and_duplicates_stay_refused() {
    let stores = Stores::new();
    let (mut host, early, block) = chain(&stores, Retention::Window(2));
    let head = host.head.height;
    // R = min(head - 2, head + 1 - 256) = head - 255.
    let floor = head + 1 - REQUEST_IDENTITY_WINDOW;
    assert_eq!(host.record_floor(), floor);
    assert!(floor > 2);
    let actor = signer(2).address();
    for height in [1, 2, floor - 1] {
        assert!(host.record_expired(height));
        assert_eq!(code(host.feed(height)), "HistoryPruned");
        assert_eq!(code(host.block(height)), "HistoryPruned");
        assert_eq!(code(host.certificate(height)), "HistoryPruned");
    }
    for height in [0, floor, head] {
        host.feed(height).unwrap();
        host.block(height).unwrap();
    }
    // Bodies are kept by default: replication and chain sync still serve
    // the expired heights.
    assert_eq!(host.replication_block(2).unwrap(), block);
    assert_eq!(host.chain_certificate(2).unwrap(), block.header);
    // The expired inclusion is gone from request status ...
    assert_eq!(
        host.request_status(actor, [1; 32]).unwrap()["status"],
        "unknown"
    );
    // ... but the transaction can never repeat: its nonce is spent.
    assert_eq!(code(host.submit(early.clone())), "NonceTooLow");
    // A request identity older than the window may be used again.
    let reused = create_tx(2, 1, 1, 2);
    host.submit(reused.clone()).unwrap();
    host.produce().unwrap();
    let status = host.request_status(actor, [1; 32]).unwrap();
    assert_eq!(status["status"], "included");
    assert_eq!(status["height"], host.head.height.to_string());
    // Inside the window it is still unique.
    let conflicting = create_tx(2, 2, 1, 3);
    assert_eq!(code(host.submit(conflicting)), "RequestConflict");
    // Expired rows are deleted from the state file, not only hidden.
    let pruned = host.engine.record_floor().pruned;
    assert!(pruned > 2, "{:?}", host.engine.record_floor());
    for height in 1..pruned {
        assert_eq!(
            stores
                .state
                .read_record(Table::Blocks, &height.to_be_bytes())
                .unwrap(),
            None
        );
    }
    // A window follower and an archive follower replay the chain from
    // genesis (bodies kept) and agree with the producer.
    for retention in [Retention::Window(2), Retention::Archive] {
        let follower_stores = Stores::new();
        let mut follower = follower_stores.create(retention, false);
        for height in 1..=host.head.height {
            follower
                .import(host.replication_block(height).unwrap())
                .unwrap();
        }
        assert_eq!(follower.head, host.head);
        assert_eq!(
            follower.request_status(actor, [1; 32]).unwrap(),
            host.request_status(actor, [1; 32]).unwrap()
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn json_rpc_and_feeds_answer_history_pruned_and_restart_reads_the_window_only() {
    let stores = Stores::new();
    let (host, early, _) = chain(&stores, Retention::Window(2));
    let floor = host.record_floor();
    let head = host.head.height;
    let rt = Runtime::new(Node::Full(Box::new(host)), None, None, None).unwrap();
    let early_hash = hex(early.hash().as_slice());
    assert_eq!(
        call(&rt, "eth_getTransactionByHash", json!([early_hash])).await["result"],
        Value::Null
    );
    assert_eq!(
        call(&rt, "eth_getTransactionReceipt", json!([early_hash])).await["result"],
        Value::Null
    );
    let refused = call(&rt, "eth_getBlockByNumber", json!(["0x2", false])).await;
    assert_eq!(refused["error"]["code"], -32006, "{refused}");
    let refused = call(
        &rt,
        "eth_getTransactionByBlockNumberAndIndex",
        json!(["0x2", "0x0"]),
    )
    .await;
    assert_eq!(refused["error"]["code"], -32006, "{refused}");
    let latest = call(&rt, "eth_getBlockByNumber", json!(["latest", false])).await;
    assert_eq!(latest["result"]["number"], format!("{head:#x}"));
    let history = call(&rt, "eth_feeHistory", json!(["0x400", "latest", []])).await;
    assert_eq!(history["result"]["oldestBlock"], format!("{floor:#x}"));
    let server = serve(runtime::public_router(rt.clone())).await;
    let client = reqwest::Client::new();
    let get = |path: String| {
        let client = client.clone();
        let url = format!("{}/signed/v1/{path}", server.url);
        async move { client.get(url).send().await.unwrap() }
    };
    let status: Value = get("status".into()).await.json().await.unwrap();
    assert_eq!(status["recordFloor"], floor.to_string());
    assert_eq!(status["bodyRetention"], "all");
    let gone = get("feed/blocks/2".into()).await;
    assert_eq!(gone.status(), 410);
    let body: Value = gone.json().await.unwrap();
    assert_eq!(body["error"], "HistoryPruned");
    assert_eq!(body["stateFloor"], floor.to_string());
    // A new feed reader starts at the oldest kept block.
    let feed: Value = get("feed/blocks?limit=2".into())
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(feed["blocks"][0]["header"]["height"], floor.to_string());
    // Headers for chain sync are still served from genesis (bodies kept).
    let headers = get("headers?limit=4".into()).await;
    assert_eq!(headers.status(), 200);
    drop(server);
    drop(rt);
    // Restart: the hash index is rebuilt from the window's blocks only.
    let host = stores.reopen(Retention::Window(2), true);
    assert_eq!(host.head.height, head);
    assert_eq!(host.record_floor(), floor);
    let latest = host.block(head).unwrap();
    let rt = Runtime::new(Node::Full(Box::new(host)), None, None, None).unwrap();
    let by_hash = call(
        &rt,
        "eth_getBlockByHash",
        json!([hex(latest.hash().as_slice()), false]),
    )
    .await;
    assert_eq!(by_hash["result"]["number"], format!("{head:#x}"));
    assert_eq!(
        call(&rt, "eth_getTransactionByHash", json!([early_hash])).await["result"],
        Value::Null
    );
}

#[test]
fn expired_bodies_leave_history_and_a_restart_still_opens() {
    let stores = Stores::new();
    let (mut host, _, _) = chain(&stores, Retention::Window(2));
    host.prune_bodies(true).unwrap();
    let target = host.head.height + 2;
    produce_to(&mut host, target);
    let floor = host.record_floor();
    let body_floor = host.history_pruned().unwrap().height;
    // The certificate just below the record floor is kept for readers at
    // the floor; everything older is gone.
    assert_eq!(body_floor, floor - 1);
    assert_eq!(code(host.replication_block(2)), "HistoryPruned");
    assert_eq!(code(host.chain_certificate(2)), "HistoryPruned");
    host.chain_certificate(floor - 1).unwrap();
    host.replication_block(floor).unwrap();
    for height in 1..body_floor {
        assert_eq!(
            stores
                .history
                .read_record(Table::Blocks, &height.to_be_bytes())
                .unwrap(),
            None
        );
    }
    let head = host.head.height;
    drop(host);
    let mut host = stores.reopen(Retention::Window(2), true);
    host.prune_bodies(true).unwrap();
    assert_eq!(host.head.height, head);
    host.produce().unwrap();
    assert!(host.history_pruned().unwrap().height >= body_floor);
}

#[test]
fn archive_keeps_every_record_and_lookup() {
    let stores = Stores::new();
    let (mut host, early, block) = chain(&stores, Retention::Archive);
    assert_eq!(host.record_floor(), 0);
    assert_eq!(host.block(2).unwrap(), block);
    host.feed(2).unwrap();
    let actor = signer(2).address();
    let status = host.request_status(actor, [1; 32]).unwrap();
    assert_eq!(status["status"], "included");
    assert_eq!(status["height"], "2");
    // The included transaction itself is answered with its inclusion.
    assert_eq!(host.submit(early).unwrap()["status"], "included");
    // The identity rule is the same on every node: an inclusion older than
    // the window no longer blocks the identity.
    host.submit(create_tx(2, 1, 1, 2)).unwrap();
    host.produce().unwrap();
    assert_eq!(
        host.request_status(actor, [1; 32]).unwrap()["height"],
        host.head.height.to_string()
    );
}

/// A file written before record expiry (no inclusion lists, no history
/// height index) is indexed by the migration, after which its records
/// expire like a new file's.
#[test]
fn migration_indexes_an_old_file_and_expiry_then_drains_it() {
    use arkiv_db_redb::{RedbConfig, RedbStore};
    use arkiv_node::signed::migrate::{migrate_records, needs_migration};
    let dir = std::env::temp_dir().join(format!("record-migration-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let open = || {
        (
            Arc::new(RedbStore::open(dir.join("node.redb"), RedbConfig::default()).unwrap()),
            Arc::new(RedbStore::open(dir.join("history.redb"), RedbConfig::default()).unwrap()),
        )
    };
    let (head, floor) = {
        let state =
            Arc::new(RedbStore::create(dir.join("node.redb"), RedbConfig::default()).unwrap());
        let history =
            Arc::new(RedbStore::create(dir.join("history.redb"), RedbConfig::default()).unwrap());
        let stores = Stores {
            state: state.clone(),
            history: history.clone(),
        };
        // Written as an archive: every record is kept.
        let (host, _, _) = chain(&stores, Retention::Archive);
        let head = host.head.height;
        drop((host, stores));
        // Strip what an older binary did not write.
        let mut lists = Vec::new();
        let mut rows = Vec::new();
        for height in 0..=head {
            let name = arkiv_db::durable::records::inclusion_list_key(height);
            if state
                .read_record(Table::Meta, name.as_bytes())
                .unwrap()
                .is_some()
            {
                lists.push((Table::Meta, name.into_bytes()));
            }
            rows.push((Table::Blocks, height.to_be_bytes().to_vec()));
        }
        // The bootstrap block and block 2 included one transaction each.
        assert_eq!(lists.len(), 2);
        rows.push((
            Table::Meta,
            arkiv_node::signed::history::INDEXED_KEY.to_vec(),
        ));
        state.rewrite_records_offline(&lists, &[]).unwrap();
        history.rewrite_records_offline(&rows, &[]).unwrap();
        assert!(needs_migration(history.as_ref()).unwrap());
        (head, head + 1 - REQUEST_IDENTITY_WINDOW)
    };
    let (state, history) = open();
    let report = migrate_records(&state, &history).unwrap();
    assert_eq!(report.inclusion_lists, 2);
    assert_eq!(report.inclusions, 2);
    assert_eq!(report.index_rows, head + 1);
    assert_eq!(report.unindexed, 0);
    assert!(!needs_migration(history.as_ref()).unwrap());
    // Idempotent.
    let again = migrate_records(&state, &history).unwrap();
    assert_eq!((again.inclusion_lists, again.index_rows), (0, 0));
    // Expiry now drains the whole backlog, inclusion rows included.
    let stores = Stores {
        state: state.clone(),
        history: history.clone(),
    };
    let mut host = stores.reopen(Retention::Window(2), true);
    host.prune_bodies(true).unwrap();
    while host.engine.record_floor().backlog() > 0 || host.record_floor() == 0 {
        host.produce().unwrap();
    }
    assert!(host.record_floor() >= floor);
    let identity = arkiv_db::encode(&create_tx(2, 0, 1, 1).identity());
    assert_eq!(
        state.read_record(Table::Inclusions, &identity).unwrap(),
        None
    );
    assert_eq!(
        state
            .read_record(Table::Blocks, &2u64.to_be_bytes())
            .unwrap(),
        None
    );
    while host.history_pruned().unwrap().height + 1 < host.record_floor() {
        host.produce().unwrap();
    }
    assert_eq!(
        history
            .read_record(Table::Blocks, &2u64.to_be_bytes())
            .unwrap(),
        None
    );
    drop((host, stores, state, history));
    let _ = std::fs::remove_dir_all(&dir);
}
