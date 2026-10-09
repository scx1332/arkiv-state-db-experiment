//! The JSON form of every engine scalar, shared by the simulator, demo and
//! explorer APIs. Experimental wire contract, not the SDK JSON-RPC form.
//!
//! - `bool` is a JSON boolean; `str` is plain text.
//! - Integers (`i32`, `i64`, `i128`, `i256`, `u32`, `u64`, `u128`, `u256`)
//!   and the time types (`date32` days, `timestamp64` microseconds since the
//!   Unix epoch) are canonical decimal strings: no sign on zero or positive
//!   values, no leading zeros.
//! - Decimals are exact decimal strings at the type's fixed scale (`dec32`
//!   4 places, `dec64` 6, `dec128` and `dec` 18), with trailing fractional
//!   zeros trimmed on output; input may carry at most the scale's places.
//! - Floats (`f32`, `f64`) are strings: Rust's shortest round-trip decimal
//!   text, `inf` or `-inf`. NaN is rejected and `-0` reads as `0`.
//! - `bytes`, `bytes4`, `bytes8`, `bytes16`, `bytes32`, `addr` and `key` are
//!   lower-case `0x` hexadecimal of exactly the type's width.

use alloy_primitives::{I256, U256};
use arkiv_db::{Float32, Float64, TypeTag, Value};
use serde_json::{Value as Json, json};

use crate::demo::wire::hex;

/// The value's type name ([`TypeTag::name`]).
pub fn type_name(value: &Value) -> &'static str {
    value.tag().name()
}

/// Two's complement big-endian `bytes` as `(negative, magnitude)`.
fn signed_magnitude(bytes: [u8; 32]) -> (bool, U256) {
    let value = U256::from_be_bytes(bytes);
    if bytes[0] & 0x80 != 0 {
        (true, U256::ZERO.wrapping_sub(value))
    } else {
        (false, value)
    }
}

fn widen(value: i128) -> [u8; 32] {
    let mut out = if value < 0 { [0xff; 32] } else { [0; 32] };
    out[16..].copy_from_slice(&value.to_be_bytes());
    out
}

/// Exact decimal text of signed `units` at `scale` places.
pub fn scaled_text(units: [u8; 32], scale: u8) -> String {
    let (negative, magnitude) = signed_magnitude(units);
    let unit = U256::from(10u8).pow(U256::from(scale));
    let whole = magnitude / unit;
    let frac = format!("{:0width$}", magnitude % unit, width = usize::from(scale));
    let frac = frac.trim_end_matches('0');
    let sign = if negative { "-" } else { "" };
    if frac.is_empty() {
        format!("{sign}{whole}")
    } else {
        format!("{sign}{whole}.{frac}")
    }
}

/// Parses exact decimal text into signed units at `scale` places that fit
/// `bits` bits of two's complement; `None` for malformed or out-of-range
/// text or more than `scale` fractional digits.
pub fn parse_scaled(input: &str, scale: u8, bits: usize) -> Option<[u8; 32]> {
    let (negative, body) = match input.as_bytes().first() {
        Some(b'-') => (true, &input[1..]),
        _ => (false, input),
    };
    let (whole, fraction) = body.split_once('.').unwrap_or((body, ""));
    if whole.is_empty()
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || fraction.len() > usize::from(scale)
        || !fraction.bytes().all(|b| b.is_ascii_digit())
        || (body.contains('.') && fraction.is_empty())
    {
        return None;
    }
    let digits = format!(
        "{whole}{fraction}{}",
        "0".repeat(usize::from(scale) - fraction.len())
    );
    let magnitude = U256::from_str_radix(&digits, 10).ok()?;
    let limit = U256::from(1u8) << (bits - 1);
    if magnitude > limit || (!negative && magnitude == limit) {
        return None;
    }
    Some(
        if negative {
            U256::ZERO.wrapping_sub(magnitude)
        } else {
            magnitude
        }
        .to_be_bytes(),
    )
}

/// The JSON form of `value`.
pub fn to_json(value: &Value) -> Json {
    match value {
        Value::Bool(flag) => json!(flag),
        Value::Str(text) => json!(text.as_ref()),
        Value::I64(n) | Value::Timestamp64(n) => json!(n.to_string()),
        Value::U64(n) => json!(n.to_string()),
        Value::I32(n) | Value::Date32(n) => json!(n.to_string()),
        Value::U32(n) => json!(n.to_string()),
        Value::U128(n) => json!(n.to_string()),
        Value::I128(n) => json!(n.to_string()),
        Value::U256(n) => json!(U256::from_be_bytes(*n).to_string()),
        Value::I256(n) => json!(I256::from_be_bytes(*n).to_string()),
        Value::Dec32(n) => json!(scaled_text(widen(i128::from(*n)), 4)),
        Value::Dec64(n) => json!(scaled_text(widen(i128::from(*n)), 6)),
        Value::Dec128(n) => json!(scaled_text(widen(*n), 18)),
        Value::Dec(n) => json!(scaled_text(*n, 18)),
        Value::F32(n) => json!(n.get().to_string()),
        Value::F64(n) => json!(n.get().to_string()),
        Value::Bytes(bytes) => json!(hex(bytes)),
        Value::Bytes4(bytes) => json!(hex(bytes)),
        Value::Bytes8(bytes) => json!(hex(bytes)),
        Value::Bytes16(bytes) => json!(hex(bytes)),
        Value::Bytes32(bytes) | Value::Key(bytes) => json!(hex(bytes)),
        Value::Addr(bytes) => json!(hex(bytes)),
    }
}

/// Lower-case `0x` hexadecimal of any even length.
fn hex_bytes(text: &str) -> Option<Vec<u8>> {
    let digits = text.strip_prefix("0x")?;
    if digits.len() % 2 != 0 {
        return None;
    }
    let nibble = |b: u8| match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    };
    digits
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| Some(nibble(pair[0])? * 16 + nibble(pair[1])?))
        .collect()
}

fn exact_hex<const N: usize>(text: &str) -> Option<[u8; N]> {
    hex_bytes(text)?.try_into().ok()
}

/// A canonical decimal integer: parses and prints back identically.
fn canonical<T: std::str::FromStr + ToString>(text: &str) -> Option<T> {
    let value: T = text.parse().ok()?;
    (value.to_string() == text).then_some(value)
}

/// Parses the JSON form of a value of type `type_name`. `None` for an
/// unknown type name or a value that is not that type's JSON form.
/// `bytes` parses (any length) even though it is never indexable.
pub fn from_json(type_name: &str, json: &Json) -> Option<Value> {
    let tag = TypeTag::from_name(type_name)?;
    if tag == TypeTag::Bool {
        return json.as_bool().map(Value::Bool);
    }
    let text = json.as_str()?;
    Some(match tag {
        TypeTag::Bool => unreachable!("handled above"),
        TypeTag::Str => Value::str(text),
        TypeTag::Bytes => Value::bytes(&hex_bytes(text)?),
        TypeTag::I32 => Value::I32(canonical(text)?),
        TypeTag::I64 => Value::I64(canonical(text)?),
        TypeTag::I128 => Value::I128(canonical(text)?),
        TypeTag::U32 => Value::U32(canonical(text)?),
        TypeTag::U64 => Value::U64(canonical(text)?),
        TypeTag::U128 => Value::U128(canonical(text)?),
        TypeTag::Date32 => Value::Date32(canonical(text)?),
        TypeTag::Timestamp64 => Value::Timestamp64(canonical(text)?),
        TypeTag::U256 => {
            let n: U256 = text.parse().ok()?;
            if n.to_string() != text {
                return None;
            }
            Value::U256(n.to_be_bytes())
        }
        TypeTag::I256 => {
            let n: I256 = text.parse().ok()?;
            if n.to_string() != text {
                return None;
            }
            Value::I256(n.to_be_bytes())
        }
        TypeTag::Dec32 => {
            let units = parse_scaled(text, 4, 32)?;
            Value::Dec32(i32::from_be_bytes(units[28..].try_into().ok()?))
        }
        TypeTag::Dec64 => {
            let units = parse_scaled(text, 6, 64)?;
            Value::Dec64(i64::from_be_bytes(units[24..].try_into().ok()?))
        }
        TypeTag::Dec128 => {
            let units = parse_scaled(text, 18, 128)?;
            Value::Dec128(i128::from_be_bytes(units[16..].try_into().ok()?))
        }
        TypeTag::Dec => Value::Dec(parse_scaled(text, 18, 256)?),
        TypeTag::F32 => Value::F32(Float32::new(text.parse().ok()?)?),
        TypeTag::F64 => Value::F64(Float64::new(text.parse().ok()?)?),
        TypeTag::Bytes4 => Value::Bytes4(exact_hex(text)?),
        TypeTag::Bytes8 => Value::Bytes8(exact_hex(text)?),
        TypeTag::Bytes16 => Value::Bytes16(exact_hex(text)?),
        TypeTag::Bytes32 => Value::Bytes32(exact_hex(text)?),
        TypeTag::Key => Value::Key(exact_hex(text)?),
        TypeTag::Addr => Value::Addr(exact_hex(text)?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn samples() -> Vec<Value> {
        let mut min256 = [0; 32];
        min256[0] = 0x80;
        vec![
            Value::Bool(false),
            Value::str("héllo"),
            Value::bytes(&[0, 0xab]),
            Value::I32(i32::MIN),
            Value::I64(-1),
            Value::I128(i128::MIN),
            Value::I256(min256),
            Value::U32(u32::MAX),
            Value::U64(u64::MAX),
            Value::U128(u128::MAX),
            Value::U256([0xff; 32]),
            Value::Dec32(i32::MIN),
            Value::Dec32(123_450),
            Value::Dec64(-1),
            Value::Dec128(i128::MAX),
            Value::Dec(min256),
            Value::f32(-1.5).unwrap(),
            Value::f32(f32::MIN_POSITIVE).unwrap(),
            Value::f64(f64::NEG_INFINITY).unwrap(),
            Value::f64(0.1).unwrap(),
            Value::Date32(-719_162),
            Value::Timestamp64(i64::MAX),
            Value::Bytes4([1, 2, 3, 4]),
            Value::Bytes8([8; 8]),
            Value::Bytes16([0xf0; 16]),
            Value::Bytes32([0x32; 32]),
            Value::Addr([0x20; 20]),
            Value::Key([0x4b; 32]),
        ]
    }

    #[test]
    fn every_type_round_trips_through_json() {
        let mut seen = std::collections::BTreeSet::new();
        for value in samples() {
            seen.insert(value.tag());
            let json = to_json(&value);
            assert_eq!(
                from_json(type_name(&value), &json),
                Some(value.clone()),
                "{value:?} as {json}"
            );
        }
        assert_eq!(seen.len(), TypeTag::ALL.len());
    }

    #[test]
    fn json_forms_are_exact() {
        assert_eq!(to_json(&Value::Dec32(123_450)), json!("12.345"));
        assert_eq!(to_json(&Value::Dec64(-1)), json!("-0.000001"));
        assert_eq!(to_json(&Value::Dec32(i32::MIN)), json!("-214748.3648"));
        assert_eq!(to_json(&Value::f64(-0.0).unwrap()), json!("0"));
        assert_eq!(to_json(&Value::f32(f32::INFINITY).unwrap()), json!("inf"));
        assert_eq!(to_json(&Value::I256([0xff; 32])), json!("-1"));
        assert_eq!(
            to_json(&Value::Bytes4([0, 1, 0xa, 0xff])),
            json!("0x00010aff")
        );
        for (name, text) in [
            ("f64", "NaN"),
            ("dec32", "0.00001"),
            ("dec32", "214748.3648"),
            ("dec64", "1."),
            ("i32", "+1"),
            ("u32", "-1"),
            ("u32", "01"),
            ("i128", "-0"),
            ("bytes4", "0x0102"),
            ("bytes4", "0x0102030G"),
            ("addr", "0X2020202020202020202020202020202020202020"),
            ("date32", "2147483648"),
        ] {
            assert_eq!(from_json(name, &json!(text)), None, "{name} {text}");
        }
        assert_eq!(from_json("nope", &json!("1")), None);
        assert_eq!(from_json("bool", &json!("true")), None);
        assert_eq!(
            from_json("dec32", &json!("-214748.3648")),
            Some(Value::Dec32(i32::MIN))
        );
        assert_eq!(from_json("f32", &json!("-0")), Value::f32(0.0));
    }
}
