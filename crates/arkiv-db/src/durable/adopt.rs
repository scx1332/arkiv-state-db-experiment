//! Verified adoption of a private staged store as a simulator database.
//!
//! [`DurableEngine::initialize_simulator_from_staged`] copies every object
//! reachable from a staged state into a new store and checks each one on the
//! way. Adoption keeps the staged store instead: a verify-only walk applies
//! the same checks to the same objects, without writing them, and one
//! [`Relabel`] then replaces the store's engine-only records. The walk is
//! bound by random object reads, so it runs in parallel over disjoint parts:
//! whole maps, and key ranges of large maps cut at their first two trie
//! branch levels. Every stored node lies on the path to some leaf, and each
//! leaf belongs to exactly one part, so every reachable object is still read
//! and decoded at least once and every entry is counted exactly once.
use super::{
    streaming::{
        POSTING_CACHE_SLOTS, PostingCache, check_host, check_namespace, check_object_size,
        check_row,
    },
    *,
};
use crate::{
    StateHeader,
    layout::{Catalog, HostMap, Keys, PostingMap, Records, Rows, Terms},
};
use arkiv_db_storage::versioned::{Relabel, SpaceId};
use arkiv_db_trie::{ExploreNode, MerkleMap, NodeKind, NodeLoader, TrieError, TrieKey, TrieValue};
use std::{
    collections::{HashMap, VecDeque, hash_map::Entry},
    sync::{
        Condvar, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, AtomicU64, AtomicUsize},
    },
};

/// Maps with more entries are verified as up to 256 key ranges.
const SPLIT_ENTRIES: u64 = 1 << 16;
/// Pending parts at most; a part found while the queue is full runs inline.
const QUEUE_PARTS: usize = 4096;

struct Stop(DurableError);
impl From<TrieError> for Stop {
    fn from(e: TrieError) -> Self {
        Self(corrupt(format!("import traversal: {e}")))
    }
}

#[derive(Clone)]
enum Map {
    Catalog(Catalog),
    Records(Records),
    Keys(Keys),
    Rows(Rows),
    Terms(Terms),
    Posting(PostingMap),
    Host(HostMap),
}
impl Map {
    fn len(&self) -> u64 {
        match self {
            Self::Catalog(m) => m.len(),
            Self::Records(m) => m.len(),
            Self::Keys(m) => m.len(),
            Self::Rows(m) => m.len(),
            Self::Terms(m) => m.len(),
            Self::Posting(m) => m.len(),
            Self::Host(m) => m.len(),
        }
    }
}

/// The entries the parts of one map must count together.
struct Total {
    expected: u64,
    seen: AtomicU64,
    parts: AtomicUsize,
}

/// A whole map (`total` unset: not yet split) or one key range of it.
struct Part {
    map: Map,
    space: SpaceId,
    /// Encoded-key bounds: inclusive lower, exclusive upper; `None` is open.
    lower: Option<Vec<u8>>,
    upper: Option<Vec<u8>>,
    total: Option<Arc<Total>>,
}

#[derive(Default)]
struct Queue {
    parts: VecDeque<Part>,
    active: usize,
}

struct Verifier<'a> {
    source: &'a dyn VersionedStore,
    state: &'a StateHeader,
    genesis: &'a EngineGenesis,
    options: &'a Options,
    object_cap: usize,
    /// Maps with more entries than this are cut into key ranges.
    split: u64,
    queue: Mutex<Queue>,
    wake: Condvar,
    failed: AtomicBool,
    error: Mutex<Option<DurableError>>,
    /// Large postings are scheduled once per (root, namespace), like the
    /// copier's posting cache does for every posting.
    split_postings: Mutex<HashMap<([u8; 32], u64), u64>>,
}

struct Loader<'a, 'b> {
    verifier: &'a Verifier<'b>,
    space: SpaceId,
}
impl<V: TrieValue> NodeLoader<V> for Loader<'_, '_> {
    type Error = Stop;
    fn read(&self, id: &crate::B256) -> Result<Option<Vec<u8>>, Stop> {
        let verifier = self.verifier;
        if verifier.failed.load(Ordering::Acquire) {
            // Another part already failed; its error is the one reported.
            return Err(Stop(DurableError::WriterUnavailable));
        }
        let bytes = verifier
            .source
            .read_object_bounded(self.space, &id.0, verifier.object_cap)
            .map_err(|e| Stop(e.into()))?;
        if let Some(bytes) = &bytes {
            check_object_size(bytes.len(), verifier.options).map_err(Stop)?;
        }
        Ok(bytes)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The smallest encoded key below a nibble prefix.
fn lower_bound(mut nibbles: Vec<u8>) -> Vec<u8> {
    if !nibbles.len().is_multiple_of(2) {
        nibbles.push(0);
    }
    nibbles.chunks_exact(2).map(|c| c[0] << 4 | c[1]).collect()
}

impl Verifier<'_> {
    fn run(&self, threads: usize) -> Result<(), DurableError> {
        let catalog = Catalog::from_root(self.state.catalog_root, self.state.catalog_len)
            .map_err(|e| Stop::from(e).0)?;
        let host = HostMap::from_root(self.state.host_root, self.state.host_len)
            .map_err(|e| Stop::from(e).0)?;
        for (map, space) in [
            (Map::Catalog(catalog), SpaceId::Catalog),
            (Map::Host(host), SpaceId::Host),
        ] {
            lock(&self.queue).parts.push_back(Part {
                map,
                space,
                lower: None,
                upper: None,
                total: None,
            });
        }
        std::thread::scope(|scope| {
            for _ in 0..threads.max(1) {
                scope.spawn(|| self.work());
            }
        });
        match lock(&self.error).take() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn fail(&self, error: DurableError) {
        lock(&self.error).get_or_insert(error);
        self.failed.store(true, Ordering::Release);
        self.wake.notify_all();
    }

    fn work(&self) {
        let mut postings = PostingCache::new(POSTING_CACHE_SLOTS);
        loop {
            let part = {
                let mut queue = lock(&self.queue);
                loop {
                    if self.failed.load(Ordering::Acquire) {
                        return;
                    }
                    if let Some(part) = queue.parts.pop_front() {
                        queue.active += 1;
                        break part;
                    }
                    if queue.active == 0 {
                        return;
                    }
                    queue = self
                        .wake
                        .wait(queue)
                        .unwrap_or_else(PoisonError::into_inner);
                }
            };
            if let Err(error) = self.part(part, &mut postings) {
                self.fail(error);
            }
            let mut queue = lock(&self.queue);
            queue.active -= 1;
            if queue.active == 0 && queue.parts.is_empty() {
                self.wake.notify_all();
            }
        }
    }

    /// Queue a part for any worker, or run it here if the queue is full.
    fn push(&self, part: Part, postings: &mut PostingCache) -> Result<(), DurableError> {
        {
            let mut queue = lock(&self.queue);
            if queue.parts.len() < QUEUE_PARTS {
                queue.parts.push_back(part);
                self.wake.notify_one();
                return Ok(());
            }
        }
        self.part(part, postings)
    }

    fn whole(
        &self,
        map: Map,
        space: SpaceId,
        postings: &mut PostingCache,
    ) -> Result<(), DurableError> {
        self.push(
            Part {
                map,
                space,
                lower: None,
                upper: None,
                total: None,
            },
            postings,
        )
    }

    fn part(&self, part: Part, postings: &mut PostingCache) -> Result<(), DurableError> {
        let Some(total) = part.total.clone() else {
            return self.split(part, postings);
        };
        let namespace = match part.space {
            SpaceId::Namespace(n) => n,
            _ => 0,
        };
        let counted = match &part.map {
            Map::Catalog(m) => self.range(m, &part, |id, ns| {
                check_namespace(self.state, id, &ns)?;
                let space = SpaceId::Namespace(id.ordinal());
                self.whole(Map::Records(ns.records), space, postings)?;
                self.whole(Map::Keys(ns.typed.keys), space, postings)?;
                self.whole(Map::Rows(ns.typed.rows), space, postings)?;
                self.whole(Map::Terms(ns.typed.terms), space, postings)
            })?,
            Map::Records(m) => self.range(m, &part, |_, _| Ok(()))?,
            Map::Keys(m) => self.range(m, &part, |_, _| Ok(()))?,
            Map::Rows(m) => self.range(m, &part, |_, row| check_row(self.genesis, &row))?,
            Map::Terms(m) => self.range(m, &part, |_, posting| {
                self.posting(posting.0, part.space, namespace, postings)
            })?,
            Map::Posting(m) => self.range(m, &part, |_, _| Ok(()))?,
            Map::Host(m) => self.range(m, &part, |_, blob| check_host(&blob))?,
        };
        total.seen.fetch_add(counted, Ordering::AcqRel);
        if total.parts.fetch_sub(1, Ordering::AcqRel) == 1
            && total.seen.load(Ordering::Acquire) != total.expected
        {
            return Err(corrupt("import map entry count mismatch"));
        }
        Ok(())
    }

    fn posting(
        &self,
        map: PostingMap,
        space: SpaceId,
        namespace: u64,
        postings: &mut PostingCache,
    ) -> Result<(), DurableError> {
        if map.len() > self.split {
            match lock(&self.split_postings).entry((map.root_hash().0, namespace)) {
                Entry::Occupied(len) if *len.get() == map.len() => return Ok(()),
                Entry::Occupied(_) => return Err(corrupt("import map entry count mismatch")),
                Entry::Vacant(slot) => {
                    slot.insert(map.len());
                }
            }
        }
        self.whole(Map::Posting(map), space, postings)
    }

    /// Cut a whole map into its parts, run the first and queue the rest.
    fn split(&self, part: Part, postings: &mut PostingCache) -> Result<(), DurableError> {
        let len = part.map.len();
        let namespace = match part.space {
            SpaceId::Namespace(n) => n,
            _ => 0,
        };
        if let Map::Posting(map) = &part.map
            && len <= self.split
        {
            let root = map.root_hash().0;
            if postings.contains(&root, namespace, len)? {
                return Ok(());
            }
            self.part(
                Part {
                    total: Some(Arc::new(Total {
                        expected: len,
                        seen: AtomicU64::new(0),
                        parts: AtomicUsize::new(1),
                    })),
                    ..part
                },
                postings,
            )?;
            // Only a complete traversal proves the count and every object.
            postings.insert(root, namespace, len);
            return Ok(());
        }
        let bounds = if len > self.split {
            match &part.map {
                Map::Catalog(m) => self.bounds(m, part.space)?,
                Map::Records(m) => self.bounds(m, part.space)?,
                Map::Keys(m) => self.bounds(m, part.space)?,
                Map::Rows(m) => self.bounds(m, part.space)?,
                Map::Terms(m) => self.bounds(m, part.space)?,
                Map::Posting(m) => self.bounds(m, part.space)?,
                Map::Host(m) => self.bounds(m, part.space)?,
            }
        } else {
            Vec::new()
        };
        let total = Arc::new(Total {
            expected: len,
            seen: AtomicU64::new(0),
            parts: AtomicUsize::new(bounds.len() + 1),
        });
        let lowers = std::iter::once(None).chain(bounds.iter().cloned().map(Some));
        let uppers = bounds
            .iter()
            .cloned()
            .map(Some)
            .chain(std::iter::once(None));
        let mut parts = lowers.zip(uppers).map(|(lower, upper)| Part {
            map: part.map.clone(),
            space: part.space,
            lower,
            upper,
            total: Some(total.clone()),
        });
        let first = parts.next().expect("one part");
        for rest in parts {
            self.push(rest, postings)?;
        }
        self.part(first, postings)
    }

    /// Lower bounds of the key ranges below the first two branch levels.
    fn bounds<K: TrieKey, V: TrieValue>(
        &self,
        map: &MerkleMap<K, V>,
        space: SpaceId,
    ) -> Result<Vec<Vec<u8>>, DurableError> {
        let loader = Loader {
            verifier: self,
            space,
        };
        let Some((route, top)) = branch(map, Vec::new(), &loader)? else {
            return Ok(Vec::new());
        };
        let mut prefixes = Vec::new();
        for child in top.children.iter().filter(|c| c.hash.is_some()) {
            let mut below = route.clone();
            below.push(child.slot);
            match branch(map, below, &loader)? {
                Some((_, node)) => {
                    for grandchild in node.children.iter().filter(|c| c.hash.is_some()) {
                        let mut prefix = node.prefix.clone();
                        prefix.push(grandchild.slot);
                        prefixes.push(prefix);
                    }
                }
                None => {
                    let mut prefix = top.prefix.clone();
                    prefix.push(child.slot);
                    prefixes.push(prefix);
                }
            }
        }
        // The first range starts at the beginning of the map.
        Ok(prefixes.into_iter().skip(1).map(lower_bound).collect())
    }

    /// Iterate one part in key order, checking and counting its entries.
    fn range<K: TrieKey, V: TrieValue>(
        &self,
        map: &MerkleMap<K, V>,
        part: &Part,
        mut leaf: impl FnMut(K, V) -> Result<(), DurableError>,
    ) -> Result<u64, DurableError> {
        let loader = Loader {
            verifier: self,
            space: part.space,
        };
        let entries = match &part.lower {
            Some(lower) => map.range_from_encoded(lower, &loader),
            None => map.iter(&loader),
        };
        let mut count = 0u64;
        for entry in entries {
            let (key, value) = entry.map_err(|e| e.0)?;
            if part
                .upper
                .as_ref()
                .is_some_and(|upper| key.encode() >= *upper)
            {
                break;
            }
            leaf(key, value)?;
            count = count
                .checked_add(1)
                .ok_or_else(|| corrupt("map count overflow"))?;
        }
        Ok(count)
    }
}

/// Follow extensions from `route` to the next branch; `None` at a leaf.
fn branch<K: TrieKey, V: TrieValue>(
    map: &MerkleMap<K, V>,
    mut route: Vec<u8>,
    loader: &Loader<'_, '_>,
) -> Result<Option<(Vec<u8>, ExploreNode)>, DurableError> {
    loop {
        let node = map
            .explore(&route, loader)
            .map_err(|e| e.0)?
            .ok_or_else(|| corrupt("import traversal: explored route vanished"))?;
        match node.kind {
            NodeKind::Branch => return Ok(Some((route, node))),
            NodeKind::Extension => route.push(16),
            NodeKind::Leaf => return Ok(None),
        }
    }
}

/// Read and check every object reachable from `state` exactly as the staged
/// copy does, in `threads` workers, without writing anything.
pub(super) fn verify_staged(
    source: &dyn VersionedStore,
    state: &StateHeader,
    genesis: &EngineGenesis,
    options: &Options,
    threads: usize,
) -> Result<(), DurableError> {
    verify_split(source, state, genesis, options, threads, SPLIT_ENTRIES)
}

fn verify_split(
    source: &dyn VersionedStore,
    state: &StateHeader,
    genesis: &EngineGenesis,
    options: &Options,
    threads: usize,
    split: u64,
) -> Result<(), DurableError> {
    Verifier {
        source,
        state,
        genesis,
        options,
        object_cap: genesis.max_object_bytes as usize,
        split,
        queue: Mutex::new(Queue::default()),
        wake: Condvar::new(),
        failed: AtomicBool::new(false),
        error: Mutex::new(None),
        split_postings: Mutex::new(HashMap::new()),
    }
    .run(threads)
}

impl DurableEngine {
    /// Adopt this engine-only staged store in place as a simulator database
    /// at height zero, instead of copying it into a new store.
    ///
    /// The store must hold exactly one height-zero publication, as
    /// [`initialize_bulk`](Self::initialize_bulk) writes it, and nothing else:
    /// no later snapshots, checkpoints or floors, hence no objects of other
    /// states. Every object reachable from the staged state is first read and
    /// checked as [`initialize_simulator_from_staged`]
    /// (Self::initialize_simulator_from_staged) checks it while copying:
    /// content hash, object and batch caps, trie structure, entry counts,
    /// namespace association, row admission and host value bounds. Only the
    /// writes are skipped, and `threads` workers share the reads. Then one
    /// durable [`Relabel`] replaces the staged profile, genesis and manifest
    /// with the simulator profile, `genesis`, `identity` and the height-zero
    /// `packet`, and keeps every object. A refusal or failure before that
    /// commit leaves the staged store unchanged.
    ///
    /// The engine is consumed, so this process keeps no handle that still
    /// interprets the store as staged. After `Committed` the store is the
    /// database [`initialize_simulator_from_staged`]
    /// (Self::initialize_simulator_from_staged) would have created, apart from
    /// its database id, and [`open_simulator`](Self::open_simulator) accepts
    /// it. After `Indeterminate`, reopen the store and read its
    /// [`storage_profile`](Self::storage_profile): the relabelling is atomic.
    pub fn adopt_staged_simulator(
        self,
        genesis: EngineGenesis,
        options: Options,
        identity: simulator::SimulatorIdentity,
        packet: simulator::SimulatorPacket,
        threads: usize,
    ) -> Result<Publication, DurableError> {
        genesis.validate()?;
        options.validate()?;
        self.context.generation()?;
        if self.state != WriterState::Ready || self.simulator.is_some() {
            return Err(incompatible(
                "adoption requires a ready engine-only staging source",
            ));
        }
        if genesis.initial_state.as_ref() != Some(&self.manifest.state)
            || genesis.limits != self.genesis.limits
            || genesis.pricing != self.genesis.pricing
        {
            return Err(incompatible("staged source/genesis association"));
        }
        if self.manifest.sequence != 0 || self.manifest.parent.is_some() {
            return Err(incompatible(
                "adoption requires a staged store with a single height-zero publication",
            ));
        }
        let store = self.context.store.clone();
        let staged_head = head_of(&self.manifest);
        if store.head()? != Some(staged_head) {
            return Err(corrupt("staged head/manifest association"));
        }
        identity.validate(&genesis)?;
        if packet.reserved_bytes()? > options.prepare_bytes {
            return Err(DurableError::ResourceLimit(
                "genesis packet exceeds batch cap".into(),
            ));
        }
        let manifest = EngineManifest {
            genesis: genesis.id(),
            sequence: 0,
            parent: None,
            state_root: self.manifest.state_root,
            state: self.manifest.state.clone(),
        };
        // The records `initialize_simulator_from_staged` commits.
        let mut batch = manifest_batch(None, &manifest);
        batch
            .put_record(Table::Meta, PROFILE_KEY, simulator::profile_bytes())
            .put_record(Table::Meta, GENESIS_KEY, genesis.envelope())
            .put_record(Table::Meta, simulator::IDENTITY_KEY, identity.envelope()?);
        packet.append(&mut batch, &manifest)?;
        if batch.logical_bytes() > options.prepare_bytes as u64 {
            return Err(DurableError::ResourceLimit(
                "genesis metadata exceeds batch cap".into(),
            ));
        }
        // Exactly the records `initialize_bulk` publishes; the store refuses
        // the relabelling if it holds any other.
        let retired = vec![
            (Table::Meta, PROFILE_KEY.to_vec(), profile_bytes()),
            (Table::Meta, GENESIS_KEY.to_vec(), self.genesis.envelope()),
            (
                Table::Manifests,
                0u64.to_be_bytes().to_vec(),
                self.manifest.envelope(),
            ),
            (
                Table::BlockIndex,
                self.manifest.id().to_vec(),
                0u64.to_be_bytes().to_vec(),
            ),
        ];
        verify_staged(store.as_ref(), &manifest.state, &genesis, &options, threads)?;
        let info = CommitInfo {
            snapshot: SnapshotId::from_sequence(0),
            commit_id: manifest.id(),
            state_root: manifest.state_root,
        };
        // Release the staged interpretation and its caches first; `store` is
        // the only handle left.
        drop(self);
        Ok(
            match store.relabel(&Relabel {
                expected_head: staged_head,
                retired,
                batch,
            }) {
                CommitOutcome::Committed => Publication::Committed(info),
                CommitOutcome::NotCommitted(error) => Publication::NotCommitted(error),
                CommitOutcome::Indeterminate(error) => Publication::Indeterminate(error),
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Cell, CellName, Limits, NamespaceDescriptor, NamespaceId, NamespaceName, OwnerId,
        PriceSchedule, RecordKey, Revision, Value, durable::BulkGenesisBuilder,
    };
    use arkiv_db_memory::MemoryVersionedStore;
    use arkiv_db_storage::versioned::{
        CommitBatch, DatabaseId, Head, Health, ObjectId, ObjectReader, StorageError, StoreStats,
    };
    use std::collections::{BTreeMap, HashSet};

    /// Records every object read; can hide or flip one object.
    struct Probe {
        inner: Arc<MemoryVersionedStore>,
        read: Mutex<HashSet<(SpaceId, ObjectId)>>,
        target: Mutex<Option<(ObjectId, bool)>>,
    }
    impl ObjectReader for Probe {
        fn read_object(&self, s: SpaceId, id: &ObjectId) -> Result<Option<Vec<u8>>, StorageError> {
            self.read_object_bounded(s, id, usize::MAX)
        }
        fn read_object_bounded(
            &self,
            space: SpaceId,
            id: &ObjectId,
            cap: usize,
        ) -> Result<Option<Vec<u8>>, StorageError> {
            let mut bytes = self.inner.read_object_bounded(space, id, cap)?;
            match *lock(&self.target) {
                Some((target, false)) if target == *id => bytes = None,
                Some((target, true)) if target == *id => {
                    if let Some(b) = bytes.as_mut() {
                        *b.last_mut().unwrap() ^= 1;
                    }
                }
                _ => {}
            }
            if bytes.is_some() {
                lock(&self.read).insert((space, *id));
            }
            Ok(bytes)
        }
    }
    impl VersionedStore for Probe {
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
            n: usize,
        ) -> Result<Option<Vec<u8>>, StorageError> {
            self.inner.read_record_bounded(t, k, n)
        }
        fn commit(&self, b: &CommitBatch) -> CommitOutcome {
            self.inner.commit(b)
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

    struct Temp(std::path::PathBuf);
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A bulk-built staged state with large shared postings, many single
    /// postings and a host map, as the streaming prefill builds them.
    fn staged(records: u64) -> (Arc<MemoryVersionedStore>, DurableEngine, EngineGenesis) {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let temp = Temp(std::env::temp_dir().join(format!(
            "arkiv-adopt-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )));
        let descriptor = NamespaceDescriptor {
            id: NamespaceId::from_ordinal(1).unwrap(),
            name: NamespaceName::new("entities").unwrap(),
            owner: OwnerId::new([1; 20]),
            revision: Revision::INITIAL,
        };
        let mut builder =
            BulkGenesisBuilder::new(&temp.0, descriptor, Limits::DEFAULT, 4096).unwrap();
        let name = |n: &str| CellName::new(n).unwrap();
        for n in 0..records {
            builder
                .add_record(
                    RecordKey::new(&n.to_be_bytes()).unwrap(),
                    vec![
                        (name("shared"), Cell::attribute(Value::str("same"))),
                        (name("mod"), Cell::attribute(Value::U64(n % 7))),
                        (name("unique"), Cell::attribute(Value::U64(n))),
                        (name("body"), Cell::field(Value::bytes(&[n as u8; 48]))),
                    ],
                )
                .unwrap();
        }
        for n in 0..40u8 {
            builder.add_host([n; 32], vec![n; 40]).unwrap();
        }
        let store = Arc::new(MemoryVersionedStore::new());
        let build = builder
            .finish(store.clone(), PriceSchedule::FIXTURE, Options::default())
            .unwrap();
        let mut genesis = EngineGenesis::new([3; 32], PriceSchedule::FIXTURE);
        genesis.initial_state = Some(build.state_header().clone());
        let Initialization::Committed(engine) =
            DurableEngine::initialize_bulk(build, genesis.clone(), Options::default()).unwrap()
        else {
            panic!("memory initialization commits")
        };
        (store, engine, genesis)
    }

    fn probe(store: &Arc<MemoryVersionedStore>) -> Probe {
        Probe {
            inner: store.clone(),
            read: Mutex::new(HashSet::new()),
            target: Mutex::new(None),
        }
    }

    const SETTINGS: [(u64, usize); 4] = [(u64::MAX, 1), (4, 1), (4, 4), (16, 3)];

    #[test]
    fn partitions_read_every_reachable_object_and_accept_the_state() {
        let (store, engine, genesis) = staged(300);
        let state = engine.head_snapshot().state_header();
        let reach = engine.reachability(&[1]).unwrap().levels[0]
            .per_space
            .clone();
        assert!(reach.values().map(|r| r.objects).sum::<u64>() > 500);
        for (split, threads) in SETTINGS {
            let probe = probe(&store);
            verify_split(
                &probe,
                &state,
                &genesis,
                &Options::default(),
                threads,
                split,
            )
            .unwrap();
            let mut read: BTreeMap<SpaceId, u64> = BTreeMap::new();
            for (space, _) in lock(&probe.read).iter() {
                *read.entry(*space).or_default() += 1;
            }
            // Reads are reachable by construction; equal counts mean every
            // reachable object was read.
            for (space, r) in &reach {
                assert_eq!(
                    read.get(space).copied().unwrap_or(0),
                    r.objects,
                    "{space:?}"
                );
            }
        }
    }

    #[test]
    fn every_missing_or_changed_object_fails_every_partitioning() {
        let (store, engine, genesis) = staged(120);
        let state = engine.head_snapshot().state_header();
        let probe = probe(&store);
        verify_split(&probe, &state, &genesis, &Options::default(), 1, u64::MAX).unwrap();
        let mut objects: Vec<_> = lock(&probe.read).iter().copied().collect();
        objects.sort();
        let step = (objects.len() / 24).max(1);
        let sample: Vec<_> = objects
            .iter()
            .step_by(step)
            .chain(objects.last())
            .copied()
            .collect();
        assert!(sample.iter().any(|(s, _)| *s == SpaceId::Catalog));
        assert!(sample.iter().any(|(s, _)| *s == SpaceId::Host));
        for (space, id) in sample {
            for flip in [false, true] {
                *lock(&probe.target) = Some((id, flip));
                for (split, threads) in SETTINGS {
                    assert!(
                        verify_split(
                            &probe,
                            &state,
                            &genesis,
                            &Options::default(),
                            threads,
                            split
                        )
                        .is_err(),
                        "{space:?} flip={flip} split={split} threads={threads}"
                    );
                }
            }
        }
        *lock(&probe.target) = None;
        verify_split(&probe, &state, &genesis, &Options::default(), 4, 4).unwrap();
    }

    #[test]
    fn split_maps_still_check_their_entry_counts_and_caps() {
        let (store, engine, genesis) = staged(60);
        let state = engine.head_snapshot().state_header();
        for (split, threads) in SETTINGS {
            for (host, catalog) in [(1, 0), (0, 1)] {
                let mut wrong = state.clone();
                wrong.host_len += host;
                wrong.catalog_len += catalog;
                let error = verify_split(
                    &*store,
                    &wrong,
                    &genesis,
                    &Options::default(),
                    threads,
                    split,
                )
                .unwrap_err();
                assert!(
                    error.to_string().contains("entry count mismatch"),
                    "{error}"
                );
            }
            let tight = Options {
                prepare_bytes: 64,
                ..Options::default()
            };
            assert!(matches!(
                verify_split(&*store, &state, &genesis, &tight, threads, split),
                Err(DurableError::ResourceLimit(_))
            ));
            let mut small = genesis.clone();
            small.max_object_bytes = 40;
            assert!(
                verify_split(&*store, &state, &small, &Options::default(), threads, split).is_err()
            );
        }
    }

    #[test]
    fn range_bounds_pad_odd_nibble_prefixes() {
        assert_eq!(lower_bound(vec![1, 2, 3]), vec![0x12, 0x30]);
        assert_eq!(lower_bound(vec![0xa, 0xb]), vec![0xab]);
        assert_eq!(lower_bound(Vec::new()), Vec::<u8>::new());
    }
}
