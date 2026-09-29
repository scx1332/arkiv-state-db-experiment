//! The volatile reference implementation of the versioned storage contract.

use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard},
};

use arkiv_db_storage::versioned::{
    CommitBatch, CommitOutcome, DatabaseId, Head, Health, ObjectId, ObjectReader, SpaceId,
    StatsRecorder, StorageError, StorageErrorKind, StoreStats, Table, VersionedStore,
    head_mismatch, object_conflict, record_conflict,
};

/// A [`VersionedStore`] held entirely in process memory.
///
/// It implements the whole contract — put-if-absent objects per space,
/// insert-only records, the head compare-and-set and atomic batches — under
/// one lock, and serves as the reference the durable stores are compared
/// with. It has no I/O failures: health is always `Available`, and `recover`
/// only returns the head. Bounded reads still reject oversized values.
/// Everything is lost when the store is dropped; it does
/// not verify content addresses, because nothing can damage its bytes.
#[derive(Debug)]
pub struct MemoryVersionedStore {
    id: DatabaseId,
    state: RwLock<MemoryState>,
    stats: StatsRecorder,
}

#[derive(Debug, Default)]
struct MemoryState {
    head: Option<Head>,
    objects: BTreeMap<SpaceId, HashMap<ObjectId, Arc<[u8]>>>,
    records: BTreeMap<(Table, Vec<u8>), Vec<u8>>,
}

impl MemoryVersionedStore {
    /// Creates an empty store with a random [`DatabaseId`].
    pub fn new() -> Self {
        Self::with_database_id(DatabaseId::random())
    }

    /// Creates an empty store with the given identity.
    pub fn with_database_id(id: DatabaseId) -> Self {
        Self {
            id,
            state: RwLock::new(MemoryState::default()),
            stats: StatsRecorder::new(),
        }
    }

    fn read(&self) -> RwLockReadGuard<'_, MemoryState> {
        // Every mutation happens after all checks, in infallible map
        // operations, so a poisoned lock still guards consistent state.
        self.state.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> RwLockWriteGuard<'_, MemoryState> {
        self.state.write().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Default for MemoryVersionedStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ObjectReader for MemoryVersionedStore {
    fn read_object(&self, space: SpaceId, id: &ObjectId) -> Result<Option<Vec<u8>>, StorageError> {
        self.read_object_bounded(space, id, usize::MAX)
    }

    fn read_object_bounded(
        &self,
        space: SpaceId,
        id: &ObjectId,
        max_bytes: usize,
    ) -> Result<Option<Vec<u8>>, StorageError> {
        let found = self
            .read()
            .objects
            .get(&space)
            .and_then(|objects| objects.get(id))
            .map(|bytes| copy_bounded(bytes, max_bytes))
            .transpose()?;
        self.stats.object_read(space, found.as_ref().map(Vec::len));
        Ok(found)
    }
}

impl VersionedStore for MemoryVersionedStore {
    fn database_id(&self) -> DatabaseId {
        self.id
    }

    fn head(&self) -> Result<Option<Head>, StorageError> {
        let head = self.read().head;
        self.stats.head_read();
        Ok(head)
    }

    fn read_record(&self, table: Table, key: &[u8]) -> Result<Option<Vec<u8>>, StorageError> {
        self.read_record_bounded(table, key, usize::MAX)
    }

    fn read_record_bounded(
        &self,
        table: Table,
        key: &[u8],
        max_bytes: usize,
    ) -> Result<Option<Vec<u8>>, StorageError> {
        let found = self
            .read()
            .records
            .get(&(table, key.to_vec()))
            .map(|bytes| copy_bounded(bytes, max_bytes))
            .transpose()?;
        self.stats.record_read(found.as_ref().map(Vec::len));
        Ok(found)
    }

    fn commit(&self, batch: CommitBatch) -> CommitOutcome {
        let outcome = self.apply(batch);
        match outcome {
            Ok((new_objects, new_records)) => {
                let outcome = CommitOutcome::Committed;
                self.stats.commit(&outcome, &new_objects, new_records);
                outcome
            }
            Err(error) => {
                let outcome = CommitOutcome::NotCommitted(error);
                self.stats.commit(&outcome, &[], 0);
                outcome
            }
        }
    }

    fn health(&self) -> Health {
        Health::Available
    }

    fn recover(&self) -> Result<Option<Head>, StorageError> {
        let head = self.read().head;
        self.stats.recovered();
        Ok(head)
    }

    fn stats(&self) -> StoreStats {
        self.stats.snapshot(None)
    }
}

/// Checks the borrowed stored value before allocating its returned copy.
fn copy_bounded(bytes: &[u8], max_bytes: usize) -> Result<Vec<u8>, StorageError> {
    if bytes.len() > max_bytes {
        return Err(StorageError::new(
            StorageErrorKind::Corrupt,
            format!(
                "stored value has {} bytes, exceeds read limit {max_bytes}",
                bytes.len()
            ),
        ));
    }
    Ok(bytes.to_vec())
}

impl MemoryVersionedStore {
    /// Checks the whole batch against the current state, then applies it.
    /// Returns the objects and the number of records newly stored.
    fn apply(&self, batch: CommitBatch) -> Result<(Vec<(SpaceId, usize)>, u64), StorageError> {
        batch.validate()?;
        let mut state = self.write();
        if state.head != batch.expected_head {
            return Err(head_mismatch(batch.expected_head, state.head));
        }
        for (space, id, bytes) in &batch.objects {
            if let Some(existing) = state.objects.get(space).and_then(|objects| objects.get(id))
                && **existing != **bytes
            {
                return Err(object_conflict(*space, id));
            }
        }
        for (table, key, value) in &batch.records {
            if let Some(existing) = state.records.get(&(*table, key.clone()))
                && existing != value
            {
                return Err(record_conflict(*table, key));
            }
        }
        // Nothing below can fail: the batch applies completely.
        let mut new_objects = Vec::new();
        for (space, id, bytes) in batch.objects {
            let objects = state.objects.entry(space).or_default();
            if let std::collections::hash_map::Entry::Vacant(slot) = objects.entry(id) {
                new_objects.push((space, bytes.len()));
                slot.insert(bytes.into());
            }
        }
        let mut new_records = 0;
        for (table, key, value) in batch.records {
            if let std::collections::btree_map::Entry::Vacant(slot) =
                state.records.entry((table, key))
            {
                new_records += 1;
                slot.insert(value);
            }
        }
        state.head = Some(batch.new_head);
        Ok((new_objects, new_records))
    }
}

#[cfg(test)]
mod tests {
    use arkiv_db_storage::versioned::{StorageErrorKind, height_key};

    use super::*;

    fn head(height: u64) -> Head {
        Head {
            height,
            hash: [height as u8 + 1; 32],
        }
    }

    fn kind(outcome: CommitOutcome) -> Option<StorageErrorKind> {
        match outcome {
            CommitOutcome::Committed => None,
            CommitOutcome::NotCommitted(error) | CommitOutcome::Indeterminate(error) => {
                Some(error.kind)
            }
        }
    }

    #[test]
    fn objects_are_put_if_absent_per_space() {
        let store = MemoryVersionedStore::new();
        let mut first = CommitBatch::new(None, head(0));
        first.put_object(SpaceId::Namespace(1), [1; 32], b"one".to_vec());
        assert_eq!(store.commit(first), CommitOutcome::Committed);

        // Same bytes again: idempotent, stored once.
        let mut again = CommitBatch::new(Some(head(0)), head(1));
        again
            .put_object(SpaceId::Namespace(1), [1; 32], b"one".to_vec())
            .put_object(SpaceId::Namespace(2), [1; 32], b"other space".to_vec());
        assert_eq!(store.commit(again), CommitOutcome::Committed);
        let stats = store.stats();
        assert_eq!(stats.space(SpaceId::Namespace(1)).objects_written, 1);
        assert_eq!(stats.space(SpaceId::Namespace(2)).objects_written, 1);
        assert_eq!(
            store.read_object(SpaceId::Namespace(1), &[1; 32]).unwrap(),
            Some(b"one".to_vec())
        );
        assert_eq!(
            store.read_object(SpaceId::Namespace(2), &[1; 32]).unwrap(),
            Some(b"other space".to_vec())
        );

        // Different bytes under an existing id: the whole batch is refused.
        let mut conflict = CommitBatch::new(Some(head(1)), head(2));
        conflict
            .put_object(SpaceId::Host, [2; 32], b"new".to_vec())
            .put_object(SpaceId::Namespace(1), [1; 32], b"changed".to_vec());
        assert_eq!(
            kind(store.commit(conflict)),
            Some(StorageErrorKind::Conflict)
        );
        assert_eq!(store.read_object(SpaceId::Host, &[2; 32]).unwrap(), None);
        assert_eq!(store.head().unwrap(), Some(head(1)));
    }

    #[test]
    fn records_are_insert_only() {
        let store = MemoryVersionedStore::new();
        let mut first = CommitBatch::new(None, head(0));
        first.put_record(Table::Blocks, height_key(0), b"b0".to_vec());
        assert_eq!(store.commit(first), CommitOutcome::Committed);

        let mut same = CommitBatch::new(Some(head(0)), head(1));
        same.put_record(Table::Blocks, height_key(0), b"b0".to_vec());
        assert_eq!(store.commit(same), CommitOutcome::Committed);
        assert_eq!(store.stats().records_written, 1);

        let mut different = CommitBatch::new(Some(head(1)), head(2));
        different
            .put_record(Table::Blocks, height_key(2), b"b2".to_vec())
            .put_record(Table::Blocks, height_key(0), b"rewritten".to_vec());
        assert_eq!(
            kind(store.commit(different)),
            Some(StorageErrorKind::Conflict)
        );
        assert_eq!(
            store.read_record(Table::Blocks, &height_key(0)).unwrap(),
            Some(b"b0".to_vec())
        );
        assert_eq!(
            store.read_record(Table::Blocks, &height_key(2)).unwrap(),
            None
        );
    }

    #[test]
    fn head_is_compare_and_set() {
        let store = MemoryVersionedStore::new();
        assert_eq!(
            kind(store.commit(CommitBatch::new(Some(head(0)), head(1)))),
            Some(StorageErrorKind::HeadMismatch)
        );
        assert_eq!(
            store.commit(CommitBatch::new(None, head(0))),
            CommitOutcome::Committed
        );
        assert_eq!(
            kind(store.commit(CommitBatch::new(None, head(1)))),
            Some(StorageErrorKind::HeadMismatch)
        );
        assert_eq!(
            store.commit(CommitBatch::new(Some(head(0)), head(1))),
            CommitOutcome::Committed
        );
        assert_eq!(
            kind(store.commit(CommitBatch::new(Some(head(0)), head(2)))),
            Some(StorageErrorKind::HeadMismatch)
        );
        assert_eq!(store.head().unwrap(), Some(head(1)));
        let stats = store.stats();
        assert_eq!((stats.committed, stats.not_committed), (2, 3));
    }

    #[test]
    fn identical_retry_is_never_applied_twice() {
        let store = MemoryVersionedStore::new();
        let mut batch = CommitBatch::new(None, head(0));
        batch
            .put_object(SpaceId::Catalog, [5; 32], b"catalog".to_vec())
            .put_record(Table::Inclusions, b"request".to_vec(), b"0".to_vec());
        assert_eq!(store.commit(batch.clone()), CommitOutcome::Committed);
        assert_eq!(
            kind(store.commit(batch)),
            Some(StorageErrorKind::HeadMismatch)
        );
        let stats = store.stats();
        assert_eq!(stats.space(SpaceId::Catalog).objects_written, 1);
        assert_eq!(stats.records_written, 1);
    }

    #[test]
    fn identity_is_fixed_and_health_never_fails() {
        let id = DatabaseId([9; 16]);
        let store = MemoryVersionedStore::with_database_id(id);
        assert_eq!(store.database_id(), id);
        assert_eq!(store.health(), Health::Available);
        assert_eq!(store.recover().unwrap(), None);
        assert_eq!(store.stats().recoveries, 1);
        assert_ne!(
            MemoryVersionedStore::new().database_id(),
            MemoryVersionedStore::new().database_id()
        );
    }
}
