//! Blocks, transactions and receipts in their Ethereum JSON shapes.

use super::super::{
    Error, ErrorCode, Result, ethereum, host::Host, pool::DropReason, protocol::SignedTx,
};
use super::{
    ADDRESS, RpcState, index, locate,
    logs::{block_logs, bloom},
    params::{at, first, quantity, uint},
};
use crate::{TxStatus, accounts::GAS_PRICE_WEI, text::hex};
use alloy_primitives::keccak256;
use serde_json::{Value, json};

pub(super) const ZERO_HASH: &str =
    "0x0000000000000000000000000000000000000000000000000000000000000000";

fn transaction(tx: &SignedTx, block: Option<(&crate::Header, usize)>) -> Result<Value> {
    let mut object = json!({
        "hash": hex(tx.hash().as_slice()),
        "nonce": quantity(tx.nonce),
        "from": hex(tx.tx.actor.as_bytes()),
        "gas": quantity(tx.tx.tx_budget),
        "gasPrice": quantity(GAS_PRICE_WEI),
        "value": "0x0",
        "input": "0x",
        "to": ADDRESS,
        "type": "0x0",
        "chainId": quantity(tx.tx.chain_id),
        "r": hex(&tx.signature[..32]),
        "s": hex(&tx.signature[32..64]),
        "v": quantity(tx.signature[64] as u64),
        "blockHash": null,
        "blockNumber": null,
        "transactionIndex": null,
    });
    if let Some(raw) = &tx.ethereum_raw {
        let parsed = ethereum::parse(raw)?;
        let parity = parsed.signature[64] as u64;
        object["to"] = json!(hex(&parsed.to));
        object["input"] = json!(hex(&parsed.input));
        object["value"] = json!(format!("{:#x}", parsed.value));
        object["type"] = json!(quantity(parsed.tx_type as u64));
        if parsed.tx_type == 2 {
            object["maxFeePerGas"] = json!(format!("{:#x}", parsed.max_fee_per_gas));
            object["maxPriorityFeePerGas"] =
                json!(format!("{:#x}", parsed.max_priority_fee_per_gas));
            object["accessList"] = json!([]);
            object["yParity"] = json!(quantity(parity));
        } else {
            object["gasPrice"] = json!(format!("{:#x}", parsed.max_fee_per_gas));
            // EIP-155: v = 2 * chain id + 35 + parity.
            object["v"] = json!(quantity(
                parsed
                    .chain_id
                    .checked_mul(2)
                    .and_then(|n| n.checked_add(35 + parity))
                    .ok_or(ErrorCode::InvalidTransaction)?
            ));
        }
    }
    if let Some((header, index)) = block {
        object["blockHash"] = json!(hex(header.hash().as_slice()));
        object["blockNumber"] = json!(quantity(header.height));
        object["transactionIndex"] = json!(quantity(index as u64));
    }
    Ok(object)
}

fn block_json(host: &Host, height: u64, full: bool) -> Result<Value> {
    let block = host.shared_block(height)?;
    let header = &block.header.header;
    let transactions = block
        .inputs
        .txs
        .iter()
        .enumerate()
        .map(|(index, tx)| {
            if full {
                transaction(tx, Some((header, index)))
            } else {
                Ok(json!(hex(tx.hash().as_slice())))
            }
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(json!({
        "number": quantity(height),
        "hash": hex(header.hash().as_slice()),
        "parentHash": hex(header.parent_hash.as_slice()),
        "timestamp": quantity(header.timestamp_ms / 1000),
        "nonce": "0x0000000000000000",
        "sha3Uncles": hex(keccak256([0xc0]).as_slice()),
        "logsBloom": bloom(&block_logs(host, height)?),
        "transactionsRoot": hex(header.inputs_digest.as_slice()),
        "stateRoot": hex(header.state_root.as_slice()),
        "receiptsRoot": hex(header.outcomes_digest.as_slice()),
        "miner": hex(&host.genesis.proposer),
        "difficulty": "0x0",
        "totalDifficulty": "0x0",
        "extraData": "0x",
        "size": quantity(arkiv_db::encode(&*block).len() as u64),
        "gasLimit": quantity(host.genesis.genesis.params.block_budget),
        "gasUsed": quantity(block.outcomes.spent),
        "baseFeePerGas": quantity(GAS_PRICE_WEI),
        "mixHash": ZERO_HASH,
        "uncles": [],
        "transactions": transactions,
    }))
}

pub(super) fn receipt(host: &Host, height: u64, index: usize) -> Result<Value> {
    let block = host.shared_block(height)?;
    let tx = &block.inputs.txs[index];
    let outcome = &block.outcomes.txs[index];
    let logs: Vec<_> = block_logs(host, height)?
        .into_iter()
        .filter(|log| log["transactionIndex"] == quantity(index as u64))
        .collect();
    let mut receipt = transaction(tx, Some((&block.header.header, index)))?;
    let cumulative: u64 = block.outcomes.txs[..=index].iter().map(|t| t.spent).sum();
    let status = if outcome.status == TxStatus::Committed {
        "0x1"
    } else {
        "0x0"
    };
    let fields = receipt.as_object_mut().ok_or(ErrorCode::StorageCorrupt)?;
    fields.insert("transactionHash".into(), json!(hex(tx.hash().as_slice())));
    fields.insert("status".into(), json!(status));
    fields.insert("gasUsed".into(), json!(quantity(outcome.spent)));
    // Entity rent the transaction burned beside its gas fee, in wei: not gas,
    // so not in gasUsed × effectiveGasPrice (like an L2's separate l1Fee).
    fields.insert("rentFee".into(), json!(format!("{:#x}", outcome.rent_wei)));
    fields.insert("cumulativeGasUsed".into(), json!(quantity(cumulative)));
    fields.insert("effectiveGasPrice".into(), json!(quantity(GAS_PRICE_WEI)));
    fields.insert("contractAddress".into(), Value::Null);
    fields.insert("logsBloom".into(), json!(bloom(&logs)));
    fields.insert("logs".into(), json!(logs));
    Ok(receipt)
}

/// Whether `eth_getBlockBy*` asked for whole transactions, not their hashes.
fn full_transactions(args: &[Value]) -> bool {
    args.get(1).and_then(Value::as_bool).unwrap_or(false)
}

pub(super) fn block_by_number(host: &Host, args: &[Value]) -> Result<Value> {
    match at(host, args.first()) {
        Ok(height) => block_json(host, height, full_transactions(args)),
        Err(Error {
            code: ErrorCode::UnknownBlock,
            ..
        }) => Ok(Value::Null),
        Err(e) => Err(e),
    }
}

pub(super) fn block_by_hash(host: &Host, state: &mut RpcState, args: &[Value]) -> Result<Value> {
    index(host, state)?;
    let hash = first(args)?.as_str().ok_or(ErrorCode::InvalidParams)?;
    match state.blocks.get(&hash.to_ascii_lowercase()) {
        Some(height) => block_json(host, *height, full_transactions(args)),
        None => Ok(Value::Null),
    }
}

/// Served from the kept body below the record floor, like
/// [`transaction_by_index`]: the count needs no state.
pub(super) fn block_transaction_count(host: &Host, args: &[Value]) -> Result<Value> {
    let height = at(host, args.first())?;
    Ok(json!(quantity(
        host.body_block(height)?.inputs.txs.len() as u64
    )))
}

/// Where a transaction hash leads.
enum Located<'a> {
    /// In a block, at this height and index.
    Included(u64, usize),
    Pending(&'a SignedTx),
    Unknown,
}

/// A transaction evicted without a block is a definite refusal, so a client
/// polling for it stops instead of waiting forever.
fn find_transaction<'a>(
    host: &'a Host,
    state: &mut RpcState,
    args: &[Value],
) -> Result<Located<'a>> {
    let hash = first(args)?.as_str().ok_or(ErrorCode::InvalidParams)?;
    if let Some((height, index)) = locate(host, state, hash)? {
        return Ok(Located::Included(height, index));
    }
    if let Some(tx) = host.pool.by_hash(hash) {
        return Ok(Located::Pending(tx));
    }
    match host.pool.dropped.reason(hash) {
        Some(DropReason::ExceedsStorageCap) => {
            Err(ErrorCode::TransactionDroppedExceedsStorageCap.into())
        }
        Some(DropReason::InsufficientBalance) => {
            Err(ErrorCode::TransactionDroppedInsufficientBalance.into())
        }
        None => Ok(Located::Unknown),
    }
}

pub(super) fn transaction_by_hash(
    host: &Host,
    state: &mut RpcState,
    args: &[Value],
) -> Result<Value> {
    match find_transaction(host, state, args)? {
        Located::Included(height, index) => {
            let block = host.shared_block(height)?;
            transaction(
                &block.inputs.txs[index],
                Some((&block.header.header, index)),
            )
        }
        Located::Pending(tx) => transaction(tx, None),
        Located::Unknown => Ok(Value::Null),
    }
}

pub(super) fn transaction_receipt(
    host: &Host,
    state: &mut RpcState,
    args: &[Value],
) -> Result<Value> {
    match find_transaction(host, state, args)? {
        Located::Included(height, index) => receipt(host, height, index),
        Located::Pending(_) | Located::Unknown => Ok(Value::Null),
    }
}

/// A transaction is its body's alone, so below the record floor it is
/// served from the kept body (`bodyRetention: all`); the block JSON and the
/// receipt also need the state before the block and keep the receipt floor.
pub(super) fn transaction_by_index(host: &Host, args: &[Value]) -> Result<Value> {
    let height = at(host, args.first())?;
    let index = uint(args.get(1).ok_or(ErrorCode::InvalidParams)?)? as usize;
    let block = host.body_block(height)?;
    match block.inputs.txs.get(index) {
        Some(tx) => transaction(tx, Some((&block.header.header, index))),
        None => Ok(Value::Null),
    }
}

/// Reward percentiles must be at most 100 ascending values in `0..=100`.
fn reward_percentiles(value: Option<&Value>) -> Result<usize> {
    let Some(value) = value else {
        return Ok(0);
    };
    let percentiles = value.as_array().ok_or(ErrorCode::InvalidParams)?;
    if percentiles.len() > 100 {
        return Err(ErrorCode::InvalidParams.into());
    }
    let mut previous = 0.0;
    for percentile in percentiles {
        let n = percentile.as_f64().ok_or(ErrorCode::InvalidParams)?;
        if !n.is_finite() || !(0.0..=100.0).contains(&n) || n < previous {
            return Err(ErrorCode::InvalidParams.into());
        }
        previous = n;
    }
    Ok(percentiles.len())
}

pub(super) fn fee_history(host: &Host, args: &[Value]) -> Result<Value> {
    let count = uint(first(args)?)?.min(1024);
    let end = at(host, args.get(1))?;
    let count = count.min(end + 1);
    // Blocks below the record floor expired: the history starts above it.
    let start = (end + 1 - count).max(host.record_floor()).min(end);
    let count = end + 1 - start;
    let percentiles = reward_percentiles(args.get(2))?;
    let block_budget = host.genesis.genesis.params.block_budget as f64;
    let ratios = (start..=end)
        .map(|height| Ok(host.shared_block(height)?.outcomes.spent as f64 / block_budget))
        .collect::<Result<Vec<_>>>()?;
    Ok(json!({
        "oldestBlock": quantity(start),
        "baseFeePerGas": vec![quantity(GAS_PRICE_WEI); count as usize + 1],
        "gasUsedRatio": ratios,
        "reward": vec![vec!["0x0"; percentiles]; count as usize],
    }))
}
