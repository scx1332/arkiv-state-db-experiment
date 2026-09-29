//! Typed records: caller-assigned keys, dense record ids, validated cell
//! names, typed values with canonical encodings, and instance limits.
//!
//! A record is a key plus a set of named cells. Every cell has a kind
//! (attribute: indexed, filterable; field: stored and returned only) and a
//! type. Kind and type are bound per record, not per name: the same name may
//! be an indexed integer in one record and an unindexed string in another.
//! The engine assigns application cells no meaning.
//!
//! Every encoding here is canonical and frozen for this repository: term
//! bytes decide index order (ranges and prefixes are contiguous runs), and a
//! later milestone hashes the same bytes into roots.

use std::{fmt, sync::Arc};

/// A caller-assigned record key: 1 to 32 bytes.
///
/// Keys are unique among the live records of a namespace. The SDK's entity
/// keys are 32-byte hashes, so they fit as they are; shorter keys are
/// accepted too.
///
/// The canonical encoding (version [`Self::ENCODING_VERSION`] = 2) is 33
/// bytes, `len ‖ bytes ‖ zero padding`, a bijection: short keys round-trip
/// losslessly and every accepted byte string decodes to exactly one key.
/// The version-1 form was 32 bytes wide and held keys of 1 to 31 bytes; it
/// remains available through [`Self::encoded_v1`] and [`Self::decode_v1`]
/// for explicit migration only. The two forms differ in length, so one is
/// never silently read as the other. No version-1 encoding was ever
/// persisted. Charges count the raw key length, not the encoding.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RecordKey(Arc<[u8]>);

impl RecordKey {
    /// Longest accepted key: the SDK's 32-byte entity keys.
    pub const MAX_LEN: usize = 32;
    /// Width of the canonical (version 2) encoding: the length byte plus
    /// [`Self::MAX_LEN`] bytes.
    pub const ENCODED_LEN: usize = 33;
    /// Version of the canonical encoding produced by [`Self::encoded`].
    pub const ENCODING_VERSION: u32 = 2;
    /// Width of the version-1 encoding (keys of at most 31 bytes).
    pub const ENCODED_LEN_V1: usize = 32;

    /// Wraps `bytes`; `None` for an empty key or one longer than [`Self::MAX_LEN`].
    pub fn new(bytes: &[u8]) -> Option<Self> {
        (1..=Self::MAX_LEN)
            .contains(&bytes.len())
            .then(|| Self(Arc::from(bytes)))
    }

    /// The raw key bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Canonical version-2 encoding: `len ‖ bytes ‖ 0x00…`, 33 bytes.
    pub fn encoded(&self) -> [u8; Self::ENCODED_LEN] {
        let mut out = [0u8; Self::ENCODED_LEN];
        out[0] = u8::try_from(self.0.len()).expect("keys are at most 32 bytes");
        out[1..=self.0.len()].copy_from_slice(&self.0);
        out
    }

    /// Decodes the canonical version-2 encoding; rejects a length byte of 0
    /// or above 32 and any non-zero padding.
    pub fn decode(encoded: [u8; Self::ENCODED_LEN]) -> Option<Self> {
        Self::decode_padded(&encoded, Self::MAX_LEN)
    }

    /// The version-1 encoding (`len ‖ bytes ‖ 0x00…`, 32 bytes), for explicit
    /// migration only; `None` for a 32-byte key, which version 1 cannot hold.
    pub fn encoded_v1(&self) -> Option<[u8; Self::ENCODED_LEN_V1]> {
        let len = self.0.len();
        if len >= Self::ENCODED_LEN_V1 {
            return None;
        }
        let mut out = [0u8; Self::ENCODED_LEN_V1];
        out[0] = u8::try_from(len).expect("checked above");
        out[1..=len].copy_from_slice(&self.0);
        Some(out)
    }

    /// Decodes a version-1 encoding, for explicit migration only; rejects a
    /// length byte of 0 or above 31 and any non-zero padding.
    pub fn decode_v1(encoded: [u8; Self::ENCODED_LEN_V1]) -> Option<Self> {
        Self::decode_padded(&encoded, Self::ENCODED_LEN_V1 - 1)
    }

    /// Shared strict decoder of the `len ‖ bytes ‖ zero padding` forms.
    fn decode_padded(encoded: &[u8], max_len: usize) -> Option<Self> {
        let len = usize::from(encoded[0]);
        if !(1..=max_len).contains(&len) {
            return None;
        }
        if encoded[len + 1..].iter().any(|&byte| byte != 0) {
            return None;
        }
        Self::new(&encoded[1..=len])
    }
}

impl fmt::Debug for RecordKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "RecordKey(0x")?;
        for byte in self.0.iter() {
            write!(formatter, "{byte:02x}")?;
        }
        write!(formatter, ")")
    }
}

/// Dense per-namespace creation ordinal of a record, starting at 1.
///
/// Ids are minted from the namespace's own allocator and never reused within
/// that namespace; deleting a key and creating it again mints a new id, so a
/// reused key never joins two histories. An id is provisional until its
/// candidate is published (rollback restores the allocator). Ids are the sole
/// result order of queries and the final tie-break.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RecordId(u64);

impl RecordId {
    /// Names the record with creation ordinal `ordinal`; zero is invalid.
    pub const fn from_ordinal(ordinal: u64) -> Option<Self> {
        if ordinal == 0 {
            None
        } else {
            Some(Self(ordinal))
        }
    }

    /// The creation ordinal (never zero).
    pub const fn ordinal(self) -> u64 {
        self.0
    }
}

impl fmt::Debug for RecordId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "RecordId({})", self.0)
    }
}

impl fmt::Display for RecordId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "rec:{}", self.0)
    }
}

/// Why a cell or namespace name was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NameFault {
    /// The name is empty.
    Empty,
    /// The name is longer than [`CellName::MAX_LEN`] bytes.
    TooLong,
    /// The name does not follow the grammar (ASCII, letter first, optional
    /// single leading `$`, then letters, digits, `_`, `-`, `.`, `:`).
    Grammar,
}

impl fmt::Display for NameFault {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(formatter, "name is empty"),
            Self::TooLong => write!(formatter, "name is longer than {} bytes", CellName::MAX_LEN),
            Self::Grammar => write!(formatter, "name violates the name grammar"),
        }
    }
}

impl std::error::Error for NameFault {}

/// A validated cell name.
///
/// Grammar: ASCII only; the first character is a letter, or `$` followed by a
/// letter; every later character is a letter, a digit, `_`, `-`, `.` or `:`;
/// 1 to the selected profile's byte limit; case-sensitive with no normalisation. `#` and `@` never
/// occur, so engine-reserved names are unreachable from the data plane.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CellName(Arc<str>);

impl CellName {
    /// Longest accepted name, in bytes.
    pub const MAX_LEN: usize = if cfg!(feature = "arkiv-v2") { 32 } else { 20 };

    /// Validates `name` against the grammar.
    pub fn new(name: &str) -> Result<Self, NameFault> {
        Self::validate(name)?;
        Ok(Self(Arc::from(name)))
    }

    pub(crate) fn validate(name: &str) -> Result<(), NameFault> {
        let bytes = name.as_bytes();
        if bytes.is_empty() {
            return Err(NameFault::Empty);
        }
        if bytes.len() > Self::MAX_LEN {
            return Err(NameFault::TooLong);
        }
        let body = match bytes[0] {
            b'$' => &bytes[1..],
            _ => bytes,
        };
        let valid = body.first().is_some_and(u8::is_ascii_alphabetic)
            && body.iter().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':')
            });
        if valid {
            Ok(())
        } else {
            Err(NameFault::Grammar)
        }
    }

    /// The name text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Length in bytes, as charged by the meter.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Never true: names are at least one byte.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for CellName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "CellName({:?})", &*self.0)
    }
}

impl fmt::Display for CellName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// The type of a value. Ids are local to this repository (they are not the
/// SDK's typeIds) and are never reassigned.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TypeTag {
    /// `true` or `false`.
    Bool = 1,
    /// Signed 64-bit integer.
    I64 = 2,
    /// Unsigned 64-bit integer.
    U64 = 3,
    /// UTF-8 text.
    Str = 4,
    /// Opaque bytes; field-only, never indexed.
    Bytes = 5,
    /// Distinct Arkiv i32 scalar.
    I32 = 6,
    /// Distinct Arkiv u256 scalar.
    U256 = 7,
    /// Distinct Arkiv dec scalar.
    Dec = 8,
    /// Distinct Arkiv bytes32 scalar.
    Bytes32 = 9,
    /// Distinct Arkiv addr scalar.
    Addr = 10,
    /// Distinct Arkiv key scalar.
    Key = 11,
}

impl TypeTag {
    /// Every tag, in id order.
    #[cfg(not(feature = "arkiv-v2"))]
    pub const ALL: [Self; 5] = [Self::Bool, Self::I64, Self::U64, Self::Str, Self::Bytes];
    /// Every tag supported by the v2 profile.
    #[cfg(feature = "arkiv-v2")]
    pub const ALL: [Self; 11] = [
        Self::Bool,
        Self::I64,
        Self::U64,
        Self::Str,
        Self::Bytes,
        Self::I32,
        Self::U256,
        Self::Dec,
        Self::Bytes32,
        Self::Addr,
        Self::Key,
    ];

    /// Whether the selected engine profile admits this type.
    pub const fn is_supported(self) -> bool {
        cfg!(feature = "arkiv-v2") || self.id() <= 5
    }

    /// The type id.
    pub const fn id(self) -> u8 {
        self as u8
    }

    /// Whether attributes of this type exist (everything except `Bytes`).
    pub const fn is_indexable(self) -> bool {
        self.is_supported() && !matches!(self, Self::Bytes)
    }

    /// Whether range predicates apply to this profile's numeric types.
    pub const fn is_ordered(self) -> bool {
        self.is_supported()
            && matches!(
                self,
                Self::I64 | Self::U64 | Self::I32 | Self::U256 | Self::Dec
            )
    }

    /// Whether prefix predicates apply (`Str`).
    pub const fn supports_prefix(self) -> bool {
        matches!(self, Self::Str)
    }
}

impl fmt::Display for TypeTag {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Bool => "bool",
            Self::I64 => "i64",
            Self::U64 => "u64",
            Self::Str => "str",
            Self::Bytes => "bytes",
            Self::I32 => "i32",
            Self::U256 => "u256",
            Self::Dec => "dec",
            Self::Bytes32 => "bytes32",
            Self::Addr => "addr",
            Self::Key => "key",
        };
        formatter.write_str(name)
    }
}

/// A typed value.
///
/// Values of different types never compare equal or order against each
/// other: a predicate on one type treats a cell of another type as absent.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Value {
    /// `true` or `false`.
    Bool(bool),
    /// Signed 64-bit integer.
    I64(i64),
    /// Unsigned 64-bit integer.
    U64(u64),
    /// UTF-8 text.
    Str(Arc<str>),
    /// Opaque bytes (field-only).
    Bytes(Arc<[u8]>),
    /// Signed 32-bit integer.
    I32(i32),
    /// Unsigned 256-bit integer in natural-width big-endian bytes.
    U256([u8; 32]),
    /// Signed 256-bit two's complement big-endian units, scaled by 10^18.
    Dec([u8; 32]),
    /// Exactly 32 opaque bytes, distinct from integers and references.
    Bytes32([u8; 32]),
    /// Exactly 20 address bytes.
    Addr([u8; 20]),
    /// Exactly 32 entity-reference bytes; referential existence is not enforced.
    Key([u8; 32]),
}

impl Value {
    /// Hard bound on the encoded bytes of an indexed attribute value.
    /// Field values retain the engine's independently configured value limit.
    pub const MAX_INDEXED_LEN: usize = if cfg!(feature = "arkiv-v2") { 128 } else { 64 };

    /// A text value.
    pub fn str(text: &str) -> Self {
        Self::Str(Arc::from(text))
    }

    /// A bytes value.
    pub fn bytes(bytes: &[u8]) -> Self {
        Self::Bytes(Arc::from(bytes))
    }

    /// The value's type.
    pub const fn tag(&self) -> TypeTag {
        match self {
            Self::Bool(_) => TypeTag::Bool,
            Self::I64(_) => TypeTag::I64,
            Self::U64(_) => TypeTag::U64,
            Self::Str(_) => TypeTag::Str,
            Self::Bytes(_) => TypeTag::Bytes,
            Self::I32(_) => TypeTag::I32,
            Self::U256(_) => TypeTag::U256,
            Self::Dec(_) => TypeTag::Dec,
            Self::Bytes32(_) => TypeTag::Bytes32,
            Self::Addr(_) => TypeTag::Addr,
            Self::Key(_) => TypeTag::Key,
        }
    }

    /// Length of the canonical encoding in bytes: 1 for `Bool`, 8 for the
    /// integers, the byte length for `Str` and `Bytes`. This is what
    /// `CellByte` and `ReadByte` charges count.
    pub fn encoded_len(&self) -> usize {
        match self {
            Self::Bool(_) => 1,
            Self::I64(_) | Self::U64(_) => 8,
            Self::Str(text) => text.len(),
            Self::Bytes(bytes) => bytes.len(),
            Self::I32(_) => 4,
            Self::Addr(_) => 20,
            Self::U256(_) | Self::Dec(_) | Self::Bytes32(_) | Self::Key(_) => 32,
        }
    }

    /// Canonical order-preserving term bytes of an indexable value; `None`
    /// for `Bytes`.
    ///
    /// `Bool` is `00`/`01`; `I64` is the big-endian two's complement with the
    /// sign bit flipped, so byte order equals numeric order; `U64` is
    /// big-endian; `Str` is its UTF-8 bytes, so byte order equals code point
    /// order and a prefix is a contiguous run.
    pub fn term_bytes(&self) -> Option<Vec<u8>> {
        match self {
            Self::Bool(flag) => Some(vec![u8::from(*flag)]),
            Self::I64(value) => Some(((*value as u64) ^ (1 << 63)).to_be_bytes().to_vec()),
            Self::U64(value) => Some(value.to_be_bytes().to_vec()),
            Self::Str(text) => Some(text.as_bytes().to_vec()),
            Self::Bytes(_) => None,
            Self::I32(value) => Some(((*value as u32) ^ (1 << 31)).to_be_bytes().to_vec()),
            Self::U256(value) | Self::Bytes32(value) | Self::Key(value) => Some(value.to_vec()),
            Self::Addr(value) => Some(value.to_vec()),
            Self::Dec(value) => {
                let mut bytes = *value;
                bytes[0] ^= 0x80;
                Some(bytes.to_vec())
            }
        }
    }

    /// Decodes canonical term bytes of an indexable type; `None` for `Bytes`
    /// or malformed input. The inverse of [`Self::term_bytes`].
    pub fn from_term_bytes(tag: TypeTag, bytes: &[u8]) -> Option<Self> {
        match tag {
            TypeTag::Bool => match bytes {
                [0] => Some(Self::Bool(false)),
                [1] => Some(Self::Bool(true)),
                _ => None,
            },
            TypeTag::I64 => {
                let raw = u64::from_be_bytes(bytes.try_into().ok()?);
                Some(Self::I64((raw ^ (1 << 63)) as i64))
            }
            TypeTag::U64 => Some(Self::U64(u64::from_be_bytes(bytes.try_into().ok()?))),
            TypeTag::Str => std::str::from_utf8(bytes).ok().map(Self::str),
            TypeTag::Bytes => None,
            TypeTag::I32 => Some(Self::I32(
                (u32::from_be_bytes(bytes.try_into().ok()?) ^ (1 << 31)) as i32,
            )),
            TypeTag::U256 => Some(Self::U256(bytes.try_into().ok()?)),
            TypeTag::Bytes32 => Some(Self::Bytes32(bytes.try_into().ok()?)),
            TypeTag::Addr => Some(Self::Addr(bytes.try_into().ok()?)),
            TypeTag::Key => Some(Self::Key(bytes.try_into().ok()?)),
            TypeTag::Dec => {
                let mut raw: [u8; 32] = bytes.try_into().ok()?;
                raw[0] ^= 0x80;
                Some(Self::Dec(raw))
            }
        }
    }

    /// Orders two values of the same ordered type; `None` when the types
    /// differ or the type is not ordered.
    pub fn cmp_ordered(&self, other: &Self) -> Option<std::cmp::Ordering> {
        match (self, other) {
            (Self::I32(left), Self::I32(right)) => Some(left.cmp(right)),
            (Self::U256(left), Self::U256(right)) => Some(left.cmp(right)),
            (Self::Dec(left), Self::Dec(right)) => {
                Some((left[0] ^ 128, &left[1..]).cmp(&(right[0] ^ 128, &right[1..])))
            }
            (Self::I64(left), Self::I64(right)) => Some(left.cmp(right)),
            (Self::U64(left), Self::U64(right)) => Some(left.cmp(right)),
            _ => None,
        }
    }
}

/// Whether a cell is indexed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CellKind {
    /// Indexed on write: filterable through queries.
    Attribute,
    /// Stored and returned only; never indexed, never matches a predicate.
    Field,
}

/// A named value's kind and value; the name lives beside it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Cell {
    /// Attribute or field.
    pub kind: CellKind,
    /// The typed value.
    pub value: Value,
}

impl Cell {
    /// An indexed cell.
    pub const fn attribute(value: Value) -> Self {
        Self {
            kind: CellKind::Attribute,
            value,
        }
    }

    /// An unindexed cell.
    pub const fn field(value: Value) -> Self {
        Self {
            kind: CellKind::Field,
            value,
        }
    }

    /// Whether the cell is an attribute.
    pub const fn is_attribute(&self) -> bool {
        matches!(self.kind, CellKind::Attribute)
    }
}

/// A projected record as returned by reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    /// The record's creation ordinal; always returned.
    pub id: RecordId,
    /// The key, when projected.
    pub key: Option<RecordKey>,
    /// The projected cells, sorted by name.
    pub cells: Vec<(CellName, Cell)>,
}

/// Instance parameters: every limit user input can reach. Fixed for an
/// engine's lifetime and checked at admission, before any charge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Most namespaces the directory admits (see
    /// [`crate::DEFAULT_NAMESPACE_CAP`]).
    pub namespace_cap: u64,
    /// Most cells a record may hold, and most names a projection may list.
    pub max_cells: usize,
    /// Longest `Str`/`Bytes` value, in bytes. Indexed values and query
    /// strings also obey the hard [`Value::MAX_INDEXED_LEN`] bound.
    pub max_value_len: usize,
    /// Largest query page.
    pub max_page: usize,
    /// Most AND-groups in a query.
    pub max_groups: usize,
    /// Most literals in one AND-group.
    pub max_literals: usize,
}

impl Limits {
    /// Hard bound on a local raw engine key or scan prefix, in bytes.
    /// The namespace is a separate fixed-width ID, not part of these bytes.
    pub const MAX_RAW_KEY_LEN: usize = 20;

    pub(crate) const fn indexed_value_len(&self) -> usize {
        if self.max_value_len < Value::MAX_INDEXED_LEN {
            self.max_value_len
        } else {
            Value::MAX_INDEXED_LEN
        }
    }

    /// The defaults every engine uses unless configured otherwise.
    pub const DEFAULT: Self = Self {
        namespace_cap: crate::engine::DEFAULT_NAMESPACE_CAP,
        max_cells: 64,
        max_value_len: 4096,
        max_page: 200,
        max_groups: 8,
        max_literals: 8,
    };
}

impl Default for Limits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Why a write or raw read was rejected at admission, before any charge.
/// Rejected writes spend zero units and are not recorded in the branch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArgumentFault {
    /// A raw engine key or prefix exceeds [`Limits::MAX_RAW_KEY_LEN`].
    KeyTooLong,
    /// The record would hold more than `Limits::max_cells` cells, or the
    /// patch names more cells than that.
    TooManyCells,
    /// A name appears twice in the cells, set list or remove list.
    DuplicateCell,
    /// A name appears in both a patch's set and remove lists.
    SetAndRemoveOverlap,
    /// A value exceeds the configured value limit or the indexed-value cap.
    ValueTooLarge,
    /// An attribute cell carries a `Bytes` value.
    BytesNotIndexable,
    /// A patch sets nothing and removes nothing.
    EmptyPatch,
    /// The host path accepts typed record operations only.
    NotHostOperation,
    /// The scalar belongs to another engine profile.
    UnsupportedType,
}

impl fmt::Display for ArgumentFault {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::KeyTooLong => "raw key or prefix is too long",
            Self::TooManyCells => "too many cells",
            Self::DuplicateCell => "duplicate cell name",
            Self::SetAndRemoveOverlap => "a name is both set and removed",
            Self::ValueTooLarge => "value too large",
            Self::BytesNotIndexable => "bytes values cannot be attributes",
            Self::EmptyPatch => "patch changes nothing",
            Self::NotHostOperation => "not a host operation",
            Self::UnsupportedType => "scalar type is unsupported by this profile",
        };
        formatter.write_str(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_encode_with_a_length_byte_and_zero_padding() {
        assert_eq!(RecordKey::MAX_LEN, 32);
        assert_eq!(RecordKey::ENCODED_LEN, 33);
        assert_eq!(RecordKey::ENCODING_VERSION, 2);

        // b"a" -> 01 61 00…00 (33 bytes).
        let key = RecordKey::new(b"a").unwrap();
        let mut expected = [0u8; 33];
        expected[0] = 1;
        expected[1] = b'a';
        assert_eq!(key.encoded(), expected);
        assert_eq!(RecordKey::decode(expected), Some(key.clone()));

        // 31 zero bytes -> 1f 00…00.
        let zeros = RecordKey::new(&[0u8; 31]).unwrap();
        let mut expected = [0u8; 33];
        expected[0] = 31;
        assert_eq!(zeros.encoded(), expected);
        assert_eq!(RecordKey::decode(expected), Some(zeros.clone()));

        // A 32-byte key (an SDK entity hash) -> 20 ‖ the 32 bytes, no padding.
        let digest: [u8; 32] = std::array::from_fn(|index| 0xa0 + u8::try_from(index).unwrap());
        let full = RecordKey::new(&digest).unwrap();
        let mut expected = [0u8; 33];
        expected[0] = 0x20;
        expected[1..].copy_from_slice(&digest);
        assert_eq!(expected[1], 0xa0);
        assert_eq!(expected[32], 0xbf);
        assert_eq!(full.encoded(), expected);
        assert_eq!(RecordKey::decode(expected), Some(full.clone()));

        assert_eq!(RecordKey::new(b""), None);
        assert_eq!(RecordKey::new(&[7u8; 33]), None);
        assert_eq!(RecordKey::decode([0u8; 33]), None);
        let mut bad = [0u8; 33];
        bad[0] = 33;
        assert_eq!(RecordKey::decode(bad), None);
        let mut dirty = [0u8; 33];
        dirty[0] = 1;
        dirty[1] = b'a';
        dirty[32] = 1;
        assert_eq!(RecordKey::decode(dirty), None);

        // Version 1: 32 bytes wide, keys of at most 31 bytes, migration only.
        let mut v1 = [0u8; 32];
        v1[0] = 1;
        v1[1] = b'a';
        assert_eq!(key.encoded_v1(), Some(v1));
        assert_eq!(RecordKey::decode_v1(v1), Some(key));
        let mut v1 = [0u8; 32];
        v1[0] = 31;
        assert_eq!(zeros.encoded_v1(), Some(v1));
        assert_eq!(RecordKey::decode_v1(v1), Some(zeros));
        assert_eq!(full.encoded_v1(), None);
        let mut v1_too_long = [0u8; 32];
        v1_too_long[0] = 32;
        assert_eq!(RecordKey::decode_v1(v1_too_long), None);
        assert_eq!(RecordKey::decode_v1([0u8; 32]), None);
        let mut v1_dirty = [0u8; 32];
        v1_dirty[0] = 1;
        v1_dirty[1] = b'a';
        v1_dirty[31] = 1;
        assert_eq!(RecordKey::decode_v1(v1_dirty), None);
    }

    #[test]
    fn names_follow_the_grammar() {
        for good in ["a", "A1", "$owner", "x_y-z.w:v", "Z", &"n".repeat(20)] {
            assert!(CellName::new(good).is_ok(), "{good}");
        }
        assert_eq!(CellName::new(""), Err(NameFault::Empty));
        assert_eq!(
            CellName::new(&"n".repeat(CellName::MAX_LEN + 1)),
            Err(NameFault::TooLong)
        );
        for bad in [
            "$", "$$a", "$1", "1a", "_a", "a b", "a#b", "a@b", "é", "a/b", "-a", "a\0",
        ] {
            assert_eq!(CellName::new(bad), Err(NameFault::Grammar), "{bad}");
        }
        assert!(CellName::new("abc").unwrap() < CellName::new("abd").unwrap());
        assert!(CellName::new("B").unwrap() < CellName::new("a").unwrap());
    }

    #[test]
    fn term_bytes_preserve_order() {
        let bytes = |value: Value| value.term_bytes().unwrap();
        assert_eq!(
            bytes(Value::I64(-1)),
            vec![0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]
        );
        assert_eq!(bytes(Value::I64(0)), vec![0x80, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(bytes(Value::I64(i64::MIN)), vec![0; 8]);
        assert_eq!(bytes(Value::I64(i64::MAX)), vec![0xff; 8]);
        assert_eq!(bytes(Value::U64(258)), vec![0, 0, 0, 0, 0, 0, 1, 2]);
        assert_eq!(bytes(Value::Str(Arc::from("ab"))), vec![0x61, 0x62]);
        assert_eq!(bytes(Value::Bool(false)), vec![0]);
        assert_eq!(bytes(Value::Bool(true)), vec![1]);
        assert_eq!(Value::bytes(b"x").term_bytes(), None);
        for value in [
            Value::I64(i64::MIN),
            Value::I64(-1),
            Value::I64(0),
            Value::I64(7),
            Value::I64(i64::MAX),
            Value::U64(0),
            Value::U64(u64::MAX),
            Value::Bool(true),
            Value::Bool(false),
            Value::str(""),
            Value::str("héllo"),
        ] {
            let round = Value::from_term_bytes(value.tag(), &value.term_bytes().unwrap());
            assert_eq!(round, Some(value.clone()), "{value:?}");
        }
        assert_eq!(Value::from_term_bytes(TypeTag::Bytes, b"x"), None);
        assert_eq!(Value::from_term_bytes(TypeTag::Bool, &[2]), None);
        assert_eq!(Value::from_term_bytes(TypeTag::I64, &[1, 2, 3]), None);
        assert_eq!(Value::from_term_bytes(TypeTag::Str, &[0xff]), None);

        let samples = [i64::MIN, -2, -1, 0, 1, 2, i64::MAX];
        for window in samples.windows(2) {
            assert!(bytes(Value::I64(window[0])) < bytes(Value::I64(window[1])));
        }
        assert_eq!(
            Value::I64(3).cmp_ordered(&Value::I64(4)),
            Some(std::cmp::Ordering::Less)
        );
        assert_eq!(Value::I64(3).cmp_ordered(&Value::U64(4)), None);
        assert_eq!(Value::str("a").cmp_ordered(&Value::str("b")), None);
        assert_eq!(Value::Bool(true).encoded_len(), 1);
        assert_eq!(Value::U64(1).encoded_len(), 8);
        assert_eq!(Value::str("héllo").encoded_len(), 6);
        assert_eq!(Value::bytes(&[1, 2, 3]).encoded_len(), 3);
    }

    #[test]
    fn ids_start_at_one() {
        assert_eq!(RecordId::from_ordinal(0), None);
        let id = RecordId::from_ordinal(7).unwrap();
        assert_eq!(id.ordinal(), 7);
        assert_eq!(id.to_string(), "rec:7");
        assert_eq!(format!("{id:?}"), "RecordId(7)");
    }
}
