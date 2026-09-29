/// A single mutation in a [`WriteBatch`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WriteOperation {
    /// Insert a key or replace its existing value.
    Put {
        /// Raw storage key.
        key: Vec<u8>,
        /// New value.
        value: Vec<u8>,
    },
    /// Remove a key, if present.
    Delete {
        /// Raw storage key.
        key: Vec<u8>,
    },
}

/// Owned mutations to apply together, preserving their submission order.
///
/// Building or dropping a batch has no effect on a store. Submit it through
/// [`KvStore::write_batch`](crate::KvStore::write_batch) to apply it.
#[derive(Clone, Debug, Default)]
pub struct WriteBatch {
    operations: Vec<WriteOperation>,
}

impl WriteBatch {
    /// Creates an empty batch.
    pub const fn new() -> Self {
        Self {
            operations: Vec::new(),
        }
    }

    /// Appends an insert or replacement and returns the batch for chaining.
    pub fn put(&mut self, key: impl Into<Vec<u8>>, value: impl Into<Vec<u8>>) -> &mut Self {
        self.operations.push(WriteOperation::Put {
            key: key.into(),
            value: value.into(),
        });
        self
    }

    /// Appends a deletion and returns the batch for chaining.
    pub fn delete(&mut self, key: impl Into<Vec<u8>>) -> &mut Self {
        self.operations
            .push(WriteOperation::Delete { key: key.into() });
        self
    }
}

impl IntoIterator for WriteBatch {
    type Item = WriteOperation;
    type IntoIter = std::vec::IntoIter<WriteOperation>;

    fn into_iter(self) -> Self::IntoIter {
        self.operations.into_iter()
    }
}
