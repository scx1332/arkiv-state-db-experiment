//! An immutable ordered map with structural sharing.
//!
//! [`PersistentMap`] is a persistent AVL tree whose nodes are shared through
//! [`Arc`]. Cloning a map is O(1). An insertion or removal returns a new map
//! and copies only the O(log n) nodes on the search path; every other node is
//! shared with the previous version, which stays unchanged. This is the
//! snapshot primitive for the block engine: a published state keeps its map
//! forever, while the next candidate builds a new version on top of it.
//!
//! Every operation reports the number of tree nodes it touched through a
//! [`Probe`]. That count is physical, informational instrumentation for
//! isolation measurements; it is not a canonical cost and may change if the
//! data structure changes.

use std::{borrow::Borrow, cmp::Ordering, sync::Arc};

/// Physical node-visit counter for one or more map operations.
///
/// The counter is a measurement of the in-memory tree layout. Canonical cost
/// accounting must not read it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Probe {
    /// Tree nodes examined, copied, or traversed so far.
    pub nodes: u64,
}

impl Probe {
    /// Creates a zeroed probe.
    pub const fn new() -> Self {
        Self { nodes: 0 }
    }

    fn touch(&mut self) {
        self.nodes += 1;
    }
}

struct Node<K, V> {
    key: K,
    value: V,
    left: Link<K, V>,
    right: Link<K, V>,
    height: u32,
    size: usize,
}

type Link<K, V> = Option<Arc<Node<K, V>>>;

fn height<K, V>(link: &Link<K, V>) -> u32 {
    link.as_ref().map_or(0, |node| node.height)
}

fn size<K, V>(link: &Link<K, V>) -> usize {
    link.as_ref().map_or(0, |node| node.size)
}

fn make<K, V>(key: K, value: V, left: Link<K, V>, right: Link<K, V>) -> Arc<Node<K, V>> {
    let height = 1 + height(&left).max(height(&right));
    let size = 1 + size(&left) + size(&right);
    Arc::new(Node {
        key,
        value,
        left,
        right,
        height,
        size,
    })
}

/// Rebuilds a node from `key`, `value` and already balanced subtrees whose
/// heights differ by at most two, restoring the AVL invariant with at most
/// two rotations.
fn balance<K: Clone, V: Clone>(
    key: K,
    value: V,
    left: Link<K, V>,
    right: Link<K, V>,
) -> Arc<Node<K, V>> {
    let left_height = height(&left);
    let right_height = height(&right);
    if left_height > right_height + 1 {
        let heavy = left.expect("a taller left subtree is nonempty");
        if height(&heavy.left) >= height(&heavy.right) {
            // Single right rotation: the left child becomes the root.
            let new_right = make(key, value, heavy.right.clone(), right);
            make(
                heavy.key.clone(),
                heavy.value.clone(),
                heavy.left.clone(),
                Some(new_right),
            )
        } else {
            // Left-right rotation: the left child's right child becomes the root.
            let pivot = heavy
                .right
                .as_ref()
                .expect("a right-heavy left child has a right subtree");
            let new_left = make(
                heavy.key.clone(),
                heavy.value.clone(),
                heavy.left.clone(),
                pivot.left.clone(),
            );
            let new_right = make(key, value, pivot.right.clone(), right);
            make(
                pivot.key.clone(),
                pivot.value.clone(),
                Some(new_left),
                Some(new_right),
            )
        }
    } else if right_height > left_height + 1 {
        let heavy = right.expect("a taller right subtree is nonempty");
        if height(&heavy.right) >= height(&heavy.left) {
            // Single left rotation: the right child becomes the root.
            let new_left = make(key, value, left, heavy.left.clone());
            make(
                heavy.key.clone(),
                heavy.value.clone(),
                Some(new_left),
                heavy.right.clone(),
            )
        } else {
            // Right-left rotation: the right child's left child becomes the root.
            let pivot = heavy
                .left
                .as_ref()
                .expect("a left-heavy right child has a left subtree");
            let new_left = make(key, value, left, pivot.left.clone());
            let new_right = make(
                heavy.key.clone(),
                heavy.value.clone(),
                pivot.right.clone(),
                heavy.right.clone(),
            );
            make(
                pivot.key.clone(),
                pivot.value.clone(),
                Some(new_left),
                Some(new_right),
            )
        }
    } else {
        make(key, value, left, right)
    }
}

/// An immutable ordered map; updates return new versions that share structure.
///
/// Keys compare with [`Ord`]. Values are cloned when a node on the update path
/// is copied, so cheap-to-clone values such as `Arc<[u8]>` are recommended.
pub struct PersistentMap<K, V> {
    root: Link<K, V>,
}

impl<K, V> Clone for PersistentMap<K, V> {
    fn clone(&self) -> Self {
        Self {
            root: self.root.clone(),
        }
    }
}

impl<K, V> Default for PersistentMap<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K, V> std::fmt::Debug for PersistentMap<K, V> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PersistentMap")
            .field("len", &self.len())
            .field("height", &self.height())
            .finish()
    }
}

impl<K, V> PersistentMap<K, V> {
    /// Creates an empty map.
    pub const fn new() -> Self {
        Self { root: None }
    }

    /// Returns the number of entries in O(1).
    pub fn len(&self) -> usize {
        size(&self.root)
    }

    /// Returns whether the map has no entries.
    pub fn is_empty(&self) -> bool {
        self.root.is_none()
    }

    /// Returns the tree height; zero for an empty map.
    pub fn height(&self) -> u32 {
        height(&self.root)
    }

    /// Returns whether two maps share the same root node.
    ///
    /// Sharing implies equal contents; distinct roots say nothing.
    pub fn shares_root(&self, other: &Self) -> bool {
        match (&self.root, &other.root) {
            (Some(left), Some(right)) => Arc::ptr_eq(left, right),
            (None, None) => true,
            _ => false,
        }
    }

    /// Iterates over all entries in ascending key order.
    pub fn iter(&self) -> Range<'_, K, V> {
        let mut range = Range {
            stack: Vec::new(),
            visited: 0,
        };
        range.push_left_spine(&self.root);
        range
    }
}

impl<K: Ord, V> PersistentMap<K, V> {
    /// Returns the value stored under `key`, counting visited nodes in `probe`.
    pub fn get<Q>(&self, key: &Q, probe: &mut Probe) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        let mut link = &self.root;
        while let Some(node) = link {
            probe.touch();
            match key.cmp(node.key.borrow()) {
                Ordering::Less => link = &node.left,
                Ordering::Greater => link = &node.right,
                Ordering::Equal => return Some(&node.value),
            }
        }
        None
    }

    /// Returns whether `key` is present, counting visited nodes in `probe`.
    pub fn contains_key<Q>(&self, key: &Q, probe: &mut Probe) -> bool
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        self.get(key, probe).is_some()
    }

    /// Iterates in ascending order over entries whose key is at least `lower`.
    ///
    /// The iterator is lazy: nodes are visited only as entries are requested,
    /// so a caller can stop early without paying for the rest of the range.
    pub fn range_from<Q>(&self, lower: &Q) -> Range<'_, K, V>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        let mut range = Range {
            stack: Vec::new(),
            visited: 0,
        };
        let mut link = &self.root;
        while let Some(node) = link {
            range.visited += 1;
            if node.key.borrow() < lower {
                link = &node.right;
            } else {
                range.stack.push(node);
                link = &node.left;
            }
        }
        range
    }
}

impl<K: Ord + Clone, V: Clone> PersistentMap<K, V> {
    /// Returns a new version with `key` bound to `value`, plus the replaced value.
    ///
    /// The existing key object is retained when the key was already present.
    /// This map is unchanged; the versions share every node off the search path.
    pub fn insert(&self, key: K, value: V, probe: &mut Probe) -> (Self, Option<V>) {
        let (root, previous) = insert_at(&self.root, key, value, probe);
        (Self { root: Some(root) }, previous)
    }

    /// Returns a new version without `key`, plus the removed value.
    ///
    /// Removing an absent key returns a version sharing this map's root and
    /// allocates nothing.
    pub fn remove<Q>(&self, key: &Q, probe: &mut Probe) -> (Self, Option<V>)
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        let (root, previous) = remove_at(&self.root, key, probe);
        (Self { root }, previous)
    }
}

fn insert_at<K: Ord + Clone, V: Clone>(
    link: &Link<K, V>,
    key: K,
    value: V,
    probe: &mut Probe,
) -> (Arc<Node<K, V>>, Option<V>) {
    let Some(node) = link else {
        return (make(key, value, None, None), None);
    };
    probe.touch();
    match key.cmp(&node.key) {
        Ordering::Less => {
            let (left, previous) = insert_at(&node.left, key, value, probe);
            let rebuilt = balance(
                node.key.clone(),
                node.value.clone(),
                Some(left),
                node.right.clone(),
            );
            (rebuilt, previous)
        }
        Ordering::Greater => {
            let (right, previous) = insert_at(&node.right, key, value, probe);
            let rebuilt = balance(
                node.key.clone(),
                node.value.clone(),
                node.left.clone(),
                Some(right),
            );
            (rebuilt, previous)
        }
        Ordering::Equal => {
            let rebuilt = make(
                node.key.clone(),
                value,
                node.left.clone(),
                node.right.clone(),
            );
            (rebuilt, Some(node.value.clone()))
        }
    }
}

fn remove_at<K, V, Q>(link: &Link<K, V>, key: &Q, probe: &mut Probe) -> (Link<K, V>, Option<V>)
where
    K: Ord + Clone + Borrow<Q>,
    V: Clone,
    Q: Ord + ?Sized,
{
    let Some(node) = link else {
        return (None, None);
    };
    probe.touch();
    match key.cmp(node.key.borrow()) {
        Ordering::Less => {
            let (left, previous) = remove_at(&node.left, key, probe);
            if previous.is_none() {
                return (link.clone(), None);
            }
            let rebuilt = balance(
                node.key.clone(),
                node.value.clone(),
                left,
                node.right.clone(),
            );
            (Some(rebuilt), previous)
        }
        Ordering::Greater => {
            let (right, previous) = remove_at(&node.right, key, probe);
            if previous.is_none() {
                return (link.clone(), None);
            }
            let rebuilt = balance(
                node.key.clone(),
                node.value.clone(),
                node.left.clone(),
                right,
            );
            (Some(rebuilt), previous)
        }
        Ordering::Equal => {
            let previous = Some(node.value.clone());
            let replacement = match (&node.left, &node.right) {
                (None, None) => None,
                (Some(left), None) => Some(Arc::clone(left)),
                (None, Some(right)) => Some(Arc::clone(right)),
                (Some(_), Some(right)) => {
                    // Replace this node with its in-order successor.
                    let ((successor_key, successor_value), right) = remove_min(right, probe);
                    Some(balance(
                        successor_key,
                        successor_value,
                        node.left.clone(),
                        right,
                    ))
                }
            };
            (replacement, previous)
        }
    }
}

fn remove_min<K: Clone, V: Clone>(
    node: &Arc<Node<K, V>>,
    probe: &mut Probe,
) -> ((K, V), Link<K, V>) {
    probe.touch();
    match &node.left {
        None => ((node.key.clone(), node.value.clone()), node.right.clone()),
        Some(left) => {
            let (minimum, left) = remove_min(left, probe);
            let rebuilt = balance(
                node.key.clone(),
                node.value.clone(),
                left,
                node.right.clone(),
            );
            (minimum, Some(rebuilt))
        }
    }
}

/// A lazy ascending iterator over a [`PersistentMap`].
pub struct Range<'a, K, V> {
    stack: Vec<&'a Node<K, V>>,
    visited: u64,
}

impl<'a, K, V> Range<'a, K, V> {
    fn push_left_spine(&mut self, mut link: &'a Link<K, V>) {
        while let Some(node) = link {
            self.visited += 1;
            self.stack.push(node);
            link = &node.left;
        }
    }

    /// Returns the number of tree nodes touched so far, including the seek.
    pub fn visited(&self) -> u64 {
        self.visited
    }
}

impl<'a, K, V> Iterator for Range<'a, K, V> {
    type Item = (&'a K, &'a V);

    fn next(&mut self) -> Option<Self::Item> {
        let node = self.stack.pop()?;
        self.push_left_spine(&node.right);
        Some((&node.key, &node.value))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{PersistentMap, Probe};

    struct Generator(u64);

    impl Generator {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    /// Recomputes every invariant: BST order, AVL balance, cached height and size.
    fn check_invariants<K: Ord, V>(link: &super::Link<K, V>) -> (u32, usize) {
        fn walk<'a, K: Ord, V>(
            link: &'a super::Link<K, V>,
            lower: Option<&'a K>,
            upper: Option<&'a K>,
        ) -> (u32, usize) {
            let Some(node) = link else {
                return (0, 0);
            };
            if let Some(lower) = lower {
                assert!(lower < &node.key, "BST order violated on the left");
            }
            if let Some(upper) = upper {
                assert!(&node.key < upper, "BST order violated on the right");
            }
            let (left_height, left_size) = walk(&node.left, lower, Some(&node.key));
            let (right_height, right_size) = walk(&node.right, Some(&node.key), upper);
            assert!(
                left_height.abs_diff(right_height) <= 1,
                "AVL balance violated"
            );
            let height = 1 + left_height.max(right_height);
            let size = 1 + left_size + right_size;
            assert_eq!(node.height, height, "cached height is wrong");
            assert_eq!(node.size, size, "cached size is wrong");
            (height, size)
        }
        walk(link, None, None)
    }

    fn assert_matches(map: &PersistentMap<u32, u32>, model: &BTreeMap<u32, u32>) {
        check_invariants(&map.root);
        assert_eq!(map.len(), model.len());
        let entries: Vec<_> = map.iter().map(|(key, value)| (*key, *value)).collect();
        let expected: Vec<_> = model.iter().map(|(key, value)| (*key, *value)).collect();
        assert_eq!(entries, expected);
        let mut probe = Probe::new();
        for key in 0..80 {
            assert_eq!(map.get(&key, &mut probe), model.get(&key));
            let from: Vec<_> = map.range_from(&key).map(|(key, _)| *key).collect();
            let expected: Vec<_> = model.range(key..).map(|(key, _)| *key).collect();
            assert_eq!(from, expected, "range from {key}");
        }
        if !map.is_empty() {
            // AVL height bound: h < 1.4405 log2(n + 2) - 0.3277.
            let bound = 1.4405 * ((map.len() + 2) as f64).log2();
            assert!(f64::from(map.height()) < bound);
        }
    }

    #[derive(Clone, Copy)]
    enum Step {
        Insert(u32, u32),
        Remove(u32),
    }

    /// Rebuilds the expected contents after `steps` by replaying an operation
    /// log into a fresh map, independently of any retained version.
    fn replay(log: &[Step]) -> BTreeMap<u32, u32> {
        let mut model = BTreeMap::new();
        for step in log {
            match *step {
                Step::Insert(key, value) => {
                    model.insert(key, value);
                }
                Step::Remove(key) => {
                    model.remove(&key);
                }
            }
        }
        model
    }

    #[test]
    fn matches_a_reference_map_under_random_updates() {
        for seed in [1, 7, 0x9e37_79b9, 0xdead_beef] {
            let mut generator = Generator(seed);
            let mut map = PersistentMap::new();
            let mut model = BTreeMap::new();
            let mut log = Vec::new();
            let mut versions = Vec::new();
            for step in 0..2000 {
                let key = (generator.next() % 64) as u32;
                let mut probe = Probe::new();
                if generator.next().is_multiple_of(3) {
                    let (next, removed) = map.remove(&key, &mut probe);
                    assert_eq!(removed, model.remove(&key));
                    if removed.is_none() {
                        assert!(next.shares_root(&map), "absent removal must not copy");
                    }
                    log.push(Step::Remove(key));
                    map = next;
                } else {
                    let value = step;
                    let (next, replaced) = map.insert(key, value, &mut probe);
                    assert_eq!(replaced, model.insert(key, value));
                    log.push(Step::Insert(key, value));
                    map = next;
                }
                assert!(probe.nodes <= u64::from(map.height()) + 2);
                check_invariants(&map.root);
                if step % 97 == 0 {
                    assert_matches(&map, &model);
                    versions.push((map.clone(), log.len()));
                }
            }
            assert_matches(&map, &model);
            // Earlier versions are unaffected by later updates: compare each
            // retained handle with a replay of the log up to its position.
            for (version, length) in &versions {
                assert_matches(version, &replay(&log[..*length]));
            }
        }
    }

    #[test]
    fn removals_cover_root_two_child_and_missing_cases() {
        let mut probe = Probe::new();
        let mut map = PersistentMap::new();
        for key in [50u32, 30, 70, 20, 40, 60, 80, 35, 45] {
            map = map.insert(key, key, &mut probe).0;
        }
        let original = map.clone();
        check_invariants(&map.root);
        // Root with two children.
        let (without_root, removed) = map.remove(&50, &mut probe);
        assert_eq!(removed, Some(50));
        check_invariants(&without_root.root);
        assert_eq!(without_root.len(), 8);
        // Interior node with two children.
        let (without_30, removed) = without_root.remove(&30, &mut probe);
        assert_eq!(removed, Some(30));
        check_invariants(&without_30.root);
        // Missing key.
        let (same, removed) = without_30.remove(&31, &mut probe);
        assert_eq!(removed, None);
        assert!(same.shares_root(&without_30));
        // Replacing an existing key changes the value only.
        let (replaced, previous) = without_30.insert(40, 400, &mut probe);
        assert_eq!(previous, Some(40));
        assert_eq!(replaced.len(), without_30.len());
        assert_eq!(replaced.get(&40, &mut probe), Some(&400));
        assert_eq!(without_30.get(&40, &mut probe), Some(&40));
        // The original version is untouched by every later operation.
        let keys: Vec<_> = original.iter().map(|(key, _)| *key).collect();
        assert_eq!(keys, vec![20, 30, 35, 40, 45, 50, 60, 70, 80]);
        assert_eq!(original.get(&50, &mut probe), Some(&50));
        let empty = [20u32, 35, 40, 45, 60, 70, 80]
            .iter()
            .fold(without_30.clone(), |map, key| map.remove(key, &mut probe).0);
        assert!(empty.is_empty());
        assert_eq!(empty.height(), 0);
        assert_eq!(without_30.len(), 7);
    }

    #[test]
    fn sequential_inserts_and_removals_stay_balanced() {
        let mut map = PersistentMap::new();
        let mut probe = Probe::new();
        for key in 0..1024u32 {
            map = map.insert(key, key * 2, &mut probe).0;
        }
        assert!(map.height() <= 11);
        check_invariants(&map.root);
        let full = map.clone();
        for key in (0..1024u32).rev() {
            map = map.remove(&key, &mut probe).0;
            check_invariants(&map.root);
        }
        assert!(map.is_empty());
        assert_eq!(full.len(), 1024);
        assert_eq!(full.get(&1023, &mut probe), Some(&2046));
    }

    #[test]
    fn range_seek_is_lazy_and_counts_visits() {
        let mut map = PersistentMap::new();
        let mut probe = Probe::new();
        for key in (0..4096u32).step_by(2) {
            map = map.insert(key, (), &mut probe).0;
        }
        let mut range = map.range_from(&2001);
        let seek_visits = range.visited();
        assert!(seek_visits <= u64::from(map.height()));
        assert_eq!(range.next().map(|(key, _)| *key), Some(2002));
        assert_eq!(range.next().map(|(key, _)| *key), Some(2004));
        // Two entries never visit the whole tree.
        assert!(range.visited() < 64);
        assert!(map.range_from(&5000).next().is_none());
        assert_eq!(map.range_from(&0).count(), 2048);
    }

    #[test]
    fn byte_keys_borrow_as_slices() {
        let mut probe = Probe::new();
        let empty: PersistentMap<std::sync::Arc<[u8]>, std::sync::Arc<[u8]>> = PersistentMap::new();
        let (map, _) = empty.insert(
            std::sync::Arc::from(&b"a/b"[..]),
            std::sync::Arc::from(&b"value"[..]),
            &mut probe,
        );
        let (map, _) = map.insert(
            std::sync::Arc::from(&b"a"[..]),
            std::sync::Arc::from(&b""[..]),
            &mut probe,
        );
        assert_eq!(
            map.get(b"a/b".as_slice(), &mut probe).map(|v| &v[..]),
            Some(&b"value"[..])
        );
        assert_eq!(map.get(b"a/".as_slice(), &mut probe), None);
        let keys: Vec<_> = map
            .range_from(b"a/".as_slice())
            .map(|(key, _)| key.to_vec())
            .collect();
        assert_eq!(keys, vec![b"a/b".to_vec()]);
        assert!(empty.is_empty());
    }
}
