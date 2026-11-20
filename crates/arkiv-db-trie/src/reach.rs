//! Read-only reachability walks over committed maps, for storage reports.
use crate::{
    Link, MerkleMap, NodeBox, NodeLoader, ObjectId, TrieError, TrieKey, TrieValue, node::Node,
};

impl<K: TrieKey, V: TrieValue> MerkleMap<K, V> {
    /// Visits every stored object reachable from this map's root, depth
    /// first, without decoding a subtree twice when the caller says so.
    ///
    /// For each stored node `object(id, encoded_len)` is called first; if it
    /// returns `false` the node's subtree (and the leaf values in it) is
    /// skipped, which is how a caller that walks several roots visits shared
    /// subtrees once. Otherwise the node is verified and decoded, and every
    /// leaf value in it (including leaves of inline children) is passed to
    /// `leaf`, so the caller can walk nested maps. Inline nodes are part of
    /// their parent's object and are not reported separately. Nodes still in
    /// memory (never committed) are walked but not reported.
    ///
    /// Skipping by id alone is only sound per value type: the same bytes can
    /// be a plain leaf in one map and a nested-root holder in another, so a
    /// caller keys its "seen" set by the map kind as well as the id.
    pub fn walk_stored<L, E>(
        &self,
        loader: &L,
        object: &mut dyn FnMut(&ObjectId, usize) -> bool,
        leaf: &mut dyn FnMut(&V) -> Result<(), E>,
    ) -> Result<(), E>
    where
        L: NodeLoader<V> + ?Sized,
        E: From<L::Error> + From<TrieError>,
    {
        let mut stack = vec![self.root.clone()];
        while let Some(link) = stack.pop() {
            let node = match link {
                Link::Empty => continue,
                Link::Mem(node) => node,
                Link::Stored(id) => {
                    let bytes = loader.read(&id)?.ok_or(TrieError::Missing { id })?;
                    if !object(&id, bytes.len()) {
                        continue;
                    }
                    NodeBox::decode_stored(&id, &bytes)?
                }
            };
            match &node.node {
                Node::Leaf { value, .. } => leaf(value)?,
                Node::Extension { child, .. } => stack.push(child.clone()),
                Node::Branch { children } => {
                    stack.extend(children.iter().rev().cloned());
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::{MemoryNodeStore, MerkleMap, ObjectId, Probe, Staged};
    use std::collections::BTreeSet;

    type Map = MerkleMap<[u8; 4], Vec<u8>>;

    #[test]
    fn walk_reports_each_stored_node_once_and_every_leaf() {
        let mut store = MemoryNodeStore::new();
        let mut map = Map::new();
        for i in 0u32..200 {
            map = map
                .insert(
                    &i.to_be_bytes(),
                    vec![i as u8; 40],
                    &store,
                    &mut Probe::new(),
                )
                .unwrap()
                .0;
        }
        let mut staged = Staged::new();
        map.stage(&mut staged);
        store.commit(&staged);
        let stored = map.to_stored();
        let mut ids = BTreeSet::new();
        let mut leaves = 0;
        stored
            .walk_stored::<_, crate::TrieError>(
                &store,
                &mut |id: &ObjectId, _| ids.insert(*id),
                &mut |_| {
                    leaves += 1;
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(leaves, 200);
        assert_eq!(ids.len(), store.len());
        // A second walk with the same seen-set skips everything.
        let mut again = 0;
        stored
            .walk_stored::<_, crate::TrieError>(
                &store,
                &mut |id: &ObjectId, _| ids.insert(*id),
                &mut |_| {
                    again += 1;
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(again, 0);
    }

    #[test]
    fn object_references_name_every_stored_child_and_leaf_value_once() {
        let mut store = MemoryNodeStore::new();
        let mut map = Map::new();
        for i in 0u32..300 {
            // Short values give inline leaves, long ones stored leaves.
            let value = vec![i as u8; if i % 3 == 0 { 2 } else { 40 }];
            map = map
                .insert(&i.to_be_bytes(), value, &store, &mut Probe::new())
                .unwrap()
                .0;
        }
        let mut staged = Staged::new();
        map.stage(&mut staged);
        store.commit(&staged);
        let root = map.root_hash();
        let mut hashes = Vec::new();
        let mut values = Vec::new();
        let mut ids = BTreeSet::new();
        map.to_stored()
            .walk_stored::<_, crate::TrieError>(
                &store,
                &mut |id: &ObjectId, _| {
                    ids.insert(*id);
                    true
                },
                &mut |_| Ok(()),
            )
            .unwrap();
        for id in &ids {
            let bytes = crate::NodeLoader::<Vec<u8>>::read(&store, id)
                .unwrap()
                .unwrap();
            crate::object_references(&bytes, &mut hashes, &mut values).unwrap();
        }
        let referenced: BTreeSet<_> = hashes.iter().copied().collect();
        // Equal leaves under different branch slots are one object with
        // several parents: references are a multiset.
        assert!(referenced.len() < hashes.len());
        assert!(!referenced.contains(&root));
        assert_eq!(referenced.len() + 1, ids.len());
        // Following references from the root with multiplicity finds every
        // leaf value exactly once per key.
        fn leaves(store: &MemoryNodeStore, id: &ObjectId) -> usize {
            let bytes = crate::NodeLoader::<Vec<u8>>::read(store, id)
                .unwrap()
                .unwrap();
            let (mut hashes, mut values) = (Vec::new(), Vec::new());
            crate::object_references(&bytes, &mut hashes, &mut values).unwrap();
            values.len()
                + hashes
                    .iter()
                    .map(|child| leaves(store, child))
                    .sum::<usize>()
        }
        assert_eq!(leaves(&store, &root), 300);
        assert!(crate::object_references(&[0xc0], &mut hashes, &mut values).is_err());
    }
}
