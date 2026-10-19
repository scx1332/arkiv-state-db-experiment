//! Bounded, restartable copying of a private import's current state.
use super::*;
use crate::{
    StateHeader,
    layout::{Catalog, HostMap, PostingMap},
};
use arkiv_db_storage::versioned::{ObjectId, SpaceId};
use arkiv_db_trie::{MerkleMap, NodeLoader, TrieError, TrieKey, TrieValue};
use std::cell::{Cell, RefCell};

pub(super) fn checkpoint_key(sequence: u64) -> String {
    format!("import/checkpoint/{sequence:016x}")
}

struct CopyFailure(DurableError);
impl From<TrieError> for CopyFailure {
    fn from(e: TrieError) -> Self {
        Self(corrupt(format!("import traversal: {e}")))
    }
}

/// A direct-mapped cache bounds duplicate posting work independently of the
/// number of terms. A collision only repeats a traversal. The nonzero length
/// marks an occupied slot: decoded term leaves cannot contain empty postings.
/// At 48 bytes per slot the production cache retains exactly 768 KiB.
const POSTING_CACHE_SLOTS: usize = 16_384;

#[derive(Clone, Copy, Default)]
struct CopiedPosting {
    root: ObjectId,
    namespace: u64,
    len: u64,
}

struct PostingCache {
    slots: Box<[CopiedPosting]>,
}
impl PostingCache {
    fn new(slots: usize) -> Self {
        assert!(slots.is_power_of_two());
        Self {
            slots: vec![CopiedPosting::default(); slots].into_boxed_slice(),
        }
    }

    fn slot(&self, root: &ObjectId, namespace: u64) -> usize {
        let prefix = u64::from_le_bytes(root[..8].try_into().expect("hash prefix"));
        (prefix ^ namespace) as usize & (self.slots.len() - 1)
    }

    fn contains(&self, root: &ObjectId, namespace: u64, len: u64) -> Result<bool, DurableError> {
        let cached = &self.slots[self.slot(root, namespace)];
        if cached.len == 0 || cached.root != *root || cached.namespace != namespace {
            return Ok(false);
        }
        if cached.len != len {
            return Err(corrupt("import map entry count mismatch"));
        }
        Ok(true)
    }

    fn insert(&mut self, root: ObjectId, namespace: u64, len: u64) {
        let slot = self.slot(&root, namespace);
        self.slots[slot] = CopiedPosting {
            root,
            namespace,
            len,
        };
    }
}

struct Copier<'a> {
    source: &'a dyn VersionedStore,
    target: &'a dyn VersionedStore,
    options: &'a Options,
    object_cap: usize,
    pending_bytes: Cell<usize>,
    pending: RefCell<Vec<(SpaceId, ObjectId, Vec<u8>)>>,
    postings: RefCell<PostingCache>,
}
impl Copier<'_> {
    fn flush(&self) -> Result<(), DurableError> {
        let mut pending = self.pending.borrow_mut();
        if pending.is_empty() {
            return Ok(());
        }
        match self.target.stage_objects(&pending) {
            CommitOutcome::Committed => {
                pending.clear();
                self.pending_bytes.set(0);
                Ok(())
            }
            CommitOutcome::NotCommitted(e) | CommitOutcome::Indeterminate(e) => Err(e.into()),
        }
    }
    fn push(&self, space: SpaceId, id: ObjectId, bytes: &[u8]) -> Result<(), DurableError> {
        let size = bytes
            .len()
            .checked_add(32)
            .ok_or_else(|| incompatible("object size overflow"))?;
        if size > self.options.prepare_bytes || self.options.prepare_objects == 0 {
            return Err(DurableError::ResourceLimit(
                "copy object exceeds batch cap".into(),
            ));
        }
        let flush = {
            let pending = self.pending.borrow();
            pending.len() >= self.options.prepare_objects
                || self.pending_bytes.get() > self.options.prepare_bytes - size
        };
        if flush {
            self.flush()?;
        }
        self.pending.borrow_mut().push((space, id, bytes.to_vec()));
        self.pending_bytes.set(self.pending_bytes.get() + size);
        Ok(())
    }
    fn map<K: TrieKey, V: TrieValue>(
        &self,
        map: &MerkleMap<K, V>,
        space: SpaceId,
        mut leaf: impl FnMut(K, V) -> Result<(), DurableError>,
    ) -> Result<(), DurableError> {
        let loader = CopyLoader {
            copier: self,
            space,
        };
        let mut count = 0u64;
        for entry in map.iter(&loader) {
            let (key, value) = entry.map_err(|e| e.0)?;
            leaf(key, value)?;
            count = count
                .checked_add(1)
                .ok_or_else(|| corrupt("map count overflow"))?;
        }
        if count != map.len() {
            return Err(corrupt("import map entry count mismatch"));
        }
        Ok(())
    }
    fn posting(&self, posting: &PostingMap, namespace: u64) -> Result<(), DurableError> {
        let root = posting.root_hash().0;
        if self
            .postings
            .borrow()
            .contains(&root, namespace, posting.len())?
        {
            return Ok(());
        }
        self.map(posting, SpaceId::Namespace(namespace), |_, _| Ok(()))?;
        // Only a full successful traversal proves the count and every object.
        // Its bytes are either staged or retained in pending; state() still
        // flushes the final batch before publishing any destination metadata.
        self.postings
            .borrow_mut()
            .insert(root, namespace, posting.len());
        Ok(())
    }
    fn state(&self, state: &StateHeader, genesis: &EngineGenesis) -> Result<(), DurableError> {
        let catalog = Catalog::from_root(state.catalog_root, state.catalog_len)
            .map_err(|e| CopyFailure::from(e).0)?;
        self.map(&catalog, SpaceId::Catalog, |id, ns| {
            if ns.descriptor.id != id || id.ordinal() >= state.next_namespace {
                return Err(corrupt("import namespace association/allocator"));
            }
            let space = SpaceId::Namespace(id.ordinal());
            self.map(&ns.records, space, |_, _| Ok(()))?;
            self.map(&ns.typed.keys, space, |_, _| Ok(()))?;
            self.map(&ns.typed.rows, space, |_, row| {
                crate::typed::validate_create(&genesis.limits, &row.cells)
                    .map_err(|_| incompatible("import row exceeds admission limits"))
            })?;
            self.map(&ns.typed.terms, space, |_, posting| {
                self.posting(&posting.0, id.ordinal())
            })
        })?;
        let host = HostMap::from_root(state.host_root, state.host_len)
            .map_err(|e| CopyFailure::from(e).0)?;
        self.map(&host, SpaceId::Host, |_, blob| {
            if blob.0.len() > crate::HOST_VALUE_MAX {
                return Err(corrupt("import host value exceeds limit"));
            }
            Ok(())
        })?;
        self.flush()
    }
}
struct CopyLoader<'a, 'b> {
    copier: &'a Copier<'b>,
    space: SpaceId,
}
impl<V: TrieValue> NodeLoader<V> for CopyLoader<'_, '_> {
    type Error = CopyFailure;
    fn read(&self, id: &crate::B256) -> Result<Option<Vec<u8>>, CopyFailure> {
        let bytes = self
            .copier
            .source
            .read_object_bounded(self.space, &id.0, self.copier.object_cap)
            .map_err(|e| CopyFailure(e.into()))?;
        if let Some(bytes) = &bytes {
            self.copier
                .push(self.space, id.0, bytes)
                .map_err(CopyFailure)?;
        }
        Ok(bytes)
    }
}

impl DurableEngine {
    /// Atomically associates a bounded opaque restart cursor with an engine-only
    /// import publication. This metadata never enters the canonical state root.
    pub fn prepare_import(
        &mut self,
        candidate: SealedCandidate,
        checkpoint: &[u8],
    ) -> Result<CommitInfo, PrepareFailure> {
        let result = if checkpoint.len() > self.options.prepare_bytes {
            Err(DurableError::ResourceLimit(
                "import checkpoint exceeds batch cap".into(),
            ))
        } else {
            self.prepare_inner(&candidate, None, Some(checkpoint))
        };
        match result {
            Ok(prepared) => {
                let info = prepared.info;
                self.pending = Some(prepared);
                self.state = WriterState::Prepared;
                Ok(info)
            }
            Err(error) => Err(PrepareFailure {
                error,
                candidate: Box::new(candidate),
            }),
        }
    }
    pub fn import_checkpoint(&self, max_bytes: usize) -> Result<Option<Vec<u8>>, DurableError> {
        Ok(self.context.store.read_record_bounded(
            Table::Meta,
            checkpoint_key(self.head().sequence()).as_bytes(),
            max_bytes,
        )?)
    }
    /// Copy only current reachable state into an unpublished store and publish
    /// an engine-only genesis. Useful for bounded-memory staging compaction.
    pub fn initialize_from_staged(
        &self,
        target: Arc<dyn VersionedStore>,
        genesis: EngineGenesis,
        options: Options,
    ) -> Result<Initialization, DurableError> {
        self.publish_staged(target, genesis, options, None, None)
    }
    /// Copy current state in bounded durable batches, then atomically publish
    /// the sole height-zero manifest and simulator packet. Until publication
    /// the destination has no head and cannot be opened as a node. On failure,
    /// reopen/recover the destination and restart: object copies are idempotent.
    /// The caller must keep this destination private until successful return.
    pub fn initialize_simulator_from_staged(
        &self,
        target: Arc<dyn VersionedStore>,
        genesis: EngineGenesis,
        options: Options,
        identity: simulator::SimulatorIdentity,
        packet: simulator::SimulatorPacket,
    ) -> Result<Initialization, DurableError> {
        self.publish_staged(target, genesis, options, Some(identity), Some(packet))
    }
    fn publish_staged(
        &self,
        target: Arc<dyn VersionedStore>,
        genesis: EngineGenesis,
        options: Options,
        identity: Option<simulator::SimulatorIdentity>,
        packet: Option<simulator::SimulatorPacket>,
    ) -> Result<Initialization, DurableError> {
        genesis.validate()?;
        options.validate()?;
        self.context.generation()?;
        if self.state != WriterState::Ready || self.simulator.is_some() {
            return Err(incompatible(
                "copy requires ready engine-only staging source",
            ));
        }
        if genesis.initial_state.as_ref() != Some(&self.manifest.state)
            || genesis.limits != self.genesis.limits
            || genesis.pricing != self.genesis.pricing
        {
            return Err(incompatible("staged source/genesis association"));
        }
        if target.head()?.is_some() || target.database_id() == self.context.store.database_id() {
            return Err(incompatible(
                "copy destination must be a separate headless store",
            ));
        }
        for key in [PROFILE_KEY, GENESIS_KEY, simulator::IDENTITY_KEY] {
            if target
                .read_record_bounded(
                    Table::Meta,
                    key,
                    GENESIS_MAX_BYTES + simulator::IDENTITY_MAX_BYTES + 32,
                )?
                .is_some()
            {
                return Err(incompatible("copy destination already has engine metadata"));
            }
        }
        if let Some(identity) = &identity {
            identity.validate(&genesis)?;
        }
        let manifest = EngineManifest {
            genesis: genesis.id(),
            sequence: 0,
            parent: None,
            state_root: self.manifest.state_root,
            state: self.manifest.state.clone(),
        };
        let mut batch = manifest_batch(None, &manifest);
        batch
            .put_record(
                Table::Meta,
                PROFILE_KEY,
                identity
                    .as_ref()
                    .map_or_else(profile_bytes, |_| simulator::profile_bytes()),
            )
            .put_record(Table::Meta, GENESIS_KEY, genesis.envelope());
        if let Some(identity) = &identity {
            batch.put_record(Table::Meta, simulator::IDENTITY_KEY, identity.envelope()?);
        }
        if let Some(packet) = packet {
            if packet.reserved_bytes()? > options.prepare_bytes {
                return Err(DurableError::ResourceLimit(
                    "genesis packet exceeds batch cap".into(),
                ));
            }
            packet.append(&mut batch, &manifest)?;
        }
        if identity.is_none()
            && let Some(checkpoint) = self.import_checkpoint(options.prepare_bytes)?
        {
            batch.put_record(Table::Meta, checkpoint_key(0).into_bytes(), checkpoint);
        }
        if batch.logical_bytes() > options.prepare_bytes as u64 {
            return Err(DurableError::ResourceLimit(
                "genesis metadata exceeds batch cap".into(),
            ));
        }
        Copier {
            source: self.context.store.as_ref(),
            target: target.as_ref(),
            options: &options,
            object_cap: genesis.max_object_bytes as usize,
            pending_bytes: Cell::new(0),
            pending: RefCell::new(Vec::new()),
            postings: RefCell::new(PostingCache::new(POSTING_CACHE_SLOTS)),
        }
        .state(&manifest.state, &genesis)?;
        let engine = Self::from_manifest(target, genesis, options, manifest, identity)?;
        InitializationPending {
            engine,
            batch,
            retry_allowed: true,
        }
        .commit()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RecordId, layout::Member};
    use arkiv_db_memory::MemoryVersionedStore;
    use arkiv_db_storage::versioned::ObjectReader;
    use arkiv_db_trie::{MemoryOnly, Probe, Staged};

    fn posting(first: u64, count: u64) -> (PostingMap, Vec<(ObjectId, Vec<u8>)>) {
        let mut map = PostingMap::new();
        for ordinal in first..first + count {
            map = map
                .insert(
                    &RecordId::from_ordinal(ordinal).unwrap(),
                    Member,
                    &MemoryOnly,
                    &mut Probe::new(),
                )
                .unwrap()
                .0;
        }
        let mut staged = Staged::new();
        map.stage(&mut staged);
        (
            PostingMap::from_root(map.root_hash(), map.len()).unwrap(),
            staged
                .into_objects()
                .into_iter()
                .map(|(id, bytes)| (id.0, bytes))
                .collect(),
        )
    }

    fn stage(store: &MemoryVersionedStore, namespace: u64, objects: &[(ObjectId, Vec<u8>)]) {
        let objects: Vec<_> = objects
            .iter()
            .map(|(id, bytes)| (SpaceId::Namespace(namespace), *id, bytes.clone()))
            .collect();
        assert_eq!(store.stage_objects(&objects), CommitOutcome::Committed);
    }

    fn copier<'a>(
        source: &'a MemoryVersionedStore,
        target: &'a MemoryVersionedStore,
        options: &'a Options,
        slots: usize,
    ) -> Copier<'a> {
        Copier {
            source,
            target,
            options,
            object_cap: usize::MAX,
            pending_bytes: Cell::new(0),
            pending: RefCell::new(Vec::new()),
            postings: RefCell::new(PostingCache::new(slots)),
        }
    }

    #[test]
    fn posting_cache_skips_completed_trees_and_collisions_only_repeat_work() {
        let source = MemoryVersionedStore::new();
        let target = MemoryVersionedStore::new();
        let options = Options {
            prepare_objects: 2,
            ..Options::default()
        };
        let (first, first_objects) = posting(1, 256);
        let (second, second_objects) = posting(257, 256);
        stage(&source, 1, &first_objects);
        stage(&source, 1, &second_objects);
        // One slot forces distinct roots to evict each other.
        let copy = copier(&source, &target, &options, 1);
        copy.posting(&first, 1).unwrap();
        let first_reads = source.stats().object_reads();
        assert!(first_reads > 1);
        let pending = copy.pending.borrow().len();
        copy.posting(&first, 1).unwrap();
        assert_eq!(source.stats().object_reads(), first_reads);
        assert_eq!(copy.pending.borrow().len(), pending);
        copy.posting(&second, 1).unwrap();
        let both_reads = source.stats().object_reads();
        assert!(both_reads > first_reads);
        copy.posting(&first, 1).unwrap();
        assert_eq!(source.stats().object_reads(), both_reads + first_reads);
        assert_eq!(copy.postings.borrow().slots.len(), 1);
        copy.flush().unwrap();
        for (id, bytes) in first_objects.iter().chain(&second_objects) {
            assert_eq!(
                target.read_object(SpaceId::Namespace(1), id).unwrap(),
                Some(bytes.clone())
            );
        }
    }

    #[test]
    fn posting_cache_is_namespace_scoped_and_final_pending_bytes_are_flushed() {
        let source = MemoryVersionedStore::new();
        let target = MemoryVersionedStore::new();
        let options = Options::default();
        let (map, objects) = posting(1, 32);
        stage(&source, 1, &objects);
        let copy = copier(&source, &target, &options, 2);
        copy.posting(&map, 1).unwrap();
        assert!(
            copy.posting(&map, 2).is_err(),
            "another namespace is absent"
        );
        stage(&source, 2, &objects);
        copy.posting(&map, 2).unwrap();
        let reads = source.stats().object_reads();
        for namespace in [1, 2] {
            copy.posting(&map, namespace).unwrap();
        }
        assert_eq!(source.stats().object_reads(), reads);
        assert_eq!(
            target
                .stats()
                .spaces
                .values()
                .map(|s| s.objects_written)
                .sum::<u64>(),
            0
        );
        copy.flush().unwrap();
        for namespace in [1, 2] {
            for (id, bytes) in &objects {
                assert_eq!(
                    target
                        .read_object(SpaceId::Namespace(namespace), id)
                        .unwrap(),
                    Some(bytes.clone())
                );
            }
        }
    }

    #[test]
    fn posting_cache_never_accepts_a_mismatched_count() {
        let source = MemoryVersionedStore::new();
        let target = MemoryVersionedStore::new();
        let options = Options::default();
        let (map, objects) = posting(1, 32);
        stage(&source, 1, &objects);
        let copy = copier(&source, &target, &options, 1);
        for len in [map.len() - 1, map.len() + 1] {
            let invalid = PostingMap::from_root(map.root_hash(), len).unwrap();
            for _ in 0..2 {
                let reads = source.stats().object_reads();
                let error = copy.posting(&invalid, 1).unwrap_err();
                assert!(error.to_string().contains("entry count mismatch"));
                assert!(source.stats().object_reads() > reads);
            }
        }
        copy.posting(&map, 1).unwrap();
        let reads = source.stats().object_reads();
        for len in [map.len() - 1, map.len() + 1] {
            let invalid = PostingMap::from_root(map.root_hash(), len).unwrap();
            let error = copy.posting(&invalid, 1).unwrap_err();
            assert!(error.to_string().contains("entry count mismatch"));
        }
        assert_eq!(source.stats().object_reads(), reads);
        copy.posting(&map, 1).unwrap();
    }

    #[test]
    fn posting_cache_does_not_remember_partial_or_corrupt_traversals() {
        let source = MemoryVersionedStore::new();
        let target = MemoryVersionedStore::new();
        let options = Options::default();
        let (map, mut objects) = posting(1, 256);
        let missing = objects
            .iter()
            .position(|(id, _)| *id != map.root_hash().0)
            .unwrap();
        let missing = objects.remove(missing);
        stage(&source, 1, &objects);
        let copy = copier(&source, &target, &options, 1);
        for _ in 0..2 {
            let reads = source.stats().object_reads();
            assert!(copy.posting(&map, 1).is_err());
            assert!(source.stats().object_reads() > reads);
            assert!(
                !copy
                    .postings
                    .borrow()
                    .contains(&map.root_hash().0, 1, map.len())
                    .unwrap()
            );
        }
        stage(&source, 1, &[missing]);
        copy.posting(&map, 1).unwrap();
        let reads = source.stats().object_reads();
        copy.posting(&map, 1).unwrap();
        assert_eq!(source.stats().object_reads(), reads);

        // Hash-valid malformed RLP still must fail trie validation on each try.
        let bytes = vec![0x42; 40];
        let root = alloy_primitives::keccak256(&bytes);
        stage(&source, 1, &[(root.0, bytes)]);
        let corrupt = PostingMap::from_root(root, 1).unwrap();
        for _ in 0..2 {
            let reads = source.stats().object_reads();
            assert!(copy.posting(&corrupt, 1).is_err());
            assert_eq!(source.stats().object_reads(), reads + 1);
            assert!(!copy.postings.borrow().contains(&root.0, 1, 1).unwrap());
        }
    }

    #[test]
    fn posting_cache_capacity_is_fixed() {
        let cache = PostingCache::new(POSTING_CACHE_SLOTS);
        assert_eq!(std::mem::size_of_val(&*cache.slots), 768 * 1024);
    }
}
