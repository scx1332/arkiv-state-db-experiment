//! Streaming construction from an already sorted sequence of encoded leaves.

use std::{fmt, marker::PhantomData};

use alloy_primitives::keccak256;
use alloy_trie::nodes::RlpNode;

use crate::{
    EMPTY_ROOT_HASH, ObjectId, TrieKey, TryObjectSink, nibbles,
    node::{encode_branch, encode_extension, encode_leaf},
};

/// An invalid sorted input, or a failure while emitting a completed node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SortedTrieError<E> {
    /// The supplied key exceeds the key type's encoded-length bound.
    KeyTooLong { len: usize, max: usize },
    /// Leaf values must have nonempty encodings.
    EmptyValue,
    /// Keys must increase strictly; duplicates are rejected as well.
    OutOfOrder,
    /// One key is a strict prefix of another.
    KeyNotPrefixFree,
    /// The entry count cannot be represented by `u64`.
    LengthOverflow,
    /// The sink stopped construction. Earlier objects remain in the sink.
    Sink(E),
    /// A previous sink failure makes this builder unusable.
    Failed,
}

impl<E: fmt::Display> fmt::Display for SortedTrieError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::KeyTooLong { len, max } => {
                write!(formatter, "encoded key length {len} exceeds maximum {max}")
            }
            Self::EmptyValue => formatter.write_str("a trie value cannot have an empty encoding"),
            Self::OutOfOrder => formatter.write_str("sorted trie keys must increase strictly"),
            Self::KeyNotPrefixFree => formatter.write_str("sorted trie keys must be prefix-free"),
            Self::LengthOverflow => formatter.write_str("sorted trie entry count overflow"),
            Self::Sink(error) => write!(formatter, "sorted trie sink failed: {error}"),
            Self::Failed => formatter.write_str("sorted trie construction already failed"),
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for SortedTrieError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sink(error) => Some(error),
            _ => None,
        }
    }
}

/// A partially completed branch on the rightmost key's path. Its children
/// contain only inline encodings or hashes, never retained subtrees.
#[derive(Debug)]
struct Branch {
    depth: usize,
    children: [Option<RlpNode>; 16],
}

/// Builds the same canonical trie as [`crate::MerkleMap`] from sorted leaves.
///
/// Input consists of encoded, strictly increasing, prefix-free keys of at most
/// `K::MAX_LEN` bytes and nonempty encoded values. This builder does not invoke
/// either codec's decoder or stage maps referenced by encoded values: callers
/// validate their encodings and emit nested maps separately.
///
/// Completed subtrees are emitted immediately through a [`TryObjectSink`].
/// Only a previous key/value, a reusable node encoding buffer, and the branches
/// on the rightmost path remain in memory. Each branch holds at most sixteen
/// child references of at most 33 bytes. Memory therefore depends on the
/// longest allowed key and largest leaf, not the number of entries. No
/// recursion or whole-trie node set is used. The sink owns deduplication and
/// any storage buffering.
///
/// All objects of at least 32 bytes are emitted, and [`finish`](Self::finish)
/// also emits an inline root as its own object. Invalid input is rejected
/// before changing the builder or sink. A sink failure stops immediately and
/// poisons the builder; previously emitted objects may remain unreferenced.
/// Publishing the returned root belongs to the caller's atomic storage
/// protocol, after `finish` succeeds.
#[derive(Debug)]
pub struct SortedTrieBuilder<K> {
    previous: Vec<u8>,
    value: Vec<u8>,
    branches: Vec<Branch>,
    encoding: Vec<u8>,
    len: u64,
    failed: bool,
    key: PhantomData<fn() -> K>,
}

impl<K: TrieKey> Default for SortedTrieBuilder<K> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: TrieKey> SortedTrieBuilder<K> {
    /// Starts an empty stream without preallocating from `K::MAX_LEN`.
    pub const fn new() -> Self {
        Self {
            previous: Vec::new(),
            value: Vec::new(),
            branches: Vec::new(),
            encoding: Vec::new(),
            len: 0,
            failed: false,
            key: PhantomData,
        }
    }

    /// Number of successfully accepted leaves.
    pub const fn len(&self) -> u64 {
        self.len
    }

    /// Whether no leaves have been accepted.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Accepts the next encoded leaf, emitting subtrees it completes.
    ///
    /// The sink must address the same object space on every call, including
    /// `finish`. Changing a sink's transaction/buffer is fine provided earlier
    /// accepted objects remain available when the completed root is published.
    pub fn push<S: TryObjectSink + ?Sized>(
        &mut self,
        encoded_key: &[u8],
        encoded_value: &[u8],
        sink: &mut S,
    ) -> Result<(), SortedTrieError<S::Error>> {
        if self.failed {
            return Err(SortedTrieError::Failed);
        }
        if encoded_key.len() > K::MAX_LEN {
            return Err(SortedTrieError::KeyTooLong {
                len: encoded_key.len(),
                max: K::MAX_LEN,
            });
        }
        if encoded_value.is_empty() {
            return Err(SortedTrieError::EmptyValue);
        }
        let len = self
            .len
            .checked_add(1)
            .ok_or(SortedTrieError::LengthOverflow)?;
        let next = nibbles::unpack(encoded_key);
        if self.len != 0 {
            let common = nibbles::common_prefix(&self.previous, &next);
            if common == self.previous.len().min(next.len()) {
                return Err(if self.previous.len() == next.len() {
                    SortedTrieError::OutOfOrder
                } else {
                    SortedTrieError::KeyNotPrefixFree
                });
            }
            if next[common] < self.previous[common] {
                return Err(SortedTrieError::OutOfOrder);
            }

            // The next key seals the previous leaf and every deeper branch.
            // Set the poison flag before mutating the frontier or sink.
            self.failed = true;
            let depth = self.branches.last().map_or(common, |b| b.depth.max(common));
            let mut child = self.seal_leaf(depth + 1, sink)?;
            while self.branches.last().is_some_and(|b| b.depth > common) {
                let branch = self.branches.pop().expect("branch was checked");
                let depth = branch.depth;
                child = self.seal_branch(branch, child, sink)?;
                let start = self.branches.last().map_or(common, |b| b.depth.max(common)) + 1;
                child = self.seal_extension(start, depth, child, sink)?;
            }
            if self.branches.last().is_none_or(|b| b.depth < common) {
                self.branches.push(Branch {
                    depth: common,
                    children: std::array::from_fn(|_| None),
                });
            }
            let branch = self.branches.last_mut().expect("common branch exists");
            let slot = &mut branch.children[self.previous[common] as usize];
            debug_assert!(slot.is_none());
            *slot = Some(child);
            self.failed = false;
        }
        self.previous = next;
        self.value.clear();
        self.value.extend_from_slice(encoded_value);
        self.len = len;
        Ok(())
    }

    /// Emits the remaining frontier and returns `(root_hash, entry_count)`.
    /// Empty input returns [`EMPTY_ROOT_HASH`] without emitting an object.
    pub fn finish<S: TryObjectSink + ?Sized>(
        mut self,
        sink: &mut S,
    ) -> Result<(ObjectId, u64), SortedTrieError<S::Error>> {
        if self.failed {
            return Err(SortedTrieError::Failed);
        }
        if self.len == 0 {
            return Ok((EMPTY_ROOT_HASH, 0));
        }
        let start = self.branches.last().map_or(0, |b| b.depth + 1);
        let mut child = self.seal_leaf(start, sink)?;
        while let Some(branch) = self.branches.pop() {
            let depth = branch.depth;
            child = self.seal_branch(branch, child, sink)?;
            let start = self.branches.last().map_or(0, |b| b.depth + 1);
            child = self.seal_extension(start, depth, child, sink)?;
        }
        let root = match child.as_hash() {
            Some(id) => id, // Already emitted by `emit`.
            None => {
                let id = keccak256(child.as_ref());
                sink.record(id, child.as_ref())
                    .map_err(SortedTrieError::Sink)?;
                id
            }
        };
        Ok((root, self.len))
    }

    fn seal_leaf<S: TryObjectSink + ?Sized>(
        &mut self,
        start: usize,
        sink: &mut S,
    ) -> Result<RlpNode, SortedTrieError<S::Error>> {
        self.encoding.clear();
        encode_leaf(&self.previous[start..], &self.value, &mut self.encoding);
        self.emit(sink)
    }

    fn seal_branch<S: TryObjectSink + ?Sized>(
        &mut self,
        mut branch: Branch,
        child: RlpNode,
        sink: &mut S,
    ) -> Result<RlpNode, SortedTrieError<S::Error>> {
        let slot = &mut branch.children[self.previous[branch.depth] as usize];
        debug_assert!(slot.is_none());
        *slot = Some(child);
        debug_assert!(branch.children.iter().flatten().count() >= 2);
        self.encoding.clear();
        encode_branch(&branch.children, &mut self.encoding);
        self.emit(sink)
    }

    fn seal_extension<S: TryObjectSink + ?Sized>(
        &mut self,
        start: usize,
        end: usize,
        child: RlpNode,
        sink: &mut S,
    ) -> Result<RlpNode, SortedTrieError<S::Error>> {
        if start == end {
            return Ok(child);
        }
        self.encoding.clear();
        encode_extension(&self.previous[start..end], &child, &mut self.encoding);
        self.emit(sink)
    }

    fn emit<S: TryObjectSink + ?Sized>(
        &self,
        sink: &mut S,
    ) -> Result<RlpNode, SortedTrieError<S::Error>> {
        let reference = RlpNode::from_rlp(&self.encoding);
        if let Some(id) = reference.as_hash() {
            sink.record(id, &self.encoding)
                .map_err(SortedTrieError::Sink)?;
        }
        Ok(reference)
    }
}

#[cfg(test)]
mod tests {
    use super::{Branch, SortedTrieBuilder, SortedTrieError};
    use crate::{ObjectId, StageError, TryObjectSink};

    #[derive(Default)]
    struct Discard {
        calls: u64,
    }

    impl TryObjectSink for Discard {
        type Error = StageError;

        fn record(&mut self, _: ObjectId, _: &[u8]) -> Result<(), StageError> {
            self.calls += 1;
            Ok(())
        }
    }

    fn retained<K>(builder: &SortedTrieBuilder<K>) -> usize {
        builder.previous.capacity()
            + builder.value.capacity()
            + builder.encoding.capacity()
            + builder.branches.capacity() * std::mem::size_of::<Branch>()
    }

    #[test]
    fn long_stream_retains_only_a_key_bounded_frontier() {
        let mut builder = SortedTrieBuilder::<u64>::new();
        let mut sink = Discard::default();
        let mut maximum = 0;
        for index in 0_u64..100_000 {
            builder
                .push(&index.to_be_bytes(), &[0x55; 64], &mut sink)
                .unwrap();
            assert!(builder.branches.len() <= 16);
            maximum = maximum.max(retained(&builder));
        }
        // Fewer than 20 KiB, independent of the 100,000 leaves already emitted.
        assert!(maximum < 20 * 1024, "retained {maximum} bytes");
        assert!(
            sink.calls >= 99_999,
            "completed leaves must stream before finish"
        );
        assert_eq!(builder.finish(&mut sink).unwrap().1, 100_000);
    }

    #[test]
    fn every_key_nibble_can_have_a_branch_without_retaining_subtrees() {
        let mut builder = SortedTrieBuilder::<[u8; 128]>::new();
        let mut sink = Discard::default();
        let mut key = [0; 128];
        builder.push(&key, &[0x55; 64], &mut sink).unwrap();
        for depth in 0..256 {
            key[depth / 2] |= if depth % 2 == 0 { 0x10 } else { 0x01 };
            builder.push(&key, &[0x55; 64], &mut sink).unwrap();
            assert_eq!(builder.branches.len(), depth + 1);
        }
        assert!(retained(&builder) < 256 * std::mem::size_of::<Branch>() + 2048);
        assert_eq!(builder.finish(&mut sink).unwrap().1, 257);
    }

    #[test]
    fn count_overflow_does_not_emit_or_mutate() {
        let mut builder = SortedTrieBuilder::<u64>::new();
        let mut sink = Discard::default();
        builder.push(&0_u64.to_be_bytes(), &[1], &mut sink).unwrap();
        builder.len = u64::MAX;
        assert_eq!(
            builder.push(&1_u64.to_be_bytes(), &[1], &mut sink),
            Err(SortedTrieError::LengthOverflow)
        );
        assert_eq!(builder.len(), u64::MAX);
        assert_eq!(builder.previous, vec![0; 16]);
        assert_eq!(sink.calls, 0);
    }
}
