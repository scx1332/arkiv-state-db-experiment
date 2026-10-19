//! The append-only Arkiv pseudo node: block format, deterministic block
//! execution, request identity, block selection and replay (node step
//! N0b-3), plus the volatile protocol-2 full/light process demo in [`demo`].
//! The explicitly unsigned [`simulator`] stores execution state, canonical
//! bodies, inclusion identities and metadata atomically in one durable file.
//!
//! - [`block`]: block format version 1 — genesis, headers, block inputs,
//!   transaction envelopes and outcomes — with canonical encodings and
//!   domain-separated hashes (see `docs/block-format.md`).
//! - [`params`]: the genesis invariants, the expiry reservation and the
//!   expiry sweep's read bound; [`ChainSpec`] is a validated genesis.
//! - [`validate`]: static validation of requests and blocks, applied to
//!   each operation as it will execute.
//! - [`select`]: the producer's selection rules and their replay check.
//! - [`identity`]: request identities and the pending/inclusion index.
//! - [`exec`]: [`execute_block`], the one function that produces and
//!   replays a block: admin inputs, the expiry sweep, then user
//!   transactions with per-transaction rollback.
//! - [`replay`]: [`MemoryChain`], a chain on the memory engine, and
//!   [`replay`](replay::replay) from genesis.
//!
//! The executor reaches the engine only through its fallible `try_` API
//! (enforced by this crate's `clippy.toml`), so a storage error of the
//! persistent engine aborts the candidate instead of becoming a rejection or
//! an empty read. The signed demo remains an independent volatile profile.

pub mod accounts;
pub mod arkiv;
pub mod block;
pub mod demo;
pub mod exec;
pub mod gas;
pub mod genesis_authoring;
mod genesis_bulk;
pub mod genesis_bundle;
mod genesis_import_resources;
pub mod genesis_streaming;
pub mod identity;
pub mod params;
pub mod replay;
pub mod retention;
pub mod select;
pub mod signed;
pub mod simulator;
pub mod validate;
pub mod value_json;

pub use block::{
    AdminAction, AdminInput, AdminOutcome, AdminRejection, Block, BlockInputs, BlockOutcomes,
    ChainParams, EXPIRES_AT, Expiry, ExpiryDeletion, ExpiryFault, FORMAT_VERSION, Genesis, Header,
    HostFault, OpOutcome, PROTOCOL_VERSION, RequestId, TxEnvelope, TxOp, TxOutcome, TxStatus,
};
pub use exec::{ExecError, ExecutedBlock, Invariant, ScheduleFault, execute_block};
pub use identity::{Inclusion, RequestIdentity, RequestIndex, RequestStatus, Scope, Submission};
pub use params::{ChainSpec, GenesisError, MAX_ADMIN_BYTES, ReservationFault};
pub use replay::{ChainError, MemoryChain, PreparedBlock, ReplayError, replay};
pub use select::{Selection, SelectionFault, select, select_inputs};
pub use validate::{AdminFault, BlockFault, ExpiryArgumentFault, OpFault, SuccessorFault, TxFault};
