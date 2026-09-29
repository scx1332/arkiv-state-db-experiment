use alloy_primitives::keccak256;
use arkiv_db_trie::{
    EMPTY_ROOT_HASH, IntervalDecision, MAX_INTERVAL_TRACE_NODES, MemoryOnly, MerkleMap, NodeKind,
    PathChild, PathSource, Probe, ProofError, inspect_interval, walk_interval,
};

fn map<const N: usize>(
    entries: impl IntoIterator<Item = ([u8; N], Vec<u8>)>,
) -> MerkleMap<[u8; N], Vec<u8>> {
    entries
        .into_iter()
        .fold(MerkleMap::new(), |map, (key, value)| {
            map.insert(&key, value, &MemoryOnly, &mut Probe::new())
                .unwrap()
                .0
        })
}

fn proof<const N: usize>(
    map: &MerkleMap<[u8; N], Vec<u8>>,
    lower: &[u8; N],
    upper: &[u8; N],
) -> Vec<Vec<u8>> {
    map.interval_proof(lower, upper, &MemoryOnly)
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[test]
fn branch_trace_distinguishes_open_opaque_and_authenticated_empty_slots() {
    let map = map([9, 10, 15, 20, 21].map(|key| ([key], vec![key; 40])));
    let proof = proof(&map, &[10], &[19]);
    let (entries, trace) = inspect_interval(map.root_hash(), &[10], &[19], &proof, 5).unwrap();
    assert_eq!(
        entries,
        vec![(vec![10], vec![10; 40]), (vec![15], vec![15; 40])]
    );
    assert_eq!(
        walk_interval(map.root_hash(), &[10], &[19], &proof, 5).unwrap(),
        entries
    );
    assert_eq!(trace.nodes.len(), 5);
    assert_eq!(trace.nodes[0].prefix, Vec::<u8>::new());
    assert_eq!(trace.nodes[0].parent_index, None);
    assert_eq!(trace.nodes[0].path_node.source, PathSource::Root);
    assert_eq!(
        trace.nodes[0].child_decisions[..2],
        [IntervalDecision::Open; 2]
    );

    let low = &trace.nodes[1];
    assert_eq!(low.parent_index, Some(0));
    assert_eq!(low.parent_slot, Some(0));
    assert_eq!(low.prefix, [0]);
    assert_eq!(low.child_decisions[9], IntervalDecision::Outside);
    assert!(matches!(low.path_node.children[9], PathChild::Hash(_)));
    for slot in 11..=14 {
        assert_eq!(low.child_decisions[slot], IntervalDecision::Empty);
        assert_eq!(low.path_node.children[slot], PathChild::Empty);
    }
    assert_eq!(low.child_decisions[10], IntervalDecision::Open);
    assert_eq!(low.child_decisions[15], IntervalDecision::Open);
    assert_eq!(trace.nodes[2].parent_index, Some(1));
    assert_eq!(trace.nodes[2].parent_slot, Some(10));
    assert_eq!(trace.nodes[2].prefix, [0, 10]);
    assert_eq!(trace.nodes[2].leaf_key, Some(vec![10]));
    assert_eq!(trace.nodes[2].leaf_value, Some(vec![10; 40]));
    assert!(trace.nodes[2].leaf_included);

    let high = &trace.nodes[4];
    assert_eq!(high.parent_index, Some(0));
    assert_eq!(high.parent_slot, Some(1));
    assert_eq!(high.prefix, [1]);
    assert_eq!(high.child_decisions[..4], [IntervalDecision::Empty; 4]);
    assert_eq!(high.child_decisions[4..6], [IntervalDecision::Outside; 2]);
    for (index, node) in trace.nodes.iter().enumerate() {
        assert_eq!(node.path_node.proof_index, index);
        assert_eq!(node.path_node.rlp, proof[index]);
        assert_eq!(node.path_node.hash, keccak256(&proof[index]));
        assert_eq!(node.path_node.depth, node.prefix.len());
        assert_eq!(node.path_node.selected_nibble, None);
    }
}

#[test]
fn inline_visits_keep_their_supplied_ancestor_after_a_hashed_sibling() {
    let map = map([
        ([0x10], vec![1; 40]),
        ([0x20], vec![2]),
        ([0x30], vec![3]),
        ([0x31], vec![4]),
    ]);
    let proof = proof(&map, &[0], &[255]);
    assert_eq!(proof.len(), 2);
    let (entries, trace) = inspect_interval(map.root_hash(), &[0], &[255], &proof, 4).unwrap();
    assert_eq!(entries.len(), 4);
    assert_eq!(trace.nodes.len(), 6);
    assert_eq!(trace.nodes[1].path_node.source, PathSource::Hash);
    assert_eq!(trace.nodes[1].path_node.proof_index, 1);
    for (index, node) in trace.nodes.iter().enumerate().skip(2) {
        assert_eq!(node.path_node.source, PathSource::Inline);
        assert_eq!(node.path_node.proof_index, 0);
        assert_eq!(node.path_node.hash, keccak256(&node.path_node.rlp));
        let parent_index = node.parent_index.unwrap();
        assert!(parent_index < index);
        let parent = &trace.nodes[parent_index];
        assert_eq!(
            parent.path_node.children[usize::from(node.parent_slot.unwrap())],
            PathChild::Inline(node.path_node.rlp.clone())
        );
    }
    assert_eq!(trace.nodes[3].path_node.kind, NodeKind::Branch);
    assert_eq!(trace.nodes[4].parent_index, Some(3));
    assert_eq!(trace.nodes[5].parent_index, Some(3));
}

#[test]
fn extension_edges_record_prefixes_and_outside_decisions() {
    let map = map([([0xaa, 0x10], vec![1; 40]), ([0xaa, 0x20], vec![2; 40])]);
    let outside_proof = proof(&map, &[0xab, 0], &[255, 255]);
    assert_eq!(outside_proof.len(), 1);
    let (entries, outside) =
        inspect_interval(map.root_hash(), &[0xab, 0], &[255, 255], &outside_proof, 0).unwrap();
    assert!(entries.is_empty());
    assert_eq!(outside.nodes.len(), 1);
    assert_eq!(outside.nodes[0].path_node.kind, NodeKind::Extension);
    assert_eq!(outside.nodes[0].path_node.compressed_path, [10, 10]);
    assert_eq!(
        outside.nodes[0].child_decisions,
        [IntervalDecision::Outside]
    );
    assert!(matches!(
        outside.nodes[0].path_node.children[0],
        PathChild::Hash(_)
    ));

    let all_proof = proof(&map, &[0, 0], &[255, 255]);
    let (_, all) = inspect_interval(map.root_hash(), &[0, 0], &[255, 255], &all_proof, 2).unwrap();
    assert_eq!(all.nodes[0].child_decisions, [IntervalDecision::Open]);
    assert_eq!(all.nodes[1].parent_index, Some(0));
    assert_eq!(all.nodes[1].parent_slot, None);
    assert_eq!(all.nodes[1].prefix, [10, 10]);
}

#[test]
fn a_visited_boundary_leaf_can_be_excluded_from_results() {
    let map = map([([0x10, 0], vec![1; 40]), ([0x20, 0], vec![2; 40])]);
    let proof = proof(&map, &[0x10, 1], &[0x11, 0]);
    let (entries, trace) =
        inspect_interval(map.root_hash(), &[0x10, 1], &[0x11, 0], &proof, 0).unwrap();
    assert!(entries.is_empty());
    assert_eq!(trace.nodes.len(), 2);
    let leaf = &trace.nodes[1];
    assert_eq!(leaf.prefix, [1]);
    assert_eq!(leaf.path_node.compressed_path, [0, 0, 0]);
    assert_eq!(leaf.leaf_key, Some(vec![0x10, 0]));
    assert_eq!(leaf.leaf_value, Some(vec![1; 40]));
    assert!(!leaf.leaf_included);
}

#[test]
fn empty_and_reversed_intervals_have_no_visited_nodes() {
    let nodes: Vec<Vec<u8>> = Vec::new();
    for (root, lower, upper) in [(EMPTY_ROOT_HASH, [0], [255]), (keccak256([]), [255], [0])] {
        let (entries, trace) = inspect_interval(root, &lower, &upper, &nodes, 0).unwrap();
        assert!(entries.is_empty());
        assert!(trace.nodes.is_empty());
        assert_eq!(
            inspect_interval(root, &lower, &upper, &[vec![0xc0]], 0),
            Err(ProofError::TrailingNodes { index: 0 })
        );
    }
}

#[test]
fn inspected_witnesses_reject_missing_reordered_mutated_and_trailing_nodes() {
    let map = map([9, 10, 15, 20, 21].map(|key| ([key], vec![key; 40])));
    let proof = proof(&map, &[10], &[19]);
    for omitted in 0..proof.len() {
        let mut incomplete = proof.clone();
        incomplete.remove(omitted);
        assert!(inspect_interval(map.root_hash(), &[10], &[19], &incomplete, 5).is_err());
    }
    let mut reordered = proof.clone();
    reordered.swap(2, 3);
    assert!(matches!(
        inspect_interval(map.root_hash(), &[10], &[19], &reordered, 5),
        Err(ProofError::HashMismatch { index: 2 })
    ));
    let mut trailing = proof.clone();
    trailing.push(vec![0xc0]);
    assert_eq!(
        inspect_interval(map.root_hash(), &[10], &[19], &trailing, 5),
        Err(ProofError::TrailingNodes { index: proof.len() })
    );
    let mut mutated = proof.clone();
    mutated[2][0] ^= 1;
    assert!(inspect_interval(map.root_hash(), &[10], &[19], &mutated, 5).is_err());
    assert_eq!(
        inspect_interval(map.root_hash(), &[10], &[19], &proof, 1),
        Err(ProofError::EntryLimitExceeded)
    );
    assert!(matches!(
        inspect_interval(map.root_hash(), &[10], &[19], &proof[..proof.len() - 1], 5),
        Err(ProofError::Incomplete { .. })
    ));
}

#[test]
fn inspection_node_limit_rejects_instead_of_truncating_an_otherwise_valid_walk() {
    let count = MAX_INTERVAL_TRACE_NODES;
    let map = map((0..count).map(|key| ((key as u32).to_be_bytes(), vec![1])));
    let lower = [0; 4];
    let upper = [255; 4];
    let proof = proof(&map, &lower, &upper);
    assert_eq!(
        walk_interval(map.root_hash(), &lower, &upper, &proof, count)
            .unwrap()
            .len(),
        count
    );
    assert_eq!(
        inspect_interval(map.root_hash(), &lower, &upper, &proof, count),
        Err(ProofError::EntryLimitExceeded)
    );
}
