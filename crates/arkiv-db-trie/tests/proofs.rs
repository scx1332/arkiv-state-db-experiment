//! Point proofs and the strict verifier `walk`.
//!
//! Establishes: generated proofs equal the independent proofs of
//! `ProofRetainer` (keys ≤ 32 bytes, retained entries under 32 bytes other
//! than the root dropped because they are inline) and of the reference
//! construction (33- and 88-byte keys), for present and absent keys; `walk`
//! returns exactly the model's answer for every honest proof, including
//! the empty map and single-entry maps whose root node is under 32 bytes;
//! honest proofs whose root node is at least 32 bytes also pass
//! `alloy_trie::proof::verify_proof`; `alloy_trie::proof::verify_proof`
//! alone is unsound as a verifier (it accepts truncated inclusion proofs as
//! absence, a node supplied after a leaf whose value is that node's
//! reference, and a hash reference as a value), and `walk` rejects each of
//! those forgeries; under randomized mutation (bit flips, truncation of the
//! list and of node bytes, reordering, duplication, extra nodes, inline
//! nodes supplied separately, wrong roots, proofs of another key or of an
//! older root) `walk` never panics and never returns a value other than
//! the honest one; self-consistent adversarial tries (hashes fixed up after
//! mutating node contents) and random garbage never make it panic; a key
//! of the wrong length (a caller error `walk` cannot detect) is never
//! proven included.

mod support;

use std::collections::BTreeMap;

use alloy_primitives::{B256, Bytes, keccak256};
use alloy_trie::{Nibbles, proof::verify_proof};
use arkiv_db_trie::{EMPTY_ROOT_HASH, MemoryOnly, MerkleMap, Probe, ProofError, walk};
use support::{Rng, byte_model, reference, retainer_proof};

type Map<const N: usize> = MerkleMap<[u8; N], Vec<u8>>;

/// An independent proof generator over a byte-keyed model.
type ProofOracle = fn(&BTreeMap<Vec<u8>, Vec<u8>>, &[u8]) -> Vec<Vec<u8>>;

fn build<const N: usize>(entries: &BTreeMap<[u8; N], Vec<u8>>) -> Map<N> {
    let mut map = Map::<N>::new();
    for (key, value) in entries {
        map = map
            .insert(key, value.clone(), &MemoryOnly, &mut Probe::new())
            .unwrap()
            .0;
    }
    map
}

fn random_entries<const N: usize>(
    rng: &mut Rng,
    count: usize,
    alphabet: u8,
    max_value: usize,
) -> BTreeMap<[u8; N], Vec<u8>> {
    (0..count)
        .map(|_| (rng.key::<N>(alphabet), rng.value(max_value)))
        .collect()
}

fn prove<const N: usize>(map: &Map<N>, key: &[u8; N]) -> Vec<Vec<u8>> {
    map.prove(key, &MemoryOnly, &mut Probe::new()).unwrap()
}

fn to_bytes(proof: &[Vec<u8>]) -> Vec<Bytes> {
    proof
        .iter()
        .map(|node| Bytes::copy_from_slice(node))
        .collect()
}

/// Keys to prove: some present, some absent.
fn probe_keys<const N: usize>(
    rng: &mut Rng,
    entries: &BTreeMap<[u8; N], Vec<u8>>,
    alphabet: u8,
) -> Vec<[u8; N]> {
    let mut keys: Vec<_> = entries.keys().take(12).copied().collect();
    keys.extend((0..12).map(|_| rng.key::<N>(alphabet)));
    keys
}

/// Checks generated proofs against an oracle and verifies them.
fn honest_proofs<const N: usize>(seed: u64, rounds: usize, oracle: ProofOracle) {
    let mut rng = Rng(seed);
    for round in 0..rounds {
        let alphabet = [2u8, 3, 16, 255][round % 4];
        let count = [0, 1, 2, 5, 40, 200][round % 6];
        let entries = random_entries::<N>(&mut rng, count, alphabet, 45);
        let map = build(&entries);
        let root = map.root_hash();
        let model = byte_model(&entries);
        for key in probe_keys(&mut rng, &entries, alphabet) {
            let proof = prove(&map, &key);
            assert_eq!(proof, oracle(&model, &key), "proof of {key:?}");
            assert_eq!(walk(root, &key, &proof), Ok(entries.get(&key).cloned()));
            // alloy's verifier agrees on honest proofs it can check: keys of
            // at most 32 bytes and a root node of at least 32 bytes (it
            // compares the first node's reference form with the root hash).
            if N <= 32 && proof.first().is_some_and(|node| node.len() >= 32) {
                assert_eq!(
                    verify_proof(
                        root,
                        Nibbles::unpack(key),
                        entries.get(&key).cloned(),
                        &to_bytes(&proof)
                    ),
                    Ok(())
                );
            }
        }
    }
}

#[test]
fn proofs_match_the_proof_retainer() {
    honest_proofs::<1>(1, 24, retainer_proof);
    honest_proofs::<2>(2, 24, retainer_proof);
    honest_proofs::<8>(3, 24, retainer_proof);
    honest_proofs::<21>(4, 24, retainer_proof);
    honest_proofs::<32>(5, 24, retainer_proof);
}

#[test]
fn proofs_with_long_keys_match_the_reference() {
    honest_proofs::<33>(6, 12, reference::proof);
    honest_proofs::<88>(7, 12, reference::proof);
}

#[test]
fn empty_and_single_entry_maps() {
    let key = [9u8; 8];
    let empty = Map::<8>::new();
    assert!(prove(&empty, &key).is_empty());
    assert_eq!(walk(EMPTY_ROOT_HASH, &key, &[] as &[Vec<u8>]), Ok(None));
    // keccak256(0x80) is the empty root, but the empty map has no nodes.
    assert_eq!(keccak256([0x80]), EMPTY_ROOT_HASH);
    assert_eq!(
        walk(EMPTY_ROOT_HASH, &key, &[vec![0x80]]),
        Err(ProofError::NodesForEmptyRoot)
    );
    assert_eq!(
        walk(B256::repeat_byte(1), &key, &[] as &[Vec<u8>]),
        Err(ProofError::Incomplete { index: 0 })
    );

    // One short entry: the root is a leaf of 13 bytes, still referenced by
    // its hash. Its proof is that leaf, for the key and for any other key.
    let entries = BTreeMap::from([([1u8; 8], vec![0x2a])]);
    let map = build(&entries);
    let root = map.root_hash();
    let proof = prove(&map, &[1; 8]);
    assert_eq!(proof.len(), 1);
    assert!(proof[0].len() < 32);
    assert_eq!(keccak256(&proof[0]), root);
    assert_eq!(walk(root, &[1; 8], &proof), Ok(Some(vec![0x2a])));
    assert_eq!(prove(&map, &[2; 8]), proof);
    assert_eq!(walk(root, &[2; 8], &proof), Ok(None));
    // alloy's verifier rejects this honest proof: it expects the first node
    // to be referenced by hash, which a node under 32 bytes is not. Hence
    // the cross-check above is limited to root nodes of 32 bytes or more.
    assert!(
        verify_proof(
            root,
            Nibbles::unpack([1u8; 8]),
            Some(vec![0x2a]),
            &to_bytes(&proof)
        )
        .is_err()
    );
}

/// Keys have variable length and branches hold no values, so `walk`
/// accepts any byte string: a key that ends inside the trie's paths, or that
/// extends a stored key, is proven absent. Pinned here: such a key is never
/// proven included, and a proof for another key never verifies for it
/// unless that proof also proves its absence.
#[test]
fn prefix_related_keys_are_proven_absent_never_included() {
    // Hand-derived shape: three 8-byte keys that differ in their first
    // nibble give a root branch with leaves in slots 1, 2 and 3, each
    // holding the 15 remaining nibbles; 40-byte values make every leaf a
    // hash-referenced proof node of its own.
    let key = |first: u8| {
        let mut key = [0u8; 8];
        key[0] = first;
        key
    };
    let entries = BTreeMap::from([
        (key(0x10), vec![1; 40]),
        (key(0x20), vec![2; 40]),
        (key(0x30), vec![3; 40]),
    ]);
    let map = build(&entries);
    let root = map.root_hash();
    let present = prove(&map, &key(0x10));
    let absent = prove(&map, &key(0xf0));
    assert_eq!((present.len(), absent.len()), (2, 1));
    assert_eq!(walk(root, &key(0x10), &present), Ok(Some(vec![1; 40])));
    assert_eq!(walk(root, &key(0xf0), &absent), Ok(None));
    // Slot 15 of the root branch is empty: absence at any length.
    assert_eq!(walk(root, &[0xf0], &absent), Ok(None));
    assert_eq!(walk(root, &[0xf0; 9], &absent), Ok(None));
    // Slot 1 holds a leaf of 15 nibbles, which a 1- or 9-byte key (1 or 17
    // nibbles left there) cannot match: a divergent leaf.
    assert_eq!(walk(root, &[0x10], &present), Ok(None));
    let mut longer = [0u8; 9];
    longer[0] = 0x10;
    assert_eq!(walk(root, &longer, &present), Ok(None));
    // The empty key ends at the root branch.
    assert_eq!(walk(root, &[], &absent), Ok(None));
    let (_, trace) = arkiv_db_trie::inspect_path(root, &[], &absent).unwrap();
    assert_eq!(trace.terminal, arkiv_db_trie::PathTerminal::KeyEndsAtBranch);

    // Random maps: a present key cut short or extended with zeros follows
    // the key's own path until it runs out (absence, or trailing nodes when
    // the key's proof continues) or reaches the key's leaf, whose path then
    // diverges (absence). Never a value.
    let mut rng = Rng(21);
    for round in 0..40 {
        let count = [1, 2, 5, 40, 200][round % 5];
        let alphabet = [2u8, 16, 255][round % 3];
        let entries = random_entries::<8>(&mut rng, count, alphabet, 45);
        let map = build(&entries);
        let root = map.root_hash();
        for key in probe_keys(&mut rng, &entries, alphabet) {
            let proof = prove(&map, &key);
            for len in (0..=12).filter(|&len| len != 8) {
                let mut other = key.to_vec();
                other.resize(len, 0);
                let result = walk(root, &other, &proof);
                assert!(
                    matches!(
                        result,
                        Ok(None)
                            | Err(ProofError::TrailingNodes { .. } | ProofError::Incomplete { .. })
                    ),
                    "{result:?}"
                );
            }
        }
    }
}

#[test]
fn alloy_verify_proof_accepts_truncated_proofs_as_absence() {
    let mut rng = Rng(40);
    let entries = random_entries::<32>(&mut rng, 300, 255, 40);
    let map = build(&entries);
    let root = map.root_hash();
    let mut checked = 0;
    for (key, value) in entries.iter().take(40) {
        let proof = prove(&map, key);
        assert!(proof.len() >= 2, "300 random keys give hashed inner nodes");
        let nibbles = Nibbles::unpack(key);
        assert_eq!(
            verify_proof(root, nibbles, Some(value.clone()), &to_bytes(&proof)),
            Ok(())
        );
        for cut in 1..proof.len() {
            let truncated = &proof[..cut];
            // The forgery: a prefix of an inclusion proof "proves" absence.
            assert_eq!(
                verify_proof(root, nibbles, None, &to_bytes(truncated)),
                Ok(()),
                "alloy rejected a truncation; the unsoundness evidence changed"
            );
            assert_eq!(
                walk(root, key, truncated),
                Err(ProofError::Incomplete { index: cut })
            );
            checked += 1;
        }
    }
    assert!(checked >= 40);
}

/// The encoding of a leaf whose path is empty (a key's last node below a
/// branch at its final nibble), built from the specification by hand.
fn empty_path_leaf(value: &[u8]) -> Vec<u8> {
    reference::list(&[reference::string(&[0x20]), reference::string(value)])
}

#[test]
fn alloy_verify_proof_accepts_a_leaf_value_forged_as_a_node() {
    let mut rng = Rng(41);
    let mut entries = random_entries::<32>(&mut rng, 100, 255, 40);
    let key = [0x77u8; 32];

    // Hash form: the stored value is 0xa0 ‖ keccak(F) for a node F of 32
    // bytes or more, here a leaf with an empty path and a forged value.
    let forged_value = vec![0xf0; 40];
    let forged_node = empty_path_leaf(&forged_value);
    assert!(forged_node.len() >= 32);
    let mut stored = vec![0xa0];
    stored.extend_from_slice(keccak256(&forged_node).as_slice());
    entries.insert(key, stored.clone());
    let map = build(&entries);
    let root = map.root_hash();
    let honest = prove(&map, &key);
    assert_eq!(walk(root, &key, &honest), Ok(Some(stored.clone())));
    let mut forged = honest.clone();
    forged.push(forged_node);
    assert_eq!(
        verify_proof(
            root,
            Nibbles::unpack(key),
            Some(forged_value),
            &to_bytes(&forged)
        ),
        Ok(()),
        "alloy rejected the forged value; the unsoundness evidence changed"
    );
    assert_eq!(
        walk(root, &key, &forged),
        Err(ProofError::TrailingNodes {
            index: honest.len()
        })
    );

    // Inline form: the stored value is itself the encoding of a short leaf.
    let forged_node = empty_path_leaf(&[0x42]);
    assert!(forged_node.len() < 32);
    entries.insert(key, forged_node.clone());
    let map = build(&entries);
    let root = map.root_hash();
    let honest = prove(&map, &key);
    let mut forged = honest.clone();
    forged.push(forged_node.clone());
    assert_eq!(
        verify_proof(
            root,
            Nibbles::unpack(key),
            Some(vec![0x42]),
            &to_bytes(&forged)
        ),
        Ok(()),
        "alloy rejected the forged inline value; the unsoundness evidence changed"
    );
    assert_eq!(walk(root, &key, &honest), Ok(Some(forged_node)));
    assert!(matches!(
        walk(root, &key, &forged),
        Err(ProofError::TrailingNodes { .. })
    ));
}

#[test]
fn alloy_verify_proof_accepts_a_hash_reference_as_the_value() {
    // Two keys that differ only in their last nibble, with values long
    // enough that both leaves (empty paths) are hash-referenced from the
    // last branch.
    let first = [0u8; 32];
    let mut second = [0u8; 32];
    second[31] = 0x01;
    let entries = BTreeMap::from([(first, vec![0x11; 40]), (second, vec![0x22; 40])]);
    let map = build(&entries);
    let root = map.root_hash();
    let honest = prove(&map, &first);
    // Root extension over 63 nibbles, the branch, the leaf.
    assert_eq!(honest.len(), 3);
    let leaf_reference = {
        let mut reference = vec![0xa0];
        reference.extend_from_slice(keccak256(&honest[2]).as_slice());
        reference
    };
    let truncated = &honest[..2];
    assert_eq!(
        verify_proof(
            root,
            Nibbles::unpack(first),
            Some(leaf_reference),
            &to_bytes(truncated)
        ),
        Ok(()),
        "alloy rejected a hash reference as value; the unsoundness evidence changed"
    );
    assert_eq!(
        walk(root, &first, truncated),
        Err(ProofError::Incomplete { index: 2 })
    );
    assert_eq!(walk(root, &first, &honest), Ok(Some(vec![0x11; 40])));
}

/// The inline children (items that are RLP lists) of an encoded node.
fn inline_children(node: &[u8]) -> Vec<Vec<u8>> {
    let mut rest = node;
    let Ok(header) = alloy_rlp::Header::decode(&mut rest) else {
        return Vec::new();
    };
    assert!(header.list);
    let mut children = Vec::new();
    while !rest.is_empty() {
        let start = rest;
        let item = alloy_rlp::Header::decode(&mut rest).unwrap();
        rest = &rest[item.payload_length..];
        let bytes = &start[..start.len() - rest.len()];
        if item.list {
            children.push(bytes.to_vec());
        }
    }
    children
}

/// Every mutation of an honest proof must be rejected or, for the classes
/// where that is possible, give the honest answer; none may panic.
fn mutations<const N: usize>(seed: u64, maps: usize, alphabet: u8, max_value: usize) {
    let mut rng = Rng(seed);
    let mut rejected = 0u64;
    for _ in 0..maps {
        let count = 1 + rng.below(120);
        let entries = random_entries::<N>(&mut rng, count, alphabet, max_value);
        let map = build(&entries);
        let root = map.root_hash();
        let other_version = {
            let (key, value) = (rng.key::<N>(alphabet), rng.value(max_value));
            map.insert(&key, value, &MemoryOnly, &mut Probe::new())
                .unwrap()
                .0
        };
        let keys = probe_keys(&mut rng, &entries, alphabet);
        for key in &keys {
            let honest = prove(&map, key);
            let expected = entries.get(key).cloned();
            assert_eq!(walk(root, key, &honest), Ok(expected.clone()));
            let mut reject = |proof: &[Vec<u8>], what: &str| {
                let outcome = walk(root, key, proof);
                assert!(outcome.is_err(), "{what} accepted: {outcome:?}");
                rejected += 1;
            };

            // Bit flips anywhere in any node.
            for _ in 0..8 {
                let mut proof = honest.clone();
                let node = rng.below(proof.len());
                let byte = rng.below(proof[node].len());
                proof[node][byte] ^= 1 << rng.below(8);
                reject(&proof, "bit flip");
            }
            // Truncation of the node list at every depth, and of node bytes.
            for cut in 0..honest.len() {
                assert_eq!(
                    walk(root, key, &honest[..cut]),
                    Err(ProofError::Incomplete { index: cut })
                );
                let mut proof = honest.clone();
                let keep = rng.below(proof[cut].len());
                proof[cut].truncate(keep);
                reject(&proof, "truncated node");
            }
            // Reordering, duplication, extra nodes.
            if honest.len() >= 2 {
                let mut proof = honest.clone();
                let (left, right) = (rng.below(proof.len()), rng.below(proof.len()));
                if left != right {
                    proof.swap(left, right);
                    reject(&proof, "reordered");
                }
                proof = honest.clone();
                proof.reverse();
                reject(&proof, "reversed");
            }
            for position in 0..=honest.len() {
                let mut proof = honest.clone();
                let copy = proof[rng.below(proof.len())].clone();
                proof.insert(position, copy);
                reject(&proof, "duplicated");
            }
            let other = keys[rng.below(keys.len())];
            let mut proof = honest.clone();
            proof.extend(prove(&map, &other).into_iter().skip(1));
            if proof.len() > honest.len() {
                reject(&proof, "extra honest nodes");
            }
            let mut proof = honest.clone();
            proof.push(rng.some_bytes(1, 80));
            reject(&proof, "extra garbage");
            // Inline nodes supplied separately.
            for (index, node) in honest.iter().enumerate() {
                for child in inline_children(node) {
                    let mut proof = honest.clone();
                    proof.insert(index + 1, child.clone());
                    reject(&proof, "separate inline node");
                    let mut proof = honest.clone();
                    proof.push(child);
                    reject(&proof, "trailing inline node");
                }
            }
            // A wrong root, and the proof from another version of the map.
            assert_eq!(
                walk(B256::from_slice(&rng.bytes(32)), key, &honest),
                Err(ProofError::RootMismatch)
            );
            let stale = other_version
                .prove(key, &MemoryOnly, &mut Probe::new())
                .unwrap();
            if stale != honest {
                reject(&stale, "proof for another root");
            }
            // A proof of another key: rejected, or the true answer for
            // that other key (proofs of neighbors can coincide).
            let other = keys[rng.below(keys.len())];
            match walk(root, &other, &honest) {
                Ok(value) => assert_eq!(value, entries.get(&other).cloned()),
                Err(_) => rejected += 1,
            }
        }
    }
    assert!(rejected > 1000, "only {rejected} rejections");
}

#[test]
fn mutated_proofs_are_rejected() {
    mutations::<8>(50, 8, 3, 40);
    mutations::<8>(51, 8, 255, 8);
    mutations::<2>(52, 8, 255, 3);
    mutations::<33>(53, 5, 2, 40);
    mutations::<88>(54, 4, 255, 60);
}

/// Replaces `old` by `new` everywhere in `node` (a hash reference).
fn replace_reference(node: &mut [u8], old: &[u8; 32], new: &[u8; 32]) -> bool {
    let mut replaced = false;
    for start in 0..node.len().saturating_sub(31) {
        if &node[start..start + 32] == old {
            node[start..start + 32].copy_from_slice(new);
            replaced = true;
        }
    }
    replaced
}

#[test]
fn adversarial_tries_and_garbage_never_panic() {
    let mut rng = Rng(60);
    let mut outcomes = [0u64; 3];
    for round in 0..1000 {
        let count = 1 + rng.below(60);
        let entries = random_entries::<8>(&mut rng, count, [2, 16, 255][round % 3], 40);
        let map = build(&entries);
        let key = match round % 2 {
            0 => *entries.keys().nth(rng.below(entries.len())).unwrap(),
            _ => rng.key::<8>(255),
        };
        let mut proof = prove(&map, &key);
        // Mutate one node's contents, then repair every hash reference
        // above it, so the walk decodes the mutated bytes: the adversary
        // controls the whole trie and publishes its root.
        let target = rng.below(proof.len());
        let before: [u8; 32] = keccak256(&proof[target]).0;
        let node = &mut proof[target];
        match rng.below(4) {
            0 => {
                let at = rng.below(node.len());
                node[at] ^= 1 << rng.below(8);
            }
            1 => {
                let at = rng.below(node.len());
                node[at] = rng.next() as u8;
            }
            2 => node.truncate(rng.below(node.len())),
            _ => {
                let extra = rng.some_bytes(1, 4);
                let at = rng.below(node.len() + 1);
                node.splice(at..at, extra);
            }
        }
        // `old` is the original hash of the node below `index`.
        let mut old = before;
        for index in (0..target).rev() {
            let child: [u8; 32] = keccak256(&proof[index + 1]).0;
            let original: [u8; 32] = keccak256(&proof[index]).0;
            if !replace_reference(&mut proof[index], &old, &child) {
                break;
            }
            old = original;
        }
        let root = keccak256(&proof[0]);
        match walk(root, &key, &proof) {
            Ok(Some(_)) => outcomes[0] += 1,
            Ok(None) => outcomes[1] += 1,
            Err(_) => outcomes[2] += 1,
        }
        // Pure garbage under its own hash.
        let garbage: Vec<Vec<u8>> = (0..1 + rng.below(4))
            .map(|_| rng.some_bytes(0, 120))
            .collect();
        let _ = walk(keccak256(&garbage[0]), &key, &garbage);
        let _ = walk(keccak256(&garbage[0]), &[], &garbage);
    }
    // Many mutations break the encoding; the rest decode to other tries the
    // walk follows to a terminal. All three outcomes occur, so the walk did
    // run over adversarial node contents rather than stopping at a hash.
    assert!(outcomes.iter().all(|count| *count > 0), "{outcomes:?}");
}

#[test]
fn old_proofs_still_verify_after_later_updates() {
    let mut rng = Rng(70);
    let entries = random_entries::<21>(&mut rng, 150, 16, 30);
    let old = build(&entries);
    let old_root = old.root_hash();
    let key = *entries.keys().nth(10).unwrap();
    let old_proof = prove(&old, &key);
    let mut map = old.clone();
    for _ in 0..50 {
        let (other, value) = (rng.key::<21>(16), rng.value(30));
        map = map
            .insert(&other, value, &MemoryOnly, &mut Probe::new())
            .unwrap()
            .0;
    }
    map = map.remove(&key, &MemoryOnly, &mut Probe::new()).unwrap().0;
    assert_eq!(
        walk(old_root, &key, &old_proof),
        Ok(entries.get(&key).cloned())
    );
    assert_eq!(prove(&old, &key), old_proof, "the old handle still proves");
    assert_eq!(walk(map.root_hash(), &key, &prove(&map, &key)), Ok(None));
    assert_eq!(
        walk(map.root_hash(), &key, &old_proof),
        Err(ProofError::RootMismatch)
    );
}

#[test]
fn proof_generation_is_lazy_and_counted() {
    let mut rng = Rng(80);
    let entries = random_entries::<32>(&mut rng, 500, 255, 40);
    let map = build(&entries);
    let key = *entries.keys().next().unwrap();
    let mut proof = map.proof(&key, &MemoryOnly);
    assert_eq!(
        proof.probe().nodes,
        0,
        "nothing visited before the first item"
    );
    proof.next().unwrap().unwrap();
    assert_eq!(proof.probe().nodes, 1, "the root alone");
    let rest = proof.by_ref().count();
    let mut lookup = Probe::new();
    map.get(&key, &MemoryOnly, &mut lookup).unwrap();
    // The generator visits exactly the lookup path.
    assert_eq!(proof.probe(), lookup);
    let mut probe = Probe::new();
    let nodes = map.prove(&key, &MemoryOnly, &mut probe).unwrap();
    assert_eq!(nodes.len(), rest + 1);
    assert_eq!(probe, lookup);
}

#[test]
fn inspected_paths_report_actual_inline_and_hash_nodes_and_all_absence_terminals() {
    use arkiv_db_trie::{NodeKind, PathChild, PathSource, PathTerminal, inspect_path};
    // Hand-derived shape: extension 12 -> branch slots 3 and 4 -> leaf 0.
    // Everything fits inline, so the wire proof contains only its root.
    let map = build(&BTreeMap::from([
        ([0x12, 0x30], vec![b'a']),
        ([0x12, 0x40], vec![b'b']),
    ]));
    let proof = prove(&map, &[0x12, 0x30]);
    assert_eq!(proof.len(), 1);
    let (value, trace) = inspect_path(map.root_hash(), &[0x12, 0x30], &proof).unwrap();
    assert_eq!(value, Some(vec![b'a']));
    assert_eq!(trace.terminal, PathTerminal::Inclusion);
    assert_eq!(
        trace
            .nodes
            .iter()
            .map(|n| (n.kind, n.depth, n.source, n.proof_index))
            .collect::<Vec<_>>(),
        vec![
            (NodeKind::Extension, 0, PathSource::Root, 0),
            (NodeKind::Branch, 2, PathSource::Inline, 0),
            (NodeKind::Leaf, 3, PathSource::Inline, 0),
        ]
    );
    assert_eq!(trace.nodes[0].compressed_path, [1, 2]);
    assert_eq!(trace.nodes[1].selected_nibble, Some(3));
    assert_eq!(
        trace.nodes[1].children[4],
        PathChild::Inline(vec![0xc2, 0x30, b'b'])
    );
    assert_eq!(trace.nodes[2].compressed_path, [0]);
    assert_eq!(trace.nodes[2].rlp, [0xc2, 0x30, b'a']);
    for (key, terminal) in [
        ([0x22, 0x30], PathTerminal::DivergentExtension),
        ([0x12, 0x50], PathTerminal::EmptyBranchSlot),
        ([0x12, 0x31], PathTerminal::DivergentLeaf),
    ] {
        let proof = prove(&map, &key);
        let (value, trace) = inspect_path(map.root_hash(), &key, &proof).unwrap();
        assert_eq!(value, None);
        assert_eq!(trace.terminal, terminal);
    }
    let (_, trace) = inspect_path(EMPTY_ROOT_HASH, &[0; 2], &[] as &[Vec<u8>]).unwrap();
    assert!(trace.nodes.is_empty());
    assert_eq!(trace.terminal, PathTerminal::EmptyRoot);

    let large = build(&BTreeMap::from([
        ([0x12, 0x30], vec![b'a'; 40]),
        ([0x12, 0x40], vec![b'b'; 40]),
    ]));
    let proof = prove(&large, &[0x12, 0x30]);
    let (_, trace) = inspect_path(large.root_hash(), &[0x12, 0x30], &proof).unwrap();
    assert_eq!(trace.nodes[1].source, PathSource::Hash);
    assert_eq!(trace.nodes[2].source, PathSource::Hash);
    for node in &trace.nodes {
        assert_eq!(node.rlp, proof[node.proof_index]);
        assert_eq!(node.hash, keccak256(&node.rlp));
    }
    for end in 0..proof.len() {
        assert!(inspect_path(large.root_hash(), &[0x12, 0x30], &proof[..end]).is_err());
    }
    let mut bad = proof.clone();
    bad.last_mut().unwrap()[2] ^= 1;
    assert!(inspect_path(large.root_hash(), &[0x12, 0x30], &bad).is_err());
    bad = proof.clone();
    bad.push(proof[0].clone());
    assert!(inspect_path(large.root_hash(), &[0x12, 0x30], &bad).is_err());
}
