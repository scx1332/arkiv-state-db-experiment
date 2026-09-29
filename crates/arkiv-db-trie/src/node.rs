//! Trie nodes, their Ethereum MPT encoding, and strict decoding.
//!
//! A node encodes exactly as `alloy_trie::nodes` encodes it: a leaf is the
//! RLP list `[hex_prefix(path, leaf), value]`, an extension is
//! `[hex_prefix(path, extension), child]`, and a branch is the 17-item list
//! of its sixteen child references followed by an empty value. A child
//! reference is the child's own encoding when that is shorter than 32 bytes
//! (an *inline* child) and otherwise `0xa0 ‖ keccak256(encoding)`. Paths
//! here may exceed alloy's 64-nibble limit; the unit tests check that the
//! bytes are identical to alloy's encoders wherever alloy can represent the
//! node.
//!
//! Decoding is strict and total: it never panics, accepts only the encoding
//! this module produces for some node (so `encode(decode(b)) == b`), and
//! rejects the shapes a canonical map never contains (branch values,
//! branches with fewer than two children, empty extension paths, empty leaf
//! values, inline children of 32 bytes or more).

use std::{
    fmt,
    sync::{Arc, OnceLock},
};

use alloy_primitives::{B256, keccak256};
use alloy_rlp::{EMPTY_STRING_CODE, Encodable, Header};
use alloy_trie::nodes::RlpNode;

use crate::{TrieError, TrieValue, nibbles};

/// The identity of a stored node: keccak256 of its encoding.
///
/// This is the same 32-byte value that a parent embeds as its hash
/// reference, so a store keyed by object id is content-addressed.
pub type ObjectId = B256;

/// A reference to a node from a map handle or a parent node.
///
/// Links own no loader or store context. A [`Stored`](Self::Stored) link is
/// only an id; reading through it takes a [`NodeLoader`](crate::NodeLoader)
/// argument. Cloning a link is O(1).
pub enum Link<V> {
    /// No node: an empty map, or an unused branch slot.
    Empty,
    /// A node held in memory: built by an update, or decoded from the store.
    Mem(Arc<NodeBox<V>>),
    /// A committed node that has not been loaded into this handle.
    Stored(ObjectId),
}

impl<V> Clone for Link<V> {
    fn clone(&self) -> Self {
        match self {
            Self::Empty => Self::Empty,
            Self::Mem(node) => Self::Mem(Arc::clone(node)),
            Self::Stored(id) => Self::Stored(*id),
        }
    }
}

impl<V> fmt::Debug for Link<V> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("Empty"),
            Self::Mem(node) => formatter.debug_tuple("Mem").field(node).finish(),
            Self::Stored(id) => formatter.debug_tuple("Stored").field(id).finish(),
        }
    }
}

impl<V> Link<V> {
    /// Returns whether this is [`Link::Empty`].
    pub const fn is_empty(&self) -> bool {
        matches!(self, Self::Empty)
    }
}

impl<V: TrieValue> Link<V> {
    /// The reference a parent embeds for this link; `None` for an empty slot.
    pub(crate) fn reference(&self) -> Option<RlpNode> {
        match self {
            Self::Empty => None,
            Self::Mem(node) => Some(node.reference().clone()),
            Self::Stored(id) => Some(RlpNode::word_rlp(id)),
        }
    }
}

/// The type of a node, for inspection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeKind {
    /// A leaf: the remaining key path and a value.
    Leaf,
    /// An extension: a shared path segment followed by one branch.
    Extension,
    /// A branch: up to sixteen children indexed by the next nibble.
    Branch,
}

/// An immutable node plus its lazily computed reference.
///
/// `from_store` is true for a node decoded from committed bytes, including
/// the inline children decoded out of such a node; it is false for a node
/// built in memory by an update. The invariant is that a `from_store` node's
/// bytes are durably committed in the map's object space, either as an
/// object of their own or inside the committed object of an ancestor; the
/// staging walk relies on it to stop there. A `NodeBox` holds no store,
/// loader or cache reference, so a decoded-node cache can own it without
/// creating cycles.
pub struct NodeBox<V> {
    pub(crate) node: Node<V>,
    rlp: OnceLock<RlpNode>,
    from_store: bool,
}

impl<V> fmt::Debug for NodeBox<V> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NodeBox")
            .field("kind", &self.kind())
            .field("from_store", &self.from_store)
            .finish_non_exhaustive()
    }
}

impl<V> NodeBox<V> {
    /// Wraps a node built in memory.
    pub(crate) fn new(node: Node<V>) -> Arc<Self> {
        Arc::new(Self {
            node,
            rlp: OnceLock::new(),
            from_store: false,
        })
    }

    /// Whether the node was decoded from committed bytes.
    pub const fn from_store(&self) -> bool {
        self.from_store
    }

    /// Reads only a reference installed by `decode_stored`, never computing it.
    pub(crate) fn stored_reference_len(&self) -> Option<usize> {
        self.from_store.then(|| {
            self.rlp
                .get()
                .expect("decoded nodes have a reference")
                .len()
        })
    }

    /// The node's type.
    pub const fn kind(&self) -> NodeKind {
        match self.node {
            Node::Leaf { .. } => NodeKind::Leaf,
            Node::Extension { .. } => NodeKind::Extension,
            Node::Branch { .. } => NodeKind::Branch,
        }
    }
}

impl<V: TrieValue> NodeBox<V> {
    /// The reference a parent embeds: the encoding itself when shorter than
    /// 32 bytes, else `0xa0 ‖ keccak256(encoding)`.
    ///
    /// Computed on first use and cached. Computing it for a node built in
    /// memory hashes its dirty descendants; it never reads the store,
    /// because a stored child's reference is its id.
    pub fn reference(&self) -> &RlpNode {
        self.rlp.get_or_init(|| {
            let mut encoding = Vec::new();
            self.node.encode(&mut encoding);
            RlpNode::from_rlp(&encoding)
        })
    }

    /// Appends the node's canonical encoding.
    pub fn encode(&self, out: &mut Vec<u8>) {
        self.node.encode(out);
    }

    /// Verifies and decodes the committed object `id`.
    ///
    /// Checks `keccak256(bytes) == id`, decodes the node strictly, decodes
    /// its inline children into [`Link::Mem`] nodes and every leaf value
    /// with [`TrieValue::decode`], and marks them all `from_store`. Loaders
    /// that keep a decoded-node cache call this on a miss.
    pub fn decode_stored(id: &ObjectId, bytes: &[u8]) -> Result<Arc<Self>, TrieError> {
        if keccak256(bytes) != *id {
            return Err(TrieError::HashMismatch { id: *id });
        }
        let raw = decode_raw(bytes).map_err(|reason| TrieError::Malformed { id: *id, reason })?;
        let node = materialize(raw, id)?;
        // A committed object is at least 32 bytes unless it is a map root,
        // which is stored by hash whatever its size.
        let rlp = if bytes.len() < 32 {
            RlpNode::from_raw(bytes).ok_or(TrieError::Malformed {
                id: *id,
                reason: "node too long for an inline reference",
            })?
        } else {
            RlpNode::word_rlp(id)
        };
        Ok(Arc::new(Self {
            node,
            rlp: OnceLock::from(rlp),
            from_store: true,
        }))
    }
}

/// A trie node. Paths hold one nibble per byte.
pub(crate) enum Node<V> {
    /// The rest of a key and its value.
    Leaf { path: Box<[u8]>, value: V },
    /// A nonempty shared path segment; the child is always a branch.
    Extension { path: Box<[u8]>, child: Link<V> },
    /// Children by next nibble; at least two are nonempty.
    Branch { children: Box<[Link<V>; 16]> },
}

impl<V: TrieValue> Node<V> {
    /// Appends the canonical Ethereum MPT encoding.
    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Self::Leaf { path, value } => {
                let value = value.encode();
                assert!(
                    !value.is_empty(),
                    "TrieValue::encode returned an empty encoding"
                );
                let mut encoded_path = Vec::with_capacity(nibbles::encoded_path_len(path.len()));
                nibbles::encode_path(path, true, &mut encoded_path);
                let payload_length = encoded_path.as_slice().length() + value.as_slice().length();
                Header {
                    list: true,
                    payload_length,
                }
                .encode(out);
                encoded_path.as_slice().encode(out);
                value.as_slice().encode(out);
            }
            Self::Extension { path, child } => {
                let mut encoded_path = Vec::with_capacity(nibbles::encoded_path_len(path.len()));
                nibbles::encode_path(path, false, &mut encoded_path);
                let child = child.reference();
                let child = child.as_deref().unwrap_or(&[EMPTY_STRING_CODE]);
                let payload_length = encoded_path.as_slice().length() + child.len();
                Header {
                    list: true,
                    payload_length,
                }
                .encode(out);
                encoded_path.as_slice().encode(out);
                out.extend_from_slice(child);
            }
            Self::Branch { children } => {
                let references: [Option<RlpNode>; 16] =
                    std::array::from_fn(|index| children[index].reference());
                // Sixteen child slots (an empty slot is 0x80) plus the empty value.
                let payload_length = 1 + references
                    .iter()
                    .map(|reference| reference.as_ref().map_or(1, |reference| reference.len()))
                    .sum::<usize>();
                Header {
                    list: true,
                    payload_length,
                }
                .encode(out);
                for reference in &references {
                    match reference {
                        Some(reference) => out.extend_from_slice(reference),
                        None => out.push(EMPTY_STRING_CODE),
                    }
                }
                out.push(EMPTY_STRING_CODE);
            }
        }
    }
}

/// A child reference as it appears in an encoded node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RawChild<'a> {
    /// `0x80`: no child.
    Empty,
    /// `0xa0 ‖ hash`: a node of 32 or more bytes, identified by its hash.
    Hash(B256),
    /// A node shorter than 32 bytes, embedded in its parent.
    Inline(&'a [u8]),
}

/// A strictly decoded node whose children and value are still raw bytes.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RawNode<'a> {
    Leaf { path: Vec<u8>, value: &'a [u8] },
    Extension { path: Vec<u8>, child: RawChild<'a> },
    Branch { children: Box<[RawChild<'a>; 16]> },
}

/// Decodes one canonical node occupying all of `bytes`.
///
/// Never panics. Returns a static reason on rejection.
pub(crate) fn decode_raw(bytes: &[u8]) -> Result<RawNode<'_>, &'static str> {
    let mut rest = bytes;
    let header = Header::decode(&mut rest).map_err(|_| "invalid RLP header")?;
    if !header.list {
        return Err("node is not an RLP list");
    }
    if rest.len() != header.payload_length {
        return Err("bytes after the node");
    }
    let mut items: [&[u8]; 17] = [&[]; 17];
    let mut count = 0;
    while !rest.is_empty() {
        let slot = items.get_mut(count).ok_or("node has more than 17 items")?;
        *slot = split_item(&mut rest)?;
        count += 1;
    }
    match count {
        2 => decode_short(items[0], items[1]),
        17 => decode_branch(&items),
        _ => Err("node is neither a 2-item nor a 17-item list"),
    }
}

/// Splits the next complete RLP item (header and payload) off `rest`.
fn split_item<'a>(rest: &mut &'a [u8]) -> Result<&'a [u8], &'static str> {
    let start = *rest;
    let header = Header::decode(rest).map_err(|_| "invalid RLP item")?;
    // A single byte below 0x80 is its own item: `Header::decode` reports a
    // one-byte payload without consuming it, so skipping the payload works
    // for both forms.
    let payload = rest
        .get(..header.payload_length)
        .ok_or("RLP item overruns the node")?;
    *rest = &rest[payload.len()..];
    Ok(&start[..start.len() - rest.len()])
}

/// Returns the payload of an item that must be an RLP string.
fn string_payload(item: &[u8]) -> Result<&[u8], &'static str> {
    let mut rest = item;
    Header::decode_bytes(&mut rest, false).map_err(|_| "expected an RLP string")
}

fn decode_short<'a>(path: &'a [u8], second: &'a [u8]) -> Result<RawNode<'a>, &'static str> {
    let (path, leaf) = nibbles::decode_path(string_payload(path)?)?;
    if leaf {
        let value = string_payload(second)?;
        if value.is_empty() {
            return Err("empty leaf value");
        }
        return Ok(RawNode::Leaf { path, value });
    }
    if path.is_empty() {
        return Err("empty extension path");
    }
    match decode_child(second)? {
        RawChild::Empty => Err("extension without a child"),
        child => Ok(RawNode::Extension { path, child }),
    }
}

fn decode_branch<'a>(items: &[&'a [u8]; 17]) -> Result<RawNode<'a>, &'static str> {
    let mut children = Box::new([RawChild::Empty; 16]);
    for (slot, item) in children.iter_mut().zip(items) {
        *slot = decode_child(item)?;
    }
    if items[16] != [EMPTY_STRING_CODE] {
        return Err("branch node carries a value");
    }
    let occupied = children
        .iter()
        .filter(|child| **child != RawChild::Empty)
        .count();
    if occupied < 2 {
        return Err("branch node with fewer than two children");
    }
    Ok(RawNode::Branch { children })
}

fn decode_child(item: &[u8]) -> Result<RawChild<'_>, &'static str> {
    match item {
        [EMPTY_STRING_CODE] => Ok(RawChild::Empty),
        [0xa0, hash @ ..] if hash.len() == 32 => Ok(RawChild::Hash(B256::from_slice(hash))),
        [first, ..] if *first >= 0xc0 => {
            if item.len() < 32 {
                Ok(RawChild::Inline(item))
            } else {
                Err("inline child of 32 bytes or more")
            }
        }
        _ => Err("invalid child reference"),
    }
}

/// Builds a node from a decoded object, decoding inline children and values.
fn materialize<V: TrieValue>(raw: RawNode<'_>, id: &ObjectId) -> Result<Node<V>, TrieError> {
    Ok(match raw {
        RawNode::Leaf { path, value } => Node::Leaf {
            path: path.into_boxed_slice(),
            value: V::decode(value).map_err(|error| TrieError::Value { id: *id, error })?,
        },
        RawNode::Extension { path, child } => {
            let child = materialize_child(child, id)?;
            if let Link::Mem(inline) = &child
                && inline.kind() != NodeKind::Branch
            {
                return Err(TrieError::Malformed {
                    id: *id,
                    reason: "extension child is not a branch",
                });
            }
            Node::Extension {
                path: path.into_boxed_slice(),
                child,
            }
        }
        RawNode::Branch { children } => {
            let mut links: [Link<V>; 16] = std::array::from_fn(|_| Link::Empty);
            for (link, child) in links.iter_mut().zip(*children) {
                *link = materialize_child(child, id)?;
            }
            Node::Branch {
                children: Box::new(links),
            }
        }
    })
}

fn materialize_child<V: TrieValue>(
    child: RawChild<'_>,
    id: &ObjectId,
) -> Result<Link<V>, TrieError> {
    Ok(match child {
        RawChild::Empty => Link::Empty,
        RawChild::Hash(hash) => Link::Stored(hash),
        RawChild::Inline(bytes) => {
            let malformed = |reason| TrieError::Malformed { id: *id, reason };
            let node = materialize(decode_raw(bytes).map_err(malformed)?, id)?;
            let rlp = RlpNode::from_raw(bytes).ok_or(malformed("inline child too long"))?;
            Link::Mem(Arc::new(NodeBox {
                node,
                rlp: OnceLock::from(rlp),
                from_store: true,
            }))
        }
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_primitives::{B256, keccak256};
    use alloy_rlp::Encodable;
    use alloy_trie::{
        Nibbles, TrieMask,
        nodes::{BranchNode, ExtensionNode, LeafNode, RlpNode, TrieNode},
    };

    use super::{Link, Node, NodeBox, NodeKind, RawChild, RawNode, decode_raw};
    use crate::TrieError;

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            // SplitMix64.
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }

        fn below(&mut self, bound: usize) -> usize {
            (self.next() % bound as u64) as usize
        }

        fn bytes(&mut self, len: usize) -> Vec<u8> {
            (0..len).map(|_| self.next() as u8).collect()
        }

        fn nibbles(&mut self, len: usize) -> Vec<u8> {
            (0..len).map(|_| (self.next() & 0x0f) as u8).collect()
        }

        /// Between `min` and `min + spread - 1` random bytes.
        fn some_bytes(&mut self, min: usize, spread: usize) -> Vec<u8> {
            let len = min + self.below(spread);
            self.bytes(len)
        }

        /// Between `min` and `min + spread - 1` random nibbles.
        fn some_nibbles(&mut self, min: usize, spread: usize) -> Vec<u8> {
            let len = min + self.below(spread);
            self.nibbles(len)
        }
    }

    fn encode(node: &Node<Vec<u8>>) -> Vec<u8> {
        let mut out = Vec::new();
        node.encode(&mut out);
        out
    }

    fn inline_leaf(rng: &mut Rng) -> Arc<NodeBox<Vec<u8>>> {
        let path = rng.some_nibbles(0, 4).into_boxed_slice();
        let value = rng.some_bytes(1, 8);
        NodeBox::new(Node::Leaf { path, value })
    }

    #[test]
    fn encoding_matches_alloy_nodes() {
        let mut rng = Rng(11);
        for _ in 0..500 {
            // Leaf: any path alloy can hold (up to 64 nibbles).
            let path = rng.some_nibbles(0, 65);
            let value = rng.some_bytes(1, 70);
            let ours = encode(&Node::Leaf {
                path: path.clone().into_boxed_slice(),
                value: value.clone(),
            });
            let alloy = alloy_rlp::encode(LeafNode::new(Nibbles::from_nibbles(&path), value));
            assert_eq!(ours, alloy);

            // Extension over a hashed or an inline child.
            let path = rng.some_nibbles(1, 64);
            let child = if rng.next().is_multiple_of(2) {
                Link::Stored(B256::from_slice(&rng.bytes(32)))
            } else {
                Link::Mem(inline_leaf(&mut rng))
            };
            let reference = child.reference().unwrap();
            let ours = encode(&Node::Extension {
                path: path.clone().into_boxed_slice(),
                child,
            });
            let alloy =
                alloy_rlp::encode(ExtensionNode::new(Nibbles::from_nibbles(&path), reference));
            assert_eq!(ours, alloy);

            // Branch with a random mix of empty, hashed and inline children.
            let children: [Link<Vec<u8>>; 16] = std::array::from_fn(|_| match rng.below(3) {
                0 => Link::Empty,
                1 => Link::Stored(B256::from_slice(&rng.bytes(32))),
                _ => Link::Mem(inline_leaf(&mut rng)),
            });
            let mut mask = TrieMask::default();
            let mut stack = Vec::new();
            for (index, child) in children.iter().enumerate() {
                if let Some(reference) = child.reference() {
                    mask.set_bit(index as u8);
                    stack.push(reference);
                }
            }
            let ours = encode(&Node::Branch {
                children: Box::new(children),
            });
            let alloy = alloy_rlp::encode(BranchNode::new(stack, mask));
            assert_eq!(ours, alloy);
        }
    }

    #[test]
    fn long_paths_extend_the_same_format() {
        // 176 nibbles (an 88-byte key) is beyond alloy's Nibbles; the layout
        // is still hex prefix inside an RLP string.
        let path: Vec<u8> = (0..176).map(|index| (index % 16) as u8).collect();
        let encoding = encode(&Node::Leaf {
            path: path.clone().into_boxed_slice(),
            value: vec![7],
        });
        // The path is 1 + 88 = 89 hex-prefix bytes (string header 0xb8 89, flag
        // 0x20) and the value one byte, so the list payload is 2 + 89 + 1 = 92.
        assert_eq!(&encoding[..5], &[0xf8, 92, 0xb8, 89, 0x20]);
        assert_eq!(encoding.len(), 2 + 2 + 89 + 1);
        assert_eq!(
            decode_raw(&encoding),
            Ok(RawNode::Leaf { path, value: &[7] })
        );
    }

    #[test]
    fn decoding_round_trips_canonical_nodes() {
        let mut rng = Rng(5);
        for _ in 0..300 {
            let children: [Link<Vec<u8>>; 16] = std::array::from_fn(|index| {
                if index < 2 || rng.next().is_multiple_of(2) {
                    Link::Stored(B256::from_slice(&rng.bytes(32)))
                } else {
                    Link::Mem(inline_leaf(&mut rng))
                }
            });
            let branch = encode(&Node::Branch {
                children: Box::new(children),
            });
            let id = keccak256(&branch);
            let decoded = NodeBox::<Vec<u8>>::decode_stored(&id, &branch).unwrap();
            assert_eq!(decoded.kind(), NodeKind::Branch);
            assert!(decoded.from_store());
            let mut again = Vec::new();
            decoded.encode(&mut again);
            assert_eq!(again, branch);
            // Inline children come back as `from_store` memory nodes.
            let super::Node::Branch { children } = &decoded.node else {
                panic!("decoded a branch");
            };
            for child in children.iter() {
                if let Link::Mem(child) = child {
                    assert!(child.from_store());
                    assert_eq!(child.kind(), NodeKind::Leaf);
                }
            }
        }
    }

    #[test]
    fn decode_stored_verifies_the_hash() {
        let leaf = encode(&Node::Leaf {
            path: vec![1; 40].into_boxed_slice(),
            value: vec![9; 20],
        });
        let id = keccak256(&leaf);
        assert!(NodeBox::<Vec<u8>>::decode_stored(&id, &leaf).is_ok());
        let mut flipped = leaf.clone();
        flipped[10] ^= 1;
        assert_eq!(
            NodeBox::<Vec<u8>>::decode_stored(&id, &flipped).unwrap_err(),
            TrieError::HashMismatch { id }
        );
    }

    #[test]
    fn decoder_rejects_noncanonical_nodes() {
        let hash = [0xa0].into_iter().chain([0x11; 32]).collect::<Vec<u8>>();
        let branch_with = |slots: &[&[u8]], value: &[u8]| {
            let mut payload = Vec::new();
            for slot in slots {
                payload.extend_from_slice(slot);
            }
            payload.extend_from_slice(value);
            let mut out = Vec::new();
            alloy_rlp::Header {
                list: true,
                payload_length: payload.len(),
            }
            .encode(&mut out);
            out.extend_from_slice(&payload);
            out
        };
        let empty: &[u8] = &[0x80];
        let two_children: Vec<&[u8]> = [hash.as_slice(), hash.as_slice()]
            .into_iter()
            .chain(std::iter::repeat_n(empty, 14))
            .collect();
        assert!(decode_raw(&branch_with(&two_children, &[0x80])).is_ok());
        // A branch value, a one-child branch, a bad child reference.
        assert!(decode_raw(&branch_with(&two_children, &[0x01])).is_err());
        let one_child: Vec<&[u8]> = [hash.as_slice()]
            .into_iter()
            .chain(std::iter::repeat_n(empty, 15))
            .collect();
        assert!(decode_raw(&branch_with(&one_child, &[0x80])).is_err());
        let mut short_hash = two_children.clone();
        let truncated = [0x9f].into_iter().chain([0x11; 31]).collect::<Vec<u8>>();
        short_hash[3] = &truncated;
        assert!(decode_raw(&branch_with(&short_hash, &[0x80])).is_err());

        // Leaf and extension forms, built through alloy where it can.
        let leaf = alloy_rlp::encode(LeafNode::new(Nibbles::from_nibbles([1, 2]), vec![5]));
        assert!(decode_raw(&leaf).is_ok());
        let empty_value = alloy_rlp::encode(LeafNode::new(Nibbles::from_nibbles([1, 2]), vec![]));
        assert_eq!(decode_raw(&empty_value), Err("empty leaf value"));
        let mut trailing = leaf.clone();
        trailing.push(0);
        assert!(decode_raw(&trailing).is_err());
        let mut junk_padding = leaf.clone();
        // [0xc4, 0x82, 0x20, 0x12, 0x05]: set the even path's low nibble.
        junk_padding[2] = 0x21;
        assert!(decode_raw(&junk_padding).is_err());
        let empty_extension = [0xc2, 0x00, 0x80];
        assert!(decode_raw(&empty_extension).is_err());
        let extension_without_child = [0xc2, 0x11, 0x80];
        assert!(decode_raw(&extension_without_child).is_err());
        // An inline child must be shorter than 32 bytes.
        let big_leaf = alloy_rlp::encode(LeafNode::new(Nibbles::from_nibbles([1]), vec![3; 40]));
        let extension = {
            let mut payload = vec![0x11];
            payload.extend_from_slice(&big_leaf);
            let mut out = Vec::new();
            alloy_rlp::Header {
                list: true,
                payload_length: payload.len(),
            }
            .encode(&mut out);
            out.extend_from_slice(&payload);
            out
        };
        assert_eq!(
            decode_raw(&extension),
            Err("inline child of 32 bytes or more")
        );
        // A string is not a node, nor is the empty-root marker.
        assert!(decode_raw(&[0x80]).is_err());
        assert!(decode_raw(&[]).is_err());
        assert!(decode_raw(&[0xc0]).is_err());
    }

    #[test]
    fn decoder_never_panics_and_accepts_only_canonical_bytes() {
        let mut rng = Rng(99);
        let seeds: Vec<Vec<u8>> = {
            let mut seeds = Vec::new();
            for _ in 0..40 {
                seeds.push(alloy_rlp::encode(LeafNode::new(
                    Nibbles::from_nibbles(rng.some_nibbles(0, 20)),
                    rng.some_bytes(1, 40),
                )));
                let stack = vec![RlpNode::word_rlp(&B256::repeat_byte(3)); 3];
                seeds.push(alloy_rlp::encode(BranchNode::new(
                    stack,
                    TrieMask::new(0b1000_0000_0101_0000),
                )));
                seeds.push(alloy_rlp::encode(ExtensionNode::new(
                    Nibbles::from_nibbles(rng.some_nibbles(1, 10)),
                    RlpNode::word_rlp(&B256::repeat_byte(4)),
                )));
            }
            seeds
        };
        for round in 0..20_000 {
            let mut bytes = seeds[rng.below(seeds.len())].clone();
            match round % 4 {
                0 => {
                    let at = rng.below(bytes.len());
                    bytes[at] ^= 1 << rng.below(8);
                }
                1 => bytes.truncate(rng.below(bytes.len())),
                2 => {
                    let at = rng.below(bytes.len());
                    bytes[at] = rng.next() as u8;
                    bytes.push(rng.next() as u8);
                }
                _ => bytes = rng.some_bytes(0, 80),
            }
            if let Ok(raw) = decode_raw(&bytes) {
                // Whatever decodes must re-encode to the same bytes.
                assert_eq!(reencode(&raw), bytes, "accepted a noncanonical encoding");
            }
        }
    }

    #[test]
    fn alloy_node_decoder_panics_on_paths_beyond_32_bytes() {
        // Evidence for decoding nodes here: alloy's `TrieNode::decode` unpacks
        // paths into `Nibbles`, which asserts at most 32 path bytes, so an
        // adversarial (or merely long-keyed) node aborts the decoder instead
        // of returning an error. Our decoder returns the node.
        let path: Vec<u8> = vec![0x5; 70];
        let node = encode(&Node::Leaf {
            path: path.clone().into_boxed_slice(),
            value: vec![1],
        });
        let outcome = std::panic::catch_unwind(|| {
            <TrieNode as alloy_rlp::Decodable>::decode(&mut node.as_slice())
        });
        assert!(outcome.is_err(), "alloy decoded a 35-byte path");
        assert!(matches!(decode_raw(&node), Ok(RawNode::Leaf { .. })));
    }

    /// Re-encodes a raw node, copying child references verbatim.
    fn reencode(raw: &RawNode<'_>) -> Vec<u8> {
        fn child(child: &RawChild<'_>, out: &mut Vec<u8>) {
            match child {
                RawChild::Empty => out.push(0x80),
                RawChild::Hash(hash) => out.extend_from_slice(&RlpNode::word_rlp(hash)),
                RawChild::Inline(bytes) => out.extend_from_slice(bytes),
            }
        }
        let mut payload = Vec::new();
        match raw {
            RawNode::Leaf { path, value } => {
                let mut encoded = Vec::new();
                crate::nibbles::encode_path(path, true, &mut encoded);
                encoded.as_slice().encode(&mut payload);
                value.encode(&mut payload);
            }
            RawNode::Extension { path, child: next } => {
                let mut encoded = Vec::new();
                crate::nibbles::encode_path(path, false, &mut encoded);
                encoded.as_slice().encode(&mut payload);
                child(next, &mut payload);
            }
            RawNode::Branch { children } => {
                for next in children.iter() {
                    child(next, &mut payload);
                }
                payload.push(0x80);
            }
        }
        let mut out = Vec::new();
        alloy_rlp::Header {
            list: true,
            payload_length: payload.len(),
        }
        .encode(&mut out);
        out.extend_from_slice(&payload);
        out
    }
}
