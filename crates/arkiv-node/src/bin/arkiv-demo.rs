//! Volatile development chain. Run `scripts/full-light-demo.py` for the terminal view.
use arkiv_db::{decode_exact, encode};
use arkiv_node::demo::{
    auth::{SignedHeader, TrustConfig},
    runtime::{self, Node, Role},
    wire::{self, GenesisResponse, Request},
    workload,
};
use ed25519_dalek::SigningKey;
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

fn read_bounded(path: &Path, cap: usize) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(cap as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > cap {
        return Err("file too large".into());
    }
    Ok(bytes)
}
fn option<'a>(args: &'a BTreeMap<String, String>, name: &str, default: &'a str) -> &'a str {
    args.get(name).map(String::as_str).unwrap_or(default)
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let command = args
        .next()
        .ok_or("usage: arkiv-demo producer|full|light|rpc [--option value]")?;
    if command == "--version" {
        println!("arkiv-demo {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let mut options = BTreeMap::new();
    while let Some(key) = args.next() {
        if ![
            "--listen",
            "--run-dir",
            "--interval-ms",
            "--capacity",
            "--peer",
            "--trust",
            "--seed",
            "--workload",
            "--request",
        ]
        .contains(&key.as_str())
        {
            return Err(format!("unknown option {key}").into());
        }
        if options
            .insert(key, args.next().ok_or("option requires a value")?)
            .is_some()
        {
            return Err("duplicate option".into());
        }
    }
    let peer = option(&options, "--peer", "http://127.0.0.1:8781");
    if command == "rpc" {
        let text = option(&options, "--request", "{\"method\":\"status\"}");
        let request: Request = serde_json::from_str(text)?;
        println!(
            "{}",
            serde_json::to_string_pretty(&wire::rpc(&wire::client()?, peer, request).await?)?
        );
        return Ok(());
    }
    let capacity: usize = option(&options, "--capacity", "10000").parse()?;
    if capacity == 0 || capacity > 1_000_000 {
        return Err("capacity must be 1..1000000 retained blocks including genesis".into());
    }
    let role = match command.as_str() {
        "producer" => Role::Producer,
        "full" => Role::Full,
        "light" => Role::Light,
        _ => return Err("unknown command".into()),
    };
    let node = if role == Role::Producer {
        let run = Path::new(
            options
                .get("--run-dir")
                .ok_or("producer requires a NEW --run-dir")?,
        );
        // Atomic directory creation makes identities single-use. There is no
        // load/resume-signing path, even for a valid truncated recording.
        fs::create_dir(run)?;
        let mut secret = [0u8; 32];
        getrandom::fill(&mut secret).map_err(|e| format!("randomness: {e}"))?;
        let signer = SigningKey::from_bytes(&secret);
        let mut entropy = [0u8; 8];
        getrandom::fill(&mut entropy).map_err(|e| format!("randomness: {e}"))?;
        let timestamp = u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis())?;
        let genesis = workload::genesis(u64::from_be_bytes(entropy), timestamp);
        let trust = TrustConfig {
            genesis_hash: genesis.hash(),
            genesis,
            public_key: signer.verifying_key().to_bytes(),
        };
        let mut open = fs::OpenOptions::new();
        open.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            open.mode(0o600);
        }
        let mut key_file = open.open(run.join("producer.key"))?;
        key_file.write_all(&secret)?;
        secret.fill(0);
        fs::write(run.join("trust.bin"), encode(&trust))?;
        let workload = option(&options, "--workload", "on");
        let seed = match workload {
            "on" => Some(option(&options, "--seed", "7").parse()?),
            "off" => None,
            _ => return Err("--workload is on or off".into()),
        };
        let node = Node::producer(trust, signer, capacity, seed)?;
        println!(
            "single-producer development chain; retained in memory; trust={} capacity={capacity}",
            run.join("trust.bin").display()
        );
        node
    } else {
        let trust: TrustConfig = decode_exact(&read_bounded(
            Path::new(
                options
                    .get("--trust")
                    .ok_or("follower requires local --trust")?,
            ),
            65536,
        )?)?;
        trust.validate()?;
        let client = wire::client()?;
        // Endpoint supplies evidence, never the key or trusted genesis.
        let reply: GenesisResponse =
            wire::parse(wire::rpc(&client, peer, Request::Genesis {}).await?)?;
        let genesis: SignedHeader = wire::unpack(&reply.signed_header, 1024)?;
        Node::follower(role, trust, genesis, capacity, peer.into())?
    };
    let default_listen = match role {
        Role::Producer => "127.0.0.1:8781",
        Role::Full => "127.0.0.1:8782",
        Role::Light => "127.0.0.1:8783",
    };
    let listen = option(&options, "--listen", default_listen);
    let default_interval = if role == Role::Producer {
        "1000"
    } else {
        "100"
    };
    let interval =
        Duration::from_millis(option(&options, "--interval-ms", default_interval).parse()?);
    println!(
        "{} listening on {listen}; {}",
        role.name(),
        node.status().trust
    );
    runtime::serve(Arc::new(Mutex::new(node)), listen, interval).await?;
    Ok(())
}
