//! Variable-length, prefix-free keys against a byte-ordered `BTreeMap`.
//!
//! Keys are [`ByteKey`]s (escaped, `00`-terminated byte strings, the empty
//! string included) drawn from an alphabet rich in `00`, `01` and `ff`, so
//! many keys are prefix-related as raw strings while their encodings stay
//! prefix-free. Establishes, after random insert/overwrite/remove histories:
//! contents, length, iteration order and lower-bounded seeks (typed and raw)
//! agree with the model; roots equal both independent oracles
//! (`HashBuilder` and the reference construction) and do not depend on
//! insertion order; retained handles keep their contents; temporary entries
//! collapse back to the original root; point proofs (inclusion and absence)
//! verify and equal the reference proofs; complete interval proofs over
//! arbitrary inclusive/exclusive/unbounded raw endpoints and prefix runs
//! return exactly the model's entries and reject every omission, extra node
//! and entry-limit overflow; stored maps reopen and walk identically; and a
//! key that breaks the prefix-free contract is rejected without change.

mod support;

use std::{collections::BTreeMap, ops::Bound};

use arkiv_db_trie::{
    IntervalLimits, KeyRange, MemoryNodeStore, MemoryOnly, MerkleMap, Probe, ProofError, Staged,
    TrieError, TrieKey, is_prefix_free,
    order::{ByteKey, encode_bytes, escape_bytes},
    walk, walk_interval,
};
use support::{Rng, hash_builder_root, reference};

const MAX: usize = 12;
type Key = ByteKey<MAX>;
type Map = MerkleMap<Key, Vec<u8>>;
type Model = BTreeMap<Vec<u8>, Vec<u8>>;

const ALPHABET: [u8; 6] = [0x00, 0x01, 0x02, b'a', b'b', 0xff];

fn enc(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    encode_bytes(raw, &mut out);
    out
}

fn random_raw(rng: &mut Rng) -> Vec<u8> {
    let len = rng.below(MAX + 1);
    // Bias towards short keys so prefix relations are frequent.
    let len = if rng.below(2) == 0 { len.min(3) } else { len };
    (0..len)
        .map(|_| ALPHABET[rng.below(ALPHABET.len())])
        .collect()
}

fn random_value(rng: &mut Rng) -> Vec<u8> {
    // Short values keep nodes inline; long ones force hashed nodes.
    let len = if rng.below(2) == 0 { 1 } else { 33 };
    vec![rng.below(256) as u8; len]
}

/// The model keyed by encodings, as the oracles take it.
fn encoded(model: &Model) -> Model {
    model.iter().map(|(k, v)| (enc(k), v.clone())).collect()
}

fn check(map: &Map, model: &Model, rng: &mut Rng) {
    assert_eq!(map.len(), model.len() as u64);
    let encoded_model = encoded(model);
    // Encoding order is raw order.
    assert!(
        encoded_model
            .keys()
            .zip(model.keys())
            .all(|(e, raw)| *e == enc(raw))
    );
    assert_eq!(
        map.root_hash(),
        reference::root(&encoded_model),
        "reference root"
    );
    assert_eq!(
        map.root_hash(),
        hash_builder_root(&encoded_model),
        "HashBuilder root"
    );
    let all: Vec<_> = map
        .iter(&MemoryOnly)
        .map(|entry| entry.map(|(key, value)| (key.0, value)))
        .collect::<Result<_, TrieError>>()
        .unwrap();
    assert_eq!(all, model.clone().into_iter().collect::<Vec<_>>());
    for _ in 0..6 {
        let raw = random_raw(rng);
        let mut probe = Probe::new();
        assert_eq!(
            map.get(&ByteKey(raw.clone()), &MemoryOnly, &mut probe)
                .unwrap(),
            model.get(&raw).cloned()
        );
        assert!(probe.nodes <= 2 * Key::MAX_LEN as u64 + 1);
        // Typed seek.
        let from: Vec<_> = map
            .range_from(&ByteKey(raw.clone()), &MemoryOnly)
            .take(4)
            .map(|entry| entry.unwrap().0.0)
            .collect();
        let expected: Vec<_> = model
            .range(raw.clone()..)
            .take(4)
            .map(|(k, _)| k.clone())
            .collect();
        assert_eq!(from, expected, "seek from {raw:?}");
        // Raw seek positions: escaped prefixes and arbitrary bytes.
        let mut position = Vec::new();
        escape_bytes(&raw, &mut position);
        if rng.below(2) == 0 {
            position.push(ALPHABET[rng.below(ALPHABET.len())]);
        }
        let from: Vec<_> = map
            .range_from_encoded(&position, &MemoryOnly)
            .take(4)
            .map(|entry| enc(&entry.unwrap().0.0))
            .collect();
        let expected: Vec<_> = encoded_model
            .range(position.clone()..)
            .take(4)
            .map(|(k, _)| k.clone())
            .collect();
        assert_eq!(from, expected, "raw seek from {position:?}");
        // Point proofs for present and absent keys.
        let proof = map
            .prove(&ByteKey(raw.clone()), &MemoryOnly, &mut Probe::new())
            .unwrap();
        assert_eq!(
            walk(map.root_hash(), &enc(&raw), &proof),
            Ok(model.get(&raw).cloned())
        );
        assert_eq!(proof, reference::proof(&encoded_model, &enc(&raw)));
    }
}

fn random_bound(rng: &mut Rng, model: &Model) -> Bound<Vec<u8>> {
    let raw = match (
        rng.below(3),
        model.keys().nth(rng.below(model.len().max(1))),
    ) {
        (0, Some(key)) => key.clone(),
        _ => random_raw(rng),
    };
    // Endpoints are raw positions: full encodings, escaped prefixes, or
    // encodings with junk appended.
    let mut point = Vec::new();
    escape_bytes(&raw, &mut point);
    match rng.below(3) {
        0 => point.push(0),
        1 => {}
        _ => point.extend_from_slice(&[0, 0xff]),
    }
    match rng.below(5) {
        0 => Bound::Unbounded,
        1 | 2 => Bound::Included(point),
        _ => Bound::Excluded(point),
    }
}

fn check_interval(map: &Map, model: &Model, range: &KeyRange) {
    let expected: Vec<_> = encoded(model)
        .into_iter()
        .filter(|(key, _)| range.contains(key))
        .collect();
    let proof: Vec<_> = map
        .interval_proof(range, &MemoryOnly)
        .collect::<Result<_, TrieError>>()
        .unwrap();
    let limits = IntervalLimits {
        max_entries: model.len(),
        max_key_len: Key::MAX_LEN,
    };
    let root = map.root_hash();
    assert_eq!(
        walk_interval(root, range, &proof, limits),
        Ok(expected.clone()),
        "{range:?}"
    );
    if range.is_empty() {
        assert!(proof.is_empty());
    }
    if !expected.is_empty() {
        let tight = IntervalLimits {
            max_entries: expected.len() - 1,
            ..limits
        };
        assert_eq!(
            walk_interval(root, range, &proof, tight),
            Err(ProofError::EntryLimitExceeded)
        );
        // A key bound below the stored keys rejects instead of truncating.
        let short = IntervalLimits {
            max_key_len: 0,
            ..limits
        };
        assert!(walk_interval(root, range, &proof, short).is_err());
    }
    for omitted in 0..proof.len() {
        let mut incomplete = proof.clone();
        incomplete.remove(omitted);
        assert!(walk_interval(root, range, &incomplete, limits).is_err());
    }
    let mut extra = proof.clone();
    extra.push(vec![0xc0]);
    assert!(walk_interval(root, range, &extra, limits).is_err());
}

#[test]
fn random_histories_match_the_model() {
    for seed in 0..24 {
        let mut rng = Rng(seed);
        let mut map = Map::new();
        let mut model = Model::new();
        let mut retained = Vec::new();
        for step in 0..160 {
            let raw = random_raw(&mut rng);
            if rng.below(3) == 0 {
                let (next, old) = map
                    .remove(&ByteKey(raw.clone()), &MemoryOnly, &mut Probe::new())
                    .unwrap();
                assert_eq!(old, model.remove(&raw));
                map = next;
            } else {
                let value = random_value(&mut rng);
                let (next, old) = map
                    .insert(
                        &ByteKey(raw.clone()),
                        value.clone(),
                        &MemoryOnly,
                        &mut Probe::new(),
                    )
                    .unwrap();
                assert_eq!(old, model.insert(raw, value));
                map = next;
            }
            assert_eq!(map.len(), model.len() as u64);
            if step % 20 == 0 {
                check(&map, &model, &mut rng);
                retained.push((map.clone(), model.clone()));
                for _ in 0..4 {
                    let range = KeyRange::new(
                        random_bound(&mut rng, &model),
                        random_bound(&mut rng, &model),
                    );
                    check_interval(&map, &model, &range);
                }
            }
        }
        check(&map, &model, &mut rng);
        // Earlier handles are unaffected by later updates.
        for (handle, snapshot) in &retained {
            check(handle, snapshot, &mut rng);
        }
    }
}

#[test]
fn prefix_runs_select_exactly_the_strings_with_the_prefix() {
    let mut rng = Rng(99);
    let mut map = Map::new();
    let mut model = Model::new();
    for _ in 0..300 {
        let raw = random_raw(&mut rng);
        let value = random_value(&mut rng);
        map = map
            .insert(
                &ByteKey(raw.clone()),
                value.clone(),
                &MemoryOnly,
                &mut Probe::new(),
            )
            .unwrap()
            .0;
        model.insert(raw, value);
    }
    let mut prefixes: Vec<Vec<u8>> = vec![
        vec![],
        vec![0],
        vec![1],
        vec![0xff],
        vec![0xff, 0xff],
        b"a".to_vec(),
        b"a\0".to_vec(),
    ];
    prefixes.extend((0..40).map(|_| random_raw(&mut rng)));
    for prefix in prefixes {
        let mut escaped = Vec::new();
        escape_bytes(&prefix, &mut escaped);
        let range = KeyRange::prefix(&escaped);
        let expected: Vec<_> = model
            .iter()
            .filter(|(key, _)| key.starts_with(&prefix))
            .map(|(key, value)| (enc(key), value.clone()))
            .collect();
        let selected: Vec<_> = encoded(&model)
            .into_iter()
            .filter(|(key, _)| range.contains(key))
            .collect();
        assert_eq!(selected, expected, "prefix {prefix:?}");
        check_interval(&map, &model, &range);
    }
}

#[test]
fn hand_picked_prefix_related_strings() {
    // Raw strings that are prefixes of each other, NUL/0x01/0xff heavy.
    let raws: Vec<Vec<u8>> = vec![
        vec![],
        vec![0],
        vec![0, 0],
        vec![1],
        b"a".to_vec(),
        b"a\0".to_vec(),
        b"a\0\0".to_vec(),
        b"a\0b".to_vec(),
        b"a\x01".to_vec(),
        b"aa".to_vec(),
        b"ab".to_vec(),
        vec![b'a', 0xff],
        vec![0xff],
        vec![0xff; MAX],
        vec![0; MAX],
    ];
    let encodings: Vec<_> = raws.iter().map(|raw| enc(raw)).collect();
    assert!(is_prefix_free(&encodings));
    assert!(!is_prefix_free(&[b"a".to_vec(), b"ab".to_vec()]));
    assert!(!is_prefix_free(&[b"a".to_vec(), b"a".to_vec()]));
    let model: Model = raws
        .iter()
        .map(|raw| (raw.clone(), vec![raw.len() as u8 + 1; 33]))
        .collect();
    let mut roots = Vec::new();
    let mut rng = Rng(5);
    for _ in 0..8 {
        let mut order = raws.clone();
        rng.shuffle(&mut order);
        let map = order.iter().fold(Map::new(), |map, raw| {
            map.insert(
                &ByteKey(raw.clone()),
                model[raw].clone(),
                &MemoryOnly,
                &mut Probe::new(),
            )
            .unwrap()
            .0
        });
        check(&map, &model, &mut rng);
        roots.push(map.root_hash());
    }
    assert!(
        roots.windows(2).all(|pair| pair[0] == pair[1]),
        "insertion order changed the root"
    );
    // Exclusive bounds compare whole encodings: `(enc("a"), enc("aa"))`
    // holds `a\0`, `a\0\0`, `a\0b` and `a\x01`, not `a` or `aa`.
    let map = raws.iter().fold(Map::new(), |map, raw| {
        map.insert(
            &ByteKey(raw.clone()),
            model[raw].clone(),
            &MemoryOnly,
            &mut Probe::new(),
        )
        .unwrap()
        .0
    });
    let range = KeyRange::new(Bound::Excluded(enc(b"a")), Bound::Excluded(enc(b"aa")));
    let proof: Vec<_> = map
        .interval_proof(&range, &MemoryOnly)
        .collect::<Result<_, TrieError>>()
        .unwrap();
    let entries = walk_interval(
        map.root_hash(),
        &range,
        &proof,
        IntervalLimits {
            max_entries: 100,
            max_key_len: Key::MAX_LEN,
        },
    )
    .unwrap();
    let keys: Vec<_> = entries.into_iter().map(|(key, _)| key).collect();
    assert_eq!(
        keys,
        [b"a\0".as_slice(), b"a\0\0", b"a\0b", b"a\x01"]
            .map(enc)
            .to_vec()
    );
}

#[test]
fn temporary_entries_collapse_back_to_the_original_root() {
    let mut rng = Rng(3);
    let mut map = Map::new();
    for _ in 0..40 {
        let raw = random_raw(&mut rng);
        map = map
            .insert(
                &ByteKey(raw),
                random_value(&mut rng),
                &MemoryOnly,
                &mut Probe::new(),
            )
            .unwrap()
            .0;
    }
    let original = map.root_hash();
    let len = map.len();
    let mut added = Vec::new();
    let mut grown = map.clone();
    for _ in 0..60 {
        let raw = random_raw(&mut rng);
        let (next, old) = grown
            .insert(
                &ByteKey(raw.clone()),
                random_value(&mut rng),
                &MemoryOnly,
                &mut Probe::new(),
            )
            .unwrap();
        grown = next;
        if old.is_none() {
            added.push(raw);
        }
    }
    rng.shuffle(&mut added);
    for raw in added {
        grown = grown
            .remove(&ByteKey(raw), &MemoryOnly, &mut Probe::new())
            .unwrap()
            .0;
    }
    // Overwritten originals changed values; restore by re-reading the
    // original handle, then compare.
    let mut restored = grown;
    for entry in map.iter(&MemoryOnly) {
        let (key, value) = entry.unwrap();
        restored = restored
            .insert(&key, value, &MemoryOnly, &mut Probe::new())
            .unwrap()
            .0;
    }
    assert_eq!(restored.len(), len);
    assert_eq!(restored.root_hash(), original);
}

#[test]
fn stored_maps_reopen_walk_and_prove_identically() {
    let mut rng = Rng(11);
    let mut map = Map::new();
    let mut model = Model::new();
    for _ in 0..200 {
        let raw = random_raw(&mut rng);
        let value = random_value(&mut rng);
        map = map
            .insert(
                &ByteKey(raw.clone()),
                value.clone(),
                &MemoryOnly,
                &mut Probe::new(),
            )
            .unwrap()
            .0;
        model.insert(raw, value);
    }
    let mut staged = Staged::new();
    map.stage(&mut staged);
    let mut store = MemoryNodeStore::new();
    store.commit(&staged);
    assert_eq!(
        staged
            .objects()
            .iter()
            .map(|(id, _)| *id)
            .collect::<std::collections::BTreeSet<_>>(),
        reference::objects(&encoded(&model))
    );
    let reopened = Map::from_root(map.root_hash(), map.len()).unwrap();
    let all: Vec<_> = reopened
        .iter(&store)
        .map(|entry| entry.map(|(key, value)| (key.0, value)))
        .collect::<Result<_, TrieError>>()
        .unwrap();
    assert_eq!(all, model.clone().into_iter().collect::<Vec<_>>());
    let mut leaves = 0;
    reopened
        .walk_stored::<_, TrieError>(&store, &mut |_, _| true, &mut |_| {
            leaves += 1;
            Ok(())
        })
        .unwrap();
    assert_eq!(leaves, model.len());
    for raw in model.keys().take(30) {
        let proof = reopened
            .prove(&ByteKey(raw.clone()), &store, &mut Probe::new())
            .unwrap();
        assert_eq!(
            walk(map.root_hash(), &enc(raw), &proof),
            Ok(Some(model[raw].clone()))
        );
    }
    // Updates over stored nodes, then removing everything, reach the empty map.
    let mut shrinking = reopened;
    for raw in model.keys() {
        shrinking = shrinking
            .remove(&ByteKey(raw.clone()), &store, &mut Probe::new())
            .unwrap()
            .0;
    }
    assert!(shrinking.is_empty());
    assert_eq!(shrinking.root_hash(), arkiv_db_trie::EMPTY_ROOT_HASH);
}

/// A key codec that breaks the prefix-free contract on purpose: raw bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Loose(Vec<u8>);

impl TrieKey for Loose {
    const MAX_LEN: usize = 8;
    fn encode(&self) -> Vec<u8> {
        self.0.clone()
    }
    fn decode(bytes: &[u8]) -> Result<Self, arkiv_db_trie::CodecError> {
        Ok(Self(bytes.to_vec()))
    }
}

#[test]
fn prefix_violations_are_rejected_and_prefix_queries_are_absent() {
    let mut map = MerkleMap::<Loose, Vec<u8>>::new();
    for key in [&b"abcd"[..], b"abce", b"b"] {
        map = map
            .insert(
                &Loose(key.to_vec()),
                vec![1; 33],
                &MemoryOnly,
                &mut Probe::new(),
            )
            .unwrap()
            .0;
    }
    let root = map.root_hash();
    // Shorter than a stored key (inside an extension, at a branch, inside a
    // leaf path) or extending a stored key: rejected, map unchanged.
    for key in [&b"ab"[..], b"abc", b"abcde", b"ba", b""] {
        assert_eq!(
            map.insert(
                &Loose(key.to_vec()),
                vec![2],
                &MemoryOnly,
                &mut Probe::new()
            )
            .unwrap_err(),
            TrieError::KeyNotPrefixFree,
            "{key:?}"
        );
        // Lookups, removals and proofs answer absence.
        assert_eq!(
            map.get(&Loose(key.to_vec()), &MemoryOnly, &mut Probe::new()),
            Ok(None)
        );
        let (same, old) = map
            .remove(&Loose(key.to_vec()), &MemoryOnly, &mut Probe::new())
            .unwrap();
        assert_eq!((same.root_hash(), old), (root, None));
        let proof = map
            .prove(&Loose(key.to_vec()), &MemoryOnly, &mut Probe::new())
            .unwrap();
        assert_eq!(walk(root, key, &proof), Ok(None), "{key:?}");
    }
    assert_eq!(map.root_hash(), root);
    // The key bound is enforced by the encoder contract.
    let result = std::panic::catch_unwind(|| {
        MerkleMap::<Loose, Vec<u8>>::new()
            .insert(&Loose(vec![0; 9]), vec![1], &MemoryOnly, &mut Probe::new())
            .map(|_| ())
    });
    assert!(result.is_err(), "an encoding over MAX_LEN must panic");
}

#[test]
fn empty_key_and_maximum_length_keys() {
    let empty = ByteKey::<MAX>(vec![]);
    assert_eq!(empty.encode(), [0x00]);
    let longest = ByteKey::<MAX>(vec![0; MAX]);
    assert_eq!(longest.encode().len(), Key::MAX_LEN);
    assert_eq!(Key::decode(&longest.encode()), Ok(longest.clone()));
    assert!(Key::decode(&enc(&[0; MAX + 1])).is_err());
    assert!(Key::decode(&[0x61]).is_err(), "missing terminator");
    assert!(Key::decode(&[0x61, 0x00, 0x00]).is_err(), "trailing bytes");
    assert!(Key::decode(&[0x01, 0x05, 0x00]).is_err(), "bad escape");
    let map = Map::new()
        .insert(&empty, vec![7], &MemoryOnly, &mut Probe::new())
        .unwrap()
        .0
        .insert(&longest, vec![8], &MemoryOnly, &mut Probe::new())
        .unwrap()
        .0;
    let all: Vec<_> = map.iter(&MemoryOnly).map(|e| e.unwrap()).collect();
    assert_eq!(all, vec![(empty, vec![7]), (longest, vec![8])]);
}
