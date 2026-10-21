//! Opt-in installation of a streaming genesis by adopting its private bulk
//! store as the node database (`producer --genesis-install adopt`).
//!
//! The default installation copies every reachable object of the staged
//! store at `prefill-work/bulk-v1/complete/state.redb` into a separate
//! `node.redb`. Adoption reads and verifies the same objects without writing
//! them, relabels the staged store in place
//! ([`Host::adopt_staged`](crate::signed::host::Host::adopt_staged)), and then
//! renames its file over the fresh, headless `node.redb`.
//!
//! Crash safety. The relabelling is one durable store commit, and the rename
//! happens only after it returns and after
//! [`Host::check_adopted`](crate::signed::host::Host::check_adopted) has
//! accepted the result; the rename and both directories are synced. A cut
//! therefore leaves one of three states, each resumed by the next start:
//!
//! - the staged store is still engine-only ([`Staged::Engine`]): reuse,
//!   verification and relabelling run again;
//! - the staged store is relabelled but `node.redb` is still headless
//!   ([`Staged::Relabelled`]): only the check and the rename run again;
//! - `node.redb` is the adopted database: the node opens it normally.
//!
//! `node.redb` never names a partially relabelled store: it is the headless
//! file until one atomic rename replaces it with a complete database.
//! Adoption consumes the staged store, so an interrupted adoption must be
//! resumed with adoption; the copy installation refuses a relabelled store.
use crate::genesis_streaming::check_disk;
use arkiv_db::durable::{DurableEngine, simulator::StorageProfile};
use arkiv_db_redb::{RedbConfig, RedbStore};
use arkiv_db_storage::versioned::VersionedStore;
use std::{
    fs::{self, File},
    path::{Path, PathBuf},
};

type Result<T> = std::result::Result<T, String>;
fn err(e: impl std::fmt::Display) -> String {
    format!("genesis adoption: {e}")
}

/// The staged store inside a run directory's import scratch (`prefill-work`).
pub const STAGED_STATE: &str = "bulk-v1/complete/state.redb";

/// Workers for the verification walk: it is bound by random object reads,
/// which a solid-state disk serves in parallel. Memory stays bounded.
pub fn verify_threads() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get().min(8))
}

pub fn staged_path(work_dir: &Path) -> PathBuf {
    work_dir.join(STAGED_STATE)
}

/// What a run directory's staged store is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Staged {
    /// Not built yet, or already moved into place.
    Absent,
    /// An engine-only bulk store: verify and relabel it.
    Engine,
    /// Relabelled by an interrupted adoption: check and move it.
    Relabelled,
}

/// Reads only the staged store's profile record.
pub fn inspect(staged: &Path) -> Result<Staged> {
    match fs::symlink_metadata(staged) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Staged::Absent),
        Err(e) => return Err(err(e)),
        Ok(m) if !m.file_type().is_file() => {
            return Err(err("staged store is not a regular file"));
        }
        Ok(_) => {}
    }
    let store = RedbStore::open(staged, RedbConfig::default()).map_err(err)?;
    match DurableEngine::storage_profile(&store).map_err(err)? {
        Some(StorageProfile::Engine) => Ok(Staged::Engine),
        Some(StorageProfile::Simulator) => Ok(Staged::Relabelled),
        None => Err(err("staged store has no published profile")),
    }
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

/// Refuse before relabelling if the staged store could not later be renamed
/// to `node`: a relabelled store cannot be installed by the copy path.
pub fn check_rename(staged: &Path, node: &Path) -> Result<()> {
    let id = std::process::id();
    let from = parent(staged).join(format!(".adopt-probe-{id}"));
    let to = parent(node).join(format!(".adopt-probe-{id}.to"));
    for stale in [&from, &to] {
        if fs::symlink_metadata(stale).is_ok() {
            fs::remove_file(stale).map_err(err)?;
        }
    }
    File::create(&from).map_err(err)?;
    let probe = fs::rename(&from, &to);
    fs::remove_file(if probe.is_ok() { &to } else { &from }).map_err(err)?;
    probe.map_err(|e| {
        err(format!(
            "the staged store must be renameable to {} (same filesystem and mount): {e}",
            node.display()
        ))
    })?;
    // Only a few pages are rewritten in place; keep the operator's reserve.
    check_disk(parent(node), 0)
}

/// Move a relabelled staged store over the node database. `held` is this
/// process's open handle on the headless database at `node`; holding it
/// keeps other node processes from opening that name until the rename.
pub fn publish(staged: &Path, node: &Path, held: &dyn VersionedStore) -> Result<()> {
    if held.head().map_err(err)?.is_some() {
        return Err(err("node database is already initialized"));
    }
    // An atomic replacement: `node` names the headless file or the complete
    // adopted database, never neither and never a partial one.
    fs::rename(staged, node).map_err(err)?;
    sync_dir(parent(node))?;
    sync_dir(parent(staged))
}
