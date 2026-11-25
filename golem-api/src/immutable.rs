//! Immutable data: append-only typed rows in named segments.
//!
//! golem-db keeps these rows in files beside its key/value store and their
//! keys and per-commit ranges inside it. This crate keeps all of it in a
//! store of its own, one redb file (`immutable.redb`) in the immutable-data
//! directory, or the same tables in memory. The engine never sees a row:
//! nothing here is under the state root, and the node's stores are not
//! touched. The types, the row and projection checks and the errors are
//! golem-db's (`golemdb-immutable-data`); the storage and the publication
//! protocol are this module's.
//!
//! # Semantics (golem-db `main`)
//!
//! - A segment's rows have dense ordinals from 0 that continue across
//!   commits. A row may carry a 32-byte key, unique within its segment.
//! - `append` and `truncate` only change the branch's staged batch, before
//!   and after `seal`. The first staged ordinal is the segment's published
//!   end at the branch's origin, so competing branches stage the same
//!   ordinals; only the one that commits keeps them. Checkpoints and
//!   `rollback` do not touch staged rows, and the roots never depend on them.
//! - Staged rows become readable, with their keys, when their branch
//!   commits, and only then. `get` and `rows_of` read what the head they
//!   observed has published.
//!
//! # Tables
//!
//! | Table | Key | Value |
//! | --- | --- | --- |
//! | `meta` | `"genesis"` | the deployment's genesis id |
//! | `rows` | segment id, ordinal | the row's optional key and its column payloads |
//! | `keys` | segment id, key | ordinal |
//! | `ranges` | segment id, commit | first ordinal, end ordinal of the rows that commit appended |
//!
//! Segment ids are positions in segment-name order, as in golem-db.
//!
//! # Publication with a commit, and recovery
//!
//! A commit writes two stores, this one and the engine's. The order is fixed:
//!
//! 1. This store commits the batch as the rows of commit `N` (durable).
//! 2. The engine publishes commit `N`.
//!
//! The engine's head is the only authority on what is published. Every read
//! is bounded by the head the reader observed, so rows of a commit the
//! engine has not published are invisible wherever they came from: a crash
//! between the two steps, or a failed step 2. Such a tail is deleted before
//! anything else is written: on open, against the engine's head, and at the
//! start of the next publication in this process. A crash before step 1
//! loses nothing that was promised, and a crash after step 2 loses nothing.
//! The reverse order would not be safe: the engine could hold a commit whose
//! rows were lost.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use golemdb_immutable_data::ImmutableDataStorageError as Error;
use real::{
    ApiError, ImmutableDataAddress, ImmutableDataConfig, ImmutableDataKey, ImmutableDataOrdinal,
    ImmutableDataReadOptions, ImmutableDataRow, ImmutableDataSchema, OpenError, OpenResult,
};
use redb::{ReadableDatabase, ReadableTable, TableDefinition};

/// The store's file inside the immutable-data directory.
pub const IMMUTABLE_FILE: &str = "immutable.redb";

const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
const ROWS: TableDefinition<(u32, u64), &[u8]> = TableDefinition::new("rows");
const KEYS: TableDefinition<(u32, &[u8; 32]), u64> = TableDefinition::new("keys");
const RANGES: TableDefinition<(u32, u64), (u64, u64)> = TableDefinition::new("ranges");
const GENESIS: &str = "genesis";

type StoreResult<T> = std::result::Result<T, Error>;

/// A failure of this store's own file is golem-db's internal storage error.
fn io(error: impl Into<redb::Error>) -> Error {
    Error::Io(std::io::Error::other(error.into()))
}

struct Segment {
    id: u32,
    schema: ImmutableDataSchema,
}

/// One branch's staged rows (golem-db's `ImmutableStaging`). Competing
/// branches may stage the same key and ordinal; the head check at commit
/// invalidates the losers.
#[derive(Clone, Debug, Default)]
pub(crate) struct Staging {
    batches: BTreeMap<u32, Batch>,
}

#[derive(Clone, Debug)]
struct Batch {
    /// The segment's published end at the branch's origin.
    first: u64,
    rows: Vec<(Option<ImmutableDataKey>, ImmutableDataRow)>,
    keys: BTreeSet<ImmutableDataKey>,
}

impl Batch {
    fn end(&self) -> StoreResult<u64> {
        self.first
            .checked_add(self.rows.len() as u64)
            .ok_or(Error::Overflow)
    }
}

impl Staging {
    pub(crate) fn is_empty(&self) -> bool {
        self.batches.values().all(|batch| batch.rows.is_empty())
    }
}

/// The immutable-data store of one database.
pub(crate) struct ImmutableStore {
    db: redb::Database,
    segments: BTreeMap<String, Segment>,
    /// Whether the store may hold rows of a commit the engine did not
    /// publish: set before a batch is written, cleared once the engine has
    /// published it. A set flag makes the next publication delete the tail
    /// first.
    tail: AtomicBool,
}

impl ImmutableStore {
    /// The store in `directory`, created if missing, for the deployment
    /// `genesis_id`. One process holds a directory at a time.
    pub(crate) fn open(
        config: &ImmutableDataConfig,
        genesis_id: [u8; 32],
        directory: &Path,
    ) -> OpenResult<Self> {
        std::fs::create_dir_all(directory).map_err(OpenError::Io)?;
        let db = redb::Database::create(directory.join(IMMUTABLE_FILE)).map_err(|error| {
            OpenError::ImmutableData(match error {
                redb::DatabaseError::DatabaseAlreadyOpen => {
                    Error::Invalid("the immutable-data directory is already open")
                }
                other => io(other),
            })
        })?;
        Self::new(db, config, genesis_id)
    }

    /// The same tables in memory, for a database that is not persisted.
    pub(crate) fn in_memory(
        config: &ImmutableDataConfig,
        genesis_id: [u8; 32],
    ) -> OpenResult<Self> {
        let db = redb::Database::builder()
            .create_with_backend(redb::backends::InMemoryBackend::new())
            .map_err(|error| OpenError::ImmutableData(io(error)))?;
        Self::new(db, config, genesis_id)
    }

    fn new(
        db: redb::Database,
        config: &ImmutableDataConfig,
        genesis_id: [u8; 32],
    ) -> OpenResult<Self> {
        let mut segments = BTreeMap::new();
        let declared = config
            .segments_by_id()
            .map_err(|error| OpenError::InvalidConfig(error.to_string()))?;
        for (id, declaration) in declared {
            let schema = declaration
                .schema()
                .map_err(|error| OpenError::InvalidConfig(error.to_string()))?;
            segments.insert(declaration.name.clone(), Segment { id, schema });
        }
        let store = Self {
            db,
            segments,
            tail: AtomicBool::new(false),
        };
        store.bind(genesis_id).map_err(OpenError::ImmutableData)?;
        Ok(store)
    }

    /// Creates the tables and records the deployment, or checks that the
    /// store belongs to it.
    fn bind(&self, genesis_id: [u8; 32]) -> StoreResult<()> {
        let recorded = {
            let read = self.db.begin_read().map_err(io)?;
            match read.open_table(META) {
                Ok(meta) => meta
                    .get(GENESIS)
                    .map_err(io)?
                    .map(|value| value.value().to_vec()),
                Err(redb::TableError::TableDoesNotExist(_)) => None,
                Err(error) => return Err(io(error)),
            }
        };
        match recorded {
            Some(recorded) if recorded == genesis_id => Ok(()),
            Some(_) => Err(Error::Invalid(
                "the immutable-data directory belongs to another deployment",
            )),
            None => {
                let write = self.db.begin_write().map_err(io)?;
                write
                    .open_table(META)
                    .map_err(io)?
                    .insert(GENESIS, genesis_id.as_slice())
                    .map_err(io)?;
                write.open_table(ROWS).map_err(io)?;
                write.open_table(KEYS).map_err(io)?;
                write.open_table(RANGES).map_err(io)?;
                write.commit().map_err(io)
            }
        }
    }

    fn segment(&self, name: &str) -> StoreResult<&Segment> {
        self.segments.get(name).ok_or(Error::NotFound)
    }

    pub(crate) fn schema(&self, segment: &str) -> StoreResult<ImmutableDataSchema> {
        Ok(self.segment(segment)?.schema.clone())
    }

    /// The segment's end ordinal as of `commit`: the end of the last range
    /// at or before it, zero when there is none.
    fn end_at(
        ranges: &impl ReadableTable<(u32, u64), (u64, u64)>,
        segment: u32,
        commit: u64,
    ) -> StoreResult<u64> {
        let last = ranges
            .range((segment, 0)..=(segment, commit))
            .map_err(io)?
            .next_back()
            .transpose()
            .map_err(io)?;
        Ok(last.map_or(0, |(_, range)| range.value().1))
    }

    /// Stages a complete typed row on a branch whose origin is `origin`, and
    /// returns its provisional ordinal. Nothing is written.
    pub(crate) fn append(
        &self,
        staging: &mut Staging,
        origin: u64,
        segment: &str,
        key: Option<ImmutableDataKey>,
        row: ImmutableDataRow,
    ) -> StoreResult<ImmutableDataOrdinal> {
        let segment = self.segment(segment)?;
        segment.schema.validate_row(&row)?;
        let batch = staging.batches.get(&segment.id);
        let read = self.db.begin_read().map_err(io)?;
        let first = match batch {
            Some(batch) => batch.first,
            None => Self::end_at(&read.open_table(RANGES).map_err(io)?, segment.id, origin)?,
        };
        if let Some(key) = key {
            if batch.is_some_and(|batch| batch.keys.contains(&key)) {
                return Err(Error::AlreadyExists);
            }
            let bound = read
                .open_table(KEYS)
                .map_err(io)?
                .get((segment.id, &key.0))
                .map_err(io)?
                .map(|ordinal| ordinal.value());
            // A binding at or past the published end belongs to a tail the
            // engine never published; it is not a row.
            if bound.is_some_and(|ordinal| ordinal < first) {
                return Err(Error::AlreadyExists);
            }
        }
        let ordinal = match batch {
            Some(batch) => batch.end()?,
            None => first,
        };
        ordinal.checked_add(1).ok_or(Error::Overflow)?;
        // Every check precedes the mutation.
        let batch = staging.batches.entry(segment.id).or_insert_with(|| Batch {
            first,
            rows: Vec::new(),
            keys: BTreeSet::new(),
        });
        batch.rows.push((key, row));
        if let Some(key) = key {
            batch.keys.insert(key);
        }
        Ok(ordinal)
    }

    /// Removes the branch's staged suffix from `from`, releasing its keys
    /// and ordinals. `from` must lie within the staged tail.
    pub(crate) fn truncate(
        &self,
        staging: &mut Staging,
        origin: u64,
        segment: &str,
        from: ImmutableDataOrdinal,
    ) -> StoreResult<()> {
        const OUTSIDE: Error = Error::Invalid("truncate must stay within the branch's staged tail");
        let segment = self.segment(segment)?;
        let Some(batch) = staging.batches.get_mut(&segment.id) else {
            let read = self.db.begin_read().map_err(io)?;
            let end = Self::end_at(&read.open_table(RANGES).map_err(io)?, segment.id, origin)?;
            return if from == end { Ok(()) } else { Err(OUTSIDE) };
        };
        if from < batch.first || from > batch.end()? {
            return Err(OUTSIDE);
        }
        for (key, _) in batch.rows.drain((from - batch.first) as usize..) {
            if let Some(key) = key {
                batch.keys.remove(&key);
            }
        }
        Ok(())
    }

    /// Step 1 of a commit: makes `staging` durable as the rows of `commit`,
    /// the commit the engine is about to publish over head `commit - 1`.
    /// Rows of a commit at or after `commit` that the engine never published
    /// are deleted first.
    pub(crate) fn publish(&self, commit: u64, staging: &Staging) -> StoreResult<()> {
        let tail = self.tail.load(Ordering::Acquire);
        if staging.is_empty() && !tail {
            return Ok(());
        }
        let write = self.db.begin_write().map_err(io)?;
        {
            let mut rows = write.open_table(ROWS).map_err(io)?;
            let mut keys = write.open_table(KEYS).map_err(io)?;
            let mut ranges = write.open_table(RANGES).map_err(io)?;
            if tail {
                Self::drop_from(
                    &mut rows,
                    &mut keys,
                    &mut ranges,
                    self.segments.values(),
                    commit,
                )?;
            }
            for (&segment, batch) in &staging.batches {
                if batch.rows.is_empty() {
                    continue;
                }
                if Self::end_at(&ranges, segment, commit)? != batch.first {
                    return Err(Error::Corrupt(
                        "staged ordinals do not follow the published end",
                    ));
                }
                for (ordinal, (key, row)) in (batch.first..).zip(&batch.rows) {
                    if let Some(key) = key
                        && keys
                            .insert((segment, &key.0), ordinal)
                            .map_err(io)?
                            .is_some()
                    {
                        return Err(Error::AlreadyExists);
                    }
                    rows.insert((segment, ordinal), encode(*key, row).as_slice())
                        .map_err(io)?;
                }
                ranges
                    .insert((segment, commit), (batch.first, batch.end()?))
                    .map_err(io)?;
            }
        }
        if !staging.is_empty() {
            self.tail.store(true, Ordering::Release);
        }
        write.commit().map_err(io)?;
        if staging.is_empty() {
            self.tail.store(false, Ordering::Release);
        }
        Ok(())
    }

    /// Step 2 succeeded: the engine published the commit whose rows
    /// [`publish`](Self::publish) wrote.
    pub(crate) fn published(&self) {
        self.tail.store(false, Ordering::Release);
    }

    /// Deletes what the engine never published: every row, key and range of
    /// a commit after `head`. Called on open, before the store is used.
    pub(crate) fn recover(&self, head: u64) -> StoreResult<()> {
        let Some(from) = head.checked_add(1) else {
            return Ok(());
        };
        let dirty = {
            let read = self.db.begin_read().map_err(io)?;
            let ranges = read.open_table(RANGES).map_err(io)?;
            let mut dirty = false;
            for segment in self.segments.values() {
                let mut tail = ranges
                    .range((segment.id, from)..=(segment.id, u64::MAX))
                    .map_err(io)?;
                dirty |= tail.next().transpose().map_err(io)?.is_some();
            }
            dirty
        };
        if dirty {
            let write = self.db.begin_write().map_err(io)?;
            {
                let mut rows = write.open_table(ROWS).map_err(io)?;
                let mut keys = write.open_table(KEYS).map_err(io)?;
                let mut ranges = write.open_table(RANGES).map_err(io)?;
                Self::drop_from(
                    &mut rows,
                    &mut keys,
                    &mut ranges,
                    self.segments.values(),
                    from,
                )?;
            }
            write.commit().map_err(io)?;
        }
        self.tail.store(false, Ordering::Release);
        Ok(())
    }

    /// Deletes the ranges of commits at or after `from`, their rows and
    /// those rows' keys.
    fn drop_from<'a>(
        rows: &mut redb::Table<'_, (u32, u64), &[u8]>,
        keys: &mut redb::Table<'_, (u32, &[u8; 32]), u64>,
        ranges: &mut redb::Table<'_, (u32, u64), (u64, u64)>,
        segments: impl Iterator<Item = &'a Segment>,
        from: u64,
    ) -> StoreResult<()> {
        for segment in segments {
            let mut doomed = Vec::new();
            for entry in ranges
                .range((segment.id, from)..=(segment.id, u64::MAX))
                .map_err(io)?
            {
                let (key, range) = entry.map_err(io)?;
                doomed.push((key.value().1, range.value()));
            }
            for (commit, (first, end)) in doomed {
                for ordinal in first..end {
                    let Some(row) = rows.remove((segment.id, ordinal)).map_err(io)? else {
                        continue;
                    };
                    if let (Some(key), _) = decode(row.value())? {
                        // Only a binding that points at this row is this row's.
                        let bound = keys
                            .get((segment.id, &key.0))
                            .map_err(io)?
                            .map(|o| o.value());
                        if bound == Some(ordinal) {
                            keys.remove((segment.id, &key.0)).map_err(io)?;
                        }
                    }
                }
                ranges.remove((segment.id, commit)).map_err(io)?;
            }
        }
        Ok(())
    }

    /// A row that `head` has published, by ordinal or by key.
    pub(crate) fn get(
        &self,
        head: u64,
        segment: &str,
        address: ImmutableDataAddress,
        options: &ImmutableDataReadOptions,
    ) -> StoreResult<ImmutableDataRow> {
        let segment = self.segment(segment)?;
        let projection = segment.schema.projection(options)?;
        let read = self.db.begin_read().map_err(io)?;
        let end = Self::end_at(&read.open_table(RANGES).map_err(io)?, segment.id, head)?;
        let ordinal = match address {
            ImmutableDataAddress::Ordinal(ordinal) => ordinal,
            ImmutableDataAddress::Key(key) => read
                .open_table(KEYS)
                .map_err(io)?
                .get((segment.id, &key.0))
                .map_err(io)?
                .map(|ordinal| ordinal.value())
                .ok_or(Error::NotFound)?,
        };
        if ordinal >= end {
            return Err(Error::NotFound);
        }
        let rows = read.open_table(ROWS).map_err(io)?;
        let row = rows
            .get((segment.id, ordinal))
            .map_err(io)?
            .ok_or(Error::Corrupt("missing published immutable row"))?;
        let (_, payloads) = decode(row.value())?;
        project(&projection, &payloads)
    }

    /// The rows `commit` appended, in append order; none when it appended
    /// none. `commit` must not be after `head`.
    pub(crate) fn rows_of(
        &self,
        head: u64,
        segment: &str,
        commit: u64,
        options: &ImmutableDataReadOptions,
    ) -> StoreResult<Vec<ImmutableDataRow>> {
        let segment = self.segment(segment)?;
        let projection = segment.schema.projection(options)?;
        if commit > head {
            return Err(Error::CommitUnavailable {
                requested: commit,
                published: head,
            });
        }
        let read = self.db.begin_read().map_err(io)?;
        let range = read
            .open_table(RANGES)
            .map_err(io)?
            .get((segment.id, commit))
            .map_err(io)?
            .map(|range| range.value());
        let Some((first, end)) = range else {
            return Ok(Vec::new());
        };
        let rows = read.open_table(ROWS).map_err(io)?;
        (first..end)
            .map(|ordinal| {
                let row = rows
                    .get((segment.id, ordinal))
                    .map_err(io)?
                    .ok_or(Error::Corrupt("missing published immutable rows"))?;
                let (_, payloads) = decode(row.value())?;
                project(&projection, &payloads)
            })
            .collect()
    }
}

/// The selected columns of a stored row, as golem-db decodes them.
fn project(
    projection: &golemdb_immutable_data::ImmutableDataProjection,
    payloads: &[&[u8]],
) -> StoreResult<ImmutableDataRow> {
    let selected = projection
        .indices()
        .iter()
        .map(|&column| {
            payloads
                .get(column)
                .copied()
                .ok_or(Error::Corrupt("stored row is shorter than its schema"))
        })
        .collect::<StoreResult<Vec<_>>>()?;
    Ok(projection.decode(&selected)?)
}

/// `flag ‖ [key] ‖ count u32 ‖ (len u32 ‖ payload)*`, integers big endian.
/// Payloads are golem-db's: the cell payload, without type tag or kind.
fn encode(key: Option<ImmutableDataKey>, row: &ImmutableDataRow) -> Vec<u8> {
    let mut out = Vec::new();
    match key {
        Some(key) => {
            out.push(1);
            out.extend_from_slice(&key.0);
        }
        None => out.push(0),
    }
    out.extend_from_slice(&(row.values().len() as u32).to_be_bytes());
    for payload in row.payloads() {
        out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        out.extend_from_slice(payload);
    }
    out
}

fn decode(bytes: &[u8]) -> StoreResult<(Option<ImmutableDataKey>, Vec<&[u8]>)> {
    const CORRUPT: Error = Error::Corrupt("malformed stored immutable row");
    fn take<'a>(bytes: &mut &'a [u8], n: usize) -> StoreResult<&'a [u8]> {
        if bytes.len() < n {
            return Err(CORRUPT);
        }
        let (head, rest) = bytes.split_at(n);
        *bytes = rest;
        Ok(head)
    }
    fn word(bytes: &mut &[u8]) -> StoreResult<usize> {
        let word: [u8; 4] = take(bytes, 4)?.try_into().map_err(|_| CORRUPT)?;
        Ok(u32::from_be_bytes(word) as usize)
    }
    let mut rest = bytes;
    let key = match take(&mut rest, 1)? {
        [0] => None,
        [1] => Some(ImmutableDataKey(
            take(&mut rest, 32)?.try_into().map_err(|_| CORRUPT)?,
        )),
        _ => return Err(CORRUPT),
    };
    let count = word(&mut rest)?;
    let mut payloads = Vec::with_capacity(count.min(64));
    for _ in 0..count {
        let len = word(&mut rest)?;
        payloads.push(take(&mut rest, len)?);
    }
    if !rest.is_empty() {
        return Err(CORRUPT);
    }
    Ok((key, payloads))
}

/// golem-db's mapping of an immutable-data failure to its public error,
/// with the one exception its facade makes: a `truncate` outside the staged
/// tail is the caller's error.
pub(crate) fn api_error(error: Error) -> ApiError {
    match error {
        Error::Invalid(message) => ApiError::invalid_argument(message),
        other => other.into(),
    }
}
