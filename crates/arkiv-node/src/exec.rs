//! Block execution: one function for production and replay.
//!
//! [`execute_block`] runs a block's recorded inputs on a fresh branch of
//! the parent state and returns the sealed candidate, the outcomes and the
//! header. Replay calls it with a block's recorded timestamp and inputs and
//! must reproduce the recorded outcome and header bytes. The timestamp is
//! only a header field: execution never reads it.
//!
//! Order within a block:
//!
//! 1. The inputs are checked statically ([`crate::validate::check_block`])
//!    and the block must follow its parent.
//! 2. Admin inputs, in order. `SetPricing` is checked against the expiry
//!    reservation and committed into the candidate, in force from the next
//!    block; this block keeps the parent's schedule.
//! 3. The expiry sweep: every record due at this height is deleted through
//!    the host path, namespace by namespace, before any user transaction.
//! 4. User transactions, in order, each under a checkpoint. A transaction
//!    whose operation fails is rolled back (its receipts and charged units
//!    stay), its remaining operations are not executed, and execution
//!    continues with the next transaction.
//!
//! # Expiry schedule
//!
//! The host-state map holds the due schedule, maintained in the same
//! branch (so checkpoints roll it back with the records):
//!
//! - `b'D' ‖ height (8 bytes BE) ‖ namespace (8 bytes BE) ‖ 15 zero bytes`
//!   → the number of records of that namespace expiring at that height;
//! - `b'T' ‖ height (8 bytes BE) ‖ 23 zero bytes` → the number of records
//!   expiring at that height over all namespaces.
//!
//! Counts are `u64` big-endian, never stored as zero (the entry is removed
//! instead), and changed with checked arithmetic; a missing or zero count
//! that should be decremented is [`ScheduleFault::Underflow`]. The sweep
//! visits only the due entries at or below the block height, in (height,
//! namespace) order, never other namespaces; it deletes exactly the counted
//! records and removes the entries. Any mismatch is
//! [`ExecError::ScheduleInconsistent`].
//!
//! # Failures
//!
//! [`ExecError`] means no block: the candidate is discarded. A storage
//! error is an availability failure, never a rejection or an empty read;
//! a schedule inconsistency, an expiry halt or a broken invariant means
//! the chain's own state or parameters are wrong, and the producer must
//! stop with an observable error rather than publish a block. Under a
//! validated genesis the last three are unreachable.

use std::{collections::BTreeMap, fmt};

use arkiv_db::{
    BlockNumber, Branch, Budget, CellKind, CellName, Cost, EngineError, HostKey, NamespaceId,
    OwnerId, Projection, RecordKey, Rejection, SealedCandidate, StorageError, Value,
    host::try_expire_records_observed,
};

use crate::{
    block::{
        AdminAction, AdminInput, AdminOutcome, AdminRejection, BlockInputs, BlockOutcomes, Expiry,
        ExpiryDeletion, ExpiryFault, Header, HostFault, OpOutcome, TxEnvelope, TxOp, TxOutcome,
        TxStatus, expires_at_name,
    },
    params::{ChainSpec, ReservationFault, check_reservation, sweep_read_bound},
    validate::{self, BlockFault},
};

const DUE_TAG: u8 = b'D';
const TOTAL_TAG: u8 = b'T';
/// Due entries read per host-map range call of the sweep.
const SWEEP_BATCH: usize = 64;

/// The host-map key of the due count of `namespace` at `height`.
pub fn due_key(height: u64, namespace: NamespaceId) -> HostKey {
    let mut key = [0u8; 32];
    key[0] = DUE_TAG;
    key[1..9].copy_from_slice(&height.to_be_bytes());
    key[9..17].copy_from_slice(&namespace.to_bytes());
    key
}

/// The host-map key of the total due count at `height`.
pub fn total_key(height: u64) -> HostKey {
    let mut key = [0u8; 32];
    key[0] = TOTAL_TAG;
    key[1..9].copy_from_slice(&height.to_be_bytes());
    key
}

/// A block executed on a branch: ready for publication (and, in step N4,
/// for a durable commit).
#[derive(Debug)]
pub struct ExecutedBlock {
    /// The sealed terminal state.
    pub candidate: SealedCandidate,
    /// What the inputs produced.
    pub outcomes: BlockOutcomes,
    /// The block's header.
    pub header: Header,
}

/// How the expiry schedule in host state disagrees with itself or with the
/// records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScheduleFault {
    /// A due or total entry has a malformed key or a value that is not a
    /// nonzero 8-byte count.
    MalformedEntry {
        /// The entry's key.
        key: HostKey,
    },
    /// A count to decrement is missing.
    Underflow {
        /// The height.
        height: u64,
        /// The namespace.
        namespace: NamespaceId,
    },
    /// A count does not fit in 64 bits.
    Overflow,
    /// The sweep deleted a different number of records than were due.
    CountMismatch {
        /// The height.
        height: u64,
        /// The namespace.
        namespace: NamespaceId,
        /// The due count.
        due: u64,
        /// The records deleted.
        deleted: u64,
    },
    /// A height's total differs from the sum of its due counts.
    TotalMismatch {
        /// The height.
        height: u64,
        /// The stored total (0 if absent).
        total: u64,
        /// The sum of the due counts.
        due: u64,
    },
    /// A total remains at a swept height without due counts.
    OrphanTotal {
        /// The height.
        height: u64,
    },
}

/// A broken internal invariant: unreachable under a validated genesis and
/// a correctly begun branch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Invariant {
    /// The branch was not begun fresh with the chain's block budget and
    /// limits.
    Branch,
    /// A statically valid operation was refused at engine admission.
    AdmissionRejected {
        /// The transaction.
        tx: u32,
        /// The operation.
        op: u32,
        /// The engine's reason.
        reason: Rejection,
    },
    /// A record's `$expiresAt` cell is not a `U64` attribute.
    MalformedExpiryCell {
        /// The namespace.
        namespace: NamespaceId,
    },
    /// The host's own read of a record's expiry was refused.
    ExpiryReadRejected(Rejection),
    /// The sweep read bound overflows under the schedule in force.
    SweepBound,
    /// An engine lifecycle call failed.
    Engine(EngineError),
    /// The host map refused a count.
    HostValue,
    /// The block's spend differs from the sum of its outcomes.
    Spent,
}

/// Why a block could not be executed. The candidate is discarded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecError {
    /// Invalid account transition invalidates the containing block.
    Account(crate::accounts::AccountFault),
    /// The storage beneath the state failed: an availability error.
    Storage(StorageError),
    /// The expiry schedule is inconsistent.
    ScheduleInconsistent(ScheduleFault),
    /// The expiry sweep stopped at an engine rejection.
    ExpiryHalted {
        /// The due height being swept.
        height: u64,
        /// The namespace being swept.
        namespace: NamespaceId,
        /// Records of that entry deleted before the halt.
        deleted: usize,
        /// The rejection.
        reason: Rejection,
    },
    /// A broken invariant.
    Invariant(Invariant),
    /// The block's inputs or position are invalid; nothing was executed.
    InvalidBlock(BlockFault),
}

impl fmt::Display for ExecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Account(fault) => write!(formatter, "account: {fault:?}"),
            Self::Storage(error) => write!(formatter, "storage unavailable: {error}"),
            Self::ScheduleInconsistent(fault) => {
                write!(formatter, "expiry schedule inconsistent: {fault:?}")
            }
            Self::ExpiryHalted {
                height,
                namespace,
                deleted,
                reason,
            } => write!(
                formatter,
                "expiry of height {height} in {namespace} halted after {deleted} deletes: {reason}"
            ),
            Self::Invariant(invariant) => write!(formatter, "invariant broken: {invariant:?}"),
            Self::InvalidBlock(fault) => write!(formatter, "invalid block: {fault}"),
        }
    }
}

impl std::error::Error for ExecError {}

impl From<StorageError> for ExecError {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}

/// A record's lifetime. `Until(E)` expires (is deleted) in block `E`.
/// Ordered with every finite height before `Permanent`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Lifetime {
    Until(u64),
    Permanent,
}

impl Lifetime {
    /// The stored `$expiresAt` value; permanent is `U64(u64::MAX)`.
    const fn stored(self) -> Value {
        match self {
            Self::Until(height) => Value::U64(height),
            Self::Permanent => Value::U64(u64::MAX),
        }
    }
}

/// Resolves an expiry pair at `height`: `E = max(expires_at, height +
/// min_lifetime)`; `E == u64::MAX` is permanent; otherwise `E` must be after
/// this block and at most `max_lifetime` blocks away.
fn resolve(expiry: &Expiry, height: u64, max_lifetime: u64) -> Result<Lifetime, ExpiryFault> {
    let floor = height
        .checked_add(expiry.min_lifetime)
        .ok_or(ExpiryFault::Overflow)?;
    let resolved = expiry.expires_at.max(floor);
    if resolved == u64::MAX {
        return Ok(Lifetime::Permanent);
    }
    if resolved <= height {
        return Err(ExpiryFault::DeadOnArrival);
    }
    if height
        .checked_add(max_lifetime)
        .is_some_and(|limit| resolved > limit)
    {
        return Err(ExpiryFault::TooFar);
    }
    Ok(Lifetime::Until(resolved))
}

fn position(index: usize) -> u32 {
    u32::try_from(index).expect("block counts fit in u32")
}

/// Executes `inputs` as the block at `height` on `branch`, which must have
/// been begun on `parent`'s state with the chain's block budget and have
/// executed nothing yet. See the [module docs](self) for the order and the
/// failure rules.
pub fn execute_block(
    spec: &ChainSpec,
    branch: Branch,
    parent: &Header,
    height: u64,
    timestamp_ms: u64,
    inputs: &BlockInputs,
) -> Result<ExecutedBlock, ExecError> {
    execute_block_observed(
        spec,
        branch,
        parent,
        height,
        timestamp_ms,
        inputs,
        |_, _, _| Ok(()),
    )
}

/// A successful expiry deletion's key, observed without additional state reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExpiredRecord {
    pub namespace: NamespaceId,
    pub id: arkiv_db::RecordId,
    pub key: RecordKey,
}

/// Executes once, then lets a bounded metadata collector inspect terminal state
/// before sealing. Observer reads are noncanonical and do not alter receipts.
#[allow(clippy::too_many_arguments)]
pub fn execute_block_observed(
    spec: &ChainSpec,
    branch: Branch,
    parent: &Header,
    height: u64,
    timestamp_ms: u64,
    inputs: &BlockInputs,
    observer: impl FnMut(&Branch, &BlockOutcomes, &[ExpiredRecord]) -> Result<(), StorageError>,
) -> Result<ExecutedBlock, ExecError> {
    execute_block_hooked(
        spec,
        branch,
        parent,
        height,
        timestamp_ms,
        inputs,
        None,
        |_, _, _| Ok(()),
        observer,
    )
}

/// Internal host bookkeeping after each transaction's rollback decision.
/// Unsigned chains use a no-op; signed hosts advance the account nonce.
#[allow(clippy::too_many_arguments)]
pub(crate) fn execute_block_hooked(
    spec: &ChainSpec,
    branch: Branch,
    parent: &Header,
    height: u64,
    timestamp_ms: u64,
    inputs: &BlockInputs,
    accounts: Option<&crate::accounts::AccountConfig>,
    after_tx: impl FnMut(&mut Branch, usize, &TxEnvelope) -> Result<(), StorageError>,
    observer: impl FnMut(&Branch, &BlockOutcomes, &[ExpiredRecord]) -> Result<(), StorageError>,
) -> Result<ExecutedBlock, ExecError> {
    execute_block_inner(
        spec,
        branch,
        parent,
        height,
        timestamp_ms,
        inputs,
        accounts,
        false,
        after_tx,
        observer,
    )
}

/// Estimate with ordinary gas/rent and operation validation, but without caller
/// affordability or fee settlement. The candidate is discarded here so this
/// path cannot publish its synthetic account state as a real block.
pub(crate) fn estimate_transaction(
    spec: &ChainSpec,
    branch: Branch,
    parent: &Header,
    height: u64,
    timestamp_ms: u64,
    tx: TxEnvelope,
    accounts: Option<&crate::accounts::AccountConfig>,
) -> Result<TxOutcome, ExecError> {
    let executed = execute_block_inner(
        spec,
        branch,
        parent,
        height,
        timestamp_ms,
        &BlockInputs {
            admin: vec![],
            txs: vec![tx],
        },
        accounts,
        true,
        |_, _, _| Ok(()),
        |_, _, _| Ok(()),
    )?;
    Ok(executed
        .outcomes
        .txs
        .into_iter()
        .next()
        .expect("one estimated transaction"))
}

#[allow(clippy::too_many_arguments)]
fn execute_block_inner(
    spec: &ChainSpec,
    branch: Branch,
    parent: &Header,
    height: u64,
    timestamp_ms: u64,
    inputs: &BlockInputs,
    accounts: Option<&crate::accounts::AccountConfig>,
    estimate: bool,
    mut after_tx: impl FnMut(&mut Branch, usize, &TxEnvelope) -> Result<(), StorageError>,
    mut observer: impl FnMut(&Branch, &BlockOutcomes, &[ExpiredRecord]) -> Result<(), StorageError>,
) -> Result<ExecutedBlock, ExecError> {
    validate::check_successor(spec, parent, height, timestamp_ms)
        .map_err(|fault| ExecError::InvalidBlock(BlockFault::Successor(fault)))?;
    let fresh = branch.block_limit() == Cost::new(spec.params().block_budget)
        && branch.limits() == *spec.limits()
        && branch.receipts().is_empty()
        && branch.spent() == Cost::ZERO;
    if !fresh {
        return Err(ExecError::Invariant(Invariant::Branch));
    }
    validate::check_block(spec, inputs).map_err(ExecError::InvalidBlock)?;
    let mut executor = Executor {
        spec,
        accounts,
        estimate,
        branch,
        height,
        expires_at: expires_at_name(),
        expired: Vec::new(),
    };
    let admin = inputs
        .admin
        .iter()
        .map(|input| executor.admin(input))
        .collect::<Result<Vec<_>, _>>()?;
    let expiry = executor.sweep()?;
    let txs = inputs
        .txs
        .iter()
        .enumerate()
        .map(|(index, tx)| {
            let outcome = executor.tx(position(index), tx)?;
            after_tx(&mut executor.branch, index, tx)?;
            Ok::<_, ExecError>(outcome)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let spent = executor.branch.spent().units();
    let accounted = expiry
        .iter()
        .map(|deletion| deletion.receipt.spent.units())
        .chain(txs.iter().map(|tx| tx.spent))
        .try_fold(0u64, u64::checked_add);
    if accounted != Some(spent) {
        return Err(ExecError::Invariant(Invariant::Spent));
    }
    let outcomes = BlockOutcomes {
        admin,
        expiry,
        txs,
        spent,
    };
    observer(&executor.branch, &outcomes, &executor.expired)?;
    let candidate = executor
        .branch
        .try_seal()
        .map_err(|e| ExecError::Invariant(Invariant::Engine(e)))?;
    let header = Header {
        chain_id: spec.chain_id(),
        height,
        parent_hash: parent.hash(),
        timestamp_ms,
        inputs_digest: inputs.digest(),
        outcomes_digest: outcomes.digest(),
        state_root: candidate.state_root(),
    };
    Ok(ExecutedBlock {
        candidate,
        outcomes,
        header,
    })
}

/// The outcome of one operation: applied, or the transaction fails here.
enum Step {
    Applied(OpOutcome),
    Failed(OpOutcome),
}

struct Executor<'s> {
    spec: &'s ChainSpec,
    accounts: Option<&'s crate::accounts::AccountConfig>,
    estimate: bool,
    branch: Branch,
    height: u64,
    expires_at: CellName,
    expired: Vec<ExpiredRecord>,
}

impl Executor<'_> {
    // -----------------------------------------------------------------------
    // Admin inputs.

    fn admin(&mut self, input: &AdminInput) -> Result<AdminOutcome, ExecError> {
        let AdminAction::SetPricing(schedule) = &input.action;
        if crate::gas::is_active(self.branch.pricing()) || crate::gas::is_active(schedule) {
            return Ok(AdminOutcome::Rejected(AdminRejection::GasProfileLocked));
        }
        match check_reservation(self.spec.params(), self.spec.limits(), schedule) {
            Ok(()) => {}
            Err(ReservationFault::Violated { .. }) => {
                return Ok(AdminOutcome::Rejected(AdminRejection::ReservationViolated));
            }
            Err(ReservationFault::Overflow) => {
                return Ok(AdminOutcome::Rejected(AdminRejection::ReservationOverflow));
            }
        }
        match self.branch.set_pricing(schedule.clone()) {
            Ok(()) => Ok(AdminOutcome::Applied),
            Err(EngineError::StaleSchedule { current }) => {
                Ok(AdminOutcome::Rejected(AdminRejection::StaleSchedule {
                    current,
                }))
            }
            Err(other) => Err(ExecError::Invariant(Invariant::Engine(other))),
        }
    }

    // -----------------------------------------------------------------------
    // The due schedule.

    fn read_count(&self, key: &HostKey) -> Result<u64, ExecError> {
        match self.branch.try_host_get(key)? {
            None => Ok(0),
            Some(value) => parse_count(key, &value),
        }
    }

    fn write_count(&mut self, key: HostKey, count: u64) -> Result<(), ExecError> {
        if count == 0 {
            self.branch.try_host_remove(&key)?;
        } else {
            self.branch
                .try_host_put(key, &count.to_be_bytes())?
                .map_err(|_| ExecError::Invariant(Invariant::HostValue))?;
        }
        Ok(())
    }

    /// Adds one record due at `height` in `namespace`.
    fn schedule(&mut self, height: u64, namespace: NamespaceId) -> Result<(), ExecError> {
        for key in [due_key(height, namespace), total_key(height)] {
            let count = self
                .read_count(&key)?
                .checked_add(1)
                .ok_or(ExecError::ScheduleInconsistent(ScheduleFault::Overflow))?;
            self.write_count(key, count)?;
        }
        Ok(())
    }

    /// Removes one record due at `height` in `namespace`.
    fn unschedule(&mut self, height: u64, namespace: NamespaceId) -> Result<(), ExecError> {
        for key in [due_key(height, namespace), total_key(height)] {
            let count =
                self.read_count(&key)?
                    .checked_sub(1)
                    .ok_or(ExecError::ScheduleInconsistent(ScheduleFault::Underflow {
                        height,
                        namespace,
                    }))?;
            self.write_count(key, count)?;
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // The expiry sweep.

    fn sweep(&mut self) -> Result<Vec<ExpiryDeletion>, ExecError> {
        let bound = sweep_read_bound(self.branch.pricing(), self.spec.limits())
            .ok_or(ExecError::Invariant(Invariant::SweepBound))?;
        // The smallest due key: every due entry sorts at or after it, in
        // (height, namespace) order, and before every total entry.
        let mut start = [0u8; 32];
        start[0] = DUE_TAG;
        let mut deletions = Vec::new();
        let mut swept: BTreeMap<u64, u64> = BTreeMap::new();
        loop {
            let mut due = Vec::new();
            for (key, value) in self.branch.try_host_range(&start, SWEEP_BATCH)? {
                if key[0] != DUE_TAG {
                    break;
                }
                let (height, namespace) = parse_due_key(&key)?;
                if height > self.height {
                    break;
                }
                due.push((key, height, namespace, parse_count(&key, &value)?));
            }
            if due.is_empty() {
                break;
            }
            for (key, height, namespace, count) in due {
                let expired = &mut self.expired;
                let cap = self.spec.params().expiry_per_block as usize;
                let deleted = match try_expire_records_observed(
                    &mut self.branch,
                    namespace,
                    &self.expires_at,
                    BlockNumber(height),
                    Budget::limit(bound),
                    Budget::unlimited(),
                    &mut |id, record_key, _| {
                        if expired.len() >= cap {
                            return Err(StorageError::new(
                                arkiv_db::StorageErrorKind::Corrupt,
                                "expiry metadata exceeds genesis capacity",
                            ));
                        }
                        expired.push(ExpiredRecord {
                            namespace,
                            id,
                            key: record_key.clone(),
                        });
                        Ok(())
                    },
                )? {
                    Ok(deleted) => deleted,
                    Err(halt) => {
                        return Err(ExecError::ExpiryHalted {
                            height,
                            namespace,
                            deleted: halt.applied.len(),
                            reason: halt.rejected.reason,
                        });
                    }
                };
                let deleted_count = u64::try_from(deleted.len()).expect("usize fits in u64");
                if deleted_count != count {
                    return Err(ExecError::ScheduleInconsistent(
                        ScheduleFault::CountMismatch {
                            height,
                            namespace,
                            due: count,
                            deleted: deleted_count,
                        },
                    ));
                }
                deletions.extend(
                    deleted
                        .into_iter()
                        .map(|(record_id, receipt)| ExpiryDeletion {
                            namespace,
                            record_id,
                            receipt,
                        }),
                );
                self.branch.try_host_remove(&key)?;
                let sum = swept.entry(height).or_insert(0);
                *sum = sum
                    .checked_add(count)
                    .ok_or(ExecError::ScheduleInconsistent(ScheduleFault::Overflow))?;
            }
        }
        for (height, due) in swept {
            let key = total_key(height);
            let total = self.read_count(&key)?;
            if total != due {
                return Err(ExecError::ScheduleInconsistent(
                    ScheduleFault::TotalMismatch { height, total, due },
                ));
            }
            self.branch.try_host_remove(&key)?;
        }
        if let Some((key, _)) = self
            .branch
            .try_host_range(&total_key(0), 1)?
            .into_iter()
            .next()
            && key[0] == TOTAL_TAG
        {
            let height = parse_total_key(&key)?;
            if height <= self.height {
                return Err(ExecError::ScheduleInconsistent(
                    ScheduleFault::OrphanTotal { height },
                ));
            }
        }
        Ok(deletions)
    }

    // -----------------------------------------------------------------------
    // User transactions.

    fn tx(&mut self, index: u32, tx: &TxEnvelope) -> Result<TxOutcome, ExecError> {
        let charged_accounts = self.accounts.filter(|_| !self.estimate);
        if let Some(accounts) = charged_accounts {
            accounts.reserve(&mut self.branch, tx)?;
        }
        let checkpoint = self.branch.checkpoint();
        let mut ops = Vec::with_capacity(tx.ops.len());
        let mut spent = 0u64;
        let mut failed = None;
        if crate::gas::is_active(self.branch.pricing()) {
            match self
                .branch
                .try_charge_host(crate::gas::TX_INTRINSIC_GAS, Budget::limit(tx.tx_budget))?
            {
                Ok(receipt) => spent = receipt.spent.units(),
                Err(rejected) => {
                    ops.push(OpOutcome::Rejected {
                        reason: rejected.reason,
                        receipt: rejected.receipt,
                    });
                    ops.resize(tx.ops.len(), OpOutcome::NotExecuted);
                    if let Some(accounts) = charged_accounts {
                        accounts.settle(&mut self.branch, tx, 0)?;
                    }
                    return Ok(TxOutcome {
                        status: TxStatus::Failed { op_index: 0 },
                        ops,
                        spent: 0,
                    });
                }
            }
        }
        for (op_index, op) in tx.ops.iter().enumerate() {
            let remaining = tx.tx_budget.saturating_sub(spent);
            let op_index = position(op_index);
            let step = self.op(
                index,
                op_index,
                tx.actor,
                op,
                remaining,
                tx.ops.len() == 1 && spent == crate::gas::TX_INTRINSIC_GAS,
            )?;
            let (outcome, fails) = match step {
                Step::Applied(outcome) => (outcome, false),
                Step::Failed(outcome) => (outcome, true),
            };
            spent = spent
                .checked_add(outcome.spent())
                .ok_or(ExecError::Invariant(Invariant::Spent))?;
            ops.push(outcome);
            if fails {
                failed = Some(op_index);
                break;
            }
        }
        let status = match failed {
            None => TxStatus::Committed,
            Some(op_index) => {
                self.branch
                    .rollback_to(checkpoint)
                    .map_err(|error| ExecError::Invariant(Invariant::Engine(error)))?;
                for outcome in &mut ops {
                    if let OpOutcome::Applied { effect, receipt } = outcome {
                        *outcome = OpOutcome::RolledBack {
                            effect: *effect,
                            receipt: receipt.clone(),
                        };
                    }
                }
                ops.resize(tx.ops.len(), OpOutcome::NotExecuted);
                TxStatus::Failed { op_index }
            }
        };
        if let Some(accounts) = charged_accounts {
            accounts.settle(&mut self.branch, tx, spent)?;
        }
        Ok(TxOutcome { status, ops, spent })
    }

    /// The lifetime of the live record `key` in `namespace` before the
    /// operation: `None` if no record is live (or the namespace does not
    /// exist; the engine then rejects the operation itself). A read through
    /// the host's own view with an unlimited budget; its receipt is
    /// discarded. It costs at most
    /// [`max_expiry_read_cost`](crate::params::max_expiry_read_cost), which
    /// every accepted schedule keeps representable, so it cannot overflow.
    fn current_lifetime(
        &self,
        namespace: NamespaceId,
        key: &RecordKey,
    ) -> Result<Option<Lifetime>, ExecError> {
        let projection = Projection::named(false, [self.expires_at.clone()]);
        let row =
            match self
                .branch
                .try_get_record(namespace, key, &projection, Budget::unlimited())?
            {
                Ok(read) => read.value,
                Err(rejected) if rejected.reason == Rejection::UnknownNamespace => None,
                Err(rejected) => {
                    return Err(ExecError::Invariant(Invariant::ExpiryReadRejected(
                        rejected.reason,
                    )));
                }
            };
        let Some(row) = row else {
            return Ok(None);
        };
        match row.cells.first() {
            None => Ok(Some(Lifetime::Permanent)),
            Some((_, cell)) => match (&cell.kind, &cell.value) {
                (CellKind::Attribute, Value::U64(u64::MAX)) => Ok(Some(Lifetime::Permanent)),
                (CellKind::Attribute, Value::U64(height)) => Ok(Some(Lifetime::Until(*height))),
                _ => Err(ExecError::Invariant(Invariant::MalformedExpiryCell {
                    namespace,
                })),
            },
        }
    }

    fn op(
        &mut self,
        tx: u32,
        op_index: u32,
        actor: OwnerId,
        op: &TxOp,
        remaining: u64,
        intrinsic_covers_transfer: bool,
    ) -> Result<Step, ExecError> {
        if let TxOp::Transfer { to, amount } = op {
            let Some(accounts) = self.accounts else {
                return Ok(Step::Failed(crate::accounts::unsupported()));
            };
            if self.estimate {
                // Only the disposable caller balance is overridden. The same
                // transfer still charges gas and checks recipient overflow.
                let mut sender = accounts.read(&self.branch, actor)?;
                if sender.balance < *amount {
                    sender.balance = *amount;
                    crate::accounts::put(&mut self.branch, actor, sender)?;
                }
            }
            let outcome = accounts.transfer(
                &mut self.branch,
                actor,
                *to,
                *amount,
                remaining,
                intrinsic_covers_transfer,
            )?;
            return Ok(if matches!(outcome, OpOutcome::Applied { .. }) {
                Step::Applied(outcome)
            } else {
                Step::Failed(outcome)
            });
        }
        if let TxOp::Arkiv {
            namespace,
            operation,
        } = op
        {
            let context = crate::arkiv::ArkivExecutionContext {
                caller: *actor.as_bytes(),
                block_height: self.height,
                chain_id: self.spec.chain_id(),
                max_lifetime: self.spec.params().max_lifetime,
                expiry_per_block: self.spec.params().expiry_per_block,
            };
            let outcome = crate::arkiv::execute_operation(
                &mut self.branch,
                *namespace,
                &context,
                operation,
                Budget::limit(remaining),
            )?;
            return Ok(if matches!(outcome, OpOutcome::Applied { .. }) {
                Step::Applied(outcome)
            } else {
                Step::Failed(outcome)
            });
        }
        if crate::gas::is_active(self.branch.pricing())
            // Namespace creation pays the intrinsic fee plus the existing
            // metered directory/create/root charges. Other generic writes stay
            // disabled: they do not implement the Arkiv storage-rent contract.
            && !matches!(op, TxOp::CreateNamespace { .. })
        {
            return Ok(Step::Failed(OpOutcome::HostRejected {
                fault: HostFault::Arkiv(crate::arkiv::ArkivFault::UnsupportedGasOperation),
            }));
        }
        // The expiry pre-step: the record's current lifetime, for every
        // operation that sets an expiry or patches or deletes a record.
        let target = match op {
            TxOp::CreateRecord {
                namespace,
                key,
                expiry: Some(_),
                ..
            }
            | TxOp::PatchRecord { namespace, key, .. }
            | TxOp::DeleteRecord { namespace, key } => Some((*namespace, key)),
            _ => None,
        };
        let old = match target {
            Some((namespace, key)) => self.current_lifetime(namespace, key)?,
            None => None,
        };
        let new = match op.expiry() {
            None => None,
            Some(expiry) => match self.admit_expiry(op, expiry, old)? {
                Ok(lifetime) => Some(lifetime),
                Err(fault) => return Ok(Step::Failed(OpOutcome::HostRejected { fault })),
            },
        };
        let write = op.to_write_op(new.map(Lifetime::stored));
        let recorded = self.branch.receipts().len();
        let executed = match self
            .branch
            .try_execute(actor, write, Budget::limit(remaining))?
        {
            Ok(executed) => executed,
            Err(rejected) if self.branch.receipts().len() == recorded => {
                return Err(ExecError::Invariant(Invariant::AdmissionRejected {
                    tx,
                    op: op_index,
                    reason: rejected.reason,
                }));
            }
            Err(rejected) => {
                return Ok(Step::Failed(OpOutcome::Rejected {
                    reason: rejected.reason,
                    receipt: rejected.receipt,
                }));
            }
        };
        // Maintain the due schedule for the applied change.
        if let Some(namespace) = op.namespace() {
            let changed = match op {
                TxOp::DeleteRecord { .. } => Some((old, None)),
                _ if new.is_some() && new != old => Some((old, new)),
                _ => None,
            };
            if let Some((before, after)) = changed {
                if let Some(Lifetime::Until(height)) = before {
                    self.unschedule(height, namespace)?;
                }
                if let Some(Lifetime::Until(height)) = after {
                    self.schedule(height, namespace)?;
                }
            }
        }
        Ok(Step::Applied(OpOutcome::Applied {
            effect: executed.effect,
            receipt: executed.receipt,
        }))
    }

    /// Resolves and admits an operation's expiry against the record's
    /// current lifetime `old`: resolution (overflow, dead on arrival, too
    /// far), extend-only for patches, then the capacity of the resolved
    /// height, `total(E) + 1 − [old == E] ≤ expiry_per_block`.
    fn admit_expiry(
        &self,
        op: &TxOp,
        expiry: &Expiry,
        old: Option<Lifetime>,
    ) -> Result<Result<Lifetime, HostFault>, ExecError> {
        let lifetime = match resolve(expiry, self.height, self.spec.params().max_lifetime) {
            Ok(lifetime) => lifetime,
            Err(fault) => return Ok(Err(HostFault::ExpiryInvalid(fault))),
        };
        if matches!(op, TxOp::PatchRecord { .. }) && old.is_some_and(|old| lifetime < old) {
            return Ok(Err(HostFault::ExpiryShortened));
        }
        if let Lifetime::Until(height) = lifetime {
            let unchanged = u64::from(old == Some(lifetime));
            let after = self
                .read_count(&total_key(height))?
                .checked_add(1)
                .ok_or(ExecError::ScheduleInconsistent(ScheduleFault::Overflow))?
                - unchanged;
            if after > u64::from(self.spec.params().expiry_per_block) {
                return Ok(Err(HostFault::ExpiryCapacityFull));
            }
        }
        Ok(Ok(lifetime))
    }
}

fn parse_count(key: &HostKey, value: &[u8]) -> Result<u64, ExecError> {
    let malformed = || ExecError::ScheduleInconsistent(ScheduleFault::MalformedEntry { key: *key });
    let bytes: [u8; 8] = value.try_into().map_err(|_| malformed())?;
    match u64::from_be_bytes(bytes) {
        0 => Err(malformed()),
        count => Ok(count),
    }
}

fn parse_due_key(key: &HostKey) -> Result<(u64, NamespaceId), ExecError> {
    let malformed = || ExecError::ScheduleInconsistent(ScheduleFault::MalformedEntry { key: *key });
    if key[17..].iter().any(|&byte| byte != 0) {
        return Err(malformed());
    }
    let height = u64::from_be_bytes(key[1..9].try_into().expect("8 bytes"));
    let namespace =
        NamespaceId::from_bytes(key[9..17].try_into().expect("8 bytes")).ok_or_else(malformed)?;
    Ok((height, namespace))
}

fn parse_total_key(key: &HostKey) -> Result<u64, ExecError> {
    if key[9..].iter().any(|&byte| byte != 0) {
        return Err(ExecError::ScheduleInconsistent(
            ScheduleFault::MalformedEntry { key: *key },
        ));
    }
    Ok(u64::from_be_bytes(key[1..9].try_into().expect("8 bytes")))
}
