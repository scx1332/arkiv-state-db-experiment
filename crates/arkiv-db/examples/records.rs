//! The milestone-2 flow: typed records with attributes and fields written in
//! metered blocks, queries over the attribute indexes (equality, range,
//! prefix, inequality and negation, AND, OR and projection), a patch that
//! moves a record between type indexes, a keyset walk pinned to one snapshot
//! while a later block publishes, a historical query through the host's block
//! table, and host-driven expiry in the block where the records expire.
//!
//! Every cost asserted below is a literal derived by hand, in the comment
//! beside it, from the FIXTURE weights (DirectoryLookup 1, PointRead 3,
//! ReadByte 1, RecordCreate 20, RecordPatch 10, RecordDelete 20, CellWrite 4,
//! CellByte 1, CellRemove 4, IndexInsert 6, IndexTermCreate 30, IndexRemove 6,
//! QueryGroup 5, TermSeek 4, TermVisit 3, PostingEntry 1, RowVisit 1,
//! RecordProbe 2, ResultRow 3, NamespaceCreate 50, RootUpdate 2, RowByte 1)
//! and the model-version-3 charge points. Nothing here asks the engine or
//! the schedule what an operation should cost.
//!
//! What the example establishes: the flow runs end to end and every receipt,
//! result set and rejection is the one the charge rules predict. What it does
//! not establish: durability, authenticated roots, or anything about the
//! non-canonical physical node counts.

use std::ops::Bound::{Included, Unbounded};

use arkiv_db::{
    BlockMap, BlockNumber, Budget, Cell, CellName, Cost, Effect, Engine, Literal, OwnerId,
    PriceSchedule, Projection, Query, QueryPage, Receipt, RecordId, RecordKey, Rejection, Row,
    Term, TypeTag, Value, WriteOp, expire_records,
};

const ALICE: OwnerId = OwnerId::new([0xa1; 20]);

/// Every block here fits comfortably; the largest (block 2) spends 1174.
const BLOCK_BUDGET: Cost = Cost::new(2_000);
/// Per-operation budgets: writes are capped, reads run to completion.
const WRITE: Budget = Budget::limit(500);
const READ: Budget = Budget::unlimited();

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn name(text: &str) -> CellName {
    CellName::new(text).expect("a valid cell name")
}

fn key(text: &str) -> RecordKey {
    RecordKey::new(text.as_bytes()).expect("a valid key")
}

fn rec(ordinal: u64) -> RecordId {
    RecordId::from_ordinal(ordinal).expect("ordinals start at 1")
}

/// Prints a receipt and checks its hand-derived cost.
fn check(label: &str, receipt: &Receipt, expected: u64) {
    println!("{label}: {} ({:?})", receipt.spent, receipt.outcome);
    assert_eq!(receipt.spent, Cost::new(expected), "{label}");
}

fn ids(page: &QueryPage) -> Vec<u64> {
    page.rows.iter().map(|row| row.id.ordinal()).collect()
}

fn show_value(value: &Value) -> String {
    match value {
        Value::Bool(flag) => flag.to_string(),
        Value::I64(number) => format!("{number}i64"),
        Value::U64(number) => format!("{number}u64"),
        Value::Str(text) => format!("{text:?}"),
        Value::Bytes(bytes) => format!("{} bytes", bytes.len()),
        Value::I32(n) => n.to_string(),
        value => format!("{value:?}"),
    }
}

/// `rec:n key [name=value ...]`; attributes use `=`, fields use `~`.
fn describe(row: &Row) -> String {
    let key = row.key.as_ref().map_or_else(
        || "-".to_owned(),
        |key| String::from_utf8_lossy(key.as_bytes()).into_owned(),
    );
    let cells: Vec<String> = row
        .cells
        .iter()
        .map(|(name, cell)| {
            let sign = if cell.is_attribute() { '=' } else { '~' };
            format!("{name}{sign}{}", show_value(&cell.value))
        })
        .collect();
    format!("{} {key} [{}]", row.id, cells.join(" "))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut engine = Engine::new(PriceSchedule::FIXTURE);
    let mut blocks = BlockMap::new();
    blocks.record(BlockNumber(0), engine.head())?;

    // Cell names; their byte lengths enter the CellByte and ReadByte charges:
    // kind 4, price 5, $expiresAt 10, note 4. `$expiresAt` is an ordinary
    // U64 attribute to the engine; only the host gives it a meaning.
    let kind = name("kind");
    let price = name("price");
    let expires_at = name("$expiresAt");
    let note = name("note");

    // Block 1: alice creates the shop. DirectoryLookup 1 + {NamespaceCreate
    // 50 + RootUpdate 1 × 2 (the new catalog leaf)} = 53.
    let mut branch = engine.begin(engine.head(), Some(BLOCK_BUDGET))?;
    let created = branch.execute(
        ALICE,
        WriteOp::CreateNamespace {
            name: arkiv_db::NamespaceName::new("shop")?,
        },
        WRITE,
    )?;
    let Effect::NamespaceCreated(shop) = created.effect else {
        unreachable!()
    };
    check("block 1: create namespace", &created.receipt, 53);
    let sealed = branch.seal();
    blocks.record(BlockNumber(1), engine.publish(&sealed)?)?;

    // Block 2: four records, each with three attributes (kind: Str,
    // price: I64, $expiresAt: U64) and one field (note: Bytes, 3 bytes).
    //
    // Create = DirectoryLookup 1 + PointRead 3 + {RecordCreate 20 + CellWrite
    // 4c + CellByte b + RowByte n + RootUpdate 3 × 2 (key binding, row,
    // catalog leaf)} + {IndexInsert 6 + RootUpdate 2 × 2 (posting entry, term
    // leaf)} per term (two per attribute: value and existence) +
    // IndexTermCreate 30 per term nobody holds yet, with c = 4, a = 3,
    // b = Σ(name bytes + encoded value bytes) and n = key 5 + b:
    // b = kind (4 + len) + price (5 + 8) + $expiresAt (10 + 8) + note (4 + 3).
    // Without new terms a record costs 1 + 3 + (20 + 16 + b + 5 + b + 6) +
    // 6 × 10 = 111 + 2b.
    let catalog: [(&str, &str, i64, u64, u64); 4] = [
        // key, kind, price, $expiresAt, expected cost
        ("sku/1", "book", 10, 5, 383), // b = 8+13+18+7 = 46; all 6 terms new: 203 + 180
        ("sku/2", "book", 20, 5, 233), // b = 46; only the value term price=20 is new
        ("sku/3", "pen", 5, 5, 261),   // b = 45; kind=pen and price=5 are new: 201 + 60
        ("sku/4", "pencil", 7, 9, 297), // b = 48; kind=pencil, price=7, $expiresAt=9 new
    ];
    let mut branch = engine.begin(engine.head(), Some(BLOCK_BUDGET))?;
    for (index, &(sku, kind_of, price_of, expiry, expected)) in catalog.iter().enumerate() {
        let create = WriteOp::CreateRecord {
            namespace: shop,
            key: key(sku),
            cells: vec![
                (kind.clone(), Cell::attribute(Value::str(kind_of))),
                (price.clone(), Cell::attribute(Value::I64(price_of))),
                (expires_at.clone(), Cell::attribute(Value::U64(expiry))),
                (note.clone(), Cell::field(Value::bytes(b"xyz"))),
            ],
        };
        let executed = branch.execute(ALICE, create, WRITE)?;
        let expected_id = rec(index as u64 + 1);
        assert_eq!(executed.effect, Effect::RecordCreated { id: expected_id });
        check(
            &format!("block 2: create {sku} as {expected_id}"),
            &executed.receipt,
            expected,
        );
    }
    let sealed = branch.seal();
    // 383 + 233 + 261 + 297.
    println!("block 2: aggregate write cost {}", sealed.spent());
    assert_eq!(sealed.spent(), Cost::new(1174));
    blocks.record(BlockNumber(2), engine.publish(&sealed)?)?;
    let after_block_2 = engine.head_snapshot();
    assert_eq!(after_block_2.typed_record_count(shop), Some(4));

    // -----------------------------------------------------------------------
    // Queries against the state after block 2. Every query pays
    // DirectoryLookup 1, then QueryGroup 5 per group, then whatever the
    // group's first literal needs to position itself, then per candidate
    // revealed and per row emitted.
    // -----------------------------------------------------------------------

    // Eq seeks the value term kind="book" (TermSeek 4) and reads its posting
    // {1, 2} entry by entry (PostingEntry 2). Each emitted row pays ResultRow 3
    // + ReadByte p with p = key 5 + kind 8 + price 13 + $expiresAt 18 + note 7
    // = 51 under Projection::ALL. Total 1 + 5 + 4 + 2 + 2 × 54 = 120.
    let books = Query::term(Term::eq(kind.clone(), Value::str("book")));
    let page = after_block_2.query(shop, &books, &Projection::ALL, 10, None, READ)?;
    check("query kind == \"book\", all cells", &page.receipt, 120);
    assert_eq!(ids(&page.value), [1, 2]);
    assert!(page.value.next.is_none(), "no further match, so no cursor");
    for row in &page.value.rows {
        println!("  {}", describe(row));
    }

    // Range visits every term key of (price, I64) inside the bounds in key
    // order: 5, 7, 10 (TermVisit 3 = 9); 20 ends the run and is free. The run
    // yields ids 1, 3, 4 (PostingEntry 3); ID_ONLY projects nothing, so each
    // row is ResultRow 3. Total 1 + 5 + 4 + 9 + 3 + 9 = 31.
    let cheap = Query::term(Term::range(
        price.clone(),
        Included(Value::I64(5)),
        Included(Value::I64(10)),
    ));
    let page = after_block_2.query(shop, &cheap, &Projection::ID_ONLY, 10, None, READ)?;
    check("query 5 <= price <= 10, ids only", &page.receipt, 31);
    assert_eq!(ids(&page.value), [1, 3, 4]);

    // StartsWith visits the term keys "pen" and "pencil" (TermVisit 2 = 6),
    // yields ids 3, 4 (PostingEntry 2) and projects the 5-byte key of each:
    // 1 + 5 + 4 + 6 + 2 + 2 × (3 + 5) = 34.
    let pens = Query::term(Term::starts_with(kind.clone(), "pen"));
    let page = after_block_2.query(shop, &pens, &Projection::KEY_ONLY, 10, None, READ)?;
    check("query kind starts with \"pen\", keys", &page.receipt, 34);
    assert_eq!(ids(&page.value), [3, 4]);
    for row in &page.value.rows {
        println!("  {}", describe(row));
    }

    // NotEq seeks the existence term of (kind, Str) instead, whose posting
    // holds all four records, and probes each candidate against the literal
    // (PostingEntry 4, RecordProbe 4 = 8); records 3 and 4 pass.
    // 1 + 5 + 4 + 4 + 8 + 2 × 3 = 28.
    let not_books = Query::term(Term::not_eq(kind.clone(), Value::str("book")));
    let page = after_block_2.query(shop, &not_books, &Projection::ID_ONLY, 10, None, READ)?;
    check("query kind != \"book\", ids only", &page.receipt, 28);
    assert_eq!(ids(&page.value), [3, 4]);

    // A negated first literal has no index to seek: it walks the row map
    // (RowVisit 4, no TermSeek) and probes each row (RecordProbe 4 = 8);
    // records 1 and 2 pass. 1 + 5 + 4 + 8 + 2 × 3 = 24.
    let not_pens = Query::all(vec![Literal::not(Term::starts_with(kind.clone(), "pen"))]);
    let page = after_block_2.query(shop, &not_pens, &Projection::ID_ONLY, 10, None, READ)?;
    check("query NOT kind starts with \"pen\"", &page.receipt, 24);
    assert_eq!(ids(&page.value), [1, 2]);

    // AND: the first literal drives (kind="book", TermSeek 4, posting {1, 2},
    // PostingEntry 2); the second is a probe on each candidate (RecordProbe 2
    // = 4): price 10 fails, price 20 passes. The one row projects the key and
    // the price cell only: p = 5 + (5 + 8) = 18. 1 + 5 + 4 + 2 + 4 + 3 + 18 = 37.
    let dear_books = Query::all(vec![
        Literal::of(Term::eq(kind.clone(), Value::str("book"))),
        Literal::of(Term::range(
            price.clone(),
            Included(Value::I64(15)),
            Unbounded,
        )),
    ]);
    let key_and_price = Projection::named(true, [price.clone()]);
    let page = after_block_2.query(shop, &dear_books, &key_and_price, 10, None, READ)?;
    check(
        "query kind == \"book\" AND price >= 15, key + price",
        &page.receipt,
        37,
    );
    assert_eq!(ids(&page.value), [2]);
    assert_eq!(page.value.rows[0].key, Some(key("sku/2")));
    assert_eq!(
        page.value.rows[0].cells,
        vec![(price.clone(), Cell::attribute(Value::I64(20)))]
    );
    println!("  {}", describe(&page.value.rows[0]));

    // OR: two groups, each opened (QueryGroup 5 + TermSeek 4) and each
    // reading its single posting entry (PostingEntry 2 in total); the merge
    // emits the smaller head first. 1 + 2 × 9 + 2 + 2 × 3 = 27.
    let pens_or_twenty = Query {
        groups: vec![
            vec![Literal::of(Term::eq(kind.clone(), Value::str("pen")))],
            vec![Literal::of(Term::eq(price.clone(), Value::I64(20)))],
        ],
    };
    let page = after_block_2.query(shop, &pens_or_twenty, &Projection::ID_ONLY, 10, None, READ)?;
    check("query kind == \"pen\" OR price == 20", &page.receipt, 27);
    assert_eq!(ids(&page.value), [2, 3]);

    // count runs the same enumeration without emitting rows: the existence
    // term of ($expiresAt, U64) lists all four. 1 + 5 + 4 + 4 = 14.
    let has_expiry = Query::term(Term::exists(expires_at.clone(), TypeTag::U64));
    let counted = after_block_2.count(shop, &has_expiry, READ)?;
    check("count records with a u64 $expiresAt", &counted.receipt, 14);
    assert_eq!(counted.value, 4);

    // -----------------------------------------------------------------------
    // Block 3: a patch changes the type of sku/1's price from I64 to U64 and
    // drops its note. The record leaves the (price, I64) index, whose value
    // term 10 is now empty and dropped, and joins a brand new (price, U64)
    // index: both of its terms are created.
    //
    // DirectoryLookup 1 + PointRead 3 + {RecordPatch 10 + CellWrite 4 +
    // CellByte (5 + 8) + CellRemove 4 + RowByte 44 (the row left behind: key
    // 5 + $expiresAt 18 + kind 8 + price 13) + RootUpdate 2 × 2} = 79; then
    // per name in sorted order: note (field, nothing indexed) 0; price: two
    // removals {IndexRemove 6 + RootUpdate 4} (old value and existence
    // terms) = 20, two inserts {IndexInsert 6 + IndexTermCreate 30 +
    // RootUpdate 4} (new value and existence terms) = 80. Total 1 + 3 + 79 +
    // 20 + 80 = 183.
    // -----------------------------------------------------------------------
    let mut branch = engine.begin(engine.head(), Some(BLOCK_BUDGET))?;
    let patch = WriteOp::PatchRecord {
        namespace: shop,
        key: key("sku/1"),
        set: vec![(price.clone(), Cell::attribute(Value::U64(10)))],
        remove: vec![note.clone()],
    };
    let patched = branch.execute(ALICE, patch, WRITE)?;
    assert_eq!(patched.effect, Effect::RecordPatched { id: rec(1) });
    check(
        "block 3: patch sku/1 price := 10u64, remove note",
        &patched.receipt,
        183,
    );

    // Still inside the branch: the I64 range now visits only 5 and 7 (20
    // terminates; 10 is gone) and yields ids 3, 4: 1 + 5 + 4 + 6 + 2 + 6 = 24.
    let page = branch.query(shop, &cheap, &Projection::ID_ONLY, 10, None, READ)?;
    check(
        "block 3: 5 <= price <= 10 after the patch",
        &page.receipt,
        24,
    );
    assert_eq!(ids(&page.value), [3, 4]);
    // The U64 existence term holds exactly the patched record: 1+5+4+1+3 = 14.
    let u64_prices = Query::term(Term::exists(price.clone(), TypeTag::U64));
    let page = branch.query(shop, &u64_prices, &Projection::ID_ONLY, 10, None, READ)?;
    check("block 3: records with a u64 price", &page.receipt, 14);
    assert_eq!(ids(&page.value), [1]);
    let sealed = branch.seal();
    blocks.record(BlockNumber(3), engine.publish(&sealed)?)?;

    // -----------------------------------------------------------------------
    // Keyset walk pinned to block 3 while block 4 publishes.
    // -----------------------------------------------------------------------
    let pinned = engine.head_snapshot();
    let everything = Query::match_all();

    // Page 1, limit 2: the match-all group walks the row map. RowVisit 3: ids
    // 1 and 2 fill the page and id 3 is revealed as the head beyond it, which
    // is what makes `next` an exact claim. Each row projects its 5-byte key.
    // 1 + 5 + 3 + 2 × (3 + 5) = 25.
    let page_1 = pinned.query(shop, &everything, &Projection::KEY_ONLY, 2, None, READ)?;
    check("walk page 1 at block 3", &page_1.receipt, 25);
    assert_eq!(ids(&page_1.value), [1, 2]);
    let cursor = page_1.value.next.clone().expect("records 3 and 4 follow");
    assert_eq!(cursor.after(), rec(2));
    assert_eq!(cursor.snapshot(), Some(pinned.id()));
    println!(
        "  cursor after {} pinned to {}",
        cursor.after(),
        pinned.id()
    );

    // Block 4 publishes a fifth record in the meantime: three attributes, no
    // field. b = 8 + 13 + 18 = 39, n = 5 + 39 = 44; price=12 and
    // $expiresAt=7 are new terms. 1 + 3 + (20 + 12 + 39 + 44 + 6) + 6 × 10 +
    // 2 × 30 = 245.
    let mut branch = engine.begin(engine.head(), Some(BLOCK_BUDGET))?;
    let create = WriteOp::CreateRecord {
        namespace: shop,
        key: key("sku/5"),
        cells: vec![
            (kind.clone(), Cell::attribute(Value::str("book"))),
            (price.clone(), Cell::attribute(Value::I64(12))),
            (expires_at.clone(), Cell::attribute(Value::U64(7))),
        ],
    };
    let executed = branch.execute(ALICE, create, WRITE)?;
    assert_eq!(executed.effect, Effect::RecordCreated { id: rec(5) });
    check("block 4: create sku/5 as rec:5", &executed.receipt, 245);
    let sealed = branch.seal();
    blocks.record(BlockNumber(4), engine.publish(&sealed)?)?;

    // Page 2 continues from the pinned snapshot with a different limit (the
    // limit is not bound by the cursor). The row map resumes after id 2:
    // RowVisit 2 for ids 3 and 4, then the map ends, so rec:5 is not there
    // and `next` is None. 1 + 5 + 2 + 2 × 8 = 24.
    let page_2 = pinned.query(
        shop,
        &everything,
        &Projection::KEY_ONLY,
        10,
        Some(&cursor),
        READ,
    )?;
    check(
        "walk page 2 at block 3 (after block 4 published)",
        &page_2.receipt,
        24,
    );
    assert_eq!(ids(&page_2.value), [3, 4]);
    assert!(page_2.value.next.is_none());

    // The same cursor presented to the new head is refused before any charge.
    let head = engine.head_snapshot();
    let mismatch = head
        .query(
            shop,
            &everything,
            &Projection::KEY_ONLY,
            10,
            Some(&cursor),
            READ,
        )
        .expect_err("the cursor is bound to the block-3 snapshot");
    println!("walk at head with the old cursor: {mismatch}");
    assert_eq!(mismatch.reason, Rejection::CursorMismatch);
    assert_eq!(mismatch.receipt.spent, Cost::ZERO);

    // A fresh count at head sees all five rows: 1 + 5 + RowVisit 5 = 11.
    let counted = head.count(shop, &everything, READ)?;
    check("count everything at head", &counted.receipt, 11);
    assert_eq!(counted.value, 5);

    // -----------------------------------------------------------------------
    // History: the block table pins the state after block 2, where sku/1
    // still had an I64 price.
    // -----------------------------------------------------------------------
    let at_block_2 = blocks.at(&engine, BlockNumber(2))?;
    // Same run as before the patch: 5, 7, 10 visited; ids 1, 3, 4. Total 31.
    let page = at_block_2.query(shop, &cheap, &Projection::ID_ONLY, 10, None, READ)?;
    check("history: 5 <= price <= 10 at block 2", &page.receipt, 31);
    assert_eq!(ids(&page.value), [1, 3, 4]);
    // At head the run visits 5 and 7; 12 terminates it. Ids 3, 4. Total 24.
    let page = head.query(shop, &cheap, &Projection::ID_ONLY, 10, None, READ)?;
    check("history: 5 <= price <= 10 at head", &page.receipt, 24);
    assert_eq!(ids(&page.value), [3, 4]);

    // get_record: DirectoryLookup 1 + PointRead 3 + {ResultRow 3 + ReadByte p}.
    // At block 2 sku/1 has four cells: p = 5 + 8 + 13 + 18 + 7 = 51, total 58.
    // At head it has three: p = 5 + 8 + 13 + 18 = 44, total 51.
    let then = at_block_2.get_record(shop, &key("sku/1"), &Projection::ALL, READ)?;
    check("history: get sku/1 at block 2", &then.receipt, 58);
    let now = head.get_record(shop, &key("sku/1"), &Projection::ALL, READ)?;
    check("history: get sku/1 at head", &now.receipt, 51);
    let (then, now) = (
        then.value.expect("live at block 2"),
        now.value.expect("live at head"),
    );
    println!("  block 2: {}", describe(&then));
    println!("  head:    {}", describe(&now));
    // Cells come back sorted bytewise ($expiresAt, kind, [note,] price), so
    // the price cell is found by name rather than by position.
    let price_cell = |row: &Row| {
        row.cells
            .iter()
            .find(|(cell_name, _)| *cell_name == price)
            .map(|(_, cell)| cell.clone())
    };
    assert_eq!(price_cell(&then), Some(Cell::attribute(Value::I64(10))));
    assert_eq!(price_cell(&now), Some(Cell::attribute(Value::U64(10))));
    assert_eq!(then.cells.len(), 4);
    assert_eq!(now.cells.len(), 3);

    // -----------------------------------------------------------------------
    // Block 5: host expiry. The engine has no clock; the host deletes, with
    // host authority, every record whose $expiresAt is at most the block
    // number, in the block where it expires. Records 1, 2 and 3 carry 5;
    // records 4 (9) and 5 (7) stay.
    //
    // Delete = DirectoryLookup 1 + PointRead 3 + {RecordDelete 20 +
    // CellRemove 4c + RootUpdate 3 × 2} + {IndexRemove 6 + RootUpdate 2 × 2}
    // × 2a. rec:1 has c = 3, a = 3: 1 + 3 + 38 + 60 = 102; rec:2 and rec:3
    // have c = 4, a = 3: 1 + 3 + 42 + 60 = 106 each.
    // The expiry query is a read and never enters the block's receipts.
    // -----------------------------------------------------------------------
    let mut branch = engine.begin(engine.head(), Some(BLOCK_BUDGET))?;
    let expired = expire_records(&mut branch, shop, &expires_at, BlockNumber(5), READ, WRITE)?;
    for (id, receipt) in &expired {
        check(
            &format!("block 5: expire {id}"),
            receipt,
            if *id == rec(1) { 102 } else { 106 },
        );
    }
    let summary: Vec<(u64, u64)> = expired
        .iter()
        .map(|(id, receipt)| (id.ordinal(), receipt.spent.units()))
        .collect();
    assert_eq!(summary, [(1, 102), (2, 106), (3, 106)]);
    assert_eq!(
        branch.receipts().len(),
        3,
        "three deletes, no read receipts"
    );
    assert_eq!(branch.spent(), Cost::new(102 + 106 + 106));
    let sealed = branch.seal();
    blocks.record(BlockNumber(5), engine.publish(&sealed)?)?;

    // Snapshot B-1 still has the expired records; snapshot B does not.
    let before = blocks.at(&engine, BlockNumber(4))?;
    let after = blocks.at(&engine, BlockNumber(5))?;
    assert_eq!(before.typed_record_count(shop), Some(5));
    assert_eq!(after.typed_record_count(shop), Some(2));
    // count match-all: 1 + 5 + RowVisit n.
    let counted = before.count(shop, &everything, READ)?;
    check("block 4: count everything", &counted.receipt, 11);
    assert_eq!(counted.value, 5);
    let counted = after.count(shop, &everything, READ)?;
    check("block 5: count everything", &counted.receipt, 8);
    assert_eq!(counted.value, 2);
    // A miss costs DirectoryLookup 1 + PointRead 3 and returns None.
    let gone = after.get_record(shop, &key("sku/1"), &Projection::KEY_ONLY, READ)?;
    check("block 5: get sku/1", &gone.receipt, 4);
    assert!(gone.value.is_none());
    let survivors = after.query(shop, &everything, &Projection::ALL, 10, None, READ)?;
    assert_eq!(ids(&survivors.value), [4, 5]);
    for row in &survivors.value.rows {
        println!("  {}", describe(row));
    }

    println!(
        "blocks 0..=5 map to snapshots {:?}",
        (0..=5)
            .map(|block| blocks.resolve(BlockNumber(block)).unwrap().sequence())
            .collect::<Vec<_>>()
    );
    Ok(())
}
