//! Streaming construction must preserve both the root and the exact object set.

mod support;

use std::collections::BTreeMap;

use arkiv_db_trie::{
    CodecError, EMPTY_ROOT_HASH, MemoryNodeStore, MemoryOnly, MerkleMap, ObjectId, ObjectSink,
    Probe, SortedTrieBuilder, SortedTrieError, StageError, Staged, TrieKey, TryObjectSink,
    order::{ByteKey, encode_bytes},
    walk,
};
use support::Rng;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Raw(Vec<u8>);

impl TrieKey for Raw {
    const MAX_LEN: usize = 2048;

    fn encode(&self) -> Vec<u8> {
        self.0.clone()
    }

    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        Ok(Self(bytes.to_vec()))
    }
}

#[derive(Default)]
struct Sink {
    staged: Staged,
    calls: usize,
    fail_at: Option<usize>,
}

impl TryObjectSink for Sink {
    type Error = StageError;

    fn record(&mut self, id: ObjectId, bytes: &[u8]) -> Result<(), Self::Error> {
        self.calls += 1;
        if self.fail_at == Some(self.calls) {
            return Err(StageError::VisitLimit);
        }
        self.staged.record(id, bytes);
        Ok(())
    }
}

fn check(model: &BTreeMap<Vec<u8>, Vec<u8>>) -> Sink {
    let mut oracle = MerkleMap::<Raw, Vec<u8>>::new();
    // A different insertion order also exercises canonical shape.
    for (key, value) in model.iter().rev() {
        oracle = oracle
            .insert(
                &Raw(key.clone()),
                value.clone(),
                &MemoryOnly,
                &mut Probe::new(),
            )
            .unwrap()
            .0;
    }
    let mut expected = Staged::new();
    oracle.stage(&mut expected);

    let mut builder = SortedTrieBuilder::<Raw>::new();
    let mut actual = Sink::default();
    for (key, value) in model {
        builder.push(key, value, &mut actual).unwrap();
    }
    assert_eq!(builder.len(), model.len() as u64);
    let (root, len) = builder.finish(&mut actual).unwrap();
    assert_eq!((root, len), (oracle.root_hash(), oracle.len()));
    assert_eq!(
        actual
            .staged
            .objects()
            .iter()
            .cloned()
            .collect::<BTreeMap<_, _>>(),
        expected
            .into_objects()
            .into_iter()
            .collect::<BTreeMap<_, _>>(),
        "streaming emits exactly the persistent trie's object set"
    );

    let mut store = MemoryNodeStore::new();
    store.commit(&actual.staged);
    let reopened = MerkleMap::<Raw, Vec<u8>>::from_root(root, len).unwrap();
    let recovered = reopened
        .iter(&store)
        .map(|entry| entry.map(|(key, value)| (key.0, value)))
        .collect::<Result<BTreeMap<_, _>, _>>()
        .unwrap();
    assert_eq!(recovered, *model);
    for (key, value) in model.iter().step_by((model.len() / 8).max(1)) {
        let proof = reopened
            .prove(&Raw(key.clone()), &store, &mut Probe::new())
            .unwrap();
        assert_eq!(walk(root, key, &proof), Ok(Some(value.clone())));
    }
    actual
}

#[test]
fn empty_single_inline_root_and_inline_children() {
    let mut sink = Sink::default();
    let empty = SortedTrieBuilder::<Raw>::new();
    assert!(empty.is_empty());
    assert_eq!(empty.finish(&mut sink), Ok((EMPTY_ROOT_HASH, 0)));
    assert_eq!(sink.calls, 0);

    // Both an empty encoded key and an ordinary key are valid singleton maps.
    for key in [vec![], vec![0x12]] {
        let sink = check(&BTreeMap::from([(key, vec![1])]));
        assert_eq!(sink.staged.len(), 1);
        assert!(
            sink.staged.objects()[0].1.len() < 32,
            "inline roots are emitted"
        );
    }
    // Tiny leaves, an inline branch, and an extension above that branch.
    let sink = check(&BTreeMap::from([
        (vec![0x12, 0x30], vec![1]),
        (vec![0x12, 0x31], vec![2]),
    ]));
    assert_eq!(
        sink.staged.len(),
        1,
        "inline descendants are not separate objects"
    );
}

#[test]
fn variable_keys_long_extensions_and_rlp_boundaries() {
    let sizes = [1, 2, 25, 26, 27, 28, 29, 30, 31, 32, 33, 55, 56, 80, 256];
    let mut model = BTreeMap::new();
    for (index, size) in sizes.into_iter().enumerate() {
        for length in [0, 1, 31, 32, 33, 127, 512, 900] {
            let mut raw = vec![0x56; length];
            raw.push(index as u8);
            let mut key = Vec::new();
            encode_bytes(&raw, &mut key);
            model.insert(key, vec![index as u8; size]);
        }
    }
    // Force all sixteen slots of branches to fill.
    for first in 0..=255 {
        model.insert(ByteKey::<16>(vec![first, 0x55]).encode(), vec![first; 33]);
    }
    check(&model);
}

#[test]
fn random_sorted_streams_match_merkle_map_objects() {
    for seed in 0..24 {
        let mut rng = Rng(seed);
        let mut model = BTreeMap::new();
        for index in 0..300 {
            let len = rng.below(80);
            let mut raw = Vec::new();
            for _ in 0..len {
                raw.push(rng.below(if seed % 2 == 0 { 3 } else { 256 }) as u8);
            }
            let mut key = Vec::new();
            encode_bytes(&raw, &mut key);
            let value_len = if index % 2 == 0 { 1 } else { 1 + rng.below(90) };
            model.insert(key, vec![rng.below(256) as u8; value_len]);
        }
        check(&model);
    }
}

#[test]
fn input_errors_preserve_frontier_and_allow_corrected_input() {
    let mut builder = SortedTrieBuilder::<Raw>::new();
    let mut sink = Sink::default();
    builder.push(&[0x12, 0x34], &[1; 40], &mut sink).unwrap();
    builder.push(&[0x12, 0x35], &[2; 40], &mut sink).unwrap();
    let calls = sink.calls;
    for (key, value, error) in [
        (vec![0x12, 0x35], vec![3], SortedTrieError::OutOfOrder),
        (vec![0x12, 0x30], vec![3], SortedTrieError::OutOfOrder),
        (vec![0x12], vec![3], SortedTrieError::KeyNotPrefixFree),
        (
            vec![0x12, 0x35, 0x01],
            vec![3],
            SortedTrieError::KeyNotPrefixFree,
        ),
        (vec![0x13], vec![], SortedTrieError::EmptyValue),
        (
            vec![0x13; Raw::MAX_LEN + 1],
            vec![3],
            SortedTrieError::KeyTooLong {
                len: Raw::MAX_LEN + 1,
                max: Raw::MAX_LEN,
            },
        ),
    ] {
        assert_eq!(builder.push(&key, &value, &mut sink), Err(error));
        assert_eq!(builder.len(), 2);
        assert_eq!(sink.calls, calls, "invalid input must not emit anything");
    }
    builder.push(&[0x13], &[3; 40], &mut sink).unwrap();
    let (root, len) = builder.finish(&mut sink).unwrap();
    let model = BTreeMap::from([
        (vec![0x12, 0x34], vec![1; 40]),
        (vec![0x12, 0x35], vec![2; 40]),
        (vec![0x13], vec![3; 40]),
    ]);
    let expected = check(&model);
    let mut store = MemoryNodeStore::new();
    store.commit(&expected.staged);
    let reopened = MerkleMap::<Raw, Vec<u8>>::from_root(root, len).unwrap();
    let recovered = reopened
        .iter(&store)
        .map(|entry| entry.map(|(key, value)| (key.0, value)))
        .collect::<Result<BTreeMap<_, _>, _>>()
        .unwrap();
    assert_eq!(recovered, model);
}

#[test]
fn sink_errors_abort_immediately_and_poison_the_builder() {
    // The third push completes a leaf, a branch and an extension. Fail each
    // emission in turn and ensure no subsequent output or root can escape.
    for fail_at in 2..=4 {
        let mut builder = SortedTrieBuilder::<Raw>::new();
        let mut sink = Sink {
            fail_at: Some(fail_at),
            ..Sink::default()
        };
        builder.push(&[0x12, 0x30], &[1; 40], &mut sink).unwrap();
        builder.push(&[0x12, 0x31], &[2; 40], &mut sink).unwrap();
        assert_eq!(
            builder.push(&[0x22], &[3; 40], &mut sink),
            Err(SortedTrieError::Sink(StageError::VisitLimit))
        );
        assert_eq!(sink.calls, fail_at);
        assert_eq!(
            builder.push(&[0x23], &[3], &mut sink),
            Err(SortedTrieError::Failed)
        );
        assert_eq!(builder.finish(&mut sink), Err(SortedTrieError::Failed));
        assert_eq!(sink.calls, fail_at);
    }
}

#[test]
fn finish_propagates_inline_and_hashed_root_sink_errors() {
    for size in [1, 40] {
        let mut builder = SortedTrieBuilder::<Raw>::new();
        let mut sink = Sink {
            fail_at: Some(1),
            ..Sink::default()
        };
        builder.push(&[0x12], &vec![1; size], &mut sink).unwrap();
        assert_eq!(
            builder.finish(&mut sink),
            Err(SortedTrieError::Sink(StageError::VisitLimit))
        );
        assert_eq!(sink.calls, 1);
    }
}
