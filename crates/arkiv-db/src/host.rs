//! Host-side helpers that are deliberately outside the engine.
//!
//! The host owns consensus, finality and the mapping from block numbers to
//! engine snapshots. A block may be produced by several publications (for
//! example an admin stage and then a data stage); only its terminal snapshot
//! is recorded, so block numbers and snapshot IDs are distinct sequences.

use std::{fmt, ops::Bound};

use arkiv_db_storage::versioned::StorageError;

use crate::{
    engine::{Branch, Engine, EngineError, Rejected, Snapshot, SnapshotId, WriteOp, in_memory},
    meter::{Budget, Receipt},
    namespace::NamespaceId,
    query::{Projection, Query, Term},
    record::{CellName, RecordId, TypeTag, Value},
};

/// A block height as assigned by the host chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockNumber(pub u64);

impl fmt::Display for BlockNumber {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "block#{}", self.0)
    }
}

/// Failures of the host's block table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostError {
    /// Blocks must be recorded consecutively after the first one.
    NonConsecutiveBlock {
        /// The block the host tried to record.
        block: BlockNumber,
        /// The block expected next.
        expected: BlockNumber,
    },
    /// Every block must map to a later snapshot than the previous block.
    NonMonotonicSnapshot {
        /// The snapshot the host tried to record.
        snapshot: SnapshotId,
        /// The snapshot recorded for the previous block.
        previous: SnapshotId,
    },
    /// The block has no recorded terminal snapshot.
    UnknownBlock(BlockNumber),
    /// The next consecutive block number does not fit in 64 bits.
    BlockNumbersExhausted,
    /// The recorded snapshot could not be pinned.
    Engine(EngineError),
}

impl fmt::Display for HostError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonConsecutiveBlock { block, expected } => {
                write!(formatter, "cannot record {block}: expected {expected}")
            }
            Self::NonMonotonicSnapshot { snapshot, previous } => write!(
                formatter,
                "cannot map a block to {snapshot}: previous block maps to {previous}"
            ),
            Self::UnknownBlock(block) => write!(formatter, "unknown {block}"),
            Self::BlockNumbersExhausted => write!(formatter, "block numbers exhausted"),
            Self::Engine(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for HostError {}

/// An expiry run that stopped at a rejection. The deletes applied before it
/// stay applied in the branch and keep their receipts there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExpiryHalt {
    /// The deletes applied before the rejection, in id order.
    pub applied: Vec<(RecordId, Receipt)>,
    /// The query or delete rejection that stopped the run.
    pub rejected: Rejected,
}

impl fmt::Display for ExpiryHalt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "expiry halted after {} deletes: {}",
            self.applied.len(),
            self.rejected
        )
    }
}

impl std::error::Error for ExpiryHalt {}

/// Deletes, with host authority, every live record of `namespace` whose
/// `name` attribute is a `U64` no greater than `block`: the host's expiry
/// step for a block, run on the block's branch before it is sealed.
///
/// The engine has no clock: expiry is this deletion in the block where the
/// record expires, so snapshot(B) lacks the record while snapshot(B-1) keeps
/// it, and no read consults the head. The host stores the expiry height as
/// an ordinary `U64` attribute and must keep users from writing it (the
/// engine assigns `$`-names no meaning).
///
/// The sweep goes height by height in ascending order: `terms` reads the
/// smallest live height (`query_budget`; one seek and one term), then the
/// rows holding that height are read in pages of at most `max_page`
/// (`query_budget`, an `Eq` posting positioned from its start) and deleted
/// one by one with `delete_budget`, until the height has no rows and the
/// next smallest height is read. Read receipts are discarded. Deletes are
/// reported in (height, id) order. Every read is positional, so the work is
/// linear in the distinct expired heights plus the deleted rows, and a
/// backlog of heights (after a halted run) is caught up one height per
/// iteration rather than re-scanned per page.
///
/// Any rejection stops the run with the deletes applied so far, which
/// remain applied in the branch. The host must reserve block budget for
/// expiry (run it first, or size the block budget for it); a halted run
/// leaves the remaining rows live until a later block, where the smallest
/// live height is read first again.
#[allow(clippy::result_large_err)]
pub fn expire_records(
    branch: &mut Branch,
    namespace: NamespaceId,
    name: &CellName,
    block: BlockNumber,
    query_budget: Budget,
    delete_budget: Budget,
) -> Result<Vec<(RecordId, Receipt)>, ExpiryHalt> {
    in_memory(try_expire_records(
        branch,
        namespace,
        name,
        block,
        query_budget,
        delete_budget,
    ))
}

/// Fallible form of [`expire_records`], built on the branch's `try_` reads
/// and host deletes (see
/// [Fallible surface](crate::engine#fallible-surface)).
///
/// The outer `Err` is a storage failure: the sweep stopped without a
/// deterministic outcome and the host must discard the branch, never record
/// a partial expiry. The inner result is exactly what [`expire_records`]
/// returns. The in-memory engine never fails.
#[allow(clippy::result_large_err)]
pub fn try_expire_records(
    branch: &mut Branch,
    namespace: NamespaceId,
    name: &CellName,
    block: BlockNumber,
    query_budget: Budget,
    delete_budget: Budget,
) -> Result<Result<Vec<(RecordId, Receipt)>, ExpiryHalt>, StorageError> {
    try_expire_records_observed(
        branch,
        namespace,
        name,
        block,
        query_budget,
        delete_budget,
        &mut |_, _, _| Ok(()),
    )
}

/// Expiry with a synchronous, noncanonical observer of successful deletions.
/// The key comes from the existing query; observing adds no reads or charges.
/// An observer failure requires discarding the entire branch, just like an I/O failure.
/// Hosts must bound any collection they retain in the observer.
#[allow(clippy::result_large_err, clippy::too_many_arguments)]
pub fn try_expire_records_observed(
    branch: &mut Branch,
    namespace: NamespaceId,
    name: &CellName,
    block: BlockNumber,
    query_budget: Budget,
    delete_budget: Budget,
    observer: &mut dyn FnMut(RecordId, &crate::RecordKey, &Receipt) -> Result<(), StorageError>,
) -> Result<Result<Vec<(RecordId, Receipt)>, ExpiryHalt>, StorageError> {
    let limit = branch.limits().max_page;
    let mut applied = Vec::new();
    loop {
        let heights = match branch.try_terms(
            namespace,
            name,
            TypeTag::U64,
            Bound::Unbounded,
            1,
            query_budget,
        )? {
            Ok(heights) => heights.value,
            Err(rejected) => return Ok(Err(ExpiryHalt { applied, rejected })),
        };
        let height = match heights.first() {
            Some(Value::U64(height)) if *height <= block.0 => *height,
            _ => return Ok(Ok(applied)),
        };
        let due = Query::term(Term::eq(name.clone(), Value::U64(height)));
        loop {
            let page = match branch.try_query(
                namespace,
                &due,
                &Projection::KEY_ONLY,
                limit,
                None,
                query_budget,
            )? {
                Ok(page) => page.value,
                Err(rejected) => return Ok(Err(ExpiryHalt { applied, rejected })),
            };
            if page.rows.is_empty() {
                break;
            }
            for row in page.rows {
                let key = row.key.expect("the key was projected");
                let op = WriteOp::DeleteRecord {
                    namespace,
                    key: key.clone(),
                };
                match branch.try_execute_host(op, delete_budget)? {
                    Ok(executed) => {
                        observer(row.id, &key, &executed.receipt)?;
                        applied.push((row.id, executed.receipt));
                    }
                    Err(rejected) => return Ok(Err(ExpiryHalt { applied, rejected })),
                }
            }
        }
    }
}

impl From<EngineError> for HostError {
    fn from(error: EngineError) -> Self {
        Self::Engine(error)
    }
}

/// The host's table of completed blocks and their terminal snapshots.
///
/// This is a test-harness mapping, not consensus evidence: the chain's
/// commitment rules authenticate what a block's state is; this table only
/// remembers which in-process snapshot the host resolved it to. Snapshot IDs
/// are per engine instance, so a table is meaningful only for the engine
/// whose IDs it recorded; [`BlockMap::at`] does not check engine identity.
#[derive(Clone, Debug, Default)]
pub struct BlockMap {
    first: Option<BlockNumber>,
    snapshots: Vec<SnapshotId>,
}

impl BlockMap {
    /// Creates an empty table.
    pub const fn new() -> Self {
        Self {
            first: None,
            snapshots: Vec::new(),
        }
    }

    /// Records `block`'s terminal snapshot.
    ///
    /// The first block may be any number; every later block must be the next
    /// consecutive number and map to a strictly later snapshot. Recording the
    /// same block twice is rejected, so a block resolves to one state only.
    pub fn record(&mut self, block: BlockNumber, snapshot: SnapshotId) -> Result<(), HostError> {
        if let Some(first) = self.first {
            let expected = first
                .0
                .checked_add(self.count())
                .map(BlockNumber)
                .ok_or(HostError::BlockNumbersExhausted)?;
            if block != expected {
                return Err(HostError::NonConsecutiveBlock { block, expected });
            }
        }
        if let Some(&previous) = self.snapshots.last()
            && snapshot <= previous
        {
            return Err(HostError::NonMonotonicSnapshot { snapshot, previous });
        }
        if self.first.is_none() {
            self.first = Some(block);
        }
        self.snapshots.push(snapshot);
        Ok(())
    }

    fn count(&self) -> u64 {
        u64::try_from(self.snapshots.len()).expect("block count fits u64")
    }

    /// The block number the next record must carry: `None` before the first
    /// record, or when the next consecutive number would not fit in 64 bits.
    pub fn next_block(&self) -> Option<BlockNumber> {
        let first = self.first?;
        first.0.checked_add(self.count()).map(BlockNumber)
    }

    /// The most recently recorded block and its snapshot.
    pub fn latest(&self) -> Option<(BlockNumber, SnapshotId)> {
        let first = self.first?;
        let last = *self.snapshots.last()?;
        let offset = self.count() - 1;
        first
            .0
            .checked_add(offset)
            .map(|block| (BlockNumber(block), last))
    }

    /// Number of recorded blocks.
    pub fn len(&self) -> usize {
        self.snapshots.len()
    }

    /// Whether no block has been recorded.
    pub fn is_empty(&self) -> bool {
        self.snapshots.is_empty()
    }

    /// The terminal snapshot recorded for `block`, if any.
    pub fn resolve(&self, block: BlockNumber) -> Option<SnapshotId> {
        let first = self.first?;
        let offset = usize::try_from(block.0.checked_sub(first.0)?).ok()?;
        self.snapshots.get(offset).copied()
    }

    /// Pins the state after `block`.
    pub fn at(&self, engine: &Engine, block: BlockNumber) -> Result<Snapshot, HostError> {
        let snapshot = self.resolve(block).ok_or(HostError::UnknownBlock(block))?;
        Ok(engine.snapshot(snapshot)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Engine, PriceSchedule};

    #[test]
    fn block_numbers_cannot_wrap() {
        let mut engine = Engine::new(PriceSchedule::FIXTURE);
        let sealed = engine.begin(engine.head(), None).unwrap().seal();
        let one = engine.publish(&sealed).unwrap();
        let mut blocks = BlockMap::new();
        blocks
            .record(BlockNumber(u64::MAX), SnapshotId::GENESIS)
            .unwrap();
        assert_eq!(blocks.next_block(), None);
        assert_eq!(
            blocks.latest(),
            Some((BlockNumber(u64::MAX), SnapshotId::GENESIS))
        );
        assert_eq!(
            blocks.record(BlockNumber(0), one),
            Err(HostError::BlockNumbersExhausted)
        );
        assert_eq!(blocks.resolve(BlockNumber(0)), None);
        assert_eq!(
            blocks.resolve(BlockNumber(u64::MAX)),
            Some(SnapshotId::GENESIS)
        );
        assert_eq!(blocks.len(), 1);
    }

    #[test]
    fn blocks_are_consecutive_and_map_to_increasing_snapshots() {
        let mut blocks = BlockMap::new();
        assert_eq!(blocks.next_block(), None);
        assert_eq!(blocks.resolve(BlockNumber(0)), None);
        blocks.record(BlockNumber(7), SnapshotId::GENESIS).unwrap();
        assert_eq!(blocks.next_block(), Some(BlockNumber(8)));
        assert_eq!(
            blocks.record(BlockNumber(9), SnapshotId::GENESIS),
            Err(HostError::NonConsecutiveBlock {
                block: BlockNumber(9),
                expected: BlockNumber(8)
            })
        );
        assert_eq!(
            blocks.record(BlockNumber(8), SnapshotId::GENESIS),
            Err(HostError::NonMonotonicSnapshot {
                snapshot: SnapshotId::GENESIS,
                previous: SnapshotId::GENESIS
            })
        );
        assert_eq!(blocks.resolve(BlockNumber(6)), None);
        assert_eq!(blocks.resolve(BlockNumber(7)), Some(SnapshotId::GENESIS));
        assert_eq!(blocks.resolve(BlockNumber(8)), None);
        assert_eq!(blocks.latest(), Some((BlockNumber(7), SnapshotId::GENESIS)));
        assert_eq!(blocks.len(), 1);
    }
}
