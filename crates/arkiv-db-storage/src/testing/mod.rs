//! Test support for [`VersionedStore`](crate::versioned::VersionedStore)
//! implementations and their callers.
//!
//! - [`FaultyStore`] wraps any store and injects read failures, commit
//!   outcomes, disk-full conditions, lost acknowledgements and failed
//!   recoveries, optionally latching by `RedbStore`'s rule.
//! - [`contract`] is the behavioural suite every store implementation runs,
//!   generated as one `#[test]` per case by
//!   [`versioned_store_contract!`](crate::versioned_store_contract).
//!
//! Nothing here is used by production code paths.

pub mod contract;
mod faulty;

pub use faulty::{CommitFault, DiskFull, FaultCounters, FaultyStore, ReadFault, ReadScope};
