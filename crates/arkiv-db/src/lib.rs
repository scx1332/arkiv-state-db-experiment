//! The embedded database entry point.
//!
//! Two layers live here:
//!
//! - [`Database`]: the raw byte-key KV facade over a [`KvStore`] backend. It
//!   is the storage foundation and the boundary a future durable backend
//!   implements. It has no namespaces, snapshots or metering.
//! - [`Engine`]: the application-facing block engine. Every read and write
//!   names a namespace, is metered with checked integer arithmetic under
//!   counting-rule version [`MODEL_VERSION`], and runs against an immutable
//!   snapshot or a candidate branch. Each namespace holds a raw byte map and,
//!   as sibling fields, a typed record store: key bindings, rows and a term
//!   index that every record write maintains in the same operation. The
//!   declared query subset (typed equality, inequality, integer ranges,
//!   string prefixes, existence and negation in an ordered flat DNF) is
//!   evaluated lazily in record-id order with projection and keyset
//!   pagination; `count` and `terms` (the distinct indexed values of an
//!   attribute in value order, read positionally) are separate metered
//!   reads; a [`Cursor`] is bound to one snapshot of one engine, or to
//!   one branch state, and never re-resolves head. Instance parameters are
//!   [`Limits`]. The engine keeps its state in structurally shared Merkle
//!   Patricia maps (`arkiv_db_trie::MerkleMap`) and shares nothing with the
//!   raw facade, so nothing reachable through it bypasses namespace or
//!   metering rules; the metered data-plane reads are
//!   `get`, `scan_prefix`, `descriptor`, `get_record`, `query`, `count` and
//!   `terms`, and the only other calls that read state are count and
//!   map-identity statistics that return no keys or values, a hidden
//!   index-consistency test aid, and the host-state calls, which see only
//!   host-owned entries.
//!
//! The host owns consensus, block numbers and expiry: [`expire_records`]
//! deletes expired records through the host path in the block where they
//! expire, sweeping the expired heights in ascending order (`terms` reads
//! the smallest live height, its rows are deleted as `Eq` pages, then the
//! next height), and the engine assigns `$`-cells no meaning. Each state
//! also holds a small host-owned map (32-byte keys, values of at most
//! [`HOST_VALUE_MAX`] bytes) for the host's own deterministic schedules,
//! reachable only through unmetered `try_host_*` calls and never through
//! the data plane.
//!
//! Every state has a logical state root ([`Snapshot::state_root`],
//! [`SealedCandidate::state_root`]): the digest of its [`StateHeader`],
//! which commits to the catalog, every namespace's maps and the host map
//! through their trie roots (see [`state`]). It depends only on the logical
//! content.
//!
//! Every [`Branch`] and [`Snapshot`] method that walks state has a `try_`
//! form returning `Result<_, StorageError>`. On an engine built in memory
//! they never fail; on one that reads committed map nodes they fail on
//! storage errors, and a storage error poisons a branch (see
//! [the engine's fallible surface](engine#fallible-surface)).
//!
//! [`codec`] is the canonical binary encoding (codec version
//! [`CODEC_VERSION`]) of the engine's types, with the domain-separated
//! [`digest`] that every block and state hash uses.
//!
//! [`proof`] supplies bounded complete equality pages, complete numeric ranges
//! without pagination, and stateless
//! verifier, with separately versioned advisory proof-read accounting.
//! [`durable::DurableEngine`] persists the same state and historical proof
//! material with lazy reads and bounded encoded caching/preparation. The
//! ordinary [`Engine`] remains volatile. Broader proofs and durable node
//! integration remain separate work.

pub mod codec;
pub mod durable;
pub mod engine;
pub mod host;
mod layout;
pub mod meter;
pub mod namespace;
mod nodes;
pub mod proof;
pub mod query;
pub mod record;
pub mod state;
mod typed;

pub use arkiv_db_memory::MemoryStore;
pub use arkiv_db_storage::{
    Entry, KvStore, WriteBatch, WriteOperation,
    versioned::{StorageError, StorageErrorKind},
};
pub use codec::{
    B256, CODEC_VERSION, Decode, DecodeError, Decoder, Encode, Encoder, decode_exact, digest,
    encode,
};
pub use engine::{
    Branch, Checkpoint, DEFAULT_NAMESPACE_CAP, Effect, Engine, EngineError, Executed,
    HOST_VALUE_MAX, HostKey, HostStateError, Metered, ReadResult, Rejected, SealedCandidate,
    Snapshot, SnapshotId, WriteOp, WriteResult,
};
pub use host::{BlockMap, BlockNumber, ExpiryHalt, HostError, expire_records, try_expire_records};
pub use meter::{
    Budget, Cost, Ledger, LedgerEntry, MODEL_VERSION, OperationClass, Outcome, PhysicalTrace,
    PriceSchedule, Receipt, ReceiptKind, Rejection,
};
pub use namespace::{NamespaceDescriptor, NamespaceId, NamespaceName, OwnerId, Revision};
pub use query::{CellSelect, Cursor, Literal, Projection, Query, QueryFault, QueryPage, Term};
pub use record::{
    ArgumentFault, Cell, CellKind, CellName, Float32, Float64, Limits, NameFault, RecordId,
    RecordKey, Row, TypeTag, Value,
};
pub use state::{
    MapKind, NamespaceRoots, STATE_FORMAT, STATE_ROOT_DOMAIN, StagedObject, StateHeader,
};

/// A small embedded KV API over a chosen storage backend.
///
/// The default backend is volatile. This is the storage foundation, not the
/// namespace-scoped, metered data plane; see [`Engine`] for that.
#[derive(Debug, Default)]
pub struct Database<S = MemoryStore> {
    store: S,
}

impl Database<MemoryStore> {
    /// Creates an empty database whose contents are lost when it is dropped.
    pub const fn in_memory() -> Self {
        Self::from_store(MemoryStore::new())
    }
}

impl<S: KvStore> Database<S> {
    /// Wraps an existing storage backend, preserving its contents.
    pub const fn from_store(store: S) -> Self {
        Self { store }
    }

    /// Returns an owned value, or `None` for a missing key.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, S::Error> {
        self.store.get(key)
    }

    /// Inserts or replaces a value. Empty keys and values are allowed.
    pub fn put(
        &mut self,
        key: impl Into<Vec<u8>>,
        value: impl Into<Vec<u8>>,
    ) -> Result<(), S::Error> {
        let mut batch = WriteBatch::new();
        batch.put(key, value);
        self.write_batch(batch)
    }

    /// Removes a key. Deleting a missing key succeeds.
    pub fn delete(&mut self, key: impl Into<Vec<u8>>) -> Result<(), S::Error> {
        let mut batch = WriteBatch::new();
        batch.delete(key);
        self.write_batch(batch)
    }

    /// Applies all mutations together in submission order.
    ///
    /// No intermediate state is visible. An error leaves the store unchanged.
    pub fn write_batch(&mut self, batch: WriteBatch) -> Result<(), S::Error> {
        self.store.write_batch(batch)
    }

    /// Returns matching entries in bytewise key order; an empty prefix matches all.
    ///
    /// All matches are collected in memory. Returned entries remain unchanged
    /// by later writes to this database.
    pub fn scan_prefix(&self, prefix: &[u8]) -> Result<Vec<Entry>, S::Error> {
        self.store.scan_prefix(prefix)
    }
}

/// Version of the logical trie layout and of the state and proof formats
/// built on it. Version 2 stores term, record and raw keys unpadded
/// (prefix-free variable-length keys); a store, genesis or proof of
/// version 1 is rejected before interpretation.
pub const LAYOUT_VERSION: u32 = 2;
