//! Errors of map operations over stored nodes.

use std::fmt;

use crate::{ObjectId, codec::CodecError};

/// A failure to read or trust the nodes of a map, or a value no map can
/// hold.
///
/// No variant is an answer about the map's contents: a lookup that fails
/// with a `TrieError` has **not** established that a key is absent. Every
/// variant except [`EmptyValue`](Self::EmptyValue) is an availability or
/// integrity failure of the node store. A host maps
/// [`Missing`](Self::Missing) to its own "object missing" error,
/// `EmptyValue` to a caller error and every other variant to "corrupt";
/// loaders with their own error type absorb these through
/// `From<TrieError>` (see [`NodeLoader`](crate::NodeLoader)).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum TrieError {
    /// A referenced object is not in the store.
    Missing {
        /// The object that was requested.
        id: ObjectId,
    },
    /// The store returned bytes whose keccak256 is not the requested id.
    HashMismatch {
        /// The object that was requested.
        id: ObjectId,
    },
    /// The object's bytes are not a canonical trie node.
    Malformed {
        /// The object whose bytes (or inline children) failed to decode.
        id: ObjectId,
        /// What was wrong.
        reason: &'static str,
    },
    /// A leaf value inside the object failed [`TrieValue::decode`](crate::TrieValue::decode).
    Value {
        /// The object holding the leaf.
        id: ObjectId,
        /// The value decoder's error.
        error: CodecError,
    },
    /// A key assembled from a leaf path failed [`TrieKey::decode`](crate::TrieKey::decode).
    Key {
        /// The key decoder's error.
        error: CodecError,
    },
    /// Individually valid nodes contradict the map's shape, for example a
    /// leaf at the wrong depth for the key length, an extension whose child
    /// is not a branch, or a hash-referenced node shorter than 32 bytes.
    Inconsistent {
        /// What was wrong.
        reason: &'static str,
    },
    /// A `(root, len)` pair that cannot describe a map: the empty root with
    /// a nonzero length, or a zero length with another root.
    InvalidHandle {
        /// The root hash given.
        root: ObjectId,
        /// The length given.
        len: u64,
    },
    /// [`MerkleMap::insert`](crate::MerkleMap::insert) was given a value
    /// whose [`TrieValue::encode`](crate::TrieValue::encode) is empty,
    /// which no leaf can hold. A caller error, raised before anything is
    /// read or changed.
    EmptyValue,
}

impl TrieError {
    pub(crate) const fn inconsistent(reason: &'static str) -> Self {
        Self::Inconsistent { reason }
    }
}

impl fmt::Display for TrieError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing { id } => write!(formatter, "trie node {id} is missing"),
            Self::HashMismatch { id } => {
                write!(
                    formatter,
                    "stored bytes of trie node {id} do not hash to its id"
                )
            }
            Self::Malformed { id, reason } => {
                write!(formatter, "trie node {id} is malformed: {reason}")
            }
            Self::Value { id, error } => {
                write!(
                    formatter,
                    "a leaf value in trie node {id} is invalid: {error}"
                )
            }
            Self::Key { error } => write!(formatter, "a trie key is invalid: {error}"),
            Self::Inconsistent { reason } => write!(formatter, "inconsistent trie: {reason}"),
            Self::InvalidHandle { root, len } => {
                write!(
                    formatter,
                    "invalid map handle: root {root} with length {len}"
                )
            }
            Self::EmptyValue => formatter.write_str("a trie value cannot have an empty encoding"),
        }
    }
}

impl std::error::Error for TrieError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Value { error, .. } | Self::Key { error } => Some(error),
            _ => None,
        }
    }
}
