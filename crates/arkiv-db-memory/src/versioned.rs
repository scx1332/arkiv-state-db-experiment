//! The volatile reference implementation of the versioned storage contract.

use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard},
};

use arkiv_db_storage::{
    gc::{
        self, DeadEntry, GcCommit, GcError, GcKey, GcRead, GcReport, GcState, GcTables, GcWrite,
        ObjectGraph, QueueKey, Seeder, gc_key,
    },
    versioned::{
        CommitBatch, CommitOutcome, DatabaseId, GcDump, Head, Health, ObjectId, ObjectReader,
        Relabel, SpaceId, StatsRecorder, StorageError, StoreStats, Table, VersionedStore, Written,
        copy_bounded, gc_write_refused, head_mismatch, object_conflict, record_conflict,
        relabel_mismatch, validate_objects,
    },
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
    /// Garbage collection, once enabled.
    gc: Option<(GcTables, GcState)>,
    gc_report: Option<GcReport>,
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

    fn commit(&self, batch: &CommitBatch) -> CommitOutcome {
        if self.read().gc.is_some() {
            return self
                .stats
                .write(Err(CommitOutcome::NotCommitted(gc_write_refused(
                    "a plain commit",
                ))));
        }
        self.stats
            .write(self.apply(batch).map_err(CommitOutcome::NotCommitted))
    }

    fn stage_objects(&self, objects: &[(SpaceId, ObjectId, Vec<u8>)]) -> CommitOutcome {
        if self.read().gc.is_some() {
            return CommitOutcome::NotCommitted(gc_write_refused("object staging"));
        }
        // A refused staging is not counted.
        match self.apply_objects(objects) {
            Ok(written) => self.stats.write(Ok(written)),
            Err(error) => CommitOutcome::NotCommitted(error),
        }
    }

    fn relabel(&self, relabel: &Relabel) -> CommitOutcome {
        if self.read().gc.is_some() {
            return CommitOutcome::NotCommitted(gc_write_refused("relabelling"));
        }
        self.stats.write(
            self.apply_relabel(relabel)
                .map_err(CommitOutcome::NotCommitted),
        )
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

    fn gc_state(&self) -> Result<Option<GcState>, StorageError> {
        Ok(self.read().gc.as_ref().map(|(_, state)| *state))
    }

    fn commit_gc(
        &self,
        batch: &CommitBatch,
        gc: &GcCommit,
        graph: &dyn ObjectGraph,
    ) -> CommitOutcome {
        self.stats.write(
            self.apply_gc(batch, gc, graph)
                .map_err(CommitOutcome::NotCommitted),
        )
    }

    fn gc_report(&self) -> Option<GcReport> {
        self.read().gc_report
    }

    fn gc_enable(
        &self,
        roots: &[(SpaceId, ObjectId)],
        graph: &dyn ObjectGraph,
        spill: Option<&std::path::Path>,
    ) -> Result<GcState, StorageError> {
        let mut state = self.write();
        let head = state.head.ok_or_else(|| {
            StorageError::new(
                arkiv_db_storage::versioned::StorageErrorKind::Incompatible,
                "garbage collection needs a head",
            )
        })?;
        let mut seeder = Seeder::new(graph, roots, spill)?;
        for (space, objects) in &state.objects {
            for (id, bytes) in objects {
                seeder.object(*space, id, bytes).map_err(gc_storage)?;
            }
        }
        let epoch = state.gc.as_ref().map_or(1, |(_, s)| s.epoch + 1);
        let mut tables = GcTables::default();
        let seeded = seeder.finish(&mut tables, epoch).map_err(gc_storage)?;
        tables.epochs.insert(head.height, epoch);
        state.gc = Some((tables, seeded));
        Ok(seeded)
    }

    fn gc_dump(&self) -> Result<Option<GcDump>, StorageError> {
        let state = self.read();
        let Some((tables, gc)) = &state.gc else {
            return Ok(None);
        };
        let mut objects = BTreeMap::new();
        for (space, stored) in &state.objects {
            for (id, bytes) in stored {
                objects.insert(gc_key(*space, id), bytes.to_vec());
            }
        }
        Ok(Some(GcDump {
            objects,
            tables: tables.clone(),
            state: *gc,
        }))
    }
}

/// An invariant failure while seeding is damage to the store.
fn gc_storage(error: GcError) -> StorageError {
    match error {
        GcError::Storage(error) => error,
        GcError::Invariant(detail) => StorageError::new(
            arkiv_db_storage::versioned::StorageErrorKind::Corrupt,
            format!("gc seeding: {detail}"),
        ),
    }
}

/// The objects and GC tables of a memory store, as the planner reads and
/// writes them.
struct MemoryGc<'a> {
    objects: &'a mut BTreeMap<SpaceId, HashMap<ObjectId, Arc<[u8]>>>,
    tables: &'a mut GcTables,
}

impl GcRead for MemoryGc<'_> {
    fn object_matches(
        &mut self,
        space: SpaceId,
        id: &ObjectId,
        bytes: &[u8],
    ) -> Result<Option<bool>, StorageError> {
        Ok(self
            .objects
            .get(&space)
            .and_then(|o| o.get(id))
            .map(|stored| **stored == *bytes))
    }
    fn object(&mut self, space: SpaceId, id: &ObjectId) -> Result<Option<Vec<u8>>, StorageError> {
        Ok(self
            .objects
            .get(&space)
            .and_then(|o| o.get(id))
            .map(|b| b.to_vec()))
    }
    fn shared(&mut self, key: &GcKey) -> Result<Option<u64>, StorageError> {
        Ok(self.tables.shared.get(key).copied())
    }
    fn dead(&mut self, key: &GcKey) -> Result<Option<DeadEntry>, StorageError> {
        Ok(self.tables.dead.get(key).copied())
    }
    fn phantom(&mut self, key: &GcKey) -> Result<Option<u64>, StorageError> {
        Ok(self.tables.phantom.get(key).copied())
    }
    fn epoch_at(&mut self, height: u64) -> Result<Option<(u64, u64)>, StorageError> {
        Ok(self.tables.epoch_at(height))
    }
    fn epochs_below(&mut self, height: u64) -> Result<Vec<u64>, StorageError> {
        Ok(self.tables.epochs_below(height))
    }
    fn queue_next(&mut self, after: Option<&QueueKey>) -> Result<Option<QueueKey>, StorageError> {
        Ok(self.tables.queue_next(after))
    }
    fn release_prev(&mut self, below: u64) -> Result<Option<(u64, GcKey)>, StorageError> {
        Ok(self.tables.release_prev(below))
    }
    fn epoch_counts_through(&mut self, epoch: u64) -> Result<Vec<(u64, u64)>, StorageError> {
        Ok(self.tables.epoch_counts_through(epoch))
    }
    fn epoch_count(&mut self, epoch: u64) -> Result<Option<u64>, StorageError> {
        Ok(self.tables.epoch_counts.get(&epoch).copied())
    }
}

impl GcWrite for MemoryGc<'_> {
    fn insert_object(
        &mut self,
        space: SpaceId,
        id: &ObjectId,
        bytes: &[u8],
    ) -> Result<(), StorageError> {
        self.objects
            .entry(space)
            .or_default()
            .insert(*id, bytes.into());
        Ok(())
    }
    fn remove_object(&mut self, space: SpaceId, id: &ObjectId) -> Result<(), StorageError> {
        if let Some(objects) = self.objects.get_mut(&space) {
            objects.remove(id);
        }
        Ok(())
    }
    fn set_shared(&mut self, key: &GcKey, count: Option<u64>) -> Result<(), StorageError> {
        self.tables.set_shared(key, count);
        Ok(())
    }
    fn set_dead(&mut self, key: &GcKey, entry: Option<DeadEntry>) -> Result<(), StorageError> {
        self.tables.set_dead(key, entry);
        Ok(())
    }
    fn set_phantom(&mut self, key: &GcKey, count: Option<u64>) -> Result<(), StorageError> {
        self.tables.set_phantom(key, count);
        Ok(())
    }
    fn set_queue(&mut self, key: &QueueKey, present: bool) -> Result<(), StorageError> {
        self.tables.set_queue(key, present);
        Ok(())
    }
    fn set_release(&mut self, seq: u64, key: Option<&GcKey>) -> Result<(), StorageError> {
        self.tables.set_release(seq, key);
        Ok(())
    }
    fn set_epoch(&mut self, height: u64, epoch: Option<u64>) -> Result<(), StorageError> {
        self.tables.set_epoch(height, epoch);
        Ok(())
    }
    fn set_epoch_count(&mut self, epoch: u64, count: Option<u64>) -> Result<(), StorageError> {
        self.tables.set_epoch_count(epoch, count);
        Ok(())
    }
}

impl MemoryVersionedStore {
    /// Checks the whole batch against the current state, then applies it.
    fn apply(&self, batch: &CommitBatch) -> Result<Written, StorageError> {
        batch.validate()?;
        self.write().put(
            batch.expected_head,
            &batch.objects,
            &batch.records,
            Some(batch.new_head),
        )
    }

    /// Applies a batch with garbage collection: everything is planned from
    /// reads first, then applied in infallible map operations.
    fn apply_gc(
        &self,
        batch: &CommitBatch,
        gc: &GcCommit,
        graph: &dyn ObjectGraph,
    ) -> Result<Written, StorageError> {
        batch.validate()?;
        let mut guard = self.write();
        let state = &mut *guard;
        let Some((tables, gc_state)) = state.gc.as_mut() else {
            return Err(StorageError::new(
                arkiv_db_storage::versioned::StorageErrorKind::Incompatible,
                "this store does not collect garbage",
            ));
        };
        if gc_state.suspended {
            let (head, objects, records) =
                (&mut state.head, &mut state.objects, &mut state.records);
            let mut plain = MemoryState {
                head: *head,
                objects: std::mem::take(objects),
                records: std::mem::take(records),
                gc: None,
                gc_report: None,
            };
            let result = plain.put(
                batch.expected_head,
                &batch.objects,
                &batch.records,
                Some(batch.new_head),
            );
            *head = plain.head;
            *objects = plain.objects;
            *records = plain.records;
            return result;
        }
        if state.head != batch.expected_head {
            return Err(head_mismatch(batch.expected_head, state.head));
        }
        for (table, key, value) in &batch.records {
            if let Some(existing) = state.records.get(&(*table, key.clone()))
                && existing != value
            {
                return Err(record_conflict(*table, key));
            }
        }
        let current = *gc_state;
        let planned = {
            let mut view = MemoryGc {
                objects: &mut state.objects,
                tables,
            };
            gc::plan(
                &mut view,
                graph,
                &current,
                &batch.objects,
                gc,
                batch.new_head.height,
            )
        };
        let plan = match planned {
            Ok(plan) => plan,
            Err(GcError::Storage(error)) => return Err(error),
            Err(GcError::Invariant(_)) => {
                // Commit without collection and stop counting.
                let (tables, gc_state) = state.gc.take().expect("checked");
                let mut suspended = gc_state;
                suspended.suspended = true;
                let result = state.put(
                    batch.expected_head,
                    &batch.objects,
                    &batch.records,
                    Some(batch.new_head),
                );
                state.gc = Some((tables, suspended));
                return result;
            }
        };
        let mut written = Written::default();
        for (space, _, bytes) in &plan.inserts {
            written.objects.push((*space, bytes.len()));
        }
        plan.apply(&mut MemoryGc {
            objects: &mut state.objects,
            tables,
        })?;
        *gc_state = plan.state;
        state.gc_report = Some(plan.report);
        for (table, key, value) in &batch.records {
            if let std::collections::btree_map::Entry::Vacant(slot) =
                state.records.entry((*table, key.clone()))
            {
                written.records += 1;
                slot.insert(value.clone());
            }
        }
        state.head = Some(batch.new_head);
        Ok(written)
    }

    /// Puts objects into a store that has no head, leaving it headless.
    fn apply_objects(
        &self,
        objects: &[(SpaceId, ObjectId, Vec<u8>)],
    ) -> Result<Written, StorageError> {
        validate_objects(objects)?;
        self.write().put(None, objects, &[], None)
    }

    /// Checks the retired records and the head, then replaces them with the
    /// batch under one lock (see [`Relabel`]).
    fn apply_relabel(&self, relabel: &Relabel) -> Result<Written, StorageError> {
        relabel.validate()?;
        let mut state = self.write();
        if state.head != Some(relabel.expected_head) {
            return Err(head_mismatch(Some(relabel.expected_head), state.head));
        }
        if state.records.len() != relabel.retired.len() {
            return Err(relabel_mismatch(format!(
                "{} records, {} retired",
                state.records.len(),
                relabel.retired.len()
            )));
        }
        for (table, key, value) in &relabel.retired {
            if state.records.get(&(*table, key.clone())) != Some(value) {
                return Err(relabel_mismatch(format!("{} record differs", table.name())));
            }
        }
        // The batch is checked against the emptied tables before anything
        // changes, so a refusal leaves the store as it was.
        let mut relabelled = MemoryState {
            head: None,
            objects: std::mem::take(&mut state.objects),
            records: BTreeMap::new(),
            gc: None,
            gc_report: None,
        };
        let batch = &relabel.batch;
        let result = relabelled.put(None, &batch.objects, &batch.records, Some(batch.new_head));
        if result.is_err() {
            state.objects = relabelled.objects;
        } else {
            *state = relabelled;
        }
        result
    }
}

impl MemoryState {
    /// Checks the entries against `expected_head` and the stored values,
    /// then stores the new ones and sets `new_head` if there is one.
    fn put(
        &mut self,
        expected_head: Option<Head>,
        objects: &[(SpaceId, ObjectId, Vec<u8>)],
        records: &[(Table, Vec<u8>, Vec<u8>)],
        new_head: Option<Head>,
    ) -> Result<Written, StorageError> {
        if self.head != expected_head {
            return Err(head_mismatch(expected_head, self.head));
        }
        for (space, id, bytes) in objects {
            if let Some(existing) = self.objects.get(space).and_then(|objects| objects.get(id))
                && **existing != **bytes
            {
                return Err(object_conflict(*space, id));
            }
        }
        for (table, key, value) in records {
            if let Some(existing) = self.records.get(&(*table, key.clone()))
                && existing != value
            {
                return Err(record_conflict(*table, key));
            }
        }
        // Nothing below can fail: the entries apply completely.
        let mut written = Written::default();
        for (space, id, bytes) in objects {
            let stored = self.objects.entry(*space).or_default();
            if let std::collections::hash_map::Entry::Vacant(slot) = stored.entry(*id) {
                written.objects.push((*space, bytes.len()));
                slot.insert(bytes.as_slice().into());
            }
        }
        for (table, key, value) in records {
            if let std::collections::btree_map::Entry::Vacant(slot) =
                self.records.entry((*table, key.clone()))
            {
                written.records += 1;
                slot.insert(value.clone());
            }
        }
        if new_head.is_some() {
            self.head = new_head;
        }
        Ok(written)
    }
}
