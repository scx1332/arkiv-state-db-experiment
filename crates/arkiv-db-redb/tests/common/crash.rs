//! Crash-point runner shared by `atomicity.rs` and the experiment example.
//!
//! See `atomicity.rs` for what the runs establish.

use arkiv_db_redb::{
    RedbConfig, RedbStore,
    testing::{CrashDisk, CrashMode, DiskOp, Granularity},
};
use arkiv_db_storage::versioned::{
    CommitBatch, CommitOutcome, Head, Reconciliation, SpaceId, StorageErrorKind, Table,
    VersionedStore, reconcile,
};

use super::{assert_absent, assert_applied, block, commit, crash_store};

pub const SEEDS: u64 = 8;
/// The same for the `#[ignore]`d heavy run.
pub const HEAVY_SEEDS: u64 = 64;

pub fn config(quick_repair: bool) -> RedbConfig {
    RedbConfig {
        cache_size: 1 << 20,
        quick_repair,
    }
}

/// The parent chain: three blocks over the catalog, host and two namespaces.
pub fn parents() -> Vec<CommitBatch> {
    let mut batches = Vec::new();
    let mut parent = None;
    for height in 0..3 {
        let batch = block(
            parent,
            height,
            1,
            &[
                (SpaceId::Catalog, 3, 120),
                (SpaceId::Host, 2, 64),
                (SpaceId::Namespace(1), 40, 180),
                (SpaceId::Namespace(2), 25, 900),
            ],
        );
        parent = Some(batch.new_head);
        batches.push(batch);
    }
    batches
}

/// A parent chain of twelve blocks for the steady-state runs: every block
/// adds objects to the catalog, host and two namespaces (so each commit
/// frees the pages of the paths it rewrites) and to one of three more
/// namespaces.
pub fn steady_parents() -> Vec<CommitBatch> {
    let mut batches = Vec::new();
    let mut parent = None;
    for height in 0..12 {
        let batch = block(
            parent,
            height,
            1,
            &[
                (SpaceId::Catalog, 2, 120),
                (SpaceId::Host, 1, 64),
                (SpaceId::Namespace(1), 15, 180),
                (SpaceId::Namespace(2), 5, 900),
                (SpaceId::Namespace(5 + height % 3), 3, 250),
            ],
        );
        parent = Some(batch.new_head);
        batches.push(batch);
    }
    batches
}

/// The candidate: ten tables in one transaction, including a new namespace
/// table, a value larger than a page and a metadata record.
pub fn candidate(parent: Head) -> CommitBatch {
    let mut batch = block(
        Some(parent),
        parent.height + 1,
        1,
        &[
            (SpaceId::Catalog, 4, 120),
            (SpaceId::Host, 2, 64),
            (SpaceId::Namespace(1), 60, 180),
            (SpaceId::Namespace(2), 10, 900),
            (SpaceId::Namespace(3), 30, 250),
            (SpaceId::Namespace(4), 1, 9_000),
        ],
    );
    batch.put_record(Table::Meta, "candidate_marker", b"block 3".to_vec());
    batch
}

/// The durable image after committing the parents (every commit synced;
/// the store is then cut off, so no clean close runs).
pub fn parent_image(config: RedbConfig) -> (Vec<u8>, Vec<CommitBatch>) {
    let disk = CrashDisk::new();
    let store = crash_store(&disk, config);
    let parents = parents();
    for batch in &parents {
        commit(&store, batch.clone());
    }
    (disk.crash(store, CrashMode::DurableOnly), parents)
}

/// Counts of one crash-point run.
#[derive(Clone, Debug, Default)]
pub struct Tally {
    pub crash_points: u64,
    pub images: u64,
    pub parent: u64,
    pub candidate: u64,
    pub full_repairs: u64,
    pub committed_outcomes: u64,
    pub indeterminate_outcomes: u64,
    pub not_committed_outcomes: u64,
    pub calls_per_commit: u64,
    pub syncs_per_commit: u64,
    pub set_lens_per_commit: u64,
}

pub fn modes(seeds: u64) -> Vec<CrashMode> {
    let mut modes = vec![CrashMode::DurableOnly, CrashMode::AllUnsynced];
    for seed in 0..seeds {
        modes.push(CrashMode::Torn {
            seed,
            granularity: Granularity::Sector,
        });
        modes.push(CrashMode::Torn {
            seed: seed + 1_000,
            granularity: Granularity::Byte,
        });
    }
    modes
}

/// Asserts that redb's own integrity check finds `image` clean: after its
/// open (and the repair the open runs), every page checksum verifies and the
/// allocator state matches the pages reachable from the roots.
pub fn assert_integrity(image: &[u8], context: &str) {
    let disk = CrashDisk::from_image(image.to_vec());
    let mut db = redb::Builder::new()
        .create_with_backend(disk.backend())
        .unwrap_or_else(|error| panic!("{context}: redb open: {error}"));
    match db.check_integrity() {
        Ok(true) => {}
        other => panic!("{context}: integrity check {other:?}"),
    }
}

/// A block on top of `parent` that touches the parents' tables again, so
/// its commit reuses pages the earlier commits freed.
fn follow_up(parent: Head) -> CommitBatch {
    block(
        Some(parent),
        parent.height + 1,
        7,
        &[
            (SpaceId::Catalog, 2, 120),
            (SpaceId::Host, 1, 64),
            (SpaceId::Namespace(1), 20, 180),
            (SpaceId::Namespace(2), 4, 900),
        ],
    )
}

/// Checks one crash image and returns whether it holds the candidate.
///
/// The image must pass redb's integrity check, reopen to exactly the parent
/// or exactly the candidate with every earlier block intact, and reconcile
/// accordingly (an absent candidate then commits exactly once). Then one
/// more block commits on top; every block reads back, and the durable image
/// after that commit passes the integrity check and reopens with every
/// block, so an allocator state the crash left wrong could not overwrite
/// live pages unnoticed.
pub fn check_image(
    image: Vec<u8>,
    config: RedbConfig,
    parents: &[CommitBatch],
    candidate: &CommitBatch,
    tally: &mut Tally,
    context: &str,
) -> bool {
    assert_integrity(&image, context);
    let disk = CrashDisk::from_image(image);
    let store = crash_store(&disk, config);
    if store.last_open().full_repair {
        tally.full_repairs += 1;
    }
    let head = store.head().unwrap();
    for batch in parents {
        assert_applied(&store, batch);
    }
    let parent_head = parents.last().unwrap().new_head;
    let holds_candidate = if head == Some(candidate.new_head) {
        assert_applied(&store, candidate);
        tally.candidate += 1;
        true
    } else {
        assert_eq!(
            head,
            Some(parent_head),
            "{context}: head is neither parent nor candidate"
        );
        assert_absent(&store, candidate);
        tally.parent += 1;
        false
    };
    // Reconciliation agrees, and an absent candidate commits exactly once.
    match reconcile(&store, candidate).unwrap() {
        Reconciliation::Committed => assert!(holds_candidate, "{context}"),
        Reconciliation::NotCommitted => {
            assert!(!holds_candidate, "{context}");
            commit(&store, candidate.clone());
            assert_applied(&store, candidate);
            assert!(matches!(
                store.commit(candidate.clone()),
                CommitOutcome::NotCommitted(error) if error.kind == StorageErrorKind::HeadMismatch
            ));
        }
        Reconciliation::Diverged { head } => panic!("{context}: diverged at {head:?}"),
    }

    let next = follow_up(candidate.new_head);
    commit(&store, next.clone());
    let everything: Vec<&CommitBatch> = parents.iter().chain([candidate, &next]).collect();
    for batch in &everything {
        assert_applied(&store, batch);
    }
    let after = disk.crash(store, CrashMode::DurableOnly);
    assert_integrity(&after, &format!("{context}, after the follow-up commit"));
    let reopened = crash_store(&CrashDisk::from_image(after), config);
    assert_eq!(reopened.head().unwrap(), Some(next.new_head), "{context}");
    for batch in &everything {
        assert_applied(&reopened, batch);
    }
    tally.images += 1;
    holds_candidate
}

/// Chooses crash points: `(k, trace of the complete commit's mutating calls)`.
pub type Select<'a> = &'a dyn Fn(u64, &[DiskOp]) -> bool;

/// Where a crash-point run starts: the device and the live store the
/// candidate is committed through, holding the parents.
pub enum Start<'a> {
    /// The parents' crash image, reopened (running redb's recovery) before
    /// every candidate commit: the first commit after a restart.
    Reopened(&'a [u8]),
    /// The parents committed through the same store just before the
    /// candidate: a commit in steady state, with pages freed by earlier
    /// commits and redb's allocator state live rather than freshly loaded.
    SameSession,
}

impl Start<'_> {
    fn open(&self, config: RedbConfig, parents: &[CommitBatch]) -> (CrashDisk, RedbStore) {
        match self {
            Start::Reopened(image) => {
                let disk = CrashDisk::from_image(image.to_vec());
                let store = crash_store(&disk, config);
                (disk, store)
            }
            Start::SameSession => {
                let disk = CrashDisk::new();
                let store = crash_store(&disk, config);
                for batch in parents {
                    commit(&store, batch.clone());
                }
                (disk, store)
            }
        }
    }
}

/// Crashes `candidate` at each selected mutating call of its commit (all of
/// them if `select` is `None`) and checks every crash image `modes` yields.
pub fn run_crash_points(
    config: RedbConfig,
    start: &Start<'_>,
    parents: &[CommitBatch],
    candidate: &CommitBatch,
    modes: &[CrashMode],
    select: Option<Select<'_>>,
) -> Tally {
    // A complete commit's mutating calls.
    let (disk, store) = start.open(config, parents);
    let first = disk.mutating_trace().len();
    commit(&store, candidate.clone());
    let trace = disk.mutating_trace()[first..].to_vec();
    drop(store);
    let calls = trace.len() as u64;
    assert!(calls >= 3, "a durable commit writes and syncs");

    let mut tally = Tally::default();
    for k in 0..=calls {
        if let Some(select) = select
            && !select(k, &trace)
        {
            continue;
        }
        let (disk, store) = start.open(config, parents);
        disk.crash_after(k);
        let outcome = store.commit(candidate.clone());
        match &outcome {
            CommitOutcome::Committed => tally.committed_outcomes += 1,
            CommitOutcome::Indeterminate(_) => tally.indeterminate_outcomes += 1,
            CommitOutcome::NotCommitted(_) => tally.not_committed_outcomes += 1,
        }
        disk.cut_off();
        drop(store);
        tally.crash_points += 1;
        for mode in modes {
            let context = format!(
                "quick_repair={} crash point {k}/{calls} {mode:?} outcome {outcome:?}",
                config.quick_repair
            );
            let holds = check_image(
                disk.crash_image(*mode),
                config,
                parents,
                candidate,
                &mut tally,
                &context,
            );
            if outcome == CommitOutcome::Committed && *mode == CrashMode::DurableOnly {
                assert!(
                    holds,
                    "{context}: a Committed batch did not survive power loss"
                );
            }
            if matches!(outcome, CommitOutcome::NotCommitted(_)) {
                assert!(!holds, "{context}: a NotCommitted batch became durable");
            }
        }
    }
    tally.calls_per_commit = calls;
    tally.set_lens_per_commit = trace.iter().filter(|op| **op == DiskOp::SetLen).count() as u64;
    tally.syncs_per_commit = trace.iter().filter(|op| **op == DiskOp::Sync).count() as u64;
    tally
}

pub fn run(quick_repair: bool, seeds: u64) -> Tally {
    let config = config(quick_repair);
    let (image, parents) = parent_image(config);
    let candidate = candidate(parents.last().unwrap().new_head);
    let start = Start::Reopened(&image);
    let tally = run_crash_points(config, &start, &parents, &candidate, &modes(seeds), None);
    // The last crash point is past the end: the commit completed.
    assert_eq!(tally.committed_outcomes, 1);
    tally
}

/// The ten-table candidate after twelve parents committed in the same
/// session, at every crash point.
pub fn run_steady(quick_repair: bool, seeds: u64) -> Tally {
    let config = config(quick_repair);
    let parents = steady_parents();
    let candidate = candidate(parents.last().unwrap().new_head);
    let tally = run_crash_points(
        config,
        &Start::SameSession,
        &parents,
        &candidate,
        &modes(seeds),
        None,
    );
    assert_eq!(tally.committed_outcomes, 1);
    tally
}

/// A commit larger than the redb cache (so pages spill to the file before
/// `commit()`) that also grows the file (so length changes occur).
pub fn growing_candidate(parent: Head) -> CommitBatch {
    let mut batch = block(
        Some(parent),
        parent.height + 1,
        2,
        &[
            (SpaceId::Catalog, 8, 120),
            (SpaceId::Namespace(1), 1_200, 400),
            (SpaceId::Namespace(9), 400, 300),
        ],
    );
    batch.put_record(Table::Meta, "candidate_marker", b"large block 3".to_vec());
    batch
}

pub fn run_growing(select: Option<Select<'_>>, seeds: u64) -> Tally {
    let config = growing_config();
    let (image, parents) = parent_image(config);
    let candidate = growing_candidate(parents.last().unwrap().new_head);
    run_spilling(
        config,
        &Start::Reopened(&image),
        &parents,
        &candidate,
        select,
        seeds,
    )
}

/// The growing candidate after twelve parents committed in the same session.
pub fn run_growing_steady(select: Option<Select<'_>>, seeds: u64) -> Tally {
    let parents = steady_parents();
    let candidate = growing_candidate(parents.last().unwrap().new_head);
    run_spilling(
        growing_config(),
        &Start::SameSession,
        &parents,
        &candidate,
        select,
        seeds,
    )
}

fn growing_config() -> RedbConfig {
    RedbConfig {
        cache_size: 32 << 10,
        quick_repair: true,
    }
}

fn run_spilling(
    config: RedbConfig,
    start: &Start<'_>,
    parents: &[CommitBatch],
    candidate: &CommitBatch,
    select: Option<Select<'_>>,
    seeds: u64,
) -> Tally {
    let mut modes = vec![CrashMode::DurableOnly, CrashMode::AllUnsynced];
    for seed in 0..seeds {
        modes.push(CrashMode::Torn {
            seed: 7 + seed,
            granularity: if seed % 2 == 0 {
                Granularity::Sector
            } else {
                Granularity::Byte
            },
        });
    }
    let tally = run_crash_points(config, start, parents, candidate, &modes, select);
    assert_eq!(tally.committed_outcomes, 1);
    assert!(
        tally.set_lens_per_commit > 0,
        "the commit must grow the file"
    );
    assert!(
        tally.not_committed_outcomes > 0,
        "pages must spill to the file before commit()"
    );
    assert_eq!(tally.full_repairs, 0);
    assert!(tally.parent > 0 && tally.candidate > 0);
    tally
}
