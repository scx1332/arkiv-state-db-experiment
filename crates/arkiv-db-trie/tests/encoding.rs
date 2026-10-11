//! The order-preserving, prefix-free key encoders of `arkiv_db_trie::order`.
//!
//! Establishes, on random inputs compared with Rust's own bytewise slice
//! ordering and lexicographic tuple ordering (the independent model): the
//! escaped (`encode_bytes`) and shifted (`encode_shifted`) encodings
//! preserve the order of byte strings, including empty strings, strings
//! differing only in trailing zero bytes, strings that are prefixes of
//! others and `00`/`01`/`ff`-heavy strings; they are prefix-free, injective
//! and decodable, and their unterminated forms select exactly the strings
//! with a prefix; the composite `(name ≤ 20 bytes, tag, presence, value ≤
//! 64 bytes)` encoding (the building blocks of the engine's term key)
//! preserves tuple order and is prefix-free; and a map keyed by such an
//! encoding iterates in tuple order.

mod support;

use std::cmp::Ordering;

use arkiv_db_trie::{
    CodecError, KeyRange, MemoryOnly, MerkleMap, Probe, TrieKey, is_prefix_free,
    order::{
        KeyReader, KeyWriter, decode_bytes, decode_shifted, encode_bytes, encode_shifted,
        escape_bytes, shift_bytes,
    },
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

fn escaped(value: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    encode_bytes(value, &mut out);
    out
}

fn shifted(value: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    encode_shifted(value, &mut out).ok()?;
    Some(out)
}

#[test]
fn byte_string_encodings_preserve_order_and_are_prefix_free() {
    let mut rng = Rng(1);
    for max in [0usize, 1, 2, 3, 20, 64] {
        let mut pool: Vec<Vec<u8>> = vec![Vec::new(), vec![0; max]];
        if max > 0 {
            pool.extend([vec![0], vec![1], vec![0xff]]);
        }
        for _ in 0..200 {
            let next = tricky(&mut rng, max, &pool);
            pool.push(next);
        }
        let mut codes: Vec<Vec<u8>> = pool.iter().map(|value| escaped(value)).collect();
        codes.sort();
        codes.dedup();
        assert!(is_prefix_free(&codes), "escaped at max {max}");
        let mut codes: Vec<Vec<u8>> = pool.iter().filter_map(|value| shifted(value)).collect();
        codes.sort();
        codes.dedup();
        assert!(is_prefix_free(&codes), "shifted at max {max}");
        for _ in 0..20_000 {
            let left = &pool[rng.below(pool.len())];
            let right = &pool[rng.below(pool.len())];
            let (left_code, right_code) = (escaped(left), escaped(right));
            assert_eq!(
                left_code.cmp(&right_code),
                left.cmp(right),
                "{left:?} vs {right:?}"
            );
            assert_eq!(decode_bytes(&left_code, max), Ok((left.clone(), &[][..])));
            if let (Some(left_code), Some(right_code)) = (shifted(left), shifted(right)) {
                assert_eq!(left_code.cmp(&right_code), left.cmp(right));
                assert_eq!(left_code.len(), left.len() + 1);
                assert_eq!(decode_shifted(&left_code, max), Ok((left.clone(), &[][..])));
            }
            // The unterminated form selects exactly the extensions.
            let mut prefix = Vec::new();
            escape_bytes(left, &mut prefix);
            assert_eq!(
                KeyRange::prefix(&prefix).contains(&escaped(right)),
                right.starts_with(left),
                "{left:?} prefix of {right:?}"
            );
            let mut prefix = Vec::new();
            if shift_bytes(left, &mut prefix).is_ok()
                && let Some(right_code) = shifted(right)
            {
                assert_eq!(
                    KeyRange::prefix(&prefix).contains(&right_code),
                    right.starts_with(left)
                );
            }
        }
    }
}

#[test]
fn byte_string_encoding_edge_cases() {
    // Trailing zeros and proper prefixes.
    assert!(escaped(b"a") < escaped(b"a\0"));
    assert!(escaped(b"a\0") < escaped(b"a\0\0"));
    assert!(escaped(b"") < escaped(b"\0"));
    assert!(escaped(b"ab\0\0") < escaped(b"ab\x01"));
    assert!(!escaped(b"a\0").starts_with(&escaped(b"a")));
    // Worst-case escaping doubles the length; shifting never does.
    assert_eq!(escaped(&[0; 64]).len(), 129);
    assert_eq!(shifted(&[0; 64]).unwrap().len(), 65);
    assert_eq!(shifted(&[0xff]), None);
    // Noncanonical and over-long fields are rejected.
    assert!(decode_bytes(&escaped(&[7; 65]), 64).is_err());
    assert!(decode_bytes(&[0x01, 0x00], 4).is_err());
    assert!(decode_shifted(&shifted(&[7; 65]).unwrap(), 64).is_err());
    assert!(decode_shifted(&[0x05], 4).is_err());
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
    const MAX_LEN: usize = 41 + 1 + 1 + 129;

    fn encode(&self) -> Vec<u8> {
        let mut key = KeyWriter::new();
        key.bytes(&self.name)
            .byte(self.tag)
            .byte(self.presence)
            .bytes(&self.value);
        key.finish()
    }

    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        let mut reader = KeyReader::new(bytes);
        let name = reader.bytes(20)?;
        let tag = reader.byte()?;
        let presence = reader.byte()?;
        let value = reader.bytes(64)?;
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
    let mut keys = Vec::new();
    for _ in 0..50_000 {
        let left = random_term(&mut rng, &names, &values);
        let right = random_term(&mut rng, &names, &values);
        let (left_key, right_key) = (left.encode(), right.encode());
        assert!(left_key.len() <= Term::MAX_LEN);
        keys.push(left_key.clone());
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
    keys.sort();
    keys.dedup();
    assert!(is_prefix_free(&keys));
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
