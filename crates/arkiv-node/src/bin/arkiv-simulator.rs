//! Explicit opt-in unsigned durable simulator; see --help.
use arkiv_db::{decode_exact, durable::Options};
use arkiv_db_redb::{RedbConfig, RedbStore};
use arkiv_db_storage::versioned::VersionedStore;
use arkiv_node::simulator::{
    Error, Result,
    host::{Host, RunConfig, parse_uuid},
    light::Light,
    runtime::{self, GenesisResponse, Node, Runtime},
    wire::{Identity, number},
    workload::Workload,
};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};

fn args() -> Result<(String, BTreeMap<String, String>)> {
    let mut a = std::env::args().skip(1);
    let role = a.next().unwrap_or_else(|| "--help".into());
    let mut out = BTreeMap::new();
    while let Some(k) = a.next() {
        let v = if matches!(k.as_str(), "--unsigned-simulator" | "--resume") {
            "true".into()
        } else {
            a.next().ok_or(Error("InvalidRequest"))?
        };
        if out.insert(k, v).is_some() {
            return Err(Error("InvalidRequest"));
        }
    }
    Ok((role, out))
}
#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("arkiv-simulator: {}", e.0);
        std::process::exit(1);
    }
}
async fn run() -> Result<()> {
    let (role, args) = args()?;
    if role == "--help" {
        println!(
            "arkiv-simulator producer|full|light --unsigned-simulator --run-dir DIR [--listen 127.0.0.1:9400] [--admin-listen 127.0.0.1:9401] [--resume | --start paused|running] [--interval-ms 1000]\nProducer: [--source-id UUID] [--chain-id 9001] [--seed 7] [--block-period-ms 1000] [--payload-bytes 64] [--extra-rows-per-block 0] [--cache-bytes 8388608]\nFollowers: --peer URL --source-id UUID --run-id HEX32 --genesis-hash 0xHEX64 [--chain-id 9001]\nPrivate listener requires SIMULATOR_ADMIN_TOKEN (>=32 bytes). Full follower additionally requires --replication-peer URL and SIMULATOR_REPLICATION_TOKEN (same private bearer).\nUnsigned source: proofs verify state against the explicitly trusted source, without consensus authentication. Restart opens retained history from one redb file; producer starts paused unless --resume (or --start running, the deployment form of the same startup policy)."
        );
        return Ok(());
    }
    const FLAGS: &[&str] = &[
        "--unsigned-simulator",
        "--run-dir",
        "--listen",
        "--admin-listen",
        "--resume",
        "--start",
        "--interval-ms",
        "--source-id",
        "--chain-id",
        "--seed",
        "--block-period-ms",
        "--payload-bytes",
        "--extra-rows-per-block",
        "--cache-bytes",
        "--peer",
        "--replication-peer",
        "--run-id",
        "--genesis-hash",
    ];
    if args.keys().any(|k| !FLAGS.contains(&k.as_str()))
        || !args.contains_key("--unsigned-simulator")
        || !matches!(
            role.as_str(),
            "producer" | "full" | "light" | "inspect" | "inspect-light"
        )
    {
        return Err(Error("InvalidRequest"));
    }
    let get = |key: &str, default: &str| args.get(key).cloned().unwrap_or_else(|| default.into());
    let path = PathBuf::from(args.get("--run-dir").ok_or(Error("InvalidRequest"))?);
    std::fs::create_dir_all(&path).map_err(|_| Error("StorageUnavailable"))?;
    let file = path.join(if matches!(role.as_str(), "light" | "inspect-light") {
        "headers.redb"
    } else {
        "node.redb"
    });
    let exists = file.exists();
    if matches!(role.as_str(), "inspect" | "inspect-light") && !exists {
        return Err(Error("UnknownRun"));
    }
    let cache = number(&get("--cache-bytes", "8388608"))?;
    if cache > 256 * 1024 * 1024 {
        return Err(Error("InvalidRequest"));
    }
    let redb = RedbConfig::default();
    let opened = if exists {
        Some(Arc::new(RedbStore::open(&file, redb)?))
    } else {
        None
    };
    let initialized = match &opened {
        Some(store) => store.head()?.is_some(),
        None => false,
    };
    let options = Options {
        cache_bytes: cache as usize,
        ..Default::default()
    };
    if matches!(role.as_str(), "inspect" | "inspect-light") {
        let store = opened.ok_or(Error("UnknownRun"))?;
        let before = store.stats();
        let opened_node = if role == "inspect-light" {
            let expected = Identity {
                source_id: args
                    .get("--source-id")
                    .ok_or(Error("InvalidRequest"))?
                    .clone(),
                run_id: args.get("--run-id").ok_or(Error("InvalidRequest"))?.clone(),
                genesis_hash: args
                    .get("--genesis-hash")
                    .ok_or(Error("InvalidRequest"))?
                    .clone(),
                chain_id: get("--chain-id", "9001"),
            };
            Node::Light(Box::new(Light::open(store.clone(), &expected)?))
        } else {
            Node::Full(Box::new(Host::open(store.clone(), None, options)?))
        };
        let mut report = match &opened_node {
            Node::Light(light) => {
                serde_json::json!({"role":"light","sourceId":light.identity.source_id,"runId":light.identity.run_id,"genesisHash":light.identity.genesis_hash,"height":light.head.height.to_string(),"residentHeaders":1,"continuations":light.continuation_count()})
            }
            Node::Full(host) => {
                let stats = host.engine.stats();
                serde_json::json!({"role":"full","sourceId":host.identity.source_id,"runId":host.identity.run_id,"genesisHash":host.identity.genesis_hash,"height":host.head.height.to_string(),"residentManifests":stats.resident_manifests,"cachePayloadBytes":stats.cache.payload_capacity,"cacheEntries":stats.cache.entries})
            }
        };
        let reads = store.stats().since(&before);
        let memory = std::fs::read_to_string("/proc/self/status").ok().map(|s| {
            s.lines()
                .filter(|l| l.starts_with("VmRSS:") || l.starts_with("VmHWM:"))
                .map(str::to_owned)
                .collect::<Vec<_>>()
        });
        report.as_object_mut().ok_or(Error("StorageUnavailable"))?.extend(
            serde_json::json!({"coldRecordReads":reads.record_reads,"coldHeadReads":reads.head_reads,"coldObjectReads":reads.object_reads(),"fileBytes":std::fs::metadata(&file).map_err(|_|Error("StorageUnavailable"))?.len(),"fullRepair":store.last_open().full_repair,"processMemory":memory}).as_object().ok_or(Error("StorageUnavailable"))?.clone()
        );
        println!("{report}");
        return Ok(());
    }
    let peer = args.get("--peer").cloned();
    let node = if role == "producer" {
        if peer.is_some() {
            return Err(Error("InvalidRequest"));
        }
        let mut host = if initialized {
            let h = Host::open(opened.ok_or(Error("StorageCorrupt"))?, None, options)?;
            if args
                .get("--source-id")
                .is_some_and(|s| s != &h.identity.source_id)
                || args
                    .get("--chain-id")
                    .is_some_and(|s| s != &h.identity.chain_id)
                || args.get("--seed").is_some_and(|s| s != &h.workload.seed)
                || args
                    .get("--block-period-ms")
                    .is_some_and(|s| s != &h.workload.block_period_ms)
            {
                return Err(Error("IdentityMismatch"));
            }
            h
        } else {
            let source = parse_uuid(&get("--source-id", "00000000-0000-4000-8000-000000000001"))?;
            let mut run_id = [0u8; 16];
            getrandom::fill(&mut run_id).map_err(|_| Error("StorageUnavailable"))?;
            let workload = Workload {
                seed: get("--seed", "7"),
                block_period_ms: get("--block-period-ms", "1000"),
                payload_bytes: u32::try_from(number(&get("--payload-bytes", "64"))?)
                    .map_err(|_| Error("InvalidRequest"))?,
                extra_rows_per_block: u32::try_from(number(&get("--extra-rows-per-block", "0"))?)
                    .map_err(|_| Error("InvalidRequest"))?,
                ..Default::default()
            };
            let genesis = arkiv_node::demo::workload::genesis(
                number(&get("--chain-id", "9001"))?,
                1_700_000_000_000,
            );
            let run = RunConfig { genesis, workload };
            run.validate()?;
            let store = create_store(opened, &file)?;
            Host::create(store, run, source, run_id, options)?
        };
        // Startup policy: paused unless explicitly told to resume. `--start`
        // carries a value so a static command line can select it.
        let start = match args.get("--start").map(String::as_str) {
            None => None,
            Some("paused") => Some(false),
            Some("running") => Some(true),
            Some(_) => return Err(Error("InvalidRequest")),
        };
        if args.contains_key("--resume") || start == Some(true) {
            host.paused = false;
            host.health = "running";
        }
        Node::Full(Box::new(host))
    } else {
        if args.contains_key("--start") {
            return Err(Error("InvalidRequest"));
        }
        let expected = Identity {
            source_id: args
                .get("--source-id")
                .ok_or(Error("InvalidRequest"))?
                .clone(),
            run_id: args.get("--run-id").ok_or(Error("InvalidRequest"))?.clone(),
            genesis_hash: args
                .get("--genesis-hash")
                .ok_or(Error("InvalidRequest"))?
                .clone(),
            chain_id: get("--chain-id", "9001"),
        };
        let endpoint = peer.as_deref().ok_or(Error("InvalidRequest"))?;
        runtime::validate_peer_with_policy(endpoint, runtime::private_http_from_env()?)?;
        if initialized {
            let store = opened.ok_or(Error("StorageCorrupt"))?;
            if role == "light" {
                Node::Light(Box::new(Light::open(store, &expected)?))
            } else {
                let mut h = Host::open(store, Some(&expected), options)?;
                h.role = "full";
                h.paused = false;
                h.health = "following";
                Node::Full(Box::new(h))
            }
        } else {
            let id = bootstrap(endpoint, &expected).await?;
            let store = create_store(opened, &file)?;
            if role == "light" {
                Node::Light(Box::new(Light::create(store, id)?))
            } else {
                let run: RunConfig =
                    decode_exact(&id.metadata).map_err(|_| Error("InvalidRequest"))?;
                let mut h = Host::create(store, run, id.source_id, id.run_id, options)?;
                h.role = "full";
                h.paused = false;
                h.health = "following";
                Node::Full(Box::new(h))
            }
        }
    };
    let mut rt = Runtime::new(node, peer, args.get("--replication-peer").cloned())?;
    Arc::get_mut(&mut rt)
        .ok_or(Error("StorageUnavailable"))?
        .storage_path = Some(file.clone());
    if role == "full"
        && (rt.replication_peer.is_none()
            || rt.replication_token.as_deref().is_none_or(|s| s.len() < 32))
    {
        return Err(Error("InvalidRequest"));
    }
    let listen = get("--listen", "127.0.0.1:9400");
    let public = tokio::net::TcpListener::bind(&listen)
        .await
        .map_err(|_| Error("StorageUnavailable"))?;
    if let Some(admin) = args.get("--admin-listen") {
        if rt.admin_token.as_deref().is_none_or(|s| s.len() < 32) {
            return Err(Error("InvalidRequest"));
        }
        let listener = tokio::net::TcpListener::bind(admin)
            .await
            .map_err(|_| Error("StorageUnavailable"))?;
        let router = runtime::control_router(rt.clone());
        tokio::spawn(async move {
            if axum::serve(listener, router).await.is_err() {
                eprintln!("simulator private listener unavailable");
            }
        });
    }
    let interval = number(&get(
        "--interval-ms",
        if role == "producer" { "1000" } else { "100" },
    ))?;
    if !(10..=60000).contains(&interval) {
        return Err(Error("InvalidRequest"));
    }
    {
        let n = rt.node.lock().map_err(|_| Error("StorageUnavailable"))?;
        println!(
            "{}",
            serde_json::to_string(&runtime::status(&n)).map_err(|_| Error("StorageUnavailable"))?
        );
    }
    tokio::spawn(runtime::run_loop(
        rt.clone(),
        Duration::from_millis(interval),
    ));
    axum::serve(public, runtime::public_router(rt))
        .await
        .map_err(|_| Error("StorageUnavailable"))
}

fn create_store(opened: Option<Arc<RedbStore>>, file: &std::path::Path) -> Result<Arc<RedbStore>> {
    match opened {
        Some(store) => Ok(store),
        None => Ok(Arc::new(RedbStore::create(file, RedbConfig::default())?)),
    }
}
async fn bootstrap(
    endpoint: &str,
    expected: &Identity,
) -> Result<arkiv_db::durable::simulator::SimulatorIdentity> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|_| Error("StorageUnavailable"))?;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let result: Result<GenesisResponse> = runtime::fetch_with_policy(
                &client,
                endpoint,
                "/sim/v1/genesis",
                None,
                256 * 1024,
                runtime::private_http_from_env()?,
            )
            .await;
            match result {
                Ok(response) => return response.validate(expected),
                Err(Error("ServerBusy" | "StorageUnavailable")) => {
                    tokio::time::sleep(Duration::from_millis(100)).await
                }
                Err(e) => return Err(e),
            }
        }
    })
    .await
    .map_err(|_| Error("StorageUnavailable"))?
}
