use std::convert::Infallible;

use arkiv_db::{Database, Entry, WriteBatch};

#[test]
fn consecutive_block_packets_preserve_operation_order() -> Result<(), Infallible> {
    let mut db = Database::in_memory();
    assert_eq!(db.get(b"missing")?, None);

    let mut block_1 = WriteBatch::new();
    block_1
        .put(b"key\0", b"first\0value")
        .put(b"removed", b"created in block 1");
    db.write_batch(block_1)?;
    let original = db.get(b"key\0")?;
    assert_eq!(original, Some(b"first\0value".to_vec()));

    let mut block_2 = WriteBatch::new();
    block_2.put(b"key\0", b"replacement").delete(b"missing");
    db.write_batch(block_2)?;
    assert_eq!(db.get(b"key\0")?, Some(b"replacement".to_vec()));
    assert_eq!(original, Some(b"first\0value".to_vec()));

    let mut block_3 = WriteBatch::new();
    block_3
        .put(b"key\0", b"temporary")
        .delete(b"key\0")
        .put(b"key\0", b"last write wins")
        .put(b"removed", b"temporary")
        .delete(b"removed")
        .delete(b"missing")
        .put(b"", b"");

    // Building the next block does not expose any intermediate operation.
    assert_eq!(db.get(b"key\0")?, Some(b"replacement".to_vec()));
    assert_eq!(db.get(b"")?, None);

    db.write_batch(block_3)?;
    assert_eq!(db.get(b"key\0")?, Some(b"last write wins".to_vec()));
    assert_eq!(db.get(b"removed")?, None);
    assert_eq!(db.get(b"missing")?, None);
    assert_eq!(db.get(b"")?, Some(Vec::new()));

    let before_empty_block = db.scan_prefix(b"")?;
    // Block 4 is empty, but still crosses the batch submission boundary.
    db.write_batch(WriteBatch::new())?;
    assert_eq!(db.scan_prefix(b"")?, before_empty_block);

    let mut block_5 = WriteBatch::new();
    block_5.delete(b"key\0").delete(b"key\0").delete(b"");
    db.write_batch(block_5)?;
    assert_eq!(db.get(b"key\0")?, None);
    assert_eq!(db.get(b"")?, None);
    assert!(db.scan_prefix(b"")?.is_empty());
    Ok(())
}

#[test]
fn prefix_scans_are_byte_ordered_and_return_owned_results() -> Result<(), Infallible> {
    let mut db = Database::in_memory();
    assert!(db.scan_prefix(b"")?.is_empty());

    // Expected unsigned byte order, including exact prefixes and non-UTF-8 keys.
    let expected = vec![
        entry(b"", b"empty key"),
        entry(b"a", b"exact prefix"),
        entry(b"a\0", b"zero byte"),
        entry(b"a/a", b"first child"),
        entry(b"a/b", b"second child"),
        entry(b"a\xff", b"high byte"),
        entry(b"b", b"outside prefix"),
        entry(b"\xff", b"high prefix"),
        entry(b"\xff\0", b"high prefix with zero"),
        entry(b"\xff\xff", b"all high bytes"),
    ];

    // The insertion order is deliberately different from the expected order.
    let mut block_1 = WriteBatch::new();
    for item in expected.iter().rev() {
        block_1.put(item.key.clone(), item.value.clone());
    }
    db.write_batch(block_1)?;

    assert_eq!(db.scan_prefix(b"")?, expected);
    let a_entries = db.scan_prefix(b"a")?;
    assert_eq!(a_entries, expected[1..6]);
    assert_eq!(db.scan_prefix(b"a/")?, expected[3..5]);
    assert_eq!(db.scan_prefix(b"\xff")?, expected[7..10]);
    assert_eq!(db.scan_prefix(b"\xff\xff")?, expected[9..10]);
    assert!(db.scan_prefix(b"absent")?.is_empty());
    assert!(db.scan_prefix(b"\xff\xff\0")?.is_empty());

    let mut block_2 = WriteBatch::new();
    block_2
        .put(b"a/a", b"updated")
        .delete(b"a\0")
        .put(b"a/c", b"new child");
    assert_eq!(db.scan_prefix(b"a")?, a_entries);
    db.write_batch(block_2)?;

    // An earlier owned result is stable while a fresh scan reflects changes.
    assert_eq!(a_entries, expected[1..6]);
    assert_eq!(
        db.scan_prefix(b"a")?,
        vec![
            entry(b"a", b"exact prefix"),
            entry(b"a/a", b"updated"),
            entry(b"a/b", b"second child"),
            entry(b"a/c", b"new child"),
            entry(b"a\xff", b"high byte"),
        ]
    );
    Ok(())
}

#[test]
fn block_replacements_preserve_value_sizes_and_neighboring_keys() -> Result<(), Infallible> {
    let mut db = Database::in_memory();
    let mut initial_block = WriteBatch::new();
    initial_block
        .put(b"item\0", b"untouched neighbor")
        .put(b"item", b"initial");
    db.write_batch(initial_block)?;

    // Include same-size replacement, growth, shrinkage, empty, and regrowth.
    for (generation, size) in [31, 31, 4097, 3, 0, 8193, 1].into_iter().enumerate() {
        let value = vec![generation as u8; size];
        let before_block = db.scan_prefix(b"item")?;
        let mut block = WriteBatch::new();
        block
            .put(b"item", b"intermediate value")
            .delete(b"item")
            .put(b"item", value.clone());
        assert_eq!(db.scan_prefix(b"item")?, before_block);
        db.write_batch(block)?;
        assert_eq!(db.get(b"item")?, Some(value.clone()), "size {size}");
        assert_eq!(
            db.scan_prefix(b"item")?,
            vec![
                entry(b"item", &value),
                entry(b"item\0", b"untouched neighbor")
            ],
            "replacement generation {generation}, size {size}",
        );
    }

    let mut final_block = WriteBatch::new();
    final_block.put(b"item", b"temporary").delete(b"item");
    db.write_batch(final_block)?;
    assert_eq!(
        db.scan_prefix(b"item")?,
        vec![entry(b"item\0", b"untouched neighbor")]
    );
    Ok(())
}

fn entry(key: &[u8], value: &[u8]) -> Entry {
    Entry {
        key: key.to_vec(),
        value: value.to_vec(),
    }
}
