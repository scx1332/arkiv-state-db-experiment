//! Mapping redb errors onto [`StorageError`] kinds.
//!
//! | redb error | kind |
//! | --- | --- |
//! | I/O error with `StorageFull`, `QuotaExceeded` or `FileTooLarge` | `Full` |
//! | any other I/O error, `PreviousIo`, `RepairAborted`, a poisoned lock | `Io` |
//! | `Corrupted`, a fixed store table missing | `Corrupt` |
//! | `UpgradeRequired`, not a redb file, a table of the wrong type, a value too large | `Incompatible` |
//! | `DatabaseAlreadyOpen` | `Locked` |
//! | `DatabaseClosed` | `Closed` |
//!
//! What latches a [`RedbStore`](crate::RedbStore) into `Failed`:
//!
//! - `Io` and `Full` from any call: after an I/O failure redb refuses
//!   further writes on the handle, and the store must be reopened;
//! - any error returned by redb's `commit()`, whatever its kind (the
//!   outcome is `Indeterminate`): redb discards its allocator state after a
//!   failed commit and refuses later write transactions on that handle;
//! - a failed `recover()`, which leaves no handle; its error replaces the
//!   earlier one.
//!
//! Anything else — a corrupted page or object found by a read, a conflict,
//! a head mismatch, a malformed batch — is reported on the call that found
//! it, and the handle stays usable for everything else.

use std::io;

use arkiv_db_storage::versioned::{StorageError, StorageErrorKind};

/// A [`StorageError`] that redb errors convert into with `?`.
#[derive(Debug)]
pub(crate) struct Fault(pub(crate) StorageError);

impl Fault {
    pub(crate) fn new(kind: StorageErrorKind, detail: impl Into<String>) -> Self {
        Self(StorageError::new(kind, detail))
    }
}

impl From<StorageError> for Fault {
    fn from(error: StorageError) -> Self {
        Self(error)
    }
}

impl From<Fault> for StorageError {
    fn from(fault: Fault) -> Self {
        fault.0
    }
}

/// Maps an I/O error: out of space is `Full`, everything else `Io`.
pub(crate) fn io_error(error: &io::Error) -> StorageError {
    let kind = match error.kind() {
        io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded | io::ErrorKind::FileTooLarge => {
            StorageErrorKind::Full
        }
        _ => StorageErrorKind::Io,
    };
    StorageError::new(kind, format!("I/O error: {error}"))
}

impl From<io::Error> for Fault {
    fn from(error: io::Error) -> Self {
        Self(io_error(&error))
    }
}

impl From<redb::StorageError> for Fault {
    fn from(error: redb::StorageError) -> Self {
        use redb::StorageError as E;
        Self(match error {
            E::Io(error) => io_error(&error),
            E::PreviousIo => StorageError::new(
                StorageErrorKind::Io,
                "an earlier I/O error left the redb handle unusable",
            ),
            E::Corrupted(detail) => StorageError::new(StorageErrorKind::Corrupt, detail),
            E::ValueTooLarge(len) => StorageError::new(
                StorageErrorKind::Incompatible,
                format!("value of {len} bytes exceeds redb's limit"),
            ),
            E::DatabaseClosed => {
                StorageError::new(StorageErrorKind::Closed, "redb database closed")
            }
            other => StorageError::new(StorageErrorKind::Io, other.to_string()),
        })
    }
}

impl From<redb::TableError> for Fault {
    fn from(error: redb::TableError) -> Self {
        use redb::TableError as E;
        match error {
            E::Storage(error) => error.into(),
            E::TableDoesNotExist(name) => Fault::new(
                StorageErrorKind::Corrupt,
                format!("store table {name} is missing"),
            ),
            other @ (E::TableTypeMismatch { .. }
            | E::TableIsMultimap(_)
            | E::TableIsNotMultimap(_)
            | E::TypeDefinitionChanged { .. }) => {
                Fault::new(StorageErrorKind::Incompatible, other.to_string())
            }
            other => Fault::new(StorageErrorKind::Io, other.to_string()),
        }
    }
}

impl From<redb::TransactionError> for Fault {
    fn from(error: redb::TransactionError) -> Self {
        match error {
            redb::TransactionError::Storage(error) => error.into(),
            other => Fault::new(StorageErrorKind::Io, other.to_string()),
        }
    }
}

impl From<redb::DatabaseError> for Fault {
    fn from(error: redb::DatabaseError) -> Self {
        use redb::DatabaseError as E;
        match error {
            // redb reports a file that is not a redb database (magic number
            // mismatch) as invalid data while opening it.
            E::Storage(redb::StorageError::Io(error))
                if error.kind() == io::ErrorKind::InvalidData =>
            {
                Fault::new(StorageErrorKind::Incompatible, error.to_string())
            }
            E::Storage(error) => error.into(),
            E::DatabaseAlreadyOpen => Fault::new(
                StorageErrorKind::Locked,
                "the database is already open for writing",
            ),
            E::UpgradeRequired(version) => Fault::new(
                StorageErrorKind::Incompatible,
                format!("redb file format {version} requires an upgrade"),
            ),
            other => Fault::new(StorageErrorKind::Io, other.to_string()),
        }
    }
}

impl From<redb::SetDurabilityError> for Fault {
    fn from(error: redb::SetDurabilityError) -> Self {
        Fault::new(StorageErrorKind::Io, error.to_string())
    }
}

impl From<redb::CommitError> for Fault {
    fn from(error: redb::CommitError) -> Self {
        match error {
            redb::CommitError::Storage(error) => error.into(),
            other => Fault::new(StorageErrorKind::Io, other.to_string()),
        }
    }
}

/// Whether an error of a read or of a staged commit latches the store: only
/// I/O failures do. (Errors of redb's `commit()` and of `recover()` always
/// latch; see the module documentation.)
pub(crate) fn latches(error: &StorageError) -> bool {
    matches!(error.kind, StorageErrorKind::Io | StorageErrorKind::Full)
}
