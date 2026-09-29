//! Write amplification: few large blocks against many small blocks.

use std::time::Duration;

use arkiv_db_redb::{RedbConfig, RedbStore};
use arkiv_db_storage::versioned::{BackendStats, Head, SpaceId, VersionedStore};

use super::{BenchDir, block, commit, quantile, timed};

/// One series: equal object volume, one block size.
#[derive(Clone, Debug)]
pub struct Series {
    /// Objects per block (0: blocks with records only).
    pub objects_per_block: u64,
    /// Blocks committed.
    pub blocks: u64,
    /// Object and record bytes the batches carried.
    pub logical_bytes: u64,
    /// Backend I/O of all commits.
    pub backend: BackendStats,
    /// File size at the end.
    pub file_bytes: u64,
    /// Total commit time.
    pub elapsed: Duration,
    /// Median commit latency.
    pub p50: Duration,
    /// 99th-percentile commit latency.
    pub p99: Duration,
}

impl Series {
    /// Backend bytes written per logical byte.
    pub fn amplification(&self) -> f64 {
        self.backend.write_bytes as f64 / self.logical_bytes.max(1) as f64
    }
}

/// Commits `total_objects` objects of `object_len` bytes (over four
/// namespaces) in blocks of `objects_per_block`; with `objects_per_block`
/// 0, commits `empty_blocks` blocks with records only.
pub fn measure(
    dir: &BenchDir,
    total_objects: u64,
    object_len: usize,
    objects_per_block: u64,
    empty_blocks: u64,
    quick_repair: bool,
) -> Series {
    let path = dir.file(&format!(
        "amplification-{objects_per_block}-{quick_repair}.redb"
    ));
    let config = RedbConfig {
        quick_repair,
        ..RedbConfig::default()
    };
    let store = RedbStore::create(&path, config).expect("create");
    let blocks = if objects_per_block == 0 {
        empty_blocks
    } else {
        total_objects.div_ceil(objects_per_block)
    };
    let start = store.stats().backend.expect("backend");
    let mut parent: Option<Head> = None;
    let mut logical = 0;
    let mut times = Vec::with_capacity(blocks as usize);
    let mut remaining = total_objects;
    for height in 0..blocks {
        let now = objects_per_block.min(remaining);
        remaining -= now;
        let spaces: Vec<_> = (0..4)
            .map(|ns| (SpaceId::Namespace(ns), (now + 3 - ns) / 4, object_len))
            .filter(|(_, count, _)| *count > 0)
            .collect();
        let batch = block(parent, height, 1, &spaces);
        logical += batch.logical_bytes();
        parent = Some(batch.new_head);
        let ((), elapsed) = timed(|| commit(&store, batch));
        times.push(elapsed);
    }
    let backend = store.stats().backend.expect("backend").since(&start);
    drop(store);
    let elapsed = times.iter().sum();
    Series {
        objects_per_block,
        blocks,
        logical_bytes: logical,
        backend,
        file_bytes: std::fs::metadata(&path).map_or(0, |meta| meta.len()),
        elapsed,
        p50: quantile(&mut times, 0.5),
        p99: quantile(&mut times, 0.99),
    }
}
