//! Creating a store survives a crash at any point of the creation.
//!
//! redb initializes a new file in place in several synced steps and writes
//! its magic number last; the store's initialization commit follows. A file
//! left without the magic number is refused by every later open.
//!
//! - **In place** (`RedbStore::with_backend` on an empty `CrashDisk`), the
//!   creation is cut off at every mutating backend call, and each frozen
//!   device yields crash images (synced only, all unsynced, seeded torn
//!   subsets). Every image either opens (a fresh, complete or re-initialized
//!   store) or is refused as `Incompatible`, and refused images do occur:
//!   in-place creation is not crash-safe.
//! - **`RedbStore::create` on a path** builds the store in `<path>.init` and
//!   renames it into place only after the initialization commit, then syncs
//!   the directory. A crash before the rename leaves the target absent and
//!   at most the temporary file, holding one of the images above. For every
//!   such leftover, with the target absent or empty, the next `create`
//!   initializes a fresh store, the temporary file is gone, and `open` finds
//!   the same database. A crash after the rename leaves the complete image
//!   under the target name, which `create` opens unchanged.
//! - Two concurrent `create`s of one new path: exactly one succeeds, the
//!   other is `Locked`; no temporary file remains and the store reopens.
//!
//! Scope: the crash images come from the `CrashDisk` model (see
//! `atomicity.rs`); the rename and directory sync are real file-system calls
//! whose atomicity is the file system's.

mod common;

use std::sync::Barrier;

use arkiv_db_redb::{RedbStore, initialization_path, testing::CrashDisk};
use arkiv_db_storage::versioned::{StorageErrorKind, VersionedStore};
use common::{
    BenchDir,
    creation::{config, create_over_leftovers, creation_images, reopen_in_place},
};

#[test]
fn in_place_creation_can_leave_a_refused_file() {
    let created = creation_images();
    let counts = reopen_in_place(&created.images);
    println!(
        "{} calls, {} distinct in-place creation crash images: {counts:?}",
        created.calls,
        created.images.len()
    );
    assert!(counts.initialized > 0 && counts.opened > 0);
    assert!(counts.refused > 0, "in-place creation must show the hazard");
}

#[test]
fn create_discards_every_interrupted_creation() {
    let created = creation_images();
    let dir = BenchDir::new("initialization");
    let checked = create_over_leftovers(dir.path(), &created.images);
    println!("{checked} leftover temporary files discarded");
}

#[test]
fn create_opens_a_store_renamed_into_place() {
    let complete = creation_images().complete;
    let id = {
        let disk = CrashDisk::from_image(complete.clone());
        RedbStore::with_backend(move || Ok(disk.backend()), config())
            .unwrap()
            .database_id()
    };
    let dir = BenchDir::new("initialization-renamed");
    let path = dir.file("store.redb");
    std::fs::write(&path, &complete).unwrap();
    let store = RedbStore::create(&path, config()).unwrap();
    assert!(!store.last_open().initialized);
    assert_eq!(store.database_id(), id);
}

#[test]
fn concurrent_creates_of_one_path_create_one_store() {
    let dir = BenchDir::new("initialization-race");
    for round in 0..16 {
        let path = dir.file(&format!("race-{round}.redb"));
        let barrier = Barrier::new(2);
        let results: Vec<_> = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..2)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        RedbStore::create(&path, config())
                    })
                })
                .collect();
            workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .collect()
        });
        let mut created = None;
        for result in results {
            match result {
                Ok(store) => {
                    assert!(created.is_none(), "round {round}: two stores created");
                    assert!(store.last_open().initialized);
                    created = Some(store.database_id());
                }
                Err(error) => assert_eq!(error.kind, StorageErrorKind::Locked, "{error}"),
            }
        }
        let id = created.unwrap_or_else(|| panic!("round {round}: no store created"));
        assert!(!initialization_path(&path).exists());
        assert_eq!(RedbStore::open(&path, config()).unwrap().database_id(), id);
    }
}
