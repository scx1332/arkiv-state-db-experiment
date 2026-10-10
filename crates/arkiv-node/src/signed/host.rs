//! Signed admission and independent execution; bodies live outside the engine.
use super::{
    Error, Result,
    crypto::Signer,
    history::{BodyRef, History},
    pool::Pool,
    protocol::{SignedBlock, SignedGenesis, SignedHeader, SignedInputs, SignedTx},
};
use crate::{
    BlockOutcomes, Header, Inclusion, RequestIdentity, Scope,
    exec::execute_block_hooked,
    simulator::{
        metadata,
        wire::{FEED_CAP, FeedBlock, Identity, hex, json_bytes, proof_limits},
    },
};
use arkiv_db::{
    Budget, Cost, DecodeError, Decoder, Encoder, OwnerId, StorageError, StorageErrorKind,
    decode_exact,
    durable::{
        DurableEngine, EngineGenesis, Initialization, Options, Publication, Recovery,
        simulator::{SimulatorIdentity, SimulatorInclusion, SimulatorPacket},
    },
    encode,
};
use arkiv_db_storage::versioned::VersionedStore;
use serde_json::{Value as Json, json};
use std::{collections::BTreeMap, sync::Arc};

/// Bounded metadata retained atomically alongside state. No operations or calldata.
#[derive(Clone, Debug)]
pub struct HeaderRecord {
    pub certificate: SignedHeader,
    pub body: BodyRef,
    pub feed: FeedBlock,
}
impl HeaderRecord {
    pub fn bytes(&self) -> Result<Vec<u8>> {
        let mut e = Encoder::new();
        e.fixed(b"ARKSGNR1");
        e.put(&self.certificate);
        e.put(&self.body);
        e.bytes(&json_bytes(&self.feed, FEED_CAP)?);
        Ok(e.into_bytes())
    }
    pub fn from_bytes(bytes: &[u8], genesis: &SignedGenesis, identity: &Identity) -> Result<Self> {
        let parse = || -> std::result::Result<Self, DecodeError> {
            let mut d = Decoder::new(bytes);
            if d.array::<8>()? != *b"ARKSGNR1" {
                return Err(DecodeError::Invalid {
                    kind: "SignedRecord",
                    reason: "version",
                });
            }
            let certificate = d.get()?;
            let body = d.get()?;
            let feed_bytes = d.bytes()?;
            if feed_bytes.len() > FEED_CAP {
                return Err(DecodeError::Invalid {
                    kind: "SignedRecord",
                    reason: "feed cap",
                });
            }
            let feed = serde_json::from_slice(feed_bytes).map_err(|_| DecodeError::Invalid {
                kind: "SignedRecord",
                reason: "feed",
            })?;
            if d.remaining() != 0 {
                return Err(DecodeError::Invalid {
                    kind: "SignedRecord",
                    reason: "trailing bytes",
                });
            }
            Ok(Self {
                certificate,
                body,
                feed,
            })
        };
        let record = parse().map_err(|_| Error("StorageCorrupt"))?;
        record.certificate.authenticate(genesis)?;
        record.feed.validate_digest()?;
        if record.feed.header.validate(genesis.genesis.chain_id)? != record.certificate.header
            || (
                &record.feed.source_id,
                &record.feed.run_id,
                &record.feed.genesis_hash,
                &record.feed.chain_id,
            ) != (
                &identity.source_id,
                &identity.run_id,
                &identity.genesis_hash,
                &identity.chain_id,
            )
            || record.feed.feed_version != 1
            || record.bytes()? != bytes
        {
            return Err(Error("StorageCorrupt"));
        }
        Ok(record)
    }
}
fn engine_genesis(identity: &SimulatorIdentity, genesis: &SignedGenesis) -> EngineGenesis {
    EngineGenesis {
        initial_state: genesis.prefill.as_ref().map(|p| p.state.clone()),
        deployment: identity.deployment(),
        limits: genesis.genesis.limits,
        pricing: genesis.genesis.pricing.clone(),
        max_object_bytes: 8 * 1024 * 1024,
    }
}
fn identity(stored: &SimulatorIdentity, genesis: &SignedGenesis) -> Identity {
    Identity {
        source_id: crate::simulator::host::uuid(&stored.source_id),
        run_id: hex(&stored.run_id)[2..].into(),
        genesis_hash: hex(genesis.hash().as_slice()),
        chain_id: genesis.genesis.chain_id.to_string(),
    }
}
struct Pending {
    record: HeaderRecord,
    block: SignedBlock,
    bytes: Vec<u8>,
    history_saved: bool,
    engine_attempted: bool,
}
pub struct Host {
    pub engine: DurableEngine,
    pub genesis: SignedGenesis,
    pub identity: Identity,
    pub head: Header,
    pub paused: bool,
    pub health: &'static str,
    pub pool: Pool,
    pub prefill_stream: Option<Vec<u8>>,
    history: History,
    signer: Option<Signer>,
    pending: Option<Pending>,
    /// The last state-retention refusal, for API error details.
    pub refusal: crate::retention::Refusal,
}
impl Host {
    #[allow(clippy::too_many_arguments)]
    pub fn create(
        store: Arc<dyn VersionedStore>,
        history: History,
        genesis: SignedGenesis,
        source: [u8; 16],
        certificate: SignedHeader,
        signer: Option<Signer>,
        options: Options,
    ) -> Result<Self> {
        Self::create_with_prefill(
            store,
            history,
            genesis,
            source,
            certificate,
            signer,
            options,
            None,
        )
    }
    #[allow(clippy::too_many_arguments)]
    pub fn create_with_prefill(
        store: Arc<dyn VersionedStore>,
        mut history: History,
        genesis: SignedGenesis,
        source: [u8; 16],
        certificate: SignedHeader,
        signer: Option<Signer>,
        options: Options,
        prefill_stream: Option<Vec<u8>>,
    ) -> Result<Self> {
        genesis.validate()?;
        let candidate = match (&genesis.prefill, &prefill_stream) {
            (Some(_), Some(bytes)) => Some(
                crate::genesis_bundle::verified_candidate(&genesis, bytes)
                    .map_err(|_| Error("InvalidPrefillBundle"))?,
            ),
            (None, None) => None,
            _ => return Err(Error("PrefillBundleRequired")),
        };
        certificate.authenticate(&genesis)?;
        if certificate.header != genesis.header()
            || signer
                .as_ref()
                .is_some_and(|s| s.address().as_bytes() != &genesis.proposer)
        {
            return Err(Error("IdentityMismatch"));
        }
        let durable = SimulatorIdentity {
            source_id: source,
            run_id: genesis.run_id,
            node_genesis_hash: genesis.hash(),
            metadata: encode(&genesis),
        };
        let identity = identity(&durable, &genesis);
        let block = SignedBlock {
            header: certificate.clone(),
            inputs: SignedInputs { txs: vec![] },
            outcomes: BlockOutcomes::default(),
        };
        let body = history.persist(&encode(&block))?;
        let record = HeaderRecord {
            certificate,
            body,
            feed: FeedBlock::new(&identity, &genesis.header(), Default::default())?,
        };
        let packet = SimulatorPacket {
            height: 0,
            state_root: genesis.header().state_root,
            bytes: record.bytes()?,
            inclusions: vec![],
        };
        let initialization = if let Some(candidate) = candidate {
            DurableEngine::initialize_simulator_prefilled(
                store,
                engine_genesis(&durable, &genesis),
                options,
                durable,
                packet,
                candidate,
            )?
        } else {
            DurableEngine::initialize_simulator(
                store,
                engine_genesis(&durable, &genesis),
                options,
                durable,
                packet,
            )?
        };
        let engine = match initialization {
            Initialization::Committed(engine) => engine,
            Initialization::NotCommitted { .. } => return Err(Error("StorageUnavailable")),
            Initialization::Indeterminate { .. } => return Err(Error("CommitUncertain")),
        };
        let pool = Pool::new(&genesis)?;
        Ok(Self {
            head: genesis.header(),
            engine,
            genesis,
            identity,
            paused: true,
            health: "paused",
            pool,
            prefill_stream,
            history,
            signer,
            pending: None,
            refusal: Default::default(),
        })
    }
    pub fn open(
        store: Arc<dyn VersionedStore>,
        history: History,
        expected: &SignedGenesis,
        signer: Option<Signer>,
        options: Options,
    ) -> Result<Self> {
        let durable = DurableEngine::read_simulator_identity(store.as_ref())?;
        let genesis: SignedGenesis =
            decode_exact(&durable.metadata).map_err(|_| Error("IdentityMismatch"))?;
        genesis.validate()?;
        if encode(&genesis) != encode(expected)
            || durable.node_genesis_hash != genesis.hash()
            || durable.run_id != genesis.run_id
            || signer
                .as_ref()
                .is_some_and(|s| s.address().as_bytes() != &genesis.proposer)
        {
            return Err(Error("IdentityMismatch"));
        }
        let identity = identity(&durable, &genesis);
        let engine = DurableEngine::open_simulator(
            store,
            engine_genesis(&durable, &genesis),
            options,
            durable,
        )?;
        let stored = engine.simulator_block(engine.head().sequence())?;
        let record = HeaderRecord::from_bytes(&stored.bytes, &genesis, &identity)?;
        if record.certificate.header.height != stored.info.snapshot.sequence()
            || record.certificate.header.state_root != stored.info.state_root
        {
            return Err(Error("StorageCorrupt"));
        }
        let mut host = Self {
            head: record.certificate.header,
            pool: Pool::new(&genesis)?,
            prefill_stream: None,
            engine,
            genesis,
            identity,
            paused: true,
            health: "paused",
            history,
            signer,
            pending: None,
            refusal: Default::default(),
        };
        host.block(host.head.height)?;
        if host.signer.is_none() {
            host.health = "following";
        }
        Ok(host)
    }
    /// Opens the state after `height`, refusing heights below the state
    /// floor up front with `HistoryPruned` (details in `refusal`).
    pub fn state(&self, height: u64) -> Result<arkiv_db::Snapshot> {
        crate::retention::state(&self.engine, &self.refusal, height)
    }
    /// Refuses state at `height` below the floor without opening it.
    pub fn require_state(&self, height: u64) -> Result<()> {
        crate::retention::require_state(&self.engine, &self.refusal, height)
    }
    /// Refuses receipts/logs of block `height` (they need state `height - 1`).
    pub fn require_receipts(&self, height: u64) -> Result<()> {
        crate::retention::require_receipts(&self.engine, &self.refusal, height)
    }
    pub fn is_producer(&self) -> bool {
        self.signer.is_some()
    }
    pub fn record(&self, height: u64) -> Result<HeaderRecord> {
        if height > self.head.height {
            return Err(Error("BlockNotSynced"));
        }
        let stored = self.engine.simulator_block(height)?;
        let record = HeaderRecord::from_bytes(&stored.bytes, &self.genesis, &self.identity)?;
        if record.certificate.header.height != height
            || record.certificate.header.state_root != stored.info.state_root
        {
            return Err(Error("StorageCorrupt"));
        }
        Ok(record)
    }
    pub fn certificate(&self, height: u64) -> Result<SignedHeader> {
        Ok(self.record(height)?.certificate)
    }
    pub fn feed(&self, height: u64) -> Result<FeedBlock> {
        Ok(self.record(height)?.feed)
    }
    pub fn block(&self, height: u64) -> Result<SignedBlock> {
        let record = self.record(height)?;
        let bytes = self.history.read(&record.body)?;
        let block: SignedBlock = decode_exact(&bytes).map_err(|_| Error("StorageCorrupt"))?;
        block.authenticate(&self.genesis)?;
        if encode(&block) != bytes
            || block.header != record.certificate
            || block.header.header.outcomes_digest != block.outcomes.digest()
            || (height != 0 && block.header.header.inputs_digest != block.inputs.digest())
        {
            return Err(Error("StorageCorrupt"));
        }
        Ok(block)
    }
    pub fn committed_nonce(&self, actor: OwnerId) -> Result<u64> {
        Ok(self
            .genesis
            .accounts
            .resolve(
                actor,
                self.engine
                    .head_snapshot()
                    .try_host_get(&crate::accounts::account_key(actor))?,
            )?
            .nonce)
    }
    pub fn account(&self, height: u64, actor: OwnerId) -> Result<crate::accounts::Account> {
        let snapshot = self.state(height)?;
        Ok(self.genesis.accounts.resolve(
            actor,
            snapshot.try_host_get(&crate::accounts::account_key(actor))?,
        )?)
    }
    pub fn account_response(&self, height: u64, actor: OwnerId, proof: bool) -> Result<Json> {
        let config = &self.genesis.accounts;
        let certificate = self.certificate(height)?;
        let snapshot = self.state(height)?;
        let account = self.account(height, actor)?;
        let burnt = crate::accounts::burned(snapshot.try_host_get(&crate::accounts::BURN_KEY)?)?;
        let initial = config
            .initial_supply()
            .map_err(|_| Error("StorageCorrupt"))?;
        let supply = initial.checked_sub(burnt).ok_or(Error("StorageCorrupt"))?;
        let mut value = json!({"actor":hex(actor.as_bytes()),"height":height.to_string(),"blockHash":hex(certificate.header.hash().as_slice()),"balanceWei":account.balance.to_string(),"nextNonce":account.nonce.to_string(),"gasPriceWei":config.gas_price_wei.to_string(),"initialSupplyWei":initial.to_string(),"burnedWei":burnt.to_string(),"totalSupplyWei":supply.to_string()});
        if height == self.head.height {
            value["pendingReservedWei"] = json!(self.pool.reserved(actor)?.to_string());
        }
        if proof {
            value["accountProof"] = json!(hex(
                &snapshot.prove_host(&crate::accounts::account_key(actor))?
            ));
            value["supplyProof"] = json!(hex(&snapshot.prove_host(&crate::accounts::BURN_KEY)?));
        }
        Ok(value)
    }
    fn inclusion(&self, identity: &RequestIdentity) -> Result<Option<(Inclusion, u64)>> {
        self.engine
            .simulator_inclusion(&encode(identity))?
            .map(|b| decode_exact(&b).map_err(|_| Error("StorageCorrupt")))
            .transpose()
    }
    pub fn request_status(&self, actor: OwnerId, request_id: [u8; 32]) -> Result<Json> {
        let identity = RequestIdentity {
            scope: Scope::Tx,
            chain_id: self.genesis.genesis.chain_id,
            actor,
            request_id,
        };
        if let Some((inclusion, nonce)) = self.inclusion(&identity)? {
            let record = self.record(inclusion.height)?;
            let tx = record
                .feed
                .transactions
                .get(inclusion.index as usize)
                .ok_or(Error("StorageCorrupt"))?;
            let gas_used = tx
                .spent_units
                .parse::<u64>()
                .map_err(|_| Error("StorageCorrupt"))?;
            return Ok(
                json!({"gasUsed":gas_used.to_string(),"gasPriceWei":crate::accounts::GAS_PRICE_WEI.to_string(),"feeChargedWei":crate::accounts::fee(gas_used).to_string(),"status":"included","actor":hex(actor.as_bytes()),"requestId":hex(&request_id),"nonce":nonce.to_string(),"transactionHash":hex(inclusion.digest.as_slice()),"height":inclusion.height.to_string(),"index":inclusion.index,"executionStatus":tx.status,"durability":"durable"}),
            );
        }
        if let Some(tx) = self.pool.pending(actor, request_id) {
            return Ok(
                json!({"status":"pending","actor":hex(actor.as_bytes()),"requestId":hex(&request_id),"transactionHash":hex(tx.hash().as_slice()),"nonce":tx.nonce.to_string(),"durability":"volatile"}),
            );
        }
        Ok(json!({"status":"unknown","actor":hex(actor.as_bytes()),"requestId":hex(&request_id)}))
    }
    pub fn submit(&mut self, tx: SignedTx) -> Result<Json> {
        if !self.is_producer() {
            return Err(Error("ProducerOnly"));
        }
        if self.pending.is_some()
            || self.health == "storage-fenced"
            || self.health == "chain-conflict"
        {
            return Err(Error("CommitUncertain"));
        }
        tx.validate(&self.genesis)?;
        let identity = tx.identity();
        if let Some((found, _)) = self.inclusion(&identity)? {
            if found.digest != tx.hash() {
                return Err(Error("RequestConflict"));
            }
            return self.request_status(identity.actor, identity.request_id);
        }
        if self.head.height == 0 && self.genesis.prefill.is_none() {
            return Err(Error("ChainInitializing"));
        }
        let next = self.committed_nonce(identity.actor)?;
        let balance = self
            .account(self.head.height, identity.actor)
            .map(|a| a.balance)?;
        self.pool.insert_funded(tx, next, balance)?;
        self.request_status(identity.actor, identity.request_id)
    }
    pub fn produce(&mut self) -> Result<SignedHeader> {
        if !self.is_producer() {
            return Err(Error("ProducerOnly"));
        }
        if self.pending.is_some() {
            return Err(Error("CommitUncertain"));
        }
        if self.head.height == 0 && self.genesis.prefill.is_none() {
            let signer = self.signer.as_ref().ok_or(Error("ProducerOnly"))?;
            let tx = crate::TxEnvelope {
                chain_id: self.genesis.genesis.chain_id,
                actor: signer.address(),
                request_id: [0; 32],
                tx_budget: self.genesis.genesis.params.user_budget,
                ops: vec![crate::TxOp::CreateNamespace {
                    name: arkiv_db::NamespaceName::new("entities")
                        .map_err(|_| Error("InvalidRequest"))?,
                }],
            };
            let signed = SignedTx::sign(&self.genesis, 0, tx, signer)?;
            let balance = self.account(0, signer.address())?.balance;
            self.pool.insert_funded(signed, 0, balance)?;
        }
        let inputs = self.pool.select(|actor| self.committed_nonce(actor))?;
        self.execute_publish(inputs, None)
    }
    pub fn import(&mut self, block: SignedBlock) -> Result<SignedHeader> {
        if self.is_producer() {
            return Err(Error("FollowerOnly"));
        }
        self.observe_certificate(&block.header)?;
        if self.health == "chain-conflict" {
            return Err(Error("ChainConflict"));
        }
        if self.pending.is_some() {
            return Err(Error("CommitUncertain"));
        }
        let height = block.header.header.height;
        if height <= self.head.height {
            let prior = self.certificate(height)?;
            if prior.header != block.header.header {
                self.health = "chain-conflict";
                let evidence = encode(&(prior, block.header));
                self.history.freeze_conflict(&evidence)?;
                return Err(Error("ChainConflict"));
            }
            if self.block(height)?.inputs != block.inputs
                || self.block(height)?.outcomes != block.outcomes
            {
                return Err(Error("ChainConflict"));
            }
            return self.certificate(self.head.height);
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
        block.authenticate(&self.genesis)?;
        self.execute_publish(block.inputs.clone(), Some(&block))
    }
    /// A sampled authentic header at a retained height can establish
    /// equivocation even when no new block body is advertised.
    pub fn observe_certificate(&mut self, certificate: &SignedHeader) -> Result<()> {
        certificate.authenticate(&self.genesis)?;
        if self.health == "chain-conflict" {
            return Err(Error("ChainConflict"));
        }
        let previous = match self.history.remember_header(certificate) {
            Ok(previous) => previous,
            Err(error) => {
                self.health = "storage-fenced";
                return Err(error);
            }
        };
        if let Some(previous) = previous {
            self.observe_pair(&previous, certificate)?;
        }
        if certificate.header.height <= self.head.height {
            let prior = self.certificate(certificate.header.height)?;
            self.observe_pair(&prior, certificate)?;
        }
        Ok(())
    }
    /// Equivocation can also be established before either header is imported.
    pub fn observe_pair(&mut self, first: &SignedHeader, second: &SignedHeader) -> Result<()> {
        first.authenticate(&self.genesis)?;
        second.authenticate(&self.genesis)?;
        if self.health == "chain-conflict" {
            return Err(Error("ChainConflict"));
        }
        if first.header.height == second.header.height && first.header != second.header {
            self.health = "chain-conflict";
            self.history
                .freeze_conflict(&encode(&(first.clone(), second.clone())))?;
            return Err(Error("ChainConflict"));
        }
        Ok(())
    }
    fn execute_publish(
        &mut self,
        inputs: SignedInputs,
        expected: Option<&SignedBlock>,
    ) -> Result<SignedHeader> {
        if self.health == "storage-fenced" || self.health == "chain-conflict" {
            return Err(Error("CommitUncertain"));
        }
        let height = self
            .head
            .height
            .checked_add(1)
            .ok_or(Error("CapacityReached"))?;
        inputs.validate(&self.genesis, height)?;
        let mut next = BTreeMap::new();
        for tx in &inputs.txs {
            if self.inclusion(&tx.identity())?.is_some() {
                return Err(Error("RequestConflict"));
            }
            let nonce = match next.get(&tx.tx.actor) {
                Some(n) => *n,
                None => self.committed_nonce(tx.tx.actor)?,
            };
            if tx.nonce != nonce || nonce == u64::MAX {
                return Err(Error("InvalidNonce"));
            }
            next.insert(tx.tx.actor, nonce + 1);
        }
        let spec = self.genesis.spec()?;
        let branch = self.engine.begin(
            self.engine.head(),
            Some(Cost::new(spec.params().block_budget)),
        )?;
        let native = inputs.native();
        let mut captured = None;
        let mut executed = execute_block_hooked(
            &spec,
            branch,
            &self.head,
            height,
            self.genesis.timestamp(height)?,
            &native,
            Some(&self.genesis.accounts),
            |branch, index, tx| {
                let mut account = self.genesis.accounts.read(branch, tx.actor)?;
                if account.nonce != inputs.txs[index].nonce || account.nonce == u64::MAX {
                    return Err(StorageError::new(
                        StorageErrorKind::Corrupt,
                        "account nonce",
                    ));
                }
                account.nonce += 1;
                crate::accounts::put(branch, tx.actor, account)
            },
            |branch, outcomes, expired| {
                captured = Some(metadata::capture(branch, &native, outcomes, expired)?);
                Ok(())
            },
        )
        .map_err(|e| match e {
            crate::ExecError::Storage(e) => Error::from(e),
            _ => Error("InvalidBlock"),
        })?;
        executed.header.inputs_digest = inputs.digest();
        if height == 1
            && self.genesis.prefill.is_none()
            && !matches!(
                executed.outcomes.txs.first().map(|tx| tx.status),
                Some(crate::TxStatus::Committed)
            )
        {
            return Err(Error("BootstrapFailed"));
        }
        let certificate = match expected {
            Some(block) => block.header.clone(),
            None => SignedHeader::sign(
                &self.genesis,
                executed.header,
                self.signer.as_ref().ok_or(Error("ProducerOnly"))?,
            )?,
        };
        if certificate.header != executed.header {
            return Err(Error("ChainConflict"));
        }
        let block = SignedBlock {
            header: certificate.clone(),
            inputs,
            outcomes: executed.outcomes,
        };
        if expected.is_some_and(|e| e.outcomes != block.outcomes) {
            return Err(Error("ChainConflict"));
        }
        let body_bytes = encode(&block);
        if body_bytes.len() > self.genesis.limits.max_body_bytes as usize {
            return Err(Error("CapacityReached"));
        }
        let mut metadata = captured.ok_or(Error("StorageCorrupt"))?;
        for (tx, signed) in metadata.transactions.iter_mut().zip(&block.inputs.txs) {
            tx.digest = hex(signed.hash().as_slice());
        }
        // History references are computed without publication; persist happens after prepare.
        let body = self.history.reference(&body_bytes)?;
        let record = HeaderRecord {
            certificate,
            body,
            feed: FeedBlock::new(&self.identity, &executed.header, metadata)?,
        };
        let bytes = record.bytes()?;
        if bytes.len() > 8 * 1024 * 1024 - 116 || body_bytes.len() * 2 + 4096 > 8 * 1024 * 1024 {
            return Err(Error("CapacityReached"));
        }
        let inclusions = block
            .inputs
            .txs
            .iter()
            .enumerate()
            .map(|(index, tx)| SimulatorInclusion {
                key: encode(&tx.identity()),
                value: encode(&(
                    Inclusion {
                        height,
                        scope: Scope::Tx,
                        index: index as u32,
                        digest: tx.hash(),
                    },
                    tx.nonce,
                )),
            })
            .collect();
        let packet = SimulatorPacket {
            height,
            state_root: executed.header.state_root,
            bytes,
            inclusions,
        };
        self.engine
            .prepare_simulator(executed.candidate, packet)
            .map_err(|e| Error::from(e.error))?;
        self.pending = Some(Pending {
            record,
            block,
            bytes: body_bytes,
            history_saved: false,
            engine_attempted: false,
        });
        self.finish_pending(false)
    }
    fn finish_pending(&mut self, retry: bool) -> Result<SignedHeader> {
        if self.health == "chain-conflict" {
            return Err(Error("ChainConflict"));
        }
        let pending = self.pending.as_mut().ok_or(Error("InvalidRequest"))?;
        if !pending.history_saved {
            let result = if retry {
                self.history.retry_exact(&pending.bytes)
            } else {
                self.history.persist(&pending.bytes)
            };
            match result {
                Ok(reference) if reference == pending.record.body => pending.history_saved = true,
                Ok(_) => {
                    self.health = "storage-fenced";
                    return Err(Error("StorageCorrupt"));
                }
                Err(e) => {
                    self.health = "storage-fenced";
                    return Err(e);
                }
            }
        }
        if pending.engine_attempted {
            match self.engine.recover_pending()? {
                Recovery::RetryPrepared => {}
                _ => return Err(Error("CommitUncertain")),
            }
        }
        pending.engine_attempted = true;
        match self.engine.commit()? {
            Publication::Committed(info) => {
                let pending = self.pending.take().ok_or(Error("StorageCorrupt"))?;
                if info.snapshot.sequence() != pending.record.certificate.header.height
                    || info.state_root != pending.record.certificate.header.state_root
                {
                    self.health = "storage-fenced";
                    return Err(Error("StorageCorrupt"));
                }
                self.pool.remove_committed(&pending.block.inputs);
                self.head = pending.record.certificate.header;
                self.health = if self.is_producer() {
                    if self.paused { "paused" } else { "running" }
                } else {
                    "following"
                };
                Ok(pending.record.certificate)
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
    pub fn retry_pending(&mut self) -> Result<SignedHeader> {
        self.finish_pending(true)
    }
    pub fn proof(&self, request: &arkiv_db::proof::EqRequest) -> Result<Vec<u8>> {
        let header = self.certificate(request.snapshot.height)?.header;
        if request.snapshot.genesis_hash != self.genesis.hash()
            || request.snapshot.block_hash != header.hash()
            || request.snapshot.state_root != header.state_root
        {
            return Err(Error("CursorMismatch"));
        }
        Ok(self
            .state(header.height)?
            .prove_eq(request, proof_limits(), Budget::limit(10_000_000))?
            .bytes)
    }

    pub fn range_proof(&self, request: &arkiv_db::proof::RangeRequest) -> Result<Vec<u8>> {
        let header = self.certificate(request.snapshot.height)?.header;
        if request.snapshot.genesis_hash != self.genesis.hash()
            || request.snapshot.block_hash != header.hash()
            || request.snapshot.state_root != header.state_root
        {
            return Err(Error("InvalidRequest"));
        }
        Ok(self
            .state(header.height)?
            .prove_range(
                request,
                crate::simulator::wire::range_proof_limits()?,
                Budget::limit(10_000_000),
            )?
            .bytes)
    }
}
