//! Lost acknowledgements and indeterminate commits are reconciled from
//! durable state, never by executing a block again.
//!
//! - An I/O error injected at every mutating backend call of a commit (not a
//!   crash: the device keeps what it was given, as an operating system's page
//!   cache does when a process survives an I/O error) gives `NotCommitted` or
//!   `Indeterminate`, latches the store, and after `recover` the
//!   reconciliation finds the head at the parent (the identical batch then
//!   commits exactly once) or at the candidate with identical records (the
//!   batch is done). A `NotCommitted` outcome never turns out committed. The
//!   sweep must include indeterminate commits that did commit: a failure of
//!   the final sync after the commit record was written.
//! - The same failure followed by a power cut instead of a recovery leaves
//!   the parent: an indeterminate commit really is undecided until durable
//!   state is read. When the process survives, reconciliation reads the
//!   commit back as committed, and in the `CrashDisk` model a power cut
//!   after that keeps it (the failed sync kept its unsynced writes, and the
//!   reopen synced them).
//! - Linux can instead mark pages whose write-back failed clean: they keep
//!   serving reads but never reach the medium. Modelled with a final sync
//!   that fails and loses the unsynced writes, reconciliation still reads
//!   the batch back as committed. With two-phase commit (the store's
//!   setting) a power cut afterwards keeps the block, because the lost sync
//!   held only the header, which redb rewrites and syncs at reopen. Without
//!   it the lost sync also held the data pages; redb's repair at reopen
//!   commits a header naming them, and after a power cut redb refuses the
//!   file as corrupt. Reading a commit back is therefore not by itself proof
//!   of durability, and quick repair must stay on.
//! - A process that committed and died before acknowledging reconciles the
//!   block as committed after restart, and a retry of the identical batch is
//!   refused by the head check.
//! - A `FaultyStore` "applied but indeterminate" fault over a real file
//!   store reconciles through an actual reopen.

mod common;

use std::io;

use arkiv_db_redb::{
    RedbConfig, RedbStore,
    testing::{CrashDisk, CrashMode, DiskOp},
};
use arkiv_db_storage::{
    testing::{CommitFault, FaultyStore},
    versioned::{
        CommitBatch, CommitOutcome, Health, Reconciliation, SpaceId, StorageErrorKind,
        VersionedStore, reconcile,
    },
};
use common::{
    BenchDir, assert_absent, assert_applied, block, commit, crash_store,
    lost_sync::{self, lost_final_sync},
};

fn config() -> RedbConfig {
    RedbConfig::with_cache_size(1 << 20)
}

fn chain() -> (Vec<CommitBatch>, CommitBatch) {
    let genesis = block(None, 0, 1, &[(SpaceId::Namespace(1), 30, 200)]);
    let candidate = block(
        Some(genesis.new_head),
        1,
        1,
        &[
            (SpaceId::Namespace(1), 40, 200),
            (SpaceId::Namespace(2), 20, 700),
            (SpaceId::Host, 2, 64),
        ],
    );
    (vec![genesis], candidate)
}

fn parent_image(parents: &[CommitBatch]) -> Vec<u8> {
    let disk = CrashDisk::new();
    let store = crash_store(&disk, config());
    for batch in parents {
        commit(&store, batch.clone());
    }
    disk.crash(store, CrashMode::DurableOnly)
}

#[test]
fn io_errors_at_every_commit_call_reconcile_without_re_execution() {
    let (parents, candidate) = chain();
    let image = parent_image(&parents);

    let disk = CrashDisk::from_image(image.clone());
    let store = crash_store(&disk, config());
    let start = disk.mutating_trace().len();
    commit(&store, candidate.clone());
    let trace = disk.mutating_trace()[start..].to_vec();
    drop(store);

    let (mut not_committed, mut indeterminate_absent, mut indeterminate_committed) = (0, 0, 0);
    for (k, op) in trace.iter().enumerate() {
        let skip = trace[..k].iter().filter(|earlier| *earlier == op).count() as u64;
        let disk = CrashDisk::from_image(image.clone());
        let store = crash_store(&disk, config());
        disk.fail(*op, skip, io::ErrorKind::Other);
        let outcome = store.commit(&candidate);
        assert!(
            matches!(store.health(), Health::Failed(_)),
            "call {k} {op:?}: {outcome:?}"
        );
        // Every call fails until recovery, even though the device is fine.
        assert!(store.head().is_err());
        match reconcile(&store, &candidate).unwrap() {
            Reconciliation::Committed => {
                assert!(
                    matches!(outcome, CommitOutcome::Indeterminate(_)),
                    "call {k} {op:?}: {outcome:?} but committed"
                );
                indeterminate_committed += 1;
                assert_applied(&store, &candidate);
                assert!(matches!(
                    store.commit(&candidate),
                    CommitOutcome::NotCommitted(error) if error.kind == StorageErrorKind::HeadMismatch
                ));
            }
            Reconciliation::NotCommitted => {
                match outcome {
                    CommitOutcome::NotCommitted(_) => not_committed += 1,
                    CommitOutcome::Indeterminate(_) => indeterminate_absent += 1,
                    CommitOutcome::Committed => panic!("call {k}: Committed but absent"),
                }
                assert_absent(&store, &candidate);
                commit(&store, candidate.clone());
                assert_applied(&store, &candidate);
            }
            Reconciliation::Diverged { head } => panic!("call {k}: diverged at {head:?}"),
        }
        for batch in &parents {
            assert_applied(&store, batch);
        }
    }
    println!(
        "{} calls: {not_committed} NotCommitted, {indeterminate_absent} Indeterminate and absent, \
         {indeterminate_committed} Indeterminate and committed",
        trace.len()
    );
    assert!(indeterminate_absent > 0);
    assert!(
        indeterminate_committed > 0,
        "the final sync failing after the commit record must be covered"
    );
}

#[test]
fn a_failed_final_sync_is_undecided_until_durable_state_is_read() {
    let (parents, candidate) = chain();
    let image = parent_image(&parents);
    let disk = CrashDisk::from_image(image.clone());
    let store = crash_store(&disk, config());
    // With quick repair a commit syncs twice; fail the second (final) one.
    disk.fail(DiskOp::Sync, 1, io::ErrorKind::Other);
    let outcome = store.commit(&candidate);
    assert!(
        matches!(outcome, CommitOutcome::Indeterminate(ref error) if error.kind == StorageErrorKind::Io)
    );

    // A power cut now: the commit record was never synced.
    let cut = crash_store(
        &CrashDisk::from_image(disk.crash_image(CrashMode::DurableOnly)),
        config(),
    );
    assert_eq!(
        reconcile(&cut, &candidate).unwrap(),
        Reconciliation::NotCommitted
    );

    // The process survives instead: the written commit record is read back.
    assert_eq!(
        reconcile(&store, &candidate).unwrap(),
        Reconciliation::Committed
    );
    assert_eq!(store.health(), Health::Available);
    assert_eq!(store.epoch(), 1);

    // In this model the failed sync kept its writes, and the reopen synced
    // them: a power cut now keeps the block.
    let cut = crash_store(
        &CrashDisk::from_image(disk.crash_image(CrashMode::DurableOnly)),
        config(),
    );
    assert_eq!(cut.head().unwrap(), Some(candidate.new_head));
    assert_applied(&cut, &candidate);
}

#[test]
fn a_block_read_back_after_a_lost_sync_is_durable_only_with_two_phase_commit() {
    // Two-phase commit (quick repair, the store's setting): the data pages
    // were synced before the header was written, so the lost sync lost only
    // the header, which redb rewrites and syncs when `recover` reopens.
    let (_, candidate) = lost_sync::chain();
    assert_eq!(lost_final_sync(true), Ok(Some(candidate.new_head)));
    // One-phase commit: the lost sync also lost the data pages. The repair
    // at reopen commits a header naming them, and after a power cut redb
    // refuses the file.
    let error = lost_final_sync(false).unwrap_err();
    assert_eq!(error.kind, StorageErrorKind::Corrupt, "{error}");
}

#[test]
fn a_block_committed_before_a_process_kill_reconciles_as_committed() {
    let dir = BenchDir::new("lost-ack");
    let path = dir.file("store.redb");
    let (parents, candidate) = chain();
    let store = RedbStore::create(&path, config()).unwrap();
    for batch in &parents {
        commit(&store, batch.clone());
    }
    commit(&store, candidate.clone());
    // The acknowledgement is lost with the process.
    store.probe().kill();
    drop(store);
    let store = RedbStore::open(&path, config()).unwrap();
    assert_eq!(
        reconcile(&store, &candidate).unwrap(),
        Reconciliation::Committed
    );
    assert!(matches!(
        store.commit(&candidate),
        CommitOutcome::NotCommitted(error) if error.kind == StorageErrorKind::HeadMismatch
    ));
    assert_eq!(store.stats().committed, 0);
}

#[test]
fn faulty_indeterminate_over_a_real_file_reconciles_through_a_reopen() {
    let dir = BenchDir::new("faulty-redb");
    let (parents, candidate) = chain();
    let store = FaultyStore::latching(RedbStore::create(dir.file("store.redb"), config()).unwrap());
    for batch in &parents {
        commit(&store, batch.clone());
    }
    store.push_commit_fault(CommitFault::IndeterminateApplied(StorageErrorKind::Io));
    assert!(matches!(
        store.commit(&candidate),
        CommitOutcome::Indeterminate(_)
    ));
    assert_eq!(
        reconcile(&store, &candidate).unwrap(),
        Reconciliation::Committed
    );
    assert_eq!(store.inner().epoch(), 1);
    assert_applied(&store, &candidate);
}
