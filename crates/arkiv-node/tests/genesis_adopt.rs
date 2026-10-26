//! `--genesis-install adopt`: a fresh producer installs a streaming genesis by
//! verifying and relabelling its private staged store and renaming it over
//! the headless `node.redb`, instead of copying it.
//!
//! The library tests drive the same steps as the producer and stop between
//! them, as a crash would; the binary tests run `arkiv-proposer` itself.
use alloy_primitives::U256;
use arkiv_db::{
    Budget, Cell, CellName, Literal, Projection, Query, RecordKey, Term, Value, decode_exact,
    durable::Options, encode,
};
use arkiv_db_redb::{RedbConfig, RedbStore};
use arkiv_db_storage::versioned::{CommitBatch, CommitOutcome, Head, VersionedStore};
use arkiv_node::{
    accounts::AccountConfig,
    arkiv,
    genesis_adopt::{self, Staged},
    genesis_bundle::PrefillEntity,
    genesis_streaming::{build_bundle, import_redb_config, prepare_streaming},
    signed::{
        crypto::Signer,
        history::History,
        host::Host,
        protocol::{SignedGenesis, SignedHeader, SignedLimits, default_native_genesis},
    },
};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

const BIN: &str = env!("CARGO_BIN_EXE_arkiv-proposer");
const ROWS: u64 = 48;
const MARKER: &[u8] = b"adopt-tamper-marker-7f3e9a21";
/// The producer's default `--source-id`.
const SOURCE: [u8; 16] = [0, 0, 0, 0, 0, 0, 0x40, 0, 0x80, 0, 0, 0, 0, 0, 0, 1];

struct Temp(PathBuf);
impl Temp {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!("arkiv-adopt-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn signer() -> Signer {
    Signer::from_bytes([1; 32]).unwrap()
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn address() -> String {
    format!("0x{}", hex(signer().address().as_bytes()))
}

/// A funded base genesis, a canonical stream and a fixture key.
fn inputs(dir: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let proposer = signer().address();
    let base = SignedGenesis {
        genesis: default_native_genesis(9452, 1000),
        run_id: [9; 16],
        proposer: *proposer.as_bytes(),
        block_period_ms: 1000,
        limits: SignedLimits::default(),
        accounts: AccountConfig::new(vec![(proposer, U256::from(10).pow(U256::from(30)))]).unwrap(),
        prefill: None,
    };
    let mut stream = b"ARKPFL02".to_vec();
    stream.extend_from_slice(&arkiv_node::genesis_bundle::ROW_CODEC.to_be_bytes());
    stream.extend_from_slice(&ROWS.to_be_bytes());
    for n in 0..ROWS {
        let mut body = format!("entity {n} ").into_bytes();
        if n == 5 {
            body.extend_from_slice(MARKER);
        }
        let frame = encode(&PrefillEntity {
            creator: [2; 20],
            create_nonce: n,
            salt: n as u128,
            expires_at_height: 1000 + n / 4,
            flags: 0,
            attributes: vec![
                (
                    CellName::new("body").unwrap(),
                    Cell::field(Value::bytes(&body)),
                ),
                (
                    CellName::new("kind").unwrap(),
                    Cell::attribute(Value::str("fixture")),
                ),
                (
                    CellName::new("mod").unwrap(),
                    Cell::attribute(Value::U64(n % 3)),
                ),
                (CellName::new("n").unwrap(), Cell::attribute(Value::U64(n))),
            ],
        });
        stream.extend_from_slice(&(frame.len() as u32).to_be_bytes());
        stream.extend(frame);
    }
    let (base_path, stream_path, key) = (
        dir.join("base.bin"),
        dir.join("input.bin"),
        dir.join("fixture.key"),
    );
    std::fs::write(&base_path, encode(&base)).unwrap();
    std::fs::write(&stream_path, stream).unwrap();
    std::fs::write(&key, format!("0x{}", "01".repeat(32))).unwrap();
    (base_path, stream_path, key)
}

fn bundle(dir: &Path) -> (PathBuf, PathBuf, SignedGenesis, serde_json::Value) {
    let (base, input, key) = inputs(dir);
    let bundle = dir.join("bundle");
    let report = build_bundle(
        &base,
        &input,
        &bundle,
        *signer().address().as_bytes(),
        arkiv_node::genesis_streaming::DEFAULT_BATCH_BYTES,
    )
    .unwrap();
    let genesis = decode_exact(&std::fs::read(bundle.join("genesis.bin")).unwrap()).unwrap();
    (bundle, key, genesis, report)
}

fn history(run: &Path, genesis: &SignedGenesis) -> History {
    let path = run.join("history.redb");
    let existing = path.exists();
    let store = Arc::new(RedbStore::create(&path, RedbConfig::default()).unwrap());
    if existing {
        History::open(store, genesis.hash()).unwrap()
    } else {
        History::create(store, genesis.hash()).unwrap()
    }
}
fn certificate(genesis: &SignedGenesis) -> SignedHeader {
    SignedHeader::sign(genesis, genesis.header(), &signer()).unwrap()
}

/// The default installation, as `create_host` runs it.
fn install_copy(bundle: &Path, run: &Path, genesis: &SignedGenesis) -> Host {
    std::fs::create_dir_all(run).unwrap();
    let staged = prepare_streaming(
        genesis,
        &bundle.join("prefill.bin"),
        &run.join("prefill-work"),
        Options::default(),
    )
    .unwrap();
    let node = Arc::new(RedbStore::create(run.join("node.redb"), import_redb_config()).unwrap());
    Host::create_from_staged(
        node,
        history(run, genesis),
        genesis.clone(),
        SOURCE,
        certificate(genesis),
        Some(signer()),
        Options::default(),
        &staged,
        bundle.join("prefill.bin"),
    )
    .unwrap()
}

/// Where a simulated crash stops [`install_adopt`].
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stop {
    BeforeRelabel,
    AfterRelabel,
    Never,
}

/// The adoption, as the producer's `adopt_host` runs it, with `held` the
/// headless node database the producer opened first.
fn install_adopt(bundle: &Path, run: &Path, genesis: &SignedGenesis, stop: Stop) -> Option<Host> {
    std::fs::create_dir_all(run).unwrap();
    let node = run.join("node.redb");
    let held = Arc::new(RedbStore::create(&node, import_redb_config()).unwrap());
    assert_eq!(held.head().unwrap(), None);
    let mut history = history(run, genesis);
    let work = run.join("prefill-work");
    let staged = genesis_adopt::staged_path(&work);
    if genesis_adopt::inspect(&staged).unwrap() != Staged::Relabelled {
        let engine = prepare_streaming(
            genesis,
            &bundle.join("prefill.bin"),
            &work,
            Options::default(),
        )
        .unwrap();
        if stop == Stop::BeforeRelabel {
            return None;
        }
        genesis_adopt::check_rename(&staged, &node).unwrap();
        Host::adopt_staged(
            &mut history,
            genesis,
            SOURCE,
            certificate(genesis),
            Some(&signer()),
            Options::default(),
            engine,
            3,
        )
        .unwrap();
        if stop == Stop::AfterRelabel {
            return None;
        }
    }
    Host::check_adopted(
        Arc::new(RedbStore::open(&staged, RedbConfig::default()).unwrap()),
        genesis,
        SOURCE,
        Options::default(),
    )
    .unwrap();
    genesis_adopt::publish(&staged, &node, held.as_ref()).unwrap();
    drop(held);
    let mut host = Host::open(
        Arc::new(RedbStore::open(&node, RedbConfig::default()).unwrap()),
        history,
        genesis,
        Some(signer()),
        Options::default(),
    )
    .unwrap();
    host.prefill_file = Some(bundle.join("prefill.bin"));
    Some(host)
}

fn reopen(run: &Path, genesis: &SignedGenesis) -> Host {
    Host::open(
        Arc::new(RedbStore::open(run.join("node.redb"), RedbConfig::default()).unwrap()),
        history(run, genesis),
        genesis,
        Some(signer()),
        Options::default(),
    )
    .unwrap()
}

fn assert_same(adopted: &Host, copied: &Host, genesis: &SignedGenesis) {
    assert_eq!(adopted.head, copied.head);
    assert_eq!(adopted.head.state_root, genesis.header().state_root);
    assert_eq!(adopted.engine.head_info(), copied.engine.head_info());
    assert_eq!(adopted.identity, copied.identity);
    assert_eq!(
        adopted.record(0).unwrap().bytes().unwrap(),
        copied.record(0).unwrap().bytes().unwrap()
    );
    let (a, c) = (adopted.state(0).unwrap(), copied.state(0).unwrap());
    assert_eq!(a.state_header(), c.state_header());
    let namespace = arkiv::entity_namespace();
    for n in 0..ROWS {
        let key = arkiv::derive_entity_key(genesis.genesis.chain_id, &[2; 20], n, n as u128);
        let key = RecordKey::new(&key).unwrap();
        let get = |s: &arkiv_db::Snapshot| {
            s.try_get_record(namespace, &key, &Projection::ALL, Budget::unlimited())
                .unwrap()
                .unwrap()
                .value
        };
        assert!(get(&a).is_some(), "entity {n}");
        assert_eq!(get(&a), get(&c), "entity {n}");
    }
    for (attribute, value, expected) in [
        ("kind", Value::str("fixture"), ROWS),
        ("mod", Value::U64(1), ROWS / 3),
        ("n", Value::U64(7), 1),
    ] {
        let query = Query::all(vec![Literal::of(Term::eq(
            CellName::new(attribute).unwrap(),
            value,
        ))]);
        for s in [&a, &c] {
            assert_eq!(
                s.try_count(namespace, &query, Budget::unlimited())
                    .unwrap()
                    .unwrap()
                    .value,
                expected
            );
        }
    }
}

#[test]
fn adopted_node_equals_the_copy_restarts_and_keeps_producing() {
    let tmp = Temp::new("library");
    let (bundle, _, genesis, _) = bundle(&tmp.0);
    let copied = install_copy(&bundle, &tmp.0.join("copy"), &genesis);
    let run = tmp.0.join("adopt");
    let adopted = install_adopt(&bundle, &run, &genesis, Stop::Never).unwrap();
    assert_same(&adopted, &copied, &genesis);
    // The staged store became node.redb; nothing was copied.
    let staged = genesis_adopt::staged_path(&run.join("prefill-work"));
    assert_eq!(genesis_adopt::inspect(&staged).unwrap(), Staged::Absent);
    drop(adopted);
    let mut adopted = reopen(&run, &genesis);
    assert_same(&adopted, &copied, &genesis);
    let header = adopted.produce().unwrap().header;
    assert_eq!(header.height, 1);
    drop(adopted);
    assert_eq!(reopen(&run, &genesis).head.height, 1);
}

#[test]
fn an_interrupted_adoption_resumes_from_each_durable_state() {
    let tmp = Temp::new("resume");
    let (bundle, _, genesis, _) = bundle(&tmp.0);
    let copied = install_copy(&bundle, &tmp.0.join("copy"), &genesis);
    let staged = |run: &Path| genesis_adopt::staged_path(&run.join("prefill-work"));
    let headless = |run: &Path| {
        RedbStore::open(run.join("node.redb"), RedbConfig::default())
            .unwrap()
            .head()
            .unwrap()
            .is_none()
    };

    // Cut before the relabelling: the staged store is untouched and reused.
    let run = tmp.0.join("before");
    assert!(install_adopt(&bundle, &run, &genesis, Stop::BeforeRelabel).is_none());
    assert_eq!(
        genesis_adopt::inspect(&staged(&run)).unwrap(),
        Staged::Engine
    );
    assert!(headless(&run));
    let adopted = install_adopt(&bundle, &run, &genesis, Stop::Never).unwrap();
    assert_same(&adopted, &copied, &genesis);

    // Cut after the durable relabelling, before the rename: node.redb is
    // still the headless file, and only the check and rename remain.
    let run = tmp.0.join("after");
    assert!(install_adopt(&bundle, &run, &genesis, Stop::AfterRelabel).is_none());
    assert_eq!(
        genesis_adopt::inspect(&staged(&run)).unwrap(),
        Staged::Relabelled
    );
    assert!(headless(&run));
    // The copy installation refuses the consumed staged store instead of
    // serving or rebuilding over it.
    let error = prepare_streaming(
        &genesis,
        &bundle.join("prefill.bin"),
        &run.join("prefill-work"),
        Options::default(),
    )
    .err()
    .unwrap();
    assert!(error.contains("profile"), "{error}");
    let adopted = install_adopt(&bundle, &run, &genesis, Stop::Never).unwrap();
    assert_same(&adopted, &copied, &genesis);
    drop(adopted);
    // After the rename the node simply opens, scratch leftovers or not.
    assert_same(&reopen(&run, &genesis), &copied, &genesis);
}

#[test]
fn a_foreign_relabelled_store_or_initialized_node_is_never_replaced() {
    let tmp = Temp::new("foreign");
    let (bundle, _, genesis, _) = bundle(&tmp.0);
    let run = tmp.0.join("node");
    assert!(install_adopt(&bundle, &run, &genesis, Stop::AfterRelabel).is_none());
    let staged = genesis_adopt::staged_path(&run.join("prefill-work"));
    let open = || Arc::new(RedbStore::open(&staged, RedbConfig::default()).unwrap());
    let mut other = SOURCE;
    other[15] ^= 1;
    let error = Host::check_adopted(open(), &genesis, other, Options::default())
        .err()
        .unwrap();
    assert_eq!(error.0, "IdentityMismatch");
    Host::check_adopted(open(), &genesis, SOURCE, Options::default()).unwrap();

    // A node database with a head is never replaced.
    let node = run.join("node.redb");
    let held = RedbStore::open(&node, RedbConfig::default()).unwrap();
    let mut batch = CommitBatch::new(None, Head::default());
    batch.put_record(
        arkiv_db_storage::versioned::Table::Meta,
        "marker",
        b"x".to_vec(),
    );
    assert_eq!(held.commit(&batch), CommitOutcome::Committed);
    assert!(genesis_adopt::publish(&staged, &node, &held).is_err());
    assert_eq!(genesis_adopt::inspect(&staged).unwrap(), Staged::Relabelled);
}

// The producer binary.

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}
fn http(
    port: u16,
    method: &str,
    path: &str,
    body: Option<&str>,
    token: Option<&str>,
) -> Option<serde_json::Value> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .ok()?;
    let body = body.unwrap_or("");
    let auth = token.map_or(String::new(), |t| format!("Authorization: Bearer {t}\r\n"));
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .ok()?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).ok()?;
    let split = response.windows(4).position(|w| w == b"\r\n\r\n")?;
    serde_json::from_slice(&response[split + 4..]).ok()
}

const TOKEN: &str = "adopt-test-token-0123456789abcdef0123";

struct Producer {
    child: std::process::Child,
    log: PathBuf,
    port: u16,
    private: u16,
}
impl Producer {
    fn start(
        dir: &Path,
        node: &Path,
        bundle: &Path,
        key: &Path,
        install: Option<&str>,
        tag: &str,
    ) -> Self {
        let (port, private) = (free_port(), free_port());
        let log = dir.join(format!("{tag}.log"));
        let mut command = Command::new(BIN);
        command
            .arg("producer")
            .arg("--run-dir")
            .arg(node)
            .arg("--bundle")
            .arg(bundle)
            .arg("--key-file")
            .arg(key)
            .arg("--listen")
            .arg(format!("127.0.0.1:{port}"))
            .arg("--private-listen")
            .arg(format!("127.0.0.1:{private}"))
            .env("ARKIV_PROPOSER_TOKEN", TOKEN);
        if let Some(install) = install {
            command.arg("--genesis-install").arg(install);
        }
        let file = std::fs::File::create(&log).unwrap();
        let child = command
            .stdout(Stdio::null())
            .stderr(Stdio::from(file))
            .spawn()
            .unwrap();
        Self {
            child,
            log,
            port,
            private,
        }
    }
    fn ready(&mut self) -> Result<serde_json::Value, std::process::ExitStatus> {
        let deadline = Instant::now() + Duration::from_secs(120);
        while Instant::now() < deadline {
            if let Some(code) = self.child.try_wait().unwrap() {
                return Err(code);
            }
            if let Some(status) = http(self.port, "GET", "/signed/v1/status", None, None) {
                return Ok(status);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("producer readiness timed out: {}", self.log());
    }
    /// The full node's equality-page proof bytes for every fixture entity
    /// at height zero: identical state gives identical bytes.
    fn query(&self) -> serde_json::Value {
        let body = r#"{"height":"0","namespace":"1","attribute":"kind","valueType":"str","value":"fixture","limit":64,"cursor":null}"#;
        let resolved = http(
            self.port,
            "POST",
            "/signed/v1/query/resolve",
            Some(body),
            None,
        )
        .unwrap();
        let request = serde_json::json!({"request": resolved["request"]}).to_string();
        http(
            self.port,
            "POST",
            "/signed/v1/query/eq",
            Some(&request),
            None,
        )
        .unwrap()
    }
    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
}
impl Drop for Producer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn assert_genesis_head(status: &serde_json::Value, report: &serde_json::Value) {
    let head = &status["head"];
    assert_eq!(head["height"], "0");
    assert_eq!(head["hash"], report["genesisHash"]);
    assert_eq!(head["stateRoot"], report["stateRoot"]);
}

#[test]
fn producer_adopts_serves_the_copy_s_state_and_resumes() {
    let tmp = Temp::new("cli");
    let (bundle, key, genesis, report) = bundle(&tmp.0);
    let mut copy = Producer::start(&tmp.0, &tmp.0.join("copy"), &bundle, &key, None, "copy");
    let copied = copy
        .ready()
        .unwrap_or_else(|c| panic!("{c}: {}", copy.log()));
    assert_genesis_head(&copied, &report);
    let expected = copy.query();
    let proof = expected["proof"]
        .as_str()
        .unwrap_or_else(|| panic!("{expected}"));
    // Every fixture entity's unindexed body is in the proof page.
    assert!(proof.contains(&hex(b"entity 47 ")), "{expected}");
    assert!(!copy.log().contains("prefill_genesis_adopt"));
    drop(copy);

    let node = tmp.0.join("adopt");
    let mut adopt = Producer::start(&tmp.0, &node, &bundle, &key, Some("adopt"), "adopt");
    let adopted = adopt
        .ready()
        .unwrap_or_else(|c| panic!("{c}: {}", adopt.log()));
    assert_eq!(adopted["head"], copied["head"]);
    assert_eq!(adopted["identity"], copied["identity"]);
    assert_eq!(adopt.query(), expected);
    let log = adopt.log();
    assert!(log.contains(r#""stage":"relabelled""#), "{log}");
    assert!(log.contains(r#""resumed":false"#), "{log}");
    assert!(!node.join("prefill-work").exists());
    // The adopted node produces and restarts like any other.
    let stepped = http(
        adopt.private,
        "POST",
        "/signed/v1/control",
        Some(r#"{"action":"step"}"#),
        Some(TOKEN),
    )
    .unwrap();
    assert_eq!(stepped["head"]["height"], "1", "{stepped}");
    drop(adopt);
    let mut restarted = Producer::start(&tmp.0, &node, &bundle, &key, Some("adopt"), "restart");
    assert_eq!(restarted.ready().unwrap()["head"], stepped["head"]);
    assert!(!restarted.log().contains("prefill_genesis_adopt"));
    drop(restarted);

    // A cut after the durable relabelling resumes with only the rename.
    let node = tmp.0.join("resume");
    assert!(install_adopt(&bundle, &node, &genesis, Stop::AfterRelabel).is_none());
    std::fs::copy(bundle.join("prefill.bin"), node.join("prefill.bin")).unwrap();
    // The default copy installation refuses the consumed store and leaves
    // everything as it was.
    let mut refused = Producer::start(&tmp.0, &node, &bundle, &key, None, "refused");
    assert!(!refused.ready().unwrap_err().success());
    assert!(refused.log().contains("profile"), "{}", refused.log());
    drop(refused);
    let staged = genesis_adopt::staged_path(&node.join("prefill-work"));
    assert_eq!(genesis_adopt::inspect(&staged).unwrap(), Staged::Relabelled);
    let mut resumed = Producer::start(&tmp.0, &node, &bundle, &key, Some("adopt"), "resumed");
    let status = resumed
        .ready()
        .unwrap_or_else(|c| panic!("{c}: {}", resumed.log()));
    assert_eq!(status["head"], copied["head"]);
    assert_eq!(resumed.query(), expected);
    let log = resumed.log();
    assert!(log.contains(r#""resumed":true"#), "{log}");
    assert!(!log.contains("bulk_normalize"), "{log}");
    drop(resumed);

    // A cut before the relabelling reuses the staged store without a rebuild.
    let node = tmp.0.join("staged");
    assert!(install_adopt(&bundle, &node, &genesis, Stop::BeforeRelabel).is_none());
    let mut reused = Producer::start(&tmp.0, &node, &bundle, &key, Some("adopt"), "reused");
    assert_eq!(reused.ready().unwrap()["head"], copied["head"]);
    let log = reused.log();
    assert!(log.contains(r#""resumed":false"#), "{log}");
    assert!(!log.contains("bulk_normalize"), "{log}");
}

#[test]
fn producer_refuses_a_damaged_staged_store_and_misplaced_flags() {
    let tmp = Temp::new("refuse");
    let (bundle, key, genesis, report) = bundle(&tmp.0);
    let node = tmp.0.join("node");
    assert!(install_adopt(&bundle, &node, &genesis, Stop::BeforeRelabel).is_none());
    let staged = genesis_adopt::staged_path(&node.join("prefill-work"));
    let mut bytes = std::fs::read(&staged).unwrap();
    let mut found = 0;
    for at in 0..bytes.len() - MARKER.len() {
        if &bytes[at..at + MARKER.len()] == MARKER {
            bytes[at + MARKER.len() - 1] ^= 1;
            found += 1;
        }
    }
    assert!(found > 0, "fixture marker is stored in the staged store");
    std::fs::write(&staged, bytes).unwrap();
    for attempt in ["damaged", "damaged-again"] {
        let mut damaged = Producer::start(&tmp.0, &node, &bundle, &key, Some("adopt"), attempt);
        assert!(!damaged.ready().unwrap_err().success());
        assert!(damaged.log().contains("Corrupt"), "{}", damaged.log());
        // Verification failed before the relabelling: nothing was consumed.
        assert_eq!(genesis_adopt::inspect(&staged).unwrap(), Staged::Engine);
        assert!(
            RedbStore::open(node.join("node.redb"), RedbConfig::default())
                .unwrap()
                .head()
                .unwrap()
                .is_none()
        );
    }

    let allocations = tmp.0.join("allocations.json");
    std::fs::write(
        &allocations,
        format!(r#"{{"{}":"1000000000000000000"}}"#, address()),
    )
    .unwrap();
    let listen = format!("127.0.0.1:{}", free_port());
    let hash = report["genesisHash"].as_str().unwrap().to_string();
    let path = |name: &str| tmp.0.join(name).display().to_string();
    let follower = |role: &str| {
        let mut args: Vec<String> = vec![
            role.into(),
            "--run-dir".into(),
            path(role),
            "--peer".into(),
            "http://127.0.0.1:9".into(),
            "--genesis-hash".into(),
            hash.clone(),
            "--proposer".into(),
            address(),
            "--chain-id".into(),
            "9452".into(),
            "--source-id".into(),
            "00000000-0000-4000-8000-000000000001".into(),
            "--run-id".into(),
            "09".repeat(16),
            "--listen".into(),
            listen.clone(),
        ];
        if role == "full" {
            args.extend(["--replication-peer".into(), "http://127.0.0.1:9".into()]);
        }
        args
    };
    let refused: Vec<Vec<String>> = vec![
        // No streaming staged store to adopt.
        vec![
            "producer".into(),
            "--run-dir".into(),
            path("alloc"),
            "--key-file".into(),
            key.display().to_string(),
            "--allocations-file".into(),
            allocations.display().to_string(),
            "--listen".into(),
            listen.clone(),
            "--genesis-install".into(),
            "adopt".into(),
        ],
        // Unknown mode.
        vec![
            "producer".into(),
            "--run-dir".into(),
            path("bogus"),
            "--key-file".into(),
            key.display().to_string(),
            "--bundle".into(),
            bundle.display().to_string(),
            "--listen".into(),
            listen.clone(),
            "--genesis-install".into(),
            "move".into(),
        ],
        [
            follower("full"),
            vec!["--genesis-install".into(), "adopt".into()],
        ]
        .concat(),
        [
            follower("light"),
            vec!["--genesis-install".into(), "copy".into()],
        ]
        .concat(),
    ];
    for args in refused {
        let output = Command::new(BIN)
            .args(&args)
            .env("ARKIV_PROPOSER_TOKEN", TOKEN)
            .stdout(Stdio::null())
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{args:?}");
        assert!(stderr.contains("InvalidRequest"), "{args:?}: {stderr}");
    }
}
