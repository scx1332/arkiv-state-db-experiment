//! A failed redb handle latches the store until `recover()` replaces it;
//! damaged bytes are errors, never "absent".
//!
//! - An injected read error latches the store `Failed`: every call — object,
//!   record and head reads, commits — returns that same error, never `None`,
//!   until `recover()` reopens the device; then everything reads back.
//! - An injected write error inside a commit latches too; the commit is
//!   refused afterwards even though the device works again, until
//!   `recover()`.
//! - `recover()` fails while the device keeps failing, leaves the store
//!   `Failed` with the recovery's error (every call returns it), and
//!   succeeds once the device works (retryable, idempotent). The same rule
//!   is pinned for `FaultyStore` by the contract case
//!   `faulty_failed_recovery_is_retryable`.
//! - `recover()` waits for a read that is in flight inside the backend.
//! - A simulated process kill fails every later call of the old handle.
//! - Object bytes damaged on the medium read as `Corrupt` (content hash
//!   check), do not latch the store, and leave other objects readable.
//! - A missing catalog or host object table (both exist from
//!   initialization) is `Corrupt`, like a missing record table, never
//!   "absent"; a namespace without a table is absent.
//! - A file with another store format is `Incompatible`, a redb file without
//!   store metadata is `Incompatible` for `open` and initialized by
//!   `create`, a missing file is `Missing`, and a file that is not a redb
//!   database is `Incompatible`.

mod common;

use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use arkiv_db_redb::{
    RedbConfig, RedbStore,
    testing::{CrashDisk, CrashMode, DiskOp},
};
use arkiv_db_storage::versioned::{
    CommitOutcome, Health, ObjectReader, SpaceId, StorageErrorKind, Table, VersionedStore,
    height_key,
};
use common::{BenchDir, assert_applied, block, commit, crash_store};

fn uncached() -> RedbConfig {
    RedbConfig::with_cache_size(0)
}

#[test]
fn a_read_error_latches_every_call_until_recover() {
    let disk = CrashDisk::new();
    let store = crash_store(&disk, uncached());
    let genesis = block(
        None,
        0,
        1,
        &[(SpaceId::Namespace(1), 10, 200), (SpaceId::Catalog, 2, 80)],
    );
    commit(&store, genesis.clone());
    let (space, id, _) = genesis.objects[0].clone();

    disk.fail(DiskOp::Read, 0, io::ErrorKind::Other);
    let error = store.read_object(space, &id).unwrap_err();
    assert_eq!(error.kind, StorageErrorKind::Io);
    assert_eq!(store.health(), Health::Failed(error.clone()));
    // The device works again, but the handle stays failed.
    assert_eq!(store.read_object(space, &id).unwrap_err(), error);
    assert_eq!(
        store
            .read_object(SpaceId::Catalog, &genesis.objects[10].1)
            .unwrap_err(),
        error
    );
    assert_eq!(
        store
            .read_object(SpaceId::Namespace(9), &[0; 32])
            .unwrap_err(),
        error
    );
    assert_eq!(store.head().unwrap_err(), error);
    assert_eq!(
        store
            .read_record(Table::Blocks, &height_key(0))
            .unwrap_err(),
        error
    );
    let next = block(
        Some(genesis.new_head),
        1,
        1,
        &[(SpaceId::Namespace(1), 1, 50)],
    );
    assert_eq!(store.commit(&next), CommitOutcome::NotCommitted(error));

    assert_eq!(store.recover().unwrap(), Some(genesis.new_head));
    assert_eq!(store.health(), Health::Available);
    assert_eq!(store.epoch(), 1);
    assert_applied(&store, &genesis);
    commit(&store, next);
}

#[test]
fn a_write_error_in_a_commit_latches_until_recover() {
    let disk = CrashDisk::new();
    let store = crash_store(&disk, RedbConfig::default());
    let genesis = block(None, 0, 1, &[(SpaceId::Namespace(1), 10, 200)]);
    commit(&store, genesis.clone());
    let next = block(
        Some(genesis.new_head),
        1,
        1,
        &[(SpaceId::Namespace(1), 10, 200)],
    );
    disk.fail(DiskOp::Write, 2, io::ErrorKind::Other);
    let outcome = store.commit(&next);
    let error = match outcome {
        CommitOutcome::NotCommitted(error) | CommitOutcome::Indeterminate(error) => error,
        CommitOutcome::Committed => panic!("the write error must fail the commit"),
    };
    assert_eq!(error.kind, StorageErrorKind::Io);
    assert!(matches!(store.health(), Health::Failed(_)));
    assert!(matches!(
        store.commit(&next),
        CommitOutcome::NotCommitted(_)
    ));
    assert_eq!(store.recover().unwrap(), Some(genesis.new_head));
    commit(&store, next);
}

#[test]
fn recover_is_retryable_while_the_device_fails() {
    let disk = CrashDisk::new();
    let store = crash_store(&disk, uncached());
    let genesis = block(None, 0, 1, &[(SpaceId::Host, 4, 64)]);
    commit(&store, genesis.clone());
    disk.fail_all(DiskOp::Read, io::ErrorKind::Other);
    let first = store.head().unwrap_err();
    assert_eq!(store.health(), Health::Failed(first.clone()));
    for _ in 0..3 {
        let error = store.recover().unwrap_err();
        assert_eq!(error.kind, StorageErrorKind::Io);
        // The recovery's error replaces the earlier one; there is no handle.
        assert_eq!(store.health(), Health::Failed(error.clone()));
        assert_eq!(
            store
                .read_record(Table::Blocks, &height_key(0))
                .unwrap_err(),
            error
        );
        assert_eq!(
            store.read_object(SpaceId::Host, &[0; 32]).unwrap_err(),
            error
        );
        assert_eq!(
            store.commit(&block(Some(genesis.new_head), 1, 1, &[])),
            CommitOutcome::NotCommitted(error)
        );
    }
    disk.clear_faults();
    assert_eq!(store.recover().unwrap(), Some(genesis.new_head));
    assert_eq!(store.recover().unwrap(), Some(genesis.new_head));
    assert_applied(&store, &genesis);
}

#[test]
fn recover_waits_for_a_read_in_flight() {
    let disk = CrashDisk::new();
    let store = Arc::new(crash_store(&disk, uncached()));
    let genesis = block(None, 0, 1, &[(SpaceId::Namespace(4), 3, 300)]);
    commit(store.as_ref(), genesis.clone());
    let (space, id, bytes) = genesis.objects[0].clone();
    let recovered = AtomicBool::new(false);

    disk.hold_reads();
    std::thread::scope(|scope| {
        let reader = scope.spawn(|| store.read_object(space, &id));
        assert!(disk.wait_for_held_readers(1, Duration::from_secs(10)));
        let recovering = scope.spawn(|| {
            let head = store.recover();
            recovered.store(true, Ordering::SeqCst);
            head
        });
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !recovered.load(Ordering::SeqCst),
            "recover() replaced the handle under a read in flight"
        );
        disk.release_reads();
        assert_eq!(reader.join().unwrap().unwrap(), Some(bytes.clone()));
        assert_eq!(recovering.join().unwrap().unwrap(), Some(genesis.new_head));
    });
    assert!(recovered.load(Ordering::SeqCst));
    assert_eq!(store.read_object(space, &id).unwrap(), Some(bytes));
}

#[test]
fn a_killed_handle_fails_every_call() {
    let dir = BenchDir::new("killed");
    let store = RedbStore::create(dir.file("store.redb"), uncached()).unwrap();
    let genesis = block(None, 0, 1, &[(SpaceId::Namespace(1), 2, 40)]);
    commit(&store, genesis.clone());
    store.probe().kill();
    let error = store
        .read_object(SpaceId::Namespace(1), &genesis.objects[0].1)
        .unwrap_err();
    assert_eq!(error.kind, StorageErrorKind::Io);
    assert_eq!(store.head().unwrap_err(), error);
    assert!(store.recover().is_err());
}

#[test]
fn damaged_object_bytes_are_corrupt_and_do_not_latch() {
    let disk = CrashDisk::new();
    let store = crash_store(&disk, uncached());
    let genesis = block(None, 0, 1, &[(SpaceId::Namespace(1), 4, 600)]);
    commit(&store, genesis.clone());
    let mut image = disk.crash(store, CrashMode::DurableOnly);

    let (space, damaged, bytes) = genesis.objects[1].clone();
    let at = image
        .windows(bytes.len())
        .position(|window| window == bytes.as_slice())
        .expect("object bytes are stored contiguously");
    image[at + 300] ^= 0x40;

    let store = crash_store(&CrashDisk::from_image(image), uncached());
    let error = store.read_object(space, &damaged).unwrap_err();
    assert_eq!(error.kind, StorageErrorKind::Corrupt);
    assert_eq!(
        store.load_object(space, &damaged).unwrap_err().kind,
        StorageErrorKind::Corrupt
    );
    assert_eq!(store.health(), Health::Available);
    for (space, id, bytes) in genesis.objects.iter().filter(|(_, id, _)| *id != damaged) {
        assert_eq!(store.read_object(*space, id).unwrap().as_ref(), Some(bytes));
    }
}

#[test]
fn missing_fixed_tables_are_corrupt_not_absent() {
    let dir = BenchDir::new("missing-tables");
    let path = dir.file("store.redb");
    let genesis = block(None, 0, 1, &[(SpaceId::Namespace(1), 2, 40)]);
    {
        let store = RedbStore::create(&path, uncached()).unwrap();
        commit(&store, genesis.clone());
    }
    {
        let db = redb::Database::open(&path).unwrap();
        let txn = db.begin_write().unwrap();
        for table in ["objects_catalog", "objects_host"] {
            let definition = redb::TableDefinition::<&[u8; 32], &[u8]>::new(table);
            assert!(txn.delete_table(definition).unwrap());
        }
        let manifests = redb::TableDefinition::<u64, &[u8]>::new("manifests");
        assert!(txn.delete_table(manifests).unwrap());
        txn.commit().unwrap();
    }
    let store = RedbStore::open(&path, uncached()).unwrap();
    for space in [SpaceId::Catalog, SpaceId::Host] {
        let error = store.read_object(space, &[1; 32]).unwrap_err();
        assert_eq!(error.kind, StorageErrorKind::Corrupt, "{space:?}: {error}");
    }
    let error = store
        .read_record(Table::Manifests, &height_key(0))
        .unwrap_err();
    assert_eq!(error.kind, StorageErrorKind::Corrupt, "{error}");
    // Damage is reported where it is found; everything else still reads.
    assert_eq!(store.health(), Health::Available);
    assert_eq!(
        store.read_object(SpaceId::Namespace(2), &[1; 32]).unwrap(),
        None
    );
    for (space, id, bytes) in &genesis.objects {
        assert_eq!(store.read_object(*space, id).unwrap().as_ref(), Some(bytes));
    }
}

#[test]
fn incompatible_missing_and_foreign_files_are_refused() {
    let dir = BenchDir::new("formats");

    let missing = RedbStore::open(dir.file("missing.redb"), RedbConfig::default()).unwrap_err();
    assert_eq!(missing.kind, StorageErrorKind::Missing);

    // A redb database without store metadata.
    let foreign = dir.file("foreign.redb");
    {
        let db = redb::Database::create(&foreign).unwrap();
        let txn = db.begin_write().unwrap();
        txn.open_table(redb::TableDefinition::<u64, u64>::new("other"))
            .unwrap();
        txn.commit().unwrap();
    }
    let error = RedbStore::open(&foreign, RedbConfig::default()).unwrap_err();
    assert_eq!(error.kind, StorageErrorKind::Incompatible);
    let created = RedbStore::create(&foreign, RedbConfig::default()).unwrap();
    assert!(created.last_open().initialized);
    drop(created);

    // Another store format version.
    let other_format = dir.file("format2.redb");
    drop(RedbStore::create(&other_format, RedbConfig::default()).unwrap());
    {
        let db = redb::Database::open(&other_format).unwrap();
        let txn = db.begin_write().unwrap();
        {
            let mut meta = txn
                .open_table(redb::TableDefinition::<&str, &[u8]>::new("meta"))
                .unwrap();
            meta.insert("store/format", 2u32.to_be_bytes().as_slice())
                .unwrap();
        }
        txn.commit().unwrap();
    }
    for error in [
        RedbStore::open(&other_format, RedbConfig::default()).unwrap_err(),
        RedbStore::create(&other_format, RedbConfig::default()).unwrap_err(),
    ] {
        assert_eq!(error.kind, StorageErrorKind::Incompatible, "{error}");
    }

    // Not a redb file at all.
    let garbage = dir.file("garbage.redb");
    std::fs::write(&garbage, vec![0x5a; 64 << 10]).unwrap();
    let error = RedbStore::open(&garbage, RedbConfig::default()).unwrap_err();
    assert_eq!(error.kind, StorageErrorKind::Incompatible, "{error}");
}
