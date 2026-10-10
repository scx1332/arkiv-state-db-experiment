//! The disk-full quota sweep, shared by `disk_full.rs` and the experiment
//! example.

use arkiv_db_redb::testing::{BLOCK, CrashDisk, CrashMode};
use arkiv_db_storage::versioned::{
    CommitBatch, CommitOutcome, Health, Reconciliation, SpaceId, StorageErrorKind, VersionedStore,
    reconcile,
};

use super::{assert_absent, assert_applied, block, commit, crash_store};

pub fn config() -> arkiv_db_redb::RedbConfig {
    arkiv_db_redb::RedbConfig::with_cache_size(64 << 10)
}

/// Whether `outcome` failed with `Full`.
pub fn full(outcome: &CommitOutcome) -> bool {
    matches!(
        outcome,
        CommitOutcome::NotCommitted(error) | CommitOutcome::Indeterminate(error)
            if error.kind == StorageErrorKind::Full
    )
}

/// The space model of a sweep (see `arkiv_db_redb::testing`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Space {
    /// A write or length change ending beyond the quota fails.
    Length,
    /// A write allocating blocks beyond the quota fails; length is free.
    Allocation,
}

impl Space {
    fn used(self, disk: &CrashDisk) -> u64 {
        match self {
            Space::Length => disk.visible_image().len() as u64,
            Space::Allocation => disk.allocated_bytes(),
        }
    }

    fn limit(self, disk: &CrashDisk, quota: Option<u64>) {
        match self {
            Space::Length => disk.set_quota(quota),
            Space::Allocation => disk.set_allocation_quota(quota),
        }
    }
}

/// Outcomes of one sweep.
#[derive(Clone, Copy, Debug, Default)]
pub struct Sweep {
    pub quotas: u64,
    /// Space the parent uses.
    pub base: u64,
    /// Space the committed candidate needs.
    pub needed: u64,
    pub not_committed: u64,
    pub indeterminate: u64,
    pub committed: u64,
}

/// Commits a large candidate under quotas between the space the parent
/// uses and the space the candidate needs, and checks every failure: `Full`
/// and latched, reconciled as absent while still full, `Full` again on
/// retry, the parent after a power cut, and exactly one commit once the
/// quota is lifted.
pub fn sweep(space: Space) -> Sweep {
    let genesis = block(None, 0, 1, &[(SpaceId::Namespace(1), 50, 300)]);
    let candidate: CommitBatch = block(
        Some(genesis.new_head),
        1,
        1,
        &[
            (SpaceId::Namespace(1), 1_500, 500),
            (SpaceId::Namespace(2), 300, 400),
        ],
    );
    let image = {
        let disk = CrashDisk::new();
        let store = crash_store(&disk, config());
        commit(&store, genesis.clone());
        disk.crash(store, CrashMode::DurableOnly)
    };
    let (base, needed) = {
        let disk = CrashDisk::from_image(image.clone());
        let base = space.used(&disk);
        let store = crash_store(&disk, config());
        commit(&store, candidate.clone());
        (base, space.used(&disk))
    };
    assert!(needed > base, "the candidate must need more space");

    // 24 even steps, and the last 8 blocks below the need, where the failing
    // write is one of the last the commit makes.
    let steps = 24;
    let quotas: Vec<u64> = (0..steps)
        .map(|step| base + (needed - base) * step / steps)
        .chain((1..=8).map(|blocks| needed - blocks * BLOCK))
        .collect();
    let mut sweep = Sweep {
        quotas: quotas.len() as u64,
        base,
        needed,
        ..Sweep::default()
    };
    for &quota in &quotas {
        let disk = CrashDisk::from_image(image.clone());
        let store = crash_store(&disk, config());
        space.limit(&disk, Some(quota));
        let outcome = store.commit(&candidate);
        match &outcome {
            CommitOutcome::Committed => {
                sweep.committed += 1;
                assert_applied(&store, &candidate);
                continue;
            }
            CommitOutcome::NotCommitted(_) => sweep.not_committed += 1,
            CommitOutcome::Indeterminate(_) => sweep.indeterminate += 1,
        }
        assert!(full(&outcome), "{space:?} quota {quota}: {outcome:?}");
        assert!(
            matches!(store.health(), Health::Failed(ref error) if error.kind == StorageErrorKind::Full)
        );

        // Still full: recovery works, the candidate is absent, a retry fails.
        assert_eq!(
            reconcile(&store, &candidate).unwrap(),
            Reconciliation::NotCommitted
        );
        assert_absent(&store, &candidate);
        assert_applied(&store, &genesis);
        assert!(
            full(&store.commit(&candidate)),
            "{space:?} quota {quota}: retry while full"
        );
        assert_eq!(
            reconcile(&store, &candidate).unwrap(),
            Reconciliation::NotCommitted
        );

        // A power cut now leaves the parent.
        let cut = crash_store(
            &CrashDisk::from_image(disk.crash_image(CrashMode::DurableOnly)),
            config(),
        );
        assert_eq!(cut.head().unwrap(), Some(genesis.new_head));
        assert_absent(&cut, &candidate);

        // Space returns: the identical batch commits exactly once.
        space.limit(&disk, None);
        commit(&store, candidate.clone());
        assert_applied(&store, &candidate);
        assert!(matches!(
            store.commit(&candidate),
            CommitOutcome::NotCommitted(error) if error.kind == StorageErrorKind::HeadMismatch
        ));
    }
    sweep
}
