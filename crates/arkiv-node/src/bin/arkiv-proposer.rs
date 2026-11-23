//! Explicit signed single-proposer profile. Keys are local and never returned by HTTP.

use arkiv_db::{
    decode_exact,
    durable::{DurableEngine, Options},
    encode,
};
use arkiv_db_redb::{RedbConfig, RedbStore};
use arkiv_db_storage::versioned::VersionedStore;
use arkiv_node::{
    genesis_error::GenesisToolError,
    health::Health,
    signed::{
        Error, ErrorCode,
        crypto::Signer,
        history::History,
        host::{Host, Launch, Prefill},
        light::Light,
        protocol::{SignedGenesis, SignedHeader, SignedTx, TX_CAP},
        reads::ReadLimits,
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

/// Why the command failed. A genesis-tooling failure prints its message
/// and exits as `InvalidGenesisBundle`.
enum CliError {
    Node(Error),
    Genesis(GenesisToolError),
}

impl From<Error> for CliError {
    fn from(error: Error) -> Self {
        Self::Node(error)
    }
}

impl From<ErrorCode> for CliError {
    fn from(code: ErrorCode) -> Self {
        Self::Node(code.into())
    }
}

impl From<arkiv_db::StorageError> for CliError {
    fn from(error: arkiv_db::StorageError) -> Self {
        Self::Node(error.into())
    }
}

impl From<arkiv_db::durable::DurableError> for CliError {
    fn from(error: arkiv_db::durable::DurableError) -> Self {
        Self::Node(error.into())
    }
}

impl From<GenesisToolError> for CliError {
    fn from(error: GenesisToolError) -> Self {
        Self::Genesis(error)
    }
}

type Result<T> = std::result::Result<T, CliError>;

/// The flags of one subcommand, each given once with a value.
struct Flags(BTreeMap<String, String>);

impl Flags {
    /// Refuses a flag the subcommand does not take.
    fn only(self, allowed: &[&[&str]]) -> Result<Self> {
        let known = |key: &String| allowed.iter().any(|set| set.contains(&key.as_str()));
        if self.0.keys().all(known) {
            Ok(self)
        } else {
            Err(ErrorCode::InvalidRequest.into())
        }
    }

    fn get(&self, key: &str) -> Option<&String> {
        self.0.get(key)
    }

    fn contains_key(&self, key: &str) -> bool {
        self.0.contains_key(key)
    }

    fn required(&self, key: &str) -> Result<&str> {
        self.get(key)
            .map(String::as_str)
            .ok_or(ErrorCode::InvalidRequest.into())
    }

    /// An optional flag holding a count.
    fn count(&self, key: &str) -> Result<Option<usize>> {
        let Some(value) = self.get(key) else {
            return Ok(None);
        };
        let count = usize::try_from(number(value)?).map_err(|_| ErrorCode::InvalidRequest)?;
        Ok(Some(count))
    }

    fn get_or(&self, key: &str, default: &str) -> String {
        self.get(key).cloned().unwrap_or_else(|| default.into())
    }
}

fn arguments() -> Result<(String, Flags)> {
    let mut args = std::env::args().skip(1);
    let command = args.next().unwrap_or_else(|| "--help".into());
    let mut flags = BTreeMap::new();
    while let Some(key) = args.next() {
        if !key.starts_with("--") {
            return Err(ErrorCode::InvalidRequest.into());
        }
        let value = args.next().ok_or(ErrorCode::InvalidRequest)?;
        if flags.insert(key, value).is_some() {
            return Err(ErrorCode::InvalidRequest.into());
        }
    }
    Ok((command, Flags(flags)))
}

/// `StorageUnavailable` for an I/O error on `path`, which the cause names.
fn unavailable(path: &Path) -> impl Fn(std::io::Error) -> Error + '_ {
    move |error| {
        ErrorCode::StorageUnavailable.caused_by(format_args!("{}: {error}", path.display()))
    }
}

fn read_file(path: &str, cap: usize) -> Result<Vec<u8>> {
    use std::io::Read;
    let unreadable = |error: std::io::Error| {
        ErrorCode::InvalidRequest.caused_by(format_args!("{path}: {error}"))
    };
    let file = std::fs::File::open(path).map_err(unreadable)?;
    let mut bytes = Vec::new();
    file.take(cap as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(unreadable)?;
    if bytes.len() > cap {
        return Err(ErrorCode::InvalidRequest
            .caused_by(format_args!("{path}: longer than {cap} bytes"))
            .into());
    }
    Ok(bytes)
}

fn signer(path: &str) -> Result<Signer> {
    let bytes = read_file(path, 128)?;
    let text = std::str::from_utf8(&bytes).map_err(|_| ErrorCode::InvalidKey)?;
    let raw = unhex(text.trim(), 32)?;
    Ok(Signer::from_bytes(
        raw.try_into().map_err(|_| ErrorCode::InvalidKey)?,
    )?)
}

fn store(path: &Path, streaming_import: bool) -> Result<Arc<RedbStore>> {
    let opened = if path.exists() {
        RedbStore::open(path, RedbConfig::default())?
    } else {
        RedbStore::create(path, RedbConfig::default())?
    };
    if streaming_import && opened.head()?.is_none() {
        // Only a headless streaming-import destination gets the larger bounded
        // cache. Existing nodes and non-state stores retain the normal default.
        drop(opened);
        return Ok(Arc::new(RedbStore::open(
            path,
            arkiv_node::genesis_streaming::import_redb_config(),
        )?));
    }
    Ok(Arc::new(opened))
}

fn write_new(path: &Path, bytes: &[u8], private: bool) -> Result<()> {
    use std::io::Write;
    let mut unique = [0u8; 16];
    getrandom::fill(&mut unique).map_err(|_| ErrorCode::StorageUnavailable)?;
    let temporary = path.with_file_name(format!(
        ".{}.{}.pending",
        path.file_name()
            .and_then(|n| n.to_str())
            .ok_or(ErrorCode::InvalidRequest)?,
        hex(&unique)
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(if private { 0o600 } else { 0o644 });
    }
    let mut file = options.open(&temporary).map_err(unavailable(&temporary))?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(unavailable(&temporary))?;
    // Atomic no-clobber installation: a process cut leaves only an unreferenced
    // temporary file, never partially written trust material under its final name.
    std::fs::hard_link(&temporary, path).map_err(unavailable(path))?;
    std::fs::remove_file(&temporary).map_err(unavailable(&temporary))?;
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::File::open(parent)
            .and_then(|f| f.sync_all())
            .map_err(unavailable(parent))?;
    }
    Ok(())
}

fn retain_public(path: &Path, bytes: &[u8]) -> Result<()> {
    if path.exists() {
        if read_file(
            path.to_str().ok_or(ErrorCode::InvalidRequest)?,
            arkiv_node::genesis_bundle::MAX_CANONICAL_BYTES as usize,
        )? != bytes
        {
            return Err(ErrorCode::IdentityMismatch.into());
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
        return Err(ErrorCode::InvalidAmount.into());
    }
    value.parse().map_err(|_| ErrorCode::InvalidAmount.into())
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
        .map_err(|_| ErrorCode::InvalidAllocations)?;
    let values = values
        .0
        .into_iter()
        .map(|(actor, balance)| {
            Ok((
                arkiv_db::OwnerId::new(
                    unhex(&actor, 20)?
                        .try_into()
                        .map_err(|_| ErrorCode::InvalidAllocations)?,
                ),
                amount(&balance)?,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    arkiv_node::accounts::AccountConfig::new(values)
        .map_err(|_| ErrorCode::InvalidAllocations.into())
}

enum PrefillSource {
    Small(Vec<u8>),
    Streaming {
        path: PathBuf,
        verified: arkiv_node::genesis_streaming::VerifiedStream,
    },
}

fn streaming(genesis: &SignedGenesis) -> bool {
    genesis.prefill.as_ref().is_some_and(|p| p.is_streaming())
}

fn load_bundle(path: &Path) -> Result<(SignedGenesis, PathBuf)> {
    let genesis: SignedGenesis = decode_exact(&read_file(
        path.join("genesis.bin")
            .to_str()
            .ok_or(ErrorCode::InvalidRequest)?,
        65536,
    )?)
    .map_err(|_| ErrorCode::InvalidGenesisBundle)?;
    genesis.validate()?;
    let input = path.join("prefill.bin");
    if streaming(&genesis) {
        arkiv_node::genesis_streaming::verify_stream_file(&genesis, &input)?;
    } else {
        arkiv_node::genesis_authoring::verify(path)?;
    }
    Ok((genesis, input))
}

fn local_prefill(
    path: &Path,
    genesis: &SignedGenesis,
    supplied: Option<&Path>,
) -> Result<Option<PrefillSource>> {
    if genesis.prefill.is_none() {
        return Ok(None);
    }
    let target = path.join("prefill.bin");
    if streaming(genesis) {
        let verified = if !target.exists() {
            let source = supplied.ok_or(ErrorCode::PrefillBundleRequired)?;
            arkiv_node::genesis_streaming::check_disk(
                path,
                genesis
                    .prefill
                    .as_ref()
                    .ok_or(ErrorCode::InvalidPrefillBundle)?
                    .canonical_bytes,
            )?;
            let temporary = path.join("prefill.copy.pending");
            // A partial copy is disposable; publication is atomic and no-clobber.
            std::fs::copy(source, &temporary).map_err(unavailable(source))?;
            let file = std::fs::File::open(&temporary).map_err(unavailable(&temporary))?;
            file.sync_all().map_err(unavailable(&temporary))?;
            let verified = arkiv_node::genesis_streaming::VerifiedStream::from_file(genesis, file)?;
            std::fs::hard_link(&temporary, &target).map_err(unavailable(&target))?;
            verified.check_path_identity(&target)?;
            std::fs::remove_file(&temporary).map_err(unavailable(&temporary))?;
            std::fs::File::open(path)
                .and_then(|f| f.sync_all())
                .map_err(unavailable(path))?;
            verified
        } else {
            arkiv_node::genesis_streaming::VerifiedStream::open(genesis, &target)?
        };
        return Ok(Some(PrefillSource::Streaming {
            path: target,
            verified,
        }));
    }
    let stream = read_file(
        supplied
            .unwrap_or(&target)
            .to_str()
            .ok_or(ErrorCode::InvalidRequest)?,
        arkiv_node::genesis_bundle::MAX_CANONICAL_BYTES as usize,
    )?;
    arkiv_node::genesis_bundle::verify_genesis(genesis, &stream)?;
    retain_public(&target, &stream)?;
    Ok(Some(PrefillSource::Small(stream)))
}

fn create_host(
    state: Arc<RedbStore>,
    options: Options,
    launch: Launch,
    prefill: Option<PrefillSource>,
    path: &Path,
) -> Result<Host> {
    match prefill {
        Some(PrefillSource::Streaming {
            path: input,
            verified,
        }) => {
            let staged = arkiv_node::genesis_streaming::prepare_verified_streaming(
                &launch.genesis,
                verified,
                &path.join("prefill-work"),
                options.clone(),
            )?;
            let staged_file = path.join("prefill-work/bulk-v1/complete/state.redb");
            let staging_bytes = std::fs::metadata(&staged_file)
                .map_err(unavailable(&staged_file))?
                .len();
            arkiv_node::genesis_streaming::check_disk(path, staging_bytes)?;
            let prefill = Prefill::Staged {
                engine: &staged,
                stream_file: input.clone(),
            };
            let host = Host::create(state, options, launch, prefill)?;
            // Host serves the retained pathname. Re-audit after the final copy
            // to catch path replacement or inode mutation since bulk construction ended.
            arkiv_node::genesis_streaming::verify_stream_file(&host.genesis, &input)?;
            drop(staged);
            if let Err(error) = std::fs::remove_dir_all(path.join("prefill-work")) {
                eprintln!("genesis installed; could not remove import scratch: {error}");
            }
            Ok(host)
        }
        Some(PrefillSource::Small(bytes)) => Ok(Host::create(
            state,
            options,
            launch,
            Prefill::Stream(bytes),
        )?),
        None => Ok(Host::create(state, options, launch, Prefill::None)?),
    }
}

/// `--genesis-install adopt`: install a streaming genesis by verifying and
/// relabelling the staged store in place and renaming it over the headless
/// `state` (see `arkiv_node::genesis_adopt`), instead of copying it.
fn adopt_host(
    state: Arc<RedbStore>,
    options: Options,
    mut launch: Launch,
    prefill: Option<PrefillSource>,
    path: &Path,
    reopen: &dyn Fn(&Path, bool) -> Result<Arc<RedbStore>>,
) -> Result<Host> {
    use arkiv_node::genesis_adopt::{self, Staged};
    let Some(PrefillSource::Streaming {
        path: input,
        verified,
    }) = prefill
    else {
        eprintln!("--genesis-install adopt requires a streaming prefill bundle");
        return Err(ErrorCode::InvalidRequest.into());
    };
    let started = std::time::Instant::now();
    let work = path.join("prefill-work");
    let staged = genesis_adopt::staged_path(&work);
    let node = path.join("node.redb");
    let resumed = genesis_adopt::inspect(&staged)? == Staged::Relabelled;
    if !resumed {
        let engine = arkiv_node::genesis_streaming::prepare_verified_streaming(
            &launch.genesis,
            verified,
            &work,
            options.clone(),
        )?;
        genesis_adopt::check_rename(&staged, &node)?;
        let verifying = std::time::Instant::now();
        Host::adopt_staged(
            &mut launch,
            options.clone(),
            engine,
            genesis_adopt::verify_threads(),
        )?;
        eprintln!(
            "{}",
            serde_json::json!({"event": "prefill_genesis_adopt", "stage": "relabelled",
                "verifySeconds": verifying.elapsed().as_secs_f64()})
        );
    }
    // Check the relabelled store before it takes the node database's name.
    Host::check_adopted(
        Arc::new(RedbStore::open(&staged, RedbConfig::default())?),
        &launch.genesis,
        launch.source,
        options.clone(),
    )?;
    genesis_adopt::publish(&staged, &node, state.as_ref())?;
    drop(state);
    let host = Host::open(
        reopen(&node, false)?,
        launch.history,
        &launch.genesis,
        launch.signer,
        options,
    )?
    .serving_prefill_file(input.clone());
    // As after a copy: catch path replacement or inode mutation since the
    // bulk construction ended.
    arkiv_node::genesis_streaming::verify_stream_file(&host.genesis, &input)?;
    eprintln!(
        "{}",
        serde_json::json!({"event": "prefill_genesis_adopt", "stage": "adopted",
            "resumed": resumed, "elapsedSeconds": started.elapsed().as_secs_f64()})
    );
    if let Err(error) = std::fs::remove_dir_all(&work) {
        eprintln!("genesis adopted; could not remove import scratch: {error}");
    }
    Ok(host)
}

/// A reopened host serving the prefill it was created from.
fn serving(host: Host, prefill: Option<PrefillSource>) -> Host {
    match prefill {
        Some(PrefillSource::Small(bytes)) => host.serving_prefill_stream(bytes),
        Some(PrefillSource::Streaming { path, .. }) => host.serving_prefill_file(path),
        None => host,
    }
}

async fn download_prefill(
    client: &reqwest::Client,
    peer: &str,
    path: &Path,
    genesis: &SignedGenesis,
) -> Result<()> {
    use std::io::Write;
    let launch = genesis
        .prefill
        .as_ref()
        .ok_or(ErrorCode::InvalidPrefillBundle)?;
    let target = path.join("prefill.bin");
    if target.exists() {
        return Ok(());
    }
    let partial = path.join("prefill.download.pending");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&partial)
        .map_err(unavailable(&partial))?;
    let mut offset = file.metadata().map_err(unavailable(&partial))?.len();
    if offset > launch.canonical_bytes {
        return Err(ErrorCode::InvalidPrefillBundle.into());
    }
    arkiv_node::genesis_streaming::check_disk(path, launch.canonical_bytes - offset)?;
    while offset < launch.canonical_bytes {
        arkiv_node::genesis_streaming::check_disk(path, 1024 * 1024)?;
        let length = (launch.canonical_bytes - offset).min(1024 * 1024);
        let url = format!(
            "{}/signed/v1/prefill/chunk?offset={offset}&length={length}",
            peer.trim_end_matches('/')
        );
        let unreachable = |error: reqwest::Error| ErrorCode::PeerUnavailable.caused_by(error);
        let mut response = client.get(url).send().await.map_err(unreachable)?;
        if !response.status().is_success() {
            return Err(ErrorCode::PeerUnavailable
                .caused_by(format_args!("prefill chunk: HTTP {}", response.status()))
                .into());
        }
        let mut buffer = Vec::with_capacity(length as usize);
        while let Some(chunk) = response.chunk().await.map_err(unreachable)? {
            if buffer.len() + chunk.len() > length as usize {
                return Err(ErrorCode::InvalidPrefillBundle.into());
            }
            buffer.extend_from_slice(&chunk);
        }
        if buffer.len() != length as usize {
            return Err(ErrorCode::InvalidPrefillBundle.into());
        }
        file.write_all(&buffer)
            .and_then(|_| file.sync_data())
            .map_err(unavailable(&partial))?;
        offset += length;
    }
    file.sync_all().map_err(unavailable(&partial))?;
    drop(file);
    if let Err(error) = arkiv_node::genesis_streaming::verify_stream_file(genesis, &partial) {
        // Never retry an already complete, corrupt download forever. Its pinned
        // identity remains in trust.json; the next start downloads fresh bytes.
        std::fs::remove_file(&partial).map_err(unavailable(&partial))?;
        return Err(error.into());
    }
    std::fs::hard_link(&partial, &target).map_err(unavailable(&target))?;
    std::fs::remove_file(&partial).map_err(unavailable(&partial))?;
    std::fs::File::open(path)
        .and_then(|f| f.sync_all())
        .map_err(unavailable(path))?;
    Ok(())
}

fn genesis_command() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(2).collect();
    let [command, source, rest @ ..] = args.as_slice() else {
        return Err(ErrorCode::InvalidRequest.into());
    };
    if command == "stream-build" {
        let mut out = None;
        let mut prefill = None;
        let mut sponsor = None;
        let mut state_out = None;
        let mut batch_bytes = arkiv_node::genesis_streaming::DEFAULT_BATCH_BYTES;
        let mut flags = rest.iter();
        while let Some(flag) = flags.next() {
            let value = flags.next().ok_or(ErrorCode::InvalidRequest)?;
            match flag.as_str() {
                "--out" if out.is_none() => out = Some(value),
                "--prefill" if prefill.is_none() => prefill = Some(value),
                "--sponsor" if sponsor.is_none() => sponsor = Some(value),
                "--state-out" if state_out.is_none() => state_out = Some(value),
                "--batch-bytes" => {
                    eprintln!(concat!(
                        "--batch-bytes is deprecated: bulk genesis uses fixed bounded sort ",
                        "buffers; this option only controls progress sampling.",
                    ));
                    batch_bytes = value.parse().map_err(|_| ErrorCode::InvalidRequest)?
                }
                _ => return Err(ErrorCode::InvalidRequest.into()),
            }
        }
        let address = unhex(sponsor.ok_or(ErrorCode::InvalidRequest)?, 20)?;
        let result = arkiv_node::genesis_streaming::build_bundle_with_state(
            Path::new(source),
            Path::new(prefill.ok_or(ErrorCode::InvalidRequest)?),
            Path::new(out.ok_or(ErrorCode::InvalidRequest)?),
            state_out.map(Path::new),
            address.try_into().map_err(|_| ErrorCode::InvalidRequest)?,
            batch_bytes,
        )?;
        println!("{result}");
        return Ok(());
    }
    if command == "stream-verify" {
        if !rest.is_empty() {
            return Err(ErrorCode::InvalidRequest.into());
        }
        let dir = Path::new(source);
        let genesis: SignedGenesis = decode_exact(&read_file(
            dir.join("genesis.bin")
                .to_str()
                .ok_or(ErrorCode::InvalidRequest)?,
            65536,
        )?)
        .map_err(|_| ErrorCode::InvalidGenesis)?;
        let work = dir.with_extension("verify-work");
        arkiv_node::genesis_streaming::prepare_streaming(
            &genesis,
            &dir.join("prefill.bin"),
            &work,
            Options::default(),
        )?;
        std::fs::remove_dir_all(&work).map_err(|_| ErrorCode::InvalidRequest)?;
        println!(
            "{}",
            arkiv_node::genesis_streaming::summary_report(&genesis)?
        );
        return Ok(());
    }
    let mut out = None;
    let mut locked = false;
    let mut refresh = false;
    let mut flags = rest.iter();
    while let Some(flag) = flags.next() {
        match flag.as_str() {
            "--out" if out.is_none() => out = Some(flags.next().ok_or(ErrorCode::InvalidRequest)?),
            "--locked" if !locked => locked = true,
            "--refresh" if !refresh => refresh = true,
            _ => return Err(ErrorCode::InvalidRequest.into()),
        }
    }
    let result = match command.as_str() {
        "lock" if out.is_none() && !locked => {
            arkiv_node::genesis_authoring::lock(Path::new(source), refresh)
        }
        "build" if out.is_some() && !refresh => {
            arkiv_node::genesis_authoring::build(Path::new(source), Path::new(out.unwrap()), locked)
        }
        "verify" if out.is_none() && !locked && !refresh => {
            arkiv_node::genesis_authoring::verify(Path::new(source))
        }
        _ => {
            eprintln!(concat!(
                "genesis lock CHAIN [--refresh] | genesis build CHAIN --out BUNDLE --locked | ",
                "genesis verify BUNDLE | genesis stream-build BASE_GENESIS --prefill FILE ",
                "--out BUNDLE --sponsor 0xADDRESS [--state-out DIR] | genesis stream-verify BUNDLE",
            ));
            return Err(ErrorCode::InvalidRequest.into());
        }
    }?;
    println!("{result}");
    Ok(())
}

/// `state gc-enable|gc-status|gc-verify|compact --run-dir DIR`: offline
/// garbage collection tooling for a stopped node's `node.redb`.
fn state_command() -> Result<()> {
    let mut args = std::env::args().skip(2);
    let sub = args.next().unwrap_or_default();
    let mut flags = BTreeMap::new();
    while let Some(key) = args.next() {
        let value = args.next().ok_or(ErrorCode::InvalidRequest)?;
        if !key.starts_with("--") || flags.insert(key, value).is_some() {
            return Err(ErrorCode::InvalidRequest.into());
        }
    }
    let flags = Flags(flags).only(&[&["--run-dir", "--state-retention"]])?;
    let run_dir = PathBuf::from(flags.required("--run-dir")?);
    let path = run_dir.join("node.redb");
    if sub == "migrate" {
        return migrate_state(&run_dir, &flags);
    }
    if sub == "migrate-records" {
        let state = RedbStore::open(&path, RedbConfig::default())?;
        let history = RedbStore::open(run_dir.join("history.redb"), RedbConfig::default())?;
        let started = std::time::Instant::now();
        let report = arkiv_node::signed::migrate::migrate_records(&state, &history)?;
        let mut out = serde_json::json!(report);
        out["seconds"] = serde_json::json!(started.elapsed().as_secs_f64());
        println!("{out}");
        return Ok(());
    }
    if flags.contains_key("--state-retention") {
        return Err(ErrorCode::InvalidRequest.into());
    }
    // Opening takes the file lock: refused while a node runs on it.
    let store = Arc::new(RedbStore::open(&path, RedbConfig::default())?);
    let report = |state: &arkiv_db_storage::gc::GcState| {
        serde_json::json!({
            "suspended": state.suspended,
            "epoch": state.epoch,
            "storedObjects": state.stored_objects,
            "storedBytes": state.stored_bytes,
            "liveObjects": state.live_objects(),
            "deadObjects": state.dead_objects,
            "pendingRelease": state.pending,
            "queued": state.queued,
            "sharedRows": state.shared,
            "phantomRows": state.phantoms,
            "deletedTotal": state.deleted_total,
        })
    };
    match sub.as_str() {
        "gc-enable" => {
            let spill = run_dir.join("gc-seed");
            let _ = std::fs::remove_dir_all(&spill);
            let started = std::time::Instant::now();
            let state = arkiv_db::durable::gc::enable(store.as_ref(), Some(&spill))?;
            let _ = std::fs::remove_dir_all(&spill);
            let mut out = report(&state);
            out["seconds"] = serde_json::json!(started.elapsed().as_secs_f64());
            println!("{out}");
        }
        "table-stats" => {
            let file = std::fs::metadata(&path).map_err(unavailable(&path))?;
            let tables: Vec<_> = store
                .table_stats()?
                .into_iter()
                .map(|(name, entries, stored, pages)| {
                    serde_json::json!({"table": name, "entries": entries,
                        "storedBytes": stored, "pageBytes": pages})
                })
                .collect();
            println!(
                "{}",
                serde_json::json!({"fileBytes": file.len(), "tables": tables})
            );
        }
        "gc-status" => match store.gc_state()? {
            Some(state) => println!("{}", report(&state)),
            None => println!("{}", serde_json::json!({"stateGc": "off"})),
        },
        "gc-verify" => {
            arkiv_db::durable::gc::verify(store.as_ref()).map_err(|detail| {
                ErrorCode::StorageUnavailable.caused_by(format_args!("gc verify: {detail}"))
            })?;
            println!("{}", serde_json::json!({"verified": true}));
        }
        _ => {
            eprintln!(
                "state gc-enable|gc-status|gc-verify|table-stats|migrate-records \
                 --run-dir DIR | state migrate --run-dir DIR --state-retention N"
            );
            return Err(ErrorCode::InvalidRequest.into());
        }
    }
    Ok(())
}

/// Writes the indexes that record expiry needs into files written before
/// it (`arkiv_node::signed::migrate`), once, before the node opens them.
fn migrate_records_if_needed(state: &RedbStore, history: &RedbStore) -> Result<()> {
    if state.head()?.is_none() || !arkiv_node::signed::migrate::needs_migration(history)? {
        return Ok(());
    }
    let started = std::time::Instant::now();
    let report = arkiv_node::signed::migrate::migrate_records(state, history)?;
    eprintln!(
        "{}",
        serde_json::json!({
            "event": "records_migrated",
            "report": report,
            "seconds": started.elapsed().as_secs_f64(),
        })
    );
    Ok(())
}

/// `state migrate --run-dir DIR --state-retention N`: copies `node.redb`
/// into a new file holding every record but only the objects the snapshots
/// from the new floor to the head reach, counts it, and swaps it in. The
/// old file is kept as `node.redb.archive` for the operator to delete.
fn migrate_state(run_dir: &Path, flags: &Flags) -> Result<()> {
    let window = match flags.required("--state-retention")?.parse() {
        Ok(arkiv_db::durable::Retention::Window(window)) => window,
        _ => return Err(ErrorCode::InvalidRequest.into()),
    };
    let path = run_dir.join("node.redb");
    let copy = run_dir.join("node.redb.migrate");
    let archive = run_dir.join("node.redb.archive");
    if archive.exists() {
        eprintln!("{} exists; remove it first", archive.display());
        return Err(ErrorCode::InvalidRequest.into());
    }
    let started = std::time::Instant::now();
    let retained = {
        let store = RedbStore::open(&path, RedbConfig::default())?;
        arkiv_db::durable::gc::retained(&store, window)?
    };
    let _ = std::fs::remove_file(&copy);
    let overrides: Vec<_> = retained.floor_record.iter().cloned().collect();
    let report = arkiv_db_redb::copy_reachable(
        &path,
        &copy,
        &retained.snapshots,
        &arkiv_db::durable::gc::EngineGraph,
        &overrides,
    )?;
    let spill = run_dir.join("gc-seed");
    let _ = std::fs::remove_dir_all(&spill);
    let state = {
        let store = RedbStore::open(&copy, RedbConfig::default())?;
        arkiv_db::durable::gc::enable(&store, Some(&spill))?
    };
    let _ = std::fs::remove_dir_all(&spill);
    std::fs::rename(&path, &archive).map_err(unavailable(&path))?;
    std::fs::rename(&copy, &path).map_err(unavailable(&copy))?;
    std::fs::File::open(run_dir)
        .and_then(|dir| dir.sync_all())
        .map_err(unavailable(run_dir))?;
    let size = |p: &Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    println!(
        "{}",
        serde_json::json!({
            "head": retained.head,
            "floor": retained.floor,
            "records": report.records,
            "objectsCopied": report.objects,
            "objectBytesCopied": report.object_bytes,
            "deadAtHead": state.dead_objects,
            "fileBytesBefore": size(&archive),
            "fileBytesAfter": size(&path),
            "seconds": started.elapsed().as_secs_f64(),
        })
    );
    Ok(())
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        let code = match error {
            CliError::Node(e) => {
                if let Some(cause) = &e.cause {
                    eprintln!("{cause}");
                }
                e.code
            }
            CliError::Genesis(e) => {
                eprintln!("{e}");
                ErrorCode::InvalidGenesisBundle
            }
        };
        eprintln!("arkiv-proposer: {code}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    if std::env::args().nth(1).as_deref() == Some("genesis") {
        return genesis_command();
    }
    if std::env::args().nth(1).as_deref() == Some("state") {
        return state_command();
    }
    let (command, args) = arguments()?;
    match command.as_str() {
        "--version" => println!("arkiv-proposer {}", env!("CARGO_PKG_VERSION")),
        "--help" => help(),
        "keygen" => keygen(&args.only(&[&["--out"]])?)?,
        "sign" => sign(&command, &args.only(&[SIGNING_FLAGS, &["--tx-file"]])?)?,
        "create" => sign(&command, &args.only(&[SIGNING_FLAGS, CREATE_FLAGS])?)?,
        "transfer" => sign(&command, &args.only(&[SIGNING_FLAGS, TRANSFER_FLAGS])?)?,
        "submit" => submit(&args.only(&[&["--transaction-file", "--peer"]])?).await?,
        "producer" => node(&command, &args.only(&[NODE_FLAGS, PRODUCER_FLAGS])?).await?,
        "full" => {
            let flags = [
                NODE_FLAGS,
                FOLLOWER_FLAGS,
                &["--state-retention", "--body-retention"],
            ];
            node(&command, &args.only(&flags)?).await?
        }
        "light" => node(&command, &args.only(&[NODE_FLAGS, FOLLOWER_FLAGS])?).await?,
        _ => return Err(ErrorCode::InvalidRequest.into()),
    }
    Ok(())
}

const SIGNING_FLAGS: &[&str] = &["--key-file", "--genesis-file", "--nonce", "--out"];
const CREATE_FLAGS: &[&str] = &[
    "--request-id",
    "--data-file",
    "--gas-limit",
    "--salt",
    "--lifetime-blocks",
];
const TRANSFER_FLAGS: &[&str] = &["--request-id", "--to", "--amount-wei", "--gas-limit"];
/// Flags of every node role.
const NODE_FLAGS: &[&str] = &[
    "--run-dir",
    "--listen",
    "--private-listen",
    "--bundle",
    "--metrics-listen",
    "--metrics-table-stats-secs",
    "--read-concurrency",
    "--read-deadline-secs",
    "--read-max-groups",
    "--read-max-literals",
    "--read-max-offset",
];
const PRODUCER_FLAGS: &[&str] = &[
    "--key-file",
    "--chain-id",
    "--block-period-ms",
    "--allocations-file",
    "--source-id",
    "--start",
    "--genesis-install",
    "--prebuilt-state",
    "--state-retention",
    "--body-retention",
];
const FOLLOWER_FLAGS: &[&str] = &[
    "--peer",
    "--replication-peer",
    "--proposer",
    "--genesis-hash",
    "--source-id",
    "--run-id",
    "--chain-id",
];

fn help() {
    println!(concat!(
        "arkiv-proposer genesis lock CHAIN [--refresh]\n",
        "arkiv-proposer genesis build CHAIN --out BUNDLE --locked\n",
        "arkiv-proposer genesis verify BUNDLE\n",
        "arkiv-proposer genesis stream-build BASE_GENESIS --prefill FILE --out BUNDLE ",
        "--sponsor 0xADDRESS [--state-out DIR]\n",
        "arkiv-proposer genesis stream-verify BUNDLE\n",
        "arkiv-proposer keygen --out FILE\n",
        "arkiv-proposer producer --run-dir DIR --key-file FILE [--listen 127.0.0.1:9500] ",
        "[--private-listen 127.0.0.1:9501] [--start paused|running] [--source-id UUID] ",
        "[--chain-id 9001] [--block-period-ms 1000] [--state-retention archive|N (256)] ",
        "[--body-retention archive|window (archive)] ",
        "[--read-concurrency 4] [--read-deadline-secs 30] [--read-max-groups N] ",
        "[--read-max-literals N] [--read-max-offset 100000] (--allocations-file FILE | --bundle BUNDLE ",
        "[--prebuilt-state DIR])\n",
        "arkiv-proposer full|light --run-dir DIR --peer URL --genesis-hash 0xHEX ",
        "--proposer 0xADDRESS --chain-id N --source-id UUID --run-id HEX32 ",
        "[--replication-peer URL] [--bundle BUNDLE] [--state-retention archive|N (full ",
        "only, 256)] [--body-retention archive|window (full only, archive)] ",
        "[--read-concurrency 4] [--read-deadline-secs 30] [--read-max-groups N] ",
        "[--read-max-literals N] [--read-max-offset 100000]\n",
        "arkiv-proposer sign --key-file FILE --genesis-file FILE --tx-file FILE --nonce N ",
        "--out FILE\n",
        "arkiv-proposer create --key-file FILE --genesis-file FILE --nonce N --request-id ",
        "0xHEX64 --data-file FILE --out FILE [--salt 0] [--gas-limit 1000000] ",
        "[--lifetime-blocks 17280]\n",
        "arkiv-proposer transfer --key-file FILE --genesis-file FILE --nonce N ",
        "--request-id 0xHEX64 --to 0xADDRESS --amount-wei DECIMAL --out FILE [--gas-limit ",
        "21000]\n",
        "arkiv-proposer submit --peer URL --transaction-file FILE\n",
        "arkiv-proposer state gc-enable|gc-status|gc-verify|table-stats|migrate-records ",
        "--run-dir DIR ",
        "(node stopped)\n",
        "arkiv-proposer state migrate --run-dir DIR --state-retention N (node stopped)\n",
        "Optional private Prometheus exporter: --metrics-listen 127.0.0.1:9600 ",
        "[--metrics-table-stats-secs N] (minimum 10; opt-in table walk, which keeps redb from ",
        "reusing freed pages while it runs). Metrics use rogue_one_ prefix.\n",
        "Private listener/full replication use ARKIV_PROPOSER_TOKEN (at least 32 bytes). ",
        "Producer starts paused. Every chain runs the glm-v2 gas profile with funded ",
        "accounts. A bundle chain creates the entities namespace (and its prefill) at ",
        "height 0; a chain created from --allocations-file creates it with the proposer's ",
        "bootstrap transaction in block 1. Each run exports public ",
        "genesis.bin/certificate.bin. stream-build --state-out keeps its verified private ",
        "state; a fresh producer given it with --prebuilt-state moves it into the run ",
        "directory and installs it without rebuilding (it is ignored once the node is ",
        "initialized). create writes FILE as an unindexed bytes attribute `data` (at most ",
        "128 KiB) beside an indexed u64 `group` = 1. Sign/create files use native ",
        "canonical bytes; submit polls pending/included status. Single-proposer ",
        "authentication supplies no consensus or finality. Bodies are retained in ",
        "history.redb separately from node.redb. --state-retention N serves state ",
        "(balances, proofs, queries, receipts/logs) only for the head and the N snapshots ",
        "before it and refuses older heights with HistoryPruned; archive serves every ",
        "height. Without the flag a new node keeps 256, and a node whose state file has ",
        "always been an archive stays one. A pruned floor never moves back. A window node that creates or ",
        "adopts its state turns on garbage collection in node.redb: objects no snapshot ",
        "from the floor up (or open reader) reaches are deleted inside block commits, so ",
        "the file stays bounded. An older state file keeps everything until `state ",
        "gc-enable` counts it offline. ",
        "A window node also expires its per-height records with the state (feeds, ",
        "blocks, receipts, request inclusions, transaction and block hash lookups): ",
        "below the record floor they answer HistoryPruned (hash lookups: null), and ",
        "restart reads only the retained blocks. A request identity stays unique for ",
        "256 blocks; nonces keep a transaction from ever repeating. ",
        "--body-retention archive (default) keeps every block body and its ",
        "certificate in history.redb, so followers and light clients can still start ",
        "from genesis and transactions by block number and index (and block ",
        "transaction counts) are served at every height; window deletes them with the records, which bounds history.redb ",
        "but leaves new followers without a source (no snapshot sync yet). Files from ",
        "before record expiry are indexed once at start (or with `state ",
        "migrate-records`). ",
        "Entity queries and counts, proofs and the state tree explorer run outside the ",
        "node lock against a pinned snapshot: at most --read-concurrency (1-64) at once, ",
        "others refused with ServerBusy (HTTP 429), each stopped after ",
        "--read-deadline-secs (1-3600) with ReadDeadlineExceeded (HTTP 503) or when its ",
        "client disconnects. Entity queries and counts are planned: each AND-group is ",
        "driven by its smallest index posting, so clause order does not change their ",
        "cost, and counts use stored posting sizes. An offset or ORDER BY walk that ",
        "stored posting sizes show the 25-million-unit read budget cannot finish is ",
        "refused at once (-32004), and so is an arkiv_query offset above ",
        "--read-max-offset (default 100000; 0 = no limit but the budget) unless the ",
        "request carries a cursor. --read-max-groups (1-64) and ",
        "--read-max-literals (1-256) raise the genesis query bounds for these reads only ",
        "(default: the genesis values); larger queries still use the bounded scan ",
        "fallback (queries) or are refused (counts).",
    ));
    println!(concat!(
        "producer --genesis-install copy|adopt: how a fresh producer installs a streaming ",
        "bundle's staged genesis state. copy (default) copies every reachable object into ",
        "a new node.redb. adopt reads and verifies the same objects without writing them, ",
        "relabels the private staged store in place and renames it to node.redb, so no ",
        "second copy of the state is written or stored; it consumes the run directory's ",
        "prefill-work store, an interrupted adoption resumes only with adopt, and the ",
        "option is ignored once the node is initialized.",
    ));
}

fn keygen(args: &Flags) -> Result<()> {
    let mut key = [0u8; 32];
    getrandom::fill(&mut key).map_err(|_| ErrorCode::StorageUnavailable)?;
    let s = Signer::from_bytes(key)?;
    write_new(
        Path::new(args.required("--out")?),
        hex(&key).as_bytes(),
        true,
    )?;
    println!("{}", hex(s.address().as_bytes()));
    Ok(())
}

/// `sign`, `create` and `transfer`: writes one signed transaction.
fn sign(command: &str, args: &Flags) -> Result<()> {
    let s = signer(args.required("--key-file")?)?;
    let genesis: SignedGenesis = decode_exact(&read_file(args.required("--genesis-file")?, 65536)?)
        .map_err(|_| ErrorCode::InvalidRequest)?;
    genesis.validate()?;
    let nonce = number(args.required("--nonce")?)?;
    let tx = if command == "sign" {
        decode_exact(&read_file(args.required("--tx-file")?, TX_CAP as usize)?)
            .map_err(|_| ErrorCode::InvalidRequest)?
    } else if command == "transfer" {
        arkiv_node::TxEnvelope {
            chain_id: genesis.genesis.chain_id,
            actor: s.address(),
            request_id: unhex(args.required("--request-id")?, 32)?
                .try_into()
                .map_err(|_| ErrorCode::InvalidRequest)?,
            tx_budget: number(&args.get_or("--gas-limit", "21000"))?,
            ops: vec![arkiv_node::TxOp::Transfer {
                to: arkiv_db::OwnerId::new(
                    unhex(args.required("--to")?, 20)?
                        .try_into()
                        .map_err(|_| ErrorCode::InvalidRequest)?,
                ),
                amount: amount(args.required("--amount-wei")?)?,
            }],
        }
    } else {
        let data = read_file(args.required("--data-file")?, 128 * 1024)?;
        let request_id = unhex(args.required("--request-id")?, 32)?
            .try_into()
            .map_err(|_| ErrorCode::InvalidRequest)?;
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
                    salt: number(&args.get_or("--salt", "0"))? as u128,
                    flags: 0,
                    expiry: if args.contains_key("--lifetime-blocks")
                        || arkiv_node::gas::is_active(&genesis.genesis.pricing)
                    {
                        arkiv_node::Expiry {
                            expires_at: 0,
                            min_lifetime: number(&args.get_or("--lifetime-blocks", "17280"))?,
                        }
                    } else {
                        arkiv_node::Expiry::PERMANENT
                    },
                    attributes: vec![
                        (
                            arkiv_db::CellName::new("data").unwrap(),
                            arkiv_db::Cell::field(arkiv_db::Value::bytes(&data)),
                        ),
                        (
                            arkiv_db::CellName::new("group").unwrap(),
                            arkiv_db::Cell::attribute(arkiv_db::Value::U64(1)),
                        ),
                    ],
                },
            }],
        }
    };
    let signed = SignedTx::sign(&genesis, nonce, tx, &s)?;
    write_new(Path::new(args.required("--out")?), &encode(&signed), false)?;
    println!("{}", hex(signed.hash().as_slice()));
    Ok(())
}

async fn submit(args: &Flags) -> Result<()> {
    let bytes = read_file(args.required("--transaction-file")?, TX_CAP as usize)?;
    let tx: SignedTx = decode_exact(&bytes).map_err(|_| ErrorCode::InvalidRequest)?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|_| ErrorCode::StorageUnavailable)?;
    let peer = args.required("--peer")?;
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
    Ok(())
}

/// What the node roles start from: the flags, the run directory and the
/// databases opened in it.
struct NodeEnv<'a> {
    args: &'a Flags,
    path: &'a Path,
    bundle: Option<(SignedGenesis, PathBuf)>,
    options: Options,
    /// `--body-retention window`: bodies expire from `history.redb` with
    /// the node's records.
    prune_bodies: bool,
    store: &'a dyn Fn(&Path, bool) -> Result<Arc<RedbStore>>,
    peer: Option<String>,
    replication: Option<String>,
    token: Option<String>,
}

/// `producer`, `full` and `light`: opens or creates the node and serves it.
/// The window producers and full nodes keep without `--state-retention`.
const DEFAULT_WINDOW: u64 = 256;

/// `--state-retention` when not given: a window of [`DEFAULT_WINDOW`] for a
/// producer or full node's new state, or for one that already prunes. A
/// state file that has always been an archive stays one: turning it into a
/// window is irreversible, so it takes the flag (or `state migrate`).
fn default_retention(run_dir: &Path, command: &str) -> Result<arkiv_db::durable::Retention> {
    use arkiv_db::durable::Retention;
    if command == "light" {
        return Ok(Retention::Archive);
    }
    // A missing or unopenable file is new state; the real open reports
    // any damage.
    let Ok(store) = RedbStore::open(run_dir.join("node.redb"), RedbConfig::default()) else {
        return Ok(Retention::Window(DEFAULT_WINDOW));
    };
    let archive = store.head()?.is_some()
        && store.gc_state()?.is_none()
        && arkiv_db::durable::gc::durable_floor(&store)? == 0;
    Ok(if archive {
        Retention::Archive
    } else {
        Retention::Window(DEFAULT_WINDOW)
    })
}

async fn node(command: &str, args: &Flags) -> Result<()> {
    // Only a bundle producer may reuse prebuilt state; followers always derive
    // streaming state independently from the stream.
    if args.contains_key("--prebuilt-state") && !args.contains_key("--bundle") {
        return Err(ErrorCode::InvalidRequest.into());
    }
    let path = PathBuf::from(args.required("--run-dir")?);
    std::fs::create_dir_all(&path).map_err(unavailable(&path))?;
    let token = std::env::var("ARKIV_PROPOSER_TOKEN").ok();
    if args.contains_key("--private-listen") && token.as_ref().is_none_or(|s| s.len() < 32) {
        return Err(ErrorCode::InvalidRequest.into());
    }
    let peer = args.get("--peer").cloned();
    let replication = args.get("--replication-peer").cloned();
    let bundle = args
        .get("--bundle")
        .map(|dir| load_bundle(Path::new(dir)))
        .transpose()?;
    // The table walk is opt-in: it pins every page freed while it runs.
    let table_stats = args
        .get("--metrics-table-stats-secs")
        .map(|secs| number(secs))
        .transpose()?;
    if table_stats.is_some_and(|secs| secs < 10) {
        return Err(ErrorCode::InvalidRequest.into());
    }
    if table_stats.is_some() && !args.contains_key("--metrics-listen") {
        return Err(ErrorCode::InvalidRequest.into());
    }
    let defaults = ReadLimits::default();
    let read_limits = ReadLimits {
        concurrency: args
            .count("--read-concurrency")?
            .unwrap_or(defaults.concurrency),
        deadline: match args.get("--read-deadline-secs") {
            Some(value) => Duration::from_secs(number(value)?),
            None => defaults.deadline,
        },
        max_groups: args.count("--read-max-groups")?,
        max_literals: args.count("--read-max-literals")?,
        // 0 leaves offsets to the read budget alone.
        max_offset: match args.get("--read-max-offset") {
            Some(value) => Some(number(value)?).filter(|max| *max > 0),
            None => defaults.max_offset,
        },
    };
    read_limits.validate()?;
    // Only a producer may adopt its own staged genesis store; followers keep
    // the copy installation.
    let adopt = match args.get("--genesis-install").map(String::as_str) {
        None | Some("copy") => false,
        Some("adopt") => true,
        Some(_) => return Err(ErrorCode::InvalidRequest.into()),
    };
    let retention = match args.get("--state-retention") {
        Some(value) => value.parse().map_err(|_| ErrorCode::InvalidRequest)?,
        None => default_retention(&path, command)?,
    };
    let prune_bodies = match args.get("--body-retention").map(String::as_str) {
        None | Some("archive") => false,
        // Bodies can only expire below a record floor, which needs a window.
        Some("window") if retention.window().is_some() => true,
        Some(_) => return Err(ErrorCode::InvalidRequest.into()),
    };
    let options = Options {
        retention,
        // Measurement switch: count but never delete (soak step "count-only").
        gc_count_only: std::env::var_os("ARKIV_STATE_GC_COUNT_ONLY").is_some(),
        ..Options::default()
    };
    let databases = std::cell::RefCell::new(Vec::<arkiv_node::signed::metrics::Database>::new());
    let store = |path: &Path, streaming_import: bool| -> Result<Arc<RedbStore>> {
        let opened = store(path, streaming_import)?;
        let name = match path.file_name().and_then(|s| s.to_str()) {
            Some("node.redb") => "state",
            Some("history.redb") => "history",
            Some("headers.redb") => "headers",
            _ => return Err(ErrorCode::InvalidRequest.into()),
        };
        let mut databases = databases.borrow_mut();
        // A reopened database (an adopted node.redb) replaces its entry.
        databases.retain(|database| database.name != name);
        databases.push(arkiv_node::signed::metrics::Database {
            name,
            path: path.to_owned(),
            store: opened.clone(),
        });
        Ok(opened)
    };
    let env = NodeEnv {
        args,
        path: &path,
        bundle,
        options,
        prune_bodies,
        store: &store,
        peer,
        replication,
        token,
    };
    let node = if command == "producer" {
        producer_node(&env, adopt)?
    } else {
        follower_node(&env, command == "light").await?
    };
    let NodeEnv {
        peer,
        replication,
        token,
        ..
    } = env;
    let rt = Runtime::with_read_limits(node, peer, replication, token, read_limits)?;
    println!(
        "{}",
        runtime::status(&*rt.node.lock().map_err(|_| ErrorCode::StorageUnavailable)?)
    );
    let metrics = if let Some(listen) = args.get("--metrics-listen") {
        Some(
            arkiv_node::signed::metrics::Metrics::start(
                rt.clone(),
                databases.into_inner(),
                match command {
                    "producer" => "producer",
                    "full" => "full",
                    _ => "light",
                },
                listen,
                table_stats.map(Duration::from_secs),
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
                    // Waits for the lock: reads yield to production.
                    let mut node = timer.write_node()?;
                    if let Node::Full(h) = &mut *node
                        && !h.paused
                    {
                        h.produce()?;
                        return Ok(true);
                    }
                    Ok(false)
                })
                .await
                .unwrap_or(Err(ErrorCode::StorageUnavailable.into()))
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
                    if e.code != ErrorCode::ServerBusy {
                        eprintln!("signed node: {}", e.diagnostic());
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
            .map_err(unavailable(Path::new(private)))?;
        let router = runtime::private_router(rt.clone());
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
    }
    let listen = args.get_or("--listen", "127.0.0.1:9500");
    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .map_err(unavailable(Path::new(&listen)))?;
    exit_on_signal(rt.clone());
    Ok(axum::serve(listener, runtime::public_router(rt))
        .await
        .map_err(unavailable(Path::new(&listen)))?)
}

/// Exits on SIGTERM or SIGINT (`docker stop`, Kubernetes, Ctrl-C) once no
/// block is in flight: production and import commit under the node lock, and
/// every commit is durable, so holding the lock is a clean stopping point.
/// Without a handler a node running as PID 1 in a container ignores SIGTERM
/// and is killed only when the stop grace period ends.
fn exit_on_signal(rt: runtime::Shared) {
    #[cfg(unix)]
    tokio::spawn(async move {
        use tokio::signal::unix::{SignalKind, signal};
        let (Ok(mut term), Ok(mut int)) = (
            signal(SignalKind::terminate()),
            signal(SignalKind::interrupt()),
        ) else {
            eprintln!("signal handlers unavailable; stop the node with SIGKILL");
            return;
        };
        let name = tokio::select! {
            _ = term.recv() => "SIGTERM",
            _ = int.recv() => "SIGINT",
        };
        eprintln!(
            "{}",
            serde_json::json!({"event": "shutdown", "signal": name})
        );
        let _ = tokio::task::spawn_blocking(move || {
            let _node = rt.write_node();
            eprintln!(
                "{}",
                serde_json::json!({"event": "shutdown", "result": "stopped"})
            );
            std::process::exit(0);
        })
        .await;
    });
    #[cfg(not(unix))]
    let _ = rt;
}

/// The genesis retained in the run directory by an earlier start, if any.
fn retained_genesis(path: &Path) -> Result<Option<SignedGenesis>> {
    let file = path.join("genesis.bin");
    if !file.exists() {
        return Ok(None);
    }
    let bytes = read_file(file.to_str().ok_or(ErrorCode::InvalidRequest)?, 65536)?;
    Ok(Some(
        decode_exact(&bytes).map_err(|_| ErrorCode::IdentityMismatch)?,
    ))
}

/// Whether every given flag of `fixed` has the value the chain fixes.
fn flags_match(args: &Flags, fixed: &[(&str, String)]) -> bool {
    fixed
        .iter()
        .all(|(flag, value)| args.get(flag).is_none_or(|given| given == value))
}

/// Refuses an `--allocations-file` other than the one `genesis` was made of.
fn check_allocations(args: &Flags, genesis: &SignedGenesis) -> Result<()> {
    if let Some(file) = args.get("--allocations-file")
        && genesis.accounts != allocations(file)?
    {
        return Err(ErrorCode::IdentityMismatch.into());
    }
    Ok(())
}

/// Refuses a bundle of another chain than `genesis`.
fn check_bundle(env: &NodeEnv<'_>, genesis: &SignedGenesis) -> Result<()> {
    if env.bundle.as_ref().is_some_and(|(g, _)| g != genesis) {
        return Err(ErrorCode::IdentityMismatch.into());
    }
    Ok(())
}

fn producer_node(env: &NodeEnv<'_>, adopt: bool) -> Result<Node> {
    let (args, path, store) = (env.args, env.path, env.store);
    let signer = signer(args.required("--key-file")?)?;
    let streaming_import = match &env.bundle {
        Some((genesis, _)) => streaming(genesis),
        None => retained_genesis(path)?.as_ref().is_some_and(streaming),
    };
    let state = store(&path.join("node.redb"), streaming_import)?;
    let history_store = store(&path.join("history.redb"), false)?;
    migrate_records_if_needed(&state, &history_store)?;
    let mut host = if state.head()?.is_some() {
        reopen_producer(env, state, history_store, signer)?
    } else {
        launch_producer(env, state, history_store, signer, adopt)?
    };
    arkiv_node::retention::collect_new_state(&mut host.engine, path)?;
    host.prune_bodies(env.prune_bodies)?;
    match args.get_or("--start", "paused").as_str() {
        "paused" => {}
        "running" => {
            host.paused = false;
            host.health = Health::Running;
        }
        _ => return Err(ErrorCode::InvalidRequest.into()),
    };
    for (name, bytes) in [
        ("genesis.bin", encode(&host.genesis)),
        ("certificate.bin", encode(&host.certificate(0)?)),
    ] {
        let file = path.join(name);
        if !file.exists() {
            write_new(&file, &bytes, false)?;
        } else if read_file(file.to_str().ok_or(ErrorCode::InvalidRequest)?, 65536)? != bytes {
            return Err(ErrorCode::IdentityMismatch.into());
        }
    }
    Ok(Node::Full(Box::new(host)))
}

/// Opens the producer of the chain `state` already holds.
fn reopen_producer(
    env: &NodeEnv<'_>,
    state: Arc<RedbStore>,
    history_store: Arc<RedbStore>,
    signer: Signer,
) -> Result<Host> {
    let id = DurableEngine::read_simulator_identity(state.as_ref())?;
    let genesis: SignedGenesis =
        decode_exact(&id.metadata).map_err(|_| ErrorCode::IdentityMismatch)?;
    let fixed = [
        ("--chain-id", genesis.genesis.chain_id.to_string()),
        (
            "--source-id",
            arkiv_node::simulator::host::uuid(&id.source_id),
        ),
        ("--block-period-ms", genesis.block_period_ms.to_string()),
    ];
    if !flags_match(env.args, &fixed) {
        return Err(ErrorCode::IdentityMismatch.into());
    }
    check_allocations(env.args, &genesis)?;
    check_bundle(env, &genesis)?;
    if env.args.contains_key("--prebuilt-state") {
        eprintln!(
            "{}",
            serde_json::json!({
                "event": "prefill_prebuilt_install",
                "result": "initialized",
            })
        );
    }
    let history = History::open(history_store, genesis.hash())?;
    let host = Host::open(state, history, &genesis, Some(signer), env.options.clone())?;
    let bundle_prefill = env.bundle.as_ref().map(|(_, b)| b.as_path());
    let prefill = local_prefill(env.path, &genesis, bundle_prefill)?;
    Ok(serving(host, prefill))
}

/// The genesis a first start launches: the one an interrupted start
/// retained, else the bundle's, else a new one from the flags.
fn producer_genesis(env: &NodeEnv<'_>, signer: &Signer) -> Result<SignedGenesis> {
    let args = env.args;
    let proposed = if let Some((genesis, _)) = &env.bundle {
        genesis.clone()
    } else {
        let block_period_ms = number(&args.get_or("--block-period-ms", "1000"))?;
        // Rent is priced at this block period; the cap is a year.
        let native = arkiv_node::signed::protocol::default_native_genesis(
            number(&args.get_or("--chain-id", "9001"))?,
            1_700_000_000_000,
            block_period_ms,
        );
        let mut run_id = [0u8; 16];
        getrandom::fill(&mut run_id).map_err(|_| ErrorCode::StorageUnavailable)?;
        SignedGenesis::new(
            native,
            run_id,
            *signer.address().as_bytes(),
            block_period_ms,
            allocations(args.required("--allocations-file")?)?,
        )?
    };
    let genesis = retained_genesis(env.path)?.unwrap_or(proposed);
    genesis.validate()?;
    check_bundle(env, &genesis)?;
    check_allocations(args, &genesis)?;
    let fixed = [
        ("--chain-id", genesis.genesis.chain_id.to_string()),
        ("--block-period-ms", genesis.block_period_ms.to_string()),
    ];
    if genesis.proposer != *signer.address().as_bytes() || !flags_match(args, &fixed) {
        return Err(ErrorCode::IdentityMismatch.into());
    }
    Ok(genesis)
}

/// Creates the producer's chain in the empty `state`.
fn launch_producer(
    env: &NodeEnv<'_>,
    state: Arc<RedbStore>,
    history_store: Arc<RedbStore>,
    signer: Signer,
    adopt: bool,
) -> Result<Host> {
    let (args, path) = (env.args, env.path);
    let genesis = producer_genesis(env, &signer)?;
    if let Some(dir) = args.get("--prebuilt-state") {
        if !streaming(&genesis) {
            return Err(ErrorCode::InvalidRequest.into());
        }
        // The node database lock is held, so no other process uses
        // this run directory's private import scratch.
        let installed = arkiv_node::genesis_prebuilt::install(
            &genesis,
            Path::new(dir),
            &path.join("prefill-work"),
        )?;
        eprintln!(
            "{}",
            serde_json::json!({
                "event": "prefill_prebuilt_install",
                "result": installed.as_str(),
            })
        );
    }
    let bundle_prefill = env.bundle.as_ref().map(|(_, b)| b.as_path());
    let prefill = local_prefill(path, &genesis, bundle_prefill)?;
    let certificate = SignedHeader::sign(&genesis, genesis.header(), &signer)?;
    // Retain the chain identity before either database initialization;
    // a cut during genesis publication can retry the same chain.
    retain_public(&path.join("genesis.bin"), &encode(&genesis))?;
    retain_public(&path.join("certificate.bin"), &encode(&certificate))?;
    let history = if history_store.head()?.is_some() {
        History::open(history_store, genesis.hash())?
    } else {
        History::create(history_store, genesis.hash())?
    };
    let source = parse_uuid(&args.get_or("--source-id", "00000000-0000-4000-8000-000000000001"))?;
    let launch = Launch {
        history,
        genesis,
        source,
        certificate,
        signer: Some(signer),
    };
    if adopt {
        adopt_host(
            state,
            env.options.clone(),
            launch,
            prefill,
            path,
            &env.store,
        )
    } else {
        create_host(state, env.options.clone(), launch, prefill, path)
    }
}

async fn follower_node(env: &NodeEnv<'_>, light: bool) -> Result<Node> {
    let (args, path, store) = (env.args, env.path, env.store);
    let (bundle, options) = (&env.bundle, &env.options);
    let (peer, replication, token) = (&env.peer, &env.replication, &env.token);
    let peer = peer.as_deref().ok_or(ErrorCode::InvalidRequest)?;
    let proposer: [u8; 20] = unhex(args.required("--proposer")?, 20)?
        .try_into()
        .map_err(|_| ErrorCode::InvalidRequest)?;
    let expected = hash(args.required("--genesis-hash")?)?;
    let identity = Identity {
        source_id: args.required("--source-id")?.into(),
        run_id: args.required("--run-id")?.into(),
        genesis_hash: hex(expected.as_slice()),
        chain_id: args.required("--chain-id")?.into(),
    };
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|_| ErrorCode::StorageUnavailable)?;
    let pin_file = path.join("trust.json");
    let response: GenesisResponse = if pin_file.exists() {
        serde_json::from_slice(&read_file(
            pin_file.to_str().ok_or(ErrorCode::InvalidRequest)?,
            256 * 1024,
        )?)
        .map_err(|_| ErrorCode::IdentityMismatch)?
    } else {
        runtime::fetch(&client, peer, "/signed/v1/genesis", None, None, 256 * 1024).await?
    };
    if response.identity != identity {
        return Err(ErrorCode::IdentityMismatch.into());
    }
    let (genesis, certificate) =
        response.validate(expected, proposer, number(&identity.chain_id)?)?;
    if bundle.as_ref().is_some_and(|(g, _)| g != &genesis) {
        return Err(ErrorCode::IdentityMismatch.into());
    }
    retain_public(
        &pin_file,
        &serde_json::to_vec(&response).map_err(|_| ErrorCode::InvalidRequest)?,
    )?;
    retain_public(&path.join("genesis.bin"), &encode(&genesis))?;
    retain_public(&path.join("certificate.bin"), &encode(&certificate))?;
    if light {
        let state = store(&path.join("headers.redb"), false)?;
        let light = if state.head()?.is_some() {
            Light::open(state, &genesis)?
        } else {
            Light::create(state, genesis, certificate)?
        };
        Ok(Node::Light {
            light: Box::new(light),
            identity,
        })
    } else {
        if replication.is_none() || token.as_ref().is_none_or(|s| s.len() < 32) {
            return Err(ErrorCode::InvalidRequest.into());
        }
        let prefill = if genesis.prefill.is_some() {
            if bundle.is_none() && !path.join("prefill.bin").exists() {
                if streaming(&genesis) {
                    download_prefill(&client, peer, path, &genesis).await?;
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
                        .ok_or(ErrorCode::InvalidPrefillBundle)?;
                    let bytes = unhex(
                        encoded,
                        arkiv_node::genesis_bundle::MAX_CANONICAL_BYTES as usize,
                    )?;
                    arkiv_node::genesis_bundle::verify_genesis(&genesis, &bytes)?;
                    retain_public(&path.join("prefill.bin"), &bytes)?;
                }
            }
            local_prefill(path, &genesis, bundle.as_ref().map(|(_, p)| p.as_path()))?
        } else {
            None
        };
        let state = store(&path.join("node.redb"), streaming(&genesis))?;
        let hs = store(&path.join("history.redb"), false)?;
        migrate_records_if_needed(&state, &hs)?;
        let history = if hs.head()?.is_some() {
            History::open(hs, genesis.hash())?
        } else {
            History::create(hs, genesis.hash())?
        };
        let mut host = if state.head()?.is_some() {
            let host = Host::open(state, history, &genesis, None, options.clone())?;
            serving(host, prefill)
        } else {
            let launch = Launch {
                history,
                genesis,
                source: parse_uuid(&identity.source_id)?,
                certificate,
                signer: None,
            };
            create_host(state, options.clone(), launch, prefill, path)?
        };
        if host.identity != identity {
            return Err(ErrorCode::IdentityMismatch.into());
        }
        arkiv_node::retention::collect_new_state(&mut host.engine, path)?;
        host.prune_bodies(env.prune_bodies)?;
        host.paused = false;
        host.health = Health::Following;
        Ok(Node::Full(Box::new(host)))
    }
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
