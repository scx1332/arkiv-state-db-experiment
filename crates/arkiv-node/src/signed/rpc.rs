//! Ethereum JSON-RPC compatibility over the native, authenticated Arkiv chain.
//! Block/state hashes identify native commitments, not Ethereum MPT roots.
use super::{
    Error, Result, ethereum,
    host::Host,
    protocol::SignedTx,
    runtime::{Node, Shared},
};
use crate::{
    TxOp, TxStatus,
    arkiv::{self, ArkivOperation},
};
use alloy_primitives::{U256, keccak256};
use arkiv_db::{Budget, Cost, OwnerId, Projection, RecordKey, SnapshotId};
use axum::{
    body::{Body, to_bytes},
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use std::collections::BTreeMap;

const LOG_BLOCK_CAP: u64 = 256;
const INDEX_CAP: usize = 1_000_000;
const FILTER_CAP: usize = 1024;
const FILTER_TTL: std::time::Duration = std::time::Duration::from_secs(15 * 60);
const ADDRESS: &str = "0x4400000000000000000000000000000000000044";
const ZERO_HASH: &str = "0x0000000000000000000000000000000000000000000000000000000000000000";
#[derive(Default)]
pub struct RpcState {
    indexed: Option<u64>,
    transactions: BTreeMap<String, (u64, usize)>,
    blocks: BTreeMap<String, u64>,
    filters: BTreeMap<u64, Filter>,
    next_filter: u64,
    revert_data: Option<String>,
}
struct Filter {
    spec: Value,
    next: u64,
    blocks: bool,
    touched: std::time::Instant,
}
fn expire_filters(state: &mut RpcState, now: std::time::Instant) {
    state
        .filters
        .retain(|_, f| now.saturating_duration_since(f.touched) < FILTER_TTL);
}
/// Bound retained filters as well as reject silently ignored selector typos.
pub fn validate_filter(filter: &Value) -> Result<()> {
    let object = filter.as_object().ok_or(Error("InvalidParams"))?;
    for (name, value) in object {
        match name.as_str() {
            "blockHash" => {
                bytes(value, Some(32))?;
            }
            "fromBlock" | "toBlock" => {
                filter_height(0, Some(value))?;
            }
            "address" => {
                if let Some(addresses) = value.as_array() {
                    if addresses.len() > 64 {
                        return Err(Error("InvalidParams"));
                    }
                    for address in addresses {
                        bytes(address, Some(20))?;
                    }
                } else {
                    bytes(value, Some(20))?;
                }
            }
            "topics" => {
                let topics = value.as_array().ok_or(Error("InvalidParams"))?;
                if topics.len() > 4 {
                    return Err(Error("InvalidParams"));
                }
                for topic in topics {
                    if topic.is_null() {
                        continue;
                    }
                    if let Some(alternatives) = topic.as_array() {
                        if alternatives.len() > 64 {
                            return Err(Error("InvalidParams"));
                        }
                        for alternative in alternatives {
                            if !alternative.is_null() {
                                bytes(alternative, Some(32))?;
                            }
                        }
                    } else {
                        bytes(topic, Some(32))?;
                    }
                }
            }
            _ => return Err(Error("InvalidParams")),
        }
    }
    if object.contains_key("blockHash")
        && (object.contains_key("fromBlock") || object.contains_key("toBlock"))
    {
        return Err(Error("InvalidParams"));
    }
    Ok(())
}
// Numeric filter bounds may point beyond the current head. They stay fixed
// while latest/pending/safe/finalized follow the head on each poll.
fn filter_height(head: u64, value: Option<&Value>) -> Result<u64> {
    match value {
        None => Ok(head),
        Some(value) => match value.as_str().ok_or(Error("InvalidParams"))? {
            "latest" | "pending" | "safe" | "finalized" => Ok(head),
            "earliest" => Ok(0),
            s if s.len() <= 18 => uint(value),
            _ => Err(Error("InvalidParams")),
        },
    }
}
pub fn hex(bytes: &[u8]) -> String {
    crate::simulator::wire::hex(bytes)
}
pub fn quantity(n: u64) -> String {
    format!("0x{n:x}")
}
fn uint(v: &Value) -> Result<u64> {
    let s = v.as_str().ok_or(Error("InvalidParams"))?;
    if !s.starts_with("0x") || s.len() < 3 || (s.len() > 3 && s.as_bytes()[2] == b'0') {
        return Err(Error("InvalidParams"));
    }
    u64::from_str_radix(&s[2..], 16).map_err(|_| Error("InvalidParams"))
}
fn bytes(v: &Value, len: Option<usize>) -> Result<Vec<u8>> {
    let s = v.as_str().ok_or(Error("InvalidParams"))?;
    let s = s.strip_prefix("0x").ok_or(Error("InvalidParams"))?;
    if !s.bytes().all(|b| b.is_ascii_hexdigit())
        || s.len() % 2 != 0
        || s.len() > 2 * super::protocol::TX_CAP as usize
        || len.is_some_and(|l| s.len() != l * 2)
    {
        return Err(Error("InvalidParams"));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| Error("InvalidParams")))
        .collect()
}
fn actor(v: &Value) -> Result<OwnerId> {
    Ok(OwnerId::new(
        bytes(v, Some(20))?
            .try_into()
            .map_err(|_| Error("InvalidParams"))?,
    ))
}
fn at(host: &Host, v: Option<&Value>) -> Result<u64> {
    match v
        .map(|v| v.as_str().ok_or(Error("InvalidParams")))
        .transpose()?
        .unwrap_or("latest")
    {
        "latest" | "pending" | "safe" | "finalized" => Ok(host.head.height),
        "earliest" => Ok(0),
        _ => {
            let n = uint(v.ok_or(Error("InvalidParams"))?)?;
            if n > host.head.height {
                return Err(Error("UnknownBlock"));
            }
            Ok(n)
        }
    }
}
fn args(v: &Value) -> Result<&[Value]> {
    v.as_array()
        .map(Vec::as_slice)
        .ok_or(Error("InvalidParams"))
}
fn index(host: &Host, state: &mut RpcState) -> Result<()> {
    let start = state.indexed.map_or(0, |n| n + 1);
    for h in start..=host.head.height {
        let b = host.block(h)?;
        if state.transactions.len() + b.inputs.txs.len() > INDEX_CAP
            || state.blocks.len() >= INDEX_CAP
        {
            return Err(Error("RpcIndexCapacity"));
        }
        state.blocks.insert(hex(b.hash().as_slice()), h);
        for (i, t) in b.inputs.txs.iter().enumerate() {
            state.transactions.insert(hex(t.hash().as_slice()), (h, i));
        }
        state.indexed = Some(h);
    }
    Ok(())
}
/// Rebuild the derived lookup index before accepting public requests. Source
/// blocks remain authoritative; this cache contains no separately trusted state.
pub(super) fn initial_state(node: &Node) -> Result<RpcState> {
    let mut state = RpcState::default();
    if let Node::Full(host) = node {
        match index(host, &mut state) {
            Ok(()) | Err(Error("RpcIndexCapacity")) => (),
            Err(e) => return Err(e),
        }
    }
    Ok(state)
}
fn locate(host: &Host, state: &mut RpcState, hash: &str) -> Result<Option<(u64, usize)>> {
    index(host, state)?;
    Ok(state.transactions.get(&hash.to_ascii_lowercase()).copied())
}
fn transaction(tx: &SignedTx, block: Option<(&crate::Header, usize)>) -> Result<Value> {
    let mut v = json!({"hash":hex(tx.hash().as_slice()),"nonce":quantity(tx.nonce),"from":hex(tx.tx.actor.as_bytes()),"gas":quantity(tx.tx.tx_budget),"gasPrice":quantity(crate::accounts::GAS_PRICE_WEI),"value":"0x0","input":"0x","to":ADDRESS,"type":"0x0","chainId":quantity(tx.tx.chain_id),"r":hex(&tx.signature[..32]),"s":hex(&tx.signature[32..64]),"v":quantity(tx.signature[64] as u64),"blockHash":null,"blockNumber":null,"transactionIndex":null});
    if let Some(raw) = &tx.ethereum_raw {
        let p = ethereum::parse(raw)?;
        v["to"] = json!(hex(&p.to));
        v["input"] = json!(hex(&p.input));
        v["value"] = json!(format!("{:#x}", p.value));
        v["type"] = json!(quantity(p.tx_type as u64));
        if p.tx_type == 2 {
            v["maxFeePerGas"] = json!(format!("{:#x}", p.max_fee_per_gas));
            v["maxPriorityFeePerGas"] = json!(format!("{:#x}", p.max_priority_fee_per_gas));
            v["accessList"] = json!([]);
            v["yParity"] = json!(quantity(p.signature[64] as u64));
        } else {
            v["gasPrice"] = json!(format!("{:#x}", p.max_fee_per_gas));
            v["v"] = json!(quantity(
                p.chain_id
                    .checked_mul(2)
                    .and_then(|n| n.checked_add(35 + p.signature[64] as u64))
                    .ok_or(Error("InvalidTransaction"))?
            ));
        }
    }
    if let Some((h, i)) = block {
        v["blockHash"] = json!(hex(h.hash().as_slice()));
        v["blockNumber"] = json!(quantity(h.height));
        v["transactionIndex"] = json!(quantity(i as u64));
    }
    Ok(v)
}
pub fn block_json(host: &Host, height: u64, full: bool) -> Result<Value> {
    let b = host.block(height)?;
    let h = &b.header.header;
    let txs = b
        .inputs
        .txs
        .iter()
        .enumerate()
        .map(|(i, t)| {
            if full {
                transaction(t, Some((h, i)))
            } else {
                Ok(json!(hex(t.hash().as_slice())))
            }
        })
        .collect::<Result<Vec<_>>>()?;
    let gas = b.outcomes.spent;
    Ok(
        json!({"number":quantity(height),"hash":hex(h.hash().as_slice()),"parentHash":hex(h.parent_hash.as_slice()),"timestamp":quantity(h.timestamp_ms/1000),"nonce":"0x0000000000000000","sha3Uncles":hex(keccak256([0xc0]).as_slice()),"logsBloom":bloom(&block_logs(host,height)?),"transactionsRoot":hex(h.inputs_digest.as_slice()),"stateRoot":h.state_root.map(|r|hex(r.as_slice())).unwrap_or_else(||ZERO_HASH.into()),"receiptsRoot":hex(h.outcomes_digest.as_slice()),"miner":hex(&host.genesis.proposer),"difficulty":"0x0","totalDifficulty":"0x0","extraData":"0x","size":quantity(arkiv_db::encode(&b).len() as u64),"gasLimit":quantity(host.genesis.genesis.params.block_budget),"gasUsed":quantity(gas),"baseFeePerGas":quantity(crate::accounts::GAS_PRICE_WEI),"mixHash":ZERO_HASH,"uncles":[],"transactions":txs}),
    )
}
fn entity_nonce(host: &Host, height: u64, a: &[u8; 20]) -> Result<u64> {
    let s = host.engine.snapshot(SnapshotId::from_sequence(height))?;
    match s.try_host_get(&arkiv::nonce_key(a))? {
        None => Ok(0),
        Some(b) => Ok(u64::from_be_bytes(
            b.try_into().map_err(|_| Error("StorageCorrupt"))?,
        )),
    }
}
fn previous_owner(host: &Host, height: u64, key: &RecordKey) -> Result<[u8; 20]> {
    let s = host.engine.snapshot(SnapshotId::from_sequence(height))?;
    let r = s
        .try_get_record(
            arkiv::entity_namespace(),
            key,
            &Projection::ALL,
            Budget::limit(25_000_000),
        )?
        .map_err(|_| Error("StorageCorrupt"))?
        .value
        .ok_or(Error("StorageCorrupt"))?;
    Ok(arkiv::project(&r)
        .map_err(|_| Error("StorageCorrupt"))?
        .owner)
}
fn word(n: u64) -> String {
    format!("{n:064x}")
}
fn topic_address(a: &[u8; 20]) -> String {
    format!("0x{}{}", "0".repeat(24), &hex(a)[2..])
}
pub fn block_logs(host: &Host, height: u64) -> Result<Vec<Value>> {
    if height == 0 {
        return Ok(vec![]);
    }
    let b = host.block(height)?;
    let mut nonces = BTreeMap::new();
    let mut owners: BTreeMap<Vec<u8>, [u8; 20]> = BTreeMap::new();
    let mut logs = Vec::new();
    for (ti, (tx, out)) in b.inputs.txs.iter().zip(&b.outcomes.txs).enumerate() {
        if out.status != TxStatus::Committed {
            continue;
        }
        for op in &tx.tx.ops {
            let TxOp::Arkiv { operation, .. } = op else {
                continue;
            };
            let caller = *tx.tx.actor.as_bytes();
            let (key, signature, topics, data) = match operation {
                ArkivOperation::Create {
                    salt,
                    flags,
                    expiry,
                    ..
                } => {
                    let nonce = match nonces.get(&caller) {
                        Some(n) => *n,
                        None => entity_nonce(host, height - 1, &caller)?,
                    };
                    let key = arkiv::derive_entity_key(
                        host.genesis.genesis.chain_id,
                        &caller,
                        nonce,
                        *salt,
                    )
                    .to_vec();
                    nonces.insert(caller, nonce + 1);
                    owners.insert(key.clone(), caller);
                    let expiry = expiry.expires_at.max(
                        height
                            .checked_add(expiry.min_lifetime)
                            .ok_or(Error("StorageCorrupt"))?,
                    );
                    (
                        key,
                        "EntityCreated(bytes32,address,uint64,uint8)",
                        vec![topic_address(&caller)],
                        format!("0x{}{}", word(expiry), word(*flags as u64)),
                    )
                }
                other => {
                    let key = other.key().ok_or(Error("StorageCorrupt"))?;
                    let owner = match owners.get(key.as_bytes()) {
                        Some(a) => *a,
                        None => previous_owner(host, height - 1, key)?,
                    };
                    let mut topics = vec![topic_address(&owner)];
                    let (signature, data) = match other {
                        ArkivOperation::Patch { .. } => {
                            ("EntityPatched(bytes32,address)", "0x".into())
                        }
                        ArkivOperation::ExtendExpiry { expiry, .. } => (
                            "ExpiryExtended(bytes32,address,uint64)",
                            format!(
                                "0x{}",
                                word(
                                    expiry.expires_at.max(
                                        height
                                            .checked_add(expiry.min_lifetime)
                                            .ok_or(Error("StorageCorrupt"))?
                                    )
                                )
                            ),
                        ),
                        ArkivOperation::TransferOwnership { owner: new, .. } => {
                            topics.push(topic_address(new));
                            owners.insert(key.as_bytes().to_vec(), *new);
                            ("OwnershipTransferred(bytes32,address,address)", "0x".into())
                        }
                        ArkivOperation::Delete { .. } => {
                            ("EntityDeleted(bytes32,address)", "0x".into())
                        }
                        _ => unreachable!(),
                    };
                    (key.as_bytes().to_vec(), signature, topics, data)
                }
            };
            let mut all = vec![hex(keccak256(signature.as_bytes()).as_slice()), hex(&key)];
            all.extend(topics);
            logs.push(json!({"address":ADDRESS,"topics":all,"data":data,"blockNumber":quantity(height),"transactionHash":hex(tx.hash().as_slice()),"transactionIndex":quantity(ti as u64),"blockHash":hex(b.hash().as_slice()),"logIndex":quantity(logs.len() as u64),"removed":false}));
        }
    }
    Ok(logs)
}
fn bloom(logs: &[Value]) -> String {
    let mut bloom = [0u8; 256];
    for l in logs {
        for s in std::iter::once(l["address"].as_str())
            .chain(
                l["topics"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(Value::as_str),
            )
            .flatten()
        {
            if let Ok(b) = bytes(&json!(s), None) {
                let h = keccak256(b);
                for i in [0, 2, 4] {
                    let bit = (((h[i] as usize) << 8) | h[i + 1] as usize) & 2047;
                    bloom[255 - bit / 8] |= 1 << (bit % 8);
                }
            }
        }
    }
    hex(&bloom)
}
fn receipt(host: &Host, h: u64, i: usize) -> Result<Value> {
    let b = host.block(h)?;
    let tx = &b.inputs.txs[i];
    let out = &b.outcomes.txs[i];
    let logs: Vec<_> = block_logs(host, h)?
        .into_iter()
        .filter(|l| l["transactionIndex"] == quantity(i as u64))
        .collect();
    let mut v = transaction(tx, Some((&b.header.header, i)))?;
    let cumulative = b.outcomes.txs[..=i].iter().map(|t| t.spent).sum::<u64>();
    let obj = v.as_object_mut().ok_or(Error("StorageCorrupt"))?;
    obj.insert("transactionHash".into(), json!(hex(tx.hash().as_slice())));
    obj.insert(
        "status".into(),
        json!(if out.status == TxStatus::Committed {
            "0x1"
        } else {
            "0x0"
        }),
    );
    obj.insert("gasUsed".into(), json!(quantity(out.spent)));
    obj.insert("cumulativeGasUsed".into(), json!(quantity(cumulative)));
    obj.insert(
        "effectiveGasPrice".into(),
        json!(quantity(crate::accounts::GAS_PRICE_WEI)),
    );
    obj.insert("contractAddress".into(), Value::Null);
    obj.insert("logsBloom".into(), json!(bloom(&logs)));
    obj.insert("logs".into(), json!(logs));
    Ok(v)
}
pub fn matches_log(log: &Value, filter: &Value) -> bool {
    if let Some(a) = filter.get("address") {
        let address = log["address"].as_str().unwrap_or_default();
        let yes = if let Some(s) = a.as_str() {
            s.eq_ignore_ascii_case(address)
        } else {
            a.as_array().is_some_and(|v| {
                v.iter()
                    .any(|s| s.as_str().is_some_and(|s| s.eq_ignore_ascii_case(address)))
            })
        };
        if !yes {
            return false;
        }
    }
    if let Some(topics) = filter.get("topics").and_then(Value::as_array) {
        if topics.len() > log["topics"].as_array().map_or(0, Vec::len) {
            return false;
        }
        for (i, t) in topics.iter().enumerate() {
            if t.is_null() {
                continue;
            }
            let actual = log["topics"][i].as_str().unwrap_or_default();
            let yes = if let Some(s) = t.as_str() {
                s.eq_ignore_ascii_case(actual)
            } else {
                t.as_array().is_some_and(|v| {
                    v.iter().any(|s| {
                        s.is_null() || s.as_str().is_some_and(|s| s.eq_ignore_ascii_case(actual))
                    })
                })
            };
            if !yes {
                return false;
            }
        }
    }
    true
}
pub fn logs(host: &Host, state: &mut RpcState, filter: &Value) -> Result<Value> {
    validate_filter(filter)?;
    let (start, end) = if let Some(hash) = filter.get("blockHash").and_then(Value::as_str) {
        if filter.get("fromBlock").is_some() || filter.get("toBlock").is_some() {
            return Err(Error("InvalidParams"));
        }
        index(host, state)?;
        let h = *state
            .blocks
            .get(&hash.to_ascii_lowercase())
            .ok_or(Error("UnknownBlock"))?;
        (h, h)
    } else {
        (
            at(host, filter.get("fromBlock"))?,
            at(host, filter.get("toBlock"))?,
        )
    };
    if end < start || end - start >= LOG_BLOCK_CAP {
        return Err(Error("LogRangeTooLarge"));
    }
    let mut out = Vec::new();
    for h in start..=end {
        for l in block_logs(host, h)? {
            if matches_log(&l, filter) {
                out.push(l);
            }
        }
    }
    Ok(json!(out))
}
fn revert_data(
    host: &Host,
    tx: &crate::TxEnvelope,
    out: &crate::TxOutcome,
    height: u64,
) -> Result<Option<String>> {
    let Some((index, fault)) = out.ops.iter().enumerate().find_map(|(i, o)| match o {
        crate::OpOutcome::HostRejected { fault } => Some((i, fault)),
        _ => None,
    }) else {
        return Ok(None);
    };
    let Some(TxOp::Arkiv { operation, .. }) = tx.ops.get(index) else {
        return Ok(None);
    };
    let Some(key) = operation.key() else {
        return Ok(None);
    };
    let key_word = &hex(key.as_bytes())[2..];
    let snapshot = host
        .engine
        .snapshot(SnapshotId::from_sequence(host.head.height))?;
    let current = snapshot
        .try_get_record(
            arkiv::entity_namespace(),
            key,
            &Projection::ALL,
            Budget::limit(25_000_000),
        )?
        .map_err(|_| Error("StorageCorrupt"))?
        .value
        .map(|r| arkiv::project(&r).map_err(|_| Error("StorageCorrupt")))
        .transpose()?;
    let mut owner = current.as_ref().map(|p| p.owner);
    let mut expiry = current.as_ref().map(|p| p.expires_at);
    let mut nonce = entity_nonce(host, host.head.height, tx.actor.as_bytes())?;
    for prior in &tx.ops[..index] {
        if let TxOp::Arkiv { operation, .. } = prior {
            match operation {
                ArkivOperation::Create {
                    salt, expiry: e, ..
                } => {
                    let derived =
                        arkiv::derive_entity_key(tx.chain_id, tx.actor.as_bytes(), nonce, *salt);
                    nonce = nonce.checked_add(1).ok_or(Error("NonceExhausted"))?;
                    if derived == key.as_bytes() {
                        owner = Some(*tx.actor.as_bytes());
                        expiry = Some(e.expires_at.max(height.saturating_add(e.min_lifetime)));
                    }
                }
                op if op.key() == Some(key) => match op {
                    ArkivOperation::TransferOwnership { owner: new, .. } => owner = Some(*new),
                    ArkivOperation::ExtendExpiry { expiry: e, .. } => {
                        expiry = Some(e.expires_at.max(height.saturating_add(e.min_lifetime)))
                    }
                    ArkivOperation::Delete { .. } => {
                        owner = None;
                        expiry = None;
                    }
                    _ => (),
                },
                _ => (),
            }
        }
    }
    let (signature, args) = match fault {
        crate::HostFault::Arkiv(arkiv::ArkivFault::NotFound) => {
            ("EntityNotFound(bytes32)", key_word.to_owned())
        }
        crate::HostFault::Arkiv(arkiv::ArkivFault::Expired) => {
            let Some(e) = expiry else { return Ok(None) };
            (
                "EntityExpired(bytes32,uint64)",
                format!("{key_word}{}", word(e)),
            )
        }
        crate::HostFault::Arkiv(arkiv::ArkivFault::Readonly) => {
            ("ReadOnlyEntity(bytes32)", key_word.to_owned())
        }
        crate::HostFault::Arkiv(arkiv::ArkivFault::NotOwner) => {
            let Some(owner) = owner else { return Ok(None) };
            (
                "NotOwner(bytes32,address,address)",
                format!(
                    "{key_word}{}{}",
                    &topic_address(tx.actor.as_bytes())[2..],
                    &topic_address(&owner)[2..]
                ),
            )
        }
        crate::HostFault::Arkiv(arkiv::ArkivFault::Owner) => {
            let ArkivOperation::TransferOwnership { owner, .. } = operation else {
                return Ok(None);
            };
            (
                if *owner == [0; 20] {
                    "TransferToZeroAddress(bytes32)"
                } else {
                    "TransferToSelf(bytes32)"
                },
                key_word.to_owned(),
            )
        }
        crate::HostFault::ExpiryShortened => {
            let (Some(e), Some(old)) = (operation.expiry(), expiry) else {
                return Ok(None);
            };
            (
                "ExpiryNotExtended(bytes32,uint64,uint64)",
                format!(
                    "{key_word}{}{}",
                    word(e.expires_at.max(height.saturating_add(e.min_lifetime))),
                    word(old)
                ),
            )
        }
        _ => return Ok(None),
    };
    Ok(Some(format!(
        "{}{}",
        hex(&keccak256(signature.as_bytes())[..4]),
        args
    )))
}
#[derive(Clone, Copy)]
enum Simulation {
    Call,
    Estimate,
}
fn simulation(
    host: &Host,
    call: &Value,
    mode: Simulation,
    revert: &mut Option<String>,
) -> Result<(u64, Value)> {
    if !call.is_object() {
        return Err(Error("InvalidParams"));
    }
    let from = actor(call.get("from").ok_or(Error("InvalidParams"))?)?;
    let to = actor(call.get("to").ok_or(Error("InvalidParams"))?)?;
    let data = bytes(
        call.get("data")
            .or_else(|| call.get("input"))
            .unwrap_or(&json!("0x")),
        None,
    )?;
    let value = call
        .get("value")
        .map(|v| {
            U256::from_str_radix(
                v.as_str()
                    .and_then(|s| s.strip_prefix("0x"))
                    .ok_or(Error("InvalidParams"))?,
                16,
            )
            .map_err(|_| Error("InvalidParams"))
        })
        .transpose()?
        .unwrap_or(U256::ZERO);
    let ops = if to.as_bytes() == &arkiv::ARKIV_ADDRESS {
        if value != U256::ZERO {
            return Err(Error("NonpayableCall"));
        }
        ethereum::call_ops(&host.genesis, &data)?
    } else {
        if !data.is_empty() {
            return Err(Error("UnsupportedContractCall"));
        }
        vec![TxOp::Transfer { to, amount: value }]
    };
    let spec = host.genesis.spec()?;
    let gas = call
        .get("gas")
        .map(uint)
        .transpose()?
        .unwrap_or(spec.params().user_budget);
    let mut create_nonce = entity_nonce(host, host.head.height, from.as_bytes())?;
    let mut result_keys = Vec::new();
    for op in &ops {
        if let TxOp::Arkiv { operation, .. } = op {
            match operation {
                ArkivOperation::Create { salt, .. } => {
                    result_keys.push(
                        arkiv::derive_entity_key(
                            host.genesis.genesis.chain_id,
                            from.as_bytes(),
                            create_nonce,
                            *salt,
                        )
                        .to_vec(),
                    );
                    create_nonce = create_nonce.checked_add(1).ok_or(Error("NonceExhausted"))?;
                }
                op => result_keys.push(
                    op.key()
                        .ok_or(Error("InvalidTransaction"))?
                        .as_bytes()
                        .to_vec(),
                ),
            }
        }
    }
    let tx = crate::TxEnvelope {
        chain_id: host.genesis.genesis.chain_id,
        actor: from,
        request_id: [0; 32],
        tx_budget: gas,
        ops,
    };
    let branch = host.engine.begin(
        host.engine.head(),
        Some(Cost::new(spec.params().block_budget)),
    )?;
    let h = host
        .head
        .height
        .checked_add(1)
        .ok_or(Error("CapacityReached"))?;
    let outcome = match mode {
        Simulation::Estimate => crate::exec::estimate_transaction(
            &spec,
            branch,
            &host.head,
            h,
            host.genesis.timestamp(h)?,
            tx.clone(),
            host.genesis.accounts.as_ref(),
        ),
        Simulation::Call => crate::exec::execute_block_hooked(
            &spec,
            branch,
            &host.head,
            h,
            host.genesis.timestamp(h)?,
            &crate::BlockInputs {
                admin: vec![],
                txs: vec![tx.clone()],
            },
            host.genesis.accounts.as_ref(),
            |_, _, _| Ok(()),
            |_, _, _| Ok(()),
        )
        .map(|executed| {
            executed
                .outcomes
                .txs
                .into_iter()
                .next()
                .expect("one simulated transaction")
        }),
    }
    .map_err(|e| match e {
        crate::ExecError::Account(crate::accounts::AccountFault::InsufficientBalance) => {
            Error("InsufficientBalance")
        }
        crate::ExecError::Account(_) => Error("AccountOverflow"),
        crate::ExecError::Storage(e) => Error::from(e),
        crate::ExecError::InvalidBlock(_) => Error("InvalidTransaction"),
        _ => Error("SimulationFailed"),
    })?;
    let out = &outcome;
    if out.status != TxStatus::Committed {
        if out.ops.iter().any(|op| {
            matches!(
                op,
                crate::OpOutcome::AccountRejected {
                    fault: crate::accounts::AccountFault::InsufficientBalance,
                    ..
                }
            )
        }) {
            return Err(Error("InsufficientBalance"));
        }
        *revert = revert_data(host, &tx, out, h)?;
        return Err(Error("ExecutionReverted"));
    }
    let result = if to.as_bytes() == &arkiv::ARKIV_ADDRESS {
        let mut encoded = format!("0x{}{}", word(32), word(result_keys.len() as u64));
        for key in result_keys {
            encoded.push_str(&hex(&key)[2..]);
        }
        encoded
    } else {
        "0x".into()
    };
    Ok((out.spent, json!(result)))
}
fn dispatch(host: &mut Host, state: &mut RpcState, method: &str, params: &Value) -> Result<Value> {
    state.revert_data = None;
    if let Some(result) = super::rpc_reads::dispatch(host, method, params) {
        return result;
    }
    let a = args(params)?;
    match method {
        "web3_clientVersion" => Ok(json!("arkiv-native/ethereum-compat-v1")),
        "net_version" => Ok(json!(host.genesis.genesis.chain_id.to_string())),
        "net_listening" => Ok(json!(true)),
        "eth_chainId" => Ok(json!(quantity(host.genesis.genesis.chain_id))),
        "eth_blockNumber" => Ok(json!(quantity(host.head.height))),
        "eth_syncing" => Ok(json!(false)),
        "eth_gasPrice" => Ok(json!(quantity(crate::accounts::GAS_PRICE_WEI))),
        "eth_maxPriorityFeePerGas" => Ok(json!("0x0")),
        "eth_accounts" => Ok(json!([])),
        "eth_getBalance" | "eth_getTransactionCount" => {
            let owner = actor(a.first().ok_or(Error("InvalidParams"))?)?;
            let height = at(host, a.get(1))?;
            let account = host.account(height, owner)?;
            if method == "eth_getBalance" {
                Ok(json!(format!("{:#x}", account.balance)))
            } else {
                Ok(json!(quantity(
                    if a.get(1).and_then(Value::as_str) == Some("pending") {
                        host.pool.contiguous_next(owner, account.nonce)
                    } else {
                        account.nonce
                    }
                )))
            }
        }
        "eth_getCode" => {
            actor(a.first().ok_or(Error("InvalidParams"))?)?;
            at(host, a.get(1))?;
            Ok(json!("0x"))
        }
        "eth_getBlockByNumber" => {
            let h = match at(host, a.first()) {
                Ok(h) => h,
                Err(Error("UnknownBlock")) => return Ok(Value::Null),
                Err(e) => return Err(e),
            };
            block_json(host, h, a.get(1).and_then(Value::as_bool).unwrap_or(false))
        }
        "eth_getBlockByHash" => {
            index(host, state)?;
            let hash = a
                .first()
                .and_then(Value::as_str)
                .ok_or(Error("InvalidParams"))?;
            match state.blocks.get(&hash.to_ascii_lowercase()) {
                Some(h) => block_json(host, *h, a.get(1).and_then(Value::as_bool).unwrap_or(false)),
                None => Ok(Value::Null),
            }
        }
        "eth_getBlockTransactionCountByNumber" => {
            let h = at(host, a.first())?;
            Ok(json!(quantity(host.block(h)?.inputs.txs.len() as u64)))
        }
        "eth_getTransactionByHash" | "eth_getTransactionReceipt" => {
            let hash = a
                .first()
                .and_then(Value::as_str)
                .ok_or(Error("InvalidParams"))?;
            if let Some((h, i)) = locate(host, state, hash)? {
                if method == "eth_getTransactionReceipt" {
                    receipt(host, h, i)
                } else {
                    let b = host.block(h)?;
                    transaction(&b.inputs.txs[i], Some((&b.header.header, i)))
                }
            } else if method == "eth_getTransactionByHash" {
                match host.pool.by_hash(hash) {
                    Some(t) => transaction(t, None),
                    None => Ok(Value::Null),
                }
            } else {
                Ok(Value::Null)
            }
        }
        "eth_getTransactionByBlockNumberAndIndex" => {
            let h = at(host, a.first())?;
            let i = uint(a.get(1).ok_or(Error("InvalidParams"))?)? as usize;
            let b = host.block(h)?;
            match b.inputs.txs.get(i) {
                Some(t) => transaction(t, Some((&b.header.header, i))),
                None => Ok(Value::Null),
            }
        }
        "eth_sendRawTransaction" => {
            let raw = bytes(a.first().ok_or(Error("InvalidParams"))?, None)?;
            let tx = SignedTx::from_ethereum(&host.genesis, &raw)?;
            let hash = hex(tx.hash().as_slice());
            host.submit(tx)?;
            Ok(json!(hash))
        }
        "eth_estimateGas" => {
            if a.get(1)
                .is_some_and(|v| !matches!(v.as_str(), Some("latest" | "pending")))
            {
                return Err(Error("UnsupportedHistoricalSimulation"));
            }
            let (gas, _) = simulation(
                host,
                a.first().ok_or(Error("InvalidParams"))?,
                Simulation::Estimate,
                &mut state.revert_data,
            )?;
            // A plain transfer has a fixed cost and needs no estimate margin.
            let call = a.first().ok_or(Error("InvalidParams"))?;
            let to = actor(call.get("to").ok_or(Error("InvalidParams"))?)?;
            if to.as_bytes() != &arkiv::ARKIV_ADDRESS {
                return Ok(json!(quantity(gas)));
            }
            Ok(json!(quantity(
                gas.saturating_add(gas / 10)
                    .saturating_add(1000)
                    .min(host.genesis.genesis.params.user_budget)
            )))
        }
        "eth_call" => {
            let c = a.first().ok_or(Error("InvalidParams"))?;
            let to = actor(c.get("to").ok_or(Error("InvalidParams"))?)?;
            let data = bytes(
                c.get("data")
                    .or_else(|| c.get("input"))
                    .ok_or(Error("InvalidParams"))?,
                None,
            )?;
            if to.as_bytes() != &arkiv::ARKIV_ADDRESS {
                return Err(Error("UnsupportedContractCall"));
            }
            if data.len() == 36 && data[..4] == keccak256(b"entityNonce(address)")[..4] {
                if data[4..16].iter().any(|b| *b != 0) {
                    return Err(Error("InvalidParams"));
                }
                let address: [u8; 20] =
                    data[16..].try_into().map_err(|_| Error("InvalidParams"))?;
                Ok(json!(format!(
                    "0x{}",
                    word(entity_nonce(host, at(host, a.get(1))?, &address)?)
                )))
            } else {
                if at(host, a.get(1))? != host.head.height {
                    return Err(Error("UnsupportedHistoricalSimulation"));
                }
                simulation(host, c, Simulation::Call, &mut state.revert_data).map(|(_, v)| v)
            }
        }
        "eth_getLogs" => logs(host, state, a.first().ok_or(Error("InvalidParams"))?),
        "eth_newFilter" | "eth_newBlockFilter" => {
            let now = std::time::Instant::now();
            expire_filters(state, now);
            if state.filters.len() >= FILTER_CAP {
                return Err(Error("CapacityReached"));
            }
            let mut spec = if method == "eth_newFilter" {
                let spec = a.first().ok_or(Error("InvalidParams"))?;
                validate_filter(spec)?;
                spec.clone()
            } else {
                json!({})
            };
            let next = if let Some(hash) = spec.get("blockHash").and_then(Value::as_str) {
                index(host, state)?;
                *state
                    .blocks
                    .get(&hash.to_ascii_lowercase())
                    .ok_or(Error("UnknownBlock"))?
            } else {
                let start = if spec.get("fromBlock").is_some() {
                    filter_height(host.head.height, spec.get("fromBlock"))?
                } else {
                    host.head
                        .height
                        .checked_add(1)
                        .ok_or(Error("CapacityReached"))?
                };
                // Freeze the creation-time starting point for getFilterLogs.
                spec["fromBlock"] = json!(quantity(start));
                start
            };
            state.next_filter = state
                .next_filter
                .checked_add(1)
                .ok_or(Error("CapacityReached"))?;
            let id = state.next_filter;
            state.filters.insert(
                id,
                Filter {
                    spec,
                    next,
                    blocks: method == "eth_newBlockFilter",
                    touched: now,
                },
            );
            Ok(json!(quantity(id)))
        }
        "eth_uninstallFilter" => {
            expire_filters(state, std::time::Instant::now());
            let id = uint(a.first().ok_or(Error("InvalidParams"))?)?;
            Ok(json!(state.filters.remove(&id).is_some()))
        }
        "eth_getFilterChanges" | "eth_getFilterLogs" => {
            let now = std::time::Instant::now();
            expire_filters(state, now);
            let id = uint(a.first().ok_or(Error("InvalidParams"))?)?;
            let f = state.filters.get_mut(&id).ok_or(Error("FilterNotFound"))?;
            f.touched = now;
            let mut spec = f.spec.clone();
            let next = f.next;
            let blocks = f.blocks;
            if spec.get("blockHash").is_some() {
                // A block-hash filter is a fixed one-shot change set. Never add
                // fromBlock/toBlock to this mutually exclusive selector.
                if method == "eth_getFilterChanges" && next > 0 {
                    let hash = spec["blockHash"].as_str().ok_or(Error("InvalidParams"))?;
                    let height = *state
                        .blocks
                        .get(&hash.to_ascii_lowercase())
                        .ok_or(Error("UnknownBlock"))?;
                    if next > height {
                        return Ok(json!([]));
                    }
                }
                let result = logs(host, state, &spec)?;
                if method == "eth_getFilterChanges" {
                    state
                        .filters
                        .get_mut(&id)
                        .ok_or(Error("FilterNotFound"))?
                        .next = next.checked_add(1).ok_or(Error("CapacityReached"))?;
                }
                return Ok(result);
            }
            if method == "eth_getFilterLogs" && blocks {
                return Err(Error("InvalidParams"));
            }
            let start = if method == "eth_getFilterLogs" {
                filter_height(host.head.height, spec.get("fromBlock"))?
            } else {
                next
            };
            let bound = filter_height(host.head.height, spec.get("toBlock"))?.min(host.head.height);
            if start > bound {
                return Ok(json!([]));
            }
            let end = if method == "eth_getFilterLogs" {
                bound
            } else {
                bound.min(start.saturating_add(LOG_BLOCK_CAP - 1))
            };
            let result = if blocks {
                (start..=end)
                    .map(|h| {
                        host.certificate(h)
                            .map(|h| json!(hex(h.header.hash().as_slice())))
                    })
                    .collect::<Result<Vec<_>>>()
                    .map(|v| json!(v))?
            } else {
                spec["fromBlock"] = json!(quantity(start));
                spec["toBlock"] = json!(quantity(end));
                logs(host, state, &spec)?
            };
            if method == "eth_getFilterChanges" {
                state
                    .filters
                    .get_mut(&id)
                    .ok_or(Error("FilterNotFound"))?
                    .next = end.checked_add(1).ok_or(Error("CapacityReached"))?;
            }
            Ok(result)
        }
        "eth_feeHistory" => {
            let count = uint(a.first().ok_or(Error("InvalidParams"))?)?.min(1024);
            let end = at(host, a.get(1))?;
            let count = count.min(end + 1);
            let start = end + 1 - count;
            let percentiles = a
                .get(2)
                .map(|v| v.as_array().ok_or(Error("InvalidParams")))
                .transpose()?;
            if let Some(p) = percentiles {
                if p.len() > 100 {
                    return Err(Error("InvalidParams"));
                }
                let mut prev = 0.0;
                for v in p {
                    let n = v.as_f64().ok_or(Error("InvalidParams"))?;
                    if !n.is_finite() || !(0.0..=100.0).contains(&n) || n < prev {
                        return Err(Error("InvalidParams"));
                    }
                    prev = n;
                }
            }
            let mut ratios = Vec::new();
            for h in start..=end {
                let b = host.block(h)?;
                let used = b.outcomes.spent;
                ratios.push(used as f64 / host.genesis.genesis.params.block_budget as f64);
            }
            Ok(
                json!({"oldestBlock":quantity(start),"baseFeePerGas":vec![quantity(crate::accounts::GAS_PRICE_WEI);count as usize+1],"gasUsedRatio":ratios,"reward":vec![vec!["0x0";percentiles.map_or(0,Vec::len)];count as usize]}),
            )
        }
        _ => Err(Error("MethodNotFound")),
    }
}
fn error(id: Value, e: Error) -> Value {
    let message = match e.0 {
        "InsufficientBalance" => "insufficient funds for gas * price + value",
        _ => e.0,
    };
    json!({"jsonrpc":"2.0","id":id,"error":{"code":match e.0{"MethodNotFound"=>-32601,"InvalidParams"=>-32602,"InvalidRequest"=>-32600,"ExecutionReverted"=>3,_=>-32000},"message":message}})
}
async fn single(rt: &Shared, request: Value) -> Value {
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    if !matches!(id, Value::Null | Value::String(_) | Value::Number(_)) {
        return error(Value::Null, Error("InvalidRequest"));
    }
    let method = match request.get("method").and_then(Value::as_str) {
        Some(m) if request.get("jsonrpc").and_then(Value::as_str) == Some("2.0") => m.to_owned(),
        _ => return error(id, Error("InvalidRequest")),
    };
    let params = request.get("params").cloned().unwrap_or_else(|| json!([]));
    // RPC bursts wait for the node mutex in the blocking pool instead of
    // competing for the native REST API's four admission permits.
    let runtime = rt.clone();
    let response_id = id.clone();
    let job = tokio::task::spawn_blocking(move || {
        let mut node = runtime
            .node
            .lock()
            .map_err(|_| Error("StorageUnavailable"))?;
        let forward = matches!(&*node, Node::Light { .. })
            || matches!(&*node,Node::Full(h) if method=="eth_sendRawTransaction"&&!h.is_producer());
        let local = if forward {
            None
        } else {
            let mut state = runtime
                .rpc
                .lock()
                .map_err(|_| Error("StorageUnavailable"))?;
            Some(match &mut *node {
                Node::Full(h) => match dispatch(h, &mut state, &method, &params) {
                    Ok(v) => Ok(json!({"jsonrpc":"2.0","id":response_id,"result":v})),
                    Err(e) => {
                        let mut response = error(response_id, e);
                        if let Some(data) = state.revert_data.take() {
                            response["error"]["data"] = json!(data);
                        }
                        Ok(response)
                    }
                },
                _ => Err(Error("FullNodeRequired")),
            })
        };
        Ok::<_, Error>(local)
    })
    .await;
    let result = match job {
        Ok(Ok(Some(result))) => result,
        Ok(Ok(None)) => match rt.peer.as_ref() {
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
            None => Err(Error("FullNodeRequired")),
        },
        Ok(Err(e)) => Err(e),
        Err(_) => Err(Error("StorageUnavailable")),
    };
    if request.get("id").is_none() {
        Value::Null
    } else {
        result.unwrap_or_else(|e| error(id, e))
    }
}
pub async fn handle_request(rt: &Shared, request: Value) -> Value {
    const CAP: usize = 8 * 1024 * 1024;
    if let Some(batch) = request.as_array() {
        if batch.is_empty() || batch.len() > 64 {
            return error(Value::Null, Error("InvalidRequest"));
        }
        let mut out = Vec::new();
        let mut size = 2;
        for r in batch {
            let v = single(rt, r.clone()).await;
            if !v.is_null() {
                size += serde_json::to_vec(&v).map_or(CAP + 1, |b| b.len() + 1);
                if size > CAP {
                    return error(Value::Null, Error("ResponseTooLarge"));
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
            error(id, Error("ResponseTooLarge"))
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
    let request =
        match serde_json::from_slice(&input) {
            Ok(v) => v,
            Err(_) => return axum::Json(
                json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"Parse error"}}),
            )
            .into_response(),
        };
    let result = handle_request(&rt, request).await;
    if result.is_null() {
        StatusCode::NO_CONTENT.into_response()
    } else {
        axum::Json(result).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        crypto::Signer,
        history::History,
        protocol::{INPUT_CAP, SIGNED_TX_OVERHEAD, SignedGenesis, SignedHeader, TX_CAP},
    };
    use super::*;
    use arkiv_db::{CellName, Value as DbValue, durable::Options};
    use arkiv_db_memory::MemoryVersionedStore;
    use std::sync::Arc;
    fn host() -> Host {
        let signer = Signer::from_bytes([2; 32]).unwrap();
        let mut g = crate::demo::workload::genesis(9300, 1000);
        g.admins.clear();
        g.params.max_admin_per_block = 0;
        g.params.max_tx_bytes = TX_CAP - SIGNED_TX_OVERHEAD;
        g.params.max_block_input_bytes = INPUT_CAP;
        g.pricing = crate::gas::schedule();
        g.params.block_budget = 100_000_000;
        g.params.user_budget = 90_000_000;
        let g = SignedGenesis::new(g, [7; 16], *signer.address().as_bytes(), 1000)
            .unwrap()
            .with_accounts(
                crate::accounts::AccountConfig::new(vec![(
                    signer.address(),
                    U256::from(10_000_000_000_000_000_000u64),
                )])
                .unwrap(),
            )
            .unwrap();
        let certificate = SignedHeader::sign(&g, g.header(), &signer).unwrap();
        let history = History::create(Arc::new(MemoryVersionedStore::new()), g.hash()).unwrap();
        let mut host = Host::create(
            Arc::new(MemoryVersionedStore::new()),
            history,
            g,
            [1; 16],
            certificate,
            Some(signer),
            Options::default(),
        )
        .unwrap();
        host.produce().unwrap();
        host
    }
    fn native(host: &Host, nonce: u64, ops: Vec<ArkivOperation>) -> SignedTx {
        let signer = Signer::from_bytes([2; 32]).unwrap();
        let mut id = [0; 32];
        id[..8].copy_from_slice(&nonce.to_be_bytes());
        SignedTx::sign(
            &host.genesis,
            nonce,
            crate::TxEnvelope {
                chain_id: 9300,
                actor: signer.address(),
                request_id: id,
                tx_budget: 2_000_000,
                ops: ops
                    .into_iter()
                    .map(|operation| TxOp::Arkiv {
                        namespace: arkiv::entity_namespace(),
                        operation,
                    })
                    .collect(),
            },
            &signer,
        )
        .unwrap()
    }
    #[test]
    fn log_owners_follow_operations_and_failed_batches_emit_nothing() {
        let mut h = host();
        let caller = *Signer::from_bytes([2; 32]).unwrap().address().as_bytes();
        let key = arkiv::derive_entity_key(9300, &caller, 0, 3);
        let new = [4; 20];
        let create = ArkivOperation::Create {
            salt: 3,
            flags: 0,
            expiry: crate::Expiry {
                expires_at: 0,
                min_lifetime: 20,
            },
            attributes: vec![(CellName::new("$payload").unwrap(), DbValue::bytes(b"test"))],
        };
        let tx = native(
            &h,
            1,
            vec![
                create,
                ArkivOperation::TransferOwnership {
                    key: RecordKey::new(&key).unwrap(),
                    owner: new,
                },
            ],
        );
        h.submit(tx).unwrap();
        h.produce().unwrap();
        let events = block_logs(&h, 2).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["topics"][1], hex(&key));
        assert_eq!(events[0]["data"], format!("0x{}{}", word(22), word(0)));
        assert_eq!(events[1]["topics"][2], topic_address(&caller));
        assert_eq!(events[1]["topics"][3], topic_address(&new));
        let bad = native(
            &h,
            2,
            vec![
                ArkivOperation::Create {
                    salt: 4,
                    flags: 0,
                    expiry: crate::Expiry {
                        expires_at: 0,
                        min_lifetime: 21,
                    },
                    attributes: vec![],
                },
                ArkivOperation::Delete {
                    key: RecordKey::new(&[9; 32]).unwrap(),
                },
            ],
        );
        h.submit(bad).unwrap();
        h.produce().unwrap();
        assert!(block_logs(&h, 3).unwrap().is_empty());
        assert_eq!(receipt(&h, 3, 0).unwrap()["status"], "0x0");
        assert_eq!(entity_nonce(&h, 3, &caller).unwrap(), 1);
    }
    #[test]
    fn estimate_has_no_nonce_balance_or_state_effect() {
        let h = host();
        let who = Signer::from_bytes([2; 32]).unwrap().address();
        let before = h.account(h.head.height, who).unwrap();
        let root = h.head.state_root;
        let call = json!({"from":hex(who.as_bytes()),"to":hex(&[4;20]),"value":"0x1"});
        let (gas, _) = simulation(&h, &call, Simulation::Estimate, &mut None).unwrap();
        assert_eq!(gas, 21_000);
        assert_eq!(h.account(h.head.height, who).unwrap(), before);
        assert_eq!(h.head.state_root, root);
        assert_eq!(h.head.height, 1);
    }
    #[test]
    fn unfunded_estimates_match_execution_gas_without_mutating_state() {
        let mut h = host();
        let funded = Signer::from_bytes([2; 32]).unwrap().address();
        let empty = Signer::from_bytes([3; 32]).unwrap().address();
        let vectors: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/ethereum-sdk-vectors.json"
        ))
        .unwrap();
        let before = h.head.clone();
        let snapshot = h.engine.snapshot(h.engine.head()).unwrap();
        let burned = snapshot.try_host_get(&crate::accounts::BURN_KEY).unwrap();
        let balances = [funded, empty].map(|a| h.account(h.head.height, a).unwrap());
        let mut state = RpcState::default();
        let mut call = json!({"from":hex(empty.as_bytes()),"to":ADDRESS,"data":vectors["data"]});
        let estimate = dispatch(
            &mut h,
            &mut state,
            "eth_estimateGas",
            &json!([call, "pending"]),
        )
        .unwrap();
        let (used, _) = simulation(&h, &call, Simulation::Estimate, &mut None).unwrap();
        assert_eq!(uint(&estimate).unwrap(), used + used / 10 + 1000);
        // A returned estimate must itself be sufficient, including rent.
        call["gas"] = estimate.clone();
        assert_eq!(
            simulation(&h, &call, Simulation::Estimate, &mut None)
                .unwrap()
                .0,
            used
        );
        // eth_call retains its existing account rules.
        assert_eq!(
            dispatch(&mut h, &mut state, "eth_call", &json!([call]))
                .unwrap_err()
                .0,
            "InsufficientBalance"
        );
        call["from"] = json!(hex(funded.as_bytes()));
        assert_eq!(
            dispatch(&mut h, &mut state, "eth_estimateGas", &json!([call])).unwrap(),
            estimate
        );
        assert_eq!(
            simulation(&h, &call, Simulation::Call, &mut None)
                .unwrap()
                .0,
            used
        );
        call["gas"] = json!("0x1");
        assert_eq!(
            dispatch(&mut h, &mut state, "eth_estimateGas", &json!([call]))
                .unwrap_err()
                .0,
            "ExecutionReverted"
        );
        assert_eq!(h.head, before);
        assert_eq!(
            h.engine
                .snapshot(h.engine.head())
                .unwrap()
                .try_host_get(&crate::accounts::BURN_KEY)
                .unwrap(),
            burned
        );
        assert!(h.pool.is_empty());
        for (a, balance) in [funded, empty].into_iter().zip(balances) {
            assert_eq!(h.account(h.head.height, a).unwrap(), balance);
            assert_eq!(entity_nonce(&h, h.head.height, a.as_bytes()).unwrap(), 0);
        }
        let operations = ethereum::operations(&bytes(&vectors["data"], None).unwrap()).unwrap();
        h.submit(native(&h, 1, operations)).unwrap();
        h.produce().unwrap();
        assert_eq!(h.block(2).unwrap().outcomes.txs[0].spent, used);
    }
    #[test]
    fn transfer_estimation_ignores_fee_and_value_affordability() {
        let mut h = host();
        let empty = Signer::from_bytes([3; 32]).unwrap().address();
        let recipient = Signer::from_bytes([4; 32]).unwrap().address();
        let before = h.head.clone();
        let mut state = RpcState::default();
        for value in [U256::ZERO, U256::from(1), U256::MAX] {
            for to in [empty, recipient] {
                let call = json!({"from":hex(empty.as_bytes()),"to":hex(to.as_bytes()),"value":format!("{value:#x}")});
                assert_eq!(
                    dispatch(&mut h, &mut state, "eth_estimateGas", &json!([call])).unwrap(),
                    quantity(21_000)
                );
            }
        }
        assert_eq!(h.head, before);
        assert!(h.pool.is_empty());
        for a in [empty, recipient] {
            assert_eq!(h.account(h.head.height, a).unwrap().balance, U256::ZERO);
            assert_eq!(h.account(h.head.height, a).unwrap().nonce, 0);
        }
    }
    fn signed_transfer(signer: &Signer, nonce: u64, to: OwnerId, value: U256) -> Value {
        signed_transfer_with_gas(signer, nonce, to, value, 100_000, false)
    }
    fn signed_transfer_with_gas(
        signer: &Signer,
        nonce: u64,
        to: OwnerId,
        value: U256,
        gas: u64,
        typed: bool,
    ) -> Value {
        use alloy_rlp::Encodable;
        let list = |payload: Vec<u8>| {
            let mut encoded = Vec::new();
            alloy_rlp::Header {
                list: true,
                payload_length: payload.len(),
            }
            .encode(&mut encoded);
            encoded.extend(payload);
            encoded
        };
        let mut payload = Vec::new();
        if typed {
            9300u64.encode(&mut payload);
        }
        nonce.encode(&mut payload);
        if typed {
            0u8.encode(&mut payload);
        }
        crate::accounts::GAS_PRICE_WEI.encode(&mut payload);
        gas.encode(&mut payload);
        to.as_bytes().as_slice().encode(&mut payload);
        value.encode(&mut payload);
        (&[] as &[u8]).encode(&mut payload);
        if typed {
            payload.extend(list(Vec::new()));
            let mut signing = vec![2];
            signing.extend(list(payload.clone()));
            let sig = signer.sign_digest(keccak256(signing)).unwrap();
            sig[64].encode(&mut payload);
            U256::from_be_slice(&sig[..32]).encode(&mut payload);
            U256::from_be_slice(&sig[32..64]).encode(&mut payload);
            let mut raw = vec![2];
            raw.extend(list(payload));
            return json!(hex(&raw));
        }
        let mut unsigned = payload.clone();
        9300u64.encode(&mut unsigned);
        0u8.encode(&mut unsigned);
        0u8.encode(&mut unsigned);
        let sig = signer.sign_digest(keccak256(list(unsigned))).unwrap();
        (9300 * 2 + 35 + sig[64] as u64).encode(&mut payload);
        U256::from_be_slice(&sig[..32]).encode(&mut payload);
        U256::from_be_slice(&sig[32..64]).encode(&mut payload);
        json!(hex(&list(payload)))
    }
    #[test]
    fn legacy_and_eip1559_transfers_execute_with_exact_intrinsic_gas() {
        for typed in [false, true] {
            let mut h = host();
            let signer = Signer::from_bytes([2; 32]).unwrap();
            let recipient = Signer::from_bytes([4; 32]).unwrap().address();
            let before = h.account(h.head.height, signer.address()).unwrap();
            let mut state = RpcState::default();
            let call = json!({"from":hex(signer.address().as_bytes()),"to":hex(recipient.as_bytes()),"value":"0x1","gas":"0x5208"});
            assert_eq!(
                dispatch(&mut h, &mut state, "eth_estimateGas", &json!([call])).unwrap(),
                quantity(21_000)
            );
            let raw = signed_transfer_with_gas(
                &signer,
                before.nonce,
                recipient,
                U256::from(1),
                21_000,
                typed,
            );
            let hash =
                dispatch(&mut h, &mut state, "eth_sendRawTransaction", &json!([raw])).unwrap();
            h.produce().unwrap();
            let result = dispatch(
                &mut h,
                &mut state,
                "eth_getTransactionReceipt",
                &json!([hash]),
            )
            .unwrap();
            assert_eq!(result["status"], "0x1");
            assert_eq!(result["gasUsed"], "0x5208");
            assert_eq!(
                h.account(h.head.height, signer.address()).unwrap().balance,
                before.balance - U256::from(1) - crate::accounts::fee(21_000)
            );
            assert_eq!(
                h.account(h.head.height, recipient).unwrap().balance,
                U256::from(1)
            );
            assert_eq!(
                h.account(h.head.height, signer.address()).unwrap().nonce,
                before.nonce + 1
            );
        }
    }
    #[tokio::test]
    async fn raw_send_enforces_balance_and_pending_reservations_after_estimation() {
        let h = host();
        let signer = Signer::from_bytes([3; 32]).unwrap();
        let actor = signer.address();
        let recipient = Signer::from_bytes([4; 32]).unwrap().address();
        let raw = signed_transfer(&signer, 0, recipient, U256::from(1));
        let rt =
            super::super::runtime::Runtime::new(Node::Full(Box::new(h)), None, None, None).unwrap();
        let request =
            |method, params| json!({"jsonrpc":"2.0","id":7,"method":method,"params":params});
        let call =
            json!({"from":hex(actor.as_bytes()),"to":hex(recipient.as_bytes()),"value":"0x1"});
        let estimate = handle_request(&rt, request("eth_estimateGas", json!([call]))).await;
        assert_eq!(estimate["result"], quantity(21_000));
        let rejected = handle_request(&rt, request("eth_sendRawTransaction", json!([raw]))).await;
        assert_eq!(
            rejected,
            json!({"jsonrpc":"2.0","id":7,"error":{"code":-32000,"message":"insufficient funds for gas * price + value"}})
        );
        {
            let mut guard = rt.node.lock().unwrap();
            let Node::Full(h) = &mut *guard else {
                panic!("full node")
            };
            assert!(h.pool.is_empty());
            assert_eq!(h.account(h.head.height, actor).unwrap().nonce, 0);
            assert_eq!(h.account(h.head.height, actor).unwrap().balance, U256::ZERO);
            // Fund just one reservation, far below the default simulation gas cap.
            let fund = crate::TxEnvelope {
                chain_id: 9300,
                actor: Signer::from_bytes([2; 32]).unwrap().address(),
                request_id: [42; 32],
                tx_budget: 100_000,
                ops: vec![TxOp::Transfer {
                    to: actor,
                    amount: crate::accounts::fee(100_000) + U256::from(1),
                }],
            };
            h.submit(
                SignedTx::sign(&h.genesis, 1, fund, &Signer::from_bytes([2; 32]).unwrap()).unwrap(),
            )
            .unwrap();
            h.produce().unwrap();
        }
        let accepted = handle_request(&rt, request("eth_sendRawTransaction", json!([raw]))).await;
        assert!(accepted["result"].is_string(), "{accepted}");
        assert_eq!(
            handle_request(&rt, request("eth_estimateGas", json!([call]))).await["result"],
            estimate["result"]
        );
        let second = signed_transfer(&signer, 1, recipient, U256::from(1));
        assert_eq!(
            handle_request(&rt, request("eth_sendRawTransaction", json!([second]))).await,
            rejected
        );
        let mut guard = rt.node.lock().unwrap();
        let Node::Full(h) = &mut *guard else {
            panic!("full node")
        };
        assert_eq!(h.pool.len(), 1);
        h.produce().unwrap();
        assert_eq!(h.account(h.head.height, actor).unwrap().nonce, 1);
        assert_eq!(
            h.account(h.head.height, actor).unwrap().balance,
            crate::accounts::fee(79_000)
        );
        assert_eq!(
            h.account(h.head.height, recipient).unwrap().balance,
            U256::from(1)
        );
    }
    #[test]
    fn malformed_hex_and_log_ranges_are_rejected_without_panics() {
        for s in ["0x€a", "0xéé", "0xz0", "0x1", "0X11"] {
            assert!(bytes(&json!(s), None).is_err());
        }
        let h = host();
        assert!(
            logs(
                &h,
                &mut RpcState::default(),
                &json!({"blockHash":ZERO_HASH,"fromBlock":"0x0"})
            )
            .is_err()
        );
        assert!(
            logs(
                &h,
                &mut RpcState::default(),
                &json!({"fromBlock":"0x1","toBlock":"0x0"})
            )
            .is_err()
        );
    }
    #[tokio::test]
    async fn jsonrpc_batches_notifications_and_errors_keep_protocol_shape() {
        let rt =
            super::super::runtime::Runtime::new(Node::Full(Box::new(host())), None, None, None)
                .unwrap();
        let response=handle_request(&rt,json!([{"jsonrpc":"2.0","id":1,"method":"eth_chainId"},{"jsonrpc":"2.0","method":"eth_chainId"},{"jsonrpc":"2.0","id":2,"method":"unknown"}])).await;
        assert_eq!(response.as_array().unwrap().len(), 2);
        assert_eq!(response[0]["result"], "0x2454");
        assert_eq!(response[1]["error"]["code"], -32601);
        let bad =
            handle_request(&rt, json!({"jsonrpc":"2.0","id":{},"method":"eth_chainId"})).await;
        assert_eq!(bad["error"]["code"], -32600);
        let notification = handle_request(&rt, json!({"jsonrpc":"2.0","method":"unknown"})).await;
        assert!(notification.is_null());
    }
    #[tokio::test]
    async fn concurrent_rpc_receipt_reads_do_not_use_rest_admission_slots() {
        let rt =
            super::super::runtime::Runtime::new(Node::Full(Box::new(host())), None, None, None)
                .unwrap();
        // Saturate native REST admission while a scanner-sized HTTP burst runs.
        let _rest_permits = rt.slots.clone().acquire_many_owned(4).await.unwrap();
        let mut requests = tokio::task::JoinSet::new();
        for id in 0..32 {
            let rt = rt.clone();
            requests.spawn(async move {
                let response = http(
                    State(rt),
                    Body::from(
                        json!({"jsonrpc":"2.0","id":id,"method":"eth_getTransactionReceipt","params":[ZERO_HASH]}).to_string(),
                    ),
                )
                .await;
                assert_eq!(response.status(), StatusCode::OK);
                let body = to_bytes(response.into_body(), 4096).await.unwrap();
                let value: Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(value, json!({"jsonrpc":"2.0","id":id,"result":null}));
            });
        }
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while let Some(result) = requests.join_next().await {
                result.unwrap();
            }
        })
        .await
        .unwrap();
    }
    #[test]
    fn filters_reject_unknown_fields_and_bound_every_retained_dimension() {
        let address = hex(&[1; 20]);
        let topic = hex(&[2; 32]);
        validate_filter(&json!({"address":[address.clone()],"topics":[null,[topic.clone(),null]],"fromBlock":"earliest","toBlock":"0xffffffffffffffff"})).unwrap();
        for bad in [
            json!({"typo":true}),
            json!({"address":null}),
            json!({"address":topic}),
            json!({"address":vec![address;65]}),
            json!({"topics":null}),
            json!({"topics":vec![Value::Null;5]}),
            json!({"topics":[vec![Value::Null;65]]}),
            json!({"topics":["0x01"]}),
            json!({"fromBlock":false}),
            json!({"fromBlock":"0x00000000000000000001"}),
            json!({"blockHash":ZERO_HASH,"toBlock":"latest"}),
        ] {
            assert!(validate_filter(&bad).is_err(), "{bad}");
            let mut h = host();
            let mut state = RpcState::default();
            assert!(logs(&h, &mut state, &bad).is_err());
            assert!(dispatch(&mut h, &mut state, "eth_newFilter", &json!([bad])).is_err());
            assert!(state.filters.is_empty());
        }
    }
    fn create_filter_event(h: &mut Host, nonce: u64) {
        let tx = native(
            h,
            nonce,
            vec![ArkivOperation::Create {
                salt: nonce as u128,
                flags: 0,
                expiry: crate::Expiry {
                    expires_at: 0,
                    min_lifetime: 20,
                },
                attributes: vec![],
            }],
        );
        h.submit(tx).unwrap();
        h.produce().unwrap();
    }
    #[test]
    fn finite_and_block_hash_filters_keep_bounds_and_poll_without_duplicates() {
        let mut h = host();
        let mut state = RpcState::default();
        create_filter_event(&mut h, 1);
        create_filter_event(&mut h, 2);
        let hash = hex(h.block(2).unwrap().hash().as_slice());
        for spec in [
            json!({"fromBlock":"0x2","toBlock":"0x2"}),
            json!({"blockHash":hash}),
        ] {
            let id = dispatch(&mut h, &mut state, "eth_newFilter", &json!([spec])).unwrap();
            let first = dispatch(&mut h, &mut state, "eth_getFilterChanges", &json!([id])).unwrap();
            assert_eq!(first.as_array().unwrap().len(), 1);
            assert_eq!(first[0]["blockNumber"], "0x2");
            assert_eq!(
                dispatch(&mut h, &mut state, "eth_getFilterChanges", &json!([id])).unwrap(),
                json!([])
            );
            let all = dispatch(&mut h, &mut state, "eth_getFilterLogs", &json!([id])).unwrap();
            assert_eq!(all, first);
        }
        let id = dispatch(
            &mut h,
            &mut state,
            "eth_newFilter",
            &json!([{"fromBlock":"0x5","toBlock":"0x5"}]),
        )
        .unwrap();
        assert_eq!(
            dispatch(&mut h, &mut state, "eth_getFilterChanges", &json!([id])).unwrap(),
            json!([])
        );
        assert_eq!(
            dispatch(&mut h, &mut state, "eth_getFilterLogs", &json!([id])).unwrap(),
            json!([])
        );
    }
    #[test]
    fn filters_expire_release_capacity_and_refresh_on_poll() {
        let mut h = host();
        let mut state = RpcState::default();
        let id = dispatch(&mut h, &mut state, "eth_newFilter", &json!([{}])).unwrap();
        let numeric = uint(&id).unwrap();
        state.filters.get_mut(&numeric).unwrap().touched = std::time::Instant::now() - FILTER_TTL;
        let err = dispatch(&mut h, &mut state, "eth_getFilterChanges", &json!([id])).unwrap_err();
        assert_eq!(err.0, "FilterNotFound");
        assert!(state.filters.is_empty());
        let id = dispatch(&mut h, &mut state, "eth_newBlockFilter", &json!([])).unwrap();
        let numeric = uint(&id).unwrap();
        state.filters.get_mut(&numeric).unwrap().touched =
            std::time::Instant::now() - FILTER_TTL / 2;
        let before = std::time::Instant::now();
        dispatch(&mut h, &mut state, "eth_getFilterChanges", &json!([id])).unwrap();
        assert!(state.filters[&numeric].touched >= before);
        state.filters.get_mut(&numeric).unwrap().touched = before - FILTER_TTL;
        dispatch(&mut h, &mut state, "eth_newFilter", &json!([{}])).unwrap();
        assert!(!state.filters.contains_key(&numeric));
        assert_eq!(state.filters.len(), 1);
    }
    #[tokio::test]
    async fn execute_call_returns_abi_keys_and_missing_entity_errors_decode() {
        use alloy_sol_types::{SolCall, SolValue};
        let h = host();
        let who = Signer::from_bytes([2; 32]).unwrap().address();
        let vectors: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/ethereum-sdk-vectors.json"
        ))
        .unwrap();
        let (_, output) = simulation(
            &h,
            &json!({"from":hex(who.as_bytes()),"to":ADDRESS,"data":vectors["data"]}),
            Simulation::Call,
            &mut None,
        )
        .unwrap();
        let expected = arkiv::derive_entity_key(9300, who.as_bytes(), 0, 9);
        assert_eq!(
            output,
            format!("0x{}{}{}", word(32), word(1), &hex(&expected)[2..])
        );
        assert_eq!(entity_nonce(&h, h.head.height, who.as_bytes()).unwrap(), 0);
        let data = ethereum::executeCall {
            ops: vec![ethereum::Operation {
                operation: 5,
                operationData: ethereum::Delete {
                    entityKey: alloy_primitives::B256::from([0x11; 32]),
                }
                .abi_encode()
                .into(),
            }],
        }
        .abi_encode();
        let rt =
            super::super::runtime::Runtime::new(Node::Full(Box::new(h)), None, None, None).unwrap();
        let empty = Signer::from_bytes([3; 32]).unwrap().address();
        let result=handle_request(&rt,json!({"jsonrpc":"2.0","id":1,"method":"eth_estimateGas","params":[{"from":hex(empty.as_bytes()),"to":ADDRESS,"data":hex(&data)}]})).await;
        assert_eq!(result["error"]["code"], 3);
        assert_eq!(
            result["error"]["data"],
            format!(
                "{}{}",
                hex(&keccak256(b"EntityNotFound(bytes32)")[..4]),
                "11".repeat(32)
            )
        );
    }
    #[test]
    fn fee_history_rejects_allocation_amplification_before_building_rewards() {
        let mut h = host();
        let mut state = RpcState::default();
        assert_eq!(
            dispatch(
                &mut h,
                &mut state,
                "eth_feeHistory",
                &json!(["0x400", "latest", vec![0; 101]])
            )
            .unwrap_err()
            .0,
            "InvalidParams"
        );
        for values in [json!([2, 1]), json!([-1]), json!([101]), json!(["0"])] {
            assert_eq!(
                dispatch(
                    &mut h,
                    &mut state,
                    "eth_feeHistory",
                    &json!(["0x1", "latest", values])
                )
                .unwrap_err()
                .0,
                "InvalidParams"
            );
        }
    }
}
