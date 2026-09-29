//! Lazy iteration in encoded key order.

use std::{cmp::Ordering, fmt, marker::PhantomData, sync::Arc};

use crate::{
    Link, NodeBox, NodeLoader, TrieError, TrieKey, TrieValue,
    map::{Probe, Role, check_extension, check_leaf, resolve},
    nibbles,
    node::Node,
};

/// A lazy iterator over a [`MerkleMap`](crate::MerkleMap) in encoded key
/// order, from [`range_from`](crate::MerkleMap::range_from) or
/// [`iter`](crate::MerkleMap::iter).
///
/// It owns the nodes it walks (the map handle may be dropped) and borrows
/// only the loader. Nodes are visited, and stored ones loaded, only as
/// entries are requested, so a caller can stop early without paying for
/// the rest of the range. Each item is a `Result`; after an error the
/// iterator ends. [`probe`](Self::probe) reports the visits so far.
pub struct Range<'l, K, V, L: ?Sized> {
    loader: &'l L,
    /// The root and the lower bound's nibbles, until the first step.
    start: Option<(Link<V>, Option<Vec<u8>>)>,
    /// Branches whose later children are still to be visited.
    frames: Vec<Frame<V>>,
    /// The nibble path to the node being visited.
    path: Vec<u8>,
    /// The next node to visit and its depth in nibbles.
    pending: Option<(Pending<V>, usize)>,
    /// The key length in nibbles.
    key_nibbles: usize,
    probe: Probe,
    done: bool,
    key: PhantomData<fn() -> K>,
}

struct Frame<V> {
    branch: Arc<NodeBox<V>>,
    depth: usize,
    next_slot: usize,
}

enum Pending<V> {
    /// Not visited yet.
    Link(Link<V>, Role),
    /// Already visited (and counted) by the seek.
    Node(Arc<NodeBox<V>>),
}

impl<K, V, L: ?Sized> fmt::Debug for Range<'_, K, V, L> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Range")
            .field("probe", &self.probe)
            .field("done", &self.done)
            .finish_non_exhaustive()
    }
}

impl<'l, K: TrieKey, V: TrieValue, L: NodeLoader<V> + ?Sized> Range<'l, K, V, L> {
    pub(crate) fn new(root: Link<V>, lower: Option<Vec<u8>>, loader: &'l L) -> Self {
        Self {
            loader,
            start: Some((root, lower)),
            frames: Vec::new(),
            path: Vec::new(),
            pending: None,
            key_nibbles: 2 * K::LEN,
            probe: Probe::new(),
            done: false,
            key: PhantomData,
        }
    }

    /// The nodes visited and loads made so far, the seek included.
    pub const fn probe(&self) -> Probe {
        self.probe
    }

    /// Descends along the lower bound, leaving the first entry at or above
    /// it pending and every later sibling on the frame stack.
    fn seek(&mut self, root: Link<V>, lower: Option<Vec<u8>>) -> Result<(), L::Error> {
        let Some(lower) = lower else {
            self.pending = Some((Pending::Link(root, Role::Root), 0));
            return Ok(());
        };
        let mut link = root;
        let mut role = Role::Root;
        let mut depth = 0;
        loop {
            let Some(node) = resolve(&link, role, self.loader, &mut self.probe)? else {
                return Ok(());
            };
            let rest = &lower[depth..];
            match &node.node {
                Node::Leaf { path, .. } => {
                    check_leaf(path, rest)?;
                    if **path >= *rest {
                        self.pending = Some((Pending::Node(Arc::clone(&node)), depth));
                    }
                    return Ok(());
                }
                Node::Extension { path, child } => {
                    check_extension(path, rest)?;
                    match (**path).cmp(&rest[..path.len()]) {
                        // Every key below is above the bound.
                        Ordering::Greater => {
                            self.pending = Some((Pending::Node(Arc::clone(&node)), depth));
                            return Ok(());
                        }
                        // Every key below is under the bound.
                        Ordering::Less => return Ok(()),
                        Ordering::Equal => {
                            self.path.extend_from_slice(path);
                            depth += path.len();
                            link = child.clone();
                            role = Role::ExtensionChild;
                        }
                    }
                }
                Node::Branch { children } => {
                    let nibble = *rest
                        .first()
                        .ok_or(TrieError::inconsistent("branch below the key length"))?;
                    let slot = usize::from(nibble);
                    self.frames.push(Frame {
                        branch: Arc::clone(&node),
                        depth,
                        next_slot: slot + 1,
                    });
                    self.path.push(nibble);
                    depth += 1;
                    link = children[slot].clone();
                    role = Role::Child;
                }
            }
        }
    }

    fn step(&mut self) -> Result<Option<(K, V)>, L::Error> {
        if let Some((root, lower)) = self.start.take() {
            self.seek(root, lower)?;
        }
        loop {
            if let Some((pending, depth)) = self.pending.take() {
                let node = match pending {
                    Pending::Node(node) => node,
                    Pending::Link(link, role) => {
                        match resolve(&link, role, self.loader, &mut self.probe)? {
                            Some(node) => node,
                            None => continue,
                        }
                    }
                };
                self.path.truncate(depth);
                match &node.node {
                    Node::Leaf { path, value } => {
                        self.path.extend_from_slice(path);
                        if self.path.len() != self.key_nibbles {
                            return Err(TrieError::inconsistent(
                                "leaf at the wrong depth for the key length",
                            )
                            .into());
                        }
                        let key = K::decode(&nibbles::pack(&self.path))
                            .map_err(|error| TrieError::Key { error })?;
                        return Ok(Some((key, value.clone())));
                    }
                    Node::Extension { path, child } => {
                        if depth + path.len() >= self.key_nibbles {
                            return Err(TrieError::inconsistent(
                                "extension reaches the end of the key",
                            )
                            .into());
                        }
                        self.path.extend_from_slice(path);
                        let child = Pending::Link(child.clone(), Role::ExtensionChild);
                        self.pending = Some((child, depth + path.len()));
                    }
                    Node::Branch { .. } => {
                        if depth >= self.key_nibbles {
                            return Err(
                                TrieError::inconsistent("branch below the key length").into()
                            );
                        }
                        self.frames.push(Frame {
                            branch: node,
                            depth,
                            next_slot: 0,
                        });
                    }
                }
                continue;
            }
            let Some(frame) = self.frames.last_mut() else {
                return Ok(None);
            };
            let Node::Branch { children } = &frame.branch.node else {
                return Err(TrieError::inconsistent("iterator frame is not a branch").into());
            };
            match (frame.next_slot..16).find(|slot| !children[*slot].is_empty()) {
                Some(slot) => {
                    frame.next_slot = slot + 1;
                    let depth = frame.depth;
                    let child = Pending::Link(children[slot].clone(), Role::Child);
                    self.path.truncate(depth);
                    self.path.push(slot as u8);
                    self.pending = Some((child, depth + 1));
                }
                None => {
                    self.frames.pop();
                }
            }
        }
    }
}

impl<K, V, L> Iterator for Range<'_, K, V, L>
where
    K: TrieKey,
    V: TrieValue,
    L: NodeLoader<V> + ?Sized,
{
    type Item = Result<(K, V), L::Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        match self.step() {
            Ok(Some(entry)) => Some(Ok(entry)),
            Ok(None) => {
                self.done = true;
                None
            }
            Err(error) => {
                self.done = true;
                self.frames.clear();
                self.pending = None;
                Some(Err(error))
            }
        }
    }
}
