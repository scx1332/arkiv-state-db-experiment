//! The persistent Merkle Patricia map.

use std::{fmt, marker::PhantomData, sync::Arc};

use alloy_primitives::keccak256;
use alloy_trie::EMPTY_ROOT_HASH;

use crate::{
    KeyRange, Link, NodeBox, NodeKind, NodeLoader, ObjectId, TrieError, TrieKey, TrieValue,
    nibbles::{self, common_prefix, join},
    node::Node,
    proof::ProofIter,
    range::Range,
    stage::{
        NestedStageVisitor, ObjectSink, StageBudget, StageError, TryObjectSink, preflight_link,
        try_stage_link,
    },
};

/// Node visits of one or more map operations.
///
/// `nodes` follows the engine's visit semantics: every node an operation
/// examines counts once per operation, whether it was already in memory or
/// had to be loaded, and inline children count like any other node. A
/// lookup counts the nodes on the key's path down to the terminal node; an
/// insert counts the same path; a removal counts the path plus the sibling
/// it examines when a branch collapses (at most one per removal); an
/// iterator counts its seek path and every node it steps into. Nodes an
/// update creates are not visits. `nodes` therefore depends only on the
/// map's contents and the operation, never on cache warmth or on which
/// nodes a handle holds in memory.
///
/// `loads` counts the stored links an operation followed through its
/// loader (cache hits in the loader included). It depends on what the
/// handle already holds in memory and is physical instrumentation only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Probe {
    /// Nodes examined.
    pub nodes: u64,
    /// Stored links followed through the loader.
    pub loads: u64,
}

impl Probe {
    /// Creates a zeroed probe.
    pub const fn new() -> Self {
        Self { nodes: 0, loads: 0 }
    }
}

/// How the node behind a link is reached; decides the shape checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Role {
    /// A map root: stored by hash whatever its size.
    Root,
    /// A branch child: any node type.
    Child,
    /// An extension's child: always a branch.
    ExtensionChild,
}

/// Returns the node behind `link`, counting the visit; `None` if empty.
///
/// A stored node is loaded (and verified) through `loader`. A stored node
/// that is not a root must be at least 32 bytes, since a shorter one would
/// have been inlined into its parent, and an extension's child must be a
/// branch; either violation is [`TrieError::Inconsistent`].
pub(crate) fn resolve<V, L>(
    link: &Link<V>,
    role: Role,
    loader: &L,
    probe: &mut Probe,
) -> Result<Option<Arc<NodeBox<V>>>, L::Error>
where
    V: TrieValue,
    L: NodeLoader<V> + ?Sized,
{
    let node = match link {
        Link::Empty => return Ok(None),
        Link::Mem(node) => Arc::clone(node),
        Link::Stored(id) => {
            probe.loads += 1;
            let node = loader.load(id)?;
            if role != Role::Root && !node.reference().is_hash() {
                return Err(
                    TrieError::inconsistent("hash-referenced node shorter than 32 bytes").into(),
                );
            }
            node
        }
    };
    probe.nodes += 1;
    if role == Role::ExtensionChild && node.kind() != NodeKind::Branch {
        return Err(TrieError::inconsistent("extension child is not a branch").into());
    }
    Ok(Some(node))
}

/// Checks that a leaf whose key ends `end` nibbles below the root holds a
/// whole-byte key of at most `max` nibbles (twice the key type's
/// [`TrieKey::MAX_LEN`]). The bound comes from the key type, never from a
/// requested key, so an unrelated leaf is not judged by a query's length.
pub(crate) fn check_leaf_end(end: usize, max: usize) -> Result<(), TrieError> {
    if !end.is_multiple_of(2) {
        Err(TrieError::inconsistent("leaf key ends inside a byte"))
    } else if end > max {
        Err(TrieError::inconsistent(
            "leaf key exceeds the key length bound",
        ))
    } else {
        Ok(())
    }
}

/// Checks that an inner node (a branch at `end`, or an extension ending at
/// `end`) leaves room for at least one more nibble below the key bound.
pub(crate) fn check_inner_end(end: usize, max: usize) -> Result<(), TrieError> {
    if end < max {
        Ok(())
    } else {
        Err(TrieError::inconsistent(
            "inner node reaches the key length bound",
        ))
    }
}

/// The nibble bound of a key type: `2 · K::MAX_LEN`.
pub(crate) const fn max_nibbles<K: TrieKey>() -> usize {
    2 * K::MAX_LEN
}

/// Returns the key's encoding as nibbles.
///
/// # Panics
///
/// Panics if the key type's encoding is longer than `K::MAX_LEN` bytes.
pub(crate) fn key_nibbles<K: TrieKey>(key: &K) -> Vec<u8> {
    let bytes = key.encode();
    assert!(
        bytes.len() <= K::MAX_LEN,
        "TrieKey::encode returned more than MAX_LEN bytes"
    );
    nibbles::unpack(&bytes)
}

/// A persistent hexary Merkle Patricia map with bounded, prefix-free,
/// variable-length keys.
///
/// The node encoding is the Ethereum MPT encoding of `alloy_trie::nodes`,
/// hashed with keccak256, and the shape is canonical: branches have at least
/// two children and no value, extensions only precede branches, and every
/// update restores that form (a removal collapses a branch left with one
/// child and merges extension paths, loading the remaining sibling if it is
/// stored). The root hash therefore depends only on the key/value set, and
/// equals the root `alloy_trie::HashBuilder` computes over the same sorted
/// leaves. Since branches hold no values, the key set must be prefix-free
/// (see [`TrieKey`]); keys may otherwise have any length up to
/// `K::MAX_LEN`, and a key path visits at most `2·K::MAX_LEN + 1` nodes.
///
/// A handle is a root [`Link`] and the entry count. Cloning is O(1); an
/// update returns a new handle and shares every node off the updated path
/// with the old one, which stays valid. Handles hold no loader: operations
/// that may follow a stored link take a [`NodeLoader`] by reference and are
/// fallible, returning the loader's error type; a failed read is never
/// reported as an absent key. Iteration is in encoded key order.
pub struct MerkleMap<K, V> {
    pub(crate) root: Link<V>,
    len: u64,
    key: PhantomData<fn() -> K>,
}

impl<K, V> Clone for MerkleMap<K, V> {
    fn clone(&self) -> Self {
        Self {
            root: self.root.clone(),
            len: self.len,
            key: PhantomData,
        }
    }
}

impl<K, V> fmt::Debug for MerkleMap<K, V> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MerkleMap")
            .field("root", &self.root)
            .field("len", &self.len)
            .finish()
    }
}

impl<K: TrieKey, V: TrieValue> Default for MerkleMap<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: TrieKey, V: TrieValue> MerkleMap<K, V> {
    /// Creates an empty map.
    pub const fn new() -> Self {
        const { assert!(K::MAX_LEN > 0, "TrieKey::MAX_LEN must be at least one byte") };
        Self {
            root: Link::Empty,
            len: 0,
            key: PhantomData,
        }
    }

    /// Opens a committed map from its root hash and entry count.
    ///
    /// `(EMPTY_ROOT_HASH, 0)` is the empty map; any other pair involving the
    /// empty root or a zero length is [`TrieError::InvalidHandle`]. Nothing
    /// is read: the root is a [`Link::Stored`] loaded on first use.
    pub fn from_root(root: ObjectId, len: u64) -> Result<Self, TrieError> {
        const { assert!(K::MAX_LEN > 0, "TrieKey::MAX_LEN must be at least one byte") };
        match (root == EMPTY_ROOT_HASH, len == 0) {
            (true, true) => Ok(Self::new()),
            (false, false) => Ok(Self {
                root: Link::Stored(root),
                len,
                key: PhantomData,
            }),
            _ => Err(TrieError::InvalidHandle { root, len }),
        }
    }

    /// The number of entries, tracked in the handle.
    pub const fn len(&self) -> u64 {
        self.len
    }

    /// Whether the map has no entries.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The root link, for inspection.
    pub const fn root_link(&self) -> &Link<V> {
        &self.root
    }

    /// The root hash: keccak256 of the root node's encoding, or
    /// `EMPTY_ROOT_HASH` for the empty map.
    ///
    /// Only nodes built in memory since the last computation are encoded
    /// and hashed; the result is cached in them. It never reads the store,
    /// because a stored link's hash is its id, so it cannot fail.
    pub fn root_hash(&self) -> ObjectId {
        match &self.root {
            Link::Empty => EMPTY_ROOT_HASH,
            Link::Stored(id) => *id,
            Link::Mem(node) => {
                let reference = node.reference();
                reference
                    .as_hash()
                    .unwrap_or_else(|| keccak256(reference.as_slice()))
            }
        }
    }

    /// Whether both handles have the same root hash, hence the same contents.
    ///
    /// A shared root or equal stored ids answer without hashing.
    pub fn same_root(&self, other: &Self) -> bool {
        match (&self.root, &other.root) {
            (Link::Empty, Link::Empty) => true,
            (Link::Mem(left), Link::Mem(right)) if Arc::ptr_eq(left, right) => true,
            (Link::Stored(left), Link::Stored(right)) if left == right => true,
            _ => self.root_hash() == other.root_hash(),
        }
    }

    /// A handle to the same contents whose root is a [`Link::Stored`] link.
    ///
    /// Use it once every object [`stage`](Self::stage) emitted is committed:
    /// the new handle drops this handle's in-memory nodes and loads lazily.
    pub fn to_stored(&self) -> Self {
        Self {
            root: match &self.root {
                Link::Empty => Link::Empty,
                _ => Link::Stored(self.root_hash()),
            },
            len: self.len,
            key: PhantomData,
        }
    }

    /// Returns the value stored under `key`.
    pub fn get<L>(&self, key: &K, loader: &L, probe: &mut Probe) -> Result<Option<V>, L::Error>
    where
        L: NodeLoader<V> + ?Sized,
    {
        let key = key_nibbles(key);
        let max = max_nibbles::<K>();
        let mut link = self.root.clone();
        let mut role = Role::Root;
        let mut position = 0;
        loop {
            let Some(node) = resolve(&link, role, loader, probe)? else {
                return Ok(None);
            };
            let rest = &key[position..];
            match &node.node {
                Node::Leaf { path, value } => {
                    check_leaf_end(position + path.len(), max)?;
                    return Ok((**path == *rest).then(|| value.clone()));
                }
                Node::Extension { path, child } => {
                    check_inner_end(position + path.len(), max)?;
                    if !rest.starts_with(path) {
                        return Ok(None);
                    }
                    position += path.len();
                    link = child.clone();
                    role = Role::ExtensionChild;
                }
                Node::Branch { children } => {
                    check_inner_end(position, max)?;
                    // A key ending at a branch is a strict prefix of stored
                    // keys; branches hold no values, so it is absent.
                    let Some(&nibble) = rest.first() else {
                        return Ok(None);
                    };
                    link = children[usize::from(nibble)].clone();
                    position += 1;
                    role = Role::Child;
                }
            }
        }
    }

    /// Returns a new version with `key` bound to `value`, plus the value it
    /// replaced. This handle is unchanged.
    ///
    /// A value whose encoding is empty is rejected with
    /// [`TrieError::EmptyValue`] before anything is visited or read; the
    /// check is [`TrieValue::encodes_empty`], which by default encodes the
    /// value once.
    pub fn insert<L>(
        &self,
        key: &K,
        value: V,
        loader: &L,
        probe: &mut Probe,
    ) -> Result<(Self, Option<V>), L::Error>
    where
        L: NodeLoader<V> + ?Sized,
    {
        if value.encodes_empty() {
            return Err(TrieError::EmptyValue.into());
        }
        let key = key_nibbles(key);
        let at = At {
            depth: 0,
            max: max_nibbles::<K>(),
        };
        let (root, previous) = insert_at(&self.root, Role::Root, at, &key, value, loader, probe)?;
        let len = if previous.is_some() {
            self.len
        } else {
            self.len
                .checked_add(1)
                .ok_or(TrieError::inconsistent("entry count overflow"))?
        };
        Ok((
            Self {
                root: Link::Mem(root),
                len,
                key: PhantomData,
            },
            previous,
        ))
    }

    /// Returns a new version without `key`, plus the value it held.
    ///
    /// Removing an absent key returns a handle sharing this root. A branch
    /// left with a single child collapses into its parent's path; if that
    /// child is stored it is loaded to learn its type.
    pub fn remove<L>(
        &self,
        key: &K,
        loader: &L,
        probe: &mut Probe,
    ) -> Result<(Self, Option<V>), L::Error>
    where
        L: NodeLoader<V> + ?Sized,
    {
        let key = key_nibbles(key);
        let at = At {
            depth: 0,
            max: max_nibbles::<K>(),
        };
        match remove_at(&self.root, Role::Root, at, &key, loader, probe)? {
            Removal::Absent => Ok((self.clone(), None)),
            Removal::Removed { replacement, value } => {
                let len = self
                    .len
                    .checked_sub(1)
                    .ok_or(TrieError::inconsistent("entry count below the entries"))?;
                let root = replacement.map_or(Link::Empty, Link::Mem);
                if root.is_empty() != (len == 0) {
                    return Err(
                        TrieError::inconsistent("entry count disagrees with the trie").into(),
                    );
                }
                Ok((
                    Self {
                        root,
                        len,
                        key: PhantomData,
                    },
                    Some(value),
                ))
            }
        }
    }

    /// Iterates lazily, in encoded key order, over entries with keys at
    /// least `lower`.
    ///
    /// Nothing is visited until the first call to `next`. Each item is a
    /// `Result`; after an error the iterator ends.
    pub fn range_from<'l, L>(&self, lower: &K, loader: &'l L) -> Range<'l, K, V, L>
    where
        L: NodeLoader<V> + ?Sized,
    {
        Range::new(self.root.clone(), Some(key_nibbles(lower)), loader)
    }

    /// Iterates lazily, in encoded key order, over entries whose encoded
    /// keys are at least the raw byte string `lower`, which need not be the
    /// encoding of a key (any seek position works).
    pub fn range_from_encoded<'l, L>(&self, lower: &[u8], loader: &'l L) -> Range<'l, K, V, L>
    where
        L: NodeLoader<V> + ?Sized,
    {
        Range::new(self.root.clone(), Some(nibbles::unpack(lower)), loader)
    }

    /// Iterates lazily over all entries in encoded key order.
    pub fn iter<'l, L>(&self, loader: &'l L) -> Range<'l, K, V, L>
    where
        L: NodeLoader<V> + ?Sized,
    {
        Range::new(self.root.clone(), None, loader)
    }

    /// Emits, lazily, the proof nodes for `key`: the root node, then every
    /// hash-referenced node on the key's path, in path order.
    ///
    /// Inline nodes are part of their parent's bytes and are never emitted
    /// on their own. The proof is the same for a present and an absent key:
    /// it ends at the leaf, at the leaf or extension that diverges from the
    /// key, or at the branch whose slot for the key is empty. The empty map
    /// has an empty proof. Verify with [`walk`](crate::walk).
    pub fn proof<'l, L>(&self, key: &K, loader: &'l L) -> ProofIter<'l, V, L>
    where
        L: NodeLoader<V> + ?Sized,
    {
        ProofIter::new(
            self.root.clone(),
            key_nibbles(key),
            max_nibbles::<K>(),
            loader,
        )
    }

    /// Emits a complete interval witness, opening only the subtrees whose
    /// key prefix can intersect `range`. Bounds are raw encoded byte strings
    /// (inclusive, exclusive or unbounded) and need not be encodings of
    /// keys. Verify with [`walk_interval`](crate::walk_interval), passing
    /// `K::MAX_LEN` as the key bound.
    pub fn interval_proof<'l, L>(
        &self,
        range: &KeyRange,
        loader: &'l L,
    ) -> crate::IntervalProofIter<'l, V, L>
    where
        L: NodeLoader<V> + ?Sized,
    {
        crate::IntervalProofIter::new(self.root.clone(), range, K::MAX_LEN, loader)
    }

    /// Collects [`proof`](Self::proof) into a vector, adding its visits to `probe`.
    pub fn prove<L>(&self, key: &K, loader: &L, probe: &mut Probe) -> Result<Vec<Vec<u8>>, L::Error>
    where
        L: NodeLoader<V> + ?Sized,
    {
        let mut proof = self.proof(key, loader);
        let nodes = proof.by_ref().collect::<Result<Vec<_>, _>>();
        let visits = proof.probe();
        probe.nodes += visits.nodes;
        probe.loads += visits.loads;
        nodes
    }

    /// Stages this map's new nodes into `sink`, and, through
    /// [`TrieValue::stage_nested`], the new nodes of maps nested in the
    /// values of new leaves.
    ///
    /// Emits the root (whatever its size) and every other node of 32 bytes
    /// or more that an update built in memory; stops at stored links and at
    /// nodes decoded from the store (`from_store`); never emits an inline
    /// node. Every new node is walked, and every leaf value of one is
    /// visited, even when the sink already holds the node's id (the sink
    /// deduplicates what it keeps). Hashes whatever is not hashed yet;
    /// reads nothing.
    pub fn stage(&self, sink: &mut dyn ObjectSink) {
        let mut visit = |value: &V, sink: &mut dyn ObjectSink| value.stage_nested(sink);
        stage_link(&self.root, true, sink, &mut visit, &mut Vec::new());
    }

    /// Stages like [`stage`](Self::stage), but hands every leaf value of a
    /// new node to `nested` instead of [`TrieValue::stage_nested`].
    ///
    /// For values whose nested maps belong to another object space: the
    /// caller stages them into that space's sink.
    pub fn stage_with(&self, sink: &mut dyn ObjectSink, nested: &mut dyn FnMut(&V)) {
        let mut visit = |value: &V, _: &mut dyn ObjectSink| nested(value);
        stage_link(&self.root, true, sink, &mut visit, &mut Vec::new());
    }

    /// Checks new nodes' full encoded-size bounds without encoding, hashing or
    /// reading storage. Even a new node with an already cached hash is checked.
    ///
    /// `value_size` must return a checked upper bound on `TrieValue::encode`'s
    /// output length without calling that encoder. It must also synchronously
    /// preflight every nested map using this same `budget`. This walk adds
    /// compact-path, RLP-list/string and child-reference framing. Bounds can
    /// conservatively reject a fitting node (notably one-byte values); accepted
    /// nodes cannot exceed the ceiling when the callback's bounds are correct.
    ///
    /// Preflight all maps before calling any root/hash/staging method: encoding
    /// a parent leaf can indirectly hash its nested maps. A fresh budget with
    /// the same limit is then used for [`try_stage_with`](Self::try_stage_with).
    pub fn preflight_with<E: From<StageError>>(
        &self,
        max_object_bytes: usize,
        budget: &mut StageBudget,
        value_size: &mut dyn FnMut(&V, &mut StageBudget) -> Result<usize, E>,
    ) -> Result<(), E> {
        preflight_link(&self.root, max_object_bytes, budget, value_size).map(|_| ())
    }

    /// Stages preflighted new nodes with immediate error propagation.
    ///
    /// Run [`preflight_with`](Self::preflight_with) for the complete reachable
    /// graph first: computing a parent reference hashes its new descendants
    /// before this walk visits them. The sink bounds retained objects/bytes;
    /// `budget` bounds visits, shared with synchronous nested-map callbacks.
    /// Neither the sink nor callback is called after an error. Duplicate object
    /// ids do not suppress nested visits. This does not use the infallible
    /// [`TrieValue::stage_nested`] hook; the callback routes every nested map.
    pub fn try_stage_with<S: TryObjectSink + ?Sized>(
        &self,
        sink: &mut S,
        budget: &mut StageBudget,
        nested: &mut NestedStageVisitor<'_, V, S>,
    ) -> Result<(), S::Error> {
        try_stage_link(&self.root, true, sink, budget, nested, &mut Vec::new())
    }
}

/// The result of a removal below some link.
enum Removal<V> {
    Absent,
    Removed {
        /// The normalized replacement node; `None` if nothing is left.
        replacement: Option<Arc<NodeBox<V>>>,
        value: V,
    },
}

fn leaf<V>(path: &[u8], value: V) -> Arc<NodeBox<V>> {
    NodeBox::new(Node::Leaf {
        path: path.into(),
        value,
    })
}

/// A branch holding exactly two children in distinct slots.
fn branch_of_two<V>(first: (u8, Link<V>), second: (u8, Link<V>)) -> Arc<NodeBox<V>> {
    let mut children: [Link<V>; 16] = std::array::from_fn(|_| Link::Empty);
    children[usize::from(first.0)] = first.1;
    children[usize::from(second.0)] = second.1;
    NodeBox::new(Node::Branch {
        children: Box::new(children),
    })
}

/// `branch`, preceded by an extension over `prefix` when that is nonempty.
fn above<V>(prefix: &[u8], branch: Arc<NodeBox<V>>) -> Arc<NodeBox<V>> {
    if prefix.is_empty() {
        branch
    } else {
        NodeBox::new(Node::Extension {
            path: prefix.into(),
            child: Link::Mem(branch),
        })
    }
}

/// The node replacing the one an insert descended into, and the value it
/// replaced.
type Inserted<V> = (Arc<NodeBox<V>>, Option<V>);

/// Where an update stands: the depth of the node in nibbles and the key
/// type's nibble bound.
#[derive(Clone, Copy)]
struct At {
    depth: usize,
    max: usize,
}

impl At {
    const fn below(self, nibbles: usize) -> Self {
        Self {
            depth: self.depth + nibbles,
            max: self.max,
        }
    }
}

fn insert_at<V, L>(
    link: &Link<V>,
    role: Role,
    at: At,
    rest: &[u8],
    value: V,
    loader: &L,
    probe: &mut Probe,
) -> Result<Inserted<V>, L::Error>
where
    V: TrieValue,
    L: NodeLoader<V> + ?Sized,
{
    let Some(node) = resolve(link, role, loader, probe)? else {
        return Ok((leaf(rest, value), None));
    };
    match &node.node {
        Node::Leaf {
            path,
            value: existing,
        } => {
            check_leaf_end(at.depth + path.len(), at.max)?;
            if **path == *rest {
                return Ok((leaf(rest, value), Some(existing.clone())));
            }
            let common = common_prefix(path, rest);
            if common == path.len() || common == rest.len() {
                // One key is a strict prefix of the other.
                return Err(TrieError::KeyNotPrefixFree.into());
            }
            let branch = branch_of_two(
                (
                    path[common],
                    Link::Mem(leaf(&path[common + 1..], existing.clone())),
                ),
                (rest[common], Link::Mem(leaf(&rest[common + 1..], value))),
            );
            Ok((above(&rest[..common], branch), None))
        }
        Node::Extension { path, child } => {
            check_inner_end(at.depth + path.len(), at.max)?;
            let common = common_prefix(path, rest);
            if common == path.len() {
                let (child, previous) = insert_at(
                    child,
                    Role::ExtensionChild,
                    at.below(common),
                    &rest[common..],
                    value,
                    loader,
                    probe,
                )?;
                let extension = NodeBox::new(Node::Extension {
                    path: path.clone(),
                    child: Link::Mem(child),
                });
                return Ok((extension, previous));
            }
            if common == rest.len() {
                // The key ends inside the extension: a prefix of the keys below.
                return Err(TrieError::KeyNotPrefixFree.into());
            }
            // The key leaves the extension at `common`: a new branch there
            // holds the rest of the extension (or its branch directly) and
            // the new leaf. The old child is kept as it was, unloaded.
            let old = if common + 1 == path.len() {
                child.clone()
            } else {
                Link::Mem(NodeBox::new(Node::Extension {
                    path: path[common + 1..].into(),
                    child: child.clone(),
                }))
            };
            let branch = branch_of_two(
                (path[common], old),
                (rest[common], Link::Mem(leaf(&rest[common + 1..], value))),
            );
            Ok((above(&rest[..common], branch), None))
        }
        Node::Branch { children } => {
            check_inner_end(at.depth, at.max)?;
            // A key ending at a branch is a prefix of the keys below it.
            let (&nibble, tail) = rest.split_first().ok_or(TrieError::KeyNotPrefixFree)?;
            let slot = usize::from(nibble);
            let (child, previous) = insert_at(
                &children[slot],
                Role::Child,
                at.below(1),
                tail,
                value,
                loader,
                probe,
            )?;
            let mut children = children.clone();
            children[slot] = Link::Mem(child);
            Ok((NodeBox::new(Node::Branch { children }), previous))
        }
    }
}

fn remove_at<V, L>(
    link: &Link<V>,
    role: Role,
    at: At,
    rest: &[u8],
    loader: &L,
    probe: &mut Probe,
) -> Result<Removal<V>, L::Error>
where
    V: TrieValue,
    L: NodeLoader<V> + ?Sized,
{
    let Some(node) = resolve(link, role, loader, probe)? else {
        return Ok(Removal::Absent);
    };
    match &node.node {
        Node::Leaf { path, value } => {
            check_leaf_end(at.depth + path.len(), at.max)?;
            Ok(if **path == *rest {
                Removal::Removed {
                    replacement: None,
                    value: value.clone(),
                }
            } else {
                Removal::Absent
            })
        }
        Node::Extension { path, child } => {
            check_inner_end(at.depth + path.len(), at.max)?;
            if !rest.starts_with(path) {
                return Ok(Removal::Absent);
            }
            match remove_at(
                child,
                Role::ExtensionChild,
                at.below(path.len()),
                &rest[path.len()..],
                loader,
                probe,
            )? {
                Removal::Absent => Ok(Removal::Absent),
                Removal::Removed {
                    replacement: Some(child),
                    value,
                } => Ok(Removal::Removed {
                    replacement: Some(prefixed(path, &Link::Mem(Arc::clone(&child)), &child)),
                    value,
                }),
                Removal::Removed {
                    replacement: None, ..
                } => Err(TrieError::inconsistent("extension over an emptied branch").into()),
            }
        }
        Node::Branch { children } => {
            check_inner_end(at.depth, at.max)?;
            // A key ending at a branch is not stored: branches hold no values.
            let Some((&nibble, tail)) = rest.split_first() else {
                return Ok(Removal::Absent);
            };
            let slot = usize::from(nibble);
            match remove_at(
                &children[slot],
                Role::Child,
                at.below(1),
                tail,
                loader,
                probe,
            )? {
                Removal::Absent => Ok(Removal::Absent),
                Removal::Removed { replacement, value } => {
                    let mut children = children.clone();
                    children[slot] = replacement.map_or(Link::Empty, Link::Mem);
                    Ok(Removal::Removed {
                        replacement: Some(normalize_branch(children, loader, probe)?),
                        value,
                    })
                }
            }
        }
    }
}

/// Rebuilds a branch after a removal below it, collapsing it into its only
/// remaining child when one is left.
fn normalize_branch<V, L>(
    children: Box<[Link<V>; 16]>,
    loader: &L,
    probe: &mut Probe,
) -> Result<Arc<NodeBox<V>>, L::Error>
where
    V: TrieValue,
    L: NodeLoader<V> + ?Sized,
{
    let mut occupied = (0u8..)
        .zip(children.iter())
        .filter(|(_, child)| !child.is_empty());
    let (Some((nibble, only)), None) = (occupied.next(), occupied.next()) else {
        return Ok(NodeBox::new(Node::Branch { children }));
    };
    // A single child is left: load it (a counted visit) to merge its path.
    let sibling = resolve(only, Role::Child, loader, probe)?
        .ok_or(TrieError::inconsistent("empty link counted as a child"))?;
    Ok(prefixed(&[nibble], only, &sibling))
}

/// Puts `prefix` in front of `node`, reached through `link`: a leaf or an
/// extension absorbs the prefix into its path; a branch gets an extension
/// over `prefix` that keeps `link` as it was (stored or in memory).
fn prefixed<V: Clone>(prefix: &[u8], link: &Link<V>, node: &NodeBox<V>) -> Arc<NodeBox<V>> {
    match &node.node {
        Node::Leaf { path, value } => NodeBox::new(Node::Leaf {
            path: join(prefix, path),
            value: value.clone(),
        }),
        Node::Extension { path, child } => NodeBox::new(Node::Extension {
            path: join(prefix, path),
            child: child.clone(),
        }),
        Node::Branch { .. } => NodeBox::new(Node::Extension {
            path: prefix.into(),
            child: link.clone(),
        }),
    }
}

/// The staging walk over one map; see [`MerkleMap::stage`].
fn stage_link<V: TrieValue>(
    link: &Link<V>,
    root: bool,
    sink: &mut dyn ObjectSink,
    visit: &mut dyn FnMut(&V, &mut dyn ObjectSink),
    buffer: &mut Vec<u8>,
) {
    let Link::Mem(node) = link else {
        return;
    };
    if node.from_store() {
        return;
    }
    let reference = node.reference();
    if root || reference.is_hash() {
        buffer.clear();
        node.encode(buffer);
        let id = reference
            .as_hash()
            .unwrap_or_else(|| keccak256(buffer.as_slice()));
        // The walk continues even when the sink already holds `id`: maps
        // with different value types can share a sink, and a leaf of one
        // can be byte-identical to a leaf of another whose value names a
        // nested map, so an equal id does not mean the nested maps below
        // were staged.
        sink.record(id, buffer);
    }
    match &node.node {
        Node::Leaf { value, .. } => visit(value, sink),
        Node::Extension { child, .. } => stage_link(child, false, sink, visit, buffer),
        Node::Branch { children } => {
            for child in children.iter() {
                stage_link(child, false, sink, visit, buffer);
            }
        }
    }
}

#[cfg(test)]
mod staging_ownership_tests {
    use std::{cell::RefCell, sync::Weak};

    use super::*;
    use crate::{MemoryNodeStore, MemoryOnly, Staged};

    fn track(link: &Link<Vec<u8>>, weak: &mut Vec<Weak<NodeBox<Vec<u8>>>>) {
        let Link::Mem(node) = link else {
            return;
        };
        weak.push(Arc::downgrade(node));
        match &node.node {
            Node::Leaf { .. } => (),
            Node::Extension { child, .. } => track(child, weak),
            Node::Branch { children } => {
                for child in children.iter() {
                    track(child, weak);
                }
            }
        }
    }

    struct TrackingLoader {
        store: MemoryNodeStore,
        weak: RefCell<Vec<Weak<NodeBox<Vec<u8>>>>>,
    }

    impl NodeLoader<Vec<u8>> for TrackingLoader {
        type Error = TrieError;

        fn read(&self, id: &ObjectId) -> Result<Option<Vec<u8>>, TrieError> {
            <MemoryNodeStore as NodeLoader<Vec<u8>>>::read(&self.store, id)
        }

        fn load(&self, id: &ObjectId) -> Result<Arc<NodeBox<Vec<u8>>>, TrieError> {
            let node = <MemoryNodeStore as NodeLoader<Vec<u8>>>::load(&self.store, id)?;
            track(&Link::Mem(Arc::clone(&node)), &mut self.weak.borrow_mut());
            Ok(node)
        }
    }

    #[test]
    fn stored_handles_release_dirty_nodes_and_do_not_retain_read_nodes() {
        let mut map: MerkleMap<[u8; 2], Vec<u8>> = MerkleMap::new();
        for i in 0_u16..512 {
            map = map
                .insert(
                    &i.to_be_bytes(),
                    vec![1; 1 + usize::from(i % 64)],
                    &MemoryOnly,
                    &mut Probe::new(),
                )
                .unwrap()
                .0;
        }
        let mut weak = Vec::new();
        track(&map.root, &mut weak);
        assert!(weak.len() > 512);
        let caller_clone = map.clone();
        let mut staged = Staged::new();
        map.stage(&mut staged);
        let mut store = MemoryNodeStore::new();
        store.commit(&staged);
        let stored = map.to_stored();
        assert!(matches!(stored.root, Link::Stored(_)));
        drop(map);
        assert!(weak.iter().all(|node| node.strong_count() > 0));
        drop(caller_clone);
        assert!(weak.iter().all(|node| node.upgrade().is_none()));

        let loader = TrackingLoader {
            store,
            weak: RefCell::new(Vec::new()),
        };
        let retained: Vec<_> = (0..128).map(|_| stored.clone()).collect();
        for (i, handle) in retained.iter().enumerate() {
            let key = u16::try_from(i).unwrap().to_be_bytes();
            assert!(
                handle
                    .get(&key, &loader, &mut Probe::new())
                    .unwrap()
                    .is_some()
            );
            assert!(
                !handle
                    .prove(&key, &loader, &mut Probe::new())
                    .unwrap()
                    .is_empty()
            );
            assert!(matches!(handle.root, Link::Stored(_)));
            assert!(
                loader
                    .weak
                    .borrow()
                    .iter()
                    .all(|node| node.upgrade().is_none())
            );
        }
        assert!(!loader.weak.borrow().is_empty());
    }
}
