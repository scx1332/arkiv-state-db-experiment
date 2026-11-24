//! The instrumented backends.
//!
//! - `CountingBackend` forwards every `StorageBackend` method, including the
//!   lock methods: a second open of a store's file — through this crate or
//!   through plain redb — is refused while the first is open
//!   (`Locked` / `DatabaseAlreadyOpen`). As a control, a wrapper that leaves
//!   the lock methods at redb's defaults lets the second open through, so the
//!   refusal comes from the forwarded locks.
//! - Its counters see the commit's syncs and writes, and they survive
//!   `recover()` (one probe per store).
//! - `read_only_head` reads a closed store's head and leaves the file's bytes
//!   as they are, which opening a store does not; an open store locks it out.

use std::{io, ops::Bound};

use crate::common::{BenchDir, block, commit};
use arkiv_db_redb::{CountingBackend, RedbConfig, RedbStore, read_only_head};
use arkiv_db_storage::versioned::{SpaceId, StorageErrorKind, VersionedStore};
use redb::{StorageBackend, backends::FileBackend};

/// Forwards I/O but not locks (redb's defaults: unsupported).
#[derive(Debug)]
struct NoLocks(FileBackend);

impl StorageBackend for NoLocks {
    fn len(&self) -> io::Result<u64> {
        self.0.len()
    }
    fn read(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        self.0.read(offset, out)
    }
    fn set_len(&self, len: u64) -> io::Result<()> {
        self.0.set_len(len)
    }
    fn sync_data(&self) -> io::Result<()> {
        self.0.sync_data()
    }
    fn write(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        self.0.write(offset, data)
    }
}

fn open_file(path: &std::path::Path) -> std::fs::File {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap()
}

#[test]
fn a_second_open_is_locked_out_through_the_counting_backend() {
    let dir = BenchDir::new("locks");
    let path = dir.file("store.redb");
    let store = RedbStore::create(&path, RedbConfig::default()).unwrap();
    commit(&store, &block(None, 0, 1, &[(SpaceId::Host, 1, 32)]));
    assert!(store.probe().lock_calls() > 0);

    let second = RedbStore::open(&path, RedbConfig::default()).unwrap_err();
    assert_eq!(second.kind, StorageErrorKind::Locked);
    assert!(matches!(
        redb::Database::open(&path),
        Err(redb::DatabaseError::DatabaseAlreadyOpen)
    ));
    let counted = CountingBackend::new(FileBackend::new(open_file(&path)).unwrap());
    assert!(matches!(
        redb::Builder::new().create_with_backend(counted),
        Err(redb::DatabaseError::DatabaseAlreadyOpen)
    ));

    // Control: without forwarded locks, redb cannot see the first writer.
    let unlocked = redb::Builder::new()
        .create_with_backend(NoLocks(FileBackend::new(open_file(&path)).unwrap()));
    assert!(unlocked.is_ok(), "the control must show the locks matter");
    drop(unlocked);
}

#[test]
fn every_lock_method_is_forwarded() {
    let dir = BenchDir::new("lock-methods");
    let path = dir.file("locks.bin");
    std::fs::write(&path, [0u8; 4096]).unwrap();
    let first = CountingBackend::new(FileBackend::new(open_file(&path)).unwrap());
    let second = CountingBackend::new(FileBackend::new(open_file(&path)).unwrap());
    let range = (Bound::Included(10), Bound::Included(20));
    assert!(first.try_lock_range(range.0, range.1).unwrap());
    assert!(!second.try_lock_range(range.0, range.1).unwrap());
    assert!(second.query_lock_range(range.0, range.1).unwrap());
    assert!(!second.try_lock_shared_range(range.0, range.1).unwrap());
    first.unlock_range(range.0, range.1).unwrap();
    assert!(second.try_lock_shared_range(range.0, range.1).unwrap());
    assert!(first.try_lock_shared_range(range.0, range.1).unwrap());
    second.unlock_range(range.0, range.1).unwrap();
    first.unlock_range(range.0, range.1).unwrap();
    let other = (Bound::Included(30), Bound::Included(40));
    first.lock_range(other.0, other.1).unwrap();
    assert!(!second.try_lock_range(other.0, other.1).unwrap());
    first.unlock_range(other.0, other.1).unwrap();
    second.lock_shared_range(other.0, other.1).unwrap();
    second.close().unwrap();
    assert!(first.try_lock_range(other.0, other.1).unwrap());
    assert_eq!(first.probe().lock_calls(), 7);
    first.close().unwrap();
}

#[test]
fn counters_see_commits_and_survive_recovery() {
    let dir = BenchDir::new("counters");
    let store = RedbStore::create(dir.file("store.redb"), RedbConfig::default()).unwrap();
    let before = store.stats().backend.unwrap();
    commit(
        &store,
        &block(None, 0, 1, &[(SpaceId::Namespace(1), 10, 100)]),
    );
    let after = store.stats().backend.unwrap();
    let delta = after.since(&before);
    assert_eq!(delta.syncs, 2);
    assert!(delta.writes > 0 && delta.write_bytes > 1_000);
    store.recover().unwrap();
    let recovered = store.stats().backend.unwrap();
    assert!(recovered.reads >= after.reads && recovered.syncs >= after.syncs);
}

#[test]
fn a_read_only_head_read_leaves_the_closed_file_as_it_is() {
    let dir = BenchDir::new("read-only-head");
    let path = dir.file("store.redb");
    let batch = block(None, 0, 1, &[(SpaceId::Host, 1, 32)]);
    let store = RedbStore::create(&path, RedbConfig::default()).unwrap();
    assert_eq!(
        read_only_head(&path).unwrap_err().kind,
        StorageErrorKind::Locked
    );
    drop(store);
    assert_eq!(read_only_head(&path).unwrap(), None);

    let store = RedbStore::open(&path, RedbConfig::default()).unwrap();
    commit(&store, &batch);
    drop(store);
    let closed = std::fs::read(&path).unwrap();
    for _ in 0..2 {
        assert_eq!(read_only_head(&path).unwrap(), Some(batch.new_head));
        assert_eq!(std::fs::read(&path).unwrap(), closed);
    }
    // Opening the store and closing it again rewrites part of the file: a
    // copy that must keep its hash is checked read-only.
    drop(RedbStore::open(&path, RedbConfig::default()).unwrap());
    assert_ne!(std::fs::read(&path).unwrap(), closed);
    assert_eq!(read_only_head(&path).unwrap(), Some(batch.new_head));

    // A file that is no store.
    let other = dir.file("other.redb");
    std::fs::write(&other, b"not a database").unwrap();
    assert!(read_only_head(&other).is_err());
    assert!(read_only_head(dir.file("absent.redb")).is_err());
}
