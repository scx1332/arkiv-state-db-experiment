//! A redb adapter for the versioned storage contract.
//!
//! [`RedbStore`] implements
//! [`VersionedStore`](arkiv_db_storage::versioned::VersionedStore) in one
//! redb file (redb 4.3.0, pinned exactly). It is the storage candidate of
//! node step N2; `docs/redb-experiment.md` states its pass criteria and the
//! measured results. This crate adds no engine integration: it stores the
//! objects, records and head that callers commit.
//!
//! # Layout
//!
//! | Table | Key → value | Contents |
//! | --- | --- | --- |
//! | `meta` | `&str` → bytes | `store/format` (u32 BE), `store/database_id` (16 bytes), `store/head` ([`Head::encode`](arkiv_db_storage::versioned::Head::encode)), and the caller's metadata records |
//! | `blocks` | `u64` → bytes | block records by height |
//! | `manifests` | `u64` → bytes | manifest records by height |
//! | `inclusions` | bytes → bytes | inclusion records |
//! | `block_index` | `[u8; 32]` → `u64` | block height by hash |
//! | `objects_catalog` | `[u8; 32]` → bytes | catalog objects |
//! | `objects_host` | `[u8; 32]` → bytes | host objects |
//! | `objects_ns_<16 hex digits>` | `[u8; 32]` → bytes | one table per namespace, created by its first object |
//!
//! Engine storage v1 uses an explicitly identified engine-only profile in
//! caller metadata: head height is a snapshot sequence, head hash is a
//! domain-separated manifest digest, and `block_index` maps that digest to
//! the sequence. `blocks` and `inclusions` remain unused in that profile.
//! These shapes require no adapter-format change and are not node block
//! identities. Callers must validate their profile before interpreting them.
//!
//! # Guarantees and their mechanism
//!
//! - Every commit is one redb `WriteTransaction` covering the head check,
//!   all tables and the new head, with `Durability::Immediate` and
//!   quick repair on (two-phase commit plus a saved allocator state), so
//!   `Committed` means redb's commit returned after its final sync. The
//!   store's own initialization commit uses the same settings.
//! - [`RedbStore::create`] builds a new store in a temporary file
//!   ([`initialization_path`]) and renames it into place only after the
//!   initialization commit, then syncs the directory, so a crash while
//!   creating never leaves a partial file under the store's name.
//! - Historical objects are ordinary immutable entries of the current
//!   tables. The store never creates savepoints and never restores one to
//!   answer a read.
//! - Every read opens a fresh redb `ReadTransaction` and drops it before
//!   returning, so no read pins an old redb version across calls. Object
//!   reads verify `keccak256(bytes) == id`.
//! - Bounded object and record reads inspect the borrowed redb value guard's
//!   length before copying it into the returned buffer. An oversized value
//!   is `Corrupt`, does not count as an answered read, and does not latch
//!   the store. This bounds that value copy, not redb's pager/I/O buffers.
//! - The redb `Database` lives in a swappable slot. Any redb I/O error
//!   (including `PreviousIo`) latches the store `Failed`; every call then
//!   returns that error until [`recover`](arkiv_db_storage::versioned::VersionedStore::recover)
//!   drops the handle, reopens the file (running redb's repair) and checks
//!   the database identity; a failed `recover` latches its own error.
//!   Errors before redb's `commit()` are `NotCommitted`; any error from
//!   `commit()` itself is `Indeterminate` and latches whatever its kind,
//!   because redb refuses further writes on that handle. Other errors (a
//!   corrupted page or object, a conflict) do not latch. The store never
//!   resolves an indeterminate commit itself.
//! - redb's page cache is set explicitly ([`RedbConfig::cache_size`],
//!   [`DEFAULT_CACHE_SIZE`] by default instead of redb's 1 GiB).
//!
//! # Backends
//!
//! Every store reads and writes through a [`CountingBackend`], so
//! [`StoreStats::backend`](arkiv_db_storage::versioned::StoreStats) reports
//! the actual backend I/O. [`testing::CrashDisk`] is a simulated device with
//! a power-loss and torn-write model, fault injection and a space quota, for
//! [`RedbStore::with_backend`].

mod counting;
mod error;
mod metrics;
mod store;
pub mod testing;

pub use counting::{CountingBackend, IoProbe};
pub use metrics::{DURATION_BUCKETS_NANOSECONDS, OperationTimingSnapshot};
pub use store::{
    DEFAULT_CACHE_SIZE, DataStats, LayoutStats, OpenReport, RedbConfig, RedbStore,
    STORE_FORMAT_VERSION, TableLayout, initialization_path, object_table_name,
};
