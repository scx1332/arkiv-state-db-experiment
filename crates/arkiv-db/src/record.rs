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
///
/// Ids 1–11 are the original scalars; ids 12–25 complete golem-db's cell
/// type grid (see `docs/block-format.md`, "Value types"). Every id is
/// admitted by both engine profiles.
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
    /// Signed 32-bit integer.
    I32 = 6,
    /// Unsigned 256-bit integer.
    U256 = 7,
    /// Signed 256-bit decimal at scale 18 (golem `dec256`).
    Dec = 8,
    /// Exactly 32 opaque bytes.
    Bytes32 = 9,
    /// Exactly 20 address bytes (golem `bytes20`).
    Addr = 10,
    /// Exactly 32 entity-reference bytes.
    Key = 11,
    /// Exactly 4 opaque bytes.
    Bytes4 = 12,
    /// Exactly 8 opaque bytes.
    Bytes8 = 13,
    /// Exactly 16 opaque bytes.
    Bytes16 = 14,
    /// Unsigned 32-bit integer.
    U32 = 15,
    /// Unsigned 128-bit integer.
    U128 = 16,
    /// Signed 128-bit integer.
    I128 = 17,
    /// Signed 256-bit integer.
    I256 = 18,
    /// Signed 32-bit decimal at scale 4.
    Dec32 = 19,
    /// Signed 64-bit decimal at scale 6.
    Dec64 = 20,
    /// Signed 128-bit decimal at scale 18.
    Dec128 = 21,
    /// IEEE-754 binary32, never NaN or negative zero.
    F32 = 22,
    /// IEEE-754 binary64, never NaN or negative zero.
    F64 = 23,
    /// Signed days since the Unix epoch.
    Date32 = 24,
    /// Signed microseconds since the Unix epoch.
    Timestamp64 = 25,
}

impl TypeTag {
    /// Every tag, in id order. Both profiles admit every tag.
    pub const ALL: [Self; 25] = [
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
        Self::Bytes4,
        Self::Bytes8,
        Self::Bytes16,
        Self::U32,
        Self::U128,
        Self::I128,
        Self::I256,
        Self::Dec32,
        Self::Dec64,
        Self::Dec128,
        Self::F32,
        Self::F64,
        Self::Date32,
        Self::Timestamp64,
    ];

    /// Whether the selected engine profile admits this type. Every type is
    /// admitted by both profiles; the method is kept for callers that gate
    /// on it.
    pub const fn is_supported(self) -> bool {
        true
    }

    /// The type id.
    pub const fn id(self) -> u8 {
        self as u8
    }

    /// The tag with type id `id`, if any.
    pub const fn from_id(id: u8) -> Option<Self> {
        if id == 0 || id as usize > Self::ALL.len() {
            None
        } else {
            Some(Self::ALL[id as usize - 1])
        }
    }

    /// The tag whose [`Self::name`] is `name`, if any.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|tag| tag.name() == name)
    }

    /// The lower-case type name used by docs and JSON APIs.
    pub const fn name(self) -> &'static str {
        match self {
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
            Self::Bytes4 => "bytes4",
            Self::Bytes8 => "bytes8",
            Self::Bytes16 => "bytes16",
            Self::U32 => "u32",
            Self::U128 => "u128",
            Self::I128 => "i128",
            Self::I256 => "i256",
            Self::Dec32 => "dec32",
            Self::Dec64 => "dec64",
            Self::Dec128 => "dec128",
            Self::F32 => "f32",
            Self::F64 => "f64",
            Self::Date32 => "date32",
            Self::Timestamp64 => "timestamp64",
        }
    }

    /// Width of the value in bytes; `None` for `Str` and `Bytes`.
    pub const fn width(self) -> Option<usize> {
        match self {
            Self::Str | Self::Bytes => None,
            Self::Bool => Some(1),
            Self::I32 | Self::Bytes4 | Self::U32 | Self::Dec32 | Self::F32 | Self::Date32 => {
                Some(4)
            }
            Self::I64 | Self::U64 | Self::Bytes8 | Self::Dec64 | Self::F64 | Self::Timestamp64 => {
                Some(8)
            }
            Self::Bytes16 | Self::U128 | Self::I128 | Self::Dec128 => Some(16),
            Self::Addr => Some(20),
            Self::U256 | Self::Dec | Self::Bytes32 | Self::Key | Self::I256 => Some(32),
        }
    }

    /// Decimal places of a decimal type; `None` for other types.
    pub const fn decimal_scale(self) -> Option<u8> {
        match self {
            Self::Dec32 => Some(4),
            Self::Dec64 => Some(6),
            Self::Dec128 | Self::Dec => Some(18),
            _ => None,
        }
    }

    /// Whether attributes of this type exist (everything except `Bytes`).
    pub const fn is_indexable(self) -> bool {
        self.is_supported() && !matches!(self, Self::Bytes)
    }

    /// Whether range predicates apply: the integers, decimals, floats and
    /// time types. Booleans, strings and the fixed-width byte types are
    /// equality-only.
    pub const fn is_ordered(self) -> bool {
        self.is_supported()
            && matches!(
                self,
                Self::I64
                    | Self::U64
                    | Self::I32
                    | Self::U256
                    | Self::Dec
                    | Self::U32
                    | Self::U128
                    | Self::I128
                    | Self::I256
                    | Self::Dec32
                    | Self::Dec64
                    | Self::Dec128
                    | Self::F32
                    | Self::F64
                    | Self::Date32
                    | Self::Timestamp64
            )
    }

    /// Whether prefix predicates apply (`Str`).
    pub const fn supports_prefix(self) -> bool {
        matches!(self, Self::Str)
    }
}

impl fmt::Display for TypeTag {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

macro_rules! float_scalar {
    ($(#[$doc:meta])* $name:ident, $float:ty, $bits:ty) => {
        $(#[$doc])*
        ///
        /// The value is never NaN and never negative zero: [`Self::new`]
        /// rejects NaN and normalizes `-0.0` to `+0.0`, and
        /// [`Self::from_bits`] rejects both bit patterns, so every value has
        /// exactly one canonical encoding. Infinities are admitted. Equality
        /// is bit equality, which equals numeric equality on this domain.
        #[derive(Clone, Copy, PartialEq, Eq, Hash)]
        pub struct $name($bits);

        impl $name {
            const SIGN: $bits = 1 << (<$bits>::BITS - 1);

            /// Wraps `value`; `None` for NaN. Negative zero becomes zero.
            pub fn new(value: $float) -> Option<Self> {
                if value.is_nan() {
                    None
                } else if value == 0.0 {
                    Some(Self(0))
                } else {
                    Some(Self(value.to_bits()))
                }
            }

            /// Wraps raw IEEE-754 bits; `None` for NaN or negative zero.
            pub fn from_bits(bits: $bits) -> Option<Self> {
                (!<$float>::from_bits(bits).is_nan() && bits != Self::SIGN).then_some(Self(bits))
            }

            /// The raw IEEE-754 bits (the canonical storage form).
            pub const fn to_bits(self) -> $bits {
                self.0
            }

            /// The float value.
            pub fn get(self) -> $float {
                <$float>::from_bits(self.0)
            }

            /// Order-preserving bits: a non-negative value flips its sign
            /// bit, a negative value flips every bit (golem-db's float
            /// order encoding).
            pub const fn order_bits(self) -> $bits {
                if self.0 & Self::SIGN == 0 {
                    self.0 ^ Self::SIGN
                } else {
                    !self.0
                }
            }

            /// Inverse of [`Self::order_bits`]; `None` for the encodings of
            /// NaN and negative zero.
            pub fn from_order_bits(order: $bits) -> Option<Self> {
                Self::from_bits(if order & Self::SIGN != 0 {
                    order ^ Self::SIGN
                } else {
                    !order
                })
            }
        }

        impl PartialOrd for $name {
            fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
                Some(self.cmp(other))
            }
        }

        impl Ord for $name {
            fn cmp(&self, other: &Self) -> std::cmp::Ordering {
                self.order_bits().cmp(&other.order_bits())
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(formatter, "{}({:?})", stringify!($name), self.get())
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.get(), formatter)
            }
        }
    };
}

float_scalar!(
    /// An IEEE-754 binary32 scalar.
    Float32,
    f32,
    u32
);
float_scalar!(
    /// An IEEE-754 binary64 scalar.
    Float64,
    f64,
    u64
);

/// Flips the sign bit of a big-endian two's complement integer: the
/// order-preserving term form, and its own inverse.
const fn flip_sign<const N: usize>(mut bytes: [u8; N]) -> [u8; N] {
    bytes[0] ^= 0x80;
    bytes
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
    /// Exactly 4 opaque bytes.
    Bytes4([u8; 4]),
    /// Exactly 8 opaque bytes.
    Bytes8([u8; 8]),
    /// Exactly 16 opaque bytes.
    Bytes16([u8; 16]),
    /// Unsigned 32-bit integer.
    U32(u32),
    /// Unsigned 128-bit integer.
    U128(u128),
    /// Signed 128-bit integer.
    I128(i128),
    /// Signed 256-bit integer in natural two's complement big-endian bytes.
    I256([u8; 32]),
    /// Signed unscaled units of 10^-4.
    Dec32(i32),
    /// Signed unscaled units of 10^-6.
    Dec64(i64),
    /// Signed unscaled units of 10^-18.
    Dec128(i128),
    /// IEEE-754 binary32 (never NaN or negative zero).
    F32(Float32),
    /// IEEE-754 binary64 (never NaN or negative zero).
    F64(Float64),
    /// Signed days since the Unix epoch.
    Date32(i32),
    /// Signed microseconds since the Unix epoch.
    Timestamp64(i64),
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

    /// An `F32` value; `None` for NaN, and `-0.0` becomes `0.0`.
    pub fn f32(value: f32) -> Option<Self> {
        Float32::new(value).map(Self::F32)
    }

    /// An `F64` value; `None` for NaN, and `-0.0` becomes `0.0`.
    pub fn f64(value: f64) -> Option<Self> {
        Float64::new(value).map(Self::F64)
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
            Self::Bytes4(_) => TypeTag::Bytes4,
            Self::Bytes8(_) => TypeTag::Bytes8,
            Self::Bytes16(_) => TypeTag::Bytes16,
            Self::U32(_) => TypeTag::U32,
            Self::U128(_) => TypeTag::U128,
            Self::I128(_) => TypeTag::I128,
            Self::I256(_) => TypeTag::I256,
            Self::Dec32(_) => TypeTag::Dec32,
            Self::Dec64(_) => TypeTag::Dec64,
            Self::Dec128(_) => TypeTag::Dec128,
            Self::F32(_) => TypeTag::F32,
            Self::F64(_) => TypeTag::F64,
            Self::Date32(_) => TypeTag::Date32,
            Self::Timestamp64(_) => TypeTag::Timestamp64,
        }
    }

    /// Length of the canonical encoding in bytes: the type's fixed width
    /// ([`TypeTag::width`]), or the byte length for `Str` and `Bytes`. This
    /// is what `CellByte` and `ReadByte` charges count.
    pub fn encoded_len(&self) -> usize {
        match self {
            Self::Str(text) => text.len(),
            Self::Bytes(bytes) => bytes.len(),
            other => other.tag().width().expect("fixed-width type"),
        }
    }

    /// Canonical order-preserving term bytes of an indexable value; `None`
    /// for `Bytes`.
    ///
    /// `Bool` is `00`/`01`; unsigned integers are big-endian; signed
    /// integers, decimals and the time types are big-endian two's complement
    /// with the sign bit flipped, so byte order equals numeric order; floats
    /// use the IEEE total-order form ([`Float64::order_bits`]); `Str` is its
    /// UTF-8 bytes, so byte order equals code point order and a prefix is a
    /// contiguous run; fixed-width bytes are themselves. Every form equals
    /// golem-db's stored value bytes for the same type.
    pub fn term_bytes(&self) -> Option<Vec<u8>> {
        Some(match self {
            Self::Bool(flag) => vec![u8::from(*flag)],
            Self::I64(value) | Self::Timestamp64(value) | Self::Dec64(value) => {
                flip_sign(value.to_be_bytes()).to_vec()
            }
            Self::U64(value) => value.to_be_bytes().to_vec(),
            Self::Str(text) => text.as_bytes().to_vec(),
            Self::Bytes(_) => return None,
            Self::I32(value) | Self::Dec32(value) | Self::Date32(value) => {
                flip_sign(value.to_be_bytes()).to_vec()
            }
            Self::U256(value) | Self::Bytes32(value) | Self::Key(value) => value.to_vec(),
            Self::Addr(value) => value.to_vec(),
            Self::Dec(value) | Self::I256(value) => flip_sign(*value).to_vec(),
            Self::Bytes4(value) => value.to_vec(),
            Self::Bytes8(value) => value.to_vec(),
            Self::Bytes16(value) => value.to_vec(),
            Self::U32(value) => value.to_be_bytes().to_vec(),
            Self::U128(value) => value.to_be_bytes().to_vec(),
            Self::I128(value) | Self::Dec128(value) => flip_sign(value.to_be_bytes()).to_vec(),
            Self::F32(value) => value.order_bits().to_be_bytes().to_vec(),
            Self::F64(value) => value.order_bits().to_be_bytes().to_vec(),
        })
    }

    /// Decodes canonical term bytes of an indexable type; `None` for `Bytes`
    /// or malformed input (wrong width, invalid UTF-8, a bool byte above 1,
    /// or the term form of a NaN or negative-zero float). The inverse of
    /// [`Self::term_bytes`].
    pub fn from_term_bytes(tag: TypeTag, bytes: &[u8]) -> Option<Self> {
        fn signed<const N: usize>(bytes: &[u8]) -> Option<[u8; N]> {
            bytes.try_into().ok().map(flip_sign)
        }
        Some(match tag {
            TypeTag::Bool => match bytes {
                [0] => Self::Bool(false),
                [1] => Self::Bool(true),
                _ => return None,
            },
            TypeTag::I64 => Self::I64(i64::from_be_bytes(signed(bytes)?)),
            TypeTag::U64 => Self::U64(u64::from_be_bytes(bytes.try_into().ok()?)),
            TypeTag::Str => return std::str::from_utf8(bytes).ok().map(Self::str),
            TypeTag::Bytes => return None,
            TypeTag::I32 => Self::I32(i32::from_be_bytes(signed(bytes)?)),
            TypeTag::U256 => Self::U256(bytes.try_into().ok()?),
            TypeTag::Bytes32 => Self::Bytes32(bytes.try_into().ok()?),
            TypeTag::Addr => Self::Addr(bytes.try_into().ok()?),
            TypeTag::Key => Self::Key(bytes.try_into().ok()?),
            TypeTag::Dec => Self::Dec(signed(bytes)?),
            TypeTag::Bytes4 => Self::Bytes4(bytes.try_into().ok()?),
            TypeTag::Bytes8 => Self::Bytes8(bytes.try_into().ok()?),
            TypeTag::Bytes16 => Self::Bytes16(bytes.try_into().ok()?),
            TypeTag::U32 => Self::U32(u32::from_be_bytes(bytes.try_into().ok()?)),
            TypeTag::U128 => Self::U128(u128::from_be_bytes(bytes.try_into().ok()?)),
            TypeTag::I128 => Self::I128(i128::from_be_bytes(signed(bytes)?)),
            TypeTag::I256 => Self::I256(signed(bytes)?),
            TypeTag::Dec32 => Self::Dec32(i32::from_be_bytes(signed(bytes)?)),
            TypeTag::Dec64 => Self::Dec64(i64::from_be_bytes(signed(bytes)?)),
            TypeTag::Dec128 => Self::Dec128(i128::from_be_bytes(signed(bytes)?)),
            TypeTag::F32 => Self::F32(Float32::from_order_bits(u32::from_be_bytes(
                bytes.try_into().ok()?,
            ))?),
            TypeTag::F64 => Self::F64(Float64::from_order_bits(u64::from_be_bytes(
                bytes.try_into().ok()?,
            ))?),
            TypeTag::Date32 => Self::Date32(i32::from_be_bytes(signed(bytes)?)),
            TypeTag::Timestamp64 => Self::Timestamp64(i64::from_be_bytes(signed(bytes)?)),
        })
    }

    /// Orders two values of the same ordered type; `None` when the types
    /// differ or the type is not ordered ([`TypeTag::is_ordered`]). Agrees
    /// with the byte order of [`Self::term_bytes`].
    pub fn cmp_ordered(&self, other: &Self) -> Option<std::cmp::Ordering> {
        match (self, other) {
            (Self::I32(left), Self::I32(right))
            | (Self::Dec32(left), Self::Dec32(right))
            | (Self::Date32(left), Self::Date32(right)) => Some(left.cmp(right)),
            (Self::U256(left), Self::U256(right)) => Some(left.cmp(right)),
            (Self::Dec(left), Self::Dec(right)) | (Self::I256(left), Self::I256(right)) => {
                Some(flip_sign(*left).cmp(&flip_sign(*right)))
            }
            (Self::I64(left), Self::I64(right))
            | (Self::Dec64(left), Self::Dec64(right))
            | (Self::Timestamp64(left), Self::Timestamp64(right)) => Some(left.cmp(right)),
            (Self::U64(left), Self::U64(right)) => Some(left.cmp(right)),
            (Self::U32(left), Self::U32(right)) => Some(left.cmp(right)),
            (Self::U128(left), Self::U128(right)) => Some(left.cmp(right)),
            (Self::I128(left), Self::I128(right)) | (Self::Dec128(left), Self::Dec128(right)) => {
                Some(left.cmp(right))
            }
            (Self::F32(left), Self::F32(right)) => Some(left.cmp(right)),
            (Self::F64(left), Self::F64(right)) => Some(left.cmp(right)),
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
    /// Retired: never produced. An empty patch is a valid no-op (golem-db
    /// semantics). The variant and its codec tag 7 remain so that receipts
    /// recorded before the change still decode.
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
