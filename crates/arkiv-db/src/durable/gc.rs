//! Garbage collection of the engine's trie objects (see
//! [`arkiv_db_storage::gc`] for the mechanism).
//!
//! [`EngineGraph`] is the object graph the store counts with: every
//! object's references follow from its space and bytes alone. A durable
//! engine on a store with garbage collection commits through
//! [`VersionedStore::commit_gc`] with the roots of the head it replaces and
//! of the new head, and a horizon of `min(durable floor, lowest pinned
//! snapshot)`: nothing a retained snapshot or an open handle can read is
//! ever deleted. None of this enters a state root, a receipt or a charge:
//! those are computed before the store sees the batch, and execution reads
//! only the head, which is always whole.

use super::{DurableEngine, DurableError, EngineManifest, corrupt, format::MANIFEST_MAX_BYTES};
use crate::{
    StateHeader,
    layout::{Catalog, HostMap, NamespaceState, Posting},
};
use arkiv_db_storage::{
    gc::{Edge, GcState, ObjectGraph},
    versioned::{ObjectId, SpaceId, StorageError, Table, VersionedStore},
};
use arkiv_db_trie::{EMPTY_ROOT_HASH, MerkleMap, NodeLoader, TrieError, TrieKey, TrieValue};

/// The edges of the engine's objects, version 1:
///
/// - the hash children of a branch or extension, in the same space
///   (inline children are part of their parent and are searched too);
/// - for a catalog leaf ([`NamespaceState`]), the roots of its four
///   non-empty maps, in that namespace's space (strict);
/// - in a namespace space, every leaf value of 40 bytes that reads as a
///   non-empty map handle (`root ‖ len`): a term's posting map. Other
///   values of that length (a raw value or a row) cannot be told apart by
///   their bytes, so the edge is speculative: it may name nothing, which
///   only delays deletion of what it does name;
/// - host values reference nothing.
#[derive(Clone, Copy, Debug, Default)]
pub struct EngineGraph;

/// [`ObjectGraph::version`] of [`EngineGraph`].
pub const ENGINE_GRAPH_VERSION: u32 = 1;

impl ObjectGraph for EngineGraph {
    fn version(&self) -> u32 {
        ENGINE_GRAPH_VERSION
    }

    fn edges(&self, space: SpaceId, bytes: &[u8], out: &mut Vec<Edge>) -> Result<(), String> {
        let mut hashes = Vec::new();
        let mut values = Vec::new();
        arkiv_db_trie::object_references(bytes, &mut hashes, &mut values)
            .map_err(str::to_string)?;
        out.extend(hashes.into_iter().map(|id| Edge {
            space,
            id: id.0,
            strict: true,
        }));
        match space {
            SpaceId::Catalog => {
                for value in values {
                    let leaf = NamespaceState::decode(&value).map_err(|e| e.to_string())?;
                    let roots = leaf.roots();
                    let namespace = SpaceId::Namespace(leaf.descriptor.id.ordinal());
                    for (root, len) in [roots.records, roots.keys, roots.rows, roots.terms] {
                        if len > 0 {
                            out.push(Edge {
                                space: namespace,
                                id: root.0,
                                strict: true,
                            });
                        }
                    }
                }
            }
            SpaceId::Namespace(_) => {
                for value in values {
                    if value.len() != 40 {
                        continue;
                    }
                    let len = u64::from_be_bytes(value[32..].try_into().expect("8 bytes"));
                    if len == 0 || value[..32] == EMPTY_ROOT_HASH.0 {
                        continue;
                    }
                    let mut id = [0u8; 32];
                    id.copy_from_slice(&value[..32]);
                    out.push(Edge {
                        space,
                        id,
                        strict: false,
                    });
                }
            }
            SpaceId::Host => {}
        }
        Ok(())
    }
}

/// The stored roots a state header names: the catalog's and the host
/// map's, when not empty.
pub fn state_roots(state: &StateHeader) -> Vec<(SpaceId, ObjectId)> {
    let mut roots = Vec::with_capacity(2);
    if state.catalog_len > 0 {
        roots.push((SpaceId::Catalog, state.catalog_root.0));
    }
    if state.host_len > 0 {
        roots.push((SpaceId::Host, state.host_root.0));
    }
    roots
}

fn manifest_at(store: &dyn VersionedStore, sequence: u64) -> Result<EngineManifest, DurableError> {
    let bytes = store
        .read_record_bounded(
            Table::Manifests,
            &sequence.to_be_bytes(),
            MANIFEST_MAX_BYTES,
        )?
        .ok_or_else(|| corrupt(format!("manifest {sequence} missing")))?;
    let manifest = EngineManifest::from_envelope(&bytes)?;
    if manifest.sequence != sequence {
        return Err(corrupt("manifest sequence association"));
    }
    Ok(manifest)
}

fn head_manifest(store: &dyn VersionedStore) -> Result<EngineManifest, DurableError> {
    let head = store.head()?.ok_or_else(|| corrupt("store has no head"))?;
    manifest_at(store, head.height)
}

/// Turns garbage collection on for a store no engine has open (or
/// rebuilds its counts): one scan of every object, counted from the head
/// recorded in the store. Works on every engine profile. `spill` is a
/// scratch directory for stores too large to count in memory (about 42
/// bytes per object and per edge are written there).
///
/// Objects the head does not reach become dead at the head's epoch, so
/// the snapshots below the head stay readable until the retention floor
/// passes the head.
pub fn enable(
    store: &dyn VersionedStore,
    spill: Option<&std::path::Path>,
) -> Result<GcState, DurableError> {
    let manifest = head_manifest(store)?;
    Ok(store.gc_enable(&state_roots(&manifest.state), &EngineGraph, spill)?)
}

/// Checks the store's GC tables against its objects and head
/// ([`arkiv_db_storage::gc::verify`]). Reads the whole store into memory:
/// tests and small stores only.
pub fn verify(store: &dyn VersionedStore) -> Result<(), String> {
    let dump = store
        .gc_dump()
        .map_err(|e| e.to_string())?
        .ok_or("the store does not collect garbage")?;
    let manifest = head_manifest(store).map_err(|e| e.to_string())?;
    arkiv_db_storage::gc::verify(
        &dump.objects,
        &dump.tables,
        &dump.state,
        &EngineGraph,
        &state_roots(&manifest.state),
    )
}

struct Loader<'a> {
    store: &'a dyn VersionedStore,
    space: SpaceId,
}

struct Missing(String);
impl From<TrieError> for Missing {
    fn from(e: TrieError) -> Self {
        Self(e.to_string())
    }
}
impl From<StorageError> for Missing {
    fn from(e: StorageError) -> Self {
        Self(e.to_string())
    }
}

impl<V: TrieValue> NodeLoader<V> for Loader<'_> {
    type Error = Missing;
    fn read(&self, id: &arkiv_db_trie::ObjectId) -> Result<Option<Vec<u8>>, Missing> {
        Ok(self.store.read_object(self.space, &id.0)?)
    }
}

fn whole<K: TrieKey, V: TrieValue>(
    store: &dyn VersionedStore,
    map: &MerkleMap<K, V>,
    space: SpaceId,
    objects: &mut u64,
    leaf: &mut dyn FnMut(&V) -> Result<(), Missing>,
) -> Result<(), Missing> {
    map.walk_stored(
        &Loader { store, space },
        &mut |_, _| {
            *objects += 1;
            true
        },
        leaf,
    )
}

/// Reads every object snapshot `sequence` reaches, through its typed maps,
/// and returns how many it read (shared objects once per reference).
/// Fails on the first missing or damaged object: the check that garbage
/// collection never deletes what a retained snapshot needs.
pub fn check_snapshot(store: &dyn VersionedStore, sequence: u64) -> Result<u64, String> {
    let manifest = manifest_at(store, sequence).map_err(|e| e.to_string())?;
    let state = &manifest.state;
    let mut objects = 0u64;
    let catalog =
        Catalog::from_root(state.catalog_root, state.catalog_len).map_err(|e| e.to_string())?;
    let mut namespaces = Vec::new();
    whole(
        store,
        &catalog,
        SpaceId::Catalog,
        &mut objects,
        &mut |leaf| {
            namespaces.push(leaf.clone());
            Ok(())
        },
    )
    .map_err(|e| e.0)?;
    for leaf in namespaces {
        let space = SpaceId::Namespace(leaf.descriptor.id.ordinal());
        let walk = |objects: &mut u64| -> Result<(), Missing> {
            whole(store, &leaf.records, space, objects, &mut |_| Ok(()))?;
            whole(store, &leaf.typed.keys, space, objects, &mut |_| Ok(()))?;
            whole(store, &leaf.typed.rows, space, objects, &mut |_| Ok(()))?;
            let mut postings = Vec::new();
            whole(
                store,
                &leaf.typed.terms,
                space,
                objects,
                &mut |p: &Posting| {
                    postings.push(p.0.clone());
                    Ok(())
                },
            )?;
            for posting in postings {
                whole(store, &posting, space, objects, &mut |_| Ok(()))?;
            }
            Ok(())
        };
        walk(&mut objects).map_err(|e| e.0)?;
    }
    let host = HostMap::from_root(state.host_root, state.host_len).map_err(|e| e.to_string())?;
    whole(store, &host, SpaceId::Host, &mut objects, &mut |_| Ok(())).map_err(|e| e.0)?;
    Ok(objects)
}

impl DurableEngine {
    /// Whether this engine's store collects garbage.
    pub fn collects_garbage(&self) -> bool {
        self.gc
    }

    /// Turns garbage collection on for this engine's store at its head
    /// ([`enable`]). Refused under archive retention, which keeps every
    /// snapshot, and with a prepared publication outstanding.
    pub fn enable_gc(&mut self, spill: Option<&std::path::Path>) -> Result<GcState, DurableError> {
        if self.options.retention == super::Retention::Archive {
            return Err(super::incompatible(
                "garbage collection needs a retention window, not archive",
            ));
        }
        if self.pending.is_some() || self.state != super::WriterState::Ready {
            return Err(DurableError::WriterUnavailable);
        }
        let state = self.context.store.gc_enable(
            &state_roots(&self.manifest.state),
            &EngineGraph,
            spill,
        )?;
        self.gc = true;
        Ok(state)
    }

    /// The store's garbage-collection state, if it collects garbage. Kept
    /// by the commits themselves: reading it walks nothing.
    pub fn gc_state(&self) -> Result<Option<GcState>, DurableError> {
        Ok(self.context.store.gc_state()?)
    }

    /// The deletion horizon the next commit would use:
    /// `min(durable floor, lowest pinned snapshot)`.
    pub fn gc_horizon(&self) -> u64 {
        let durable = self.retention.durable();
        self.retention
            .lowest_pinned()
            .map_or(durable, |pin| pin.min(durable))
    }
}
