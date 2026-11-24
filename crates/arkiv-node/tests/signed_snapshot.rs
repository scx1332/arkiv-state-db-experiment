//! Snapshots of a stopped full node (`arkiv_node::signed::snapshot`): the
//! pin keeps the bodies a node installed from a snapshot needs and they are
//! served below the record floor, a follower whose next block is gone finds
//! the gap, a snapshot is created from closed files only and installs into
//! a node that continues the chain, and what does not match the pins, the
//! recorded hashes or the peer is refused.

use arkiv_db::{
    Cell, CellName, RecordKey, Value as DbValue,
    durable::{Options, Retention},
};
use arkiv_db_memory::MemoryVersionedStore;
use arkiv_db_redb::{RedbConfig, RedbStore};
use arkiv_db_storage::versioned::VersionedStore;
use arkiv_node::{
    Expiry,
    arkiv::{self, ArkivOperation},
    signed::{
        ErrorCode,
        history::History,
        host::{Host, Prefill},
        light::Light,
        protocol::{REQUEST_IDENTITY_WINDOW, SignedGenesis, SignedTx},
        runtime::{self, Node, Runtime, Shared, SyncGap},
        snapshot::{
            self, DATABASE_FILES, InstallMode, MANIFEST_FILE, Manifest, PIN_FILE, PinMode, Pins,
            Snapshot, Stopped,
        },
    },
    simulator::wire::hex,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

mod signed_support;
use signed_support::{arkiv_ops, http::serve, launch, sign_tx, signer};

const TOKEN: &str = "signed-test-replication-token-0123456789";
const SOURCE: [u8; 16] = [3; 16];

fn genesis() -> SignedGenesis {
    signed_support::genesis(9200, 1000, [9; 16], 1, 100, &[1, 2, 3])
}

fn options(retention: Retention) -> Options {
    Options {
        retention,
        ..Options::default()
    }
}

/// A node of [`genesis`] with a two-snapshot state window over fresh
/// memory stores.
fn memory(producer: bool) -> Host {
    Host::create(
        Arc::new(MemoryVersionedStore::new()),
        options(Retention::Window(2)),
        launch(
            &genesis(),
            signer(1),
            SOURCE,
            Arc::new(MemoryVersionedStore::new()),
            producer,
        ),
        Prefill::None,
    )
    .unwrap()
}

fn tx(key: u8, nonce: u64, operations: Vec<ArkivOperation>) -> SignedTx {
    let mut request_id = [key; 32];
    request_id[..8].copy_from_slice(&nonce.to_be_bytes());
    sign_tx(
        &genesis(),
        &signer(key),
        nonce,
        request_id,
        10_000_000,
        arkiv_ops(operations),
    )
}

fn create_entity(salt: u128, body: &[u8]) -> ArkivOperation {
    ArkivOperation::Create {
        salt,
        flags: 0,
        expiry: Expiry {
            expires_at: 0,
            min_lifetime: 100_000,
        },
        attributes: vec![
            (
                CellName::new("body").unwrap(),
                Cell::field(DbValue::bytes(body)),
            ),
            (
                CellName::new("group").unwrap(),
                Cell::attribute(DbValue::U64(1)),
            ),
        ],
    }
}

fn entity_key(key: u8, creation_nonce: u64, salt: u128) -> RecordKey {
    RecordKey::new(&arkiv::derive_entity_key(
        genesis().genesis.chain_id,
        signer(key).address().as_bytes(),
        creation_nonce,
        salt,
    ))
    .unwrap()
}

fn produce_to(host: &mut Host, height: u64) {
    while host.head.height < height {
        host.produce().unwrap();
    }
}

/// Imports the producer's blocks after the follower's head up to `height`.
fn follow(follower: &mut Host, producer: &Host, height: u64) {
    for next in follower.head.height + 1..=height {
        follower
            .import(producer.replication_block(next).unwrap())
            .unwrap();
    }
}

fn code<T>(result: arkiv_node::signed::Result<T>) -> ErrorCode {
    result.err().expect("expected a refusal").code
}

fn full_rt(host: Host) -> Shared {
    Runtime::new(Node::Full(Box::new(host)), None, None, Some(TOKEN.into())).unwrap()
}

fn status(rt: &Shared) -> Value {
    runtime::status(&rt.node.lock().unwrap())
}

fn head(rt: &Shared) -> arkiv_node::Header {
    rt.node.lock().unwrap().head()
}

/// Polls until the follower has the head `height`.
async fn sync_to(rt: &Shared, height: u64) {
    for _ in 0..2_000 {
        if head(rt).height >= height {
            return;
        }
        runtime::sync_once(rt.clone()).await.unwrap();
    }
    panic!("the follower stopped at {}", head(rt).height);
}

/// A producer that expires bodies at head 10, and two followers stopped
/// there, as nodes installed from a snapshot at height 10 would be.
fn snapshot_at_ten() -> (Host, Host, Host) {
    let mut producer = memory(true);
    producer.prune_bodies(true).unwrap();
    producer.produce().unwrap();
    producer
        .submit(tx(2, 0, vec![create_entity(1, b"before the snapshot")]))
        .unwrap();
    produce_to(&mut producer, 10);
    let followers = [(); 2].map(|()| {
        let mut follower = memory(false);
        follow(&mut follower, &producer, 10);
        follower
    });
    let [first, second] = followers;
    (producer, first, second)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_pin_keeps_bodies_past_the_window_and_feeds_a_follower_far_behind() {
    let (mut producer, public_follower, private_follower) = snapshot_at_ten();
    assert_eq!(producer.body_pin(), None);
    producer.set_body_pin(Some(10));
    producer
        .submit(tx(2, 1, vec![create_entity(2, b"after the snapshot")]))
        .unwrap();
    let target = 10 + REQUEST_IDENTITY_WINDOW + 40;
    produce_to(&mut producer, target);
    // The record floor is far above the pin; bodies stop at the pin.
    let floor = producer.record_floor();
    assert!(floor > 12, "{floor}");
    assert_eq!(producer.history_pruned().unwrap().height, 10);
    assert_eq!(
        code(producer.chain_certificate(9)),
        ErrorCode::HistoryPruned
    );
    assert_eq!(code(producer.block_bytes(9)), ErrorCode::HistoryPruned);
    // Below the record floor the pinned heights are served from the height
    // index: the header for a follower's head, and every body after it.
    assert!(producer.record_expired(10) && producer.record_expired(floor - 1));
    assert_eq!(code(producer.certificate(10)), ErrorCode::HistoryPruned);
    assert_eq!(producer.chain_certificate(10).unwrap().header.height, 10);
    for height in [10, 11, floor - 1, floor, target] {
        producer.block_bytes(height).unwrap();
        producer.replication_block(height).unwrap();
    }

    let producer = full_rt(producer);
    let report = status(&producer);
    assert_eq!(report["bodyPin"], "10");
    assert_eq!(report["bodyFloor"], "10");
    assert_eq!(report["bodyRetention"], "window");
    let public = serve(runtime::public_router(producer.clone())).await;
    let private = serve(runtime::private_router(producer.clone())).await;

    // A follower more than a record window behind catches up from public
    // `GET blocks`, in batches.
    let follower = Runtime::new(
        Node::Full(Box::new(public_follower)),
        Some(public.url.clone()),
        None,
        None,
    )
    .unwrap();
    sync_to(&follower, target).await;
    assert_eq!(head(&follower), head(&producer));
    assert_eq!(follower.sync_gap(), None);
    assert_eq!(status(&follower)["bodyPin"], Value::Null);

    // And one that fetches each body from the private listener (the path
    // of peers before `GET blocks`).
    let follower = Runtime::new(
        Node::Full(Box::new(private_follower)),
        Some(public.url.clone()),
        Some(private.url.clone()),
        Some(TOKEN.into()),
    )
    .unwrap();
    sync_to(&follower, target).await;
    assert_eq!(head(&follower), head(&producer));
    assert_eq!(follower.sync_gap(), None);

    // Without the pin the kept bodies expire with the next blocks.
    {
        let mut node = producer.node.lock().unwrap();
        let Node::Full(host) = &mut *node else {
            unreachable!()
        };
        host.set_body_pin(None);
        while host.history_pruned().unwrap().height + 1 < host.record_floor() {
            host.produce().unwrap();
        }
        assert!(host.history_pruned().unwrap().height > 11);
        assert_eq!(code(host.chain_certificate(10)), ErrorCode::HistoryPruned);
        assert_eq!(code(host.block_bytes(11)), ErrorCode::HistoryPruned);
    }
    assert_eq!(status(&producer)["bodyPin"], Value::Null);
}

#[test]
fn a_pin_changes_nothing_while_bodies_are_kept() {
    let mut archive = memory(true);
    archive.set_body_pin(Some(200));
    produce_to(&mut archive, REQUEST_IDENTITY_WINDOW + 30);
    assert!(archive.record_floor() > 1);
    assert_eq!(archive.history_pruned().unwrap().height, 0);
    archive.block_bytes(1).unwrap();
    // A pin above what expires does not hold expiry back, and one below
    // what is already gone brings nothing back.
    let mut window = memory(true);
    window.prune_bodies(true).unwrap();
    window.set_body_pin(Some(REQUEST_IDENTITY_WINDOW + 500));
    produce_to(&mut window, REQUEST_IDENTITY_WINDOW + 30);
    let pruned = window.history_pruned().unwrap().height;
    assert_eq!(pruned, window.record_floor() - 1);
    window.set_body_pin(Some(3));
    window.produce().unwrap();
    assert!(window.history_pruned().unwrap().height >= pruned);
    assert_eq!(code(window.block_bytes(3)), ErrorCode::HistoryPruned);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_follower_whose_next_block_expired_on_its_peer_reports_the_gap() {
    let (mut producer, public_follower, private_follower) = snapshot_at_ten();
    // No pin: the bodies after height 10 expire.
    produce_to(&mut producer, 10 + REQUEST_IDENTITY_WINDOW + 40);
    assert!(producer.history_pruned().unwrap().height > 11);
    let producer = full_rt(producer);
    let public = serve(runtime::public_router(producer.clone())).await;
    let private = serve(runtime::private_router(producer.clone())).await;
    let gap = Some(SyncGap {
        head: 10,
        peer: public.url.clone(),
    });

    let follower = Runtime::new(
        Node::Full(Box::new(public_follower)),
        Some(public.url.clone()),
        None,
        None,
    )
    .unwrap();
    assert_eq!(follower.sync_gap(), None);
    let refused = runtime::sync_once(follower.clone()).await;
    assert_eq!(code(refused), ErrorCode::HistoryPruned);
    assert_eq!(follower.sync_gap(), gap);
    assert_eq!(head(&follower).height, 10);

    // The same over the header and private body path.
    let follower = Runtime::new(
        Node::Full(Box::new(private_follower)),
        Some(public.url.clone()),
        Some(private.url.clone()),
        Some(TOKEN.into()),
    )
    .unwrap();
    assert_eq!(
        code(runtime::sync_once(follower.clone()).await),
        ErrorCode::HistoryPruned
    );
    assert_eq!(follower.sync_gap(), gap);

    // Reads that answer HistoryPruned are not a gap: a pruned height asked
    // of the producer or of a follower in sync, and a failing poll for any
    // other reason.
    let client = reqwest::Client::new();
    let gone = client
        .get(format!("{}/signed/v1/feed/blocks/2", public.url))
        .send()
        .await
        .unwrap();
    assert_eq!(gone.status(), 410);
    assert_eq!(producer.sync_gap(), None);
    // A light client that cannot get the old headers keeps polling.
    let light = {
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
            Some(public.url.clone()),
            None,
            None,
        )
        .unwrap()
    };
    assert_eq!(
        code(runtime::sync_once(light.clone()).await),
        ErrorCode::HistoryPruned
    );
    assert_eq!(light.sync_gap(), None);
    assert_eq!(status(&light)["bodyPin"], Value::Null);
    // A follower whose peer has its next block, or no new block, has none.
    let mut source = memory(true);
    produce_to(&mut source, 5);
    let mut synced = memory(false);
    follow(&mut synced, &source, 4);
    let source = full_rt(source);
    let server = serve(runtime::public_router(source.clone())).await;
    let synced = Runtime::new(
        Node::Full(Box::new(synced)),
        Some(server.url.clone()),
        None,
        None,
    )
    .unwrap();
    for _ in 0..2 {
        runtime::sync_once(synced.clone()).await.unwrap();
        assert_eq!(head(&synced), head(&source));
        assert_eq!(synced.sync_gap(), None);
    }
}

/// A scratch directory of this test, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("arkiv-snapshot-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn redb(path: PathBuf, create: bool) -> Arc<dyn VersionedStore> {
    Arc::new(if create {
        RedbStore::create(path, RedbConfig::default()).unwrap()
    } else {
        RedbStore::open(path, RedbConfig::default()).unwrap()
    })
}

/// A new producer in `dir` that expires bodies.
fn create_producer(dir: &Path) -> Host {
    std::fs::create_dir_all(dir).unwrap();
    let mut host = Host::create(
        redb(dir.join("node.redb"), true),
        options(Retention::Window(2)),
        launch(
            &genesis(),
            signer(1),
            SOURCE,
            redb(dir.join("history.redb"), true),
            true,
        ),
        Prefill::None,
    )
    .unwrap();
    // As the node binary does for a new window state: garbage is collected.
    arkiv_node::retention::collect_new_state(&mut host.engine, dir).unwrap();
    host.prune_bodies(true).unwrap();
    host
}

fn open_producer(dir: &Path) -> Host {
    let genesis = genesis();
    let mut host = Host::open(
        redb(dir.join("node.redb"), false),
        History::open(redb(dir.join("history.redb"), false), genesis.hash()).unwrap(),
        &genesis,
        Some(signer(1)),
        options(Retention::Window(2)),
    )
    .unwrap();
    host.set_body_pin(snapshot::read_pin(dir).unwrap());
    host.prune_bodies(true).unwrap();
    host
}

fn pins() -> Pins {
    let genesis = genesis();
    Pins {
        genesis_hash: genesis.hash(),
        proposer: genesis.proposer,
        chain_id: genesis.genesis.chain_id,
        run_id: hex(&genesis.run_id)[2..].into(),
        source_id: arkiv_node::simulator::host::uuid(&SOURCE),
    }
}

fn sha256(path: &Path) -> String {
    hex(Sha256::digest(std::fs::read(path).unwrap()).as_slice())[2..].into()
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

/// A stopped producer in `dir/node` at head 14 with a created and a patched
/// entity, and its head.
fn stopped_producer(scratch: &Scratch) -> (PathBuf, arkiv_node::Header) {
    let dir = scratch.path("node");
    let mut host = create_producer(&dir);
    host.produce().unwrap();
    host.submit(tx(2, 0, vec![create_entity(1, b"first")]))
        .unwrap();
    host.submit(tx(3, 0, vec![create_entity(2, b"second")]))
        .unwrap();
    host.produce().unwrap();
    host.submit(tx(
        2,
        1,
        vec![ArkivOperation::Patch {
            key: entity_key(2, 0, 1),
            set: vec![(
                CellName::new("body").unwrap(),
                Cell::field(DbValue::bytes(b"first, patched")),
            )],
            remove: vec![],
        }],
    ))
    .unwrap();
    host.submit(tx(
        3,
        1,
        vec![ArkivOperation::Delete {
            key: entity_key(3, 0, 2),
        }],
    ))
    .unwrap();
    produce_to(&mut host, 14);
    (dir, host.head)
}

#[test]
fn a_snapshot_round_trip_installs_a_node_that_continues_the_chain() {
    let scratch = Scratch::new("round-trip");
    let (node, head) = stopped_producer(&scratch);
    let out = scratch.path("snapshot");
    let created = snapshot::create(&node, &out, PinMode::Set).unwrap();

    // The description: chain, head and every file as it is on disk.
    let manifest = created.manifest;
    let genesis = genesis();
    assert_eq!(manifest.format_version, 1);
    assert_eq!(manifest.identity(), pins().identity());
    assert_eq!(manifest.proposer, hex(&genesis.proposer));
    assert_eq!(manifest.height, "14");
    assert_eq!(manifest.header_hash, hex(head.hash().as_slice()));
    assert_eq!(manifest.state_root, hex(head.state_root.as_slice()));
    assert_eq!(manifest.body_floor, "0");
    assert_eq!(manifest.node_version, env!("CARGO_PKG_VERSION"));
    assert_eq!(manifest.created_at.len(), 20);
    assert_eq!(
        names(&out),
        [
            "certificate.bin",
            "genesis.bin",
            "history.redb",
            "node.redb",
            MANIFEST_FILE
        ]
    );
    let stored: Manifest =
        serde_json::from_slice(&std::fs::read(out.join(MANIFEST_FILE)).unwrap()).unwrap();
    assert_eq!(stored, manifest);
    for file in &manifest.files {
        let path = out.join(&file.name);
        assert_eq!(file.bytes, std::fs::metadata(&path).unwrap().len());
        assert_eq!(file.sha256, sha256(&path), "{}", file.name);
    }
    assert_eq!(
        std::fs::read(out.join("genesis.bin")).unwrap(),
        arkiv_db::encode(&genesis)
    );
    // The database files are the closed files of the node: the copy and
    // its check wrote to neither side.
    for (index, name) in DATABASE_FILES.iter().enumerate() {
        assert_eq!(
            sha256(&node.join(name)),
            manifest.files[index].sha256,
            "{name}"
        );
    }
    // The node is pinned at the snapshot.
    assert_eq!(created.pin, Some(14));
    assert_eq!(
        std::fs::read_to_string(node.join(PIN_FILE)).unwrap(),
        "14\n"
    );

    // Installed: the same head, no pin, no prefill, and what a follower
    // retains about the chain.
    let run = scratch.path("installed");
    let loaded = Snapshot::load(&out, &pins()).unwrap();
    let installed = loaded.install(&run, InstallMode::Copy, false).unwrap();
    assert_eq!(installed.head.height, 14);
    assert_eq!(installed.head.hash, head.hash());
    assert_eq!(installed.head.state_root, head.state_root);
    assert!(!installed.verified_state);
    assert_eq!(
        names(&run),
        [
            "certificate.bin",
            "genesis.bin",
            "history.redb",
            "node.redb",
            "trust.json"
        ]
    );
    let trust: runtime::GenesisResponse =
        serde_json::from_slice(&std::fs::read(run.join("trust.json")).unwrap()).unwrap();
    assert_eq!(trust.identity, pins().identity());
    trust
        .validate(genesis.hash(), genesis.proposer, genesis.genesis.chain_id)
        .unwrap();
    // The snapshot directory is left as it was.
    for file in &manifest.files {
        assert_eq!(file.sha256, sha256(&out.join(&file.name)));
    }

    // The producer continues past the record window; the installed node
    // follows it from the pinned bodies.
    let mut producer = open_producer(&node);
    assert_eq!(producer.body_pin(), Some(14));
    producer
        .submit(tx(2, 2, vec![create_entity(3, b"after the snapshot")]))
        .unwrap();
    let target = 14 + REQUEST_IDENTITY_WINDOW + 20;
    produce_to(&mut producer, target);
    assert_eq!(producer.history_pruned().unwrap().height, 14);
    let mut follower = Stopped::open(&run, Some(&genesis)).unwrap().host;
    assert_eq!(follower.head, head);
    assert!(!follower.is_producer());
    follow(&mut follower, &producer, target);
    assert_eq!(follower.head, producer.head);
}

#[test]
fn pin_modes_and_the_pin_commands() {
    let scratch = Scratch::new("pins");
    let (node, _) = stopped_producer(&scratch);
    // No pin is left alone, and none is invented.
    let leave = snapshot::create(&node, &scratch.path("a"), PinMode::Leave).unwrap();
    assert_eq!(leave.pin, None);
    assert!(!node.join(PIN_FILE).exists());
    // A lower pin of an older snapshot stays with `KeepLower` and moves
    // with `Set`.
    assert!(snapshot::pin(&node, 5).unwrap());
    assert_eq!(snapshot::read_pin(&node).unwrap(), Some(5));
    let keep = snapshot::create(&node, &scratch.path("b"), PinMode::KeepLower).unwrap();
    assert_eq!(keep.pin, Some(5));
    let set = snapshot::create(&node, &scratch.path("c"), PinMode::Set).unwrap();
    assert_eq!(set.pin, Some(14));
    // `KeepLower` without a pin sets one.
    assert!(snapshot::remove_pin(&node).unwrap());
    assert!(!snapshot::remove_pin(&node).unwrap());
    let keep = snapshot::create(&node, &scratch.path("d"), PinMode::KeepLower).unwrap();
    assert_eq!(keep.pin, Some(14));
    // A height the node cannot keep is refused: above its head ...
    assert_eq!(code(snapshot::pin(&node, 15)), ErrorCode::InvalidRequest);
    assert_eq!(snapshot::read_pin(&node).unwrap(), Some(14));
    // ... and a file that is not a height stops the node from starting.
    std::fs::write(node.join(PIN_FILE), "fourteen\n").unwrap();
    assert_eq!(code(snapshot::read_pin(&node)), ErrorCode::InvalidRequest);
    std::fs::write(node.join(PIN_FILE), "14").unwrap();
    assert_eq!(snapshot::read_pin(&node).unwrap(), Some(14));
    // While a node runs, the pin is written unchecked for its next start.
    let running = open_producer(&node);
    assert!(!snapshot::pin(&node, 9_999).unwrap());
    assert_eq!(snapshot::read_pin(&node).unwrap(), Some(9_999));
    drop(running);
    // Not a run directory of a full node.
    assert_eq!(
        code(snapshot::pin(&scratch.path("absent"), 1)),
        ErrorCode::InvalidRequest
    );
}

#[test]
fn a_pin_below_the_deleted_bodies_is_refused() {
    let scratch = Scratch::new("pin-floor");
    let node = scratch.path("node");
    let mut host = create_producer(&node);
    produce_to(&mut host, REQUEST_IDENTITY_WINDOW + 30);
    let floor = host.history_pruned().unwrap().height;
    assert!(floor > 2);
    drop(host);
    assert_eq!(
        code(snapshot::pin(&node, floor - 1)),
        ErrorCode::InvalidRequest
    );
    assert!(snapshot::pin(&node, floor).unwrap());
    // The snapshot records what its history no longer holds.
    let created = snapshot::create(&node, &scratch.path("out"), PinMode::Leave).unwrap();
    assert_eq!(created.manifest.body_floor, floor.to_string());
    assert_eq!(created.pin, Some(floor));
}

#[test]
fn create_refuses_a_running_node_and_an_occupied_out_directory() {
    let scratch = Scratch::new("create-refusals");
    let (node, _) = stopped_producer(&scratch);
    let out = scratch.path("out");
    // A node holds the database locks.
    let running = open_producer(&node);
    let refused = snapshot::create(&node, &out, PinMode::Set).unwrap_err();
    assert_eq!(refused.code, ErrorCode::StorageUnavailable);
    assert!(refused.cause.unwrap().contains("a node is running"));
    assert!(!out.exists() && !node.join(PIN_FILE).exists());
    drop(running);
    // An out directory with anything in it.
    std::fs::create_dir_all(&out).unwrap();
    std::fs::write(out.join("note.txt"), "kept").unwrap();
    let refused = snapshot::create(&node, &out, PinMode::Set).unwrap_err();
    assert_eq!(refused.code, ErrorCode::InvalidRequest);
    assert_eq!(names(&out), ["note.txt"]);
    assert!(!node.join(PIN_FILE).exists());
    // An empty one is used; a directory that is no node is refused.
    std::fs::remove_file(out.join("note.txt")).unwrap();
    snapshot::create(&node, &out, PinMode::Set).unwrap();
    assert_eq!(
        code(snapshot::create(
            &scratch.path("absent"),
            &scratch.path("x"),
            PinMode::Set
        )),
        ErrorCode::InvalidRequest
    );
}

#[test]
fn install_refuses_other_pins_damaged_files_and_an_occupied_run_directory() {
    let scratch = Scratch::new("install-refusals");
    let (node, _) = stopped_producer(&scratch);
    let out = scratch.path("snapshot");
    snapshot::create(&node, &out, PinMode::Leave).unwrap();
    let run = scratch.path("run");

    // Each pin, before any large file is read.
    let other = signed_support::genesis(9201, 1000, [8; 16], 2, 100, &[1, 2, 3]);
    for (field, wrong) in [
        (
            "genesisHash",
            Pins {
                genesis_hash: other.hash(),
                ..pins()
            },
        ),
        (
            "proposer",
            Pins {
                proposer: other.proposer,
                ..pins()
            },
        ),
        (
            "chainId",
            Pins {
                chain_id: 9201,
                ..pins()
            },
        ),
        (
            "runId",
            Pins {
                run_id: hex(&other.run_id)[2..].into(),
                ..pins()
            },
        ),
        (
            "sourceId",
            Pins {
                source_id: arkiv_node::simulator::host::uuid(&[4; 16]),
                ..pins()
            },
        ),
    ] {
        let refused = Snapshot::load(&out, &wrong).err().expect(field);
        assert_eq!(refused.code, ErrorCode::IdentityMismatch, "{field}");
        assert!(refused.cause.unwrap().contains(field), "{field}");
    }
    assert!(!run.exists());

    // A description edited to claim the pinned source: the database says
    // otherwise when it is opened, and the run directory is emptied.
    let edited = scratch.path("edited");
    std::fs::create_dir_all(&edited).unwrap();
    for name in names(&out) {
        std::fs::copy(out.join(&name), edited.join(&name)).unwrap();
    }
    let claimed = arkiv_node::simulator::host::uuid(&[4; 16]);
    let mut description: Value =
        serde_json::from_slice(&std::fs::read(out.join(MANIFEST_FILE)).unwrap()).unwrap();
    description["sourceId"] = json!(claimed);
    std::fs::write(
        edited.join(MANIFEST_FILE),
        serde_json::to_vec(&description).unwrap(),
    )
    .unwrap();
    let wrong = Pins {
        source_id: claimed,
        ..pins()
    };
    let loaded = Snapshot::load(&edited, &wrong).unwrap();
    assert_eq!(
        code(loaded.install(&run, InstallMode::Copy, false)),
        ErrorCode::IdentityMismatch
    );
    assert_eq!(names(&run), Vec::<String>::new());
    // A described head the files do not have.
    description["sourceId"] = json!(pins().source_id);
    description["height"] = json!("13");
    std::fs::write(
        edited.join(MANIFEST_FILE),
        serde_json::to_vec(&description).unwrap(),
    )
    .unwrap();
    let loaded = Snapshot::load(&edited, &pins()).unwrap();
    assert_eq!(
        code(loaded.install(&run, InstallMode::Copy, false)),
        ErrorCode::StorageCorrupt
    );
    assert_eq!(names(&run), Vec::<String>::new());

    // A database file with one changed byte: the hash, in both modes.
    let damaged = scratch.path("damaged");
    std::fs::create_dir_all(&damaged).unwrap();
    for name in names(&out) {
        std::fs::copy(out.join(&name), damaged.join(&name)).unwrap();
    }
    let mut bytes = std::fs::read(damaged.join("node.redb")).unwrap();
    let middle = bytes.len() / 2;
    bytes[middle] ^= 1;
    std::fs::write(damaged.join("node.redb"), &bytes).unwrap();
    let loaded = Snapshot::load(&damaged, &pins()).unwrap();
    for mode in [InstallMode::Copy, InstallMode::Move] {
        let refused = loaded.install(&run, mode, false).unwrap_err();
        assert_eq!(refused.code, ErrorCode::StorageCorrupt, "{mode:?}");
        assert!(refused.cause.unwrap().contains("node.redb"));
        assert_eq!(names(&run), Vec::<String>::new(), "{mode:?}");
        assert_eq!(names(&damaged).len(), 5, "{mode:?}");
    }
    // A truncated one, and a public file that is not the described one.
    std::fs::write(damaged.join("node.redb"), &bytes[..middle]).unwrap();
    assert_eq!(
        code(Snapshot::load(&damaged, &pins()).map(|_| ())),
        ErrorCode::StorageCorrupt
    );
    std::fs::copy(out.join("node.redb"), damaged.join("node.redb")).unwrap();
    Snapshot::load(&damaged, &pins()).unwrap();
    std::fs::write(damaged.join("certificate.bin"), b"not the certificate").unwrap();
    assert_eq!(
        code(Snapshot::load(&damaged, &pins()).map(|_| ())),
        ErrorCode::StorageCorrupt
    );

    // Not a snapshot: no description, or one of another format.
    let partial = scratch.path("partial");
    std::fs::create_dir_all(&partial).unwrap();
    std::fs::copy(out.join("node.redb"), partial.join("node.redb")).unwrap();
    assert_eq!(
        code(Snapshot::load(&partial, &pins()).map(|_| ())),
        ErrorCode::InvalidRequest
    );
    description["height"] = json!("14");
    description["formatVersion"] = json!(2);
    std::fs::write(
        edited.join(MANIFEST_FILE),
        serde_json::to_vec(&description).unwrap(),
    )
    .unwrap();
    assert_eq!(
        code(Snapshot::load(&edited, &pins()).map(|_| ())),
        ErrorCode::UnsupportedVersion
    );

    // A run directory that holds anything.
    let loaded = Snapshot::load(&out, &pins()).unwrap();
    std::fs::create_dir_all(&run).unwrap();
    std::fs::write(run.join("node.redb"), b"").unwrap();
    assert_eq!(
        code(loaded.install(&run, InstallMode::Copy, false)),
        ErrorCode::InvalidRequest
    );
    assert_eq!(names(&run), ["node.redb"]);
    std::fs::remove_file(run.join("node.redb")).unwrap();
    // The root of a fresh volume is empty enough.
    std::fs::create_dir_all(run.join("lost+found")).unwrap();
    loaded.install(&run, InstallMode::Copy, false).unwrap();
    // And it is not installed twice.
    assert_eq!(
        code(loaded.install(&run, InstallMode::Copy, false)),
        ErrorCode::InvalidRequest
    );
}

#[test]
fn install_by_move_and_with_state_verification() {
    let scratch = Scratch::new("move-verify");
    let (node, head) = stopped_producer(&scratch);
    let out = scratch.path("snapshot");
    snapshot::create(&node, &out, PinMode::Leave).unwrap();
    // The walk reads every object of the head state (created, patched and
    // deleted rows went into it) and the counts are rebuilt from them.
    let verified = scratch.path("verified");
    let installed = Snapshot::load(&out, &pins())
        .unwrap()
        .install(&verified, InstallMode::Copy, true)
        .unwrap();
    assert!(installed.verified_state);
    assert_eq!(installed.head.hash, head.hash());
    let counts = installed
        .rebuilt_gc
        .expect("a window state file collects garbage");
    assert!(counts["liveObjects"].as_u64().unwrap() > 0, "{counts}");
    assert!(!verified.join("gc-seed").exists());
    // The rebuilt file is a node: its counts agree with its objects, and it
    // follows the chain while garbage is collected.
    let store = RedbStore::open(verified.join("node.redb"), RedbConfig::default()).unwrap();
    arkiv_db::durable::gc::verify(&store).unwrap();
    drop(store);
    let mut producer = open_producer(&node);
    producer
        .submit(tx(2, 2, vec![create_entity(3, b"after the snapshot")]))
        .unwrap();
    produce_to(&mut producer, 24);
    let mut follower = Stopped::open(&verified, Some(&genesis())).unwrap().host;
    follow(&mut follower, &producer, 24);
    assert_eq!(follower.head, producer.head);
    drop((producer, follower));

    // Moved: the database files leave the snapshot directory.
    let moved = scratch.path("moved");
    let installed = Snapshot::load(&out, &pins())
        .unwrap()
        .install(&moved, InstallMode::Move, false)
        .unwrap();
    assert_eq!(installed.head.hash, head.hash());
    assert_eq!(
        names(&out),
        ["certificate.bin", "genesis.bin", MANIFEST_FILE]
    );
    assert_eq!(
        names(&moved),
        [
            "certificate.bin",
            "genesis.bin",
            "history.redb",
            "node.redb",
            "trust.json"
        ]
    );
    assert_eq!(
        Stopped::open(&moved, Some(&genesis())).unwrap().host.head,
        head
    );
}

/// The description of a snapshot of `host` at `height`, as `create` writes
/// it.
fn manifest_at(host: &Host, height: u64) -> Manifest {
    let header = host.chain_certificate(height).unwrap().header;
    Manifest {
        format_version: 1,
        chain_id: host.identity.chain_id.clone(),
        run_id: host.identity.run_id.clone(),
        source_id: host.identity.source_id.clone(),
        genesis_hash: host.identity.genesis_hash.clone(),
        proposer: hex(&host.genesis.proposer),
        height: height.to_string(),
        header_hash: hex(header.hash().as_slice()),
        state_root: hex(header.state_root.as_slice()),
        body_floor: "0".into(),
        node_version: "test".into(),
        created_at: "2026-11-24T00:00:00Z".into(),
        files: vec![],
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn install_asks_the_peer_whether_it_can_continue_the_snapshot() {
    let (mut producer, pinned, _) = snapshot_at_ten();
    let at_ten = manifest_at(&producer, 10);
    let mut wrong_block = at_ten.clone();
    wrong_block.header_hash = at_ten.state_root.clone();
    // A pinned follower of the same chain, and the producer, which is not
    // pinned and expires the bodies after the snapshot.
    let mut pinned = pinned;
    pinned.prune_bodies(true).unwrap();
    pinned.set_body_pin(Some(10));
    let target = 10 + REQUEST_IDENTITY_WINDOW + 40;
    while producer.head.height < target {
        producer.produce().unwrap();
        follow(&mut pinned, &producer, producer.head.height);
    }
    let recent = manifest_at(&producer, target - 3);
    let floor = producer.history_pruned().unwrap().height;
    let at_floor = manifest_at(&producer, floor);
    let below_floor = Manifest {
        height: (floor - 1).to_string(),
        ..at_floor.clone()
    };
    let ahead = Manifest {
        height: (target + 5).to_string(),
        ..at_ten.clone()
    };
    let light = Light::create(
        Arc::new(MemoryVersionedStore::new()),
        genesis(),
        producer.certificate(0).unwrap(),
    )
    .unwrap();
    let light = Runtime::new(
        Node::Light {
            light: Box::new(light),
            identity: producer.identity.clone(),
        },
        None,
        None,
        None,
    )
    .unwrap();
    let producer = serve(runtime::public_router(full_rt(producer))).await;
    let pinned = serve(runtime::public_router(full_rt(pinned))).await;
    let light = serve(runtime::public_router(light)).await;
    let client = reqwest::Client::new();
    let check = |peer: &str, manifest: &Manifest| {
        let (client, peer, manifest) = (client.clone(), peer.to_owned(), manifest.clone());
        async move { snapshot::check_peer(&client, &peer, &manifest).await }
    };

    // The pinned node continues the old snapshot and a recent one.
    let answer = check(&pinned.url, &at_ten).await.unwrap();
    assert_eq!(
        (answer.head, answer.body_floor, answer.behind),
        (target, 10, false)
    );
    assert_eq!(answer.role, "full");
    check(&pinned.url, &recent).await.unwrap();
    // The producer continues only what its bodies still reach: it needs the
    // snapshot's own block as well as the ones after it.
    let refused = check(&producer.url, &at_ten).await.unwrap_err();
    assert_eq!(refused.code, ErrorCode::HistoryPruned);
    let cause = refused.cause.unwrap();
    assert!(
        cause.contains("cannot continue a snapshot at height 10"),
        "{cause}"
    );
    assert!(cause.contains(&format!("from height {floor}")), "{cause}");
    assert_eq!(
        code(check(&producer.url, &below_floor).await),
        ErrorCode::HistoryPruned
    );
    let answer = check(&producer.url, &at_floor).await.unwrap();
    assert_eq!(
        (answer.body_floor, answer.role.as_str()),
        (floor, "producer")
    );
    check(&producer.url, &recent).await.unwrap();
    // A peer with another block at the snapshot height is refused; one
    // that has not reached the snapshot yet is not.
    assert_eq!(
        code(check(&pinned.url, &wrong_block).await),
        ErrorCode::ChainConflict
    );
    assert!(check(&pinned.url, &ahead).await.unwrap().behind);
    // Another chain, and a node without bodies.
    let other = Manifest {
        chain_id: "9201".into(),
        ..at_ten.clone()
    };
    assert_eq!(
        code(check(&pinned.url, &other).await),
        ErrorCode::IdentityMismatch
    );
    let other = Manifest {
        proposer: hex(signer(2).address().as_bytes()),
        ..at_ten.clone()
    };
    assert_eq!(
        code(check(&pinned.url, &other).await),
        ErrorCode::IdentityMismatch
    );
    assert_eq!(
        code(check(&light.url, &at_ten).await),
        ErrorCode::FullNodeRequired
    );
}
