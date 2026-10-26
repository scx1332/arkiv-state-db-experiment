//! Strict Ethereum authentication and Arkiv execute ABI translation.
//! The original signed bytes are retained and rechecked by every replaying node.
use super::{
    Error, Result,
    crypto::recover,
    protocol::{SignedGenesis, SignedTx, TX_CAP},
};
use crate::{
    Expiry, TxEnvelope, TxOp,
    arkiv::{self, ArkivOperation},
};
use alloy_primitives::{U256, keccak256};
use alloy_rlp::{Encodable, Header};
use alloy_sol_types::{SolCall, SolValue, abi::AbiDecoderConfig, sol};
use arkiv_db::{B256, OwnerId, RecordKey};

sol! {
    struct Attribute { bytes32 name; uint8 typeId; bytes value; }
    struct Operation { uint8 operation; bytes operationData; }
    function execute(Operation[] ops) external returns (bytes32[] keys);
    struct Create { uint128 salt; uint64 expiresAt; uint64 minLifetime; uint8 creationFlags; Attribute[] attributes; }
    struct Patch { bytes32 entityKey; Attribute[] mutations; }
    struct Extend { bytes32 entityKey; uint64 expiresAt; uint64 minLifetime; }
    struct Transfer { bytes32 entityKey; address newOwner; }
    struct Delete { bytes32 entityKey; }
}

#[derive(Clone, Debug)]
pub struct EthereumTransaction {
    pub chain_id: u64,
    pub nonce: u64,
    pub gas_limit: u64,
    pub gas_price: U256,
    pub max_fee_per_gas: U256,
    pub max_priority_fee_per_gas: U256,
    pub to: [u8; 20],
    pub value: U256,
    pub input: Vec<u8>,
    pub actor: OwnerId,
    pub signature: [u8; 65],
    pub hash: B256,
    pub tx_type: u8,
}
fn bad() -> Error {
    Error("InvalidEthereumTransaction")
}
struct Item<'a> {
    raw: &'a [u8],
    payload: &'a [u8],
    list: bool,
}
fn take<'a>(input: &mut &'a [u8]) -> Result<Item<'a>> {
    let original = *input;
    let h = Header::decode(input).map_err(|_| bad())?;
    let payload = &input[..h.payload_length];
    *input = &input[h.payload_length..];
    Ok(Item {
        raw: &original[..original.len() - input.len()],
        payload,
        list: h.list,
    })
}
fn uint(item: &Item<'_>) -> Result<U256> {
    if item.list || item.payload.len() > 32 || item.payload.first() == Some(&0) {
        return Err(bad());
    }
    Ok(U256::from_be_slice(item.payload))
}
fn u64_value(item: &Item<'_>) -> Result<u64> {
    uint(item)?.try_into().map_err(|_| bad())
}
fn list_bytes(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    Header {
        list: true,
        payload_length: payload.len(),
    }
    .encode(&mut out);
    out.extend_from_slice(payload);
    out
}
/// Accept protected legacy and EIP-1559 transactions only. Nonempty access lists
/// are rejected because this engine has no EVM warm/cold storage semantics.
pub fn parse(raw: &[u8]) -> Result<EthereumTransaction> {
    if raw.is_empty() || raw.len() > TX_CAP as usize {
        return Err(bad());
    }
    let typed = raw[0] == 2;
    if raw[0] < 0xc0 && !typed {
        return Err(Error("UnsupportedEthereumTransactionType"));
    }
    let mut cursor = if typed { &raw[1..] } else { raw };
    let root = take(&mut cursor)?;
    if !cursor.is_empty() || !root.list {
        return Err(bad());
    }
    let mut fields = Vec::new();
    let mut cursor = root.payload;
    while !cursor.is_empty() {
        fields.push(take(&mut cursor)?);
        if fields.len() > 12 {
            return Err(bad());
        }
    }
    if fields.len() != if typed { 12 } else { 9 } {
        return Err(bad());
    }
    let (chain_id, nonce, max_fee, priority, gas_limit, to_idx, sig_idx, parity, signing) = if typed
    {
        let chain_id = u64_value(&fields[0])?;
        let nonce = u64_value(&fields[1])?;
        let priority = uint(&fields[2])?;
        let fee = uint(&fields[3])?;
        if priority > fee || !fields[8].list || !fields[8].payload.is_empty() {
            return Err(bad());
        }
        let mut payload = Vec::new();
        for f in &fields[..9] {
            payload.extend_from_slice(f.raw);
        }
        let mut signing = vec![2];
        signing.extend(list_bytes(&payload));
        (
            chain_id,
            nonce,
            fee,
            priority,
            u64_value(&fields[4])?,
            5,
            10,
            u64_value(&fields[9])?,
            signing,
        )
    } else {
        let v = u64_value(&fields[6])?;
        if v < 35 {
            return Err(Error("UnprotectedEthereumTransaction"));
        }
        let chain_id = (v - 35) / 2;
        let parity = (v - 35) % 2;
        let mut payload = Vec::new();
        for f in &fields[..6] {
            payload.extend_from_slice(f.raw);
        }
        chain_id.encode(&mut payload);
        0u8.encode(&mut payload);
        0u8.encode(&mut payload);
        (
            chain_id,
            u64_value(&fields[0])?,
            uint(&fields[1])?,
            U256::ZERO,
            u64_value(&fields[2])?,
            3,
            7,
            parity,
            list_bytes(&payload),
        )
    };
    if parity > 1 || max_fee < U256::from(1_000_000_000u64) {
        return Err(Error("GasPriceBelowMinimum"));
    }
    let to = &fields[to_idx];
    if to.list || to.payload.len() != 20 {
        return Err(Error("UnsupportedContractCreation"));
    }
    if fields[to_idx + 2].list {
        return Err(bad());
    }
    let mut signature = [0u8; 65];
    signature[..32].copy_from_slice(&uint(&fields[sig_idx])?.to_be_bytes::<32>());
    signature[32..64].copy_from_slice(&uint(&fields[sig_idx + 1])?.to_be_bytes::<32>());
    signature[64] = parity as u8;
    let actor = recover(keccak256(&signing), &signature)?;
    Ok(EthereumTransaction {
        chain_id,
        nonce,
        gas_limit,
        gas_price: U256::from(1_000_000_000u64),
        max_fee_per_gas: max_fee,
        max_priority_fee_per_gas: priority,
        to: to.payload.try_into().map_err(|_| bad())?,
        value: uint(&fields[to_idx + 1])?,
        input: fields[to_idx + 2].payload.to_vec(),
        actor,
        signature,
        hash: keccak256(raw),
        tx_type: if typed { 2 } else { 0 },
    })
}
/// The ABI triples as cells: bit 7 of `typeId` selects an indexed attribute
/// or an unindexed field; `None` is a patch tombstone.
fn triples(
    attrs: Vec<Attribute>,
    patch: bool,
) -> Result<Vec<(arkiv_db::CellName, Option<arkiv_db::Cell>)>> {
    let attrs = attrs
        .into_iter()
        .map(|a| arkiv::AbiAttribute {
            name: a.name.0,
            type_id: a.typeId,
            value: a.value.to_vec(),
        })
        .collect::<Vec<_>>();
    arkiv::decode_protocol_triples(&attrs, patch).map_err(|_| Error("InvalidArkivAttribute"))
}
fn key(bytes: alloy_primitives::B256) -> Result<RecordKey> {
    RecordKey::new(bytes.as_slice()).ok_or_else(bad)
}
fn abi_config() -> AbiDecoderConfig {
    // A canonical 256 KiB input needs far less than this; aliased dynamic
    // offsets must not multiply a small hostile payload into large allocations.
    AbiDecoderConfig::new()
        .strict(true)
        .memory_limit(2 * 1024 * 1024)
}
pub fn operations(input: &[u8]) -> Result<Vec<ArkivOperation>> {
    if input.len() > TX_CAP as usize {
        return Err(bad());
    }
    let call = executeCall::abi_decode_with_config(input, abi_config())
        .map_err(|_| Error("InvalidArkivCalldata"))?;
    // Reject offset aliases, trailing data and noncanonical padding.
    if call.abi_encode() != input || call.ops.is_empty() || call.ops.len() > 128 {
        return Err(Error("InvalidArkivCalldata"));
    }
    call.ops
        .into_iter()
        .map(|op| {
            let bytes = op.operationData.as_ref();
            macro_rules! decode {
                ($ty:ty) => {{
                    let v = <$ty>::abi_decode_with_config(bytes, abi_config())
                        .map_err(|_| Error("InvalidArkivCalldata"))?;
                    if v.abi_encode() != bytes {
                        return Err(Error("InvalidArkivCalldata"));
                    }
                    v
                }};
            }
            let value = match op.operation {
                1 => {
                    let v = decode!(Create);
                    ArkivOperation::Create {
                        salt: v.salt,
                        flags: v.creationFlags,
                        expiry: Expiry {
                            expires_at: v.expiresAt,
                            min_lifetime: v.minLifetime,
                        },
                        attributes: triples(v.attributes, false)?
                            .into_iter()
                            .map(|(n, c)| (n, c.expect("create has no tombstones")))
                            .collect(),
                    }
                }
                2 => {
                    let v = decode!(Patch);
                    let mut set = Vec::new();
                    let mut remove = Vec::new();
                    for (n, c) in triples(v.mutations, true)? {
                        if let Some(c) = c {
                            set.push((n, c))
                        } else {
                            remove.push(n)
                        }
                    }
                    ArkivOperation::Patch {
                        key: key(v.entityKey)?,
                        set,
                        remove,
                    }
                }
                3 => {
                    let v = decode!(Extend);
                    ArkivOperation::ExtendExpiry {
                        key: key(v.entityKey)?,
                        expiry: Expiry {
                            expires_at: v.expiresAt,
                            min_lifetime: v.minLifetime,
                        },
                    }
                }
                4 => {
                    let v = decode!(Transfer);
                    ArkivOperation::TransferOwnership {
                        key: key(v.entityKey)?,
                        owner: v.newOwner.into_array(),
                    }
                }
                5 => {
                    let v = decode!(Delete);
                    ArkivOperation::Delete {
                        key: key(v.entityKey)?,
                    }
                }
                _ => return Err(Error("UnsupportedArkivOperation")),
            };
            value
                .validate()
                .map_err(|_| Error("InvalidArkivOperation"))?;
            Ok(value)
        })
        .collect()
}
pub fn call_ops(_genesis: &SignedGenesis, calldata: &[u8]) -> Result<Vec<TxOp>> {
    Ok(operations(calldata)?
        .into_iter()
        .map(|operation| TxOp::Arkiv {
            namespace: arkiv::entity_namespace(),
            operation,
        })
        .collect())
}
pub fn make_envelope(genesis: &SignedGenesis, tx: &EthereumTransaction) -> Result<TxEnvelope> {
    if tx.chain_id != genesis.genesis.chain_id {
        return Err(Error("IdentityMismatch"));
    }
    let ops = if tx.to == arkiv::ARKIV_ADDRESS {
        if !tx.value.is_zero() {
            return Err(Error("NonzeroArkivValue"));
        }
        call_ops(genesis, &tx.input)?
    } else {
        if !tx.input.is_empty() {
            return Err(Error("UnsupportedEthereumCall"));
        }
        vec![TxOp::Transfer {
            to: OwnerId::new(tx.to),
            amount: tx.value,
        }]
    };
    Ok(TxEnvelope {
        chain_id: tx.chain_id,
        actor: tx.actor,
        request_id: tx.hash.0,
        tx_budget: tx.gas_limit,
        ops,
    })
}
pub fn decode(genesis: &SignedGenesis, raw: &[u8]) -> Result<SignedTx> {
    let parsed = parse(raw)?;
    let tx = SignedTx {
        genesis_hash: genesis.hash(),
        nonce: parsed.nonce,
        tx: make_envelope(genesis, &parsed)?,
        signature: parsed.signature,
        ethereum_raw: Some(raw.to_vec()),
    };
    tx.validate(genesis)?;
    Ok(tx)
}
