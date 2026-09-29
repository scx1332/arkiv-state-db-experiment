//! Actual child-process kills at the two facade publication boundaries.

mod common;
mod engine_support;

use arkiv_db::durable::DurableEngine;
use arkiv_db_redb::RedbStore;
use arkiv_db_storage::versioned::{
    CommitBatch, CommitOutcome, DatabaseId, Head, Health, ObjectId, ObjectReader, SpaceId,
    StorageError, StoreStats, Table, VersionedStore,
};
use common::BenchDir;
use engine_support::*;
use std::{
    io::{BufRead, BufReader, Write},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};

fn pause() -> ! {
    println!("ENGINE_KILL_BOUNDARY");
    std::io::stdout().flush().unwrap();
    loop {
        std::thread::park();
    }
}

/// The only interception is after the real durable adapter returns success.
/// It stops delivery of that result to the facade; all storage is real redb.
struct BoundaryStore {
    inner: RedbStore,
    pause_after_commit: AtomicBool,
}
impl ObjectReader for BoundaryStore {
    fn read_object(&self, s: SpaceId, id: &ObjectId) -> Result<Option<Vec<u8>>, StorageError> {
        self.inner.read_object(s, id)
    }
    fn read_object_bounded(
        &self,
        s: SpaceId,
        id: &ObjectId,
        cap: usize,
    ) -> Result<Option<Vec<u8>>, StorageError> {
        self.inner.read_object_bounded(s, id, cap)
    }
}
impl VersionedStore for BoundaryStore {
    fn database_id(&self) -> DatabaseId {
        self.inner.database_id()
    }
    fn head(&self) -> Result<Option<Head>, StorageError> {
        self.inner.head()
    }
    fn read_record(&self, t: Table, k: &[u8]) -> Result<Option<Vec<u8>>, StorageError> {
        self.inner.read_record(t, k)
    }
    fn read_record_bounded(
        &self,
        t: Table,
        k: &[u8],
        cap: usize,
    ) -> Result<Option<Vec<u8>>, StorageError> {
        self.inner.read_record_bounded(t, k, cap)
    }
    fn commit(&self, b: CommitBatch) -> CommitOutcome {
        let outcome = self.inner.commit(b);
        if outcome == CommitOutcome::Committed && self.pause_after_commit.load(Ordering::SeqCst) {
            pause();
        }
        outcome
    }
    fn health(&self) -> Health {
        self.inner.health()
    }
    fn recover(&self) -> Result<Option<Head>, StorageError> {
        self.inner.recover()
    }
    fn stats(&self) -> StoreStats {
        self.inner.stats()
    }
}

#[test]
fn engine_process_child() {
    let Ok(path) = std::env::var("ARKIV_ENGINE_CHILD_PATH") else {
        return;
    };
    let mode = std::env::var("ARKIV_ENGINE_CHILD_MODE").unwrap();
    let store = Arc::new(BoundaryStore {
        inner: RedbStore::create(path, config()).unwrap(),
        pause_after_commit: AtomicBool::new(false),
    });
    let mut engine = initialize(store.clone(), 0);
    let mut branch = engine.begin(engine.head(), None).unwrap();
    base(&mut branch);
    publish(&mut engine, branch);
    let mut branch = engine.begin(engine.head(), None).unwrap();
    candidate(&mut branch, false);
    engine.prepare(branch.seal()).unwrap();
    if mode == "before" {
        pause();
    }
    store.pause_after_commit.store(true, Ordering::SeqCst);
    engine.commit().unwrap();
    panic!("child passed intercepted commit boundary");
}

#[test]
fn process_kills_before_commit_and_after_disk_commit_before_publication() {
    let (parent, expected) = memory_snapshots(false);
    for (mode, sequence) in [("before", 1), ("after", 2)] {
        let dir = BenchDir::new("engine-process");
        let path = dir.file("engine.redb");
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "engine_process_child", "--nocapture"])
            .env("ARKIV_ENGINE_CHILD_PATH", &path)
            .env("ARKIV_ENGINE_CHILD_MODE", mode)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (send, recv) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if line.unwrap().contains("ENGINE_KILL_BOUNDARY") {
                    send.send(()).unwrap();
                    return;
                }
            }
        });
        let boundary = recv.recv_timeout(Duration::from_secs(30));
        child.kill().unwrap();
        let status = child.wait().unwrap();
        reader.join().unwrap();
        boundary.expect("child reached real engine commit boundary");
        assert!(!status.success());
        let store = Arc::new(RedbStore::open(&path, config()).unwrap());
        let engine = DurableEngine::open(store.clone(), genesis(), options(0)).unwrap();
        assert_eq!(engine.head().sequence(), sequence);
        assert!(!store.last_open().full_repair);
        check(&engine, &parent, &expected, false);
    }
}
