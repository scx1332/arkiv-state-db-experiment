//! Offline migration of a stopped node's files to expiring records.
//!
//! Files written before per-height records could expire lack two indexes
//! the expiry needs:
//!
//! - `node.redb` has no inclusion list (`engine/inclusions-at/<h>`) for its
//!   heights, so an expired height could not find its inclusion rows;
//! - `history.redb` has no height index row (`Blocks[h]`) for its bodies,
//!   so a body could neither be served by height once the node's record of
//!   that height expired, nor be deleted by height.
//!
//! [`migrate_records`] writes both from what the files already hold: one
//! scan of the inclusion rows, one of the node records (certificate and
//! body reference of every height) and one of the history entries (which
//! entry persisted each body). It moves no head and changes no existing
//! record, so it is idempotent and safe to interrupt; it ends by writing
//! [`INDEXED_KEY`] into `history.redb`. A node starts it by itself when that
//! marker is missing.

use super::{
    ErrorCode, Result,
    history::{INDEXED_KEY, entry_body, index_row},
    host::HeaderRecord,
};
use arkiv_db::durable::{records, simulator::decode_block_record};
use arkiv_db_redb::RedbStore;
use arkiv_db_storage::versioned::{Table, VersionedStore};
use std::collections::BTreeMap;

/// What [`migrate_records`] wrote.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrationReport {
    /// Heights given an inclusion list in `node.redb`.
    pub inclusion_lists: u64,
    /// Inclusion rows those lists name.
    pub inclusions: u64,
    /// Heights given a height index row in `history.redb`.
    pub index_rows: u64,
    /// Node records whose body has no history entry (not indexed).
    pub unindexed: u64,
}

/// Whether `history` still needs [`migrate_records`].
pub fn needs_migration(history: &dyn VersionedStore) -> Result<bool> {
    Ok(history.head()?.is_some()
        && history
            .read_record_bounded(Table::Meta, INDEXED_KEY, 8)?
            .is_none())
}

/// Batches of offline record insertions.
const CHUNK: usize = 4096;

fn insert_chunked(store: &RedbStore, records: Vec<(Table, Vec<u8>, Vec<u8>)>) -> Result<()> {
    for chunk in records.chunks(CHUNK) {
        store.insert_records_offline(chunk)?;
    }
    Ok(())
}

/// Writes the inclusion lists and history height index a file written
/// before record expiry lacks (see the module docs). Run with the node
/// stopped.
pub fn migrate_records(node: &RedbStore, history: &RedbStore) -> Result<MigrationReport> {
    let mut report = MigrationReport::default();
    // Inclusion lists, from the inclusion rows.
    let mut by_height: BTreeMap<u64, Vec<Vec<u8>>> = BTreeMap::new();
    node.for_each_record(Table::Inclusions, |key, value| {
        let height = records::inclusion_height(value).map_err(|e| {
            arkiv_db_storage::versioned::StorageError::new(
                arkiv_db_storage::versioned::StorageErrorKind::Corrupt,
                e.to_string(),
            )
        })?;
        by_height.entry(height).or_default().push(key.to_vec());
        Ok(true)
    })?;
    let mut lists = Vec::new();
    for (height, keys) in by_height {
        let name = records::inclusion_list_key(height);
        if height == 0
            || node
                .read_record_bounded(Table::Meta, name.as_bytes(), records::INCLUSION_LIST_MAX)?
                .is_some()
        {
            continue;
        }
        report.inclusion_lists += 1;
        report.inclusions += keys.len() as u64;
        lists.push((
            Table::Meta,
            name.into_bytes(),
            records::encode_inclusion_list(keys.iter().map(Vec::as_slice)),
        ));
    }
    insert_chunked(node, lists)?;
    // History height index: which entry persisted each body ...
    let mut entries: BTreeMap<[u8; 32], u64> = BTreeMap::new();
    history.for_each_record(Table::Manifests, |key, value| {
        if let (Some(body), Ok(sequence)) = (entry_body(value), <[u8; 8]>::try_from(key)) {
            entries.insert(body.id.0, u64::from_be_bytes(sequence));
        }
        Ok(true)
    })?;
    // ... and which height each body belongs to.
    let mut rows = Vec::new();
    let mut failure = None;
    node.for_each_record(Table::Blocks, |key, value| {
        let parsed = decode_block_record(value)
            .map_err(|e| ErrorCode::StorageCorrupt.caused_by(e))
            .and_then(|(height, payload)| {
                let (certificate, body) = HeaderRecord::parts(&payload)?;
                if key != height.to_be_bytes() || certificate.header.height != height {
                    return Err(ErrorCode::StorageCorrupt.into());
                }
                Ok((height, certificate, body))
            });
        match parsed {
            Ok((height, certificate, body)) => {
                match entries.get(&body.id.0) {
                    Some(entry) => rows.push((
                        Table::Blocks,
                        height.to_be_bytes().to_vec(),
                        index_row(&body, *entry, &certificate),
                    )),
                    None => report.unindexed += 1,
                }
                Ok(true)
            }
            Err(error) => {
                failure = Some(error);
                Ok(false)
            }
        }
    })?;
    if let Some(error) = failure {
        return Err(error);
    }
    let mut missing = Vec::new();
    for row in rows {
        if history
            .read_record_bounded(Table::Blocks, &row.1, 1024)?
            .is_none()
        {
            missing.push(row);
        }
    }
    report.index_rows = missing.len() as u64;
    insert_chunked(history, missing)?;
    history.insert_records_offline(&[(Table::Meta, INDEXED_KEY.to_vec(), vec![1])])?;
    Ok(report)
}
