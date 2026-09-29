//! Public adapter: SDK IDs are deliberately independent of engine tags.
use super::ArkivFault;
use alloy_primitives::U256;
use arkiv_db::{Cell, CellKind, CellName, Row, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ArkivTypeId {
    Bool = 1,
    I32 = 2,
    U64 = 3,
    U256 = 4,
    Dec = 5,
    Bytes32 = 6,
    Bytes = 7,
    Str = 8,
    Addr = 9,
    Key = 10,
}
impl ArkivTypeId {
    pub fn of(value: &Value) -> Result<Self, ArkivFault> {
        Ok(match value {
            Value::Bool(_) => Self::Bool,
            Value::I32(_) => Self::I32,
            Value::U64(_) => Self::U64,
            Value::U256(_) => Self::U256,
            Value::Dec(_) => Self::Dec,
            Value::Bytes32(_) => Self::Bytes32,
            Value::Bytes(_) => Self::Bytes,
            Value::Str(_) => Self::Str,
            Value::Addr(_) => Self::Addr,
            Value::Key(_) => Self::Key,
            Value::I64(_) => return Err(ArkivFault::Value),
        })
    }
}
pub fn protocol_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name.as_bytes()[0].is_ascii_alphabetic()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}
pub fn sdk_safe_name(name: &str) -> bool {
    protocol_name(name)
        && !name.contains("--")
        && ![
            "and",
            "or",
            "not",
            "true",
            "false",
            "startswith",
            "exists",
            "typeof",
            "bool",
            "i32",
            "u64",
            "u256",
            "dec",
            "bytes32",
            "bytes",
            "str",
            "addr",
            "key",
        ]
        .contains(&name.to_ascii_lowercase().as_str())
}
pub fn validate_value(value: &Value) -> Result<(), ArkivFault> {
    ArkivTypeId::of(value)?;
    match value {
        Value::Str(s) if s.len() > 128 || s.chars().any(|c| c <= '\u{1f}' || c == '\u{7f}') => {
            Err(ArkivFault::Value)
        }
        Value::Bytes(b) if b.len() > 128 * 1024 => Err(ArkivFault::Value),
        _ => Ok(()),
    }
}
pub fn user_cell(name: &CellName, value: &Value) -> Result<Cell, ArkivFault> {
    validate_value(value)?;
    match name.as_str() {
        "$payload" if matches!(value, Value::Bytes(_)) => Ok(Cell::field(value.clone())),
        "$contentType" if matches!(value, Value::Str(_)) => Ok(Cell::attribute(value.clone())),
        n if sdk_safe_name(n) && !matches!(value, Value::Bytes(_)) => {
            Ok(Cell::attribute(value.clone()))
        }
        _ => Err(ArkivFault::Name),
    }
}
pub fn validate_remove(name: &CellName) -> Result<(), ArkivFault> {
    if sdk_safe_name(name.as_str()) || matches!(name.as_str(), "$payload" | "$contentType") {
        Ok(())
    } else {
        Err(ArkivFault::Name)
    }
}
/// Parse decimal scaled units without floating point or rounding.
pub fn parse_decimal(input: &str) -> Result<[u8; 32], ArkivFault> {
    let (negative, s) = match input.as_bytes().first() {
        Some(b'-') => (true, &input[1..]),
        Some(b'+') => (false, &input[1..]),
        _ => (false, input),
    };
    let (whole, fraction) = s.split_once('.').map_or((s, ""), |(w, f)| (w, f));
    if whole.is_empty()
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || fraction.len() > 18
        || !fraction.bytes().all(|b| b.is_ascii_digit())
        || (s.contains('.') && fraction.is_empty())
    {
        return Err(ArkivFault::Value);
    }
    let digits = format!("{whole}{fraction}{}", "0".repeat(18 - fraction.len()));
    let magnitude = U256::from_str_radix(&digits, 10).map_err(|_| ArkivFault::Value)?;
    let sign = U256::from(1u8) << 255;
    if magnitude > sign || (!negative && magnitude == sign) {
        return Err(ArkivFault::Value);
    }
    Ok(if negative {
        U256::ZERO.wrapping_sub(magnitude)
    } else {
        magnitude
    }
    .to_be_bytes())
}
pub fn format_decimal(bytes: &[u8; 32]) -> String {
    let value = U256::from_be_bytes(*bytes);
    let negative = bytes[0] & 128 != 0;
    let magnitude = if negative {
        U256::ZERO.wrapping_sub(value)
    } else {
        value
    };
    let unit = U256::from(1_000_000_000_000_000_000u64);
    let whole = magnitude / unit;
    let frac = format!("{:018}", magnitude % unit);
    let frac = frac.trim_end_matches('0');
    let sign = if negative { "-" } else { "" };
    if frac.is_empty() {
        format!("{sign}{whole}")
    } else {
        format!("{sign}{whole}.{frac}")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArkivEntityProjection {
    pub key: [u8; 32],
    pub creator: [u8; 20],
    pub owner: [u8; 20],
    pub created_at: u64,
    pub updated_at: u64,
    pub expires_at: u64,
    pub creation_flags: u8,
    pub content_type: String,
    pub payload: Vec<u8>,
    pub attributes: Vec<(CellName, Value)>,
}
pub fn project(row: &Row) -> Result<ArkivEntityProjection, ArkivFault> {
    let structural: [u8; 32] = row
        .key
        .as_ref()
        .ok_or(ArkivFault::Corrupt)?
        .as_bytes()
        .try_into()
        .map_err(|_| ArkivFault::Corrupt)?;
    if row.cells.windows(2).any(|w| w[0].0 >= w[1].0) {
        return Err(ArkivFault::Corrupt);
    }
    let mut seen = std::collections::BTreeSet::new();
    for (name, cell) in &row.cells {
        if !seen.insert(name) {
            return Err(ArkivFault::Corrupt);
        }
        validate_value(&cell.value).map_err(|_| ArkivFault::Corrupt)?;
        let expected = match name.as_str() {
            "$key" => matches!(cell.value,Value::Key(k) if k==structural),
            "$creator" | "$owner" => matches!(cell.value, Value::Addr(_)),
            "$createdAt" | "$updatedAt" | "$expiresAt" => matches!(cell.value, Value::U64(_)),
            "$creationFlags" => matches!(cell.value,Value::U64(n) if n<=3),
            "$contentType" => matches!(cell.value, Value::Str(_)),
            "$payload" => matches!(cell.value, Value::Bytes(_)),
            n => protocol_name(n) && !matches!(cell.value, Value::Bytes(_)),
        };
        let kind = if matches!(name.as_str(), "$payload" | "$creationFlags") {
            CellKind::Field
        } else {
            CellKind::Attribute
        };
        if !expected || cell.kind != kind {
            return Err(ArkivFault::Corrupt);
        }
    }
    let get = |n: &str| {
        row.cells
            .iter()
            .find(|(name, _)| name.as_str() == n)
            .map(|(_, c)| &c.value)
            .ok_or(ArkivFault::Corrupt)
    };
    let number = |n| match get(n)? {
        Value::U64(v) => Ok(*v),
        _ => Err(ArkivFault::Corrupt),
    };
    let address = |n| match get(n)? {
        Value::Addr(v) => Ok(*v),
        _ => Err(ArkivFault::Corrupt),
    };
    get("$key")?;
    let result = ArkivEntityProjection {
        key: structural,
        creator: address("$creator")?,
        owner: address("$owner")?,
        created_at: number("$createdAt")?,
        updated_at: number("$updatedAt")?,
        expires_at: number("$expiresAt")?,
        creation_flags: number("$creationFlags")? as u8,
        content_type: match get("$contentType")? {
            Value::Str(s) => s.to_string(),
            _ => return Err(ArkivFault::Corrupt),
        },
        payload: match get("$payload")? {
            Value::Bytes(b) => b.to_vec(),
            _ => return Err(ArkivFault::Corrupt),
        },
        attributes: row
            .cells
            .iter()
            .filter(|(n, _)| !n.as_str().starts_with('$'))
            .map(|(n, c)| (n.clone(), c.value.clone()))
            .collect(),
    };
    if result.updated_at < result.created_at || result.expires_at <= result.created_at {
        return Err(ArkivFault::Corrupt);
    }
    Ok(result)
}
pub fn validate_proven_row(row: &Row) -> Result<(), ArkivFault> {
    project(row).map(|_| ())
}

/// SDK ABI values: fixed scalars occupy a canonical 32-byte Solidity word.
pub fn decode_abi_value(type_id: u8, bytes: &[u8]) -> Result<Value, ArkivFault> {
    if (type_id == 7 && bytes.len() > 128 * 1024) || (type_id == 8 && bytes.len() > 128) {
        return Err(ArkivFault::Value);
    }
    let value = match type_id {
        7 => Value::bytes(bytes),
        8 => Value::str(std::str::from_utf8(bytes).map_err(|_| ArkivFault::Value)?),
        1..=6 | 9 | 10 => {
            let word: [u8; 32] = bytes.try_into().map_err(|_| ArkivFault::Value)?;
            match type_id {
                1 if word[..31] == [0; 31] && word[31] <= 1 => Value::Bool(word[31] == 1),
                2 => {
                    let v = i32::from_be_bytes(word[28..].try_into().expect("width"));
                    if !word[..28].iter().all(|b| *b == if v < 0 { 255 } else { 0 }) {
                        return Err(ArkivFault::Value);
                    }
                    Value::I32(v)
                }
                3 if word[..24] == [0; 24] => {
                    Value::U64(u64::from_be_bytes(word[24..].try_into().expect("width")))
                }
                4 => Value::U256(word),
                5 => Value::Dec(word),
                6 => Value::Bytes32(word),
                9 if word[..12] == [0; 12] => Value::Addr(word[12..].try_into().expect("width")),
                10 => Value::Key(word),
                _ => return Err(ArkivFault::Value),
            }
        }
        _ => return Err(ArkivFault::Value),
    };
    validate_value(&value)?;
    Ok(value)
}
pub fn encode_abi_value(value: &Value) -> Result<Vec<u8>, ArkivFault> {
    validate_value(value)?;
    let mut word = [0; 32];
    match value {
        Value::Bool(v) => word[31] = u8::from(*v),
        Value::I32(v) => {
            word.fill(if *v < 0 { 255 } else { 0 });
            word[28..].copy_from_slice(&v.to_be_bytes())
        }
        Value::U64(v) => word[24..].copy_from_slice(&v.to_be_bytes()),
        Value::Addr(v) => word[12..].copy_from_slice(v),
        Value::U256(v) | Value::Dec(v) | Value::Bytes32(v) | Value::Key(v) => word = *v,
        Value::Str(v) => return Ok(v.as_bytes().to_vec()),
        Value::Bytes(v) => return Ok(v.to_vec()),
        Value::I64(_) => return Err(ArkivFault::Value),
    }
    Ok(word.to_vec())
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AbiAttribute {
    pub name: [u8; 32],
    pub type_id: u8,
    pub value: Vec<u8>,
}
/// Strict protocol triple validation; SDK-safe authoring is a separate policy.
/// Type zero is accepted only as an empty patch tombstone, never a stored value.
pub fn decode_protocol_triples(
    triples: &[AbiAttribute],
    patch: bool,
) -> Result<Vec<(CellName, Option<Value>)>, ArkivFault> {
    if triples.len() > 32 {
        return Err(ArkivFault::Value);
    }
    let mut previous = None;
    let mut result = Vec::new();
    for triple in triples {
        if previous.is_some_and(|n| n >= triple.name) {
            return Err(ArkivFault::Duplicate);
        }
        previous = Some(triple.name);
        let end = triple.name.iter().position(|b| *b == 0).unwrap_or(32);
        if triple.name[end..].iter().any(|b| *b != 0) {
            return Err(ArkivFault::Name);
        }
        let text = std::str::from_utf8(&triple.name[..end]).map_err(|_| ArkivFault::Name)?;
        if !protocol_name(text) && !matches!(text, "$payload" | "$contentType") {
            return Err(ArkivFault::Name);
        }
        let name = CellName::new(text).map_err(|_| ArkivFault::Name)?;
        let value = if triple.type_id == 0 {
            if !patch || !triple.value.is_empty() {
                return Err(ArkivFault::Value);
            }
            None
        } else {
            let v = decode_abi_value(triple.type_id, &triple.value)?;
            if match text {
                "$payload" => !matches!(v, Value::Bytes(_)),
                "$contentType" => !matches!(v, Value::Str(_)),
                _ => matches!(v, Value::Bytes(_)),
            } {
                return Err(ArkivFault::Value);
            }
            Some(v)
        };
        result.push((name, value));
    }
    Ok(result)
}

/// Production's natural-width stored value bytes, distinct from ABI words and
/// ordered term bytes. Generic row framing continues to use its own engine codec.
pub fn encode_natural_value(value: &Value) -> Result<Vec<u8>, ArkivFault> {
    validate_value(value)?;
    Ok(match value {
        Value::Bool(v) => vec![u8::from(*v)],
        Value::I32(v) => v.to_be_bytes().to_vec(),
        Value::U64(v) => v.to_be_bytes().to_vec(),
        Value::U256(v) | Value::Dec(v) | Value::Bytes32(v) | Value::Key(v) => v.to_vec(),
        Value::Addr(v) => v.to_vec(),
        Value::Str(v) => v.as_bytes().to_vec(),
        Value::Bytes(v) => v.to_vec(),
        Value::I64(_) => return Err(ArkivFault::Value),
    })
}
pub fn decode_natural_value(type_id: u8, bytes: &[u8]) -> Result<Value, ArkivFault> {
    let width = match type_id {
        1 => 1,
        2 => 4,
        3 => 8,
        9 => 20,
        4..=6 | 10 => 32,
        7 | 8 => return decode_abi_value(type_id, bytes),
        _ => return Err(ArkivFault::Value),
    };
    if bytes.len() != width {
        return Err(ArkivFault::Value);
    }
    let mut word = [0u8; 32];
    if type_id == 2 && bytes[0] & 128 != 0 {
        word.fill(255);
    }
    word[32 - width..].copy_from_slice(bytes);
    decode_abi_value(type_id, &word)
}
