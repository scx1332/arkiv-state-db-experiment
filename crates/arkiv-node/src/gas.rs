//! Opt-in GLM gas profile. Logical occupancy, never physical trie size, determines rent.
//! Gas prices execution work only; entity rent is a separate GLM amount (wei)
//! per storage unit per second of capped lifetime ([`rent`], [`RentClock`]),
//! debited from the sender outside the gas budget. Prices are an experimental
//! capacity policy, not a measured hardware cost.
use arkiv_db::{Cell, CellKind, CellName, PriceSchedule};

pub const SCHEDULE_ID: u32 = 0x474c4d01;
pub const TX_INTRINSIC_GAS: u64 = 21_000;
pub const DEFAULT_BLOCK_GAS_LIMIT: u64 = 100_000_000;
pub const REFERENCE_GAS_PRICE_WEI: u64 = 1_000_000_000;
pub const ENTITY_OVERHEAD_UNITS: u64 = 512;
pub const INDEX_OVERHEAD_UNITS: u64 = 128;
/// The rent rate, in GLM base units (wei) per storage unit per day of
/// chargeable lifetime: 20 gwei. It is the `glm-rent-v2` rate at 2-second
/// blocks (8 gas at 1 gwei per 17,280 blocks of 2 s), now fixed per second so
/// the same lifetime costs the same at every block period. A year is
/// 7,300 gwei per unit.
pub const RENT_WEI_PER_UNIT_DAY: u128 = 20_000_000_000;
/// Milliseconds per day, the denominator of [`RENT_WEI_PER_UNIT_DAY`].
pub const DAY_MS: u128 = 86_400_000;
/// The default rent cap, [`ChainParams::max_rent_lifetime_seconds`]: 365 days.
///
/// [`ChainParams::max_rent_lifetime_seconds`]: crate::ChainParams::max_rent_lifetime_seconds
pub const DEFAULT_RENT_CAP_SECONDS: u64 = 365 * 24 * 60 * 60;
/// The longest rent cap a genesis may set: 100 years of 365 days.
pub const MAX_RENT_CAP_SECONDS: u64 = 100 * DEFAULT_RENT_CAP_SECONDS;
/// The longest block period a genesis may set: one day.
pub const MAX_BLOCK_PERIOD_MS: u64 = 86_400_000;
/// Storage units of an entity's seven system cells and its fixed overhead:
/// a create without user attributes. 512 + 180 cell bytes + 6 indexed × 128.
pub const EMPTY_ENTITY_UNITS: u64 = 1_460;
/// The default per-block storage cap,
/// [`ChainParams::max_new_storage_units_per_block`]: 1,500,000 units, the
/// most any block can add under the default signed limits
/// ([`block_storage_bound`] of 786,432 input bytes and 128 operations is
/// 1,497,600), rounded up. So it binds no block the input cap admits; a
/// genesis lowers it to throttle storage growth.
///
/// [`ChainParams::max_new_storage_units_per_block`]: crate::ChainParams::max_new_storage_units_per_block
pub const DEFAULT_MAX_NEW_STORAGE_UNITS_PER_BLOCK: u64 = 1_500_000;
/// The most storage units the committed transactions of one block can add
/// under its input and operation limits: every user cell costs at most its
/// encoded input bytes (a name and value plus two tags, against a length
/// prefix, a kind and a value tag in the input) plus 128 if indexed, and each
/// operation adds at most an empty entity's units and 32 index reservations:
/// `max_block_input_bytes + max_ops_per_block × (EMPTY_ENTITY_UNITS + 32 × 128)`.
pub const fn block_storage_bound(max_block_input_bytes: u32, max_ops_per_block: u32) -> u64 {
    max_block_input_bytes as u64
        + max_ops_per_block as u64
            * (EMPTY_ENTITY_UNITS + crate::arkiv::MAX_ATTRIBUTES as u64 * INDEX_OVERHEAD_UNITS)
}

/// A rent cap of `seconds` at `block_period_ms`, in whole blocks rounded up so
/// the cap is never shorter than asked. `None` for a zero period or overflow.
pub const fn rent_cap_blocks(seconds: u64, block_period_ms: u64) -> Option<u64> {
    if block_period_ms == 0 {
        return None;
    }
    match seconds.checked_mul(1000) {
        Some(ms) => Some(ms.div_ceil(block_period_ms)),
        None => None,
    }
}

/// The chain's rent clock: its block period and its rent cap in blocks,
/// derived from the consensus parameters
/// [`ChainParams::block_period_ms`] and
/// [`ChainParams::max_rent_lifetime_seconds`]. Lifetimes are block heights,
/// so the cap is applied in blocks and the charged blocks are priced in
/// time at the block period.
///
/// Within the genesis bounds the cap is at most `MAX_RENT_CAP_SECONDS × 1000`
/// blocks (about 3.2·10^12), so `height + cap` cannot saturate below the
/// sentinel heights, and a quote of any `u64` size fits `u128`.
///
/// [`ChainParams::block_period_ms`]: crate::ChainParams::block_period_ms
/// [`ChainParams::max_rent_lifetime_seconds`]: crate::ChainParams::max_rent_lifetime_seconds
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RentClock {
    block_period_ms: u64,
    cap_blocks: u64,
}
impl RentClock {
    /// `None` unless `1 ≤ block_period_ms ≤ MAX_BLOCK_PERIOD_MS` and
    /// `1 ≤ cap_seconds ≤ MAX_RENT_CAP_SECONDS`.
    pub const fn new(block_period_ms: u64, cap_seconds: u64) -> Option<Self> {
        if block_period_ms == 0
            || block_period_ms > MAX_BLOCK_PERIOD_MS
            || cap_seconds == 0
            || cap_seconds > MAX_RENT_CAP_SECONDS
        {
            return None;
        }
        match rent_cap_blocks(cap_seconds, block_period_ms) {
            Some(cap_blocks) => Some(Self {
                block_period_ms,
                cap_blocks,
            }),
            None => None,
        }
    }
    /// The clock of validated chain parameters; `None` outside the bounds.
    pub const fn of(params: &crate::ChainParams) -> Option<Self> {
        Self::new(params.block_period_ms, params.max_rent_lifetime_seconds)
    }
    pub const fn block_period_ms(&self) -> u64 {
        self.block_period_ms
    }
    /// The rent cap in blocks: `ceil(cap_seconds × 1000 / block_period_ms)`.
    pub const fn cap_blocks(&self) -> u64 {
        self.cap_blocks
    }
    /// The price of `unit_blocks` storage-unit blocks:
    /// `ceil(unit_blocks × block_period_ms × RENT_WEI_PER_UNIT_DAY / DAY_MS)`
    /// wei, computed with the reduced rate `6250 / 27` wei per unit-millisecond.
    /// `None` on overflow, which needs more than `u64::MAX` units at the
    /// longest cap.
    pub const fn wei(&self, unit_blocks: u128) -> Option<u128> {
        let Some(unit_ms) = unit_blocks.checked_mul(self.block_period_ms as u128) else {
            return None;
        };
        match unit_ms.checked_mul(RATE_NUMERATOR) {
            Some(n) => Some(n.div_ceil(RATE_DENOMINATOR)),
            None => None,
        }
    }
}
const fn gcd(a: u128, b: u128) -> u128 {
    if b == 0 { a } else { gcd(b, a % b) }
}
/// [`RENT_WEI_PER_UNIT_DAY`] / [`DAY_MS`] in lowest terms: 6250 / 27.
const RATE_NUMERATOR: u128 = RENT_WEI_PER_UNIT_DAY / gcd(RENT_WEI_PER_UNIT_DAY, DAY_MS);
const RATE_DENOMINATOR: u128 = DAY_MS / gcd(RENT_WEI_PER_UNIT_DAY, DAY_MS);

/// Counts remain model 3; this schedule gives its logical work Ethereum-sized units.
pub const fn schedule() -> PriceSchedule {
    PriceSchedule::new(
        SCHEDULE_ID,
        [
            100, 20_000, 10_000, 10_000, 10_000, 4, 4, 100, 100, 10, 1, 20_000, 12_000, 10_000,
            250, 4, 100, 500, 0, 300, 100, 100, 10, 10, 10, 10, 10, 100, 2, 10, 1,
        ],
    )
}
pub fn is_active(pricing: &PriceSchedule) -> bool {
    pricing.id() == SCHEDULE_ID
}
pub fn validate_schedule(pricing: &PriceSchedule) -> bool {
    !is_active(pricing) || *pricing == schedule()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GasError {
    Overflow,
    InvalidLifetime,
}
impl std::fmt::Display for GasError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Overflow => "gas arithmetic overflow",
            Self::InvalidLifetime => "invalid or shortened entity lifetime",
        })
    }
}
impl std::error::Error for GasError {}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StorageUnits {
    pub entity_overhead: u64,
    /// Name, value and two tag bytes of every cell, indexed or not.
    pub cell_bytes: u64,
    /// [`INDEX_OVERHEAD_UNITS`] per indexed ([`CellKind::Attribute`]) cell.
    pub index_overhead: u64,
    pub total: u64,
}
fn add(a: u64, b: u64) -> Result<u64, GasError> {
    a.checked_add(b).ok_or(GasError::Overflow)
}
/// Every committed system and user cell is counted the same way, whatever its
/// name or type: its name, value and two tag bytes are stored data. Only an
/// indexed cell ([`CellKind::Attribute`]) also reserves a value and presence
/// posting, independent of sharing; an unindexed one ([`CellKind::Field`],
/// such as any `bytes` value) pays storage only.
pub fn storage_units(cells: &[(CellName, Cell)]) -> Result<StorageUnits, GasError> {
    let mut result = StorageUnits {
        entity_overhead: ENTITY_OVERHEAD_UNITS,
        ..Default::default()
    };
    for (name, cell) in cells {
        let bytes = u64::try_from(cell.value.encoded_len()).map_err(|_| GasError::Overflow)?;
        result.cell_bytes = add(result.cell_bytes, add(add(name.len() as u64, bytes)?, 2)?)?;
        if cell.kind == CellKind::Attribute {
            result.index_overhead = add(result.index_overhead, INDEX_OVERHEAD_UNITS)?;
        }
    }
    result.total = add(
        add(result.entity_overhead, result.cell_bytes)?,
        result.index_overhead,
    )?;
    Ok(result)
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RentQuote {
    pub growth_units: u64,
    /// Blocks the operation newly pays for: the growth of the capped horizon.
    pub extension_blocks: u64,
    /// Blocks the old expiry already covered, capped: what growth pays for.
    pub remaining_blocks: u64,
    /// `growth_units × remaining_blocks + after_units × extension_blocks`.
    pub unit_blocks: u128,
    /// The rent: `unit_blocks` priced by [`RentClock::wei`].
    pub wei: u128,
}
/// The rent horizon of `expiry` at `height`: rent is never charged past
/// `height + cap`, so every expiry at least `cap` blocks away, the permanent
/// `u64::MAX` included, has the same horizon. Saturates instead of
/// overflowing near `u64::MAX`.
pub const fn rent_horizon(expiry: u64, height: u64, cap: u64) -> u64 {
    let limit = height.saturating_add(cap);
    if expiry < limit { expiry } else { limit }
}
/// Rent of one operation at `height` under the chain's rent clock. Every
/// lifetime is charged through its [`rent_horizon`] only, recomputed from the
/// current height with the cap in blocks ([`RentClock::cap_blocks`]):
///
/// - a create prepays `after × (horizon(new) − height)`;
/// - growth prepays the positive size increase for the old remaining capped
///   lifetime, `growth × (horizon(old) − height)`;
/// - an extension prepays the new size for the growth of the capped horizon,
///   `after × (horizon(new) − horizon(old))`, which is zero when the old expiry
///   already reaches the cap.
///
/// The unit-blocks are then priced in time, [`RentClock::wei`]. Lifetimes at
/// or beyond the cap, permanent included, cost the same as the cap; shorter
/// ones are priced exactly. Delete and shrink give no refunds. Rounding
/// occurs once per operation, upward to the next wei, never per byte or per
/// block. All arithmetic is checked. Rent is a GLM debit of its own, not gas.
pub fn rent(
    before_units: u64,
    after_units: u64,
    old_expiry: Option<u64>,
    new_expiry: Option<u64>,
    height: u64,
    clock: RentClock,
) -> Result<RentQuote, GasError> {
    let Some(new) = new_expiry else {
        return Ok(RentQuote::default());
    };
    if new <= height || old_expiry.is_some_and(|old| old <= height || old > new) {
        return Err(GasError::InvalidLifetime);
    }
    let cap = clock.cap_blocks();
    let paid = old_expiry.map_or(height, |old| rent_horizon(old, height, cap));
    let horizon = rent_horizon(new, height, cap);
    let growth_units = if old_expiry.is_none() {
        0
    } else {
        after_units.saturating_sub(before_units)
    };
    let remaining_blocks = paid - height;
    let extension_blocks = horizon - paid;
    let unit_blocks = u128::from(growth_units)
        .checked_mul(u128::from(remaining_blocks))
        .and_then(|n| {
            u128::from(after_units)
                .checked_mul(u128::from(extension_blocks))
                .and_then(|ext| n.checked_add(ext))
        })
        .ok_or(GasError::Overflow)?;
    let wei = clock.wei(unit_blocks).ok_or(GasError::Overflow)?;
    Ok(RentQuote {
        growth_units,
        extension_blocks,
        remaining_blocks,
        unit_blocks,
        wei,
    })
}
/// Exact reference cost in GLM base units. This is a quote, not a balance transfer.
pub fn reference_fee_wei(gas: u64) -> u128 {
    u128::from(gas) * u128::from(REFERENCE_GAS_PRICE_WEI)
}
pub fn reference_fee_glm(gas: u64) -> String {
    glm(reference_fee_wei(gas))
}
/// `wei` as a decimal GLM amount without trailing zeros.
pub fn glm(wei: u128) -> String {
    let unit = 1_000_000_000_000_000_000u128;
    let whole = wei / unit;
    let fraction = format!("{:018}", wei % unit);
    let fraction = fraction.trim_end_matches('0');
    if fraction.is_empty() {
        whole.to_string()
    } else {
        format!("{whole}.{fraction}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arkiv_db::{OperationClass, Value};
    /// The SDK's nominal 2-second blocks with the default one-year cap.
    const CLOCK: RentClock = match RentClock::new(2_000, DEFAULT_RENT_CAP_SECONDS) {
        Some(clock) => clock,
        None => panic!("default clock"),
    };
    /// A year of one storage unit: 365 × 20 gwei.
    const UNIT_YEAR_WEI: u128 = 7_300_000_000_000;
    const GWEI: u128 = 1_000_000_000;
    fn clock(period_ms: u64, cap_seconds: u64) -> RentClock {
        RentClock::new(period_ms, cap_seconds).unwrap()
    }
    #[test]
    fn logical_size_is_content_and_kind_based() {
        let cells = vec![
            (
                CellName::new("body").unwrap(),
                Cell::field(Value::bytes(&[0; 1024])),
            ),
            (
                CellName::new("score").unwrap(),
                Cell::attribute(Value::U64(1)),
            ),
        ];
        let units = storage_units(&cells).unwrap();
        assert_eq!(
            units,
            StorageUnits {
                entity_overhead: 512,
                // body: 4 + 1024 + 2; score: 5 + 8 + 2.
                cell_bytes: 1045,
                index_overhead: 128,
                total: 1685
            }
        );
        // 17,280 blocks of 2 s (9.6 hours) cost what glm-rent-v2 charged in
        // gas at 1 gwei: 13,480 gwei.
        assert_eq!(
            rent(0, units.total, None, Some(17_281), 1, CLOCK)
                .unwrap()
                .wei,
            13_480 * GWEI
        );
        // The name never selects a pricing rule: only the bytes and the kind do.
        for (name, kind) in [("$payload", Cell::field), ("data", Cell::field)] {
            let renamed = vec![(CellName::new(name).unwrap(), kind(Value::bytes(&[0; 1024])))];
            assert_eq!(
                storage_units(&renamed).unwrap().cell_bytes,
                name.len() as u64 + 1026
            );
        }
        // The same value indexed pays the posting reservation on top.
        let value = Value::str("hello");
        let field = storage_units(&[(CellName::new("title").unwrap(), Cell::field(value.clone()))])
            .unwrap();
        let indexed =
            storage_units(&[(CellName::new("title").unwrap(), Cell::attribute(value))]).unwrap();
        assert_eq!(field.cell_bytes, indexed.cell_bytes);
        assert_eq!(indexed.total - field.total, INDEX_OVERHEAD_UNITS);
    }
    #[test]
    fn lifecycle_charges_growth_and_extension_without_refunds() {
        let wei = |before, after, old, new| rent(before, after, old, new, 10, CLOCK).unwrap().wei;
        assert_eq!(wei(0, 2000, None, Some(17_290)), 16_000 * GWEI);
        assert_eq!(wei(2000, 2500, Some(17_290), Some(17_290)), 4000 * GWEI);
        assert_eq!(wei(2000, 1000, Some(17_290), Some(17_290)), 0);
        assert_eq!(wei(2000, 2000, Some(17_290), Some(34_570)), 16_000 * GWEI);
        assert_eq!(wei(2000, 0, Some(17_290), None), 0);
        assert_eq!(wei(2000, 2500, Some(17_290), Some(34_570)), 24_000 * GWEI);
    }
    #[test]
    fn boundaries_round_once_and_reject_invalid_lifetimes() {
        // One unit-block of 2 s is 462,962.96… wei, rounded up once.
        assert_eq!(rent(0, 1, None, Some(2), 1, CLOCK).unwrap().wei, 462_963);
        assert_eq!(
            rent(0, 27, None, Some(2), 1, CLOCK).unwrap().wei,
            27 * 462_962 + 26
        );
        assert_eq!(
            rent(0, 1, None, Some(1), 1, CLOCK),
            Err(GasError::InvalidLifetime)
        );
        assert_eq!(
            rent(1, 1, Some(10), Some(9), 1, CLOCK),
            Err(GasError::InvalidLifetime)
        );
        assert_eq!(
            rent(1, 1, Some(1), Some(9), 1, CLOCK),
            Err(GasError::InvalidLifetime)
        );
        // Within the genesis bounds no size overflows: the largest cap at the
        // shortest period still prices u64::MAX units.
        let widest = clock(1, MAX_RENT_CAP_SECONDS);
        assert_eq!(widest.cap_blocks(), 3_153_600_000_000);
        let huge = rent(0, u64::MAX, None, Some(u64::MAX), 0, widest).unwrap();
        assert_eq!(
            huge.wei,
            (u128::from(u64::MAX) * 3_153_600_000_000 * 6250).div_ceil(27)
        );
        let grow = rent(1, u64::MAX, Some(u64::MAX), Some(u64::MAX), 7, widest).unwrap();
        assert_eq!(
            grow.unit_blocks,
            u128::from(u64::MAX - 1) * 3_153_600_000_000
        );
    }
    #[test]
    fn the_default_storage_cap_binds_no_block_the_input_cap_admits() {
        let empty = crate::arkiv::create_cells([7; 32], [1; 20], 9, u64::MAX, 3, &[]);
        assert_eq!(storage_units(&empty).unwrap().total, EMPTY_ENTITY_UNITS);
        let bound = block_storage_bound(crate::signed::protocol::INPUT_CAP, 128);
        assert_eq!(bound, 786_432 + 128 * (1_460 + 32 * 128));
        assert_eq!(bound, 1_497_600);
        assert_eq!(
            DEFAULT_MAX_NEW_STORAGE_UNITS_PER_BLOCK.max(bound),
            DEFAULT_MAX_NEW_STORAGE_UNITS_PER_BLOCK
        );
        // Eleven full 128 KiB entities fit the default; the input cap admits five.
        assert_eq!(DEFAULT_MAX_NEW_STORAGE_UNITS_PER_BLOCK / 133_134, 11);
    }
    #[test]
    fn the_clock_bounds_the_period_and_the_cap() {
        assert_eq!(DEFAULT_RENT_CAP_SECONDS, 31_536_000);
        assert_eq!(MAX_RENT_CAP_SECONDS, 3_153_600_000);
        for (period, cap) in [
            (0, DEFAULT_RENT_CAP_SECONDS),
            (MAX_BLOCK_PERIOD_MS + 1, DEFAULT_RENT_CAP_SECONDS),
            (2_000, 0),
            (2_000, MAX_RENT_CAP_SECONDS + 1),
            (2_000, u64::MAX),
        ] {
            assert_eq!(RentClock::new(period, cap), None, "{period} {cap}");
        }
        assert_eq!(clock(MAX_BLOCK_PERIOD_MS, 1).cap_blocks(), 1);
        assert_eq!(clock(1, MAX_RENT_CAP_SECONDS).block_period_ms(), 1);
        // h + cap never saturates before the sentinel neighbourhood.
        let widest = clock(1, MAX_RENT_CAP_SECONDS);
        assert!(u64::MAX / 2 + widest.cap_blocks() < u64::MAX);
        assert_eq!(rent_cap_blocks(DEFAULT_RENT_CAP_SECONDS, 0), None);
        assert_eq!(rent_cap_blocks(u64::MAX, 1_000), None);
        assert_eq!(rent_cap_blocks(1, 3_000), Some(1));
    }
    #[test]
    fn a_year_costs_the_same_at_every_block_period() {
        let units = 3_086;
        for (period, cap_blocks) in [
            (250, 126_144_000),
            (500, 63_072_000),
            (1_000, 31_536_000),
            (2_000, 15_768_000),
            (5_000, 6_307_200),
        ] {
            let c = clock(period, DEFAULT_RENT_CAP_SECONDS);
            assert_eq!(c.cap_blocks(), cap_blocks);
            let year = rent(0, units, None, Some(u64::MAX), 100, c).unwrap();
            assert_eq!(year.extension_blocks, cap_blocks);
            assert_eq!(year.wei, u128::from(units) * UNIT_YEAR_WEI, "{period}");
            // A day of the same entity is a 365th of it at any period.
            let day = rent(0, units, None, Some(100 + 86_400_000 / period), 100, c).unwrap();
            assert_eq!(day.wei * 365, year.wei);
        }
        // A period that does not divide a year rounds the cap up by less than
        // one block: 4,505,143 blocks of 7 s are one second over a year.
        let c = clock(7_000, DEFAULT_RENT_CAP_SECONDS);
        assert_eq!(c.cap_blocks(), 4_505_143);
        let year = rent(0, units, None, Some(u64::MAX), 0, c).unwrap();
        assert_eq!(
            year.wei,
            (u128::from(units) * 31_536_001_000 * 6250).div_ceil(27)
        );
    }
    #[test]
    fn every_lifetime_at_or_beyond_the_cap_costs_the_cap() {
        let (units, height) = (3_086, 1_000);
        let cap = CLOCK.cap_blocks();
        let cost = |expiry| rent(0, units, None, Some(expiry), height, CLOCK).unwrap();
        let at_cap = cost(height + cap);
        assert_eq!(at_cap.extension_blocks, cap);
        for expiry in [height + cap + 1, height + 2 * cap, u64::MAX - 1, u64::MAX] {
            assert_eq!(cost(expiry), at_cap, "{expiry}");
        }
        // Shorter lifetimes are priced exactly as without a cap.
        let uncapped = clock(2_000, MAX_RENT_CAP_SECONDS);
        for lifetime in [1, 17_280, cap - 1] {
            assert_eq!(
                cost(height + lifetime),
                rent(0, units, None, Some(height + lifetime), height, uncapped).unwrap()
            );
            assert!(cost(height + lifetime).wei < at_cap.wei);
        }
        // A one-year cap of a 1 KiB entity: 0.0225278 GLM, the
        // 22,527,800 gas glm-rent-v2 charged at 1 gwei.
        assert_eq!(at_cap.wei, 22_527_800 * GWEI);
        assert_eq!(glm(at_cap.wei), "0.0225278");
    }
    #[test]
    fn extensions_pay_only_for_the_growth_of_the_capped_horizon() {
        // A 1,000-block cap: 1,000 s at one-second blocks.
        let c = clock(1_000, 1_000);
        let (u, cap) = (1_000, c.cap_blocks());
        assert_eq!(cap, 1_000);
        let extend = |old, new, height| rent(u, u, Some(old), Some(new), height, c).unwrap();
        // Half a cap, then permanent at once: the other half.
        assert_eq!(extend(500, u64::MAX, 0).extension_blocks, 500);
        // A full cap, then permanent ten blocks later: those ten blocks.
        assert_eq!(extend(1_000, u64::MAX, 10).extension_blocks, 10);
        // Already beyond the cap: permanent costs nothing more.
        assert_eq!(extend(2_000, u64::MAX, 1_000).wei, 0);
        assert_eq!(extend(5_000, 6_000, 10).wei, 0);
        // Near the cap from below: only the part that becomes chargeable.
        assert_eq!(extend(900, 1_100, 0).extension_blocks, 100);
        // A permanent entity whose paid horizon ran out long ago: extending it
        // again (to permanent, the only non-shortening choice) is free.
        assert_eq!(extend(u64::MAX, u64::MAX, 5 * cap).wei, 0);
        // Growth pays the capped remaining lifetime, permanent included.
        let grow = rent(1_000, 1_500, Some(u64::MAX), Some(u64::MAX), 7 * cap, c).unwrap();
        assert_eq!((grow.remaining_blocks, grow.extension_blocks), (cap, 0));
        assert_eq!(grow.wei, c.wei(500 * u128::from(cap)).unwrap());
        // Ownership transfer: same size, same expiry, no rent.
        assert_eq!(
            rent(u, u, Some(u64::MAX), Some(u64::MAX), 3, c)
                .unwrap()
                .wei,
            0
        );
    }

    /// An entity under a schedule of operations, with the reference model of
    /// what it must have paid: per storage-unit layer, the height the layer
    /// (last) appeared.
    #[derive(Clone)]
    struct Walk {
        clock: RentClock,
        height: u64,
        expiry: u64,
        layers: Vec<u64>,
        paid_wei: u128,
    }
    impl Walk {
        fn create(clock: RentClock, height: u64, size: u64, expiry: u64) -> Self {
            let quote = rent(0, size, None, Some(expiry), height, clock).unwrap();
            let cap = clock.cap_blocks();
            assert_eq!(
                quote.unit_blocks,
                u128::from(size) * u128::from(rent_horizon(expiry, height, cap) - height)
            );
            let walk = Self {
                clock,
                height,
                expiry,
                layers: vec![height; size as usize],
                paid_wei: quote.wei,
            };
            walk.check();
            walk
        }
        /// One operation `dt` blocks later leaving `size` units and
        /// `expiry`; `None` if the entity expired or the expiry shortens.
        fn step(&mut self, dt: u64, size: u64, expiry: u64) -> Option<()> {
            let h = self.height + dt;
            if h >= self.expiry || expiry < self.expiry {
                return None;
            }
            let before = self.layers.len() as u64;
            let quote = rent(before, size, Some(self.expiry), Some(expiry), h, self.clock).unwrap();
            // Independent per-layer model: an added layer pays from now to the
            // new horizon, a kept one the growth of the horizon.
            let cap = self.clock.cap_blocks();
            let (old, new) = (
                rent_horizon(self.expiry, h, cap),
                rent_horizon(expiry, h, cap),
            );
            let model: u128 = (1..=size)
                .map(|k| u128::from(if k > before { new - h } else { new - old }))
                .sum();
            assert_eq!(quote.unit_blocks, model);
            assert_eq!(quote.wei, self.clock.wei(model).unwrap());
            self.layers.truncate(size as usize);
            self.layers.resize(size as usize, h);
            self.height = h;
            self.expiry = expiry;
            self.paid_wei += quote.wei;
            self.check();
            Some(())
        }
        /// Every layer held now has paid at least `min(expiry − appeared,
        /// cap)` blocks, so permanence costs each one at least the cap; the
        /// per-operation rounding only adds.
        fn check(&self) {
            let cap = self.clock.cap_blocks();
            let owed: u128 = self
                .layers
                .iter()
                .map(|&g| u128::from((self.expiry - g).min(cap)))
                .sum();
            assert!(
                self.paid_wei >= self.clock.wei(owed).unwrap(),
                "paid {} for {owed} unit-blocks at {} (expiry {})",
                self.paid_wei,
                self.height,
                self.expiry
            );
        }
    }
    fn later(expiry: u64, by: u64) -> u64 {
        if by == u64::MAX {
            u64::MAX
        } else {
            expiry.saturating_add(by)
        }
    }

    /// Every create and two later operations of a small domain: time never
    /// runs backwards, sizes grow, shrink and stay, expiries stay, grow,
    /// cross the 8-block cap and become permanent. Rent matches the per-layer
    /// model exactly and never buys storage below the cap.
    #[test]
    fn no_schedule_buys_storage_below_the_cap_exhaustively() {
        let c = clock(2_000, 16);
        assert_eq!(c.cap_blocks(), 8);
        let steps: Vec<(u64, u64, u64)> = [0, 1, 4, 7, 9]
            .into_iter()
            .flat_map(|dt| {
                [1, 2, 3].into_iter().flat_map(move |size| {
                    [0, 1, 6, 9, u64::MAX]
                        .into_iter()
                        .map(move |by| (dt, size, by))
                })
            })
            .collect();
        let (mut sequences, mut applied) = (0, 0);
        for h0 in [0, 3] {
            for lifetime in [1, 3, 8, 9, 17, u64::MAX] {
                for size in [1, 2, 3] {
                    let created = Walk::create(c, h0, size, later(h0, lifetime));
                    for &(dt1, s1, by1) in &steps {
                        let mut first = created.clone();
                        let e1 = later(first.expiry, by1);
                        let one = first.step(dt1, s1, e1).is_some();
                        for &(dt2, s2, by2) in &steps {
                            let mut walk = first.clone();
                            let two = one && walk.step(dt2, s2, later(walk.expiry, by2)).is_some();
                            sequences += 1;
                            applied += usize::from(one) + usize::from(two);
                        }
                    }
                }
            }
        }
        assert_eq!(sequences, 2 * 6 * 3 * steps.len() * steps.len());
        assert!(applied > sequences, "{applied} of {sequences}");
    }

    /// Long random schedules (fixed seed) under several caps: the same
    /// model and lower bound as the exhaustive walk.
    #[test]
    fn random_schedules_never_buy_storage_below_the_cap() {
        let mut seed = 0x5eed_u64;
        let mut next = move |n: u64| {
            // splitmix64
            seed = seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = seed;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            (z ^ (z >> 31)) % n
        };
        let mut applied = 0;
        for (period, cap_seconds) in [(1_000, 1), (2_000, 16), (1_000, 15), (5_000, 3_600)] {
            let c = clock(period, cap_seconds);
            let cap = c.cap_blocks();
            for _ in 0..4_000 {
                let h0 = next(50);
                let lifetime = if next(4) == 0 {
                    u64::MAX
                } else {
                    1 + next(3 * cap)
                };
                let mut walk = Walk::create(c, h0, 1 + next(6), later(h0, lifetime));
                for _ in 0..8 {
                    let by = match next(5) {
                        0 => 0,
                        1 => u64::MAX,
                        _ => next(2 * cap + 2),
                    };
                    let expiry = later(walk.expiry, by);
                    if walk.step(next(cap + 3), 1 + next(6), expiry).is_some() {
                        applied += 1;
                    }
                }
            }
        }
        assert!(applied > 50_000, "{applied}");
    }
    #[test]
    fn horizons_saturate_instead_of_overflowing() {
        let cap = CLOCK.cap_blocks();
        assert_eq!(rent_horizon(u64::MAX, u64::MAX - 5, cap), u64::MAX);
        assert_eq!(rent_horizon(u64::MAX - 1, u64::MAX - 5, cap), u64::MAX - 1);
        assert_eq!(rent_horizon(u64::MAX, 0, u64::MAX), u64::MAX);
        // A create five blocks below the sentinel pays for those five blocks.
        let late = rent(0, 1_000, None, Some(u64::MAX), u64::MAX - 5, CLOCK).unwrap();
        assert_eq!(late.extension_blocks, 5);
        let near = rent(0, 1_000, None, Some(u64::MAX - 1), u64::MAX - 5, CLOCK).unwrap();
        assert_eq!(near.extension_blocks, 4);
    }
    #[test]
    fn quote_is_exact_and_schedule_identity_is_pinned() {
        assert_eq!(reference_fee_glm(50_000), "0.00005");
        assert_eq!(reference_fee_glm(1_000_000), "0.001");
        assert_eq!(reference_fee_glm(100_000_000), "0.1");
        assert_eq!(reference_fee_glm(u64::MAX), "18446744073.709551615");
        assert_eq!(glm(7_300_000_000_000), "0.0000073");
        assert!(validate_schedule(&schedule()));
        assert!(!validate_schedule(
            &schedule().with_price(OperationClass::RecordCreate, 1)
        ));
        assert!(validate_schedule(&PriceSchedule::FIXTURE));
    }
}
