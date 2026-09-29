//! Independent interval results against a sorted-map model, including gaps.
use std::collections::BTreeMap;

use arkiv_db_trie::{MemoryOnly, MerkleMap, Probe, ProofError, inspect_interval, walk_interval};

fn check<const N: usize>(model: BTreeMap<[u8; N], Vec<u8>>, bounds: Vec<[u8; N]>) {
    let mut map = MerkleMap::<[u8; N], Vec<u8>>::new();
    for (key, value) in &model {
        map = map
            .insert(key, value.clone(), &MemoryOnly, &mut Probe::new())
            .unwrap()
            .0;
    }
    for lower in &bounds {
        for upper in &bounds {
            let expected: Vec<_> = model
                .iter()
                .filter(|(key, _)| *key >= lower && *key <= upper)
                .map(|(key, value)| (key.to_vec(), value.clone()))
                .collect();
            let proof: Vec<_> = map
                .interval_proof(lower, upper, &MemoryOnly)
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            assert_eq!(
                walk_interval(map.root_hash(), lower, upper, &proof, model.len()).unwrap(),
                expected
            );
            let (inspected, trace) =
                inspect_interval(map.root_hash(), lower, upper, &proof, model.len()).unwrap();
            assert_eq!(inspected, expected);
            let included_leaves: Vec<_> = trace
                .nodes
                .into_iter()
                .filter(|node| node.leaf_included)
                .map(|node| (node.leaf_key.unwrap(), node.leaf_value.unwrap()))
                .collect();
            assert_eq!(included_leaves, expected);
            if !expected.is_empty() {
                assert_eq!(
                    walk_interval(map.root_hash(), lower, upper, &proof, expected.len() - 1),
                    Err(ProofError::EntryLimitExceeded)
                );
            }
            // Removing an interior or boundary witness must never forge absence
            // or a partial successful answer, even for an honestly empty range.
            for omitted in 0..proof.len() {
                let mut incomplete = proof.clone();
                incomplete.remove(omitted);
                assert!(
                    walk_interval(map.root_hash(), lower, upper, &incomplete, model.len()).is_err()
                );
            }
            let mut extra = proof.clone();
            extra.push(vec![0xc0]);
            assert!(walk_interval(map.root_hash(), lower, upper, &extra, model.len()).is_err());
        }
    }
}

#[test]
fn intervals_match_sorted_model_for_inline_and_hashed_leaves() {
    let keys = [0, 1, 15, 16, 17, 31, 64, 127, 128, 254, 255];
    let bounds = [
        0, 1, 2, 14, 15, 16, 17, 18, 30, 31, 32, 63, 64, 65, 126, 127, 128, 129, 253, 254, 255,
    ];
    for value_size in [1, 80] {
        check(
            keys.into_iter()
                .map(|key| ([key], vec![key; value_size]))
                .collect(),
            bounds.map(|n| [n]).to_vec(),
        );
    }
    check::<1>(BTreeMap::new(), bounds.map(|n| [n]).to_vec());
    check::<1>(
        BTreeMap::from([([17], vec![1])]),
        bounds.map(|n| [n]).to_vec(),
    );
}

#[test]
fn intervals_preserve_deep_prefix_boundaries() {
    let key = |prefix, tail| {
        let mut bytes = [prefix; 132];
        bytes[131] = tail;
        bytes
    };
    let model = [0, 7, 15, 16, 31, 127, 128, 255]
        .into_iter()
        .map(|tail| (key(42, tail), vec![tail; 40]))
        .chain([(key(41, 255), vec![41; 40]), (key(43, 0), vec![43; 40])])
        .collect();
    let mut bounds: Vec<_> = [0, 1, 7, 8, 15, 16, 17, 127, 128, 129, 255]
        .into_iter()
        .map(|tail| key(42, tail))
        .collect();
    bounds.extend([key(41, 255), key(43, 0), [0; 132], [255; 132]]);
    check(model, bounds);
}
