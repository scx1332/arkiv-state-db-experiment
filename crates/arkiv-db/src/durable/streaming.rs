//! Bounded, restartable copying of a private import's current state.
use super::*;
use crate::{
    StateHeader,
    layout::{Catalog, HostMap},
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
struct Copier<'a> {
    source: &'a dyn VersionedStore,
    target: &'a dyn VersionedStore,
    options: &'a Options,
    object_cap: usize,
    pending_bytes: Cell<usize>,
    pending: RefCell<Vec<(SpaceId, ObjectId, Vec<u8>)>>,
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
                self.map(&posting.0, space, |_, _| Ok(()))
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
