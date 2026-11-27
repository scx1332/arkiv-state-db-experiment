//! Immediate durability: `Committed` means durable against power loss at the
//! moment `commit` returns.
//!
//! - On a simulated `CrashDisk`, after every `Committed` commit the image a
//!   power cut would leave *right then* (synced data only, no unsynced
//!   operation) is reopened: it holds that commit and every earlier one.
//! - Every commit syncs: two syncs per commit with redb's two-phase commit
//!   (which quick repair implies), one without.
//! - The store's initialization commit is durable too: a power cut right
//!   after `create` keeps the database identity.
//! - On a real file, a simulated process kill right after `Committed`
//!   (nothing redb writes on close reaches the file) loses nothing, and the
//!   reopen loads the saved allocator state instead of running a full repair.
//!
//! Scope: the power-loss model is the `CrashDisk` model; the real-file case
//! is a process kill, not a power cut.

use crate::common::{BenchDir, Chain, assert_applied, commit, crash_store};
use arkiv_db_redb::{
    RedbConfig, RedbStore,
    testing::{CrashDisk, CrashMode},
};
use arkiv_db_storage::versioned::{SpaceId, VersionedStore};

#[test]
fn every_committed_block_survives_an_immediate_power_cut() {
    for (quick_repair, two_phase_commit) in [(true, true), (false, true), (false, false)] {
        let config = RedbConfig {
            quick_repair,
            two_phase_commit,
            ..RedbConfig::with_cache_size(256 << 10)
        };
        let disk = CrashDisk::new();
        let store = crash_store(&disk, config);
        let mut chain = Chain::default();
        let mut committed = Vec::new();
        for height in 0..12 {
            let batch = chain.next(&[
                (SpaceId::Namespace(1 + height % 3), 20, 200),
                (SpaceId::Catalog, 1, 100),
            ]);
            let syncs = disk.counters().syncs;
            commit(&store, &batch);
            let per_commit = disk.counters().syncs - syncs;
            assert_eq!(per_commit, if two_phase_commit { 2 } else { 1 });
            committed.push(batch);

            // The power-loss image at this instant.
            let image = disk.crash_image(CrashMode::DurableOnly);
            let reopened = crash_store(&CrashDisk::from_image(image), config);
            assert_eq!(reopened.head().unwrap(), chain.head());
            assert_eq!(reopened.database_id(), store.database_id());
            for batch in &committed {
                assert_applied(&reopened, batch);
            }
        }
    }
}

#[test]
fn the_initialization_commit_is_durable() {
    let disk = CrashDisk::new();
    let store = crash_store(&disk, RedbConfig::default());
    assert!(store.last_open().initialized);
    let id = store.database_id();
    let image = disk.crash(store, CrashMode::DurableOnly);
    let reopened = crash_store(&CrashDisk::from_image(image), RedbConfig::default());
    assert!(!reopened.last_open().initialized);
    assert!(!reopened.last_open().full_repair);
    assert_eq!(reopened.database_id(), id);
    assert_eq!(reopened.head().unwrap(), None);
}

#[test]
fn a_killed_process_loses_no_committed_block_on_a_real_file() {
    let dir = BenchDir::new("durability");
    let path = dir.file("store.redb");
    let store = RedbStore::create(&path, RedbConfig::default()).unwrap();
    let id = store.database_id();
    let mut chain = Chain::default();
    let mut committed = Vec::new();
    for _ in 0..8 {
        let batch = chain.next(&[(SpaceId::Namespace(7), 50, 300)]);
        commit(&store, &batch);
        committed.push(batch);
    }
    store.probe().kill();
    drop(store);
    let reopened = RedbStore::open(&path, RedbConfig::default()).unwrap();
    assert_eq!(reopened.database_id(), id);
    assert_eq!(reopened.head().unwrap(), chain.head());
    assert!(!reopened.last_open().full_repair);
    for batch in &committed {
        assert_applied(&reopened, batch);
    }
}
