//! Narrow atomic host composition for the unsigned simulator. This profile
//! admits one bounded node envelope and typed inclusion keys, never arbitrary
//! table writes. Canonical node/body/feed validation remains the host's job.
use super::*;
use crate::{DecodeError, Decoder, Encoder, digest};
use std::collections::BTreeSet;

pub const METADATA_MAX_BYTES: usize = 64 * 1024;
pub const BLOCK_MAX_BYTES: usize = 8 * 1024 * 1024;
pub const BLOCK_FRAMING_BYTES: usize = 116;
pub const INCLUSION_MAX_BYTES: usize = 1024;
pub const MAX_INCLUSIONS: usize = 4096;
pub(crate) const IDENTITY_KEY: &[u8] = b"simulator/identity";
pub(crate) const IDENTITY_MAX_BYTES: usize = 8 + 16 + 16 + 32 + 4 + METADATA_MAX_BYTES + 32;
const INCLUSION_FRAMING_MAX: usize = 8 + 8 + 32 + 1 + 61 + 4 + 32;

pub(crate) fn profile_bytes() -> Vec<u8> {
    [b"ARKSIM01".as_slice(), &1_u32.to_be_bytes()].concat()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SimulatorIdentity {
    pub source_id: [u8; 16],
    pub run_id: [u8; 16],
    pub node_genesis_hash: B256,
    /// Host-defined canonical immutable genesis/configuration, not input values.
    pub metadata: Vec<u8>,
}
impl SimulatorIdentity {
    pub fn deployment(&self) -> [u8; 32] {
        let mut preimage = [0; 48];
        preimage[..32].copy_from_slice(self.node_genesis_hash.as_slice());
        preimage[32..].copy_from_slice(&self.run_id);
        digest("arkiv/simulator-engine/v1", &preimage).0
    }
    pub(crate) fn validate(&self, genesis: &EngineGenesis) -> Result<(), DurableError> {
        self.check_size()?;
        if genesis.deployment != self.deployment() {
            return Err(incompatible("simulator/engine genesis association"));
        }
        Ok(())
    }
    fn check_size(&self) -> Result<(), DurableError> {
        if self.metadata.is_empty() || self.metadata.len() > METADATA_MAX_BYTES {
            return Err(DurableError::ResourceLimit(
                "simulator metadata size".into(),
            ));
        }
        Ok(())
    }
    pub fn envelope(&self) -> Result<Vec<u8>, DurableError> {
        self.check_size()?;
        let mut e = Encoder::new();
        e.fixed(b"ARKSIMI1");
        e.fixed(&self.source_id);
        e.fixed(&self.run_id);
        e.fixed(self.node_genesis_hash.as_slice());
        e.bytes(&self.metadata);
        Ok(seal(e.into_bytes(), "arkiv/simulator-profile/v1"))
    }
    pub fn from_envelope(bytes: &[u8]) -> Result<Self, DurableError> {
        let body = unseal(
            bytes,
            IDENTITY_MAX_BYTES,
            b"ARKSIMI1",
            "arkiv/simulator-profile/v1",
        )?;
        let decoded = (|| -> Result<_, DecodeError> {
            let mut d = Decoder::new(&body[8..]);
            let identity = Self {
                source_id: d.array()?,
                run_id: d.array()?,
                node_genesis_hash: B256::from(d.array::<32>()?),
                metadata: d.bytes()?.to_vec(),
            };
            Ok((identity, d.remaining()))
        })()
        .map_err(|_| corrupt("malformed simulator identity"))?;
        if decoded.1 != 0 || decoded.0.check_size().is_err() {
            return Err(corrupt("invalid simulator identity"));
        }
        Ok(decoded.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SimulatorInclusion {
    /// Canonical request identity (61 bytes, tx/admin tags 0/1), or control
    /// identity (tag 2 followed by 32 command-id bytes).
    pub key: Vec<u8>,
    pub value: Vec<u8>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SimulatorPacket {
    pub height: u64,
    pub state_root: B256,
    /// Host's versioned canonical block and sanitized outbox envelope.
    pub bytes: Vec<u8>,
    pub inclusions: Vec<SimulatorInclusion>,
}
impl SimulatorPacket {
    pub(crate) fn validate(&self) -> Result<(), DurableError> {
        if self.bytes.is_empty() || self.bytes.len() > BLOCK_MAX_BYTES - BLOCK_FRAMING_BYTES {
            return Err(DurableError::ResourceLimit(
                "simulator block envelope size".into(),
            ));
        }
        if self.inclusions.len() > MAX_INCLUSIONS
            || (self.height == 0 && !self.inclusions.is_empty())
        {
            return Err(DurableError::ResourceLimit(
                "simulator inclusion count".into(),
            ));
        }
        let mut seen = BTreeSet::new();
        for item in &self.inclusions {
            validate_key(&item.key)?;
            if item.value.is_empty() || item.value.len() > INCLUSION_MAX_BYTES {
                return Err(DurableError::ResourceLimit(
                    "simulator inclusion size".into(),
                ));
            }
            if !seen.insert(item.key.as_slice()) {
                return Err(incompatible("duplicate simulator inclusion"));
            }
        }
        Ok(())
    }
    pub(crate) fn reserved_bytes(&self) -> Result<usize, DurableError> {
        self.validate()?;
        // Finite per-item/count limits make this arithmetic fit even usize32.
        Ok(8 + BLOCK_FRAMING_BYTES
            + self.bytes.len()
            + self
                .inclusions
                .iter()
                .map(|i| i.key.len() + 8 + 8 + 32 + 1 + i.key.len() + 4 + i.value.len() + 32)
                .sum::<usize>())
    }
    pub(crate) fn append(
        &self,
        batch: &mut CommitBatch,
        manifest: &EngineManifest,
    ) -> Result<(), DurableError> {
        self.validate()?;
        if self.height != manifest.sequence || self.state_root != manifest.state_root {
            return Err(incompatible("simulator packet/state association"));
        }
        let mut e = Encoder::new();
        e.fixed(b"ARKSIMB1");
        e.u64(self.height);
        e.fixed(manifest.id().as_slice());
        e.fixed(self.state_root.as_slice());
        e.bytes(&self.bytes);
        batch.put_record(
            Table::Blocks,
            self.height.to_be_bytes(),
            seal(e.into_bytes(), "arkiv/simulator-node-record/v1"),
        );
        for item in &self.inclusions {
            let mut e = Encoder::new();
            e.fixed(b"ARKSIMR1");
            e.u64(self.height);
            e.fixed(manifest.id().as_slice());
            e.u8(item.key.len() as u8);
            e.fixed(&item.key);
            e.bytes(&item.value);
            batch.put_record(
                Table::Inclusions,
                item.key.as_slice(),
                seal(e.into_bytes(), "arkiv/simulator-inclusion/v1"),
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SimulatorBlock {
    pub info: CommitInfo,
    pub bytes: Vec<u8>,
}

fn seal(mut bytes: Vec<u8>, domain: &str) -> Vec<u8> {
    let hash = digest(domain, &bytes);
    bytes.extend_from_slice(hash.as_slice());
    bytes
}
fn unseal<'a>(
    bytes: &'a [u8],
    cap: usize,
    magic: &[u8; 8],
    domain: &str,
) -> Result<&'a [u8], DurableError> {
    if bytes.len() < 40 || bytes.len() > cap || &bytes[..8] != magic {
        return Err(corrupt("simulator record size/magic"));
    }
    let (body, hash) = bytes.split_at(bytes.len() - 32);
    if digest(domain, body).as_slice() != hash {
        return Err(corrupt("simulator record digest"));
    }
    Ok(body)
}
fn validate_key(key: &[u8]) -> Result<(), DurableError> {
    if !matches!((key.first(), key.len()), (Some(0 | 1), 61) | (Some(2), 33)) {
        return Err(incompatible("simulator inclusion key tag/length"));
    }
    Ok(())
}
pub(crate) fn read_identity(store: &dyn VersionedStore) -> Result<SimulatorIdentity, DurableError> {
    if let Health::Failed(e) = store.health() {
        return Err(e.into());
    }
    SimulatorIdentity::from_envelope(&required(
        store,
        Table::Meta,
        IDENTITY_KEY,
        IDENTITY_MAX_BYTES,
    )?)
}
pub(crate) fn read_block(
    store: &dyn VersionedStore,
    manifest: &EngineManifest,
) -> Result<SimulatorBlock, DurableError> {
    let bytes = required(
        store,
        Table::Blocks,
        &manifest.sequence.to_be_bytes(),
        BLOCK_MAX_BYTES,
    )?;
    let body = unseal(
        &bytes,
        BLOCK_MAX_BYTES,
        b"ARKSIMB1",
        "arkiv/simulator-node-record/v1",
    )?;
    let decoded = (|| -> Result<_, DecodeError> {
        let mut d = Decoder::new(&body[8..]);
        let height = d.u64()?;
        let id = d.array::<32>()?;
        let root = d.array::<32>()?;
        let payload = d.bytes()?;
        Ok((height, id, root, payload, d.remaining()))
    })()
    .map_err(|_| corrupt("malformed simulator block"))?;
    if decoded.0 != manifest.sequence
        || decoded.1 != manifest.id().0
        || decoded.2 != manifest.state_root.0
        || decoded.3.is_empty()
        || decoded.4 != 0
    {
        return Err(corrupt("simulator block/manifest association"));
    }
    Ok(SimulatorBlock {
        info: CommitInfo {
            snapshot: SnapshotId::from_sequence(decoded.0),
            commit_id: manifest.id(),
            state_root: manifest.state_root,
        },
        bytes: decoded.3.to_vec(),
    })
}

impl DurableEngine {
    pub fn read_simulator_identity(
        store: &dyn VersionedStore,
    ) -> Result<SimulatorIdentity, DurableError> {
        if required(store, Table::Meta, PROFILE_KEY, 12)? != profile_bytes() {
            return Err(incompatible("expected simulator storage profile"));
        }
        read_identity(store)
    }
    pub fn initialize_simulator(
        store: Arc<dyn VersionedStore>,
        genesis: EngineGenesis,
        options: Options,
        identity: SimulatorIdentity,
        packet: SimulatorPacket,
    ) -> Result<Initialization, DurableError> {
        Self::initialize_inner(store, genesis, options, Some(identity), Some(packet), None)
    }
    /// Atomically publishes the complete, bounded initial state and host packet
    /// at height zero. The candidate must start at an in-memory engine's empty
    /// genesis and match `genesis.initial_state`; no intermediate head exists.
    /// The host validates its own checkpoint and canonical packet semantics.
    /// Preparation uses the same finite object, byte and visit caps as commits.
    pub fn initialize_simulator_prefilled(
        store: Arc<dyn VersionedStore>,
        genesis: EngineGenesis,
        options: Options,
        identity: SimulatorIdentity,
        packet: SimulatorPacket,
        candidate: SealedCandidate,
    ) -> Result<Initialization, DurableError> {
        Self::initialize_inner(
            store,
            genesis,
            options,
            Some(identity),
            Some(packet),
            Some(candidate),
        )
    }
    pub fn open_simulator(
        store: Arc<dyn VersionedStore>,
        genesis: EngineGenesis,
        options: Options,
        identity: SimulatorIdentity,
    ) -> Result<Self, DurableError> {
        genesis.validate()?;
        options.validate()?;
        identity.validate(&genesis)?;
        let manifest = read_open(store.as_ref(), &genesis, Some(&identity))?;
        Self::from_manifest(store, genesis, options, manifest, Some(identity))
    }
    pub fn prepare_simulator(
        &mut self,
        candidate: SealedCandidate,
        packet: SimulatorPacket,
    ) -> Result<CommitInfo, PrepareFailure> {
        self.prepare_packet(candidate, Some(packet))
    }
    pub fn simulator_block(&self, height: u64) -> Result<SimulatorBlock, DurableError> {
        if self.simulator.is_none() {
            return Err(incompatible("expected simulator profile"));
        }
        self.context.generation()?;
        if height > self.head().sequence() {
            return Err(EngineError::UnknownSnapshot(SnapshotId::from_sequence(height)).into());
        }
        let manifest = read_manifest(self.context.store.as_ref(), &self.genesis, height)?;
        read_block(self.context.store.as_ref(), &manifest)
    }
    pub fn simulator_inclusion(&self, key: &[u8]) -> Result<Option<Vec<u8>>, DurableError> {
        if self.simulator.is_none() {
            return Err(incompatible("expected simulator profile"));
        }
        self.context.generation()?;
        validate_key(key)?;
        let Some(bytes) = self.context.store.read_record_bounded(
            Table::Inclusions,
            key,
            INCLUSION_FRAMING_MAX + INCLUSION_MAX_BYTES,
        )?
        else {
            return Ok(None);
        };
        let body = unseal(
            &bytes,
            INCLUSION_FRAMING_MAX + INCLUSION_MAX_BYTES,
            b"ARKSIMR1",
            "arkiv/simulator-inclusion/v1",
        )?;
        let decoded = (|| -> Result<_, DecodeError> {
            let mut d = Decoder::new(&body[8..]);
            let height = d.u64()?;
            let id = d.array::<32>()?;
            let key_len = d.u8()? as usize;
            let stored_key = d.fixed(key_len)?;
            let value = d.bytes()?;
            Ok((height, id, stored_key, value, d.remaining()))
        })()
        .map_err(|_| corrupt("malformed simulator inclusion"))?;
        if decoded.2 != key
            || decoded.3.is_empty()
            || decoded.3.len() > INCLUSION_MAX_BYTES
            || decoded.4 != 0
        {
            return Err(corrupt("simulator inclusion key/value"));
        }
        // A readback of an uncertain future batch never exposes its inclusion.
        if decoded.0 > self.head().sequence() {
            return Ok(None);
        }
        let manifest = read_manifest(self.context.store.as_ref(), &self.genesis, decoded.0)?;
        if decoded.1 != manifest.id().0 {
            return Err(corrupt("simulator inclusion/manifest association"));
        }
        Ok(Some(decoded.3.to_vec()))
    }
}
