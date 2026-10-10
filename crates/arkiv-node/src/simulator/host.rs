//! One durable writer. Published headers are exposed only after Committed.
use super::{Error, Result, metadata, wire::*, workload::Workload};
use crate::{
    Block, BlockInputs, BlockOutcomes, ChainSpec, Genesis, Header, Inclusion, RequestIdentity,
    Scope, exec::execute_block_observed,
};
use arkiv_db::durable::{
    DurableEngine, EngineGenesis, Initialization, Options, Publication, Recovery,
    simulator::{SimulatorIdentity, SimulatorInclusion, SimulatorPacket},
};
use arkiv_db::{
    B256, Budget, Cost, Decode, DecodeError, Decoder, Encode, Encoder, decode_exact, digest, encode,
};
use arkiv_db_storage::versioned::VersionedStore;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::Arc};

const NODE_VERSION: u32 = arkiv_db::LAYOUT_VERSION;
#[derive(Clone, Debug)]
pub struct RunConfig {
    pub genesis: Genesis,
    pub workload: Workload,
}
impl Encode for RunConfig {
    fn encode_to(&self, e: &mut Encoder) {
        e.u32(NODE_VERSION);
        e.put(&self.genesis);
        e.put(&self.workload);
    }
}
impl Decode for RunConfig {
    fn decode_from(d: &mut Decoder<'_>) -> std::result::Result<Self, DecodeError> {
        if d.u32()? != NODE_VERSION {
            return Err(DecodeError::Invalid {
                kind: "SimulatorRun",
                reason: "version",
            });
        }
        Ok(Self {
            genesis: d.get()?,
            workload: d.get()?,
        })
    }
}
impl RunConfig {
    pub fn validate(&self) -> Result<ChainSpec> {
        self.workload.validate()?;
        let g = &self.genesis;
        let selected = crate::demo::workload::genesis(g.chain_id, g.timestamp_ms);
        // Genesis fixes the resource profile. Unindexed field values become
        // bounded descriptors in the feed. Every published feed is separately
        // capped at 4 MiB.
        if g.limits != selected.limits || g.params != selected.params {
            return Err(Error("UnsupportedVersion"));
        }
        ChainSpec::new(g.clone()).map_err(|_| Error("InvalidRequest"))
    }
}
pub fn uuid(bytes: &[u8; 16]) -> String {
    let s = hex(bytes);
    let s = &s[2..];
    format!(
        "{}-{}-{}-{}-{}",
        &s[..8],
        &s[8..12],
        &s[12..16],
        &s[16..20],
        &s[20..]
    )
}
pub fn parse_uuid(s: &str) -> Result<[u8; 16]> {
    if s.len() != 36 || [8, 13, 18, 23].iter().any(|&i| s.as_bytes()[i] != b'-') {
        return Err(Error("InvalidRequest"));
    }
    let b = unhex(&format!("0x{}", s.replace('-', "")), 16)?;
    let a: [u8; 16] = b.try_into().map_err(|_| Error("InvalidRequest"))?;
    if uuid(&a) != s {
        return Err(Error("InvalidRequest"));
    }
    Ok(a)
}
pub fn identity(i: &SimulatorIdentity, g: &Genesis) -> Identity {
    Identity {
        source_id: uuid(&i.source_id),
        run_id: hex(&i.run_id)[2..].into(),
        genesis_hash: hex(i.node_genesis_hash.as_slice()),
        chain_id: g.chain_id.to_string(),
    }
}
pub fn engine_genesis(i: &SimulatorIdentity, g: &Genesis) -> EngineGenesis {
    EngineGenesis {
        initial_state: None,
        deployment: i.deployment(),
        limits: g.limits,
        pricing: g.pricing.clone(),
        max_object_bytes: 8 * 1024 * 1024,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Control {
    pub command_id: String,
    pub run_id: String,
    pub expected_revision: String,
    pub expected_height: Option<String>,
    pub action: String,
    pub config: Option<Workload>,
}
impl Control {
    pub fn validate(&self) -> Result<([u8; 32], B256)> {
        let id: [u8; 32] = unhex(&self.command_id, 32)?
            .try_into()
            .map_err(|_| Error("InvalidRequest"))?;
        if self.run_id.len() != 32 || unhex(&format!("0x{}", self.run_id), 16)?.len() != 16 {
            return Err(Error("InvalidRequest"));
        }
        let revision = number(&self.expected_revision)?;
        let height = self.expected_height.as_deref().map(number).transpose()?;
        let action = match self.action.as_str() {
            "pause" => 1,
            "resume" => 2,
            "step" => 3,
            "configure" => 4,
            _ => return Err(Error("InvalidRequest")),
        };
        if (action == 4) != self.config.is_some() {
            return Err(Error("InvalidRequest"));
        }
        if let Some(c) = &self.config {
            c.validate()?;
        }
        let mut e = Encoder::new();
        e.u32(1);
        e.fixed(&id);
        e.str(&self.run_id);
        e.u64(revision);
        e.put(&height);
        e.u8(action);
        e.put(&self.config);
        Ok((id, digest("arkiv/simulator-control/v1", &e.into_bytes())))
    }
}
#[derive(Clone, Debug)]
pub struct ControlReceipt {
    pub id: [u8; 32],
    pub digest: B256,
}
impl Encode for ControlReceipt {
    fn encode_to(&self, e: &mut Encoder) {
        e.fixed(&self.id);
        e.put(&self.digest);
    }
}
impl Decode for ControlReceipt {
    const MIN_ENCODED_LEN: usize = 64;
    fn decode_from(d: &mut Decoder<'_>) -> std::result::Result<Self, DecodeError> {
        Ok(Self {
            id: d.array()?,
            digest: d.get()?,
        })
    }
}
#[derive(Clone, Debug)]
pub struct StoredBlock {
    pub block: Block,
    pub feed: FeedBlock,
    pub workload: Workload,
    pub revision: u64,
    pub controls: Vec<ControlReceipt>,
}
impl StoredBlock {
    pub fn bytes(&self) -> Result<Vec<u8>> {
        let feed = json_bytes(&self.feed, FEED_CAP)?;
        let body = encode(&self.block);
        if body.len() > 1024 * 1024 || self.controls.len() > 2 {
            return Err(Error("CapacityReached"));
        }
        let mut e = Encoder::new();
        e.u32(NODE_VERSION);
        e.bytes(&body);
        e.bytes(&feed);
        e.put(&self.workload);
        e.u64(self.revision);
        e.put(&self.controls);
        let bytes = e.into_bytes();
        if bytes.len() > ENVELOPE_CAP || bytes.len() * 2 + 4096 > RESPONSE_CAP {
            return Err(Error("CapacityReached"));
        }
        Ok(bytes)
    }
    pub fn from_bytes(bytes: &[u8], id: &Identity, genesis: &Genesis) -> Result<Self> {
        if bytes.len() > ENVELOPE_CAP || bytes.len() * 2 + 4096 > RESPONSE_CAP {
            return Err(Error("StorageCorrupt"));
        }
        let parse = || -> std::result::Result<Self, DecodeError> {
            let mut d = Decoder::new(bytes);
            if d.u32()? != NODE_VERSION {
                return Err(DecodeError::Invalid {
                    kind: "SimulatorBlock",
                    reason: "version",
                });
            }
            let body = d.bytes()?;
            if body.len() > 1024 * 1024 {
                return Err(DecodeError::Invalid {
                    kind: "SimulatorBlock",
                    reason: "body cap",
                });
            }
            let block = decode_exact(body)?;
            let feed = d.bytes()?;
            if feed.len() > FEED_CAP {
                return Err(DecodeError::Invalid {
                    kind: "SimulatorBlock",
                    reason: "feed cap",
                });
            }
            let feed = serde_json::from_slice(feed).map_err(|_| DecodeError::Invalid {
                kind: "SimulatorBlock",
                reason: "feed",
            })?;
            let workload = d.get()?;
            let revision = d.u64()?;
            let controls: Vec<ControlReceipt> = d.get()?;
            if controls.len() > 2 || d.remaining() != 0 {
                return Err(DecodeError::Invalid {
                    kind: "SimulatorBlock",
                    reason: "trailing/count",
                });
            }
            Ok(Self {
                block,
                feed,
                workload,
                revision,
                controls,
            })
        };
        let record = parse().map_err(|_| Error("StorageCorrupt"))?;
        let b = &record.block;
        let f = &record.feed;
        f.validate_digest().map_err(|_| Error("StorageCorrupt"))?;
        if f.feed_version != 1
            || (&f.source_id, &f.run_id, &f.genesis_hash, &f.chain_id)
                != (&id.source_id, &id.run_id, &id.genesis_hash, &id.chain_id)
            || f.header
                .validate(genesis.chain_id)
                .map_err(|_| Error("StorageCorrupt"))?
                != b.header
            || f.spent_units != b.outcomes.spent.to_string()
            || b.header.outcomes_digest != b.outcomes.digest()
        {
            return Err(Error("StorageCorrupt"));
        }
        if b.header.height == 0 {
            if b.header != genesis.header()
                || b.inputs != BlockInputs::default()
                || b.outcomes != BlockOutcomes::default()
            {
                return Err(Error("StorageCorrupt"));
            }
        } else if b.header.inputs_digest != b.inputs.digest() {
            return Err(Error("StorageCorrupt"));
        }
        if record.bytes().map_err(|_| Error("StorageCorrupt"))? != bytes {
            return Err(Error("StorageCorrupt"));
        }
        Ok(record)
    }
}

pub struct Host {
    pub engine: DurableEngine,
    pub spec: ChainSpec,
    pub run: RunConfig,
    pub durable_identity: SimulatorIdentity,
    pub identity: Identity,
    pub head: Header,
    pub workload: Workload,
    pub revision: u64,
    pub paused: bool,
    pub health: &'static str,
    pub observed: u64,
    pub role: &'static str,
    pending: Option<StoredBlock>,
    pending_config: Option<(ControlReceipt, Workload)>,
    volatile_controls: BTreeMap<[u8; 32], B256>,
    /// The last state-retention refusal, for API error details.
    pub refusal: crate::retention::Refusal,
}
impl Host {
    pub fn create(
        store: Arc<dyn VersionedStore>,
        run: RunConfig,
        source: [u8; 16],
        run_id: [u8; 16],
        options: Options,
    ) -> Result<Self> {
        let spec = run.validate()?;
        let head = run.genesis.header();
        let durable_identity = SimulatorIdentity {
            source_id: source,
            run_id,
            node_genesis_hash: head.hash(),
            metadata: encode(&run),
        };
        let identity = identity(&durable_identity, &run.genesis);
        let record = StoredBlock {
            block: Block {
                header: head,
                inputs: BlockInputs::default(),
                outcomes: BlockOutcomes::default(),
            },
            feed: FeedBlock::new(&identity, &head, Default::default())?,
            workload: run.workload.clone(),
            revision: 0,
            controls: vec![],
        };
        let packet = SimulatorPacket {
            height: 0,
            state_root: head.state_root,
            bytes: record.bytes()?,
            inclusions: vec![],
        };
        let engine = match DurableEngine::initialize_simulator(
            store,
            engine_genesis(&durable_identity, &run.genesis),
            options,
            durable_identity.clone(),
            packet,
        )? {
            Initialization::Committed(e) => e,
            Initialization::NotCommitted { .. } => return Err(Error("StorageUnavailable")),
            Initialization::Indeterminate { .. } => return Err(Error("CommitUncertain")),
        };
        Ok(Self {
            engine,
            spec,
            workload: run.workload.clone(),
            run,
            durable_identity,
            identity,
            head,
            revision: 0,
            paused: true,
            health: "paused",
            observed: 0,
            role: "producer",
            pending: None,
            pending_config: None,
            volatile_controls: BTreeMap::new(),
            refusal: Default::default(),
        })
    }
    pub fn open(
        store: Arc<dyn VersionedStore>,
        expected: Option<&Identity>,
        options: Options,
    ) -> Result<Self> {
        let durable_identity = DurableEngine::read_simulator_identity(store.as_ref())?;
        let run: RunConfig =
            decode_exact(&durable_identity.metadata).map_err(|_| Error("StorageCorrupt"))?;
        let spec = run.validate()?;
        if run.genesis.header().hash() != durable_identity.node_genesis_hash {
            return Err(Error("StorageCorrupt"));
        }
        let identity = identity(&durable_identity, &run.genesis);
        if expected.is_some_and(|e| *e != identity) {
            return Err(Error("IdentityMismatch"));
        }
        let engine = DurableEngine::open_simulator(
            store,
            engine_genesis(&durable_identity, &run.genesis),
            options,
            durable_identity.clone(),
        )?;
        let stored = engine.simulator_block(engine.head().sequence())?;
        let record = StoredBlock::from_bytes(&stored.bytes, &identity, &run.genesis)?;
        if record.block.header.height != stored.info.snapshot.sequence()
            || record.block.header.state_root != stored.info.state_root
        {
            return Err(Error("StorageCorrupt"));
        }
        run.workload
            .update_allowed(&record.workload)
            .map_err(|_| Error("StorageCorrupt"))?;
        Ok(Self {
            head: record.block.header,
            workload: record.workload,
            revision: record.revision,
            observed: record.block.header.height,
            engine,
            spec,
            run,
            durable_identity,
            identity,
            paused: true,
            health: "paused",
            role: "producer",
            pending: None,
            pending_config: None,
            volatile_controls: BTreeMap::new(),
            refusal: Default::default(),
        })
    }
    pub fn block(&self, height: u64) -> Result<StoredBlock> {
        if height > self.head.height {
            return Err(Error("UnknownBlock"));
        }
        let stored = self.engine.simulator_block(height)?;
        let block = StoredBlock::from_bytes(&stored.bytes, &self.identity, &self.run.genesis)?;
        if block.block.header.height != height
            || block.block.header.state_root != stored.info.state_root
        {
            return Err(Error("StorageCorrupt"));
        }
        Ok(block)
    }
    pub fn header(&self, height: u64) -> Result<Header> {
        if height == self.head.height {
            Ok(self.head)
        } else {
            Ok(self.block(height)?.block.header)
        }
    }
    fn existing_control(&self, id: &[u8; 32], digest: B256) -> Result<Option<u64>> {
        let mut key = vec![2];
        key.extend_from_slice(id);
        let Some(bytes) = self.engine.simulator_inclusion(&key)? else {
            return Ok(None);
        };
        if bytes.len() != 40 {
            return Err(Error("StorageCorrupt"));
        }
        if bytes[..32] != digest.0 {
            return Err(Error("RequestConflict"));
        }
        Ok(Some(u64::from_be_bytes(bytes[32..].try_into().unwrap())))
    }
    pub fn control(&mut self, c: Control) -> Result<serde_json::Value> {
        if self.role != "producer" {
            return Err(Error("InvalidRequest"));
        }
        let (id, digest) = c.validate()?;
        if c.run_id != self.identity.run_id {
            return Err(Error("IdentityMismatch"));
        }
        if let Some(height) = self.existing_control(&id, digest)? {
            return Ok(
                serde_json::json!({"status":"committed","height":height.to_string(),"configRevision":self.revision.to_string()}),
            );
        }
        if let Some(old) = self.volatile_controls.get(&id) {
            if *old != digest {
                return Err(Error("RequestConflict"));
            }
            return Ok(serde_json::json!({"status":"volatile","paused":self.paused}));
        }
        if number(&c.expected_revision)? != self.revision
            || c.expected_height
                .as_deref()
                .map(number)
                .transpose()?
                .is_some_and(|h| h != self.head.height)
        {
            return Err(Error("RequestConflict"));
        }
        match c.action.as_str() {
            "pause" | "resume" => {
                self.paused = c.action == "pause";
                if self.health != "storage-fenced" {
                    self.health = if self.paused { "paused" } else { "running" };
                }
                if self.volatile_controls.len() >= 64 {
                    let key = *self.volatile_controls.first_key_value().unwrap().0;
                    self.volatile_controls.remove(&key);
                }
                self.volatile_controls.insert(id, digest);
                Ok(serde_json::json!({"status":"volatile","paused":self.paused}))
            }
            "configure" => {
                let config = c.config.unwrap();
                self.workload.update_allowed(&config)?;
                if let Some((old, prior)) = &self.pending_config
                    && (old.id != id || old.digest != digest || prior != &config)
                {
                    return Err(Error("QueueFull"));
                }
                self.pending_config = Some((ControlReceipt { id, digest }, config));
                Ok(
                    serde_json::json!({"status":"pending","effectiveHeight":self.head.height.checked_add(1).ok_or(Error("CapacityReached"))?.to_string()}),
                )
            }
            "step" => {
                if !self.paused {
                    return Err(Error("RequestConflict"));
                }
                let h = self.produce(Some(ControlReceipt { id, digest }))?;
                Ok(
                    serde_json::json!({"status":"committed","height":h.height.to_string(),"configRevision":self.revision.to_string()}),
                )
            }
            _ => Err(Error("InvalidRequest")),
        }
    }
    pub fn produce(&mut self, command: Option<ControlReceipt>) -> Result<Header> {
        if self.pending.is_some() {
            return Err(Error("CommitUncertain"));
        }
        let height = self
            .head
            .height
            .checked_add(1)
            .ok_or(Error("CapacityReached"))?;
        let workload = self
            .pending_config
            .as_ref()
            .map_or(&self.workload, |(_, c)| c)
            .clone();
        let revision = if self.pending_config.is_some() {
            self.revision
                .checked_add(1)
                .ok_or(Error("CapacityReached"))?
        } else {
            self.revision
        };
        let inputs = workload.inputs(self.spec.chain_id(), height)?;
        let timestamp = workload.timestamp(self.run.genesis.timestamp_ms, height)?;
        let mut controls = Vec::new();
        if let Some((c, _)) = &self.pending_config {
            controls.push(c.clone());
        }
        if let Some(c) = command {
            controls.push(c);
        }
        self.execute_publish(inputs, timestamp, workload, revision, controls, None)
    }
    pub fn import(&mut self, record: StoredBlock) -> Result<Header> {
        let height = record.block.header.height;
        if height <= self.head.height {
            if self.block(height)?.bytes()? != record.bytes()? {
                return Err(Error("ChainConflict"));
            }
            return Ok(self.head);
        }
        if height
            != self
                .head
                .height
                .checked_add(1)
                .ok_or(Error("CapacityReached"))?
        {
            return Err(Error("BlockNotSynced"));
        }
        self.workload.update_allowed(&record.workload)?;
        if record.revision < self.revision
            || record.revision > self.revision.saturating_add(1)
            || (record.revision == self.revision && record.workload != self.workload)
        {
            return Err(Error("ChainConflict"));
        }
        if record.block.header.timestamp_ms
            != record
                .workload
                .timestamp(self.run.genesis.timestamp_ms, height)?
        {
            return Err(Error("ChainConflict"));
        }
        self.execute_publish(
            record.block.inputs.clone(),
            record.block.header.timestamp_ms,
            record.workload.clone(),
            record.revision,
            record.controls.clone(),
            Some(&record),
        )
    }
    fn execute_publish(
        &mut self,
        inputs: BlockInputs,
        timestamp: u64,
        workload: Workload,
        revision: u64,
        controls: Vec<ControlReceipt>,
        expected: Option<&StoredBlock>,
    ) -> Result<Header> {
        let height = self
            .head
            .height
            .checked_add(1)
            .ok_or(Error("CapacityReached"))?;
        // Included requests may never be replayed as new work.
        for (identity, _, _) in RequestIdentity::of_block(self.spec.chain_id(), &inputs) {
            if self
                .engine
                .simulator_inclusion(&encode(&identity))?
                .is_some()
            {
                return Err(Error("RequestConflict"));
            }
        }
        for c in &controls {
            if self.existing_control(&c.id, c.digest)?.is_some() {
                return Err(Error("RequestConflict"));
            }
        }
        let branch = self.engine.begin(
            self.engine.head(),
            Some(Cost::new(self.spec.params().block_budget)),
        )?;
        let mut metadata = None;
        let executed = execute_block_observed(
            &self.spec,
            branch,
            &self.head,
            height,
            timestamp,
            &inputs,
            |branch, outcomes, expired| {
                metadata = Some(metadata::capture(branch, &inputs, outcomes, expired)?);
                Ok(())
            },
        )
        .map_err(|e| match e {
            crate::ExecError::Storage(s) => s.into(),
            _ => Error("InvalidRequest"),
        })?;
        let record = StoredBlock {
            feed: FeedBlock::new(
                &self.identity,
                &executed.header,
                metadata.ok_or(Error("StorageCorrupt"))?,
            )?,
            block: Block {
                header: executed.header,
                inputs,
                outcomes: executed.outcomes,
            },
            workload,
            revision,
            controls,
        };
        let bytes = record.bytes()?;
        if let Some(expected) = expected
            && expected.bytes()? != bytes
        {
            return Err(Error("ChainConflict"));
        }
        let mut inclusions = Vec::new();
        for (identity, scope, index) in
            RequestIdentity::of_block(self.spec.chain_id(), &record.block.inputs)
        {
            let digest = match scope {
                Scope::Tx => record.block.inputs.txs[index as usize].digest(),
                Scope::Admin => record.block.inputs.admin[index as usize].digest(),
            };
            inclusions.push(SimulatorInclusion {
                key: encode(&identity),
                value: encode(&Inclusion {
                    height,
                    scope,
                    index,
                    digest,
                }),
            });
        }
        for c in &record.controls {
            let mut key = vec![2];
            key.extend_from_slice(&c.id);
            let mut value = c.digest.to_vec();
            value.extend_from_slice(&height.to_be_bytes());
            inclusions.push(SimulatorInclusion { key, value });
        }
        let packet = SimulatorPacket {
            height,
            state_root: record.block.header.state_root,
            bytes,
            inclusions,
        };
        self.engine
            .prepare_simulator(executed.candidate, packet)
            .map_err(|e| Error::from(e.error))?;
        self.pending = Some(record);
        self.commit_prepared()
    }
    fn commit_prepared(&mut self) -> Result<Header> {
        match self.engine.commit()? {
            Publication::Committed(info) => {
                let record = self.pending.take().ok_or(Error("StorageCorrupt"))?;
                if record.block.header.height != info.snapshot.sequence()
                    || record.block.header.state_root != info.state_root
                {
                    self.health = "storage-fenced";
                    return Err(Error("StorageCorrupt"));
                }
                self.head = record.block.header;
                self.workload = record.workload;
                self.revision = record.revision;
                self.pending_config = None;
                self.observed = self.observed.max(self.head.height);
                self.health = if self.paused { "paused" } else { "running" };
                Ok(self.head)
            }
            Publication::NotCommitted(_) => {
                self.health = "storage-fenced";
                Err(Error("StorageUnavailable"))
            }
            Publication::Indeterminate(_) => {
                self.health = "storage-fenced";
                Err(Error("CommitUncertain"))
            }
        }
    }
    /// Explicit operator/test recovery; only the engine's exact retained retry can publish.
    pub fn retry_pending(&mut self) -> Result<Header> {
        match self.engine.recover_pending()? {
            Recovery::RetryPrepared => self.commit_prepared(),
            Recovery::ObservedCommittedDurabilityUnknown(_) => Err(Error("CommitUncertain")),
            Recovery::Ready => Err(Error("InvalidRequest")),
        }
    }
    pub fn request(&self, r: &arkiv_db::proof::EqRequest) -> Result<arkiv_db::Snapshot> {
        let h = self.header(r.snapshot.height)?;
        if r.snapshot.genesis_hash != self.durable_identity.node_genesis_hash
            || r.snapshot.block_hash != h.hash()
            || r.snapshot.state_root != h.state_root
        {
            return Err(Error("CursorMismatch"));
        }
        crate::retention::state(&self.engine, &self.refusal, h.height)
    }
    pub fn proof(&self, r: &arkiv_db::proof::EqRequest) -> Result<Vec<u8>> {
        Ok(self
            .request(r)?
            .prove_eq(r, proof_limits(), Budget::limit(10_000_000))?
            .bytes)
    }
}
