//! Static validation: every check on a request that depends only on the
//! request and the chain definition, never on state.
//!
//! The node applies these checks before it accepts a request into its
//! queue, and the executor applies them again to every block it executes,
//! so replay rejects a recorded block that contains a request the node
//! would have refused. An operation is validated **as it will execute**:
//! when it carries an expiry, a placeholder `$expiresAt = U64(0)` attribute
//! is injected first, so a user gets at most `max_cells − 1` cells with an
//! expiry, and an expiry-only patch is a valid (non-empty) patch. The
//! engine's own argument validation ([`arkiv_db::WriteOp::validate`]) is
//! reused, so an operation that passes here is never refused by the
//! engine at admission.

use std::fmt;

use arkiv_db::{ArgumentFault, CellName, Rejection, Value, encode};

use crate::{
    block::{AdminInput, BlockInputs, Expiry, Header, TxEnvelope, TxOp},
    identity::{RequestIdentity, Scope},
    params::ChainSpec,
    select::{self, SelectionFault},
};

/// Why an expiry pair is refused statically.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExpiryArgumentFault {
    /// `expires_at == 0 && min_lifetime == 0`: no expiry requested.
    Zero,
    /// `min_lifetime > max_lifetime`.
    LifetimeTooLong,
}

/// Why a transaction envelope is refused statically.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxFault {
    /// The envelope names another chain.
    WrongChain {
        /// The chain id found.
        found: u64,
    },
    /// The budget is not in `1..=user_budget`.
    Budget,
    /// The operation count is not in `1..=max_ops_per_tx`.
    OpCount,
    /// The encoded envelope is longer than `max_tx_bytes`.
    TooLarge {
        /// Its encoded length.
        bytes: usize,
    },
    /// Operation `op` names a cell starting with `$` (host-owned).
    HostOwnedCell {
        /// Index of the operation.
        op: u32,
    },
    /// Operation `op` carries an invalid expiry pair.
    Expiry {
        /// Index of the operation.
        op: u32,
        /// Why.
        fault: ExpiryArgumentFault,
    },
    /// Operation `op`, as it will execute, fails the engine's argument
    /// validation.
    Argument {
        /// Index of the operation.
        op: u32,
        /// Why.
        fault: ArgumentFault,
    },
}

impl fmt::Display for TxFault {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongChain { found } => write!(formatter, "transaction for chain {found}"),
            Self::Budget => write!(formatter, "transaction budget out of range"),
            Self::OpCount => write!(formatter, "operation count out of range"),
            Self::TooLarge { bytes } => write!(formatter, "transaction of {bytes} bytes too large"),
            Self::HostOwnedCell { op } => write!(formatter, "operation {op} names a `$` cell"),
            Self::Expiry { op, fault } => write!(formatter, "operation {op}: expiry {fault:?}"),
            Self::Argument { op, fault } => write!(formatter, "operation {op}: {fault}"),
        }
    }
}

impl std::error::Error for TxFault {}

/// Why an admin input is refused statically.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdminFault {
    /// The actor is not a genesis admin.
    NotAdmin,
}

/// Why a block's inputs are invalid. A block with invalid inputs is never
/// executed; replay rejects it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockFault {
    /// Admin input `index` is invalid.
    Admin {
        /// Its position.
        index: u32,
        /// Why.
        fault: AdminFault,
    },
    /// Transaction `index` is invalid.
    Tx {
        /// Its position.
        index: u32,
        /// Why.
        fault: TxFault,
    },
    /// The inputs break a selection limit.
    Selection(SelectionFault),
    /// A request identity appears twice in the block.
    DuplicateIdentity {
        /// Its scope.
        scope: Scope,
        /// Position of the second occurrence.
        index: u32,
    },
    /// A request identity was already included in an earlier block.
    AlreadyIncluded {
        /// Its scope.
        scope: Scope,
        /// Its position in this block.
        index: u32,
    },
    /// The header does not follow its parent.
    Successor(SuccessorFault),
}

/// How a block fails to follow its parent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SuccessorFault {
    /// The chain id differs from the genesis chain id.
    ChainId,
    /// The height is not the parent's plus one (or would overflow).
    Height,
    /// The parent hash is not the parent's block hash.
    ParentHash,
    /// The timestamp is before the parent's.
    Timestamp,
}

impl fmt::Display for BlockFault {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Admin { index, fault } => write!(formatter, "admin input {index}: {fault:?}"),
            Self::Tx { index, fault } => write!(formatter, "transaction {index}: {fault}"),
            Self::Selection(fault) => write!(formatter, "selection: {fault}"),
            Self::DuplicateIdentity { scope, index } => {
                write!(formatter, "{scope} request {index} repeats an identity")
            }
            Self::AlreadyIncluded { scope, index } => {
                write!(formatter, "{scope} request {index} is already included")
            }
            Self::Successor(fault) => write!(formatter, "not a successor: {fault:?}"),
        }
    }
}

impl std::error::Error for BlockFault {}

fn index(position: usize) -> u32 {
    u32::try_from(position).expect("positions are bounded by u32 counts")
}

fn host_owned(name: &CellName) -> bool {
    name.as_str().starts_with('$')
}

/// The static expiry checks of a generic record: not `(0, 0)`, and
/// `min_lifetime ≤ max_lifetime`. Arkiv entities only refuse `(0, 0)`.
pub fn check_expiry(spec: &ChainSpec, expiry: &Expiry) -> Result<(), ExpiryArgumentFault> {
    if expiry.expires_at == 0 && expiry.min_lifetime == 0 {
        return Err(ExpiryArgumentFault::Zero);
    }
    if expiry.min_lifetime > spec.params().max_lifetime {
        return Err(ExpiryArgumentFault::LifetimeTooLong);
    }
    Ok(())
}

/// Validates one operation of a transaction, as it will execute.
pub fn check_op(spec: &ChainSpec, op: &TxOp) -> Result<(), OpFault> {
    if matches!(op, TxOp::Transfer { .. }) {
        return Ok(());
    }
    if let TxOp::Arkiv {
        namespace,
        operation,
    } = op
    {
        if (*namespace != crate::arkiv::entity_namespace()
            && !crate::gas::is_active(&spec.genesis().pricing))
            || operation.validate().is_err()
        {
            return Err(OpFault::HostOwnedCell);
        }
        // An entity lifetime has no upper bound (the rent cap bounds its
        // cost), so only the `(0, 0)` pair is refused statically.
        if operation
            .expiry()
            .is_some_and(|e| e.expires_at == 0 && e.min_lifetime == 0)
        {
            return Err(OpFault::Expiry(ExpiryArgumentFault::Zero));
        }
        return Ok(());
    }
    // The entity namespace is exclusively host-controlled. Generic mutation
    // would otherwise bypass per-entity ownership and immutable system cells.
    if op.namespace() == Some(crate::arkiv::entity_namespace()) {
        return Err(OpFault::HostOwnedCell);
    }
    let names_host_cell = match op {
        TxOp::CreateRecord { cells, .. } => cells.iter().any(|(name, _)| host_owned(name)),
        TxOp::PatchRecord { set, remove, .. } => {
            set.iter().any(|(name, _)| host_owned(name)) || remove.iter().any(host_owned)
        }
        _ => false,
    };
    if names_host_cell {
        return Err(OpFault::HostOwnedCell);
    }
    if let Some(expiry) = op.expiry() {
        check_expiry(spec, expiry).map_err(OpFault::Expiry)?;
    }
    let placeholder = op.expiry().map(|_| Value::U64(0));
    match op.to_write_op(placeholder).validate(spec.limits()) {
        Ok(()) => Ok(()),
        Err(Rejection::InvalidArgument(fault)) => Err(OpFault::Argument(fault)),
        Err(other) => unreachable!("argument validation only rejects arguments: {other}"),
    }
}

/// Why one operation is refused statically; see [`TxFault`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpFault {
    /// It names a `$` cell.
    HostOwnedCell,
    /// Its expiry pair is invalid.
    Expiry(ExpiryArgumentFault),
    /// It fails the engine's argument validation as it will execute.
    Argument(ArgumentFault),
}

/// Validates a transaction envelope: chain id, budget in
/// `1..=user_budget`, operation count in `1..=max_ops_per_tx`, encoded size
/// at most `max_tx_bytes`, then every operation in order.
pub fn check_tx(spec: &ChainSpec, envelope: &TxEnvelope) -> Result<(), TxFault> {
    let params = spec.params();
    if envelope.chain_id != spec.chain_id() {
        return Err(TxFault::WrongChain {
            found: envelope.chain_id,
        });
    }
    if !(1..=params.user_budget).contains(&envelope.tx_budget) {
        return Err(TxFault::Budget);
    }
    let max_ops = usize::try_from(params.max_ops_per_tx).expect("u32 fits in usize");
    if !(1..=max_ops).contains(&envelope.ops.len()) {
        return Err(TxFault::OpCount);
    }
    let bytes = encode(envelope).len();
    if bytes > usize::try_from(params.max_tx_bytes).expect("u32 fits in usize") {
        return Err(TxFault::TooLarge { bytes });
    }
    for (position, op) in envelope.ops.iter().enumerate() {
        let op_index = index(position);
        check_op(spec, op).map_err(|fault| match fault {
            OpFault::HostOwnedCell => TxFault::HostOwnedCell { op: op_index },
            OpFault::Expiry(fault) => TxFault::Expiry {
                op: op_index,
                fault,
            },
            OpFault::Argument(fault) => TxFault::Argument {
                op: op_index,
                fault,
            },
        })?;
    }
    Ok(())
}

/// Validates an admin input: its actor must be a genesis admin.
pub fn check_admin(spec: &ChainSpec, input: &AdminInput) -> Result<(), AdminFault> {
    if spec.is_admin(&input.actor) {
        Ok(())
    } else {
        Err(AdminFault::NotAdmin)
    }
}

/// Checks that a block at `height` with `timestamp_ms` may follow
/// `parent`: same chain, `height ==
/// parent.height + 1` (checked, never wrapping) and a timestamp no earlier
/// than the parent's.
pub fn check_successor(
    spec: &ChainSpec,
    parent: &Header,
    height: u64,
    timestamp_ms: u64,
) -> Result<(), SuccessorFault> {
    if parent.chain_id != spec.chain_id() {
        return Err(SuccessorFault::ChainId);
    }
    if parent.height.checked_add(1) != Some(height) {
        return Err(SuccessorFault::Height);
    }
    if timestamp_ms < parent.timestamp_ms {
        return Err(SuccessorFault::Timestamp);
    }
    Ok(())
}

/// Checks that the recorded `header` follows `parent`: every rule of
/// [`check_successor`], plus the header's own chain id and its parent hash.
pub fn check_header(
    spec: &ChainSpec,
    parent: &Header,
    header: &Header,
) -> Result<(), SuccessorFault> {
    if header.chain_id != spec.chain_id() {
        return Err(SuccessorFault::ChainId);
    }
    if header.parent_hash != parent.hash() {
        return Err(SuccessorFault::ParentHash);
    }
    check_successor(spec, parent, header.height, header.timestamp_ms)
}

/// Validates a block's inputs without state: every admin input and
/// transaction, the selection limits, and that no request identity repeats
/// within the block. Whether an identity was included by an earlier block
/// is checked against the inclusion index
/// ([`crate::RequestIndex::check_block`]).
pub fn check_block(spec: &ChainSpec, inputs: &BlockInputs) -> Result<(), BlockFault> {
    for (position, input) in inputs.admin.iter().enumerate() {
        check_admin(spec, input).map_err(|fault| BlockFault::Admin {
            index: index(position),
            fault,
        })?;
    }
    for (position, envelope) in inputs.txs.iter().enumerate() {
        check_tx(spec, envelope).map_err(|fault| BlockFault::Tx {
            index: index(position),
            fault,
        })?;
    }
    select::check(spec.params(), inputs).map_err(BlockFault::Selection)?;
    let mut seen = std::collections::BTreeSet::new();
    for (identity, scope, position) in RequestIdentity::of_block(spec.chain_id(), inputs) {
        if !seen.insert(identity) {
            return Err(BlockFault::DuplicateIdentity {
                scope,
                index: position,
            });
        }
    }
    Ok(())
}
