//! Bounded volatile multi-process demo. Development authentication is not consensus.
pub mod auth;
pub mod runtime;
pub mod wire;
pub mod workload;

use serde::{Deserialize, Serialize};
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DemoError {
    UnsupportedVersion,
    UnsupportedProof,
    InvalidRequest,
    CursorMismatch,
    BlockNotSynced,
    UnknownBlock,
    HistoryUnavailable,
    ProofLimitExceeded,
    BudgetExceeded,
    ServerBusy,
    StorageUnavailable,
    InvalidHeader,
    ChainConflict,
    InvalidProof,
    CapacityReached,
    QueueFull,
    RequestConflict,
    Transport,
    ProductionFailed,
}
impl std::fmt::Display for DemoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for DemoError {}
pub type Result<T> = std::result::Result<T, DemoError>;
