//! `RedbStore::relabel`: a private store's whole publication is replaced in
//! one durable redb transaction, its objects are kept, and a crash at any
//! point of that transaction leaves exactly the old or exactly the new
//! publication, never a mix.

mod common;

use arkiv_db_redb::{
    RedbConfig, RedbStore,
    testing::{CrashDisk, CrashMode},
};
use arkiv_db_storage::versioned::{
    CommitBatch, CommitOutcome, Head, Relabel, SpaceId, Table, VersionedStore, height_key,
};
use common::{
    BenchDir, block, commit,
    crash::{SEEDS, assert_integrity, config, modes},
    crash_store, head, object,
};

/// A staged-like store's only publication, and its replacement under
/// another head at the same height.
fn publication() -> (CommitBatch, Relabel) {
    let mut first = block(
        None,
        0,
        1,
        &[
            (SpaceId::Catalog, 3, 120),
            (SpaceId::Host, 2, 64),
            (SpaceId::Namespace(1), 40, 180),
            (SpaceId::Namespace(2), 5, 9_000),
        ],
    );
    first
        .put_record(Table::Meta, "engine/profile", b"staged".to_vec())
        .put_record(Table::Meta, "engine/genesis", b"staged genesis".to_vec());
    let new_head = head(0, 2);
    let mut batch = CommitBatch::new(None, new_head);
    let (id, bytes) = object(77, 1, 300);
    batch
        .put_object(SpaceId::Host, id, bytes)
        .put_record(Table::Meta, "engine/profile", b"node".to_vec())
        .put_record(Table::Meta, "simulator/identity", b"identity".to_vec())
        .put_record(Table::Manifests, height_key(0), b"node manifest".to_vec())
        .put_record(Table::BlockIndex, new_head.hash, height_key(0))
        .put_record(Table::Blocks, height_key(0), b"genesis block".to_vec());
    let relabel = Relabel {
        expected_head: first.new_head,
        retired: first.records.clone(),
        batch,
    };
    (first, relabel)
}

fn is_old(store: &dyn VersionedStore, first: &CommitBatch, relabel: &Relabel) -> bool {
    store.head().unwrap() == Some(first.new_head)
        && relabel
            .retired
            .iter()
            .all(|(t, k, v)| store.read_record(*t, k).unwrap().as_ref() == Some(v))
        && relabel
            .batch
            .objects
            .iter()
            .all(|(s, id, _)| store.read_object(*s, id).unwrap().is_none())
}

fn is_new(store: &dyn VersionedStore, relabel: &Relabel) -> bool {
    let replaced = |table: Table, key: &[u8]| {
        relabel
            .batch
            .records
            .iter()
            .any(|(t, k, _)| *t == table && k == key)
    };
    store.head().unwrap() == Some(relabel.batch.new_head)
        && relabel
            .batch
            .records
            .iter()
            .all(|(t, k, v)| store.read_record(*t, k).unwrap().as_ref() == Some(v))
        && relabel
            .retired
            .iter()
            .filter(|(t, k, _)| !replaced(*t, k))
            .all(|(t, k, _)| store.read_record(*t, k).unwrap().is_none())
        && relabel
            .batch
            .objects
            .iter()
            .all(|(s, id, b)| store.read_object(*s, id).unwrap().as_ref() == Some(b))
}

fn objects_kept(store: &dyn VersionedStore, first: &CommitBatch) -> bool {
    first
        .objects
        .iter()
        .all(|(s, id, b)| store.read_object(*s, id).unwrap().as_ref() == Some(b))
}

#[test]
fn relabel_replaces_only_an_exact_publication_and_survives_reopen() {
    let dir = BenchDir::new("relabel");
    let path = dir.file("staged.redb");
    let store = RedbStore::create(&path, RedbConfig::default()).unwrap();
    let (first, relabel) = publication();
    commit(&store, first.clone());
    let id = store.database_id();

    let mut refused = Vec::new();
    let mut stale = relabel.clone();
    stale.expected_head = relabel.batch.new_head;
    refused.push(stale);
    let mut extra_record = relabel.clone();
    extra_record.retired.pop();
    refused.push(extra_record);
    let mut differs = relabel.clone();
    differs.retired[0].2.push(1);
    refused.push(differs);
    let mut missing = relabel.clone();
    missing
        .retired
        .push((Table::Meta, b"import/checkpoint".to_vec(), b"x".to_vec()));
    refused.push(missing);
    let mut extra_block = relabel.clone();
    extra_block
        .retired
        .push((Table::Blocks, height_key(9).to_vec(), b"x".to_vec()));
    refused.push(extra_block);
    let mut later = relabel.clone();
    later.batch.expected_head = Some(first.new_head);
    later.batch.new_head = head(1, 2);
    refused.push(later);
    for refusal in &refused {
        let outcome = store.relabel(refusal);
        assert!(
            matches!(outcome, CommitOutcome::NotCommitted(_)),
            "{outcome:?}"
        );
        assert!(is_old(&store, &first, &relabel));
    }

    assert_eq!(store.relabel(&relabel), CommitOutcome::Committed);
    assert!(is_new(&store, &relabel) && objects_kept(&store, &first));
    // The store still serves and recovers as the same database.
    assert_eq!(store.recover().unwrap(), Some(relabel.batch.new_head));
    assert!(matches!(
        store.relabel(&relabel),
        CommitOutcome::NotCommitted(_)
    ));
    drop(store);
    let reopened = RedbStore::open(&path, RedbConfig::default()).unwrap();
    assert_eq!(reopened.database_id(), id);
    assert!(is_new(&reopened, &relabel) && objects_kept(&reopened, &first));
    // An ordinary commit continues from the relabelled head.
    let next = block(
        Some(relabel.batch.new_head),
        1,
        2,
        &[(SpaceId::Namespace(1), 3, 180)],
    );
    commit(&reopened, next);
}

#[test]
fn every_crash_point_is_the_old_or_the_new_publication() {
    let (first, relabel) = publication();
    let config = config(true);
    let disk = CrashDisk::new();
    let store = crash_store(&disk, config);
    commit(&store, first.clone());
    let parent = disk.crash(store, CrashMode::DurableOnly);
    // A complete relabelling's mutating calls.
    let disk = CrashDisk::from_image(parent.clone());
    let store = crash_store(&disk, config);
    let start = disk.mutating_trace().len();
    assert_eq!(store.relabel(&relabel), CommitOutcome::Committed);
    let calls = (disk.mutating_trace().len() - start) as u64;
    drop(store);
    assert!(calls >= 3, "a durable relabelling writes and syncs");

    let (mut old, mut new) = (0, 0);
    for k in 0..=calls {
        let disk = CrashDisk::from_image(parent.clone());
        let store = crash_store(&disk, config);
        disk.crash_after(k);
        let outcome = store.relabel(&relabel);
        disk.cut_off();
        drop(store);
        for mode in modes(SEEDS) {
            let context = format!("crash point {k}/{calls} {mode:?} outcome {outcome:?}");
            let image = disk.crash_image(mode);
            assert_integrity(&image, &context);
            let reopened = crash_store(&CrashDisk::from_image(image), config);
            assert!(objects_kept(&reopened, &first), "{context}");
            if is_old(&reopened, &first, &relabel) {
                assert!(
                    !(outcome == CommitOutcome::Committed && mode == CrashMode::DurableOnly),
                    "{context}: a committed relabelling was lost"
                );
                old += 1;
            } else {
                assert!(
                    is_new(&reopened, &relabel),
                    "{context}: a mixed publication"
                );
                assert!(
                    !matches!(outcome, CommitOutcome::NotCommitted(_)),
                    "{context}: a refused relabelling became durable"
                );
                new += 1;
            }
        }
    }
    assert!(old > 0 && new > 0, "old {old}, new {new}");
}

#[test]
fn relabel_never_applies_to_a_headless_store() {
    let disk = CrashDisk::new();
    let store = crash_store(&disk, config(true));
    let (first, mut relabel) = publication();
    assert_eq!(
        store.stage_objects(&first.objects),
        CommitOutcome::Committed
    );
    relabel.retired.clear();
    assert!(matches!(
        store.relabel(&relabel),
        CommitOutcome::NotCommitted(_)
    ));
    assert_eq!(store.head().unwrap(), None::<Head>);
}
