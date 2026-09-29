//! Collecting the new nodes of a map for an atomic commit.
//!
//! The staging walk ([`MerkleMap::stage`](crate::MerkleMap::stage)) starts
//! at a map's root and emits `(id, encoding)` for every node built in memory
//! (`from_store == false`) that must be an object of its own: the root,
//! whatever its size, and every node of 32 bytes or more. Inline nodes live
//! inside their parent's encoding and are never emitted separately. The
//! walk stops at [`Link::Stored`](crate::Link::Stored) links and at
//! `from_store` nodes, because everything below them is already committed,
//! so its work is bounded by the nodes the updates since the last commit
//! created. Leaf values of new nodes get the chance to stage their nested
//! maps. The caller decides the object space: it hands each space its own
//! sink, which also deduplicates by id within that space.
//!
//! The walk does not stop at a node whose id the sink already holds. Maps
//! with different value types may share a space and so a sink, and a leaf
//! of one map can be byte-identical to a leaf of another whose value names
//! a nested map (the same bytes read as a plain value in one and as a
//! nested root in the other). An equal id therefore does not mean that the
//! nested maps below it were staged, so every new node is walked and every
//! new leaf value visited, and only the recording is deduplicated.

use std::{collections::HashSet, fmt};

use alloy_primitives::keccak256;

use crate::{Link, ObjectId, TrieValue, nibbles, node::Node};

/// A preparation failure, independent of the durable sink's own errors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StageError {
    /// The next new node would exceed this phase's visit allowance.
    VisitLimit,
    /// A node's conservative encoded size exceeds the object ceiling.
    ObjectTooLarge { size: usize, limit: usize },
    /// An encoded-size calculation cannot be represented by `usize`.
    SizeOverflow,
}

impl fmt::Display for StageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::VisitLimit => formatter.write_str("new-node visit allowance exhausted"),
            Self::ObjectTooLarge { size, limit } => {
                write!(
                    formatter,
                    "node size bound {size} exceeds object limit {limit}"
                )
            }
            Self::SizeOverflow => formatter.write_str("encoded node size overflow"),
        }
    }
}

impl std::error::Error for StageError {}

/// A finite new-node allowance shared across all maps of one preparation phase.
///
/// Use a fresh budget with the same limit for preflight and staging, and pass
/// each phase's budget through every nested-map callback. Empty/stored links
/// and nodes decoded from storage do not spend this allowance. This physical
/// instrumentation does not change logical [`crate::Probe`] counts.
#[derive(Debug)]
pub struct StageBudget {
    limit: u64,
    visited: u64,
}

impl StageBudget {
    /// Creates a new allowance; zero permits only already committed/empty maps.
    pub const fn new(max_new_nodes: u64) -> Self {
        Self {
            limit: max_new_nodes,
            visited: 0,
        }
    }

    /// New nodes visited so far, including visits in nested maps.
    pub const fn visited(&self) -> u64 {
        self.visited
    }

    /// Visits still available.
    pub const fn remaining(&self) -> u64 {
        self.limit - self.visited
    }

    fn visit(&mut self) -> Result<(), StageError> {
        if self.visited == self.limit {
            return Err(StageError::VisitLimit);
        }
        self.visited += 1;
        Ok(())
    }
}

/// A fallible destination for a preflighted staging walk.
///
/// The sink checks its object/byte capacities before retaining an encoding and
/// owns physical deduplication. Returning an error immediately stops the walk,
/// including nested callbacks; entries accepted earlier remain in the sink.
pub trait TryObjectSink {
    /// Sink/resource errors, including traversal failures.
    type Error: From<StageError>;

    /// Records an object or aborts preparation without retaining this object.
    fn record(&mut self, id: ObjectId, encoding: &[u8]) -> Result<(), Self::Error>;
}

/// Synchronous nested-map staging callback with the shared sink and allowance.
pub type NestedStageVisitor<'a, V, S> =
    dyn FnMut(&V, &mut S, &mut StageBudget) -> Result<(), <S as TryObjectSink>::Error> + 'a;

fn add(left: usize, right: usize) -> Result<usize, StageError> {
    left.checked_add(right).ok_or(StageError::SizeOverflow)
}

/// RLP list size, also a conservative byte-string size. A one-byte string
/// below 0x80 actually omits its prefix; its length alone cannot prove that.
fn framed_size(payload: usize) -> Result<usize, StageError> {
    let length_bytes = if payload < 56 {
        0
    } else {
        (usize::BITS - payload.leading_zeros()).div_ceil(8) as usize
    };
    add(add(payload, 1)?, length_bytes)
}

fn path_size(nibble_count: usize) -> Result<usize, StageError> {
    let bytes = nibbles::encoded_path_len(nibble_count);
    // A one-byte compact path always has its flag nibble below 0x80.
    if bytes == 1 {
        Ok(1)
    } else {
        framed_size(bytes)
    }
}

pub(crate) fn preflight_link<V: TrieValue, E: From<StageError>>(
    link: &Link<V>,
    max_object_bytes: usize,
    budget: &mut StageBudget,
    value_size: &mut dyn FnMut(&V, &mut StageBudget) -> Result<usize, E>,
) -> Result<usize, E> {
    let node = match link {
        Link::Empty => return Ok(1),
        Link::Stored(_) => return Ok(33),
        Link::Mem(node) => node,
    };
    if let Some(length) = node.stored_reference_len() {
        return Ok(length);
    }
    budget.visit()?;
    let payload = match &node.node {
        Node::Leaf { path, value } => {
            let value = value_size(value, budget)?;
            add(path_size(path.len())?, framed_size(value)?)?
        }
        Node::Extension { path, child } => {
            let reference = preflight_link(child, max_object_bytes, budget, value_size)?;
            add(path_size(path.len())?, reference)?
        }
        Node::Branch { children } => {
            let mut payload = 1; // Empty value after the sixteen child slots.
            for child in children.iter() {
                payload = add(
                    payload,
                    preflight_link(child, max_object_bytes, budget, value_size)?,
                )?;
            }
            payload
        }
    };
    let size = framed_size(payload)?;
    if size > max_object_bytes {
        return Err(StageError::ObjectTooLarge {
            size,
            limit: max_object_bytes,
        }
        .into());
    }
    Ok(if size < 32 { size } else { 33 })
}

pub(crate) fn try_stage_link<V: TrieValue, S: TryObjectSink + ?Sized>(
    link: &Link<V>,
    root: bool,
    sink: &mut S,
    budget: &mut StageBudget,
    nested: &mut NestedStageVisitor<'_, V, S>,
    buffer: &mut Vec<u8>,
) -> Result<(), S::Error> {
    let Link::Mem(node) = link else {
        return Ok(());
    };
    if node.from_store() {
        return Ok(());
    }
    budget.visit()?;
    let reference = node.reference();
    if root || reference.is_hash() {
        buffer.clear();
        node.encode(buffer);
        let id = reference
            .as_hash()
            .unwrap_or_else(|| keccak256(buffer.as_slice()));
        sink.record(id, buffer)?;
    }
    match &node.node {
        Node::Leaf { value, .. } => nested(value, sink, budget)?,
        Node::Extension { child, .. } => {
            try_stage_link(child, false, sink, budget, nested, buffer)?
        }
        Node::Branch { children } => {
            for child in children.iter() {
                try_stage_link(child, false, sink, budget, nested, buffer)?;
            }
        }
    }
    Ok(())
}

/// The destination of a staging walk, one per object space.
pub trait ObjectSink {
    /// Records the new object `id` with its node encoding.
    ///
    /// The same `id` may be recorded more than once, always with the same
    /// encoding; a sink keeps it once. The walk does not depend on whether
    /// the sink knew `id` already.
    fn record(&mut self, id: ObjectId, encoding: &[u8]);
}

/// A simple sink: the staged objects of one space, in walk order, each once.
#[derive(Clone, Debug, Default)]
pub struct Staged {
    objects: Vec<(ObjectId, Vec<u8>)>,
    seen: HashSet<ObjectId>,
}

impl Staged {
    /// Creates an empty collection.
    pub fn new() -> Self {
        Self::default()
    }

    /// The staged objects in the order they were first recorded.
    pub fn objects(&self) -> &[(ObjectId, Vec<u8>)] {
        &self.objects
    }

    /// Consumes the collection, returning the objects.
    pub fn into_objects(self) -> Vec<(ObjectId, Vec<u8>)> {
        self.objects
    }

    /// The number of distinct objects staged.
    pub fn len(&self) -> usize {
        self.objects.len()
    }

    /// Whether nothing was staged.
    pub fn is_empty(&self) -> bool {
        self.objects.is_empty()
    }
}

impl ObjectSink for Staged {
    fn record(&mut self, id: ObjectId, encoding: &[u8]) {
        if self.seen.insert(id) {
            self.objects.push((id, encoding.to_vec()));
        }
    }
}
