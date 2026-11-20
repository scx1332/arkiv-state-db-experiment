//! Garbage collection in a redb file: the GC tables, the planner's view of
//! one write transaction, and seeding.
//!
//! | Table | Key → value |
//! | --- | --- |
//! | `gc_shared` | `space ‖ id` (41 bytes) → count, only for counts of two or more |
//! | `gc_dead` | `space ‖ id` → `epoch ‖ release seq ‖ size` ([`DeadEntry`]) |
//! | `gc_phantom` | `space ‖ id` → speculative references to an absent id |
//! | `gc_queue` | `epoch ‖ space ‖ id` → `()`: released dead objects by death epoch |
//! | `gc_release` | release seq → `space ‖ id`: dead objects whose edges are not released |
//! | `gc_epochs` | height → epoch, for the retained heights |
//! | `gc_epoch_counts` | epoch → queue entries of that epoch |
//!
//! `meta` holds `store/gc/state` ([`GcState`]) and `store/format` is
//! [`GC_STORE_FORMAT`], so a binary without garbage collection refuses the
//! file instead of adding objects nobody counts.

use std::{collections::HashMap, ops::Bound};

use arkiv_db_storage::{
    gc::{
        DeadEntry, GC_KEY_LEN, GC_STATE_KEY, GC_STORE_FORMAT, GcKey, GcRead, GcState, GcWrite,
        QueueKey, SeedSink,
    },
    versioned::{ObjectId, SpaceId, StorageError},
};
use redb::{ReadableTable, Table, TableDefinition, TableHandle, WriteTransaction};

use crate::{
    error::Fault,
    store::{FORMAT_KEY, META, STORE_FORMAT_VERSION, object_table, object_table_name},
};

type Key41 = &'static [u8; GC_KEY_LEN];

pub(crate) const GC_SHARED: TableDefinition<Key41, u64> = TableDefinition::new("gc_shared");
pub(crate) const GC_DEAD: TableDefinition<Key41, &[u8; DeadEntry::ENCODED_LEN]> =
    TableDefinition::new("gc_dead");
pub(crate) const GC_PHANTOM: TableDefinition<Key41, u64> = TableDefinition::new("gc_phantom");
pub(crate) const GC_QUEUE: TableDefinition<&[u8; 8 + GC_KEY_LEN], ()> =
    TableDefinition::new("gc_queue");
pub(crate) const GC_RELEASE: TableDefinition<u64, Key41> = TableDefinition::new("gc_release");
pub(crate) const GC_EPOCHS: TableDefinition<u64, u64> = TableDefinition::new("gc_epochs");
pub(crate) const GC_EPOCH_COUNTS: TableDefinition<u64, u64> =
    TableDefinition::new("gc_epoch_counts");

/// Every GC table name, for deletion before a rebuild.
pub(crate) const GC_TABLES: [&str; 7] = [
    "gc_shared",
    "gc_dead",
    "gc_phantom",
    "gc_queue",
    "gc_release",
    "gc_epochs",
    "gc_epoch_counts",
];

/// The GC state recorded in `meta`, if the store collects garbage.
pub(crate) fn read_state(txn: &WriteTransaction) -> Result<Option<GcState>, Fault> {
    let meta = txn.open_table(META)?;
    let found = meta.get(GC_STATE_KEY)?;
    Ok(match found {
        Some(bytes) => Some(GcState::decode(bytes.value())?),
        None => None,
    })
}

/// Records `state` and the GC store format in `meta`.
pub(crate) fn write_state(txn: &WriteTransaction, state: &GcState) -> Result<(), Fault> {
    let mut meta = txn.open_table(META)?;
    meta.insert(GC_STATE_KEY, state.encode().as_slice())?;
    meta.insert(FORMAT_KEY, GC_STORE_FORMAT.to_be_bytes().as_slice())?;
    Ok(())
}

/// Removes every GC table and record: the store becomes a plain one.
pub(crate) fn clear(txn: &WriteTransaction) -> Result<(), Fault> {
    for name in GC_TABLES {
        let definition: TableDefinition<&[u8], &[u8]> = TableDefinition::new(name);
        txn.delete_table(definition)?;
    }
    let mut meta = txn.open_table(META)?;
    meta.remove(GC_STATE_KEY)?;
    meta.insert(FORMAT_KEY, STORE_FORMAT_VERSION.to_be_bytes().as_slice())?;
    Ok(())
}

fn key41(key: &GcKey) -> &GcKey {
    key
}

/// The planner's view of one write transaction: every GC table and the
/// object tables it touches, open together.
pub(crate) struct TxnGc<'t> {
    txn: &'t WriteTransaction,
    objects: HashMap<SpaceId, Table<'t, &'static [u8; 32], &'static [u8]>>,
    shared: Table<'t, Key41, u64>,
    dead: Table<'t, Key41, &'static [u8; DeadEntry::ENCODED_LEN]>,
    phantom: Table<'t, Key41, u64>,
    queue: Table<'t, &'static [u8; 8 + GC_KEY_LEN], ()>,
    release: Table<'t, u64, Key41>,
    epochs: Table<'t, u64, u64>,
    epoch_counts: Table<'t, u64, u64>,
}

impl<'t> TxnGc<'t> {
    pub(crate) fn open(txn: &'t WriteTransaction) -> Result<Self, Fault> {
        Ok(Self {
            txn,
            objects: HashMap::new(),
            shared: txn.open_table(GC_SHARED)?,
            dead: txn.open_table(GC_DEAD)?,
            phantom: txn.open_table(GC_PHANTOM)?,
            queue: txn.open_table(GC_QUEUE)?,
            release: txn.open_table(GC_RELEASE)?,
            epochs: txn.open_table(GC_EPOCHS)?,
            epoch_counts: txn.open_table(GC_EPOCH_COUNTS)?,
        })
    }

    fn objects(
        &mut self,
        space: SpaceId,
    ) -> Result<&mut Table<'t, &'static [u8; 32], &'static [u8]>, StorageError> {
        if !self.objects.contains_key(&space) {
            let name = object_table_name(space);
            let table = self
                .txn
                .open_table(object_table(&name))
                .map_err(|e| Fault::from(e).0)?;
            self.objects.insert(space, table);
        }
        Ok(self.objects.get_mut(&space).expect("opened"))
    }
}

fn storage(error: impl Into<Fault>) -> StorageError {
    error.into().0
}

impl GcRead for TxnGc<'_> {
    fn object_matches(
        &mut self,
        space: SpaceId,
        id: &ObjectId,
        bytes: &[u8],
    ) -> Result<Option<bool>, StorageError> {
        let table = self.objects(space)?;
        let found = table.get(id).map_err(storage)?;
        Ok(found.map(|stored| stored.value() == bytes))
    }

    fn object(&mut self, space: SpaceId, id: &ObjectId) -> Result<Option<Vec<u8>>, StorageError> {
        let table = self.objects(space)?;
        let found = table.get(id).map_err(storage)?;
        Ok(found.map(|stored| stored.value().to_vec()))
    }

    fn shared(&mut self, key: &GcKey) -> Result<Option<u64>, StorageError> {
        Ok(self
            .shared
            .get(key41(key))
            .map_err(storage)?
            .map(|v| v.value()))
    }

    fn dead(&mut self, key: &GcKey) -> Result<Option<DeadEntry>, StorageError> {
        match self.dead.get(key41(key)).map_err(storage)? {
            Some(v) => Ok(Some(DeadEntry::decode(v.value())?)),
            None => Ok(None),
        }
    }

    fn phantom(&mut self, key: &GcKey) -> Result<Option<u64>, StorageError> {
        Ok(self
            .phantom
            .get(key41(key))
            .map_err(storage)?
            .map(|v| v.value()))
    }

    fn epoch_at(&mut self, height: u64) -> Result<Option<(u64, u64)>, StorageError> {
        let mut range = self.epochs.range(..=height).map_err(storage)?;
        match range.next_back() {
            Some(entry) => {
                let (h, e) = entry.map_err(storage)?;
                Ok(Some((h.value(), e.value())))
            }
            None => Ok(None),
        }
    }

    fn epochs_below(&mut self, height: u64) -> Result<Vec<u64>, StorageError> {
        let mut out = Vec::new();
        for entry in self.epochs.range(..height).map_err(storage)? {
            out.push(entry.map_err(storage)?.0.value());
        }
        Ok(out)
    }

    fn queue_next(&mut self, after: Option<&QueueKey>) -> Result<Option<QueueKey>, StorageError> {
        let mut range = match after {
            None => self.queue.range::<&[u8; 8 + GC_KEY_LEN]>(..),
            Some(after) => self
                .queue
                .range::<&[u8; 8 + GC_KEY_LEN]>((Bound::Excluded(after), Bound::Unbounded)),
        }
        .map_err(storage)?;
        match range.next() {
            Some(entry) => Ok(Some(*entry.map_err(storage)?.0.value())),
            None => Ok(None),
        }
    }

    fn release_prev(&mut self, below: u64) -> Result<Option<(u64, GcKey)>, StorageError> {
        let mut range = self.release.range(..below).map_err(storage)?;
        match range.next_back() {
            Some(entry) => {
                let (seq, key) = entry.map_err(storage)?;
                Ok(Some((seq.value(), *key.value())))
            }
            None => Ok(None),
        }
    }

    fn epoch_counts_through(&mut self, epoch: u64) -> Result<Vec<(u64, u64)>, StorageError> {
        let mut out = Vec::new();
        for entry in self.epoch_counts.range(..=epoch).map_err(storage)? {
            let (e, n) = entry.map_err(storage)?;
            out.push((e.value(), n.value()));
        }
        Ok(out)
    }

    fn epoch_count(&mut self, epoch: u64) -> Result<Option<u64>, StorageError> {
        Ok(self
            .epoch_counts
            .get(epoch)
            .map_err(storage)?
            .map(|v| v.value()))
    }
}

impl GcWrite for TxnGc<'_> {
    fn insert_object(
        &mut self,
        space: SpaceId,
        id: &ObjectId,
        bytes: &[u8],
    ) -> Result<(), StorageError> {
        self.objects(space)?.insert(id, bytes).map_err(storage)?;
        Ok(())
    }

    fn remove_object(&mut self, space: SpaceId, id: &ObjectId) -> Result<(), StorageError> {
        self.objects(space)?.remove(id).map_err(storage)?;
        Ok(())
    }

    fn set_shared(&mut self, key: &GcKey, count: Option<u64>) -> Result<(), StorageError> {
        match count {
            Some(count) => self.shared.insert(key41(key), count).map(drop),
            None => self.shared.remove(key41(key)).map(drop),
        }
        .map_err(storage)
    }

    fn set_dead(&mut self, key: &GcKey, entry: Option<DeadEntry>) -> Result<(), StorageError> {
        match entry {
            Some(entry) => self.dead.insert(key41(key), &entry.encode()).map(drop),
            None => self.dead.remove(key41(key)).map(drop),
        }
        .map_err(storage)
    }

    fn set_phantom(&mut self, key: &GcKey, count: Option<u64>) -> Result<(), StorageError> {
        match count {
            Some(count) => self.phantom.insert(key41(key), count).map(drop),
            None => self.phantom.remove(key41(key)).map(drop),
        }
        .map_err(storage)
    }

    fn set_queue(&mut self, key: &QueueKey, present: bool) -> Result<(), StorageError> {
        if present {
            self.queue.insert(key, ()).map(drop)
        } else {
            self.queue.remove(key).map(drop)
        }
        .map_err(storage)
    }

    fn set_release(&mut self, seq: u64, key: Option<&GcKey>) -> Result<(), StorageError> {
        match key {
            Some(key) => self.release.insert(seq, key).map(drop),
            None => self.release.remove(seq).map(drop),
        }
        .map_err(storage)
    }

    fn set_epoch(&mut self, height: u64, epoch: Option<u64>) -> Result<(), StorageError> {
        match epoch {
            Some(epoch) => self.epochs.insert(height, epoch).map(drop),
            None => self.epochs.remove(height).map(drop),
        }
        .map_err(storage)
    }

    fn set_epoch_count(&mut self, epoch: u64, count: Option<u64>) -> Result<(), StorageError> {
        match count {
            Some(count) => self.epoch_counts.insert(epoch, count).map(drop),
            None => self.epoch_counts.remove(epoch).map(drop),
        }
        .map_err(storage)
    }
}

/// Seeding output written in key order, in write transactions of bounded
/// size committed without durability; the final GC state commit makes
/// them durable together.
pub(crate) struct SeedWriter<'d> {
    db: &'d redb::Database,
    shared: Vec<(GcKey, u64)>,
    dead: Vec<(GcKey, DeadEntry)>,
    phantom: Vec<(GcKey, u64)>,
    quick_repair: bool,
}

const SEED_FLUSH: usize = 1 << 20;

impl<'d> SeedWriter<'d> {
    pub(crate) fn new(db: &'d redb::Database, quick_repair: bool) -> Self {
        Self {
            db,
            shared: Vec::new(),
            dead: Vec::new(),
            phantom: Vec::new(),
            quick_repair,
        }
    }

    fn pending(&self) -> usize {
        self.shared.len() + self.dead.len() + self.phantom.len()
    }

    pub(crate) fn flush(&mut self) -> Result<(), Fault> {
        if self.pending() == 0 {
            return Ok(());
        }
        let mut txn = self.db.begin_write()?;
        txn.set_durability(redb::Durability::None)?;
        txn.set_quick_repair(self.quick_repair);
        {
            let mut shared = txn.open_table(GC_SHARED)?;
            for (key, count) in self.shared.drain(..) {
                shared.insert(&key, count)?;
            }
            let mut dead = txn.open_table(GC_DEAD)?;
            let mut release = txn.open_table(GC_RELEASE)?;
            for (key, entry) in self.dead.drain(..) {
                dead.insert(&key, &entry.encode())?;
                if let Some(seq) = entry.pending {
                    release.insert(seq, &key)?;
                }
            }
            let mut phantom = txn.open_table(GC_PHANTOM)?;
            for (key, count) in self.phantom.drain(..) {
                phantom.insert(&key, count)?;
            }
        }
        txn.commit().map_err(Fault::from)?;
        Ok(())
    }

    fn maybe_flush(&mut self) -> Result<(), StorageError> {
        if self.pending() >= SEED_FLUSH {
            self.flush().map_err(|f| f.0)?;
        }
        Ok(())
    }
}

impl SeedSink for SeedWriter<'_> {
    fn shared(&mut self, key: &GcKey, count: u64) -> Result<(), StorageError> {
        self.shared.push((*key, count));
        self.maybe_flush()
    }
    fn dead(&mut self, key: &GcKey, entry: DeadEntry) -> Result<(), StorageError> {
        self.dead.push((*key, entry));
        self.maybe_flush()
    }
    fn phantom(&mut self, key: &GcKey, count: u64) -> Result<(), StorageError> {
        self.phantom.push((*key, count));
        self.maybe_flush()
    }
}

/// The object tables of a read transaction, by space.
pub(crate) fn object_spaces(read: &redb::ReadTransaction) -> Result<Vec<SpaceId>, Fault> {
    let mut spaces = Vec::new();
    for handle in read.list_tables()? {
        let name = handle.name();
        let space = match name {
            "objects_catalog" => SpaceId::Catalog,
            "objects_host" => SpaceId::Host,
            _ => match name.strip_prefix("objects_ns_") {
                Some(hex) => match u64::from_str_radix(hex, 16) {
                    Ok(n) if hex.len() == 16 => SpaceId::Namespace(n),
                    _ => continue,
                },
                None => continue,
            },
        };
        spaces.push(space);
    }
    Ok(spaces)
}

/// Every GC key with a value, for a dump.
pub(crate) fn dump_tables(
    read: &redb::ReadTransaction,
) -> Result<arkiv_db_storage::gc::GcTables, Fault> {
    fn open<T>(result: Result<T, redb::TableError>) -> Result<Option<T>, Fault> {
        match result {
            Ok(table) => Ok(Some(table)),
            Err(redb::TableError::TableDoesNotExist(_)) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }
    let mut tables = arkiv_db_storage::gc::GcTables::default();
    if let Some(table) = open(read.open_table(GC_SHARED))? {
        for entry in table.iter()? {
            let (k, v) = entry?;
            tables.shared.insert(*k.value(), v.value());
        }
    }
    if let Some(table) = open(read.open_table(GC_DEAD))? {
        for entry in table.iter()? {
            let (k, v) = entry?;
            tables
                .dead
                .insert(*k.value(), DeadEntry::decode(v.value())?);
        }
    }
    if let Some(table) = open(read.open_table(GC_PHANTOM))? {
        for entry in table.iter()? {
            let (k, v) = entry?;
            tables.phantom.insert(*k.value(), v.value());
        }
    }
    if let Some(table) = open(read.open_table(GC_QUEUE))? {
        for entry in table.iter()? {
            tables.queue.insert(*entry?.0.value());
        }
    }
    if let Some(table) = open(read.open_table(GC_RELEASE))? {
        for entry in table.iter()? {
            let (k, v) = entry?;
            tables.release.insert(k.value(), *v.value());
        }
    }
    if let Some(table) = open(read.open_table(GC_EPOCHS))? {
        for entry in table.iter()? {
            let (k, v) = entry?;
            tables.epochs.insert(k.value(), v.value());
        }
    }
    if let Some(table) = open(read.open_table(GC_EPOCH_COUNTS))? {
        for entry in table.iter()? {
            let (k, v) = entry?;
            tables.epoch_counts.insert(k.value(), v.value());
        }
    }
    Ok(tables)
}
