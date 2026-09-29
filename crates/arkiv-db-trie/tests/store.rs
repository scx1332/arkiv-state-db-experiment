//! Stored links: staging, committing, reopening from a root and lazy,
//! fallible operation through a loader.
//!
//! Establishes, with the `HashMap`-backed `MemoryNodeStore` and loaders
//! written here: staging a map emits exactly the objects the independent
//! reference construction says the trie consists of (the root, whatever
//! its size, and every node of 32 bytes or more), each once; a map reopened
//! from `(root, len)` answers every read like the in-memory original while
//! loading only the nodes each operation visits, with identical visit
//! counts (so counts do not depend on what is cached); updates on the
//! reopened map load only their paths (plus one collapse sibling), and
//! staging afterwards emits only nodes of the new trie, including every one
//! the store lacks, at most 2·LEN + 2 per update; an insert that creates
//! exactly 2·LEN + 2 nodes and a removal that creates exactly 2·LEN exist
//! (the per-update bounds); stored history stays readable; missing,
//! corrupt and unreadable objects surface as errors from reads, iteration,
//! updates and proofs, never as absent keys, and a host loader's own error
//! type passes through unchanged; a caching loader that overrides `load`
//! serves decoded nodes without store reads and without changing visit
//! counts; maps nested in leaf values are staged through the value hook
//! (same space, deduplicated) or `stage_with` (another space) and reopen
//! lazily, also when a map of another value type staged a byte-identical
//! leaf into the same sink first; hand-built tries whose hashes are valid
//! but whose shape is not canonical (an extension over a leaf, a short node
//! referenced by hash, a leaf at the wrong depth) are reported as
//! inconsistent by the map and rejected by `walk`.

mod support;

use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::{Arc, Mutex},
};

use alloy_primitives::{B256, keccak256};
use arkiv_db_trie::{
    CodecError, EMPTY_ROOT_HASH, MemoryNodeStore, MemoryOnly, MerkleMap, NodeBox, NodeLoader,
    ObjectId, ObjectSink, Probe, Staged, TrieError, TrieValue, walk,
};
use support::{Rng, byte_model, deep_keys, hash_builder_root, reference};

type Map<const N: usize> = MerkleMap<[u8; N], Vec<u8>>;

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

fn staged_ids(staged: &Staged) -> BTreeSet<B256> {
    staged.objects().iter().map(|(id, _)| *id).collect()
}

/// Every staged object is content-addressed, and only a root may be short.
fn check_staged(staged: &Staged, root: B256) {
    for (id, bytes) in staged.objects() {
        assert_eq!(keccak256(bytes), *id);
        assert!(*id == root || bytes.len() >= 32, "short non-root object");
    }
}

fn store_and_reopen<const N: usize>(seed: u64, count: usize, alphabet: u8) {
    let mut rng = Rng(seed);
    let mut model: BTreeMap<[u8; N], Vec<u8>> = (0..count)
        .map(|_| (rng.key::<N>(alphabet), rng.value(50)))
        .collect();
    let memory = build(&model);
    let root = memory.root_hash();

    // Staging emits exactly the trie's objects, once each.
    let mut staged = Staged::new();
    memory.stage(&mut staged);
    check_staged(&staged, root);
    let objects = reference::objects(&byte_model(&model));
    assert_eq!(staged_ids(&staged), objects);
    assert_eq!(staged.len(), objects.len(), "no object staged twice");
    let mut store = MemoryNodeStore::new();
    assert_eq!(store.commit(&staged), objects.len());

    // Reopen from the root alone; nothing is read until used.
    let stored = Map::<N>::from_root(root, model.len() as u64).unwrap();
    assert_eq!(store.reads(), 0);
    assert_eq!(stored.root_hash(), root);
    for (key, value) in model.iter().take(300) {
        let mut in_memory = Probe::new();
        let mut loaded = Probe::new();
        let reads = store.reads();
        assert_eq!(
            stored.get(key, &store, &mut loaded).unwrap().as_ref(),
            Some(value)
        );
        memory.get(key, &MemoryOnly, &mut in_memory).unwrap();
        assert_eq!(
            loaded.nodes, in_memory.nodes,
            "visits independent of residency"
        );
        assert_eq!(in_memory.loads, 0);
        assert!(loaded.loads >= 1 && loaded.loads <= loaded.nodes);
        assert_eq!(store.reads() - reads, loaded.loads, "one read per load");
    }
    // Bounded ranges and proofs read through the store agree with memory.
    for _ in 0..20 {
        let lower = rng.key::<N>(alphabet);
        let mut range = stored.range_from(&lower, &store);
        let first: Vec<_> = range.by_ref().take(5).map(Result::unwrap).collect();
        let expected: Vec<_> = model
            .range(lower..)
            .take(5)
            .map(|(key, value)| (*key, value.clone()))
            .collect();
        assert_eq!(first, expected);
        // Lazy: five entries never load the whole map.
        assert!(range.probe().loads < objects.len() as u64 / 2 || objects.len() < 40);
        let mut probe = Probe::new();
        let proof = stored.prove(&lower, &store, &mut probe).unwrap();
        assert_eq!(
            proof,
            memory
                .prove(&lower, &MemoryOnly, &mut Probe::new())
                .unwrap()
        );
        assert_eq!(walk(root, &lower, &proof), Ok(model.get(&lower).cloned()));
        // Every proof node is a stored object loaded once, and nothing else is.
        assert_eq!(probe.loads as usize, proof.len());
    }
    // A full iteration loads every object exactly once.
    let mut range = stored.iter(&store);
    let entries: Vec<_> = range.by_ref().map(Result::unwrap).collect();
    assert_eq!(
        entries,
        model
            .iter()
            .map(|(k, v)| (*k, v.clone()))
            .collect::<Vec<_>>()
    );
    assert_eq!(range.probe().loads, objects.len() as u64);
    let mut from_memory = memory.iter(&MemoryOnly);
    from_memory.by_ref().for_each(drop);
    assert_eq!(range.probe().nodes, from_memory.probe().nodes);

    // Update the reopened map lazily: new keys, removals, replacements.
    let old_model = model.clone();
    let mut map = stored.clone();
    let reads_before = store.reads();
    let mut loads = 0;
    let mut operations = 0u64;
    for step in 0..90 {
        let mut probe = Probe::new();
        match step % 3 {
            0 => {
                let (key, value) = (rng.key::<N>(alphabet), rng.value(50));
                let (next, previous) = map.insert(&key, value.clone(), &store, &mut probe).unwrap();
                assert_eq!(previous, model.insert(key, value));
                map = next;
            }
            1 => {
                let key = *model.keys().nth(rng.below(model.len())).unwrap();
                let (next, removed) = map.remove(&key, &store, &mut probe).unwrap();
                assert_eq!(removed, model.remove(&key));
                // The path and at most one collapse sibling.
                assert!(probe.nodes <= 2 * N as u64 + 2);
                map = next;
            }
            _ => {
                let key = *model.keys().nth(rng.below(model.len())).unwrap();
                let value = rng.value(50);
                map = map
                    .insert(&key, value.clone(), &store, &mut probe)
                    .unwrap()
                    .0;
                model.insert(key, value);
            }
        }
        assert!(probe.loads <= probe.nodes);
        loads += probe.loads;
        operations += 1;
    }
    assert_eq!(store.reads() - reads_before, loads);
    assert!(
        loads <= operations * (2 * N as u64 + 2),
        "only touched paths load"
    );
    assert_eq!(map.root_hash(), reference::root(&byte_model(&model)));

    // Staging again emits only nodes of the new trie, every one the store
    // lacks, and no more than the touched paths can hold.
    let mut again = Staged::new();
    map.stage(&mut again);
    check_staged(&again, map.root_hash());
    let new_objects = reference::objects(&byte_model(&model));
    let again_ids = staged_ids(&again);
    assert!(
        again_ids.is_subset(&new_objects),
        "staged an unreachable node"
    );
    let missing: BTreeSet<B256> = new_objects
        .iter()
        .filter(|id| !store.contains(id))
        .copied()
        .collect();
    assert!(
        missing.is_subset(&again_ids),
        "a needed node was not staged"
    );
    // An insert creates at most 2·N + 2 nodes (the new key's path and the
    // leaf or extension it splits off), a replacement 2·N + 1 and a removal
    // 2·N; `update_creation_bounds` reaches the first and the last.
    assert!(again.len() as u64 <= operations * (2 * N as u64 + 2));
    assert!(
        again.len() < objects.len(),
        "staging did not rewrite the map"
    );
    store.commit(&again);

    let reopened = Map::<N>::from_root(map.root_hash(), map.len()).unwrap();
    let entries: Vec<_> = reopened.iter(&store).map(Result::unwrap).collect();
    assert_eq!(
        entries,
        model
            .iter()
            .map(|(k, v)| (*k, v.clone()))
            .collect::<Vec<_>>()
    );
    // History: the old root is still complete in the store.
    let old = Map::<N>::from_root(root, old_model.len() as u64).unwrap();
    let entries: Vec<_> = old.iter(&store).map(Result::unwrap).collect();
    assert_eq!(entries.len(), old_model.len());
    assert!(
        entries
            .iter()
            .all(|(key, value)| old_model.get(key) == Some(value))
    );
    // After the commit, a stored handle has nothing left to stage.
    let mut nothing = Staged::new();
    map.to_stored().stage(&mut nothing);
    assert!(nothing.is_empty());
    // Staging the same map again into the same sink adds nothing.
    let before = again.len();
    map.stage(&mut again);
    assert_eq!(again.len(), before);
}

#[test]
fn build_stage_store_reopen_and_update_lazily() {
    store_and_reopen::<8>(1, 2000, 255);
    store_and_reopen::<8>(2, 800, 3);
    store_and_reopen::<21>(3, 600, 16);
    store_and_reopen::<33>(4, 500, 2);
    store_and_reopen::<88>(5, 300, 255);
}

#[test]
fn a_short_root_is_stored_by_hash() {
    let map = build(&BTreeMap::from([([1u8; 4], vec![2])]));
    let mut staged = Staged::new();
    map.stage(&mut staged);
    assert_eq!(staged.len(), 1);
    let (id, bytes) = &staged.objects()[0];
    assert!(bytes.len() < 32);
    assert_eq!(*id, map.root_hash());
    let mut store = MemoryNodeStore::new();
    store.commit(&staged);
    let stored = Map::<4>::from_root(*id, 1).unwrap();
    let mut probe = Probe::new();
    assert_eq!(
        stored.get(&[1; 4], &store, &mut probe).unwrap(),
        Some(vec![2])
    );
    let proof = stored.prove(&[1; 4], &store, &mut probe).unwrap();
    assert_eq!(walk(*id, &[1; 4], &proof), Ok(Some(vec![2])));
    // The empty map stages nothing.
    let mut nothing = Staged::new();
    Map::<4>::new().stage(&mut nothing);
    assert!(nothing.is_empty());
}

#[test]
fn a_collapse_loads_the_stored_sibling() {
    // The deep-path key set with 40-byte values, so that every leaf and
    // branch is at least 32 bytes and stored on its own.
    const N: usize = 8;
    let keys = deep_keys::<N>();
    let model: BTreeMap<[u8; N], Vec<u8>> = keys.iter().map(|key| (*key, vec![7; 40])).collect();
    let mut staged = Staged::new();
    let memory = build(&model);
    memory.stage(&mut staged);
    let mut store = MemoryNodeStore::new();
    store.commit(&staged);
    let stored = Map::<N>::from_root(memory.root_hash(), model.len() as u64).unwrap();
    // Hand-derived: 2·N stored branches and the leaf on the path, then the
    // zero key's leaf, the one sibling left in the deepest branch.
    let mut probe = Probe::new();
    let (smaller, removed) = stored.remove(&keys[2 * N], &store, &mut probe).unwrap();
    assert_eq!(removed, Some(vec![7; 40]));
    assert_eq!(
        probe,
        Probe {
            nodes: 2 * N as u64 + 2,
            loads: 2 * N as u64 + 2
        }
    );
    let mut expected = byte_model(&model);
    expected.remove(keys[2 * N].as_slice());
    assert_eq!(smaller.root_hash(), hash_builder_root(&expected));
    // Removing a key whose last branch keeps a branch sibling: the new
    // extension keeps the sibling's stored link (it is not re-staged).
    let mut probe = Probe::new();
    let (other, _) = stored.remove(&keys[1], &store, &mut probe).unwrap();
    let mut expected = byte_model(&model);
    expected.remove(keys[1].as_slice());
    assert_eq!(other.root_hash(), hash_builder_root(&expected));
    let mut again = Staged::new();
    other.stage(&mut again);
    // The root branch collapses into an extension over the old branch at
    // depth 1, whose stored link is reused: only the extension is new.
    assert_eq!(again.len(), 1);
}

/// The most nodes one update creates, reached exactly, for key length `N`.
fn update_creation_bounds<const N: usize>() {
    // The deep-path key set without its last key, with 40-byte values so
    // that every leaf and branch is at least 32 bytes and an object of its
    // own.
    let keys = deep_keys::<N>();
    let last = keys[2 * N];
    let without: BTreeMap<[u8; N], Vec<u8>> = keys[..2 * N]
        .iter()
        .map(|key| (*key, vec![7; 40]))
        .collect();
    let memory = build(&without);
    let mut staged = Staged::new();
    memory.stage(&mut staged);
    let mut store = MemoryNodeStore::new();
    store.commit(&staged);
    let stored = Map::<N>::from_root(memory.root_hash(), without.len() as u64).unwrap();

    // Hand-derived: without `last`, the zero key's path has branches at
    // nibbles 0..=2·N−2 and ends at a leaf whose path is the one nibble
    // left. Inserting `last`, which differs from the zero key only in that
    // nibble, visits those 2·N − 1 branches and the leaf, then replaces
    // the leaf by a branch holding two leaves with empty paths. New nodes:
    // the 2·N − 1 rebuilt branches, the new branch and the new key's leaf
    // (the new key's whole path, 2·N + 1 nodes) plus the zero key's leaf
    // re-created with a shorter path: 2·N + 2.
    let mut probe = Probe::new();
    let (grown, previous) = stored
        .insert(&last, vec![8; 40], &store, &mut probe)
        .unwrap();
    assert_eq!(previous, None);
    assert_eq!(probe.nodes, 2 * N as u64);
    let mut again = Staged::new();
    grown.stage(&mut again);
    check_staged(&again, grown.root_hash());
    assert_eq!(again.len(), 2 * N + 2);
    // Independently: exactly the objects of the new trie the old one lacks.
    let mut with = byte_model(&without);
    with.insert(last.to_vec(), vec![8; 40]);
    let old_objects = reference::objects(&byte_model(&without));
    let new_objects: BTreeSet<B256> = reference::objects(&with)
        .difference(&old_objects)
        .copied()
        .collect();
    assert_eq!(staged_ids(&again), new_objects);
    // With the zero key's value, the two new leaves (both with empty
    // paths) are the same object, staged once.
    let (same, _) = stored
        .insert(&last, vec![7; 40], &store, &mut Probe::new())
        .unwrap();
    let mut deduplicated = Staged::new();
    same.stage(&mut deduplicated);
    assert_eq!(deduplicated.len(), 2 * N + 1);

    // Removing `last` again from the committed full trie visits its path
    // (2·N branches and its leaf) and the collapse sibling, and creates the
    // 2·N − 1 branches above the collapsed one plus the zero key's leaf
    // with its nibble back: 2·N nodes. The result is the earlier trie, so
    // every one of them is already stored (the walk cannot know that).
    store.commit(&again);
    let full = Map::<N>::from_root(grown.root_hash(), grown.len()).unwrap();
    let mut probe = Probe::new();
    let (shrunk, removed) = full.remove(&last, &store, &mut probe).unwrap();
    assert_eq!(removed, Some(vec![8; 40]));
    assert_eq!(probe.nodes, 2 * N as u64 + 2);
    assert_eq!(shrunk.root_hash(), memory.root_hash());
    let mut restaged = Staged::new();
    shrunk.stage(&mut restaged);
    assert_eq!(restaged.len(), 2 * N);
    assert!(staged_ids(&restaged).is_subset(&old_objects));
}

#[test]
fn update_creation_bounds_are_reached() {
    update_creation_bounds::<1>();
    update_creation_bounds::<8>();
    update_creation_bounds::<21>();
    update_creation_bounds::<33>();
    update_creation_bounds::<88>();
}

#[test]
fn missing_and_corrupt_objects_are_errors_not_absence() {
    let mut rng = Rng(9);
    let model: BTreeMap<[u8; 8], Vec<u8>> = (0..500)
        .map(|_| (rng.key::<8>(255), rng.value(40)))
        .collect();
    let memory = build(&model);
    let root = memory.root_hash();
    let mut staged = Staged::new();
    memory.stage(&mut staged);
    let (victim_key, victim_value) = model.iter().nth(77).unwrap();
    let proof = memory
        .prove(victim_key, &MemoryOnly, &mut Probe::new())
        .unwrap();
    let victim = keccak256(proof.last().unwrap());
    assert_ne!(victim, root);

    // A lost object: reads through it fail with `Missing`.
    let mut store = MemoryNodeStore::new();
    store.commit(&staged);
    let lost = store.remove(&victim).unwrap();
    let map = Map::<8>::from_root(root, model.len() as u64).unwrap();
    let mut probe = Probe::new();
    let missing = TrieError::Missing { id: victim };
    assert_eq!(
        map.get(victim_key, &store, &mut probe),
        Err(missing.clone())
    );
    assert_eq!(
        map.insert(victim_key, vec![1], &store, &mut probe)
            .unwrap_err(),
        missing
    );
    assert_eq!(
        map.remove(victim_key, &store, &mut probe).unwrap_err(),
        missing
    );
    assert_eq!(
        map.prove(victim_key, &store, &mut probe),
        Err(missing.clone())
    );
    // Iteration yields every entry before the lost node, then the error,
    // then ends.
    let items: Vec<_> = map.iter(&store).collect();
    let before = items.iter().take_while(|item| item.is_ok()).count();
    assert_eq!(items.len(), before + 1);
    assert_eq!(items[before], Err(missing.clone()));
    assert!(before <= model.keys().position(|key| key == victim_key).unwrap());
    // Keys that leave the victim's path at the root branch still read.
    let (other_key, other_value) = model
        .iter()
        .find(|(key, _)| key[0] >> 4 != victim_key[0] >> 4)
        .unwrap();
    assert_eq!(
        map.get(other_key, &store, &mut probe).unwrap().as_ref(),
        Some(other_value)
    );

    // Corrupt bytes under the id: `HashMismatch`. Bytes of another valid
    // node under the id are corrupt too.
    let mut flipped = lost.clone();
    flipped[5] ^= 0x40;
    store.overwrite(victim, flipped);
    assert_eq!(
        map.get(victim_key, &store, &mut probe),
        Err(TrieError::HashMismatch { id: victim })
    );
    store.overwrite(victim, proof[0].clone());
    assert_eq!(
        map.get(victim_key, &store, &mut probe),
        Err(TrieError::HashMismatch { id: victim })
    );
    store.overwrite(victim, lost);
    assert_eq!(
        map.get(victim_key, &store, &mut probe).unwrap().as_ref(),
        Some(victim_value)
    );

    // A lost root.
    let mut empty_store = MemoryNodeStore::new();
    assert_eq!(
        map.get(victim_key, &empty_store, &mut probe),
        Err(TrieError::Missing { id: root })
    );
    // A memory-only loader never sees stored links; if it does, `Missing`.
    assert_eq!(
        map.get(victim_key, &MemoryOnly, &mut probe),
        Err(TrieError::Missing { id: root })
    );
    empty_store.commit(&Staged::new());
    assert!(empty_store.is_empty());
}

/// A host storage error type, as an engine would define it.
#[derive(Debug, PartialEq)]
enum HostError {
    Io,
    Trie(TrieError),
}

impl From<TrieError> for HostError {
    fn from(error: TrieError) -> Self {
        Self::Trie(error)
    }
}

/// Fails every read after the first `budget` reads.
struct FlakyStore<'a> {
    store: &'a MemoryNodeStore,
    budget: Cell<u64>,
}

impl<V: TrieValue> NodeLoader<V> for FlakyStore<'_> {
    type Error = HostError;

    fn read(&self, id: &ObjectId) -> Result<Option<Vec<u8>>, HostError> {
        let left = self.budget.get();
        if left == 0 {
            return Err(HostError::Io);
        }
        self.budget.set(left - 1);
        NodeLoader::<V>::read(self.store, id).map_err(HostError::Trie)
    }
}

#[test]
fn host_loader_errors_pass_through_unchanged() {
    let mut rng = Rng(10);
    let model: BTreeMap<[u8; 8], Vec<u8>> = (0..300)
        .map(|_| (rng.key::<8>(255), rng.value(40)))
        .collect();
    let memory = build(&model);
    let mut staged = Staged::new();
    memory.stage(&mut staged);
    let mut store = MemoryNodeStore::new();
    store.commit(&staged);
    let map = Map::<8>::from_root(memory.root_hash(), model.len() as u64).unwrap();
    let (key, value) = model.iter().next().unwrap();
    for budget in 0..4 {
        let flaky = FlakyStore {
            store: &store,
            budget: Cell::new(budget),
        };
        let mut probe = Probe::new();
        match map.get(key, &flaky, &mut probe) {
            Ok(found) => assert_eq!(found.as_ref(), Some(value)),
            Err(error) => assert_eq!(error, HostError::Io),
        }
        let flaky = FlakyStore {
            store: &store,
            budget: Cell::new(budget),
        };
        let items: Vec<_> = map.iter(&flaky).take(50).collect();
        assert!(items.contains(&Err(HostError::Io)));
        assert_eq!(
            items.last(),
            Some(&Err(HostError::Io)),
            "ends after the error"
        );
    }
    // Trie errors arrive through `From<TrieError>`.
    let empty = MemoryNodeStore::new();
    let flaky = FlakyStore {
        store: &empty,
        budget: Cell::new(10),
    };
    assert_eq!(
        map.get(key, &flaky, &mut Probe::new()),
        Err(HostError::Trie(TrieError::Missing {
            id: memory.root_hash()
        }))
    );
}

/// A loader with a decoded-node cache, as an engine would plug one in.
struct CachingLoader<'a> {
    store: &'a MemoryNodeStore,
    cache: Mutex<HashMap<ObjectId, Arc<NodeBox<Vec<u8>>>>>,
}

impl NodeLoader<Vec<u8>> for CachingLoader<'_> {
    type Error = TrieError;

    fn read(&self, id: &ObjectId) -> Result<Option<Vec<u8>>, TrieError> {
        NodeLoader::<Vec<u8>>::read(self.store, id)
    }

    fn load(&self, id: &ObjectId) -> Result<Arc<NodeBox<Vec<u8>>>, TrieError> {
        if let Some(node) = self.cache.lock().unwrap().get(id) {
            return Ok(Arc::clone(node));
        }
        let bytes = self.read(id)?.ok_or(TrieError::Missing { id: *id })?;
        let node = NodeBox::decode_stored(id, &bytes)?;
        self.cache.lock().unwrap().insert(*id, Arc::clone(&node));
        Ok(node)
    }
}

#[test]
fn a_caching_loader_changes_reads_but_not_visits() {
    let mut rng = Rng(11);
    let model: BTreeMap<[u8; 8], Vec<u8>> = (0..400)
        .map(|_| (rng.key::<8>(255), rng.value(40)))
        .collect();
    let memory = build(&model);
    let mut staged = Staged::new();
    memory.stage(&mut staged);
    let mut store = MemoryNodeStore::new();
    store.commit(&staged);
    let map = Map::<8>::from_root(memory.root_hash(), model.len() as u64).unwrap();
    let loader = CachingLoader {
        store: &store,
        cache: Mutex::new(HashMap::new()),
    };
    let key = model.keys().nth(123).unwrap();
    let mut cold = Probe::new();
    let reads = store.reads();
    map.get(key, &loader, &mut cold).unwrap();
    assert_eq!(store.reads() - reads, cold.loads);
    let mut warm = Probe::new();
    let reads = store.reads();
    map.get(key, &loader, &mut warm).unwrap();
    assert_eq!(store.reads(), reads, "served from the cache");
    assert_eq!(warm, cold, "cache hits count like loads");
    // Cached nodes hold no loader: dropping the cache leaves the handle and
    // the nodes fully usable.
    let cached: Vec<_> = loader.cache.lock().unwrap().values().cloned().collect();
    drop(loader);
    assert!(cached.iter().all(|node| node.from_store()));
    assert_eq!(
        map.get(key, &store, &mut Probe::new()).unwrap(),
        model.get(key).cloned()
    );
}

/// A posting set nested in a leaf value: the value encodes the nested
/// root and length and stages the nested nodes into the same space.
#[derive(Clone, Debug)]
struct Postings(MerkleMap<u64, Vec<u8>>);

impl TrieValue for Postings {
    fn encode(&self) -> Vec<u8> {
        let mut out = self.0.root_hash().to_vec();
        out.extend_from_slice(&self.0.len().to_be_bytes());
        out
    }

    fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        let (root, len) = bytes
            .split_at_checked(32)
            .filter(|(_, len)| len.len() == 8)
            .ok_or_else(|| CodecError::new("posting handle is not 40 bytes"))?;
        let len = u64::from_be_bytes(len.try_into().expect("8 bytes"));
        MerkleMap::from_root(B256::from_slice(root), len)
            .map(Self)
            .map_err(|_| CodecError::new("invalid posting handle"))
    }

    fn stage_nested(&self, sink: &mut dyn ObjectSink) {
        self.0.stage(sink);
    }
}

type Terms = MerkleMap<[u8; 4], Postings>;

fn postings(ids: &BTreeSet<u64>) -> Postings {
    let mut map = MerkleMap::new();
    for id in ids {
        map = map
            .insert(id, vec![0x01], &MemoryOnly, &mut Probe::new())
            .unwrap()
            .0;
    }
    Postings(map)
}

/// The byte model of a posting set, and of the terms map over those sets.
fn posting_model(ids: &BTreeSet<u64>) -> BTreeMap<Vec<u8>, Vec<u8>> {
    ids.iter()
        .map(|id| (id.to_be_bytes().to_vec(), vec![0x01]))
        .collect()
}

fn terms_model(terms: &BTreeMap<[u8; 4], BTreeSet<u64>>) -> BTreeMap<Vec<u8>, Vec<u8>> {
    terms
        .iter()
        .map(|(term, ids)| {
            let mut value = hash_builder_root(&posting_model(ids)).to_vec();
            value.extend_from_slice(&(ids.len() as u64).to_be_bytes());
            (term.to_vec(), value)
        })
        .collect()
}

#[test]
fn nested_maps_stage_through_the_value_hook() {
    let mut rng = Rng(12);
    let mut model: BTreeMap<[u8; 4], BTreeSet<u64>> = BTreeMap::new();
    for _ in 0..40 {
        let size = 1 + rng.below(60);
        let ids = (0..size).map(|_| rng.next() % 1000).collect();
        model.insert(rng.key::<4>(255), ids);
    }
    // Two terms with identical postings share their nested nodes.
    let shared: BTreeSet<u64> = (1..=30).collect();
    model.insert(*b"aaaa", shared.clone());
    model.insert(*b"bbbb", shared);
    let mut terms = Terms::new();
    for (term, ids) in &model {
        terms = terms
            .insert(term, postings(ids), &MemoryOnly, &mut Probe::new())
            .unwrap()
            .0;
    }
    let outer = terms_model(&model);
    assert_eq!(terms.root_hash(), hash_builder_root(&outer));

    let mut staged = Staged::new();
    terms.stage(&mut staged);
    let mut expected = reference::objects(&outer);
    for ids in model.values() {
        expected.extend(reference::objects(&posting_model(ids)));
    }
    assert_eq!(staged_ids(&staged), expected);
    assert_eq!(staged.len(), expected.len());
    let mut store = MemoryNodeStore::new();
    store.commit(&staged);

    // Reopen the outer map; the nested map loads lazily from its handle.
    let reopened = Terms::from_root(terms.root_hash(), terms.len()).unwrap();
    let mut probe = Probe::new();
    let term = *model.keys().nth(5).unwrap();
    let found = reopened.get(&term, &store, &mut probe).unwrap().unwrap();
    assert!(matches!(
        found.0.root_link(),
        arkiv_db_trie::Link::Stored(_)
    ));
    let ids: Vec<u64> = found.0.iter(&store).map(|entry| entry.unwrap().0).collect();
    assert_eq!(ids, model[&term].iter().copied().collect::<Vec<_>>());

    // Add a posting to one term through the reopened handles: only the new
    // outer path and the new posting path are staged.
    let (posting, _) = found
        .0
        .insert(&5000, vec![0x01], &store, &mut Probe::new())
        .unwrap();
    let (updated, _) = reopened
        .insert(&term, Postings(posting), &store, &mut Probe::new())
        .unwrap();
    model.get_mut(&term).unwrap().insert(5000);
    let outer = terms_model(&model);
    assert_eq!(updated.root_hash(), hash_builder_root(&outer));
    let mut again = Staged::new();
    updated.stage(&mut again);
    let mut expected = reference::objects(&outer);
    for ids in model.values() {
        expected.extend(reference::objects(&posting_model(ids)));
    }
    let again_ids = staged_ids(&again);
    assert!(again_ids.is_subset(&expected));
    assert!(
        expected
            .iter()
            .filter(|id| !store.contains(id))
            .all(|id| again_ids.contains(id))
    );
    // Replacing the term's value rebuilds its path (≤ 2·4 + 1 nodes);
    // inserting a posting creates at most 2·8 + 2.
    assert!(again.len() <= 9 + 18, "staged {}", again.len());
}

#[test]
fn stage_with_routes_nested_maps_to_another_space() {
    let mut rng = Rng(13);
    let mut model: BTreeMap<[u8; 4], BTreeSet<u64>> = BTreeMap::new();
    for _ in 0..20 {
        let size = 1 + rng.below(40);
        model.insert(
            rng.key::<4>(255),
            (0..size).map(|_| rng.next() % 500).collect(),
        );
    }
    let mut terms = Terms::new();
    for (term, ids) in &model {
        terms = terms
            .insert(term, postings(ids), &MemoryOnly, &mut Probe::new())
            .unwrap()
            .0;
    }
    let mut outer_space = Staged::new();
    let mut inner_space = Staged::new();
    terms.stage_with(&mut outer_space, &mut |value: &Postings| {
        value.0.stage(&mut inner_space)
    });
    assert_eq!(
        staged_ids(&outer_space),
        reference::objects(&terms_model(&model))
    );
    let mut inner = BTreeSet::new();
    for ids in model.values() {
        inner.extend(reference::objects(&posting_model(ids)));
    }
    assert_eq!(staged_ids(&inner_space), inner);
    assert_eq!(
        MerkleMap::<[u8; 4], Postings>::from_root(EMPTY_ROOT_HASH, 0)
            .unwrap()
            .root_hash(),
        EMPTY_ROOT_HASH
    );
}

/// Maps with different value types share a space, and so a sink. Here a
/// plain map holds, as bytes, exactly the encoding of a terms map's value,
/// so the two single-entry maps have the same root leaf (same path, same
/// value bytes) and the same id, but only the terms leaf names a nested
/// posting map. Whichever map is staged first, the posting map is staged,
/// and both maps read back in full after the commit.
#[test]
fn a_shared_sink_stages_nested_maps_whatever_was_staged_first() {
    let ids: BTreeSet<u64> = (1..=20).collect();
    let term = *b"term";
    let terms = Terms::new()
        .insert(&term, postings(&ids), &MemoryOnly, &mut Probe::new())
        .unwrap()
        .0;
    // The terms value's encoding, from the independent posting model.
    let mut plain_value = hash_builder_root(&posting_model(&ids)).to_vec();
    plain_value.extend_from_slice(&(ids.len() as u64).to_be_bytes());
    let plain = Map::<4>::new()
        .insert(&term, plain_value.clone(), &MemoryOnly, &mut Probe::new())
        .unwrap()
        .0;
    assert_eq!(plain.root_hash(), terms.root_hash(), "one leaf, one id");
    let mut expected = reference::objects(&terms_model(&BTreeMap::from([(term, ids.clone())])));
    expected.extend(reference::objects(&posting_model(&ids)));

    for plain_first in [true, false] {
        let mut sink = Staged::new();
        if plain_first {
            plain.stage(&mut sink);
            terms.stage(&mut sink);
        } else {
            terms.stage(&mut sink);
            plain.stage(&mut sink);
        }
        assert_eq!(staged_ids(&sink), expected, "plain first: {plain_first}");
        assert_eq!(sink.len(), expected.len());
        let mut store = MemoryNodeStore::new();
        store.commit(&sink);

        let reopened = Terms::from_root(terms.root_hash(), 1).unwrap();
        let found = reopened
            .get(&term, &store, &mut Probe::new())
            .unwrap()
            .unwrap();
        let read: Vec<u64> = found.0.iter(&store).map(|entry| entry.unwrap().0).collect();
        assert_eq!(read, ids.iter().copied().collect::<Vec<_>>());
        let reopened = Map::<4>::from_root(plain.root_hash(), 1).unwrap();
        assert_eq!(
            reopened.get(&term, &store, &mut Probe::new()).unwrap(),
            Some(plain_value.clone())
        );
    }
}

/// Stores hand-built nodes and returns the id of the first.
fn put_all(store: &mut MemoryNodeStore, nodes: &[Vec<u8>]) -> B256 {
    let mut staged = Staged::new();
    for node in nodes {
        staged.record(keccak256(node), node);
    }
    store.commit(&staged);
    keccak256(&nodes[0])
}

#[test]
fn hash_valid_but_noncanonical_tries_are_inconsistent() {
    use reference::{compact, list, string};
    let hashed = |node: &[u8]| string(keccak256(node).as_slice());
    let key = [0x12u8, 0x34];
    let mut store = MemoryNodeStore::new();
    let inconsistent = |reason| TrieError::Inconsistent { reason };

    // An extension whose hashed child is a leaf, not a branch.
    let leaf = list(&[string(&compact(&[2, 3, 4], true)), string(&[9; 40])]);
    let extension = list(&[string(&compact(&[1], false)), hashed(&leaf)]);
    let root = put_all(&mut store, &[extension.clone(), leaf.clone()]);
    let map = Map::<2>::from_root(root, 1).unwrap();
    assert_eq!(
        map.get(&key, &store, &mut Probe::new()),
        Err(inconsistent("extension child is not a branch"))
    );
    assert!(matches!(
        walk(root, &key, &[extension, leaf]),
        Err(arkiv_db_trie::ProofError::InvalidNode { index: 1, .. })
    ));

    // A branch referencing a 5-byte leaf by hash instead of inline.
    let small = list(&[string(&compact(&[2, 3, 4], true)), string(&[5])]);
    assert_eq!(small.len(), 5);
    let big = list(&[string(&compact(&[3, 4, 5], true)), string(&[6; 40])]);
    let mut slots: Vec<Vec<u8>> = vec![string(&[]); 17];
    slots[1] = hashed(&small);
    slots[2] = hashed(&big);
    let branch = list(&slots);
    let root = put_all(&mut store, &[branch.clone(), small.clone(), big]);
    let map = Map::<2>::from_root(root, 2).unwrap();
    assert_eq!(
        map.get(&key, &store, &mut Probe::new()),
        Err(inconsistent("hash-referenced node shorter than 32 bytes"))
    );
    assert!(matches!(
        walk(root, &key, &[branch, small]),
        Err(arkiv_db_trie::ProofError::InvalidNode { index: 1, .. })
    ));

    // A root leaf two nibbles short of the four-nibble key.
    let short = list(&[string(&compact(&[1, 2], true)), string(&[7])]);
    let root = put_all(&mut store, std::slice::from_ref(&short));
    let map = Map::<2>::from_root(root, 1).unwrap();
    assert_eq!(
        map.get(&key, &store, &mut Probe::new()),
        Err(inconsistent("leaf at the wrong depth for the key length"))
    );
    let items: Vec<_> = map.iter(&store).collect();
    assert_eq!(
        items,
        [Err(inconsistent(
            "leaf at the wrong depth for the key length"
        ))]
    );
    assert!(matches!(
        walk(root, &key, &[short]),
        Err(arkiv_db_trie::ProofError::InvalidNode { index: 0, .. })
    ));
}
