//! `MerkleMap` against independent models: contents from a `BTreeMap`,
//! roots from `alloy_trie::HashBuilder` (keys ≤ 32 bytes) or the reference
//! construction in `support` (any length), and hand-derived visit counts.
//!
//! Establishes: replacement and removal results and lengths agree with the
//! model after every step of random insert/remove histories, roots every
//! ten steps, and point reads, ordered and lower-bounded iteration every
//! fifty steps and at the end; the root depends
//! only on the key/value set (permuted insertion orders and different
//! histories reaching one set give one root); earlier handles are
//! unaffected by later updates; the path of any key visits at most
//! 2·LEN + 1 nodes, reached exactly by crafted deep-path key sets; removal
//! adds at most one visit (the collapse sibling); a value with an empty
//! encoding is rejected by `insert` before anything is visited or read.
//! Everything here runs in memory; stored links are covered by `store.rs`.

mod support;

use std::collections::BTreeMap;

use alloy_primitives::B256;
use arkiv_db_trie::{EMPTY_ROOT_HASH, MemoryOnly, MerkleMap, Probe, TrieError};
use support::{Rng, byte_model, deep_keys, hash_builder_root, reference};

type Oracle = fn(&BTreeMap<Vec<u8>, Vec<u8>>) -> B256;

/// Checks every observable of `map` against `model`.
fn assert_matches<const N: usize>(
    map: &MerkleMap<[u8; N], Vec<u8>>,
    model: &BTreeMap<[u8; N], Vec<u8>>,
    oracle: Oracle,
    rng: &mut Rng,
    alphabet: u8,
) {
    assert_eq!(map.len(), model.len() as u64);
    assert_eq!(map.root_hash(), oracle(&byte_model(model)), "root");
    let entries: Vec<_> = map
        .iter(&MemoryOnly)
        .collect::<Result<_, TrieError>>()
        .unwrap();
    let expected: Vec<_> = model
        .iter()
        .map(|(key, value)| (*key, value.clone()))
        .collect();
    assert_eq!(entries, expected, "full iteration");
    let bound = 2 * N as u64 + 1;
    for (key, value) in model {
        let mut probe = Probe::new();
        assert_eq!(
            map.get(key, &MemoryOnly, &mut probe).unwrap().as_ref(),
            Some(value)
        );
        assert!(probe.nodes <= bound, "path of {} nodes", probe.nodes);
        assert_eq!(probe.loads, 0);
    }
    for _ in 0..8 {
        let lower = rng.key::<N>(alphabet);
        let mut probe = Probe::new();
        assert_eq!(
            map.get(&lower, &MemoryOnly, &mut probe).unwrap(),
            model.get(&lower).cloned()
        );
        assert!(probe.nodes <= bound);
        let from: Vec<_> = map
            .range_from(&lower, &MemoryOnly)
            .map(|entry| entry.unwrap().0)
            .collect();
        let expected: Vec<_> = model.range(lower..).map(|(key, _)| *key).collect();
        assert_eq!(from, expected, "range from {lower:?}");
    }
}

/// A random history of inserts, replacements and removals of present and
/// absent keys, compared with the model after every step (results and
/// length), every ten steps (root) and every fifty steps (everything);
/// the fully checked versions are retained and rechecked at the end.
fn random_history<const N: usize>(seed: u64, steps: usize, alphabet: u8, oracle: Oracle) {
    let mut rng = Rng(seed);
    let mut map: MerkleMap<[u8; N], Vec<u8>> = MerkleMap::new();
    let mut model = BTreeMap::new();
    let mut retained = Vec::new();
    for step in 0..steps {
        let mut probe = Probe::new();
        let key = match model.keys().nth(rng.below(model.len().max(1))) {
            Some(existing) if rng.below(3) > 0 => *existing,
            _ => rng.key::<N>(alphabet),
        };
        if rng.below(3) == 0 {
            let (next, removed) = map.remove(&key, &MemoryOnly, &mut probe).unwrap();
            assert_eq!(removed, model.remove(&key));
            if removed.is_none() {
                assert!(next.same_root(&map));
            }
            // The path, plus at most one collapse sibling.
            assert!(probe.nodes <= 2 * N as u64 + 2);
            map = next;
        } else {
            let value = rng.value(40);
            let (next, replaced) = map
                .insert(&key, value.clone(), &MemoryOnly, &mut probe)
                .unwrap();
            assert_eq!(replaced, model.insert(key, value));
            assert!(probe.nodes <= 2 * N as u64 + 1);
            map = next;
        }
        assert_eq!(map.len(), model.len() as u64);
        if step % 10 == 0 {
            assert_eq!(map.root_hash(), oracle(&byte_model(&model)), "step {step}");
        }
        if step % 50 == 0 || step + 1 == steps {
            assert_matches(&map, &model, oracle, &mut rng, alphabet);
            retained.push((map.clone(), model.clone()));
        }
    }
    for (version, model) in &retained {
        assert_matches(version, model, oracle, &mut rng, alphabet);
    }
    // Remove everything: back to the empty root.
    for key in model.keys() {
        map = map.remove(key, &MemoryOnly, &mut Probe::new()).unwrap().0;
    }
    assert!(map.is_empty());
    assert_eq!(map.root_hash(), EMPTY_ROOT_HASH);
    assert!(map.root_link().is_empty());
}

#[test]
fn handles_are_shareable_across_threads() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<MerkleMap<[u8; 8], Vec<u8>>>();
    assert_send_sync::<arkiv_db_trie::NodeBox<Vec<u8>>>();
    let map: MerkleMap<u64, Vec<u8>> = MerkleMap::new()
        .insert(&1, vec![1], &MemoryOnly, &mut Probe::new())
        .unwrap()
        .0;
    let reader = map.clone();
    let value = std::thread::spawn(move || reader.get(&1, &MemoryOnly, &mut Probe::new()))
        .join()
        .unwrap()
        .unwrap();
    assert_eq!(value, Some(vec![1]));
}

#[test]
fn empty_map() {
    let map: MerkleMap<[u8; 8], Vec<u8>> = MerkleMap::new();
    assert_eq!(map.root_hash(), EMPTY_ROOT_HASH);
    assert_eq!(map.len(), 0);
    let mut probe = Probe::new();
    assert_eq!(map.get(&[1; 8], &MemoryOnly, &mut probe).unwrap(), None);
    assert_eq!(probe, Probe::new(), "an empty map visits nothing");
    assert!(map.iter(&MemoryOnly).next().is_none());
    let (same, removed) = map.remove(&[1; 8], &MemoryOnly, &mut probe).unwrap();
    assert_eq!(removed, None);
    assert_eq!(same.root_hash(), EMPTY_ROOT_HASH);
    assert_eq!(hash_builder_root(&BTreeMap::new()), EMPTY_ROOT_HASH);
}

#[test]
fn reference_oracle_agrees_with_hash_builder() {
    // The reference construction is checked against alloy's HashBuilder
    // wherever alloy can hold the keys, before it is trusted for longer ones.
    let mut rng = Rng(3);
    for round in 0..200 {
        let len = [1, 2, 3, 8, 21, 32][round % 6];
        let count = rng.below(60);
        let alphabet = [2u8, 3, 16, 255][rng.below(4)];
        let mut entries = BTreeMap::new();
        for _ in 0..count {
            let key: Vec<u8> = (0..len)
                .map(|_| (rng.next() % u64::from(alphabet)) as u8)
                .collect();
            entries.insert(key, rng.value(50));
        }
        assert_eq!(reference::root(&entries), hash_builder_root(&entries));
        let probe_key: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
        for key in entries.keys().take(3).chain([&probe_key]) {
            assert_eq!(
                reference::proof(&entries, key),
                support::retainer_proof(&entries, key)
            );
        }
    }
}

#[test]
fn random_histories_match_hash_builder() {
    random_history::<1>(1, 600, 255, hash_builder_root);
    random_history::<2>(2, 600, 4, hash_builder_root);
    random_history::<8>(3, 1500, 3, hash_builder_root);
    random_history::<8>(4, 1500, 255, hash_builder_root);
    random_history::<21>(5, 1000, 2, hash_builder_root);
    random_history::<32>(6, 1000, 255, hash_builder_root);
    random_history::<32>(7, 1000, 2, hash_builder_root);
}

#[test]
fn random_histories_with_long_keys_match_the_reference() {
    random_history::<33>(8, 500, 2, reference::root);
    random_history::<33>(9, 500, 255, reference::root);
    random_history::<88>(10, 300, 2, reference::root);
    random_history::<88>(11, 300, 255, reference::root);
}

#[test]
fn root_depends_only_on_the_key_value_set() {
    let mut rng = Rng(21);
    for round in 0..12 {
        let alphabet = [2u8, 16, 255][round % 3];
        let entries: BTreeMap<[u8; 8], Vec<u8>> = (0..(1 + rng.below(150)))
            .map(|_| (rng.key::<8>(alphabet), rng.value(40)))
            .collect();
        let expected = hash_builder_root(&byte_model(&entries));
        let mut order: Vec<_> = entries.iter().collect();
        let mut roots = Vec::new();
        for _ in 0..4 {
            rng.shuffle(&mut order);
            let mut map: MerkleMap<[u8; 8], Vec<u8>> = MerkleMap::new();
            // Interleave decoys inserted and removed again, and a stale
            // value later replaced, so the histories differ in more than order.
            for (key, value) in &order {
                let decoy = rng.key::<8>(alphabet);
                let mut probe = Probe::new();
                if !entries.contains_key(&decoy) {
                    map = map
                        .insert(&decoy, vec![0xdd], &MemoryOnly, &mut probe)
                        .unwrap()
                        .0;
                }
                map = map
                    .insert(key, vec![0xee], &MemoryOnly, &mut probe)
                    .unwrap()
                    .0;
                map = map
                    .insert(key, (*value).clone(), &MemoryOnly, &mut probe)
                    .unwrap()
                    .0;
                if !entries.contains_key(&decoy) {
                    map = map.remove(&decoy, &MemoryOnly, &mut probe).unwrap().0;
                }
            }
            assert_eq!(map.len(), entries.len() as u64);
            roots.push(map.root_hash());
            let keys: Vec<_> = map
                .iter(&MemoryOnly)
                .map(|entry| entry.unwrap().0)
                .collect();
            assert_eq!(keys, entries.keys().copied().collect::<Vec<_>>());
        }
        assert!(roots.iter().all(|root| *root == expected));
    }
}

fn deep_path<const N: usize>(oracle: Oracle) {
    let keys = deep_keys::<N>();
    let mut map: MerkleMap<[u8; N], Vec<u8>> = MerkleMap::new();
    let mut model = BTreeMap::new();
    for key in &keys {
        map = map
            .insert(key, vec![1], &MemoryOnly, &mut Probe::new())
            .unwrap()
            .0;
        model.insert(*key, vec![1]);
    }
    assert_eq!(map.root_hash(), oracle(&byte_model(&model)));
    // Hand-derived: the zero key passes a branch at each of its 2·N nibbles
    // (the zero key and key j part at nibble j) and ends at a leaf with an
    // empty path, so its lookup visits exactly 2·N + 1 nodes.
    let mut probe = Probe::new();
    assert_eq!(
        map.get(&keys[0], &MemoryOnly, &mut probe).unwrap(),
        Some(vec![1])
    );
    assert_eq!(probe.nodes, 2 * N as u64 + 1);
    // Key j (at index j + 1) leaves the zero key's path at the branch of
    // depth j, so it visits the branches of depths 0..=j and its leaf:
    // j + 2 nodes.
    for (index, key) in keys.iter().enumerate().skip(1) {
        let mut probe = Probe::new();
        map.get(key, &MemoryOnly, &mut probe).unwrap();
        assert_eq!(probe.nodes, index as u64 + 1);
    }
    // Removing the key that parts at the last nibble walks the full path
    // (2·N branches and its leaf) and then examines the one sibling left in
    // the deepest branch (the zero key's leaf), which absorbs the nibble:
    // 2·N + 2 visits.
    let mut probe = Probe::new();
    let (smaller, removed) = map.remove(&keys[2 * N], &MemoryOnly, &mut probe).unwrap();
    assert_eq!(removed, Some(vec![1]));
    assert_eq!(probe.nodes, 2 * N as u64 + 2);
    model.remove(&keys[2 * N]);
    assert_eq!(smaller.root_hash(), oracle(&byte_model(&model)));
    let mut probe = Probe::new();
    smaller.get(&keys[0], &MemoryOnly, &mut probe).unwrap();
    assert_eq!(probe.nodes, 2 * N as u64);
}

#[test]
fn depth_bound_is_reached_by_crafted_keys() {
    deep_path::<1>(hash_builder_root);
    deep_path::<8>(hash_builder_root);
    deep_path::<21>(hash_builder_root);
    deep_path::<32>(hash_builder_root);
    deep_path::<33>(reference::root);
    deep_path::<88>(reference::root);
}

#[test]
fn an_empty_value_is_rejected_by_insert() {
    let mut probe = Probe::new();
    let empty: MerkleMap<[u8; 4], Vec<u8>> = MerkleMap::new();
    assert_eq!(
        empty
            .insert(b"abcd", Vec::new(), &MemoryOnly, &mut probe)
            .unwrap_err(),
        TrieError::EmptyValue
    );
    let (map, _) = empty
        .insert(b"abcd", vec![1], &MemoryOnly, &mut Probe::new())
        .unwrap();
    let root = map.root_hash();
    // Neither a new key nor an existing one takes an empty value, and the
    // map is unchanged and still hashes.
    for key in [b"abcd", b"efgh"] {
        assert_eq!(
            map.insert(key, Vec::new(), &MemoryOnly, &mut probe)
                .unwrap_err(),
            TrieError::EmptyValue
        );
    }
    assert_eq!(probe, Probe::new(), "rejected before any visit");
    assert_eq!(map.root_hash(), root);
    assert_eq!(
        map.get(b"abcd", &MemoryOnly, &mut Probe::new()).unwrap(),
        Some(vec![1])
    );
    // Rejected before anything is read: an unreadable root is not reached.
    let unreadable = MerkleMap::<[u8; 4], Vec<u8>>::from_root(B256::repeat_byte(1), 1).unwrap();
    assert_eq!(
        unreadable
            .insert(b"abcd", Vec::new(), &MemoryOnly, &mut Probe::new())
            .unwrap_err(),
        TrieError::EmptyValue
    );
}

#[test]
fn single_entries_and_inline_nodes() {
    let mut probe = Probe::new();
    // One short entry: the root is a leaf shorter than 32 bytes, hashed
    // anyway because a root is always referenced by hash.
    let map: MerkleMap<[u8; 2], Vec<u8>> = MerkleMap::new()
        .insert(&[0xab, 0xcd], vec![1], &MemoryOnly, &mut probe)
        .unwrap()
        .0;
    let model = BTreeMap::from([(vec![0xab, 0xcd], vec![1])]);
    assert_eq!(map.root_hash(), hash_builder_root(&model));
    // Leaf [0x20, 0xab, 0xcd] with value 0x01: c5 83 20 ab cd 01, 6 bytes.
    let mut encoding = Vec::new();
    match map.root_link() {
        arkiv_db_trie::Link::Mem(node) => node.encode(&mut encoding),
        other => panic!("unexpected root {other:?}"),
    }
    assert_eq!(encoding, [0xc5, 0x83, 0x20, 0xab, 0xcd, 0x01]);
    assert_eq!(map.root_hash(), alloy_primitives::keccak256(&encoding));

    // One-byte keys with one-byte values: every leaf is inline in the root
    // branch, and each lookup visits the branch and one inline leaf.
    let mut map: MerkleMap<[u8; 1], Vec<u8>> = MerkleMap::new();
    let mut model = BTreeMap::new();
    for key in [0x01u8, 0x02, 0x13, 0xf0] {
        map = map
            .insert(&[key], vec![key], &MemoryOnly, &mut probe)
            .unwrap()
            .0;
        model.insert(vec![key], vec![key]);
    }
    assert_eq!(map.root_hash(), hash_builder_root(&model));
    for key in [0x01u8, 0x02] {
        let mut probe = Probe::new();
        map.get(&[key], &MemoryOnly, &mut probe).unwrap();
        // Root branch (nibble 0), inner branch (nibble 1 or 2), leaf.
        assert_eq!(probe.nodes, 3);
    }
    let mut probe = Probe::new();
    map.get(&[0x13], &MemoryOnly, &mut probe).unwrap();
    assert_eq!(probe.nodes, 2, "root branch and its leaf");
}

#[test]
fn from_root_validates_the_handle() {
    let empty = MerkleMap::<[u8; 4], Vec<u8>>::from_root(EMPTY_ROOT_HASH, 0).unwrap();
    assert!(empty.root_link().is_empty());
    assert_eq!(
        MerkleMap::<[u8; 4], Vec<u8>>::from_root(EMPTY_ROOT_HASH, 3).unwrap_err(),
        TrieError::InvalidHandle {
            root: EMPTY_ROOT_HASH,
            len: 3
        }
    );
    let other = B256::repeat_byte(7);
    assert!(MerkleMap::<[u8; 4], Vec<u8>>::from_root(other, 0).is_err());
    let stored = MerkleMap::<[u8; 4], Vec<u8>>::from_root(other, 2).unwrap();
    assert_eq!(stored.root_hash(), other);
    assert_eq!(stored.len(), 2);
}

#[test]
fn replacing_a_value_keeps_the_length_and_old_versions() {
    let mut probe = Probe::new();
    let first: MerkleMap<u64, Vec<u8>> = MerkleMap::new()
        .insert(&7, vec![1], &MemoryOnly, &mut probe)
        .unwrap()
        .0;
    let (second, replaced) = first.insert(&7, vec![2], &MemoryOnly, &mut probe).unwrap();
    assert_eq!(replaced, Some(vec![1]));
    assert_eq!(second.len(), 1);
    assert_eq!(
        first.get(&7, &MemoryOnly, &mut probe).unwrap(),
        Some(vec![1])
    );
    assert_eq!(
        second.get(&7, &MemoryOnly, &mut probe).unwrap(),
        Some(vec![2])
    );
    assert!(!first.same_root(&second));
    let (third, _) = second.insert(&7, vec![1], &MemoryOnly, &mut probe).unwrap();
    assert!(third.same_root(&first), "same contents, same root");
    // u64 keys iterate in numeric order (big-endian encoding).
    let mut map: MerkleMap<u64, Vec<u8>> = MerkleMap::new();
    for key in [300u64, 2, 1 << 40, 70_000, 1] {
        map = map
            .insert(&key, vec![1], &MemoryOnly, &mut probe)
            .unwrap()
            .0;
    }
    let keys: Vec<u64> = map
        .iter(&MemoryOnly)
        .map(|entry| entry.unwrap().0)
        .collect();
    assert_eq!(keys, [1, 2, 300, 70_000, 1 << 40]);
}
