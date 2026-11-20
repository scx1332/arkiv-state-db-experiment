//! Copy-migration of a store file: a new file holding every record and only
//! the objects some given snapshots reach.
//!
//! An archive keeps every object any snapshot ever reached. Deleting them in
//! place lets redb reuse the pages but never shrinks the file; copying what
//! the retained snapshots reach into a fresh file does, at the cost of one
//! walk of the live state and one sequential write of about its size. The
//! caller then turns garbage collection on in the copy (one scan), so the
//! objects reached only by snapshots below the head die at once and are
//! deleted when the retention floor passes the head.

use std::{
    collections::HashMap,
    path::Path,
    time::{Duration, Instant},
};

use arkiv_db_storage::{
    gc::{Edge, ObjectGraph},
    versioned::{ObjectId, SpaceId, StorageError, StorageErrorKind},
};
use redb::{
    Database, Durability, ReadableDatabase, ReadableTable, TableDefinition, WriteTransaction,
};

use crate::{
    error::Fault,
    gc as tables,
    store::{META, object_table, object_table_name},
};

const BLOCKS: TableDefinition<u64, &[u8]> = TableDefinition::new("blocks");
const MANIFESTS: TableDefinition<u64, &[u8]> = TableDefinition::new("manifests");
const INCLUSIONS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("inclusions");
const BLOCK_INDEX: TableDefinition<&[u8; 32], u64> = TableDefinition::new("block_index");

/// Bytes written per non-durable copy transaction.
const CHUNK_BYTES: usize = 64 << 20;

/// What a copy did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CopyReport {
    /// Records copied (every table but the object tables).
    pub records: u64,
    /// Objects the snapshots reach, copied once each.
    pub objects: u64,
    pub object_bytes: u64,
    /// Speculative references that named nothing in the source.
    pub absent_speculative: u64,
    pub duration: Duration,
}

fn storage(error: impl Into<Fault>) -> StorageError {
    error.into().0
}

/// One write transaction of the copy, committed without durability when it
/// has written `CHUNK_BYTES`; the last one is durable.
struct Writer<'d> {
    db: &'d Database,
    txn: Option<WriteTransaction>,
    bytes: usize,
}

impl<'d> Writer<'d> {
    fn new(db: &'d Database) -> Self {
        Self {
            db,
            txn: None,
            bytes: 0,
        }
    }

    fn txn(&mut self) -> Result<&WriteTransaction, StorageError> {
        if self.txn.is_none() {
            let mut txn = self.db.begin_write().map_err(storage)?;
            txn.set_durability(Durability::None).map_err(storage)?;
            self.txn = Some(txn);
        }
        Ok(self.txn.as_ref().expect("begun"))
    }

    fn wrote(&mut self, bytes: usize) -> Result<(), StorageError> {
        self.bytes += bytes;
        if self.bytes >= CHUNK_BYTES {
            self.bytes = 0;
            if let Some(txn) = self.txn.take() {
                txn.commit().map_err(storage)?;
            }
        }
        Ok(())
    }

    fn finish(mut self) -> Result<(), StorageError> {
        let mut txn = match self.txn.take() {
            Some(txn) => txn,
            None => self.db.begin_write().map_err(storage)?,
        };
        txn.set_durability(Durability::Immediate).map_err(storage)?;
        txn.set_quick_repair(true);
        txn.commit().map_err(storage)
    }
}

/// Copies the store file at `source` into a new file at `target` (which
/// must not exist): every record of every table, `meta` included (the
/// database id, format and head stay the same), and each object that one
/// of `snapshots` reaches through `graph`, once. GC tables of the source
/// are not copied: the caller seeds the copy. `meta_overrides` replace or
/// add metadata records in the copy (a raised retention floor, say).
///
/// Offline: `source` must not be open anywhere else. A crash leaves a
/// partial `target` and an untouched `source`.
pub fn copy_reachable(
    source: &Path,
    target: &Path,
    snapshots: &[Vec<(SpaceId, ObjectId)>],
    graph: &dyn ObjectGraph,
    meta_overrides: &[(String, Vec<u8>)],
) -> Result<CopyReport, StorageError> {
    let started = Instant::now();
    if target.exists() {
        return Err(StorageError::new(
            StorageErrorKind::Incompatible,
            format!("{} exists", target.display()),
        ));
    }
    let src = Database::open(source).map_err(storage)?;
    let dst = Database::create(target).map_err(storage)?;
    let read = src.begin_read().map_err(storage)?;
    let mut report = CopyReport::default();
    let mut writer = Writer::new(&dst);

    // Records, table by table in key order.
    {
        let meta = read.open_table(META).map_err(storage)?;
        for entry in meta.iter().map_err(storage)? {
            let (key, value) = entry.map_err(storage)?;
            let (key, value) = (key.value(), value.value());
            if key == arkiv_db_storage::gc::GC_STATE_KEY {
                continue;
            }
            writer
                .txn()?
                .open_table(META)
                .map_err(storage)?
                .insert(key, value)
                .map_err(storage)?;
            report.records += 1;
            writer.wrote(key.len() + value.len())?;
        }
        for (key, value) in meta_overrides {
            writer
                .txn()?
                .open_table(META)
                .map_err(storage)?
                .insert(key.as_str(), value.as_slice())
                .map_err(storage)?;
        }
    }
    for definition in [BLOCKS, MANIFESTS] {
        let table = read.open_table(definition).map_err(storage)?;
        for entry in table.iter().map_err(storage)? {
            let (key, value) = entry.map_err(storage)?;
            writer
                .txn()?
                .open_table(definition)
                .map_err(storage)?
                .insert(key.value(), value.value())
                .map_err(storage)?;
            report.records += 1;
            writer.wrote(8 + value.value().len())?;
        }
    }
    {
        let table = read.open_table(INCLUSIONS).map_err(storage)?;
        for entry in table.iter().map_err(storage)? {
            let (key, value) = entry.map_err(storage)?;
            writer
                .txn()?
                .open_table(INCLUSIONS)
                .map_err(storage)?
                .insert(key.value(), value.value())
                .map_err(storage)?;
            report.records += 1;
            writer.wrote(key.value().len() + value.value().len())?;
        }
        let table = read.open_table(BLOCK_INDEX).map_err(storage)?;
        for entry in table.iter().map_err(storage)? {
            let (key, value) = entry.map_err(storage)?;
            writer
                .txn()?
                .open_table(BLOCK_INDEX)
                .map_err(storage)?
                .insert(key.value(), value.value())
                .map_err(storage)?;
            report.records += 1;
            writer.wrote(40)?;
        }
    }
    // Every store table exists, even when empty.
    {
        let txn = writer.txn()?;
        txn.open_table(META).map_err(storage)?;
        txn.open_table(BLOCKS).map_err(storage)?;
        txn.open_table(MANIFESTS).map_err(storage)?;
        txn.open_table(INCLUSIONS).map_err(storage)?;
        txn.open_table(BLOCK_INDEX).map_err(storage)?;
        for space in [SpaceId::Catalog, SpaceId::Host] {
            let name = object_table_name(space);
            txn.open_table(object_table(&name)).map_err(storage)?;
        }
    }

    // Objects: a depth-first walk from every snapshot's roots. The copy is
    // the visited set: an object already there was walked.
    let mut sources = HashMap::new();
    let mut edges = Vec::new();
    let mut stack: Vec<Edge> = snapshots
        .iter()
        .flatten()
        .map(|(space, id)| Edge {
            space: *space,
            id: *id,
            strict: true,
        })
        .collect();
    while let Some(edge) = stack.pop() {
        let name = object_table_name(edge.space);
        {
            let copied = writer
                .txn()?
                .open_table(object_table(&name))
                .map_err(storage)?;
            if copied.get(&edge.id).map_err(storage)?.is_some() {
                continue;
            }
        }
        if let std::collections::hash_map::Entry::Vacant(slot) = sources.entry(edge.space) {
            match read.open_table(object_table(&name)) {
                Ok(table) => {
                    slot.insert(table);
                }
                Err(redb::TableError::TableDoesNotExist(_)) if !edge.strict => {
                    report.absent_speculative += 1;
                    continue;
                }
                Err(error) => return Err(storage(error)),
            }
        }
        let source_table = &sources[&edge.space];
        let Some(bytes) = source_table.get(&edge.id).map_err(storage)? else {
            if edge.strict {
                return Err(StorageError::new(
                    StorageErrorKind::Missing,
                    format!(
                        "a retained snapshot references a missing object in {:?}",
                        edge.space
                    ),
                ));
            }
            report.absent_speculative += 1;
            continue;
        };
        let bytes = bytes.value();
        writer
            .txn()?
            .open_table(object_table(&name))
            .map_err(storage)?
            .insert(&edge.id, bytes)
            .map_err(storage)?;
        report.objects += 1;
        report.object_bytes += bytes.len() as u64;
        edges.clear();
        graph
            .edges(edge.space, bytes, &mut edges)
            .map_err(|e| StorageError::new(StorageErrorKind::Corrupt, e))?;
        stack.extend(edges.iter().copied());
        writer.wrote(32 + bytes.len())?;
    }
    drop(sources);
    drop(read);
    writer.finish()?;
    // The copy holds no GC table; the format it inherited from an
    // archive is 1, and a collecting source's format is reset with them.
    {
        let mut txn = dst.begin_write().map_err(storage)?;
        txn.set_durability(Durability::Immediate).map_err(storage)?;
        txn.set_quick_repair(true);
        tables::clear(&txn)?;
        txn.commit().map_err(storage)?;
    }
    report.duration = started.elapsed();
    Ok(report)
}
