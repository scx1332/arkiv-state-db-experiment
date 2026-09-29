use std::convert::Infallible;

use arkiv_db::{Database, WriteBatch};

fn main() -> Result<(), Infallible> {
    let mut db = Database::in_memory();

    db.put(b"greeting", b"hello")?;
    println!("greeting: {:?}", db.get(b"greeting")?);

    let mut batch = WriteBatch::new();
    batch
        .put(b"notes/second", b"batches preserve write order")
        .put(b"notes/first", b"scans use bytewise key order")
        .delete(b"greeting");
    db.write_batch(batch)?;

    for entry in db.scan_prefix(b"notes/")? {
        println!(
            "{} = {}",
            String::from_utf8_lossy(&entry.key),
            String::from_utf8_lossy(&entry.value),
        );
    }

    println!("greeting after delete: {:?}", db.get(b"greeting")?);
    Ok(())
}
