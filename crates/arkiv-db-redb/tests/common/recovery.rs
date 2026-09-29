//! Recovery and per-commit cost against database size (gate 3).

use std::time::Duration;

use arkiv_db_redb::{OpenReport, RedbConfig, RedbStore};
use arkiv_db_storage::versioned::{BackendStats, Head, SpaceId, Table, VersionedStore, height_key};

use super::{BenchDir, block, commit, quantile, timed};

/// One database size, one quick-repair setting.
#[derive(Clone, Debug)]
pub struct RecoveryRow {
    /// Quick repair on every commit.
    pub quick_repair: bool,
    /// File size when the process was killed, which is also its size at the
    /// reopen (a killed handle writes nothing more).
    pub file_bytes: u64,
    /// Blocks committed.
    pub blocks: u64,
    /// The reopen after the kill.
    pub open: OpenReport,
    /// Store-level reads of the application's open sequence: the store's
    /// metadata reads plus `head()` and the manifest at the head. Fixed by
    /// the code (no read depends on the database's contents); counted to
    /// check that nothing traverses.
    pub app_reads_at_open: u64,
    /// Backend I/O of `head()` and the manifest read right after the reopen
    /// (through the 1 MiB cache the open warmed): the physical cost of the
    /// application's part of the open sequence.
    pub app_backend_at_open: BackendStats,
    /// Backend I/O of one small commit (16 objects of 256 bytes plus
    /// records), averaged over `probe_commits` commits at this size.
    pub commit_io: BackendStats,
    /// Median wall-clock time of those commits.
    pub commit_p50: Duration,
    /// Commits averaged.
    pub probe_commits: u64,
}

fn file_len(path: &std::path::Path) -> u64 {
    std::fs::metadata(path).map_or(0, |meta| meta.len())
}

/// Grows a store to at least `target_bytes` with blocks of `objects_per_block`
/// objects of `object_len` bytes (spread over 8 namespaces), measures small
/// commits at that size, kills the process's handle (no clean close), and
/// reopens it with a 1 MiB cache.
pub fn measure(
    dir: &BenchDir,
    target_bytes: u64,
    quick_repair: bool,
    object_len: usize,
    objects_per_block: u64,
    probe_commits: u64,
) -> RecoveryRow {
    let path = dir.file(&format!("recovery-{target_bytes}-{quick_repair}.redb"));
    let config = RedbConfig {
        cache_size: 1 << 20,
        quick_repair,
    };
    let store = RedbStore::create(&path, config).expect("create");
    let mut parent: Option<Head> = None;
    let mut height = 0;
    let per_namespace = objects_per_block.div_ceil(8);
    while file_len(&path) < target_bytes {
        let spaces: Vec<_> = (0..8)
            .map(|ns| (SpaceId::Namespace(ns), per_namespace, object_len))
            .collect();
        let batch = block(parent, height, 1, &spaces);
        parent = Some(batch.new_head);
        commit(&store, batch);
        height += 1;
    }
    let mut times = Vec::new();
    let before = store.stats().backend.expect("backend stats");
    for _ in 0..probe_commits {
        let batch = block(parent, height, 1, &[(SpaceId::Namespace(3), 16, 256)]);
        parent = Some(batch.new_head);
        let ((), elapsed) = timed(|| commit(&store, batch));
        times.push(elapsed);
        height += 1;
    }
    let after = store.stats().backend.expect("backend stats");
    let total = after.since(&before);
    let per = |value: u64| value / probe_commits.max(1);
    let commit_io = BackendStats {
        reads: per(total.reads),
        read_bytes: per(total.read_bytes),
        writes: per(total.writes),
        write_bytes: per(total.write_bytes),
        syncs: per(total.syncs),
        set_lens: per(total.set_lens),
    };
    let file_bytes = file_len(&path);
    // Simulated process death: nothing redb would write on close reaches
    // the file.
    store.probe().kill();
    drop(store);

    let store = RedbStore::open(&path, config).expect("reopen after kill");
    let open = store.last_open();
    let stats_before = store.stats();
    let head = store.head().expect("head").expect("committed head");
    assert_eq!(Some(head), parent);
    let manifest = store
        .read_record(Table::Manifests, &height_key(head.height))
        .expect("manifest read");
    assert!(manifest.is_some());
    let app = store.stats().since(&stats_before);
    RecoveryRow {
        quick_repair,
        file_bytes,
        blocks: height,
        open,
        app_reads_at_open: u64::from(open.meta_reads) + app.head_reads + app.record_reads,
        app_backend_at_open: app.backend.expect("backend stats"),
        commit_io,
        commit_p50: quantile(&mut times, 0.5),
        probe_commits,
    }
}
