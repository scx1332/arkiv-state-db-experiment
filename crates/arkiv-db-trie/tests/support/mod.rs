//! Shared test support: a seeded generator and the independent oracles.
//!
//! Two root/proof oracles, neither of which shares code with the crate
//! under test:
//!
//! - `alloy_trie::HashBuilder` (with `ProofRetainer`) over sorted leaves,
//!   usable for keys of at most 32 bytes because alloy's `Nibbles` holds at
//!   most 64 nibbles;
//! - `reference`, a textbook recursive Merkle Patricia construction written
//!   here from the Ethereum yellow paper (appendices B, C and D) with its
//!   own RLP and hex-prefix code, for any key length. It is checked against
//!   `HashBuilder` for keys up to 32 bytes and then used for 33- and 88-byte
//!   keys.

#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};

use alloy_primitives::{B256, keccak256};
use alloy_trie::{EMPTY_ROOT_HASH, HashBuilder, Nibbles, proof::ProofRetainer};

/// SplitMix64: small, seeded and deterministic.
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    pub fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }

    pub fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.next() as u8).collect()
    }

    /// Between `min` and `min + spread - 1` random bytes.
    pub fn some_bytes(&mut self, min: usize, spread: usize) -> Vec<u8> {
        let len = min + self.below(spread);
        self.bytes(len)
    }

    /// A value of 1 to `max` random bytes (values are never empty).
    pub fn value(&mut self, max: usize) -> Vec<u8> {
        let len = 1 + self.below(max);
        self.bytes(len)
    }

    /// A key of `N` bytes, each below `alphabet`: a small alphabet makes
    /// keys share long prefixes and exercises extensions and deep paths.
    pub fn key<const N: usize>(&mut self, alphabet: u8) -> [u8; N] {
        let mut key = [0u8; N];
        for byte in &mut key {
            *byte = (self.next() % u64::from(alphabet)) as u8;
        }
        key
    }

    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        for index in (1..items.len()).rev() {
            let other = self.below(index + 1);
            items.swap(index, other);
        }
    }
}

/// The root `HashBuilder` computes over sorted `(key, value)` leaves.
pub fn hash_builder_root(entries: &BTreeMap<Vec<u8>, Vec<u8>>) -> B256 {
    let mut builder = HashBuilder::default();
    for (key, value) in entries {
        builder.add_leaf(Nibbles::unpack(key), value);
    }
    builder.root()
}

/// The proof `ProofRetainer` keeps for `key`, as this crate emits proofs:
/// sorted by path (root first), with retained nodes shorter than 32 bytes
/// dropped except the root, because those are inline in their parent. The
/// empty map's retained `0x80` becomes the empty proof.
pub fn retainer_proof(entries: &BTreeMap<Vec<u8>, Vec<u8>>, key: &[u8]) -> Vec<Vec<u8>> {
    if entries.is_empty() {
        return Vec::new();
    }
    let retainer = ProofRetainer::from_iter([Nibbles::unpack(key)]);
    let mut builder = HashBuilder::default().with_proof_retainer(retainer);
    for (key, value) in entries {
        builder.add_leaf(Nibbles::unpack(key), value);
    }
    builder.root();
    builder
        .take_proof_nodes()
        .into_nodes_sorted()
        .into_iter()
        .filter(|(path, node)| path.is_empty() || node.len() >= 32)
        .map(|(_, node)| node.to_vec())
        .collect()
}

/// An independent Merkle Patricia construction over sorted leaves.
pub mod reference {
    use super::*;

    fn length_prefix(offset: u8, len: usize) -> Vec<u8> {
        if len < 56 {
            return vec![offset + len as u8];
        }
        let be = len.to_be_bytes();
        let skip = be.iter().take_while(|byte| **byte == 0).count();
        let mut out = vec![offset + 55 + (be.len() - skip) as u8];
        out.extend_from_slice(&be[skip..]);
        out
    }

    /// RLP string (yellow paper appendix B).
    pub fn string(bytes: &[u8]) -> Vec<u8> {
        if bytes.len() == 1 && bytes[0] < 0x80 {
            return bytes.to_vec();
        }
        let mut out = length_prefix(0x80, bytes.len());
        out.extend_from_slice(bytes);
        out
    }

    /// RLP list of already encoded items.
    pub fn list(items: &[Vec<u8>]) -> Vec<u8> {
        let payload: Vec<u8> = items.concat();
        let mut out = length_prefix(0xc0, payload.len());
        out.extend_from_slice(&payload);
        out
    }

    /// Hex-prefix encoding (yellow paper appendix C).
    pub fn compact(nibbles: &[u8], leaf: bool) -> Vec<u8> {
        let flag = if leaf { 2 } else { 0 };
        let mut out = Vec::new();
        let rest = if nibbles.len() % 2 == 1 {
            out.push(((flag + 1) << 4) | nibbles[0]);
            &nibbles[1..]
        } else {
            out.push(flag << 4);
            nibbles
        };
        for pair in rest.chunks(2) {
            out.push((pair[0] << 4) | pair[1]);
        }
        out
    }

    pub fn unpack(bytes: &[u8]) -> Vec<u8> {
        bytes
            .iter()
            .flat_map(|byte| [byte >> 4, byte & 15])
            .collect()
    }

    /// A child reference: the encoding if under 32 bytes, else its hash;
    /// a hashed child is an object of its own and is added to `objects`.
    fn reference(encoding: Vec<u8>, objects: &mut BTreeSet<B256>) -> Vec<u8> {
        if encoding.len() < 32 {
            encoding
        } else {
            let id = keccak256(&encoding);
            objects.insert(id);
            string(id.as_slice())
        }
    }

    type Leaves<'a> = [(Vec<u8>, &'a [u8])];

    /// The encoding of the node holding `leaves` (sorted, sharing their
    /// first `depth` nibbles); adds the ids of hashed descendants to `objects`.
    fn node(leaves: &Leaves<'_>, depth: usize, objects: &mut BTreeSet<B256>) -> Vec<u8> {
        if let [(path, value)] = leaves {
            return list(&[string(&compact(&path[depth..], true)), string(value)]);
        }
        let first = &leaves[0].0;
        let last = &leaves[leaves.len() - 1].0;
        let shared = first[depth..]
            .iter()
            .zip(&last[depth..])
            .take_while(|(left, right)| left == right)
            .count();
        if shared > 0 {
            let child = node(leaves, depth + shared, objects);
            return list(&[
                string(&compact(&first[depth..depth + shared], false)),
                reference(child, objects),
            ]);
        }
        let mut items = Vec::new();
        for nibble in 0..16u8 {
            let group: Vec<(Vec<u8>, &[u8])> = leaves
                .iter()
                .filter(|(path, _)| path[depth] == nibble)
                .cloned()
                .collect();
            items.push(if group.is_empty() {
                string(&[])
            } else {
                let child = node(&group, depth + 1, objects);
                reference(child, objects)
            });
        }
        items.push(string(&[]));
        list(&items)
    }

    fn leaves(entries: &BTreeMap<Vec<u8>, Vec<u8>>) -> Vec<(Vec<u8>, &[u8])> {
        entries
            .iter()
            .map(|(key, value)| (unpack(key), value.as_slice()))
            .collect()
    }

    /// The root hash of the trie of `entries`.
    pub fn root(entries: &BTreeMap<Vec<u8>, Vec<u8>>) -> B256 {
        if entries.is_empty() {
            return EMPTY_ROOT_HASH;
        }
        keccak256(node(&leaves(entries), 0, &mut BTreeSet::new()))
    }

    /// The ids of the objects a store holds for the trie of `entries`: the
    /// root, whatever its size, and every node of 32 bytes or more.
    pub fn objects(entries: &BTreeMap<Vec<u8>, Vec<u8>>) -> BTreeSet<B256> {
        let mut objects = BTreeSet::new();
        if !entries.is_empty() {
            let root = node(&leaves(entries), 0, &mut objects);
            objects.insert(keccak256(&root));
        }
        objects
    }

    /// The root node and every node of 32 bytes or more on `key`'s path.
    pub fn proof(entries: &BTreeMap<Vec<u8>, Vec<u8>>, key: &[u8]) -> Vec<Vec<u8>> {
        let key = unpack(key);
        let mut group = leaves(entries);
        let mut proof = Vec::new();
        let mut depth = 0;
        let mut root = true;
        while !group.is_empty() {
            let encoding = node(&group, depth, &mut BTreeSet::new());
            if root || encoding.len() >= 32 {
                proof.push(encoding);
            }
            root = false;
            if group.len() == 1 {
                break;
            }
            let first = group[0].0.clone();
            let last = &group[group.len() - 1].0;
            let shared = first[depth..]
                .iter()
                .zip(&last[depth..])
                .take_while(|(left, right)| left == right)
                .count();
            if shared > 0 {
                // A key ending inside the shared path is absent here.
                if key.get(depth..depth + shared) != Some(&first[depth..depth + shared]) {
                    break;
                }
                depth += shared;
                continue;
            }
            // A key ending at this branch is absent (branches hold no value).
            let Some(&nibble) = key.get(depth) else {
                break;
            };
            group.retain(|(path, _)| path[depth] == nibble);
            depth += 1;
        }
        proof
    }
}

/// Converts a model with array keys into the byte-keyed form the oracles take.
pub fn byte_model<const N: usize>(
    model: &BTreeMap<[u8; N], Vec<u8>>,
) -> BTreeMap<Vec<u8>, Vec<u8>> {
    model
        .iter()
        .map(|(key, value)| (key.to_vec(), value.clone()))
        .collect()
}

/// `2·N + 1` keys that force a branch at every nibble of the all-zero key:
/// key `0` is all zeros and key `j + 1` is zero except nibble `j`, which is
/// one, so key `j + 1` leaves the zero key's path at the branch of depth
/// `j`.
pub fn deep_keys<const N: usize>() -> Vec<[u8; N]> {
    let mut keys = vec![[0u8; N]];
    for nibble in 0..2 * N {
        let mut key = [0u8; N];
        key[nibble / 2] = if nibble % 2 == 0 { 0x10 } else { 0x01 };
        keys.push(key);
    }
    keys
}
