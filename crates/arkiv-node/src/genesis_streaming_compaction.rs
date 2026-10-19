//! Private import generations. CURRENT changes only after both the replacement
//! database and its genesis description are durable. A leftover next generation
//! is disposable; the currently selected generation is never modified in place.
use arkiv_db::{
    decode_exact,
    durable::{DurableEngine, EngineGenesis, Initialization, Options},
    encode,
};
use arkiv_db_redb::{RedbConfig, RedbStore};
use arkiv_db_storage::versioned::VersionedStore;
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};
type Result<T> = std::result::Result<T, String>;
fn err(e: impl std::fmt::Display) -> String {
    format!("import generation: {e}")
}
/// Import-local caches: two redb handles during copying plus one decoded
/// engine cache have a combined configured ceiling of 768 MiB. Preparation
/// caps remain unchanged. These settings are operational, never canonical.
const IMPORT_CACHE_BYTES: usize = 256 * 1024 * 1024;
const IMPORT_CACHE_ENTRIES: usize = 262_144;
pub fn import_options(mut options: Options) -> Options {
    options.cache_bytes = IMPORT_CACHE_BYTES;
    options.cache_entries = IMPORT_CACHE_ENTRIES;
    options
}
fn import_redb_config() -> RedbConfig {
    RedbConfig::with_cache_size(IMPORT_CACHE_BYTES)
}
fn compaction_progress(
    phase: &str,
    generation: u64,
    source_bytes: u64,
    output_bytes: Option<u64>,
    started: Instant,
) {
    let record = serde_json::json!({
        "event": "prefill_compaction", "phase": phase,
        "generation": generation, "sourceBytes": source_bytes,
        "outputBytes": output_bytes, "elapsedSeconds": started.elapsed().as_secs_f64(),
    });
    let _ = writeln!(std::io::stderr().lock(), "{record}");
}
/// Enforce a configurable free-space reserve before growing an import.
/// `additional_bytes` is a conservative projection, not an exact disk quota.
/// Operational full imports should set ARKIV_PREFILL_RESERVE_GIB=80 or higher.
pub fn check_disk(path: &Path, additional_bytes: u64) -> Result<()> {
    let reserve_gib = match std::env::var("ARKIV_PREFILL_RESERVE_GIB") {
        Ok(value) => value
            .parse::<u64>()
            .map_err(|_| "invalid ARKIV_PREFILL_RESERVE_GIB")?,
        Err(std::env::VarError::NotPresent) => 1,
        Err(_) => return Err("invalid ARKIV_PREFILL_RESERVE_GIB".into()),
    };
    let required = reserve_gib
        .checked_mul(1024 * 1024 * 1024)
        .and_then(|reserve| reserve.checked_add(additional_bytes))
        .ok_or("prefill disk projection overflow")?;
    let mut existing = path;
    while !existing.exists() {
        existing = match existing.parent() {
            Some(parent) if parent.as_os_str().is_empty() => Path::new("."),
            Some(parent) => parent,
            None => return Err("prefill disk check has no existing parent".into()),
        };
    }
    let stats = rustix::fs::statvfs(existing).map_err(err)?;
    let available = u128::from(stats.f_bavail) * u128::from(stats.f_frsize);
    if available < u128::from(required) {
        return Err(format!(
            "insufficient prefill disk space: available={available} required={required} bytes (including reserve)"
        ));
    }
    Ok(())
}
fn db_path(dir: &Path, generation: u64) -> PathBuf {
    dir.join(format!("import-{generation:016x}.redb"))
}
fn genesis_path(dir: &Path, generation: u64) -> PathBuf {
    dir.join(format!("import-{generation:016x}.genesis"))
}
fn size_path(dir: &Path, generation: u64) -> PathBuf {
    dir.join(format!("import-{generation:016x}.size"))
}
fn save_size(dir: &Path, generation: u64) -> Result<()> {
    let size = fs::metadata(db_path(dir, generation)).map_err(err)?.len();
    write_sync(&size_path(dir, generation), &size.to_be_bytes())
}
fn original_size(dir: &Path, generation: u64) -> Result<u64> {
    let bytes = read_bounded(&size_path(dir, generation), 8)?;
    Ok(u64::from_be_bytes(
        bytes
            .try_into()
            .map_err(|_| "invalid import generation size")?,
    ))
}
fn read_bounded(path: &Path, cap: u64) -> Result<Vec<u8>> {
    let file = File::open(path).map_err(err)?;
    if file.metadata().map_err(err)?.len() > cap {
        return Err("import metadata exceeds size cap".into());
    }
    let mut bytes = Vec::new();
    file.take(cap + 1).read_to_end(&mut bytes).map_err(err)?;
    if bytes.len() as u64 > cap {
        return Err("import metadata grew beyond size cap".into());
    }
    Ok(bytes)
}
fn current(dir: &Path) -> Result<Option<u64>> {
    let path = dir.join("CURRENT");
    if !path.exists() {
        return Ok(None);
    }
    let bytes = read_bounded(&path, 32)?;
    let text = std::str::from_utf8(&bytes).map_err(err)?;
    let generation = text.trim_end_matches('\n').parse::<u64>().map_err(err)?;
    if text != format!("{generation}\n") {
        return Err("noncanonical import generation pointer".into());
    }
    Ok(Some(generation))
}
fn write_sync(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)
        .map_err(err)?;
    file.write_all(bytes).map_err(err)?;
    file.sync_all().map_err(err)
}
fn sync_dir(dir: &Path) -> Result<()> {
    File::open(dir).and_then(|f| f.sync_all()).map_err(err)
}
fn select(dir: &Path, generation: u64) -> Result<()> {
    write_sync(
        &dir.join("CURRENT.next"),
        format!("{generation}\n").as_bytes(),
    )?;
    fs::rename(dir.join("CURRENT.next"), dir.join("CURRENT")).map_err(err)?;
    sync_dir(dir)
}
fn compatible(actual: &EngineGenesis, original: &EngineGenesis) -> Result<()> {
    if actual.deployment != original.deployment
        || actual.limits != original.limits
        || actual.pricing != original.pricing
        || actual.max_object_bytes != original.max_object_bytes
    {
        return Err("import generation configuration mismatch".into());
    }
    actual.validate().map_err(err)
}
fn committed(init: Initialization) -> Result<DurableEngine> {
    match init {
        Initialization::Committed(engine) => Ok(engine),
        _ => Err("import generation initialization uncertain; reopen to reconcile".into()),
    }
}
fn remove_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(err(e)),
    }
}
fn remove_generation(dir: &Path, generation: u64) -> Result<()> {
    let path = db_path(dir, generation);
    remove_if_present(&path)?;
    remove_if_present(&arkiv_db_redb::initialization_path(&path))?;
    remove_if_present(&genesis_path(dir, generation))?;
    remove_if_present(&size_path(dir, generation))
}
/// Open the selected durable generation, verifying the expected import identity.
/// Missing CURRENT means initial generation zero has not been selected yet.
pub fn open_stage(dir: &Path, original: EngineGenesis, options: Options) -> Result<DurableEngine> {
    fs::create_dir_all(dir).map_err(err)?;
    let selected = current(dir)?;
    let generation = selected.unwrap_or(0);
    let metadata = genesis_path(dir, generation);
    let genesis = if metadata.exists() {
        decode_exact::<EngineGenesis>(&read_bounded(&metadata, 65536)?).map_err(err)?
    } else {
        if selected.is_some() {
            return Err("selected import generation metadata missing".into());
        }
        // Publish complete metadata without replacing another fresh import's
        // identity, even when two processes race before acquiring the DB lock.
        // A crash before linking leaves only an unselected temporary file.
        let temporary = dir.join(format!(
            "genesis-{}.pending",
            arkiv_db_storage::versioned::DatabaseId::random()
        ));
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .map_err(err)?;
        file.write_all(&encode(&original)).map_err(err)?;
        file.sync_all().map_err(err)?;
        drop(file);
        let published = fs::hard_link(&temporary, &metadata);
        remove_if_present(&temporary)?;
        match published {
            Ok(()) => original.clone(),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                decode_exact::<EngineGenesis>(&read_bounded(&metadata, 65536)?).map_err(err)?
            }
            Err(e) => return Err(err(e)),
        }
    };
    compatible(&genesis, &original)?;
    let path = db_path(dir, generation);
    if selected.is_some() && !path.exists() {
        return Err("selected import generation database missing".into());
    }
    let store = Arc::new(
        if path.exists() {
            RedbStore::open(&path, import_redb_config())
        } else {
            RedbStore::create(&path, import_redb_config())
        }
        .map_err(err)?,
    );
    let engine = if store.head().map_err(err)?.is_some() {
        DurableEngine::open(store, genesis, options).map_err(err)?
    } else {
        if selected.is_some() {
            return Err("selected import generation has no head".into());
        }
        committed(DurableEngine::initialize(store, genesis, options).map_err(err)?)?
    };
    if selected.is_none() {
        save_size(dir, generation)?;
        select(dir, generation)?;
    } else {
        original_size(dir, generation)?;
    }
    // A crash after switching CURRENT may have left the previous generation.
    // Delete only that obsolete, strictly preceding generation after opening
    // and validating the selected one successfully.
    if generation > 0 {
        remove_generation(dir, generation - 1)?;
    }
    if let Some(next) = generation.checked_add(1) {
        remove_generation(dir, next)?;
    }
    sync_dir(dir)?;
    check_disk(dir, 0)?;
    Ok(engine)
}
/// Compact after at least 1024 batches and doubling the selected generation's
/// original file size (with a 256 MiB minimum), or when explicitly forced.
/// Geometric size thresholds avoid repeatedly copying the complete state after
/// each fixed small input chunk as a large import grows.
/// The restart cursor is copied atomically by initialize_from_staged.
pub fn maybe_compact(
    dir: &Path,
    engine: DurableEngine,
    original: &EngineGenesis,
    options: Options,
    force: bool,
) -> Result<DurableEngine> {
    if !force && engine.head().sequence() < 1024 {
        return Ok(engine);
    }
    let old = current(dir)?.ok_or("import CURRENT missing before compaction")?;
    if !force {
        let threshold = original_size(dir, old)?
            .saturating_mul(2)
            .max(256 * 1024 * 1024);
        if fs::metadata(db_path(dir, old)).map_err(err)?.len() < threshold {
            return Ok(engine);
        }
    }
    let next = old.checked_add(1).ok_or("import generation exhausted")?;
    // Only an unselected successor is discarded. An interrupted copy cannot
    // have changed CURRENT, and copying again is safe.
    remove_generation(dir, next)?;
    let source_bytes = fs::metadata(db_path(dir, old)).map_err(err)?.len();
    check_disk(dir, source_bytes)?;
    let started = Instant::now();
    compaction_progress("begin", next, source_bytes, None, started);
    let mut genesis = original.clone();
    genesis.initial_state = Some(engine.head_snapshot().state_header());
    let store = Arc::new(RedbStore::create(db_path(dir, next), import_redb_config()).map_err(err)?);
    let replacement = committed(
        engine
            .initialize_from_staged(store, genesis.clone(), options)
            .map_err(err)?,
    )?;
    write_sync(&genesis_path(dir, next), &encode(&genesis))?;
    save_size(dir, next)?;
    sync_dir(dir)?;
    select(dir, next)?;
    drop(engine);
    remove_generation(dir, old)?;
    sync_dir(dir)?;
    let output_bytes = fs::metadata(db_path(dir, next)).map_err(err)?.len();
    compaction_progress("complete", next, source_bytes, Some(output_bytes), started);
    Ok(replacement)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arkiv_db::{PriceSchedule, durable::Publication};
    #[test]
    fn disk_projection_rejects_impossible_growth_and_resolves_relative_parent() {
        assert!(check_disk(Path::new("."), u64::MAX).is_err());
        assert!(check_disk(Path::new("missing-prefill-output/child"), 0).is_ok());
    }
    #[test]
    fn generation_switch_preserves_checkpoint_and_resumes_with_original_identity() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!(
            "../../target/compaction-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let original = EngineGenesis::new([91; 32], PriceSchedule::FIXTURE);
        let options = Options::default();
        let mut engine = open_stage(&dir, original.clone(), options.clone()).unwrap();
        let mut branch = engine.begin(engine.head(), None).unwrap();
        branch.try_host_put([1; 32], b"value").unwrap().unwrap();
        engine.prepare_import(branch.seal(), b"cursor").unwrap();
        assert!(matches!(
            engine.commit().unwrap(),
            Publication::Committed(_)
        ));
        let root = engine.head_snapshot().state_header();
        // An interrupted predecessor attempt may have left arbitrary incomplete
        // successor files. They are unselected and can safely be rebuilt.
        fs::write(db_path(&dir, 1), b"incomplete copy").unwrap();
        let engine = maybe_compact(
            &dir,
            engine,
            &original,
            import_options(options.clone()),
            true,
        )
        .unwrap();
        assert_eq!(current(&dir).unwrap(), Some(1));
        assert!(!db_path(&dir, 0).exists());
        assert_eq!(engine.head().sequence(), 0);
        drop(engine);
        let mut reopened = open_stage(&dir, original, import_options(options)).unwrap();
        assert_eq!(reopened.head_snapshot().state_header(), root);
        assert_eq!(
            reopened.import_checkpoint(6).unwrap(),
            Some(b"cursor".to_vec())
        );
        let branch = reopened.begin(reopened.head(), None).unwrap();
        reopened.prepare_import(branch.seal(), b"next").unwrap();
        assert!(matches!(
            reopened.commit().unwrap(),
            Publication::Committed(_)
        ));
        drop(reopened);
        fs::remove_dir_all(&dir).unwrap();
    }
}
