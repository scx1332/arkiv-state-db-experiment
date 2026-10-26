//! Block format version [`FORMAT_VERSION`]: genesis, headers, block
//! inputs, transaction envelopes and block outcomes, with their canonical
//! encodings and hashes.
//!
//! Every type here is encoded with the engine's canonical codec
//! ([`arkiv_db::codec`]): big-endian fixed-width integers,
//! `u32`-length-prefixed bytes and text, `u32`-counted sequences, `00`/`01`
//! options and one explicit tag byte per enum variant. Tags start at 1 and
//! are listed in `docs/block-format.md`. Decoders are strict: they reject
//! unknown tags and versions and trailing bytes, and every accepted input
//! re-encodes to itself.
//!
//! Hashes are [`digest`]s of canonical bytes under one domain per object
//! layout:
//!
//! | Hash | Preimage |
//! | --- | --- |
//! | block hash | `digest("arkiv/header/v1", encode(header))` |
//! | `inputs_digest` | `digest("arkiv/inputs/v2", encode(inputs))` |
//! | `outcomes_digest` | `digest("arkiv/outcomes/v1", encode(outcomes))` |
//! | transaction digest | `digest("arkiv/tx/v2", encode(envelope))` |
//! | admin input digest | `digest("arkiv/admin/v1", encode(admin_input))` |
//! | genesis `inputs_digest` | `digest("arkiv/genesis/v2", encode(genesis))` |
//!
//! The genesis block is height 0 with a zero parent hash, the genesis digest
//! as its inputs digest, the digest of empty outcomes and the root of the
//! empty initial state; the genesis hash (the chain's identity) is its block
//! hash. Every header commits to the terminal state root of its block.

use std::fmt;

use arkiv_db::{
    B256, Cell, CellName, Decode, DecodeError, Decoder, Effect, Encode, Encoder, Limits,
    NamespaceId, NamespaceName, OwnerId, PriceSchedule, Receipt, RecordId, RecordKey, Rejection,
    Revision, Value, WriteOp, digest, encode,
};

/// Version of the block format: the layouts and tag tables of this module.
/// Headers carry it, so a decoder never reinterprets another layout.
/// Version 2 carries entity attributes as cells (`kind ‖ value`) in
/// `ArkivOperation`; version 1 carried bare values. Version 3 adds
/// [`ChainParams::max_rent_lifetime`] to the genesis.
pub const FORMAT_VERSION: u32 = 3;

/// Version of the execution rules a genesis selects. Version 2 is the
/// user-defined-columns entity model: per-attribute indexed flags, user
/// `bytes` and no payload or content type ([`crate::arkiv`]). Version 3
/// admits live permanent entities and caps the rent of any lifetime at
/// [`ChainParams::max_rent_lifetime`] blocks.
pub const PROTOCOL_VERSION: u32 = 3;

/// Domain of the block hash.
pub const HEADER_DOMAIN: &str = "arkiv/header/v1";
/// Domain of a block's inputs digest.
pub const INPUTS_DOMAIN: &str = "arkiv/inputs/v2";
/// Domain of a block's outcomes digest.
pub const OUTCOMES_DOMAIN: &str = "arkiv/outcomes/v1";
/// Domain of a transaction envelope's digest.
pub const TX_DOMAIN: &str = "arkiv/tx/v2";
/// Domain of an admin input's digest.
pub const ADMIN_DOMAIN: &str = "arkiv/admin/v1";
/// Domain of the genesis digest (the genesis block's inputs digest).
pub const GENESIS_DOMAIN: &str = "arkiv/genesis/v2";

/// The host-owned attribute holding a record's expiry height. Users can
/// neither write nor remove it (any cell name starting with `$` is refused
/// by static validation); the executor injects it.
pub const EXPIRES_AT: &str = "$expiresAt";

/// A client-chosen 32-byte request identifier, scoped by chain and actor.
pub type RequestId = [u8; 32];

/// The [`EXPIRES_AT`] cell name.
pub fn expires_at_name() -> CellName {
    CellName::new(EXPIRES_AT).expect("the expiry cell name is valid")
}

// Tags. Every table is documented in docs/block-format.md.

const ADMIN_SET_PRICING: u8 = 1;

const TX_CREATE_NAMESPACE: u8 = 1;
const TX_TRANSFER_NAMESPACE: u8 = 2;
const TX_PUT: u8 = 3;
const TX_DELETE: u8 = 4;
const TX_CREATE_RECORD: u8 = 5;
const TX_PATCH_RECORD: u8 = 6;
const TX_DELETE_RECORD: u8 = 7;
const TX_ARKIV: u8 = 8;
const TX_TRANSFER: u8 = 9;

const ADMIN_OUTCOME_APPLIED: u8 = 1;
const ADMIN_OUTCOME_REJECTED: u8 = 2;

const ADMIN_REJECT_RESERVATION_VIOLATED: u8 = 1;
const ADMIN_REJECT_RESERVATION_OVERFLOW: u8 = 2;
const ADMIN_REJECT_STALE_SCHEDULE: u8 = 3;
const ADMIN_REJECT_GAS_PROFILE_LOCKED: u8 = 4;

const TX_STATUS_COMMITTED: u8 = 1;
const TX_STATUS_FAILED: u8 = 2;

const OP_APPLIED: u8 = 1;
const OP_ROLLED_BACK: u8 = 2;
const OP_REJECTED: u8 = 3;
const OP_ADMISSION_REJECTED: u8 = 4;
const OP_HOST_REJECTED: u8 = 5;
const OP_NOT_EXECUTED: u8 = 6;
const OP_ACCOUNT_REJECTED: u8 = 7;

const HOST_EXPIRY_CAPACITY_FULL: u8 = 1;
const HOST_EXPIRY_INVALID: u8 = 2;
const HOST_EXPIRY_SHORTENED: u8 = 3;
const HOST_ARKIV: u8 = 4;
const HOST_ACCOUNT: u8 = 5;

const EXPIRY_OVERFLOW: u8 = 1;
const EXPIRY_DEAD_ON_ARRIVAL: u8 = 2;
const EXPIRY_TOO_FAR: u8 = 3;

/// Encoded size of a [`PriceSchedule`]: id, class count, 31 weights.
const SCHEDULE_LEN: usize = 4 + 4 + 8 * 31;

// ---------------------------------------------------------------------------
// Genesis and chain parameters.

/// Chain-wide execution parameters, fixed at genesis.
///
/// Invariants are checked by [`crate::ChainSpec::new`]; see
/// [`crate::params`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChainParams {
    /// Write budget of a whole block, including the expiry reservation.
    pub block_budget: u64,
    /// The part of the block budget user transactions may claim: the sum of
    /// the selected transactions' budgets never exceeds it.
    pub user_budget: u64,
    /// Most records that may expire at one height (the expiry capacity).
    pub expiry_per_block: u32,
    /// Most admin inputs per block.
    pub max_admin_per_block: u32,
    /// Most user transactions per block.
    pub max_txs_per_block: u32,
    /// Most operations per transaction.
    pub max_ops_per_tx: u32,
    /// Most operations over all transactions of a block.
    pub max_ops_per_block: u32,
    /// Longest encoded transaction envelope, in bytes.
    pub max_tx_bytes: u32,
    /// Longest encoded [`BlockInputs`], in bytes.
    pub max_block_input_bytes: u32,
    /// Longest finite lifetime of a generic record: its expiry height may be
    /// at most this many blocks after the block that sets it. Arkiv entities
    /// are not bounded by it: any finite expiry, or the permanent `u64::MAX`,
    /// is accepted, and only its rent is capped ([`Self::max_rent_lifetime`]).
    pub max_lifetime: u64,
    /// The rent cap of an Arkiv entity, in blocks: an operation charges rent
    /// through `min(expiry, height + max_rent_lifetime)` at most, so every
    /// lifetime of at least this many blocks, permanent included, costs the
    /// same. At least 1.
    pub max_rent_lifetime: u64,
}

impl Encode for ChainParams {
    fn encode_to(&self, encoder: &mut Encoder) {
        encoder.u64(self.block_budget);
        encoder.u64(self.user_budget);
        encoder.u32(self.expiry_per_block);
        encoder.u32(self.max_admin_per_block);
        encoder.u32(self.max_txs_per_block);
        encoder.u32(self.max_ops_per_tx);
        encoder.u32(self.max_ops_per_block);
        encoder.u32(self.max_tx_bytes);
        encoder.u32(self.max_block_input_bytes);
        encoder.u64(self.max_lifetime);
        encoder.u64(self.max_rent_lifetime);
    }
}

impl Decode for ChainParams {
    const MIN_ENCODED_LEN: usize = 8 + 8 + 7 * 4 + 8 + 8;

    fn decode_from(decoder: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            block_budget: decoder.u64()?,
            user_budget: decoder.u64()?,
            expiry_per_block: decoder.u32()?,
            max_admin_per_block: decoder.u32()?,
            max_txs_per_block: decoder.u32()?,
            max_ops_per_tx: decoder.u32()?,
            max_ops_per_block: decoder.u32()?,
            max_tx_bytes: decoder.u32()?,
            max_block_input_bytes: decoder.u32()?,
            max_lifetime: decoder.u64()?,
            max_rent_lifetime: decoder.u64()?,
        })
    }
}

/// Everything that defines a chain: its format and protocol versions,
/// identity, engine limits, initial pricing, execution parameters and the
/// actors allowed to submit admin inputs. Its digest is the genesis block's
/// inputs digest, so the genesis hash commits to all of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Genesis {
    /// Block format version; [`FORMAT_VERSION`].
    pub format_version: u32,
    /// Protocol version; [`PROTOCOL_VERSION`].
    pub protocol_version: u32,
    /// Chain identity, repeated in every header and transaction.
    pub chain_id: u64,
    /// Timestamp of the genesis header, in milliseconds.
    pub timestamp_ms: u64,
    /// Engine instance limits.
    pub limits: Limits,
    /// The pricing schedule in force at height 1.
    pub pricing: PriceSchedule,
    /// Execution parameters.
    pub params: ChainParams,
    /// Actors whose admin inputs are valid, in any order.
    pub admins: Vec<OwnerId>,
}

impl Genesis {
    /// The genesis digest: `digest("arkiv/genesis/v2", encode(self))`.
    pub fn digest(&self) -> B256 {
        digest(GENESIS_DOMAIN, &encode(self))
    }

    /// The genesis block's header: height 0, zero parent hash, the genesis
    /// digest as inputs digest, the digest of empty outcomes and the root of
    /// the empty initial state (computed without constructing an engine).
    pub fn header(&self) -> Header {
        Header {
            chain_id: self.chain_id,
            height: 0,
            parent_hash: B256::ZERO,
            timestamp_ms: self.timestamp_ms,
            inputs_digest: self.digest(),
            outcomes_digest: BlockOutcomes::default().digest(),
            state_root: arkiv_db::StateHeader {
                format: arkiv_db::STATE_FORMAT,
                catalog_root: arkiv_db_trie::EMPTY_ROOT_HASH,
                catalog_len: 0,
                next_namespace: 1,
                pricing: self.pricing.clone(),
                host_root: arkiv_db_trie::EMPTY_ROOT_HASH,
                host_len: 0,
            }
            .root(),
        }
    }

    /// The genesis hash: the genesis header's block hash.
    pub fn hash(&self) -> B256 {
        self.header().hash()
    }
}

/// `format_version u32 ‖ protocol_version u32 ‖ chain_id ‖ timestamp_ms ‖
/// limits ‖ pricing ‖ params ‖ admins`. Decoding rejects a format version
/// other than [`FORMAT_VERSION`] and a protocol version other than
/// [`PROTOCOL_VERSION`].
impl Encode for Genesis {
    fn encode_to(&self, encoder: &mut Encoder) {
        encoder.u32(self.format_version);
        encoder.u32(self.protocol_version);
        encoder.u64(self.chain_id);
        encoder.u64(self.timestamp_ms);
        encoder.put(&self.limits);
        encoder.put(&self.pricing);
        encoder.put(&self.params);
        encoder.put(&self.admins);
    }
}

impl Decode for Genesis {
    const MIN_ENCODED_LEN: usize =
        4 + 4 + 8 + 8 + 48 + SCHEDULE_LEN + ChainParams::MIN_ENCODED_LEN + 4;

    fn decode_from(decoder: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        let format_version = decoder.u32()?;
        if format_version != FORMAT_VERSION {
            return Err(DecodeError::UnsupportedVersion {
                kind: "block format",
                version: format_version,
            });
        }
        let protocol_version = decoder.u32()?;
        if protocol_version != PROTOCOL_VERSION {
            return Err(DecodeError::UnsupportedVersion {
                kind: "protocol",
                version: protocol_version,
            });
        }
        Ok(Self {
            format_version,
            protocol_version,
            chain_id: decoder.u64()?,
            timestamp_ms: decoder.u64()?,
            limits: decoder.get()?,
            pricing: decoder.get()?,
            params: decoder.get()?,
            admins: decoder.get()?,
        })
    }
}

// ---------------------------------------------------------------------------
// Headers.

/// A block header. The block hash is the digest of its encoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    /// Chain identity.
    pub chain_id: u64,
    /// Block height; the genesis block is 0.
    pub height: u64,
    /// Block hash of the parent; zero for genesis.
    pub parent_hash: B256,
    /// Producer-chosen timestamp in milliseconds, never before the
    /// parent's. Execution never reads it.
    pub timestamp_ms: u64,
    /// `digest("arkiv/inputs/v2", encode(inputs))`; the genesis digest for
    /// the genesis block.
    pub inputs_digest: B256,
    /// `digest("arkiv/outcomes/v1", encode(outcomes))`.
    pub outcomes_digest: B256,
    /// The terminal state root.
    pub state_root: B256,
}

impl Header {
    /// The block hash: `digest("arkiv/header/v1", encode(self))`.
    pub fn hash(&self) -> B256 {
        digest(HEADER_DOMAIN, &encode(self))
    }
}

/// `format_version u32 ‖ chain_id ‖ height ‖ parent_hash ‖ timestamp_ms ‖
/// inputs_digest ‖ outcomes_digest ‖ state_root`.
impl Encode for Header {
    fn encode_to(&self, encoder: &mut Encoder) {
        encoder.u32(FORMAT_VERSION);
        encoder.u64(self.chain_id);
        encoder.u64(self.height);
        encoder.put(&self.parent_hash);
        encoder.u64(self.timestamp_ms);
        encoder.put(&self.inputs_digest);
        encoder.put(&self.outcomes_digest);
        encoder.put(&self.state_root);
    }
}

impl Decode for Header {
    const MIN_ENCODED_LEN: usize = 4 + 8 + 8 + 32 + 8 + 32 + 32 + 32;

    fn decode_from(decoder: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        let version = decoder.u32()?;
        if version != FORMAT_VERSION {
            return Err(DecodeError::UnsupportedVersion {
                kind: "header",
                version,
            });
        }
        Ok(Self {
            chain_id: decoder.u64()?,
            height: decoder.u64()?,
            parent_hash: decoder.get()?,
            timestamp_ms: decoder.u64()?,
            inputs_digest: decoder.get()?,
            outcomes_digest: decoder.get()?,
            state_root: decoder.get()?,
        })
    }
}

// ---------------------------------------------------------------------------
// Block inputs.

/// An admin action. Admin inputs run before expiry and user transactions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdminAction {
    /// Commit a new pricing schedule, in force from the next block; this
    /// block stays on the parent's schedule. Rejected if it would break the
    /// expiry reservation or does not carry a larger schedule id.
    SetPricing(PriceSchedule),
}

/// `tag ‖ fields`: `SetPricing` is 1 followed by the schedule.
impl Encode for AdminAction {
    fn encode_to(&self, encoder: &mut Encoder) {
        match self {
            Self::SetPricing(schedule) => {
                encoder.u8(ADMIN_SET_PRICING);
                encoder.put(schedule);
            }
        }
    }
}

impl Decode for AdminAction {
    const MIN_ENCODED_LEN: usize = 1 + SCHEDULE_LEN;

    fn decode_from(decoder: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        match decoder.u8()? {
            ADMIN_SET_PRICING => Ok(Self::SetPricing(decoder.get()?)),
            tag => Err(DecodeError::UnknownTag {
                kind: "AdminAction",
                tag,
            }),
        }
    }
}

/// One admin input: the authenticated admin, its request id and the action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdminInput {
    /// The admin actor accepted by the host; replay never re-authenticates.
    pub actor: OwnerId,
    /// Request id, unique per (chain, actor) among admin inputs.
    pub request_id: RequestId,
    /// What to do.
    pub action: AdminAction,
}

impl AdminInput {
    /// `digest("arkiv/admin/v1", encode(self))`.
    pub fn digest(&self) -> B256 {
        digest(ADMIN_DOMAIN, &encode(self))
    }
}

/// `actor ‖ request_id ‖ action`.
impl Encode for AdminInput {
    fn encode_to(&self, encoder: &mut Encoder) {
        encoder.put(&self.actor);
        encoder.put(&self.request_id);
        encoder.put(&self.action);
    }
}

impl Decode for AdminInput {
    const MIN_ENCODED_LEN: usize = 20 + 32 + AdminAction::MIN_ENCODED_LEN;

    fn decode_from(decoder: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            actor: decoder.get()?,
            request_id: decoder.get()?,
            action: decoder.get()?,
        })
    }
}

/// The SDK's expiry pair. The executor resolves it at the block that
/// executes the operation to the expiry height
/// `E = max(expires_at, height + min_lifetime)`; `E == u64::MAX` (in
/// particular `expires_at == u64::MAX`) is permanent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Expiry {
    /// Requested expiry height; `u64::MAX` is permanent.
    pub expires_at: u64,
    /// Least number of blocks the record must live after this block.
    pub min_lifetime: u64,
}

impl Expiry {
    /// The permanent expiry: the record never expires.
    pub const PERMANENT: Self = Self {
        expires_at: u64::MAX,
        min_lifetime: 0,
    };
}

/// `expires_at ‖ min_lifetime`.
impl Encode for Expiry {
    fn encode_to(&self, encoder: &mut Encoder) {
        encoder.u64(self.expires_at);
        encoder.u64(self.min_lifetime);
    }
}

impl Decode for Expiry {
    const MIN_ENCODED_LEN: usize = 16;

    fn decode_from(decoder: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            expires_at: decoder.u64()?,
            min_lifetime: decoder.u64()?,
        })
    }
}

/// One user operation of a transaction: an engine write plus, for record
/// creations and patches, an optional expiry the host resolves and injects
/// as the `$expiresAt` attribute.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TxOp {
    /// Transfer native GLM base units; executes only on chains with
    /// accounts.
    Transfer {
        /// The recipient.
        to: OwnerId,
        /// Amount in base units.
        amount: alloy_primitives::U256,
    },
    /// An Arkiv entity operation, executed by the host's entity rules.
    Arkiv {
        /// The namespace holding the entity.
        namespace: NamespaceId,
        /// The operation.
        operation: crate::arkiv::ArkivOperation,
    },
    /// Create a namespace owned by the actor.
    CreateNamespace {
        /// Readable name.
        name: NamespaceName,
    },
    /// Transfer a namespace the actor owns.
    TransferNamespace {
        /// The namespace.
        namespace: NamespaceId,
        /// The revision the request was built against.
        expected_revision: Revision,
        /// The new owner.
        new_owner: OwnerId,
    },
    /// Insert or replace a raw entry.
    Put {
        /// The namespace.
        namespace: NamespaceId,
        /// Raw key, at most 20 bytes.
        key: Vec<u8>,
        /// Value.
        value: Vec<u8>,
    },
    /// Remove a raw entry.
    Delete {
        /// The namespace.
        namespace: NamespaceId,
        /// Raw key, at most 20 bytes.
        key: Vec<u8>,
    },
    /// Create a typed record.
    CreateRecord {
        /// The namespace.
        namespace: NamespaceId,
        /// Record key.
        key: RecordKey,
        /// User cells; no name may start with `$`.
        cells: Vec<(CellName, Cell)>,
        /// Expiry; `None` makes the record permanent.
        expiry: Option<Expiry>,
    },
    /// Patch a typed record; with an expiry and nothing else it only
    /// extends the record's lifetime (expiry may never be shortened).
    PatchRecord {
        /// The namespace.
        namespace: NamespaceId,
        /// Record key.
        key: RecordKey,
        /// Cells to set; no name may start with `$`.
        set: Vec<(CellName, Cell)>,
        /// Cells to remove; no name may start with `$`.
        remove: Vec<CellName>,
        /// New expiry; `None` leaves it unchanged.
        expiry: Option<Expiry>,
    },
    /// Delete a typed record.
    DeleteRecord {
        /// The namespace.
        namespace: NamespaceId,
        /// Record key.
        key: RecordKey,
    },
}

impl TxOp {
    /// The namespace the operation addresses; `None` for a creation.
    pub const fn namespace(&self) -> Option<NamespaceId> {
        match self {
            Self::Arkiv { namespace, .. } => Some(*namespace),
            Self::CreateNamespace { .. } | Self::Transfer { .. } => None,
            Self::TransferNamespace { namespace, .. }
            | Self::Put { namespace, .. }
            | Self::Delete { namespace, .. }
            | Self::CreateRecord { namespace, .. }
            | Self::PatchRecord { namespace, .. }
            | Self::DeleteRecord { namespace, .. } => Some(*namespace),
        }
    }

    /// The expiry the operation carries, if any.
    pub fn expiry(&self) -> Option<&Expiry> {
        match self {
            Self::Arkiv { operation, .. } => operation.expiry(),
            Self::CreateRecord { expiry, .. } | Self::PatchRecord { expiry, .. } => expiry.as_ref(),
            _ => None,
        }
    }

    /// The engine write this operation executes as: the same operation,
    /// with `$expiresAt = Attribute(expires_at)` appended to a creation's
    /// cells or a patch's set when `expires_at` is given. The value is
    /// ignored for operations without cells.
    pub fn to_write_op(&self, expires_at: Option<Value>) -> WriteOp {
        let inject = |cells: &[(CellName, Cell)]| {
            let mut cells = cells.to_vec();
            if let Some(value) = &expires_at {
                cells.push((expires_at_name(), Cell::attribute(value.clone())));
            }
            cells
        };
        match self {
            Self::Arkiv { .. } => panic!("Arkiv operation requires host execution"),
            Self::Transfer { .. } => panic!("native transfer requires account execution"),
            Self::CreateNamespace { name } => WriteOp::CreateNamespace { name: *name },
            Self::TransferNamespace {
                namespace,
                expected_revision,
                new_owner,
            } => WriteOp::TransferNamespace {
                namespace: *namespace,
                expected_revision: *expected_revision,
                new_owner: *new_owner,
            },
            Self::Put {
                namespace,
                key,
                value,
            } => WriteOp::Put {
                namespace: *namespace,
                key: key.clone(),
                value: value.clone(),
            },
            Self::Delete { namespace, key } => WriteOp::Delete {
                namespace: *namespace,
                key: key.clone(),
            },
            Self::CreateRecord {
                namespace,
                key,
                cells,
                ..
            } => WriteOp::CreateRecord {
                namespace: *namespace,
                key: key.clone(),
                cells: inject(cells),
            },
            Self::PatchRecord {
                namespace,
                key,
                set,
                remove,
                ..
            } => WriteOp::PatchRecord {
                namespace: *namespace,
                key: key.clone(),
                set: inject(set),
                remove: remove.clone(),
            },
            Self::DeleteRecord { namespace, key } => WriteOp::DeleteRecord {
                namespace: *namespace,
                key: key.clone(),
            },
        }
    }
}

/// `tag ‖ fields`, with the engine `WriteOp` tags and layouts plus
/// `‖ expiry: Option<Expiry>` for record creations and patches.
impl Encode for TxOp {
    fn encode_to(&self, encoder: &mut Encoder) {
        match self {
            Self::Transfer { to, amount } => {
                encoder.u8(TX_TRANSFER);
                encoder.put(to);
                encoder.fixed(&amount.to_be_bytes::<32>());
            }
            Self::Arkiv {
                namespace,
                operation,
            } => {
                encoder.u8(TX_ARKIV);
                encoder.put(namespace);
                encoder.put(operation);
            }
            Self::CreateNamespace { name } => {
                encoder.u8(TX_CREATE_NAMESPACE);
                encoder.put(name);
            }
            Self::TransferNamespace {
                namespace,
                expected_revision,
                new_owner,
            } => {
                encoder.u8(TX_TRANSFER_NAMESPACE);
                encoder.put(namespace);
                encoder.put(expected_revision);
                encoder.put(new_owner);
            }
            Self::Put {
                namespace,
                key,
                value,
            } => {
                encoder.u8(TX_PUT);
                encoder.put(namespace);
                encoder.bytes(key);
                encoder.bytes(value);
            }
            Self::Delete { namespace, key } => {
                encoder.u8(TX_DELETE);
                encoder.put(namespace);
                encoder.bytes(key);
            }
            Self::CreateRecord {
                namespace,
                key,
                cells,
                expiry,
            } => {
                encoder.u8(TX_CREATE_RECORD);
                encoder.put(namespace);
                encoder.put(key);
                encoder.put(cells);
                encoder.put(expiry);
            }
            Self::PatchRecord {
                namespace,
                key,
                set,
                remove,
                expiry,
            } => {
                encoder.u8(TX_PATCH_RECORD);
                encoder.put(namespace);
                encoder.put(key);
                encoder.put(set);
                encoder.put(remove);
                encoder.put(expiry);
            }
            Self::DeleteRecord { namespace, key } => {
                encoder.u8(TX_DELETE_RECORD);
                encoder.put(namespace);
                encoder.put(key);
            }
        }
    }
}

impl Decode for TxOp {
    /// The shortest operation: `CreateNamespace` with a one-character name.
    const MIN_ENCODED_LEN: usize = 1 + 5;

    fn decode_from(decoder: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        let op = match decoder.u8()? {
            TX_TRANSFER => Self::Transfer {
                to: decoder.get()?,
                amount: alloy_primitives::U256::from_be_bytes(decoder.array::<32>()?),
            },
            TX_ARKIV => Self::Arkiv {
                namespace: decoder.get()?,
                operation: decoder.get()?,
            },
            TX_CREATE_NAMESPACE => Self::CreateNamespace {
                name: decoder.get()?,
            },
            TX_TRANSFER_NAMESPACE => Self::TransferNamespace {
                namespace: decoder.get()?,
                expected_revision: decoder.get()?,
                new_owner: decoder.get()?,
            },
            TX_PUT => Self::Put {
                namespace: decoder.get()?,
                key: decoder.bytes()?.to_vec(),
                value: decoder.bytes()?.to_vec(),
            },
            TX_DELETE => Self::Delete {
                namespace: decoder.get()?,
                key: decoder.bytes()?.to_vec(),
            },
            TX_CREATE_RECORD => Self::CreateRecord {
                namespace: decoder.get()?,
                key: decoder.get()?,
                cells: decoder.get()?,
                expiry: decoder.get()?,
            },
            TX_PATCH_RECORD => Self::PatchRecord {
                namespace: decoder.get()?,
                key: decoder.get()?,
                set: decoder.get()?,
                remove: decoder.get()?,
                expiry: decoder.get()?,
            },
            TX_DELETE_RECORD => Self::DeleteRecord {
                namespace: decoder.get()?,
                key: decoder.get()?,
            },
            tag => return Err(DecodeError::UnknownTag { kind: "TxOp", tag }),
        };
        Ok(op)
    }
}

/// A user transaction: the authenticated actor, its request id, a budget
/// covering every operation, and the operations in order. The operations
/// commit together or not at all.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TxEnvelope {
    /// Chain identity; must equal the genesis chain id.
    pub chain_id: u64,
    /// The actor accepted by the host; replay never re-authenticates.
    pub actor: OwnerId,
    /// Request id, unique per (chain, actor) among transactions.
    pub request_id: RequestId,
    /// Units the transaction's operations may spend together.
    pub tx_budget: u64,
    /// The operations, executed in order.
    pub ops: Vec<TxOp>,
}

impl TxEnvelope {
    /// `digest("arkiv/tx/v2", encode(self))`.
    pub fn digest(&self) -> B256 {
        digest(TX_DOMAIN, &encode(self))
    }
}

/// `chain_id ‖ actor ‖ request_id ‖ tx_budget ‖ ops`.
impl Encode for TxEnvelope {
    fn encode_to(&self, encoder: &mut Encoder) {
        encoder.u64(self.chain_id);
        encoder.put(&self.actor);
        encoder.put(&self.request_id);
        encoder.u64(self.tx_budget);
        encoder.put(&self.ops);
    }
}

impl Decode for TxEnvelope {
    const MIN_ENCODED_LEN: usize = 8 + 20 + 32 + 8 + 4;

    fn decode_from(decoder: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            chain_id: decoder.u64()?,
            actor: decoder.get()?,
            request_id: decoder.get()?,
            tx_budget: decoder.u64()?,
            ops: decoder.get()?,
        })
    }
}

/// The recorded inputs of one block: admin inputs, then user transactions,
/// each in execution order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BlockInputs {
    /// Admin inputs, executed first.
    pub admin: Vec<AdminInput>,
    /// User transactions, executed after the expiry sweep.
    pub txs: Vec<TxEnvelope>,
}

impl BlockInputs {
    /// `digest("arkiv/inputs/v2", encode(self))`.
    pub fn digest(&self) -> B256 {
        digest(INPUTS_DOMAIN, &encode(self))
    }
}

/// `admin ‖ txs`; empty inputs are the 8 bytes of two zero counts.
impl Encode for BlockInputs {
    fn encode_to(&self, encoder: &mut Encoder) {
        encoder.put(&self.admin);
        encoder.put(&self.txs);
    }
}

impl Decode for BlockInputs {
    const MIN_ENCODED_LEN: usize = 8;

    fn decode_from(decoder: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            admin: decoder.get()?,
            txs: decoder.get()?,
        })
    }
}

// ---------------------------------------------------------------------------
// Block outcomes.

/// Why an admin input was rejected. Nothing changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdminRejection {
    /// Gas profiles are fixed at genesis and cannot be switched by repricing.
    GasProfileLocked,
    /// The schedule would leave less block budget than the user budget
    /// plus the expiry reservation.
    ReservationViolated,
    /// The reservation arithmetic under the schedule overflows 64 bits.
    ReservationOverflow,
    /// The schedule id is not larger than the id of the schedule it would
    /// replace.
    StaleSchedule {
        /// The id of the schedule committed so far in the block.
        current: u32,
    },
}

impl fmt::Display for AdminRejection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GasProfileLocked => write!(formatter, "gas profile is fixed at genesis"),
            Self::ReservationViolated => write!(formatter, "schedule breaks the reservation"),
            Self::ReservationOverflow => write!(formatter, "reservation arithmetic overflows"),
            Self::StaleSchedule { current } => {
                write!(formatter, "schedule id must exceed {current}")
            }
        }
    }
}

/// One tag byte, then `current u32` for `StaleSchedule`.
impl Encode for AdminRejection {
    fn encode_to(&self, encoder: &mut Encoder) {
        match self {
            Self::GasProfileLocked => encoder.u8(ADMIN_REJECT_GAS_PROFILE_LOCKED),
            Self::ReservationViolated => encoder.u8(ADMIN_REJECT_RESERVATION_VIOLATED),
            Self::ReservationOverflow => encoder.u8(ADMIN_REJECT_RESERVATION_OVERFLOW),
            Self::StaleSchedule { current } => {
                encoder.u8(ADMIN_REJECT_STALE_SCHEDULE);
                encoder.u32(*current);
            }
        }
    }
}

impl Decode for AdminRejection {
    fn decode_from(decoder: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        match decoder.u8()? {
            ADMIN_REJECT_GAS_PROFILE_LOCKED => Ok(Self::GasProfileLocked),
            ADMIN_REJECT_RESERVATION_VIOLATED => Ok(Self::ReservationViolated),
            ADMIN_REJECT_RESERVATION_OVERFLOW => Ok(Self::ReservationOverflow),
            ADMIN_REJECT_STALE_SCHEDULE => Ok(Self::StaleSchedule {
                current: decoder.u32()?,
            }),
            tag => Err(DecodeError::UnknownTag {
                kind: "AdminRejection",
                tag,
            }),
        }
    }
}

/// What happened to one admin input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdminOutcome {
    /// The action took effect (a schedule: from the next block).
    Applied,
    /// The action was refused and changed nothing.
    Rejected(AdminRejection),
}

/// `01` applied, or `02 ‖ AdminRejection`.
impl Encode for AdminOutcome {
    fn encode_to(&self, encoder: &mut Encoder) {
        match self {
            Self::Applied => encoder.u8(ADMIN_OUTCOME_APPLIED),
            Self::Rejected(reason) => {
                encoder.u8(ADMIN_OUTCOME_REJECTED);
                encoder.put(reason);
            }
        }
    }
}

impl Decode for AdminOutcome {
    fn decode_from(decoder: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        match decoder.u8()? {
            ADMIN_OUTCOME_APPLIED => Ok(Self::Applied),
            ADMIN_OUTCOME_REJECTED => Ok(Self::Rejected(decoder.get()?)),
            tag => Err(DecodeError::UnknownTag {
                kind: "AdminOutcome",
                tag,
            }),
        }
    }
}

/// One record deleted by the expiry sweep, with its host-path receipt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExpiryDeletion {
    /// The record's namespace.
    pub namespace: NamespaceId,
    /// The deleted record.
    pub record_id: RecordId,
    /// The delete's write receipt; its units count toward the block.
    pub receipt: Receipt,
}

/// `namespace ‖ record_id ‖ receipt`.
impl Encode for ExpiryDeletion {
    fn encode_to(&self, encoder: &mut Encoder) {
        encoder.put(&self.namespace);
        encoder.put(&self.record_id);
        encoder.put(&self.receipt);
    }
}

impl Decode for ExpiryDeletion {
    const MIN_ENCODED_LEN: usize = 8 + 8 + Receipt::MIN_ENCODED_LEN;

    fn decode_from(decoder: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            namespace: decoder.get()?,
            record_id: decoder.get()?,
            receipt: decoder.get()?,
        })
    }
}

/// Why the host refused an operation before it reached the engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExpiryFault {
    /// `height + min_lifetime` overflows 64 bits.
    Overflow,
    /// The resolved expiry height is not after this block.
    DeadOnArrival,
    /// The resolved expiry height is more than `max_lifetime` blocks away.
    TooFar,
}

/// One tag byte.
impl Encode for ExpiryFault {
    fn encode_to(&self, encoder: &mut Encoder) {
        encoder.u8(match self {
            Self::Overflow => EXPIRY_OVERFLOW,
            Self::DeadOnArrival => EXPIRY_DEAD_ON_ARRIVAL,
            Self::TooFar => EXPIRY_TOO_FAR,
        });
    }
}

impl Decode for ExpiryFault {
    fn decode_from(decoder: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        match decoder.u8()? {
            EXPIRY_OVERFLOW => Ok(Self::Overflow),
            EXPIRY_DEAD_ON_ARRIVAL => Ok(Self::DeadOnArrival),
            EXPIRY_TOO_FAR => Ok(Self::TooFar),
            tag => Err(DecodeError::UnknownTag {
                kind: "ExpiryFault",
                tag,
            }),
        }
    }
}

/// A host-side refusal of an operation, decided from state before the
/// engine runs it. No receipt, no charge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostFault {
    /// Native account execution failure.
    Account(crate::accounts::AccountFault),
    /// The Arkiv entity rules refused the operation.
    Arkiv(crate::arkiv::ArkivFault),
    /// The resolved expiry height already has `expiry_per_block` records
    /// due.
    ExpiryCapacityFull,
    /// The expiry pair does not resolve to a valid height.
    ExpiryInvalid(ExpiryFault),
    /// A patch would move the record's expiry earlier (expiry only
    /// extends; permanent is the latest).
    ExpiryShortened,
}

impl fmt::Display for HostFault {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Account(fault) => write!(formatter, "account: {fault:?}"),
            Self::Arkiv(fault) => write!(formatter, "Arkiv operation: {fault:?}"),
            Self::ExpiryCapacityFull => write!(formatter, "expiry height is at capacity"),
            Self::ExpiryInvalid(ExpiryFault::Overflow) => write!(formatter, "expiry overflows"),
            Self::ExpiryInvalid(ExpiryFault::DeadOnArrival) => {
                write!(formatter, "expiry is not after this block")
            }
            Self::ExpiryInvalid(ExpiryFault::TooFar) => {
                write!(formatter, "expiry exceeds the longest lifetime")
            }
            Self::ExpiryShortened => write!(formatter, "expiry may only be extended"),
        }
    }
}

/// One tag byte, then the `ExpiryFault` for `ExpiryInvalid`.
impl Encode for HostFault {
    fn encode_to(&self, encoder: &mut Encoder) {
        match self {
            Self::Account(fault) => {
                encoder.u8(HOST_ACCOUNT);
                encoder.put(fault);
            }
            Self::Arkiv(fault) => {
                encoder.u8(HOST_ARKIV);
                encoder.put(fault);
            }
            Self::ExpiryCapacityFull => encoder.u8(HOST_EXPIRY_CAPACITY_FULL),
            Self::ExpiryInvalid(fault) => {
                encoder.u8(HOST_EXPIRY_INVALID);
                encoder.put(fault);
            }
            Self::ExpiryShortened => encoder.u8(HOST_EXPIRY_SHORTENED),
        }
    }
}

impl Decode for HostFault {
    fn decode_from(decoder: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        match decoder.u8()? {
            HOST_ACCOUNT => Ok(Self::Account(decoder.get()?)),
            HOST_ARKIV => Ok(Self::Arkiv(decoder.get()?)),
            HOST_EXPIRY_CAPACITY_FULL => Ok(Self::ExpiryCapacityFull),
            HOST_EXPIRY_INVALID => Ok(Self::ExpiryInvalid(decoder.get()?)),
            HOST_EXPIRY_SHORTENED => Ok(Self::ExpiryShortened),
            tag => Err(DecodeError::UnknownTag {
                kind: "HostFault",
                tag,
            }),
        }
    }
}

/// What happened to one operation of a transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpOutcome {
    /// Account operation failed after charging bounded work.
    AccountRejected {
        fault: crate::accounts::AccountFault,
        receipt: Receipt,
    },
    /// Applied, and the transaction committed.
    Applied {
        /// The change.
        effect: Effect,
        /// Its write receipt.
        receipt: Receipt,
    },
    /// Applied, then undone because a later operation of the transaction
    /// failed. The receipt and its units remain.
    RolledBack {
        /// The change that was undone.
        effect: Effect,
        /// Its write receipt.
        receipt: Receipt,
    },
    /// The engine rejected the operation after charging it; the
    /// transaction failed here.
    Rejected {
        /// Why.
        reason: Rejection,
        /// The write receipt of the charged work.
        receipt: Receipt,
    },
    /// The engine rejected the operation at admission, before any charge.
    /// Unreachable for statically valid operations; kept so the format can
    /// record it.
    AdmissionRejected {
        /// Why.
        reason: Rejection,
    },
    /// The host refused the operation before the engine saw it; the
    /// transaction failed here.
    HostRejected {
        /// Why.
        fault: HostFault,
    },
    /// Not executed because an earlier operation of the transaction failed.
    NotExecuted,
}

impl OpOutcome {
    /// Units the operation charged: its receipt's, or zero.
    pub const fn spent(&self) -> u64 {
        match self {
            Self::AccountRejected { receipt, .. }
            | Self::Applied { receipt, .. }
            | Self::RolledBack { receipt, .. }
            | Self::Rejected { receipt, .. } => receipt.spent.units(),
            Self::AdmissionRejected { .. } | Self::HostRejected { .. } | Self::NotExecuted => 0,
        }
    }
}

/// `tag ‖ fields`.
impl Encode for OpOutcome {
    fn encode_to(&self, encoder: &mut Encoder) {
        match self {
            Self::AccountRejected { fault, receipt } => {
                encoder.u8(OP_ACCOUNT_REJECTED);
                encoder.put(fault);
                encoder.put(receipt);
            }
            Self::Applied { effect, receipt } => {
                encoder.u8(OP_APPLIED);
                encoder.put(effect);
                encoder.put(receipt);
            }
            Self::RolledBack { effect, receipt } => {
                encoder.u8(OP_ROLLED_BACK);
                encoder.put(effect);
                encoder.put(receipt);
            }
            Self::Rejected { reason, receipt } => {
                encoder.u8(OP_REJECTED);
                encoder.put(reason);
                encoder.put(receipt);
            }
            Self::AdmissionRejected { reason } => {
                encoder.u8(OP_ADMISSION_REJECTED);
                encoder.put(reason);
            }
            Self::HostRejected { fault } => {
                encoder.u8(OP_HOST_REJECTED);
                encoder.put(fault);
            }
            Self::NotExecuted => encoder.u8(OP_NOT_EXECUTED),
        }
    }
}

impl Decode for OpOutcome {
    fn decode_from(decoder: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        let outcome = match decoder.u8()? {
            OP_ACCOUNT_REJECTED => Self::AccountRejected {
                fault: decoder.get()?,
                receipt: decoder.get()?,
            },
            OP_APPLIED => Self::Applied {
                effect: decoder.get()?,
                receipt: decoder.get()?,
            },
            OP_ROLLED_BACK => Self::RolledBack {
                effect: decoder.get()?,
                receipt: decoder.get()?,
            },
            OP_REJECTED => Self::Rejected {
                reason: decoder.get()?,
                receipt: decoder.get()?,
            },
            OP_ADMISSION_REJECTED => Self::AdmissionRejected {
                reason: decoder.get()?,
            },
            OP_HOST_REJECTED => Self::HostRejected {
                fault: decoder.get()?,
            },
            OP_NOT_EXECUTED => Self::NotExecuted,
            tag => {
                return Err(DecodeError::UnknownTag {
                    kind: "OpOutcome",
                    tag,
                });
            }
        };
        Ok(outcome)
    }
}

/// Whether a transaction committed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxStatus {
    /// Every operation applied.
    Committed,
    /// The operation at `op_index` failed; the transaction's state was
    /// rolled back.
    Failed {
        /// Index of the failing operation.
        op_index: u32,
    },
}

/// `01` committed, or `02 ‖ op_index u32`.
impl Encode for TxStatus {
    fn encode_to(&self, encoder: &mut Encoder) {
        match self {
            Self::Committed => encoder.u8(TX_STATUS_COMMITTED),
            Self::Failed { op_index } => {
                encoder.u8(TX_STATUS_FAILED);
                encoder.u32(*op_index);
            }
        }
    }
}

impl Decode for TxStatus {
    fn decode_from(decoder: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        match decoder.u8()? {
            TX_STATUS_COMMITTED => Ok(Self::Committed),
            TX_STATUS_FAILED => Ok(Self::Failed {
                op_index: decoder.u32()?,
            }),
            tag => Err(DecodeError::UnknownTag {
                kind: "TxStatus",
                tag,
            }),
        }
    }
}

/// What happened to one transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TxOutcome {
    /// Committed or failed.
    pub status: TxStatus,
    /// One outcome per operation, in order.
    pub ops: Vec<OpOutcome>,
    /// Units the transaction's operations charged, including those of
    /// rolled-back and rejected operations.
    pub spent: u64,
}

/// `status ‖ ops ‖ spent`.
impl Encode for TxOutcome {
    fn encode_to(&self, encoder: &mut Encoder) {
        encoder.put(&self.status);
        encoder.put(&self.ops);
        encoder.u64(self.spent);
    }
}

impl Decode for TxOutcome {
    const MIN_ENCODED_LEN: usize = 1 + 4 + 8;

    fn decode_from(decoder: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            status: decoder.get()?,
            ops: decoder.get()?,
            spent: decoder.u64()?,
        })
    }
}

/// Everything a block's execution produced, in execution order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BlockOutcomes {
    /// One outcome per admin input.
    pub admin: Vec<AdminOutcome>,
    /// The expiry sweep's deletions, in (height, namespace, record id)
    /// order.
    pub expiry: Vec<ExpiryDeletion>,
    /// One outcome per user transaction.
    pub txs: Vec<TxOutcome>,
    /// Units charged to the block: every expiry deletion plus every
    /// transaction's spend.
    pub spent: u64,
}

impl BlockOutcomes {
    /// `digest("arkiv/outcomes/v1", encode(self))`.
    pub fn digest(&self) -> B256 {
        digest(OUTCOMES_DOMAIN, &encode(self))
    }
}

/// `admin ‖ expiry ‖ txs ‖ spent`.
impl Encode for BlockOutcomes {
    fn encode_to(&self, encoder: &mut Encoder) {
        encoder.put(&self.admin);
        encoder.put(&self.expiry);
        encoder.put(&self.txs);
        encoder.u64(self.spent);
    }
}

impl Decode for BlockOutcomes {
    const MIN_ENCODED_LEN: usize = 4 + 4 + 4 + 8;

    fn decode_from(decoder: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            admin: decoder.get()?,
            expiry: decoder.get()?,
            txs: decoder.get()?,
            spent: decoder.u64()?,
        })
    }
}

/// A block: its header, the inputs it executed and what they produced.
/// The header commits to both through their digests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    /// The header.
    pub header: Header,
    /// The recorded inputs.
    pub inputs: BlockInputs,
    /// The recorded outcomes.
    pub outcomes: BlockOutcomes,
}

impl Block {
    /// The block hash (the header's).
    pub fn hash(&self) -> B256 {
        self.header.hash()
    }
}

/// `header ‖ inputs ‖ outcomes`.
impl Encode for Block {
    fn encode_to(&self, encoder: &mut Encoder) {
        encoder.put(&self.header);
        encoder.put(&self.inputs);
        encoder.put(&self.outcomes);
    }
}

impl Decode for Block {
    const MIN_ENCODED_LEN: usize =
        Header::MIN_ENCODED_LEN + BlockInputs::MIN_ENCODED_LEN + BlockOutcomes::MIN_ENCODED_LEN;

    fn decode_from(decoder: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            header: decoder.get()?,
            inputs: decoder.get()?,
            outcomes: decoder.get()?,
        })
    }
}
