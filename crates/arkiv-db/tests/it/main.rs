//! The integration tests of `arkiv-db`, one module per area, in one binary
//! so that `support` is compiled once.

#[path = "../support/durable_history.rs"]
mod durable_history;
#[path = "../support/mod.rs"]
mod support;
#[path = "../../../arkiv-db-trie/tests/support/mod.rs"]
mod trie_support;

mod adopt;
mod blocks;
mod bounds;
mod cell_limits;
mod codec;
mod durable;
mod durable_failures;
mod durable_gc;
mod durable_retention;
mod eq_proofs;
mod golem_types;
mod history;
mod host_state;
mod interrupt;
mod isolation;
mod metering;
mod namespaces;
mod ordered_ranges;
mod pagination;
mod prefix_free_terms;
mod queries;
mod query_history;
mod query_isolation;
mod query_metering;
mod query_planner;
mod range_proofs;
mod records;
mod rehash;
mod rollback;
mod roots;
mod scalar_profile;
mod simulator_composition;
mod storage_faults;
mod streaming_import;
mod terms;
mod trace_regression;
