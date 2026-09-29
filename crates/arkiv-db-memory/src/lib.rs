//! In-memory storage structures: the mutable ordered KV backend, the
//! immutable, structurally shared map used for engine snapshots, and the
//! volatile reference implementation of the versioned storage contract.

mod persistent;
mod versioned;

use std::{collections::BTreeMap, convert::Infallible, ops::Bound};

use arkiv_db_storage::{Entry, KvStore, WriteBatch, WriteOperation};

pub use persistent::{PersistentMap, Probe, Range};
pub use versioned::MemoryVersionedStore;

/// A volatile store backed by the standard library's ordered map.
///
/// All reads own their results. Writes require exclusive access and have no
/// recoverable errors. Data is lost when the store is dropped; this backend
/// does not provide persistence or crash recovery.
#[derive(Debug, Default)]
pub struct MemoryStore {
    entries: BTreeMap<Vec<u8>, Vec<u8>>,
}

impl MemoryStore {
    /// Creates an empty store.
    pub const fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }
}

impl KvStore for MemoryStore {
    type Error = Infallible;

    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Self::Error> {
        Ok(self.entries.get(key).cloned())
    }

    fn scan_prefix(&self, prefix: &[u8]) -> Result<Vec<Entry>, Self::Error> {
        // Seek directly to the first possible match. Stopping at the first
        // nonmatch also handles prefixes ending in 0xff without an upper-bound
        // successor calculation or a full-store scan.
        Ok(self
            .entries
            .range::<[u8], _>((Bound::Included(prefix), Bound::Unbounded))
            .take_while(|(key, _)| key.starts_with(prefix))
            .map(|(key, value)| Entry {
                key: key.clone(),
                value: value.clone(),
            })
            .collect())
    }

    fn write_batch(&mut self, batch: WriteBatch) -> Result<(), Self::Error> {
        // Exclusive access prevents observers from seeing intermediate writes.
        // No operation here can return a recoverable storage error partway
        // through the batch; allocation failure and panics are not recovered.
        for operation in batch {
            match operation {
                WriteOperation::Put { key, value } => {
                    self.entries.insert(key, value);
                }
                WriteOperation::Delete { key } => {
                    self.entries.remove(&key);
                }
            }
        }
        Ok(())
    }
}
