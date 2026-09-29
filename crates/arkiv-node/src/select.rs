//! Block selection: which queued requests the producer puts into the next
//! block, and the replay-side check of the same limits.
//!
//! The producer drains the admin queue first, in FIFO order, up to
//! `max_admin_per_block` inputs. It then takes user transactions in strict
//! FIFO order while all of these hold with the transaction added:
//!
//! - transaction count `≤ max_txs_per_block`;
//! - total operations `≤ max_ops_per_block`;
//! - `len(encode(BlockInputs)) ≤ max_block_input_bytes`;
//! - total declared `tx_budget` `≤ user_budget`;
//!
//! and stops at the first transaction that does not fit, which stays at
//! the head of the queue (no transaction overtakes another). The recorded
//! block makes the choice deterministic for replay, which checks exactly
//! these limits with [`check`]. The genesis invariants guarantee that a
//! statically valid transaction always fits an otherwise empty block (even
//! one with a full admin section), so the queue cannot stall.

use std::fmt;

use arkiv_db::encode;

use crate::{
    block::{AdminInput, BlockInputs, ChainParams, TxEnvelope},
    params::EMPTY_INPUTS_BYTES,
};

/// How many requests to take from the front of each queue.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Selection {
    /// Admin inputs taken.
    pub admin: usize,
    /// User transactions taken.
    pub txs: usize,
    /// `len(encode(BlockInputs))` of the selected inputs.
    pub bytes: u64,
    /// Total operations of the selected transactions.
    pub ops: u64,
    /// Total declared budget of the selected transactions.
    pub budget: u64,
}

/// Which selection limit a block's inputs break.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionFault {
    /// More than `max_admin_per_block` admin inputs.
    AdminCount,
    /// More than `max_txs_per_block` transactions.
    TxCount,
    /// More than `max_ops_per_block` operations.
    OpCount,
    /// Encoded inputs longer than `max_block_input_bytes`.
    Bytes,
    /// Declared budgets summing to more than `user_budget`.
    Budget,
}

impl fmt::Display for SelectionFault {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::AdminCount => "too many admin inputs",
            Self::TxCount => "too many transactions",
            Self::OpCount => "too many operations",
            Self::Bytes => "block inputs too large",
            Self::Budget => "transaction budgets exceed the user budget",
        })
    }
}

impl std::error::Error for SelectionFault {}

fn wide(value: usize) -> u64 {
    u64::try_from(value).expect("usize fits in u64")
}

/// Selects the next block's requests from the fronts of the admin and
/// transaction queues; see the [module docs](self).
pub fn select<'q>(
    params: &ChainParams,
    admin: impl IntoIterator<Item = &'q AdminInput>,
    txs: impl IntoIterator<Item = &'q TxEnvelope>,
) -> Selection {
    let mut selection = Selection {
        bytes: EMPTY_INPUTS_BYTES,
        ..Selection::default()
    };
    let max_admin = usize::try_from(params.max_admin_per_block).expect("u32 fits in usize");
    for input in admin.into_iter().take(max_admin) {
        selection.admin += 1;
        selection.bytes = selection.bytes.saturating_add(wide(encode(input).len()));
    }
    for tx in txs {
        let count = selection.txs + 1;
        let ops = selection.ops.checked_add(wide(tx.ops.len()));
        let bytes = selection.bytes.checked_add(wide(encode(tx).len()));
        let budget = selection.budget.checked_add(tx.tx_budget);
        let fits = wide(count) <= u64::from(params.max_txs_per_block)
            && ops.is_some_and(|ops| ops <= u64::from(params.max_ops_per_block))
            && bytes.is_some_and(|bytes| bytes <= u64::from(params.max_block_input_bytes))
            && budget.is_some_and(|budget| budget <= params.user_budget);
        let (Some(ops), Some(bytes), Some(budget), true) = (ops, bytes, budget, fits) else {
            break;
        };
        selection = Selection {
            txs: count,
            ops,
            bytes,
            budget,
            ..selection
        };
    }
    selection
}

/// Builds the next block's inputs from the queue fronts with [`select`].
pub fn select_inputs(
    params: &ChainParams,
    admin: &[AdminInput],
    txs: &[TxEnvelope],
) -> BlockInputs {
    let selection = select(params, admin, txs);
    BlockInputs {
        admin: admin[..selection.admin].to_vec(),
        txs: txs[..selection.txs].to_vec(),
    }
}

/// Checks a block's inputs against every selection limit (the replay side
/// of [`select`]); the byte limit is checked on the actual encoding.
pub fn check(params: &ChainParams, inputs: &BlockInputs) -> Result<(), SelectionFault> {
    if wide(inputs.admin.len()) > u64::from(params.max_admin_per_block) {
        return Err(SelectionFault::AdminCount);
    }
    if wide(inputs.txs.len()) > u64::from(params.max_txs_per_block) {
        return Err(SelectionFault::TxCount);
    }
    let ops = inputs
        .txs
        .iter()
        .try_fold(0u64, |total, tx| total.checked_add(wide(tx.ops.len())));
    if ops.is_none_or(|ops| ops > u64::from(params.max_ops_per_block)) {
        return Err(SelectionFault::OpCount);
    }
    if wide(encode(inputs).len()) > u64::from(params.max_block_input_bytes) {
        return Err(SelectionFault::Bytes);
    }
    let budget = inputs
        .txs
        .iter()
        .try_fold(0u64, |total, tx| total.checked_add(tx.tx_budget));
    if budget.is_none_or(|budget| budget > params.user_budget) {
        return Err(SelectionFault::Budget);
    }
    Ok(())
}
