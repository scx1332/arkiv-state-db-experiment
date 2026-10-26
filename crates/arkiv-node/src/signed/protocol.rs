//! Canonical native signed envelopes. Every layout starts with a version
//! field, and decoders accept only the versions defined here.
use std::collections::BTreeSet;

use super::{
    Error, Result,
    crypto::{Signer, recover, validate_signature},
};
use crate::{
    BlockInputs, BlockOutcomes, ChainSpec, Genesis, Header, RequestIdentity, Scope, TxEnvelope,
    TxOp, validate,
};
use arkiv_db::{
    B256, Decode, DecodeError, Decoder, Encode, Encoder, OwnerId, decode_exact, digest, encode,
};

pub const VERSION: u32 = 1;
/// Layout version of a [`SignedTx`] that carries its original Ethereum
/// transaction.
pub const ETHEREUM_TX_VERSION: u32 = 2;
pub const SIGNED_TX_OVERHEAD: u32 = 4 + 32 + 8 + 65;
/// The most bytes of one signed transaction. A transaction submitted through
/// Ethereum JSON-RPC carries its original bytes beside the decoded envelope,
/// so its payload counts twice: 384 KiB fits an Ethereum create of a full
/// 128 KiB entity (about 264 KB signed) next to the [`NATIVE_TX_CAP`]
/// envelope cap.
pub const TX_CAP: u32 = 384 * 1024;
/// The most bytes of the native envelope of a signed transaction, its
/// overhead included: the default `ChainParams::max_tx_bytes` is this less
/// [`SIGNED_TX_OVERHEAD`].
pub const NATIVE_TX_CAP: u32 = 256 * 1024;
pub const INPUT_CAP: u32 = 768 * 1024;
pub const BODY_CAP: u32 = 1024 * 1024;
/// Native outcomes contain only bounded scalar effects/rejections/receipts.
/// Reserve well over their maximum at 128 operations and 16 expiry deletions,
/// in addition to the certificate and body framing, even for tightened profiles.
pub const OUTCOME_RESERVE: u32 = 64 * 1024;
pub const MAX_TXS: u32 = 16;
pub const GENESIS_DOMAIN: &str = "arkiv/signed-genesis/v1";
pub const TX_INTENT_DOMAIN: &str = "arkiv/signed-tx-intent/v1";
pub const TX_DOMAIN: &str = "arkiv/signed-tx/v1";
pub const INPUTS_DOMAIN: &str = "arkiv/signed-inputs/v1";
pub const HEADER_DOMAIN: &str = "arkiv/signed-header/v1";

fn version(decoder: &mut Decoder<'_>, kind: &'static str) -> std::result::Result<(), DecodeError> {
    let found = decoder.u32()?;
    if found != VERSION {
        return Err(DecodeError::UnsupportedVersion {
            kind,
            version: found,
        });
    }
    Ok(())
}

fn invalid(kind: &'static str, reason: &'static str) -> DecodeError {
    DecodeError::Invalid { kind, reason }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SignedLimits {
    pub max_tx_bytes: u32,
    pub max_input_bytes: u32,
    pub max_body_bytes: u32,
}
impl Default for SignedLimits {
    fn default() -> Self {
        Self {
            max_tx_bytes: TX_CAP,
            max_input_bytes: INPUT_CAP,
            max_body_bytes: BODY_CAP,
        }
    }
}
impl Encode for SignedLimits {
    fn encode_to(&self, e: &mut Encoder) {
        e.u32(self.max_tx_bytes);
        e.u32(self.max_input_bytes);
        e.u32(self.max_body_bytes);
    }
}
impl Decode for SignedLimits {
    const MIN_ENCODED_LEN: usize = 12;
    fn decode_from(d: &mut Decoder<'_>) -> std::result::Result<Self, DecodeError> {
        Ok(Self {
            max_tx_bytes: d.u32()?,
            max_input_bytes: d.u32()?,
            max_body_bytes: d.u32()?,
        })
    }
}

/// The signed chain's identity: the native genesis, the single proposer,
/// the block period, the signed wire limits, the GLM account allocations and
/// an optional height-zero entity prefill. Signed chains always run the
/// `glm-v2` gas profile ([`GAS_PROFILE`]): GLM pricing with funded accounts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedGenesis {
    pub genesis: Genesis,
    pub run_id: [u8; 16],
    pub proposer: [u8; 20],
    pub block_period_ms: u64,
    pub limits: SignedLimits,
    pub accounts: crate::accounts::AccountConfig,
    pub prefill: Option<crate::genesis_bundle::PrefillLaunch>,
}
/// The gas profile of every signed chain: GLM pricing, balances and burnt fees.
pub const GAS_PROFILE: &str = "glm-v2";

/// The fewest and most engine cells per record a signed chain admits. The
/// floor fits [`crate::arkiv::MAX_ATTRIBUTES`] user attributes beside the
/// [`crate::arkiv::SYSTEM_CELLS`] system cells, so system cells never reduce
/// the user attribute budget; the ceiling is the proof verifier's row cap.
pub const MIN_CELLS: usize = crate::arkiv::MIN_ENTITY_CELLS;
pub const MAX_CELLS: usize = 64;

/// The native genesis of a signed chain created without a genesis bundle:
/// the development workload limits under GLM pricing, without admins, with
/// the signed wire caps. Its [`MIN_CELLS`] cells per record fit a full entity.
/// Its block period is `block_period_ms`, which the [`SignedGenesis`] must
/// repeat, and its rent cap a year at any period
/// ([`crate::gas::DEFAULT_RENT_CAP_SECONDS`]).
pub fn default_native_genesis(chain_id: u64, timestamp_ms: u64, block_period_ms: u64) -> Genesis {
    let mut native = crate::demo::workload::genesis(chain_id, timestamp_ms);
    native.params.block_period_ms = block_period_ms;
    native.params.max_rent_lifetime_seconds = crate::gas::DEFAULT_RENT_CAP_SECONDS;
    native.admins.clear();
    native.pricing = crate::gas::schedule();
    native.params.max_admin_per_block = 0;
    native.params.max_tx_bytes = NATIVE_TX_CAP - SIGNED_TX_OVERHEAD;
    native.params.max_block_input_bytes = INPUT_CAP;
    native.params.block_budget = crate::gas::DEFAULT_BLOCK_GAS_LIMIT;
    native.params.user_budget = 90_000_000;
    native.params.max_lifetime = 6_307_200;
    native
}
impl SignedGenesis {
    pub fn new(
        genesis: Genesis,
        run_id: [u8; 16],
        proposer: [u8; 20],
        block_period_ms: u64,
        accounts: crate::accounts::AccountConfig,
    ) -> Result<Self> {
        let value = Self {
            genesis,
            run_id,
            proposer,
            block_period_ms,
            limits: SignedLimits::default(),
            accounts,
            prefill: None,
        };
        value.validate()?;
        Ok(value)
    }
    pub fn validate(&self) -> Result<()> {
        self.accounts
            .validate()
            .map_err(|_| Error("InvalidAccountGenesis"))?;
        if !crate::gas::is_active(&self.genesis.pricing) {
            return Err(Error("InvalidGasSchedule"));
        }
        // Without a prefill the proposer creates the entity namespace in
        // block 1 and must be able to pay for it.
        if self.prefill.is_none()
            && self.accounts.initial_balance(OwnerId::new(self.proposer))
                < crate::accounts::fee(self.genesis.params.user_budget)
        {
            return Err(Error("UnfundedBootstrap"));
        }
        let p = self.genesis.params;
        let l = self.limits;
        if !self.genesis.admins.is_empty()
            || p.max_admin_per_block != 0
            || self.block_period_ms == 0
            // Rent is priced at the period the chain's timestamps advance by.
            || p.block_period_ms != self.block_period_ms
            || p.max_txs_per_block > MAX_TXS
            || p.max_ops_per_tx > 16
            || p.max_ops_per_block > 128
            || p.expiry_per_block > 16
            || self.genesis.limits.namespace_cap > 1024
            || !(MIN_CELLS..=MAX_CELLS).contains(&self.genesis.limits.max_cells)
            || self.genesis.limits.max_payload_len() > 128 * 1024
            || self.genesis.limits.max_page > 64
            || self.genesis.limits.max_groups > 8
            || self.genesis.limits.max_literals > 8
            || l.max_tx_bytes > TX_CAP
            || l.max_tx_bytes <= SIGNED_TX_OVERHEAD
            || l.max_input_bytes > INPUT_CAP
            || l.max_body_bytes > BODY_CAP
            || l.max_tx_bytes
                .checked_add(8)
                .is_none_or(|n| n > l.max_input_bytes)
            || l.max_input_bytes
                .checked_add(OUTCOME_RESERVE)
                .is_none_or(|n| n > l.max_body_bytes)
            || p.max_tx_bytes
                .checked_add(SIGNED_TX_OVERHEAD)
                .is_none_or(|n| n > l.max_tx_bytes)
            || p.max_block_input_bytes > l.max_input_bytes
        {
            return Err(Error("InvalidSignedGenesis"));
        }
        if let Some(prefill) = &self.prefill {
            prefill
                .validate(self)
                .map_err(|_| Error("InvalidPrefillGenesis"))?;
        }
        ChainSpec::new(self.genesis.clone()).map_err(|_| Error("InvalidSignedGenesis"))?;
        self.timestamp(1)?;
        Ok(())
    }
    /// The native execution parameters; signed chain identity comes from `self.hash()`.
    pub fn spec(&self) -> Result<ChainSpec> {
        self.validate()?;
        ChainSpec::new(self.genesis.clone()).map_err(|_| Error("InvalidSignedGenesis"))
    }
    pub fn header(&self) -> Header {
        Header {
            inputs_digest: digest(GENESIS_DOMAIN, &encode(self)),
            state_root: self
                .prefill
                .as_ref()
                .map_or(self.genesis.header().state_root, |p| p.state.root()),
            ..self.genesis.header()
        }
    }
    pub fn hash(&self) -> B256 {
        self.header().hash()
    }
    pub fn timestamp(&self, height: u64) -> Result<u64> {
        height
            .checked_mul(self.block_period_ms)
            .and_then(|n| self.genesis.timestamp_ms.checked_add(n))
            .ok_or(Error("TimestampOverflow"))
    }
}
impl Encode for SignedGenesis {
    fn encode_to(&self, e: &mut Encoder) {
        e.u32(VERSION);
        e.put(&self.genesis);
        e.fixed(&self.run_id);
        e.fixed(&self.proposer);
        e.u64(self.block_period_ms);
        e.put(&self.limits);
        e.put(&self.accounts);
        e.put(&self.prefill);
    }
}
impl Decode for SignedGenesis {
    const MIN_ENCODED_LEN: usize = 4
        + Genesis::MIN_ENCODED_LEN
        + 16
        + 20
        + 8
        + 12
        + crate::accounts::AccountConfig::MIN_ENCODED_LEN
        + 1;
    fn decode_from(d: &mut Decoder<'_>) -> std::result::Result<Self, DecodeError> {
        version(d, "SignedGenesis")?;
        let value = Self {
            genesis: d.get()?,
            run_id: d.array()?,
            proposer: d.array()?,
            block_period_ms: d.u64()?,
            limits: d.get()?,
            accounts: d.get()?,
            prefill: d.get()?,
        };
        value
            .validate()
            .map_err(|e| invalid("SignedGenesis", e.0))?;
        Ok(value)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedTx {
    pub genesis_hash: B256,
    pub nonce: u64,
    pub tx: TxEnvelope,
    pub signature: [u8; 65],
    /// The original Ethereum transaction when the transaction was submitted
    /// through Ethereum JSON-RPC: encoded as [`ETHEREUM_TX_VERSION`] and
    /// authenticated by that transaction's signature instead of the native
    /// intent signature.
    pub ethereum_raw: Option<Vec<u8>>,
}
impl SignedTx {
    fn intent(&self, e: &mut Encoder) {
        e.u32(VERSION);
        e.put(&self.genesis_hash);
        e.u64(self.nonce);
        e.put(&self.tx);
    }
    pub fn intent_digest(&self) -> B256 {
        let mut e = Encoder::new();
        self.intent(&mut e);
        digest(TX_INTENT_DOMAIN, e.as_bytes())
    }
    pub fn hash(&self) -> B256 {
        self.ethereum_raw.as_ref().map_or_else(
            || digest(TX_DOMAIN, &encode(self)),
            alloy_primitives::keccak256,
        )
    }
    pub fn identity(&self) -> RequestIdentity {
        RequestIdentity {
            scope: Scope::Tx,
            chain_id: self.tx.chain_id,
            actor: self.tx.actor,
            request_id: self.tx.request_id,
        }
    }
    pub fn sign(
        genesis: &SignedGenesis,
        nonce: u64,
        tx: TxEnvelope,
        signer: &Signer,
    ) -> Result<Self> {
        let mut value = Self {
            genesis_hash: genesis.hash(),
            nonce,
            tx,
            signature: [0; 65],
            ethereum_raw: None,
        };
        value.signature = signer.sign_digest(value.intent_digest())?;
        value.validate(genesis)?;
        Ok(value)
    }
    pub fn validate(&self, genesis: &SignedGenesis) -> Result<()> {
        let spec = genesis.spec()?;
        if self.genesis_hash != genesis.hash() || self.tx.chain_id != genesis.genesis.chain_id {
            return Err(Error("IdentityMismatch"));
        }
        if self.tx.tx_budget < crate::gas::TX_INTRINSIC_GAS {
            return Err(Error("GasLimitBelowIntrinsic"));
        }
        if self.tx.ops.iter().any(|op| {
            !matches!(
                op,
                TxOp::Arkiv { .. } | TxOp::Transfer { .. } | TxOp::CreateNamespace { .. }
            )
        }) {
            return Err(Error("UnsupportedGasOperation"));
        }
        if self.nonce == u64::MAX {
            return Err(Error("NonceExhausted"));
        }
        if encode(self).len() > genesis.limits.max_tx_bytes as usize {
            return Err(Error("TransactionTooLarge"));
        }
        if let Some(raw) = &self.ethereum_raw {
            let original = super::ethereum::parse(raw)?;
            if original.nonce != self.nonce
                || original.signature != self.signature
                || super::ethereum::make_envelope(genesis, &original)? != self.tx
            {
                return Err(Error("InvalidEthereumEnvelope"));
            }
        } else if recover(self.intent_digest(), &self.signature)? != self.tx.actor {
            return Err(Error("InvalidSignature"));
        }
        validate::check_tx(&spec, &self.tx).map_err(|_| Error("InvalidTransaction"))?;
        // Admission requires fitting an otherwise empty block, including wrappers.
        if self.tx.ops.len() > spec.params().max_ops_per_block as usize
            || encode(self).len() + 8 > genesis.limits.max_input_bytes as usize
            || encode(&self.tx).len() + 8 > spec.params().max_block_input_bytes as usize
        {
            return Err(Error("TransactionTooLarge"));
        }
        Ok(())
    }
    pub fn from_ethereum(genesis: &SignedGenesis, bytes: &[u8]) -> Result<Self> {
        super::ethereum::decode(genesis, bytes)
    }
    /// Bounded exact transport decode, before signature verification or pool insertion.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > TX_CAP as usize {
            return Err(Error("TransactionTooLarge"));
        }
        decode_exact(bytes).map_err(|_| Error("InvalidTransactionEncoding"))
    }
}
impl Encode for SignedTx {
    fn encode_to(&self, e: &mut Encoder) {
        if let Some(raw) = &self.ethereum_raw {
            e.u32(ETHEREUM_TX_VERSION);
            e.put(&self.genesis_hash);
            e.u64(self.nonce);
            e.put(&self.tx);
            e.fixed(&self.signature);
            e.bytes(raw);
        } else {
            self.intent(e);
            e.fixed(&self.signature);
        }
    }
}
impl Decode for SignedTx {
    const MIN_ENCODED_LEN: usize = SIGNED_TX_OVERHEAD as usize + TxEnvelope::MIN_ENCODED_LEN;
    fn decode_from(d: &mut Decoder<'_>) -> std::result::Result<Self, DecodeError> {
        let before = d.remaining();
        let wire = d.u32()?;
        if wire != VERSION && wire != ETHEREUM_TX_VERSION {
            return Err(DecodeError::UnsupportedVersion {
                kind: "SignedTx",
                version: wire,
            });
        }
        let value = Self {
            genesis_hash: d.get()?,
            nonce: d.u64()?,
            tx: d.get()?,
            signature: d.array()?,
            ethereum_raw: if wire == ETHEREUM_TX_VERSION {
                Some(d.bytes()?.to_vec())
            } else {
                None
            },
        };
        if before - d.remaining() > TX_CAP as usize {
            return Err(invalid("SignedTx", "size limit"));
        }
        validate_signature(&value.signature).map_err(|e| invalid("SignedTx", e.0))?;
        Ok(value)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SignedInputs {
    pub txs: Vec<SignedTx>,
}
impl SignedInputs {
    pub fn digest(&self) -> B256 {
        digest(INPUTS_DOMAIN, &encode(self))
    }
    pub fn native(&self) -> BlockInputs {
        BlockInputs {
            admin: vec![],
            txs: self.txs.iter().map(|t| t.tx.clone()).collect(),
        }
    }
    pub fn validate(&self, genesis: &SignedGenesis, height: u64) -> Result<()> {
        let spec = genesis.spec()?;
        if encode(self).len() > genesis.limits.max_input_bytes as usize {
            return Err(Error("InputsTooLarge"));
        }
        if height == 0 {
            return Err(Error("InvalidHeight"));
        }
        if height == 1
            && genesis.prefill.is_none()
            && (self.txs.len() != 1
                || self.txs[0].tx.actor != OwnerId::new(genesis.proposer)
                || self.txs[0].nonce != 0
                || !matches!(self.txs[0].tx.ops.as_slice(), [TxOp::CreateNamespace {name}] if name.as_str() == "entities"))
        {
            return Err(Error("InvalidBootstrap"));
        }
        let mut requests = BTreeSet::new();
        for tx in &self.txs {
            tx.validate(genesis)?;
            if !requests.insert(tx.identity()) {
                return Err(Error("RequestConflict"));
            }
        }
        validate::check_block(&spec, &self.native()).map_err(|_| Error("InvalidInputs"))
    }
}
impl Encode for SignedInputs {
    fn encode_to(&self, e: &mut Encoder) {
        e.u32(VERSION);
        e.put(&self.txs);
    }
}
impl Decode for SignedInputs {
    const MIN_ENCODED_LEN: usize = 8;
    fn decode_from(d: &mut Decoder<'_>) -> std::result::Result<Self, DecodeError> {
        let before = d.remaining();
        version(d, "SignedInputs")?;
        let count = d.u32()?;
        if count > MAX_TXS || count as usize > d.remaining() / SignedTx::MIN_ENCODED_LEN {
            return Err(invalid("SignedInputs", "transaction count"));
        }
        let mut txs = Vec::with_capacity(count as usize);
        for _ in 0..count {
            txs.push(d.get()?);
        }
        if before - d.remaining() > INPUT_CAP as usize {
            return Err(invalid("SignedInputs", "size limit"));
        }
        Ok(Self { txs })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedHeader {
    pub genesis_hash: B256,
    pub header: Header,
    pub signature: [u8; 65],
}
impl SignedHeader {
    pub fn intent_digest(&self) -> B256 {
        let mut e = Encoder::new();
        e.u32(VERSION);
        e.put(&self.genesis_hash);
        e.put(&self.header.hash());
        digest(HEADER_DOMAIN, e.as_bytes())
    }
    pub fn sign(genesis: &SignedGenesis, header: Header, signer: &Signer) -> Result<Self> {
        let mut value = Self {
            genesis_hash: genesis.hash(),
            header,
            signature: [0; 65],
        };
        value.signature = signer.sign_digest(value.intent_digest())?;
        value.authenticate(genesis)?;
        Ok(value)
    }
    pub fn authenticate(&self, genesis: &SignedGenesis) -> Result<()> {
        genesis.validate()?;
        if self.genesis_hash != genesis.hash() || self.header.chain_id != genesis.genesis.chain_id {
            return Err(Error("IdentityMismatch"));
        }
        if self.header.timestamp_ms != genesis.timestamp(self.header.height)?
            || (self.header.height == 0 && self.header != genesis.header())
        {
            return Err(Error("InvalidHeader"));
        }
        if recover(self.intent_digest(), &self.signature)? != OwnerId::new(genesis.proposer) {
            return Err(Error("InvalidSignature"));
        }
        Ok(())
    }
}
impl Encode for SignedHeader {
    fn encode_to(&self, e: &mut Encoder) {
        e.u32(VERSION);
        e.put(&self.genesis_hash);
        e.put(&self.header);
        e.fixed(&self.signature);
    }
}
impl Decode for SignedHeader {
    const MIN_ENCODED_LEN: usize = 4 + 32 + Header::MIN_ENCODED_LEN + 65;
    fn decode_from(d: &mut Decoder<'_>) -> std::result::Result<Self, DecodeError> {
        version(d, "SignedHeader")?;
        let value = Self {
            genesis_hash: d.get()?,
            header: d.get()?,
            signature: d.array()?,
        };
        validate_signature(&value.signature).map_err(|e| invalid("SignedHeader", e.0))?;
        Ok(value)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedBlock {
    pub header: SignedHeader,
    pub inputs: SignedInputs,
    pub outcomes: BlockOutcomes,
}
impl SignedBlock {
    pub fn hash(&self) -> B256 {
        self.header.header.hash()
    }
    /// Authenticate commitments and static structure. Independent execution is still required.
    pub fn authenticate(&self, genesis: &SignedGenesis) -> Result<()> {
        self.header.authenticate(genesis)?;
        if encode(self).len() > genesis.limits.max_body_bytes as usize {
            return Err(Error("BlockTooLarge"));
        }
        if self.header.header.height == 0 {
            if !self.inputs.txs.is_empty() || self.outcomes != BlockOutcomes::default() {
                return Err(Error("InvalidGenesisBlock"));
            }
            return Ok(());
        }
        self.inputs.validate(genesis, self.header.header.height)?;
        if self.header.header.height == 1
            && genesis.prefill.is_none()
            && (self.outcomes.txs.len() != 1
                || self.outcomes.txs[0].status != crate::TxStatus::Committed)
        {
            return Err(Error("InvalidBootstrap"));
        }
        if self.header.header.inputs_digest != self.inputs.digest()
            || self.header.header.outcomes_digest != self.outcomes.digest()
            || self.outcomes.txs.len() != self.inputs.txs.len()
            || !self.outcomes.admin.is_empty()
        {
            return Err(Error("InvalidBlockCommitment"));
        }
        Ok(())
    }
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > BODY_CAP as usize {
            return Err(Error("BlockTooLarge"));
        }
        decode_exact(bytes).map_err(|_| Error("InvalidBlockEncoding"))
    }
}
impl Encode for SignedBlock {
    fn encode_to(&self, e: &mut Encoder) {
        e.u32(VERSION);
        e.put(&self.header);
        e.put(&self.inputs);
        e.put(&self.outcomes);
    }
}
impl Decode for SignedBlock {
    const MIN_ENCODED_LEN: usize = 4
        + SignedHeader::MIN_ENCODED_LEN
        + SignedInputs::MIN_ENCODED_LEN
        + BlockOutcomes::MIN_ENCODED_LEN;
    fn decode_from(d: &mut Decoder<'_>) -> std::result::Result<Self, DecodeError> {
        let before = d.remaining();
        version(d, "SignedBlock")?;
        let value = Self {
            header: d.get()?,
            inputs: d.get()?,
            outcomes: d.get()?,
        };
        if before - d.remaining() > BODY_CAP as usize {
            return Err(invalid("SignedBlock", "size limit"));
        }
        Ok(value)
    }
}
