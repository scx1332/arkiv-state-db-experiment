//! The integration tests of `arkiv-db-redb`, one module per area, in one
//! binary so that the shared helpers are compiled once.

#[path = "../common/mod.rs"]
mod common;
#[path = "../../../arkiv-db/tests/support/durable_history.rs"]
mod durable_history;
#[path = "../engine_support/mod.rs"]
mod engine_support;

mod atomicity;
mod backends;
mod bounded_reads;
mod contract;
mod disk_full;
mod durability;
mod engine_crashes;
mod engine_history;
mod engine_process;
mod gc;
mod history;
mod initialization;
mod isolation;
mod latching;
mod reconciliation;
mod records;
mod recovery;
mod relabel;
mod streaming_import;
mod write_amplification;
