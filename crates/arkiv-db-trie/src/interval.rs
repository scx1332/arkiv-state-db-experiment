//! Complete intervals over encoded variable-length keys. Only subtrees
//! whose key prefix can intersect the interval are opened. Witness nodes are
//! in depth-first key order; inline children remain in their parent's
//! encoding.
//!
//! Bounds are raw byte strings compared by unsigned bytewise lexicographic
//! order (a proper prefix first), each inclusive, exclusive or unbounded.
//! The producer ([`IntervalProofIter`]) and the verifier
//! ([`walk_interval`]) share one pruning rule, [`KeyRange::may_hold`] over
//! nibble prefixes, so they open exactly the same children.

use std::ops::Bound;

use alloy_primitives::keccak256;

use crate::{
    EMPTY_ROOT_HASH, Link, NodeLoader, ObjectId, PathChild, PathNode, PathSource, Probe,
    ProofError, TrieValue,
    map::{Role, check_inner_end, check_leaf_end, resolve},
    nibbles,
    node::{Node, RawChild, RawNode, decode_raw},
};

/// An interval of encoded keys: each end inclusive, exclusive or unbounded.
///
/// Endpoints are raw byte strings and need not be encodings of stored keys.
/// Order is unsigned bytewise lexicographic, a proper prefix first, so an
/// exclusive lower bound `a` still admits `aa`. A prefix run is
/// [`KeyRange::prefix`]: `[p, prefix_successor(p))`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyRange {
    /// The lower endpoint.
    pub lower: Bound<Vec<u8>>,
    /// The upper endpoint.
    pub upper: Bound<Vec<u8>>,
}

impl KeyRange {
    /// The interval between two endpoints.
    pub const fn new(lower: Bound<Vec<u8>>, upper: Bound<Vec<u8>>) -> Self {
        Self { lower, upper }
    }

    /// The closed interval `[lower, upper]`.
    pub fn closed(lower: &[u8], upper: &[u8]) -> Self {
        Self::new(
            Bound::Included(lower.to_vec()),
            Bound::Included(upper.to_vec()),
        )
    }

    /// Every key.
    pub const fn all() -> Self {
        Self::new(Bound::Unbounded, Bound::Unbounded)
    }

    /// Every key starting with `prefix`: `[prefix, prefix_successor(prefix))`,
    /// unbounded above when the prefix is empty or all `0xff`.
    pub fn prefix(prefix: &[u8]) -> Self {
        Self::new(
            Bound::Included(prefix.to_vec()),
            crate::order::prefix_successor(prefix).map_or(Bound::Unbounded, Bound::Excluded),
        )
    }

    /// Whether no byte string can satisfy both ends by endpoint comparison:
    /// a lower endpoint above the upper one, or equal endpoints with either
    /// excluded. An empty interval has an empty witness.
    pub fn is_empty(&self) -> bool {
        match (&self.lower, &self.upper) {
            (Bound::Included(lower), Bound::Included(upper)) => lower > upper,
            (
                Bound::Included(lower) | Bound::Excluded(lower),
                Bound::Included(upper) | Bound::Excluded(upper),
            ) => lower >= upper,
            _ => false,
        }
    }

    /// Whether the encoded key `key` lies in the interval.
    pub fn contains(&self, key: &[u8]) -> bool {
        (match &self.lower {
            Bound::Unbounded => true,
            Bound::Included(lower) => key >= lower.as_slice(),
            Bound::Excluded(lower) => key > lower.as_slice(),
        }) && (match &self.upper {
            Bound::Unbounded => true,
            Bound::Included(upper) => key <= upper.as_slice(),
            Bound::Excluded(upper) => key < upper.as_slice(),
        })
    }

    fn unpacked(&self) -> Nibbled {
        let unpack = |bound: &Bound<Vec<u8>>| match bound {
            Bound::Unbounded => Bound::Unbounded,
            Bound::Included(bytes) => Bound::Included(nibbles::unpack(bytes)),
            Bound::Excluded(bytes) => Bound::Excluded(nibbles::unpack(bytes)),
        };
        Nibbled {
            lower: unpack(&self.lower),
            upper: unpack(&self.upper),
        }
    }

    /// Whether a subtree whose keys all start with the nibble path `prefix`
    /// can hold a key in the interval. Conservative (it may open a subtree
    /// that turns out to hold nothing in range) and shared by the producer
    /// and the verifier. A key below `prefix` extends it or equals it, so:
    /// the subtree is entirely below the lower endpoint only when `prefix`
    /// differs from it at a nibble where `prefix` is smaller; and it is
    /// entirely above the upper endpoint when its smallest possible key
    /// (`prefix`, completed with a zero nibble to a whole byte) is above it
    /// (or equal to an excluded upper endpoint).
    pub fn may_hold(&self, prefix_nibbles: &[u8]) -> bool {
        self.unpacked().may_hold(prefix_nibbles)
    }
}

/// A [`KeyRange`] with nibble endpoints.
struct Nibbled {
    lower: Bound<Vec<u8>>,
    upper: Bound<Vec<u8>>,
}

impl Nibbled {
    fn may_hold(&self, prefix: &[u8]) -> bool {
        let above_lower = match &self.lower {
            Bound::Unbounded => true,
            Bound::Included(lower) | Bound::Excluded(lower) => {
                let shared = prefix.len().min(lower.len());
                prefix[..shared] >= lower[..shared]
            }
        };
        above_lower
            && match &self.upper {
                Bound::Unbounded => true,
                Bound::Included(upper) => smallest_completion(prefix).cmp(upper.iter()).is_le(),
                Bound::Excluded(upper) => smallest_completion(prefix).cmp(upper.iter()).is_lt(),
            }
    }

    /// Whether the whole key (as nibbles, an even count) is in the interval.
    fn contains(&self, key: &[u8]) -> bool {
        (match &self.lower {
            Bound::Unbounded => true,
            Bound::Included(lower) => key >= lower.as_slice(),
            Bound::Excluded(lower) => key > lower.as_slice(),
        }) && (match &self.upper {
            Bound::Unbounded => true,
            Bound::Included(upper) => key <= upper.as_slice(),
            Bound::Excluded(upper) => key < upper.as_slice(),
        })
    }
}

/// The smallest whole-byte key starting with the nibble path `prefix`:
/// `prefix`, plus a zero nibble when it ends inside a byte.
fn smallest_completion(prefix: &[u8]) -> impl Iterator<Item = &u8> {
    prefix
        .iter()
        .chain((!prefix.len().is_multiple_of(2)).then_some(&0))
}

/// Lazy interval witness. Each `next` may also visit inline descendants;
/// `probe` counts those visits. Bounds are raw encoded keys and need not be
/// inhabited keys (which permits typed-index sentinels).
pub struct IntervalProofIter<'a, V, L: ?Sized> {
    loader: &'a L,
    range: Nibbled,
    max_nibbles: usize,
    pending: Vec<(Link<V>, Role, usize, Option<u8>)>,
    prefix: Vec<u8>,
    probe: Probe,
}

impl<'a, V: TrieValue, L: NodeLoader<V> + ?Sized> IntervalProofIter<'a, V, L> {
    pub(crate) fn new(root: Link<V>, range: &KeyRange, max_key_len: usize, loader: &'a L) -> Self {
        Self {
            loader,
            range: range.unpacked(),
            max_nibbles: max_key_len.saturating_mul(2),
            pending: if range.is_empty() {
                vec![]
            } else {
                vec![(root, Role::Root, 0, None)]
            },
            prefix: Vec::new(),
            probe: Probe::new(),
        }
    }

    pub const fn probe(&self) -> Probe {
        self.probe
    }

    fn step(&mut self) -> Result<Option<Vec<u8>>, L::Error> {
        while let Some((link, role, depth, slot)) = self.pending.pop() {
            self.prefix.truncate(depth);
            if let Some(slot) = slot {
                self.prefix.push(slot);
            }
            let prefix = &mut self.prefix;
            let Some(node) = resolve(&link, role, self.loader, &mut self.probe)? else {
                continue;
            };
            match &node.node {
                Node::Leaf { path, .. } => {
                    check_leaf_end(prefix.len() + path.len(), self.max_nibbles)?;
                }
                Node::Extension { path, child } => {
                    check_inner_end(prefix.len() + path.len(), self.max_nibbles)?;
                    prefix.extend_from_slice(path);
                    if self.range.may_hold(prefix) {
                        self.pending.push((
                            child.clone(),
                            Role::ExtensionChild,
                            prefix.len(),
                            None,
                        ));
                    }
                }
                Node::Branch { children } => {
                    check_inner_end(prefix.len(), self.max_nibbles)?;
                    for (slot, child) in children.iter().enumerate().rev() {
                        if child.is_empty() {
                            continue;
                        }
                        prefix.push(slot as u8);
                        if self.range.may_hold(prefix) {
                            self.pending.push((
                                child.clone(),
                                Role::Child,
                                prefix.len() - 1,
                                Some(slot as u8),
                            ));
                        }
                        prefix.pop();
                    }
                }
            }
            if role == Role::Root || node.reference().is_hash() {
                let mut bytes = Vec::new();
                node.encode(&mut bytes);
                return Ok(Some(bytes));
            }
        }
        Ok(None)
    }
}

impl<V: TrieValue, L: NodeLoader<V> + ?Sized> Iterator for IntervalProofIter<'_, V, L> {
    type Item = Result<Vec<u8>, L::Error>;
    fn next(&mut self) -> Option<Self::Item> {
        match self.step() {
            Ok(node) => node.map(Ok),
            Err(error) => {
                self.pending.clear();
                Some(Err(error))
            }
        }
    }
}

/// The strict interval walk's decision for an authenticated child reference.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntervalDecision {
    /// The child's prefix intersects the interval, so the child was visited.
    Open,
    /// The nonempty child's prefix cannot intersect the interval.
    Outside,
    /// The child reference is empty, as authenticated by the parent node.
    Empty,
}

/// One node actually visited by the strict interval verifier.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntervalNode {
    /// Exact node bytes and references. For inline nodes, `proof_index` names
    /// the supplied ancestor whose bytes contain this node, even if another
    /// hashed subtree was visited in between. `selected_nibble` is always None.
    pub path_node: PathNode,
    /// Parent's index in [`VerifiedInterval::nodes`], including inline nodes.
    pub parent_index: Option<usize>,
    /// Branch slot leading to this node; None for a root or extension edge.
    pub parent_slot: Option<u8>,
    /// Nibble prefix before this node's compressed path, if any.
    pub prefix: Vec<u8>,
    /// Aligned with `path_node.children`: sixteen branch slots, one extension
    /// child, or no entries for a leaf. Empty slots are marked even outside
    /// the interval, since their absence is authenticated by the parent.
    pub child_decisions: Vec<IntervalDecision>,
    /// Complete encoded key for a visited leaf, including excluded leaves.
    pub leaf_key: Option<Vec<u8>>,
    pub leaf_value: Option<Vec<u8>>,
    pub leaf_included: bool,
}

/// A successful interval proof. No trace is returned on verification failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedInterval {
    pub nodes: Vec<IntervalNode>,
}

/// Maximum visited nodes (including inline nodes) in an inspected interval.
/// A larger witness fails with [`ProofError::EntryLimitExceeded`]; it never
/// yields a truncated successful trace. Ordinary interval walks have no
/// trace-node limit.
pub const MAX_INTERVAL_TRACE_NODES: usize = 16_384;

/// Complete encoded key/value pairs in increasing key order.
pub type IntervalEntries = Vec<(Vec<u8>, Vec<u8>)>;

/// Strictly verify and describe the exact branching witness, using the same
/// walker as [`walk_interval`]. Callers must bound witness size and trace
/// allocation: each visited node copies its bytes, references and prefix,
/// including inline nodes and leaves excluded by the final key check.
pub fn inspect_interval<N: AsRef<[u8]>>(
    root: ObjectId,
    range: &KeyRange,
    nodes: &[N],
    limits: IntervalLimits,
) -> Result<(IntervalEntries, VerifiedInterval), ProofError> {
    let mut trace = VerifiedInterval { nodes: Vec::new() };
    let entries = walk_interval_inner(root, range, nodes, limits, Some(&mut trace))?;
    Ok((entries, trace))
}

/// Bounds of one interval verification. Exceeding `max_entries` is
/// [`ProofError::EntryLimitExceeded`]; a path or key longer than
/// `max_key_len` bytes is [`ProofError::InvalidNode`]. Neither ever yields a
/// truncated successful result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IntervalLimits {
    /// Most entries the interval may hold.
    pub max_entries: usize,
    /// The map's key bound, [`TrieKey::MAX_LEN`](crate::TrieKey::MAX_LEN).
    pub max_key_len: usize,
}

/// Verify and return *every* entry in the encoded-key interval, in key
/// order. A missing intersecting child is an error, never an empty subtree.
/// Empty intervals ([`KeyRange::is_empty`]) require an empty witness.
/// `max_entries` is checked before copying a leaf and `max_key_len` before
/// descending; the caller must additionally bound witness bytes/nodes before
/// invoking this function (as with the point walker).
pub fn walk_interval<N: AsRef<[u8]>>(
    root: ObjectId,
    range: &KeyRange,
    nodes: &[N],
    limits: IntervalLimits,
) -> Result<IntervalEntries, ProofError> {
    walk_interval_inner(root, range, nodes, limits, None)
}

struct PendingNode<'a> {
    child: RawChild<'a>,
    role: Role,
    depth: usize,
    slot: Option<u8>,
    parent_index: Option<usize>,
    supplied_ancestor: usize,
}

fn walk_interval_inner<N: AsRef<[u8]>>(
    root: ObjectId,
    range: &KeyRange,
    nodes: &[N],
    limits: IntervalLimits,
    mut trace: Option<&mut VerifiedInterval>,
) -> Result<IntervalEntries, ProofError> {
    let IntervalLimits {
        max_entries,
        max_key_len,
    } = limits;
    if range.is_empty() || root == EMPTY_ROOT_HASH {
        return if nodes.is_empty() {
            Ok(vec![])
        } else {
            Err(ProofError::TrailingNodes { index: 0 })
        };
    }
    let max_nibbles = max_key_len.saturating_mul(2);
    let range = range.unpacked();
    let mut pending = vec![PendingNode {
        child: RawChild::Hash(root),
        role: Role::Root,
        depth: 0,
        slot: None,
        parent_index: None,
        supplied_ancestor: 0,
    }];
    let mut prefix = Vec::new();
    let mut consumed = 0;
    let mut entries = Vec::new();
    while let Some(PendingNode {
        child,
        role,
        depth,
        slot,
        parent_index,
        supplied_ancestor,
    }) = pending.pop()
    {
        prefix.truncate(depth);
        if let Some(slot) = slot {
            prefix.push(slot);
        }
        let index = consumed;
        let (bytes, source, proof_index) = match child {
            RawChild::Empty => continue,
            RawChild::Inline(bytes) => (bytes, PathSource::Inline, supplied_ancestor),
            RawChild::Hash(hash) => {
                let bytes = nodes
                    .get(index)
                    .ok_or(ProofError::Incomplete { index })?
                    .as_ref();
                if role != Role::Root && bytes.len() < 32 {
                    return Err(ProofError::InvalidNode {
                        index,
                        reason: "hash-referenced node shorter than 32 bytes",
                    });
                }
                if keccak256(bytes) != hash {
                    return Err(if role == Role::Root {
                        ProofError::RootMismatch
                    } else {
                        ProofError::HashMismatch { index }
                    });
                }
                consumed += 1;
                let source = if role == Role::Root {
                    PathSource::Root
                } else {
                    PathSource::Hash
                };
                (bytes, source, index)
            }
        };
        let invalid = |reason| ProofError::InvalidNode { index, reason };
        let raw = decode_raw(bytes).map_err(invalid)?;
        if role == Role::ExtensionChild && !matches!(raw, RawNode::Branch { .. }) {
            return Err(invalid("extension child is not a branch"));
        }
        let trace_index = trace.as_ref().map(|trace| trace.nodes.len());
        if let Some(trace) = trace.as_deref_mut() {
            if trace.nodes.len() == MAX_INTERVAL_TRACE_NODES {
                return Err(ProofError::EntryLimitExceeded);
            }
            let (kind, path, children, value_bytes) = match &raw {
                RawNode::Leaf { path, value } => {
                    (crate::NodeKind::Leaf, path.clone(), Vec::new(), value.len())
                }
                RawNode::Extension { path, child } => (
                    crate::NodeKind::Extension,
                    path.clone(),
                    vec![(*child).into()],
                    0,
                ),
                RawNode::Branch { children } => (
                    crate::NodeKind::Branch,
                    Vec::new(),
                    children.iter().copied().map(PathChild::from).collect(),
                    0,
                ),
            };
            trace.nodes.push(IntervalNode {
                child_decisions: vec![IntervalDecision::Empty; children.len()],
                path_node: PathNode {
                    proof_index,
                    depth: prefix.len(),
                    source,
                    hash: keccak256(bytes),
                    rlp: bytes.to_vec(),
                    kind,
                    compressed_path: path,
                    selected_nibble: None,
                    children,
                    value_bytes,
                },
                parent_index,
                parent_slot: slot,
                prefix: prefix.clone(),
                leaf_key: None,
                leaf_value: None,
                leaf_included: false,
            });
        }
        match raw {
            RawNode::Leaf { path, value } => {
                let end = prefix.len() + path.len();
                if !end.is_multiple_of(2) {
                    return Err(invalid("leaf key ends inside a byte"));
                }
                if end > max_nibbles {
                    return Err(invalid("leaf key exceeds the key length bound"));
                }
                prefix.extend_from_slice(&path);
                let included = range.contains(&prefix);
                if included {
                    if entries.len() == max_entries {
                        return Err(ProofError::EntryLimitExceeded);
                    }
                    entries.push((nibbles::pack(&prefix), value.to_vec()));
                }
                if let Some(node) = trace
                    .as_deref_mut()
                    .and_then(|trace| trace.nodes.last_mut())
                {
                    node.leaf_key = Some(nibbles::pack(&prefix));
                    node.leaf_value = Some(value.to_vec());
                    node.leaf_included = included;
                }
            }
            RawNode::Extension { path, child } => {
                if prefix.len() + path.len() >= max_nibbles {
                    return Err(invalid("extension reaches the key length bound"));
                }
                prefix.extend_from_slice(&path);
                let open = range.may_hold(&prefix);
                if let Some(node) = trace
                    .as_deref_mut()
                    .and_then(|trace| trace.nodes.last_mut())
                {
                    node.child_decisions[0] = if open {
                        IntervalDecision::Open
                    } else {
                        IntervalDecision::Outside
                    };
                }
                if open {
                    pending.push(PendingNode {
                        child,
                        role: Role::ExtensionChild,
                        depth: prefix.len(),
                        slot: None,
                        parent_index: trace_index,
                        supplied_ancestor: proof_index,
                    });
                }
            }
            RawNode::Branch { children } => {
                if prefix.len() >= max_nibbles {
                    return Err(invalid("branch reaches the key length bound"));
                }
                for (slot, child) in children.into_iter().enumerate().rev() {
                    if child == RawChild::Empty {
                        continue;
                    }
                    prefix.push(slot as u8);
                    let open = range.may_hold(&prefix);
                    if let Some(node) = trace
                        .as_deref_mut()
                        .and_then(|trace| trace.nodes.last_mut())
                    {
                        node.child_decisions[slot] = if open {
                            IntervalDecision::Open
                        } else {
                            IntervalDecision::Outside
                        };
                    }
                    if open {
                        pending.push(PendingNode {
                            child,
                            role: Role::Child,
                            depth: prefix.len() - 1,
                            slot: Some(slot as u8),
                            parent_index: trace_index,
                            supplied_ancestor: proof_index,
                        });
                    }
                    prefix.pop();
                }
            }
        }
    }
    if consumed != nodes.len() {
        return Err(ProofError::TrailingNodes { index: consumed });
    }
    Ok(entries)
}
