//! Database snapshots of a stopped full node, and the pin that keeps the
//! node able to feed the nodes installed from them.
//!
//! A snapshot of height `N` is a directory holding copies of `node.redb` and
//! `history.redb`, the public `genesis.bin` and `certificate.bin`, and
//! [`MANIFEST_FILE`], which names the chain and the head and carries the size
//! and SHA-256 of every file. The manifest is written last: a directory
//! without it is not a snapshot. `prefill.bin` and the pin are never part of
//! one.
//!
//! [`create`] makes one from a stopped node's run directory and
//! [`Snapshot::install`] turns one into a new run directory, which a `full`
//! node then opens with its usual pins and continues from block `N + 1`.
//!
//! Trust stays with the pins. The SHA-256 values detect a damaged or
//! incomplete transfer; they are not what makes a snapshot authentic. The
//! installed files are opened exactly as a node opens them: the stored
//! genesis is the pinned one, the head record carries a certificate signed
//! by the pinned proposer, and the head manifest has the state root of that
//! certificate. Every state object is stored under the hash of its bytes and
//! is linked to that root, so a wrong object fails a read; it cannot answer
//! one. Bodies and headers are checked against the proposer's signature by
//! whoever reads them. What the files hold beside that (the
//! garbage-collection counts, the feed and inclusion records of the last
//! heights) is bound to nothing: see [`Snapshot::install`] for what its
//! `verify_state` covers.
//!
//! The pin ([`PIN_FILE`] in the run directory) is a height. A node started
//! with it keeps every block body from that height on
//! ([`Host::set_body_pin`]), whatever its body retention.

use super::{
    Error, ErrorCode, Result,
    history::History,
    host::Host,
    protocol::{SignedGenesis, SignedHeader},
    runtime::{self, GenesisResponse},
};
use crate::simulator::wire::{Identity, RESPONSE_CAP, hex, number};
use arkiv_db::{
    B256, StorageErrorKind, decode_exact,
    durable::{DurableEngine, Options, Retention},
    encode,
};
use arkiv_db_redb::{RedbConfig, RedbStore};
use arkiv_db_storage::versioned::{Head as StoreHead, VersionedStore};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

/// The description of a snapshot, written last into its directory.
pub const MANIFEST_FILE: &str = "snapshot.json";
/// The pin of a run directory: the height from which bodies are kept.
pub const PIN_FILE: &str = "snapshot-pin";
/// [`Manifest::format_version`] this build writes and reads.
pub const FORMAT_VERSION: u32 = 1;
/// The database files of a snapshot, in the order they are copied.
pub const DATABASE_FILES: [&str; 2] = ["node.redb", "history.redb"];
/// The public files of a snapshot, as a node exports them.
pub const PUBLIC_FILES: [&str; 2] = ["genesis.bin", "certificate.bin"];

const MANIFEST_CAP: usize = 64 * 1024;
const GENESIS_CAP: usize = 65536;
const COPY_CHUNK: usize = 4 * 1024 * 1024;
/// The window a stopped node's files are opened with when they prune. Only
/// their own recorded floors are read: nothing is written.
const OPEN_WINDOW: u64 = 256;

/// One file of a snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FileEntry {
    pub name: String,
    pub bytes: u64,
    /// Lower-case hex, without a prefix (as `sha256sum` prints it).
    pub sha256: String,
}

/// `snapshot.json`: what a snapshot directory holds. Heights and the chain
/// ID are decimal strings and hashes `0x` hex, as in the node's status.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Manifest {
    pub format_version: u32,
    pub chain_id: String,
    pub run_id: String,
    pub source_id: String,
    pub genesis_hash: String,
    pub proposer: String,
    /// The head of the snapshot: a node installed from it continues with
    /// the block after this one.
    pub height: String,
    pub header_hash: String,
    pub state_root: String,
    /// Bodies of heights 1 up to (excluding) this one are not in
    /// `history.redb` (the status field of the same name); `0` when the
    /// snapshot holds every body.
    pub body_floor: String,
    /// The version of the build that made the snapshot.
    pub node_version: String,
    /// When the snapshot was made (RFC 3339, UTC).
    pub created_at: String,
    pub files: Vec<FileEntry>,
}

impl Manifest {
    pub fn height(&self) -> Result<u64> {
        number(&self.height)
    }

    pub fn body_floor(&self) -> Result<u64> {
        number(&self.body_floor)
    }

    pub fn identity(&self) -> Identity {
        Identity {
            source_id: self.source_id.clone(),
            run_id: self.run_id.clone(),
            genesis_hash: self.genesis_hash.clone(),
            chain_id: self.chain_id.clone(),
        }
    }

    fn file(&self, name: &str) -> Result<&FileEntry> {
        self.files
            .iter()
            .find(|file| file.name == name)
            .ok_or_else(|| refuse(ErrorCode::InvalidRequest, format_args!("{name} missing")))
    }

    /// Bytes of all files.
    pub fn total_bytes(&self) -> u64 {
        self.files.iter().map(|file| file.bytes).sum()
    }
}

fn refuse(code: ErrorCode, cause: impl std::fmt::Display) -> Error {
    code.caused_by(cause)
}

/// `StorageUnavailable` for an I/O error on `path`, which the cause names.
fn io_error(path: &Path) -> impl Fn(std::io::Error) -> Error + '_ {
    move |error| {
        refuse(
            ErrorCode::StorageUnavailable,
            format_args!("{}: {error}", path.display()),
        )
    }
}

fn sync_dir(path: &Path) -> Result<()> {
    File::open(path)
        .and_then(|dir| dir.sync_all())
        .map_err(io_error(path))
}

/// Writes `bytes` under `path` through a synced temporary file and a
/// rename, so that the name never holds a partial file.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(ErrorCode::InvalidRequest)?;
    let temporary = path.with_file_name(format!(".{name}.pending"));
    let written = File::create(&temporary).and_then(|mut file| {
        file.write_all(bytes)?;
        file.sync_all()
    });
    written.map_err(io_error(&temporary))?;
    std::fs::rename(&temporary, path).map_err(io_error(path))?;
    sync_dir(path.parent().unwrap_or(Path::new(".")))
}

fn read_capped(path: &Path, cap: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|file| file.take(cap as u64 + 1).read_to_end(&mut bytes))
        .map_err(io_error(path))?;
    if bytes.len() > cap {
        return Err(refuse(
            ErrorCode::InvalidRequest,
            format_args!("{}: longer than {cap} bytes", path.display()),
        ));
    }
    Ok(bytes)
}

/// The pin of `run_dir`, if it has one. A file that is not one decimal
/// height is refused: guessing would let the pinned bodies expire.
pub fn read_pin(run_dir: &Path) -> Result<Option<u64>> {
    let path = run_dir.join(PIN_FILE);
    if !path.exists() {
        return Ok(None);
    }
    let bytes = read_capped(&path, 64)?;
    let height = std::str::from_utf8(&bytes)
        .ok()
        .and_then(|text| crate::text::decimal(text.trim_end_matches('\n')));
    match height {
        Some(height) => Ok(Some(height)),
        None => Err(refuse(
            ErrorCode::InvalidRequest,
            format_args!("{}: not a block height", path.display()),
        )),
    }
}

/// Pins the bodies of `run_dir` from `height` on. The node reads the pin
/// when it starts.
pub fn write_pin(run_dir: &Path, height: u64) -> Result<()> {
    write_atomic(&run_dir.join(PIN_FILE), format!("{height}\n").as_bytes())
}

/// [`write_pin`] for an operator: `run_dir` must hold a node database, and
/// when no node runs on it the height is checked first: not above the head,
/// and not below the bodies that are already deleted, which no pin brings
/// back. Returns whether that check could be made (`false`: a node is
/// running, which reads the pin at its next start).
pub fn pin(run_dir: &Path, height: u64) -> Result<bool> {
    let state = run_dir.join(DATABASE_FILES[0]);
    if !state.is_file() {
        return Err(refuse(
            ErrorCode::InvalidRequest,
            format_args!("{}: no such database", state.display()),
        ));
    }
    let running = matches!(
        RedbStore::open(&state, RedbConfig::default()),
        Err(error) if error.kind == StorageErrorKind::Locked
    );
    if !running {
        let node = Stopped::open(run_dir, None)?;
        let (head, floor) = (node.host.head.height, node.host.history_pruned()?.height);
        if height > head || height < floor {
            return Err(refuse(
                ErrorCode::InvalidRequest,
                format_args!(
                    "height {height} cannot be pinned: the node has bodies from {floor} to \
                     its head {head}"
                ),
            ));
        }
    }
    write_pin(run_dir, height)?;
    Ok(!running)
}

/// Removes the pin of `run_dir`; whether it had one.
pub fn remove_pin(run_dir: &Path) -> Result<bool> {
    let path = run_dir.join(PIN_FILE);
    match std::fs::remove_file(&path) {
        Ok(()) => {
            sync_dir(run_dir)?;
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io_error(&path)(error)),
    }
}

/// Progress of a long file pass as JSON lines on stderr, at most one every
/// ten seconds and one when the file is complete (as `prefill_progress`).
struct Progress {
    stage: &'static str,
    started: Instant,
    last: Instant,
}

impl Progress {
    fn new(stage: &'static str) -> Self {
        let started = Instant::now();
        Self {
            stage,
            started,
            last: started,
        }
    }

    fn report(&mut self, file: &str, bytes: u64, total: u64, complete: bool) {
        let now = Instant::now();
        if !complete && now.duration_since(self.last) < Duration::from_secs(10) {
            return;
        }
        self.last = now;
        let record = json!({
            "event": "snapshot_progress", "stage": self.stage, "file": file,
            "bytes": bytes, "totalBytes": total,
            "elapsedSeconds": now.duration_since(self.started).as_secs_f64(),
            "complete": complete,
        });
        // Telemetry must never change what the command does.
        let _ = writeln!(std::io::stderr().lock(), "{record}");
    }
}

fn event(fields: Json) {
    let _ = writeln!(std::io::stderr().lock(), "{fields}");
}

/// Reads the open file `input` (at `source`) once from its start, hashing
/// it, and writes what it read to the new file `target` (if any), which is
/// synced before this returns. Reading and hashing run beside the writes.
fn hash_file(
    mut input: &File,
    source: &Path,
    target: Option<&Path>,
    name: &str,
    progress: &mut Progress,
) -> Result<FileEntry> {
    let total = input.metadata().map_err(io_error(source))?.len();
    let mut output = match target {
        Some(target) => Some(
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(target)
                .map_err(io_error(target))?,
        ),
        None => None,
    };
    let mut hasher = Sha256::new();
    let mut done = 0u64;
    let read = std::thread::scope(|scope| -> Result<()> {
        // Filled chunks to the writer; written ones come back for reuse.
        let (full, to_write) = std::sync::mpsc::sync_channel::<Vec<u8>>(4);
        let (written, spare) = std::sync::mpsc::channel::<Vec<u8>>();
        let writer = output.as_mut().map(|output| {
            scope.spawn(move || -> std::io::Result<()> {
                for chunk in to_write {
                    output.write_all(&chunk)?;
                    let _ = written.send(chunk);
                }
                output.sync_all()
            })
        });
        let mut outcome = Ok(());
        // Without a writer one chunk is reused.
        let mut idle = None;
        loop {
            let mut chunk: Vec<u8> = idle
                .take()
                .or_else(|| spare.try_recv().ok())
                .unwrap_or_default();
            chunk.resize(COPY_CHUNK, 0);
            let mut filled = 0;
            while filled < chunk.len() {
                match input.read(&mut chunk[filled..]) {
                    Ok(0) => break,
                    Ok(n) => filled += n,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(error) => {
                        outcome = Err(io_error(source)(error));
                        break;
                    }
                }
            }
            if filled == 0 || outcome.is_err() {
                break;
            }
            chunk.truncate(filled);
            hasher.update(&chunk);
            done += filled as u64;
            if writer.is_none() {
                idle = Some(chunk);
            } else if full.send(chunk).is_err() {
                // A writer that failed has hung up; its error is reported
                // below.
                break;
            }
            progress.report(name, done, total, false);
        }
        drop(full);
        if let Some(writer) = writer {
            let target = target.unwrap_or(source);
            writer
                .join()
                .map_err(|_| ErrorCode::StorageUnavailable)?
                .map_err(io_error(target))?;
        }
        outcome
    });
    read?;
    if done != total {
        return Err(refuse(
            ErrorCode::StorageUnavailable,
            format_args!("{}: changed while it was read", source.display()),
        ));
    }
    progress.report(name, done, total, true);
    Ok(FileEntry {
        name: name.into(),
        bytes: done,
        sha256: hex(hasher.finalize().as_slice())[2..].into(),
    })
}

fn entry_of(name: &str, bytes: &[u8]) -> FileEntry {
    FileEntry {
        name: name.into(),
        bytes: bytes.len() as u64,
        sha256: hex(Sha256::digest(bytes).as_slice())[2..].into(),
    }
}

/// The retention a stopped node's state file is opened with: an archive
/// when it has always been one, else a window. The file's own recorded
/// floors stand: opening with it prunes nothing.
fn stored_retention(store: &RedbStore) -> Result<Retention> {
    let archive = store.gc_state()?.is_none() && arkiv_db::durable::gc::durable_floor(store)? == 0;
    Ok(if archive {
        Retention::Archive
    } else {
        Retention::Window(OPEN_WINDOW)
    })
}

fn open_store(path: &Path) -> Result<Arc<RedbStore>> {
    if !path.is_file() {
        return Err(refuse(
            ErrorCode::InvalidRequest,
            format_args!("{}: no such database", path.display()),
        ));
    }
    match RedbStore::open(path, RedbConfig::default()) {
        Ok(store) => Ok(Arc::new(store)),
        Err(error) if error.kind == StorageErrorKind::Locked => Err(refuse(
            ErrorCode::StorageUnavailable,
            format_args!(
                "{}: locked: a node is running on this run directory; stop it first",
                path.display()
            ),
        )),
        Err(error) => Err(error.into()),
    }
}

/// The databases of a stopped node, opened as a follower opens them (no
/// key). Holding it holds the locks of both files: no node can start on
/// the directory meanwhile. Opening and closing them writes to the files
/// (redb marks a file open and rewrites its header when it closes), as
/// every start of a node does.
pub struct Stopped {
    pub host: Host,
    /// The storage heads of `node.redb` and `history.redb`.
    stores: [StoreHead; 2],
}

impl Stopped {
    /// Opens `dir/node.redb` and `dir/history.redb`. With `expected` the
    /// stored genesis must be that one; without, the stored one is used, as
    /// a producer does on restart. Refuses a directory a node runs on, one
    /// without a head, and files from before the height index (start the
    /// node once, or run `state migrate-records`).
    pub fn open(dir: &Path, expected: Option<&SignedGenesis>) -> Result<Self> {
        let state = open_store(&dir.join(DATABASE_FILES[0]))?;
        let history_store = open_store(&dir.join(DATABASE_FILES[1]))?;
        if state.head()?.is_none() {
            return Err(refuse(
                ErrorCode::InvalidRequest,
                format_args!("{}: the node database has no head", dir.display()),
            ));
        }
        if super::migrate::needs_migration(history_store.as_ref())? {
            return Err(refuse(
                ErrorCode::InvalidRequest,
                format_args!(
                    "{}: files from before the height index; run `arkiv-proposer state \
                     migrate-records --run-dir DIR` first",
                    dir.display()
                ),
            ));
        }
        let genesis = match expected {
            Some(genesis) => genesis.clone(),
            None => {
                let stored = DurableEngine::read_simulator_identity(state.as_ref())?;
                decode_exact(&stored.metadata).map_err(|_| ErrorCode::IdentityMismatch)?
            }
        };
        let options = Options {
            retention: stored_retention(&state)?,
            ..Options::default()
        };
        let stores = [
            state.head()?.ok_or(ErrorCode::StorageCorrupt)?,
            history_store.head()?.ok_or(ErrorCode::StorageCorrupt)?,
        ];
        let history = History::open(history_store, genesis.hash())?;
        let host = Host::open(state, history, &genesis, None, options)?;
        Ok(Self { host, stores })
    }

    fn head(&self) -> Head {
        Head {
            height: self.host.head.height,
            hash: self.host.head.hash(),
            state_root: self.host.head.state_root,
        }
    }
}

/// The head of a node: what a snapshot and its copy must agree on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Head {
    pub height: u64,
    pub hash: B256,
    pub state_root: B256,
}

impl Head {
    pub fn json(&self) -> Json {
        json!({
            "height": self.height.to_string(),
            "hash": hex(self.hash.as_slice()),
            "stateRoot": hex(self.state_root.as_slice()),
        })
    }
}

/// What [`create`] does with the pin of the run directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PinMode {
    /// Pin the snapshot height (the default): the newest snapshot is the
    /// one the node feeds.
    Set,
    /// Pin the snapshot height unless a lower pin exists, which stays: an
    /// older snapshot is still published.
    KeepLower,
    /// Leave the pin as it is.
    Leave,
}

/// A finished [`create`].
#[derive(Clone, Debug)]
pub struct Created {
    pub manifest: Manifest,
    /// The pin of the run directory afterwards.
    pub pin: Option<u64>,
    pub seconds: f64,
}

/// Refuses when the file system of the existing directory `dir` has less
/// than `bytes` free.
fn require_space(dir: &Path, bytes: u64) -> Result<()> {
    let stats = rustix::fs::statvfs(dir).map_err(|error| io_error(dir)(error.into()))?;
    let available = u128::from(stats.f_bavail) * u128::from(stats.f_frsize);
    if available < u128::from(bytes) {
        return Err(refuse(
            ErrorCode::StorageUnavailable,
            format_args!(
                "{}: {available} bytes free, the copy needs {bytes}",
                dir.display()
            ),
        ));
    }
    Ok(())
}

/// Whether `dir` exists and holds anything but `lost+found` (the root of a
/// fresh volume).
fn occupied(dir: &Path) -> Result<bool> {
    if !dir.exists() {
        return Ok(false);
    }
    for entry in std::fs::read_dir(dir).map_err(io_error(dir))? {
        if entry.map_err(io_error(dir))?.file_name() != "lost+found" {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Opens the database file at `path` for reading and takes a shared
/// record lock on all of it, held until the file is dropped: a node cannot
/// open the file meanwhile (redb's own locks are exclusive record locks on
/// the same file), and the file stays closed, which is the only state it
/// can be copied in. Refused while a node has the file open.
///
/// The lock is the process's (`fcntl`): closing any other handle of the
/// same file in this process would release it, so the caller reads through
/// the returned handle only.
fn lock_closed(path: &Path) -> Result<File> {
    use rustix::{fs::FlockOperation, io::Errno};
    let file = File::open(path).map_err(io_error(path))?;
    match rustix::fs::fcntl_lock(&file, FlockOperation::NonBlockingLockShared) {
        Ok(()) => Ok(file),
        Err(Errno::AGAIN | Errno::ACCESS) => Err(refuse(
            ErrorCode::StorageUnavailable,
            format_args!(
                "{}: locked: a node is running on this run directory; stop it first",
                path.display()
            ),
        )),
        // A file system without record locks has none of redb's either:
        // the head comparison after the copy is then the only guard.
        Err(Errno::NOLCK | Errno::NOSYS | Errno::OPNOTSUPP) => Ok(file),
        Err(error) => Err(io_error(path)(error.into())),
    }
}

/// Makes a snapshot of the stopped node in `run_dir` in the new or empty
/// directory `out`.
///
/// The two databases are first opened as a node opens them, which a
/// running node's locks refuse, to read the head, and closed again: a
/// database file can only be copied while it is closed. Both files are then
/// locked against a starting node until the end. Each is copied in one pass
/// that also computes its SHA-256, and synced. The copy is opened read-only
/// (which leaves its bytes as they were hashed) and must have the storage
/// heads that were read. `snapshot.json` is written last, through a
/// temporary file. Progress is reported on stderr.
///
/// The pin moves as `pin` says. A pin that makes the node keep more is
/// written before `snapshot.json`, and one that lets older bodies go only
/// after it: at no point is a complete snapshot left without its bodies.
pub fn create(run_dir: &Path, out: &Path, pin: PinMode) -> Result<Created> {
    let started = Instant::now();
    if occupied(out)? {
        return Err(refuse(
            ErrorCode::InvalidRequest,
            format_args!("{}: exists and is not empty", out.display()),
        ));
    }
    // Read as a node reads them (after an unclean stop this repairs the
    // files, as the next start would), then closed cleanly.
    let (head, stores, identity, proposer, body_floor, genesis, certificate) = {
        let source = Stopped::open(run_dir, None)?;
        let host = &source.host;
        (
            source.head(),
            source.stores,
            host.identity.clone(),
            hex(&host.genesis.proposer),
            host.history_pruned()?.height,
            encode(&host.genesis),
            encode(&host.chain_certificate(0)?),
        )
    };
    // From here no node can start on the run directory. One that started
    // since the close above is refused here; the check of the copy below
    // would catch even one that had started and stopped again.
    let locked = [
        lock_closed(&run_dir.join(DATABASE_FILES[0]))?,
        lock_closed(&run_dir.join(DATABASE_FILES[1]))?,
    ];
    let existing_pin = read_pin(run_dir)?;
    let mut partial = Partial {
        out,
        made_directory: !out.exists(),
        keep: false,
    };
    std::fs::create_dir_all(out).map_err(io_error(out))?;
    let total: u64 = DATABASE_FILES
        .iter()
        .map(|name| std::fs::metadata(run_dir.join(name)).map_or(0, |meta| meta.len()))
        .sum();
    require_space(out, total)?;
    event(json!({
        "event": "snapshot_create", "stage": "copying", "head": head.json(),
        "totalBytes": total,
    }));
    let mut files = Vec::new();
    let mut progress = Progress::new("copy");
    for (name, file) in DATABASE_FILES.iter().zip(&locked) {
        files.push(hash_file(
            file,
            &run_dir.join(name),
            Some(&out.join(name)),
            name,
            &mut progress,
        )?);
    }
    for (name, bytes) in PUBLIC_FILES.iter().zip([&genesis, &certificate]) {
        let path = out.join(name);
        File::create(&path)
            .and_then(|mut file| {
                file.write_all(bytes)?;
                file.sync_all()
            })
            .map_err(io_error(&path))?;
        files.push(entry_of(name, bytes));
    }
    sync_dir(out)?;
    // The copy must be the node that was read: each file opens, read-only,
    // with the storage head its original had. A read-only open cannot
    // repair, so it also shows the file was copied closed, and it leaves
    // the bytes as they were hashed.
    for (name, expected) in DATABASE_FILES.iter().zip(&stores) {
        let found = arkiv_db_redb::read_only_head(out.join(name))?;
        if found.as_ref() != Some(expected) {
            return Err(refuse(
                ErrorCode::StorageCorrupt,
                format_args!(
                    "{}: the copy is not the node that was read",
                    out.join(name).display()
                ),
            ));
        }
    }
    let manifest = Manifest {
        format_version: FORMAT_VERSION,
        chain_id: identity.chain_id,
        run_id: identity.run_id,
        source_id: identity.source_id,
        genesis_hash: identity.genesis_hash,
        proposer,
        height: head.height.to_string(),
        header_hash: hex(head.hash.as_slice()),
        state_root: hex(head.state_root.as_slice()),
        body_floor: body_floor.to_string(),
        node_version: env!("CARGO_PKG_VERSION").into(),
        created_at: rfc3339(crate::signed::host::system_clock_ms() / 1000),
        files,
    };
    let new_pin = match (pin, existing_pin) {
        (PinMode::Leave, _) => None,
        (PinMode::KeepLower, Some(existing)) if existing <= head.height => None,
        _ => Some(head.height),
    };
    // Keeping more is safe before the snapshot exists; letting bodies of an
    // older snapshot go waits until this one is complete.
    let keeps_more = existing_pin.is_none_or(|existing| existing > head.height);
    if let Some(height) = new_pin.filter(|_| keeps_more) {
        write_pin(run_dir, height)?;
    }
    let mut text = serde_json::to_vec_pretty(&manifest).map_err(|_| ErrorCode::InvalidRequest)?;
    text.push(b'\n');
    write_atomic(&out.join(MANIFEST_FILE), &text)?;
    partial.keep = true;
    if let Some(height) = new_pin.filter(|_| !keeps_more) {
        write_pin(run_dir, height)?;
    }
    let pin = read_pin(run_dir)?;
    drop(locked);
    Ok(Created {
        manifest,
        pin,
        seconds: started.elapsed().as_secs_f64(),
    })
}

/// The files of a snapshot that is being written into the empty directory
/// `out`; removed again unless the snapshot was completed.
struct Partial<'a> {
    out: &'a Path,
    made_directory: bool,
    keep: bool,
}

impl Drop for Partial<'_> {
    fn drop(&mut self) {
        if self.keep {
            return;
        }
        for name in DATABASE_FILES.iter().chain(&PUBLIC_FILES) {
            let _ = std::fs::remove_file(self.out.join(name));
        }
        let _ = std::fs::remove_file(self.out.join(format!(".{MANIFEST_FILE}.pending")));
        if self.made_directory {
            let _ = std::fs::remove_dir(self.out);
        }
    }
}

/// `YYYY-MM-DDThh:mm:ssZ` of a Unix time in seconds.
fn rfc3339(unix_seconds: u64) -> String {
    let (days, rest) = (unix_seconds / 86_400, unix_seconds % 86_400);
    // Civil date of a day count since 1970-01-01 (Howard Hinnant's
    // `civil_from_days`), for dates from 1970 on.
    let z = days + 719_468;
    let (era, day_of_era) = (z / 146_097, z % 146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + u64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3_600,
        rest % 3_600 / 60,
        rest % 60
    )
}

/// The chain a node is told to follow: the pins of `full`.
#[derive(Clone, Debug)]
pub struct Pins {
    pub genesis_hash: B256,
    pub proposer: [u8; 20],
    pub chain_id: u64,
    /// 32 hex digits, without a prefix.
    pub run_id: String,
    pub source_id: String,
}

impl Pins {
    pub fn identity(&self) -> Identity {
        Identity {
            source_id: self.source_id.clone(),
            run_id: self.run_id.clone(),
            genesis_hash: hex(self.genesis_hash.as_slice()),
            chain_id: self.chain_id.to_string(),
        }
    }
}

/// How [`Snapshot::install`] brings the database files into the run
/// directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallMode {
    /// Copy them, computing the SHA-256 in the same pass; a copy that does
    /// not match is removed. The snapshot directory is left as it is.
    Copy,
    /// Check the SHA-256 of each first, then rename them into the run
    /// directory (same file system). The snapshot directory is consumed.
    Move,
}

/// A finished [`Snapshot::install`].
#[derive(Clone, Debug)]
pub struct Installed {
    pub head: Head,
    /// The garbage-collection state rebuilt by `verify_state`, when the
    /// state file collects garbage.
    pub rebuilt_gc: Option<Json>,
    pub verified_state: bool,
    pub seconds: f64,
}

/// A snapshot directory whose description and public files match the pins.
/// The database files are not read yet.
pub struct Snapshot {
    pub dir: PathBuf,
    pub manifest: Manifest,
    genesis: SignedGenesis,
    trust: GenesisResponse,
    public: [Vec<u8>; 2],
}

impl Snapshot {
    /// Reads `dir/snapshot.json` and the public files and checks them
    /// against `pins`: format version, chain ID, run ID, source ID, genesis
    /// hash and proposer in the description; then `genesis.bin` and
    /// `certificate.bin` themselves, as a follower checks what its peer
    /// answers (the genesis hashes to the pinned hash and the certificate
    /// of the genesis header is the pinned proposer's). The database files
    /// must exist with the described sizes.
    pub fn load(dir: &Path, pins: &Pins) -> Result<Self> {
        let path = dir.join(MANIFEST_FILE);
        if !path.is_file() {
            return Err(refuse(
                ErrorCode::InvalidRequest,
                format_args!("{}: not a snapshot (no {MANIFEST_FILE})", dir.display()),
            ));
        }
        let manifest: Manifest = serde_json::from_slice(&read_capped(&path, MANIFEST_CAP)?)
            .map_err(|error| {
                refuse(
                    ErrorCode::InvalidRequest,
                    format_args!("{}: {error}", path.display()),
                )
            })?;
        if manifest.format_version != FORMAT_VERSION {
            return Err(refuse(
                ErrorCode::UnsupportedVersion,
                format_args!(
                    "snapshot format {}, this build reads {FORMAT_VERSION}",
                    manifest.format_version
                ),
            ));
        }
        let identity = pins.identity();
        for (field, found, pinned) in [
            ("chainId", &manifest.chain_id, &identity.chain_id),
            ("runId", &manifest.run_id, &identity.run_id),
            ("sourceId", &manifest.source_id, &identity.source_id),
            (
                "genesisHash",
                &manifest.genesis_hash,
                &identity.genesis_hash,
            ),
            ("proposer", &manifest.proposer, &hex(&pins.proposer)),
        ] {
            if found != pinned {
                return Err(refuse(
                    ErrorCode::IdentityMismatch,
                    format_args!("snapshot {field} is {found}, pinned {pinned}"),
                ));
            }
        }
        let expected: Vec<&str> = DATABASE_FILES
            .iter()
            .chain(&PUBLIC_FILES)
            .copied()
            .collect();
        if manifest.files.len() != expected.len()
            || manifest
                .files
                .iter()
                .zip(&expected)
                .any(|(file, name)| file.name != *name)
        {
            return Err(refuse(
                ErrorCode::InvalidRequest,
                format_args!("{}: unexpected file list", path.display()),
            ));
        }
        manifest.height()?;
        manifest.body_floor()?;
        let mut public = [Vec::new(), Vec::new()];
        for (bytes, name) in public.iter_mut().zip(PUBLIC_FILES) {
            *bytes = read_capped(&dir.join(name), GENESIS_CAP)?;
            if entry_of(name, bytes) != *manifest.file(name)? {
                return Err(mismatch(name));
            }
        }
        let trust = GenesisResponse {
            identity,
            genesis_bytes: hex(&public[0]),
            certificate_bytes: hex(&public[1]),
        };
        // The check a follower makes of its peer's genesis answer.
        let (genesis, certificate): (SignedGenesis, SignedHeader) =
            trust.validate(pins.genesis_hash, pins.proposer, pins.chain_id)?;
        if encode(&genesis) != public[0] || encode(&certificate) != public[1] {
            return Err(mismatch("genesis encoding"));
        }
        for name in DATABASE_FILES {
            let file = dir.join(name);
            let found = std::fs::metadata(&file).map_err(io_error(&file))?.len();
            if found != manifest.file(name)?.bytes {
                return Err(refuse(
                    ErrorCode::StorageCorrupt,
                    format_args!(
                        "{}: {found} bytes, {MANIFEST_FILE} says {}",
                        file.display(),
                        manifest.file(name)?.bytes
                    ),
                ));
            }
        }
        Ok(Self {
            dir: dir.into(),
            manifest,
            genesis,
            trust,
            public,
        })
    }

    /// Installs the snapshot as the run directory `run_dir`, which must be
    /// empty or absent, and opens it as a `full` node does.
    ///
    /// The database files are copied or moved as `mode` says; either way
    /// each must have the described SHA-256. `genesis.bin`,
    /// `certificate.bin` and `trust.json` are written as a follower retains
    /// them, so the node needs its peer for blocks only. The files are then
    /// opened with the pinned genesis: the head record must carry a
    /// certificate of the pinned proposer and the head manifest its state
    /// root, the node identity must be the pinned one, and the head must be
    /// the described one. On any failure the run directory is emptied again
    /// (moved files go back).
    ///
    /// With `verify_state`, for a snapshot from a source that is not
    /// trusted: every object the head state reaches is read and checked
    /// (content hash, trie structure, entry counts, namespace association,
    /// row admission, object and host value bounds), with the walk genesis
    /// adoption uses, and the garbage-collection counts are rebuilt from
    /// the objects (a state file that collects garbage; an archive has no
    /// counts). Not covered: the states of the heights before the head that
    /// the window still serves (their objects are checked by hash when
    /// read), the bodies (checked against the proposer's certificates when
    /// read), and the feed and inclusion records of the heights still in
    /// the record window, which nothing binds: from an untrusted source
    /// they can be wrong until those heights expire.
    ///
    /// The run directory gets no pin and no `prefill.bin`.
    pub fn install(
        &self,
        run_dir: &Path,
        mode: InstallMode,
        verify_state: bool,
    ) -> Result<Installed> {
        let started = Instant::now();
        if occupied(run_dir)? {
            return Err(refuse(
                ErrorCode::InvalidRequest,
                format_args!(
                    "{}: not empty; a snapshot is installed into a new run directory",
                    run_dir.display()
                ),
            ));
        }
        std::fs::create_dir_all(run_dir).map_err(io_error(run_dir))?;
        if mode == InstallMode::Copy {
            require_space(run_dir, self.manifest.total_bytes())?;
        }
        let mut placed = Placed {
            run_dir,
            from: &self.dir,
            created: Vec::new(),
            moved: Vec::new(),
            keep: false,
        };
        self.place(mode, &mut placed)?;
        let (head, rebuilt_gc) = self.check(run_dir, verify_state)?;
        placed.keep = true;
        Ok(Installed {
            head,
            rebuilt_gc,
            verified_state: verify_state,
            seconds: started.elapsed().as_secs_f64(),
        })
    }

    fn place<'a>(&'a self, mode: InstallMode, placed: &mut Placed<'a>) -> Result<()> {
        let run_dir = placed.run_dir;
        match mode {
            InstallMode::Copy => {
                // Under another name until its hash is known: nothing
                // called `node.redb` is ever a partial or unchecked copy.
                let partial = |name: &str| run_dir.join(format!("{name}.installing"));
                let mut progress = Progress::new("copy");
                for name in DATABASE_FILES {
                    let (source, target) = (self.dir.join(name), partial(name));
                    placed.created.push(target.clone());
                    let input = File::open(&source).map_err(io_error(&source))?;
                    let copied = hash_file(&input, &source, Some(&target), name, &mut progress)?;
                    if copied != *self.manifest.file(name)? {
                        return Err(mismatch(name));
                    }
                }
                for name in DATABASE_FILES {
                    let target = run_dir.join(name);
                    placed.created.push(target.clone());
                    std::fs::rename(partial(name), &target).map_err(io_error(&target))?;
                }
            }
            InstallMode::Move => {
                let mut progress = Progress::new("hash");
                for name in DATABASE_FILES {
                    let source = self.dir.join(name);
                    let input = File::open(&source).map_err(io_error(&source))?;
                    let found = hash_file(&input, &source, None, name, &mut progress)?;
                    if found != *self.manifest.file(name)? {
                        return Err(mismatch(name));
                    }
                }
                for name in DATABASE_FILES {
                    let (source, target) = (self.dir.join(name), run_dir.join(name));
                    std::fs::rename(&source, &target).map_err(|error| {
                        refuse(
                            ErrorCode::StorageUnavailable,
                            format_args!(
                                "{} -> {}: {error} (--move needs one file system)",
                                source.display(),
                                target.display()
                            ),
                        )
                    })?;
                    placed.moved.push(name);
                }
            }
        }
        let trust = serde_json::to_vec(&self.trust).map_err(|_| ErrorCode::InvalidRequest)?;
        for (name, bytes) in [
            (PUBLIC_FILES[0], &self.public[0]),
            (PUBLIC_FILES[1], &self.public[1]),
            ("trust.json", &trust),
        ] {
            let path = run_dir.join(name);
            placed.created.push(path.clone());
            File::create(&path)
                .and_then(|mut file| {
                    file.write_all(bytes)?;
                    file.sync_all()
                })
                .map_err(io_error(&path))?;
        }
        sync_dir(run_dir)
    }

    /// Opens the installed files as `full` does and compares the head with
    /// the description; with `verify_state` also walks the head state and
    /// rebuilds the garbage-collection counts.
    fn check(&self, run_dir: &Path, verify_state: bool) -> Result<(Head, Option<Json>)> {
        let open = || -> Result<Stopped> {
            let node = Stopped::open(run_dir, Some(&self.genesis))?;
            if node.host.identity != self.trust.identity {
                return Err(refuse(
                    ErrorCode::IdentityMismatch,
                    "the node database is of another source or run",
                ));
            }
            let head = node.head();
            if head.height != self.manifest.height()?
                || hex(head.hash.as_slice()) != self.manifest.header_hash
                || hex(head.state_root.as_slice()) != self.manifest.state_root
            {
                return Err(refuse(
                    ErrorCode::StorageCorrupt,
                    format_args!("the files do not have the head {MANIFEST_FILE} describes"),
                ));
            }
            Ok(node)
        };
        let node = open()?;
        let head = node.head();
        if !verify_state {
            return Ok((head, None));
        }
        let walking = Instant::now();
        event(json!({"event": "snapshot_verify", "stage": "walking", "head": head.json()}));
        node.host
            .engine
            .verify_head_state(crate::genesis_adopt::verify_threads())?;
        event(json!({
            "event": "snapshot_verify", "stage": "walked",
            "seconds": walking.elapsed().as_secs_f64(),
        }));
        if !node.host.engine.collects_garbage() {
            return Ok((head, None));
        }
        drop(node);
        let counting = Instant::now();
        let spill = run_dir.join("gc-seed");
        let _ = std::fs::remove_dir_all(&spill);
        let state = {
            let store = open_store(&run_dir.join(DATABASE_FILES[0]))?;
            arkiv_db::durable::gc::enable(store.as_ref(), Some(&spill))
        };
        let _ = std::fs::remove_dir_all(&spill);
        let state = state?;
        let report = json!({
            "storedObjects": state.stored_objects,
            "storedBytes": state.stored_bytes,
            "liveObjects": state.live_objects(),
            "deadObjects": state.dead_objects,
            "sharedRows": state.shared,
            "seconds": counting.elapsed().as_secs_f64(),
        });
        event(json!({"event": "snapshot_verify", "stage": "counted", "gc": report}));
        // The rebuilt file is the one the node will open.
        Ok((open()?.head(), Some(report)))
    }
}

fn mismatch(name: &str) -> Error {
    refuse(
        ErrorCode::StorageCorrupt,
        format_args!("{name}: does not match {MANIFEST_FILE} (size or SHA-256)"),
    )
}

/// What an install put into the run directory; undone unless `keep`.
struct Placed<'a> {
    run_dir: &'a Path,
    from: &'a Path,
    created: Vec<PathBuf>,
    moved: Vec<&'static str>,
    keep: bool,
}

impl Drop for Placed<'_> {
    fn drop(&mut self) {
        if self.keep {
            return;
        }
        for name in &self.moved {
            let _ = std::fs::rename(self.run_dir.join(name), self.from.join(name));
        }
        for path in &self.created {
            let _ = std::fs::remove_file(path);
        }
        let _ = std::fs::remove_dir_all(self.run_dir.join("gc-seed"));
    }
}

/// What a peer answered when asked whether it can continue a snapshot.
#[derive(Clone, Debug)]
pub struct PeerCheck {
    pub head: u64,
    /// The peer's `bodyFloor`.
    pub body_floor: u64,
    pub role: String,
    /// The peer is behind the snapshot: it has no block after it yet, and
    /// its block at the snapshot height could not be compared.
    pub behind: bool,
}

impl PeerCheck {
    pub fn json(&self) -> Json {
        json!({
            "role": self.role,
            "head": self.head.to_string(),
            "bodyFloor": self.body_floor.to_string(),
            "behind": self.behind,
        })
    }
}

/// Asks `peer` whether a node installed from `manifest` can sync from it,
/// and refuses if not: the peer must be a producer or full node of the
/// same chain (identity and proposer) and must still hold the snapshot's
/// block, which proves the new node's head to it, and every body after it.
/// That is `height >= bodyFloor` in the peer's status; the peer is then
/// also asked for the header after the snapshot's, which fails on a
/// different block at that height. The peer URL policy
/// ([`runtime::allow_http_peers`]) applies.
pub async fn check_peer(
    client: &reqwest::Client,
    peer: &str,
    manifest: &Manifest,
) -> Result<PeerCheck> {
    let status: Json =
        runtime::fetch(client, peer, "/signed/v1/status", None, None, RESPONSE_CAP).await?;
    let text = |field: &str| status[field].as_str().unwrap_or_default().to_owned();
    if serde_json::from_value::<Identity>(status["identity"].clone()).ok()
        != Some(manifest.identity())
        || text("proposer") != manifest.proposer
    {
        return Err(refuse(
            ErrorCode::IdentityMismatch,
            format_args!("{peer} is not a node of the snapshot's chain"),
        ));
    }
    let role = text("role");
    let (Some(head), Some(body_floor)) = (
        status["head"]["height"]
            .as_str()
            .and_then(crate::text::decimal),
        status["bodyFloor"].as_str().and_then(crate::text::decimal),
    ) else {
        return Err(refuse(
            ErrorCode::FullNodeRequired,
            format_args!("{peer} (role {role}) serves no block bodies"),
        ));
    };
    let height = manifest.height()?;
    if height < body_floor {
        return Err(refuse(
            ErrorCode::HistoryPruned,
            format_args!(
                "{peer} keeps block bodies from height {body_floor}: it cannot continue a \
                 snapshot at height {height}; install a newer snapshot, or sync from the node \
                 that pinned this one"
            ),
        ));
    }
    let behind = head < height;
    if !behind {
        let probe: Result<Json> = runtime::fetch(
            client,
            peer,
            &format!(
                "/signed/v1/headers?afterHeight={height}&afterHash={}&limit=1",
                manifest.header_hash
            ),
            None,
            None,
            RESPONSE_CAP,
        )
        .await;
        if let Err(error) = probe {
            return Err(refuse(
                error.code,
                format_args!(
                    "{peer} does not continue the snapshot's block {height} ({})",
                    error.code
                ),
            ));
        }
    }
    Ok(PeerCheck {
        head,
        body_floor,
        role,
        behind,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The lock a copy is taken under and redb's own exclude each other,
    /// both ways.
    #[test]
    fn a_locked_closed_file_cannot_be_opened_as_a_store() {
        let dir = std::env::temp_dir().join(format!("arkiv-snapshot-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("node.redb");
        let open = || RedbStore::open(&path, RedbConfig::default());
        drop(RedbStore::create(&path, RedbConfig::default()).unwrap());
        let locked = lock_closed(&path).unwrap();
        assert_eq!(open().err().unwrap().kind, StorageErrorKind::Locked);
        drop(locked);
        let store = open().unwrap();
        let refused = lock_closed(&path).unwrap_err();
        assert_eq!(refused.code, ErrorCode::StorageUnavailable);
        assert!(refused.cause.unwrap().contains("a node is running"));
        drop(store);
        lock_closed(&path).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A snapshot that was not completed leaves nothing behind, and a
    /// directory the command did not make stays.
    #[test]
    fn an_abandoned_snapshot_is_removed() {
        let dir = std::env::temp_dir().join(format!("arkiv-snapshot-part-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (made_directory, keep) in [(true, false), (false, false), (true, true)] {
            std::fs::create_dir_all(&dir).unwrap();
            for name in ["node.redb", "genesis.bin", ".snapshot.json.pending"] {
                std::fs::write(dir.join(name), b"partial").unwrap();
            }
            drop(Partial {
                out: &dir,
                made_directory,
                keep,
            });
            assert_eq!(dir.exists(), !made_directory || keep);
            if dir.exists() {
                let left = std::fs::read_dir(&dir).unwrap().count();
                assert_eq!(left, if keep { 3 } else { 0 });
                std::fs::remove_dir_all(&dir).unwrap();
            }
        }
    }

    #[test]
    fn creation_time_is_rfc_3339() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(1_795_564_799), "2026-11-24T23:59:59Z");
        assert_eq!(rfc3339(4_102_444_800), "2100-01-01T00:00:00Z");
    }
}
