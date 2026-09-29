//! Gate 2 (namespace isolation), debug-scale checks. The release run with
//! larger sizes, cold and warm small-cache passes, is
//! `examples/redb_experiment.rs`.
//!
//! Namespace A holds 200 objects and is then frozen. B grows to 1x, 10x and
//! 100x A's object count, the number of namespaces grows to 100 and 400, and
//! 100 more blocks of history are committed. In every phase the file is
//! reopened with a zero-byte redb cache and all of A's objects are read in a
//! fixed order (see `common::isolation`).
//!
//! What it establishes on this layout (one redb table per namespace):
//!
//! - A's application-level reads are identical in every phase (same calls,
//!   hits and bytes) and touch no other space.
//! - With no cache, each of A's object reads costs exactly
//!   `directory height + A's table height` backend page reads: redb descends
//!   its table directory to find A's table, then A's own B-tree. That bound
//!   is derived from B-tree depth and holds in every phase.
//! - B's growth and history growth leave A's backend reads identical: they
//!   change neither the directory height nor A's table.
//! - Namespace-count growth raises the directory height (logarithmically in
//!   the number of tables), and A's uncached reads rise by one page per read
//!   per level: physical O(log namespaces), not constant.
//!
//! What it does not establish: latency isolation on shared hardware, and the
//! cost of the shared catalog object table, which the engine does not use
//! yet (N3).

mod common;

use arkiv_db_storage::versioned::SpaceStats;
use common::{
    BenchDir,
    isolation::{self, A, Params},
};

#[test]
fn a_reads_are_identical_and_within_the_b_tree_depth_bound() {
    let dir = BenchDir::new("isolation");
    let params = Params {
        a_objects: 200,
        object_len: 200,
        b_factors: vec![1, 10, 100],
        namespace_counts: vec![100, 400],
        history_blocks: 100,
        objects_per_block: 1_000,
        small_cache: 256 << 10,
    };
    let phases = isolation::run(&dir, &params);
    assert_eq!(phases.len(), 6);
    let baseline = &phases[0];
    let expected_app = vec![(
        A,
        SpaceStats {
            reads: 200,
            hits: 200,
            read_bytes: 200 * 200,
            objects_written: 0,
            bytes_written: 0,
        },
    )];
    for phase in &phases {
        println!(
            "{:>14}: B {:>6} objects, {:>3} namespaces, {:>3} blocks, {:>9} bytes; directory \
             height {}, A height {}; uncached {} page reads (bound {})",
            phase.name,
            phase.b_objects,
            phase.namespaces,
            phase.blocks,
            phase.file_bytes,
            phase.directory_height,
            phase.a_height,
            phase.uncached.reads,
            phase.uncached_bound()
        );
        assert_eq!(phase.app, expected_app, "{}", phase.name);
        assert_eq!(phase.a_height, baseline.a_height, "A is frozen");
        assert_eq!(
            phase.uncached.reads,
            phase.uncached_bound(),
            "{}",
            phase.name
        );
        assert_eq!(phase.uncached.writes, 0);
    }
    // B's growth and history growth: identical physical reads.
    for phase in phases[1..3].iter().chain(&phases[5..]) {
        if phase.directory_height == baseline.directory_height {
            assert_eq!(phase.uncached, baseline.uncached, "{}", phase.name);
        }
    }
    for phase in &phases[1..3] {
        assert_eq!(
            phase.directory_height, baseline.directory_height,
            "{}",
            phase.name
        );
    }
    // Namespace growth: the directory deepens by a bounded number of levels.
    let deepest = phases
        .iter()
        .map(|phase| phase.directory_height)
        .max()
        .unwrap();
    assert!(
        deepest > baseline.directory_height,
        "400 tables need a deeper directory"
    );
    assert!(deepest <= baseline.directory_height + 2);
    assert_eq!(phases[5].directory_height, phases[4].directory_height);
    assert_eq!(phases[5].uncached, phases[4].uncached, "history growth");
}
