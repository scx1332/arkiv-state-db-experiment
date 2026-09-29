//! Point proofs: generation from a map and the strict verifier walk.
//!
//! A proof for key `k` in one map is the root node followed by every
//! hash-referenced node on `k`'s path, in path order; inline nodes travel
//! inside their parent. The same proof shows inclusion (the path ends at
//! the leaf for `k`) or absence (it ends at a leaf or extension that
//! diverges from `k`, or at a branch whose slot for `k` is empty).
//!
//! [`walk`] is the verifier. It returns the value the proof establishes and
//! never accepts a claimed value, so a caller compares the result with what
//! it was told. It rejects, rather than reinterprets, every deviation from
//! the honest proof: a missing, extra, duplicated or reordered node, a
//! proof that stops at a hash reference (which a lenient verifier would
//! read as absence), bytes after the terminal node, a node that is not in
//! canonical form or that contradicts a trie of fixed-length keys. It
//! decodes adversarial bytes without panicking, and works iteratively, so
//! input size bounds its work and memory.
//!
//! `alloy_trie::proof::verify_proof` is not used as the verifier: it takes a
//! claimed value and accepts any proof that ends before the key's path does
//! as a proof of absence, so truncating an inclusion proof forges absence;
//! it also accepts a node supplied after a leaf whose value equals that
//! node's reference. The tests demonstrate both on alloy's own verifier.

use std::fmt;

use alloy_primitives::keccak256;
use alloy_trie::EMPTY_ROOT_HASH;

use crate::{
    Link, NodeLoader, ObjectId, TrieError, TrieValue,
    map::{Probe, Role, check_extension, check_leaf, resolve},
    nibbles,
    node::{Node, RawChild, RawNode, decode_raw},
};

/// A lazy proof generator, from [`MerkleMap::proof`](crate::MerkleMap::proof).
///
/// Each item is the encoding of one proof node. Nodes are visited (and
/// loaded) only as items are requested, so a caller can charge for each
/// node before it is produced and stop without further work.
pub struct ProofIter<'l, V, L: ?Sized> {
    loader: &'l L,
    next: Option<(Link<V>, Role)>,
    key: Vec<u8>,
    position: usize,
    probe: Probe,
}

impl<V, L: ?Sized> fmt::Debug for ProofIter<'_, V, L> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProofIter")
            .field("position", &self.position)
            .field("probe", &self.probe)
            .finish_non_exhaustive()
    }
}

impl<'l, V: TrieValue, L: NodeLoader<V> + ?Sized> ProofIter<'l, V, L> {
    pub(crate) fn new(root: Link<V>, key: Vec<u8>, loader: &'l L) -> Self {
        Self {
            loader,
            next: Some((root, Role::Root)),
            key,
            position: 0,
            probe: Probe::new(),
        }
    }

    /// The nodes visited and loads made so far.
    pub const fn probe(&self) -> Probe {
        self.probe
    }

    fn step(&mut self) -> Result<Option<Vec<u8>>, L::Error> {
        while let Some((link, role)) = self.next.take() {
            let Some(node) = resolve(&link, role, self.loader, &mut self.probe)? else {
                return Ok(None);
            };
            let rest = &self.key[self.position..];
            match &node.node {
                Node::Leaf { path, .. } => check_leaf(path, rest)?,
                Node::Extension { path, child } => {
                    check_extension(path, rest)?;
                    if rest.starts_with(path) {
                        self.position += path.len();
                        self.next = Some((child.clone(), Role::ExtensionChild));
                    }
                }
                Node::Branch { children } => {
                    let nibble = *rest
                        .first()
                        .ok_or(TrieError::inconsistent("branch below the key length"))?;
                    let child = &children[usize::from(nibble)];
                    if !child.is_empty() {
                        self.position += 1;
                        self.next = Some((child.clone(), Role::Child));
                    }
                }
            }
            if role == Role::Root || node.reference().is_hash() {
                let mut encoding = Vec::new();
                node.encode(&mut encoding);
                return Ok(Some(encoding));
            }
        }
        Ok(None)
    }
}

impl<V, L> Iterator for ProofIter<'_, V, L>
where
    V: TrieValue,
    L: NodeLoader<V> + ?Sized,
{
    type Item = Result<Vec<u8>, L::Error>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.step() {
            Ok(node) => node.map(Ok),
            Err(error) => {
                self.next = None;
                Some(Err(error))
            }
        }
    }
}

/// Why [`walk`] rejected a proof.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProofError {
    /// A complete interval contains more entries than the caller permits.
    EntryLimitExceeded,
    /// The trusted root is the empty-map root but the proof has nodes.
    NodesForEmptyRoot,
    /// keccak256 of the first node is not the trusted root.
    RootMismatch,
    /// The node at `index` does not hash to the reference that leads to it.
    HashMismatch {
        /// Position in the proof.
        index: usize,
    },
    /// The walk reached a hash reference with key nibbles left and the
    /// proof has no node at `index`. Never treated as absence.
    Incomplete {
        /// Position in the proof where a node was needed.
        index: usize,
    },
    /// The walk reached its terminal node but the proof continues at `index`.
    TrailingNodes {
        /// Position of the first unused node.
        index: usize,
    },
    /// The node at `index`, or a node inlined in it, is not a canonical trie
    /// node or contradicts a trie of keys of the given length.
    InvalidNode {
        /// Position in the proof.
        index: usize,
        /// What was wrong.
        reason: &'static str,
    },
}

impl fmt::Display for ProofError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EntryLimitExceeded => formatter.write_str("interval entry limit exceeded"),
            Self::NodesForEmptyRoot => formatter.write_str("nodes supplied for the empty root"),
            Self::RootMismatch => formatter.write_str("the first node does not hash to the root"),
            Self::HashMismatch { index } => {
                write!(formatter, "proof node {index} does not match its reference")
            }
            Self::Incomplete { index } => {
                write!(
                    formatter,
                    "proof ends before the key's path (node {index} missing)"
                )
            }
            Self::TrailingNodes { index } => {
                write!(
                    formatter,
                    "proof continues after its terminal node (node {index})"
                )
            }
            Self::InvalidNode { index, reason } => {
                write!(formatter, "proof node {index} is invalid: {reason}")
            }
        }
    }
}

impl std::error::Error for ProofError {}

/// How a visited node was referenced by its parent (roots are always hashed).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathSource {
    Root,
    Hash,
    Inline,
}

/// An authenticated child reference. Inline bytes are the exact child RLP.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathChild {
    Empty,
    Hash(ObjectId),
    Inline(Vec<u8>),
}
impl From<RawChild<'_>> for PathChild {
    fn from(child: RawChild<'_>) -> Self {
        match child {
            RawChild::Empty => Self::Empty,
            RawChild::Hash(hash) => Self::Hash(hash),
            RawChild::Inline(bytes) => Self::Inline(bytes.to_vec()),
        }
    }
}

/// One node actually visited by the strict verifier, including inline nodes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathNode {
    pub proof_index: usize,
    pub depth: usize,
    pub source: PathSource,
    pub hash: ObjectId,
    pub rlp: Vec<u8>,
    pub kind: crate::NodeKind,
    pub compressed_path: Vec<u8>,
    pub selected_nibble: Option<u8>,
    /// Sixteen slots for a branch, one child for an extension, none for a leaf.
    pub children: Vec<PathChild>,
    pub value_bytes: usize,
}

/// The reason a successfully verified path terminates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathTerminal {
    Inclusion,
    EmptyRoot,
    DivergentLeaf,
    DivergentExtension,
    EmptyBranchSlot,
}

/// A successful point proof. No trace is returned on any verification failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedPath {
    pub nodes: Vec<PathNode>,
    pub terminal: PathTerminal,
}

/// Strictly verify and describe the exact witness using the same walker as
/// [`walk`]. Callers bound key lengths and witness bytes before requesting a
/// trace; each trace node copies one visited node and its child references.
pub fn inspect_path<N: AsRef<[u8]>>(
    root: ObjectId,
    key: &[u8],
    nodes: &[N],
) -> Result<(Option<Vec<u8>>, VerifiedPath), ProofError> {
    let mut trace = VerifiedPath {
        nodes: Vec::new(),
        terminal: PathTerminal::EmptyRoot,
    };
    let value = walk_inner(root, key, nodes, Some(&mut trace))?;
    Ok((value, trace))
}

/// Verifies a point proof for `key` against a trusted `root`.
///
/// Returns `Ok(Some(value))` for inclusion (the leaf value bytes, as
/// [`TrieValue::encode`] produced them) and `Ok(None)` for absence. The
/// rules, all enforced:
///
/// - the empty map: `root == EMPTY_ROOT_HASH` and no nodes, which proves
///   absence; nodes with the empty root are [`ProofError::NodesForEmptyRoot`];
/// - `keccak256(nodes[0]) == root`, whatever the node's length;
/// - every later node is at least 32 bytes and hashes to the 33-byte hash
///   reference the walk reached along `key`'s nibbles; inline children are
///   decoded in place and never supplied separately;
/// - the terminal node is a leaf whose path equals the remaining key
///   (inclusion), a leaf or extension that diverges from it, or a branch
///   whose slot for the next nibble is empty (absence); nothing may follow
///   it;
/// - a hash reference reached with key nibbles left and no next node is
///   [`ProofError::Incomplete`], never absence;
/// - every node decodes strictly (canonical RLP and hex-prefix, no branch
///   value, at least two branch children, extension child a branch, leaf
///   paths exactly as long as the remaining key).
///
/// It never panics, whatever the input.
///
/// **Precondition:** `key` is the encoding of a key of the map, exactly
/// its [`TrieKey::LEN`](crate::TrieKey::LEN) bytes as
/// [`TrieKey::encode`](crate::TrieKey::encode) produces them; a verifier
/// composing proofs encodes the key itself before calling `walk`. The
/// proof does not carry the length, so `walk` cannot check it. A key of
/// another length is never proven included under the root of a map of
/// fixed-length keys, but depending on the trie's shape it is reported as
/// absent or as [`ProofError::InvalidNode`] (a leaf at the wrong depth
/// for the key), and neither result then says anything about the proof.
pub fn walk<N: AsRef<[u8]>>(
    root: ObjectId,
    key: &[u8],
    nodes: &[N],
) -> Result<Option<Vec<u8>>, ProofError> {
    walk_inner(root, key, nodes, None)
}

fn walk_inner<N: AsRef<[u8]>>(
    root: ObjectId,
    key: &[u8],
    nodes: &[N],
    mut trace: Option<&mut VerifiedPath>,
) -> Result<Option<Vec<u8>>, ProofError> {
    if root == EMPTY_ROOT_HASH {
        return if nodes.is_empty() {
            Ok(None)
        } else {
            Err(ProofError::NodesForEmptyRoot)
        };
    }
    let first = nodes
        .first()
        .ok_or(ProofError::Incomplete { index: 0 })?
        .as_ref();
    if keccak256(first) != root {
        return Err(ProofError::RootMismatch);
    }
    let key = nibbles::unpack(key);
    let finish = |index: usize, value: Option<Vec<u8>>| {
        if index + 1 == nodes.len() {
            Ok(value)
        } else {
            Err(ProofError::TrailingNodes { index: index + 1 })
        }
    };
    let mut index = 0;
    let mut current = first;
    let mut position = 0;
    let mut below_extension = false;
    let mut source = PathSource::Root;
    loop {
        let invalid = move |reason| ProofError::InvalidNode { index, reason };
        let rest = &key[position..];
        let raw = decode_raw(current).map_err(invalid)?;
        if let Some(trace) = trace.as_deref_mut() {
            let (kind, path, selected_nibble, children, value_bytes) = match &raw {
                RawNode::Leaf { path, value } => (
                    crate::NodeKind::Leaf,
                    path.clone(),
                    None,
                    Vec::new(),
                    value.len(),
                ),
                RawNode::Extension { path, child } => (
                    crate::NodeKind::Extension,
                    path.clone(),
                    None,
                    vec![(*child).into()],
                    0,
                ),
                RawNode::Branch { children } => (
                    crate::NodeKind::Branch,
                    Vec::new(),
                    rest.first().copied(),
                    children.iter().copied().map(PathChild::from).collect(),
                    0,
                ),
            };
            trace.nodes.push(PathNode {
                proof_index: index,
                depth: position,
                source,
                hash: keccak256(current),
                rlp: current.to_vec(),
                kind,
                compressed_path: path,
                selected_nibble,
                children,
                value_bytes,
            });
        }
        let child = match raw {
            RawNode::Leaf { path, value } => {
                if below_extension {
                    return Err(invalid("extension child is not a branch"));
                }
                if path.len() != rest.len() {
                    return Err(invalid("leaf at the wrong depth for the key length"));
                }
                if let Some(trace) = trace.as_deref_mut() {
                    trace.terminal = if path == rest {
                        PathTerminal::Inclusion
                    } else {
                        PathTerminal::DivergentLeaf
                    };
                }
                return finish(index, (path == rest).then(|| value.to_vec()));
            }
            RawNode::Extension { path, child } => {
                if below_extension {
                    return Err(invalid("extension child is not a branch"));
                }
                if path.len() >= rest.len() {
                    return Err(invalid("extension reaches the end of the key"));
                }
                if !rest.starts_with(&path) {
                    if let Some(trace) = trace.as_deref_mut() {
                        trace.terminal = PathTerminal::DivergentExtension;
                    }
                    return finish(index, None);
                }
                position += path.len();
                below_extension = true;
                child
            }
            RawNode::Branch { children } => {
                below_extension = false;
                let nibble = *rest
                    .first()
                    .ok_or_else(|| invalid("branch below the key length"))?;
                let child = children[usize::from(nibble)];
                if child == RawChild::Empty {
                    if let Some(trace) = trace.as_deref_mut() {
                        trace.terminal = PathTerminal::EmptyBranchSlot;
                    }
                    return finish(index, None);
                }
                position += 1;
                child
            }
        };
        current = match child {
            RawChild::Inline(bytes) => {
                source = PathSource::Inline;
                bytes
            }
            RawChild::Hash(hash) => {
                source = PathSource::Hash;
                index += 1;
                let node = nodes
                    .get(index)
                    .ok_or(ProofError::Incomplete { index })?
                    .as_ref();
                if node.len() < 32 {
                    return Err(ProofError::InvalidNode {
                        index,
                        reason: "hash-referenced node shorter than 32 bytes",
                    });
                }
                if keccak256(node) != hash {
                    return Err(ProofError::HashMismatch { index });
                }
                node
            }
            RawChild::Empty => return Err(invalid("extension without a child")),
        };
    }
}
