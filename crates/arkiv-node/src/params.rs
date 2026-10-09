//! Chain parameters: the genesis invariants, the expiry reservation and the
//! expiry sweep's read bound, all with checked arithmetic.
//!
//! The reservation keeps expiry from ever competing with users for block
//! budget. A block runs its expiry sweep before any user transaction; at
//! most `expiry_per_block` records expire at one height (the capacity
//! check of every expiry-setting operation), and each deletion costs at
//! most [`max_expiry_delete_cost`] under the schedule in force. So a block
//! budget of at least `user_budget + expiry_per_block ×
//! max_expiry_delete_cost` leaves every user operation the whole budget its
//! transaction declared: selection keeps the declared budgets within
//! `user_budget`, and user operations never meet `ExceedsBlockBudget`. The
//! reservation is checked at genesis and for every `SetPricing`.

use std::fmt;

use arkiv_db::{B256, Limits, OperationClass, OwnerId, PriceSchedule, RecordKey};

use crate::block::{ChainParams, FORMAT_VERSION, Genesis, Header, PROTOCOL_VERSION};

/// Encoded size of the largest admin input: `actor` (20) + `request_id`
/// (32) + the `SetPricing` tag (1) + a price schedule (4 + 4 + 31 × 8 =
/// 256).
pub const MAX_ADMIN_BYTES: u64 = 20 + 32 + 1 + 256;

/// Encoded size of empty [`crate::BlockInputs`]: two `u32` counts.
pub const EMPTY_INPUTS_BYTES: u64 = 8;

/// Why a reservation check failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReservationFault {
    /// `block_budget < user_budget + expiry_per_block ×
    /// max_expiry_delete_cost`.
    Violated {
        /// The least block budget the schedule needs.
        required: u64,
        /// The chain's block budget.
        block_budget: u64,
    },
    /// The reservation, the sweep read bound or the host's expiry read
    /// cost does not fit in 64 bits.
    Overflow,
}

impl fmt::Display for ReservationFault {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Violated {
                required,
                block_budget,
            } => write!(
                formatter,
                "block budget {block_budget} is below the required {required}"
            ),
            Self::Overflow => write!(formatter, "reservation arithmetic overflows"),
        }
    }
}

/// Why a genesis is not acceptable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GenesisError {
    /// A named gas schedule has noncanonical weights.
    InvalidGasSchedule,
    /// The block format version is not [`FORMAT_VERSION`].
    UnsupportedFormat(u32),
    /// The protocol version is not one this build executes.
    UnsupportedProtocol(u32),
    /// `1 ≤ max_ops_per_tx ≤ max_ops_per_block` does not hold.
    OpsPerTx,
    /// `max_txs_per_block` is zero.
    TxsPerBlock,
    /// `expiry_per_block` is zero.
    ExpiryPerBlock,
    /// `user_budget` is zero.
    UserBudget,
    /// `8 + max_admin_per_block × MAX_ADMIN_BYTES + max_tx_bytes ≤
    /// max_block_input_bytes` does not hold (or overflows).
    InputBytes,
    /// The initial schedule breaks the expiry reservation.
    Reservation(ReservationFault),
    /// `limits.max_cells` is zero: no record could carry `$expiresAt`,
    /// and the host's own expiry read would be refused.
    MaxCells,
    /// `limits.max_page` is zero: the expiry sweep's reads would be
    /// refused.
    MaxPage,
    /// `limits.max_groups` is zero: the expiry sweep's one-group query
    /// would be refused.
    MaxGroups,
    /// `limits.max_literals` is zero: the expiry sweep's one-literal query
    /// would be refused.
    MaxLiterals,
    /// A cell limit is outside its range (see `Limits::validate`).
    CellLimits(&'static str),
}

impl fmt::Display for GenesisError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedFormat(version) => {
                write!(formatter, "unsupported block format version {version}")
            }
            Self::UnsupportedProtocol(version) => {
                write!(formatter, "unsupported protocol version {version}")
            }
            Self::InvalidGasSchedule => write!(formatter, "invalid gas schedule"),
            Self::OpsPerTx => write!(
                formatter,
                "operations per transaction must be 1 to the operations per block"
            ),
            Self::TxsPerBlock => write!(formatter, "transactions per block must be at least 1"),
            Self::ExpiryPerBlock => write!(formatter, "expiry capacity must be at least 1"),
            Self::UserBudget => write!(formatter, "user budget must be at least 1"),
            Self::InputBytes => write!(
                formatter,
                "block input bytes cannot hold a full admin section and a largest transaction"
            ),
            Self::Reservation(fault) => write!(formatter, "expiry reservation: {fault}"),
            Self::MaxCells => write!(formatter, "records must hold at least one cell"),
            Self::CellLimits(reason) => formatter.write_str(reason),
            Self::MaxPage => write!(formatter, "query pages must hold at least one row"),
            Self::MaxGroups => write!(formatter, "queries must allow at least one group"),
            Self::MaxLiterals => {
                write!(formatter, "query groups must allow at least one literal")
            }
        }
    }
}

impl std::error::Error for GenesisError {}

fn units(value: usize) -> u64 {
    u64::try_from(value).expect("usize fits in u64")
}

/// Largest cost of one expiry deletion under `schedule`: the model-3
/// `DeleteRecord` charges of a record whose `max_cells` cells are all
/// attributes, `DirectoryLookup + PointRead + RecordDelete + c·CellRemove +
/// 3·RootUpdate + 2a·(IndexRemove + 2·RootUpdate)` with `c = a =
/// max_cells`. `None` on overflow.
pub fn max_expiry_delete_cost(schedule: &PriceSchedule, limits: &Limits) -> Option<u64> {
    let price = |class| schedule.price(class);
    let cells = units(limits.max_cells);
    let index_remove = price(OperationClass::RootUpdate)
        .checked_mul(2)?
        .checked_add(price(OperationClass::IndexRemove))?;
    price(OperationClass::DirectoryLookup)
        .checked_add(price(OperationClass::PointRead))?
        .checked_add(price(OperationClass::RecordDelete))?
        .checked_add(cells.checked_mul(price(OperationClass::CellRemove))?)?
        .checked_add(price(OperationClass::RootUpdate).checked_mul(3)?)?
        .checked_add(cells.checked_mul(2)?.checked_mul(index_remove)?)
}

/// Budget of each read of the expiry sweep under `schedule`: the larger of
/// a `terms` read of one `U64` height (`DirectoryLookup + TermSeek +
/// TermVisit + 8·ReadByte`) and a full `KEY_ONLY` `Eq` page
/// (`DirectoryLookup + QueryGroup + TermSeek + (max_page + 1)·PostingEntry
/// + max_page·(ResultRow + 32·ReadByte)`, the page's rows plus the
/// look-ahead entry, with keys of at most 32 bytes). `None` on overflow.
pub fn sweep_read_bound(schedule: &PriceSchedule, limits: &Limits) -> Option<u64> {
    let price = |class| schedule.price(class);
    let page = units(limits.max_page);
    let key_bytes = units(RecordKey::MAX_LEN);
    let terms = price(OperationClass::DirectoryLookup)
        .checked_add(price(OperationClass::TermSeek))?
        .checked_add(price(OperationClass::TermVisit))?
        .checked_add(price(OperationClass::ReadByte).checked_mul(8)?)?;
    let row = price(OperationClass::ReadByte)
        .checked_mul(key_bytes)?
        .checked_add(price(OperationClass::ResultRow))?;
    let query = price(OperationClass::DirectoryLookup)
        .checked_add(price(OperationClass::QueryGroup))?
        .checked_add(price(OperationClass::TermSeek))?
        .checked_add(
            page.checked_add(1)?
                .checked_mul(price(OperationClass::PostingEntry))?,
        )?
        .checked_add(page.checked_mul(row)?)?;
    Some(terms.max(query))
}

/// Encoded bytes of a `$expiresAt` cell as a read counts them: the name
/// (10 bytes) and a `U64` value (8 bytes). The host writes no other value
/// under that name, and users cannot write it at all.
const EXPIRY_CELL_BYTES: u64 = 10 + 8;

/// Largest cost of the host's expiry pre-read under `schedule`: a
/// `get_record` of one key projecting only `$expiresAt`, `DirectoryLookup +
/// PointRead + ResultRow + 18·ReadByte` (the row's key is not projected).
/// The executor runs it with an unlimited budget and discards its receipt,
/// so this only has to be representable. `None` on overflow.
pub fn max_expiry_read_cost(schedule: &PriceSchedule) -> Option<u64> {
    let price = |class| schedule.price(class);
    price(OperationClass::DirectoryLookup)
        .checked_add(price(OperationClass::PointRead))?
        .checked_add(price(OperationClass::ResultRow))?
        .checked_add(price(OperationClass::ReadByte).checked_mul(EXPIRY_CELL_BYTES)?)
}

/// Checks the expiry reservation of `schedule`: `block_budget ≥
/// user_budget + expiry_per_block × max_expiry_delete_cost`, and that the
/// sweep read bound and the host's expiry pre-read cost
/// ([`max_expiry_read_cost`]) are representable.
pub fn check_reservation(
    params: &ChainParams,
    limits: &Limits,
    schedule: &PriceSchedule,
) -> Result<(), ReservationFault> {
    sweep_read_bound(schedule, limits).ok_or(ReservationFault::Overflow)?;
    max_expiry_read_cost(schedule).ok_or(ReservationFault::Overflow)?;
    let required = max_expiry_delete_cost(schedule, limits)
        .and_then(|cost| cost.checked_mul(u64::from(params.expiry_per_block)))
        .and_then(|reserved| reserved.checked_add(params.user_budget))
        .ok_or(ReservationFault::Overflow)?;
    if params.block_budget < required {
        return Err(ReservationFault::Violated {
            required,
            block_budget: params.block_budget,
        });
    }
    Ok(())
}

/// Checks the genesis invariants of chain parameters and limits.
pub fn check_params(
    params: &ChainParams,
    limits: &Limits,
    schedule: &PriceSchedule,
) -> Result<(), GenesisError> {
    if params.max_ops_per_tx == 0 || params.max_ops_per_tx > params.max_ops_per_block {
        return Err(GenesisError::OpsPerTx);
    }
    if params.max_txs_per_block == 0 {
        return Err(GenesisError::TxsPerBlock);
    }
    if params.expiry_per_block == 0 {
        return Err(GenesisError::ExpiryPerBlock);
    }
    if params.user_budget == 0 {
        return Err(GenesisError::UserBudget);
    }
    let inputs = u64::from(params.max_admin_per_block)
        .checked_mul(MAX_ADMIN_BYTES)
        .and_then(|admin| admin.checked_add(EMPTY_INPUTS_BYTES))
        .and_then(|bytes| bytes.checked_add(u64::from(params.max_tx_bytes)));
    if inputs.is_none_or(|bytes| bytes > u64::from(params.max_block_input_bytes)) {
        return Err(GenesisError::InputBytes);
    }
    if limits.max_cells == 0 {
        return Err(GenesisError::MaxCells);
    }
    limits.validate().map_err(GenesisError::CellLimits)?;
    if limits.max_page == 0 {
        return Err(GenesisError::MaxPage);
    }
    if limits.max_groups == 0 {
        return Err(GenesisError::MaxGroups);
    }
    if limits.max_literals == 0 {
        return Err(GenesisError::MaxLiterals);
    }
    check_reservation(params, limits, schedule).map_err(GenesisError::Reservation)
}

/// A validated genesis: the chain definition every block is executed and
/// checked against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainSpec {
    genesis: Genesis,
    genesis_header: Header,
    genesis_hash: B256,
}

impl ChainSpec {
    /// Validates `genesis`: its format and protocol versions and every
    /// genesis invariant (see [`check_params`]).
    pub fn new(genesis: Genesis) -> Result<Self, GenesisError> {
        if genesis.format_version != FORMAT_VERSION {
            return Err(GenesisError::UnsupportedFormat(genesis.format_version));
        }
        if genesis.protocol_version != PROTOCOL_VERSION {
            return Err(GenesisError::UnsupportedProtocol(genesis.protocol_version));
        }
        if !crate::gas::validate_schedule(&genesis.pricing) {
            return Err(GenesisError::InvalidGasSchedule);
        }
        check_params(&genesis.params, &genesis.limits, &genesis.pricing)?;
        Ok(Self::new_unchecked(genesis))
    }

    /// Test aid: accepts `genesis` without checking any invariant, so tests
    /// can reach the executor's invariant and halt paths. A chain built
    /// this way has none of the guarantees of [`ChainSpec::new`].
    #[doc(hidden)]
    pub fn new_unchecked(genesis: Genesis) -> Self {
        let genesis_header = genesis.header();
        let genesis_hash = genesis_header.hash();
        Self {
            genesis,
            genesis_header,
            genesis_hash,
        }
    }

    /// The genesis.
    pub const fn genesis(&self) -> &Genesis {
        &self.genesis
    }

    /// The chain id.
    pub const fn chain_id(&self) -> u64 {
        self.genesis.chain_id
    }

    /// The protocol version.
    pub const fn protocol_version(&self) -> u32 {
        self.genesis.protocol_version
    }

    /// Execution parameters.
    pub const fn params(&self) -> &ChainParams {
        &self.genesis.params
    }

    /// Engine limits.
    pub const fn limits(&self) -> &Limits {
        &self.genesis.limits
    }

    /// Whether `actor` may submit admin inputs.
    pub fn is_admin(&self, actor: &OwnerId) -> bool {
        self.genesis.admins.contains(actor)
    }

    /// The genesis block's header (height 0).
    pub const fn genesis_header(&self) -> &Header {
        &self.genesis_header
    }

    /// The genesis hash: the chain's identity.
    pub const fn genesis_hash(&self) -> B256 {
        self.genesis_hash
    }
}
