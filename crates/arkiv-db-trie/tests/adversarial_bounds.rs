//! Adversarial checks of variable-length keys with arbitrary raw bounds.
//!
//! The keys here are raw byte strings (no escaping), drawn from bytes whose
//! nibbles mix `0`, `1`, `7`, `8` and `f`, so stored keys end at every kind of
//! nibble boundary and many candidate keys are prefix-related. A candidate
//! that would break prefix-freeness must be rejected without change.
//! Interval endpoints are arbitrary byte strings: truncations and
//! extensions of stored keys, the keys with their last byte stepped up or
//! down, and random strings, longer or shorter than any key.
//!
//! Establishes: [`KeyRange::may_hold`] never prunes a prefix under which an
//! in-range key can exist (exhaustively over a small universe); complete
//! interval proofs return exactly the model's entries, and every subtree the
//! strict verifier skips as `Outside` holds no in-range key; point proofs and
//! raw seeks agree with the model for keys that are prefixes or extensions
//! of stored keys.

mod support;

use std::{collections::BTreeMap, ops::Bound};

use arkiv_db_trie::{
    CodecError, IntervalDecision, IntervalLimits, KeyRange, MemoryOnly, MerkleMap, Probe,
    TrieError, TrieKey, inspect_interval, walk, walk_interval,
};
use support::Rng;

const MAX: usize = 6;

/// A raw byte-string key of at most `MAX` bytes, encoded as itself. Only
/// prefix-free sets of these can be stored.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Raw(Vec<u8>);

impl TrieKey for Raw {
    const MAX_LEN: usize = MAX;

    fn encode(&self) -> Vec<u8> {
        self.0.clone()
    }

    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        if bytes.len() > MAX {
            return Err(CodecError::new("raw key too long"));
        }
        Ok(Self(bytes.to_vec()))
    }
}

type Map = MerkleMap<Raw, Vec<u8>>;
type Model = BTreeMap<Vec<u8>, Vec<u8>>;

const ALPHABET: [u8; 9] = [0x00, 0x01, 0x10, 0x11, 0x0f, 0xf0, 0xff, 0x80, 0x7f];

fn random_raw(rng: &mut Rng, max: usize) -> Vec<u8> {
    let len = rng.below(max + 1);
    (0..len)
        .map(|_| ALPHABET[rng.below(ALPHABET.len())])
        .collect()
}

fn prefix_related(model: &Model, key: &[u8]) -> bool {
    model
        .keys()
        .any(|stored| stored != key && (stored.starts_with(key) || key.starts_with(stored)))
}

fn unpack(bytes: &[u8]) -> Vec<u8> {
    bytes
        .iter()
        .flat_map(|byte| [byte >> 4, byte & 0x0f])
        .collect()
}

/// A raw position derived from the model or at random.
fn position(rng: &mut Rng, model: &Model) -> Vec<u8> {
    let stored = model.keys().nth(rng.below(model.len().max(1))).cloned();
    match (rng.below(6), stored) {
        (0, Some(key)) => key,
        (1, Some(mut key)) => {
            key.truncate(rng.below(key.len() + 1));
            key
        }
        (2, Some(mut key)) => {
            key.extend(random_raw(rng, 3));
            key
        }
        (3, Some(mut key)) if !key.is_empty() => {
            let last = key.len() - 1;
            key[last] = if rng.below(2) == 0 {
                key[last].wrapping_add(1)
            } else {
                key[last].wrapping_sub(1)
            };
            key
        }
        (4, _) => (0..rng.below(9)).map(|_| rng.next() as u8).collect(),
        _ => random_raw(rng, MAX + 2),
    }
}

fn bound(rng: &mut Rng, model: &Model) -> Bound<Vec<u8>> {
    match rng.below(5) {
        0 => Bound::Unbounded,
        1 | 2 => Bound::Included(position(rng, model)),
        _ => Bound::Excluded(position(rng, model)),
    }
}

fn check_interval(map: &Map, model: &Model, range: &KeyRange) {
    let expected: Vec<_> = model
        .iter()
        .filter(|(key, _)| range.contains(key))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    let proof: Vec<_> = map
        .interval_proof(range, &MemoryOnly)
        .collect::<Result<_, TrieError>>()
        .unwrap();
    let limits = IntervalLimits {
        max_entries: model.len(),
        max_key_len: MAX,
    };
    let root = map.root_hash();
    assert_eq!(
        walk_interval(root, range, &proof, limits),
        Ok(expected.clone()),
        "{range:?}"
    );
    let (entries, trace) = inspect_interval(root, range, &proof, limits).unwrap();
    assert_eq!(entries, expected);
    // Every skipped subtree holds no key in the interval.
    for node in &trace.nodes {
        let mut prefix = node.prefix.clone();
        prefix.extend_from_slice(&node.path_node.compressed_path);
        for (slot, decision) in node.child_decisions.iter().enumerate() {
            if *decision != IntervalDecision::Outside {
                continue;
            }
            let mut below = prefix.clone();
            if node.child_decisions.len() == 16 {
                below.push(slot as u8);
            }
            let hidden: Vec<_> = expected
                .iter()
                .filter(|(key, _)| unpack(key).starts_with(&below))
                .collect();
            assert!(
                hidden.is_empty(),
                "{range:?} hides {hidden:?} under {below:?}"
            );
        }
    }
    // A narrower honest witness never proves the wider interval.
    if !expected.is_empty() {
        let narrower = KeyRange::new(Bound::Excluded(expected[0].0.clone()), range.upper.clone());
        let narrow_proof: Vec<_> = map
            .interval_proof(&narrower, &MemoryOnly)
            .collect::<Result<_, TrieError>>()
            .unwrap();
        if narrow_proof != proof {
            assert!(walk_interval(root, range, &narrow_proof, limits).is_err());
        }
    }
}

#[test]
fn arbitrary_raw_bounds_match_the_model() {
    for seed in 0..24 {
        let mut rng = Rng(0xadd0_0000 + seed);
        let mut map = Map::new();
        let mut model = Model::new();
        for step in 0..200 {
            let raw = random_raw(&mut rng, MAX);
            if rng.below(4) == 0 {
                let (next, old) = map
                    .remove(&Raw(raw.clone()), &MemoryOnly, &mut Probe::new())
                    .unwrap();
                assert_eq!(old, model.remove(&raw));
                map = next;
            } else {
                let value = vec![rng.below(256) as u8; 1 + 32 * rng.below(2)];
                let result = map.insert(
                    &Raw(raw.clone()),
                    value.clone(),
                    &MemoryOnly,
                    &mut Probe::new(),
                );
                if prefix_related(&model, &raw) {
                    assert_eq!(result.unwrap_err(), TrieError::KeyNotPrefixFree, "{raw:?}");
                } else {
                    let (next, old) = result.unwrap();
                    assert_eq!(old, model.insert(raw, value));
                    map = next;
                }
            }
            assert_eq!(map.len(), model.len() as u64);
            if step % 10 != 0 {
                continue;
            }
            let all: Vec<_> = map
                .iter(&MemoryOnly)
                .map(|entry| entry.map(|(key, value)| (key.0, value)))
                .collect::<Result<_, TrieError>>()
                .unwrap();
            assert_eq!(all, model.clone().into_iter().collect::<Vec<_>>());
            for _ in 0..8 {
                let at = position(&mut rng, &model);
                // Lookups and point proofs: prefixes and extensions are absent.
                if at.len() <= MAX {
                    let key = Raw(at.clone());
                    assert_eq!(
                        map.get(&key, &MemoryOnly, &mut Probe::new()).unwrap(),
                        model.get(&at).cloned()
                    );
                    let proof = map.prove(&key, &MemoryOnly, &mut Probe::new()).unwrap();
                    assert_eq!(
                        walk(map.root_hash(), &at, &proof),
                        Ok(model.get(&at).cloned())
                    );
                }
                let seek: Vec<_> = map
                    .range_from_encoded(&at, &MemoryOnly)
                    .take(5)
                    .map(|entry| entry.unwrap().0.0)
                    .collect();
                let expected: Vec<_> = model
                    .range(at.clone()..)
                    .take(5)
                    .map(|(k, _)| k.clone())
                    .collect();
                assert_eq!(seek, expected, "seek {at:?}");
                let range = KeyRange::new(bound(&mut rng, &model), bound(&mut rng, &model));
                check_interval(&map, &model, &range);
                if let Some(key) = model.keys().nth(rng.below(model.len().max(1))) {
                    let mut prefix = key.clone();
                    prefix.truncate(rng.below(key.len() + 1));
                    check_interval(&map, &model, &KeyRange::prefix(&prefix));
                }
            }
        }
    }
}

/// Exhaustive: whenever `may_hold(p)` is false, no whole-byte key that
/// starts with the nibble prefix `p` lies in the interval.
#[test]
fn may_hold_never_prunes_an_intersecting_prefix() {
    let nibbles = [0u8, 1, 0xf];
    let bytes: Vec<u8> = nibbles
        .iter()
        .flat_map(|high| nibbles.iter().map(move |low| (high << 4) | low))
        .collect();
    let strings = |max: usize| {
        let mut out = vec![Vec::new()];
        let mut frontier = vec![Vec::new()];
        for _ in 0..max {
            let mut next = Vec::new();
            for prefix in &frontier {
                for byte in &bytes {
                    let mut longer: Vec<u8> = prefix.clone();
                    longer.push(*byte);
                    next.push(longer);
                }
            }
            out.extend(next.iter().cloned());
            frontier = next;
        }
        out
    };
    let keys = strings(3);
    let endpoints = strings(2);
    let mut bounds = vec![Bound::Unbounded];
    for endpoint in &endpoints {
        bounds.push(Bound::Included(endpoint.clone()));
        bounds.push(Bound::Excluded(endpoint.clone()));
    }
    let mut prefixes: Vec<Vec<u8>> = vec![Vec::new()];
    let mut frontier = vec![Vec::new()];
    for _ in 0..4 {
        let mut next = Vec::new();
        for prefix in &frontier {
            for nibble in nibbles {
                let mut longer: Vec<u8> = prefix.clone();
                longer.push(nibble);
                next.push(longer);
            }
        }
        prefixes.extend(next.iter().cloned());
        frontier = next;
    }
    let unpacked: Vec<_> = keys.iter().map(|key| (key, unpack(key))).collect();
    for lower in &bounds {
        for upper in &bounds {
            let range = KeyRange::new(lower.clone(), upper.clone());
            let inside: Vec<_> = unpacked
                .iter()
                .filter(|(key, _)| range.contains(key))
                .collect();
            for prefix in &prefixes {
                if range.may_hold(prefix) {
                    continue;
                }
                if let Some((key, _)) = inside
                    .iter()
                    .find(|(_, nibbles)| nibbles.starts_with(prefix))
                {
                    panic!("{range:?} prunes {prefix:?} holding {key:?}");
                }
            }
        }
    }
}
