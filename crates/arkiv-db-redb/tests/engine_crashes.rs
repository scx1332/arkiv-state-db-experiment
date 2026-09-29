//! Crash evidence for actual durable catalog/namespace/index/host state.
//! Small commits sweep every mutating call; spilling commits sample writes
//! and bracket every growth/sync boundary, with durable/full/torn images.

mod common;
mod engine_support;

use arkiv_db::durable::{DurableEngine, Publication, Recovery, WriterState};
use arkiv_db_redb::testing::{CrashDisk, CrashMode, DiskOp, Granularity};
use engine_support::*;
use std::{io, sync::Arc};

fn image() -> Vec<u8> {
    let disk = CrashDisk::new();
    let store = disk_store(&disk);
    let mut engine = initialize(store.clone(), 0);
    let mut branch = engine.begin(engine.head(), None).unwrap();
    base(&mut branch);
    publish(&mut engine, branch);
    disk.crash((engine, store), CrashMode::DurableOnly)
}
fn open(image: &[u8], large: bool) -> (CrashDisk, Arc<arkiv_db_redb::RedbStore>, DurableEngine) {
    let disk = CrashDisk::from_image(image.to_vec());
    let store = disk_store(&disk);
    let mut engine = DurableEngine::open(store.clone(), genesis(), options(0)).unwrap();
    let mut branch = engine.begin(engine.head(), None).unwrap();
    candidate(&mut branch, large);
    engine.prepare(branch.seal()).unwrap();
    (disk, store, engine)
}

fn crash_sweep(large: bool) {
    let image = image();
    let (parent, expected) = memory_snapshots(large);
    let (disk, store, mut engine) = open(&image, large);
    let start = disk.mutating_trace().len();
    assert!(matches!(
        engine.commit().unwrap(),
        Publication::Committed(_)
    ));
    let trace = disk.mutating_trace()[start..].to_vec();
    drop((engine, store));
    assert!(trace.contains(&DiskOp::Write));
    assert!(trace.contains(&DiskOp::Sync));
    if large {
        assert!(
            trace.contains(&DiskOp::SetLen),
            "large engine commit must grow file"
        );
    }
    let points: Vec<_> = (0..=trace.len())
        .filter(|&k| {
            !large
                || k % 16 == 0
                || k == trace.len()
                || trace.get(k).is_some_and(|op| *op != DiskOp::Write)
                || k.checked_sub(1)
                    .and_then(|p| trace.get(p))
                    .is_some_and(|op| *op != DiskOp::Write)
        })
        .collect();
    let modes = [
        CrashMode::DurableOnly,
        CrashMode::AllUnsynced,
        CrashMode::Torn {
            seed: 17,
            granularity: Granularity::Sector,
        },
        CrashMode::Torn {
            seed: 29,
            granularity: Granularity::Byte,
        },
    ];
    let (mut parents, mut candidates) = (0, 0);
    for &cut in &points {
        let (disk, store, mut engine) = open(&image, large);
        disk.crash_after(cut as u64);
        let outcome = engine.commit().unwrap();
        disk.cut_off();
        drop((engine, store));
        for mode in modes {
            let disk = CrashDisk::from_image(disk.crash_image(mode));
            let store = disk_store(&disk);
            let engine = DurableEngine::open(store.clone(), genesis(), options(0))
                .unwrap_or_else(|e| panic!("large={large} cut={cut} mode={mode:?}: {e}"));
            check(&engine, &parent, &expected, large);
            if engine.head().sequence() == 1 {
                parents += 1;
            } else {
                candidates += 1;
            }
            if matches!(outcome, Publication::Committed(_)) {
                assert_eq!(engine.head().sequence(), 2);
            }
            if matches!(outcome, Publication::NotCommitted(_)) {
                assert_eq!(engine.head().sequence(), 1);
            }
            assert!(!store.last_open().full_repair);
        }
    }
    assert!(parents > 0 && candidates > 0);
    eprintln!(
        "engine crashes large={large}: calls={}, cuts={}, images={}, parent={parents}, candidate={candidates}, growth={}, sync={}",
        trace.len(),
        points.len(),
        points.len() * modes.len(),
        trace.iter().filter(|op| **op == DiskOp::SetLen).count(),
        trace.iter().filter(|op| **op == DiskOp::Sync).count()
    );
}

#[test]
fn small_engine_commit_at_every_mutating_call() {
    crash_sweep(false);
}
#[test]
fn spilling_engine_commit_at_growth_sync_and_sampled_writes() {
    crash_sweep(true);
}

#[test]
fn engine_io_failures_reconcile_exact_pending_identity_without_reexecution() {
    let image = image();
    let (parent, expected) = memory_snapshots(false);
    let (disk, store, mut engine) = open(&image, false);
    let start = disk.mutating_trace().len();
    engine.commit().unwrap();
    let trace = disk.mutating_trace()[start..].to_vec();
    drop((engine, store));
    let (mut absent, mut observed) = (0, 0);
    for (position, op) in trace.iter().enumerate() {
        let (disk, store, mut engine) = open(&image, false);
        let skip = trace[..position]
            .iter()
            .filter(|earlier| *earlier == op)
            .count() as u64;
        disk.fail(*op, skip, io::ErrorKind::Other);
        let outcome = engine.commit().unwrap();
        assert!(!matches!(outcome, Publication::Committed(_)));
        assert_eq!(engine.head().sequence(), 1);
        assert!(engine.begin(engine.head(), None).is_err());
        match engine.recover_pending().unwrap() {
            Recovery::RetryPrepared => {
                absent += 1;
                assert!(matches!(
                    engine.commit().unwrap(),
                    Publication::Committed(_)
                ));
                check(&engine, &parent, &expected, false);
            }
            Recovery::ObservedCommittedDurabilityUnknown(info) => {
                observed += 1;
                assert!(matches!(outcome, Publication::Indeterminate(_)));
                assert_eq!(info.state_root, expected.state_root());
                assert_eq!(engine.head().sequence(), 1);
                assert_eq!(engine.writer_state(), WriterState::Uncertain);
                assert!(engine.commit().is_err());
            }
            other => panic!("unexpected {other:?}"),
        }
        let after = disk.crash((engine, store), CrashMode::DurableOnly);
        let disk = CrashDisk::from_image(after);
        let store = disk_store(&disk);
        let engine = DurableEngine::open(store, genesis(), options(0)).unwrap();
        assert_eq!(engine.head().sequence(), 2);
        check(&engine, &parent, &expected, false);
    }
    assert!(absent > 0 && observed > 0);
    eprintln!(
        "engine I/O sweep: {} calls, {absent} exact retries, {observed} observed/uncertain",
        trace.len()
    );
}

#[test]
fn lost_final_sync_with_page_cache_loss_stays_uncertain_and_survives_model_recovery() {
    let image = image();
    let (parent, expected) = memory_snapshots(false);
    let (disk, store, mut engine) = open(&image, false);
    disk.fail_sync_losing_unsynced(1); // quick repair: data sync, then final header sync
    assert!(matches!(
        engine.commit().unwrap(),
        Publication::Indeterminate(_)
    ));
    assert!(matches!(
        engine.recover_pending().unwrap(),
        Recovery::ObservedCommittedDurabilityUnknown(_)
    ));
    assert_eq!(engine.head().sequence(), 1);
    assert!(engine.begin(engine.head(), None).is_err());
    let after = disk.crash((engine, store), CrashMode::DurableOnly);
    let disk = CrashDisk::from_image(after);
    let store = disk_store(&disk);
    let engine = DurableEngine::open(store, genesis(), options(0)).unwrap();
    assert_eq!(engine.head().sequence(), 2);
    check(&engine, &parent, &expected, false);
}

#[test]
fn disk_full_before_and_inside_engine_commit_preserves_outcome_distinction() {
    let image = image();
    for during_commit in [false, true] {
        let large = !during_commit;
        let (parent, expected) = memory_snapshots(large);
        let (disk, store, mut engine) = open(&image, large);
        if during_commit {
            disk.fail(DiskOp::Sync, 0, io::ErrorKind::StorageFull);
        } else {
            disk.set_quota(Some(disk.visible_image().len() as u64));
        }
        let outcome = engine.commit().unwrap();
        match outcome {
            Publication::NotCommitted(ref e) if !during_commit => {
                assert_eq!(e.kind, arkiv_db_storage::versioned::StorageErrorKind::Full);
            }
            Publication::Indeterminate(ref e) if during_commit => {
                assert_eq!(e.kind, arkiv_db_storage::versioned::StorageErrorKind::Full);
            }
            other => panic!("during_commit={during_commit}: {other:?}"),
        }
        assert_eq!(engine.head().sequence(), 1);
        disk.set_quota(None);
        disk.clear_faults();
        assert_eq!(engine.recover_pending().unwrap(), Recovery::RetryPrepared);
        assert!(matches!(
            engine.commit().unwrap(),
            Publication::Committed(_)
        ));
        let after = disk.crash((engine, store), CrashMode::DurableOnly);
        let disk = CrashDisk::from_image(after);
        let engine = DurableEngine::open(disk_store(&disk), genesis(), options(0)).unwrap();
        check(&engine, &parent, &expected, large);
    }
}
