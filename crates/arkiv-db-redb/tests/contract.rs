//! The versioned storage contract suite against the redb store.
//!
//! Runs every case of `arkiv_db_storage::testing::contract` against:
//!
//! - `RedbStore` on a real file (in Cargo's target temporary directory, on
//!   disk, so every commit really syncs), reopened from its path;
//! - `RedbStore` on a simulated `CrashDisk`, reopened on the same device;
//! - `FaultyStore<RedbStore>` without armed faults;
//! - `MemoryVersionedStore` and `FaultyStore<MemoryVersionedStore>`, so the
//!   durable store and the reference run the identical suite in one binary.
//!
//! The `faulty_*` cases wrap each store in a latching `FaultyStore`
//! themselves. Object ids are `keccak256`, which the redb store verifies on
//! every read.

mod common;

use std::sync::Arc;

use arkiv_db_memory::MemoryVersionedStore;
use arkiv_db_redb::{RedbConfig, RedbStore, testing::CrashDisk};
use arkiv_db_storage::{
    testing::{
        FaultyStore,
        contract::{StoreFactory, TestStore},
    },
    versioned::{ObjectId, VersionedStore},
};
use common::{BenchDir, crash_store, keccak};

/// A small cache, so the suite exercises reads from the file.
fn config() -> RedbConfig {
    RedbConfig::with_cache_size(256 << 10)
}

fn file_store() -> TestStore {
    let dir = Arc::new(BenchDir::new("contract"));
    let path = dir.file("store.redb");
    let store = RedbStore::create(&path, config()).expect("create store");
    TestStore {
        store: Box::new(store),
        reopen: Some(Box::new(move || {
            let _keep = &dir;
            Box::new(RedbStore::open(&path, config()).expect("reopen store"))
        })),
    }
}

struct RedbFile;

impl StoreFactory for RedbFile {
    fn object_id(&self, bytes: &[u8]) -> ObjectId {
        keccak(bytes)
    }

    fn create(&self) -> TestStore {
        file_store()
    }
}

struct RedbCrashDisk;

impl StoreFactory for RedbCrashDisk {
    fn object_id(&self, bytes: &[u8]) -> ObjectId {
        keccak(bytes)
    }

    fn create(&self) -> TestStore {
        let disk = CrashDisk::new();
        let store = crash_store(&disk, config());
        TestStore {
            store: Box::new(store),
            reopen: Some(Box::new(move || Box::new(crash_store(&disk, config())))),
        }
    }
}

struct WrappedRedb;

impl StoreFactory for WrappedRedb {
    fn object_id(&self, bytes: &[u8]) -> ObjectId {
        keccak(bytes)
    }

    fn create(&self) -> TestStore {
        let TestStore { store, reopen } = file_store();
        TestStore {
            store: Box::new(FaultyStore::new(store)),
            reopen: reopen.map(|reopen| {
                Box::new(move || -> Box<dyn VersionedStore> {
                    Box::new(FaultyStore::new(reopen()))
                }) as Box<dyn Fn() -> Box<dyn VersionedStore>>
            }),
        }
    }
}

struct Memory;

impl StoreFactory for Memory {
    fn object_id(&self, bytes: &[u8]) -> ObjectId {
        keccak(bytes)
    }

    fn create(&self) -> TestStore {
        TestStore {
            store: Box::new(MemoryVersionedStore::new()),
            reopen: None,
        }
    }
}

struct WrappedMemory;

impl StoreFactory for WrappedMemory {
    fn object_id(&self, bytes: &[u8]) -> ObjectId {
        keccak(bytes)
    }

    fn create(&self) -> TestStore {
        TestStore {
            store: Box::new(FaultyStore::new(MemoryVersionedStore::new())),
            reopen: None,
        }
    }
}

mod redb_file {
    arkiv_db_storage::versioned_store_contract!(super::RedbFile);
}

mod redb_crash_disk {
    arkiv_db_storage::versioned_store_contract!(super::RedbCrashDisk);
}

mod faulty_redb {
    arkiv_db_storage::versioned_store_contract!(super::WrappedRedb);
}

mod memory {
    arkiv_db_storage::versioned_store_contract!(super::Memory);
}

mod faulty_memory {
    arkiv_db_storage::versioned_store_contract!(super::WrappedMemory);
}
