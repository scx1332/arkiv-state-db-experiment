//! The versioned storage contract for durable engine state and block history.
//!
//! [`KvStore`](crate::KvStore) describes one mutable map whose failed writes
//! leave it unchanged. Durable block production needs a different contract,
//! which this module defines:
//!
//! - **Immutable objects.** Engine state is stored as content-addressed
//!   objects: an [`ObjectId`] is `keccak256` of the object bytes. Objects are
//!   grouped in [`SpaceId`]s (the namespace catalog, host state, and one space
//!   per namespace); a space is one physical table in a disk store, so the
//!   objects of one namespace never share a B-tree with another namespace's.
//!   A commit puts objects **if absent**: an object never changes once
//!   committed, so every historical version stays readable as long as it is
//!   retained (this contract has no deletion).
//! - **Insert-only records** in five fixed [`Table`]s (metadata, blocks,
//!   manifests, inclusion identities, block index). A record never changes
//!   once committed.
//! - **One durable head** ([`Head`]), changed only by a compare-and-set in
//!   [`VersionedStore::commit`], together with every object and record of the
//!   same [`CommitBatch`], atomically.
//! - **Three commit outcomes** ([`CommitOutcome`]): committed and durable,
//!   definitely not committed, or indeterminate. An indeterminate outcome is
//!   never resolved by the store itself; the caller reconciles it from durable
//!   state after [`VersionedStore::recover`] (see [`reconcile`]).
//! - **Explicit health.** An I/O failure, an indeterminate commit or a failed
//!   recovery can leave a disk handle unusable. Such a store reports
//!   [`Health::Failed`] and answers every call with the failure, never with
//!   "absent", until [`VersionedStore::recover`] succeeds.
//!
//! Storage failures are availability errors. They must never be read as a
//! missing record, an empty result or a deterministic user rejection.
//!
//! Implementations: `MemoryVersionedStore` (crate `arkiv-db-memory`, volatile
//! reference), `RedbStore` (crate `arkiv-db-redb`, durable) and
//! [`FaultyStore`](crate::testing::FaultyStore), a fault-injecting wrapper.
//! The behavioural suite in [`crate::testing::contract`] runs against all of
//! them.
//!
//! The usual node profile identifies heads by block height/hash. Engine
//! storage v1 explicitly identifies an engine-only profile in metadata and
//! uses snapshot sequence/manifest digest instead, with manifests and their
//! digest index in `Manifests`/`BlockIndex`, leaving `Blocks`/`Inclusions`
//! unused. The adapter's byte shapes do not change; profile validation and
//! interpretation belong to the caller. An engine commit is not a block.
//!
//! This module uses only the standard library. It does not hash: callers
//! supply the content address of each object, and a store that reads bytes
//! back from a medium verifies it.

use std::{
    collections::{BTreeMap, btree_map},
    fmt,
    sync::{Arc, Mutex, MutexGuard},
};

/// The stable identity of one durable database.
///
/// Chosen at random when the database is initialized and stored in its
/// metadata, so caches and handles can tell two databases apart even when
/// their chains are identical. It is not part of any logical hash.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DatabaseId(pub [u8; 16]);

impl DatabaseId {
    /// Returns a fresh identity from the operating system's random source.
    ///
    /// Reads `/dev/urandom` where it exists. Elsewhere, or if the read fails,
    /// it falls back to the standard library's randomly keyed hasher mixed
    /// with the time and process id, which is unique in practice but not a
    /// cryptographic random source.
    pub fn random() -> Self {
        let mut bytes = [0u8; 16];
        if read_urandom(&mut bytes).is_err() || bytes == [0u8; 16] {
            bytes = fallback_random();
        }
        Self(bytes)
    }
}

impl fmt::Display for DatabaseId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

fn read_urandom(out: &mut [u8]) -> std::io::Result<()> {
    use std::io::Read;
    std::fs::File::open("/dev/urandom")?.read_exact(out)
}

fn fallback_random() -> [u8; 16] {
    use std::{
        collections::hash_map::RandomState,
        hash::{BuildHasher, Hasher},
        time::{SystemTime, UNIX_EPOCH},
    };
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let mut out = [0u8; 16];
    for (index, chunk) in out.chunks_mut(8).enumerate() {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u128(nanos);
        hasher.write_u32(std::process::id());
        hasher.write_usize(index);
        chunk.copy_from_slice(&hasher.finish().to_be_bytes());
    }
    out
}

/// An object space: one group of content-addressed objects.
///
/// A durable store keeps each space in its own physical table, so reading
/// one namespace's objects never searches a structure that holds another
/// namespace's objects. Spaces order as `Catalog < Host < Namespace(0) <
/// Namespace(1) < …`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SpaceId {
    /// The namespace catalog (directory) shared by every namespace.
    Catalog,
    /// Host-owned state, such as the expiry schedule.
    Host,
    /// The objects of one namespace, by its ordinal id.
    Namespace(u64),
}

/// The content address of an object: `keccak256` of its bytes.
///
/// Callers compute it; stores that read objects back from a medium verify it
/// and report a mismatch as [`StorageErrorKind::Corrupt`]. Committing an
/// object under an id that is not its hash is a caller error the contract
/// does not define.
pub type ObjectId = [u8; 32];

/// The durable head: the sequence and identity of the last committed item.
///
/// The node profile uses a block height and block hash. The engine-only
/// profile uses a snapshot sequence and a domain-separated manifest digest;
/// these are not protocol block identities. A profile is identified by the
/// caller's immutable metadata and must never be silently reinterpreted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Head {
    /// Block height, or snapshot sequence in the engine-only profile.
    pub height: u64,
    /// Block hash, or manifest digest in the engine-only profile.
    pub hash: [u8; 32],
}

impl Head {
    /// Length of [`Head::encode`]: an 8-byte height and a 32-byte hash.
    pub const ENCODED_LEN: usize = 40;

    /// Encodes the head as `height (u64 big-endian) ‖ hash`, the form a disk
    /// store keeps in its metadata.
    pub fn encode(&self) -> [u8; Self::ENCODED_LEN] {
        let mut out = [0u8; Self::ENCODED_LEN];
        out[..8].copy_from_slice(&self.height.to_be_bytes());
        out[8..].copy_from_slice(&self.hash);
        out
    }

    /// Decodes [`Head::encode`]'s form; any other length is `Corrupt`.
    pub fn decode(bytes: &[u8]) -> Result<Self, StorageError> {
        if bytes.len() != Self::ENCODED_LEN {
            return Err(StorageError::new(
                StorageErrorKind::Corrupt,
                format!("stored head has {} bytes, expected 40", bytes.len()),
            ));
        }
        let mut height = [0u8; 8];
        height.copy_from_slice(&bytes[..8]);
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&bytes[8..]);
        Ok(Self {
            height: u64::from_be_bytes(height),
            hash,
        })
    }
}

/// The record tables. Every table is insert-only.
///
/// Keys and values follow a fixed shape per table, checked by
/// [`Table::check_record`] before a commit touches the store:
///
/// | Table | Key | Value |
/// | --- | --- | --- |
/// | `Meta` | UTF-8 name, 1–64 bytes, not starting with `store/` | any bytes |
/// | `Blocks` | height, 8 bytes big-endian ([`height_key`]) | any bytes |
/// | `Manifests` | height, 8 bytes big-endian | any bytes |
/// | `Inclusions` | 1–256 bytes | any bytes |
/// | `BlockIndex` | block hash, 32 bytes | height, 8 bytes big-endian |
///
/// In the engine-only profile, manifest keys are snapshot sequences and
/// `BlockIndex` maps manifest digest to snapshot sequence. These retain
/// the same widths. The profile never uses a state root as a unique commit
/// identity, since distinct publications can have the same state.
///
/// Metadata names starting with [`RESERVED_META_PREFIX`] belong to the store
/// itself (its format version, database id and head) and can be read but not
/// written through a batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Table {
    /// Database metadata: genesis identity, format and protocol versions.
    Meta,
    /// Encoded blocks by height.
    Blocks,
    /// Engine manifests (state header and roots) by height.
    Manifests,
    /// Request inclusion records by request identity.
    Inclusions,
    /// Block height by block hash.
    BlockIndex,
}

/// Metadata names with this prefix are owned by the store implementation.
pub const RESERVED_META_PREFIX: &str = "store/";

/// Returns the record key of a height in [`Table::Blocks`] and
/// [`Table::Manifests`] (and the value shape of [`Table::BlockIndex`]).
pub const fn height_key(height: u64) -> [u8; 8] {
    height.to_be_bytes()
}

impl Table {
    /// Every table, in order.
    pub const ALL: [Table; 5] = [
        Table::Meta,
        Table::Blocks,
        Table::Manifests,
        Table::Inclusions,
        Table::BlockIndex,
    ];

    /// A short lowercase name for diagnostics.
    pub const fn name(self) -> &'static str {
        match self {
            Table::Meta => "meta",
            Table::Blocks => "blocks",
            Table::Manifests => "manifests",
            Table::Inclusions => "inclusions",
            Table::BlockIndex => "block_index",
        }
    }

    /// Checks that `key` and `value` have this table's shape (see the table
    /// above) and that a metadata name is not reserved.
    ///
    /// A violation is [`StorageErrorKind::Incompatible`]: the record cannot
    /// be stored in this table's format.
    pub fn check_record(self, key: &[u8], value: &[u8]) -> Result<(), StorageError> {
        let fault = |what: &str| {
            Err(StorageError::new(
                StorageErrorKind::Incompatible,
                format!("{} record: {what}", self.name()),
            ))
        };
        match self {
            Table::Meta => {
                let Ok(name) = std::str::from_utf8(key) else {
                    return fault("name is not UTF-8");
                };
                if name.is_empty() || name.len() > 64 {
                    return fault("name must have 1 to 64 bytes");
                }
                if name.starts_with(RESERVED_META_PREFIX) {
                    return fault("name uses the store's reserved prefix");
                }
            }
            Table::Blocks | Table::Manifests => {
                if key.len() != 8 {
                    return fault("key must be an 8-byte height");
                }
            }
            Table::Inclusions => {
                if key.is_empty() || key.len() > 256 {
                    return fault("key must have 1 to 256 bytes");
                }
            }
            Table::BlockIndex => {
                if key.len() != 32 {
                    return fault("key must be a 32-byte block hash");
                }
                if value.len() != 8 {
                    return fault("value must be an 8-byte height");
                }
            }
        }
        Ok(())
    }
}

/// Everything one block commits, applied atomically or not at all.
///
/// The batch applies only if the store's current head equals
/// `expected_head` (`None`: the store has no head yet). It then puts every
/// object if absent, inserts every record, and sets the head to `new_head`.
/// A batch must advance the head: `new_head.height` is greater than the
/// expected head's height. This keeps a head from ever returning to an
/// earlier value, which [`reconcile`] relies on.
///
/// Duplicates inside a batch follow the same rules as duplicates against
/// committed state: identical entries are idempotent, different bytes under
/// the same object id or record key are a conflict.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommitBatch {
    /// The head this batch builds on; `None` for the first commit.
    pub expected_head: Option<Head>,
    /// The head after this batch.
    pub new_head: Head,
    /// Objects to put if absent.
    pub objects: Vec<(SpaceId, ObjectId, Vec<u8>)>,
    /// Records to insert.
    pub records: Vec<(Table, Vec<u8>, Vec<u8>)>,
}

impl CommitBatch {
    /// Creates a batch with no objects or records.
    pub const fn new(expected_head: Option<Head>, new_head: Head) -> Self {
        Self {
            expected_head,
            new_head,
            objects: Vec::new(),
            records: Vec::new(),
        }
    }

    /// Adds an object and returns the batch for chaining.
    pub fn put_object(&mut self, space: SpaceId, id: ObjectId, bytes: Vec<u8>) -> &mut Self {
        self.objects.push((space, id, bytes));
        self
    }

    /// Adds a record and returns the batch for chaining.
    pub fn put_record(
        &mut self,
        table: Table,
        key: impl Into<Vec<u8>>,
        value: impl Into<Vec<u8>>,
    ) -> &mut Self {
        self.records.push((table, key.into(), value.into()));
        self
    }

    /// The object and record bytes the batch carries (keys and values),
    /// excluding the head. Used to report write amplification.
    pub fn logical_bytes(&self) -> u64 {
        let objects: usize = self
            .objects
            .iter()
            .map(|(_, id, bytes)| id.len() + bytes.len())
            .sum();
        let records: usize = self
            .records
            .iter()
            .map(|(_, key, value)| key.len() + value.len())
            .sum();
        (objects + records) as u64
    }

    /// Checks everything that can be decided without the store's contents:
    /// the head advances, every record has its table's shape, and no two
    /// entries of the batch conflict.
    ///
    /// Stores call this before touching their state, so a malformed batch is
    /// `NotCommitted` with nothing applied.
    pub fn validate(&self) -> Result<(), StorageError> {
        if let Some(expected) = self.expected_head
            && self.new_head.height <= expected.height
        {
            return Err(StorageError::new(
                StorageErrorKind::Incompatible,
                format!(
                    "new head height {} does not advance expected head height {}",
                    self.new_head.height, expected.height
                ),
            ));
        }
        let mut objects: BTreeMap<(SpaceId, &ObjectId), &[u8]> = BTreeMap::new();
        for (space, id, bytes) in &self.objects {
            match objects.entry((*space, id)) {
                btree_map::Entry::Vacant(slot) => {
                    slot.insert(bytes);
                }
                btree_map::Entry::Occupied(slot) => {
                    if *slot.get() != bytes.as_slice() {
                        return Err(object_conflict(*space, id));
                    }
                }
            }
        }
        let mut records: BTreeMap<(Table, &[u8]), &[u8]> = BTreeMap::new();
        for (table, key, value) in &self.records {
            table.check_record(key, value)?;
            match records.entry((*table, key)) {
                btree_map::Entry::Vacant(slot) => {
                    slot.insert(value);
                }
                btree_map::Entry::Occupied(slot) => {
                    if *slot.get() != value.as_slice() {
                        return Err(record_conflict(*table, key));
                    }
                }
            }
        }
        Ok(())
    }
}

/// The `Conflict` error for different bytes under one object id.
pub fn object_conflict(space: SpaceId, id: &ObjectId) -> StorageError {
    StorageError::new(
        StorageErrorKind::Conflict,
        format!(
            "object {} in {space:?} already holds different bytes",
            hex_prefix(id)
        ),
    )
}

/// The `Conflict` error for a different value under one record key.
pub fn record_conflict(table: Table, key: &[u8]) -> StorageError {
    StorageError::new(
        StorageErrorKind::Conflict,
        format!(
            "{} record {} already holds a different value",
            table.name(),
            hex_prefix(key)
        ),
    )
}

/// The `HeadMismatch` error for a failed compare-and-set.
pub fn head_mismatch(expected: Option<Head>, actual: Option<Head>) -> StorageError {
    StorageError::new(
        StorageErrorKind::HeadMismatch,
        format!(
            "expected head {}, store head is {}",
            describe_head(expected),
            describe_head(actual)
        ),
    )
}

fn describe_head(head: Option<Head>) -> String {
    match head {
        None => "none".to_owned(),
        Some(head) => format!("{}@{}", head.height, hex_prefix(&head.hash)),
    }
}

fn hex_prefix(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(18);
    for byte in bytes.iter().take(8) {
        out.push_str(&format!("{byte:02x}"));
    }
    if bytes.len() > 8 {
        out.push('…');
    }
    out
}

/// The result of [`VersionedStore::commit`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommitOutcome {
    /// The whole batch is applied and durable, and the head is the batch's
    /// new head.
    Committed,
    /// Nothing from the batch was applied: the head check, a conflict, a
    /// malformed batch, a failed store or an error before the storage
    /// transaction started to commit.
    NotCommitted(StorageError),
    /// The storage transaction failed while committing: the batch may or may
    /// not be durable, but it is never partially applied. The caller must
    /// reconcile from durable state (see [`reconcile`]) and must not execute
    /// the block again.
    Indeterminate(StorageError),
}

/// Whether a store can serve calls.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Health {
    /// Calls are served.
    Available,
    /// A failure latched: every call returns this error until
    /// [`VersionedStore::recover`] succeeds.
    Failed(StorageError),
}

/// What went wrong in a storage call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StorageErrorKind {
    /// An I/O error of the medium, or a handle latched by an earlier one.
    Io,
    /// The medium is out of space or quota.
    Full,
    /// Stored bytes are damaged: a checksum or content-hash mismatch, or a
    /// structure that cannot be decoded.
    Corrupt,
    /// A durable item the caller relies on is absent: an object a committed
    /// structure references, or the database itself.
    Missing,
    /// The store's head is not the batch's expected head.
    HeadMismatch,
    /// Different bytes under an existing object id or record key.
    Conflict,
    /// A format, version or shape this store does not accept.
    Incompatible,
    /// Another writer holds the database.
    Locked,
    /// The store is closed.
    Closed,
}

/// A storage failure: its kind and a human-readable detail.
///
/// The detail is diagnostic text only. It never enters a logical hash, a
/// receipt or a block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StorageError {
    /// The failure class callers act on.
    pub kind: StorageErrorKind,
    /// Diagnostic text.
    pub detail: String,
}

impl StorageError {
    /// Creates an error.
    pub fn new(kind: StorageErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
        }
    }
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.detail)
    }
}

impl std::error::Error for StorageError {}

/// Fallible reads of committed immutable objects.
///
/// This is the only access lazy engine structures need: an object id stored
/// in a committed structure is loaded on demand from its space.
pub trait ObjectReader: Send + Sync {
    /// Returns the bytes committed under `id` in `space`, or `None` if no
    /// committed object in that space has this id.
    ///
    /// `Ok(None)` is an authoritative answer about committed state; a store
    /// that cannot answer returns an error instead. Objects in other spaces
    /// are never consulted.
    fn read_object(&self, space: SpaceId, id: &ObjectId) -> Result<Option<Vec<u8>>, StorageError>;

    /// Reads an object only if its stored length is at most `max_bytes`.
    ///
    /// Implementations check the stored value's length **before** copying
    /// it into the returned buffer, hashing it, or decoding it. An oversized
    /// value is [`StorageErrorKind::Corrupt`], not absence or a truncated
    /// answer. The limit is inclusive; zero permits an empty value. This
    /// bounds the value copy, not the backend's page-cache or I/O buffers.
    /// Successful and absent reads have the ordinary read's counter and
    /// failure semantics; an oversized value is a failed read.
    ///
    /// Reading first and checking the allocated `Vec` later does not
    /// satisfy the contract. Wrappers must forward this method.
    fn read_object_bounded(
        &self,
        space: SpaceId,
        id: &ObjectId,
        max_bytes: usize,
    ) -> Result<Option<Vec<u8>>, StorageError>;

    /// Loads an object that a committed structure references: absence is
    /// [`StorageErrorKind::Missing`], never `None`.
    fn load_object(&self, space: SpaceId, id: &ObjectId) -> Result<Vec<u8>, StorageError> {
        self.read_object(space, id)?.ok_or_else(|| {
            StorageError::new(
                StorageErrorKind::Missing,
                format!(
                    "referenced object {} is absent from {space:?}",
                    hex_prefix(id)
                ),
            )
        })
    }

    /// The bounded counterpart of [`load_object`](Self::load_object).
    ///
    /// Preserves size/read errors and reports an absent referenced object
    /// as [`StorageErrorKind::Missing`].
    fn load_object_bounded(
        &self,
        space: SpaceId,
        id: &ObjectId,
        max_bytes: usize,
    ) -> Result<Vec<u8>, StorageError> {
        self.read_object_bounded(space, id, max_bytes)?
            .ok_or_else(|| {
                StorageError::new(
                    StorageErrorKind::Missing,
                    format!(
                        "referenced object {} is absent from {space:?}",
                        hex_prefix(id)
                    ),
                )
            })
    }
}

/// A store of immutable objects, insert-only records and one durable head.
///
/// # Semantics
///
/// - [`commit`](Self::commit) is atomic: after `Committed` the whole batch
///   is durable and readable; after `NotCommitted` nothing of it was applied;
///   after `Indeterminate` either all or nothing of it is durable, and which
///   one is decided only by reading durable state after
///   [`recover`](Self::recover).
/// - Objects are put if absent, keyed by `(SpaceId, ObjectId)`: the same
///   bytes again are idempotent, different bytes are `Conflict` and the whole
///   batch is `NotCommitted`.
/// - Records are insert-only: an identical record is idempotent, a different
///   value is `Conflict`.
/// - The head changes only by compare-and-set; a mismatch is
///   `NotCommitted(HeadMismatch)` with nothing applied.
/// - A store may enter [`Health::Failed`] after an `Io`, `Full` or `Corrupt`
///   failure of any call, after any `Indeterminate` commit whatever its
///   kind, and after a failed `recover`. From then on **every** call, reads
///   included, returns the latched error — never `None`, never an empty
///   answer — until `recover` succeeds. A failed `recover` latches its own
///   error in place of the earlier one. `recover` waits for in-flight calls,
///   is idempotent and may be retried after it fails. Which of these
///   failures latch is up to the store; `RedbStore` latches `Io` and `Full`
///   on any call, every error of redb's commit, and a failed `recover`.
/// - A referenced object that is absent is `Missing`
///   ([`ObjectReader::load_object`]); a stored object whose bytes do not hash
///   to its id is `Corrupt`.
///
/// Reads may run concurrently with each other and with a commit; they see
/// either the state before the commit or after it.
pub trait VersionedStore: ObjectReader {
    /// The database's stable identity.
    fn database_id(&self) -> DatabaseId;

    /// The durable head, or `None` before the first commit.
    fn head(&self) -> Result<Option<Head>, StorageError>;

    /// Returns the committed record under `key` in `table`, or `None`.
    fn read_record(&self, table: Table, key: &[u8]) -> Result<Option<Vec<u8>>, StorageError>;

    /// Reads a record only if its stored value is at most `max_bytes`.
    ///
    /// Like [`ObjectReader::read_object_bounded`], checks length before
    /// copying and returns `Corrupt` for an oversized stored value. The
    /// bound applies to the value, not the caller-supplied lookup key or
    /// backend page buffers. All record tables, including metadata and
    /// manifests, follow the same inclusive bound and counter semantics.
    fn read_record_bounded(
        &self,
        table: Table,
        key: &[u8],
        max_bytes: usize,
    ) -> Result<Option<Vec<u8>>, StorageError>;

    /// Applies a batch atomically against the expected head.
    fn commit(&self, batch: CommitBatch) -> CommitOutcome;

    /// Whether calls are served or a failure has latched.
    fn health(&self) -> Health;

    /// Drops any live handle, reopens the durable state (running the medium's
    /// crash repair if needed) and returns the durable head.
    ///
    /// Clears a latched failure on success. On failure the store may be left
    /// [`Health::Failed`] with the returned error (a disk store has no handle
    /// until a later `recover` succeeds). It waits for in-flight calls to
    /// finish, is idempotent, and can be retried after an error.
    fn recover(&self) -> Result<Option<Head>, StorageError>;

    /// Physical and logical counters since the store was created (see
    /// [`StoreStats`]). Never fails.
    fn stats(&self) -> StoreStats;
}

impl<T: ObjectReader + ?Sized> ObjectReader for Arc<T> {
    fn read_object(&self, space: SpaceId, id: &ObjectId) -> Result<Option<Vec<u8>>, StorageError> {
        (**self).read_object(space, id)
    }
    fn read_object_bounded(
        &self,
        space: SpaceId,
        id: &ObjectId,
        max_bytes: usize,
    ) -> Result<Option<Vec<u8>>, StorageError> {
        (**self).read_object_bounded(space, id, max_bytes)
    }
}

impl<T: VersionedStore + ?Sized> VersionedStore for Arc<T> {
    fn database_id(&self) -> DatabaseId {
        (**self).database_id()
    }
    fn head(&self) -> Result<Option<Head>, StorageError> {
        (**self).head()
    }
    fn read_record(&self, table: Table, key: &[u8]) -> Result<Option<Vec<u8>>, StorageError> {
        (**self).read_record(table, key)
    }
    fn read_record_bounded(
        &self,
        table: Table,
        key: &[u8],
        max_bytes: usize,
    ) -> Result<Option<Vec<u8>>, StorageError> {
        (**self).read_record_bounded(table, key, max_bytes)
    }
    fn commit(&self, batch: CommitBatch) -> CommitOutcome {
        (**self).commit(batch)
    }
    fn health(&self) -> Health {
        (**self).health()
    }
    fn recover(&self) -> Result<Option<Head>, StorageError> {
        (**self).recover()
    }
    fn stats(&self) -> StoreStats {
        (**self).stats()
    }
}

impl<T: ObjectReader + ?Sized> ObjectReader for Box<T> {
    fn read_object(&self, space: SpaceId, id: &ObjectId) -> Result<Option<Vec<u8>>, StorageError> {
        (**self).read_object(space, id)
    }
    fn read_object_bounded(
        &self,
        space: SpaceId,
        id: &ObjectId,
        max_bytes: usize,
    ) -> Result<Option<Vec<u8>>, StorageError> {
        (**self).read_object_bounded(space, id, max_bytes)
    }
}

impl<T: VersionedStore + ?Sized> VersionedStore for Box<T> {
    fn database_id(&self) -> DatabaseId {
        (**self).database_id()
    }
    fn head(&self) -> Result<Option<Head>, StorageError> {
        (**self).head()
    }
    fn read_record(&self, table: Table, key: &[u8]) -> Result<Option<Vec<u8>>, StorageError> {
        (**self).read_record(table, key)
    }
    fn read_record_bounded(
        &self,
        table: Table,
        key: &[u8],
        max_bytes: usize,
    ) -> Result<Option<Vec<u8>>, StorageError> {
        (**self).read_record_bounded(table, key, max_bytes)
    }
    fn commit(&self, batch: CommitBatch) -> CommitOutcome {
        (**self).commit(batch)
    }
    fn health(&self) -> Health {
        (**self).health()
    }
    fn recover(&self) -> Result<Option<Head>, StorageError> {
        (**self).recover()
    }
    fn stats(&self) -> StoreStats {
        (**self).stats()
    }
}

/// The result of [`reconcile`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reconciliation {
    /// The durable head is the batch's new head and every record of the batch
    /// is stored with identical bytes: the batch committed.
    Committed,
    /// The durable head is still the batch's expected head: the batch did
    /// not commit and may be retried unchanged.
    NotCommitted,
    /// Durable state matches neither: another head, or the new head with
    /// different records. The caller must stop writing.
    Diverged {
        /// The durable head found.
        head: Option<Head>,
    },
}

/// Decides from durable state whether `batch` committed, after an
/// `Indeterminate` outcome or a lost acknowledgement.
///
/// Calls [`VersionedStore::recover`] first, so a latched handle is replaced
/// and the answer is read through a fresh handle rather than from state the
/// failed handle kept. Records are compared byte for byte; objects are not
/// read back, because a commit applies them in the same atomic transaction
/// as the head and records. Because a batch always advances the head height,
/// a head equal to the expected head proves the batch is absent. The store
/// never calls this itself.
///
/// # Durability after a failed sync
///
/// A fresh handle still reads through the operating system's page cache.
/// When the indeterminate outcome came from a failed sync, the bytes read
/// back may never have reached the medium: after an `fsync` error, Linux can
/// mark the unwritten pages clean, so they keep serving reads but a later
/// sync does not write them. `Committed` then means "committed as far as the
/// operating system reports", not "durable". A caller that must acknowledge
/// only durable blocks needs more than this answer after a sync failure (for
/// example, stopping until the data is reread from the medium after the page
/// cache is dropped).
pub fn reconcile<S: VersionedStore + ?Sized>(
    store: &S,
    batch: &CommitBatch,
) -> Result<Reconciliation, StorageError> {
    let head = store.recover()?;
    if head == Some(batch.new_head) {
        for (table, key, value) in &batch.records {
            if store.read_record(*table, key)?.as_deref() != Some(value.as_slice()) {
                return Ok(Reconciliation::Diverged { head });
            }
        }
        Ok(Reconciliation::Committed)
    } else if head == batch.expected_head {
        Ok(Reconciliation::NotCommitted)
    } else {
        Ok(Reconciliation::Diverged { head })
    }
}

/// Counters for one object space.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SpaceStats {
    /// `read_object` calls that returned an answer (present or absent).
    pub reads: u64,
    /// Of those, the calls that found an object.
    pub hits: u64,
    /// Object bytes returned.
    pub read_bytes: u64,
    /// Objects newly stored by committed batches (idempotent repeats excluded).
    pub objects_written: u64,
    /// Bytes of those objects.
    pub bytes_written: u64,
}

/// Backend I/O counters, where the store can observe its medium.
///
/// These are physical measurements of one process's calls into the storage
/// medium (for redb: its `StorageBackend`), below every cache the store
/// keeps. They are non-canonical and never enter charges or hashes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BackendStats {
    /// Read calls.
    pub reads: u64,
    /// Bytes read.
    pub read_bytes: u64,
    /// Write calls.
    pub writes: u64,
    /// Bytes written.
    pub write_bytes: u64,
    /// Durability barriers (`fsync`/`fdatasync` equivalents).
    pub syncs: u64,
    /// Length changes.
    pub set_lens: u64,
}

impl BackendStats {
    /// Field-wise difference `self − earlier`, saturating at zero.
    pub fn since(&self, earlier: &Self) -> Self {
        Self {
            reads: self.reads.saturating_sub(earlier.reads),
            read_bytes: self.read_bytes.saturating_sub(earlier.read_bytes),
            writes: self.writes.saturating_sub(earlier.writes),
            write_bytes: self.write_bytes.saturating_sub(earlier.write_bytes),
            syncs: self.syncs.saturating_sub(earlier.syncs),
            set_lens: self.set_lens.saturating_sub(earlier.set_lens),
        }
    }
}

/// Logical and physical counters of a store.
///
/// Logical counters count calls through this contract; `backend` counts the
/// medium's I/O when the store can observe it. All counters are
/// measurements: they are never part of canonical charges.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StoreStats {
    /// Object reads and writes per space; spaces never touched are absent.
    pub spaces: BTreeMap<SpaceId, SpaceStats>,
    /// `read_record` calls that returned an answer.
    pub record_reads: u64,
    /// Record bytes returned.
    pub record_read_bytes: u64,
    /// `head` calls that returned an answer.
    pub head_reads: u64,
    /// Records newly stored by committed batches.
    pub records_written: u64,
    /// Commits that returned `Committed`.
    pub committed: u64,
    /// Commits that returned `NotCommitted`.
    pub not_committed: u64,
    /// Commits that returned `Indeterminate`.
    pub indeterminate: u64,
    /// Successful `recover` calls.
    pub recoveries: u64,
    /// Backend I/O, when known.
    pub backend: Option<BackendStats>,
}

impl StoreStats {
    /// The counters of one space (zero if it was never touched).
    pub fn space(&self, space: SpaceId) -> SpaceStats {
        self.spaces.get(&space).copied().unwrap_or_default()
    }

    /// Object reads summed over every space.
    pub fn object_reads(&self) -> u64 {
        self.spaces.values().map(|space| space.reads).sum()
    }

    /// Field-wise difference `self − earlier`, saturating at zero; spaces
    /// with no activity in between are omitted.
    pub fn since(&self, earlier: &Self) -> Self {
        let mut spaces = BTreeMap::new();
        for (space, now) in &self.spaces {
            let before = earlier.space(*space);
            let delta = SpaceStats {
                reads: now.reads.saturating_sub(before.reads),
                hits: now.hits.saturating_sub(before.hits),
                read_bytes: now.read_bytes.saturating_sub(before.read_bytes),
                objects_written: now.objects_written.saturating_sub(before.objects_written),
                bytes_written: now.bytes_written.saturating_sub(before.bytes_written),
            };
            if delta != SpaceStats::default() {
                spaces.insert(*space, delta);
            }
        }
        Self {
            spaces,
            record_reads: self.record_reads.saturating_sub(earlier.record_reads),
            record_read_bytes: self
                .record_read_bytes
                .saturating_sub(earlier.record_read_bytes),
            head_reads: self.head_reads.saturating_sub(earlier.head_reads),
            records_written: self.records_written.saturating_sub(earlier.records_written),
            committed: self.committed.saturating_sub(earlier.committed),
            not_committed: self.not_committed.saturating_sub(earlier.not_committed),
            indeterminate: self.indeterminate.saturating_sub(earlier.indeterminate),
            recoveries: self.recoveries.saturating_sub(earlier.recoveries),
            backend: match (self.backend, earlier.backend) {
                (Some(now), Some(before)) => Some(now.since(&before)),
                (now, _) => now,
            },
        }
    }
}

/// Shared bookkeeping for [`StoreStats`], for store implementations.
///
/// Every method takes a short internal lock; counters are updated only after
/// the call they describe has an answer.
#[derive(Debug, Default)]
pub struct StatsRecorder {
    stats: Mutex<StoreStats>,
}

impl StatsRecorder {
    /// Creates zeroed counters.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, StoreStats> {
        // Counters stay meaningful even if a panicking thread held the lock.
        self.stats
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Records an answered object read: `Some(len)` if found.
    pub fn object_read(&self, space: SpaceId, found: Option<usize>) {
        let mut stats = self.lock();
        let entry = stats.spaces.entry(space).or_default();
        entry.reads += 1;
        if let Some(len) = found {
            entry.hits += 1;
            entry.read_bytes += len as u64;
        }
    }

    /// Records an answered record read: `Some(len)` if found.
    pub fn record_read(&self, found: Option<usize>) {
        let mut stats = self.lock();
        stats.record_reads += 1;
        if let Some(len) = found {
            stats.record_read_bytes += len as u64;
        }
    }

    /// Records an answered head read.
    pub fn head_read(&self) {
        self.lock().head_reads += 1;
    }

    /// Records a commit outcome, and for `Committed` the objects (space and
    /// byte length) and the number of records it newly stored.
    pub fn commit(
        &self,
        outcome: &CommitOutcome,
        new_objects: &[(SpaceId, usize)],
        new_records: u64,
    ) {
        let mut stats = self.lock();
        match outcome {
            CommitOutcome::Committed => {
                stats.committed += 1;
                stats.records_written += new_records;
                for (space, len) in new_objects {
                    let entry = stats.spaces.entry(*space).or_default();
                    entry.objects_written += 1;
                    entry.bytes_written += *len as u64;
                }
            }
            CommitOutcome::NotCommitted(_) => stats.not_committed += 1,
            CommitOutcome::Indeterminate(_) => stats.indeterminate += 1,
        }
    }

    /// Records a successful recovery.
    pub fn recovered(&self) {
        self.lock().recoveries += 1;
    }

    /// Returns the counters, with the given backend counters attached.
    pub fn snapshot(&self, backend: Option<BackendStats>) -> StoreStats {
        let mut stats = self.lock().clone();
        stats.backend = backend;
        stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(height: u64, tag: u8) -> Head {
        Head {
            height,
            hash: [tag; 32],
        }
    }

    #[test]
    fn head_encoding_round_trips_and_rejects_other_lengths() {
        let value = Head {
            height: 0x0102_0304_0506_0708,
            hash: [0xab; 32],
        };
        let bytes = value.encode();
        assert_eq!(&bytes[..8], &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(&bytes[8..], &[0xab; 32]);
        assert_eq!(Head::decode(&bytes), Ok(value));
        assert_eq!(
            Head::decode(&bytes[..39]).unwrap_err().kind,
            StorageErrorKind::Corrupt
        );
    }

    #[test]
    fn record_shapes_are_checked_per_table() {
        let ok = |table: Table, key: &[u8], value: &[u8]| table.check_record(key, value).is_ok();
        assert!(ok(Table::Meta, b"genesis_hash", b""));
        assert!(!ok(Table::Meta, b"", b""));
        assert!(!ok(Table::Meta, &[0xff], b""));
        assert!(!ok(Table::Meta, &[b'a'; 65], b""));
        assert!(!ok(Table::Meta, b"store/head", b""));
        assert!(ok(Table::Blocks, &height_key(7), b"block"));
        assert!(!ok(Table::Blocks, &[0; 7], b"block"));
        assert!(!ok(Table::Manifests, &[0; 9], b"m"));
        assert!(ok(Table::Inclusions, &[1; 256], b""));
        assert!(!ok(Table::Inclusions, &[], b""));
        assert!(!ok(Table::Inclusions, &[1; 257], b""));
        assert!(ok(Table::BlockIndex, &[3; 32], &height_key(3)));
        assert!(!ok(Table::BlockIndex, &[3; 31], &height_key(3)));
        assert!(!ok(Table::BlockIndex, &[3; 32], &[0; 4]));
    }

    #[test]
    fn validate_rejects_non_advancing_heads_and_batch_conflicts() {
        assert!(CommitBatch::new(None, head(0, 1)).validate().is_ok());
        let same = CommitBatch::new(Some(head(4, 1)), head(4, 2));
        assert_eq!(
            same.validate().unwrap_err().kind,
            StorageErrorKind::Incompatible
        );
        let back = CommitBatch::new(Some(head(4, 1)), head(3, 2));
        assert_eq!(
            back.validate().unwrap_err().kind,
            StorageErrorKind::Incompatible
        );

        let mut dup = CommitBatch::new(None, head(0, 1));
        dup.put_object(SpaceId::Host, [1; 32], vec![1])
            .put_object(SpaceId::Host, [1; 32], vec![1])
            .put_object(SpaceId::Catalog, [1; 32], vec![2])
            .put_record(Table::Blocks, height_key(0), b"b".to_vec())
            .put_record(Table::Blocks, height_key(0), b"b".to_vec());
        assert!(dup.validate().is_ok());

        let mut conflict = dup.clone();
        conflict.put_object(SpaceId::Host, [1; 32], vec![9]);
        assert_eq!(
            conflict.validate().unwrap_err().kind,
            StorageErrorKind::Conflict
        );
        let mut conflict = dup;
        conflict.put_record(Table::Blocks, height_key(0), b"c".to_vec());
        assert_eq!(
            conflict.validate().unwrap_err().kind,
            StorageErrorKind::Conflict
        );
    }

    #[test]
    fn stats_differences_saturate_and_drop_idle_spaces() {
        let recorder = StatsRecorder::new();
        recorder.object_read(SpaceId::Namespace(1), Some(10));
        recorder.object_read(SpaceId::Catalog, None);
        let before = recorder.snapshot(None);
        recorder.object_read(SpaceId::Namespace(1), Some(5));
        recorder.record_read(Some(3));
        let after = recorder.snapshot(None);
        let delta = after.since(&before);
        assert_eq!(delta.spaces.len(), 1);
        assert_eq!(
            delta.space(SpaceId::Namespace(1)),
            SpaceStats {
                reads: 1,
                hits: 1,
                read_bytes: 5,
                objects_written: 0,
                bytes_written: 0,
            }
        );
        assert_eq!(delta.record_reads, 1);
        assert_eq!(delta.record_read_bytes, 3);
        assert_eq!(before.since(&after).object_reads(), 0);
    }

    #[test]
    fn random_database_ids_differ() {
        assert_ne!(DatabaseId::random(), DatabaseId::random());
        assert_eq!(DatabaseId([0xa5; 16]).to_string(), "a5".repeat(16));
    }
}
