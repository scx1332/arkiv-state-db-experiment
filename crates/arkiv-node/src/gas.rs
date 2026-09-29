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
    PermanentUnsupported,
    InvalidLifetime,
}
impl std::fmt::Display for GasError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Overflow => "gas arithmetic overflow",
            Self::PermanentUnsupported => "GLM gas v1 requires a finite entity lifetime",
            Self::InvalidLifetime => "invalid or shortened entity lifetime",
        })
    }
}
impl std::error::Error for GasError {}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StorageUnits {
    pub entity_overhead: u64,
    pub payload_bytes: u64,
    pub attribute_bytes: u64,
    pub index_overhead: u64,
    pub total: u64,
}
fn add(a: u64, b: u64) -> Result<u64, GasError> {
    a.checked_add(b).ok_or(GasError::Overflow)
}
/// Every committed system and user cell is counted. The payload's name/framing
/// is covered by entity overhead; other cells count name, value and two tag bytes.
/// Indexed cells reserve a value and presence posting together, independent of sharing.
pub fn storage_units(cells: &[(CellName, Cell)]) -> Result<StorageUnits, GasError> {
    let mut result = StorageUnits {
        entity_overhead: ENTITY_OVERHEAD_UNITS,
        ..Default::default()
    };
    for (name, cell) in cells {
        let bytes = u64::try_from(cell.value.encoded_len()).map_err(|_| GasError::Overflow)?;
        if name.as_str() == "$payload" {
            result.payload_bytes = add(result.payload_bytes, bytes)?;
        } else {
            result.attribute_bytes = add(
                result.attribute_bytes,
                add(add(name.len() as u64, bytes)?, 2)?,
            )?;
        }
        if cell.kind == CellKind::Attribute {
            result.index_overhead = add(result.index_overhead, INDEX_OVERHEAD_UNITS)?;
        }
    }
    result.total = add(
        add(result.entity_overhead, result.payload_bytes)?,
        add(result.attribute_bytes, result.index_overhead)?,
    )?;
    Ok(result)
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RentQuote {
    pub growth_units: u64,
    pub extension_blocks: u64,
    pub remaining_blocks: u64,
    pub gas: u64,
}
/// A create prepays its lifetime; growth prepays positive additional occupancy
/// for the old remaining lifetime; extension prepays the new occupancy for the
/// additional blocks. Delete and shrink give no refunds. Rounding occurs once per
/// operation, upward, never per byte or per block. MAX is the permanent sentinel.
pub fn rent(
    before_units: u64,
    after_units: u64,
    old_expiry: Option<u64>,
    new_expiry: Option<u64>,
    height: u64,
) -> Result<RentQuote, GasError> {
    let Some(new) = new_expiry else {
        return Ok(RentQuote::default());
    };
    if new == u64::MAX {
        return Err(GasError::PermanentUnsupported);
    }
    if new <= height || old_expiry.is_some_and(|old| old <= height || old > new) {
        return Err(GasError::InvalidLifetime);
    }
    let old = old_expiry.unwrap_or(height);
    let growth_units = if old_expiry.is_none() {
        0
    } else {
        after_units.saturating_sub(before_units)
    };
    let remaining_blocks = old - height;
    let extension_blocks = new - old;
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
    #[test]
    fn logical_size_is_content_and_kind_based() {
        let cells = vec![
            (
                CellName::new("$payload").unwrap(),
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
                payload_bytes: 1024,
                attribute_bytes: 15,
                index_overhead: 128,
                total: 1679
            }
        );
        assert_eq!(
            rent(0, units.total, None, Some(17_281), 1).unwrap().gas,
            13_432
        );
    }
    #[test]
    fn lifecycle_charges_growth_and_extension_without_refunds() {
        assert_eq!(rent(0, 2000, None, Some(17_290), 10).unwrap().gas, 16_000);
        assert_eq!(
            rent(2000, 2500, Some(17_290), Some(17_290), 10)
                .unwrap()
                .gas,
            4000
        );
        assert_eq!(
            rent(2000, 1000, Some(17_290), Some(17_290), 10)
                .unwrap()
                .gas,
            0
        );
        assert_eq!(
            rent(2000, 2000, Some(17_290), Some(34_570), 10)
                .unwrap()
                .gas,
            16_000
        );
        assert_eq!(rent(2000, 0, Some(17_290), None, 10).unwrap().gas, 0);
        assert_eq!(
            rent(2000, 2500, Some(17_290), Some(34_570), 10)
                .unwrap()
                .gas,
            24_000
        );
    }
    #[test]
    fn boundaries_round_once_and_reject_permanent_or_overflow() {
        assert_eq!(rent(0, 1, None, Some(2), 1).unwrap().gas, 1);
        assert_eq!(rent(0, 2160, None, Some(2), 1).unwrap().gas, 1);
        assert_eq!(rent(0, 2161, None, Some(2), 1).unwrap().gas, 2);
        assert_eq!(
            rent(0, 1, None, Some(u64::MAX), 1),
            Err(GasError::PermanentUnsupported)
        );
        assert_eq!(rent(0, 1, None, Some(1), 1), Err(GasError::InvalidLifetime));
        assert_eq!(
            rent(1, 1, Some(10), Some(9), 1),
            Err(GasError::InvalidLifetime)
        );
        assert_eq!(
            rent(0, u64::MAX, None, Some(u64::MAX - 1), 0),
            Err(GasError::Overflow)
        );
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
