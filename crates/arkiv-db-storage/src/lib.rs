//! Storage contracts shared by database backends.
//!
//! [`versioned`] is the contract durable engine state and block history are
//! built on: immutable content-addressed objects, insert-only records and one
//! durable head committed atomically, with explicit commit outcomes and store
//! health.
//!
//! With the `testing` feature, the `testing` module provides a
//! fault-injecting wrapper and the behavioural suite every
//! [`versioned::VersionedStore`] implementation runs.

pub mod gc;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
pub mod versioned;

/// An owned key/value pair returned by a scan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The raw storage key, without text or record-identity interpretation.
    pub key: Vec<u8>,
    /// The value stored under the key; an empty value is valid.
    pub value: Vec<u8>,
}
