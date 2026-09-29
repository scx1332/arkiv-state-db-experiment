use std::convert::Infallible;

use arkiv_db::{Database, Entry, KvStore, MemoryStore, WriteBatch};

#[test]
fn returned_buffers_do_not_alias_stored_data() -> Result<(), Infallible> {
    let mut db = Database::in_memory();
    db.put(b"key", b"value")?;

    let mut value = db.get(b"key")?.expect("the key exists");
    value[0] = b'!';
    value.clear();

    let mut entries = db.scan_prefix(b"key")?;
    entries[0].key[0] = b'!';
    entries[0].value[0] = b'!';
    entries[0].key.clear();
    entries[0].value.clear();

    assert_eq!(db.get(b"key")?, Some(b"value".to_vec()));
    assert_eq!(db.get(b"")?, None);
    assert_eq!(db.scan_prefix(b"")?, vec![entry(b"key", b"value")]);
    Ok(())
}

#[test]
fn prefix_scans_respect_binary_neighbors_without_exact_keys() -> Result<(), Infallible> {
    let mut db = Database::in_memory();
    let entries = [
        entry(b"\x12\xfe\xff", b"before"),
        entry(b"\x12\xff\0", b"zero child"),
        entry(b"\x12\xff\x80", b"high child"),
        entry(b"\x12\xff\xff", b"last child"),
        entry(b"\x13", b"after"),
        entry(b"\xff\xff\0", b"maximum prefix child"),
    ];
    let mut block = WriteBatch::new();
    for item in entries.into_iter().rev() {
        block.put(item.key, item.value);
    }
    db.write_batch(block)?;

    assert_eq!(db.get(b"\x12\xff")?, None);
    assert_eq!(
        db.scan_prefix(b"\x12\xff")?,
        vec![
            entry(b"\x12\xff\0", b"zero child"),
            entry(b"\x12\xff\x80", b"high child"),
            entry(b"\x12\xff\xff", b"last child"),
        ]
    );
    assert_eq!(db.get(b"\xff\xff")?, None);
    assert_eq!(
        db.scan_prefix(b"\xff\xff")?,
        vec![entry(b"\xff\xff\0", b"maximum prefix child")]
    );
    assert!(db.scan_prefix(b"\x12\xff\x01")?.is_empty());
    Ok(())
}

#[test]
fn block_packets_own_inputs_and_discarded_candidates_do_not_publish() -> Result<(), Infallible> {
    let mut db = Database::in_memory();
    db.put(b"existing", b"original")?;

    let mut key = b"new".to_vec();
    let mut value = b"staged".to_vec();
    let mut batch = WriteBatch::new();
    batch
        .put(b"new", b"intermediate")
        .delete(b"new")
        .put(key.as_slice(), value.as_slice());
    key.fill(b'!');
    value.fill(b'!');

    assert_eq!(db.get(b"new")?, None);
    db.write_batch(batch)?;
    assert_eq!(db.get(b"new")?, Some(b"staged".to_vec()));
    assert_eq!(db.get(&key)?, None);

    let mut abandoned = WriteBatch::new();
    abandoned
        .put(b"existing", b"replacement")
        .put(b"abandoned", b"unpublished")
        .delete(b"new");
    drop(abandoned);

    assert_eq!(
        db.scan_prefix(b"")?,
        vec![entry(b"existing", b"original"), entry(b"new", b"staged")]
    );
    Ok(())
}

#[test]
fn wrapping_preserves_backend_contents_and_databases_are_independent() -> Result<(), Infallible> {
    let mut store = MemoryStore::new();
    let mut initial = WriteBatch::new();
    initial
        .put(b"preloaded", b"preserved")
        .put(b"shared", b"first");
    store.write_batch(initial)?;

    let mut first = Database::from_store(store);
    assert_eq!(
        first.scan_prefix(b"")?,
        vec![
            entry(b"preloaded", b"preserved"),
            entry(b"shared", b"first"),
        ]
    );

    let mut second = Database::in_memory();
    assert!(second.scan_prefix(b"")?.is_empty());
    second.put(b"shared", b"second")?;
    assert_eq!(first.get(b"shared")?, Some(b"first".to_vec()));

    first.put(b"shared", b"updated")?;
    assert_eq!(second.get(b"shared")?, Some(b"second".to_vec()));
    second.delete(b"shared")?;
    assert!(second.scan_prefix(b"")?.is_empty());
    assert_eq!(
        first.scan_prefix(b"")?,
        vec![
            entry(b"preloaded", b"preserved"),
            entry(b"shared", b"updated"),
        ]
    );
    Ok(())
}

fn entry(key: &[u8], value: &[u8]) -> Entry {
    Entry {
        key: key.to_vec(),
        value: value.to_vec(),
    }
}
