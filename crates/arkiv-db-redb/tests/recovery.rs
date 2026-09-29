//! Gate 3 (recovery), debug-scale checks. The release measurements against
//! database size are `examples/redb_experiment.rs`.
//!
//! A store on a real file is grown to several sizes, a small commit is
//! measured, the process handle is killed (nothing redb writes on close
//! reaches the file) and the file is reopened:
//!
//! - with quick repair (the store's setting) the reopen never runs redb's
//!   full repair, and the application's open sequence — the store's two
//!   metadata reads, `head()`, and the manifest at the head — is exactly
//!   four store-level reads at every size. That count is fixed by the code
//!   (no read depends on the database's contents); asserting it checks
//!   that nothing traverses. The physical cost is the backend I/O, which
//!   the release example measures;
//! - without quick repair every reopen after a kill runs the full repair.
//!
//! Backend bytes and times at open are only reported here: debug builds of
//! redb walk every page on open to check their allocator (a
//! `debug_assertions` check), so open I/O is measured in release builds by
//! the example.

mod common;

use common::{BenchDir, recovery};

#[test]
fn quick_repair_reopens_with_constant_application_reads() {
    let dir = BenchDir::new("recovery");
    for size in [1 << 20, 4 << 20, 12 << 20] {
        let quick = recovery::measure(&dir, size, true, 1024, 256, 4);
        assert!(!quick.open.full_repair, "{quick:?}");
        assert_eq!(quick.open.meta_reads, 2);
        assert_eq!(quick.app_reads_at_open, 4, "{quick:?}");
        assert_eq!(quick.commit_io.syncs, 2);

        let full = recovery::measure(&dir, size, false, 1024, 256, 4);
        assert!(full.open.full_repair, "{full:?}");
        assert_eq!(full.app_reads_at_open, 4);
        assert_eq!(full.commit_io.syncs, 1);
        println!(
            "{} bytes: quick open {:?} / full open {:?}; commit write bytes quick {} full {}",
            quick.file_bytes,
            quick.open.backend,
            full.open.backend,
            quick.commit_io.write_bytes,
            full.commit_io.write_bytes
        );
    }
}
