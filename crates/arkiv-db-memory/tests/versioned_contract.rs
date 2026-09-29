//! The versioned storage contract suite against the memory store.
//!
//! Runs every case of `arkiv_db_storage::testing::contract` against
//! `MemoryVersionedStore` and against the same store wrapped in a
//! `FaultyStore` without armed faults. The `faulty_*` cases wrap the store in
//! a latching `FaultyStore` themselves. The memory store is volatile, so the
//! reopen case has nothing to check here.
//!
//! Object ids come from a standard-library stand-in for `keccak256` (four
//! differently seeded FNV-1a lanes): the memory store never verifies content
//! addresses, it only needs distinct ids for distinct test objects.

use arkiv_db_memory::MemoryVersionedStore;
use arkiv_db_storage::{
    testing::{
        FaultyStore,
        contract::{StoreFactory, TestStore},
    },
    versioned::ObjectId,
};

fn fnv_id(bytes: &[u8]) -> ObjectId {
    let mut out = [0u8; 32];
    for (lane, chunk) in out.chunks_mut(8).enumerate() {
        let mut hash = 0xcbf2_9ce4_8422_2325_u64 ^ (lane as u64).wrapping_mul(0x9e37_79b9);
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
        chunk.copy_from_slice(&hash.to_be_bytes());
    }
    out
}

struct Memory;

impl StoreFactory for Memory {
    fn object_id(&self, bytes: &[u8]) -> ObjectId {
        fnv_id(bytes)
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
        fnv_id(bytes)
    }

    fn create(&self) -> TestStore {
        TestStore {
            store: Box::new(FaultyStore::new(MemoryVersionedStore::new())),
            reopen: None,
        }
    }
}

mod memory {
    arkiv_db_storage::versioned_store_contract!(super::Memory);
}

mod faulty_memory {
    arkiv_db_storage::versioned_store_contract!(super::WrappedMemory);
}
