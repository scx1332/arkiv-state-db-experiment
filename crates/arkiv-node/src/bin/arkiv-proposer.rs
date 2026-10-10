//! Explicit signed single-proposer profile. Keys are local and never returned by HTTP.
use arkiv_db::{
    decode_exact,
    durable::{DurableEngine, Options},
    encode,
};
use arkiv_db_redb::{RedbConfig, RedbStore};
use arkiv_db_storage::versioned::VersionedStore;
use arkiv_node::{
    signed::{
        Error, Result,
        crypto::Signer,
        history::History,
        host::Host,
        light::Light,
        protocol::{SignedGenesis, SignedHeader, SignedTx, TX_CAP},
        runtime::{self, GenesisResponse, Node, Runtime},
    },
    simulator::{
        host::parse_uuid,
        wire::{Identity, hash, hex, number, unhex},
    },
};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
fn arguments() -> Result<(String, BTreeMap<String, String>)> {
    let mut args = std::env::args().skip(1);
    let command = args.next().unwrap_or_else(|| "--help".into());
    let mut flags = BTreeMap::new();
    while let Some(key) = args.next() {
        if !key.starts_with("--") {
            return Err(Error("InvalidRequest"));
        }
        let value = args.next().ok_or(Error("InvalidRequest"))?;
        if flags.insert(key, value).is_some() {
            return Err(Error("InvalidRequest"));
        }
    }
    Ok((command, flags))
}
fn read_file(path: &str, cap: usize) -> Result<Vec<u8>> {
    use std::io::Read;
    let file = std::fs::File::open(path).map_err(|_| Error("InvalidRequest"))?;
    let mut bytes = Vec::new();
    file.take(cap as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Error("InvalidRequest"))?;
    if bytes.len() > cap {
        return Err(Error("InvalidRequest"));
    }
    Ok(bytes)
}
fn signer(path: &str) -> Result<Signer> {
    let bytes = read_file(path, 128)?;
    let text = std::str::from_utf8(&bytes).map_err(|_| Error("InvalidKey"))?;
    let raw = unhex(text.trim(), 32)?;
    Signer::from_bytes(raw.try_into().map_err(|_| Error("InvalidKey"))?)
}
fn store(path: &Path) -> Result<Arc<RedbStore>> {
    Ok(Arc::new(if path.exists() {
        RedbStore::open(path, RedbConfig::default())?
    } else {
        RedbStore::create(path, RedbConfig::default())?
    }))
}
fn write_new(path: &Path, bytes: &[u8], private: bool) -> Result<()> {
    use std::io::Write;
    let mut unique = [0u8; 16];
    getrandom::fill(&mut unique).map_err(|_| Error("StorageUnavailable"))?;
    let temporary = path.with_file_name(format!(
        ".{}.{}.pending",
        path.file_name()
            .and_then(|n| n.to_str())
            .ok_or(Error("InvalidRequest"))?,
        hex(&unique)
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(if private { 0o600 } else { 0o644 });
    }
    let mut file = options
        .open(&temporary)
        .map_err(|_| Error("StorageUnavailable"))?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|_| Error("StorageUnavailable"))?;
    // Atomic no-clobber installation: a process cut leaves only an unreferenced
    // temporary file, never partially written trust material under its final name.
    std::fs::hard_link(&temporary, path).map_err(|_| Error("StorageUnavailable"))?;
    std::fs::remove_file(&temporary).map_err(|_| Error("StorageUnavailable"))?;
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::File::open(parent)
            .and_then(|f| f.sync_all())
            .map_err(|_| Error("StorageUnavailable"))?;
    }
    Ok(())
}
fn retain_public(path: &Path, bytes: &[u8]) -> Result<()> {
    if path.exists() {
        if read_file(
            path.to_str().ok_or(Error("InvalidRequest"))?,
            arkiv_node::genesis_bundle::MAX_CANONICAL_BYTES as usize,
        )? != bytes
        {
            return Err(Error("IdentityMismatch"));
        }
        Ok(())
    } else {
        write_new(path, bytes, false)
    }
}
fn amount(value: &str) -> Result<alloy_primitives::U256> {
    if value.is_empty()
        || !value.bytes().all(|b| b.is_ascii_digit())
        || (value.len() > 1 && value.starts_with('0'))
    {
        return Err(Error("InvalidAmount"));
    }
    value.parse().map_err(|_| Error("InvalidAmount"))
}
fn allocations(path: &str) -> Result<arkiv_node::accounts::AccountConfig> {
    // Read pairs directly to reject duplicate JSON keys instead of silently
    // overwriting an allocation during ordinary map deserialization.
    struct Allocations(Vec<(String, String)>);
    impl<'de> serde::Deserialize<'de> for Allocations {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
            struct Visitor;
            impl<'de> serde::de::Visitor<'de> for Visitor {
                type Value = Allocations;
                fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    f.write_str("map of addresses to decimal base-unit strings")
                }
                fn visit_map<M: serde::de::MapAccess<'de>>(
                    self,
                    mut m: M,
                ) -> std::result::Result<Allocations, M::Error> {
                    let mut values = Vec::new();
                    while let Some(pair) = m.next_entry::<String, String>()? {
                        if values.len() >= arkiv_node::accounts::MAX_ALLOCATIONS {
                            return Err(serde::de::Error::custom("too many allocations"));
                        }
                        values.push(pair);
                    }
                    Ok(Allocations(values))
                }
            }
            d.deserialize_map(Visitor)
        }
    }
    let values: Allocations = serde_json::from_slice(&read_file(path, 65536)?)
        .map_err(|_| Error("InvalidAllocations"))?;
    let values = values
        .0
        .into_iter()
        .map(|(actor, balance)| {
            Ok((
                arkiv_db::OwnerId::new(
                    unhex(&actor, 20)?
                        .try_into()
                        .map_err(|_| Error("InvalidAllocations"))?,
                ),
                amount(&balance)?,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    arkiv_node::accounts::AccountConfig::new(values).map_err(|_| Error("InvalidAllocations"))
}
fn authoring_error(error: String) -> Error {
    eprintln!("{error}");
    Error("InvalidGenesisBundle")
}
fn load_bundle(path: &Path) -> Result<(SignedGenesis, Vec<u8>)> {
    arkiv_node::genesis_authoring::verify(path).map_err(authoring_error)?;
    let genesis = decode_exact(&read_file(
        path.join("genesis.bin")
            .to_str()
            .ok_or(Error("InvalidRequest"))?,
        65536,
    )?)
    .map_err(|_| Error("InvalidGenesisBundle"))?;
    let stream = read_file(
        path.join("prefill.bin")
            .to_str()
            .ok_or(Error("InvalidRequest"))?,
        arkiv_node::genesis_bundle::MAX_CANONICAL_BYTES as usize,
    )?;
    arkiv_node::genesis_bundle::verify_genesis(&genesis, &stream).map_err(authoring_error)?;
    Ok((genesis, stream))
}
fn local_prefill(
    path: &Path,
    genesis: &SignedGenesis,
    supplied: Option<&[u8]>,
) -> Result<Option<Vec<u8>>> {
    if genesis.prefill.is_none() {
        return Ok(None);
    }
    let target = path.join("prefill.bin");
    let stream = match supplied {
        Some(bytes) => bytes.to_vec(),
        None => read_file(
            target.to_str().ok_or(Error("InvalidRequest"))?,
            arkiv_node::genesis_bundle::MAX_CANONICAL_BYTES as usize,
        )?,
    };
    arkiv_node::genesis_bundle::verify_genesis(genesis, &stream).map_err(authoring_error)?;
    retain_public(&target, &stream)?;
    Ok(Some(stream))
}
fn genesis_command() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(2).collect();
    let [command, source, rest @ ..] = args.as_slice() else {
        return Err(Error("InvalidRequest"));
    };
    let mut out = None;
    let mut locked = false;
    let mut refresh = false;
    let mut flags = rest.iter();
    while let Some(flag) = flags.next() {
        match flag.as_str() {
            "--out" if out.is_none() => out = Some(flags.next().ok_or(Error("InvalidRequest"))?),
            "--locked" if !locked => locked = true,
            "--refresh" if !refresh => refresh = true,
            _ => return Err(Error("InvalidRequest")),
        }
    }
    let result = match command.as_str() {
        "lock" if out.is_none() && !locked => arkiv_node::genesis_authoring::lock(Path::new(source), refresh),
        "build" if out.is_some() && !refresh => arkiv_node::genesis_authoring::build(Path::new(source), Path::new(out.unwrap()), locked),
        "verify" if out.is_none() && !locked && !refresh => arkiv_node::genesis_authoring::verify(Path::new(source)),
        _ => { eprintln!("genesis lock CHAIN [--refresh] | genesis build CHAIN --out BUNDLE --locked | genesis verify BUNDLE"); return Err(Error("InvalidRequest")); }
    }.map_err(authoring_error)?;
    println!("{result}");
    Ok(())
}
#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("arkiv-proposer: {}", error.0);
        std::process::exit(1);
    }
}
async fn run() -> Result<()> {
    if std::env::args().nth(1).as_deref() == Some("genesis") {
        return genesis_command();
    }
    let (command, args) = arguments()?;
    if command == "--help" {
        println!(
            "arkiv-proposer genesis lock CHAIN [--refresh]\narkiv-proposer genesis build CHAIN --out BUNDLE --locked\narkiv-proposer genesis verify BUNDLE\narkiv-proposer keygen --out FILE\narkiv-proposer producer --run-dir DIR --key-file FILE [--listen 127.0.0.1:9500] [--private-listen 127.0.0.1:9501] [--start paused|running] [--source-id UUID] [--chain-id 9001] [--block-period-ms 1000] [--state-retention archive|N] (--allocations-file FILE | --bundle BUNDLE)\narkiv-proposer full|light --run-dir DIR --peer URL --genesis-hash 0xHEX --proposer 0xADDRESS --chain-id N --source-id UUID --run-id HEX32 [--replication-peer URL] [--bundle BUNDLE] [--state-retention archive|N (full only)]\narkiv-proposer sign --key-file FILE --genesis-file FILE --tx-file FILE --nonce N --out FILE\narkiv-proposer create --key-file FILE --genesis-file FILE --nonce N --request-id 0xHEX64 --payload-file FILE --out FILE [--salt 0] [--gas-limit 1000000] [--lifetime-blocks 17280]\narkiv-proposer transfer --key-file FILE --genesis-file FILE --nonce N --request-id 0xHEX64 --to 0xADDRESS --amount-wei DECIMAL --out FILE [--gas-limit 21000]\narkiv-proposer submit --peer URL --transaction-file FILE\nOptional private Prometheus exporter: --metrics-listen 127.0.0.1:9600 [--metrics-interval-secs 300] (minimum 10). Metrics use rogue_one_ prefix.\nPrivate listener/full replication use ARKIV_PROPOSER_TOKEN (at least 32 bytes). Producer starts paused. Every chain runs the glm-v2 gas profile with funded accounts. A bundle chain creates the entities namespace (and its prefill) at height 0; a chain created from --allocations-file creates it with the proposer's bootstrap transaction in block 1. Each run exports public genesis.bin/certificate.bin. Sign/create files use native canonical bytes; submit polls pending/included status. Single-proposer authentication supplies no consensus or finality. Bodies are retained in history.redb separately from node.redb. --state-retention N serves state (balances, proofs, queries, receipts/logs) only for the head and the N snapshots before it and refuses older heights with HistoryPruned; archive (default) serves every height. Nothing is deleted yet, and a pruned floor never moves back."
        );
        return Ok(());
    }
    const FLAGS: &[&str] = &[
        "--metrics-listen",
        "--metrics-interval-secs",
        "--bundle",
        "--out",
        "--key-file",
        "--genesis-file",
        "--tx-file",
        "--nonce",
        "--request-id",
        "--payload-file",
        "--salt",
        "--allocations-file",
        "--to",
        "--amount-wei",
        "--gas-limit",
        "--lifetime-blocks",
        "--transaction-file",
        "--run-dir",
        "--listen",
        "--private-listen",
        "--start",
        "--source-id",
        "--chain-id",
        "--block-period-ms",
        "--peer",
        "--replication-peer",
        "--genesis-hash",
        "--proposer",
        "--run-id",
        "--state-retention",
    ];
    if args.keys().any(|k| !FLAGS.contains(&k.as_str())) {
        return Err(Error("InvalidRequest"));
    }
    let required = |key: &str| {
        args.get(key)
            .map(String::as_str)
            .ok_or(Error("InvalidRequest"))
    };
    let get = |key: &str, default: &str| args.get(key).cloned().unwrap_or_else(|| default.into());
    if command == "keygen" {
        let mut key = [0u8; 32];
        getrandom::fill(&mut key).map_err(|_| Error("StorageUnavailable"))?;
        let s = Signer::from_bytes(key)?;
        write_new(Path::new(required("--out")?), hex(&key).as_bytes(), true)?;
        println!("{}", hex(s.address().as_bytes()));
        return Ok(());
    }
    if command == "sign" || command == "create" || command == "transfer" {
        let s = signer(required("--key-file")?)?;
        let genesis: SignedGenesis = decode_exact(&read_file(required("--genesis-file")?, 65536)?)
            .map_err(|_| Error("InvalidRequest"))?;
        genesis.validate()?;
        let nonce = number(required("--nonce")?)?;
        let tx = if command == "sign" {
            decode_exact(&read_file(required("--tx-file")?, TX_CAP as usize)?)
                .map_err(|_| Error("InvalidRequest"))?
        } else if command == "transfer" {
            arkiv_node::TxEnvelope {
                chain_id: genesis.genesis.chain_id,
                actor: s.address(),
                request_id: unhex(required("--request-id")?, 32)?
                    .try_into()
                    .map_err(|_| Error("InvalidRequest"))?,
                tx_budget: number(&get("--gas-limit", "21000"))?,
                ops: vec![arkiv_node::TxOp::Transfer {
                    to: arkiv_db::OwnerId::new(
                        unhex(required("--to")?, 20)?
                            .try_into()
                            .map_err(|_| Error("InvalidRequest"))?,
                    ),
                    amount: amount(required("--amount-wei")?)?,
                }],
            }
        } else {
            let payload = read_file(required("--payload-file")?, 128 * 1024)?;
            let request_id = unhex(required("--request-id")?, 32)?
                .try_into()
                .map_err(|_| Error("InvalidRequest"))?;
            arkiv_node::TxEnvelope {
                chain_id: genesis.genesis.chain_id,
                actor: s.address(),
                request_id,
                tx_budget: args
                    .get("--gas-limit")
                    .map(|value| number(value))
                    .transpose()?
                    .unwrap_or_else(|| {
                        if arkiv_node::gas::is_active(&genesis.genesis.pricing) {
                            1_000_000
                        } else {
                            genesis.genesis.params.user_budget
                        }
                    }),
                ops: vec![arkiv_node::TxOp::Arkiv {
                    namespace: arkiv_node::arkiv::entity_namespace(),
                    operation: arkiv_node::arkiv::ArkivOperation::Create {
                        salt: number(&get("--salt", "0"))? as u128,
                        flags: 0,
                        expiry: if args.contains_key("--lifetime-blocks")
                            || arkiv_node::gas::is_active(&genesis.genesis.pricing)
                        {
                            arkiv_node::Expiry {
                                expires_at: 0,
                                min_lifetime: number(&get("--lifetime-blocks", "17280"))?,
                            }
                        } else {
                            arkiv_node::Expiry::PERMANENT
                        },
                        attributes: vec![
                            (
                                arkiv_db::CellName::new("group").unwrap(),
                                arkiv_db::Value::U64(1),
                            ),
                            (
                                arkiv_db::CellName::new("$payload").unwrap(),
                                arkiv_db::Value::bytes(&payload),
                            ),
                        ],
                    },
                }],
            }
        };
        let signed = SignedTx::sign(&genesis, nonce, tx, &s)?;
        write_new(Path::new(required("--out")?), &encode(&signed), false)?;
        println!("{}", hex(signed.hash().as_slice()));
        return Ok(());
    }
    if command == "submit" {
        let bytes = read_file(required("--transaction-file")?, TX_CAP as usize)?;
        let tx: SignedTx = decode_exact(&bytes).map_err(|_| Error("InvalidRequest"))?;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(|_| Error("StorageUnavailable"))?;
        let peer = required("--peer")?;
        let response: serde_json::Value = runtime::fetch(
            &client,
            peer,
            "/signed/v1/transactions",
            Some(serde_json::json!({"transactionBytes":hex(&bytes)})),
            None,
            65536,
        )
        .await?;
        println!("{response}");
        if response["status"] == "pending" {
            for _ in 0..10 {
                tokio::time::sleep(Duration::from_millis(500)).await;
                let status: serde_json::Value = runtime::fetch(
                    &client,
                    peer,
                    &format!(
                        "/signed/v1/requests/{}/{}",
                        hex(tx.tx.actor.as_bytes()),
                        hex(&tx.tx.request_id)
                    ),
                    None,
                    None,
                    65536,
                )
                .await?;
                if status["status"] != "pending" {
                    println!("{status}");
                    break;
                }
            }
        }
        return Ok(());
    }
    if !matches!(command.as_str(), "producer" | "full" | "light") {
        return Err(Error("InvalidRequest"));
    }
    let path = PathBuf::from(required("--run-dir")?);
    std::fs::create_dir_all(&path).map_err(|_| Error("StorageUnavailable"))?;
    let token = std::env::var("ARKIV_PROPOSER_TOKEN").ok();
    if args.contains_key("--private-listen") && token.as_ref().is_none_or(|s| s.len() < 32) {
        return Err(Error("InvalidRequest"));
    }
    let peer = args.get("--peer").cloned();
    let replication = args.get("--replication-peer").cloned();
    let bundle = args
        .get("--bundle")
        .map(|dir| load_bundle(Path::new(dir)))
        .transpose()?;
    let metrics_interval = number(&get("--metrics-interval-secs", "300"))?;
    if metrics_interval < 10 {
        return Err(Error("InvalidRequest"));
    }
    if args.contains_key("--metrics-interval-secs") && !args.contains_key("--metrics-listen") {
        return Err(Error("InvalidRequest"));
    }
    if command == "light" && args.contains_key("--state-retention") {
        // A light node keeps headers only; it has no state history to retain.
        return Err(Error("InvalidRequest"));
    }
    let options = Options {
        retention: get("--state-retention", "archive")
            .parse()
            .map_err(|_| Error("InvalidRequest"))?,
        ..Options::default()
    };
    let databases = std::cell::RefCell::new(Vec::new());
    let store = |path: &Path| -> Result<Arc<RedbStore>> {
        let opened = store(path)?;
        let name = match path.file_name().and_then(|s| s.to_str()) {
            Some("node.redb") => "state",
            Some("history.redb") => "history",
            Some("headers.redb") => "headers",
            _ => return Err(Error("InvalidRequest")),
        };
        databases
            .borrow_mut()
            .push(arkiv_node::signed::metrics::Database {
                name,
                path: path.to_owned(),
                store: opened.clone(),
            });
        Ok(opened)
    };
    let node = if command == "producer" {
        if peer.is_some() || replication.is_some() {
            return Err(Error("InvalidRequest"));
        }
        let s = signer(required("--key-file")?)?;
        let state = store(&path.join("node.redb"))?;
        let history_store = store(&path.join("history.redb"))?;
        let mut host = if state.head()?.is_some() {
            let id = DurableEngine::read_simulator_identity(state.as_ref())?;
            let genesis: SignedGenesis =
                decode_exact(&id.metadata).map_err(|_| Error("IdentityMismatch"))?;
            if args
                .get("--chain-id")
                .is_some_and(|v| v != &genesis.genesis.chain_id.to_string())
                || args
                    .get("--source-id")
                    .is_some_and(|v| v != &arkiv_node::simulator::host::uuid(&id.source_id))
                || args
                    .get("--block-period-ms")
                    .is_some_and(|v| v != &genesis.block_period_ms.to_string())
            {
                return Err(Error("IdentityMismatch"));
            }
            if let Some(file) = args.get("--allocations-file")
                && genesis.accounts != allocations(file)?
            {
                return Err(Error("IdentityMismatch"));
            }
            if bundle.as_ref().is_some_and(|(g, _)| g != &genesis) {
                return Err(Error("IdentityMismatch"));
            }
            let history = History::open(history_store, genesis.hash())?;
            Host::open(state, history, &genesis, Some(s), options.clone())?
        } else {
            let proposed = if let Some((genesis, _)) = &bundle {
                genesis.clone()
            } else {
                let native = arkiv_node::signed::protocol::default_native_genesis(
                    number(&get("--chain-id", "9001"))?,
                    1_700_000_000_000,
                );
                let mut run_id = [0u8; 16];
                getrandom::fill(&mut run_id).map_err(|_| Error("StorageUnavailable"))?;
                SignedGenesis::new(
                    native,
                    run_id,
                    *s.address().as_bytes(),
                    number(&get("--block-period-ms", "1000"))?,
                    allocations(required("--allocations-file")?)?,
                )?
            };
            let genesis_file = path.join("genesis.bin");
            let genesis: SignedGenesis = if genesis_file.exists() {
                decode_exact(&read_file(
                    genesis_file.to_str().ok_or(Error("InvalidRequest"))?,
                    65536,
                )?)
                .map_err(|_| Error("IdentityMismatch"))?
            } else {
                proposed
            };
            genesis.validate()?;
            if bundle.as_ref().is_some_and(|(g, _)| g != &genesis) {
                return Err(Error("IdentityMismatch"));
            }
            if let Some(file) = args.get("--allocations-file")
                && genesis.accounts != allocations(file)?
            {
                return Err(Error("IdentityMismatch"));
            }
            if genesis.proposer != *s.address().as_bytes()
                || args
                    .get("--chain-id")
                    .is_some_and(|v| v != &genesis.genesis.chain_id.to_string())
                || args
                    .get("--block-period-ms")
                    .is_some_and(|v| v != &genesis.block_period_ms.to_string())
            {
                return Err(Error("IdentityMismatch"));
            }
            let prefill =
                local_prefill(&path, &genesis, bundle.as_ref().map(|(_, b)| b.as_slice()))?;
            let certificate = SignedHeader::sign(&genesis, genesis.header(), &s)?;
            // Retain the chain identity before either database initialization;
            // a cut during genesis publication can retry the same chain.
            retain_public(&genesis_file, &encode(&genesis))?;
            retain_public(&path.join("certificate.bin"), &encode(&certificate))?;
            let history = if history_store.head()?.is_some() {
                History::open(history_store, genesis.hash())?
            } else {
                History::create(history_store, genesis.hash())?
            };
            Host::create_with_prefill(
                state,
                history,
                genesis,
                parse_uuid(&get("--source-id", "00000000-0000-4000-8000-000000000001"))?,
                certificate,
                Some(s),
                options.clone(),
                prefill,
            )?
        };
        if host.genesis.prefill.is_some() {
            host.prefill_stream = local_prefill(
                &path,
                &host.genesis,
                bundle.as_ref().map(|(_, b)| b.as_slice()),
            )?;
        }
        match get("--start", "paused").as_str() {
            "paused" => {}
            "running" => {
                host.paused = false;
                host.health = "running";
            }
            _ => return Err(Error("InvalidRequest")),
        };
        for (name, bytes) in [
            ("genesis.bin", encode(&host.genesis)),
            ("certificate.bin", encode(&host.certificate(0)?)),
        ] {
            let file = path.join(name);
            if !file.exists() {
                write_new(&file, &bytes, false)?;
            } else if read_file(file.to_str().ok_or(Error("InvalidRequest"))?, 65536)? != bytes {
                return Err(Error("IdentityMismatch"));
            }
        }
        Node::Full(Box::new(host))
    } else {
        let peer = peer.as_deref().ok_or(Error("InvalidRequest"))?;
        let proposer: [u8; 20] = unhex(required("--proposer")?, 20)?
            .try_into()
            .map_err(|_| Error("InvalidRequest"))?;
        let expected = hash(required("--genesis-hash")?)?;
        let identity = Identity {
            source_id: required("--source-id")?.into(),
            run_id: required("--run-id")?.into(),
            genesis_hash: hex(expected.as_slice()),
            chain_id: required("--chain-id")?.into(),
        };
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(|_| Error("StorageUnavailable"))?;
        let pin_file = path.join("trust.json");
        let response: GenesisResponse = if pin_file.exists() {
            serde_json::from_slice(&read_file(
                pin_file.to_str().ok_or(Error("InvalidRequest"))?,
                256 * 1024,
            )?)
            .map_err(|_| Error("IdentityMismatch"))?
        } else {
            runtime::fetch(&client, peer, "/signed/v1/genesis", None, None, 256 * 1024).await?
        };
        if response.identity != identity {
            return Err(Error("IdentityMismatch"));
        }
        let (genesis, certificate) =
            response.validate(expected, proposer, number(&identity.chain_id)?)?;
        if bundle.as_ref().is_some_and(|(g, _)| g != &genesis) {
            return Err(Error("IdentityMismatch"));
        }
        retain_public(
            &pin_file,
            &serde_json::to_vec(&response).map_err(|_| Error("InvalidRequest"))?,
        )?;
        retain_public(&path.join("genesis.bin"), &encode(&genesis))?;
        retain_public(&path.join("certificate.bin"), &encode(&certificate))?;
        if command == "light" {
            let state = store(&path.join("headers.redb"))?;
            let light = if state.head()?.is_some() {
                Light::open(state, &genesis)?
            } else {
                Light::create(state, genesis, certificate)?
            };
            Node::Light {
                light: Box::new(light),
                identity,
            }
        } else {
            if replication.is_none() || token.as_ref().is_none_or(|s| s.len() < 32) {
                return Err(Error("InvalidRequest"));
            }
            let prefill = if genesis.prefill.is_some() {
                let bytes = if let Some((_, bytes)) = &bundle {
                    bytes.clone()
                } else if path.join("prefill.bin").exists() {
                    read_file(
                        path.join("prefill.bin")
                            .to_str()
                            .ok_or(Error("InvalidRequest"))?,
                        arkiv_node::genesis_bundle::MAX_CANONICAL_BYTES as usize,
                    )?
                } else {
                    let data: serde_json::Value = runtime::fetch(
                        &client,
                        peer,
                        "/signed/v1/prefill",
                        None,
                        None,
                        2 * arkiv_node::genesis_bundle::MAX_CANONICAL_BYTES as usize + 128,
                    )
                    .await?;
                    let encoded = data["prefillBytes"]
                        .as_str()
                        .ok_or(Error("InvalidPrefillBundle"))?;
                    unhex(
                        encoded,
                        genesis
                            .prefill
                            .as_ref()
                            .ok_or(Error("InvalidPrefillBundle"))?
                            .canonical_bytes as usize,
                    )?
                };
                local_prefill(&path, &genesis, Some(&bytes))?
            } else {
                None
            };
            let state = store(&path.join("node.redb"))?;
            let hs = store(&path.join("history.redb"))?;
            let history = if hs.head()?.is_some() {
                History::open(hs, genesis.hash())?
            } else {
                History::create(hs, genesis.hash())?
            };
            let mut host = if state.head()?.is_some() {
                Host::open(state, history, &genesis, None, options.clone())?
            } else {
                Host::create_with_prefill(
                    state,
                    history,
                    genesis,
                    parse_uuid(&identity.source_id)?,
                    certificate,
                    None,
                    options.clone(),
                    prefill.clone(),
                )?
            };
            host.prefill_stream = prefill;
            if host.identity != identity {
                return Err(Error("IdentityMismatch"));
            }
            host.paused = false;
            host.health = "following";
            Node::Full(Box::new(host))
        }
    };
    let rt = Runtime::new(node, peer, replication, token)?;
    println!(
        "{}",
        runtime::status(&*rt.node.lock().map_err(|_| Error("StorageUnavailable"))?)
    );
    let metrics = if let Some(listen) = args.get("--metrics-listen") {
        Some(
            arkiv_node::signed::metrics::Metrics::start(
                rt.clone(),
                databases.into_inner(),
                match command.as_str() {
                    "producer" => "producer",
                    "full" => "full",
                    _ => "light",
                },
                listen,
                Duration::from_secs(metrics_interval),
            )
            .await?,
        )
    } else {
        None
    };
    let timer = rt.clone();
    let producing = command == "producer";
    tokio::spawn(async move {
        let period = if producing {
            Duration::from_millis(
                timer
                    .node
                    .lock()
                    .expect("node mutex")
                    .genesis()
                    .block_period_ms,
            )
        } else {
            Duration::from_millis(50)
        };
        let mut cadence = ProductionCadence::new(tokio::time::Instant::now(), period);
        loop {
            tokio::time::sleep_until(cadence.next).await;
            let tick_started = std::time::Instant::now();
            let result = if producing {
                let timer = timer.clone();
                tokio::task::spawn_blocking(move || {
                    let mut node = timer.node.try_lock().map_err(|_| Error("ServerBusy"))?;
                    if let Node::Full(h) = &mut *node
                        && !h.paused
                    {
                        h.produce()?;
                        return Ok(true);
                    }
                    Ok(false)
                })
                .await
                .unwrap_or(Err(Error("StorageUnavailable")))
            } else {
                // Followers import one block per poll; retain fast polling.
                runtime::sync_once(timer.clone()).await.map(|_| false)
            };
            if let Some(metrics) = &metrics {
                metrics.activity.record(tick_started.elapsed(), &result);
            }
            match result {
                Ok(produced) => cadence.complete(tokio::time::Instant::now(), produced),
                Err(e) => {
                    if e.0 != "ServerBusy" {
                        eprintln!("signed node: {}", e.0);
                    }
                    // Retry the same slot after contention or failure without
                    // spinning or losing scheduled production time.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }
    });
    if let Some(private) = args.get("--private-listen") {
        let listener = tokio::net::TcpListener::bind(private)
            .await
            .map_err(|_| Error("StorageUnavailable"))?;
        let router = runtime::private_router(rt.clone());
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
    }
    let listener = tokio::net::TcpListener::bind(get("--listen", "127.0.0.1:9500"))
        .await
        .map_err(|_| Error("StorageUnavailable"))?;
    axum::serve(listener, runtime::public_router(rt))
        .await
        .map_err(|_| Error("StorageUnavailable"))
}

/// Fixed deadlines keep block execution/persistence time out of the cadence.
struct ProductionCadence {
    next: tokio::time::Instant,
    period: Duration,
}
impl ProductionCadence {
    fn new(now: tokio::time::Instant, period: Duration) -> Self {
        Self {
            next: now + period,
            period,
        }
    }

    fn complete(&mut self, now: tokio::time::Instant, produced: bool) {
        if produced {
            self.next += self.period;
        } else {
            // Pausing intentionally suspends production. Do not accumulate
            // automatic catch-up blocks while paused.
            self.next = now + self.period;
        }
    }
}

#[cfg(test)]
mod cadence_tests {
    use super::*;

    #[test]
    fn execution_time_does_not_accumulate() {
        let start = tokio::time::Instant::now();
        let period = Duration::from_secs(2);
        let mut clock = ProductionCadence::new(start, period);
        for slot in 1..=1000 {
            assert_eq!(clock.next, start + period * slot);
            clock.complete(clock.next + Duration::from_millis(75), true);
        }
        assert_eq!(clock.next, start + period * 1001);
    }

    #[test]
    fn slow_block_preserves_missed_slots() {
        let start = tokio::time::Instant::now();
        let mut clock = ProductionCadence::new(start, Duration::from_secs(2));
        let recovered = start + Duration::from_secs(7);
        clock.complete(recovered, true);
        assert_eq!(clock.next, start + Duration::from_secs(4));
        clock.complete(recovered, true);
        assert_eq!(clock.next, start + Duration::from_secs(6));
        clock.complete(recovered, true);
        assert_eq!(clock.next, start + Duration::from_secs(8));
    }

    #[test]
    fn pause_discards_elapsed_slots() {
        let start = tokio::time::Instant::now();
        let mut clock = ProductionCadence::new(start, Duration::from_secs(2));
        let resumed = start + Duration::from_secs(100);
        clock.complete(resumed, false);
        assert_eq!(clock.next, resumed + Duration::from_secs(2));
    }
}
