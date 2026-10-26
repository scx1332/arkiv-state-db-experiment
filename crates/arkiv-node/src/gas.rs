//! Opt-in GLM gas profile. Logical occupancy, never physical trie size, determines rent.
//! Prices are an experimental capacity policy, not a measured hardware cost or a token debit.
use arkiv_db::{Cell, CellKind, CellName, PriceSchedule};

pub const SCHEDULE_ID: u32 = 0x474c4d01;
pub const TX_INTRINSIC_GAS: u64 = 21_000;
pub const DEFAULT_BLOCK_GAS_LIMIT: u64 = 100_000_000;
pub const REFERENCE_GAS_PRICE_WEI: u64 = 1_000_000_000;
pub const RENT_PERIOD_BLOCKS: u64 = 17_280;
pub const RENT_GAS_PER_UNIT_PERIOD: u64 = 8;
pub const ENTITY_OVERHEAD_UNITS: u64 = 512;
pub const INDEX_OVERHEAD_UNITS: u64 = 128;
/// The default rent cap: 365 days ([`ChainParams::max_rent_lifetime`] at the
/// chain's block period; see [`rent_cap_blocks`]).
///
/// [`ChainParams::max_rent_lifetime`]: crate::ChainParams::max_rent_lifetime
pub const DEFAULT_RENT_CAP_SECONDS: u64 = 365 * 24 * 60 * 60;
/// [`DEFAULT_RENT_CAP_SECONDS`] at the SDK's nominal 2-second block period.
pub const DEFAULT_RENT_CAP_BLOCKS: u64 = 15_768_000;

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
    pub gas: u64,
}
/// The rent horizon of `expiry` at `height`: rent is never charged past
/// `height + cap`, so every expiry at least `cap` blocks away, the permanent
/// `u64::MAX` included, has the same horizon. Saturates instead of
/// overflowing near `u64::MAX`.
pub const fn rent_horizon(expiry: u64, height: u64, cap: u64) -> u64 {
    let limit = height.saturating_add(cap);
    if expiry < limit { expiry } else { limit }
}
/// Rent of one operation at `height` with the chain's rent cap `cap`
/// (`ChainParams::max_rent_lifetime`, at least 1). Every lifetime is charged
/// through its [`rent_horizon`] only, recomputed from the current height:
///
/// - a create prepays `after × (horizon(new) − height)`;
/// - growth prepays the positive size increase for the old remaining capped
///   lifetime, `growth × (horizon(old) − height)`;
/// - an extension prepays the new size for the growth of the capped horizon,
///   `after × (horizon(new) − horizon(old))`, which is zero when the old expiry
///   already reaches the cap.
///
/// Lifetimes at or beyond the cap, permanent included, cost the same as the
/// cap; shorter ones are priced exactly as before. Delete and shrink give no
/// refunds. Rounding occurs once per operation, upward, never per byte or per
/// block. All arithmetic is checked.
pub fn rent(
    before_units: u64,
    after_units: u64,
    old_expiry: Option<u64>,
    new_expiry: Option<u64>,
    height: u64,
    cap: u64,
) -> Result<RentQuote, GasError> {
    let Some(new) = new_expiry else {
        return Ok(RentQuote::default());
    };
    if cap == 0 || new <= height || old_expiry.is_some_and(|old| old <= height || old > new) {
        return Err(GasError::InvalidLifetime);
    }
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
    let numerator = unit_blocks
        .checked_mul(u128::from(RENT_GAS_PER_UNIT_PERIOD))
        .ok_or(GasError::Overflow)?;
    let gas = u64::try_from(numerator.div_ceil(u128::from(RENT_PERIOD_BLOCKS)))
        .map_err(|_| GasError::Overflow)?;
    Ok(RentQuote {
        growth_units,
        extension_blocks,
        remaining_blocks,
        gas,
    })
}
/// Exact reference cost in GLM base units. This is a quote, not a balance transfer.
pub fn reference_fee_wei(gas: u64) -> u128 {
    u128::from(gas) * u128::from(REFERENCE_GAS_PRICE_WEI)
}
pub fn reference_fee_glm(gas: u64) -> String {
    let wei = reference_fee_wei(gas);
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
    /// A cap longer than every lifetime of the unchanged cases.
    const CAP: u64 = DEFAULT_RENT_CAP_BLOCKS;
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
        assert_eq!(
            rent(0, units.total, None, Some(17_281), 1, CAP)
                .unwrap()
                .gas,
            13_480
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
        assert_eq!(
            rent(0, 2000, None, Some(17_290), 10, CAP).unwrap().gas,
            16_000
        );
        assert_eq!(
            rent(2000, 2500, Some(17_290), Some(17_290), 10, CAP)
                .unwrap()
                .gas,
            4000
        );
        assert_eq!(
            rent(2000, 1000, Some(17_290), Some(17_290), 10, CAP)
                .unwrap()
                .gas,
            0
        );
        assert_eq!(
            rent(2000, 2000, Some(17_290), Some(34_570), 10, CAP)
                .unwrap()
                .gas,
            16_000
        );
        assert_eq!(rent(2000, 0, Some(17_290), None, 10, CAP).unwrap().gas, 0);
        assert_eq!(
            rent(2000, 2500, Some(17_290), Some(34_570), 10, CAP)
                .unwrap()
                .gas,
            24_000
        );
    }
    #[test]
    fn boundaries_round_once_and_reject_invalid_or_overflow() {
        assert_eq!(rent(0, 1, None, Some(2), 1, CAP).unwrap().gas, 1);
        assert_eq!(rent(0, 2160, None, Some(2), 1, CAP).unwrap().gas, 1);
        assert_eq!(rent(0, 2161, None, Some(2), 1, CAP).unwrap().gas, 2);
        assert_eq!(
            rent(0, 1, None, Some(1), 1, CAP),
            Err(GasError::InvalidLifetime)
        );
        assert_eq!(
            rent(1, 1, Some(10), Some(9), 1, CAP),
            Err(GasError::InvalidLifetime)
        );
        assert_eq!(
            rent(0, 1, None, Some(10), 1, 0),
            Err(GasError::InvalidLifetime)
        );
        // An uncapped huge lifetime overflows the unit-block product.
        assert_eq!(
            rent(0, u64::MAX, None, Some(u64::MAX - 1), 0, u64::MAX),
            Err(GasError::Overflow)
        );
        // A huge entity at the default cap does not fit u64 gas.
        assert_eq!(
            rent(0, u64::MAX, None, Some(u64::MAX), 0, CAP),
            Err(GasError::Overflow)
        );
    }
    #[test]
    fn the_default_cap_is_a_year_at_the_block_period() {
        assert_eq!(DEFAULT_RENT_CAP_SECONDS, 31_536_000);
        assert_eq!(
            rent_cap_blocks(DEFAULT_RENT_CAP_SECONDS, 2_000),
            Some(DEFAULT_RENT_CAP_BLOCKS)
        );
        assert_eq!(DEFAULT_RENT_CAP_BLOCKS, 15_768_000);
        assert_eq!(
            rent_cap_blocks(DEFAULT_RENT_CAP_SECONDS, 1_000),
            Some(31_536_000)
        );
        assert_eq!(
            rent_cap_blocks(DEFAULT_RENT_CAP_SECONDS, 5_000),
            Some(6_307_200)
        );
        // A period that does not divide a year rounds the cap up.
        assert_eq!(
            rent_cap_blocks(DEFAULT_RENT_CAP_SECONDS, 7_000),
            Some(4_505_143)
        );
        assert_eq!(rent_cap_blocks(1, 3_000), Some(1));
        assert_eq!(rent_cap_blocks(DEFAULT_RENT_CAP_SECONDS, 0), None);
        assert_eq!(rent_cap_blocks(u64::MAX, 1_000), None);
    }
    #[test]
    fn every_lifetime_at_or_beyond_the_cap_costs_the_cap() {
        let (units, height, cap) = (3_086, 1_000, 15_768_000);
        let cost = |expiry| rent(0, units, None, Some(expiry), height, cap).unwrap();
        let at_cap = cost(height + cap);
        assert_eq!(at_cap.extension_blocks, cap);
        assert_eq!(at_cap.gas, (units * cap * 8).div_ceil(RENT_PERIOD_BLOCKS));
        for expiry in [height + cap + 1, height + 2 * cap, u64::MAX - 1, u64::MAX] {
            assert_eq!(cost(expiry), at_cap, "{expiry}");
        }
        // Shorter lifetimes are priced exactly as without a cap.
        for lifetime in [1, 17_280, cap - 1] {
            assert_eq!(
                cost(height + lifetime),
                rent(0, units, None, Some(height + lifetime), height, u64::MAX).unwrap()
            );
            assert!(cost(height + lifetime).gas < at_cap.gas);
        }
        // A one-year cap of a 1 KiB entity fits one transaction.
        assert_eq!(at_cap.gas, 22_527_800);
    }
    #[test]
    fn extensions_pay_only_for_the_growth_of_the_capped_horizon() {
        let (u, cap) = (1_000, 1_000);
        let extend = |old, new, height| rent(u, u, Some(old), Some(new), height, cap).unwrap();
        // Half a cap, then permanent at once: the other half.
        assert_eq!(extend(500, u64::MAX, 0).extension_blocks, 500);
        // A full cap, then permanent ten blocks later: those ten blocks.
        assert_eq!(extend(1_000, u64::MAX, 10).extension_blocks, 10);
        // Already beyond the cap: permanent costs nothing more.
        assert_eq!(extend(2_000, u64::MAX, 1_000).gas, 0);
        assert_eq!(extend(5_000, 6_000, 10).gas, 0);
        // Near the cap from below: only the part that becomes chargeable.
        assert_eq!(extend(900, 1_100, 0).extension_blocks, 100);
        // A permanent entity whose paid horizon ran out long ago: extending it
        // again (to permanent, the only non-shortening choice) is free.
        assert_eq!(extend(u64::MAX, u64::MAX, 5 * cap).gas, 0);
        // Growth pays the capped remaining lifetime, permanent included.
        let grow = rent(1_000, 1_500, Some(u64::MAX), Some(u64::MAX), 7 * cap, cap).unwrap();
        assert_eq!((grow.remaining_blocks, grow.extension_blocks), (cap, 0));
        assert_eq!(grow.gas, (500 * cap * 8).div_ceil(RENT_PERIOD_BLOCKS));
        // Ownership transfer: same size, same expiry, no rent.
        assert_eq!(
            rent(u, u, Some(u64::MAX), Some(u64::MAX), 3, cap)
                .unwrap()
                .gas,
            0
        );
    }
    /// No sequence of a create and extensions reaches an expiry for less
    /// than `min(final − created, cap)` blocks: permanence costs the cap.
    #[test]
    fn no_schedule_buys_permanence_below_the_cap() {
        const CAP: u64 = 8;
        let expiries = [1, 2, 3, 5, 8, 9, 12, 17, u64::MAX];
        let mut checked = 0;
        for &e0 in &expiries {
            for &(h1, e1) in &[
                (0u64, 0u64),
                (1, 3),
                (2, 9),
                (4, 12),
                (7, u64::MAX),
                (9, 17),
            ] {
                for &(h2, e2) in &[
                    (0u64, 0u64),
                    (3, 9),
                    (8, u64::MAX),
                    (11, u64::MAX),
                    (16, 17),
                ] {
                    let mut expiry = e0;
                    let mut paid = rent(0, 1, None, Some(e0), 0, CAP).unwrap().extension_blocks;
                    for (h, e) in [(h1, e1), (h2, e2)] {
                        // Skip empty steps and ones on an expired or shortened entity.
                        if e == 0 || h >= expiry || e < expiry {
                            continue;
                        }
                        paid += rent(1, 1, Some(expiry), Some(e), h, CAP)
                            .unwrap()
                            .extension_blocks;
                        expiry = e;
                    }
                    assert!(
                        paid >= (expiry).min(CAP),
                        "{e0} {h1}/{e1} {h2}/{e2}: {paid}"
                    );
                    if expiry == u64::MAX {
                        assert!(paid >= CAP);
                    }
                    checked += 1;
                }
            }
        }
        assert_eq!(checked, expiries.len() * 6 * 5);
    }
    #[test]
    fn horizons_saturate_instead_of_overflowing() {
        let cap = DEFAULT_RENT_CAP_BLOCKS;
        assert_eq!(rent_horizon(u64::MAX, u64::MAX - 5, cap), u64::MAX);
        assert_eq!(rent_horizon(u64::MAX - 1, u64::MAX - 5, cap), u64::MAX - 1);
        assert_eq!(rent_horizon(u64::MAX, 0, u64::MAX), u64::MAX);
        // A create five blocks below the sentinel pays for those five blocks.
        let late = rent(0, 1_000, None, Some(u64::MAX), u64::MAX - 5, cap).unwrap();
        assert_eq!(late.extension_blocks, 5);
        let near = rent(0, 1_000, None, Some(u64::MAX - 1), u64::MAX - 5, cap).unwrap();
        assert_eq!(near.extension_blocks, 4);
    }
    #[test]
    fn quote_is_exact_and_schedule_identity_is_pinned() {
        assert_eq!(reference_fee_glm(50_000), "0.00005");
        assert_eq!(reference_fee_glm(1_000_000), "0.001");
        assert_eq!(reference_fee_glm(100_000_000), "0.1");
        assert_eq!(reference_fee_glm(u64::MAX), "18446744073.709551615");
        assert!(validate_schedule(&schedule()));
        assert!(!validate_schedule(
            &schedule().with_price(OperationClass::RecordCreate, 1)
        ));
        assert!(validate_schedule(&PriceSchedule::FIXTURE));
    }
}
