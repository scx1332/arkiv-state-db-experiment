//! Helpers shared by the redb experiment tests and `examples/redb_experiment.rs`.
//!
//! Measurement directories live on a real disk, never in
//! `std::env::temp_dir()` (a RAM tmpfs here, where `fsync` does nothing):
//! `ARKIV_BENCH_DIR` if set, otherwise Cargo's per-target temporary
//! directory for tests, otherwise `<workspace>/target/bench`.

#![allow(dead_code)]

pub mod amplification;
pub mod crash;
pub mod creation;
pub mod disk_full;
pub mod isolation;
pub mod lost_sync;
pub mod recovery;

use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

use alloy_primitives::keccak256;
use arkiv_db_redb::{RedbConfig, RedbStore, testing::CrashDisk};
use arkiv_db_storage::versioned::{
    CommitBatch, CommitOutcome, Head, ObjectId, SpaceId, Table, VersionedStore, height_key,
};

/// A unique directory on a real disk, removed on drop.
#[derive(Debug)]
pub struct BenchDir {
    path: PathBuf,
}

impl BenchDir {
    /// Creates `<base>/<name>-<pid>-<n>`.
    pub fn new(name: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = base_dir().join(format!("{name}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create bench dir");
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn file(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for BenchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// The real-disk base directory for measurements.
pub fn base_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("ARKIV_BENCH_DIR") {
        return PathBuf::from(dir);
    }
    if let Some(dir) = option_env!("CARGO_TARGET_TMPDIR") {
        return PathBuf::from(dir).join("arkiv-bench");
    }
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/bench")
}

/// The file system type of `path` from `/proc/mounts` (longest mount prefix).
pub fn filesystem_of(path: &Path) -> String {
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mounts = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
    let mut best = (0, "unknown".to_owned());
    for line in mounts.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 3 {
            continue;
        }
        let mount = Path::new(fields[1]);
        if path.starts_with(mount) && fields[1].len() >= best.0 {
            best = (fields[1].len(), fields[2].to_owned());
        }
    }
    best.1
}

/// `keccak256`, the content address the redb store verifies.
pub fn keccak(bytes: &[u8]) -> ObjectId {
    keccak256(bytes).0
}

/// A deterministic object of exactly `len` bytes (at least 16).
pub fn object(tag: u64, n: u64, len: usize) -> (ObjectId, Vec<u8>) {
    let mut bytes = Vec::with_capacity(len.max(16));
    bytes.extend_from_slice(&tag.to_be_bytes());
    bytes.extend_from_slice(&n.to_be_bytes());
    let mut state = tag
        .wrapping_mul(0x9e37_79b9_7f4a_7c15)
        .wrapping_add(n)
        .wrapping_add(1);
    while bytes.len() < len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        bytes.extend_from_slice(&state.to_le_bytes());
    }
    bytes.truncate(len.max(16));
    (keccak(&bytes), bytes)
}

/// The head of block `height` on chain `fork`.
pub fn head(height: u64, fork: u8) -> Head {
    let mut hash = keccak(&[&height.to_be_bytes()[..], &[fork]].concat());
    hash[0] = fork;
    Head { height, hash }
}

/// Encoded bytes of a stand-in block record.
pub fn block_bytes(height: u64, fork: u8, len: usize) -> Vec<u8> {
    let (_, mut bytes) = object(0xb10c_0000 + u64::from(fork), height, len.max(16));
    bytes[..8].copy_from_slice(&height.to_be_bytes());
    bytes
}

/// A block-shaped batch: `objects` objects per listed space, plus a block,
/// manifest, block-index and one inclusion record, moving the head from
/// `parent` to `head(height, fork)`.
pub fn block(
    parent: Option<Head>,
    height: u64,
    fork: u8,
    objects: &[(SpaceId, u64, usize)],
) -> CommitBatch {
    let new = head(height, fork);
    let mut batch = CommitBatch::new(parent, new);
    for (space, count, len) in objects {
        let tag = match space {
            SpaceId::Catalog => 1,
            SpaceId::Host => 2,
            SpaceId::Namespace(id) => 1_000 + id,
        };
        for n in 0..*count {
            let (id, bytes) = object(
                tag * 1_000_003 + u64::from(fork),
                height * 100_000 + n,
                *len,
            );
            batch.put_object(*space, id, bytes);
        }
    }
    batch
        .put_record(
            Table::Blocks,
            height_key(height),
            block_bytes(height, fork, 300),
        )
        .put_record(
            Table::Manifests,
            height_key(height),
            block_bytes(height, fork.wrapping_add(100), 120),
        )
        .put_record(Table::BlockIndex, new.hash, height_key(height))
        .put_record(
            Table::Inclusions,
            [&[0u8][..], &height.to_be_bytes(), &[fork]].concat(),
            height_key(height).to_vec(),
        );
    batch
}

/// Asserts `Committed`.
pub fn commit(store: &dyn VersionedStore, batch: CommitBatch) {
    let outcome = store.commit(&batch);
    assert_eq!(outcome, CommitOutcome::Committed);
}

/// Asserts that every object and record of `batch` is stored and the head
/// is at least its new head.
pub fn assert_applied(store: &dyn VersionedStore, batch: &CommitBatch) {
    for (space, id, bytes) in &batch.objects {
        assert_eq!(
            store.read_object(*space, id).unwrap().as_ref(),
            Some(bytes),
            "object of block {} in {space:?}",
            batch.new_head.height
        );
    }
    for (table, key, value) in &batch.records {
        assert_eq!(
            store.read_record(*table, key).unwrap().as_ref(),
            Some(value),
            "{table:?} record of block {}",
            batch.new_head.height
        );
    }
}

/// Asserts that no object and no record of `batch` is stored (for batches
/// whose objects and records are all new).
pub fn assert_absent(store: &dyn VersionedStore, batch: &CommitBatch) {
    for (space, id, _) in &batch.objects {
        assert_eq!(
            store.read_object(*space, id).unwrap(),
            None,
            "object of uncommitted block {} in {space:?}",
            batch.new_head.height
        );
    }
    for (table, key, _) in &batch.records {
        assert_eq!(
            store.read_record(*table, key).unwrap(),
            None,
            "{table:?} record of uncommitted block {}",
            batch.new_head.height
        );
    }
}

/// A redb store on a simulated disk, reopenable on the same disk.
pub fn crash_store(disk: &CrashDisk, config: RedbConfig) -> RedbStore {
    let disk = disk.clone();
    RedbStore::with_backend(move || Ok(disk.backend()), config).expect("open store on crash disk")
}

/// Wall-clock timer.
pub fn timed<T>(work: impl FnOnce() -> T) -> (T, Duration) {
    let started = Instant::now();
    let value = work();
    (value, started.elapsed())
}

/// The value at quantile `q` (0..=1) of `samples` (sorted in place).
pub fn quantile(samples: &mut [Duration], q: f64) -> Duration {
    if samples.is_empty() {
        return Duration::ZERO;
    }
    samples.sort();
    let index = ((samples.len() - 1) as f64 * q).round() as usize;
    samples[index]
}

/// Megabytes (10^6 bytes) for reports.
pub fn mb(bytes: u64) -> f64 {
    bytes as f64 / 1e6
}
