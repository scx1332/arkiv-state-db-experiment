//! Historical objects stay directly readable: after reopening with a tiny
//! cache, and while new blocks commit.
//!
//! Every block writes new objects into three namespaces, including a new
//! version of the same logical entries (new bytes, so a new content address)
//! each block, the way a persistent tree writes a new node for every changed
//! path while old nodes stay reachable from old roots.
//!
//! - After 60 blocks and a reopen with a zero-byte and with a 64 KiB redb
//!   cache, every object and record of every block reads back identical.
//! - While a writer commits 80 blocks, four readers repeatedly read the head
//!   and verify every object and record of a block at or below it; a
//!   committed block is never missing or different.
//! - The store never created a redb savepoint: history is ordinary immutable
//!   entries of the current tables, not restored snapshots.

mod common;

use std::sync::{
    Arc, RwLock,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

use arkiv_db_redb::{RedbConfig, RedbStore, testing::SplitMix};
use arkiv_db_storage::versioned::{CommitBatch, SpaceId, VersionedStore};
use common::{BenchDir, assert_applied, block, commit};

fn history_block(parent: Option<arkiv_db_storage::versioned::Head>, height: u64) -> CommitBatch {
    block(
        parent,
        height,
        1,
        &[
            (SpaceId::Namespace(1), 12, 180),
            (SpaceId::Namespace(2), 3, 1_500),
            (SpaceId::Namespace(3 + height % 5), 5, 90),
            (SpaceId::Catalog, 2, 110),
        ],
    )
}

#[test]
fn every_historical_object_is_readable_after_reopen_with_a_small_cache() {
    let dir = BenchDir::new("history");
    let path = dir.file("store.redb");
    let store = RedbStore::create(&path, RedbConfig::default()).unwrap();
    let mut parent = None;
    let mut blocks = Vec::new();
    for height in 0..60 {
        let batch = history_block(parent, height);
        commit(&store, batch.clone());
        parent = Some(batch.new_head);
        blocks.push(batch);
    }
    drop(store);

    for cache in [0, 64 << 10] {
        let store = RedbStore::open(&path, RedbConfig::with_cache_size(cache)).unwrap();
        assert_eq!(store.head().unwrap(), parent);
        for batch in &blocks {
            assert_applied(&store, batch);
        }
    }

    let db = redb::Database::open(&path).unwrap();
    let txn = db.begin_write().unwrap();
    assert_eq!(txn.list_persistent_savepoints().unwrap().count(), 0);
    txn.abort().unwrap();
}

#[test]
fn historical_reads_run_concurrently_with_commits() {
    let dir = BenchDir::new("history-concurrent");
    let store = Arc::new(
        RedbStore::create(
            dir.file("store.redb"),
            RedbConfig::with_cache_size(128 << 10),
        )
        .unwrap(),
    );
    let published: Arc<RwLock<Vec<CommitBatch>>> = Arc::default();
    let done = Arc::new(AtomicBool::new(false));
    let verified = Arc::new(AtomicU64::new(0));

    let first = history_block(None, 0);
    commit(store.as_ref(), first.clone());
    published.write().unwrap().push(first);

    std::thread::scope(|scope| {
        for reader in 0..4 {
            let (store, published, done, verified) = (
                Arc::clone(&store),
                Arc::clone(&published),
                Arc::clone(&done),
                Arc::clone(&verified),
            );
            scope.spawn(move || {
                let mut rng = SplitMix::new(reader);
                while !done.load(Ordering::Acquire) {
                    let head = store.head().unwrap().expect("genesis committed");
                    // Blocks are published after `Committed`, so every block
                    // listed is at or below the head.
                    let batch = {
                        let list = published.read().unwrap();
                        assert!(list.len() as u64 <= head.height + 1);
                        list[rng.below(list.len() as u64) as usize].clone()
                    };
                    assert_applied(store.as_ref(), &batch);
                    verified.fetch_add(1, Ordering::Relaxed);
                }
            });
        }
        let mut parent = Some(published.read().unwrap()[0].new_head);
        for height in 1..80 {
            let batch = history_block(parent, height);
            commit(store.as_ref(), batch.clone());
            parent = Some(batch.new_head);
            published.write().unwrap().push(batch);
        }
        done.store(true, Ordering::Release);
    });
    assert!(verified.load(Ordering::Relaxed) > 0);
    let blocks = published.read().unwrap();
    for batch in blocks.iter() {
        assert_applied(store.as_ref(), batch);
    }
}
