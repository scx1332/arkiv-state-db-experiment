//! Snapshot-pinned keyset pagination and cursor binding.
//!
//! A page walk reads every page from one pinned handle with the cursor the
//! previous page issued. The cursor is a record id, never a count, so a page
//! is the first `limit` matches with a larger id. Every expectation about
//! rows comes from an independent reference: a `BTreeMap` model of the
//! records and a full-scan evaluator of the query semantics in the
//! milestone-2 brief. Every expected cost is a literal derived by hand from
//! the FIXTURE weights (DirectoryLookup 1, ReadByte 1, QueryGroup 5, TermSeek
//! 4, PostingEntry 1, RecordProbe 2, ResultRow 3) and the model-version-3
//! charge points (the read charge points are those of version 2); nothing
//! here asks the engine what a page should cost.
//!
//! What this file establishes:
//! - A full walk with limit 1, 3, 7 and `max_page` over a namespace of 64
//!   records (50 of them matching the primary query) is the reference set for
//!   eight query shapes (posting, run, row-domain, probed and multi-group
//!   sources), whether or not new snapshots are published between the pages:
//!   creates, deletes of already-paged and not-yet-paged rows, patches that
//!   stop rows matching, namespace transfers, a competing candidate that loses
//!   publication and empty blocks. The disturbed walk's pages (rows, cursors
//!   and receipts) are identical to the quiet walk's.
//! - `next` is `Some` iff a further match exists: an exactly-full last page
//!   has `next == None`; a continuation's `after()` is the last row's id; the
//!   limit is not bound, so pages of one walk may differ in size.
//! - Replaying a page on the same snapshot, through the same or another
//!   handle, gives identical rows and receipts.
//! - Hand-derived costs: a two-page `Eq` walk charges `limit + 1`
//!   `PostingEntry` on the non-final page and exactly the remaining entries on
//!   the final page; in an OR of two groups, a group whose head lies beyond
//!   the page end does no further work until that head is emitted.
//! - Cursor binding: a cursor presented to another snapshot (even one with
//!   identical contents), to a branch, to another engine at the same
//!   `SnapshotId`, with another namespace, a different query or a different
//!   projection is `CursorMismatch` with spent 0 and an empty ledger; the
//!   cursor check comes after query and projection validation.
//! - Branch cursors continue on the same untouched branch, across estimates,
//!   checkpoints and rejected writes, and are invalidated by an applied
//!   `execute`, an `execute_host`, a `rollback_to` (even one that discards
//!   nothing), a `set_pricing`, another branch and publication.
//! - The host-side pattern: a request that omits the block resolves
//!   `BlockMap::latest()` once and pages that snapshot while the head and the
//!   committed pricing advance; the total is a separate `count` read.
//!
//! What it does not establish: the costs of the walk over the 64-record
//! namespace (its pages are compared between runs, not derived), query
//! semantics beyond what the reference evaluator needs, budget-limited
//! pages, or anything about the physical trace.

use std::{collections::BTreeMap, ops::Bound};

use arkiv_db::{
    BlockMap, BlockNumber, Branch, Budget, Cell, CellName, CellSelect, Cost, Cursor, Effect,
    Engine, EngineError, Executed, Ledger, LedgerEntry, Literal, NamespaceId, OperationClass,
    Outcome, OwnerId, PriceSchedule, Projection, Query, QueryFault, QueryPage, ReadResult, Receipt,
    ReceiptKind, RecordId, RecordKey, Rejected, Rejection, Revision, Row, Snapshot, SnapshotId,
    Term, TypeTag, Value, WriteOp,
};

// ---------------------------------------------------------------------------
// Actors and fixture shape
// ---------------------------------------------------------------------------

const ALICE: OwnerId = OwnerId::new([0xa1; 20]);
const BOB: OwnerId = OwnerId::new([0xb0; 20]);

/// Counting-rule version recorded in every receipt.
const MODEL_VERSION: u32 = 3;
/// The id `PriceSchedule::FIXTURE` carries.
const FIXTURE_ID: u32 = 1;
/// `Limits::DEFAULT.max_page`.
const MAX_PAGE: usize = 200;
/// The page limits every full walk is run with.
const LIMITS: [usize; 4] = [1, 3, 7, MAX_PAGE];

/// The walk namespace holds records `r00`..`r63` (ids 1..=64). Record `i`
/// is a pen when `i % 9 == 4`, a lamp when `i % 9 == 8` and a book
/// otherwise: 7 pens, 7 lamps, 50 books.
const WALK_RECORDS: u64 = 64;
const BOOKS: usize = 50;

/// The cost namespace: six records with `kind: Str` and `price: I64`
/// attributes, created in this order (ids 1..=6). Books are the posting
/// `[1, 2, 4]`, lamps `[5, 6]`.
const COST_RECORDS: [(&str, &str, i64); 6] = [
    ("a", "book", 10),
    ("b", "book", 20),
    ("c", "pen", 5),
    ("d", "book", 30),
    ("e", "lamp", 7),
    ("f", "lamp", 8),
];

// ---------------------------------------------------------------------------
// Hand-computed read costs under PriceSchedule::FIXTURE (id 1). The charge
// table is transcribed from the brief; every total is a literal, and the
// `const _` lines re-add it from the table at compile time.
// ---------------------------------------------------------------------------

const DIRECTORY_LOOKUP: u64 = 1;
const READ_BYTE: u64 = 1;
const QUERY_GROUP: u64 = 5;
const TERM_SEEK: u64 = 4;
const POSTING_ENTRY: u64 = 1;
const RECORD_PROBE: u64 = 2;
const RESULT_ROW: u64 = 3;

/// `Eq(kind, book)` over the posting `[1, 2, 4]`, `ID_ONLY`, limit 2, no
/// cursor: 1 + 5 + 4, PostingEntry 3 (rows 1 and 2 plus the head 4 beyond
/// the page, which makes `next` exact), ResultRow 2 = 19.
const EQ_PAGE_ONE: u64 = 19;
const _: () = assert!(
    EQ_PAGE_ONE == DIRECTORY_LOOKUP + QUERY_GROUP + TERM_SEEK + 3 * POSTING_ENTRY + 2 * RESULT_ROW
);
/// Page 2 after 2: 1 + 5 + 4, PostingEntry 1 (exactly the remaining entry;
/// the exhausted posting is not charged), ResultRow 1 = 14.
const EQ_PAGE_TWO: u64 = 14;
const _: () =
    assert!(EQ_PAGE_TWO == DIRECTORY_LOOKUP + QUERY_GROUP + TERM_SEEK + POSTING_ENTRY + RESULT_ROW);
/// Limit 3 or more over the same posting: 1 + 5 + 4, PostingEntry 3,
/// ResultRow 3 = 22; the page is exactly full and `next` is `None`.
const EQ_FULL_PAGE: u64 = 22;
const _: () = assert!(
    EQ_FULL_PAGE == DIRECTORY_LOOKUP + QUERY_GROUP + TERM_SEEK + 3 * POSTING_ENTRY + 3 * RESULT_ROW
);
/// Limit 1 walk over the same posting: pages 1 and 2 charge PostingEntry 2
/// (the row and the head beyond it), ResultRow 1: 1 + 5 + 4 + 2 + 3 = 15.
const EQ_LIMIT_ONE_PAGE: u64 = 15;
const _: () = assert!(
    EQ_LIMIT_ONE_PAGE
        == DIRECTORY_LOOKUP + QUERY_GROUP + TERM_SEEK + 2 * POSTING_ENTRY + RESULT_ROW
);
/// A value term nobody holds: the seek only. 1 + 5 + 4 = 10.
const EQ_MISSING_TERM: u64 = 10;
const _: () = assert!(EQ_MISSING_TERM == DIRECTORY_LOOKUP + QUERY_GROUP + TERM_SEEK);
/// `Projection::ALL` of a cost record: key 1 + kind (4 + 4) + price (5 + 8)
/// = 22 bytes.
const COST_ROW_BYTES: u64 = 22;
/// Limit 2 with `Projection::ALL`: 19 + ReadByte 2 × 22 = 63.
const EQ_PAGE_ONE_ALL: u64 = 63;
const _: () = assert!(EQ_PAGE_ONE_ALL == EQ_PAGE_ONE + 2 * COST_ROW_BYTES * READ_BYTE);
/// `[Eq(kind, book)] OR [Eq(kind, lamp), Exists(price, I64)]`, `ID_ONLY`,
/// limit 2, no cursor. Group 1 (posting `[1, 2, 4]`) and group 2 (posting
/// `[5, 6]` probed by the existence literal): 1 + 2 × (5 + 4); heads:
/// PostingEntry 1 (id 1) and PostingEntry 1 + RecordProbe 1 (id 5); rows 1
/// and 2 (ResultRow 2) each advance group 1 (PostingEntry 2); group 2's head
/// 5 lies beyond the page end and is never advanced: 1 + 18 + 4 + 2 + 6 = 31.
const OR_PAGE_ONE: u64 = 31;
const _: () = assert!(
    OR_PAGE_ONE
        == DIRECTORY_LOOKUP
            + 2 * (QUERY_GROUP + TERM_SEEK)
            + 4 * POSTING_ENTRY
            + RECORD_PROBE
            + 2 * RESULT_ROW
);
/// Page 2 after 2: heads 4 (PostingEntry 1) and 5 (PostingEntry 1 +
/// RecordProbe 1); row 4 exhausts group 1 for free; row 5 advances group 2
/// to 6 (PostingEntry 1 + RecordProbe 1): 1 + 18 + 3 + 4 + 6 = 32.
const OR_PAGE_TWO: u64 = 32;
const _: () = assert!(
    OR_PAGE_TWO
        == DIRECTORY_LOOKUP
            + 2 * (QUERY_GROUP + TERM_SEEK)
            + 3 * POSTING_ENTRY
            + 2 * RECORD_PROBE
            + 2 * RESULT_ROW
);
/// Page 3 after 5: group 1's posting is empty past 5 (no entry charged);
/// group 2's head 6 (PostingEntry 1 + RecordProbe 1); row 6 exhausts it:
/// 1 + 18 + 1 + 2 + 3 = 25.
const OR_PAGE_THREE: u64 = 25;
const _: () = assert!(
    OR_PAGE_THREE
        == DIRECTORY_LOOKUP
            + 2 * (QUERY_GROUP + TERM_SEEK)
            + POSTING_ENTRY
            + RECORD_PROBE
            + RESULT_ROW
);
/// A branch whose working state adds a fourth book (id 7): page 2 after 2
/// charges PostingEntry 2 (4 and 7) and ResultRow 2: 1 + 5 + 4 + 2 + 6 = 18.
const EQ_BRANCH_PAGE_TWO: u64 = 18;
const _: () = assert!(
    EQ_BRANCH_PAGE_TWO
        == DIRECTORY_LOOKUP + QUERY_GROUP + TERM_SEEK + 2 * POSTING_ENTRY + 2 * RESULT_ROW
);
/// `count(Eq(kind, book))` over the 50 books: 1 + 5 + 4 + PostingEntry 50 =
/// 60 (a count emits no row).
const COUNT_BOOKS: u64 = 60;
const _: () =
    assert!(COUNT_BOOKS == DIRECTORY_LOOKUP + QUERY_GROUP + TERM_SEEK + 50 * POSTING_ENTRY);
/// The same count after seven books were deleted at head: 1 + 5 + 4 + 43 = 53.
const COUNT_BOOKS_AFTER_DELETES: u64 = 53;
const _: () = assert!(
    COUNT_BOOKS_AFTER_DELETES == DIRECTORY_LOOKUP + QUERY_GROUP + TERM_SEEK + 43 * POSTING_ENTRY
);

const CURSOR_MISMATCH: Outcome = Outcome::Rejected(Rejection::CursorMismatch);

// ---------------------------------------------------------------------------
// Value helpers
// ---------------------------------------------------------------------------

fn name(text: &str) -> CellName {
    CellName::new(text).expect("fixture cell names follow the grammar")
}

fn key(text: &str) -> RecordKey {
    RecordKey::new(text.as_bytes()).expect("fixture keys are 1..=32 bytes")
}

fn rid(ordinal: u64) -> RecordId {
    RecordId::from_ordinal(ordinal).expect("record ordinals start at 1")
}

fn attr(value: Value) -> Cell {
    Cell::attribute(value)
}

fn field(value: Value) -> Cell {
    Cell::field(value)
}

/// A row under `Projection::ID_ONLY`.
fn id_row(ordinal: u64) -> Row {
    Row {
        id: rid(ordinal),
        key: None,
        cells: Vec::new(),
    }
}

/// `Eq(kind, value)` as a one-literal query.
fn kind_is(value: &str) -> Query {
    Query::term(Term::eq(name("kind"), Value::str(value)))
}

/// The primary query of every walk.
fn books() -> Query {
    kind_is("book")
}

/// `[Eq(kind, book)] OR [Eq(kind, lamp), Exists(price, I64)]`.
fn books_or_priced_lamps() -> Query {
    Query {
        groups: vec![
            vec![Literal::of(Term::eq(name("kind"), Value::str("book")))],
            vec![
                Literal::of(Term::eq(name("kind"), Value::str("lamp"))),
                Literal::of(Term::exists(name("price"), TypeTag::I64)),
            ],
        ],
    }
}

/// The cells of walk record `i` (its key is `r{i:02}`, its id `i + 1`):
/// `kind: Str` (book, pen or lamp), `price` (an `I64` attribute in
/// `-20..=80`, or a `U64` attribute when `i % 7 == 6`, so ranges over `I64`
/// skip it), `stock: U64(i / 4)` and, for even `i`, a `note` field.
fn walk_cells(i: u64) -> Vec<(CellName, Cell)> {
    let kind = match i % 9 {
        4 => "pen",
        8 => "lamp",
        _ => "book",
    };
    let price = if i % 7 == 6 {
        Value::U64(i)
    } else {
        let scrambled = i64::try_from((i * 37) % 101).expect("fits");
        Value::I64(scrambled - 20)
    };
    let mut cells = vec![
        (name("kind"), attr(Value::str(kind))),
        (name("price"), attr(price)),
        (name("stock"), attr(Value::U64(i / 4))),
    ];
    if i.is_multiple_of(2) {
        cells.push((name("note"), field(Value::str(&format!("n{i}")))));
    }
    cells
}

fn cost_cells(kind: &str, price: i64) -> Vec<(CellName, Cell)> {
    vec![
        (name("kind"), attr(Value::str(kind))),
        (name("price"), attr(Value::I64(price))),
    ]
}

/// A cost record under `Projection::ALL`.
fn cost_row(ordinal: u64, key_text: &str, kind: &str, price: i64) -> Row {
    Row {
        id: rid(ordinal),
        key: Some(key(key_text)),
        cells: cost_cells(kind, price),
    }
}

/// The eight query shapes every walk is run with, over the walk namespace.
fn shapes() -> Vec<(&'static str, Query)> {
    let kind = || name("kind");
    let price = || name("price");
    let stock = || name("stock");
    vec![
        ("Eq(kind, book)", books()),
        ("match-all", Query::match_all()),
        (
            "NOT Eq(kind, pen)",
            Query::all(vec![Literal::not(Term::eq(kind(), Value::str("pen")))]),
        ),
        (
            "Range(price, [0, +inf))",
            Query::term(Term::range(
                price(),
                Bound::Included(Value::I64(0)),
                Bound::Unbounded,
            )),
        ),
        (
            "StartsWith(kind, \"\")",
            Query::term(Term::starts_with(kind(), "")),
        ),
        (
            "[Eq(kind, pen)] OR [Range(stock, [3, 9))]",
            Query {
                groups: vec![
                    vec![Literal::of(Term::eq(kind(), Value::str("pen")))],
                    vec![Literal::of(Term::range(
                        stock(),
                        Bound::Included(Value::U64(3)),
                        Bound::Excluded(Value::U64(9)),
                    ))],
                ],
            },
        ),
        (
            "Exists(price, I64) AND NotEq(kind, book) AND NOT Range(stock, (-inf, 2])",
            Query::all(vec![
                Literal::of(Term::exists(price(), TypeTag::I64)),
                Literal::of(Term::not_eq(kind(), Value::str("book"))),
                Literal::not(Term::range(
                    stock(),
                    Bound::Unbounded,
                    Bound::Included(Value::U64(2)),
                )),
            ]),
        ),
        (
            "NotEq(kind, book)",
            Query::term(Term::not_eq(kind(), Value::str("book"))),
        ),
    ]
}

// ---------------------------------------------------------------------------
// Receipt helpers
// ---------------------------------------------------------------------------

fn receipt(kind: ReceiptKind, schedule_id: u32, spent: u64, outcome: Outcome) -> Receipt {
    Receipt {
        kind,
        model_version: MODEL_VERSION,
        schedule_id,
        spent: Cost::new(spent),
        outcome,
        ledger: None,
    }
}

fn read_receipt(spent: u64, outcome: Outcome) -> Receipt {
    receipt(ReceiptKind::Read, FIXTURE_ID, spent, outcome)
}

fn read_applied(spent: u64) -> Receipt {
    read_receipt(spent, Outcome::Applied)
}

/// Ledger lines as `(class, count, unit_price, subtotal)`.
fn ledger(lines: &[(OperationClass, u64, u64, u64)]) -> Ledger {
    Ledger {
        entries: lines
            .iter()
            .map(|&(class, count, unit_price, subtotal)| LedgerEntry {
                class,
                count,
                unit_price,
                subtotal: Cost::new(subtotal),
            })
            .collect(),
    }
}

fn traced(receipt: Receipt, lines: &[(OperationClass, u64, u64, u64)]) -> Receipt {
    Receipt {
        ledger: Some(ledger(lines)),
        ..receipt
    }
}

/// Unwraps a rejected read.
fn rejection(result: ReadResult<QueryPage>, what: &str) -> Rejected {
    match result {
        Ok(page) => panic!(
            "{what}: expected a rejection, got a page of {} rows",
            page.value.rows.len()
        ),
        Err(rejected) => rejected,
    }
}

/// A rejection decided at admission: spent 0, a read receipt under the
/// reader's own schedule, and an empty ledger when one was requested.
fn assert_zero_cost_under(
    result: ReadResult<QueryPage>,
    reason: Rejection,
    schedule_id: u32,
    what: &str,
) {
    let mut rejected = rejection(result, what);
    assert_eq!(rejected.reason, reason, "{what}");
    let ledger = rejected.receipt.ledger.take();
    assert!(
        ledger
            .as_ref()
            .is_none_or(|ledger| ledger.entries.is_empty()),
        "{what}: nothing is charged, got {ledger:?}"
    );
    assert_eq!(
        rejected.receipt,
        receipt(ReceiptKind::Read, schedule_id, 0, Outcome::Rejected(reason)),
        "{what}"
    );
}

fn assert_zero_cost(result: ReadResult<QueryPage>, reason: Rejection, what: &str) {
    assert_zero_cost_under(result, reason, FIXTURE_ID, what);
}

fn assert_mismatch(result: ReadResult<QueryPage>, what: &str) {
    assert_zero_cost(result, Rejection::CursorMismatch, what);
}

// ---------------------------------------------------------------------------
// The reference: a BTreeMap model of one namespace's live records and a
// full-scan evaluator of the brief's query semantics.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Record {
    key: RecordKey,
    cells: BTreeMap<CellName, Cell>,
}

#[derive(Clone, Debug)]
struct Model {
    next: u64,
    live: BTreeMap<RecordId, Record>,
    keys: BTreeMap<RecordKey, RecordId>,
}

impl Model {
    fn new() -> Self {
        Self {
            next: 1,
            live: BTreeMap::new(),
            keys: BTreeMap::new(),
        }
    }

    /// Mints the next id and binds `key`; the key must not be live.
    fn create(&mut self, key: &RecordKey, cells: &[(CellName, Cell)]) -> RecordId {
        assert!(!self.keys.contains_key(key), "{key:?} is already live");
        let id = rid(self.next);
        self.next += 1;
        self.keys.insert(key.clone(), id);
        self.live.insert(
            id,
            Record {
                key: key.clone(),
                cells: cells.iter().cloned().collect(),
            },
        );
        id
    }

    fn delete(&mut self, key: &RecordKey) -> RecordId {
        let id = self.keys.remove(key).expect("deleting a live key");
        self.live.remove(&id);
        id
    }

    fn patch(&mut self, key: &RecordKey, set: &[(CellName, Cell)], remove: &[CellName]) {
        let id = self.keys[key];
        let cells = &mut self.live.get_mut(&id).expect("bound keys have rows").cells;
        for (name, cell) in set {
            cells.insert(name.clone(), cell.clone());
        }
        for name in remove {
            cells.remove(name);
        }
    }

    fn is_live(&self, id: RecordId) -> bool {
        self.live.contains_key(&id)
    }

    fn key_of(&self, id: RecordId) -> RecordKey {
        self.live[&id].key.clone()
    }

    fn len(&self) -> usize {
        self.live.len()
    }

    /// Every matching live record in ascending id order, projected.
    fn evaluate(&self, query: &Query, projection: &Projection) -> Vec<Row> {
        self.live
            .iter()
            .filter(|(_, record)| query_holds(query, &record.cells))
            .map(|(&id, record)| project(id, record, projection))
            .collect()
    }
}

fn project(id: RecordId, record: &Record, projection: &Projection) -> Row {
    let cells = match &projection.cells {
        CellSelect::None => Vec::new(),
        CellSelect::All => record
            .cells
            .iter()
            .map(|(name, cell)| (name.clone(), cell.clone()))
            .collect(),
        CellSelect::Named(names) => record
            .cells
            .iter()
            .filter(|(name, _)| names.contains(*name))
            .map(|(name, cell)| (name.clone(), cell.clone()))
            .collect(),
    };
    Row {
        id,
        key: projection.key.then(|| record.key.clone()),
        cells,
    }
}

/// Query = OR of groups; group = AND of literals; empty group = every live
/// record; negated literal = complement.
fn query_holds(query: &Query, cells: &BTreeMap<CellName, Cell>) -> bool {
    query.groups.iter().any(|group| {
        group
            .iter()
            .all(|literal| term_holds(&literal.term, cells) != literal.negated)
    })
}

/// A term holds only on an attribute cell of the term's name; fields and
/// cells of another type count as absent.
fn term_holds(term: &Term, cells: &BTreeMap<CellName, Cell>) -> bool {
    let Some(cell) = cells.get(term.name()) else {
        return false;
    };
    if !cell.is_attribute() {
        return false;
    }
    let held = &cell.value;
    match term {
        Term::Eq { value, .. } => held.tag() == value.tag() && held == value,
        Term::NotEq { value, .. } => held.tag() == value.tag() && held != value,
        Term::Range { lower, upper, .. } => within(held, lower, upper),
        Term::StartsWith { prefix, .. } => match held {
            Value::Str(text) => text.as_bytes().starts_with(prefix.as_bytes()),
            _ => false,
        },
        Term::Exists { tag, .. } => held.tag() == *tag,
    }
}

/// The type and the widened magnitude of an ordered value.
fn ordered(value: &Value) -> Option<(TypeTag, i128)> {
    match value {
        Value::I64(number) => Some((TypeTag::I64, i128::from(*number))),
        Value::U64(number) => Some((TypeTag::U64, i128::from(*number))),
        _ => None,
    }
}

fn within(held: &Value, lower: &Bound<Value>, upper: &Bound<Value>) -> bool {
    let Some((tag, held)) = ordered(held) else {
        return false;
    };
    let same_type = |bound: &Value| {
        ordered(bound)
            .filter(|(bound_tag, _)| *bound_tag == tag)
            .map(|(_, magnitude)| magnitude)
    };
    let above = match lower {
        Bound::Unbounded => true,
        Bound::Included(bound) => same_type(bound).is_some_and(|bound| held >= bound),
        Bound::Excluded(bound) => same_type(bound).is_some_and(|bound| held > bound),
    };
    let below = match upper {
        Bound::Unbounded => true,
        Bound::Included(bound) => same_type(bound).is_some_and(|bound| held <= bound),
        Bound::Excluded(bound) => same_type(bound).is_some_and(|bound| held < bound),
    };
    above && below
}

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

fn apply(branch: &mut Branch, actor: OwnerId, op: WriteOp) -> Executed {
    match branch.execute(actor, op, Budget::unlimited()) {
        Ok(executed) => executed,
        Err(rejected) => panic!("fixture write rejected: {rejected}"),
    }
}

fn create_namespace(branch: &mut Branch, actor: OwnerId) -> NamespaceId {
    match apply(
        branch,
        actor,
        WriteOp::CreateNamespace {
            name: arkiv_db::NamespaceName::new("test").unwrap(),
        },
    )
    .effect
    {
        Effect::NamespaceCreated(id) => id,
        other => panic!("creation reports the new id, got {other:?}"),
    }
}

/// Snapshot 1 of a fresh engine: the walk namespace (ordinal 1) with its 64
/// records, the cost namespace (ordinal 2) with its six, both owned by
/// ALICE, priced with FIXTURE. `model` mirrors the walk namespace.
struct Fixture {
    engine: Engine,
    walk_ns: NamespaceId,
    cost_ns: NamespaceId,
    model: Model,
}

fn fixture() -> Fixture {
    let mut engine = Engine::new(PriceSchedule::FIXTURE);
    assert_eq!(engine.limits().max_page, MAX_PAGE);
    let mut branch = engine
        .begin(engine.head(), None)
        .expect("genesis is the head");
    let walk_ns = create_namespace(&mut branch, ALICE);
    let cost_ns = create_namespace(&mut branch, ALICE);
    let mut model = Model::new();
    for i in 0..WALK_RECORDS {
        let key = key(&format!("r{i:02}"));
        let cells = walk_cells(i);
        let expected = model.create(&key, &cells);
        let executed = apply(
            &mut branch,
            ALICE,
            WriteOp::CreateRecord {
                namespace: walk_ns,
                key,
                cells,
            },
        );
        assert_eq!(executed.effect, Effect::RecordCreated { id: expected });
    }
    for ((key_text, kind, price), ordinal) in COST_RECORDS.iter().zip(1u64..) {
        let executed = apply(
            &mut branch,
            ALICE,
            WriteOp::CreateRecord {
                namespace: cost_ns,
                key: key(key_text),
                cells: cost_cells(kind, *price),
            },
        );
        assert_eq!(executed.effect, Effect::RecordCreated { id: rid(ordinal) });
    }
    let published = engine
        .publish(&branch.seal())
        .expect("the fixture publishes on genesis");
    assert_eq!(published.sequence(), 1, "the fixture is snapshot 1");
    assert_eq!(model.len(), 64);
    assert_eq!(
        model.evaluate(&books(), &Projection::ID_ONLY).len(),
        BOOKS,
        "50 of the 64 walk records are books"
    );
    Fixture {
        engine,
        walk_ns,
        cost_ns,
        model,
    }
}

// ---------------------------------------------------------------------------
// The page walk
// ---------------------------------------------------------------------------

/// One page as the engine returned it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Page {
    rows: Vec<Row>,
    next: Option<Cursor>,
    receipt: Receipt,
}

fn rows_of(pages: &[Page]) -> Vec<Row> {
    pages
        .iter()
        .flat_map(|page| page.rows.iter().cloned())
        .collect()
}

/// Pages `query` from `snapshot` to the end, `limit_of(page index)` rows per
/// page, calling `between(rows so far)` after every page that issued a
/// continuation. Checks the cursor invariants of every page: rows ascend and
/// exceed the cursor, a continuation is issued only for an exactly full page
/// and names the last row, and it is bound to this snapshot, namespace,
/// query and projection.
fn walk(
    snapshot: &Snapshot,
    namespace: NamespaceId,
    query: &Query,
    projection: &Projection,
    mut limit_of: impl FnMut(usize) -> usize,
    mut between: impl FnMut(&[Row]),
) -> Vec<Page> {
    let mut pages: Vec<Page> = Vec::new();
    let mut paged: Vec<Row> = Vec::new();
    let mut cursor: Option<Cursor> = None;
    loop {
        let index = pages.len();
        assert!(index < 1_000, "runaway walk: the engine keeps continuing");
        let limit = limit_of(index);
        let read = match snapshot.query(
            namespace,
            query,
            projection,
            limit,
            cursor.as_ref(),
            Budget::unlimited(),
        ) {
            Ok(read) => read,
            Err(rejected) => panic!("page {index} rejected: {rejected}"),
        };
        let QueryPage { rows, next } = read.value;
        assert!(rows.len() <= limit, "page {index} exceeds its limit");
        if index > 0 {
            assert!(
                !rows.is_empty(),
                "page {index}: a continuation leads to at least one more row"
            );
        }
        let mut floor = cursor.as_ref().map(Cursor::after);
        for row in &rows {
            assert!(
                floor.is_none_or(|floor| row.id > floor),
                "page {index}: rows ascend past the cursor"
            );
            floor = Some(row.id);
        }
        if let Some(continuation) = &next {
            assert_eq!(rows.len(), limit, "page {index}: a continued page is full");
            assert_eq!(
                continuation.after(),
                rows.last().expect("a full page has rows").id,
                "page {index}: after() is the last row"
            );
            assert_eq!(continuation.snapshot(), Some(snapshot.id()));
            assert_eq!(continuation.namespace(), namespace);
            assert_eq!(continuation.query(), query);
            assert_eq!(continuation.projection(), projection);
        }
        paged.extend(rows.iter().cloned());
        pages.push(Page {
            rows,
            next: next.clone(),
            receipt: read.receipt,
        });
        match next {
            Some(continuation) => {
                between(&paged);
                cursor = Some(continuation);
            }
            None => return pages,
        }
    }
}

// ---------------------------------------------------------------------------
// The advancing head during a disturbed walk
// ---------------------------------------------------------------------------

/// Publishes one disturbance per call and mirrors the published state in a
/// model of the head, so later disturbances pick rows that are still live
/// there. The pinned snapshot never sees any of it.
struct Head<'e> {
    engine: &'e mut Engine,
    ns: NamespaceId,
    model: Model,
    owner: OwnerId,
    revision: u64,
    step: usize,
    minted: u64,
    published: u64,
    stale_candidates: usize,
}

impl<'e> Head<'e> {
    fn new(engine: &'e mut Engine, ns: NamespaceId, model: Model) -> Self {
        Self {
            engine,
            ns,
            model,
            owner: ALICE,
            revision: 0,
            step: 0,
            minted: 0,
            published: 0,
            stale_candidates: 0,
        }
    }

    fn publish(&mut self, ops: Vec<WriteOp>) -> SnapshotId {
        let mut branch = self
            .engine
            .begin(self.engine.head(), None)
            .expect("begun at the current head");
        for op in ops {
            apply(&mut branch, self.owner, op);
        }
        let id = self
            .engine
            .publish(&branch.seal())
            .expect("nothing else publishes");
        self.published += 1;
        id
    }

    /// A new book whose id lies beyond every pinned id.
    fn fresh_book(&mut self) -> (RecordKey, Vec<(CellName, Cell)>) {
        self.minted += 1;
        (
            key(&format!("x{}", self.minted)),
            vec![
                (name("kind"), attr(Value::str("book"))),
                (name("price"), attr(Value::I64(1))),
                (name("stock"), attr(Value::U64(0))),
            ],
        )
    }

    /// The rows matching `query` at head beyond the paged prefix.
    fn unpaged(&self, query: &Query, paged: &[Row]) -> Vec<RecordKey> {
        let last = paged.last().map(|row| row.id);
        self.model
            .evaluate(query, &Projection::KEY_ONLY)
            .into_iter()
            .filter(|row| last.is_none_or(|last| row.id > last))
            .map(|row| row.key.expect("the key was projected"))
            .collect()
    }

    fn delete(&mut self, key: &RecordKey) {
        self.model.delete(key);
        self.publish(vec![WriteOp::DeleteRecord {
            namespace: self.ns,
            key: key.clone(),
        }]);
    }

    /// One disturbance, cycling through the seven kinds.
    fn disturb(&mut self, query: &Query, paged: &[Row]) {
        let step = self.step;
        self.step += 1;
        let unpaged = self.unpaged(query, paged);
        match step % 7 {
            // Delete an already-paged row: the latest one still live at head.
            0 => {
                let victim = paged
                    .iter()
                    .rev()
                    .map(|row| row.id)
                    .find(|&id| self.model.is_live(id));
                match victim {
                    Some(id) => self.delete(&self.model.key_of(id)),
                    None => {
                        self.publish(Vec::new());
                    }
                }
            }
            // Delete the next not-yet-paged match.
            1 => match unpaged.first() {
                Some(key) => self.delete(key),
                None => {
                    self.publish(Vec::new());
                }
            },
            // Patch the next not-yet-paged match so it stops matching: the
            // kind becomes pen, the price a field, the stock disappears.
            2 => match unpaged.first() {
                Some(key) => {
                    let set = vec![
                        (name("kind"), attr(Value::str("pen"))),
                        (name("price"), field(Value::I64(0))),
                    ];
                    let remove = vec![name("stock")];
                    self.model.patch(key, &set, &remove);
                    self.publish(vec![WriteOp::PatchRecord {
                        namespace: self.ns,
                        key: key.clone(),
                        set,
                        remove,
                    }]);
                }
                None => {
                    self.publish(Vec::new());
                }
            },
            // Create two matching records.
            3 => {
                let mut ops = Vec::new();
                for _ in 0..2 {
                    let (key, cells) = self.fresh_book();
                    self.model.create(&key, &cells);
                    ops.push(WriteOp::CreateRecord {
                        namespace: self.ns,
                        key,
                        cells,
                    });
                }
                self.publish(ops);
            }
            // Transfer the namespace; later writes come from the new owner.
            4 => {
                let new_owner = if self.owner == ALICE { BOB } else { ALICE };
                self.publish(vec![WriteOp::TransferNamespace {
                    namespace: self.ns,
                    expected_revision: Revision(self.revision),
                    new_owner,
                }]);
                self.owner = new_owner;
                self.revision += 1;
            }
            // A competing candidate deletes the next not-yet-paged match but
            // loses publication to a candidate that creates a record.
            5 => {
                let head = self.engine.head();
                let mut winner = self.engine.begin(head, None).expect("at head");
                let mut loser = self.engine.begin(head, None).expect("at head");
                if let Some(key) = unpaged.first() {
                    apply(
                        &mut loser,
                        self.owner,
                        WriteOp::DeleteRecord {
                            namespace: self.ns,
                            key: key.clone(),
                        },
                    );
                }
                let (key, cells) = self.fresh_book();
                let expected = self.model.create(&key, &cells);
                let executed = apply(
                    &mut winner,
                    self.owner,
                    WriteOp::CreateRecord {
                        namespace: self.ns,
                        key,
                        cells,
                    },
                );
                assert_eq!(executed.effect, Effect::RecordCreated { id: expected });
                self.engine
                    .publish(&winner.seal())
                    .expect("the winner publishes on head");
                self.published += 1;
                assert!(
                    matches!(
                        self.engine.publish(&loser.seal()),
                        Err(EngineError::StaleHead { .. })
                    ),
                    "the competing candidate is refused"
                );
                self.stale_candidates += 1;
            }
            // An empty block.
            _ => {
                self.publish(Vec::new());
            }
        }
    }

    fn finish(self) -> (Model, u64, usize) {
        (self.model, self.published, self.stale_candidates)
    }
}

// ---------------------------------------------------------------------------
// Walks against the reference
// ---------------------------------------------------------------------------

#[test]
fn pinned_walks_match_the_reference_whether_or_not_the_head_advances() {
    let Fixture {
        mut engine,
        walk_ns,
        model,
        ..
    } = fixture();
    let pinned = engine.head_snapshot();
    let mut head = Head::new(&mut engine, walk_ns, model.clone());
    let projection = Projection::ALL;
    let mut expected_disturbances = 0;

    for (label, query) in shapes() {
        let reference = model.evaluate(&query, &projection);
        assert!(!reference.is_empty(), "{label}: the shape has matches");
        for limit in LIMITS {
            let quiet = walk(&pinned, walk_ns, &query, &projection, |_| limit, |_| {});
            assert_eq!(rows_of(&quiet), reference, "{label}, limit {limit}");
            assert_eq!(
                quiet.len(),
                reference.len().div_ceil(limit),
                "{label}, limit {limit}: pages"
            );
            assert!(quiet.last().is_some_and(|page| page.next.is_none()));

            let disturbed = walk(
                &pinned,
                walk_ns,
                &query,
                &projection,
                |_| limit,
                |paged| head.disturb(&query, paged),
            );
            assert_eq!(
                disturbed, quiet,
                "{label}, limit {limit}: pages differ once the head advances"
            );
            expected_disturbances += quiet.len() - 1;
        }
    }
    assert_eq!(pinned.typed_record_count(walk_ns), Some(64));

    // Every disturbance published exactly one block (the losing candidates
    // none), and the head's state is what the head model says it is.
    let (at_head, published, stale_candidates) = head.finish();
    assert_eq!(
        published,
        u64::try_from(expected_disturbances).expect("fits")
    );
    assert!(
        stale_candidates >= 7 && published >= 100,
        "the head moved: {published} blocks, {stale_candidates} refused candidates"
    );
    let now = engine.head_snapshot();
    assert_eq!(now.id().sequence(), 1 + published);
    assert_eq!(now.typed_record_count(walk_ns), Some(at_head.len()));
    for (label, query) in shapes() {
        let head_rows = rows_of(&walk(&now, walk_ns, &query, &projection, |_| 7, |_| {}));
        assert_eq!(
            head_rows,
            at_head.evaluate(&query, &projection),
            "{label} at head"
        );
    }
    let books_now = at_head.evaluate(&books(), &projection);
    assert_ne!(books_now, model.evaluate(&books(), &projection));
    assert!(
        books_now.iter().any(|row| row.id > rid(WALK_RECORDS)),
        "books created at head carry ids beyond the pinned ones"
    );
}

#[test]
fn a_walk_may_change_its_limit_between_pages() {
    let Fixture {
        engine,
        walk_ns,
        model,
        ..
    } = fixture();
    let pinned = engine.head_snapshot();
    let query = books();
    let projection = Projection::KEY_ONLY;
    let reference = model.evaluate(&query, &projection);

    // Limits 1, 3, 7, 2, 5 cycle: 18 rows per cycle, so the 50 books take
    // two full cycles (36), then 1 + 3 + 7 + 2 = 13 and a final page of 1
    // (limit 5) that is not full and has no continuation.
    let cycle = [1, 3, 7, 2, 5];
    let pages = walk(
        &pinned,
        walk_ns,
        &query,
        &projection,
        |page| cycle[page % cycle.len()],
        |_| {},
    );
    assert_eq!(rows_of(&pages), reference);
    let sizes: Vec<usize> = pages.iter().map(|page| page.rows.len()).collect();
    assert_eq!(sizes, [1, 3, 7, 2, 5, 1, 3, 7, 2, 5, 1, 3, 7, 2, 1]);
    assert!(pages[..14].iter().all(|page| page.next.is_some()));
    assert!(pages[14].next.is_none());

    // A `max_page` page after a small one returns everything left.
    let first = pinned
        .query(walk_ns, &query, &projection, 3, None, Budget::unlimited())
        .expect("the namespace exists");
    let cursor = first.value.next.expect("47 books remain");
    let rest = pinned
        .query(
            walk_ns,
            &query,
            &projection,
            MAX_PAGE,
            Some(&cursor),
            Budget::unlimited(),
        )
        .expect("the cursor is bound to this snapshot");
    assert_eq!(rest.value.rows, reference[3..]);
    assert_eq!(rest.value.next, None);
}

// ---------------------------------------------------------------------------
// Hand-derived costs on the cost namespace
// ---------------------------------------------------------------------------

#[test]
fn eq_walk_charges_limit_plus_one_entries_then_exactly_the_remainder() {
    let Fixture {
        engine, cost_ns, ..
    } = fixture();
    let snapshot = engine.head_snapshot();
    let traced_budget = Budget::unlimited().traced();
    let query = books();
    let read = |limit: usize, cursor: Option<&Cursor>| {
        snapshot
            .query(
                cost_ns,
                &query,
                &Projection::ID_ONLY,
                limit,
                cursor,
                traced_budget,
            )
            .expect("the cost namespace exists")
    };
    let seek = [
        (OperationClass::DirectoryLookup, 1, 1, 1),
        (OperationClass::QueryGroup, 1, 5, 5),
        (OperationClass::TermSeek, 1, 4, 4),
    ];

    // Page 1 of 2: limit + 1 = 3 posting entries, the third being the head
    // beyond the page that proves a further match exists.
    let first = read(2, None);
    assert_eq!(first.value.rows, [id_row(1), id_row(2)]);
    let cursor = first.value.next.clone().expect("rec:4 is a further match");
    assert_eq!(cursor.after(), rid(2));
    assert_eq!(cursor.snapshot(), Some(snapshot.id()));
    assert_eq!(
        first.receipt,
        traced(
            read_applied(EQ_PAGE_ONE),
            &[
                seek[0],
                seek[1],
                seek[2],
                (OperationClass::PostingEntry, 3, 1, 3),
                (OperationClass::ResultRow, 2, 3, 6),
            ],
        )
    );

    // Page 2 of 2: exactly the one remaining entry; the exhausted posting
    // costs nothing more and the page has no continuation.
    let second = read(2, Some(&cursor));
    assert_eq!(second.value.rows, [id_row(4)]);
    assert_eq!(second.value.next, None);
    assert_eq!(
        second.receipt,
        traced(
            read_applied(EQ_PAGE_TWO),
            &[
                seek[0],
                seek[1],
                seek[2],
                (OperationClass::PostingEntry, 1, 1, 1),
                (OperationClass::ResultRow, 1, 3, 3),
            ],
        )
    );

    // An exactly full page is the last page: `next` is `None` and only the
    // three entries are charged, not a fourth.
    let full = read(3, None);
    assert_eq!(full.value.rows, [id_row(1), id_row(2), id_row(4)]);
    assert_eq!(full.value.next, None);
    assert_eq!(
        full.receipt,
        traced(
            read_applied(EQ_FULL_PAGE),
            &[
                seek[0],
                seek[1],
                seek[2],
                (OperationClass::PostingEntry, 3, 1, 3),
                (OperationClass::ResultRow, 3, 3, 9),
            ],
        )
    );
    let oversize = read(MAX_PAGE, None);
    assert_eq!(oversize.value.rows, full.value.rows);
    assert_eq!(oversize.value.next, None);
    assert_eq!(oversize.receipt, full.receipt);

    // Limit 1: two pages of `limit + 1` entries, then the remainder.
    let limit_one_ledger = [
        seek[0],
        seek[1],
        seek[2],
        (OperationClass::PostingEntry, 2, 1, 2),
        (OperationClass::ResultRow, 1, 3, 3),
    ];
    let one = read(1, None);
    assert_eq!(one.value.rows, [id_row(1)]);
    assert_eq!(
        one.receipt,
        traced(read_applied(EQ_LIMIT_ONE_PAGE), &limit_one_ledger)
    );
    let after_one = one.value.next.expect("rec:2 follows");
    assert_eq!(after_one.after(), rid(1));
    let two = read(1, Some(&after_one));
    assert_eq!(two.value.rows, [id_row(2)]);
    assert_eq!(
        two.receipt,
        traced(read_applied(EQ_LIMIT_ONE_PAGE), &limit_one_ledger)
    );
    let after_two = two.value.next.expect("rec:4 follows");
    assert_eq!(after_two.after(), rid(2));
    let three = read(1, Some(&after_two));
    assert_eq!(three.value.rows, [id_row(4)]);
    assert_eq!(three.value.next, None);
    assert_eq!(three.receipt.spent, Cost::new(EQ_PAGE_TWO));

    // A value nobody holds costs the seek only and claims no continuation.
    let none = snapshot
        .query(
            cost_ns,
            &kind_is("desk"),
            &Projection::ID_ONLY,
            2,
            None,
            traced_budget,
        )
        .expect("the cost namespace exists");
    assert_eq!(none.value.rows, []);
    assert_eq!(none.value.next, None);
    assert_eq!(none.receipt, traced(read_applied(EQ_MISSING_TERM), &seek));

    // With `Projection::ALL` every emitted row adds its 22 projected bytes.
    let all = snapshot
        .query(cost_ns, &query, &Projection::ALL, 2, None, traced_budget)
        .expect("the cost namespace exists");
    assert_eq!(
        all.value.rows,
        [cost_row(1, "a", "book", 10), cost_row(2, "b", "book", 20)]
    );
    assert_eq!(all.value.next.map(|cursor| cursor.after()), Some(rid(2)));
    assert_eq!(
        all.receipt,
        traced(
            read_applied(EQ_PAGE_ONE_ALL),
            &[
                seek[0],
                seek[1],
                seek[2],
                (OperationClass::PostingEntry, 3, 1, 3),
                (OperationClass::ResultRow, 2, 3, 6),
                (OperationClass::ReadByte, 44, 1, 44),
            ],
        )
    );
}

#[test]
fn a_group_whose_head_is_beyond_the_page_end_does_no_further_work() {
    let Fixture {
        engine, cost_ns, ..
    } = fixture();
    let snapshot = engine.head_snapshot();
    let traced_budget = Budget::unlimited().traced();
    let query = books_or_priced_lamps();
    let read = |cursor: Option<&Cursor>| {
        snapshot
            .query(
                cost_ns,
                &query,
                &Projection::ID_ONLY,
                2,
                cursor,
                traced_budget,
            )
            .expect("the cost namespace exists")
    };
    let open = [
        (OperationClass::DirectoryLookup, 1, 1, 1),
        (OperationClass::QueryGroup, 2, 5, 10),
        (OperationClass::TermSeek, 2, 4, 8),
    ];

    // Page 1: the lamp group pulls its head (rec:5, one entry and one probe)
    // and is then left alone, although its posting holds rec:6 too.
    let first = read(None);
    assert_eq!(first.value.rows, [id_row(1), id_row(2)]);
    let cursor = first.value.next.clone().expect("rec:4 follows");
    assert_eq!(cursor.after(), rid(2));
    assert_eq!(
        first.receipt,
        traced(
            read_applied(OR_PAGE_ONE),
            &[
                open[0],
                open[1],
                open[2],
                (OperationClass::PostingEntry, 4, 1, 4),
                (OperationClass::RecordProbe, 1, 2, 2),
                (OperationClass::ResultRow, 2, 3, 6),
            ],
        )
    );

    // Page 2: rec:4 exhausts the book group for free; emitting rec:5 is what
    // advances the lamp group to rec:6, the head beyond this page.
    let second = read(Some(&cursor));
    assert_eq!(second.value.rows, [id_row(4), id_row(5)]);
    let cursor = second.value.next.clone().expect("rec:6 follows");
    assert_eq!(cursor.after(), rid(5));
    assert_eq!(
        second.receipt,
        traced(
            read_applied(OR_PAGE_TWO),
            &[
                open[0],
                open[1],
                open[2],
                (OperationClass::PostingEntry, 3, 1, 3),
                (OperationClass::RecordProbe, 2, 2, 4),
                (OperationClass::ResultRow, 2, 3, 6),
            ],
        )
    );

    // Page 3: the book posting has nothing past rec:5 (no entry charged);
    // the lamp group yields rec:6 and the walk ends.
    let third = read(Some(&cursor));
    assert_eq!(third.value.rows, [id_row(6)]);
    assert_eq!(third.value.next, None);
    assert_eq!(
        third.receipt,
        traced(
            read_applied(OR_PAGE_THREE),
            &[
                open[0],
                open[1],
                open[2],
                (OperationClass::PostingEntry, 1, 1, 1),
                (OperationClass::RecordProbe, 1, 2, 2),
                (OperationClass::ResultRow, 1, 3, 3),
            ],
        )
    );
}

// ---------------------------------------------------------------------------
// Replay
// ---------------------------------------------------------------------------

#[test]
fn replaying_a_page_on_the_same_snapshot_is_identical() {
    let Fixture {
        engine,
        walk_ns,
        model,
        ..
    } = fixture();
    let pinned = engine.head_snapshot();
    let query = books();
    let projection = Projection::ALL;
    let reference = model.evaluate(&query, &projection);
    let traced_budget = Budget::unlimited().traced();
    let read = |snapshot: &Snapshot, cursor: Option<&Cursor>| {
        snapshot
            .query(walk_ns, &query, &projection, 7, cursor, traced_budget)
            .expect("the walk namespace exists")
    };

    let first = read(&pinned, None);
    assert_eq!(first.value.rows, reference[..7]);
    let cursor = first.value.next.clone().expect("43 books remain");
    assert_eq!(cursor.after(), reference[6].id);
    let second = read(&pinned, Some(&cursor));
    assert_eq!(second.value.rows, reference[7..14]);
    assert_eq!(second.receipt.kind, ReceiptKind::Read);
    assert_eq!(second.receipt.model_version, MODEL_VERSION);
    assert_eq!(second.receipt.schedule_id, FIXTURE_ID);
    assert!(second.receipt.ledger.is_some());

    // The same page twice: identical rows, continuation and receipt.
    let replay = read(&pinned, Some(&cursor));
    assert_eq!(replay.value, second.value);
    assert_eq!(replay.receipt, second.receipt);
    let first_again = read(&pinned, None);
    assert_eq!(first_again.value, first.value);
    assert_eq!(first_again.receipt, first.receipt);

    // Another handle to the same snapshot is the same snapshot: the cursor
    // is accepted there and the page is the same.
    let again = engine
        .snapshot(pinned.id())
        .expect("published snapshots resolve");
    let through_again = read(&again, Some(&cursor));
    assert_eq!(through_again.value, second.value);
    assert_eq!(through_again.receipt, second.receipt);
}

// ---------------------------------------------------------------------------
// Cursor binding
// ---------------------------------------------------------------------------

#[test]
fn cursor_from_another_snapshot_or_a_branch_is_a_mismatch() {
    let Fixture {
        mut engine,
        walk_ns,
        ..
    } = fixture();
    let query = books();
    let projection = Projection::ALL;
    let one = engine.head_snapshot();
    let page_one = one
        .query(walk_ns, &query, &projection, 7, None, Budget::unlimited())
        .expect("the walk namespace exists");
    let cursor_one = page_one.value.next.clone().expect("43 books remain");

    // An empty block: snapshot 2 has the identical (physically shared)
    // contents, yet it is another snapshot.
    let empty = engine.begin(engine.head(), None).expect("at head").seal();
    let two_id = engine.publish(&empty).expect("published on head");
    let two = engine.snapshot(two_id).expect("just published");
    assert!(two.typed_shared_with(&one, walk_ns));
    let page_two = two
        .query(walk_ns, &query, &projection, 7, None, Budget::unlimited())
        .expect("the walk namespace exists");
    assert_eq!(page_two.value.rows, page_one.value.rows);
    assert_eq!(page_two.receipt, page_one.receipt);
    let cursor_two = page_two.value.next.clone().expect("43 books remain");
    assert_ne!(cursor_two, cursor_one);
    assert_eq!(cursor_one.snapshot(), Some(one.id()));
    assert_eq!(cursor_two.snapshot(), Some(two_id));
    assert_eq!(cursor_two.after(), cursor_one.after());

    assert_mismatch(
        two.query(
            walk_ns,
            &query,
            &projection,
            7,
            Some(&cursor_one),
            Budget::unlimited(),
        ),
        "snapshot 1's cursor at snapshot 2",
    );
    assert_mismatch(
        one.query(
            walk_ns,
            &query,
            &projection,
            7,
            Some(&cursor_two),
            Budget::unlimited(),
        ),
        "snapshot 2's cursor at snapshot 1",
    );
    // A traced mismatch carries an empty ledger: nothing was charged.
    let traced_mismatch = rejection(
        two.query(
            walk_ns,
            &query,
            &projection,
            7,
            Some(&cursor_one),
            Budget::unlimited().traced(),
        ),
        "traced mismatch",
    );
    assert_eq!(
        traced_mismatch.receipt,
        traced(read_receipt(0, CURSOR_MISMATCH), &[])
    );
    // Each cursor continues at home.
    let home_one = one
        .query(
            walk_ns,
            &query,
            &projection,
            7,
            Some(&cursor_one),
            Budget::unlimited(),
        )
        .expect("bound to snapshot 1");
    let home_two = two
        .query(
            walk_ns,
            &query,
            &projection,
            7,
            Some(&cursor_two),
            Budget::unlimited(),
        )
        .expect("bound to snapshot 2");
    assert_eq!(home_one.value.rows, home_two.value.rows);

    // A branch refuses a snapshot cursor, its base refuses the branch's
    // cursor, and so does the snapshot the branch publishes.
    let branch = engine.begin(engine.head(), None).expect("at head");
    assert_mismatch(
        branch.query(
            walk_ns,
            &query,
            &projection,
            7,
            Some(&cursor_two),
            Budget::unlimited(),
        ),
        "the base snapshot's cursor on a branch",
    );
    let branch_page = branch
        .query(walk_ns, &query, &projection, 7, None, Budget::unlimited())
        .expect("the walk namespace exists");
    assert_eq!(branch_page.value.rows, page_two.value.rows);
    let branch_cursor = branch_page.value.next.expect("43 books remain");
    assert_eq!(branch_cursor.snapshot(), None);
    assert_mismatch(
        two.query(
            walk_ns,
            &query,
            &projection,
            7,
            Some(&branch_cursor),
            Budget::unlimited(),
        ),
        "a branch cursor on the branch's base",
    );
    let three_id = engine.publish(&branch.seal()).expect("published on head");
    let three = engine.snapshot(three_id).expect("just published");
    assert_mismatch(
        three.query(
            walk_ns,
            &query,
            &projection,
            7,
            Some(&branch_cursor),
            Budget::unlimited(),
        ),
        "a branch cursor on the snapshot the branch published",
    );
}

#[test]
fn cursor_from_another_engine_at_the_same_snapshot_id_is_a_mismatch() {
    let left = fixture();
    let right = fixture();
    let query = books();
    let projection = Projection::ALL;
    let left_snapshot = left.engine.head_snapshot();
    let right_snapshot = right.engine.head_snapshot();
    assert_eq!(left_snapshot.id(), right_snapshot.id());
    assert_eq!(left.walk_ns, right.walk_ns);
    let ns = left.walk_ns;

    let left_page = left_snapshot
        .query(ns, &query, &projection, 7, None, Budget::unlimited())
        .expect("the walk namespace exists");
    let right_page = right_snapshot
        .query(ns, &query, &projection, 7, None, Budget::unlimited())
        .expect("the walk namespace exists");
    assert_eq!(left_page.value.rows, right_page.value.rows);
    assert_eq!(left_page.receipt, right_page.receipt);
    let left_cursor = left_page.value.next.expect("43 books remain");
    let right_cursor = right_page.value.next.expect("43 books remain");
    assert_eq!(left_cursor.after(), right_cursor.after());
    assert_eq!(left_cursor.snapshot(), right_cursor.snapshot());

    assert_mismatch(
        right_snapshot.query(
            ns,
            &query,
            &projection,
            7,
            Some(&left_cursor),
            Budget::unlimited(),
        ),
        "the left engine's cursor at the right engine",
    );
    assert_mismatch(
        left_snapshot.query(
            ns,
            &query,
            &projection,
            7,
            Some(&right_cursor),
            Budget::unlimited(),
        ),
        "the right engine's cursor at the left engine",
    );
    let left_second = left_snapshot
        .query(
            ns,
            &query,
            &projection,
            7,
            Some(&left_cursor),
            Budget::unlimited(),
        )
        .expect("bound to the left engine");
    let right_second = right_snapshot
        .query(
            ns,
            &query,
            &projection,
            7,
            Some(&right_cursor),
            Budget::unlimited(),
        )
        .expect("bound to the right engine");
    assert_eq!(left_second.value.rows, right_second.value.rows);
}

#[test]
fn cursor_bound_to_another_namespace_query_or_projection_is_a_mismatch() {
    let Fixture {
        engine,
        walk_ns,
        cost_ns,
        model,
    } = fixture();
    let snapshot = engine.head_snapshot();
    let query = books();
    let projection = Projection::ALL;
    let reference = model.evaluate(&query, &projection);
    let page = snapshot
        .query(walk_ns, &query, &projection, 7, None, Budget::unlimited())
        .expect("the walk namespace exists");
    let cursor = page.value.next.expect("43 books remain");
    // The first seven books are records r00..r07 minus the pen r04: ids
    // 1, 2, 3, 4, 6, 7, 8.
    assert_eq!(cursor.after(), rid(8));
    assert_eq!(cursor.after(), reference[6].id);
    assert_eq!(cursor.namespace(), walk_ns);
    assert_eq!(cursor.query(), &query);
    assert_eq!(cursor.projection(), &projection);

    // Another namespace, although it holds books too.
    assert_mismatch(
        snapshot.query(
            cost_ns,
            &query,
            &projection,
            7,
            Some(&cursor),
            Budget::unlimited(),
        ),
        "another namespace",
    );

    // A different query: another value, the same literal in two groups, a
    // second literal.
    for (label, other) in [
        ("another value", kind_is("lamp")),
        (
            "the same literal in two groups",
            Query {
                groups: vec![query.groups[0].clone(), query.groups[0].clone()],
            },
        ),
        (
            "a second literal",
            Query::all(vec![
                Literal::of(Term::eq(name("kind"), Value::str("book"))),
                Literal::of(Term::exists(name("price"), TypeTag::I64)),
            ]),
        ),
    ] {
        assert_mismatch(
            snapshot.query(
                walk_ns,
                &other,
                &projection,
                7,
                Some(&cursor),
                Budget::unlimited(),
            ),
            label,
        );
    }
    // The same literals in another order are another query.
    let ordered_one = Query::all(vec![
        Literal::of(Term::exists(name("price"), TypeTag::I64)),
        Literal::of(Term::eq(name("kind"), Value::str("book"))),
    ]);
    let ordered_two = Query::all(vec![
        Literal::of(Term::eq(name("kind"), Value::str("book"))),
        Literal::of(Term::exists(name("price"), TypeTag::I64)),
    ]);
    let ordered_cursor = snapshot
        .query(
            walk_ns,
            &ordered_one,
            &projection,
            7,
            None,
            Budget::unlimited(),
        )
        .expect("the walk namespace exists")
        .value
        .next
        .expect("further matches exist");
    assert_mismatch(
        snapshot.query(
            walk_ns,
            &ordered_two,
            &projection,
            7,
            Some(&ordered_cursor),
            Budget::unlimited(),
        ),
        "the same literals in another order",
    );

    // A different projection.
    for (label, other) in [
        ("ids only", Projection::ID_ONLY),
        ("the key only", Projection::KEY_ONLY),
        ("named cells", Projection::named(true, [name("kind")])),
    ] {
        assert_mismatch(
            snapshot.query(
                walk_ns,
                &query,
                &other,
                7,
                Some(&cursor),
                Budget::unlimited(),
            ),
            label,
        );
    }
    let named = Projection::named(true, [name("kind")]);
    let named_cursor = snapshot
        .query(walk_ns, &query, &named, 7, None, Budget::unlimited())
        .expect("the walk namespace exists")
        .value
        .next
        .expect("43 books remain");
    assert_mismatch(
        snapshot.query(
            walk_ns,
            &query,
            &Projection::named(false, [name("kind")]),
            7,
            Some(&named_cursor),
            Budget::unlimited(),
        ),
        "the same names without the key",
    );

    // The limit is not bound: the same cursor continues with another limit.
    let continued = snapshot
        .query(
            walk_ns,
            &query,
            &projection,
            1,
            Some(&cursor),
            Budget::unlimited(),
        )
        .expect("bound to this snapshot, namespace, query and projection");
    assert_eq!(continued.value.rows, reference[7..8]);
    assert_eq!(
        continued.value.next.map(|cursor| cursor.after()),
        Some(reference[7].id)
    );
}

#[test]
fn cursor_check_comes_after_query_and_projection_validation() {
    let Fixture {
        mut engine,
        walk_ns,
        ..
    } = fixture();
    let query = books();
    let projection = Projection::ALL;
    let one = engine.head_snapshot();
    let foreign = one
        .query(walk_ns, &query, &projection, 7, None, Budget::unlimited())
        .expect("the walk namespace exists")
        .value
        .next
        .expect("43 books remain");
    let empty = engine.begin(engine.head(), None).expect("at head").seal();
    let two_id = engine.publish(&empty).expect("published on head");
    let two = engine.snapshot(two_id).expect("just published");

    // The query shape, the limit and the projection are checked first.
    assert_zero_cost(
        two.query(
            walk_ns,
            &Query { groups: Vec::new() },
            &projection,
            7,
            Some(&foreign),
            Budget::unlimited(),
        ),
        Rejection::InvalidQuery(QueryFault::NoGroups),
        "no groups",
    );
    assert_zero_cost(
        two.query(
            walk_ns,
            &query,
            &projection,
            0,
            Some(&foreign),
            Budget::unlimited(),
        ),
        Rejection::InvalidQuery(QueryFault::ZeroLimit),
        "zero limit",
    );
    assert_zero_cost(
        two.query(
            walk_ns,
            &query,
            &projection,
            MAX_PAGE + 1,
            Some(&foreign),
            Budget::unlimited(),
        ),
        Rejection::InvalidQuery(QueryFault::LimitTooLarge),
        "limit too large",
    );
    let too_many = Projection::named(true, (0..65).map(|n| name(&format!("c{n}"))));
    assert_zero_cost(
        two.query(
            walk_ns,
            &query,
            &too_many,
            7,
            Some(&foreign),
            Budget::unlimited(),
        ),
        Rejection::InvalidQuery(QueryFault::ProjectionTooLarge),
        "projection too large",
    );
    // Only a valid request reaches the cursor check.
    assert_mismatch(
        two.query(
            walk_ns,
            &query,
            &projection,
            7,
            Some(&foreign),
            Budget::unlimited(),
        ),
        "a valid request with a foreign cursor",
    );
    // An unknown namespace is charged (the directory lookup) but a foreign
    // cursor is refused first, at admission.
    let unknown = NamespaceId::from_ordinal(99).expect("non-zero");
    assert_mismatch(
        two.query(
            unknown,
            &query,
            &projection,
            7,
            Some(&foreign),
            Budget::unlimited(),
        ),
        "a foreign cursor on an unknown namespace",
    );
}

// ---------------------------------------------------------------------------
// Branch cursors
// ---------------------------------------------------------------------------

/// A fresh branch on the fixture head with the first page (limit 2) of the
/// cost namespace's books read: rows `[1, 2]`, cursor after 2.
fn branch_with_cursor(engine: &Engine, cost_ns: NamespaceId) -> (Branch, Cursor) {
    let branch = engine.begin(engine.head(), None).expect("at head");
    let page = branch
        .query(
            cost_ns,
            &books(),
            &Projection::ID_ONLY,
            2,
            None,
            Budget::unlimited(),
        )
        .expect("the cost namespace exists");
    assert_eq!(page.value.rows, [id_row(1), id_row(2)]);
    assert_eq!(page.receipt, read_applied(EQ_PAGE_ONE));
    let cursor = page.value.next.expect("rec:4 follows");
    assert_eq!(cursor.after(), rid(2));
    assert_eq!(cursor.snapshot(), None, "a branch cursor names no snapshot");
    (branch, cursor)
}

fn second_page(branch: &Branch, cost_ns: NamespaceId, cursor: &Cursor) -> ReadResult<QueryPage> {
    branch.query(
        cost_ns,
        &books(),
        &Projection::ID_ONLY,
        2,
        Some(cursor),
        Budget::unlimited().traced(),
    )
}

fn create_g(cost_ns: NamespaceId) -> WriteOp {
    WriteOp::CreateRecord {
        namespace: cost_ns,
        key: key("g"),
        cells: cost_cells("book", 40),
    }
}

#[test]
fn branch_cursor_continues_on_the_same_untouched_branch() {
    let Fixture {
        engine, cost_ns, ..
    } = fixture();
    let mut branch = engine.begin(engine.head(), None).expect("at head");
    // A branch-local fourth book, id 7, visible to the branch's own pages.
    let created = apply(&mut branch, ALICE, create_g(cost_ns));
    assert_eq!(created.effect, Effect::RecordCreated { id: rid(7) });
    let first = branch
        .query(
            cost_ns,
            &books(),
            &Projection::ID_ONLY,
            2,
            None,
            Budget::unlimited().traced(),
        )
        .expect("the cost namespace exists");
    assert_eq!(first.value.rows, [id_row(1), id_row(2)]);
    assert_eq!(first.receipt.spent, Cost::new(EQ_PAGE_ONE));
    let cursor = first.value.next.expect("rec:4 and rec:7 follow");
    assert_eq!(cursor.snapshot(), None);

    // Nothing below advances the branch's mutation serial: estimates,
    // a checkpoint, writes rejected after charging and writes rejected at
    // admission.
    let estimated = branch
        .estimate(
            ALICE,
            WriteOp::CreateRecord {
                namespace: cost_ns,
                key: key("h"),
                cells: cost_cells("book", 50),
            },
            Budget::unlimited(),
        )
        .expect("the estimate applies on scratch state");
    assert_eq!(estimated.effect, Effect::RecordCreated { id: rid(8) });
    branch
        .estimate_host(
            WriteOp::DeleteRecord {
                namespace: cost_ns,
                key: key("c"),
            },
            Budget::unlimited(),
        )
        .expect("the host estimate applies on scratch state");
    let _checkpoint = branch.checkpoint();
    let unauthorized = branch
        .execute(
            BOB,
            WriteOp::CreateRecord {
                namespace: cost_ns,
                key: key("h"),
                cells: cost_cells("book", 50),
            },
            Budget::unlimited(),
        )
        .expect_err("BOB does not own the namespace");
    assert_eq!(unauthorized.reason, Rejection::Unauthorized);
    assert_eq!(unauthorized.receipt.spent, Cost::new(1));
    let not_found = branch
        .execute(
            ALICE,
            WriteOp::DeleteRecord {
                namespace: cost_ns,
                key: key("zz"),
            },
            Budget::unlimited(),
        )
        .expect_err("zz is not bound");
    assert_eq!(not_found.reason, Rejection::NotFound);
    assert_eq!(not_found.receipt.spent, Cost::new(4));
    let empty_patch = branch
        .execute(
            ALICE,
            WriteOp::PatchRecord {
                namespace: cost_ns,
                key: key("zz"),
                set: Vec::new(),
                remove: Vec::new(),
            },
            Budget::unlimited(),
        )
        .expect_err("an empty patch of an unbound key is not found");
    assert_eq!(empty_patch.reason, Rejection::NotFound);
    assert_eq!(empty_patch.receipt.spent, Cost::new(4));
    let not_host = branch
        .execute_host(
            WriteOp::CreateNamespace {
                name: arkiv_db::NamespaceName::new("test").unwrap(),
            },
            Budget::unlimited(),
        )
        .expect_err("the host path accepts record operations only");
    assert_eq!(
        not_host.reason,
        Rejection::InvalidArgument(arkiv_db::ArgumentFault::NotHostOperation)
    );
    assert_eq!(not_host.receipt.spent, Cost::new(0));
    assert_eq!(
        branch.receipts().len(),
        4,
        "the create and the three charged rejections were recorded"
    );

    let second = second_page(&branch, cost_ns, &cursor).expect("the cursor is still bound");
    assert_eq!(second.value.rows, [id_row(4), id_row(7)]);
    assert_eq!(second.value.next, None);
    assert_eq!(
        second.receipt,
        traced(
            read_applied(EQ_BRANCH_PAGE_TWO),
            &[
                (OperationClass::DirectoryLookup, 1, 1, 1),
                (OperationClass::QueryGroup, 1, 5, 5),
                (OperationClass::TermSeek, 1, 4, 4),
                (OperationClass::PostingEntry, 2, 1, 2),
                (OperationClass::ResultRow, 2, 3, 6),
            ],
        )
    );
    // Replaying the branch page is identical too.
    let replay = second_page(&branch, cost_ns, &cursor).expect("still bound");
    assert_eq!(replay.value, second.value);
    assert_eq!(replay.receipt, second.receipt);
}

#[test]
fn branch_cursor_is_invalidated_by_every_applied_mutation() {
    let Fixture {
        engine, cost_ns, ..
    } = fixture();
    let renewed_page_two = |branch: &Branch| {
        let first = branch
            .query(
                cost_ns,
                &books(),
                &Projection::ID_ONLY,
                2,
                None,
                Budget::unlimited(),
            )
            .expect("the cost namespace exists");
        assert_eq!(first.value.rows, [id_row(1), id_row(2)]);
        let cursor = first.value.next.expect("rec:4 follows");
        second_page(branch, cost_ns, &cursor).expect("a cursor issued at the current serial")
    };

    // An applied execute.
    let (mut branch, cursor) = branch_with_cursor(&engine, cost_ns);
    apply(&mut branch, ALICE, create_g(cost_ns));
    assert_mismatch(
        second_page(&branch, cost_ns, &cursor),
        "after an applied execute",
    );
    let renewed = renewed_page_two(&branch);
    assert_eq!(renewed.value.rows, [id_row(4), id_row(7)]);
    assert_eq!(renewed.receipt.spent, Cost::new(EQ_BRANCH_PAGE_TWO));

    // An applied execute_host, even one that leaves the result set as it
    // was (it deletes the pen).
    let (mut branch, cursor) = branch_with_cursor(&engine, cost_ns);
    branch
        .execute_host(
            WriteOp::DeleteRecord {
                namespace: cost_ns,
                key: key("c"),
            },
            Budget::unlimited(),
        )
        .expect("the host deletes without an owner check");
    assert_mismatch(
        second_page(&branch, cost_ns, &cursor),
        "after an execute_host",
    );
    let renewed = renewed_page_two(&branch);
    assert_eq!(renewed.value.rows, [id_row(4)]);
    assert_eq!(renewed.receipt.spent, Cost::new(EQ_PAGE_TWO));

    // A rollback, even one that discards nothing.
    let (mut branch, cursor) = branch_with_cursor(&engine, cost_ns);
    let checkpoint = branch.checkpoint();
    assert_eq!(branch.rollback_to(checkpoint), Ok(0..0));
    assert_mismatch(second_page(&branch, cost_ns, &cursor), "after a rollback");
    assert_eq!(renewed_page_two(&branch).value.rows, [id_row(4)]);

    // A pricing change: the cursor is gone, yet the branch's reads stay
    // priced with the pinned schedule.
    let (mut branch, cursor) = branch_with_cursor(&engine, cost_ns);
    branch
        .set_pricing(PriceSchedule::FIXTURE.with_id(2))
        .expect("id 2 exceeds the committed id 1");
    assert_mismatch(second_page(&branch, cost_ns, &cursor), "after set_pricing");
    let renewed = renewed_page_two(&branch);
    assert_eq!(renewed.value.rows, [id_row(4)]);
    assert_eq!(
        renewed.receipt.schedule_id, FIXTURE_ID,
        "the branch prices reads with its base's schedule"
    );
    assert_eq!(renewed.receipt.spent, Cost::new(EQ_PAGE_TWO));

    // Another branch on the same base, untouched, is another branch.
    let (branch_a, cursor_a) = branch_with_cursor(&engine, cost_ns);
    let (branch_b, cursor_b) = branch_with_cursor(&engine, cost_ns);
    assert_ne!(cursor_a, cursor_b);
    assert_mismatch(second_page(&branch_b, cost_ns, &cursor_a), "another branch");
    assert_mismatch(second_page(&branch_a, cost_ns, &cursor_b), "another branch");
    assert_eq!(
        second_page(&branch_a, cost_ns, &cursor_a)
            .expect("at home")
            .value
            .rows,
        [id_row(4)]
    );
}

// ---------------------------------------------------------------------------
// The host-side pattern
// ---------------------------------------------------------------------------

#[test]
fn host_resolves_an_omitted_block_once_and_pages_that_snapshot() {
    let Fixture {
        mut engine,
        walk_ns,
        model,
        ..
    } = fixture();
    let query = books();
    let projection = Projection::ALL;
    let reference = model.evaluate(&query, &projection);
    let mut blocks = BlockMap::new();
    blocks
        .record(BlockNumber(100), engine.head())
        .expect("the first block may be any number");

    // A request that omits the block: the host resolves the latest block
    // once and pins that snapshot for the whole walk.
    let (block, resolved) = blocks.latest().expect("block 100 is recorded");
    assert_eq!(block, BlockNumber(100));
    assert_eq!(resolved.sequence(), 1);
    let pinned = blocks.at(&engine, block).expect("recorded snapshots pin");
    assert_eq!(pinned.id(), resolved);

    // The total is a separate metered read at the same snapshot.
    let total = pinned
        .count(walk_ns, &query, Budget::unlimited())
        .expect("the walk namespace exists");
    assert_eq!(total.value, 50);
    assert_eq!(total.receipt, read_applied(COUNT_BOOKS));

    // Between pages the chain produces a block: it deletes the next book at
    // head (the very row the pinned walk returns next) and the first one
    // also commits a new pricing schedule.
    let mut at_head = model.clone();
    let mut deleted: Vec<RecordId> = Vec::new();
    let pages = walk(
        &pinned,
        walk_ns,
        &query,
        &projection,
        |_| 7,
        |paged| {
            let last = paged.last().expect("a continued page has rows").id;
            let victim = at_head
                .evaluate(&query, &Projection::KEY_ONLY)
                .into_iter()
                .find(|row| row.id > last)
                .expect("the head still holds books beyond the paged prefix");
            let key = victim.key.expect("the key was projected");
            let mut branch = engine.begin(engine.head(), None).expect("at head");
            apply(
                &mut branch,
                ALICE,
                WriteOp::DeleteRecord {
                    namespace: walk_ns,
                    key: key.clone(),
                },
            );
            if deleted.is_empty() {
                branch
                    .set_pricing(PriceSchedule::FIXTURE.with_id(2))
                    .expect("id 2 exceeds the committed id 1");
            }
            at_head.delete(&key);
            deleted.push(victim.id);
            let published = engine.publish(&branch.seal()).expect("published on head");
            blocks
                .record(blocks.next_block().expect("consecutive"), published)
                .expect("blocks are consecutive and snapshots increase");
        },
    );
    assert_eq!(rows_of(&pages), reference);
    assert_eq!(pages.len(), 8, "ceil(50 / 7)");
    assert_eq!(deleted.len(), 7);
    assert!(
        pages
            .iter()
            .all(|page| page.receipt.schedule_id == FIXTURE_ID),
        "the pinned snapshot prices every page with its own schedule"
    );
    // Every row deleted at head was still returned by the pinned walk, on
    // the page right after its deletion.
    for (index, id) in deleted.iter().enumerate() {
        assert_eq!(pages[index + 1].rows[0].id, *id);
    }
    assert_eq!(
        pinned
            .count(walk_ns, &query, Budget::unlimited())
            .expect("the walk namespace exists")
            .value,
        50
    );

    // The head moved on: a fresh resolution names block 107, prices reads
    // with schedule 2, answers with 43 books and refuses the old cursor.
    let (latest_block, latest) = blocks.latest().expect("blocks were recorded");
    assert_eq!(latest_block, BlockNumber(107));
    assert_eq!(latest.sequence(), 8);
    assert_ne!(latest, pinned.id());
    let now = blocks
        .at(&engine, latest_block)
        .expect("recorded snapshots pin");
    assert_eq!(now.pricing().id(), 2);
    let now_total = now
        .count(walk_ns, &query, Budget::unlimited())
        .expect("the walk namespace exists");
    assert_eq!(now_total.value, 43);
    assert_eq!(
        now_total.receipt,
        receipt(
            ReceiptKind::Read,
            2,
            COUNT_BOOKS_AFTER_DELETES,
            Outcome::Applied
        )
    );
    let now_rows = rows_of(&walk(&now, walk_ns, &query, &projection, |_| 7, |_| {}));
    assert_eq!(now_rows, at_head.evaluate(&query, &projection));
    assert_eq!(now_rows.len(), 43);
    assert!(
        deleted
            .iter()
            .all(|id| !now_rows.iter().any(|row| row.id == *id))
    );
    // The refusal is priced (at zero) under the new snapshot's own schedule.
    let first_cursor = pages[0].next.clone().expect("page 1 continued");
    assert_zero_cost_under(
        now.query(
            walk_ns,
            &query,
            &projection,
            7,
            Some(&first_cursor),
            Budget::unlimited(),
        ),
        Rejection::CursorMismatch,
        2,
        "the pinned walk's cursor at the new latest snapshot",
    );
    // The pinned handle is still the resolved snapshot and still answers.
    assert_eq!(pinned.id(), resolved);
    let again = pinned
        .query(
            walk_ns,
            &query,
            &projection,
            7,
            Some(&first_cursor),
            Budget::unlimited(),
        )
        .expect("bound to the pinned snapshot");
    assert_eq!(again.value.rows, pages[1].rows);
}
