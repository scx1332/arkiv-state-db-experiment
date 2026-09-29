//! Write amplification: few large blocks against many small blocks, debug
//! scale. The release report is `examples/redb_experiment.rs`.
//!
//! The same 1,024 objects of 256 bytes are committed in blocks of 1, 16,
//! 256 and 1,024 objects (each block also carries a block, manifest, index
//! and inclusion record and the head), plus a series of blocks with records
//! only. Every commit costs two syncs with quick repair, and a fixed share
//! of its backend writes (redb's commit header twice, the allocator state,
//! the table directory and each touched table's path) is paid per commit, so
//! bytes written per logical byte fall as blocks grow. The measured numbers
//! are printed; the test checks the sync count (two per commit, plus at most
//! one per file growth) and the ordering.

mod common;

use common::{BenchDir, amplification};

#[test]
fn larger_blocks_write_fewer_bytes_per_logical_byte() {
    let dir = BenchDir::new("amplification");
    let mut previous: Option<amplification::Series> = None;
    for objects_per_block in [1, 16, 256, 1_024] {
        let series = amplification::measure(&dir, 1_024, 256, objects_per_block, 0, true);
        println!(
            "{:>5} objects/block: {:>5} blocks, {:>8} logical bytes, {:>9} bytes written \
             ({:.1}x), {} syncs, file {} bytes",
            objects_per_block,
            series.blocks,
            series.logical_bytes,
            series.backend.write_bytes,
            series.amplification(),
            series.backend.syncs,
            series.file_bytes
        );
        // Two syncs per commit, plus at most one per file growth.
        assert!(series.backend.syncs >= 2 * series.blocks);
        assert!(series.backend.syncs <= 2 * series.blocks + series.backend.set_lens);
        assert!(series.backend.write_bytes >= series.logical_bytes);
        if let Some(previous) = previous {
            assert!(series.amplification() < previous.amplification());
            assert!(series.backend.write_bytes < previous.backend.write_bytes);
        }
        previous = Some(series);
    }
    let empty = amplification::measure(&dir, 0, 256, 0, 64, true);
    println!(
        "records only: {} blocks, {} bytes written per block",
        empty.blocks,
        empty.backend.write_bytes / empty.blocks
    );
    assert!(empty.backend.syncs >= 2 * empty.blocks);
    assert!(empty.backend.syncs <= 2 * empty.blocks + empty.backend.set_lens);
}
