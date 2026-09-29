//! Complete closed intervals over encoded fixed-length keys. Only subtrees
//! whose key prefix intersects the interval are opened. Witness nodes are in
//! depth-first key order; inline children remain in their parent's encoding.

use alloy_primitives::keccak256;

use crate::{
    EMPTY_ROOT_HASH, Link, NodeLoader, ObjectId, PathChild, PathNode, PathSource, Probe,
    ProofError, TrieError, TrieValue,
    map::{Role, resolve},
    nibbles,
    node::{Node, RawChild, RawNode, decode_raw},
};

// Every completion of a prefix lies between prefix||00… and prefix||ff….
// Comparing against the bound prefixes avoids allocating these completions.
fn intersects(prefix: &[u8], lower: &[u8], upper: &[u8]) -> bool {
    lower <= upper && prefix >= &lower[..prefix.len()] && prefix <= &upper[..prefix.len()]
}

/// Lazy interval witness. Each `next` may also visit inline descendants;
/// `probe` counts those visits. Bounds are raw encoded keys and need not be
/// inhabited keys (which permits typed-index sentinels).
pub struct IntervalProofIter<'a, V, L: ?Sized> {
    loader: &'a L,
    lower: Vec<u8>,
    upper: Vec<u8>,
    pending: Vec<(Link<V>, Role, usize, Option<u8>)>,
    prefix: Vec<u8>,
    probe: Probe,
}

impl<'a, V: TrieValue, L: NodeLoader<V> + ?Sized> IntervalProofIter<'a, V, L> {
    pub(crate) fn new(root: Link<V>, lower: &[u8], upper: &[u8], loader: &'a L) -> Self {
        Self {
            loader,
            lower: nibbles::unpack(lower),
            upper: nibbles::unpack(upper),
            pending: if lower <= upper {
                vec![(root, Role::Root, 0, None)]
            } else {
                vec![]
            },
            prefix: Vec::with_capacity(2 * lower.len()),
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
                    if prefix.len() + path.len() != self.lower.len() {
                        return Err(TrieError::inconsistent(
                            "leaf at the wrong depth for the key length",
                        )
                        .into());
                    }
                }
                Node::Extension { path, child } => {
                    if prefix.len() + path.len() >= self.lower.len() {
                        return Err(TrieError::inconsistent(
                            "extension reaches the end of the key",
                        )
                        .into());
                    }
                    prefix.extend_from_slice(path);
                    if intersects(prefix, &self.lower, &self.upper) {
                        self.pending.push((
                            child.clone(),
                            Role::ExtensionChild,
                            prefix.len(),
                            None,
                        ));
                    }
                }
                Node::Branch { children } => {
                    if prefix.len() >= self.lower.len() {
                        return Err(TrieError::inconsistent("branch below the key length").into());
                    }
                    for (slot, child) in children.iter().enumerate().rev() {
                        if child.is_empty() {
                            continue;
                        }
                        prefix.push(slot as u8);
                        if intersects(prefix, &self.lower, &self.upper) {
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
/// walker as [`walk_interval`]. Callers must bound key lengths, witness size,
/// and trace allocation: each visited node copies its bytes, references and
/// prefix, including inline nodes and leaves excluded by the final key check.
pub fn inspect_interval<N: AsRef<[u8]>>(
    root: ObjectId,
    lower: &[u8],
    upper: &[u8],
    nodes: &[N],
    max_entries: usize,
) -> Result<(IntervalEntries, VerifiedInterval), ProofError> {
    let mut trace = VerifiedInterval { nodes: Vec::new() };
    let entries = walk_interval_inner(root, lower, upper, nodes, max_entries, Some(&mut trace))?;
    Ok((entries, trace))
}

/// Verify and return *every* entry in the closed encoded-key interval, in key
/// order. A missing intersecting child is an error, never an empty subtree.
/// Empty intervals require an empty witness. `max_entries` is checked before
/// copying a leaf; the caller must additionally bound key size and witness
/// bytes/nodes before invoking this function (as with the point walker).
pub fn walk_interval<N: AsRef<[u8]>>(
    root: ObjectId,
    lower: &[u8],
    upper: &[u8],
    nodes: &[N],
    max_entries: usize,
) -> Result<IntervalEntries, ProofError> {
    walk_interval_inner(root, lower, upper, nodes, max_entries, None)
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
    lower: &[u8],
    upper: &[u8],
    nodes: &[N],
    max_entries: usize,
    mut trace: Option<&mut VerifiedInterval>,
) -> Result<IntervalEntries, ProofError> {
    if lower.is_empty() || lower.len() != upper.len() {
        return Err(ProofError::InvalidNode {
            index: 0,
            reason: "interval key lengths disagree",
        });
    }
    if lower > upper || root == EMPTY_ROOT_HASH {
        return if nodes.is_empty() {
            Ok(vec![])
        } else {
            Err(ProofError::TrailingNodes { index: 0 })
        };
    }
    let lower = nibbles::unpack(lower);
    let upper = nibbles::unpack(upper);
    let mut pending = vec![PendingNode {
        child: RawChild::Hash(root),
        role: Role::Root,
        depth: 0,
        slot: None,
        parent_index: None,
        supplied_ancestor: 0,
    }];
    let mut prefix = Vec::with_capacity(lower.len());
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
                if prefix.len() + path.len() != lower.len() {
                    return Err(invalid("leaf at the wrong depth for the key length"));
                }
                prefix.extend_from_slice(&path);
                let included = intersects(&prefix, &lower, &upper);
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
                if prefix.len() + path.len() >= lower.len() {
                    return Err(invalid("extension reaches the end of the key"));
                }
                prefix.extend_from_slice(&path);
                let open = intersects(&prefix, &lower, &upper);
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
                if prefix.len() >= lower.len() {
                    return Err(invalid("branch below the key length"));
                }
                for (slot, child) in children.into_iter().enumerate().rev() {
                    if child == RawChild::Empty {
                        continue;
                    }
                    prefix.push(slot as u8);
                    let open = intersects(&prefix, &lower, &upper);
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
