//! Read-only reachability report: which stored trie objects the head and
//! the most recent snapshots still reach, per object space.
//!
//! The report is the mark phase of a future pruning pass run without a
//! sweep. It reads the head and `Manifests` records and walks the typed
//! maps of each snapshot (catalog → each namespace's records, keys, rows
//! and terms → each term's posting map; host), so it works on every file
//! with engine manifests: the engine-only, simulator and signed-node
//! profiles alike. Subtrees shared between snapshots are walked once.
//! Store totals per space are backend-specific and come from the caller
//! (for redb, `RedbStore::layout`).

use super::{
    DurableEngine, DurableError, EngineManifest, corrupt, format::MANIFEST_MAX_BYTES, read_floor,
};
use crate::{
    layout::{Catalog, HostMap, NamespaceState, Posting},
    state::MapKind,
};
use arkiv_db_storage::versioned::{SpaceId, StorageError, Table, VersionedStore};
use arkiv_db_trie::{MerkleMap, NodeLoader, ObjectId, TrieError, TrieKey, TrieValue};
use std::{
    cell::RefCell,
    collections::{BTreeMap, HashSet},
};

/// Distinct objects and their encoded bytes (values only, no keys).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Reach {
    pub objects: u64,
    pub bytes: u64,
}

/// The objects reachable from the union of the `recent` newest snapshots.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReachLevel {
    /// Requested snapshot count (the head counts as one).
    pub recent: u64,
    /// The oldest snapshot walked for this level: never below the state
    /// floor, so a level may cover fewer snapshots than requested.
    pub oldest: u64,
    pub per_space: BTreeMap<SpaceId, Reach>,
}

impl ReachLevel {
    /// The sum over all spaces.
    pub fn total(&self) -> Reach {
        self.per_space
            .values()
            .fold(Reach::default(), |a, r| Reach {
                objects: a.objects + r.objects,
                bytes: a.bytes + r.bytes,
            })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReachabilityReport {
    pub head: u64,
    /// The state floor the walk was clamped to.
    pub floor: u64,
    /// The head's namespace allocator: spaces `Namespace(0..next_namespace)`
    /// are the only namespace spaces any snapshot up to the head can use.
    pub next_namespace: u64,
    /// One level per requested count, in ascending order.
    pub levels: Vec<ReachLevel>,
}

impl ReachabilityReport {
    /// Every object space a level reports or a store may hold.
    pub fn spaces(&self) -> Vec<SpaceId> {
        [SpaceId::Catalog, SpaceId::Host]
            .into_iter()
            .chain((0..self.next_namespace).map(SpaceId::Namespace))
            .collect()
    }
}

struct Fail(DurableError);
impl From<TrieError> for Fail {
    fn from(e: TrieError) -> Self {
        Self(corrupt(format!("reachability walk: {e}")))
    }
}
impl From<StorageError> for Fail {
    fn from(e: StorageError) -> Self {
        Self(e.into())
    }
}

struct Loader<'a> {
    store: &'a dyn VersionedStore,
    space: SpaceId,
}
impl<V: TrieValue> NodeLoader<V> for Loader<'_> {
    type Error = Fail;
    fn read(&self, id: &ObjectId) -> Result<Option<Vec<u8>>, Fail> {
        Ok(self.store.read_object(self.space, &id.0)?)
    }
}

#[derive(Default)]
struct Marks {
    /// Walked (space, map kind, id): a skip is only sound per kind.
    walked: HashSet<(SpaceId, MapKind, [u8; 32])>,
    /// Counted (space, id).
    counted: HashSet<(SpaceId, [u8; 32])>,
    per_space: BTreeMap<SpaceId, Reach>,
}

struct Walk<'a> {
    store: &'a dyn VersionedStore,
    marks: RefCell<Marks>,
}

impl Walk<'_> {
    fn map<K: TrieKey, V: TrieValue>(
        &self,
        map: &MerkleMap<K, V>,
        space: SpaceId,
        kind: MapKind,
        leaf: &mut dyn FnMut(&V) -> Result<(), Fail>,
    ) -> Result<(), Fail> {
        let loader = Loader {
            store: self.store,
            space,
        };
        map.walk_stored(
            &loader,
            &mut |id: &ObjectId, len| {
                let mut marks = self.marks.borrow_mut();
                if marks.counted.insert((space, id.0)) {
                    let reach = marks.per_space.entry(space).or_default();
                    reach.objects += 1;
                    reach.bytes += len as u64;
                }
                marks.walked.insert((space, kind, id.0))
            },
            leaf,
        )
    }

    fn snapshot(&self, manifest: &EngineManifest) -> Result<(), Fail> {
        let state = &manifest.state;
        let catalog = Catalog::from_root(state.catalog_root, state.catalog_len)?;
        self.map(
            &catalog,
            SpaceId::Catalog,
            MapKind::Catalog,
            &mut |leaf: &NamespaceState| {
                let space = SpaceId::Namespace(leaf.descriptor.id.ordinal());
                self.map(&leaf.records, space, MapKind::Records, &mut |_| Ok(()))?;
                self.map(&leaf.typed.keys, space, MapKind::Keys, &mut |_| Ok(()))?;
                self.map(&leaf.typed.rows, space, MapKind::Rows, &mut |_| Ok(()))?;
                self.map(
                    &leaf.typed.terms,
                    space,
                    MapKind::Terms,
                    &mut |posting: &Posting| {
                        self.map(&posting.0, space, MapKind::Postings, &mut |_| Ok(()))
                    },
                )
            },
        )?;
        let host = HostMap::from_root(state.host_root, state.host_len)?;
        self.map(&host, SpaceId::Host, MapKind::Host, &mut |_| Ok(()))
    }
}

fn manifest(store: &dyn VersionedStore, sequence: u64) -> Result<EngineManifest, DurableError> {
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

/// Offline report on a store no engine has open: walks the head and then
/// ever older snapshots and reports, for each count in `recent` (each at
/// least one; the head counts as one), the distinct objects reachable from
/// the union of that many newest snapshots. Levels are clamped to the
/// durable state floor recorded with the head (no snapshot below it is
/// walked). Read-only; it never consults the decoded cache. While an engine
/// is open on the store use [`DurableEngine::reachability`], which pins the
/// oldest walked snapshot.
pub fn reachability(
    store: &dyn VersionedStore,
    recent: &[u64],
) -> Result<ReachabilityReport, DurableError> {
    let head = store
        .head()?
        .ok_or_else(|| corrupt("store has no head"))?
        .height;
    let floor = read_floor(store, head)?;
    walk_levels(store, head, floor, recent)
}

impl DurableEngine {
    /// [`reachability`] over this engine's store, clamped to the live state
    /// floor. The oldest snapshot walked is pinned (through an open handle)
    /// for the whole walk, so later physical deletion cannot remove what the
    /// walk still reads.
    pub fn reachability(&self, recent: &[u64]) -> Result<ReachabilityReport, DurableError> {
        let head = self.head().sequence();
        let floor = self.state_floor().sequence();
        let deepest = recent.iter().map(|n| (*n).max(1)).max().unwrap_or(1);
        let oldest = head.saturating_sub(deepest - 1).max(floor);
        let _pin = self.snapshot(crate::SnapshotId::from_sequence(oldest))?;
        walk_levels(self.context.store.as_ref(), head, floor, recent)
    }
}

fn walk_levels(
    store: &dyn VersionedStore,
    head: u64,
    floor: u64,
    recent: &[u64],
) -> Result<ReachabilityReport, DurableError> {
    let mut counts: Vec<u64> = recent.iter().map(|n| (*n).max(1)).collect();
    counts.sort_unstable();
    counts.dedup();
    let head_manifest = manifest(store, head)?;
    let walk = Walk {
        store,
        marks: RefCell::default(),
    };
    let mut levels = Vec::new();
    let mut walked = 0u64;
    for count in counts {
        let oldest = head.saturating_sub(count - 1).max(floor);
        while walked < count && walked <= head - oldest {
            let sequence = head - walked;
            let m = if walked == 0 {
                head_manifest.clone()
            } else {
                manifest(store, sequence)?
            };
            walk.snapshot(&m).map_err(|f| f.0)?;
            walked += 1;
        }
        levels.push(ReachLevel {
            recent: count,
            oldest,
            per_space: walk.marks.borrow().per_space.clone(),
        });
    }
    Ok(ReachabilityReport {
        head,
        floor,
        next_namespace: head_manifest.state.next_namespace,
        levels,
    })
}
