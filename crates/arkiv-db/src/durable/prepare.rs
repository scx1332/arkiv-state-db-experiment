use super::{DurableError, Options};
use crate::{
    MapKind, Value,
    engine::WorkingState,
    layout::{Blob, NamespaceState, Posting, StoredRecord},
};
use arkiv_db_storage::versioned::{ObjectId, SpaceId};
use arkiv_db_trie::{MerkleMap, StageBudget, StageError, TrieKey, TrieValue, TryObjectSink};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PreparationStats {
    pub preflight_visits: u64,
    pub staging_visits: u64,
    pub unique_objects: usize,
    pub payload_bytes: usize,
    pub batch_container_bytes: usize,
    pub dedup_entry_bytes: usize,
    pub commit_clone_payload_bytes: usize,
    pub emitted_by_map: BTreeMap<MapKind, u64>,
}

impl From<StageError> for DurableError {
    fn from(error: StageError) -> Self {
        Self::ResourceLimit(error.to_string())
    }
}

fn add(a: usize, b: usize) -> Result<usize, DurableError> {
    a.checked_add(b)
        .ok_or_else(|| DurableError::ResourceLimit("encoded size overflow".into()))
}
fn blob_size(blob: &Blob) -> Result<usize, DurableError> {
    u32::try_from(blob.0.len())
        .map_err(|_| DurableError::ResourceLimit("blob exceeds canonical u32 length".into()))?;
    add(4, blob.0.len())
}
fn row_size(row: &StoredRecord) -> Result<usize, DurableError> {
    let mut bytes = add(5, row.key.as_bytes().len())?;
    u32::try_from(row.cells.len())
        .map_err(|_| DurableError::ResourceLimit("cell count width".into()))?;
    for (name, cell) in row.cells.iter() {
        bytes = add(bytes, 4 + name.as_str().len() + 2)?;
        bytes = add(
            bytes,
            match &cell.value {
                Value::Str(value) => {
                    u32::try_from(value.len())
                        .map_err(|_| DurableError::ResourceLimit("string width".into()))?;
                    add(4, value.len())?
                }
                Value::Bytes(value) => {
                    u32::try_from(value.len())
                        .map_err(|_| DurableError::ResourceLimit("value width".into()))?;
                    add(4, value.len())?
                }
                fixed => fixed.encoded_len(),
            },
        )?;
    }
    Ok(bytes)
}

fn namespace_preflight(
    leaf: &NamespaceState,
    cap: usize,
    budget: &mut StageBudget,
) -> Result<usize, DurableError> {
    leaf.records
        .preflight_with(cap, budget, &mut |blob, _| blob_size(blob))?;
    leaf.typed
        .keys
        .preflight_with(cap, budget, &mut |_, _| Ok::<_, DurableError>(8))?;
    leaf.typed
        .rows
        .preflight_with(cap, budget, &mut |row, _| row_size(row))?;
    leaf.typed
        .terms
        .preflight_with(cap, budget, &mut |posting: &Posting, budget| {
            posting
                .0
                .preflight_with(cap, budget, &mut |_, _| Ok::<_, DurableError>(1))?;
            Ok::<_, DurableError>(40)
        })?;
    Ok(208 + leaf.descriptor.name.as_str().len())
}

pub(crate) fn preflight(
    state: &WorkingState,
    cap: usize,
    max_visits: u64,
) -> Result<u64, DurableError> {
    let mut budget = StageBudget::new(max_visits);
    state
        .catalog
        .preflight_with(cap, &mut budget, &mut |leaf, budget| {
            namespace_preflight(leaf, cap, budget)
        })?;
    state
        .host
        .preflight_with(cap, &mut budget, &mut |blob, _| blob_size(blob))?;
    Ok(budget.visited())
}

pub(crate) struct Staging {
    pub(crate) objects: Vec<(SpaceId, ObjectId, Vec<u8>)>,
    pub(crate) stats: PreparationStats,
    seen: BTreeMap<(SpaceId, ObjectId), usize>,
    space: SpaceId,
    map: MapKind,
    options: Options,
}

impl Staging {
    pub(crate) fn collect(
        state: &WorkingState,
        options: &Options,
        reserved_bytes: usize,
        preflight_visits: u64,
    ) -> Result<Self, DurableError> {
        if reserved_bytes > options.prepare_bytes {
            return Err(DurableError::ResourceLimit(
                "manifest/record bytes exceed preparation cap".into(),
            ));
        }
        let mut sink = Self {
            objects: Vec::new(),
            stats: PreparationStats {
                payload_bytes: reserved_bytes,
                preflight_visits,
                ..PreparationStats::default()
            },
            seen: BTreeMap::new(),
            space: SpaceId::Catalog,
            map: MapKind::Catalog,
            options: options.clone(),
        };
        let mut budget = StageBudget::new(options.prepare_visits);
        state
            .catalog
            .try_stage_with(&mut sink, &mut budget, &mut |leaf, sink, budget| {
                let space = SpaceId::Namespace(leaf.descriptor.id.ordinal());
                sink.simple(&leaf.records, space, MapKind::Records, budget)?;
                sink.simple(&leaf.typed.keys, space, MapKind::Keys, budget)?;
                sink.simple(&leaf.typed.rows, space, MapKind::Rows, budget)?;
                sink.space = space;
                sink.map = MapKind::Terms;
                leaf.typed.terms.try_stage_with(
                    sink,
                    budget,
                    &mut |posting: &Posting, sink, budget| {
                        sink.simple(&posting.0, space, MapKind::Postings, budget)?;
                        sink.space = space;
                        sink.map = MapKind::Terms;
                        Ok(())
                    },
                )?;
                sink.space = SpaceId::Catalog;
                sink.map = MapKind::Catalog;
                Ok(())
            })?;
        sink.simple(&state.host, SpaceId::Host, MapKind::Host, &mut budget)?;
        sink.stats.staging_visits = budget.visited();
        sink.stats.unique_objects = sink.objects.len();
        sink.stats.batch_container_bytes =
            sink.objects.capacity() * std::mem::size_of::<(SpaceId, ObjectId, Vec<u8>)>();
        // BTreeMap has allocator/node overhead in addition to these payloads;
        // it is temporary and bounded by prepare_objects, never by history.
        sink.stats.dedup_entry_bytes =
            sink.seen.len() * std::mem::size_of::<((SpaceId, ObjectId), usize)>();
        Ok(sink)
    }

    fn simple<K: TrieKey, V: TrieValue>(
        &mut self,
        map: &MerkleMap<K, V>,
        space: SpaceId,
        kind: MapKind,
        budget: &mut StageBudget,
    ) -> Result<(), DurableError> {
        self.space = space;
        self.map = kind;
        map.try_stage_with(self, budget, &mut |_, _, _| Ok(()))
    }
}

impl TryObjectSink for Staging {
    type Error = DurableError;
    fn record(&mut self, id: crate::B256, bytes: &[u8]) -> Result<(), DurableError> {
        *self.stats.emitted_by_map.entry(self.map).or_default() += 1;
        let key = (self.space, id.0);
        if let Some(index) = self.seen.get(&key) {
            if self.objects[*index].2 != bytes {
                return Err(super::corrupt("conflicting bytes for staged object hash"));
            }
            return Ok(());
        }
        if self.objects.len() >= self.options.prepare_objects {
            return Err(DurableError::ResourceLimit("staged object cap".into()));
        }
        let payload = add(self.stats.payload_bytes, add(32, bytes.len())?)?;
        if payload > self.options.prepare_bytes {
            return Err(DurableError::ResourceLimit("staged byte cap".into()));
        }
        self.seen.insert(key, self.objects.len());
        if self.objects.len() == self.objects.capacity() {
            self.objects.reserve_exact(1);
        }
        self.objects.push((self.space, id.0, bytes.to_vec()));
        self.stats.payload_bytes = payload;
        Ok(())
    }
}
