//! The same seeded engine history used by the volatile versioned adapter,
//! now committed to a real file and completely closed before historical reads.

mod common;
#[path = "../../arkiv-db/tests/support/durable_history.rs"]
mod durable_history;

use arkiv_db_redb::{RedbConfig, RedbStore};
use common::BenchDir;
use std::sync::Arc;

#[test]
fn seeded_histories_match_memory_after_real_redb_close_and_reopen() {
    for seed in [1, 27] {
        let dir = BenchDir::new("engine-differential");
        let path = dir.file("history.redb");
        let config = RedbConfig::with_cache_size(256 << 10);
        let store = Arc::new(RedbStore::create(&path, config).unwrap());
        durable_history::run_seeded_history(
            store,
            || Arc::new(RedbStore::open(&path, config).unwrap()),
            seed,
        );
    }
}
