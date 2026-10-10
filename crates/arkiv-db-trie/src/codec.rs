//! The key and value contracts of a [`MerkleMap`](crate::MerkleMap).

use std::{borrow::Cow, fmt};

use crate::stage::ObjectSink;

/// A key or value encoding that could not be decoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodecError {
    reason: Cow<'static, str>,
}

impl CodecError {
    /// Creates an error with a short description of what was wrong.
    pub fn new(reason: impl Into<Cow<'static, str>>) -> Self {
        Self {
            reason: reason.into(),
        }
    }

    /// Returns the description given at construction.
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl fmt::Display for CodecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.reason)
    }
}

impl std::error::Error for CodecError {}

/// A fixed-length, order-preserving key encoding.
///
/// Every key of a type encodes to exactly [`LEN`](Self::LEN) bytes, and the
/// map orders its entries by unsigned bytewise comparison of those bytes, so
/// an implementation whose encoding preserves the intended order gets
/// iteration in that order. Encoding must be injective and `decode` its
/// inverse on every encoding `encode` produces. The map panics if `encode`
/// returns a different length: that is a bug in the implementation, not a
/// data error.
pub trait TrieKey: Sized {
    /// The encoded length in bytes; at least one.
    const LEN: usize;

    /// Returns the `LEN`-byte encoding.
    fn encode(&self) -> Vec<u8>;

    /// Decodes a `LEN`-byte encoding produced by [`encode`](Self::encode).
    fn decode(bytes: &[u8]) -> Result<Self, CodecError>;
}

impl<const N: usize> TrieKey for [u8; N] {
    const LEN: usize = N;

    fn encode(&self) -> Vec<u8> {
        self.to_vec()
    }

    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        bytes
            .try_into()
            .map_err(|_| CodecError::new("key has the wrong length"))
    }
}

impl TrieKey for u64 {
    const LEN: usize = 8;

    /// Big-endian, so numeric order is byte order.
    fn encode(&self) -> Vec<u8> {
        self.to_be_bytes().to_vec()
    }

    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        let bytes: [u8; 8] = bytes
            .try_into()
            .map_err(|_| CodecError::new("u64 key is not 8 bytes"))?;
        Ok(Self::from_be_bytes(bytes))
    }
}

/// A leaf value with a canonical byte encoding.
///
/// `encode` is the leaf value that enters the node encoding and therefore
/// the root hash; it must be canonical (equal values encode equally) and
/// deterministic, and `decode` must accept exactly the encodings `encode`
/// produces. A leaf value is never empty: [`MerkleMap::insert`] rejects a
/// value that encodes to nothing with
/// [`TrieError::EmptyValue`](crate::TrieError::EmptyValue), and the decoder
/// rejects stored leaves with empty values. (The map panics if an encoding
/// it hashes is empty anyway, which only a nondeterministic `encode` can
/// cause.)
///
/// [`MerkleMap::insert`]: crate::MerkleMap::insert
///
/// A value may contain nested maps (for example a posting map inside a term
/// entry). Such a value encodes the nested map's root hash and length, which
/// is the hashing hook: hashing the outer map hashes the nested map's dirty
/// nodes on demand. [`stage_nested`](Self::stage_nested) is the staging
/// hook: the staging walk calls it for every leaf value of a new node so the
/// value can stage its nested maps into the same object space.
pub trait TrieValue: Clone {
    /// Returns the canonical, nonempty encoding.
    fn encode(&self) -> Vec<u8>;

    /// Decodes an encoding produced by [`encode`](Self::encode).
    fn decode(bytes: &[u8]) -> Result<Self, CodecError>;

    /// Whether [`encode`](Self::encode) returns the empty string, which no
    /// leaf may hold; [`MerkleMap::insert`](crate::MerkleMap::insert) asks
    /// before it does anything else.
    ///
    /// The default encodes the value. A type whose encoding can never be
    /// empty (it always holds a length prefix or a fixed-width field)
    /// overrides this to return `false` without encoding. That matters for
    /// values holding nested maps: encoding one writes the nested root,
    /// which hashes the nested map's nodes built since they were last
    /// hashed, so with the default every insert of such a value would hash
    /// eagerly instead of when a root is requested. An override must agree
    /// with `encode`.
    fn encodes_empty(&self) -> bool {
        self.encode().is_empty()
    }

    /// Stages the new nodes of maps nested in this value into `sink`.
    ///
    /// The default has nothing nested. An implementation holding a
    /// [`MerkleMap`](crate::MerkleMap) calls its
    /// [`stage`](crate::MerkleMap::stage) with the same sink. Maps that
    /// belong to another object space are staged by the caller with
    /// [`MerkleMap::stage_with`](crate::MerkleMap::stage_with) instead.
    fn stage_nested(&self, sink: &mut dyn ObjectSink) {
        let _ = sink;
    }

    /// Approximate heap bytes owned by this value beyond its inline size,
    /// for hosts that bound a cache of decoded nodes by memory.
    ///
    /// The default is zero, right for plain fixed-size values. A value
    /// decoded from the store that holds nested maps holds them only as
    /// stored roots, so their nodes are not counted here.
    fn heap_bytes(&self) -> usize {
        0
    }
}

impl TrieValue for Vec<u8> {
    /// The bytes themselves. An empty vector cannot be stored:
    /// [`MerkleMap::insert`](crate::MerkleMap::insert) rejects it with
    /// [`TrieError::EmptyValue`](crate::TrieError::EmptyValue).
    fn encode(&self) -> Vec<u8> {
        self.clone()
    }

    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        if bytes.is_empty() {
            return Err(CodecError::new("empty leaf value"));
        }
        Ok(bytes.to_vec())
    }

    fn heap_bytes(&self) -> usize {
        self.capacity()
    }
}
