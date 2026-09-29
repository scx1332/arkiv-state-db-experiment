use alloy_primitives::keccak256;
use arkiv_db_trie::{MemoryNodeStore, MemoryOnly, MerkleMap, Probe, Staged};

#[test]
fn root_reachable_walk_preserves_inline_edges_and_complete_keys() {
    let mut map: MerkleMap<[u8; 2], Vec<u8>> = MerkleMap::new();
    for key in [[0x12, 0x30], [0x12, 0x31], [0x99, 0x99]] {
        map = map
            .insert(&key, vec![1], &MemoryOnly, &mut Probe::new())
            .unwrap()
            .0;
    }
    let mut store = MemoryNodeStore::new();
    let mut staged = Staged::new();
    map.stage(&mut staged);
    store.commit(&staged);
    let map: MerkleMap<[u8; 2], Vec<u8>> = MerkleMap::from_root(map.root_hash(), 3).unwrap();
    let mut pending = vec![(vec![], map.root_hash())];
    let mut leaves = vec![];
    let mut inline = 0;
    while let Some((path, hash)) = pending.pop() {
        let node = map.explore(&path, &store).unwrap().unwrap();
        assert_eq!(node.hash, hash);
        assert_eq!(keccak256(&node.rlp), hash);
        if let Some(key) = node.key {
            leaves.push(key)
        }
        for child in node.children {
            if let Some(hash) = child.hash {
                if let Some(bytes) = child.inline {
                    inline += 1;
                    assert_eq!(keccak256(bytes), hash)
                }
                let mut next = path.clone();
                next.push(child.slot);
                pending.push((next, hash));
            }
        }
    }
    leaves.sort();
    assert_eq!(
        leaves,
        vec![vec![0x12, 0x30], vec![0x12, 0x31], vec![0x99, 0x99]]
    );
    assert!(inline > 0);
    assert!(map.explore(&[17], &store).unwrap().is_none());
    assert!(map.explore(&[0; 5], &store).unwrap().is_none());
    assert!(map.explore(&[0], &store).unwrap().is_none());
}
