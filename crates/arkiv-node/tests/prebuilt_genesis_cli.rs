//! Build-once streaming genesis through the proposer binary: `stream-build
//! --state-out` keeps the built state and `producer --prebuilt-state` serves it
//! without rebuilding, while the legacy producer rebuild remains available.
use alloy_primitives::U256;
use arkiv_db::{Cell, CellName, Value, encode};
use arkiv_node::{
    accounts::AccountConfig,
    genesis_bundle::PrefillEntity,
    signed::{
        crypto::Signer,
        protocol::{SignedGenesis, SignedLimits, default_native_genesis},
    },
};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const BIN: &str = env!("CARGO_BIN_EXE_arkiv-proposer");
const MARKER: &[u8] = b"prebuilt-tamper-marker-5b0c1e7f";

struct Temp(PathBuf);
impl Temp {
    fn new(tag: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("arkiv-prebuilt-cli-{tag}-{}", std::process::id()));
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
fn address() -> String {
    format!("0x{}", hex(signer().address().as_bytes()))
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A funded base genesis and a small canonical stream, as the runner supplies them.
fn inputs(dir: &Path, run: u8) -> (PathBuf, PathBuf, PathBuf) {
    let proposer = signer().address();
    let base = SignedGenesis {
        genesis: default_native_genesis(9451, 1000),
        run_id: [run; 16],
        proposer: *proposer.as_bytes(),
        block_period_ms: 1000,
        limits: SignedLimits::default(),
        accounts: AccountConfig::new(vec![(proposer, U256::from(10).pow(U256::from(30)))]).unwrap(),
        prefill: None,
    };
    let mut stream = b"ARKPFL02".to_vec();
    stream.extend_from_slice(&arkiv_node::genesis_bundle::ROW_CODEC.to_be_bytes());
    stream.extend_from_slice(&24u64.to_be_bytes());
    for n in 0..24u64 {
        let mut body = format!("entity {n} ").into_bytes();
        if n == 5 {
            body.extend_from_slice(MARKER);
        }
        let frame = encode(&PrefillEntity {
            creator: [2; 20],
            create_nonce: n,
            salt: n as u128,
            expires_at_height: 1000 + n,
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

fn run(args: &[&Path]) -> std::process::Output {
    Command::new(BIN).args(args).output().unwrap()
}
fn stream_build(
    dir: &Path,
    base: &Path,
    input: &Path,
    bundle: &Path,
    state: Option<&Path>,
) -> serde_json::Value {
    let sponsor = PathBuf::from(address());
    let mut args = vec![
        Path::new("genesis"),
        Path::new("stream-build"),
        base,
        Path::new("--prefill"),
        input,
        Path::new("--out"),
        bundle,
        Path::new("--sponsor"),
        &sponsor,
    ];
    if let Some(state) = state {
        args.extend([Path::new("--state-out"), state]);
    }
    let output = run(&args);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !dir.join(format!(
            ".{}.stream-work",
            bundle.file_name().unwrap().to_str().unwrap()
        ))
        .exists()
    );
    serde_json::from_slice(&std::fs::read(bundle.join("report.json")).unwrap()).unwrap()
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}
fn status(port: u16) -> Option<serde_json::Value> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    write!(
        stream,
        "GET /signed/v1/status HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).ok()?;
    let split = response.windows(4).position(|w| w == b"\r\n\r\n")?;
    serde_json::from_slice(&response[split + 4..]).ok()
}

struct Producer {
    child: std::process::Child,
    log: PathBuf,
    port: u16,
}
impl Producer {
    fn start(
        dir: &Path,
        node: &Path,
        bundle: &Path,
        key: &Path,
        prebuilt: Option<&Path>,
        tag: &str,
    ) -> Self {
        let port = free_port();
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
            .arg(format!("127.0.0.1:{port}"));
        if let Some(prebuilt) = prebuilt {
            command.arg("--prebuilt-state").arg(prebuilt);
        }
        let file = std::fs::File::create(&log).unwrap();
        let child = command
            .stdout(Stdio::null())
            .stderr(Stdio::from(file))
            .spawn()
            .unwrap();
        Self { child, log, port }
    }
    /// The served height-zero status, or the exit status if the producer failed.
    fn ready(&mut self) -> Result<serde_json::Value, std::process::ExitStatus> {
        let deadline = Instant::now() + Duration::from_secs(120);
        while Instant::now() < deadline {
            if let Some(code) = self.child.try_wait().unwrap() {
                return Err(code);
            }
            if let Some(status) = status(self.port) {
                return Ok(status);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("producer readiness timed out: {}", self.log());
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
fn producer_serves_the_prebuilt_state_without_rebuilding_and_legacy_rebuild_agrees() {
    let tmp = Temp::new("once");
    let (base, input, key) = inputs(&tmp.0, 7);
    let (bundle, state) = (tmp.0.join("bundle"), tmp.0.join("genesis-state"));
    let report = stream_build(&tmp.0, &base, &input, &bundle, Some(&state));
    assert!(state.join("complete/state.redb").is_file());

    let node = tmp.0.join("node");
    let mut producer = Producer::start(&tmp.0, &node, &bundle, &key, Some(&state), "once");
    let ready = producer
        .ready()
        .unwrap_or_else(|code| panic!("{code}: {}", producer.log()));
    assert_genesis_head(&ready, &report);
    let log = producer.log();
    assert!(
        log.contains(r#""event":"prefill_prebuilt_install","result":"moved""#),
        "{log}"
    );
    for rebuilt in ["bulk_normalize", "bulk_indexes", "prefill_bulk_phase"] {
        assert!(!log.contains(rebuilt), "producer rebuilt state: {log}");
    }
    assert!(!state.exists(), "the producer consumed the prebuilt state");
    assert!(
        !node.join("prefill-work").exists(),
        "private import scratch was removed"
    );
    drop(producer);

    // A restart with the same command line opens the initialized node.
    let mut restarted = Producer::start(&tmp.0, &node, &bundle, &key, Some(&state), "restart");
    assert_genesis_head(&restarted.ready().unwrap(), &report);
    assert!(restarted.log().contains(r#""result":"initialized""#));
    drop(restarted);

    // The legacy producer rebuild from the same bundle reaches the same root.
    let legacy_node = tmp.0.join("legacy-node");
    let mut legacy = Producer::start(&tmp.0, &legacy_node, &bundle, &key, None, "legacy");
    assert_genesis_head(&legacy.ready().unwrap(), &report);
    assert!(legacy.log().contains("bulk_normalize"));
    assert!(!legacy.log().contains("prefill_prebuilt_install"));
    drop(legacy);

    // And a bundle authored without --state-out is byte-identical.
    let plain = tmp.0.join("plain-bundle");
    assert_eq!(stream_build(&tmp.0, &base, &input, &plain, None), report);
    for name in ["genesis.bin", "prefill.bin", "report.json"] {
        assert_eq!(
            std::fs::read(plain.join(name)).unwrap(),
            std::fs::read(bundle.join(name)).unwrap()
        );
    }
}

#[test]
fn tampered_or_foreign_prebuilt_state_never_serves() {
    let tmp = Temp::new("reject");
    let (base, input, key) = inputs(&tmp.0, 7);
    let (bundle, state) = (tmp.0.join("bundle"), tmp.0.join("state"));
    let report = stream_build(&tmp.0, &base, &input, &bundle, Some(&state));
    let other_dir = tmp.0.join("other");
    std::fs::create_dir(&other_dir).unwrap();
    let (other_base, _, _) = inputs(&other_dir, 8);
    let (other_bundle, other_state) = (other_dir.join("bundle"), other_dir.join("state"));
    stream_build(
        &other_dir,
        &other_base,
        &input,
        &other_bundle,
        Some(&other_state),
    );

    // Another bundle's state is refused before it is moved.
    let node = tmp.0.join("foreign-node");
    let mut foreign = Producer::start(&tmp.0, &node, &bundle, &key, Some(&other_state), "foreign");
    assert!(!foreign.ready().unwrap_err().success());
    assert!(
        foreign.log().contains("identity does not match"),
        "{}",
        foreign.log()
    );
    assert!(other_state.join("complete/state.redb").is_file());
    assert!(!node.join("prefill-work").exists());
    drop(foreign);

    // Changed object bytes inside the store fail hash verification during installation.
    let database = state.join("complete/state.redb");
    let mut bytes = std::fs::read(&database).unwrap();
    let mut found = 0;
    for at in 0..bytes.len() - MARKER.len() {
        if &bytes[at..at + MARKER.len()] == MARKER {
            bytes[at + MARKER.len() - 1] ^= 1;
            found += 1;
        }
    }
    assert!(found > 0, "fixture marker is stored in the database");
    std::fs::write(&database, bytes).unwrap();
    let node = tmp.0.join("tampered-node");
    for attempt in ["tampered", "tampered-again"] {
        let mut tampered = Producer::start(&tmp.0, &node, &bundle, &key, Some(&state), attempt);
        let code = tampered.ready().unwrap_err();
        assert!(!code.success(), "{}", tampered.log());
        assert!(
            tampered.log().contains("StorageCorrupt"),
            "{}",
            tampered.log()
        );
    }

    // Only a bundle producer accepts the flag. These command lines are otherwise
    // complete: without the guard a producer would start and a follower would
    // try its peer (PeerUnavailable) instead of failing with InvalidRequest.
    let allocations = tmp.0.join("allocations.json");
    std::fs::write(
        &allocations,
        format!(r#"{{"{}":"1000000000000000000"}}"#, address()),
    )
    .unwrap();
    let port = free_port().to_string();
    let listen = format!("127.0.0.1:{port}");
    let hash = report["genesisHash"].as_str().unwrap().to_string();
    let refused: Vec<Vec<String>> = vec![
        vec![
            "producer".into(),
            "--run-dir".into(),
            tmp.0.join("alloc-node").display().to_string(),
            "--key-file".into(),
            key.display().to_string(),
            "--allocations-file".into(),
            allocations.display().to_string(),
            "--listen".into(),
            listen.clone(),
        ],
        vec![
            "full".into(),
            "--run-dir".into(),
            tmp.0.join("full-node").display().to_string(),
            "--peer".into(),
            "http://127.0.0.1:9".into(),
            "--replication-peer".into(),
            "http://127.0.0.1:9".into(),
            "--genesis-hash".into(),
            hash.clone(),
            "--proposer".into(),
            address(),
            "--chain-id".into(),
            "9451".into(),
            "--source-id".into(),
            "00000000-0000-4000-8000-000000000001".into(),
            "--run-id".into(),
            "07".repeat(16),
            "--bundle".into(),
            bundle.display().to_string(),
            "--listen".into(),
            listen.clone(),
        ],
        vec![
            "light".into(),
            "--run-dir".into(),
            tmp.0.join("light-node").display().to_string(),
            "--peer".into(),
            "http://127.0.0.1:9".into(),
            "--genesis-hash".into(),
            hash,
            "--proposer".into(),
            address(),
            "--chain-id".into(),
            "9451".into(),
            "--source-id".into(),
            "00000000-0000-4000-8000-000000000001".into(),
            "--run-id".into(),
            "07".repeat(16),
            "--listen".into(),
            listen,
        ],
        vec![
            "keygen".into(),
            "--out".into(),
            tmp.0.join("new.key").display().to_string(),
        ],
    ];
    for mut args in refused {
        args.extend(["--prebuilt-state".into(), state.display().to_string()]);
        let mut child = Command::new(BIN)
            .args(&args)
            .env("ARKIV_PROPOSER_TOKEN", "t".repeat(32))
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        let code = loop {
            if let Some(code) = child.try_wait().unwrap() {
                break code;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("{args:?} was accepted");
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let mut stderr = String::new();
        child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut stderr)
            .unwrap();
        assert!(!code.success(), "{args:?}");
        assert!(stderr.contains("InvalidRequest"), "{args:?}: {stderr}");
    }
    assert!(!tmp.0.join("new.key").exists());
    assert_eq!(report["rowCount"], "24");
}
