//! `arkiv-proposer snapshot` and the follower start around it, through the
//! node binary on a chain with a streaming prefill: a stopped follower is
//! snapshotted and pinned, a node installed from the snapshot more than a
//! record window later starts without the prefill and catches up from the
//! pinned node, and one pointed at a peer that pruned its next block exits.

mod cli_support;
mod signed_support;

use alloy_primitives::U256;
use arkiv_db::{Cell, CellName, Value, encode};
use arkiv_node::{
    accounts::AccountConfig,
    genesis_bundle::PrefillEntity,
    signed::protocol::{SignedGenesis, SignedLimits, default_native_genesis},
};
use cli_support::{Child, Temp, free_port, http, ready, refused};
use signed_support::signer;
use std::{
    io::{Read, Write},
    net::TcpStream,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const BIN: &str = env!("CARGO_BIN_EXE_arkiv-proposer");
const TOKEN: &str = "signed-test-replication-token-0123456789";
/// Blocks are made on request; the period only spaces their timestamps.
const PERIOD_MS: u64 = 20;
/// Records, and with `--body-retention window` bodies, expire once this
/// many blocks old.
const WINDOW: u64 = 256;

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| (*arg).to_owned()).collect()
}

/// A streaming genesis bundle with 24 prefilled entities and fast blocks,
/// and the proposer's key file.
fn bundle(dir: &Path) -> (PathBuf, PathBuf) {
    let proposer = signer(1).address();
    let base = SignedGenesis {
        genesis: default_native_genesis(9452, 1000, PERIOD_MS),
        run_id: [0x5a; 16],
        proposer: *proposer.as_bytes(),
        block_period_ms: PERIOD_MS,
        limits: SignedLimits::default(),
        accounts: AccountConfig::new(vec![(proposer, U256::from(10).pow(U256::from(30)))]).unwrap(),
        prefill: None,
    };
    let rows: Vec<PrefillEntity> = (0..24u64)
        .map(|n| PrefillEntity {
            creator: [2; 20],
            create_nonce: n,
            salt: n as u128,
            expires_at_height: 100_000 + n,
            flags: 0,
            attributes: vec![
                (
                    CellName::new("body").unwrap(),
                    Cell::field(Value::bytes(format!("entity {n}").as_bytes())),
                ),
                (CellName::new("n").unwrap(), Cell::attribute(Value::U64(n))),
            ],
        })
        .collect();
    let (base_path, stream, key, out) = (
        dir.join("base.bin"),
        dir.join("input.bin"),
        dir.join("proposer.key"),
        dir.join("bundle"),
    );
    std::fs::write(&base_path, encode(&base)).unwrap();
    std::fs::write(
        &stream,
        arkiv_node::genesis_streaming::write_stream(rows.iter()),
    )
    .unwrap();
    std::fs::write(&key, format!("0x{}", "01".repeat(32))).unwrap();
    let built = Command::new(BIN)
        .args(["genesis", "stream-build"])
        .arg(&base_path)
        .arg("--prefill")
        .arg(&stream)
        .arg("--out")
        .arg(&out)
        .args(["--sponsor", &arkiv_node::text::hex(proposer.as_bytes())])
        .output()
        .unwrap();
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    (out, key)
}

/// A node process with its log and public port.
struct Node {
    child: Child,
    log: PathBuf,
    port: u16,
}

impl Node {
    fn start(dir: &Path, name: &str, port: u16, args: &[String]) -> Self {
        let log = dir.join(format!("{name}.log"));
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
            .unwrap();
        let child = Child(
            Command::new(BIN)
                .args(args)
                .env("ARKIV_PROPOSER_TOKEN", TOKEN)
                .stdout(Stdio::null())
                .stderr(Stdio::from(file))
                .spawn()
                .unwrap(),
        );
        Self { child, log, port }
    }

    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn status(&mut self) -> serde_json::Value {
        ready(&mut self.child, self.port, "/signed/v1/status", &self.log)
            .unwrap_or_else(|code| panic!("{code}: {}", self.log()))
    }

    fn height(&mut self) -> u64 {
        self.status()["head"]["height"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap()
    }

    /// Waits until `reached` holds for the node's status.
    fn until(&mut self, what: &str, reached: impl Fn(&serde_json::Value) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(180);
        loop {
            let status = self.status();
            if reached(&status) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}: {status}\n{}",
                self.log()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn until_height(&mut self, height: u64) {
        self.until(&format!("height {height}"), |status| {
            status["head"]["height"]
                .as_str()
                .is_some_and(|found| found.parse::<u64>().unwrap() >= height)
        });
    }

    /// SIGTERM, and the exit status of a clean stop.
    fn stop(mut self) {
        let pid = self.child.0.id().to_string();
        assert!(
            Command::new("kill")
                .args(["-TERM", &pid])
                .status()
                .unwrap()
                .success()
        );
        let code = self.wait_exit();
        assert_eq!(code, Some(0), "{}", self.log());
    }

    /// The exit code once the node exits by itself.
    fn wait_exit(&mut self) -> Option<i32> {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            if let Some(status) = self.child.0.try_wait().unwrap() {
                return status.code();
            }
            assert!(
                Instant::now() < deadline,
                "the node keeps running: {}",
                self.log()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// Runs a command that must succeed and returns the JSON it prints.
fn run(args: &[String]) -> serde_json::Value {
    let output = Command::new(BIN).args(args).output().unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|_| panic!("{args:?}: {}", String::from_utf8_lossy(&output.stdout)))
}

/// The HTTP status and body of one GET.
fn get(port: u16, path: &str) -> (u16, Vec<u8>) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    let split = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap();
    let status = std::str::from_utf8(&response[9..12])
        .unwrap()
        .parse()
        .unwrap();
    (status, response[split + 4..].to_vec())
}

fn number(value: &serde_json::Value) -> u64 {
    value.as_str().unwrap().parse().unwrap()
}

fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

#[test]
fn a_prefilled_chain_is_snapshotted_installed_and_continued() {
    let tmp = Temp::new("arkiv-snapshot-cli");
    let dir = &tmp.0;
    let (bundle, key) = bundle(dir);
    let path = |name: &str| dir.join(name).display().to_string();
    let (producer_port, private_port) = (free_port(), free_port());
    // The producer stays paused and makes one block per `step`, so it is
    // never further ahead of a follower than the test lets it be: with
    // `--body-retention window` a follower more than 256 blocks behind is
    // lost, which is what the end of this test shows on purpose.
    let step = |blocks: u64| {
        for _ in 0..blocks {
            let deadline = Instant::now() + Duration::from_secs(60);
            loop {
                let answer = http(
                    private_port,
                    "POST",
                    "/signed/v1/control",
                    Some("{\"action\":\"step\"}"),
                    Some(TOKEN),
                );
                // The status after the block; a busy node is asked again.
                if answer
                    .as_ref()
                    .is_some_and(|status| status["head"].is_object())
                {
                    break;
                }
                assert!(Instant::now() < deadline, "step: {answer:?}");
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    };
    // The producer expires bodies with its records.
    let mut producer = Node::start(
        dir,
        "producer",
        producer_port,
        &strings(&[
            "producer",
            "--run-dir",
            &path("producer"),
            "--bundle",
            &bundle.display().to_string(),
            "--key-file",
            &key.display().to_string(),
            "--listen",
            &format!("127.0.0.1:{producer_port}"),
            "--private-listen",
            &format!("127.0.0.1:{private_port}"),
            "--state-retention",
            "8",
            "--body-retention",
            "window",
        ]),
    );
    let status = producer.status();
    assert_eq!(status["capabilities"]["genesisPrefill"], true);
    assert_eq!(status["bodyPin"], serde_json::Value::Null);
    let identity = &status["identity"];
    let text = |value: &serde_json::Value| value.as_str().unwrap().to_owned();
    let pins = [
        "--genesis-hash".to_owned(),
        text(&identity["genesisHash"]),
        "--proposer".to_owned(),
        text(&status["proposer"]),
        "--chain-id".to_owned(),
        text(&identity["chainId"]),
        "--source-id".to_owned(),
        text(&identity["sourceId"]),
        "--run-id".to_owned(),
        text(&identity["runId"]),
    ];
    let full = |name: &str, port: u16, peer: u16| {
        let mut args = strings(&[
            "full",
            "--run-dir",
            &path(name),
            "--listen",
            &format!("127.0.0.1:{port}"),
            "--peer",
            &format!("http://127.0.0.1:{peer}"),
            "--state-retention",
            "8",
            "--body-retention",
            "window",
        ]);
        args.extend(pins.iter().cloned());
        Node::start(dir, name, port, &args)
    };
    let install = |name: &str, extra: &[&str]| {
        let mut args = strings(&[
            "snapshot",
            "install",
            "--from",
            &path("snapshot"),
            "--run-dir",
            &path(name),
        ]);
        args.extend(pins.iter().cloned());
        args.extend(strings(extra));
        args
    };

    // A follower from genesis downloads the prefill and keeps it.
    let follower_port = free_port();
    let mut follower = full("follower", follower_port, producer_port);
    follower.status();
    let prefill = dir.join("follower/prefill.bin");
    assert!(prefill.is_file());
    step(6);
    follower.until_height(6);

    // A running node is not snapshotted.
    let create = strings(&[
        "snapshot",
        "create",
        "--run-dir",
        &path("follower"),
        "--out",
        &path("snapshot"),
    ]);
    let stderr = refused(BIN, &create, &[]);
    assert!(stderr.contains("a node is running"), "{stderr}");
    assert!(!dir.join("snapshot").exists());

    // Stopped at the producer's head, snapshotted, pinned.
    let stopped_at = producer.height();
    follower.until_height(stopped_at);
    follower.stop();
    let created = run(&create);
    let snapshot = &created["snapshot"];
    let height = number(&snapshot["height"]);
    assert_eq!(height, stopped_at);
    assert_eq!(snapshot["formatVersion"], 1);
    assert_eq!(snapshot["genesisHash"], identity["genesisHash"]);
    assert_eq!(created["pin"], height.to_string());
    assert_eq!(
        entries(&dir.join("snapshot")),
        [
            "certificate.bin",
            "genesis.bin",
            "history.redb",
            "node.redb",
            "snapshot.json"
        ]
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("follower/snapshot-pin")).unwrap(),
        format!("{height}\n")
    );
    assert!(dir.join("follower/prefill.bin").is_file());

    // Restarted: it reports the pin, and with its prefill.bin still there
    // it audits and serves the prefill as before.
    let mut follower = full("follower", follower_port, producer_port);
    assert_eq!(follower.status()["bodyPin"], height.to_string());
    assert!(!follower.log().contains("prefill_skipped"));
    let (code, bytes) = get(follower_port, "/signed/v1/prefill/chunk?offset=0&length=8");
    assert_eq!(code, 200);
    assert_eq!(bytes, std::fs::read(&prefill).unwrap()[..8]);

    // More than a record window later the producer has dropped the bodies
    // after the snapshot; the pinned follower has them all.
    let later = height + WINDOW + 40;
    while producer.height() < later {
        step(48);
        follower.until_height(producer.height());
    }
    producer.until("the producer's bodies to pass the snapshot", |status| {
        number(&status["bodyFloor"]) > height + 1
    });
    let pinned = follower.status();
    assert_eq!(pinned["bodyFloor"], height.to_string());
    assert!(number(&pinned["recordFloor"]) > height + 1, "{pinned}");

    // The producer cannot continue the snapshot: refused before any file
    // is copied.
    let stderr = refused(
        BIN,
        &install(
            "refused",
            &["--peer", &format!("http://127.0.0.1:{producer_port}")],
        ),
        &[],
    );
    assert!(stderr.contains("cannot continue a snapshot"), "{stderr}");
    assert!(stderr.contains("HistoryPruned"), "{stderr}");
    assert!(!dir.join("refused").exists());

    // The pinned follower can. The installed directory has no prefill and
    // no pin, and the node starts without either.
    let installed = run(&install(
        "installed",
        &[
            "--peer",
            &format!("http://127.0.0.1:{follower_port}"),
            "--verify-state",
        ],
    ));
    assert_eq!(installed["head"]["height"], height.to_string());
    assert_eq!(installed["head"]["hash"], snapshot["headerHash"]);
    assert_eq!(installed["peer"]["role"], "full");
    assert_eq!(installed["verifiedState"], true);
    assert!(installed["rebuiltGc"]["liveObjects"].as_u64().unwrap() > 24);
    let installed_files = [
        "certificate.bin",
        "genesis.bin",
        "history.redb",
        "node.redb",
        "trust.json",
    ];
    assert_eq!(entries(&dir.join("installed")), installed_files);
    let installed_port = free_port();
    let mut node = full("installed", installed_port, follower_port);
    let first = node.status();
    assert!(number(&first["head"]["height"]) >= height, "{first}");
    assert_eq!(first["bodyPin"], serde_json::Value::Null);
    let log = node.log();
    assert!(log.contains("prefill_skipped"), "{log}");
    assert!(!log.contains("prefill_progress"), "{log}");
    assert_eq!(entries(&dir.join("installed")), installed_files);
    // It serves no prefill: nothing it has was audited.
    for query in ["prefill/chunk?offset=0&length=8", "prefill"] {
        let (code, body) = get(installed_port, &format!("/signed/v1/{query}"));
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            (code, body["error"].as_str()),
            (422, Some("PrefillBundleRequired"))
        );
    }
    // It catches up from the pinned follower to the same chain.
    let target = producer.height();
    follower.until_height(target);
    node.until_height(target);
    let block = format!("/signed/v1/feed/blocks/{target}");
    let expected = http(producer_port, "GET", &block, None, None).unwrap();
    assert_eq!(expected["header"]["height"], target.to_string());
    for port in [follower_port, installed_port] {
        assert_eq!(http(port, "GET", &block, None, None).unwrap(), expected);
    }
    let synced = node.status();
    assert_eq!(synced["head"], producer.status()["head"]);
    assert_eq!(synced["health"], "following");
    // And follows what comes next.
    step(3);
    node.until_height(target + 3);
    node.stop();

    // A node installed from the same snapshot and pointed at the producer
    // cannot recover: it says so once and exits with its own status.
    run(&install("stranded", &[]));
    let mut stranded = full("stranded", free_port(), producer_port);
    assert_eq!(stranded.wait_exit(), Some(3), "{}", stranded.log());
    let log = stranded.log();
    assert_eq!(log.matches("sync_gap").count(), 1, "{log}");
    assert!(log.contains("reinstalled from a newer snapshot"), "{log}");
    assert!(log.contains(&format!("\"head\":\"{height}\"")), "{log}");
    assert!(
        log.contains(&format!("http://127.0.0.1:{producer_port}")),
        "{log}"
    );

    // Unpinned and restarted, the follower lets the kept bodies expire.
    follower.stop();
    let unpinned = run(&strings(&[
        "snapshot",
        "unpin",
        "--run-dir",
        &path("follower"),
    ]));
    assert_eq!(unpinned["removed"], true);
    assert!(!dir.join("follower/snapshot-pin").exists());
    let mut follower = full("follower", follower_port, producer_port);
    assert_eq!(follower.status()["bodyPin"], serde_json::Value::Null);
    step(8);
    follower.until("the unpinned bodies to expire", |status| {
        number(&status["bodyFloor"]) > height + 1
    });
    // Pinned again for the next start, checked against the stopped node.
    follower.stop();
    let floor = {
        let stderr = refused(
            BIN,
            &strings(&[
                "snapshot",
                "pin",
                "--run-dir",
                &path("follower"),
                "--height",
                &height.to_string(),
            ]),
            &[],
        );
        assert!(stderr.contains("cannot be pinned"), "{stderr}");
        let mut follower = full("follower", follower_port, producer_port);
        let floor = number(&follower.status()["bodyFloor"]);
        follower.stop();
        floor
    };
    let pinned = run(&strings(&[
        "snapshot",
        "pin",
        "--run-dir",
        &path("follower"),
        "--height",
        &floor.to_string(),
    ]));
    assert_eq!(pinned["pin"], floor.to_string());
    assert_eq!(pinned["checked"], true);
    let mut follower = full("follower", follower_port, producer_port);
    assert_eq!(follower.status()["bodyPin"], floor.to_string());
}

#[test]
fn snapshot_command_lines_are_checked() {
    let tmp = Temp::new("arkiv-snapshot-cli-usage");
    let dir = tmp.0.display().to_string();
    for args in [
        vec!["snapshot"],
        vec!["snapshot", "restore", "--run-dir", &dir],
        vec!["snapshot", "create", "--run-dir", &dir],
        vec!["snapshot", "create", "--run-dir", &dir, "--out"],
        vec![
            "snapshot",
            "create",
            "--run-dir",
            &dir,
            "--out",
            &dir,
            "--no-pin",
            "--keep-pin",
        ],
        vec![
            "snapshot",
            "create",
            "--run-dir",
            &dir,
            "--out",
            &dir,
            "--move",
        ],
        vec!["snapshot", "pin", "--run-dir", &dir],
        vec!["snapshot", "unpin", "--run-dir", &dir, "--height", "3"],
        vec!["snapshot", "install", "--from", &dir, "--run-dir", &dir],
    ] {
        let stderr = refused(BIN, &strings(&args), &[]);
        assert!(stderr.contains("InvalidRequest"), "{args:?}: {stderr}");
        assert!(
            stderr.contains("snapshot create --run-dir"),
            "{args:?}: {stderr}"
        );
    }
    // Complete command lines that name nothing usable.
    for (args, cause) in [
        (
            vec!["snapshot", "create", "--run-dir", &dir, "--out", &dir],
            "no such database",
        ),
        (
            vec!["snapshot", "pin", "--run-dir", &dir, "--height", "3"],
            "no such database",
        ),
    ] {
        let stderr = refused(BIN, &strings(&args), &[]);
        assert!(stderr.contains(cause), "{args:?}: {stderr}");
    }
    let unpinned = run(&strings(&["snapshot", "unpin", "--run-dir", &dir]));
    assert_eq!(unpinned["removed"], false);
}
