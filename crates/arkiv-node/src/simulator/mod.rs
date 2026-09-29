//! Explicitly unsigned, durable development simulator. Independent of signed demo trust.
pub mod host;
pub(crate) mod inspection;
pub mod light;
pub mod metadata;
pub mod profile;
pub mod query_language;
pub mod runtime;
pub mod wire;
pub mod workload;

use std::fmt;
pub type Result<T> = std::result::Result<T, Error>;
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error(pub &'static str);
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for Error {}
impl From<arkiv_db::StorageError> for Error {
    fn from(e: arkiv_db::StorageError) -> Self {
        use arkiv_db::StorageErrorKind;
        Self(match e.kind {
            StorageErrorKind::Corrupt | StorageErrorKind::Missing => "StorageCorrupt",
            StorageErrorKind::Incompatible => "IdentityMismatch",
            _ => "StorageUnavailable",
        })
    }
}
impl From<arkiv_db::durable::DurableError> for Error {
    fn from(e: arkiv_db::durable::DurableError) -> Self {
        use arkiv_db::durable::DurableError;
        match e {
            DurableError::Storage(e) => e.into(),
            DurableError::ResourceLimit(_) => Self("CapacityReached"),
            _ => Self("StorageUnavailable"),
        }
    }
}
impl From<arkiv_db::proof::ProofError> for Error {
    fn from(e: arkiv_db::proof::ProofError) -> Self {
        use arkiv_db::proof::ProofError::*;
        Self(match e {
            UnsupportedVersion => "UnsupportedVersion",
            UnsupportedProof => "UnsupportedProof",
            InvalidRequest => "InvalidRequest",
            CursorMismatch => "CursorMismatch",
            ProofLimitExceeded => "ProofLimitExceeded",
            BudgetExceeded => "BudgetExceeded",
            StorageUnavailable => "StorageUnavailable",
            InvalidProof => "InvalidProof",
        })
    }
}
