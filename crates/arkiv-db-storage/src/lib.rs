//! Storage contracts shared by database backends.
//!
//! Two contracts live here:
//!
//! - [`KvStore`]: the original ordered byte-key map behind the raw
//!   `Database` facade. It is single-version and has no durability outcome.
//! - [`versioned`]: immutable content-addressed objects, insert-only records
//!   and one durable head committed atomically, with explicit commit outcomes
//!   and store health. This is the contract durable engine state and block
//!   history are built on.
//!
//! [`testing`] provides a fault-injecting wrapper and the behavioural suite
//! every [`versioned::VersionedStore`] implementation runs.

mod batch;
pub mod testing;
pub mod versioned;

pub use batch::{WriteBatch, WriteOperation};

/// An owned key/value pair returned by a scan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The raw storage key, without text or record-identity interpretation.
    pub key: Vec<u8>,
    /// The value stored under the key; an empty value is valid.
    pub value: Vec<u8>,
}

/// An ordered store of arbitrary byte keys and values.
///
/// Keys are unique and compare lexicographically as unsigned bytes. Empty keys
/// and values are valid. Reads return owned data that remains unchanged after
/// subsequent writes. No operation implies persistence: durability depends on
/// the backend.
pub trait KvStore {
    /// A backend's recoverable failure type.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Returns the current value, or `None` if the key does not exist.
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Self::Error>;

    /// Returns all entries whose keys start with `prefix`, in ascending order.
    ///
    /// An empty prefix matches every key. This initial interface materializes
    /// all matching entries; it provides neither pagination nor a result cap.
    fn scan_prefix(&self, prefix: &[u8]) -> Result<Vec<Entry>, Self::Error>;

    /// Applies an entire batch in submission order, with no partial visibility.
    ///
    /// Puts replace existing values and deletes of missing keys succeed. The
    /// last operation for a repeated key wins. An empty batch is a no-op.
    /// If this returns an error, the store's logical contents must be unchanged.
    /// This contract does not promise recovery from panics or process crashes.
    fn write_batch(&mut self, batch: WriteBatch) -> Result<(), Self::Error>;
}
