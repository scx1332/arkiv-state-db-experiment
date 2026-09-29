//! Bounded encoded bytes, gated together with recovery and store health.
use super::{DurableError, Options};
use alloy_primitives::keccak256;
use arkiv_db_storage::versioned::{
    DatabaseId, Health, ObjectId, ObjectReader, SpaceId, StorageError, StorageErrorKind,
    VersionedStore,
};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, MutexGuard},
};

type CacheKey = (DatabaseId, SpaceId, ObjectId);
type Entry = (CacheKey, Vec<u8>);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub payload_capacity: usize,
    pub entries: usize,
    pub container_bytes: usize,
    pub peak_payload_capacity: usize,
    pub peak_entries: usize,
    pub generation: u64,
}

struct Gate {
    available: bool,
    generation: u64,
    entries: VecDeque<Entry>,
    stats: CacheStats,
}

pub(crate) struct Context {
    pub(crate) store: Arc<dyn VersionedStore>,
    id: DatabaseId,
    max_object_bytes: usize,
    cache_bytes: usize,
    cache_entries: usize,
    gate: Mutex<Gate>,
}

impl Context {
    pub(crate) fn new(
        store: Arc<dyn VersionedStore>,
        max_object_bytes: usize,
        options: &Options,
    ) -> Self {
        Self {
            id: store.database_id(),
            store,
            max_object_bytes,
            cache_bytes: options.cache_bytes,
            cache_entries: options.cache_entries,
            gate: Mutex::new(Gate {
                available: true,
                generation: 0,
                entries: VecDeque::new(),
                stats: CacheStats::default(),
            }),
        }
    }

    fn lock(&self) -> Result<MutexGuard<'_, Gate>, StorageError> {
        self.gate.lock().map_err(|_| {
            StorageError::new(StorageErrorKind::Closed, "engine context gate poisoned")
        })
    }

    fn healthy(&self, gate: &Gate) -> Result<(), StorageError> {
        if !gate.available {
            return Err(StorageError::new(
                StorageErrorKind::Closed,
                "engine context unavailable; recover required",
            ));
        }
        match self.store.health() {
            Health::Available => Ok(()),
            Health::Failed(error) => Err(error),
        }
    }

    pub(crate) fn generation(&self) -> Result<u64, StorageError> {
        let gate = self.lock()?;
        self.healthy(&gate)?;
        Ok(gate.generation)
    }

    pub(crate) fn stats(&self) -> CacheStats {
        let gate = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        CacheStats {
            entries: gate.entries.len(),
            container_bytes: gate.entries.capacity() * std::mem::size_of::<Entry>(),
            generation: gate.generation,
            ..gate.stats
        }
    }

    /// All in-flight loads finish first. Validation uses raw bounded record
    /// reads, never recursively locks this context. On *any* error it stays fenced.
    pub(crate) fn recover<T>(
        &self,
        validate: impl FnOnce() -> Result<T, DurableError>,
    ) -> Result<T, DurableError> {
        let mut gate = self.lock()?;
        gate.available = false;
        gate.entries = VecDeque::new();
        gate.stats.payload_capacity = 0;
        gate.generation = gate
            .generation
            .checked_add(1)
            .ok_or(DurableError::SequenceExhausted)?;
        self.store.recover()?;
        let value = validate()?;
        if let Health::Failed(e) = self.store.health() {
            return Err(e.into());
        }
        gate.available = true;
        Ok(value)
    }
}

impl ObjectReader for Context {
    fn read_object(&self, space: SpaceId, id: &ObjectId) -> Result<Option<Vec<u8>>, StorageError> {
        let mut gate = self.lock()?;
        self.healthy(&gate)?;
        let key = (self.id, space, *id);
        if let Some(index) = gate.entries.iter().position(|(k, _)| *k == key) {
            gate.stats.hits = gate.stats.hits.saturating_add(1);
            // The clone belongs to the active read, not the retained cache.
            return Ok(Some(gate.entries[index].1.clone()));
        }
        gate.stats.misses = gate.stats.misses.saturating_add(1);
        let bytes = self
            .store
            .read_object_bounded(space, id, self.max_object_bytes)?;
        self.healthy(&gate)?;
        let Some(bytes) = bytes else {
            return Ok(None);
        };
        if bytes.len() > self.max_object_bytes || keccak256(&bytes).0 != *id {
            return Err(StorageError::new(
                StorageErrorKind::Corrupt,
                "object size/hash mismatch",
            ));
        }
        // Vec::clone reserves exactly len; account actual capacity nonetheless.
        if self.cache_bytes > 0 && self.cache_entries > 0 && bytes.len() <= self.cache_bytes {
            let retained = bytes.clone();
            if retained.capacity() <= self.cache_bytes {
                while gate.entries.len() >= self.cache_entries
                    || gate.stats.payload_capacity > self.cache_bytes - retained.capacity()
                {
                    if let Some((_, old)) = gate.entries.pop_front() {
                        gate.stats.payload_capacity -= old.capacity();
                    } else {
                        break;
                    }
                }
                // Reserve exact capacity, rather than geometric growth beyond
                // the configured entry limit. FIFO metadata never accumulates.
                if gate.entries.len() == gate.entries.capacity() {
                    gate.entries.reserve_exact(1);
                }
                gate.stats.payload_capacity += retained.capacity();
                gate.entries.push_back((key, retained));
                gate.stats.peak_payload_capacity = gate
                    .stats
                    .peak_payload_capacity
                    .max(gate.stats.payload_capacity);
                gate.stats.peak_entries = gate.stats.peak_entries.max(gate.entries.len());
            }
        }
        Ok(Some(bytes))
    }
}
