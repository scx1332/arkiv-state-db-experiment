//! Disk full while committing: the commit fails with `Full` and is never
//! applied twice; once space returns the identical batch commits exactly
//! once.
//!
//! The candidate block is larger than the redb cache and needs more space.
//! For a sweep of quotas between the space the parent uses and the space
//! the committed candidate needs, under both space models of `CrashDisk`:
//!
//! - **length quota** (a write or length change ending beyond the quota
//!   fails): redb grows the file with `set_len` before its `commit()`, so
//!   every failure is `NotCommitted(Full)`;
//! - **allocation quota** (sparse files: growing the length is free, a
//!   write allocating blocks beyond the quota fails, as on a file system
//!   that allocates on write): a failed spill of pages before `commit()` is
//!   `NotCommitted(Full)`, a failed write inside `commit()` is
//!   `Indeterminate(Full)`, and both occur.
//!
//! For every quota that fails the commit: the outcome is `Full`-kinded and
//! the store is `Failed(Full)`; `recover()` works while the disk is still
//! full and reconciliation finds the parent (a `NotCommitted` outcome never
//! turns out applied, an `Indeterminate` one is decided here); retrying
//! while still full fails again with `Full` and applies nothing; a power cut
//! at that point leaves the parent; once the quota is lifted the identical
//! batch commits, exactly once.
//!
//! File systems can also report `ENOSPC` from `fsync`. The second test
//! injects `StorageFull` at every mutating call of a commit: every outcome is
//! `Full`-kinded, and reconciliation after recovery decides it (an
//! `Indeterminate(Full)` from the final sync did commit).

mod common;

use std::io;

use arkiv_db_redb::testing::{CrashDisk, CrashMode};
use arkiv_db_storage::versioned::{
    CommitOutcome, Reconciliation, SpaceId, VersionedStore, reconcile,
};
use common::{
    assert_applied, block, commit, crash_store,
    disk_full::{Space, config, full, sweep},
};

#[test]
fn a_length_quota_fails_before_the_commit_and_the_batch_commits_once_later() {
    let result = sweep(Space::Length);
    println!("length quota: {result:?}");
    assert!(result.not_committed > 0);
    // redb grows the file before `commit()`, where this model charges.
    assert_eq!(result.indeterminate, 0);
}

#[test]
fn an_allocation_quota_also_fails_inside_the_commit_and_the_batch_commits_once_later() {
    let result = sweep(Space::Allocation);
    println!("allocation quota: {result:?}");
    assert!(result.not_committed > 0);
    assert!(
        result.indeterminate > 0,
        "some writes inside commit() must fail"
    );
}

#[test]
fn storage_full_at_any_commit_call_is_full_and_reconciles() {
    let genesis = block(None, 0, 1, &[(SpaceId::Namespace(1), 20, 300)]);
    let candidate = block(
        Some(genesis.new_head),
        1,
        1,
        &[(SpaceId::Namespace(1), 30, 300)],
    );
    let image = {
        let disk = CrashDisk::new();
        let store = crash_store(&disk, config());
        commit(&store, genesis.clone());
        disk.crash(store, CrashMode::DurableOnly)
    };
    let trace = {
        let disk = CrashDisk::from_image(image.clone());
        let store = crash_store(&disk, config());
        let start = disk.mutating_trace().len();
        commit(&store, candidate.clone());
        disk.mutating_trace()[start..].to_vec()
    };
    let (mut absent, mut done) = (0, 0);
    for (k, op) in trace.iter().enumerate() {
        let skip = trace[..k].iter().filter(|earlier| *earlier == op).count() as u64;
        let disk = CrashDisk::from_image(image.clone());
        let store = crash_store(&disk, config());
        disk.fail(*op, skip, io::ErrorKind::StorageFull);
        let outcome = store.commit(&candidate);
        assert!(full(&outcome), "call {k} {op:?}: {outcome:?}");
        match reconcile(&store, &candidate).unwrap() {
            Reconciliation::NotCommitted => {
                absent += 1;
                commit(&store, candidate.clone());
            }
            Reconciliation::Committed => {
                assert!(matches!(outcome, CommitOutcome::Indeterminate(_)));
                done += 1;
            }
            Reconciliation::Diverged { head } => panic!("call {k}: diverged at {head:?}"),
        }
        assert_applied(&store, &candidate);
    }
    println!(
        "{} calls: {absent} reconciled absent, {done} reconciled committed",
        trace.len()
    );
    assert!(absent > 0 && done > 0);
}
