//! Signed admission and independent execution; bodies live outside the engine.

use super::{
    Error, ErrorCode, Result,
    crypto::Signer,
    history::{BodyRef, History},
    pool::{DropReason, Pool},
    protocol::{
        REQUEST_IDENTITY_WINDOW, SignedBlock, SignedGenesis, SignedHeader, SignedInputs, SignedTx,
        ValidTx,
    },
};
use crate::health::Health;
use crate::{
    BlockOutcomes, Header, Inclusion, RequestIdentity, Scope,
    exec::{BlockEnv, DeferReason, Mode, execute_block_hooked},
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
use std::{cell::RefCell, collections::BTreeMap, sync::Arc};

/// Bounded metadata retained atomically alongside state. No operations or calldata.
#[derive(Clone, Debug)]
pub struct HeaderRecord {
    pub certificate: SignedHeader,
    pub body: BodyRef,
    pub feed: FeedBlock,
}

impl HeaderRecord {
    /// The certificate and body reference at the start of a stored record,
    /// without the feed or any authentication: offline migrations.
    pub fn parts(bytes: &[u8]) -> Result<(SignedHeader, BodyRef)> {
        let parse = || -> std::result::Result<(SignedHeader, BodyRef), DecodeError> {
            let mut d = Decoder::new(bytes);
            if d.array::<8>()? != *b"ARKSGNR1" {
                return Err(DecodeError::Invalid {
                    kind: "SignedRecord",
                    reason: "version",
                });
            }
            Ok((d.get()?, d.get()?))
        };
        parse().map_err(|_| ErrorCode::StorageCorrupt.into())
    }

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
        let record = parse().map_err(|_| ErrorCode::StorageCorrupt)?;
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
            return Err(ErrorCode::StorageCorrupt.into());
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

/// The engine options of a signed node: per-height records expire with the
/// state window (archive keeps them), and an inclusion older than the
/// request identity window may be replaced by a new block (see
/// [`REQUEST_IDENTITY_WINDOW`]).
fn signed_options(mut options: Options) -> Options {
    options.inclusion_window = Some(REQUEST_IDENTITY_WINDOW);
    options.prune_records = options.retention.window().is_some();
    options
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
    pub health: Health,
    pub pool: Pool,
    prefill_stream: Option<Vec<u8>>,
    prefill_file: Option<std::path::PathBuf>,
    history: History,
    signer: Option<Signer>,
    pending: Option<Pending>,
    /// Blocks this host has executed for production or import (simulations
    /// excluded): production executes each candidate once.
    pub block_executions: u64,
    /// Certificates [`Self::certificate`] authenticated, for up to
    /// [`CERTIFICATE_CACHE`] of the highest heights read. Header reads and
    /// proof captures run under the node lock, where re-authenticating a
    /// stored record (signature recovery, genesis validation) would hold it
    /// for milliseconds; a stored certificate never changes.
    certificates: RefCell<BTreeMap<u64, SignedHeader>>,
    /// Blocks [`Self::block`] read and authenticated, for up to
    /// [`BLOCK_CACHE`] of the highest heights read. A receipt poll reads its
    /// block under the node lock; re-authenticating it there (a signature
    /// recovery per transaction, every digest) on each poll starves
    /// production. A stored block never changes.
    blocks: RefCell<BTreeMap<u64, std::sync::Arc<SignedBlock>>>,
    /// Bodies expire from `history.redb` with the node's records (see
    /// [`Self::prune_bodies`]); otherwise they are kept for followers.
    prune_bodies: bool,
    /// The snapshot pin ([`Self::set_body_pin`]): bodies from this height on
    /// never expire.
    body_pin: Option<u64>,
    /// Wall clock in Unix milliseconds for the timestamps this host stamps
    /// ([`SignedGenesis::next_timestamp`]); replaceable in tests.
    clock: fn() -> u64,
}

/// The system clock in Unix milliseconds (0 before 1970).
pub fn system_clock_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Heights of bodies [`Host`] deletes at least per block, on top of a
/// sixteenth of the backlog, once bodies expire.
const BODY_DELETE_BUDGET: u64 = 64;

/// Heights whose certificates a [`Host`] keeps authenticated.
const CERTIFICATE_CACHE: usize = 256;
/// Heights whose blocks a [`Host`] keeps authenticated: at most
/// `BLOCK_CACHE × INPUT_CAP` (about 24 MiB) of inputs.
const BLOCK_CACHE: usize = 32;

/// The chain a node creates its height-zero database for.
pub struct Launch {
    pub history: History,
    pub genesis: SignedGenesis,
    pub source: [u8; 16],
    /// The proposer's certificate of the genesis header.
    pub certificate: SignedHeader,
    pub signer: Option<Signer>,
}

/// The entity prefill a genesis launches with.
pub enum Prefill<'a> {
    None,
    /// The small canonical stream, verified here and kept to serve followers.
    Stream(Vec<u8>),
    /// A verified, privately staged store, installed without materializing
    /// its trie, and the canonical stream file it was built from.
    Staged {
        engine: &'a DurableEngine,
        stream_file: std::path::PathBuf,
    },
}

/// What the engine publishes at height zero beside the state.
struct GenesisRecord {
    durable: SimulatorIdentity,
    identity: Identity,
    packet: SimulatorPacket,
}

impl Launch {
    /// Authenticates the certificate and the signer against the genesis,
    /// persists the empty genesis body and builds the height-zero record.
    fn genesis_record(&mut self) -> Result<GenesisRecord> {
        let genesis = &self.genesis;
        self.certificate.authenticate(genesis)?;
        if self.certificate.header != genesis.header()
            || self
                .signer
                .as_ref()
                .is_some_and(|s| s.address().as_bytes() != &genesis.proposer)
        {
            return Err(ErrorCode::IdentityMismatch.into());
        }
        let durable = SimulatorIdentity {
            source_id: self.source,
            run_id: genesis.run_id,
            node_genesis_hash: genesis.hash(),
            metadata: encode(genesis),
        };
        let identity = identity(&durable, genesis);
        let block = SignedBlock {
            header: self.certificate.clone(),
            inputs: SignedInputs { txs: vec![] },
            outcomes: BlockOutcomes::default(),
        };
        let record = HeaderRecord {
            certificate: self.certificate.clone(),
            body: self.history.persist_block(&block.header, &encode(&block))?,
            feed: FeedBlock::new(&identity, &genesis.header(), Default::default())?,
        };
        let packet = SimulatorPacket {
            height: 0,
            state_root: genesis.header().state_root,
            bytes: record.bytes()?,
            inclusions: vec![],
        };
        Ok(GenesisRecord {
            durable,
            identity,
            packet,
        })
    }
}

impl Host {
    /// Creates the height-zero database of `launch` in `store`.
    pub fn create(
        store: Arc<dyn VersionedStore>,
        options: Options,
        mut launch: Launch,
        prefill: Prefill<'_>,
    ) -> Result<Self> {
        let genesis = &launch.genesis;
        genesis.validate()?;
        let candidate = match (&genesis.prefill, &prefill) {
            (Some(_), Prefill::Stream(bytes)) => Some(
                crate::genesis_bundle::verified_candidate(genesis, bytes)
                    .map_err(|_| ErrorCode::InvalidPrefillBundle)?,
            ),
            (None, Prefill::None) => None,
            (Some(p), Prefill::Staged { engine, .. })
                if p.is_streaming() && engine.head_snapshot().state_header() == p.state =>
            {
                None
            }
            (_, Prefill::Staged { .. }) => return Err(ErrorCode::InvalidPrefillBundle.into()),
            _ => return Err(ErrorCode::PrefillBundleRequired.into()),
        };
        let GenesisRecord {
            durable,
            identity,
            packet,
        } = launch.genesis_record()?;
        let engine_genesis = engine_genesis(&durable, &launch.genesis);
        let options = signed_options(options);
        let initialization = match (&prefill, candidate) {
            (Prefill::Staged { engine, .. }, _) => engine.initialize_simulator_from_staged(
                store,
                engine_genesis,
                options,
                durable,
                packet,
            )?,
            (_, Some(candidate)) => DurableEngine::initialize_simulator_prefilled(
                store,
                engine_genesis,
                options,
                durable,
                packet,
                candidate,
            )?,
            (_, None) => DurableEngine::initialize_simulator(
                store,
                engine_genesis,
                options,
                durable,
                packet,
            )?,
        };
        let engine = match initialization {
            Initialization::Committed(engine) => engine,
            Initialization::NotCommitted { .. } => return Err(ErrorCode::StorageUnavailable.into()),
            Initialization::Indeterminate { .. } => return Err(ErrorCode::CommitUncertain.into()),
        };
        let head = launch.genesis.header();
        let host = Self::assemble(
            engine,
            launch.genesis,
            identity,
            head,
            launch.history,
            launch.signer,
        )?;
        Ok(match prefill {
            Prefill::None => host,
            Prefill::Stream(bytes) => host.serving_prefill_stream(bytes),
            Prefill::Staged { stream_file, .. } => host.serving_prefill_file(stream_file),
        })
    }

    /// A paused host at `head` that serves no prefill.
    fn assemble(
        engine: DurableEngine,
        genesis: SignedGenesis,
        identity: Identity,
        head: Header,
        history: History,
        signer: Option<Signer>,
    ) -> Result<Self> {
        Ok(Self {
            head,
            pool: Pool::new(&genesis)?,
            engine,
            genesis,
            identity,
            paused: true,
            health: Health::Paused,
            prefill_stream: None,
            prefill_file: None,
            history,
            signer,
            pending: None,
            block_executions: 0,
            certificates: Default::default(),
            blocks: Default::default(),
            prune_bodies: false,
            body_pin: None,
            clock: system_clock_ms,
        })
    }

    /// Replaces the wall clock the producer stamps blocks with (Unix ms).
    pub fn set_clock(&mut self, clock: fn() -> u64) {
        self.clock = clock;
    }

    /// The host's wall clock in Unix milliseconds.
    pub fn now_ms(&self) -> u64 {
        (self.clock)()
    }

    /// Let block bodies (and the history entries, height index rows and
    /// observed headers of their heights) expire from `history.redb` with
    /// the node's own per-height records. A node that does so can no longer
    /// serve the expired blocks or headers to followers and light clients
    /// that start from genesis. Off by default.
    pub fn prune_bodies(&mut self, on: bool) -> Result<()> {
        self.prune_bodies = on;
        self.expire_bodies()
    }

    pub fn prunes_bodies(&self) -> bool {
        self.prune_bodies
    }

    /// Pins the bodies from `height` on: with a pin, bodies expire only
    /// below it, however far the record floor has moved. A node that made a
    /// snapshot at `height` can then feed the nodes installed from it
    /// (see [`super::snapshot`]). A pin does not bring back a body that was
    /// already deleted, and changes nothing while bodies are kept
    /// ([`Self::prune_bodies`] off). `None` removes the pin; the bodies
    /// below the record floor then expire with the following blocks.
    pub fn set_body_pin(&mut self, height: Option<u64>) {
        self.body_pin = height;
    }

    /// The snapshot pin, if any ([`Self::set_body_pin`]).
    pub fn body_pin(&self) -> Option<u64> {
        self.body_pin
    }

    /// The lowest height (above zero) whose node records (feed, block,
    /// inclusions) are kept; zero when none expired.
    pub fn record_floor(&self) -> u64 {
        self.engine.record_floor().floor
    }

    /// Whether the node records of `height` expired.
    pub fn record_expired(&self, height: u64) -> bool {
        self.engine.record_floor().expired(height)
    }

    /// How far `history.redb` bodies are deleted.
    pub fn history_pruned(&self) -> Result<super::history::HistoryPruned> {
        self.history.pruned()
    }

    /// Deletes bodies below the record floor, when bodies expire, and never
    /// at or above the snapshot pin.
    fn expire_bodies(&mut self) -> Result<()> {
        if !self.prune_bodies {
            return Ok(());
        }
        // The certificate just below the floor stays: a reader that is
        // exactly at the floor checks its parent hash against it.
        let floor = self.engine.record_floor().floor.saturating_sub(1);
        // The pinned block itself stays too: a node installed from the
        // snapshot names it as its head when it asks for the next one.
        let floor = self.body_pin.map_or(floor, |pin| floor.min(pin));
        self.history.prune_below(floor, BODY_DELETE_BUDGET)?;
        Ok(())
    }

    /// Serves the small canonical prefill stream to followers.
    pub fn serving_prefill_stream(mut self, stream: Vec<u8>) -> Self {
        self.prefill_stream = Some(stream);
        self
    }

    /// Serves the verified canonical prefill stream file to followers in
    /// chunks; it is never loaded wholesale.
    pub fn serving_prefill_file(mut self, file: std::path::PathBuf) -> Self {
        self.prefill_file = Some(file);
        self
    }

    pub fn prefill_stream(&self) -> Option<&[u8]> {
        self.prefill_stream.as_deref()
    }

    pub fn prefill_file(&self) -> Option<&std::path::Path> {
        self.prefill_file.as_deref()
    }

    /// Relabel a verified, privately staged genesis store in place as this
    /// node's height-zero database instead of copying it (see
    /// [`DurableEngine::adopt_staged_simulator`]). The staged engine is
    /// consumed. On success its store holds the same height-zero record that
    /// [`Self::create`] would publish from it; the caller checks it with
    /// [`Self::check_adopted`], moves its file into place and opens it with
    /// [`Self::open`]. `threads` workers share the verification reads.
    pub fn adopt_staged(
        launch: &mut Launch,
        options: Options,
        staged: DurableEngine,
        threads: usize,
    ) -> Result<()> {
        launch.genesis.validate()?;
        // The staged header binds the store to the signed launch; the caller
        // selects this path only for a streaming prefill.
        if staged.head_snapshot().state_header()
            != launch
                .genesis
                .prefill
                .as_ref()
                .ok_or(ErrorCode::InvalidPrefillBundle)?
                .state
        {
            return Err(ErrorCode::InvalidPrefillBundle.into());
        }
        let GenesisRecord {
            durable, packet, ..
        } = launch.genesis_record()?;
        let outcome = staged.adopt_staged_simulator(
            engine_genesis(&durable, &launch.genesis),
            signed_options(options),
            durable,
            packet,
            threads,
        );
        // The node error code alone would hide why a store was refused.
        let failure = match &outcome {
            Ok(Publication::Committed(_)) => None,
            Ok(Publication::NotCommitted(e) | Publication::Indeterminate(e)) => Some(e.to_string()),
            Err(e) => Some(e.to_string()),
        };
        if let Some(error) = failure {
            eprintln!(
                "{}",
                json!({"event": "prefill_genesis_adopt", "stage": "failed", "error": error})
            );
        }
        match outcome? {
            Publication::Committed(_) => Ok(()),
            Publication::NotCommitted(_) => Err(ErrorCode::StorageUnavailable.into()),
            Publication::Indeterminate(_) => Err(ErrorCode::CommitUncertain.into()),
        }
    }

    /// Checks that `store` is exactly the height-zero database that
    /// [`Self::adopt_staged`] makes for `genesis` and `source`: the simulator
    /// identity, engine genesis, head manifest and the signed genesis record.
    /// Reads a bounded number of records and no trie objects.
    pub fn check_adopted(
        store: Arc<dyn VersionedStore>,
        genesis: &SignedGenesis,
        source: [u8; 16],
        options: Options,
    ) -> Result<()> {
        genesis.validate()?;
        let expected = SimulatorIdentity {
            source_id: source,
            run_id: genesis.run_id,
            node_genesis_hash: genesis.hash(),
            metadata: encode(genesis),
        };
        if DurableEngine::read_simulator_identity(store.as_ref())? != expected {
            return Err(ErrorCode::IdentityMismatch.into());
        }
        let identity = identity(&expected, genesis);
        let engine = DurableEngine::open_simulator(
            store,
            engine_genesis(&expected, genesis),
            signed_options(options),
            expected,
        )?;
        let stored = engine.simulator_block(0)?;
        let record = HeaderRecord::from_bytes(&stored.bytes, genesis, &identity)?;
        if engine.head().sequence() != 0
            || record.certificate.header != genesis.header()
            || stored.info.state_root != genesis.header().state_root
        {
            return Err(ErrorCode::StorageCorrupt.into());
        }
        Ok(())
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
            decode_exact(&durable.metadata).map_err(|_| ErrorCode::IdentityMismatch)?;
        genesis.validate()?;
        if encode(&genesis) != encode(expected)
            || durable.node_genesis_hash != genesis.hash()
            || durable.run_id != genesis.run_id
            || signer
                .as_ref()
                .is_some_and(|s| s.address().as_bytes() != &genesis.proposer)
        {
            return Err(ErrorCode::IdentityMismatch.into());
        }
        let identity = identity(&durable, &genesis);
        let engine = DurableEngine::open_simulator(
            store,
            engine_genesis(&durable, &genesis),
            signed_options(options),
            durable,
        )?;
        let stored = engine.simulator_block(engine.head().sequence())?;
        let record = HeaderRecord::from_bytes(&stored.bytes, &genesis, &identity)?;
        if record.certificate.header.height != stored.info.snapshot.sequence()
            || record.certificate.header.state_root != stored.info.state_root
        {
            return Err(ErrorCode::StorageCorrupt.into());
        }
        let head = record.certificate.header;
        let mut host = Self::assemble(engine, genesis, identity, head, history, signer)?;
        host.block(host.head.height)?;
        if host.signer.is_none() {
            host.health = Health::Following;
        }
        Ok(host)
    }

    /// Opens the state after `height`, refusing heights below the state
    /// floor up front with `HistoryPruned`.
    pub fn state(&self, height: u64) -> Result<arkiv_db::Snapshot> {
        crate::retention::state(&self.engine, height)
    }

    /// Refuses state at `height` below the floor without opening it.
    pub fn require_state(&self, height: u64) -> Result<()> {
        crate::retention::require_state(&self.engine, height)
    }

    /// Refuses receipts/logs of block `height` (they need state `height - 1`).
    pub fn require_receipts(&self, height: u64) -> Result<()> {
        crate::retention::require_receipts(&self.engine, height)
    }

    pub fn is_producer(&self) -> bool {
        self.signer.is_some()
    }

    pub fn record(&self, height: u64) -> Result<HeaderRecord> {
        if height > self.head.height {
            return Err(ErrorCode::BlockNotSynced.into());
        }
        let stored = self.engine.simulator_block(height)?;
        let record = HeaderRecord::from_bytes(&stored.bytes, &self.genesis, &self.identity)?;
        if record.certificate.header.height != height
            || record.certificate.header.state_root != stored.info.state_root
        {
            return Err(ErrorCode::StorageCorrupt.into());
        }
        Ok(record)
    }

    pub fn certificate(&self, height: u64) -> Result<SignedHeader> {
        if self.record_expired(height) {
            return Err(self.records_pruned(height));
        }
        if height <= self.head.height
            && let Some(certificate) = self.certificates.borrow().get(&height)
        {
            return Ok(certificate.clone());
        }
        let certificate = self.record(height)?.certificate;
        let mut cache = self.certificates.borrow_mut();
        cache.insert(height, certificate.clone());
        if cache.len() > CERTIFICATE_CACHE {
            cache.pop_first();
        }
        Ok(certificate)
    }

    pub fn feed(&self, height: u64) -> Result<FeedBlock> {
        Ok(self.record(height)?.feed)
    }

    pub fn block(&self, height: u64) -> Result<SignedBlock> {
        Ok(SignedBlock::clone(&*self.shared_block(height)?))
    }

    /// [`Self::block`] without the copy: the authenticated block, shared
    /// with the cache.
    pub fn shared_block(&self, height: u64) -> Result<std::sync::Arc<SignedBlock>> {
        if self.record_expired(height) {
            return Err(self.records_pruned(height));
        }
        if height <= self.head.height
            && let Some(block) = self.blocks.borrow().get(&height)
        {
            return Ok(block.clone());
        }
        let block = std::sync::Arc::new(self.read_block(height)?);
        let mut cache = self.blocks.borrow_mut();
        cache.insert(height, block.clone());
        if cache.len() > BLOCK_CACHE {
            cache.pop_first();
        }
        Ok(block)
    }

    fn read_block(&self, height: u64) -> Result<SignedBlock> {
        let record = self.record(height)?;
        let bytes = self.history.read(&record.body)?;
        let block: SignedBlock = decode_exact(&bytes).map_err(|e| {
            ErrorCode::StorageCorrupt.caused_by(format_args!("block {height}: {e}"))
        })?;
        block.authenticate(&self.genesis)?;
        if encode(&block) != bytes
            || block.header != record.certificate
            || block.header.header.outcomes_digest != block.outcomes.digest()
            || (height != 0 && block.header.header.inputs_digest != block.inputs.digest())
        {
            return Err(ErrorCode::StorageCorrupt.into());
        }
        Ok(block)
    }

    /// The certificate of `height` for chain sync (headers, followers,
    /// equivocation checks): from the node records inside the window, and
    /// from the `history.redb` height index below it while bodies are
    /// kept. `HistoryPruned` once the body expired too.
    pub fn chain_certificate(&self, height: u64) -> Result<SignedHeader> {
        if !self.record_expired(height) {
            return self.certificate(height);
        }
        let indexed = self
            .history
            .indexed(height)?
            .ok_or_else(|| self.records_pruned(height))?;
        indexed
            .certificate
            .authenticate(&self.genesis)
            .map_err(|_| ErrorCode::StorageCorrupt)?;
        Ok(indexed.certificate)
    }

    /// The block of `height` for replication: [`Self::block`] inside the
    /// window, and the kept body (authenticated against its indexed
    /// certificate) below it.
    pub fn replication_block(&self, height: u64) -> Result<SignedBlock> {
        if !self.record_expired(height) {
            return self.block(height);
        }
        let (indexed, bytes) = self
            .history
            .read_height(height)?
            .ok_or_else(|| self.records_pruned(height))?;
        let block: SignedBlock = decode_exact(&bytes).map_err(|e| {
            ErrorCode::StorageCorrupt.caused_by(format_args!("block {height}: {e}"))
        })?;
        block.authenticate(&self.genesis)?;
        if encode(&block) != bytes
            || block.header != indexed.certificate
            || block.header.header.height != height
            || block.header.header.outcomes_digest != block.outcomes.digest()
            || block.header.header.inputs_digest != block.inputs.digest()
        {
            return Err(ErrorCode::StorageCorrupt.into());
        }
        Ok(block)
    }

    /// The stored canonical bytes of block `height` for public sync
    /// (`GET blocks`), read through the `history.redb` height index at
    /// every height whose body is kept. They are not authenticated again
    /// here: a follower authenticates and re-executes every block it
    /// imports, and a block served to many followers must not cost a
    /// signature recovery per transaction under the node lock.
    /// `HistoryPruned` once the body expired.
    pub fn block_bytes(&self, height: u64) -> Result<Vec<u8>> {
        if height > self.head.height {
            return Err(ErrorCode::BlockNotSynced.into());
        }
        let (indexed, bytes) = self
            .history
            .read_height(height)?
            .ok_or_else(|| self.records_pruned(height))?;
        if indexed.certificate.header.height != height {
            return Err(ErrorCode::StorageCorrupt.into());
        }
        Ok(bytes)
    }

    /// The block of `height` for reads of its body alone (its transactions
    /// and their count): [`Self::shared_block`] inside the record window,
    /// and below it the kept body ([`Self::replication_block`]), while
    /// `--body-retention` keeps bodies. `HistoryPruned` once the body
    /// expired too. Reads that also need state (logs, receipts,
    /// `logsBloom`) keep their own floors.
    pub fn body_block(&self, height: u64) -> Result<std::sync::Arc<SignedBlock>> {
        if !self.record_expired(height) {
            return self.shared_block(height);
        }
        Ok(std::sync::Arc::new(self.replication_block(height)?))
    }

    fn records_pruned(&self, height: u64) -> Error {
        Error::pruned(height, self.record_floor())
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
            .map_err(|_| ErrorCode::StorageCorrupt)?;
        let supply = initial
            .checked_sub(burnt)
            .ok_or(ErrorCode::StorageCorrupt)?;
        let mut value = json!({
            "actor": hex(actor.as_bytes()),
            "height": height.to_string(),
            "blockHash": hex(certificate.header.hash().as_slice()),
            "balanceWei": account.balance.to_string(),
            "nextNonce": account.nonce.to_string(),
            "gasPriceWei": config.gas_price_wei.to_string(),
            "initialSupplyWei": initial.to_string(),
            "burnedWei": burnt.to_string(),
            "totalSupplyWei": supply.to_string(),
        });
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
            .map(|b| decode_exact(&b).map_err(|_| Error::from(ErrorCode::StorageCorrupt)))
            .transpose()
    }

    /// The inclusion of `identity` that blocks it from the block at
    /// `height`: one of the previous [`REQUEST_IDENTITY_WINDOW`] blocks.
    fn recent_inclusion(
        &self,
        identity: &RequestIdentity,
        height: u64,
    ) -> Result<Option<(Inclusion, u64)>> {
        Ok(self
            .inclusion(identity)?
            .filter(|(found, _)| found.height.saturating_add(REQUEST_IDENTITY_WINDOW) >= height))
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
                .ok_or(ErrorCode::StorageCorrupt)?;
            let gas_used = tx
                .spent_units
                .parse::<u64>()
                .map_err(|_| ErrorCode::StorageCorrupt)?;
            // Rent is burned beside the fee, not as gas.
            let rent = &tx.rent_wei;
            return Ok(json!({
                "gasUsed": gas_used.to_string(),
                "gasPriceWei": crate::accounts::GAS_PRICE_WEI.to_string(),
                "feeChargedWei": crate::accounts::fee(gas_used).to_string(),
                "rentChargedWei": rent.to_string(),
                "status": "included",
                "actor": hex(actor.as_bytes()),
                "requestId": hex(&request_id),
                "nonce": nonce.to_string(),
                "transactionHash": hex(inclusion.digest.as_slice()),
                "height": inclusion.height.to_string(),
                "index": inclusion.index,
                "executionStatus": tx.status,
                "durability": "durable",
            }));
        }
        if let Some(tx) = self.pool.pending(actor, request_id) {
            return Ok(json!({
                "status": "pending",
                "actor": hex(actor.as_bytes()),
                "requestId": hex(&request_id),
                "transactionHash": hex(tx.hash().as_slice()),
                "nonce": tx.nonce.to_string(),
                "durability": "volatile",
            }));
        }
        if let Some((hash, reason)) = self.pool.dropped.request(actor, request_id) {
            return Ok(json!({
                "status": "dropped",
                "reason": reason.as_str(),
                "actor": hex(actor.as_bytes()),
                "requestId": hex(&request_id),
                "transactionHash": hash,
                "durability": "volatile",
            }));
        }
        Ok(json!({"status":"unknown","actor":hex(actor.as_bytes()),"requestId":hex(&request_id)}))
    }

    /// Validates `tx` and admits it; callers that validated outside the node
    /// lock use [`Self::admit`].
    pub fn submit(&mut self, tx: SignedTx) -> Result<Json> {
        self.accepting()?;
        let tx = ValidTx::new(tx, &self.genesis)?;
        self.admit(tx)
    }

    fn accepting(&self) -> Result<()> {
        if !self.is_producer() {
            return Err(ErrorCode::ProducerOnly.into());
        }
        if self.pending.is_some()
            || self.health == Health::StorageFenced
            || self.health == Health::ChainConflict
        {
            return Err(ErrorCode::CommitUncertain.into());
        }
        Ok(())
    }

    /// Admits a transaction validated against this node's genesis to the pool.
    pub fn admit(&mut self, tx: ValidTx) -> Result<Json> {
        self.accepting()?;
        let tx = tx.into_inner();
        let identity = tx.identity();
        let next = self.head.height.saturating_add(1);
        if let Some((found, _)) = self.inclusion(&identity)?
            && (found.digest == tx.hash() || self.recent_inclusion(&identity, next)?.is_some())
        {
            if found.digest != tx.hash() {
                return Err(ErrorCode::RequestConflict.into());
            }
            return self.request_status(identity.actor, identity.request_id);
        }
        if self.head.height == 0 && self.genesis.prefill.is_none() {
            return Err(ErrorCode::ChainInitializing.into());
        }
        let next = self.committed_nonce(identity.actor)?;
        let balance = self
            .account(self.head.height, identity.actor)
            .map(|a| a.balance)?;
        // Nonce, size, capacity and fee first: only then read any entity.
        if self.pool.admissible(&tx, next, balance)?.is_none() {
            let quote = self.quote(&tx.tx)?;
            if quote.new_units > self.genesis.genesis.params.max_new_storage_units_per_block {
                return Err(ErrorCode::ExceedsStorageCap.into());
            }
            self.pool.insert_funded(tx, next, balance, quote.rent_wei)?;
        }
        self.request_status(identity.actor, identity.request_id)
    }

    /// What `tx` would charge in the next block on the head state
    /// ([`crate::arkiv::quote_tx`]): its operations in order, each seeing
    /// what the earlier ones of `tx` created or changed, the rent reserved
    /// at the larger of now and [`super::pool::RENT_QUOTE_MARGIN_BLOCKS`]
    /// later. It reads each named entity once and creates read nothing. An
    /// operation that builds on another pending transaction quotes what the
    /// head shows (nothing for an entity not created yet); execution checks
    /// the real rent against the sender's balance.
    pub fn quote(&self, tx: &crate::TxEnvelope) -> Result<crate::arkiv::TxQuote> {
        if !tx
            .ops
            .iter()
            .any(|op| matches!(op, crate::TxOp::Arkiv { .. }))
        {
            return Ok(crate::arkiv::TxQuote::default());
        }
        let spec = self.genesis.spec()?;
        let branch = self.engine.begin(self.engine.head(), None)?;
        let context = crate::arkiv::ArkivExecutionContext {
            caller: *tx.actor.as_bytes(),
            block_height: self
                .head
                .height
                .checked_add(1)
                .ok_or(ErrorCode::CapacityReached)?,
            chain_id: spec.chain_id(),
            rent: crate::gas::RentClock::of(spec.params())
                .ok_or(ErrorCode::InvalidSignedGenesis)?,
            expiry_per_block: spec.params().expiry_per_block,
            expiry_spill_window: crate::exec::EXPIRY_SPILL_WINDOW,
            payer: None,
            allow_permanent_create: false,
        };
        let ops = tx.ops.iter().filter_map(|op| match op {
            crate::TxOp::Arkiv {
                namespace,
                operation,
            } => Some((*namespace, operation)),
            _ => None,
        });
        crate::arkiv::quote_tx(
            &branch,
            &context,
            super::pool::RENT_QUOTE_MARGIN_BLOCKS,
            ops,
        )
        .map_err(|e| match e {
            crate::ExecError::Storage(e) => Error::from(e),
            _ => Error::from(ErrorCode::StorageCorrupt),
        })
    }

    pub fn produce(&mut self) -> Result<SignedHeader> {
        if !self.is_producer() {
            return Err(ErrorCode::ProducerOnly.into());
        }
        if self.pending.is_some() {
            return Err(ErrorCode::CommitUncertain.into());
        }
        if self.head.height == 0 && self.genesis.prefill.is_none() {
            let signer = self.signer.as_ref().ok_or(ErrorCode::ProducerOnly)?;
            let tx = crate::TxEnvelope {
                chain_id: self.genesis.genesis.chain_id,
                actor: signer.address(),
                request_id: [0; 32],
                tx_budget: self.genesis.genesis.params.user_budget,
                ops: vec![crate::TxOp::CreateNamespace {
                    name: arkiv_db::NamespaceName::new("entities")
                        .map_err(|_| ErrorCode::InvalidRequest)?,
                }],
            };
            let signed = SignedTx::sign(&self.genesis, 0, tx, signer)?;
            let balance = self.account(0, signer.address())?.balance;
            self.pool
                .insert_funded(signed, 0, balance, alloy_primitives::U256::ZERO)?;
        }
        // One execution builds the block: a candidate that cannot be
        // included is left out (exec::DeferReason) rather than rebuilt around.
        let inputs = self.pool.select(|actor| self.committed_nonce(actor))?;
        self.execute_publish(inputs, None)
    }

    pub fn import(&mut self, block: SignedBlock) -> Result<SignedHeader> {
        if self.is_producer() {
            return Err(ErrorCode::FollowerOnly.into());
        }
        self.observe_certificate(&block.header)?;
        if self.health == Health::ChainConflict {
            return Err(ErrorCode::ChainConflict.into());
        }
        if self.pending.is_some() {
            return Err(ErrorCode::CommitUncertain.into());
        }
        let height = block.header.header.height;
        if height <= self.head.height {
            let prior = self.chain_certificate(height)?;
            if prior.header != block.header.header {
                self.health = Health::ChainConflict;
                let evidence = encode(&(prior, block.header));
                self.history.freeze_conflict(&evidence)?;
                return Err(ErrorCode::ChainConflict.into());
            }
            let known = self.replication_block(height)?;
            if known.inputs != block.inputs || known.outcomes != block.outcomes {
                return Err(ErrorCode::ChainConflict.into());
            }
            return self.certificate(self.head.height);
        }
        if height
            != self
                .head
                .height
                .checked_add(1)
                .ok_or(ErrorCode::CapacityReached)?
        {
            return Err(ErrorCode::BlockNotSynced.into());
        }
        block.authenticate(&self.genesis)?;
        self.execute_publish(block.inputs.clone(), Some(&block))
    }

    /// A sampled authentic header at a retained height can establish
    /// equivocation even when no new block body is advertised.
    pub fn observe_certificate(&mut self, certificate: &SignedHeader) -> Result<()> {
        certificate.authenticate(&self.genesis)?;
        if self.health == Health::ChainConflict {
            return Err(ErrorCode::ChainConflict.into());
        }
        let previous = match self.history.remember_header(certificate) {
            Ok(previous) => previous,
            Err(error) => {
                self.health = Health::StorageFenced;
                return Err(error);
            }
        };
        if let Some(previous) = previous {
            self.observe_pair(&previous, certificate)?;
        }
        if certificate.header.height <= self.head.height {
            // Below the window with expired bodies there is nothing left to
            // compare with.
            match self.chain_certificate(certificate.header.height) {
                Ok(prior) => self.observe_pair(&prior, certificate)?,
                Err(e) if e.code == ErrorCode::HistoryPruned => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Equivocation can also be established before either header is imported.
    pub fn observe_pair(&mut self, first: &SignedHeader, second: &SignedHeader) -> Result<()> {
        first.authenticate(&self.genesis)?;
        second.authenticate(&self.genesis)?;
        if self.health == Health::ChainConflict {
            return Err(ErrorCode::ChainConflict.into());
        }
        if first.header.height == second.header.height && first.header != second.header {
            self.health = Health::ChainConflict;
            self.history
                .freeze_conflict(&encode(&(first.clone(), second.clone())))?;
            return Err(ErrorCode::ChainConflict.into());
        }
        Ok(())
    }

    /// Executes and publishes the next block: `expected`'s inputs when
    /// importing (every transaction must fit), or the producer's candidate
    /// `inputs`, built into a block without the candidates that cannot be
    /// included. A candidate that alone exceeds the storage cap leaves the
    /// pool; the others stay for a later block.
    fn execute_publish(
        &mut self,
        inputs: SignedInputs,
        expected: Option<&SignedBlock>,
    ) -> Result<SignedHeader> {
        if self.health == Health::StorageFenced || self.health == Health::ChainConflict {
            return Err(ErrorCode::CommitUncertain.into());
        }
        let height = self
            .head
            .height
            .checked_add(1)
            .ok_or(ErrorCode::CapacityReached)?;
        inputs.validate(&self.genesis, height)?;
        let mut next = BTreeMap::new();
        for tx in &inputs.txs {
            if self.recent_inclusion(&tx.identity(), height)?.is_some() {
                return Err(ErrorCode::RequestConflict.into());
            }
            let nonce = match next.get(&tx.tx.actor) {
                Some(n) => *n,
                None => self.committed_nonce(tx.tx.actor)?,
            };
            if tx.nonce != nonce || nonce == u64::MAX {
                return Err(ErrorCode::InvalidNonce.into());
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
        let accounts = &self.genesis.accounts;
        let after_tx = |branch: &mut arkiv_db::Branch, index: usize, tx: &crate::TxEnvelope| {
            let mut account = accounts.read(branch, tx.actor)?;
            if account.nonce != inputs.txs[index].nonce || account.nonce == u64::MAX {
                return Err(StorageError::new(
                    StorageErrorKind::Corrupt,
                    "account nonce",
                ));
            }
            account.nonce += 1;
            crate::accounts::put(branch, tx.actor, account)
        };
        let observer = |branch: &arkiv_db::Branch,
                        executed: &crate::BlockInputs,
                        outcomes: &BlockOutcomes,
                        expired: &[crate::exec::ExpiredRecord]| {
            captured = Some(metadata::capture(branch, executed, outcomes, expired)?);
            Ok(())
        };
        let timestamp = match expected {
            Some(block) => {
                self.genesis
                    .check_successor_timestamp(&self.head, &block.header.header)?;
                block.header.header.timestamp_ms
            }
            None => self.genesis.next_timestamp(&self.head, (self.clock)())?,
        };
        self.block_executions += 1;
        // A block received is executed as given; a block produced leaves
        // out the candidates it cannot include.
        let mode = if expected.is_some() {
            Mode::Strict
        } else {
            Mode::Build
        };
        let env = BlockEnv {
            spec: &spec,
            parent: &self.head,
            height,
            timestamp_ms: timestamp,
        };
        let executed = execute_block_hooked(
            env,
            branch,
            &native,
            Some(accounts),
            mode,
            after_tx,
            observer,
        );
        let mut executed = executed.map_err(|e| match e {
            crate::ExecError::Storage(e) => Error::from(e),
            _ => Error::from(ErrorCode::InvalidBlock),
        })?;
        // The block holds the included candidates only, in order.
        let inputs = if executed.deferred.is_empty() {
            inputs
        } else {
            let mut left_out = executed.deferred.iter().map(|d| d.index).peekable();
            let mut kept = SignedInputs::default();
            for (index, tx) in inputs.txs.into_iter().enumerate() {
                if left_out.next_if_eq(&index).is_none() {
                    kept.txs.push(tx);
                } else if executed
                    .deferred
                    .iter()
                    .any(|d| d.index == index && d.reason == DeferReason::ExceedsStorageCap)
                {
                    // No block can hold it.
                    self.pool
                        .evict_from(tx.tx.actor, tx.nonce, DropReason::ExceedsStorageCap);
                }
            }
            kept
        };
        executed.header.inputs_digest = inputs.digest();
        if height == 1
            && self.genesis.prefill.is_none()
            && !matches!(
                executed.outcomes.txs.first().map(|tx| tx.status),
                Some(crate::TxStatus::Committed)
            )
        {
            return Err(ErrorCode::BootstrapFailed.into());
        }
        let certificate = match expected {
            Some(block) => block.header.clone(),
            None => SignedHeader::sign(
                &self.genesis,
                executed.header,
                self.signer.as_ref().ok_or(ErrorCode::ProducerOnly)?,
            )?,
        };
        if certificate.header != executed.header {
            return Err(ErrorCode::ChainConflict.into());
        }
        let block = SignedBlock {
            header: certificate.clone(),
            inputs,
            outcomes: executed.outcomes,
        };
        if expected.is_some_and(|e| e.outcomes != block.outcomes) {
            return Err(ErrorCode::ChainConflict.into());
        }
        let body_bytes = encode(&block);
        if body_bytes.len() > self.genesis.limits.max_body_bytes as usize {
            return Err(ErrorCode::CapacityReached.into());
        }
        let mut metadata = captured.ok_or(ErrorCode::StorageCorrupt)?;
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
            return Err(ErrorCode::CapacityReached.into());
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
        if self.health == Health::ChainConflict {
            return Err(ErrorCode::ChainConflict.into());
        }
        let pending = self.pending.as_mut().ok_or(ErrorCode::InvalidRequest)?;
        if !pending.history_saved {
            let certificate = &pending.record.certificate;
            let result = if retry {
                self.history.retry_exact_block(certificate, &pending.bytes)
            } else {
                self.history.persist_block(certificate, &pending.bytes)
            };
            match result {
                Ok(reference) if reference == pending.record.body => pending.history_saved = true,
                Ok(_) => {
                    self.health = Health::StorageFenced;
                    return Err(ErrorCode::StorageCorrupt.into());
                }
                Err(e) => {
                    self.health = Health::StorageFenced;
                    return Err(e);
                }
            }
        }
        if pending.engine_attempted {
            match self.engine.recover_pending()? {
                Recovery::RetryPrepared => {}
                _ => return Err(ErrorCode::CommitUncertain.into()),
            }
        }
        pending.engine_attempted = true;
        match self.engine.commit()? {
            Publication::Committed(info) => {
                let pending = self.pending.take().ok_or(ErrorCode::StorageCorrupt)?;
                if info.snapshot.sequence() != pending.record.certificate.header.height
                    || info.state_root != pending.record.certificate.header.state_root
                {
                    self.health = Health::StorageFenced;
                    return Err(ErrorCode::StorageCorrupt.into());
                }
                self.pool.remove_committed(&pending.block.inputs);
                self.head = pending.record.certificate.header;
                if self.is_producer() {
                    // Rent can exceed its quote: drop what a sender can no
                    // longer cover before it is selected.
                    let snapshot = self.engine.head_snapshot();
                    let accounts = &self.genesis.accounts;
                    self.pool.evict_unaffordable(|actor| {
                        Ok(accounts
                            .resolve(
                                actor,
                                snapshot.try_host_get(&crate::accounts::account_key(actor))?,
                            )?
                            .balance)
                    })?;
                }
                self.health = if self.is_producer() {
                    if self.paused {
                        Health::Paused
                    } else {
                        Health::Running
                    }
                } else {
                    Health::Following
                };
                // The block is published; a failed body deletion is
                // retried with the next block.
                if let Err(error) = self.expire_bodies() {
                    eprintln!(
                        "{}",
                        json!({"event": "history_prune_failed", "error": error.diagnostic()})
                    );
                }
                Ok(pending.record.certificate)
            }
            Publication::NotCommitted(_) => {
                self.health = Health::StorageFenced;
                Err(ErrorCode::StorageUnavailable.into())
            }
            Publication::Indeterminate(_) => {
                self.health = Health::StorageFenced;
                Err(ErrorCode::CommitUncertain.into())
            }
        }
    }

    pub fn retry_pending(&mut self) -> Result<SignedHeader> {
        self.finish_pending(true)
    }

    pub fn proof(&self, request: &arkiv_db::proof::EqRequest) -> Result<Vec<u8>> {
        eq_proof_bytes(
            &self.proof_state(&request.snapshot, ErrorCode::CursorMismatch)?,
            request,
        )
    }

    pub fn range_proof(&self, request: &arkiv_db::proof::RangeRequest) -> Result<Vec<u8>> {
        range_proof_bytes(
            &self.proof_state(&request.snapshot, ErrorCode::InvalidRequest)?,
            request,
        )
    }

    /// The pinned state a proof for `snapshot` is generated from, once the
    /// reference names this chain's block at its height; `mismatch` is the
    /// refusal otherwise. Proving needs only the returned snapshot, so it
    /// can run after the node lock is released.
    pub fn proof_state(
        &self,
        snapshot: &arkiv_db::proof::SnapshotRef,
        mismatch: ErrorCode,
    ) -> Result<arkiv_db::Snapshot> {
        let header = self.certificate(snapshot.height)?.header;
        if snapshot.genesis_hash != self.genesis.hash()
            || snapshot.block_hash != header.hash()
            || snapshot.state_root != header.state_root
        {
            return Err(mismatch.into());
        }
        self.state(header.height)
    }
}

/// Work bound of one proof; reads abandoned by the node stop early.
const PROOF_BUDGET: Budget = Budget::limit(10_000_000).interruptible();

/// Equality proof bytes for `request` from its [`Host::proof_state`].
pub fn eq_proof_bytes(
    state: &arkiv_db::Snapshot,
    request: &arkiv_db::proof::EqRequest,
) -> Result<Vec<u8>> {
    Ok(state.prove_eq(request, proof_limits(), PROOF_BUDGET)?.bytes)
}

/// Complete range proof bytes for `request` from its [`Host::proof_state`].
pub fn range_proof_bytes(
    state: &arkiv_db::Snapshot,
    request: &arkiv_db::proof::RangeRequest,
) -> Result<Vec<u8>> {
    Ok(state
        .prove_range(
            request,
            crate::simulator::wire::range_proof_limits()?,
            PROOF_BUDGET,
        )?
        .bytes)
}
