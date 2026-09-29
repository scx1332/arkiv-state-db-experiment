//! The behavioural suite every [`VersionedStore`] implementation runs.
//!
//! Each public function is one case. A store crate instantiates all of them
//! as `#[test]`s with [`versioned_store_contract!`](crate::versioned_store_contract),
//! passing a [`StoreFactory`]. The expected answers are written into the
//! cases (a store must return exactly the bytes it was given, the head it
//! was told, and the outcomes the contract defines); nothing is predicted by
//! calling another store.
//!
//! Cases whose names start with `faulty_` wrap the factory's store in a
//! [`FaultyStore`] and check the failure contract: latching, reconciliation
//! of indeterminate commits, lost acknowledgements, disk full, scoped read
//! faults and retryable recovery.

use std::{collections::BTreeMap, sync::Arc};

use crate::{
    testing::{CommitFault, DiskFull, FaultyStore, ReadScope},
    versioned::{
        CommitBatch, CommitOutcome, Head, Health, ObjectId, ObjectReader, Reconciliation, SpaceId,
        StorageErrorKind, Table, VersionedStore, height_key, reconcile,
    },
};

/// A store under test and, for durable stores, a way to reopen it.
pub struct TestStore {
    /// The store, freshly created and empty.
    pub store: Box<dyn VersionedStore>,
    /// Opens the same durable state again. The caller drops `store` (and
    /// every other handle) first. `None` for volatile stores. It may own the
    /// storage's resources (such as a directory), so callers keep it alive as
    /// long as any store handle.
    pub reopen: Option<Box<dyn Fn() -> Box<dyn VersionedStore>>>,
}

/// Creates the stores a contract run uses.
pub trait StoreFactory {
    /// The content address of `bytes` that the store expects (`keccak256`
    /// for stores that verify it).
    fn object_id(&self, bytes: &[u8]) -> ObjectId;

    /// A fresh, empty store.
    fn create(&self) -> TestStore;
}

/// Generates one `#[test]` per contract case for a [`StoreFactory`] value.
///
/// ```text
/// mod memory_contract {
///     arkiv_db_storage::versioned_store_contract!(super::MemoryFactory);
/// }
/// ```
#[macro_export]
macro_rules! versioned_store_contract {
    ($factory:expr) => {
        $crate::versioned_store_contract!(@cases $factory;
            empty_store_answers_absent,
            first_commit_stores_everything,
            head_compare_and_set,
            objects_put_if_absent,
            object_spaces_are_separate,
            records_are_insert_only,
            batch_duplicates,
            malformed_batches_are_not_committed,
            empty_batch_advances_head,
            retry_after_commit_is_rejected,
            reconcile_distinguishes_outcomes,
            history_stays_readable,
            reopen_preserves_state,
            stats_count_reads_per_space,
            concurrent_reads_during_commits,
            load_object_reports_missing,
            bounded_reads_enforce_lengths,
            bounded_reads_preserve_counters,
            recover_is_idempotent,
            faulty_without_faults_is_transparent,
            faulty_latching_fails_every_call_until_recover,
            faulty_latching_kinds_follow_the_redb_rule,
            faulty_not_committed_applies_nothing,
            faulty_indeterminate_applied_reconciles_committed,
            faulty_indeterminate_not_applied_retries_once,
            faulty_ack_lost_keeps_store_available,
            faulty_space_read_faults_are_scoped,
            faulty_bounded_reads_preserve_faults,
            faulty_disk_full_then_retry_commits_once,
            faulty_failed_recovery_is_retryable,
        );
    };
    (@cases $factory:expr; $($case:ident),* $(,)?) => {
        $(
            #[test]
            fn $case() {
                $crate::testing::contract::$case(&$factory);
            }
        )*
    };
}

/// A deterministic head for `height`, distinct per height and `fork`.
pub fn head(height: u64, fork: u8) -> Head {
    let mut hash = [fork; 32];
    hash[..8].copy_from_slice(&height.to_be_bytes());
    Head { height, hash }
}

/// A deterministic object of about `len` bytes for `tag` and `n`.
pub fn object(factory: &dyn StoreFactory, tag: &str, n: u64, len: usize) -> (ObjectId, Vec<u8>) {
    let mut bytes = format!("{tag}:{n}:").into_bytes();
    let mut filler = n.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    while bytes.len() < len {
        filler ^= filler << 13;
        filler ^= filler >> 7;
        filler ^= filler << 17;
        bytes.push(filler as u8);
    }
    (factory.object_id(&bytes), bytes)
}

fn committed(outcome: CommitOutcome) {
    assert_eq!(outcome, CommitOutcome::Committed);
}

fn not_committed(outcome: CommitOutcome, kind: StorageErrorKind) {
    match outcome {
        CommitOutcome::NotCommitted(error) => assert_eq!(error.kind, kind, "{error}"),
        other => panic!("expected NotCommitted({kind:?}), got {other:?}"),
    }
}

fn indeterminate(outcome: CommitOutcome, kind: StorageErrorKind) {
    match outcome {
        CommitOutcome::Indeterminate(error) => assert_eq!(error.kind, kind, "{error}"),
        other => panic!("expected Indeterminate({kind:?}), got {other:?}"),
    }
}

fn read(store: &dyn VersionedStore, space: SpaceId, id: &ObjectId) -> Option<Vec<u8>> {
    store.read_object(space, id).expect("read_object")
}

fn record(store: &dyn VersionedStore, table: Table, key: &[u8]) -> Option<Vec<u8>> {
    store.read_record(table, key).expect("read_record")
}

/// A batch at `height` on `parent` with one block, manifest and index record
/// and `objects` objects in namespace 1.
fn block_batch(
    factory: &dyn StoreFactory,
    parent: Option<Head>,
    height: u64,
    objects: u64,
) -> CommitBatch {
    let new = head(height, 1);
    let mut batch = CommitBatch::new(parent, new);
    for n in 0..objects {
        let (id, bytes) = object(
            factory,
            &format!("h{height}"),
            n,
            40 + (n as usize % 7) * 30,
        );
        batch.put_object(SpaceId::Namespace(1), id, bytes);
    }
    batch
        .put_record(
            Table::Blocks,
            height_key(height),
            format!("block {height}").into_bytes(),
        )
        .put_record(
            Table::Manifests,
            height_key(height),
            format!("manifest {height}").into_bytes(),
        )
        .put_record(Table::BlockIndex, new.hash, height_key(height));
    batch
}

/// Every read on an empty store answers "absent", and the store is healthy.
pub fn empty_store_answers_absent(factory: &dyn StoreFactory) {
    let test = factory.create();
    let store = test.store.as_ref();
    assert_eq!(store.head().unwrap(), None);
    assert_eq!(store.health(), Health::Available);
    for space in [
        SpaceId::Catalog,
        SpaceId::Host,
        SpaceId::Namespace(0),
        SpaceId::Namespace(u64::MAX),
    ] {
        assert_eq!(read(store, space, &[7; 32]), None);
    }
    for table in Table::ALL {
        let key: &[u8] = match table {
            Table::Meta => b"genesis_hash",
            Table::BlockIndex => &[1; 32],
            Table::Inclusions => b"request",
            Table::Blocks | Table::Manifests => &[0; 8],
        };
        assert_eq!(record(store, table, key), None);
    }
    assert_eq!(store.database_id(), store.database_id());
    assert_eq!(store.recover().unwrap(), None);
    assert_eq!(store.health(), Health::Available);
}

/// The first commit (no expected head) stores objects in every kind of space,
/// records in every table and the head.
pub fn first_commit_stores_everything(factory: &dyn StoreFactory) {
    let test = factory.create();
    let store = test.store.as_ref();
    let genesis = head(0, 1);
    let mut batch = CommitBatch::new(None, genesis);
    let spaces = [
        SpaceId::Catalog,
        SpaceId::Host,
        SpaceId::Namespace(1),
        SpaceId::Namespace(u64::MAX),
    ];
    let mut expected = Vec::new();
    for (index, space) in spaces.into_iter().enumerate() {
        let (id, bytes) = object(factory, "genesis", index as u64, 100 + index * 900);
        batch.put_object(space, id, bytes.clone());
        expected.push((space, id, bytes));
    }
    batch
        .put_record(Table::Meta, "genesis_hash", genesis.hash.to_vec())
        .put_record(Table::Meta, "chain_id", 7u64.to_be_bytes().to_vec())
        .put_record(Table::Blocks, height_key(0), b"block zero".to_vec())
        .put_record(Table::Manifests, height_key(0), b"manifest zero".to_vec())
        .put_record(Table::Inclusions, b"no request".to_vec(), Vec::new())
        .put_record(Table::BlockIndex, genesis.hash, height_key(0));
    committed(store.commit(batch));

    assert_eq!(store.head().unwrap(), Some(genesis));
    for (space, id, bytes) in &expected {
        assert_eq!(read(store, *space, id).as_ref(), Some(bytes));
        assert_eq!(store.load_object(*space, id).unwrap(), *bytes);
    }
    assert_eq!(
        record(store, Table::Meta, b"genesis_hash"),
        Some(genesis.hash.to_vec())
    );
    assert_eq!(
        record(store, Table::Meta, b"chain_id"),
        Some(7u64.to_be_bytes().to_vec())
    );
    assert_eq!(
        record(store, Table::Blocks, &height_key(0)),
        Some(b"block zero".to_vec())
    );
    assert_eq!(
        record(store, Table::Manifests, &height_key(0)),
        Some(b"manifest zero".to_vec())
    );
    assert_eq!(
        record(store, Table::Inclusions, b"no request"),
        Some(Vec::new())
    );
    assert_eq!(
        record(store, Table::BlockIndex, &genesis.hash),
        Some(height_key(0).to_vec())
    );
    assert_eq!(store.health(), Health::Available);
}

/// A commit applies only on the expected head; a mismatch applies nothing.
pub fn head_compare_and_set(factory: &dyn StoreFactory) {
    let test = factory.create();
    let store = test.store.as_ref();

    // No head yet: an expected head is a mismatch.
    let wrong = block_batch(factory, Some(head(0, 9)), 1, 3);
    let wrong_ids: Vec<_> = wrong.objects.iter().map(|(_, id, _)| *id).collect();
    not_committed(store.commit(wrong), StorageErrorKind::HeadMismatch);
    assert_eq!(store.head().unwrap(), None);
    for id in &wrong_ids {
        assert_eq!(read(store, SpaceId::Namespace(1), id), None);
    }
    assert_eq!(record(store, Table::Blocks, &height_key(1)), None);

    committed(store.commit(block_batch(factory, None, 0, 2)));
    let genesis = head(0, 1);

    // A head exists: `None` and a different head are mismatches.
    not_committed(
        store.commit(block_batch(factory, None, 1, 1)),
        StorageErrorKind::HeadMismatch,
    );
    let fork = block_batch(factory, Some(head(0, 2)), 1, 1);
    let fork_id = fork.objects[0].1;
    not_committed(store.commit(fork), StorageErrorKind::HeadMismatch);
    assert_eq!(store.head().unwrap(), Some(genesis));
    assert_eq!(read(store, SpaceId::Namespace(1), &fork_id), None);

    committed(store.commit(block_batch(factory, Some(genesis), 1, 1)));
    assert_eq!(store.head().unwrap(), Some(head(1, 1)));

    // The old head is now stale.
    not_committed(
        store.commit(block_batch(factory, Some(genesis), 2, 1)),
        StorageErrorKind::HeadMismatch,
    );
    assert_eq!(store.head().unwrap(), Some(head(1, 1)));
    assert_eq!(record(store, Table::Blocks, &height_key(2)), None);
}

/// Objects are put if absent: the same bytes again are idempotent, different
/// bytes under a committed id reject the whole batch.
pub fn objects_put_if_absent(factory: &dyn StoreFactory) {
    let test = factory.create();
    let store = test.store.as_ref();
    let (id, bytes) = object(factory, "shared", 1, 300);
    let mut first = CommitBatch::new(None, head(0, 1));
    first.put_object(SpaceId::Namespace(3), id, bytes.clone());
    committed(store.commit(first));

    let mut again = CommitBatch::new(Some(head(0, 1)), head(1, 1));
    let (other, other_bytes) = object(factory, "other", 1, 50);
    again
        .put_object(SpaceId::Namespace(3), id, bytes.clone())
        .put_object(SpaceId::Namespace(3), other, other_bytes.clone());
    committed(store.commit(again));
    assert_eq!(read(store, SpaceId::Namespace(3), &id), Some(bytes.clone()));
    assert_eq!(
        read(store, SpaceId::Namespace(3), &other),
        Some(other_bytes)
    );

    let mut conflict = CommitBatch::new(Some(head(1, 1)), head(2, 1));
    let (fresh, fresh_bytes) = object(factory, "fresh", 1, 50);
    conflict
        .put_object(SpaceId::Namespace(3), fresh, fresh_bytes)
        .put_object(SpaceId::Namespace(3), id, b"different bytes".to_vec())
        .put_record(Table::Blocks, height_key(2), b"block".to_vec());
    not_committed(store.commit(conflict), StorageErrorKind::Conflict);
    assert_eq!(store.head().unwrap(), Some(head(1, 1)));
    assert_eq!(read(store, SpaceId::Namespace(3), &id), Some(bytes));
    assert_eq!(read(store, SpaceId::Namespace(3), &fresh), None);
    assert_eq!(record(store, Table::Blocks, &height_key(2)), None);
}

/// The same object id in different spaces names different objects.
pub fn object_spaces_are_separate(factory: &dyn StoreFactory) {
    let test = factory.create();
    let store = test.store.as_ref();
    // The same object committed to two spaces is stored twice, and a space
    // never answers from another space's table.
    let (id, bytes) = object(factory, "space", 1, 64);
    let mut batch = CommitBatch::new(None, head(0, 1));
    batch
        .put_object(SpaceId::Namespace(1), id, bytes.clone())
        .put_object(SpaceId::Catalog, id, bytes.clone());
    committed(store.commit(batch));
    assert_eq!(read(store, SpaceId::Namespace(1), &id), Some(bytes.clone()));
    assert_eq!(read(store, SpaceId::Catalog, &id), Some(bytes));
    assert_eq!(read(store, SpaceId::Namespace(2), &id), None);
    assert_eq!(read(store, SpaceId::Host, &id), None);
}

/// Records are insert-only: identical is idempotent, different rejects the
/// whole batch.
pub fn records_are_insert_only(factory: &dyn StoreFactory) {
    let test = factory.create();
    let store = test.store.as_ref();
    let mut first = CommitBatch::new(None, head(0, 1));
    first
        .put_record(Table::Inclusions, b"actor/1".to_vec(), b"height 0".to_vec())
        .put_record(Table::Meta, "chain_id", b"7".to_vec());
    committed(store.commit(first));

    let mut same = CommitBatch::new(Some(head(0, 1)), head(1, 1));
    same.put_record(Table::Inclusions, b"actor/1".to_vec(), b"height 0".to_vec())
        .put_record(Table::Inclusions, b"actor/2".to_vec(), b"height 1".to_vec());
    committed(store.commit(same));

    let (id, bytes) = object(factory, "late", 1, 32);
    let mut different = CommitBatch::new(Some(head(1, 1)), head(2, 1));
    different.put_object(SpaceId::Host, id, bytes).put_record(
        Table::Meta,
        "chain_id",
        b"8".to_vec(),
    );
    not_committed(store.commit(different), StorageErrorKind::Conflict);
    assert_eq!(store.head().unwrap(), Some(head(1, 1)));
    assert_eq!(record(store, Table::Meta, b"chain_id"), Some(b"7".to_vec()));
    assert_eq!(read(store, SpaceId::Host, &id), None);
    assert_eq!(
        record(store, Table::Inclusions, b"actor/2"),
        Some(b"height 1".to_vec())
    );
}

/// Identical duplicates inside a batch are idempotent; conflicting ones reject
/// the batch.
pub fn batch_duplicates(factory: &dyn StoreFactory) {
    let test = factory.create();
    let store = test.store.as_ref();
    let (id, bytes) = object(factory, "dup", 1, 70);
    let mut batch = CommitBatch::new(None, head(0, 1));
    batch
        .put_object(SpaceId::Host, id, bytes.clone())
        .put_object(SpaceId::Host, id, bytes.clone())
        .put_record(Table::Blocks, height_key(0), b"b".to_vec())
        .put_record(Table::Blocks, height_key(0), b"b".to_vec());
    committed(store.commit(batch));
    assert_eq!(read(store, SpaceId::Host, &id), Some(bytes.clone()));

    let mut conflict = CommitBatch::new(Some(head(0, 1)), head(1, 1));
    let (fresh, fresh_bytes) = object(factory, "dup", 2, 70);
    conflict
        .put_object(SpaceId::Host, fresh, fresh_bytes.clone())
        .put_object(SpaceId::Host, fresh, [fresh_bytes, vec![0]].concat());
    not_committed(store.commit(conflict), StorageErrorKind::Conflict);
    assert_eq!(read(store, SpaceId::Host, &fresh), None);
    assert_eq!(store.head().unwrap(), Some(head(0, 1)));
}

/// Malformed record shapes, reserved metadata names and non-advancing heads
/// are `Incompatible` and apply nothing.
pub fn malformed_batches_are_not_committed(factory: &dyn StoreFactory) {
    let test = factory.create();
    let store = test.store.as_ref();
    committed(store.commit(CommitBatch::new(None, head(0, 1))));
    let parent = Some(head(0, 1));
    let malformed: [(Table, &[u8], &[u8]); 6] = [
        (Table::Blocks, &[1; 7], b"x"),
        (Table::Manifests, &[1; 9], b"x"),
        (Table::BlockIndex, &[1; 31], &[0; 8]),
        (Table::BlockIndex, &[1; 32], &[0; 7]),
        (Table::Meta, b"store/head", b"x"),
        (Table::Inclusions, b"", b"x"),
    ];
    for (table, key, value) in malformed {
        let mut batch = CommitBatch::new(parent, head(1, 1));
        batch.put_record(Table::Blocks, height_key(1), b"block".to_vec());
        batch.put_record(table, key.to_vec(), value.to_vec());
        not_committed(store.commit(batch), StorageErrorKind::Incompatible);
        assert_eq!(record(store, Table::Blocks, &height_key(1)), None);
    }
    not_committed(
        store.commit(CommitBatch::new(parent, head(0, 2))),
        StorageErrorKind::Incompatible,
    );
    assert_eq!(store.head().unwrap(), parent);
    assert_eq!(store.health(), Health::Available);
}

/// A batch without objects or records still moves the head.
pub fn empty_batch_advances_head(factory: &dyn StoreFactory) {
    let test = factory.create();
    let store = test.store.as_ref();
    committed(store.commit(CommitBatch::new(None, head(5, 1))));
    assert_eq!(store.head().unwrap(), Some(head(5, 1)));
    committed(store.commit(CommitBatch::new(Some(head(5, 1)), head(9, 1))));
    assert_eq!(store.head().unwrap(), Some(head(9, 1)));
}

/// Re-submitting a committed batch is a head mismatch: nothing is applied
/// twice.
pub fn retry_after_commit_is_rejected(factory: &dyn StoreFactory) {
    let test = factory.create();
    let store = test.store.as_ref();
    let batch = block_batch(factory, None, 0, 4);
    committed(store.commit(batch.clone()));
    let stats = store.stats();
    not_committed(store.commit(batch), StorageErrorKind::HeadMismatch);
    let after = store.stats().since(&stats);
    assert_eq!(after.committed, 0);
    assert_eq!(after.not_committed, 1);
    assert!(
        after
            .spaces
            .values()
            .all(|space| space.objects_written == 0)
    );
    assert_eq!(store.head().unwrap(), Some(head(0, 1)));
}

/// [`reconcile`] tells a committed batch, an absent batch and a divergent
/// head apart.
pub fn reconcile_distinguishes_outcomes(factory: &dyn StoreFactory) {
    let test = factory.create();
    let store = test.store.as_ref();
    let genesis = block_batch(factory, None, 0, 1);
    assert_eq!(
        reconcile(store, &genesis).unwrap(),
        Reconciliation::NotCommitted
    );
    committed(store.commit(genesis.clone()));
    assert_eq!(
        reconcile(store, &genesis).unwrap(),
        Reconciliation::Committed
    );

    let next = block_batch(factory, Some(head(0, 1)), 1, 1);
    assert_eq!(
        reconcile(store, &next).unwrap(),
        Reconciliation::NotCommitted
    );
    committed(store.commit(next.clone()));

    // Same new head, different block bytes: divergence, not success.
    let mut forged = next.clone();
    forged.records[0].2 = b"another block".to_vec();
    assert_eq!(
        reconcile(store, &forged).unwrap(),
        Reconciliation::Diverged {
            head: Some(head(1, 1))
        }
    );
    // A candidate on a head that is neither its parent nor its result.
    let mut stale = block_batch(factory, Some(head(0, 1)), 1, 2);
    stale.new_head = head(1, 7);
    assert_eq!(
        reconcile(store, &stale).unwrap(),
        Reconciliation::Diverged {
            head: Some(head(1, 1))
        }
    );
}

/// Objects and records of every block stay readable, unchanged, after later
/// blocks.
pub fn history_stays_readable(factory: &dyn StoreFactory) {
    let test = factory.create();
    let store = test.store.as_ref();
    let mut parent = None;
    let mut all = Vec::new();
    for height in 0..20 {
        let batch = block_batch(factory, parent, height, 1 + height % 4);
        all.extend(batch.objects.iter().cloned());
        committed(store.commit(batch));
        parent = Some(head(height, 1));
    }
    for (space, id, bytes) in &all {
        assert_eq!(read(store, *space, id).as_ref(), Some(bytes));
    }
    for height in 0..20 {
        assert_eq!(
            record(store, Table::Blocks, &height_key(height)),
            Some(format!("block {height}").into_bytes())
        );
        assert_eq!(
            record(store, Table::BlockIndex, &head(height, 1).hash),
            Some(height_key(height).to_vec())
        );
    }
}

/// Reopening a durable store preserves its identity, head, objects and
/// records. Skipped for volatile stores.
pub fn reopen_preserves_state(factory: &dyn StoreFactory) {
    let test = factory.create();
    let Some(reopen) = test.reopen else {
        return;
    };
    let store = test.store;
    let id = store.database_id();
    let mut parent = None;
    let mut all = Vec::new();
    for height in 0..5 {
        let batch = block_batch(factory, parent, height, 3);
        all.extend(batch.objects.iter().cloned());
        committed(store.commit(batch));
        parent = Some(head(height, 1));
    }
    drop(store);
    let store = reopen();
    assert_eq!(store.database_id(), id);
    assert_eq!(store.head().unwrap(), parent);
    for (space, object_id, bytes) in &all {
        assert_eq!(
            read(store.as_ref(), *space, object_id).as_ref(),
            Some(bytes)
        );
    }
    assert_eq!(
        record(store.as_ref(), Table::Manifests, &height_key(4)),
        Some(b"manifest 4".to_vec())
    );
    committed(store.commit(block_batch(factory, parent, 5, 1)));
    drop(store);
    let store = reopen();
    assert_eq!(store.head().unwrap(), Some(head(5, 1)));
}

/// Object reads are counted per space, found or not; other spaces stay
/// untouched.
pub fn stats_count_reads_per_space(factory: &dyn StoreFactory) {
    let test = factory.create();
    let store = test.store.as_ref();
    let (id, bytes) = object(factory, "stats", 1, 123);
    let mut batch = CommitBatch::new(None, head(0, 1));
    batch.put_object(SpaceId::Namespace(4), id, bytes);
    committed(store.commit(batch));
    let before = store.stats();
    assert_eq!(before.space(SpaceId::Namespace(4)).objects_written, 1);
    assert_eq!(before.space(SpaceId::Namespace(4)).bytes_written, 123);
    for _ in 0..3 {
        read(store, SpaceId::Namespace(4), &id);
    }
    read(store, SpaceId::Namespace(4), &[0; 32]);
    record(store, Table::Blocks, &height_key(0));
    store.head().unwrap();
    let delta = store.stats().since(&before);
    let space = delta.space(SpaceId::Namespace(4));
    assert_eq!(space.reads, 4);
    assert_eq!(space.hits, 3);
    assert_eq!(space.read_bytes, 369);
    assert_eq!(delta.spaces.len(), 1);
    assert_eq!(delta.record_reads, 1);
    assert_eq!(delta.head_reads, 1);
}

/// Readers running while commits happen always see complete, committed
/// objects and a head that only moves forward.
pub fn concurrent_reads_during_commits(factory: &dyn StoreFactory) {
    let test = factory.create();
    let store: Arc<dyn VersionedStore> = Arc::from(test.store);
    committed(store.commit(block_batch(factory, None, 0, 4)));
    let genesis_objects = block_batch(factory, None, 0, 4).objects;
    let batches: Vec<CommitBatch> = (1..=12)
        .map(|height| block_batch(factory, Some(head(height - 1, 1)), height, 6))
        .collect();
    std::thread::scope(|scope| {
        for _ in 0..3 {
            let store = Arc::clone(&store);
            let genesis_objects = &genesis_objects;
            let batches = &batches;
            scope.spawn(move || {
                let mut last = 0;
                for _ in 0..200 {
                    let now = store.head().unwrap().expect("genesis committed").height;
                    assert!(now >= last, "head moved backwards");
                    last = now;
                    for (space, id, bytes) in genesis_objects {
                        assert_eq!(read(store.as_ref(), *space, id).as_ref(), Some(bytes));
                    }
                    // Every object of every block at or below the observed
                    // head is complete.
                    for batch in batches.iter().take(now as usize) {
                        for (space, id, bytes) in &batch.objects {
                            assert_eq!(read(store.as_ref(), *space, id).as_ref(), Some(bytes));
                        }
                    }
                }
            });
        }
        for batch in &batches {
            committed(store.commit(batch.clone()));
        }
    });
    assert_eq!(store.head().unwrap(), Some(head(12, 1)));
}

/// `load_object` reports an absent referenced object as `Missing`.
pub fn load_object_reports_missing(factory: &dyn StoreFactory) {
    let test = factory.create();
    let store = test.store.as_ref();
    let error = store
        .load_object(SpaceId::Namespace(8), &[3; 32])
        .unwrap_err();
    assert_eq!(error.kind, StorageErrorKind::Missing);
}

/// Bounded reads distinguish absent, empty, exact-fit and oversized values
/// in every record table and object space, including through Arc/Box views.
pub fn bounded_reads_enforce_lengths(factory: &dyn StoreFactory) {
    let TestStore {
        store,
        reopen: _resources,
    } = factory.create();
    let store = Arc::new(store);
    let (id, bytes) = object(factory, "bounded", 1, 8192);
    let empty_id = factory.object_id(&[]);
    let mut batch = CommitBatch::new(None, head(0, 1));
    for space in [SpaceId::Catalog, SpaceId::Host, SpaceId::Namespace(2)] {
        batch.put_object(space, id, bytes.clone());
        batch.put_object(space, empty_id, Vec::new());
    }
    let records = [
        (Table::Meta, b"bounded/value".to_vec(), bytes.clone()),
        (Table::Blocks, height_key(0).to_vec(), bytes.clone()),
        (Table::Manifests, height_key(0).to_vec(), bytes.clone()),
        (Table::Inclusions, b"request".to_vec(), bytes.clone()),
        (Table::BlockIndex, id.to_vec(), height_key(0).to_vec()),
    ];
    for (table, key, value) in &records {
        batch.put_record(*table, key.clone(), value.clone());
    }
    batch.put_record(Table::Meta, b"bounded/empty".to_vec(), Vec::new());
    committed(store.commit(batch));

    for space in [SpaceId::Catalog, SpaceId::Host, SpaceId::Namespace(2)] {
        assert_eq!(
            store.read_object_bounded(space, &id, bytes.len()).unwrap(),
            Some(bytes.clone())
        );
        assert_eq!(
            store.load_object_bounded(space, &id, bytes.len()).unwrap(),
            bytes
        );
        for cap in [0, bytes.len() - 1] {
            assert_eq!(
                store.read_object_bounded(space, &id, cap).unwrap_err().kind,
                StorageErrorKind::Corrupt
            );
        }
        assert_eq!(
            store.read_object_bounded(space, &empty_id, 0).unwrap(),
            Some(Vec::new())
        );
    }
    assert_eq!(
        store
            .read_object_bounded(SpaceId::Namespace(3), &id, 0)
            .unwrap(),
        None
    );
    assert_eq!(
        store
            .load_object_bounded(SpaceId::Namespace(3), &id, 0)
            .unwrap_err()
            .kind,
        StorageErrorKind::Missing
    );
    for (table, key, value) in records {
        assert_eq!(
            store.read_record_bounded(table, &key, value.len()).unwrap(),
            Some(value.clone())
        );
        assert_eq!(
            store
                .read_record_bounded(table, &key, value.len() - 1)
                .unwrap_err()
                .kind,
            StorageErrorKind::Corrupt
        );
        assert_eq!(store.read_record(table, &key).unwrap(), Some(value));
    }
    assert_eq!(
        store
            .read_record_bounded(Table::Meta, b"bounded/empty", 0)
            .unwrap(),
        Some(Vec::new())
    );
    assert_eq!(
        store
            .read_record_bounded(Table::Manifests, &height_key(1), 0)
            .unwrap(),
        None
    );
    assert_eq!(store.health(), Health::Available);
}

/// Answered bounded reads count once; oversized values are errors with no
/// answered-read or byte counter, just like other failed storage reads.
pub fn bounded_reads_preserve_counters(factory: &dyn StoreFactory) {
    let test = factory.create();
    let store = test.store.as_ref();
    let (id, bytes) = object(factory, "bounded-counter", 1, 256);
    let space = SpaceId::Namespace(1);
    let mut batch = CommitBatch::new(None, head(0, 1));
    batch.put_object(space, id, bytes.clone());
    batch.put_record(Table::Manifests, height_key(0), bytes.clone());
    committed(store.commit(batch));
    let before = store.stats();
    assert!(store.read_object_bounded(space, &id, 255).is_err());
    assert!(
        store
            .read_record_bounded(Table::Manifests, &height_key(0), 255)
            .is_err()
    );
    let failed = store.stats().since(&before);
    assert_eq!(failed.object_reads(), 0);
    assert_eq!(failed.record_reads, 0);
    assert_eq!(failed.record_read_bytes, 0);
    store.read_object_bounded(space, &id, 256).unwrap();
    store.read_object_bounded(SpaceId::Host, &id, 0).unwrap();
    store
        .read_record_bounded(Table::Manifests, &height_key(0), 256)
        .unwrap();
    store
        .read_record_bounded(Table::Manifests, &height_key(1), 0)
        .unwrap();
    let read = store.stats().since(&before);
    assert_eq!(read.object_reads(), 2);
    assert_eq!(read.space(space).hits, 1);
    assert_eq!(read.space(space).read_bytes, 256);
    assert_eq!(read.space(SpaceId::Host).hits, 0);
    assert_eq!(read.record_reads, 2);
    assert_eq!(read.record_read_bytes, 256);
}

/// FaultyStore forwards limits and gates bounded calls before touching the
/// inner store, including absent lookups and failed-handle recovery.
pub fn faulty_bounded_reads_preserve_faults(factory: &dyn StoreFactory) {
    let (store, _resources) = faulty(factory);
    let batch = block_batch(factory, None, 0, 1);
    let (space, id, bytes) = batch.objects[0].clone();
    committed(store.commit(batch));
    store.fail_reads(ReadScope::Space(space), StorageErrorKind::Io, Some(1));
    let before = store.stats();
    let error = store.read_object_bounded(space, &id, 0).unwrap_err();
    assert_eq!(error.kind, StorageErrorKind::Io);
    assert_eq!(
        store
            .read_object_bounded(SpaceId::Host, &id, usize::MAX)
            .unwrap_err(),
        error
    );
    assert_eq!(
        store
            .read_record_bounded(Table::Manifests, &height_key(1), 0)
            .unwrap_err(),
        error
    );
    assert_eq!(store.stats().since(&before).object_reads(), 0);
    assert_eq!(store.stats().since(&before).record_reads, 0);
    assert_eq!(store.recover().unwrap(), Some(head(0, 1)));
    assert_eq!(
        store.load_object_bounded(space, &id, bytes.len()).unwrap(),
        bytes
    );
    store.fail_reads(ReadScope::Records, StorageErrorKind::Full, Some(1));
    let error = store
        .read_record_bounded(Table::Manifests, &height_key(0), 0)
        .unwrap_err();
    assert_eq!(error.kind, StorageErrorKind::Full);
    assert_eq!(
        store
            .read_object_bounded(space, &id, usize::MAX)
            .unwrap_err(),
        error
    );
    assert_eq!(store.recover().unwrap(), Some(head(0, 1)));
    assert_eq!(
        store
            .read_record_bounded(Table::Manifests, &height_key(0), 0)
            .unwrap_err()
            .kind,
        StorageErrorKind::Corrupt
    );
}

/// `recover` on a healthy store is idempotent and loses nothing.
pub fn recover_is_idempotent(factory: &dyn StoreFactory) {
    let test = factory.create();
    let store = test.store.as_ref();
    let batch = block_batch(factory, None, 0, 5);
    committed(store.commit(batch.clone()));
    assert_eq!(store.recover().unwrap(), Some(head(0, 1)));
    assert_eq!(store.recover().unwrap(), Some(head(0, 1)));
    assert_eq!(store.health(), Health::Available);
    for (space, id, bytes) in &batch.objects {
        assert_eq!(read(store, *space, id).as_ref(), Some(bytes));
    }
    committed(store.commit(block_batch(factory, Some(head(0, 1)), 1, 1)));
    assert!(store.stats().recoveries >= 2);
}

/// A latching `FaultyStore` around a fresh store, and the resources the
/// store needs (dropped after it).
type Faulty = (
    FaultyStore<Box<dyn VersionedStore>>,
    Option<Box<dyn Fn() -> Box<dyn VersionedStore>>>,
);

fn faulty(factory: &dyn StoreFactory) -> Faulty {
    let TestStore { store, reopen } = factory.create();
    (FaultyStore::latching(store), reopen)
}

/// A `FaultyStore` without armed faults behaves exactly like its inner store.
pub fn faulty_without_faults_is_transparent(factory: &dyn StoreFactory) {
    let TestStore {
        store,
        reopen: _resources,
    } = factory.create();
    let store = FaultyStore::new(store);
    let mut parent = None;
    for height in 0..4 {
        let batch = block_batch(factory, parent, height, 2);
        committed(store.commit(batch.clone()));
        for (space, id, bytes) in &batch.objects {
            assert_eq!(read(&store, *space, id).as_ref(), Some(bytes));
        }
        parent = Some(head(height, 1));
    }
    not_committed(
        store.commit(block_batch(factory, None, 9, 1)),
        StorageErrorKind::HeadMismatch,
    );
    assert_eq!(store.head().unwrap(), parent);
    assert_eq!(store.injected(), Default::default());
}

/// In latching mode an injected I/O failure fails every later call — reads,
/// head, records, commits — with the same error, never `None`, until
/// `recover` succeeds; nothing is lost.
pub fn faulty_latching_fails_every_call_until_recover(factory: &dyn StoreFactory) {
    let (store, _resources) = faulty(factory);
    let batch = block_batch(factory, None, 0, 3);
    committed(store.commit(batch.clone()));
    let (space, id, bytes) = batch.objects[0].clone();

    store.fail_reads(ReadScope::Space(space), StorageErrorKind::Io, Some(1));
    let error = store.read_object(space, &id).unwrap_err();
    assert_eq!(error.kind, StorageErrorKind::Io);
    assert_eq!(store.health(), Health::Failed(error.clone()));
    // The fault was one-shot, but the latch answers every call.
    assert_eq!(store.read_object(space, &id).unwrap_err(), error);
    assert_eq!(
        store.read_object(SpaceId::Catalog, &[0; 32]).unwrap_err(),
        error
    );
    assert_eq!(store.head().unwrap_err(), error);
    assert_eq!(
        store
            .read_record(Table::Blocks, &height_key(0))
            .unwrap_err(),
        error
    );
    assert_eq!(
        store.load_object(space, &id).unwrap_err().kind,
        StorageErrorKind::Io
    );
    assert_eq!(
        store.commit(block_batch(factory, Some(head(0, 1)), 1, 1)),
        CommitOutcome::NotCommitted(error)
    );
    assert!(store.injected().latched_calls >= 6);

    assert_eq!(store.recover().unwrap(), Some(head(0, 1)));
    assert_eq!(store.health(), Health::Available);
    assert_eq!(read(&store, space, &id), Some(bytes));
    committed(store.commit(block_batch(factory, Some(head(0, 1)), 1, 1)));
}

/// Latching mode latches what a `RedbStore` latches: `Io` and `Full` on any
/// call, every `Indeterminate` outcome whatever its kind; not `Corrupt`
/// (unless turned on) or a conflict before the commit, and never a lost
/// acknowledgement.
pub fn faulty_latching_kinds_follow_the_redb_rule(factory: &dyn StoreFactory) {
    let (store, _resources) = faulty(factory);
    let genesis = block_batch(factory, None, 0, 2);
    committed(store.commit(genesis.clone()));
    let (space, id, bytes) = genesis.objects[0].clone();

    // Damaged bytes are reported on the call that found them only.
    store.fail_reads(ReadScope::Space(space), StorageErrorKind::Corrupt, Some(1));
    assert_eq!(
        store.read_object(space, &id).unwrap_err().kind,
        StorageErrorKind::Corrupt
    );
    assert_eq!(store.health(), Health::Available);
    assert_eq!(read(&store, space, &id), Some(bytes.clone()));

    // A refusal before the commit starts does not latch, unless it is I/O.
    for kind in [
        StorageErrorKind::Conflict,
        StorageErrorKind::Incompatible,
        StorageErrorKind::Corrupt,
    ] {
        store.push_commit_fault(CommitFault::NotCommitted(kind));
        not_committed(
            store.commit(block_batch(factory, Some(head(0, 1)), 1, 1)),
            kind,
        );
        assert_eq!(store.health(), Health::Available, "{kind:?}");
    }

    // A lost acknowledgement never latches.
    let next = block_batch(factory, Some(head(0, 1)), 1, 1);
    store.push_commit_fault(CommitFault::AckLost);
    indeterminate(store.commit(next.clone()), StorageErrorKind::Io);
    assert_eq!(store.health(), Health::Available);

    // Any indeterminate commit latches, whatever its kind.
    let after = block_batch(factory, Some(head(1, 1)), 2, 1);
    store.push_commit_fault(CommitFault::IndeterminateNotApplied(
        StorageErrorKind::Incompatible,
    ));
    let outcome = store.commit(after.clone());
    let CommitOutcome::Indeterminate(error) = outcome else {
        panic!("expected Indeterminate(Incompatible), got {outcome:?}");
    };
    assert_eq!(error.kind, StorageErrorKind::Incompatible);
    assert_eq!(store.health(), Health::Failed(error.clone()));
    assert_eq!(store.head().unwrap_err(), error);
    assert_eq!(
        reconcile(&store, &after).unwrap(),
        Reconciliation::NotCommitted
    );

    // With `Corrupt` latching on, damaged bytes latch too.
    store.set_latch_corrupt(true);
    store.fail_reads(ReadScope::Records, StorageErrorKind::Corrupt, Some(1));
    let error = store
        .read_record(Table::Blocks, &height_key(0))
        .unwrap_err();
    assert_eq!(error.kind, StorageErrorKind::Corrupt);
    assert_eq!(store.health(), Health::Failed(error.clone()));
    assert_eq!(store.read_object(space, &id).unwrap_err(), error);
    assert_eq!(store.recover().unwrap(), Some(head(1, 1)));
    committed(store.commit(after));
}

/// `NotCommitted` before apply leaves durable state untouched and the same
/// batch then commits.
pub fn faulty_not_committed_applies_nothing(factory: &dyn StoreFactory) {
    let (store, _resources) = faulty(factory);
    store.set_latching(false);
    let batch = block_batch(factory, None, 0, 3);
    store.push_commit_fault(CommitFault::NotCommitted(StorageErrorKind::Io));
    not_committed(store.commit(batch.clone()), StorageErrorKind::Io);
    assert_eq!(store.health(), Health::Available);
    assert_eq!(store.head().unwrap(), None);
    for (space, id, _) in &batch.objects {
        assert_eq!(read(&store, *space, id), None);
    }
    committed(store.commit(batch));
}

/// An `Indeterminate` commit that did apply reconciles as committed; the
/// batch is never applied twice.
pub fn faulty_indeterminate_applied_reconciles_committed(factory: &dyn StoreFactory) {
    let (store, _resources) = faulty(factory);
    committed(store.commit(block_batch(factory, None, 0, 1)));
    let batch = block_batch(factory, Some(head(0, 1)), 1, 4);
    store.push_commit_fault(CommitFault::IndeterminateApplied(StorageErrorKind::Io));
    indeterminate(store.commit(batch.clone()), StorageErrorKind::Io);
    assert!(matches!(store.health(), Health::Failed(_)));
    assert_eq!(
        reconcile(&store, &batch).unwrap(),
        Reconciliation::Committed
    );
    assert_eq!(store.health(), Health::Available);
    let before = store.stats();
    not_committed(store.commit(batch), StorageErrorKind::HeadMismatch);
    assert_eq!(store.stats().since(&before).committed, 0);
    assert_eq!(store.injected().commits_applied, 1);
}

/// An `Indeterminate` commit that did not apply reconciles as absent; the
/// identical batch then commits exactly once.
pub fn faulty_indeterminate_not_applied_retries_once(factory: &dyn StoreFactory) {
    let (store, _resources) = faulty(factory);
    committed(store.commit(block_batch(factory, None, 0, 1)));
    let batch = block_batch(factory, Some(head(0, 1)), 1, 4);
    store.push_commit_fault(CommitFault::IndeterminateNotApplied(StorageErrorKind::Io));
    indeterminate(store.commit(batch.clone()), StorageErrorKind::Io);
    assert_eq!(
        reconcile(&store, &batch).unwrap(),
        Reconciliation::NotCommitted
    );
    committed(store.commit(batch.clone()));
    not_committed(store.commit(batch.clone()), StorageErrorKind::HeadMismatch);
    assert_eq!(
        reconcile(&store, &batch).unwrap(),
        Reconciliation::Committed
    );
}

/// A lost acknowledgement is indeterminate for the caller but leaves the
/// store available; reconciliation finds the batch.
pub fn faulty_ack_lost_keeps_store_available(factory: &dyn StoreFactory) {
    let (store, _resources) = faulty(factory);
    let batch = block_batch(factory, None, 0, 2);
    store.push_commit_fault(CommitFault::AckLost);
    indeterminate(store.commit(batch.clone()), StorageErrorKind::Io);
    assert_eq!(store.health(), Health::Available);
    assert_eq!(store.head().unwrap(), Some(head(0, 1)));
    assert_eq!(
        reconcile(&store, &batch).unwrap(),
        Reconciliation::Committed
    );
}

/// A read fault scoped to one space fails only that space's reads and never
/// answers "absent".
pub fn faulty_space_read_faults_are_scoped(factory: &dyn StoreFactory) {
    let (store, _resources) = faulty(factory);
    store.set_latching(false);
    let (a, a_bytes) = object(factory, "a", 1, 40);
    let (b, b_bytes) = object(factory, "b", 1, 40);
    let mut batch = CommitBatch::new(None, head(0, 1));
    batch
        .put_object(SpaceId::Namespace(1), a, a_bytes.clone())
        .put_object(SpaceId::Namespace(2), b, b_bytes.clone());
    committed(store.commit(batch));
    store.fail_reads(
        ReadScope::Space(SpaceId::Namespace(2)),
        StorageErrorKind::Corrupt,
        None,
    );
    for _ in 0..3 {
        assert_eq!(
            read(&store, SpaceId::Namespace(1), &a),
            Some(a_bytes.clone())
        );
        assert_eq!(
            store
                .read_object(SpaceId::Namespace(2), &b)
                .unwrap_err()
                .kind,
            StorageErrorKind::Corrupt
        );
        // Even an id that does not exist is an error, not `None`.
        assert_eq!(
            store
                .read_object(SpaceId::Namespace(2), &[0; 32])
                .unwrap_err()
                .kind,
            StorageErrorKind::Corrupt
        );
    }
    assert_eq!(store.head().unwrap(), Some(head(0, 1)));
    store.clear_read_faults();
    assert_eq!(read(&store, SpaceId::Namespace(2), &b), Some(b_bytes));
    assert_eq!(store.injected().reads_failed, 6);
}

/// Disk full, before the commit starts (`NotCommitted(Full)`) or inside it
/// (`Indeterminate(Full)`): latched, nothing applied, reconciled as absent;
/// while still full, recovery works but commits fail again; once space is
/// back the identical batch commits exactly once.
pub fn faulty_disk_full_then_retry_commits_once(factory: &dyn StoreFactory) {
    let (store, _resources) = faulty(factory);
    committed(store.commit(block_batch(factory, None, 0, 1)));
    let mut parent = head(0, 1);
    for (height, full) in [(1, DiskFull::BeforeCommit), (2, DiskFull::DuringCommit)] {
        let batch = block_batch(factory, Some(parent), height, 3);
        store.set_disk_full(Some(full));
        for _ in 0..2 {
            let outcome = store.commit(batch.clone());
            match full {
                DiskFull::BeforeCommit => not_committed(outcome, StorageErrorKind::Full),
                DiskFull::DuringCommit => indeterminate(outcome, StorageErrorKind::Full),
            }
            assert!(matches!(
                store.health(),
                Health::Failed(error) if error.kind == StorageErrorKind::Full
            ));
            assert_eq!(
                reconcile(&store, &batch).unwrap(),
                Reconciliation::NotCommitted
            );
            for (space, id, _) in &batch.objects {
                assert_eq!(read(&store, *space, id), None);
            }
        }
        store.set_disk_full(None);
        committed(store.commit(batch.clone()));
        not_committed(store.commit(batch), StorageErrorKind::HeadMismatch);
        parent = head(height, 1);
        assert_eq!(store.head().unwrap(), Some(parent));
    }
}

/// A failed recovery latches its own error in place of the earlier one —
/// from a failed store and from a healthy one — and can be retried.
pub fn faulty_failed_recovery_is_retryable(factory: &dyn StoreFactory) {
    let (store, _resources) = faulty(factory);
    committed(store.commit(block_batch(factory, None, 0, 1)));
    store.fail_reads(ReadScope::Head, StorageErrorKind::Io, Some(1));
    let error = store.head().unwrap_err();
    assert_eq!(store.health(), Health::Failed(error.clone()));

    store.fail_next_recoveries(2, StorageErrorKind::Io);
    let recovery_error = store.recover().unwrap_err();
    assert_eq!(recovery_error.kind, StorageErrorKind::Io);
    assert_ne!(recovery_error, error);
    assert_eq!(store.health(), Health::Failed(recovery_error.clone()));
    assert_eq!(store.head().unwrap_err(), recovery_error);
    assert_eq!(store.recover().unwrap_err(), recovery_error);
    assert_eq!(
        store
            .read_record(Table::Blocks, &height_key(0))
            .unwrap_err(),
        recovery_error
    );
    assert_eq!(store.recover().unwrap(), Some(head(0, 1)));
    assert_eq!(store.health(), Health::Available);

    // A healthy store whose recovery fails is failed afterwards.
    store.fail_next_recoveries(1, StorageErrorKind::Corrupt);
    let recovery_error = store.recover().unwrap_err();
    assert_eq!(recovery_error.kind, StorageErrorKind::Corrupt);
    assert_eq!(store.health(), Health::Failed(recovery_error.clone()));
    assert_eq!(
        store.read_object(SpaceId::Catalog, &[0; 32]).unwrap_err(),
        recovery_error
    );
    assert_eq!(store.recover().unwrap(), Some(head(0, 1)));
    assert_eq!(store.health(), Health::Available);
    assert_eq!(store.injected().recoveries_failed, 3);
}

/// Collects `(space, id) → bytes` for a set of batches (helper for store
/// crates' own tests).
pub fn objects_of(batches: &[CommitBatch]) -> BTreeMap<(SpaceId, ObjectId), Vec<u8>> {
    batches
        .iter()
        .flat_map(|batch| batch.objects.iter())
        .map(|(space, id, bytes)| ((*space, *id), bytes.clone()))
        .collect()
}
