//! Bounded encoded bytes, gated together with recovery and store health.
use super::{DurableError, Options};
use alloy_primitives::keccak256;
use arkiv_db_storage::versioned::{
    DatabaseId, Health, ObjectId, ObjectReader, SpaceId, StorageError, StorageErrorKind,
    VersionedStore,
};
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex, MutexGuard},
};

type CacheKey = (DatabaseId, SpaceId, ObjectId);
type Entry = (CacheKey, Vec<u8>);
/// Index slot: the FIFO sequence number of the key's entry.
type IndexEntry = (CacheKey, u64);

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
    /// Approximate allocation of the key index beside the FIFO, separate
    /// from `container_bytes`. Its capacity follows the FIFO's, so it stays
    /// within a small constant multiple of the configured entry limit.
    pub index_bytes: usize,
}

/// FIFO of retained objects plus a key index for O(1) lookup.
///
/// Every retained entry has exactly one index slot holding its sequence
/// number; the entry sits at `entries[seq - popped]`. Keys are unique: an
/// insert of a key already present is skipped, so the index never points at
/// a stale or duplicate entry.
struct Gate {
    available: bool,
    generation: u64,
    entries: VecDeque<Entry>,
    index: HashMap<CacheKey, u64>,
    /// Sequence number of `entries.front()` (or of the next push if empty).
    popped: u64,
    stats: CacheStats,
}

impl Gate {
    fn new() -> Self {
        Self {
            available: true,
            generation: 0,
            entries: VecDeque::new(),
            index: HashMap::new(),
            popped: 0,
            stats: CacheStats::default(),
        }
    }

    fn get(&self, key: &CacheKey) -> Option<&Vec<u8>> {
        let seq = *self.index.get(key)?;
        let position = usize::try_from(seq.wrapping_sub(self.popped)).ok()?;
        let (stored, bytes) = self.entries.get(position)?;
        debug_assert_eq!(stored, key, "cache index points at another entry");
        // Never serve another key's bytes, even if the invariant broke.
        (stored == key).then_some(bytes)
    }

    fn evict_front(&mut self) -> bool {
        let Some((key, old)) = self.entries.pop_front() else {
            return false;
        };
        // Only the slot of this exact entry; keys are unique, so it always is.
        if self.index.get(&key) == Some(&self.popped) {
            self.index.remove(&key);
        }
        self.popped = self.popped.wrapping_add(1);
        self.stats.payload_capacity -= old.capacity();
        true
    }

    /// Drops every entry and index slot and releases their allocations.
    fn clear(&mut self) {
        self.entries = VecDeque::new();
        self.index = HashMap::new();
        self.popped = 0;
        self.stats.payload_capacity = 0;
    }

    /// Retains `bytes` under `key` within both limits, evicting oldest first.
    /// Skips keys already retained (two loads of one object) and objects that
    /// cannot fit at all.
    fn insert(&mut self, key: CacheKey, bytes: &[u8], max_bytes: usize, max_entries: usize) {
        if max_bytes == 0
            || max_entries == 0
            || bytes.len() > max_bytes
            || self.index.contains_key(&key)
        {
            return;
        }
        // Vec::clone reserves exactly len; account actual capacity nonetheless.
        let retained = bytes.to_vec();
        if retained.capacity() > max_bytes {
            return;
        }
        while self.entries.len() >= max_entries
            || self.stats.payload_capacity > max_bytes - retained.capacity()
        {
            if !self.evict_front() {
                break;
            }
        }
        // Grow geometrically but never past the configured entry limit, so
        // filling costs amortized O(1) and FIFO metadata never exceeds
        // `max_entries` slots. The index is sized alongside.
        if self.entries.len() == self.entries.capacity() {
            let grow = self
                .entries
                .len()
                .max(16)
                .min(max_entries - self.entries.len());
            self.entries.reserve_exact(grow);
            let target = self.entries.capacity().min(max_entries);
            self.index.reserve(target.saturating_sub(self.index.len()));
        }
        let seq = self.popped.wrapping_add(self.entries.len() as u64);
        self.index.insert(key, seq);
        self.stats.payload_capacity += retained.capacity();
        self.entries.push_back((key, retained));
        self.stats.peak_payload_capacity = self
            .stats
            .peak_payload_capacity
            .max(self.stats.payload_capacity);
        self.stats.peak_entries = self.stats.peak_entries.max(self.entries.len());
    }
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
            gate: Mutex::new(Gate::new()),
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
            index_bytes: gate.index.capacity() * std::mem::size_of::<IndexEntry>(),
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
        gate.clear();
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
        if let Some(bytes) = gate.get(&key) {
            // The clone belongs to the active read, not the retained cache.
            let bytes = bytes.clone();
            gate.stats.hits = gate.stats.hits.saturating_add(1);
            return Ok(Some(bytes));
        }
        gate.stats.misses = gate.stats.misses.saturating_add(1);
        let generation = gate.generation;
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
        // The gate is held across the load, so no recovery intervenes; the
        // check keeps a load from an older generation out of the cache should
        // the load ever run unlocked.
        if gate.generation == generation {
            gate.insert(key, &bytes, self.cache_bytes, self.cache_entries);
        }
        Ok(Some(bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arkiv_db_memory::MemoryVersionedStore;
    use arkiv_db_storage::versioned::{CommitBatch, CommitOutcome, Head};

    const SPACE: SpaceId = SpaceId::Namespace(1);

    fn object(i: u8) -> (ObjectId, Vec<u8>) {
        let bytes = vec![i; 16];
        (keccak256(&bytes).0, bytes)
    }

    /// A store holding objects `0..count` and a context over it.
    fn context(count: u8, cache_bytes: usize, cache_entries: usize) -> Context {
        let store = Arc::new(MemoryVersionedStore::new());
        let mut batch = CommitBatch::new(
            None,
            Head {
                height: 1,
                hash: [1; 32],
            },
        );
        for i in 0..count {
            let (id, bytes) = object(i);
            batch.put_object(SPACE, id, bytes);
        }
        assert!(matches!(store.commit(batch), CommitOutcome::Committed));
        let options = Options {
            cache_bytes,
            cache_entries,
            ..Options::default()
        };
        Context::new(store, 1024, &options)
    }

    fn read(context: &Context, i: u8) {
        let (id, bytes) = object(i);
        assert_eq!(context.read_object(SPACE, &id).unwrap(), Some(bytes));
    }

    fn key(context: &Context, i: u8) -> CacheKey {
        (context.id, SPACE, object(i).0)
    }

    /// Every index slot addresses its own entry and every entry has a slot.
    fn assert_consistent(gate: &Gate) {
        assert_eq!(gate.index.len(), gate.entries.len());
        for (key, bytes) in &gate.entries {
            assert_eq!(gate.get(key), Some(bytes));
        }
        let payload: usize = gate.entries.iter().map(|(_, b)| b.capacity()).sum();
        assert_eq!(gate.stats.payload_capacity, payload);
    }

    #[test]
    fn a_retained_object_hits_after_its_insert() {
        let context = context(3, 1024, 8);
        read(&context, 0);
        read(&context, 1);
        read(&context, 0);
        let stats = context.stats();
        assert_eq!((stats.misses, stats.hits, stats.entries), (2, 1, 2));
        assert!(stats.index_bytes > 0);
        assert_consistent(&context.lock().unwrap());
    }

    #[test]
    fn eviction_removes_the_index_slot_of_the_evicted_entry() {
        let context = context(4, 1024, 2);
        for i in 0..3 {
            read(&context, i);
        }
        {
            let gate = context.lock().unwrap();
            assert!(!gate.index.contains_key(&key(&context, 0)));
            assert!(gate.get(&key(&context, 1)).is_some());
            assert!(gate.get(&key(&context, 2)).is_some());
            assert_consistent(&gate);
        }
        read(&context, 0);
        let stats = context.stats();
        assert_eq!((stats.misses, stats.hits, stats.entries), (4, 0, 2));
        assert_consistent(&context.lock().unwrap());
    }

    #[test]
    fn byte_eviction_keeps_the_index_consistent() {
        // Room for two 16-byte payloads: entries are evicted by bytes.
        let context = context(8, 32, 100);
        for i in (0..8).chain(0..8) {
            read(&context, i);
            let gate = context.lock().unwrap();
            assert!(gate.stats.payload_capacity <= 32);
            assert_consistent(&gate);
        }
    }

    #[test]
    fn a_duplicate_insert_is_skipped() {
        let context = context(1, 1024, 8);
        let key = key(&context, 0);
        let (_, bytes) = object(0);
        let mut gate = context.lock().unwrap();
        gate.insert(key, &bytes, 1024, 8);
        let before = gate.stats;
        gate.insert(key, &bytes, 1024, 8);
        assert_eq!(gate.entries.len(), 1);
        assert_eq!(gate.stats, before);
        assert_consistent(&gate);
    }

    #[test]
    fn recovery_clears_the_index_and_restarts_the_fifo() {
        let context = context(3, 1024, 2);
        for i in 0..3 {
            read(&context, i);
        }
        context.recover(|| Ok(())).unwrap();
        {
            let gate = context.lock().unwrap();
            assert!(gate.entries.is_empty() && gate.index.is_empty());
            assert_eq!((gate.popped, gate.stats.payload_capacity), (0, 0));
        }
        let stats = context.stats();
        assert_eq!((stats.generation, stats.index_bytes), (1, 0));
        read(&context, 2);
        read(&context, 2);
        let stats = context.stats();
        assert_eq!((stats.misses, stats.hits), (4, 1));
        assert_consistent(&context.lock().unwrap());
    }

    #[test]
    fn index_matches_a_linear_scan_through_long_fifo_churn() {
        let context = context(40, 1024, 7);
        for round in 0..400_u32 {
            let i = (round.wrapping_mul(17) % 40) as u8;
            let present = {
                let gate = context.lock().unwrap();
                gate.entries.iter().any(|(k, _)| *k == key(&context, i))
            };
            let hits = context.stats().hits;
            read(&context, i);
            assert_eq!(context.stats().hits - hits, u64::from(present));
            let gate = context.lock().unwrap();
            assert!(gate.entries.len() <= 7 && gate.entries.capacity() <= 7);
            assert!(gate.index.capacity() <= 4 * 7);
            assert_consistent(&gate);
        }
    }
}
