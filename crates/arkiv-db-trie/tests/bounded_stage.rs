//! Bounded preparation must reject before encoding, and abort synchronously.

use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use arkiv_db_trie::{
    CodecError, MemoryNodeStore, MemoryOnly, MerkleMap, ObjectId, ObjectSink, Probe, StageBudget,
    StageError, Staged, TrieValue, TryObjectSink,
};

#[derive(Clone)]
struct Measured {
    len: usize,
    calls: Arc<AtomicUsize>,
    allowed: Arc<AtomicBool>,
}

impl Measured {
    fn new(len: usize) -> Self {
        Self {
            len,
            calls: Arc::new(AtomicUsize::new(0)),
            allowed: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl TrieValue for Measured {
    fn encode(&self) -> Vec<u8> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        assert!(
            self.allowed.load(Ordering::Relaxed),
            "encoded before preflight"
        );
        vec![0x81; self.len]
    }
    fn encodes_empty(&self) -> bool {
        false
    }
    fn decode(_: &[u8]) -> Result<Self, CodecError> {
        Err(CodecError::new("test-only value"))
    }
}

fn one<V: TrieValue>(value: V) -> MerkleMap<[u8; 1], V> {
    MerkleMap::new()
        .insert(&[0x12], value, &MemoryOnly, &mut Probe::new())
        .unwrap()
        .0
}

fn measured_size(value: &Measured, _: &mut StageBudget) -> Result<usize, StageError> {
    Ok(value.len)
}

// The callback receives the map's exact TrieValue type, not a slice.
#[allow(clippy::ptr_arg)]
fn byte_size(value: &Vec<u8>, _: &mut StageBudget) -> Result<usize, StageError> {
    Ok(value.len())
}

#[derive(Debug, PartialEq, Eq)]
enum Error {
    Stage(StageError),
    Full,
    Nested,
}

impl From<StageError> for Error {
    fn from(value: StageError) -> Self {
        Self::Stage(value)
    }
}

struct Sink {
    objects: Staged,
    calls: usize,
    allowed_calls: usize,
}

impl Sink {
    fn new(allowed_calls: usize) -> Self {
        Self {
            objects: Staged::new(),
            calls: 0,
            allowed_calls,
        }
    }
}

impl TryObjectSink for Sink {
    type Error = Error;
    fn record(&mut self, id: ObjectId, bytes: &[u8]) -> Result<(), Error> {
        self.calls += 1;
        if self.calls > self.allowed_calls {
            return Err(Error::Full);
        }
        self.objects.record(id, bytes);
        Ok(())
    }
}

#[test]
fn oversize_overflow_and_zero_visit_budget_never_encode() {
    for len in [1024, usize::MAX] {
        let value = Measured::new(len);
        let map = one(value.clone());
        let error = map
            .preflight_with(1024, &mut StageBudget::new(1), &mut measured_size)
            .unwrap_err();
        if len == usize::MAX {
            assert_eq!(error, StageError::SizeOverflow);
        } else {
            assert!(
                matches!(error, StageError::ObjectTooLarge { size, limit: 1024 } if size > 1024)
            );
        }
        assert_eq!(value.calls.load(Ordering::Relaxed), 0);
        assert_eq!(
            map.preflight_with(usize::MAX, &mut StageBudget::new(0), &mut measured_size),
            Err(StageError::VisitLimit)
        );
        let mut sink = Sink::new(usize::MAX);
        assert_eq!(
            map.try_stage_with(&mut sink, &mut StageBudget::new(0), &mut |_, _, _| Ok(())),
            Err(Error::Stage(StageError::VisitLimit))
        );
        assert_eq!(sink.calls, 0);
        assert_eq!(value.calls.load(Ordering::Relaxed), 0);
    }
}

#[test]
fn already_hashed_new_nodes_are_still_preflighted() {
    let value = Measured::new(512);
    value.allowed.store(true, Ordering::Relaxed);
    let map = one(value.clone());
    let _ = map.root_hash();
    value.calls.store(0, Ordering::Relaxed);
    value.allowed.store(false, Ordering::Relaxed);
    assert!(matches!(
        map.preflight_with(512, &mut StageBudget::new(1), &mut measured_size),
        Err(StageError::ObjectTooLarge { .. })
    ));
    assert_eq!(value.calls.load(Ordering::Relaxed), 0);
}

fn leaf_boundaries<const N: usize>() {
    for len in [1, 2, 26, 27, 28, 31, 32, 54, 55, 56, 254, 255, 256, 65535] {
        // A one-byte value >= 0x80 uses the full string framing bound.
        let map: MerkleMap<[u8; N], Vec<u8>> = MerkleMap::new()
            .insert(&[0x12; N], vec![0x81; len], &MemoryOnly, &mut Probe::new())
            .unwrap()
            .0;
        let mut reference = Staged::new();
        map.stage(&mut reference);
        let size = reference.objects()[0].1.len();
        map.preflight_with(size, &mut StageBudget::new(1), &mut byte_size)
            .unwrap();
        assert_eq!(
            map.preflight_with(size - 1, &mut StageBudget::new(1), &mut byte_size),
            Err(StageError::ObjectTooLarge {
                size,
                limit: size - 1
            })
        );
    }
}

#[test]
fn full_leaf_rlp_and_path_overhead_is_counted_at_boundaries() {
    leaf_boundaries::<1>();
    leaf_boundaries::<32>();
    leaf_boundaries::<55>();
    leaf_boundaries::<97>();
    // Length-only bounds intentionally allow conservative rejection of 01.
    let map = one(vec![1]);
    let mut actual = Staged::new();
    map.stage(&mut actual);
    let size = actual.objects()[0].1.len();
    assert!(matches!(
        map.preflight_with(size, &mut StageBudget::new(1), &mut byte_size),
        Err(StageError::ObjectTooLarge { .. })
    ));
    map.preflight_with(size + 1, &mut StageBudget::new(1), &mut byte_size)
        .unwrap();
}

#[test]
fn visit_failure_stops_preflight_before_the_next_leaf_callback() {
    let value = Measured::new(8);
    let mut map: MerkleMap<[u8; 1], Measured> = MerkleMap::new();
    for key in [0x10, 0x20, 0x30] {
        map = map
            .insert(&[key], value.clone(), &MemoryOnly, &mut Probe::new())
            .unwrap()
            .0;
    }
    let mut budget = StageBudget::new(2); // Branch, then only the first leaf.
    let mut callbacks = 0;
    assert_eq!(
        map.preflight_with(1024, &mut budget, &mut |v, _| {
            callbacks += 1;
            Ok(v.len)
        }),
        Err(StageError::VisitLimit)
    );
    assert_eq!(callbacks, 1);
    assert_eq!(budget.visited(), 2);
    assert_eq!(budget.remaining(), 0);
    assert_eq!(value.calls.load(Ordering::Relaxed), 0);
}

#[derive(Clone)]
struct Nested<V: TrieValue>(MerkleMap<[u8; 1], V>);

impl<V: TrieValue> TrieValue for Nested<V> {
    fn encode(&self) -> Vec<u8> {
        let mut bytes = self.0.root_hash().to_vec();
        bytes.extend_from_slice(&self.0.len().to_be_bytes());
        bytes
    }
    fn encodes_empty(&self) -> bool {
        false
    }
    fn decode(_: &[u8]) -> Result<Self, CodecError> {
        Err(CodecError::new("test-only nested value"))
    }
    fn stage_nested(&self, sink: &mut dyn ObjectSink) {
        self.0.stage(sink);
    }
}

#[test]
fn nested_preflight_shares_the_budget_and_precedes_indirect_hashing() {
    let value = Measured::new(1024);
    let outer = one(Nested(one(value.clone())));
    for (limit, expected) in [
        (1, StageError::VisitLimit),
        (
            2,
            StageError::ObjectTooLarge {
                size: 1033,
                limit: 1024,
            },
        ),
    ] {
        let mut budget = StageBudget::new(limit);
        let result = outer.preflight_with(1024, &mut budget, &mut |nested, budget| {
            nested.0.preflight_with(1024, budget, &mut measured_size)?;
            Ok(40)
        });
        assert_eq!(result, Err(expected));
        assert_eq!(budget.visited(), limit);
        assert_eq!(value.calls.load(Ordering::Relaxed), 0);
    }
}

#[test]
fn sink_and_nested_errors_stop_at_the_first_failure() {
    let mut map: MerkleMap<[u8; 1], Vec<u8>> = MerkleMap::new();
    for key in [0x10, 0x20, 0x30] {
        map = map
            .insert(&[key], vec![0x81; 64], &MemoryOnly, &mut Probe::new())
            .unwrap()
            .0;
    }
    map.preflight_with(1024, &mut StageBudget::new(4), &mut byte_size)
        .unwrap();
    let mut sink = Sink::new(1);
    let mut callbacks = 0;
    let mut budget = StageBudget::new(4);
    assert_eq!(
        map.try_stage_with(&mut sink, &mut budget, &mut |_, _, _| {
            callbacks += 1;
            Ok(())
        }),
        Err(Error::Full)
    );
    assert_eq!(sink.calls, 2);
    assert_eq!(sink.objects.len(), 1);
    assert_eq!(budget.visited(), 2);
    assert_eq!(callbacks, 0);

    let mut sink = Sink::new(usize::MAX);
    let mut budget = StageBudget::new(4);
    assert_eq!(
        map.try_stage_with(&mut sink, &mut budget, &mut |_, _, _| Err(Error::Nested)),
        Err(Error::Nested)
    );
    assert_eq!(sink.calls, 2);
    assert_eq!(budget.visited(), 2);

    let mut sink = Sink::new(usize::MAX);
    let mut budget = StageBudget::new(2);
    let mut callbacks = 0;
    assert_eq!(
        map.try_stage_with(&mut sink, &mut budget, &mut |_, _, _| {
            callbacks += 1;
            Ok(())
        }),
        Err(Error::Stage(StageError::VisitLimit))
    );
    assert_eq!(sink.calls, 2);
    assert_eq!(callbacks, 1);
    assert_eq!(budget.visited(), 2);
}

#[test]
fn extension_and_branch_framing_are_checked_against_the_object_ceiling() {
    let mut map: MerkleMap<[u8; 2], Vec<u8>> = MerkleMap::new();
    for key in 0_u8..16 {
        map = map
            .insert(
                &[0x12, key << 4],
                vec![0x81; 64],
                &MemoryOnly,
                &mut Probe::new(),
            )
            .unwrap()
            .0;
    }
    let mut objects = Staged::new();
    map.stage(&mut objects);
    let largest = objects
        .objects()
        .iter()
        .map(|(_, bytes)| bytes.len())
        .max()
        .unwrap();
    // The extension's child is a branch with sixteen hash references.
    assert_eq!(largest, 532);
    let mut budget = StageBudget::new(18);
    map.preflight_with(largest, &mut budget, &mut byte_size)
        .unwrap();
    assert_eq!(budget.visited(), 18);
    assert_eq!(
        map.preflight_with(largest - 1, &mut StageBudget::new(18), &mut byte_size),
        Err(StageError::ObjectTooLarge {
            size: largest,
            limit: largest - 1
        })
    );
}

#[test]
fn committed_inline_children_are_not_preflighted_or_reencoded() {
    let mut map: MerkleMap<[u8; 1], Vec<u8>> = MerkleMap::new();
    for key in [0x10, 0x20] {
        map = map
            .insert(&[key], vec![0x81], &MemoryOnly, &mut Probe::new())
            .unwrap()
            .0;
    }
    let mut objects = Staged::new();
    map.stage(&mut objects);
    assert_eq!(objects.len(), 1);
    let mut store = MemoryNodeStore::new();
    store.commit(&objects);
    let updated = map
        .to_stored()
        .insert(&[0x10], vec![0x82], &store, &mut Probe::new())
        .unwrap()
        .0;
    let mut updated_objects = Staged::new();
    updated.stage(&mut updated_objects);
    let ceiling = updated_objects.objects()[0].1.len();
    let reads = store.reads();
    let mut budget = StageBudget::new(2);
    let mut leaves = 0;
    updated
        .preflight_with(ceiling, &mut budget, &mut |value, _| {
            leaves += 1;
            assert_eq!(value, &[0x82]);
            Ok::<_, StageError>(value.len())
        })
        .unwrap();
    assert_eq!(leaves, 1);
    assert_eq!(budget.visited(), 2);
    let mut actual = Sink::new(1);
    updated
        .try_stage_with(&mut actual, &mut StageBudget::new(2), &mut |_, _, _| Ok(()))
        .unwrap();
    assert_eq!(actual.objects.objects(), updated_objects.objects());
    assert_eq!(store.reads(), reads);
}

#[test]
fn fallible_staging_matches_infallible_and_skips_committed_subtrees() {
    let mut map: MerkleMap<[u8; 2], Vec<u8>> = MerkleMap::new();
    for i in 0_u16..512 {
        map = map
            .insert(
                &i.to_be_bytes(),
                vec![0x81; 1 + usize::from(i % 70)],
                &MemoryOnly,
                &mut Probe::new(),
            )
            .unwrap()
            .0;
    }
    let mut initial = Staged::new();
    map.stage(&mut initial);
    let mut store = MemoryNodeStore::new();
    store.commit(&initial);
    let stored = map.to_stored();
    let updated = stored
        .insert(&[0, 3], vec![0x81; 60], &store, &mut Probe::new())
        .unwrap()
        .0;
    let before = store.reads();
    let mut budget = StageBudget::new(100);
    updated
        .preflight_with(4096, &mut budget, &mut byte_size)
        .unwrap();
    assert!(budget.visited() <= 6);
    let preflight_visits = budget.visited();
    let mut expected = Staged::new();
    updated.stage(&mut expected);
    let mut actual = Sink::new(usize::MAX);
    let mut budget = StageBudget::new(preflight_visits);
    updated
        .try_stage_with(&mut actual, &mut budget, &mut |_, _, _| Ok(()))
        .unwrap();
    assert_eq!(actual.objects.objects(), expected.objects());
    assert_eq!(budget.visited(), preflight_visits);
    assert_eq!(store.reads(), before);
    stored
        .preflight_with(0, &mut StageBudget::new(0), &mut |_,
                                                           _|
         -> Result<
            usize,
            StageError,
        > {
            panic!("stored leaf callback")
        })
        .unwrap();
    stored
        .try_stage_with(
            &mut Sink::new(0),
            &mut StageBudget::new(0),
            &mut |_, _, _| panic!("stored nested callback"),
        )
        .unwrap();
}

#[test]
fn nested_maps_stage_even_when_the_outer_object_was_already_recorded() {
    let nested = Nested(one(vec![0x81; 64]));
    let outer = one(nested.clone());
    outer
        .preflight_with(1024, &mut StageBudget::new(2), &mut |nested, budget| {
            nested.0.preflight_with(1024, budget, &mut byte_size)?;
            Ok::<_, StageError>(40)
        })
        .unwrap();
    let plain = one(nested.encode());
    assert_eq!(outer.root_hash(), plain.root_hash());
    let mut actual = Sink::new(usize::MAX);
    let mut budget = StageBudget::new(3);
    plain
        .try_stage_with(&mut actual, &mut budget, &mut |_, _, _| Ok(()))
        .unwrap();
    outer
        .try_stage_with(&mut actual, &mut budget, &mut |nested, sink, budget| {
            nested.0.try_stage_with(sink, budget, &mut |_, _, _| Ok(()))
        })
        .unwrap();
    assert_eq!(actual.calls, 3);
    assert_eq!(actual.objects.len(), 2);
    assert_eq!(budget.visited(), 3);
    let mut expected = Staged::new();
    outer.stage(&mut expected);
    assert_eq!(actual.objects.objects(), expected.objects());
}

#[test]
fn short_roots_are_emitted_and_inline_children_are_only_visited() {
    let mut map: MerkleMap<[u8; 1], Vec<u8>> = MerkleMap::new();
    for key in [0x10, 0x20] {
        map = map
            .insert(&[key], vec![0x81], &MemoryOnly, &mut Probe::new())
            .unwrap()
            .0;
    }
    map.preflight_with(32, &mut StageBudget::new(3), &mut byte_size)
        .unwrap();
    let mut sink = Sink::new(1);
    let mut callbacks = 0;
    map.try_stage_with(&mut sink, &mut StageBudget::new(3), &mut |_, _, _| {
        callbacks += 1;
        Ok(())
    })
    .unwrap();
    assert_eq!(sink.calls, 1);
    assert_eq!(callbacks, 2);
    assert!(sink.objects.objects()[0].1.len() < 32);
}
