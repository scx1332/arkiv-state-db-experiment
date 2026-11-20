//! Garbage collection in a redb file: counts that survive reopening, a
//! commit cut at every mutating disk call, the store format gate, and
//! suspension when the counts are wrong.

use crate::engine_support::*;
use arkiv_db::durable::{
    DurableEngine, Options, Publication, Retention,
    gc::{self, EngineGraph, check_snapshot},
};
use arkiv_db_redb::{
    RedbStore,
    testing::{CrashDisk, CrashMode, DiskOp, Granularity},
};
use arkiv_db_storage::{
    gc::{Edge, GcCommit, ObjectGraph},
    versioned::{CommitBatch, CommitOutcome, Head, SpaceId, StorageErrorKind, VersionedStore},
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

fn pruned(window: u64) -> Options {
    Options {
        retention: Retention::Window(window),
        gc_release_budget: 2,
        gc_release_per_object: 0,
        gc_delete_budget: 2,
        ..options(0)
    }
}

/// Block `n` of a churning workload over few keys and kinds: rows move
/// between postings, are deleted and created again, raw values come and go.
fn churn(branch: &mut arkiv_db::Branch, n: u64) {
    if n == 0 {
        namespace(branch, "target");
        namespace(branch, "other");
    }
    for i in 0..3 {
        let k = (n * 3 + i) % 5;
        match (n + i) % 4 {
            0 | 1 => {
                let _ = branch.try_execute(
                    OWNER,
                    arkiv_db::WriteOp::DeleteRecord {
                        namespace: ns(1),
                        key: key(k),
                    },
                    arkiv_db::Budget::unlimited(),
                );
                let r = k + 16 * (n % 2);
                // May exist already: the rejection is part of the block.
                let _ = branch.try_execute(
                    OWNER,
                    arkiv_db::WriteOp::CreateRecord {
                        namespace: ns(1),
                        key: key(r),
                        cells: vec![
                            (
                                name("kind"),
                                arkiv_db::Cell::attribute(arkiv_db::Value::U64(r % 3)),
                            ),
                            (
                                name("payload"),
                                arkiv_db::Cell::field(arkiv_db::Value::bytes(&[r as u8; 64])),
                            ),
                        ],
                    },
                    arkiv_db::Budget::unlimited(),
                );
            }
            2 => put(branch, ns(2), &k.to_be_bytes(), &[n as u8 % 3; 36]),
            _ => {
                let _ = branch.try_execute(
                    OWNER,
                    arkiv_db::WriteOp::Delete {
                        namespace: ns(2),
                        key: k.to_be_bytes().to_vec(),
                    },
                    arkiv_db::Budget::unlimited(),
                );
            }
        }
    }
}

fn step(engine: &mut DurableEngine, n: u64) {
    let mut branch = engine.begin(engine.head(), None).unwrap();
    churn(&mut branch, n);
    publish(engine, branch);
}

fn check(engine: &DurableEngine, store: &dyn VersionedStore) {
    gc::verify(store).unwrap_or_else(|e| panic!("head {}: {e}", engine.head().sequence()));
    for sequence in engine.gc_horizon()..=engine.head().sequence() {
        check_snapshot(store, sequence).unwrap_or_else(|e| panic!("snapshot {sequence}: {e}"));
    }
}

#[test]
fn counts_survive_reopen_and_deletion_frees_objects() {
    let disk = CrashDisk::new();
    let store = disk_store(&disk);
    let engine = initialize(store.clone(), 0);
    drop(engine);
    let mut engine = DurableEngine::open(store.clone(), genesis(), pruned(2)).unwrap();
    engine.enable_gc(None).unwrap();
    for n in 0..20 {
        step(&mut engine, n);
        check(&engine, store.as_ref());
    }
    let image = disk.crash((engine, store), CrashMode::DurableOnly);
    let disk = CrashDisk::from_image(image);
    let store = disk_store(&disk);
    let before = store.gc_state().unwrap().expect("a GC file reopens as one");
    let mut engine = DurableEngine::open(store.clone(), genesis(), pruned(2)).unwrap();
    assert!(engine.collects_garbage());
    for n in 20..60 {
        step(&mut engine, n);
        check(&engine, store.as_ref());
    }
    let after = store.gc_state().unwrap().unwrap();
    assert!(after.deleted_total > before.deleted_total, "{after:?}");
    assert!(
        after.released_total > 0 && after.resurrected_total > 0,
        "{after:?}"
    );
    // Only the window's garbage is left.
    assert!(after.dead_objects < after.born_total / 4, "{after:?}");
    assert!(store.gc_report().is_some());
}

#[test]
fn a_gc_commit_cut_at_every_mutating_call_leaves_consistent_counts() {
    // A file with garbage queued and deletable, so the cut commit inserts,
    // counts, releases and deletes.
    let disk = CrashDisk::new();
    let store = disk_store(&disk);
    let engine = initialize(store.clone(), 0);
    drop(engine);
    let mut engine = DurableEngine::open(store.clone(), genesis(), pruned(1)).unwrap();
    engine.enable_gc(None).unwrap();
    for n in 0..8 {
        step(&mut engine, n);
    }
    let image = disk.crash((engine, store), CrashMode::DurableOnly);
    let open = || {
        let disk = CrashDisk::from_image(image.clone());
        let store = disk_store(&disk);
        let mut engine = DurableEngine::open(store.clone(), genesis(), pruned(1)).unwrap();
        let mut branch = engine.begin(engine.head(), None).unwrap();
        churn(&mut branch, 8);
        engine.prepare(branch.seal()).unwrap();
        (disk, store, engine)
    };
    let (disk, store, mut engine) = open();
    let start = disk.mutating_trace().len();
    assert!(matches!(
        engine.commit().unwrap(),
        Publication::Committed(_)
    ));
    let report = store.gc_report().unwrap();
    assert!(
        report.born > 0 && report.died > 0 && report.deleted > 0,
        "{report:?}"
    );
    let calls = disk.mutating_trace().len() - start;
    drop((engine, store));
    let modes = [
        CrashMode::DurableOnly,
        CrashMode::AllUnsynced,
        CrashMode::Torn {
            seed: 5,
            granularity: Granularity::Sector,
        },
    ];
    let (mut old, mut new) = (0, 0);
    for cut in 0..=calls {
        let (disk, store, mut engine) = open();
        disk.crash_after(cut as u64);
        let _ = engine.commit();
        disk.cut_off();
        drop((engine, store));
        for mode in modes {
            let disk = CrashDisk::from_image(disk.crash_image(mode));
            let store = disk_store(&disk);
            let engine = DurableEngine::open(store.clone(), genesis(), pruned(1))
                .unwrap_or_else(|e| panic!("cut {cut} {mode:?}: {e}"));
            check(&engine, store.as_ref());
            if engine.head().sequence() == 9 {
                new += 1;
            } else {
                assert_eq!(engine.head().sequence(), 8);
                old += 1;
            }
        }
    }
    assert!(old > 0 && new > 0, "{old} {new}");
    assert!(calls > 4);
    let _ = DiskOp::Sync;
}

#[test]
fn a_collecting_file_is_format_two_and_refuses_plain_writes() {
    let dir = std::env::temp_dir().join(format!("gc-format-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("node.redb");
    let _ = std::fs::remove_file(&path);
    {
        let store: Arc<RedbStore> = Arc::new(RedbStore::create(&path, config()).unwrap());
        let mut engine = initialize(store.clone(), 0);
        let mut branch = engine.begin(engine.head(), None).unwrap();
        base(&mut branch);
        publish(&mut engine, branch);
        assert_eq!(store.gc_state().unwrap(), None);
        gc::enable(store.as_ref(), Some(&dir.join("spill"))).unwrap();
        assert!(store.gc_state().unwrap().is_some());
        let format = store
            .read_record(arkiv_db_storage::versioned::Table::Meta, b"store/format")
            .unwrap()
            .unwrap();
        assert_eq!(format, 2u32.to_be_bytes());
        let head = store.head().unwrap().unwrap();
        let batch = CommitBatch::new(
            Some(head),
            Head {
                height: head.height + 1,
                hash: [3; 32],
            },
        );
        let CommitOutcome::NotCommitted(error) = store.commit(&batch) else {
            panic!("a plain commit must be refused");
        };
        assert_eq!(error.kind, StorageErrorKind::Incompatible);
        assert!(matches!(
            store.stage_objects(&[]),
            CommitOutcome::NotCommitted(_)
        ));
    }
    let store = Arc::new(RedbStore::open(&path, config()).unwrap());
    assert!(store.gc_state().unwrap().is_some());
    gc::verify(store.as_ref()).unwrap();
    // Archive retention cannot open it.
    assert!(DurableEngine::open(store.clone(), genesis(), options(0)).is_err());
    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The engine graph, except that it forgets every edge once `lie` is set:
/// releases then no longer match the acquisitions.
struct Liar(Arc<AtomicBool>);
impl ObjectGraph for Liar {
    fn version(&self) -> u32 {
        EngineGraph.version()
    }
    fn edges(&self, space: SpaceId, bytes: &[u8], out: &mut Vec<Edge>) -> Result<(), String> {
        if self.0.load(Ordering::Relaxed) {
            return Ok(());
        }
        EngineGraph.edges(space, bytes, out)
    }
}

#[test]
fn wrong_counts_suspend_collection_and_a_rebuild_restores_it() {
    let disk = CrashDisk::new();
    let store = disk_store(&disk);
    let mut engine = initialize(store.clone(), 0);
    let mut branch = engine.begin(engine.head(), None).unwrap();
    base(&mut branch);
    publish(&mut engine, branch);
    drop(engine);
    // Seed with a graph that names no edges: the head's inner objects look
    // like garbage with no references.
    let lie = Arc::new(AtomicBool::new(true));
    let head = store.head().unwrap().unwrap();
    let roots = {
        let engine = DurableEngine::open(store.clone(), genesis(), options(0)).unwrap();
        gc::state_roots(&engine.head_snapshot().state_header())
    };
    store.gc_enable(&roots, &Liar(lie.clone()), None).unwrap();
    lie.store(false, Ordering::Relaxed);
    // The next commit releases the old roots' real edges: counts go wrong.
    let batch = CommitBatch::new(
        Some(head),
        Head {
            height: head.height + 1,
            hash: [5; 32],
        },
    );
    let gc_commit = GcCommit {
        acquire: roots.clone(),
        release: roots.clone(),
        horizon: 0,
        release_budget: 1 << 20,
        delete_budget: 0,
        count_only: false,
    };
    // The release cascade finds the dead objects' edges counted nowhere.
    let mut batch = batch;
    batch.put_record(
        arkiv_db_storage::versioned::Table::Blocks,
        9u64.to_be_bytes(),
        b"x".to_vec(),
    );
    let outcome = store.commit_gc(&batch, &gc_commit, &EngineGraph);
    assert_eq!(outcome, CommitOutcome::Committed, "production continues");
    let state = store.gc_state().unwrap().unwrap();
    assert!(state.suspended, "{state:?}");
    // A rebuild with the right graph counts from scratch.
    store.gc_enable(&roots, &EngineGraph, None).unwrap();
    assert!(!store.gc_state().unwrap().unwrap().suspended);
    let dump = store.gc_dump().unwrap().unwrap();
    arkiv_db_storage::gc::verify(
        &dump.objects,
        &dump.tables,
        &dump.state,
        &EngineGraph,
        &roots,
    )
    .unwrap();
}

/// An archive file copy-migrated to a window keeps every record, only the
/// objects of the retained snapshots, refuses older ones and then prunes
/// as any collecting file does.
#[test]
fn copy_migration_keeps_the_window_and_drops_older_history() {
    let dir = std::env::temp_dir().join(format!("gc-migrate-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let source = dir.join("node.redb");
    let target = dir.join("copy.redb");
    let archive_objects = {
        let store: Arc<RedbStore> = Arc::new(RedbStore::create(&source, config()).unwrap());
        let mut engine = initialize(store.clone(), 0);
        for n in 0..30 {
            step(&mut engine, n);
        }
        store
            .stats()
            .spaces
            .values()
            .map(|s| s.objects_written)
            .sum::<u64>()
    };
    let retained = {
        let store = RedbStore::open(&source, config()).unwrap();
        gc::retained(&store, 3).unwrap()
    };
    assert_eq!((retained.head, retained.floor), (30, 27));
    let report = arkiv_db_redb::copy_reachable(
        &source,
        &target,
        &retained.snapshots,
        &EngineGraph,
        &retained.floor_record.iter().cloned().collect::<Vec<_>>(),
    )
    .unwrap();
    assert!(
        report.objects > 0 && report.objects < archive_objects,
        "{report:?}"
    );
    let store = Arc::new(RedbStore::open(&target, config()).unwrap());
    let state = gc::enable(store.as_ref(), None).unwrap();
    assert_eq!(state.stored_objects, report.objects);
    gc::verify(store.as_ref()).unwrap();
    for sequence in 27..=30 {
        check_snapshot(store.as_ref(), sequence).unwrap();
    }
    // Records are all there: every manifest, even below the floor.
    let source_store = RedbStore::open(&source, config()).unwrap();
    for height in 0..=30u64 {
        for table in [
            arkiv_db_storage::versioned::Table::Manifests,
            arkiv_db_storage::versioned::Table::Blocks,
        ] {
            assert_eq!(
                store.read_record(table, &height.to_be_bytes()).unwrap(),
                source_store
                    .read_record(table, &height.to_be_bytes())
                    .unwrap()
            );
        }
    }
    drop(source_store);
    // Archive is refused; a window opens at the copied floor and prunes on.
    assert!(DurableEngine::open(store.clone(), genesis(), options(0)).is_err());
    let mut engine = DurableEngine::open(store.clone(), genesis(), pruned(3)).unwrap();
    assert_eq!(engine.state_floor().sequence(), 27);
    assert!(engine.collects_garbage());
    for n in 30..40 {
        step(&mut engine, n);
        check(&engine, store.as_ref());
    }
    assert!(store.gc_state().unwrap().unwrap().deleted_total > 0);
    drop((engine, store));
    std::fs::remove_dir_all(&dir).unwrap();
}
