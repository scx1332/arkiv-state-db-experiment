//! Build-once handoff of completed private bulk genesis state.
//!
//! `genesis stream-build --state-out DIR` keeps the private bulk store it built
//! to derive the bundle's signed commitment, instead of deleting it.
//! `producer --prebuilt-state DIR` moves that store into its own import scratch,
//! where the existing completed-state path reuses it instead of running the
//! normalize and index phases again.
//!
//! Trust: a prebuilt state directory must come from the operator's own
//! `stream-build --state-out` run for exactly this bundle. That build is the
//! derivation the bundle's state commitment came from. The producer does not
//! derive the state from the stream again. Its identity and receipt digests are
//! unkeyed, so they detect mix-ups and corruption, not a forger who also controls
//! `genesis.bin`. Before any use, reuse still audits the canonical stream against
//! the genesis, checks the store's import identity and checksummed receipt, and
//! requires the store's state header, rent and storage units to equal the signed
//! genesis. Installation into the node database then reads and hash-verifies every
//! object reachable from that committed root. The checks in this module only
//! refuse a store for another bundle before it is moved. Full followers and
//! `genesis stream-verify` keep deriving the state independently.
use crate::{
    genesis_streaming::{StreamSummary, check_disk, import_identity},
    signed::protocol::SignedGenesis,
};
use arkiv_db::B256;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

type Result<T> = std::result::Result<T, String>;
fn err(e: impl std::fmt::Display) -> String {
    format!("prebuilt genesis state: {e}")
}

// Layout of `genesis_bulk::build_file`'s private work directory. A prebuilt
// state directory is exactly a completed `bulk-v1` directory plus MANIFEST.
const BULK: &str = "bulk-v1";
const COMPLETE: &str = "complete";
const IDENTITY: &str = "IDENTITY";
const PENDING: &str = "pending";
const STORE_FILES: [&str; 3] = ["state.redb", "genesis.bin", "complete.json"];
/// Binds a prebuilt state directory to one signed genesis.
pub const MANIFEST: &str = "prebuilt.json";
const INSTALL_PENDING: &str = "bulk-v1.install-pending";

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Manifest {
    version: u32,
    genesis_hash: String,
    state_root: String,
    stream_hash: String,
    row_count: String,
    import_identity: String,
}

/// How [`install`] made the prebuilt store available to the producer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Installed {
    /// Renamed into the run directory: no data copied; the source is consumed.
    Moved,
    /// Copied across filesystems; the source is left in place.
    Copied,
    /// Completed private state was already selected in the run directory,
    /// for example after an interrupted earlier start.
    Existing,
}
impl Installed {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Moved => "moved",
            Self::Copied => "copied",
            Self::Existing => "existing",
        }
    }
}

fn expected(g: &SignedGenesis) -> Result<(Manifest, B256)> {
    let p = g
        .prefill
        .as_ref()
        .filter(|p| p.is_streaming())
        .ok_or("prebuilt genesis state requires a streaming prefill")?;
    // The same identity `genesis_bulk::build_file` derives from the audited
    // stream, whose summary must equal these signed launch fields.
    let identity = import_identity(
        g,
        p.limits,
        p.sponsor,
        &StreamSummary {
            rows: p.row_count,
            bytes: p.canonical_bytes,
            hash: p.stream_hash,
        },
        p.permanent_prefill(),
    );
    let manifest = Manifest {
        version: 1,
        genesis_hash: g.hash().to_string(),
        state_root: p.state.root().to_string(),
        stream_hash: p.stream_hash.to_string(),
        row_count: p.row_count.to_string(),
        import_identity: identity.to_string(),
    };
    Ok((manifest, identity))
}

fn read_bounded(path: &Path, cap: u64) -> Result<Vec<u8>> {
    let file = File::open(path).map_err(|e| err(format!("{}: {e}", path.display())))?;
    let mut bytes = Vec::new();
    file.take(cap + 1).read_to_end(&mut bytes).map_err(err)?;
    if bytes.len() as u64 > cap {
        return Err(err(format!("{} exceeds {cap} bytes", path.display())));
    }
    Ok(bytes)
}
fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(err)?;
    file.write_all(bytes).map_err(err)?;
    file.sync_all().map_err(err)
}
fn sync_dir(path: &Path) -> Result<()> {
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(err)
}
fn parent(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}
fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}
fn is_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_dir())
}
fn is_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_file())
}
fn rename_noreplace(from: &Path, to: &Path) -> rustix::io::Result<()> {
    rustix::fs::renameat_with(
        rustix::fs::CWD,
        from,
        rustix::fs::CWD,
        to,
        rustix::fs::RenameFlags::NOREPLACE,
    )
}

/// A completed bulk directory bound to `g`. Symlinks are refused so that a
/// rename moves the data itself and scratch removal cannot follow a link.
fn check_store(dir: &Path, g: &SignedGenesis, manifest: bool) -> Result<()> {
    let (expected, identity) = expected(g)?;
    if !is_dir(dir) {
        return Err(err(format!(
            "{} is missing or not a directory",
            dir.display()
        )));
    }
    if read_bounded(&dir.join(IDENTITY), 32)? != identity.as_slice() {
        return Err(err("store import identity does not match this bundle"));
    }
    if manifest {
        let found: Manifest = serde_json::from_slice(&read_bounded(&dir.join(MANIFEST), 4096)?)
            .map_err(|e| err(format!("invalid {MANIFEST}: {e}")))?;
        if found != expected {
            return Err(err(format!(
                "{MANIFEST} does not match this bundle's genesis"
            )));
        }
    }
    if exists(&dir.join(PENDING)) || !is_dir(&dir.join(COMPLETE)) {
        return Err(err("store is not a completed bulk build"));
    }
    for name in STORE_FILES {
        if !is_file(&dir.join(COMPLETE).join(name)) {
            return Err(err(format!("completed store lacks regular file {name}")));
        }
    }
    Ok(())
}

/// Validate a `--state-out` destination before the stream audit and build
/// start, so a bad path fails in seconds rather than hours. Publication is an
/// atomic rename, so the destination must share the filesystem of the bundle
/// work directory. Returns the absolute destination path.
pub(crate) fn check_destination(state_out: &Path, bundle: &Path) -> Result<PathBuf> {
    let name = state_out
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| *n != "." && *n != "..")
        .ok_or_else(|| err("invalid state destination name"))?;
    let bundle_name = bundle
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| err("invalid bundle output name"))?;
    // The bundle builder creates this parent next anyway. Creating it first
    // means the checks below see the final layout, including when the state
    // destination is (or contains) a not-yet-created bundle ancestor.
    fs::create_dir_all(parent(bundle)).map_err(err)?;
    let bundle_parent = fs::canonicalize(parent(bundle)).map_err(err)?;
    let parent_dir = fs::canonicalize(parent(state_out))
        .map_err(|e| err(format!("state destination parent must exist: {e}")))?;
    let target = parent_dir.join(name);
    if exists(&target) {
        return Err(err("state destination already exists"));
    }
    let work = bundle_parent.join(format!(".{bundle_name}.stream-work"));
    if target.starts_with(bundle_parent.join(bundle_name))
        || target.starts_with(&work)
        || bundle_parent.starts_with(&target)
    {
        return Err(err(
            "state destination overlaps the bundle or its work directory",
        ));
    }
    // Probe the exact rename publication performs, from the work directory's
    // parent to the destination's. Separate bind mounts of one filesystem share
    // a device number but still refuse that rename with EXDEV.
    let id = std::process::id();
    let from = bundle_parent.join(format!(".{bundle_name}.state-probe-{id}"));
    let to = parent_dir.join(format!(".{name}.state-probe-{id}.to"));
    for stale in [&from, &to] {
        if is_dir(stale) {
            fs::remove_dir(stale).map_err(err)?;
        }
    }
    fs::create_dir(&from).map_err(err)?;
    let probe = rename_noreplace(&from, &to);
    fs::remove_dir(if probe.is_ok() { &to } else { &from }).map_err(err)?;
    match probe {
        Ok(()) => Ok(target),
        Err(rustix::io::Errno::XDEV) => Err(err(
            "state destination must be on the bundle output's filesystem and mount (publication is a rename)",
        )),
        Err(e) => Err(err(format!("state destination rename probe: {e}"))),
    }
}

/// Bind `work`'s completed bulk store to the bundle genesis `g` before the
/// bundle is published. If a later step fails, the store left in the work
/// directory is already a complete prebuilt state directory.
pub(crate) fn bind(g: &SignedGenesis, work: &Path) -> Result<()> {
    let bulk = work.join(BULK);
    check_store(&bulk, g, false)?;
    let (manifest, _) = expected(g)?;
    let path = bulk.join(MANIFEST);
    // A manifest from an earlier attempt is unselected private scratch.
    if exists(&path) {
        fs::remove_file(&path).map_err(err)?;
    }
    write_new(&path, &serde_json::to_vec_pretty(&manifest).map_err(err)?)?;
    sync_dir(&bulk)
}

/// Publish the bound store as a prebuilt state directory. It appears only by an
/// atomic no-replace rename of synced contents.
pub(crate) fn publish(g: &SignedGenesis, work: &Path, target: &Path) -> Result<()> {
    let bulk = work.join(BULK);
    check_store(&bulk, g, true)?;
    rename_noreplace(&bulk, target).map_err(|e| {
        err(format!(
            "publication rename: {e}; the completed state remains in {}",
            bulk.display()
        ))
    })?;
    sync_dir(parent(target))?;
    sync_dir(work)
}

/// Make the prebuilt store at `source` the producer's completed private bulk
/// state under `work_dir` (the run directory's `prefill-work`). Call this only
/// for a fresh, headless node whose genesis is `g`, while holding the node
/// database lock. The source is renamed when possible and copied otherwise.
///
/// Either step is restartable. A rename is atomic. A copy is built in an
/// unselected directory, synced, and selected by a no-replace rename; an
/// interrupted copy is discarded and repeated. Afterwards the caller's normal
/// staged-genesis path verifies the store before installing it.
pub fn install(g: &SignedGenesis, source: &Path, work_dir: &Path) -> Result<Installed> {
    install_with(g, source, work_dir, false)
}

fn install_with(
    g: &SignedGenesis,
    source: &Path,
    work_dir: &Path,
    copy: bool,
) -> Result<Installed> {
    g.validate().map_err(|e| err(format!("{e:?}")))?;
    let target = work_dir.join(BULK);
    if is_dir(&target.join(COMPLETE)) {
        // Already selected by an earlier start. Reuse verifies its binding.
        return Ok(Installed::Existing);
    }
    check_store(source, g, true)?;
    fs::create_dir_all(work_dir).map_err(err)?;
    sync_dir(parent(work_dir))?;
    if exists(&target) {
        // Unselected bulk scratch: disposable by design, as in build_file.
        fs::remove_dir_all(&target).map_err(err)?;
    }
    let pending = work_dir.join(INSTALL_PENDING);
    if exists(&pending) {
        fs::remove_dir_all(&pending).map_err(err)?;
    }
    sync_dir(work_dir)?;
    if !copy {
        match rename_noreplace(source, &target) {
            Ok(()) => {
                sync_dir(work_dir)?;
                sync_dir(parent(source))?;
                return Ok(Installed::Moved);
            }
            Err(rustix::io::Errno::XDEV) => {}
            Err(e) => return Err(err(format!("install rename: {e}"))),
        }
    }
    copy_store(source, &pending)?;
    rename_noreplace(&pending, &target).map_err(|e| err(format!("install rename: {e}")))?;
    sync_dir(work_dir)?;
    Ok(Installed::Copied)
}

fn copy_store(source: &Path, pending: &Path) -> Result<()> {
    let mut files = vec![PathBuf::from(IDENTITY), PathBuf::from(MANIFEST)];
    files.extend(
        STORE_FILES
            .iter()
            .map(|name| Path::new(COMPLETE).join(name)),
    );
    let mut total = 0u64;
    for file in &files {
        total = total
            .checked_add(fs::metadata(source.join(file)).map_err(err)?.len())
            .ok_or_else(|| err("store size overflow"))?;
    }
    check_disk(parent(pending), total)?;
    fs::create_dir(pending).map_err(err)?;
    fs::create_dir(pending.join(COMPLETE)).map_err(err)?;
    for file in &files {
        let (from, to) = (source.join(file), pending.join(file));
        let expected = fs::metadata(&from).map_err(err)?.len();
        if fs::copy(&from, &to).map_err(err)? != expected {
            return Err(err(format!("{} changed while copying", file.display())));
        }
        File::open(&to).and_then(|f| f.sync_all()).map_err(err)?;
    }
    sync_dir(&pending.join(COMPLETE))?;
    sync_dir(pending)
}

#[cfg(test)]
pub(crate) fn install_by_copy(
    g: &SignedGenesis,
    source: &Path,
    work_dir: &Path,
) -> Result<Installed> {
    install_with(g, source, work_dir, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        accounts,
        genesis_bundle::PrefillEntity,
        genesis_streaming::{
            DEFAULT_BATCH_BYTES, build_bundle, build_bundle_with_state, prepare_streaming,
        },
        signed::protocol::{SignedLimits, default_native_genesis},
    };
    use alloy_primitives::U256;
    use arkiv_db::{CellName, OwnerId, Value, decode_exact, durable::Options, encode};
    use std::os::unix::fs::MetadataExt;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!(
                "arkiv-prebuilt-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn base(run: u8) -> SignedGenesis {
        SignedGenesis {
            genesis: default_native_genesis(9450, 1000),
            run_id: [run; 16],
            proposer: [1; 20],
            block_period_ms: 1000,
            limits: SignedLimits::default(),
            // A base without prefill must fund its proposer's bootstrap.
            accounts: accounts::AccountConfig::new(vec![
                (OwnerId::new([1; 20]), U256::from(10).pow(U256::from(18))),
                (OwnerId::new([2; 20]), U256::from(10).pow(U256::from(30))),
            ])
            .unwrap(),
            prefill: None,
        }
    }
    fn stream(rows: u64) -> Vec<u8> {
        stream_expiring(rows, |n| 100 + n / 4)
    }
    fn stream_expiring(rows: u64, expiry: impl Fn(u64) -> u64) -> Vec<u8> {
        let mut b = b"ARKPFL02".to_vec();
        b.extend_from_slice(&2u32.to_be_bytes());
        b.extend_from_slice(&rows.to_be_bytes());
        for n in 0..rows {
            let f = encode(&PrefillEntity {
                creator: [2; 20],
                create_nonce: n,
                salt: n as u128,
                expires_at_height: expiry(n),
                flags: 0,
                content_type: "text/plain".into(),
                payload: format!("payload {n}").into_bytes(),
                attributes: vec![(CellName::new("label").unwrap(), Value::str("hello"))],
            });
            b.extend_from_slice(&(f.len() as u32).to_be_bytes());
            b.extend(f);
        }
        b
    }
    struct Built {
        bundle: PathBuf,
        state: PathBuf,
        genesis: SignedGenesis,
        report: serde_json::Value,
    }
    fn author(root: &Path, tag: &str, run: u8, rows: u64, keep: bool) -> Built {
        let base_path = root.join(format!("{tag}-base.bin"));
        let input = root.join(format!("{tag}-input.bin"));
        fs::write(&base_path, encode(&base(run))).unwrap();
        fs::write(&input, stream(rows)).unwrap();
        let bundle = root.join(format!("{tag}-bundle"));
        let state = root.join(format!("{tag}-state"));
        let report = if keep {
            build_bundle_with_state(
                &base_path,
                &input,
                &bundle,
                Some(&state),
                [2; 20],
                DEFAULT_BATCH_BYTES,
            )
        } else {
            build_bundle(&base_path, &input, &bundle, [2; 20], DEFAULT_BATCH_BYTES)
        }
        .unwrap();
        assert!(!exists(&root.join(format!(".{tag}-bundle.stream-work"))));
        let genesis = decode_exact(&fs::read(bundle.join("genesis.bin")).unwrap()).unwrap();
        Built {
            bundle,
            state,
            genesis,
            report,
        }
    }
    fn inode(path: &Path) -> u64 {
        fs::metadata(path).unwrap().ino()
    }
    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }
    fn state_of(g: &SignedGenesis) -> arkiv_db::StateHeader {
        g.prefill.as_ref().unwrap().state.clone()
    }

    #[test]
    fn kept_state_is_reused_without_rebuild_and_matches_the_rebuild_path() {
        let tmp = Temp::new();
        let legacy = author(&tmp.0, "legacy", 7, 12, false);
        let once = author(&tmp.0, "once", 7, 12, true);
        // Keeping the state changes neither the bundle nor its commitments.
        assert_eq!(once.report, legacy.report);
        assert_eq!(
            names(&once.bundle),
            ["genesis.bin", "prefill.bin", "report.json"]
        );
        assert_eq!(
            fs::read(once.bundle.join("genesis.bin")).unwrap(),
            fs::read(legacy.bundle.join("genesis.bin")).unwrap()
        );
        assert_eq!(names(&once.state), ["IDENTITY", "LOCK", COMPLETE, MANIFEST]);
        assert_eq!(
            names(&once.state.join(COMPLETE)),
            ["complete.json", "genesis.bin", "state.redb"]
        );
        let built = inode(&once.state.join("complete/state.redb"));

        let work = tmp.0.join("node/prefill-work");
        assert_eq!(
            install(&once.genesis, &once.state, &work).unwrap(),
            Installed::Moved
        );
        assert!(
            !exists(&once.state),
            "a same-filesystem install consumes the source"
        );
        assert_eq!(inode(&work.join("bulk-v1/complete/state.redb")), built);
        let prefill = once.bundle.join("prefill.bin");
        let engine = prepare_streaming(&once.genesis, &prefill, &work, Options::default()).unwrap();
        assert_eq!(
            engine.head_snapshot().state_header(),
            state_of(&once.genesis)
        );
        drop(engine);
        // The very same file was opened: no pending rebuild was ever selected.
        assert_eq!(inode(&work.join("bulk-v1/complete/state.redb")), built);
        assert!(!exists(&work.join("bulk-v1/pending")));
        // A restart after the move finds the selected state and reuses it.
        assert_eq!(
            install(&once.genesis, &once.state, &work).unwrap(),
            Installed::Existing
        );

        // The legacy producer path still rebuilds independently, to the same state.
        let rebuilt = tmp.0.join("legacy-node/prefill-work");
        let engine =
            prepare_streaming(&once.genesis, &prefill, &rebuilt, Options::default()).unwrap();
        assert_eq!(
            engine.head_snapshot().state_header(),
            state_of(&once.genesis)
        );
        assert_ne!(inode(&rebuilt.join("bulk-v1/complete/state.redb")), built);
    }

    #[test]
    fn permanent_launch_state_is_kept_and_reused_without_rebuild() {
        // A permanent-prefill base (version 3) yields a version-4 launch whose
        // bulk store is built under the permanent import identity; the kept
        // state must bind and install under that same identity.
        let tmp = Temp::new();
        let small = crate::genesis_bundle::PrefillLimits {
            max_entities: 256,
            max_canonical_bytes: crate::genesis_bundle::MAX_CANONICAL_BYTES,
            max_storage_units: crate::genesis_bundle::MAX_STORAGE_UNITS,
        };
        let permanent_base =
            crate::genesis_bundle::build_genesis_with(base(7), small, [2; 20], &[], true).unwrap();
        let base_path = tmp.0.join("permanent-base.bin");
        let input = tmp.0.join("permanent-input.bin");
        fs::write(&base_path, encode(&permanent_base)).unwrap();
        fs::write(
            &input,
            stream_expiring(8, |n| {
                if n % 2 == 0 {
                    crate::genesis_bundle::PERMANENT_EXPIRY
                } else {
                    100 + n
                }
            }),
        )
        .unwrap();
        let (bundle, state) = (
            tmp.0.join("permanent-bundle"),
            tmp.0.join("permanent-state"),
        );
        let report = build_bundle_with_state(
            &base_path,
            &input,
            &bundle,
            Some(&state),
            [2; 20],
            DEFAULT_BATCH_BYTES,
        )
        .unwrap();
        assert_eq!(
            (
                report["version"].clone(),
                report["permanentPrefill"].clone()
            ),
            (serde_json::json!(4), serde_json::json!(true))
        );
        let genesis: SignedGenesis =
            decode_exact(&fs::read(bundle.join("genesis.bin")).unwrap()).unwrap();
        let built = inode(&state.join("complete/state.redb"));
        let work = tmp.0.join("node/prefill-work");
        assert_eq!(install(&genesis, &state, &work).unwrap(), Installed::Moved);
        let engine = prepare_streaming(
            &genesis,
            &bundle.join("prefill.bin"),
            &work,
            Options::default(),
        )
        .unwrap();
        assert_eq!(engine.head_snapshot().state_header(), state_of(&genesis));
        drop(engine);
        assert_eq!(inode(&work.join("bulk-v1/complete/state.redb")), built);
        assert!(!exists(&work.join("bulk-v1/pending")));
    }

    #[test]
    fn copy_install_discards_interrupted_copies_and_keeps_the_source() {
        let tmp = Temp::new();
        let once = author(&tmp.0, "copy", 7, 6, true);
        let work = tmp.0.join("node/prefill-work");
        // A cut during an earlier copy, and unselected legacy scratch.
        fs::create_dir_all(work.join(INSTALL_PENDING).join(COMPLETE)).unwrap();
        fs::write(
            work.join(INSTALL_PENDING).join("complete/state.redb"),
            b"partial",
        )
        .unwrap();
        fs::create_dir_all(work.join("bulk-v1/pending/sort")).unwrap();
        assert_eq!(
            install_by_copy(&once.genesis, &once.state, &work).unwrap(),
            Installed::Copied
        );
        assert!(!exists(&work.join(INSTALL_PENDING)));
        assert!(!exists(&work.join("bulk-v1/pending")));
        assert!(
            is_dir(&once.state.join(COMPLETE)),
            "a copy leaves its source in place"
        );
        for name in STORE_FILES {
            let (a, b) = (
                once.state.join(COMPLETE).join(name),
                work.join(BULK).join(COMPLETE).join(name),
            );
            assert_eq!(fs::read(a).unwrap(), fs::read(b).unwrap());
        }
        let engine = prepare_streaming(
            &once.genesis,
            &once.bundle.join("prefill.bin"),
            &work,
            Options::default(),
        )
        .unwrap();
        assert_eq!(
            engine.head_snapshot().state_header(),
            state_of(&once.genesis)
        );
    }

    #[test]
    fn foreign_incomplete_or_linked_state_is_refused_before_moving() {
        let tmp = Temp::new();
        let a = author(&tmp.0, "a", 7, 4, true);
        let b = author(&tmp.0, "b", 8, 4, true);
        let work = tmp.0.join("node/prefill-work");
        let refused = |g: &SignedGenesis, source: &Path, needle: &str| {
            let error = install(g, source, &work).unwrap_err();
            assert!(error.contains(needle), "{error}");
            assert!(!exists(&work.join(BULK)), "nothing was selected: {error}");
        };
        // Another bundle's state, and a missing source.
        refused(&b.genesis, &a.state, "identity does not match");
        refused(
            &a.genesis,
            &tmp.0.join("missing"),
            "missing or not a directory",
        );
        // A manifest naming another genesis.
        let manifest = a.state.join(MANIFEST);
        let original = fs::read(&manifest).unwrap();
        let mut changed: serde_json::Value = serde_json::from_slice(&original).unwrap();
        changed["genesisHash"] = b.report["genesisHash"].clone();
        fs::write(&manifest, serde_json::to_vec(&changed).unwrap()).unwrap();
        refused(&a.genesis, &a.state, "does not match this bundle");
        fs::write(&manifest, b"{}").unwrap();
        refused(&a.genesis, &a.state, "invalid prebuilt.json");
        fs::write(&manifest, &original).unwrap();
        // An incomplete store, and symlinks in place of the store or its files.
        let receipt = a.state.join("complete/complete.json");
        let kept = tmp.0.join("kept-receipt");
        fs::rename(&receipt, &kept).unwrap();
        refused(&a.genesis, &a.state, "lacks regular file complete.json");
        std::os::unix::fs::symlink(&kept, &receipt).unwrap();
        refused(&a.genesis, &a.state, "lacks regular file complete.json");
        fs::remove_file(&receipt).unwrap();
        fs::rename(&kept, &receipt).unwrap();
        fs::create_dir(a.state.join(PENDING)).unwrap();
        refused(&a.genesis, &a.state, "not a completed bulk build");
        fs::remove_dir(a.state.join(PENDING)).unwrap();
        let link = tmp.0.join("link");
        std::os::unix::fs::symlink(&a.state, &link).unwrap();
        refused(&a.genesis, &link, "missing or not a directory");
        // The untouched source still installs.
        assert_eq!(
            install(&a.genesis, &a.state, &work).unwrap(),
            Installed::Moved
        );
    }

    #[test]
    fn corrupt_receipt_after_install_fails_clearly_and_is_never_used() {
        let tmp = Temp::new();
        let once = author(&tmp.0, "receipt", 7, 4, true);
        let receipt = once.state.join("complete/complete.json");
        let mut corrupt: serde_json::Value =
            serde_json::from_slice(&fs::read(&receipt).unwrap()).unwrap();
        corrupt["units"] = serde_json::json!(corrupt["units"].as_u64().unwrap() + 1);
        fs::write(&receipt, serde_json::to_vec(&corrupt).unwrap()).unwrap();
        let work = tmp.0.join("node/prefill-work");
        assert_eq!(
            install(&once.genesis, &once.state, &work).unwrap(),
            Installed::Moved
        );
        let prefill = once.bundle.join("prefill.bin");
        for _ in 0..2 {
            let error = prepare_streaming(&once.genesis, &prefill, &work, Options::default())
                .err()
                .unwrap();
            assert!(error.contains("identity or bounds"), "{error}");
        }
    }

    #[test]
    fn state_destination_is_checked_before_any_work() {
        let tmp = Temp::new();
        let base_path = tmp.0.join("base.bin");
        let input = tmp.0.join("input.bin");
        fs::write(&base_path, encode(&base(7))).unwrap();
        fs::write(&input, stream(2)).unwrap();
        let bundle = tmp.0.join("bundle");
        let existing = tmp.0.join("existing");
        fs::create_dir(&existing).unwrap();
        let work = tmp.0.join(".bundle.stream-work");
        fs::create_dir_all(&work).unwrap();
        for (state, needle) in [
            (existing.clone(), "already exists"),
            (tmp.0.join("no/such/parent"), "parent must exist"),
            (bundle.clone(), "overlaps"),
            (work.join("state"), "overlaps"),
            (tmp.0.join(".."), "invalid state destination name"),
            // A missing bundle ancestor: the builder would create it first.
            (tmp.0.join("x"), "already exists"),
        ] {
            let output = if state == tmp.0.join("x") {
                tmp.0.join("x/bundle")
            } else {
                bundle.clone()
            };
            let error =
                build_bundle_with_state(&base_path, &input, &output, Some(&state), [2; 20], 1024)
                    .unwrap_err();
            assert!(error.contains(needle), "{state:?}: {error}");
            assert!(!exists(&output));
            assert!(names(&work).is_empty(), "no construction started");
            assert!(!names(&tmp.0).iter().any(|n| n.contains("state-probe")));
        }
    }

    #[test]
    fn failed_publication_leaves_a_bound_store_that_still_installs() {
        let tmp = Temp::new();
        let built = author(&tmp.0, "src", 7, 4, false);
        // A producer-style completed build is the same layout stream-build keeps.
        let work = tmp.0.join("work");
        let prefill = built.bundle.join("prefill.bin");
        drop(prepare_streaming(&built.genesis, &prefill, &work, Options::default()).unwrap());
        let other = author(&tmp.0, "other", 8, 4, false);
        assert!(
            bind(&other.genesis, &work)
                .unwrap_err()
                .contains("identity")
        );
        // Unbound stores are never published; rebinding replaces a stale manifest.
        let target = tmp.0.join("state");
        assert!(publish(&built.genesis, &work, &target).is_err());
        fs::write(work.join(BULK).join(MANIFEST), b"stale").unwrap();
        bind(&built.genesis, &work).unwrap();
        fs::create_dir(&target).unwrap();
        let error = publish(&built.genesis, &work, &target).unwrap_err();
        assert!(error.contains("remains in"), "{error}");
        // The stranded store is itself a valid prebuilt state directory.
        let node = tmp.0.join("node/prefill-work");
        assert_eq!(
            install(&built.genesis, &work.join(BULK), &node).unwrap(),
            Installed::Moved
        );
        let engine =
            prepare_streaming(&built.genesis, &prefill, &node, Options::default()).unwrap();
        assert_eq!(
            engine.head_snapshot().state_header(),
            state_of(&built.genesis)
        );
        drop(engine);
        // Normal publication of a bound store.
        let again = tmp.0.join("again");
        drop(prepare_streaming(&built.genesis, &prefill, &again, Options::default()).unwrap());
        bind(&built.genesis, &again).unwrap();
        fs::remove_dir(&target).unwrap();
        publish(&built.genesis, &again, &target).unwrap();
        assert!(!exists(&again.join(BULK)));
        check_store(&target, &built.genesis, true).unwrap();
    }
}
