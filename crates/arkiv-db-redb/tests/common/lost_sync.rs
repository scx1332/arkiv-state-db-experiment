//! A final sync that fails and loses its writes, shared by
//! `reconciliation.rs` and the experiment example.

use arkiv_db_redb::{
    RedbConfig, RedbStore,
    testing::{CrashDisk, CrashMode},
};
use arkiv_db_storage::versioned::{
    CommitBatch, CommitOutcome, Head, Reconciliation, SpaceId, StorageError, StorageErrorKind,
    VersionedStore, reconcile,
};

use super::{assert_applied, block, commit, crash_store};

/// A genesis block and a candidate on it.
pub fn chain() -> (CommitBatch, CommitBatch) {
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
    (genesis, candidate)
}

/// Commits the candidate while its last sync fails and loses its unsynced
/// writes (reads keep returning them, as Linux can after an `fsync` error),
/// checks that reconciliation reads it back as committed, then cuts the
/// power and reopens: returns the head the reopen finds, or its error.
pub fn lost_final_sync(quick_repair: bool) -> Result<Option<Head>, StorageError> {
    let config = RedbConfig {
        quick_repair,
        ..RedbConfig::with_cache_size(1 << 20)
    };
    let (genesis, candidate) = chain();
    let image = {
        let disk = CrashDisk::new();
        let store = crash_store(&disk, config);
        commit(&store, genesis);
        disk.crash(store, CrashMode::DurableOnly)
    };
    let disk = CrashDisk::from_image(image);
    let store = crash_store(&disk, config);
    // Two syncs per commit with quick repair, one without: fail the last.
    disk.fail_sync_losing_unsynced(u64::from(quick_repair));
    let outcome = store.commit(&candidate);
    assert!(
        matches!(outcome, CommitOutcome::Indeterminate(ref error) if error.kind == StorageErrorKind::Io),
        "{outcome:?}"
    );
    assert_eq!(
        reconcile(&store, &candidate).unwrap(),
        Reconciliation::Committed
    );
    assert_applied(&store, &candidate);

    let cut = CrashDisk::from_image(disk.crash_image(CrashMode::DurableOnly));
    let reopened = RedbStore::with_backend(move || Ok(cut.backend()), config)?;
    let head = reopened.head()?;
    if head == Some(candidate.new_head) {
        assert_applied(&reopened, &candidate);
    }
    Ok(head)
}
