//! Shared durability ([`RedbConfig::durable_every`]): with `n > 1` only
//! every `n`-th commit, `checkpoint` and `close` are durable.
//!
//! - A power cut leaves the store at its last durable commit: that commit
//!   and every earlier one are complete, every later one is absent, whatever
//!   part of the unsynced writes reached the disk.
//! - The commits between two durable ones sync nothing and are visible to
//!   readers at once.
//! - `checkpoint` and `close` make every commit so far durable.
//! - A store reopened after a cut continues from the last durable commit.
//! - The file is marked, durably and before its first such commit, as one
//!   whose commits shared durability; a store that never did is not.
//!
//! Scope: the power-loss model is the `CrashDisk` model.

use crate::common::{Chain, assert_absent, assert_applied, commit, crash_store};
use arkiv_db_redb::{
    RedbConfig,
    testing::{CrashDisk, CrashMode, Granularity},
};
use arkiv_db_storage::versioned::{CommitBatch, SpaceId, VersionedStore};

const EVERY: u32 = 4;

fn config(quick_repair: bool) -> RedbConfig {
    RedbConfig {
        quick_repair,
        durable_every: EVERY,
        ..RedbConfig::with_cache_size(256 << 10)
    }
}

fn next(chain: &mut Chain) -> CommitBatch {
    let height = chain.height();
    chain.next(&[
        (SpaceId::Namespace(1 + height % 3), 20, 200),
        (SpaceId::Catalog, 1, 100),
    ])
}

fn modes() -> Vec<CrashMode> {
    let mut modes = vec![CrashMode::DurableOnly, CrashMode::AllUnsynced];
    for seed in 0..6 {
        for granularity in [Granularity::Sector, Granularity::Byte] {
            modes.push(CrashMode::Torn { seed, granularity });
        }
    }
    modes
}

#[test]
fn a_power_cut_leaves_the_last_durable_commit() {
    for quick_repair in [true, false] {
        let disk = CrashDisk::new();
        let store = crash_store(&disk, config(quick_repair));
        let mut chain = Chain::default();
        let mut committed: Vec<CommitBatch> = Vec::new();
        let mut durable = 0usize;
        for number in 1..=14u32 {
            let batch = next(&mut chain);
            let syncs = disk.counters().syncs;
            commit(&store, &batch);
            committed.push(batch);
            let synced = disk.counters().syncs - syncs;
            if number % EVERY == 0 {
                assert_eq!(synced, 2, "a durable commit flushes twice");
                assert_eq!(store.undurable_commits(), 0);
                durable = committed.len();
            } else {
                // The first one is preceded by the durable commit of the
                // shared-durability mark.
                let mark = if number == 1 { 2 } else { 0 };
                assert_eq!(synced, mark, "commit {number} is not durable");
                assert_eq!(store.undurable_commits(), number % EVERY);
            }
            assert!(store.shared_durability().unwrap());
            // Readers of the live store see every commit.
            assert_eq!(store.head().unwrap(), chain.head());
            assert_applied(&store, committed.last().unwrap());

            for mode in modes() {
                let image = disk.crash_image(mode);
                let reopened = crash_store(&CrashDisk::from_image(image), config(quick_repair));
                let head = durable.checked_sub(1).map(|i| committed[i].new_head);
                assert_eq!(reopened.head().unwrap(), head, "{mode:?} after {number}");
                assert!(reopened.shared_durability().unwrap());
                for batch in &committed[..durable] {
                    assert_applied(&reopened, batch);
                }
                for batch in &committed[durable..] {
                    assert_absent(&reopened, batch);
                }
            }
        }
    }
}

#[test]
fn checkpoint_and_close_make_every_commit_durable() {
    for close in [false, true] {
        let disk = CrashDisk::new();
        let store = crash_store(&disk, config(true));
        let mut chain = Chain::default();
        let mut committed = Vec::new();
        for _ in 0..EVERY - 1 {
            let batch = next(&mut chain);
            commit(&store, &batch);
            committed.push(batch);
        }
        assert_eq!(store.undurable_commits(), EVERY - 1);
        if close {
            store.close().unwrap();
        } else {
            store.checkpoint().unwrap();
            // Nothing left to flush: a second call writes nothing.
            let syncs = disk.counters().syncs;
            store.checkpoint().unwrap();
            assert_eq!(disk.counters().syncs, syncs);
        }
        assert_eq!(store.undurable_commits(), 0);
        let image = disk.crash_image(CrashMode::DurableOnly);
        let reopened = crash_store(&CrashDisk::from_image(image), config(true));
        assert_eq!(reopened.head().unwrap(), chain.head());
        assert!(!reopened.last_open().full_repair);
        for batch in &committed {
            assert_applied(&reopened, batch);
        }
    }
}

#[test]
fn a_reopened_store_continues_from_the_last_durable_commit() {
    let disk = CrashDisk::new();
    let store = crash_store(&disk, config(true));
    let mut chain = Chain::default();
    let mut durable = Chain::default();
    let mut kept = Vec::new();
    for number in 1..=EVERY + 2 {
        let batch = next(&mut chain);
        commit(&store, &batch);
        if number <= EVERY {
            kept.push(batch);
            durable = chain.clone();
        }
    }
    let image = disk.crash(store, CrashMode::AllUnsynced);
    let disk = CrashDisk::from_image(image);
    let reopened = crash_store(&disk, config(true));
    assert_eq!(reopened.head().unwrap(), durable.head());
    assert!(!reopened.last_open().full_repair);
    // The lost heights are committed again.
    let mut chain = durable;
    for _ in 0..2 * EVERY {
        let batch = next(&mut chain);
        commit(&reopened, &batch);
        kept.push(batch);
    }
    let image = disk.crash(reopened, CrashMode::DurableOnly);
    let last = crash_store(&CrashDisk::from_image(image), config(true));
    assert_eq!(last.head().unwrap(), chain.head());
    for batch in &kept {
        assert_applied(&last, batch);
    }
}

#[test]
fn only_a_store_that_shared_durability_is_marked() {
    let disk = CrashDisk::new();
    let plain = RedbConfig::with_cache_size(256 << 10);
    let store = crash_store(&disk, plain);
    let mut chain = Chain::default();
    for _ in 0..3 {
        commit(&store, &next(&mut chain));
        assert!(!store.shared_durability().unwrap());
    }
    // Reopened with shared durability: marked from its first commit on,
    // and still marked when every commit is durable again.
    let image = disk.crash(store, CrashMode::DurableOnly);
    let disk = CrashDisk::from_image(image);
    let store = crash_store(&disk, config(true));
    assert!(!store.shared_durability().unwrap());
    commit(&store, &next(&mut chain));
    assert!(store.shared_durability().unwrap());
    let image = disk.crash(store, CrashMode::DurableOnly);
    let disk = CrashDisk::from_image(image);
    let store = crash_store(&disk, plain);
    assert!(store.shared_durability().unwrap());
    assert_eq!(store.head().unwrap().map(|head| head.height), Some(2));
    // Its owner clears the mark once nothing is left to redo.
    store.clear_shared_durability().unwrap();
    assert!(!store.shared_durability().unwrap());
    let image = disk.crash(store, CrashMode::DurableOnly);
    let disk = CrashDisk::from_image(image);
    let store = crash_store(&disk, plain);
    assert!(!store.shared_durability().unwrap());
    assert_eq!(store.head().unwrap().map(|head| head.height), Some(2));
    drop(store);
    // A store that shares durable commits keeps it.
    let store = crash_store(&disk, config(true));
    commit(&store, &next(&mut chain_at(2)));
    assert!(store.clear_shared_durability().is_err());
    assert!(store.shared_durability().unwrap());
}

/// The chain as of `height`, to build the next block on.
fn chain_at(height: u64) -> Chain {
    let mut chain = Chain::default();
    for _ in 0..=height {
        next(&mut chain);
    }
    chain
}
