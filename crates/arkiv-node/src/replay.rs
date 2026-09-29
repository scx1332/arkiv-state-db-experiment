//! An in-memory chain: produce blocks, or replay recorded ones from
//! genesis, on the volatile memory engine.
//!
//! [`MemoryChain`] holds one engine, the header and terminal snapshot of
//! every block, and the inclusion index. [`MemoryChain::produce`] executes
//! new inputs; [`MemoryChain::apply`] re-executes a recorded block and
//! checks that it reproduces the recorded outcome and header bytes. Both
//! run [`execute_block`] and publish one terminal snapshot per block.
//! [`replay`] applies a sequence of recorded blocks to a fresh chain and
//! reports the first divergence.
//!
//! Nothing here is durable: this is the memory reference the durable node
//! (step N4) is compared against, and the offline replay check.

use std::fmt;

use arkiv_db::{B256, Cost, Engine, EngineError, Snapshot, SnapshotId, encode};

use crate::{
    block::{Block, BlockInputs, Genesis, Header},
    exec::{ExecError, ExecutedBlock, execute_block},
    identity::RequestIndex,
    params::{ChainSpec, GenesisError},
    validate::{self, BlockFault},
};

/// Why a block could not be produced or applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChainError {
    /// Execution failed (or the inputs are invalid); nothing was published.
    Exec(ExecError),
    /// The block is invalid against the chain (successor rule or request
    /// identities); nothing was executed.
    InvalidBlock(BlockFault),
    /// Re-execution produced different outcome bytes.
    OutcomesDiffer,
    /// Re-execution produced a different header.
    HeaderDiffers {
        /// The block hash of the header re-execution produced.
        produced: B256,
        /// The recorded block hash.
        recorded: B256,
    },
    /// The engine refused the publication.
    Engine(EngineError),
    /// The height counter is exhausted.
    HeightExhausted,
}

impl fmt::Display for ChainError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exec(error) => write!(formatter, "{error}"),
            Self::InvalidBlock(fault) => write!(formatter, "invalid block: {fault}"),
            Self::OutcomesDiffer => write!(formatter, "re-executed outcomes differ"),
            Self::HeaderDiffers { produced, recorded } => write!(
                formatter,
                "re-executed header {produced} differs from the recorded {recorded}"
            ),
            Self::Engine(error) => write!(formatter, "publication failed: {error}"),
            Self::HeightExhausted => write!(formatter, "block heights exhausted"),
        }
    }
}

impl std::error::Error for ChainError {}

impl From<ExecError> for ChainError {
    fn from(error: ExecError) -> Self {
        Self::Exec(error)
    }
}

/// Why a replay stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReplayError {
    /// The genesis is not acceptable.
    Genesis(GenesisError),
    /// The first block that diverged, by its position in the replayed
    /// sequence (its expected height).
    Block {
        /// The expected height of the block.
        height: u64,
        /// What diverged.
        error: ChainError,
    },
}

impl fmt::Display for ReplayError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Genesis(error) => write!(formatter, "genesis: {error}"),
            Self::Block { height, error } => write!(formatter, "block {height}: {error}"),
        }
    }
}

impl std::error::Error for ReplayError {}

/// A chain on the memory engine; see the [module docs](self).
#[derive(Debug)]
pub struct MemoryChain {
    spec: ChainSpec,
    engine: Engine,
    /// Header of every block, indexed by height; `[0]` is genesis.
    headers: Vec<Header>,
    /// Terminal snapshot of every block, indexed by height.
    snapshots: Vec<SnapshotId>,
    requests: RequestIndex,
}

/// An executed candidate that has not entered any published history.
/// Hosts finish all fallible body/signature/transport preparation first.
#[derive(Debug)]
pub struct PreparedBlock {
    block: Block,
    executed: ExecutedBlock,
}

impl PreparedBlock {
    /// Canonical body available for transport admission before publication.
    pub const fn block(&self) -> &Block {
        &self.block
    }
}

impl MemoryChain {
    /// A chain holding only its genesis block: an engine with the genesis
    /// limits and pricing and no namespaces.
    pub fn new(spec: ChainSpec) -> Self {
        let genesis = spec.genesis();
        let engine = Engine::with_limits(genesis.pricing.clone(), genesis.limits);
        let headers = vec![*spec.genesis_header()];
        let snapshots = vec![engine.head()];
        Self {
            spec,
            engine,
            headers,
            snapshots,
            requests: RequestIndex::new(),
        }
    }

    /// The chain definition.
    pub const fn spec(&self) -> &ChainSpec {
        &self.spec
    }

    /// The engine holding every block's terminal state.
    pub const fn engine(&self) -> &Engine {
        &self.engine
    }

    /// The inclusion index of every committed request.
    pub const fn requests(&self) -> &RequestIndex {
        &self.requests
    }

    /// The head block's header.
    pub fn head(&self) -> &Header {
        self.headers.last().expect("a chain has a genesis block")
    }

    /// The head height.
    pub fn height(&self) -> u64 {
        self.head().height
    }

    /// The header at `height`, if that block exists.
    pub fn header(&self, height: u64) -> Option<&Header> {
        self.headers.get(usize::try_from(height).ok()?)
    }

    /// The terminal state of the block at `height`, if it exists.
    pub fn snapshot(&self, height: u64) -> Option<Snapshot> {
        let id = *self.snapshots.get(usize::try_from(height).ok()?)?;
        self.engine.snapshot(id).ok()
    }

    /// Executes `inputs` as the next block at `timestamp_ms` and publishes
    /// it. Fails without any change when an identity repeats or was already
    /// included, or when execution fails.
    pub fn produce(&mut self, timestamp_ms: u64, inputs: BlockInputs) -> Result<Block, ChainError> {
        let prepared = self.prepare(timestamp_ms, inputs)?;
        self.finalize(prepared)
    }

    /// Executes without changing the published head. Dropping this object
    /// discards the candidate, including after a host size/signing failure.
    pub fn prepare(
        &self,
        timestamp_ms: u64,
        inputs: BlockInputs,
    ) -> Result<PreparedBlock, ChainError> {
        let height = self
            .height()
            .checked_add(1)
            .ok_or(ChainError::HeightExhausted)?;
        self.requests
            .check_block(self.spec.chain_id(), &inputs)
            .map_err(ChainError::InvalidBlock)?;
        let branch = self
            .engine
            .begin(
                self.engine.head(),
                Some(Cost::new(self.spec.params().block_budget)),
            )
            .map_err(ChainError::Engine)?;
        let executed = execute_block(
            &self.spec,
            branch,
            self.head(),
            height,
            timestamp_ms,
            &inputs,
        )?;
        let block = Block {
            header: executed.header,
            inputs,
            outcomes: executed.outcomes.clone(),
        };
        Ok(PreparedBlock { block, executed })
    }

    /// Publishes a prepared candidate. Hosts call this and insert their
    /// already-prepared body/signature under the same writer lock.
    pub fn finalize(&mut self, prepared: PreparedBlock) -> Result<Block, ChainError> {
        validate::check_header(&self.spec, self.head(), &prepared.block.header)
            .map_err(|fault| ChainError::InvalidBlock(BlockFault::Successor(fault)))?;
        let mut requests = self.requests.clone();
        requests
            .commit_block(
                self.spec.chain_id(),
                prepared.block.header.height,
                &prepared.block.inputs,
            )
            .map_err(ChainError::InvalidBlock)?;
        let snapshot = self
            .engine
            .publish(&prepared.executed.candidate)
            .map_err(ChainError::Engine)?;
        self.requests = requests;
        self.headers.push(prepared.block.header);
        self.snapshots.push(snapshot);
        Ok(prepared.block)
    }

    /// Independently re-executes and compares every outcome and header byte
    /// (including protocol-2 terminal root) before publication.
    pub fn apply(&mut self, block: &Block) -> Result<(), ChainError> {
        validate::check_header(&self.spec, self.head(), &block.header)
            .map_err(|fault| ChainError::InvalidBlock(BlockFault::Successor(fault)))?;
        let prepared = self.prepare(block.header.timestamp_ms, block.inputs.clone())?;
        if encode(&prepared.block.outcomes) != encode(&block.outcomes) {
            return Err(ChainError::OutcomesDiffer);
        }
        if encode(&prepared.block.header) != encode(&block.header) {
            return Err(ChainError::HeaderDiffers {
                produced: prepared.block.header.hash(),
                recorded: block.header.hash(),
            });
        }
        self.finalize(prepared)?;
        Ok(())
    }
}

/// Replays `blocks` (heights 1, 2, …) from `genesis` on a fresh memory
/// engine, checking every block's validity and that re-execution
/// reproduces its outcome and header bytes; returns the replayed chain or
/// the first divergence.
pub fn replay(genesis: &Genesis, blocks: &[Block]) -> Result<MemoryChain, ReplayError> {
    let spec = ChainSpec::new(genesis.clone()).map_err(ReplayError::Genesis)?;
    let mut chain = MemoryChain::new(spec);
    for block in blocks {
        let height = chain.height().saturating_add(1);
        chain
            .apply(block)
            .map_err(|error| ReplayError::Block { height, error })?;
    }
    Ok(chain)
}
