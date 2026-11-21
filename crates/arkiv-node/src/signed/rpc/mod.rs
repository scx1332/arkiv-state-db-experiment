//! Ethereum JSON-RPC compatibility over the native, authenticated Arkiv chain.
//! Block/state hashes identify native commitments, not Ethereum MPT roots.

mod blocks;
mod entities;
mod filters;
mod logs;
mod params;
mod simulate;
#[cfg(test)]
mod tests;

pub use logs::{block_logs, validate_filter};

use super::{
    Detail, Error, ErrorCode, Result,
    host::Host,
    protocol::ValidTx,
    rpc_reads::{QueryErrorKind, ReadError},
    runtime::{Node, Shared},
};
use crate::{accounts::GAS_PRICE_WEI, text::hex};
use axum::{
    body::{Body, to_bytes},
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use filters::Filter;
use params::{actor, args, at, first, quantity};
use serde_json::{Value, json};
use std::collections::BTreeMap;

const INDEX_CAP: usize = 1_000_000;
const ADDRESS: &str = "0x4400000000000000000000000000000000000044";

#[derive(Default)]
pub struct RpcState {
    indexed: Option<u64>,
    transactions: BTreeMap<String, (u64, usize)>,
    blocks: BTreeMap<String, u64>,
    /// What each indexed height added, so expired heights leave the index.
    heights: BTreeMap<u64, (String, Vec<String>)>,
    filters: BTreeMap<u64, Filter>,
    next_filter: u64,
}

/// Brings the hash index up to the head: the retained blocks only. Heights
/// whose records expired leave it, so a lookup of an expired block or
/// transaction hash finds nothing (`null`), and the index holds at most the
/// record window.
fn index(host: &Host, state: &mut RpcState) -> Result<()> {
    while let Some(entry) = state.heights.first_entry() {
        if !host.record_expired(*entry.key()) {
            break;
        }
        let (block, transactions) = entry.remove();
        state.blocks.remove(&block);
        for hash in transactions {
            state.transactions.remove(&hash);
        }
    }
    let mut next = state.indexed.map_or(0, |n| n + 1);
    while next <= host.head.height {
        let h = next;
        if host.record_expired(h) {
            // Expired heights are skipped, not read.
            next = host.record_floor();
            state.indexed = Some(next - 1);
            continue;
        }
        next += 1;
        let b = host.shared_block(h)?;
        if state.transactions.len() + b.inputs.txs.len() > INDEX_CAP
            || state.blocks.len() >= INDEX_CAP
        {
            return Err(ErrorCode::RpcIndexCapacity.into());
        }
        let block = hex(b.hash().as_slice());
        state.blocks.insert(block.clone(), h);
        let mut transactions = Vec::with_capacity(b.inputs.txs.len());
        for (i, t) in b.inputs.txs.iter().enumerate() {
            let hash = hex(t.hash().as_slice());
            state.transactions.insert(hash.clone(), (h, i));
            transactions.push(hash);
        }
        state.heights.insert(h, (block, transactions));
        state.indexed = Some(h);
    }
    Ok(())
}

/// Rebuild the derived lookup index before accepting public requests, from
/// the retained blocks only (bounded by the record window, not by the
/// chain). Source blocks remain authoritative; this cache contains no
/// separately trusted state.
pub(super) fn initial_state(node: &Node) -> Result<RpcState> {
    let mut state = RpcState::default();
    if let Node::Full(host) = node {
        match index(host, &mut state) {
            Ok(())
            | Err(Error {
                code: ErrorCode::RpcIndexCapacity,
                ..
            }) => (),
            Err(e) => return Err(e),
        }
    }
    Ok(state)
}

fn locate(host: &Host, state: &mut RpcState, hash: &str) -> Result<Option<(u64, usize)>> {
    index(host, state)?;
    Ok(state.transactions.get(&hash.to_ascii_lowercase()).copied())
}

fn dispatch(
    host: &mut Host,
    state: &mut RpcState,
    method: &str,
    params: &Value,
) -> std::result::Result<Value, ReadError> {
    if let Some(result) = super::rpc_reads::dispatch(host, method, params) {
        return result;
    }
    let args = args(params)?;
    match method {
        "web3_clientVersion" => Ok(json!("arkiv-native/ethereum-compat-v1")),
        "net_version" => Ok(json!(host.genesis.genesis.chain_id.to_string())),
        "net_listening" => Ok(json!(true)),
        "eth_chainId" => Ok(json!(quantity(host.genesis.genesis.chain_id))),
        "eth_blockNumber" => Ok(json!(quantity(host.head.height))),
        "eth_syncing" => Ok(json!(false)),
        "eth_gasPrice" => Ok(json!(quantity(GAS_PRICE_WEI))),
        "eth_maxPriorityFeePerGas" => Ok(json!("0x0")),
        "eth_accounts" => Ok(json!([])),
        "eth_getBalance" => balance(host, args),
        "eth_getTransactionCount" => transaction_count(host, args),
        "eth_getCode" => code(host, args),
        "eth_getBlockByNumber" => blocks::block_by_number(host, args),
        "eth_getBlockByHash" => blocks::block_by_hash(host, state, args),
        "eth_getBlockTransactionCountByNumber" => blocks::block_transaction_count(host, args),
        "eth_getTransactionByHash" => blocks::transaction_by_hash(host, state, args),
        "eth_getTransactionReceipt" => blocks::transaction_receipt(host, state, args),
        "eth_getTransactionByBlockNumberAndIndex" => blocks::transaction_by_index(host, args),
        "eth_sendRawTransaction" => send_raw_transaction(host, args),
        "eth_estimateGas" => simulate::estimate_gas(host, args),
        "eth_call" => simulate::call(host, args),
        "eth_getLogs" => logs::logs(host, state, first(args)?),
        "eth_newFilter" => filters::new_filter(host, state, args, false),
        "eth_newBlockFilter" => filters::new_filter(host, state, args, true),
        "eth_uninstallFilter" => filters::uninstall_filter(state, args),
        "eth_getFilterChanges" => filters::filter_changes(host, state, args),
        "eth_getFilterLogs" => filters::filter_logs(host, state, args),
        "eth_feeHistory" => blocks::fee_history(host, args),
        _ => Err(ErrorCode::MethodNotFound.into()),
    }
    .map_err(ReadError::Node)
}

fn balance(host: &Host, args: &[Value]) -> Result<Value> {
    let owner = actor(first(args)?)?;
    let account = host.account(at(host, args.get(1))?, owner)?;
    Ok(json!(format!("{:#x}", account.balance)))
}

/// The account nonce; at `pending`, the one after its queued transactions.
fn transaction_count(host: &Host, args: &[Value]) -> Result<Value> {
    let owner = actor(first(args)?)?;
    let account = host.account(at(host, args.get(1))?, owner)?;
    let pending = args.get(1).and_then(Value::as_str) == Some("pending");
    Ok(json!(quantity(if pending {
        host.pool.contiguous_next(owner, account.nonce)
    } else {
        account.nonce
    })))
}

/// No account has code.
fn code(host: &Host, args: &[Value]) -> Result<Value> {
    actor(first(args)?)?;
    at(host, args.get(1))?;
    Ok(json!("0x"))
}

fn send_raw_transaction(host: &mut Host, args: &[Value]) -> Result<Value> {
    let raw = params::bytes(first(args)?, None)?;
    let tx = ValidTx::from_ethereum(&host.genesis, &raw)?;
    let hash = hex(tx.tx().hash().as_slice());
    host.admit(tx)?;
    Ok(json!(hash))
}

/// JSON-RPC code of a state-retention refusal (`HistoryPruned`) on every
/// method: the SDK's `block` kind, a height outside the retained range.
pub const HISTORY_PRUNED_CODE: i64 = QueryErrorKind::Block.code();
/// JSON-RPC code of a detached read refused at admission (`ServerBusy`).
/// Retryable; outside the SDK's query table (-32001 to -32006).
pub const SERVER_BUSY_CODE: i64 = -32010;

/// The response to a refused SDK read.
fn read_error(id: Value, e: ReadError) -> Value {
    match e {
        ReadError::Node(e) => error(id, e),
        ReadError::Query(q) => json!({"jsonrpc":"2.0","id":id,"error":q.to_json()}),
    }
}

fn error(id: Value, e: Error) -> Value {
    e.log_storage_failure();
    if e.code == ErrorCode::HistoryPruned {
        let mut error = json!({"code":HISTORY_PRUNED_CODE,"message":"history pruned"});
        if let Some((requested, floor)) = e.pruned_at() {
            error["message"] = json!(format!(
                "history pruned: requested {requested}, floor {floor}"
            ));
            error["data"] = json!({"requested":quantity(requested),"floor":quantity(floor)});
        }
        return json!({"jsonrpc":"2.0","id":id,"error":error});
    }
    let message = match e.code {
        ErrorCode::InsufficientBalance => "insufficient funds for gas * price + value + rent",
        ErrorCode::ExceedsStorageCap => {
            "transaction adds more storage units than a block may (max_new_storage_units_per_block)"
        }
        ErrorCode::TransactionDroppedInsufficientBalance => {
            "transaction dropped: insufficient funds for gas * price + value + rent"
        }
        ErrorCode::TransactionDroppedExceedsStorageCap => {
            "transaction dropped: it adds more storage units than a block may"
        }
        code => code.as_str(),
    };
    let code = match e.code {
        ErrorCode::MethodNotFound => -32601,
        ErrorCode::InvalidParams => -32602,
        ErrorCode::InvalidRequest => -32600,
        ErrorCode::ExecutionReverted => 3,
        ErrorCode::ServerBusy => SERVER_BUSY_CODE,
        _ => -32000,
    };
    let mut error = json!({"code": code, "message": message});
    if let Some(Detail::Revert(data)) = e.detail {
        error["data"] = json!(data);
    }
    json!({"jsonrpc": "2.0", "id": id, "error": error})
}

async fn single(rt: &Shared, request: Value) -> Value {
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    if !matches!(id, Value::Null | Value::String(_) | Value::Number(_)) {
        return error(Value::Null, Error::from(ErrorCode::InvalidRequest));
    }
    let method = match request.get("method").and_then(Value::as_str) {
        Some(m) if request.get("jsonrpc").and_then(Value::as_str) == Some("2.0") => m.to_owned(),
        _ => return error(id, Error::from(ErrorCode::InvalidRequest)),
    };
    let params = request.get("params").cloned().unwrap_or_else(|| json!([]));
    let local = if super::rpc_reads::detached(&method) {
        detached(rt, method, params, id.clone()).await
    } else {
        locked(rt, method, params, id.clone()).await
    };
    let result = match local {
        Ok(Some(result)) => Ok(result),
        Ok(None) => match rt.peer.as_ref() {
            Some(peer) => {
                super::runtime::fetch::<Value>(
                    &rt.client,
                    peer,
                    "/",
                    Some(request.clone()),
                    None,
                    8 * 1024 * 1024,
                )
                .await
            }
            None => Err(ErrorCode::FullNodeRequired.into()),
        },
        Err(e) => Err(e),
    };
    if request.get("id").is_none() {
        Value::Null
    } else {
        result.unwrap_or_else(|e| error(id, e))
    }
}

/// A response computed under the node lock, or `None` to forward the request.
async fn locked(rt: &Shared, method: String, params: Value, id: Value) -> Result<Option<Value>> {
    // RPC bursts wait for the node mutex in the blocking pool instead of
    // competing for the native REST API's four admission permits, one at a
    // time and behind waiting writers (see `Runtime::rpc_node`).
    let runtime = rt.clone();
    let response_id = id;
    let job = tokio::task::spawn_blocking(move || {
        let mut node = runtime.rpc_node()?;
        let Node::Full(h) = &mut *node else {
            return Ok(None);
        };
        if method == "eth_sendRawTransaction" && !h.is_producer() {
            return Ok(None);
        }
        let mut state = runtime
            .rpc
            .lock()
            .map_err(|_| ErrorCode::StorageUnavailable)?;
        Ok(Some(match dispatch(h, &mut state, &method, &params) {
            Ok(v) => json!({"jsonrpc":"2.0","id":response_id,"result":v}),
            Err(e) => read_error(response_id, e),
        }))
    })
    .await;
    job.unwrap_or(Err(ErrorCode::StorageUnavailable.into()))
}

/// `arkiv_query`, `arkiv_getEntity` and `arkiv_getEntityCount` on a full node, as detached reads
/// (see [`super::reads`]): admitted before touching the node lock, planned
/// under it, and evaluated against the pinned snapshot after releasing it.
/// `None` forwards the request (a light node).
async fn detached(rt: &Shared, method: String, params: Value, id: Value) -> Result<Option<Value>> {
    let admitted = rt.reads.admit()?;
    let runtime = rt.clone();
    let label = method.clone();
    admitted
        .run(&label, move || {
            let planned = {
                let node = runtime.read_node()?;
                let Node::Full(h) = &*node else {
                    return Ok(None);
                };
                match super::rpc_reads::plan(h, &method, &params, runtime.reads.limits()) {
                    Some(Ok(planned)) => planned,
                    Some(Err(e)) => return Ok(Some(read_error(id, e))),
                    None => return Err(ErrorCode::MethodNotFound.into()),
                }
            };
            Ok(Some(match planned.run() {
                Ok(v) => json!({"jsonrpc":"2.0","id":id,"result":v}),
                Err(e) => read_error(id, e),
            }))
        })
        .await
}

pub async fn handle_request(rt: &Shared, request: Value) -> Value {
    const CAP: usize = 8 * 1024 * 1024;
    if let Some(batch) = request.as_array() {
        if batch.is_empty() || batch.len() > 64 {
            return error(Value::Null, Error::from(ErrorCode::InvalidRequest));
        }
        let mut out = Vec::new();
        let mut size = 2;
        for r in batch {
            let v = single(rt, r.clone()).await;
            if !v.is_null() {
                size += serde_json::to_vec(&v).map_or(CAP + 1, |b| b.len() + 1);
                if size > CAP {
                    return error(Value::Null, Error::from(ErrorCode::ResponseTooLarge));
                }
                out.push(v);
            }
        }
        if out.is_empty() {
            Value::Null
        } else {
            json!(out)
        }
    } else {
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let v = single(rt, request).await;
        if serde_json::to_vec(&v).map_or(true, |b| b.len() > CAP) {
            error(id, Error::from(ErrorCode::ResponseTooLarge))
        } else {
            v
        }
    }
}

pub async fn http(State(rt): State<Shared>, body: Body) -> Response {
    let input = match tokio::time::timeout(
        std::time::Duration::from_secs(5),
        to_bytes(body, 1024 * 1024),
    )
    .await
    {
        Ok(Ok(b)) => b,
        Ok(Err(_)) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
        Err(_) => return StatusCode::REQUEST_TIMEOUT.into_response(),
    };
    let request: Value =
        match serde_json::from_slice(&input) {
            Ok(v) => v,
            Err(_) => return axum::Json(
                json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"Parse error"}}),
            )
            .into_response(),
        };
    let batch = request.is_array();
    let result = handle_request(&rt, request).await;
    if result.is_null() {
        StatusCode::NO_CONTENT.into_response()
    } else if !batch && result["error"]["message"] == "ServerBusy" {
        // A single refused request also carries HTTP 429, so client
        // transports back off and retry; a batch keeps per-entry errors.
        (StatusCode::TOO_MANY_REQUESTS, axum::Json(result)).into_response()
    } else {
        axum::Json(result).into_response()
    }
}
