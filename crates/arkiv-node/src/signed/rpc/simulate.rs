//! `eth_call` and `eth_estimateGas`: one transaction executed on a discarded
//! branch over the head, and the revert data of a refused one.

use super::super::{Error, ErrorCode, Result, ethereum, host::Host};
use super::{
    entities::{entity, entity_nonce},
    logs::topic_address,
    params::{actor, at, bytes, first, quantity, uint, word},
};
use crate::{
    ExecError, Expiry, HostFault, OpOutcome, TxEnvelope, TxOp, TxOutcome, TxStatus,
    accounts::AccountFault,
    arkiv::{self, ArkivFault, ArkivOperation},
    exec::{BlockEnv, Mode, estimate_transaction, execute_block_hooked},
    text::hex,
};
use alloy_primitives::{U256, keccak256};
use arkiv_db::{Cost, OwnerId, RecordKey};
use serde_json::{Value, json};

pub(super) fn estimate_gas(host: &Host, args: &[Value]) -> Result<Value> {
    if args
        .get(1)
        .is_some_and(|v| !matches!(v.as_str(), Some("latest" | "pending")))
    {
        return Err(ErrorCode::UnsupportedHistoricalSimulation.into());
    }
    let call = first(args)?;
    let (gas, _) = simulation(host, call, Simulation::Estimate)?;
    // A plain transfer has a fixed cost and needs no estimate margin.
    let to = actor(call.get("to").ok_or(ErrorCode::InvalidParams)?)?;
    if to.as_bytes() != &arkiv::ARKIV_ADDRESS {
        return Ok(json!(quantity(gas)));
    }
    Ok(json!(quantity(
        gas.saturating_add(gas / 10)
            .saturating_add(1000)
            .min(host.genesis.genesis.params.user_budget)
    )))
}

pub(super) fn call(host: &Host, args: &[Value]) -> Result<Value> {
    let call = first(args)?;
    let to = actor(call.get("to").ok_or(ErrorCode::InvalidParams)?)?;
    let data = bytes(
        call.get("data")
            .or_else(|| call.get("input"))
            .ok_or(ErrorCode::InvalidParams)?,
        None,
    )?;
    if to.as_bytes() != &arkiv::ARKIV_ADDRESS {
        return Err(ErrorCode::UnsupportedContractCall.into());
    }
    if data.len() == 36 && data[..4] == keccak256(b"entityNonce(address)")[..4] {
        if data[4..16].iter().any(|b| *b != 0) {
            return Err(ErrorCode::InvalidParams.into());
        }
        let address: [u8; 20] = data[16..]
            .try_into()
            .map_err(|_| ErrorCode::InvalidParams)?;
        let nonce = entity_nonce(host, at(host, args.get(1))?, &address)?;
        return Ok(json!(format!("0x{}", word(nonce))));
    }
    if at(host, args.get(1))? != host.head.height {
        return Err(ErrorCode::UnsupportedHistoricalSimulation.into());
    }
    simulation(host, call, Simulation::Call).map(|(_, result)| result)
}

#[derive(Clone, Copy)]
pub(super) enum Simulation {
    Call,
    Estimate,
}

/// The gas a call object's transaction spends on the next block and its ABI
/// return value: the entity keys of an Arkiv call, nothing for a transfer.
pub(super) fn simulation(host: &Host, call: &Value, mode: Simulation) -> Result<(u64, Value)> {
    let (from, to, ops) = requested_ops(call)?;
    let spec = host.genesis.spec()?;
    let gas = call
        .get("gas")
        .map(uint)
        .transpose()?
        .unwrap_or(spec.params().user_budget);
    let keys = entity_keys(host, &from, &ops)?;
    let tx = TxEnvelope {
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
    let height = host
        .head
        .height
        .checked_add(1)
        .ok_or(ErrorCode::CapacityReached)?;
    let env = BlockEnv {
        spec: &spec,
        parent: &host.head,
        height,
        timestamp_ms: host.genesis.next_timestamp(&host.head, host.now_ms())?,
    };
    let accounts = Some(&host.genesis.accounts);
    let outcome = match mode {
        Simulation::Estimate => estimate_transaction(env, branch, tx.clone(), accounts),
        Simulation::Call => execute_block_hooked(
            env,
            branch,
            &crate::BlockInputs {
                admin: vec![],
                txs: vec![tx.clone()],
            },
            accounts,
            Mode::Strict,
            |_, _, _| Ok(()),
            |_, _, _, _| Ok(()),
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
    .map_err(execution_refusal)?;
    if outcome.status != TxStatus::Committed {
        return Err(failure(host, &tx, &outcome, height)?);
    }
    let result = if to.as_bytes() == &arkiv::ARKIV_ADDRESS {
        let mut encoded = format!("0x{}{}", word(32), word(keys.len() as u64));
        for key in keys {
            encoded.push_str(&hex(&key)[2..]);
        }
        encoded
    } else {
        "0x".into()
    };
    Ok((outcome.spent, json!(result)))
}

/// Sender, recipient and operations of a call object: the decoded calldata
/// of an Arkiv call, or one transfer.
fn requested_ops(call: &Value) -> Result<(OwnerId, OwnerId, Vec<TxOp>)> {
    if !call.is_object() {
        return Err(ErrorCode::InvalidParams.into());
    }
    let from = actor(call.get("from").ok_or(ErrorCode::InvalidParams)?)?;
    let to = actor(call.get("to").ok_or(ErrorCode::InvalidParams)?)?;
    let data = bytes(
        call.get("data")
            .or_else(|| call.get("input"))
            .unwrap_or(&json!("0x")),
        None,
    )?;
    let value = call
        .get("value")
        .map(|v| {
            v.as_str()
                .and_then(crate::text::quantity_u256)
                .ok_or(ErrorCode::InvalidParams)
        })
        .transpose()?
        .unwrap_or(U256::ZERO);
    let ops = if to.as_bytes() == &arkiv::ARKIV_ADDRESS {
        if value != U256::ZERO {
            return Err(ErrorCode::NonpayableCall.into());
        }
        ethereum::call_ops(&data)?
    } else {
        if !data.is_empty() {
            return Err(ErrorCode::UnsupportedContractCall.into());
        }
        vec![TxOp::Transfer { to, amount: value }]
    };
    Ok((from, to, ops))
}

/// The key each Arkiv operation of `ops` addresses, a created entity's
/// derived from the creation nonces `from` would use at the head.
fn entity_keys(host: &Host, from: &OwnerId, ops: &[TxOp]) -> Result<Vec<Vec<u8>>> {
    let mut create_nonce = entity_nonce(host, host.head.height, from.as_bytes())?;
    let mut keys = Vec::new();
    for op in ops {
        let TxOp::Arkiv { operation, .. } = op else {
            continue;
        };
        let key = match operation {
            ArkivOperation::Create { salt, .. } => {
                let key = arkiv::derive_entity_key(
                    host.genesis.genesis.chain_id,
                    from.as_bytes(),
                    create_nonce,
                    *salt,
                );
                create_nonce = create_nonce
                    .checked_add(1)
                    .ok_or(ErrorCode::NonceExhausted)?;
                key.to_vec()
            }
            existing => existing
                .key()
                .ok_or(ErrorCode::InvalidTransaction)?
                .as_bytes()
                .to_vec(),
        };
        keys.push(key);
    }
    Ok(keys)
}

fn execution_refusal(error: ExecError) -> Error {
    match error {
        ExecError::Account(AccountFault::InsufficientBalance) | ExecError::Unaffordable { .. } => {
            ErrorCode::InsufficientBalance.into()
        }
        ExecError::StorageCapExceeded { .. } => ErrorCode::ExceedsStorageCap.into(),
        ExecError::Account(_) => ErrorCode::AccountOverflow.into(),
        ExecError::Storage(e) => e.into(),
        ExecError::InvalidBlock(_) => ErrorCode::InvalidTransaction.into(),
        _ => ErrorCode::SimulationFailed.into(),
    }
}

/// The refusal of a simulated transaction that did not commit.
fn failure(host: &Host, tx: &TxEnvelope, outcome: &TxOutcome, height: u64) -> Result<Error> {
    let unfunded = outcome.ops.iter().any(|op| {
        matches!(
            op,
            OpOutcome::AccountRejected {
                fault: AccountFault::InsufficientBalance,
                ..
            } | OpOutcome::HostRejected {
                fault: HostFault::Arkiv(ArkivFault::InsufficientRentBalance),
            }
        )
    });
    if unfunded {
        return Ok(ErrorCode::InsufficientBalance.into());
    }
    Ok(Error::reverted(revert_data(host, tx, outcome, height)?))
}

/// The expiry height `asked` names at block `height`, for revert data only:
/// execution refuses the overflow this saturates.
fn asked_height(asked: &Expiry, height: u64) -> u64 {
    asked
        .expires_at
        .max(height.saturating_add(asked.min_lifetime))
}

/// The revert data of the first operation the host rejected: the
/// EntityRegistry custom error it corresponds to, if any.
pub(super) fn revert_data(
    host: &Host,
    tx: &TxEnvelope,
    outcome: &TxOutcome,
    height: u64,
) -> Result<Option<String>> {
    let Some((index, fault)) = outcome.ops.iter().enumerate().find_map(|(i, o)| match o {
        OpOutcome::HostRejected { fault } => Some((i, fault)),
        _ => None,
    }) else {
        return Ok(None);
    };
    let Some(TxOp::Arkiv { operation, .. }) = tx.ops.get(index) else {
        return Ok(None);
    };
    if matches!(fault, HostFault::ExpiryCapacityFull) {
        return Ok(operation
            .expiry()
            .map(|asked| capacity_full(host, index, asked_height(asked, height))));
    }
    let Some(key) = operation.key() else {
        return Ok(None);
    };
    let before = EntityBefore::operation(host, tx, index, key, height)?;
    let asked = operation
        .expiry()
        .map(|asked| word(asked_height(asked, height)));
    let cap = crate::gas::RentClock::of(&host.genesis.genesis.params)
        .map_or(0, |clock| clock.cap_blocks());
    let cap_end = word(height.saturating_add(cap));
    let Some((signature, args)) = custom_error(tx, operation, fault, before, asked, cap_end) else {
        return Ok(None);
    };
    Ok(Some(format!(
        "{}{}{args}",
        hex(&keccak256(signature.as_bytes())[..4]),
        &hex(key.as_bytes())[2..],
    )))
}

/// Not an EntityRegistry error: a plain Solidity `Error(string)`, which
/// every Ethereum client decodes into a readable revert reason.
fn capacity_full(host: &Host, index: usize, target: u64) -> String {
    error_string(&format!(
        "Arkiv: operation {index} cannot expire at block {target}: blocks {target} to {} each \
         already have the {} expiring entities one block may have; choose another expiry",
        target
            .saturating_add(crate::exec::EXPIRY_SPILL_WINDOW)
            .min(u64::MAX - 1),
        host.genesis.genesis.params.expiry_per_block
    ))
}

/// Revert data of a Solidity `revert(reason)`: `Error(string)`.
fn error_string(reason: &str) -> String {
    let mut text = reason.as_bytes().to_vec();
    let len = text.len() as u64;
    text.resize(text.len().div_ceil(32) * 32, 0);
    format!(
        "{}{}{}{}",
        hex(&keccak256(b"Error(string)")[..4]),
        word(32),
        word(len),
        &hex(&text)[2..]
    )
}

/// Owner and expiry of an entity as the rejected operation found it; `None`
/// where the entity did not exist.
struct EntityBefore {
    owner: Option<[u8; 20]>,
    expiry: Option<u64>,
}

impl EntityBefore {
    /// The entity `key` at the head, then as the operations of `tx` before
    /// `index` left it.
    fn operation(
        host: &Host,
        tx: &TxEnvelope,
        index: usize,
        key: &RecordKey,
        height: u64,
    ) -> Result<Self> {
        let current = entity(&host.state(host.head.height)?, key)?;
        let mut before = Self {
            owner: current.as_ref().map(|entity| entity.owner),
            expiry: current.as_ref().map(|entity| entity.expires_at),
        };
        let creator = tx.actor.as_bytes();
        let mut nonce = entity_nonce(host, host.head.height, creator)?;
        for prior in &tx.ops[..index] {
            let TxOp::Arkiv { operation, .. } = prior else {
                continue;
            };
            match operation {
                ArkivOperation::Create { salt, expiry, .. } => {
                    let created = arkiv::derive_entity_key(tx.chain_id, creator, nonce, *salt);
                    nonce = nonce.checked_add(1).ok_or(ErrorCode::NonceExhausted)?;
                    if created == key.as_bytes() {
                        before.owner = Some(*creator);
                        before.expiry = Some(asked_height(expiry, height));
                    }
                }
                other if other.key() != Some(key) => (),
                ArkivOperation::TransferOwnership { owner, .. } => before.owner = Some(*owner),
                ArkivOperation::ExtendExpiry { expiry, .. } => {
                    before.expiry = Some(asked_height(expiry, height));
                }
                ArkivOperation::Delete { .. } => {
                    before.owner = None;
                    before.expiry = None;
                }
                ArkivOperation::Patch { .. } => (),
            }
        }
        Ok(before)
    }
}

/// The signature of the EntityRegistry error for a rejection with `fault`
/// and its ABI arguments after the entity key. `asked` is the word of the
/// expiry height the operation asked for, `cap_end` of the last height a
/// non-owner may extend to.
fn custom_error(
    tx: &TxEnvelope,
    operation: &ArkivOperation,
    fault: &HostFault,
    before: EntityBefore,
    asked: Option<String>,
    cap_end: String,
) -> Option<(&'static str, String)> {
    Some(match fault {
        HostFault::Arkiv(ArkivFault::NotFound) => ("EntityNotFound(bytes32)", String::new()),
        HostFault::Arkiv(ArkivFault::Expired) => {
            ("EntityExpired(bytes32,uint64)", word(before.expiry?))
        }
        HostFault::Arkiv(ArkivFault::Readonly) => ("ReadOnlyEntity(bytes32)", String::new()),
        HostFault::Arkiv(ArkivFault::PermanenceOwnerOnly) => (
            "PermanenceOwnerOnly(bytes32,uint64,uint64)",
            format!("{}{cap_end}", asked?),
        ),
        HostFault::Arkiv(ArkivFault::NotOwner) => (
            "NotOwner(bytes32,address,address)",
            format!(
                "{}{}",
                &topic_address(tx.actor.as_bytes())[2..],
                &topic_address(&before.owner?)[2..]
            ),
        ),
        HostFault::Arkiv(ArkivFault::Owner) => {
            let ArkivOperation::TransferOwnership { owner, .. } = operation else {
                return None;
            };
            let signature = if *owner == [0; 20] {
                "TransferToZeroAddress(bytes32)"
            } else {
                "TransferToSelf(bytes32)"
            };
            (signature, String::new())
        }
        HostFault::ExpiryShortened => (
            "ExpiryNotExtended(bytes32,uint64,uint64)",
            format!("{}{}", asked?, word(before.expiry?)),
        ),
        _ => return None,
    })
}
