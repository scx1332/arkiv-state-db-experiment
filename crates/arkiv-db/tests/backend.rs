use std::{cell::Cell, cell::RefCell, fmt, rc::Rc};

// A backend author should need only the public facade's exports.
use arkiv_db::{Database, Entry, KvStore, MemoryStore, WriteBatch, WriteOperation};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BackendError {
    Get,
    Scan,
    Write,
}

impl fmt::Display for BackendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "injected {self:?} failure")
    }
}

impl std::error::Error for BackendError {}

#[derive(Default)]
struct Probe {
    reject: Cell<Option<BackendError>>,
    batches: RefCell<Vec<Vec<WriteOperation>>>,
}

struct TestBackend {
    inner: MemoryStore,
    probe: Rc<Probe>,
}

impl KvStore for TestBackend {
    type Error = BackendError;

    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Self::Error> {
        if self.probe.reject.get() == Some(BackendError::Get) {
            return Err(BackendError::Get);
        }
        self.inner.get(key).map_err(|error| match error {})
    }

    fn scan_prefix(&self, prefix: &[u8]) -> Result<Vec<Entry>, Self::Error> {
        if self.probe.reject.get() == Some(BackendError::Scan) {
            return Err(BackendError::Scan);
        }
        self.inner
            .scan_prefix(prefix)
            .map_err(|error| match error {})
    }

    fn write_batch(&mut self, batch: WriteBatch) -> Result<(), Self::Error> {
        self.probe
            .batches
            .borrow_mut()
            .push(batch.clone().into_iter().collect());
        // Reject before mutation. This exercises the facade, not rollback of a
        // future backend that encounters I/O failure after starting a write.
        if self.probe.reject.get() == Some(BackendError::Write) {
            return Err(BackendError::Write);
        }
        self.inner
            .write_batch(batch)
            .map_err(|error| match error {})
    }
}

fn database() -> (Database<TestBackend>, Rc<Probe>) {
    let probe = Rc::new(Probe::default());
    let db = Database::from_store(TestBackend {
        inner: MemoryStore::new(),
        probe: Rc::clone(&probe),
    });
    (db, probe)
}

#[test]
fn backend_errors_are_preserved_and_rejected_blocks_can_be_retried() -> Result<(), BackendError> {
    let (mut db, probe) = database();
    let mut block_1 = WriteBatch::new();
    block_1
        .put(b"keep", b"original")
        .put(b"neighbor", b"unchanged");
    db.write_batch(block_1)?;
    let original = db.scan_prefix(b"")?;

    probe.reject.set(Some(BackendError::Get));
    assert_eq!(db.get(b"keep"), Err(BackendError::Get));
    probe.reject.set(Some(BackendError::Scan));
    assert_eq!(db.scan_prefix(b""), Err(BackendError::Scan));

    probe.reject.set(Some(BackendError::Write));
    assert_eq!(db.put(b"keep", b"replacement"), Err(BackendError::Write));
    assert_eq!(db.scan_prefix(b"")?, original);
    assert_eq!(db.delete(b"keep"), Err(BackendError::Write));
    assert_eq!(db.scan_prefix(b"")?, original);

    let mut block_2 = WriteBatch::new();
    block_2
        .put(b"new", b"created")
        .put(b"keep", b"intermediate")
        .delete(b"keep")
        .put(b"keep", b"recovered");
    assert_eq!(db.write_batch(block_2.clone()), Err(BackendError::Write));
    assert_eq!(db.scan_prefix(b"")?, original);

    probe.reject.set(None);
    db.write_batch(block_2)?;
    assert_eq!(db.get(b"keep")?, Some(b"recovered".to_vec()));
    assert_eq!(db.get(b"new")?, Some(b"created".to_vec()));
    assert_eq!(db.get(b"neighbor")?, Some(b"unchanged".to_vec()));
    Ok(())
}

#[test]
fn each_block_reaches_the_backend_once_including_empty_blocks() -> Result<(), BackendError> {
    let (mut db, probe) = database();
    let mut batch = WriteBatch::new();
    batch
        .put(b"key", b"first")
        .delete(b"key")
        .put(b"key", b"last");
    db.write_batch(batch)?;

    db.write_batch(WriteBatch::new())?;
    assert_eq!(db.get(b"key")?, Some(b"last".to_vec()));

    let mut next_block = WriteBatch::new();
    next_block
        .delete(b"key")
        .put(b"next", b"created in block 3");
    db.write_batch(next_block)?;

    // Splitting or reordering here would weaken the backend's atomic boundary.
    assert_eq!(
        *probe.batches.borrow(),
        vec![
            vec![
                WriteOperation::Put {
                    key: b"key".to_vec(),
                    value: b"first".to_vec(),
                },
                WriteOperation::Delete {
                    key: b"key".to_vec(),
                },
                WriteOperation::Put {
                    key: b"key".to_vec(),
                    value: b"last".to_vec(),
                },
            ],
            vec![],
            vec![
                WriteOperation::Delete {
                    key: b"key".to_vec(),
                },
                WriteOperation::Put {
                    key: b"next".to_vec(),
                    value: b"created in block 3".to_vec(),
                },
            ]
        ]
    );
    assert_eq!(db.get(b"key")?, None);
    assert_eq!(db.get(b"next")?, Some(b"created in block 3".to_vec()));
    Ok(())
}
