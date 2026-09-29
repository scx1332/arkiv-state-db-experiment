//! Bounded reads keep the real adapter's fault and integrity behavior.
//! The shared contract separately covers all size boundaries and wrappers.

mod common;

use std::io;

use arkiv_db_redb::{
    RedbConfig, RedbStore,
    testing::{CrashDisk, CrashMode, DiskOp},
};
use arkiv_db_storage::versioned::{
    Health, ObjectReader, SpaceId, StorageErrorKind, Table, VersionedStore, height_key,
};
use common::{BenchDir, block, commit, crash_store};

#[test]
fn bounded_reads_obey_real_backend_failure_and_recovery() {
    let disk = CrashDisk::new();
    let store = crash_store(&disk, RedbConfig::with_cache_size(0));
    let batch = block(None, 0, 1, &[(SpaceId::Namespace(1), 2, 1024)]);
    let (space, id, bytes) = batch.objects[0].clone();
    commit(&store, batch.clone());

    disk.fail(DiskOp::Read, 0, io::ErrorKind::Other);
    let error = store
        .read_object_bounded(space, &id, bytes.len())
        .unwrap_err();
    assert_eq!(error.kind, StorageErrorKind::Io);
    assert_eq!(store.health(), Health::Failed(error.clone()));
    assert_eq!(
        store
            .read_record_bounded(Table::Manifests, &height_key(0), 0)
            .unwrap_err(),
        error
    );
    assert_eq!(
        store
            .read_object_bounded(SpaceId::Namespace(99), &id, 0)
            .unwrap_err(),
        error
    );
    assert_eq!(store.recover().unwrap(), Some(batch.new_head));
    assert_eq!(
        store.load_object_bounded(space, &id, bytes.len()).unwrap(),
        bytes
    );
    assert_eq!(store.health(), Health::Available);
}

#[test]
fn bounded_object_read_still_checks_integrity() {
    let disk = CrashDisk::new();
    let store = crash_store(&disk, RedbConfig::with_cache_size(0));
    let batch = block(None, 0, 1, &[(SpaceId::Namespace(1), 2, 1024)]);
    let (space, id, bytes) = batch.objects[0].clone();
    commit(&store, batch);
    let mut image = disk.crash(store, CrashMode::DurableOnly);
    let at = image
        .windows(bytes.len())
        .position(|window| window == bytes.as_slice())
        .expect("stored object");
    image[at + 500] ^= 0x40;
    let disk = CrashDisk::from_image(image);
    let store = crash_store(&disk, RedbConfig::with_cache_size(0));
    assert_eq!(
        store
            .load_object_bounded(space, &id, bytes.len())
            .unwrap_err()
            .kind,
        StorageErrorKind::Corrupt
    );
    assert_eq!(store.health(), Health::Available);
}

#[test]
fn adapter_open_rejects_oversized_fixed_metadata() {
    for (key, kind) in [
        ("store/format", StorageErrorKind::Incompatible),
        ("store/database_id", StorageErrorKind::Corrupt),
    ] {
        let dir = BenchDir::new("bounded-meta");
        let path = dir.file("store.redb");
        drop(RedbStore::create(&path, RedbConfig::default()).unwrap());
        let db = redb::Database::open(&path).unwrap();
        let write = db.begin_write().unwrap();
        {
            let mut table = write
                .open_table(redb::TableDefinition::<&str, &[u8]>::new("meta"))
                .unwrap();
            table.insert(key, vec![1; 1 << 20].as_slice()).unwrap();
        }
        write.commit().unwrap();
        drop(db);
        let error = RedbStore::open(&path, RedbConfig::default()).unwrap_err();
        assert_eq!(error.kind, kind, "{key}");
    }
}
