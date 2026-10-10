//! Reading stored nodes: the loader contract and two small loaders.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use crate::{NodeBox, ObjectId, TrieError, TrieValue, stage::Staged};

/// Supplies the committed nodes of one map's object space.
///
/// Map handles hold no loader: every operation that may follow a
/// [`Link::Stored`](crate::Link::Stored) link takes one by reference, so a
/// host can pass a short-lived view of its store (for example its store
/// context plus the space and map kind of the map being read) and nothing
/// the map retains points back at the store.
///
/// The error type absorbs [`TrieError`], so a host with its own storage
/// error type returns that type from [`read`](Self::read) and receives it,
/// unchanged, from every map operation; trie integrity failures arrive
/// through its `From<TrieError>` conversion.
///
/// [`load`](Self::load) has a default that reads the bytes and calls
/// [`NodeBox::decode_stored`]. A host with a bounded decoded-node cache
/// overrides it to consult the cache (keyed by object space, map kind and
/// object id) before reading, and to insert the decoded node after a
/// successful decode, charging [`NodeBox::retained_bytes`]. Cache hits and
/// loads are indistinguishable to the map, so canonical costs cannot depend
/// on cache warmth.
pub trait NodeLoader<V: TrieValue> {
    /// The loader's failure type.
    type Error: From<TrieError>;

    /// Reads the committed bytes of object `id`, or `None` if absent.
    ///
    /// Absence of a referenced object is reported by [`load`](Self::load)
    /// as [`TrieError::Missing`]; it never means the key is absent.
    fn read(&self, id: &ObjectId) -> Result<Option<Vec<u8>>, Self::Error>;

    /// Loads, verifies and decodes the node stored under `id`.
    fn load(&self, id: &ObjectId) -> Result<Arc<NodeBox<V>>, Self::Error> {
        let bytes = self.read(id)?.ok_or(TrieError::Missing { id: *id })?;
        Ok(NodeBox::decode_stored(id, &bytes)?)
    }
}

/// The loader for maps that live entirely in memory.
///
/// A map built only by updates from [`MerkleMap::new`](crate::MerkleMap::new)
/// never holds a stored link, so this loader is never asked; if it is, it
/// reports the object as [`TrieError::Missing`].
#[derive(Clone, Copy, Debug, Default)]
pub struct MemoryOnly;

impl<V: TrieValue> NodeLoader<V> for MemoryOnly {
    type Error = TrieError;

    fn read(&self, _id: &ObjectId) -> Result<Option<Vec<u8>>, TrieError> {
        Ok(None)
    }
}

/// A content-addressed object store in a `HashMap`, for tests and examples.
///
/// It exercises stored links, lazy loading and staging without a durable
/// backend: objects are put if absent, reads are counted, and objects can
/// be removed or overwritten to simulate missing and corrupt data. It is
/// volatile and makes no durability claim.
#[derive(Debug, Default)]
pub struct MemoryNodeStore {
    objects: HashMap<ObjectId, Vec<u8>>,
    reads: AtomicU64,
}

impl MemoryNodeStore {
    /// Creates an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Puts every staged object if absent; returns how many were new.
    ///
    /// # Panics
    ///
    /// Panics if an id is already stored with different bytes, which cannot
    /// happen for content-addressed objects unless the store was tampered
    /// with through [`overwrite`](Self::overwrite).
    pub fn commit(&mut self, staged: &Staged) -> usize {
        let mut added = 0;
        for (id, bytes) in staged.objects() {
            match self.objects.get(id) {
                Some(existing) => assert_eq!(existing, bytes, "object {id} stored twice"),
                None => {
                    self.objects.insert(*id, bytes.clone());
                    added += 1;
                }
            }
        }
        added
    }

    /// Whether `id` is stored.
    pub fn contains(&self, id: &ObjectId) -> bool {
        self.objects.contains_key(id)
    }

    /// The number of stored objects.
    pub fn len(&self) -> usize {
        self.objects.len()
    }

    /// Whether the store is empty.
    pub fn is_empty(&self) -> bool {
        self.objects.is_empty()
    }

    /// The number of reads served so far, found or not.
    pub fn reads(&self) -> u64 {
        self.reads.load(Ordering::Relaxed)
    }

    /// Removes an object, simulating lost data.
    pub fn remove(&mut self, id: &ObjectId) -> Option<Vec<u8>> {
        self.objects.remove(id)
    }

    /// Replaces an object's bytes without any check, simulating corruption.
    pub fn overwrite(&mut self, id: ObjectId, bytes: Vec<u8>) {
        self.objects.insert(id, bytes);
    }

    /// Iterates over the stored ids, in no particular order.
    pub fn ids(&self) -> impl Iterator<Item = &ObjectId> {
        self.objects.keys()
    }
}

impl<V: TrieValue> NodeLoader<V> for MemoryNodeStore {
    type Error = TrieError;

    fn read(&self, id: &ObjectId) -> Result<Option<Vec<u8>>, TrieError> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        Ok(self.objects.get(id).cloned())
    }
}
