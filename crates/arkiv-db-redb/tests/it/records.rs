//! Expiry of per-height records (simulator profile) in a redb file: the
//! window boundary, `HistoryPruned` answers, physical deletion with a floor
//! budget, repeated identities, reopen, a commit cut at every mutating disk
//! call, the draining of a migrated file's backlog, and archive retention.

use crate::engine_support::disk_store;
use arkiv_db::{
    B256, EngineError, PriceSchedule,
    durable::{
        DurableEngine, DurableError, EngineGenesis, Initialization, Options, Publication,
        Retention, gc,
        records::{RecordFloor, inclusion_list_key},
        simulator::{SimulatorIdentity, SimulatorInclusion, SimulatorPacket},
    },
};
use arkiv_db_redb::testing::{CrashDisk, CrashMode, Granularity};
use arkiv_db_storage::versioned::{StorageErrorKind, Table, VersionedStore};
use std::sync::Arc;

fn identity() -> SimulatorIdentity {
    SimulatorIdentity {
        source_id: [1; 16],
        run_id: [2; 16],
        node_genesis_hash: B256::repeat_byte(3),
        metadata: b"record expiry fixture".to_vec(),
    }
}

fn genesis() -> EngineGenesis {
    EngineGenesis::new(identity().deployment(), PriceSchedule::FIXTURE)
}

/// Window `w`, identity window `k`, deletion budget `budget`.
fn options(w: u64, k: u64, budget: u64) -> Options {
    Options {
        retention: Retention::Window(w),
        inclusion_window: Some(k),
        prune_records: true,
        record_delete_budget: budget,
        gc_delete_budget: 2,
        gc_release_budget: 2,
        gc_release_per_object: 0,
        ..Options::default()
    }
}

/// The identity a block at `height` includes first, and its second one.
fn identity_key(n: u64) -> Vec<u8> {
    let mut key = vec![2];
    key.extend([0; 24]);
    key.extend(n.to_be_bytes());
    key
}

fn packet(height: u64, root: B256, keys: &[Vec<u8>]) -> SimulatorPacket {
    SimulatorPacket {
        height,
        state_root: root,
        bytes: [height.to_be_bytes().as_slice(), &[0x5a; 300]].concat(),
        inclusions: keys
            .iter()
            .map(|key| SimulatorInclusion {
                key: key.clone(),
                value: height.to_be_bytes().to_vec(),
            })
            .collect(),
    }
}

fn create(store: Arc<dyn VersionedStore>, options: Options) -> DurableEngine {
    let g = genesis();
    let root = g.initial_header().root();
    match DurableEngine::initialize_simulator(store, g, options, identity(), packet(0, root, &[]))
        .unwrap()
    {
        Initialization::Committed(engine) => engine,
        other => panic!("{other:?}"),
    }
}

fn open(store: Arc<dyn VersionedStore>, options: Options) -> DurableEngine {
    DurableEngine::open_simulator(store, genesis(), options, identity()).unwrap()
}

/// Prepares the next block: a host write (so state changes and garbage
/// appears) and the inclusion of `keys`.
fn prepare(engine: &mut DurableEngine, keys: &[Vec<u8>]) -> Result<(), DurableError> {
    let height = engine.head().sequence() + 1;
    let mut branch = engine.begin(engine.head(), None).unwrap();
    branch
        .try_host_put([(height % 5) as u8; 32], &height.to_be_bytes())
        .unwrap()
        .unwrap();
    let candidate = branch.try_seal().unwrap();
    let root = candidate.state_root();
    engine
        .prepare_simulator(candidate, packet(height, root, keys))
        .map(drop)
        .map_err(|f| f.error)
}

fn produce(engine: &mut DurableEngine, keys: &[Vec<u8>]) {
    prepare(engine, keys).unwrap();
    assert!(matches!(
        engine.commit().unwrap(),
        Publication::Committed(_)
    ));
}

/// The keys the block at `height` includes in [`run`].
fn keys_at(height: u64) -> Vec<Vec<u8>> {
    vec![identity_key(height), identity_key(1000 + height)]
}

fn pruned(engine: &DurableEngine, height: u64) -> bool {
    matches!(
        engine.simulator_block(height),
        Err(DurableError::Engine(EngineError::HistoryPruned { .. }))
    )
}

/// Everything the record floor promises, logically and on disk.
fn check(engine: &DurableEngine, store: &dyn VersionedStore) {
    let head = engine.head().sequence();
    let records = engine.record_floor();
    assert!(records.floor <= engine.state_floor().sequence());
    assert!(engine.simulator_block(0).is_ok());
    for height in 1..=head {
        let expired = records.expired(height);
        assert_eq!(
            pruned(engine, height),
            expired,
            "height {height} {records:?}"
        );
        let key = height.to_be_bytes();
        let stored = store.read_record(Table::Blocks, &key).unwrap().is_some();
        if height < records.pruned {
            assert!(!stored, "height {height} deleted {records:?}");
            for name in [
                inclusion_list_key(height),
                format!("engine/state-floor/{height:016x}"),
                format!("engine/record-floor/{height:016x}"),
            ] {
                assert_eq!(
                    store.read_record(Table::Meta, name.as_bytes()).unwrap(),
                    None
                );
            }
            assert_eq!(store.read_record(Table::Manifests, &key).unwrap(), None);
        } else {
            assert!(stored, "height {height} kept {records:?}");
        }
        if !expired {
            assert_eq!(
                &engine.simulator_block(height).unwrap().bytes[..8],
                height.to_be_bytes()
            );
        }
    }
}

#[test]
fn records_expire_at_the_window_boundary_and_survive_reopen() {
    let disk = CrashDisk::new();
    let store = disk_store(&disk);
    let (w, k) = (3, 5);
    let mut engine = create(store.clone(), options(w, k, 1));
    engine.enable_gc(None).unwrap();
    let mut expected = 0;
    for _ in 0..30 {
        let height = engine.head().sequence() + 1;
        produce(&mut engine, &keys_at(height));
        // R(S) = max(R(S-1), min(stateFloor, S + 1 - k)).
        expected = u64::max(
            expected,
            (height - w.min(height)).min((height + 1).saturating_sub(k)),
        );
        assert_eq!(engine.record_floor().floor, expected);
        check(&engine, store.as_ref());
        // The identity window: the inclusions of the last k heights answer.
        for h in 1..=height {
            let found = engine.simulator_inclusion(&identity_key(h)).unwrap();
            assert_eq!(found.is_some(), h >= expected, "inclusion {h} at {height}");
        }
        gc::verify(store.as_ref()).unwrap();
    }
    let floor = engine.record_floor();
    // A budget of one keeps up with a floor that rises one height a block.
    assert!(floor.backlog() <= 1, "{floor:?}");
    let image = disk.crash((engine, store), CrashMode::DurableOnly);
    let disk = CrashDisk::from_image(image);
    let store = disk_store(&disk);
    let mut engine = open(store.clone(), options(w, k, 1));
    assert_eq!(engine.record_floor(), floor);
    check(&engine, store.as_ref());
    produce(&mut engine, &keys_at(31));
    check(&engine, store.as_ref());
}

#[test]
fn an_identity_repeats_only_after_the_identity_window() {
    let store: Arc<dyn VersionedStore> = disk_store(&CrashDisk::new());
    let k = 4;
    // Records are kept at first, so the repeated row is still stored.
    let keep = Options {
        prune_records: false,
        ..options(2, k, 1)
    };
    let mut engine = create(store.clone(), keep);
    let repeated = identity_key(77);
    produce(&mut engine, std::slice::from_ref(&repeated)); // height 1
    // Heights 2..=5: within the window the row stays and a repeat conflicts.
    for height in 2..=5 {
        let error = prepare(&mut engine, std::slice::from_ref(&repeated))
            .and_then(|()| match engine.commit()? {
                Publication::NotCommitted(e) => Err::<(), DurableError>(e.into()),
                other => panic!("height {height}: {other:?}"),
            })
            .unwrap_err();
        assert!(
            matches!(&error, DurableError::Storage(e) if e.kind == StorageErrorKind::Conflict),
            "{error:?}"
        );
        engine.discard_prepared().unwrap();
        produce(&mut engine, &[identity_key(height)]);
    }
    // Height 6: the inclusion at 1 is older than k = 4 blocks; replaced.
    produce(&mut engine, std::slice::from_ref(&repeated));
    let value = engine.simulator_inclusion(&repeated).unwrap().unwrap();
    assert_eq!(value, 6u64.to_be_bytes());
    // Expiry starts; deleting height 1 leaves the row of height 6 alone.
    drop(engine);
    let mut engine = open(store.clone(), options(2, k, 1));
    produce(&mut engine, &[identity_key(7)]);
    assert!(engine.record_floor().pruned > 1);
    assert_eq!(
        store
            .read_record(Table::Blocks, &1u64.to_be_bytes())
            .unwrap(),
        None
    );
    assert_eq!(
        engine.simulator_inclusion(&repeated).unwrap().unwrap(),
        6u64.to_be_bytes()
    );
    check(&engine, store.as_ref());
}

#[test]
fn archive_retention_keeps_every_record() {
    let store: Arc<dyn VersionedStore> = disk_store(&CrashDisk::new());
    let archive = Options {
        retention: Retention::Archive,
        inclusion_window: Some(4),
        // A node asks for expiry only with a window; archive ignores it.
        prune_records: true,
        ..Options::default()
    };
    let mut engine = create(store.clone(), archive.clone());
    for height in 1..=20 {
        produce(&mut engine, &keys_at(height));
    }
    assert_eq!(engine.record_floor(), RecordFloor::default());
    check(&engine, store.as_ref());
    for height in 1..=20 {
        assert!(
            engine
                .simulator_inclusion(&identity_key(height))
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .read_record(Table::Meta, b"engine/record-floor/0000000000000014")
                .unwrap()
                .is_none()
        );
    }
}

#[test]
fn a_migrated_backlog_drains_geometrically() {
    // A file written without expiry: every record of 300 heights is kept.
    let store: Arc<dyn VersionedStore> = disk_store(&CrashDisk::new());
    let keep = Options {
        prune_records: false,
        ..options(4, 4, 1)
    };
    let mut engine = create(store.clone(), keep);
    for height in 1..=300 {
        produce(&mut engine, &keys_at(height));
    }
    assert_eq!(engine.record_floor(), RecordFloor::default());
    drop(engine);
    let mut engine = open(store.clone(), options(4, 4, 1));
    let mut commits = 0;
    while engine.record_floor().backlog() > 0 || engine.record_floor().floor == 0 {
        let height = engine.head().sequence() + 1;
        produce(&mut engine, &keys_at(height));
        commits += 1;
        assert!(commits < 80, "{:?}", engine.record_floor());
    }
    check(&engine, store.as_ref());
    // Every expired identity row is gone from the file.
    for height in 1..engine.record_floor().pruned {
        assert_eq!(
            store
                .read_record(Table::Inclusions, &identity_key(height))
                .unwrap(),
            None
        );
    }
}

#[test]
fn a_record_expiring_commit_cut_at_every_mutating_call_reopens_consistently() {
    let disk = CrashDisk::new();
    let store = disk_store(&disk);
    let mut engine = create(store.clone(), options(2, 2, 2));
    engine.enable_gc(None).unwrap();
    for height in 1..=10 {
        produce(&mut engine, &keys_at(height));
    }
    let image = disk.crash((engine, store), CrashMode::DurableOnly);
    let reopen = || {
        let disk = CrashDisk::from_image(image.clone());
        let store = disk_store(&disk);
        let mut engine = open(store.clone(), options(2, 2, 2));
        // Height 11 deletes two heights and repeats an expired identity.
        prepare(&mut engine, &[identity_key(11), identity_key(3)]).unwrap();
        (disk, store, engine)
    };
    let (disk, store, mut engine) = reopen();
    let before = engine.record_floor();
    let start = disk.mutating_trace().len();
    assert!(matches!(
        engine.commit().unwrap(),
        Publication::Committed(_)
    ));
    let after = engine.record_floor();
    assert!(after.pruned > before.pruned, "{before:?} {after:?}");
    let calls = disk.mutating_trace().len() - start;
    drop((engine, store));
    let modes = [
        CrashMode::DurableOnly,
        CrashMode::AllUnsynced,
        CrashMode::Torn {
            seed: 9,
            granularity: Granularity::Sector,
        },
    ];
    let (mut old, mut new) = (0, 0);
    for cut in 0..=calls {
        let (disk, store, mut engine) = reopen();
        disk.crash_after(cut as u64);
        let _ = engine.commit();
        disk.cut_off();
        drop((engine, store));
        for mode in modes {
            let disk = CrashDisk::from_image(disk.crash_image(mode));
            let store = disk_store(&disk);
            let engine = open(store.clone(), options(2, 2, 2));
            check(&engine, store.as_ref());
            gc::verify(store.as_ref()).unwrap_or_else(|e| panic!("cut {cut} {mode:?}: {e}"));
            if engine.head().sequence() == 11 {
                assert_eq!(engine.record_floor(), after);
                assert_eq!(
                    engine
                        .simulator_inclusion(&identity_key(3))
                        .unwrap()
                        .unwrap(),
                    11u64.to_be_bytes()
                );
                new += 1;
            } else {
                assert_eq!(engine.head().sequence(), 10);
                assert_eq!(engine.record_floor(), before);
                old += 1;
            }
        }
    }
    assert!(old > 0 && new > 0, "{old} {new}");
}
