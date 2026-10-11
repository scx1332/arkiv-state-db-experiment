//! The order-preserving fixed-width key encoders of `arkiv_db_trie::order`.
//!
//! Establishes, on random inputs compared with Rust's own bytewise slice
//! ordering and lexicographic tuple ordering (the independent model): the
//! padded-with-length encoding preserves the order of byte strings up to a
//! maximum length, including empty strings, strings differing only in
//! trailing zero bytes, strings that are prefixes of others and strings of
//! the maximum length, and is injective and decodable; the composite
//! `(name ≤ 20 bytes, tag, presence, value ≤ 64 bytes)` encoding (88 bytes,
//! the building blocks of the engine's planned term key) preserves tuple
//! order; and a map keyed by such an encoding iterates in tuple order.

mod support;

use std::cmp::Ordering;

use arkiv_db_trie::{
    CodecError, MemoryOnly, MerkleMap, Probe, TrieKey,
    order::{KeyReader, KeyWriter, decode_padded, encode_padded, padded_width},
};
use support::Rng;

/// A random string biased toward the cases that break naive encodings.
fn tricky(rng: &mut Rng, max: usize, pool: &[Vec<u8>]) -> Vec<u8> {
    match rng.below(6) {
        0 => Vec::new(),
        1 => {
            // A pooled string with zero bytes appended.
            let mut value = pool[rng.below(pool.len())].clone();
            let room = max - value.len().min(max);
            value.truncate(max);
            let zeros = rng.below(room + 1);
            value.resize(value.len() + zeros, 0);
            value
        }
        2 => {
            // A prefix of a pooled string.
            let value = &pool[rng.below(pool.len())];
            value[..rng.below(value.len() + 1)].to_vec()
        }
        3 => rng.bytes(max),
        4 => {
            let len = rng.below(max + 1);
            (0..len).map(|_| [0x00, 0x01, 0xff][rng.below(3)]).collect()
        }
        _ => {
            let len = rng.below(max + 1);
            rng.bytes(len)
        }
    }
}

fn padded(value: &[u8], max: u8) -> Vec<u8> {
    let mut out = Vec::new();
    encode_padded(value, max, &mut out).unwrap();
    out
}

#[test]
fn padded_encoding_preserves_bytewise_order() {
    let mut rng = Rng(1);
    for max in [0u8, 1, 2, 3, 20, 64] {
        let mut pool: Vec<Vec<u8>> = vec![Vec::new(), vec![0; max as usize]];
        if max > 0 {
            pool.push(vec![0]);
        }
        for _ in 0..200 {
            let next = tricky(&mut rng, max as usize, &pool);
            pool.push(next);
        }
        for _ in 0..20_000 {
            let left = &pool[rng.below(pool.len())];
            let right = &pool[rng.below(pool.len())];
            let (left_code, right_code) = (padded(left, max), padded(right, max));
            assert_eq!(left_code.len(), padded_width(max));
            assert_eq!(
                left_code.cmp(&right_code),
                left.cmp(right),
                "{left:?} vs {right:?} at max {max}"
            );
            assert_eq!(decode_padded(&left_code, max), Ok(left.as_slice()));
        }
    }
}

#[test]
fn padded_encoding_edge_cases() {
    // Trailing zeros are told apart by the length byte alone.
    assert!(padded(b"a", 4) < padded(b"a\0", 4));
    assert!(padded(b"a\0", 4) < padded(b"a\0\0", 4));
    assert!(padded(b"", 4) < padded(b"\0", 4));
    // A proper prefix sorts first even against a zero continuation.
    assert!(padded(b"ab", 4) < padded(b"ab\0\0", 4));
    assert!(padded(b"ab\0\0", 4) < padded(b"ab\x01", 4));
    // Maximum length and overflow.
    assert_eq!(
        padded(&[0xff; 64], 64),
        [[0xff; 64].as_slice(), &[64]].concat()
    );
    let mut out = vec![9];
    assert!(encode_padded(&[0; 65], 64, &mut out).is_err());
    assert_eq!(out, [9]);
    // Every width is fixed; a wrong-width, overlong or dirty field is rejected.
    assert!(decode_padded(&[0; 64], 64).is_err());
    assert!(decode_padded(&[[0; 64].as_slice(), &[65]].concat(), 64).is_err());
    assert!(decode_padded(&[[1, 0, 1].as_slice(), &[1]].concat(), 3).is_err());
}

/// The planned term-key building blocks: name, type tag, presence, value.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Term {
    name: Vec<u8>,
    tag: u8,
    presence: u8,
    value: Vec<u8>,
}

impl TrieKey for Term {
    const MAX_LEN: usize = 21 + 1 + 1 + 65;

    fn encode(&self) -> Vec<u8> {
        let mut key = KeyWriter::new();
        key.padded(&self.name, 20)
            .expect("names are at most 20 bytes")
            .byte(self.tag)
            .byte(self.presence)
            .padded(&self.value, 64)
            .expect("values are at most 64 bytes");
        key.finish()
    }

    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        let mut reader = KeyReader::new(bytes);
        let name = reader.padded(20)?.to_vec();
        let tag = reader.byte()?;
        let presence = reader.byte()?;
        let value = reader.padded(64)?.to_vec();
        reader.finish()?;
        Ok(Self {
            name,
            tag,
            presence,
            value,
        })
    }
}

fn random_term(rng: &mut Rng, names: &[Vec<u8>], values: &[Vec<u8>]) -> Term {
    let presence = rng.below(2) as u8;
    Term {
        name: names[rng.below(names.len())].clone(),
        tag: [1u8, 2, 3, 4, 5][rng.below(5)],
        presence,
        // An existence term carries no value bytes.
        value: if presence == 0 {
            Vec::new()
        } else {
            values[rng.below(values.len())].clone()
        },
    }
}

fn pools(rng: &mut Rng) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
    let mut names = vec![b"a".to_vec(), b"ab".to_vec(), b"b".to_vec()];
    for _ in 0..30 {
        let next = tricky(rng, 20, &names);
        names.push(next);
    }
    let mut values = vec![Vec::new(), vec![0], vec![0x80, 0, 0, 0, 0, 0, 0, 1]];
    for _ in 0..60 {
        let next = tricky(rng, 64, &values);
        values.push(next);
    }
    (names, values)
}

#[test]
fn composite_encoding_preserves_tuple_order() {
    let mut rng = Rng(2);
    let (names, values) = pools(&mut rng);
    for _ in 0..50_000 {
        let left = random_term(&mut rng, &names, &values);
        let right = random_term(&mut rng, &names, &values);
        let (left_key, right_key) = (left.encode(), right.encode());
        assert_eq!(left_key.len(), Term::MAX_LEN);
        assert_eq!(
            left_key.cmp(&right_key),
            left.cmp(&right),
            "{left:?} vs {right:?}"
        );
        assert_eq!(Term::decode(&left_key), Ok(left.clone()));
        if left_key.cmp(&right_key) == Ordering::Equal {
            assert_eq!(left, right);
        }
    }
    // Existence (presence 0) sorts before every value of its name and tag,
    // including the empty value.
    let existence = Term {
        name: b"n".to_vec(),
        tag: 4,
        presence: 0,
        value: Vec::new(),
    };
    let empty_value = Term {
        presence: 1,
        ..existence.clone()
    };
    assert!(existence.encode() < empty_value.encode());
}

#[test]
fn a_map_over_composite_keys_iterates_in_tuple_order() {
    let mut rng = Rng(3);
    let (names, values) = pools(&mut rng);
    let mut map: MerkleMap<Term, Vec<u8>> = MerkleMap::new();
    let mut model = std::collections::BTreeMap::new();
    for _ in 0..400 {
        let term = random_term(&mut rng, &names, &values);
        map = map
            .insert(&term, vec![1], &MemoryOnly, &mut Probe::new())
            .unwrap()
            .0;
        model.insert(term, vec![1u8]);
    }
    let keys: Vec<Term> = map
        .iter(&MemoryOnly)
        .map(|entry| entry.unwrap().0)
        .collect();
    assert_eq!(keys, model.keys().cloned().collect::<Vec<_>>());
    // A run of one name and tag starts at the existence term.
    let name = model.keys().next().unwrap().name.clone();
    let start = Term {
        name: name.clone(),
        tag: 0,
        presence: 0,
        value: Vec::new(),
    };
    let run: Vec<Term> = map
        .range_from(&start, &MemoryOnly)
        .map(|entry| entry.unwrap().0)
        .take_while(|term| term.name == name)
        .collect();
    let expected: Vec<Term> = model
        .range(start..)
        .map(|(term, _)| term.clone())
        .take_while(|term| term.name == name)
        .collect();
    assert_eq!(run, expected);
    assert!(!run.is_empty());
}
