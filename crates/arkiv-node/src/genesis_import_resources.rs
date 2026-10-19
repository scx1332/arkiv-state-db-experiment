//! Resource policy for private bulk genesis construction and final installation.
use arkiv_db::durable::Options;
use arkiv_db_redb::RedbConfig;
use std::path::Path;
type Result<T> = std::result::Result<T, String>;
fn err(e: impl std::fmt::Display) -> String {
    format!("import resources: {e}")
}
/// One redb cache plus decoded engine caches remain bounded independently of rows.
const IMPORT_CACHE_BYTES: usize = 256 * 1024 * 1024;
const IMPORT_CACHE_ENTRIES: usize = 262_144;
pub fn import_options(mut options: Options) -> Options {
    options.cache_bytes = IMPORT_CACHE_BYTES;
    options.cache_entries = IMPORT_CACHE_ENTRIES;
    options
}
pub fn import_redb_config() -> RedbConfig {
    RedbConfig::with_cache_size(IMPORT_CACHE_BYTES)
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn disk_projection_rejects_impossible_growth_and_resolves_relative_parent() {
        assert!(check_disk(Path::new("."), u64::MAX).is_err());
        assert!(check_disk(Path::new("missing-prefill-output/child"), 0).is_ok());
    }
}
