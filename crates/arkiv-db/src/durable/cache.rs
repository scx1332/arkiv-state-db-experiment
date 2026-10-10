//! The bounded decoded-node cache, gated together with recovery and store
//! health.
//!
//! Entries are decoded trie nodes keyed by `(space, map kind, object id)`.
//! Objects are content-addressed, so an entry is valid for every snapshot,
//! branch and historical state that reaches it, and commits never
//! invalidate anything. A hit is an `Arc` clone: the bytes were verified
//! against the id and decoded once, on the miss that inserted the node.
//!
//! Concurrency: a lifecycle `RwLock` lets any number of loads run at once
//! (each holds a read guard from its health check until its insert), while
//! [`Context::recover`] takes the write guard, so it waits for every
//! in-flight load and no load from before a recovery can insert after it.
//! The cache itself is split into shards, each behind its own mutex held
//! only for a lookup or an insert; store reads and decoding never run under
//! a shard lock. Counters are atomics, and [`Context::stats`] takes no
//! lifecycle lock. Lock order: lifecycle, then one shard, then the store's
//! own locks.
//!
//! Eviction is CLOCK (second chance): a hit marks its entry referenced, and
//! eviction re-queues a referenced entry once, clearing the mark, before it
//! evicts an unreferenced one. Upper trie levels that every operation
//! passes through therefore stay resident while scans churn the leaves.
use super::{DurableError, Options};
use crate::state::MapKind;
use alloy_primitives::map::HashMap;
use arkiv_db_storage::versioned::{
    Health, ObjectId, SpaceId, StorageError, StorageErrorKind, VersionedStore,
};
use arkiv_db_trie::{NodeBox, TrieError, TrieValue};
use std::{
    any::Any,
    collections::VecDeque,
    sync::{
        Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, TryLockError,
        atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed},
    },
    time::Instant,
};

type Key = (SpaceId, MapKind, [u8; 32]);
type Erased = Arc<dyn Any + Send + Sync>;
/// Index slot: the queue sequence number of the key's entry.
type IndexEntry = (Key, u64);

/// At most this many shards.
const MAX_SHARDS: usize = 16;
/// A shard is only split off while each keeps at least this byte budget
/// and this many entries, so second chances still have room to work.
const MIN_SHARD_BYTES: usize = 64 * 1024;
const MIN_SHARD_ENTRIES: usize = 32;

/// Noncanonical counters of the decoded-node cache.
///
/// Byte figures are the decoded nodes' [`NodeBox::retained_bytes`]
/// (approximate heap ownership), not their encoded size. Nodes that callers
/// still hold after eviction or recovery belong to those callers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    /// Retained decoded bytes now; at most `cache_bytes`.
    pub retained_bytes: usize,
    pub peak_retained_bytes: usize,
    pub entries: usize,
    pub peak_entries: usize,
    /// Allocation of the shards' entry queues.
    pub container_bytes: usize,
    /// Approximate allocation of the shards' key indexes beside the queues.
    /// Their capacity follows the queues', so it stays within a small
    /// constant multiple of the configured entry limit.
    pub index_bytes: usize,
    pub generation: u64,
    /// Nodes retained after a miss.
    pub inserts: u64,
    pub evictions: u64,
    /// Referenced entries re-queued once instead of evicted.
    pub second_chances: u64,
    /// Decoded nodes larger than their shard's whole byte budget.
    pub oversize_rejects: u64,
    /// Misses whose insert found the node already retained by a concurrent
    /// load; the retained node is returned.
    pub insert_races: u64,
    /// Hits and misses per map, indexed by [`MapKind::index`].
    pub hits_by_map: [u64; 7],
    pub misses_by_map: [u64; 7],
    pub shards: usize,
    /// Shard lock acquisitions that had to wait for another thread.
    pub shard_contended: u64,
    /// Total time of store reads on misses, nanoseconds.
    pub miss_read_ns: u64,
    /// Total time of verifying and decoding missed nodes, nanoseconds.
    pub decode_ns: u64,
}

/// Why a [`Context::load`] failed.
#[derive(Debug)]
pub(crate) enum LoadFailure {
    /// The store or the context is unavailable, or the read failed.
    Storage(StorageError),
    /// The bytes do not hash to the id or do not decode.
    Decode(TrieError),
}

impl From<StorageError> for LoadFailure {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}

struct Entry {
    key: Key,
    node: Erased,
    charge: usize,
    referenced: bool,
}

/// What [`Shard::insert`] did.
enum Inserted {
    Retained,
    /// Another load retained the key first; its node.
    Raced(Erased),
    Oversize,
}

/// One shard: a CLOCK queue of retained entries plus a key index.
///
/// Every retained entry has exactly one index slot holding its sequence
/// number; the entry sits at `entries[seq - popped]`. Keys are unique.
struct Shard {
    entries: VecDeque<Entry>,
    index: HashMap<Key, u64>,
    /// Sequence number of `entries.front()` (or of the next push if empty).
    popped: u64,
    charged: usize,
    max_bytes: usize,
    max_entries: usize,
}

impl Shard {
    fn new(max_bytes: usize, max_entries: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            index: HashMap::default(),
            popped: 0,
            charged: 0,
            max_bytes,
            max_entries,
        }
    }

    fn position(&self, key: &Key) -> Option<usize> {
        let seq = *self.index.get(key)?;
        let position = usize::try_from(seq.wrapping_sub(self.popped)).ok()?;
        let entry = self.entries.get(position)?;
        debug_assert!(entry.key == *key, "cache index points at another entry");
        // Never serve another key's node, even if the invariant broke.
        (entry.key == *key).then_some(position)
    }

    /// The retained node under `key`, marked referenced.
    fn get(&mut self, key: &Key) -> Option<Erased> {
        let position = self.position(key)?;
        let entry = &mut self.entries[position];
        entry.referenced = true;
        Some(Arc::clone(&entry.node))
    }

    fn push(&mut self, entry: Entry) {
        let seq = self.popped.wrapping_add(self.entries.len() as u64);
        self.index.insert(entry.key, seq);
        self.entries.push_back(entry);
    }

    /// Pops the front entry; a referenced one is re-queued at the back with
    /// its mark cleared. Returns false only when the shard is empty.
    fn advance(&mut self, counters: &Counters) -> bool {
        let Some(mut entry) = self.entries.pop_front() else {
            return false;
        };
        if self.index.get(&entry.key) == Some(&self.popped) {
            self.index.remove(&entry.key);
        }
        self.popped = self.popped.wrapping_add(1);
        if entry.referenced {
            entry.referenced = false;
            self.push(entry);
            counters.second_chances.fetch_add(1, Relaxed);
        } else {
            self.charged -= entry.charge;
            counters.retained.fetch_sub(entry.charge, Relaxed);
            counters.entries.fetch_sub(1, Relaxed);
            counters.evictions.fetch_add(1, Relaxed);
        }
        true
    }

    /// Retains `node` within both limits, evicting with second chances.
    fn insert(&mut self, key: Key, node: Erased, charge: usize, counters: &Counters) -> Inserted {
        if let Some(position) = self.position(&key) {
            let entry = &mut self.entries[position];
            entry.referenced = true;
            return Inserted::Raced(Arc::clone(&entry.node));
        }
        if self.max_entries == 0 || charge > self.max_bytes {
            return Inserted::Oversize;
        }
        // Each pass clears a mark or evicts, so this ends within two laps.
        while self.entries.len() >= self.max_entries || self.charged > self.max_bytes - charge {
            if !self.advance(counters) {
                break;
            }
        }
        // Grow geometrically but never past the entry limit, so filling
        // costs amortized O(1) and queue metadata never exceeds
        // `max_entries` slots. The index is sized alongside.
        if self.entries.len() == self.entries.capacity() {
            let grow = self
                .entries
                .len()
                .max(16)
                .min(self.max_entries - self.entries.len());
            self.entries.reserve_exact(grow);
            let target = self.entries.capacity().min(self.max_entries);
            self.index.reserve(target.saturating_sub(self.index.len()));
        }
        self.push(Entry {
            key,
            node,
            charge,
            referenced: false,
        });
        self.charged += charge;
        let retained = counters.retained.fetch_add(charge, Relaxed) + charge;
        counters.peak_retained.fetch_max(retained, Relaxed);
        let entries = counters.entries.fetch_add(1, Relaxed) + 1;
        counters.peak_entries.fetch_max(entries, Relaxed);
        counters.inserts.fetch_add(1, Relaxed);
        Inserted::Retained
    }

    /// Drops every entry and index slot and releases their allocations.
    fn clear(&mut self) {
        self.entries = VecDeque::new();
        self.index = HashMap::default();
        self.popped = 0;
        self.charged = 0;
    }
}

#[derive(Default)]
struct Counters {
    hits: AtomicU64,
    misses: AtomicU64,
    inserts: AtomicU64,
    evictions: AtomicU64,
    second_chances: AtomicU64,
    oversize_rejects: AtomicU64,
    insert_races: AtomicU64,
    hits_by_map: [AtomicU64; 7],
    misses_by_map: [AtomicU64; 7],
    shard_contended: AtomicU64,
    miss_read_ns: AtomicU64,
    decode_ns: AtomicU64,
    retained: AtomicUsize,
    entries: AtomicUsize,
    peak_retained: AtomicUsize,
    peak_entries: AtomicUsize,
    /// Mirror of the lifecycle generation, so stats never wait on recovery.
    generation: AtomicU64,
}

struct Lifecycle {
    available: bool,
    generation: u64,
}

pub(crate) struct Context {
    pub(crate) store: Arc<dyn VersionedStore>,
    max_object_bytes: usize,
    lifecycle: RwLock<Lifecycle>,
    shards: Box<[Mutex<Shard>]>,
    counters: Counters,
}

fn poisoned() -> StorageError {
    StorageError::new(StorageErrorKind::Closed, "engine context gate poisoned")
}

/// A power of two up to [`MAX_SHARDS`], at least one, and otherwise no more
/// than one shard per [`MIN_SHARD_BYTES`] and per [`MIN_SHARD_ENTRIES`].
fn shard_count(bytes: usize, entries: usize) -> usize {
    let mut count = MAX_SHARDS;
    while count > 1 && (count > entries / MIN_SHARD_ENTRIES || count > bytes / MIN_SHARD_BYTES) {
        count /= 2;
    }
    count
}

/// Shard `index`'s share of `total`; the shares add up to `total` exactly.
fn share(total: usize, count: usize, index: usize) -> usize {
    total / count + usize::from(index < total % count)
}

fn elapsed_ns(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

impl Context {
    pub(crate) fn new(
        store: Arc<dyn VersionedStore>,
        max_object_bytes: usize,
        options: &Options,
    ) -> Self {
        let count = shard_count(options.cache_bytes, options.cache_entries);
        let shards = (0..count)
            .map(|index| {
                Mutex::new(Shard::new(
                    share(options.cache_bytes, count, index),
                    share(options.cache_entries, count, index),
                ))
            })
            .collect();
        Self {
            store,
            max_object_bytes,
            lifecycle: RwLock::new(Lifecycle {
                available: true,
                generation: 0,
            }),
            shards,
            counters: Counters::default(),
        }
    }

    fn lifecycle(&self) -> Result<RwLockReadGuard<'_, Lifecycle>, StorageError> {
        self.lifecycle.read().map_err(|_| poisoned())
    }

    /// Ids are uniform hashes, so their first byte spreads keys evenly.
    fn shard(&self, id: &[u8; 32]) -> &Mutex<Shard> {
        &self.shards[usize::from(id[0]) & (self.shards.len() - 1)]
    }

    fn lock<'a>(&self, shard: &'a Mutex<Shard>) -> Result<MutexGuard<'a, Shard>, StorageError> {
        match shard.try_lock() {
            Ok(guard) => Ok(guard),
            Err(TryLockError::WouldBlock) => {
                self.counters.shard_contended.fetch_add(1, Relaxed);
                shard.lock().map_err(|_| poisoned())
            }
            Err(TryLockError::Poisoned(_)) => Err(poisoned()),
        }
    }

    /// The context is available and the store has not latched a failure.
    /// Checked before every hit, so a warm cache never hides a failure.
    fn healthy(&self, lifecycle: &Lifecycle) -> Result<(), StorageError> {
        if !lifecycle.available {
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
        let lifecycle = self.lifecycle()?;
        self.healthy(&lifecycle)?;
        Ok(lifecycle.generation)
    }

    pub(crate) fn stats(&self) -> CacheStats {
        let counters = &self.counters;
        let load = |counter: &AtomicU64| counter.load(Relaxed);
        let mut stats = CacheStats {
            hits: load(&counters.hits),
            misses: load(&counters.misses),
            retained_bytes: counters.retained.load(Relaxed),
            peak_retained_bytes: counters.peak_retained.load(Relaxed),
            entries: counters.entries.load(Relaxed),
            peak_entries: counters.peak_entries.load(Relaxed),
            container_bytes: 0,
            index_bytes: 0,
            generation: load(&counters.generation),
            inserts: load(&counters.inserts),
            evictions: load(&counters.evictions),
            second_chances: load(&counters.second_chances),
            oversize_rejects: load(&counters.oversize_rejects),
            insert_races: load(&counters.insert_races),
            hits_by_map: counters.hits_by_map.each_ref().map(load),
            misses_by_map: counters.misses_by_map.each_ref().map(load),
            shards: self.shards.len(),
            shard_contended: load(&counters.shard_contended),
            miss_read_ns: load(&counters.miss_read_ns),
            decode_ns: load(&counters.decode_ns),
        };
        for shard in &self.shards {
            let shard = shard.lock().unwrap_or_else(|e| e.into_inner());
            stats.container_bytes += shard.entries.capacity() * std::mem::size_of::<Entry>();
            stats.index_bytes += shard.index.capacity() * std::mem::size_of::<IndexEntry>();
        }
        stats
    }

    /// Loads the committed node `id` of map `map` in `space`: a retained
    /// node, or one read, verified and decoded now and then retained.
    /// `None` if the store has no such object.
    pub(crate) fn load<V: TrieValue + Send + Sync + 'static>(
        &self,
        space: SpaceId,
        map: MapKind,
        id: &ObjectId,
    ) -> Result<Option<Arc<NodeBox<V>>>, LoadFailure> {
        let lifecycle = self.lifecycle()?;
        self.healthy(&lifecycle)?;
        let key = (space, map, *id);
        let shard = self.shard(id);
        let counters = &self.counters;
        let retained = self.lock(shard)?.get(&key);
        if let Some(node) = retained {
            match node.downcast::<NodeBox<V>>() {
                Ok(node) => {
                    counters.hits.fetch_add(1, Relaxed);
                    counters.hits_by_map[map.index()].fetch_add(1, Relaxed);
                    return Ok(Some(node));
                }
                // A map kind always decodes to one node type.
                Err(_) => debug_assert!(false, "cached node of another type"),
            }
        }
        counters.misses.fetch_add(1, Relaxed);
        counters.misses_by_map[map.index()].fetch_add(1, Relaxed);
        let started = Instant::now();
        let bytes = self
            .store
            .read_object_bounded(space, id, self.max_object_bytes);
        counters
            .miss_read_ns
            .fetch_add(elapsed_ns(started), Relaxed);
        // The read may itself have latched a failure.
        self.healthy(&lifecycle)?;
        let Some(bytes) = bytes? else {
            return Ok(None);
        };
        if bytes.len() > self.max_object_bytes {
            return Err(StorageError::new(
                StorageErrorKind::Corrupt,
                "object exceeds the read bound",
            )
            .into());
        }
        // Verifies the hash; corrupt bytes are never retained.
        let started = Instant::now();
        let node =
            NodeBox::<V>::decode_stored(&(*id).into(), &bytes).map_err(LoadFailure::Decode)?;
        counters.decode_ns.fetch_add(elapsed_ns(started), Relaxed);
        // The read guard is still held, so no recovery ran since the health
        // check: the node belongs to the current generation.
        let erased: Erased = node.clone();
        match self
            .lock(shard)?
            .insert(key, erased, node.retained_bytes(), counters)
        {
            Inserted::Retained => Ok(Some(node)),
            Inserted::Oversize => {
                counters.oversize_rejects.fetch_add(1, Relaxed);
                Ok(Some(node))
            }
            Inserted::Raced(existing) => {
                counters.insert_races.fetch_add(1, Relaxed);
                Ok(Some(existing.downcast().unwrap_or(node)))
            }
        }
    }

    /// Reads the bytes of object `id` through the same health gate, without
    /// the cache.
    pub(crate) fn read_object(
        &self,
        space: SpaceId,
        id: &ObjectId,
    ) -> Result<Option<Vec<u8>>, StorageError> {
        let lifecycle = self.lifecycle()?;
        self.healthy(&lifecycle)?;
        let bytes = self
            .store
            .read_object_bounded(space, id, self.max_object_bytes);
        self.healthy(&lifecycle)?;
        bytes
    }

    /// All in-flight loads finish first. Validation uses raw bounded record
    /// reads, never recursively locks this context. On *any* error it stays fenced.
    pub(crate) fn recover<T>(
        &self,
        validate: impl FnOnce() -> Result<T, DurableError>,
    ) -> Result<T, DurableError> {
        let mut lifecycle = self.lifecycle.write().map_err(|_| poisoned())?;
        lifecycle.available = false;
        for shard in &self.shards {
            shard.lock().map_err(|_| poisoned())?.clear();
        }
        self.counters.retained.store(0, Relaxed);
        self.counters.entries.store(0, Relaxed);
        lifecycle.generation = lifecycle
            .generation
            .checked_add(1)
            .ok_or(DurableError::SequenceExhausted)?;
        self.counters
            .generation
            .store(lifecycle.generation, Relaxed);
        self.store.recover()?;
        let value = validate()?;
        if let Health::Failed(e) = self.store.health() {
            return Err(e.into());
        }
        lifecycle.available = true;
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arkiv_db_memory::MemoryVersionedStore;
    use arkiv_db_storage::versioned::{CommitBatch, CommitOutcome, Head, ObjectReader};
    use arkiv_db_trie::{MemoryOnly, MerkleMap, Probe, Staged};

    const SPACE: SpaceId = SpaceId::Namespace(1);
    type Node = NodeBox<Vec<u8>>;

    /// Object `i`: the single leaf of a one-entry map, stored by hash.
    fn object(i: u8) -> (ObjectId, Vec<u8>) {
        let map: MerkleMap<[u8; 1], Vec<u8>> = MerkleMap::new()
            .insert(&[i], vec![i; 40], &MemoryOnly, &mut Probe::new())
            .unwrap()
            .0;
        let mut staged = Staged::new();
        map.stage(&mut staged);
        let mut objects = staged.into_objects();
        assert_eq!(objects.len(), 1);
        let (id, bytes) = objects.pop().unwrap();
        (id.0, bytes)
    }

    fn store_with(count: u8, extra: &[(SpaceId, ObjectId, Vec<u8>)]) -> Arc<MemoryVersionedStore> {
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
        for (space, id, bytes) in extra {
            batch.put_object(*space, *id, bytes.clone());
        }
        assert!(matches!(store.commit(&batch), CommitOutcome::Committed));
        store
    }

    fn options(cache_bytes: usize, cache_entries: usize) -> Options {
        Options {
            cache_bytes,
            cache_entries,
            ..Options::default()
        }
    }

    /// A store holding objects `0..count` and a context over it.
    fn context(count: u8, cache_bytes: usize, cache_entries: usize) -> Context {
        Context::new(
            store_with(count, &[]),
            1024,
            &options(cache_bytes, cache_entries),
        )
    }

    fn load(context: &Context, i: u8) -> Arc<Node> {
        let (id, _) = object(i);
        context
            .load::<Vec<u8>>(SPACE, MapKind::Records, &id)
            .unwrap()
            .unwrap()
    }

    /// The bytes one retained node is charged.
    fn charge() -> usize {
        let (id, bytes) = object(0);
        Node::decode_stored(&id.into(), &bytes)
            .unwrap()
            .retained_bytes()
    }

    fn present(context: &Context, i: u8) -> bool {
        let key = (SPACE, MapKind::Records, object(i).0);
        context
            .shard(&key.2)
            .lock()
            .unwrap()
            .position(&key)
            .is_some()
    }

    /// Every index slot addresses its own entry, every entry has a slot,
    /// and the charges add up in each shard and overall.
    fn assert_consistent(context: &Context) {
        let mut total = 0;
        let mut entries = 0;
        for shard in &context.shards {
            let shard = shard.lock().unwrap();
            assert_eq!(shard.index.len(), shard.entries.len());
            for (position, entry) in shard.entries.iter().enumerate() {
                assert_eq!(shard.position(&entry.key), Some(position));
            }
            let charged: usize = shard.entries.iter().map(|e| e.charge).sum();
            assert_eq!(shard.charged, charged);
            assert!(shard.charged <= shard.max_bytes);
            assert!(shard.entries.len() <= shard.max_entries);
            assert!(shard.entries.capacity() <= shard.max_entries);
            total += charged;
            entries += shard.entries.len();
        }
        let stats = context.stats();
        assert_eq!((stats.retained_bytes, stats.entries), (total, entries));
    }

    #[test]
    fn a_second_load_returns_the_same_decoded_node() {
        let context = context(3, 64 * 1024, 64);
        let first = load(&context, 0);
        load(&context, 1);
        let again = load(&context, 0);
        assert!(Arc::ptr_eq(&first, &again));
        let stats = context.stats();
        assert_eq!((stats.misses, stats.hits, stats.entries), (2, 1, 2));
        assert_eq!(stats.hits_by_map[MapKind::Records.index()], 1);
        assert_eq!(stats.misses_by_map[MapKind::Records.index()], 2);
        assert_eq!(stats.inserts, 2);
        assert!(stats.index_bytes > 0);
        // The charge is the decoded nodes' retained bytes.
        assert_eq!(stats.retained_bytes, first.retained_bytes() + charge());
        assert!(first.retained_bytes() > std::mem::size_of::<Node>() + 40);
        assert_consistent(&context);
    }

    #[test]
    fn map_kinds_and_spaces_are_separate_entries() {
        let (id, bytes) = object(9);
        let store = store_with(0, &[(SPACE, id, bytes.clone()), (SpaceId::Host, id, bytes)]);
        let context = Context::new(store, 1024, &options(64 * 1024, 64));
        let records = context
            .load::<Vec<u8>>(SPACE, MapKind::Records, &id)
            .unwrap()
            .unwrap();
        // Same bytes, other map kind: a separate entry decoded as its own type.
        let rows = context
            .load::<Vec<u8>>(SPACE, MapKind::Rows, &id)
            .unwrap()
            .unwrap();
        let host = context
            .load::<Vec<u8>>(SpaceId::Host, MapKind::Host, &id)
            .unwrap()
            .unwrap();
        assert!(!Arc::ptr_eq(&records, &rows) && !Arc::ptr_eq(&records, &host));
        let stats = context.stats();
        assert_eq!((stats.misses, stats.entries), (3, 3));
        // An object absent from one space is absent, whatever another holds.
        assert!(
            context
                .load::<Vec<u8>>(SpaceId::Namespace(2), MapKind::Records, &id)
                .unwrap()
                .is_none()
        );
        let rows_again = context
            .load::<Vec<u8>>(SPACE, MapKind::Rows, &id)
            .unwrap()
            .unwrap();
        assert!(Arc::ptr_eq(&rows, &rows_again));
        assert_consistent(&context);
    }

    #[test]
    fn a_decode_failure_is_not_retained_and_repeats() {
        // Hash-valid bytes that are no trie node.
        let bytes = vec![0x42; 40];
        let id = alloy_primitives::keccak256(&bytes).0;
        let store = store_with(0, &[(SPACE, id, bytes)]);
        let context = Context::new(store, 1024, &options(64 * 1024, 64));
        for _ in 0..2 {
            assert!(matches!(
                context.load::<Vec<u8>>(SPACE, MapKind::Records, &id),
                Err(LoadFailure::Decode(TrieError::Malformed { .. }))
            ));
        }
        let stats = context.stats();
        assert_eq!((stats.misses, stats.hits, stats.entries), (2, 0, 0));
        assert_eq!(stats.inserts, 0);
    }

    #[test]
    fn eviction_by_entries_and_bytes_keeps_the_index_consistent() {
        // Two entries by count.
        let by_count = context(4, 64 * 1024, 2);
        for i in 0..3 {
            load(&by_count, i);
        }
        assert!(!present(&by_count, 0) && present(&by_count, 1) && present(&by_count, 2));
        assert_eq!(by_count.stats().evictions, 1);
        assert_consistent(&by_count);
        // Room for two nodes by bytes.
        let by_bytes = context(8, 2 * charge() + 1, 100);
        for i in (0..8).chain(0..8) {
            load(&by_bytes, i);
            assert!(by_bytes.stats().retained_bytes <= 2 * charge() + 1);
            assert_consistent(&by_bytes);
        }
        assert_eq!(by_bytes.stats().entries, 2);
    }

    #[test]
    fn a_node_larger_than_the_budget_is_never_retained() {
        let tight = context(2, charge() - 1, 8);
        load(&tight, 0);
        load(&tight, 0);
        let stats = tight.stats();
        assert_eq!((stats.entries, stats.retained_bytes, stats.hits), (0, 0, 0));
        assert_eq!(stats.oversize_rejects, 2);
        let disabled = context(2, 0, 0);
        load(&disabled, 0);
        let stats = disabled.stats();
        assert_eq!(
            (stats.entries, stats.container_bytes, stats.shards),
            (0, 0, 1)
        );
    }

    #[test]
    fn clock_keeps_a_hot_key_resident_under_scan_churn() {
        // One shard of four entries.
        let context = context(40, 60 * 1024, 4);
        assert_eq!(context.shards.len(), 1);
        let hot = load(&context, 0);
        for round in 1..400_u32 {
            // The hot key is touched between every two scan loads.
            let i = (1 + round % 39) as u8;
            load(&context, i);
            assert!(Arc::ptr_eq(&hot, &load(&context, 0)));
        }
        let stats = context.stats();
        assert_eq!(stats.misses_by_map[MapKind::Records.index()], 400);
        assert!(stats.second_chances > 0);
        assert_consistent(&context);
    }

    #[test]
    fn index_matches_a_linear_scan_through_long_churn() {
        for (bytes, entries) in [(1024 * 1024, 7), (5 * charge(), 1000), (1 << 20, 64)] {
            let context = context(40, bytes, entries);
            let mut state = 7_u32;
            for _ in 0..800 {
                state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                // Skewed: low ids are hot.
                let i = ((state >> 16) % 40).min((state >> 8) % 40) as u8;
                let before = present(&context, i);
                let hits = context.stats().hits;
                load(&context, i);
                assert_eq!(context.stats().hits - hits, u64::from(before));
                let stats = context.stats();
                assert!(stats.entries <= entries && stats.peak_entries <= entries);
                assert!(stats.retained_bytes <= bytes && stats.peak_retained_bytes <= bytes);
                assert!(
                    stats.index_bytes <= 4 * entries * std::mem::size_of::<IndexEntry>() + 1024
                );
                assert_consistent(&context);
            }
        }
    }

    #[test]
    fn shard_limits_add_up_to_the_configured_bounds() {
        for (bytes, entries, shards) in [
            (0, 0, 1),
            (512, 1, 1),
            (512, 3, 1),
            (2048, 17, 1),
            (64 * 1024, 3, 1),
            (1 << 20, 100, 2),
            (1 << 20, 1000, 16),
            (8 << 20, 16_384, 16),
            (100_000, 1000, 1),
        ] {
            let context = context(0, bytes, entries);
            assert_eq!(context.shards.len(), shards);
            let (sum_bytes, sum_entries) = context.shards.iter().fold((0, 0), |sum, shard| {
                let shard = shard.lock().unwrap();
                (sum.0 + shard.max_bytes, sum.1 + shard.max_entries)
            });
            assert_eq!((sum_bytes, sum_entries), (bytes, entries));
        }
    }

    #[test]
    fn recovery_clears_every_shard() {
        let context = context(40, 1024 * 1024, 1024);
        let held = load(&context, 0);
        for i in 0..40 {
            load(&context, i);
        }
        assert_eq!(context.stats().shards, 16);
        assert_eq!(context.stats().entries, 40);
        context.recover(|| Ok(())).unwrap();
        for shard in &context.shards {
            let shard = shard.lock().unwrap();
            assert!(shard.entries.is_empty() && shard.index.is_empty());
            assert_eq!((shard.popped, shard.charged), (0, 0));
        }
        let stats = context.stats();
        assert_eq!(
            (stats.generation, stats.entries, stats.retained_bytes),
            (1, 0, 0)
        );
        assert_eq!((stats.index_bytes, stats.container_bytes), (0, 0));
        // The caller's node survives the clear; the cache decodes anew.
        let fresh = load(&context, 0);
        assert!(!Arc::ptr_eq(&held, &fresh));
        assert!(Arc::ptr_eq(&fresh, &load(&context, 0)));
        assert_consistent(&context);
    }

    #[test]
    fn released_nodes_are_freed_once_evicted_or_cleared() {
        let context = context(4, 1024 * 1024, 2);
        let weak = Arc::downgrade(&load(&context, 0));
        assert!(weak.upgrade().is_some(), "retained by the cache");
        load(&context, 1);
        load(&context, 2);
        assert!(weak.upgrade().is_none(), "evicted and released");
        let weak = Arc::downgrade(&load(&context, 3));
        context.recover(|| Ok(())).unwrap();
        assert!(weak.upgrade().is_none(), "cleared and released");
    }

    #[test]
    fn raw_reads_pass_the_health_gate_without_the_cache() {
        let context = context(1, 1024 * 1024, 8);
        let (id, bytes) = object(0);
        assert_eq!(context.read_object(SPACE, &id).unwrap(), Some(bytes));
        assert_eq!(context.stats().entries, 0);
        assert_eq!(
            context.store.read_object(SPACE, &id).unwrap(),
            context.read_object(SPACE, &id).unwrap()
        );
    }
}
