//! The canonical binary codec, version [`CODEC_VERSION`].
//!
//! Every byte string that is hashed, stored or compared between nodes is
//! produced by this codec. The rules are few and fixed:
//!
//! - Big-endian, no padding, no alignment, no self-description.
//! - `u8` is one byte; `u32` and `u64` are fixed-width big-endian; `i64` is
//!   its big-endian two's complement (`i64::to_be_bytes`).
//! - `bool` is `00` or `01`; any other byte is rejected.
//! - Byte strings and text are `u32 length ‖ bytes`; text must be UTF-8.
//! - `Option<T>` is `00` for `None` or `01 ‖ T`; any other byte is rejected.
//! - `Vec<T>` is `u32 count ‖ items`. A count whose items cannot fit in the
//!   remaining input fails at once without allocating, and a decoder never
//!   reserves more bytes up front than the remaining input holds; the
//!   vector grows past that only as items actually decode.
//! - A pair `(A, B)` is `A ‖ B`; fixed-width arrays are their raw bytes.
//! - An enum is `u8 tag ‖ fields`. Tags are explicit constants listed in
//!   `docs/block-format.md`, start at 1 and are never reused: a new variant
//!   gets a new tag, and an unknown tag is rejected.
//! - Decoding consumes exactly the input: [`decode_exact`] rejects trailing
//!   bytes, and every accepted input re-encodes to itself
//!   (`encode(decode(x)) == x`).
//!
//! Decoders enforce every invariant of the decoded type: name grammar and
//! length, record-key length 1 to 32, nonzero namespace and record ids, the
//! type ids, UTF-8, and a price schedule's class count. They do not apply
//! the engine's admission rules (value and cell limits, attribute kinds,
//! duplicate names, raw-key length); an operation that violates them decodes
//! and is then rejected by the engine or by the host's static validation.
//! Receipts are encoded without their debug ledger, so a decoded receipt has
//! `ledger: None`.
//!
//! [`digest`] is the only hash of canonical bytes: `keccak256(u8
//! len(domain) ‖ domain ‖ bytes)` with an ASCII domain such as
//! `arkiv/header/v1`. Nothing else (Debug output, JSON, physical ids,
//! snapshot ids, cache state, process tokens) is ever hashed.

use std::{fmt, sync::Arc};

pub use alloy_primitives::B256;
use alloy_primitives::Keccak256;

use crate::{
    engine::{Effect, WriteOp},
    meter::{Cost, OperationClass, Outcome, PriceSchedule, Receipt, ReceiptKind, Rejection},
    namespace::{NamespaceDescriptor, NamespaceId, NamespaceName, OwnerId, Revision},
    query::QueryFault,
    record::{
        ArgumentFault, Cell, CellKind, CellName, Limits, NameFault, RecordId,
        RecordKey, TypeTag, Value,
    },
};

/// Version of the encoding rules and of every layout in this module.
///
/// A change to any layout, tag or rule needs a new version; decoders of one
/// version never accept another version's bytes as their own.
pub const CODEC_VERSION: u32 = crate::LAYOUT_VERSION;

// Enum tags. Every table is documented in docs/block-format.md; a tag is
// never reused and never derived from a Rust discriminant.


// CellKind.
const KIND_ATTRIBUTE: u8 = 1;
const KIND_FIELD: u8 = 2;

// WriteOp.
const OP_CREATE_NAMESPACE: u8 = 1;
const OP_TRANSFER_NAMESPACE: u8 = 2;
const OP_PUT: u8 = 3;
const OP_DELETE: u8 = 4;
const OP_CREATE_RECORD: u8 = 5;
const OP_PATCH_RECORD: u8 = 6;
const OP_DELETE_RECORD: u8 = 7;

// Effect.
const EFFECT_NAMESPACE_CREATED: u8 = 1;
const EFFECT_TRANSFERRED: u8 = 2;
const EFFECT_PUT: u8 = 3;
const EFFECT_DELETED: u8 = 4;
const EFFECT_RECORD_CREATED: u8 = 5;
const EFFECT_RECORD_PATCHED: u8 = 6;
const EFFECT_RECORD_DELETED: u8 = 7;

// ArgumentFault.
const ARGUMENT_KEY_TOO_LONG: u8 = 1;
const ARGUMENT_TOO_MANY_CELLS: u8 = 2;
const ARGUMENT_DUPLICATE_CELL: u8 = 3;
const ARGUMENT_SET_AND_REMOVE_OVERLAP: u8 = 4;
const ARGUMENT_VALUE_TOO_LARGE: u8 = 5;
const ARGUMENT_BYTES_NOT_INDEXABLE: u8 = 6;
const ARGUMENT_NOT_HOST_OPERATION: u8 = 7;
const ARGUMENT_NAME_TOO_LONG: u8 = 8;

// QueryFault.
const QUERY_NO_GROUPS: u8 = 1;
const QUERY_TOO_MANY_GROUPS: u8 = 2;
const QUERY_TOO_MANY_LITERALS: u8 = 3;
const QUERY_ZERO_LIMIT: u8 = 4;
const QUERY_LIMIT_TOO_LARGE: u8 = 5;
const QUERY_NOT_INDEXABLE: u8 = 6;
const QUERY_VALUE_TOO_LARGE: u8 = 7;
const QUERY_UNBOUNDED_RANGE: u8 = 8;
const QUERY_MIXED_BOUNDS: u8 = 9;
const QUERY_NOT_ORDERED: u8 = 10;
const QUERY_PROJECTION_TOO_LARGE: u8 = 11;

// Rejection.
const REJECT_UNKNOWN_NAMESPACE: u8 = 1;
const REJECT_UNAUTHORIZED: u8 = 2;
const REJECT_STALE_REVISION: u8 = 3;
const REJECT_EXCEEDS_BLOCK_BUDGET: u8 = 4;
const REJECT_OUT_OF_BUDGET: u8 = 5;
const REJECT_COST_OVERFLOW: u8 = 6;
const REJECT_DIRECTORY_FULL: u8 = 7;
const REJECT_COUNTER_EXHAUSTED: u8 = 8;
const REJECT_ALREADY_EXISTS: u8 = 9;
const REJECT_NOT_FOUND: u8 = 10;
const REJECT_RECORD_FULL: u8 = 11;
const REJECT_INVALID_ARGUMENT: u8 = 12;
const REJECT_INVALID_QUERY: u8 = 13;
const REJECT_CURSOR_MISMATCH: u8 = 14;

// Outcome.
const OUTCOME_APPLIED: u8 = 1;
const OUTCOME_REJECTED: u8 = 2;

// ReceiptKind.
const RECEIPT_WRITE: u8 = 1;
const RECEIPT_READ: u8 = 2;

/// Why bytes are not a canonical encoding. Decoding stops at the first
/// violation and returns no partial value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// The input ends before a field, or a length or count prefix promises
    /// more than the remaining input can hold.
    Truncated {
        /// Bytes the field or prefix requires (saturated at `usize::MAX`).
        needed: usize,
        /// Bytes left in the input.
        remaining: usize,
    },
    /// Bytes are left after the value.
    TrailingBytes {
        /// Number of unconsumed bytes.
        remaining: usize,
    },
    /// A `bool` byte other than `00` or `01`.
    InvalidBool(u8),
    /// An `Option` byte other than `00` or `01`.
    InvalidOption(u8),
    /// An enum tag that the layout does not define.
    UnknownTag {
        /// The type being decoded.
        kind: &'static str,
        /// The tag found.
        tag: u8,
    },
    /// Text that is not UTF-8.
    InvalidUtf8,
    /// A namespace or cell name that violates the name grammar or length.
    InvalidName(NameFault),
    /// A record-key length byte outside 1 to [`RecordKey::MAX_LEN`].
    InvalidKeyLength(u8),
    /// A namespace or record id of zero.
    ZeroId {
        /// The id type being decoded.
        kind: &'static str,
    },
    /// A price schedule whose class count differs from
    /// [`OperationClass::ALL`].
    ClassCountMismatch {
        /// The number of classes this build prices.
        expected: usize,
        /// The count found in the input.
        found: u32,
    },
    /// An integer that does not fit its field on this platform.
    OutOfRange {
        /// The field being decoded.
        kind: &'static str,
    },
    /// A format or protocol version that this build does not support. A
    /// container that records a version refuses one it does not know
    /// rather than guessing its layout.
    UnsupportedVersion {
        /// The versioned field being decoded.
        kind: &'static str,
        /// The version found.
        version: u32,
    },
    /// Fields that decode individually but violate an invariant of the
    /// type, such as a map root paired with an impossible entry count.
    Invalid {
        /// The type being decoded.
        kind: &'static str,
        /// The invariant violated.
        reason: &'static str,
    },
}

impl fmt::Display for DecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated { needed, remaining } => write!(
                formatter,
                "truncated input: {needed} bytes needed, {remaining} left"
            ),
            Self::TrailingBytes { remaining } => {
                write!(formatter, "{remaining} trailing bytes after the value")
            }
            Self::InvalidBool(byte) => write!(formatter, "invalid bool byte {byte:#04x}"),
            Self::InvalidOption(byte) => write!(formatter, "invalid option byte {byte:#04x}"),
            Self::UnknownTag { kind, tag } => write!(formatter, "unknown {kind} tag {tag}"),
            Self::InvalidUtf8 => write!(formatter, "text is not UTF-8"),
            Self::InvalidName(fault) => write!(formatter, "invalid name: {fault}"),
            Self::InvalidKeyLength(len) => write!(formatter, "invalid record key length {len}"),
            Self::ZeroId { kind } => write!(formatter, "{kind} of zero"),
            Self::ClassCountMismatch { expected, found } => write!(
                formatter,
                "price schedule has {found} classes, expected {expected}"
            ),
            Self::OutOfRange { kind } => write!(formatter, "{kind} out of range"),
            Self::UnsupportedVersion { kind, version } => {
                write!(formatter, "unsupported {kind} version {version}")
            }
            Self::Invalid { kind, reason } => write!(formatter, "invalid {kind}: {reason}"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// A type with a canonical encoding.
pub trait Encode {
    /// Appends the canonical encoding of `self` to `encoder`.
    fn encode_to(&self, encoder: &mut Encoder);
}

/// A type that decodes from its canonical encoding.
pub trait Decode: Sized {
    /// A lower bound on the encoded length of any value, at least 1. A
    /// sequence count whose items cannot fit in the remaining input is
    /// rejected before anything is allocated.
    const MIN_ENCODED_LEN: usize = 1;

    /// Reads one value, enforcing every invariant of the type.
    fn decode_from(decoder: &mut Decoder<'_>) -> Result<Self, DecodeError>;
}

/// Appends canonical encodings to a byte buffer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Encoder {
    bytes: Vec<u8>,
}

impl Encoder {
    /// An empty buffer.
    pub const fn new() -> Self {
        Self { bytes: Vec::new() }
    }

    /// Appends one byte.
    pub fn u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    /// Appends a big-endian `u32`.
    pub fn u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    /// Appends a big-endian `u64`.
    pub fn u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    /// Appends a big-endian two's complement `i64`.
    pub fn i64(&mut self, value: i64) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    /// Appends `00` or `01`.
    pub fn bool(&mut self, value: bool) {
        self.bytes.push(u8::from(value));
    }

    /// Appends raw bytes without a length: a fixed-width field.
    pub fn fixed(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }

    /// Appends a `u32` length or count.
    ///
    /// # Panics
    ///
    /// If `len` exceeds `u32::MAX`, which no encodable value reaches.
    pub fn length(&mut self, len: usize) {
        let len = u32::try_from(len).expect("encoded lengths and counts fit in u32");
        self.u32(len);
    }

    /// Appends `u32 length ‖ bytes`.
    pub fn bytes(&mut self, bytes: &[u8]) {
        self.length(bytes.len());
        self.fixed(bytes);
    }

    /// Appends `u32 length ‖ UTF-8 bytes`.
    pub fn str(&mut self, text: &str) {
        self.bytes(text.as_bytes());
    }

    /// Appends the canonical encoding of `value`.
    pub fn put<T: Encode + ?Sized>(&mut self, value: &T) {
        value.encode_to(self);
    }

    /// The bytes written so far.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Consumes the encoder, returning its bytes.
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

/// Reads canonical encodings from a byte slice, front to back.
#[derive(Clone, Debug)]
pub struct Decoder<'a> {
    input: &'a [u8],
}

impl<'a> Decoder<'a> {
    /// Starts reading at the beginning of `input`.
    pub const fn new(input: &'a [u8]) -> Self {
        Self { input }
    }

    /// Bytes not consumed yet.
    pub const fn remaining(&self) -> usize {
        self.input.len()
    }

    /// Reads exactly `len` raw bytes.
    pub fn fixed(&mut self, len: usize) -> Result<&'a [u8], DecodeError> {
        if len > self.input.len() {
            return Err(DecodeError::Truncated {
                needed: len,
                remaining: self.input.len(),
            });
        }
        let (head, tail) = self.input.split_at(len);
        self.input = tail;
        Ok(head)
    }

    /// Reads `N` raw bytes.
    pub fn array<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        let bytes = self.fixed(N)?;
        Ok(bytes.try_into().expect("fixed returned N bytes"))
    }

    /// Reads one byte.
    pub fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.array::<1>()?[0])
    }

    /// Reads a big-endian `u32`.
    pub fn u32(&mut self) -> Result<u32, DecodeError> {
        self.array().map(u32::from_be_bytes)
    }

    /// Reads a big-endian `u64`.
    pub fn u64(&mut self) -> Result<u64, DecodeError> {
        self.array().map(u64::from_be_bytes)
    }

    /// Reads a big-endian two's complement `i64`.
    pub fn i64(&mut self) -> Result<i64, DecodeError> {
        self.array().map(i64::from_be_bytes)
    }

    /// Reads `00` or `01`.
    pub fn bool(&mut self) -> Result<bool, DecodeError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            other => Err(DecodeError::InvalidBool(other)),
        }
    }

    /// Reads a `u32` length or count, as a `usize`.
    pub fn length(&mut self) -> Result<usize, DecodeError> {
        let len = self.u32()?;
        usize::try_from(len).map_err(|_| DecodeError::OutOfRange { kind: "length" })
    }

    /// Reads `u32 length ‖ bytes`; the length is checked against the
    /// remaining input before anything is copied.
    pub fn bytes(&mut self) -> Result<&'a [u8], DecodeError> {
        let len = self.length()?;
        self.fixed(len)
    }

    /// Reads `u32 length ‖ bytes` and checks that they are UTF-8.
    pub fn str(&mut self) -> Result<&'a str, DecodeError> {
        std::str::from_utf8(self.bytes()?).map_err(|_| DecodeError::InvalidUtf8)
    }

    /// Reads one value of type `T`.
    pub fn get<T: Decode>(&mut self) -> Result<T, DecodeError> {
        T::decode_from(self)
    }

    /// Reads a `u32` count of items of type `T`, rejecting a count whose
    /// items cannot fit in the remaining input before reserving anything.
    fn count_of<T: Decode>(&mut self) -> Result<usize, DecodeError> {
        let count = self.length()?;
        let needed = count.saturating_mul(T::MIN_ENCODED_LEN.max(1));
        if needed > self.input.len() {
            return Err(DecodeError::Truncated {
                needed,
                remaining: self.input.len(),
            });
        }
        Ok(count)
    }

    /// How many of `count` items of type `T` a sequence decoder may reserve
    /// up front: never more bytes than the remaining input holds. An item
    /// can be much larger in memory than its encoding (a `WriteOp` is 6
    /// bytes encoded at the least), so a count that fits the input can
    /// still ask for many times its size; past this bound the vector grows
    /// only as items actually decode.
    fn reservable<T>(&self, count: usize) -> usize {
        count.min(self.input.len() / size_of::<T>().max(1))
    }

    /// Succeeds only when the whole input was consumed.
    pub fn finish(self) -> Result<(), DecodeError> {
        if self.input.is_empty() {
            Ok(())
        } else {
            Err(DecodeError::TrailingBytes {
                remaining: self.input.len(),
            })
        }
    }
}

/// The canonical encoding of `value`.
pub fn encode<T: Encode + ?Sized>(value: &T) -> Vec<u8> {
    let mut encoder = Encoder::new();
    value.encode_to(&mut encoder);
    encoder.into_bytes()
}

/// Decodes one value that must span all of `bytes`; trailing bytes are an
/// error.
pub fn decode_exact<T: Decode>(bytes: &[u8]) -> Result<T, DecodeError> {
    let mut decoder = Decoder::new(bytes);
    let value = T::decode_from(&mut decoder)?;
    decoder.finish()?;
    Ok(value)
}

/// Domain-separated hash of canonical bytes:
/// `keccak256(u8 len(domain) ‖ domain ‖ bytes)`.
///
/// Domains name the hashed object and its layout version, by convention
/// `arkiv/<object>/v<version>` (for example `arkiv/header/v1`), so equal
/// bytes of different kinds never share a hash.
///
/// # Panics
///
/// If `domain` is empty, longer than 255 bytes, or contains a byte that is
/// not printable ASCII (`0x21..=0x7e`). Domains are constants, so this is a
/// programming error, never a data error.
pub fn digest(domain: &str, bytes: &[u8]) -> B256 {
    let valid = !domain.is_empty() && domain.bytes().all(|byte| byte.is_ascii_graphic());
    let len = u8::try_from(domain.len())
        .ok()
        .filter(|_| valid)
        .unwrap_or_else(|| panic!("invalid digest domain {domain:?}"));
    let mut hasher = Keccak256::new();
    hasher.update([len]);
    hasher.update(domain.as_bytes());
    hasher.update(bytes);
    hasher.finalize()
}

// ---------------------------------------------------------------------------
// Primitives and containers.

impl Encode for u8 {
    fn encode_to(&self, encoder: &mut Encoder) {
        encoder.u8(*self);
    }
}

impl Decode for u8 {
    fn decode_from(decoder: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        decoder.u8()
    }
}

impl Encode for u32 {
    fn encode_to(&self, encoder: &mut Encoder) {
        encoder.u32(*self);
    }
}

impl Decode for u32 {
    const MIN_ENCODED_LEN: usize = 4;

    fn decode_from(decoder: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        decoder.u32()
    }
}

impl Encode for u64 {
    fn encode_to(&self, encoder: &mut Encoder) {
        encoder.u64(*self);
    }
}

impl Decode for u64 {
    const MIN_ENCODED_LEN: usize = 8;

    fn decode_from(decoder: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        decoder.u64()
    }
}

impl Encode for i64 {
    fn encode_to(&self, encoder: &mut Encoder) {
        encoder.i64(*self);
    }
}

impl Decode for i64 {
    const MIN_ENCODED_LEN: usize = 8;

    fn decode_from(decoder: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        decoder.i64()
    }
}

impl Encode for bool {
    fn encode_to(&self, encoder: &mut Encoder) {
        encoder.bool(*self);
    }
}

impl Decode for bool {
    fn decode_from(decoder: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        decoder.bool()
    }
}

/// Raw bytes, no length: fixed-width identifiers such as 32-byte request ids.
impl<const N: usize> Encode for [u8; N] {
    fn encode_to(&self, encoder: &mut Encoder) {
        const { assert!(N > 0, "zero-width arrays have no encoding") };
        encoder.fixed(self);
    }
}

impl<const N: usize> Decode for [u8; N] {
    const MIN_ENCODED_LEN: usize = N;

    fn decode_from(decoder: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        const { assert!(N > 0, "zero-width arrays have no encoding") };
        decoder.array()
    }
}

/// A 32-byte hash: its raw bytes.
impl Encode for B256 {
    fn encode_to(&self, encoder: &mut Encoder) {
        encoder.fixed(self.as_slice());
    }
}

impl Decode for B256 {
    const MIN_ENCODED_LEN: usize = 32;

    fn decode_from(decoder: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        decoder.array().map(Self::new)
    }
}

impl Encode for str {
    fn encode_to(&self, encoder: &mut Encoder) {
        encoder.str(self);
    }
}

impl Encode for String {
    fn encode_to(&self, encoder: &mut Encoder) {
        encoder.str(self);
    }
}

impl Decode for String {
    const MIN_ENCODED_LEN: usize = 4;

    fn decode_from(decoder: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        decoder.str().map(str::to_owned)
    }
}

/// `u32 count ‖ items`.
impl<T: Encode> Encode for [T] {
    fn encode_to(&self, encoder: &mut Encoder) {
        encoder.length(self.len());
        for item in self {
            item.encode_to(encoder);
        }
    }
}

impl<T: Encode> Encode for Vec<T> {
    fn encode_to(&self, encoder: &mut Encoder) {
        self.as_slice().encode_to(encoder);
    }
}

impl<T: Decode> Decode for Vec<T> {
    const MIN_ENCODED_LEN: usize = 4;

    fn decode_from(decoder: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        let count = decoder.count_of::<T>()?;
        let mut items = Self::with_capacity(decoder.reservable::<T>(count));
        for _ in 0..count {
            items.push(T::decode_from(decoder)?);
        }
        Ok(items)
    }
}

/// `00` or `01 ‖ value`.
impl<T: Encode> Encode for Option<T> {
    fn encode_to(&self, encoder: &mut Encoder) {
        match self {
            None => encoder.u8(0),
            Some(value) => {
                encoder.u8(1);
                value.encode_to(encoder);
            }
        }
    }
}

impl<T: Decode> Decode for Option<T> {
    fn decode_from(decoder: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        match decoder.u8()? {
            0 => Ok(None),
            1 => T::decode_from(decoder).map(Some),
            other => Err(DecodeError::InvalidOption(other)),
        }
    }
}

/// `first ‖ second`.
impl<A: Encode, B: Encode> Encode for (A, B) {
    fn encode_to(&self, encoder: &mut Encoder) {
        self.0.encode_to(encoder);
        self.1.encode_to(encoder);
    }
}

impl<A: Decode, B: Decode> Decode for (A, B) {
    const MIN_ENCODED_LEN: usize = A::MIN_ENCODED_LEN.saturating_add(B::MIN_ENCODED_LEN);

    fn decode_from(decoder: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        let first = A::decode_from(decoder)?;
        let second = B::decode_from(decoder)?;
        Ok((first, second))
    }
}

// ---------------------------------------------------------------------------
// Identities and names.

/// The ordinal as 8 big-endian bytes; zero is rejected.
impl Encode for NamespaceId {
    fn encode_to(&self, encoder: &mut Encoder) {
        encoder.fixed(&self.to_bytes());
    }
}

impl Decode for NamespaceId {
    const MIN_ENCODED_LEN: usize = Self::ENCODED_LEN;

    fn decode_from(decoder: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        Self::from_bytes(decoder.array()?).ok_or(DecodeError::ZeroId {
            kind: "NamespaceId",
        })
    }
}

/// The 20 raw principal bytes.
impl Encode for OwnerId {
    fn encode_to(&self, encoder: &mut Encoder) {
        encoder.fixed(self.as_bytes());
    }
}

impl Decode for OwnerId {
    const MIN_ENCODED_LEN: usize = 20;

    fn decode_from(decoder: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        decoder.array().map(Self::new)
    }
}

/// `u64`.
impl Encode for Revision {
    fn encode_to(&self, encoder: &mut Encoder) {
        encoder.u64(self.0);
    }
}

impl Decode for Revision {
    const MIN_ENCODED_LEN: usize = 8;

    fn decode_from(decoder: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        decoder.u64().map(Self)
    }
}

/// Text, validated against the name grammar.
impl Encode for NamespaceName {
    fn encode_to(&self, encoder: &mut Encoder) {
        encoder.str(self.as_str());
    }
}

impl Decode for NamespaceName {
    const MIN_ENCODED_LEN: usize = 5;

    fn decode_from(decoder: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        Self::new(decoder.str()?).map_err(DecodeError::InvalidName)
    }
}

/// `id ‖ name ‖ owner ‖ revision`.
impl Encode for NamespaceDescriptor {
    fn encode_to(&self, encoder: &mut Encoder) {
        encoder.put(&self.id);
        encoder.put(&self.name);
        encoder.put(&self.owner);
        encoder.put(&self.revision);
    }
}

impl Decode for NamespaceDescriptor {
    const MIN_ENCODED_LEN: usize = 8 + 5 + 20 + 8;

    fn decode_from(decoder: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            id: decoder.get()?,
            name: decoder.get()?,
            owner: decoder.get()?,
            revision: decoder.get()?,
        })
    }
}

/// Text, validated against the name grammar.
impl Encode for CellName {
    fn encode_to(&self, encoder: &mut Encoder) {
        encoder.str(self.as_str());
    }
}

impl Decode for CellName {
    const MIN_ENCODED_LEN: usize = 5;

    fn decode_from(decoder: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        Self::new(decoder.str()?).map_err(DecodeError::InvalidName)
    }
}

/// `u8 len ‖ bytes`, length 1 to 32. This is the codec's variable-width
/// form; the fixed 33-byte [`RecordKey::encoded`] form is for ordered keys.
impl Encode for RecordKey {
    fn encode_to(&self, encoder: &mut Encoder) {
        let bytes = self.as_bytes();
        encoder.u8(u8::try_from(bytes.len()).expect("record keys are at most 32 bytes"));
        encoder.fixed(bytes);
    }
}

impl Decode for RecordKey {
    const MIN_ENCODED_LEN: usize = 2;

    fn decode_from(decoder: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        let len = decoder.u8()?;
        if !(1..=Self::MAX_LEN).contains(&usize::from(len)) {
            return Err(DecodeError::InvalidKeyLength(len));
        }
        let bytes = decoder.fixed(usize::from(len))?;
        Ok(Self::new(bytes).expect("the length was checked"))
    }
}

/// The ordinal as a `u64`; zero is rejected.
impl Encode for RecordId {
    fn encode_to(&self, encoder: &mut Encoder) {
        encoder.u64(self.ordinal());
    }
}

impl Decode for RecordId {
    const MIN_ENCODED_LEN: usize = 8;

    fn decode_from(decoder: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        Self::from_ordinal(decoder.u64()?).ok_or(DecodeError::ZeroId { kind: "RecordId" })
    }
}

// ---------------------------------------------------------------------------
// Typed values and cells.

/// The type id as one byte ([`TypeTag::id`], golem-db's cell type id).
impl Encode for TypeTag {
    fn encode_to(&self, encoder: &mut Encoder) {
        encoder.u8(self.id());
    }
}

impl Decode for TypeTag {
    fn decode_from(decoder: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        let tag = decoder.u8()?;
        TypeTag::from_id(tag).ok_or(DecodeError::UnknownTag {
            kind: "TypeTag",
            tag,
        })
    }
}

/// Decodes the payload of a value of type `tag`: `Str` and `Bytes` are
/// length-prefixed, every other type is its fixed-width term bytes.
pub(crate) fn decode_payload(tag: TypeTag, decoder: &mut Decoder<'_>) -> Result<Value, DecodeError> {
    match tag {
        TypeTag::Str => decoder.str().map(|text| Value::Str(Arc::from(text))),
        TypeTag::Bytes => decoder.bytes().map(|bytes| Value::Bytes(Arc::from(bytes))),
        fixed => {
            let width = fixed.width().expect("fixed-width type");
            let bytes = decoder.fixed(width)?;
            Value::from_term_bytes(fixed, bytes).ok_or(DecodeError::Invalid {
                kind: "Value",
                reason: "not a canonical value of its type",
            })
        }
    }
}

/// `type id ‖ payload`. `Str` and `Bytes` are length-prefixed; every other
/// type is its fixed-width term bytes ([`Value::term_bytes`]), which are
/// golem-db's stored value bytes: big-endian, signed integers, decimals and
/// time types with the sign bit flipped, floats in the IEEE total-order
/// form (never NaN or negative zero), `Bool` as `00`/`01`.
impl Encode for Value {
    fn encode_to(&self, encoder: &mut Encoder) {
        encoder.u8(self.tag().id());
        match self {
            Self::Str(text) => encoder.str(text),
            Self::Bytes(bytes) => encoder.bytes(bytes),
            fixed => encoder.fixed(&fixed.term_bytes().expect("indexable")),
        }
    }
}

impl Decode for Value {
    const MIN_ENCODED_LEN: usize = 2;

    fn decode_from(decoder: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        let tag = decoder.u8()?;
        let tag = TypeTag::from_id(tag).ok_or(DecodeError::UnknownTag { kind: "Value", tag })?;
        decode_payload(tag, decoder)
    }
}

/// One byte: attribute 1, field 2.
impl Encode for CellKind {
    fn encode_to(&self, encoder: &mut Encoder) {
        encoder.u8(match self {
            Self::Attribute => KIND_ATTRIBUTE,
            Self::Field => KIND_FIELD,
        });
    }
}

impl Decode for CellKind {
    fn decode_from(decoder: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        match decoder.u8()? {
            KIND_ATTRIBUTE => Ok(Self::Attribute),
            KIND_FIELD => Ok(Self::Field),
            tag => Err(DecodeError::UnknownTag {
                kind: "CellKind",
                tag,
            }),
        }
    }
}

/// `kind ‖ value`.
impl Encode for Cell {
    fn encode_to(&self, encoder: &mut Encoder) {
        encoder.put(&self.kind);
        encoder.put(&self.value);
    }
}

impl Decode for Cell {
    const MIN_ENCODED_LEN: usize = 3;

    fn decode_from(decoder: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            kind: decoder.get()?,
            value: decoder.get()?,
        })
    }
}

// ---------------------------------------------------------------------------
// Writes and their effects.

/// `tag ‖ fields` in declaration order; raw keys and values are
/// length-prefixed bytes, record keys `u8 len ‖ bytes`, cell lists
/// `Vec<(CellName, Cell)>` in the given order.
impl Encode for WriteOp {
    fn encode_to(&self, encoder: &mut Encoder) {
        match self {
            Self::CreateNamespace { name } => {
                encoder.u8(OP_CREATE_NAMESPACE);
                encoder.put(name);
            }
            Self::TransferNamespace {
                namespace,
                expected_revision,
                new_owner,
            } => {
                encoder.u8(OP_TRANSFER_NAMESPACE);
                encoder.put(namespace);
                encoder.put(expected_revision);
                encoder.put(new_owner);
            }
            Self::Put {
                namespace,
                key,
                value,
            } => {
                encoder.u8(OP_PUT);
                encoder.put(namespace);
                encoder.bytes(key);
                encoder.bytes(value);
            }
            Self::Delete { namespace, key } => {
                encoder.u8(OP_DELETE);
                encoder.put(namespace);
                encoder.bytes(key);
            }
            Self::CreateRecord {
                namespace,
                key,
                cells,
            } => {
                encoder.u8(OP_CREATE_RECORD);
                encoder.put(namespace);
                encoder.put(key);
                encoder.put(cells);
            }
            Self::PatchRecord {
                namespace,
                key,
                set,
                remove,
            } => {
                encoder.u8(OP_PATCH_RECORD);
                encoder.put(namespace);
                encoder.put(key);
                encoder.put(set);
                encoder.put(remove);
            }
            Self::DeleteRecord { namespace, key } => {
                encoder.u8(OP_DELETE_RECORD);
                encoder.put(namespace);
                encoder.put(key);
            }
        }
    }
}

impl Decode for WriteOp {
    /// The shortest operation: `CreateNamespace` with a one-character name
    /// (tag, `u32` length, one byte).
    const MIN_ENCODED_LEN: usize = 1 + 5;

    fn decode_from(decoder: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        let op = match decoder.u8()? {
            OP_CREATE_NAMESPACE => Self::CreateNamespace {
                name: decoder.get()?,
            },
            OP_TRANSFER_NAMESPACE => Self::TransferNamespace {
                namespace: decoder.get()?,
                expected_revision: decoder.get()?,
                new_owner: decoder.get()?,
            },
            OP_PUT => Self::Put {
                namespace: decoder.get()?,
                key: decoder.bytes()?.to_vec(),
                value: decoder.bytes()?.to_vec(),
            },
            OP_DELETE => Self::Delete {
                namespace: decoder.get()?,
                key: decoder.bytes()?.to_vec(),
            },
            OP_CREATE_RECORD => Self::CreateRecord {
                namespace: decoder.get()?,
                key: decoder.get()?,
                cells: decoder.get()?,
            },
            OP_PATCH_RECORD => Self::PatchRecord {
                namespace: decoder.get()?,
                key: decoder.get()?,
                set: decoder.get()?,
                remove: decoder.get()?,
            },
            OP_DELETE_RECORD => Self::DeleteRecord {
                namespace: decoder.get()?,
                key: decoder.get()?,
            },
            tag => {
                return Err(DecodeError::UnknownTag {
                    kind: "WriteOp",
                    tag,
                });
            }
        };
        Ok(op)
    }
}

/// `tag ‖ fields`.
impl Encode for Effect {
    fn encode_to(&self, encoder: &mut Encoder) {
        match self {
            Self::BalanceTransferred { to, amount } => {
                encoder.u8(8);
                encoder.put(to);
                encoder.fixed(amount);
            }
            Self::NamespaceCreated(id) => {
                encoder.u8(EFFECT_NAMESPACE_CREATED);
                encoder.put(id);
            }
            Self::Transferred { revision } => {
                encoder.u8(EFFECT_TRANSFERRED);
                encoder.put(revision);
            }
            Self::Put { replaced } => {
                encoder.u8(EFFECT_PUT);
                encoder.bool(*replaced);
            }
            Self::Deleted { existed } => {
                encoder.u8(EFFECT_DELETED);
                encoder.bool(*existed);
            }
            Self::RecordCreated { id } => {
                encoder.u8(EFFECT_RECORD_CREATED);
                encoder.put(id);
            }
            Self::RecordPatched { id } => {
                encoder.u8(EFFECT_RECORD_PATCHED);
                encoder.put(id);
            }
            Self::RecordDeleted { id } => {
                encoder.u8(EFFECT_RECORD_DELETED);
                encoder.put(id);
            }
        }
    }
}

impl Decode for Effect {
    const MIN_ENCODED_LEN: usize = 2;

    fn decode_from(decoder: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        let effect = match decoder.u8()? {
            8 => Self::BalanceTransferred {
                to: decoder.get()?,
                amount: decoder.array()?,
            },
            EFFECT_NAMESPACE_CREATED => Self::NamespaceCreated(decoder.get()?),
            EFFECT_TRANSFERRED => Self::Transferred {
                revision: decoder.get()?,
            },
            EFFECT_PUT => Self::Put {
                replaced: decoder.bool()?,
            },
            EFFECT_DELETED => Self::Deleted {
                existed: decoder.bool()?,
            },
            EFFECT_RECORD_CREATED => Self::RecordCreated { id: decoder.get()? },
            EFFECT_RECORD_PATCHED => Self::RecordPatched { id: decoder.get()? },
            EFFECT_RECORD_DELETED => Self::RecordDeleted { id: decoder.get()? },
            tag => {
                return Err(DecodeError::UnknownTag {
                    kind: "Effect",
                    tag,
                });
            }
        };
        Ok(effect)
    }
}

// ---------------------------------------------------------------------------
// Rejections, outcomes and receipts.

/// One tag byte.
impl Encode for ArgumentFault {
    fn encode_to(&self, encoder: &mut Encoder) {
        encoder.u8(match self {
            Self::KeyTooLong => ARGUMENT_KEY_TOO_LONG,
            Self::TooManyCells => ARGUMENT_TOO_MANY_CELLS,
            Self::DuplicateCell => ARGUMENT_DUPLICATE_CELL,
            Self::SetAndRemoveOverlap => ARGUMENT_SET_AND_REMOVE_OVERLAP,
            Self::ValueTooLarge => ARGUMENT_VALUE_TOO_LARGE,
            Self::BytesNotIndexable => ARGUMENT_BYTES_NOT_INDEXABLE,
            Self::NotHostOperation => ARGUMENT_NOT_HOST_OPERATION,
            Self::NameTooLong => ARGUMENT_NAME_TOO_LONG,
        });
    }
}

impl Decode for ArgumentFault {
    fn decode_from(decoder: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        match decoder.u8()? {
            ARGUMENT_KEY_TOO_LONG => Ok(Self::KeyTooLong),
            ARGUMENT_TOO_MANY_CELLS => Ok(Self::TooManyCells),
            ARGUMENT_DUPLICATE_CELL => Ok(Self::DuplicateCell),
            ARGUMENT_SET_AND_REMOVE_OVERLAP => Ok(Self::SetAndRemoveOverlap),
            ARGUMENT_VALUE_TOO_LARGE => Ok(Self::ValueTooLarge),
            ARGUMENT_BYTES_NOT_INDEXABLE => Ok(Self::BytesNotIndexable),
            ARGUMENT_NOT_HOST_OPERATION => Ok(Self::NotHostOperation),
            ARGUMENT_NAME_TOO_LONG => Ok(Self::NameTooLong),
            tag => Err(DecodeError::UnknownTag {
                kind: "ArgumentFault",
                tag,
            }),
        }
    }
}

/// One tag byte.
impl Encode for QueryFault {
    fn encode_to(&self, encoder: &mut Encoder) {
        encoder.u8(match self {
            Self::NoGroups => QUERY_NO_GROUPS,
            Self::TooManyGroups => QUERY_TOO_MANY_GROUPS,
            Self::TooManyLiterals => QUERY_TOO_MANY_LITERALS,
            Self::ZeroLimit => QUERY_ZERO_LIMIT,
            Self::LimitTooLarge => QUERY_LIMIT_TOO_LARGE,
            Self::NotIndexable => QUERY_NOT_INDEXABLE,
            Self::ValueTooLarge => QUERY_VALUE_TOO_LARGE,
            Self::UnboundedRange => QUERY_UNBOUNDED_RANGE,
            Self::MixedBounds => QUERY_MIXED_BOUNDS,
            Self::NotOrdered => QUERY_NOT_ORDERED,
            Self::ProjectionTooLarge => QUERY_PROJECTION_TOO_LARGE,
        });
    }
}

impl Decode for QueryFault {
    fn decode_from(decoder: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        match decoder.u8()? {
            QUERY_NO_GROUPS => Ok(Self::NoGroups),
            QUERY_TOO_MANY_GROUPS => Ok(Self::TooManyGroups),
            QUERY_TOO_MANY_LITERALS => Ok(Self::TooManyLiterals),
            QUERY_ZERO_LIMIT => Ok(Self::ZeroLimit),
            QUERY_LIMIT_TOO_LARGE => Ok(Self::LimitTooLarge),
            QUERY_NOT_INDEXABLE => Ok(Self::NotIndexable),
            QUERY_VALUE_TOO_LARGE => Ok(Self::ValueTooLarge),
            QUERY_UNBOUNDED_RANGE => Ok(Self::UnboundedRange),
            QUERY_MIXED_BOUNDS => Ok(Self::MixedBounds),
            QUERY_NOT_ORDERED => Ok(Self::NotOrdered),
            QUERY_PROJECTION_TOO_LARGE => Ok(Self::ProjectionTooLarge),
            tag => Err(DecodeError::UnknownTag {
                kind: "QueryFault",
                tag,
            }),
        }
    }
}

/// `u64` units.
impl Encode for Cost {
    fn encode_to(&self, encoder: &mut Encoder) {
        encoder.u64(self.units());
    }
}

impl Decode for Cost {
    const MIN_ENCODED_LEN: usize = 8;

    fn decode_from(decoder: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        decoder.u64().map(Self::new)
    }
}

/// `tag ‖ fields`.
impl Encode for Rejection {
    fn encode_to(&self, encoder: &mut Encoder) {
        match self {
            Self::UnknownNamespace => encoder.u8(REJECT_UNKNOWN_NAMESPACE),
            Self::Unauthorized => encoder.u8(REJECT_UNAUTHORIZED),
            Self::StaleRevision { expected, current } => {
                encoder.u8(REJECT_STALE_REVISION);
                encoder.put(expected);
                encoder.put(current);
            }
            Self::ExceedsBlockBudget { remaining } => {
                encoder.u8(REJECT_EXCEEDS_BLOCK_BUDGET);
                encoder.put(remaining);
            }
            Self::OutOfBudget => encoder.u8(REJECT_OUT_OF_BUDGET),
            Self::CostOverflow => encoder.u8(REJECT_COST_OVERFLOW),
            Self::DirectoryFull => encoder.u8(REJECT_DIRECTORY_FULL),
            Self::CounterExhausted => encoder.u8(REJECT_COUNTER_EXHAUSTED),
            Self::AlreadyExists => encoder.u8(REJECT_ALREADY_EXISTS),
            Self::NotFound => encoder.u8(REJECT_NOT_FOUND),
            Self::RecordFull => encoder.u8(REJECT_RECORD_FULL),
            Self::InvalidArgument(fault) => {
                encoder.u8(REJECT_INVALID_ARGUMENT);
                encoder.put(fault);
            }
            Self::InvalidQuery(fault) => {
                encoder.u8(REJECT_INVALID_QUERY);
                encoder.put(fault);
            }
            Self::CursorMismatch => encoder.u8(REJECT_CURSOR_MISMATCH),
        }
    }
}

impl Decode for Rejection {
    fn decode_from(decoder: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        let rejection = match decoder.u8()? {
            REJECT_UNKNOWN_NAMESPACE => Self::UnknownNamespace,
            REJECT_UNAUTHORIZED => Self::Unauthorized,
            REJECT_STALE_REVISION => Self::StaleRevision {
                expected: decoder.get()?,
                current: decoder.get()?,
            },
            REJECT_EXCEEDS_BLOCK_BUDGET => Self::ExceedsBlockBudget {
                remaining: decoder.get()?,
            },
            REJECT_OUT_OF_BUDGET => Self::OutOfBudget,
            REJECT_COST_OVERFLOW => Self::CostOverflow,
            REJECT_DIRECTORY_FULL => Self::DirectoryFull,
            REJECT_COUNTER_EXHAUSTED => Self::CounterExhausted,
            REJECT_ALREADY_EXISTS => Self::AlreadyExists,
            REJECT_NOT_FOUND => Self::NotFound,
            REJECT_RECORD_FULL => Self::RecordFull,
            REJECT_INVALID_ARGUMENT => Self::InvalidArgument(decoder.get()?),
            REJECT_INVALID_QUERY => Self::InvalidQuery(decoder.get()?),
            REJECT_CURSOR_MISMATCH => Self::CursorMismatch,
            tag => {
                return Err(DecodeError::UnknownTag {
                    kind: "Rejection",
                    tag,
                });
            }
        };
        Ok(rejection)
    }
}

/// `tag ‖ fields`: applied 1, rejected 2 followed by the rejection.
impl Encode for Outcome {
    fn encode_to(&self, encoder: &mut Encoder) {
        match self {
            Self::Applied => encoder.u8(OUTCOME_APPLIED),
            Self::Rejected(reason) => {
                encoder.u8(OUTCOME_REJECTED);
                encoder.put(reason);
            }
        }
    }
}

impl Decode for Outcome {
    fn decode_from(decoder: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        match decoder.u8()? {
            OUTCOME_APPLIED => Ok(Self::Applied),
            OUTCOME_REJECTED => decoder.get().map(Self::Rejected),
            tag => Err(DecodeError::UnknownTag {
                kind: "Outcome",
                tag,
            }),
        }
    }
}

/// One byte: write 1, read 2.
impl Encode for ReceiptKind {
    fn encode_to(&self, encoder: &mut Encoder) {
        encoder.u8(match self {
            Self::Write => RECEIPT_WRITE,
            Self::Read => RECEIPT_READ,
        });
    }
}

impl Decode for ReceiptKind {
    fn decode_from(decoder: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        match decoder.u8()? {
            RECEIPT_WRITE => Ok(Self::Write),
            RECEIPT_READ => Ok(Self::Read),
            tag => Err(DecodeError::UnknownTag {
                kind: "ReceiptKind",
                tag,
            }),
        }
    }
}

/// `kind ‖ model_version u32 ‖ schedule_id u32 ‖ spent u64 ‖ outcome`.
///
/// The debug ledger is never encoded: two receipts that differ only in
/// their ledgers have the same encoding, and a decoded receipt has none.
impl Encode for Receipt {
    fn encode_to(&self, encoder: &mut Encoder) {
        encoder.put(&self.kind);
        encoder.u32(self.model_version);
        encoder.u32(self.schedule_id);
        encoder.put(&self.spent);
        encoder.put(&self.outcome);
    }
}

impl Decode for Receipt {
    const MIN_ENCODED_LEN: usize = 1 + 4 + 4 + 8 + 1;

    fn decode_from(decoder: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            kind: decoder.get()?,
            model_version: decoder.u32()?,
            schedule_id: decoder.u32()?,
            spent: decoder.get()?,
            outcome: decoder.get()?,
            ledger: None,
        })
    }
}

/// `id u32 ‖ u32 class count ‖ one u64 weight per class` in
/// [`OperationClass::ALL`] order. Decoding rejects a count other than
/// `OperationClass::ALL.len()` (31 under counting-rule version 3).
impl Encode for PriceSchedule {
    fn encode_to(&self, encoder: &mut Encoder) {
        encoder.u32(self.id());
        encoder.length(OperationClass::ALL.len());
        for class in OperationClass::ALL {
            encoder.u64(self.price(class));
        }
    }
}

impl Decode for PriceSchedule {
    const MIN_ENCODED_LEN: usize = 4 + 4 + 8 * OperationClass::ALL.len();

    fn decode_from(decoder: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        let id = decoder.u32()?;
        let count = decoder.u32()?;
        if usize::try_from(count).ok() != Some(OperationClass::ALL.len()) {
            return Err(DecodeError::ClassCountMismatch {
                expected: OperationClass::ALL.len(),
                found: count,
            });
        }
        let mut weights = [0u64; OperationClass::ALL.len()];
        for weight in &mut weights {
            *weight = decoder.u64()?;
        }
        Ok(Self::new(id, weights))
    }
}

/// Every field as a `u64`, in declaration order: `namespace_cap`,
/// `max_cells`, `max_cell_name_len`, `max_str_len`, `max_bytes_len`,
/// `max_indexed_len`, `max_page`, `max_groups`, `max_literals`.
impl Encode for Limits {
    fn encode_to(&self, encoder: &mut Encoder) {
        let wide = |value: usize| u64::try_from(value).expect("usize fits in u64");
        encoder.u64(self.namespace_cap);
        for value in [
            self.max_cells,
            self.max_cell_name_len,
            self.max_str_len,
            self.max_bytes_len,
            self.max_indexed_len,
            self.max_page,
            self.max_groups,
            self.max_literals,
        ] {
            encoder.u64(wide(value));
        }
    }
}

impl Decode for Limits {
    const MIN_ENCODED_LEN: usize = 9 * 8;

    fn decode_from(decoder: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        let namespace_cap = decoder.u64()?;
        let mut narrow = || {
            decoder.u64().and_then(|value| {
                usize::try_from(value).map_err(|_| DecodeError::OutOfRange { kind: "Limits" })
            })
        };
        // Fields are evaluated in the order written, which is the layout order.
        Ok(Self {
            namespace_cap,
            max_cells: narrow()?,
            max_cell_name_len: narrow()?,
            max_str_len: narrow()?,
            max_bytes_len: narrow()?,
            max_indexed_len: narrow()?,
            max_page: narrow()?,
            max_groups: narrow()?,
            max_literals: narrow()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_tags_are_the_type_ids() {
        for tag in TypeTag::ALL {
            assert_eq!(encode(&tag), [tag.id()], "{tag}");
            assert_eq!(TypeTag::from_id(tag.id()), Some(tag));
        }
    }

    /// A hostile count that passes the item check (one-byte-minimum items
    /// filling the input) still reserves no more bytes than the input
    /// holds, however large the item is in memory.
    #[test]
    fn sequences_reserve_at_most_the_remaining_input() {
        fn reserved_bytes<T>(decoder: &Decoder<'_>, count: usize) -> usize {
            decoder.reservable::<T>(count) * size_of::<T>()
        }
        let input = vec![0; 4096];
        let decoder = Decoder::new(&input);
        for count in [0, 1, 100, 4096, usize::MAX] {
            assert!(reserved_bytes::<WriteOp>(&decoder, count) <= 4096);
            assert!(reserved_bytes::<Rejection>(&decoder, count) <= 4096);
            assert!(reserved_bytes::<Outcome>(&decoder, count) <= 4096);
            assert!(reserved_bytes::<Value>(&decoder, count) <= 4096);
            assert!(reserved_bytes::<(CellName, Cell)>(&decoder, count) <= 4096);
            assert!(reserved_bytes::<Receipt>(&decoder, count) <= 4096);
        }
        // Items no larger in memory than one byte reserve the whole count.
        assert_eq!(decoder.reservable::<u8>(4096), 4096);
        assert_eq!(decoder.reservable::<u8>(4097), 4096);
        assert_eq!(Decoder::new(&[]).reservable::<WriteOp>(1), 0);
    }
}
