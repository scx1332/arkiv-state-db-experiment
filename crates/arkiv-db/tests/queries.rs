//! Query operator semantics checked against an independent full-scan
//! reference, the validation order of every `QueryFault`, and a handful of
//! hand-derived query charges.
//!
//! The semantics fixture is one namespace of 45 records whose cell names
//! deliberately overlap: `n` is a `Str`, `I64`, `U64` or `Bool` attribute in
//! different records, a field holding the very same values in others, and
//! absent in the rest; `m` and `k` overlap the same way; `q` never occurs.
//! Values include the empty string, prefixes that are also full values,
//! non-ASCII text, `i64::MIN`, `i64::MAX`, `u64::MAX`, negative integers and
//! duplicates across records.
//!
//! What this file establishes:
//! - Every single-literal term over every (name, value) of the fixture,
//!   positive and negated, and a deterministic set of AND groups and ORs of
//!   groups, returns exactly the records that a literal full scan of the
//!   brief's semantics selects (a `BTreeMap` model maintained beside the
//!   engine; no engine index is consulted), in ascending id, page after page
//!   under varying limits, with `next` present iff a further match exists,
//!   with the projected rows equal to the model's, and with `count` equal to
//!   the number of matches.
//! - Fields never match; a cell of another type is absent; `NotEq` needs the
//!   attribute; `lower > upper` is empty; an empty prefix matches every `Str`
//!   attribute of the name; negation is the complement over live records.
//! - Patches move records between indexes (type change, attribute/field
//!   change, removal, addition, no-op), deletes remove them, a recreated key
//!   gets a fresh id, and `index_consistent` plus the record and term counts
//!   hold after every mutation and after a rollback.
//! - Every `QueryFault` is reported in the brief's validation order, before
//!   any charge (spent 0, a receipt still issued), for `query`, `count` and
//!   `get_record`; the cursor check comes last.
//! - A few single-group and two-group query charges are the hand-derived
//!   literals below (FIXTURE weights: DirectoryLookup 1, QueryGroup 5,
//!   TermSeek 4, TermVisit 3, PostingEntry 1, RowVisit 1, RecordProbe 2,
//!   ResultRow 3, ReadByte 1).
//!
//! What it does not establish: write charges (create, patch and delete
//! receipts are not asserted here), physical traces, snapshot isolation
//! beyond one before/after check, the full cursor-binding matrix, expiry, or
//! anything about the raw byte operations.

use std::{
    collections::{BTreeMap, HashSet},
    fmt::Debug,
    ops::Bound,
};

use arkiv_db::{
    Branch, Budget, Cell, CellKind, CellName, CellSelect, Cost, Cursor, Effect, Engine, Ledger,
    LedgerEntry, Limits, Literal, NamespaceId, OperationClass, Outcome, OwnerId, PriceSchedule,
    Projection, Query, QueryFault, QueryPage, ReadResult, Receipt, ReceiptKind, RecordId,
    RecordKey, Rejection, Row, Snapshot, SnapshotId, Term, TypeTag, Value, WriteOp,
};

const ALICE: OwnerId = OwnerId::new([0xa1; 20]);
/// Counting-rule version recorded in every receipt.
const MODEL_VERSION: u32 = 3;
/// The id `PriceSchedule::FIXTURE` carries.
const FIXTURE_ID: u32 = 1;

// ---------------------------------------------------------------------------
// Small constructors
// ---------------------------------------------------------------------------

fn name(text: &str) -> CellName {
    CellName::new(text).expect("fixture names follow the grammar")
}

fn key(text: &str) -> RecordKey {
    RecordKey::new(text.as_bytes()).expect("fixture keys are 1..=32 bytes")
}

fn id(ordinal: u64) -> RecordId {
    RecordId::from_ordinal(ordinal).expect("ordinals start at 1")
}

fn ns_id(ordinal: u64) -> NamespaceId {
    NamespaceId::from_bytes(ordinal.to_be_bytes()).expect("ordinals start at 1")
}

fn attr(value: Value) -> Cell {
    Cell::attribute(value)
}

fn field(value: Value) -> Cell {
    Cell::field(value)
}

fn s(text: &str) -> Value {
    Value::str(text)
}

fn named(cells: Vec<(&str, Cell)>) -> Vec<(CellName, Cell)> {
    cells
        .into_iter()
        .map(|(text, cell)| (name(text), cell))
        .collect()
}

fn read_receipt(spent: u64, outcome: Outcome) -> Receipt {
    Receipt {
        kind: ReceiptKind::Read,
        model_version: MODEL_VERSION,
        schedule_id: FIXTURE_ID,
        spent: Cost::new(spent),
        outcome,
        ledger: None,
    }
}

fn applied(spent: u64) -> Receipt {
    read_receipt(spent, Outcome::Applied)
}

/// Ledger lines as `(class, count, unit_price, subtotal)`.
fn traced(receipt: Receipt, lines: &[(OperationClass, u64, u64, u64)]) -> Receipt {
    Receipt {
        ledger: Some(Ledger {
            entries: lines
                .iter()
                .map(|&(class, count, unit_price, subtotal)| LedgerEntry {
                    class,
                    count,
                    unit_price,
                    subtotal: Cost::new(subtotal),
                })
                .collect(),
        }),
        ..receipt
    }
}

fn ordinals(rows: &[Row]) -> Vec<u64> {
    rows.iter().map(|row| row.id.ordinal()).collect()
}

// ---------------------------------------------------------------------------
// The independent model: records as sorted cell maps, and a full-scan
// evaluator that applies the brief's semantics literally.
// ---------------------------------------------------------------------------

type Cells = BTreeMap<CellName, Cell>;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Model {
    next: u64,
    live: BTreeMap<RecordId, (RecordKey, Cells)>,
}

impl Model {
    fn new() -> Self {
        Self {
            next: 1,
            live: BTreeMap::new(),
        }
    }

    fn find(&self, key: &RecordKey) -> Option<RecordId> {
        self.live
            .iter()
            .find(|(_, (bound, _))| bound == key)
            .map(|(id, _)| *id)
    }

    /// Mints the next id (never reused) and stores the cells.
    fn create(&mut self, key: RecordKey, cells: Vec<(CellName, Cell)>) -> RecordId {
        assert_eq!(self.find(&key), None, "model: key already bound");
        let id = RecordId::from_ordinal(self.next).expect("ids start at 1");
        self.next += 1;
        self.live.insert(id, (key, cells.into_iter().collect()));
        id
    }

    fn patch(
        &mut self,
        key: &RecordKey,
        set: Vec<(CellName, Cell)>,
        remove: &[CellName],
    ) -> RecordId {
        let id = self.find(key).expect("model: patch of a bound key");
        let (_, cells) = self.live.get_mut(&id).expect("live");
        for (name, cell) in set {
            cells.insert(name, cell);
        }
        for name in remove {
            cells.remove(name);
        }
        id
    }

    fn delete(&mut self, key: &RecordKey) -> RecordId {
        let id = self.find(key).expect("model: delete of a bound key");
        self.live.remove(&id);
        id
    }

    /// Ids of every live record the query selects, ascending.
    fn matches(&self, query: &Query) -> Vec<RecordId> {
        self.live
            .iter()
            .filter(|(_, (_, cells))| {
                query.groups.iter().any(|group| {
                    group
                        .iter()
                        .all(|literal| term_holds(&literal.term, cells) != literal.negated)
                })
            })
            .map(|(id, _)| *id)
            .collect()
    }

    /// The row a projection returns for a live record.
    fn row(&self, id: RecordId, projection: &Projection) -> Row {
        let (key, cells) = &self.live[&id];
        let cells: Vec<(CellName, Cell)> = match &projection.cells {
            CellSelect::None => Vec::new(),
            CellSelect::All => cells
                .iter()
                .map(|(name, cell)| (name.clone(), cell.clone()))
                .collect(),
            CellSelect::Named(names) => cells
                .iter()
                .filter(|(name, _)| names.contains(*name))
                .map(|(name, cell)| (name.clone(), cell.clone()))
                .collect(),
        };
        Row {
            id,
            key: projection.key.then(|| key.clone()),
            cells,
        }
    }

    /// Distinct value terms plus distinct existence terms of the live
    /// attribute cells: what the index must hold, no more and no less.
    fn term_count(&self) -> usize {
        let mut terms: HashSet<(CellName, TypeTag, Option<Value>)> = HashSet::new();
        for (_, cells) in self.live.values() {
            for (name, cell) in cells {
                if cell.kind == CellKind::Attribute {
                    let tag = type_of(&cell.value);
                    terms.insert((name.clone(), tag, Some(cell.value.clone())));
                    terms.insert((name.clone(), tag, None));
                }
            }
        }
        terms.len()
    }
}

/// The type of a value, decided by variant here rather than by the engine.
fn type_of(value: &Value) -> TypeTag {
    match value {
        Value::Bool(_) => TypeTag::Bool,
        Value::I64(_) => TypeTag::I64,
        Value::U64(_) => TypeTag::U64,
        Value::Str(_) => TypeTag::Str,
        Value::Bytes(_) => TypeTag::Bytes,
        Value::I32(_) => TypeTag::I32,
        Value::U256(_) => TypeTag::U256,
        Value::Dec(_) => TypeTag::Dec,
        Value::Bytes32(_) => TypeTag::Bytes32,
        Value::Addr(_) => TypeTag::Addr,
        Value::Key(_) => TypeTag::Key,
        Value::Bytes4(_) => TypeTag::Bytes4,
        Value::Bytes8(_) => TypeTag::Bytes8,
        Value::Bytes16(_) => TypeTag::Bytes16,
        Value::U32(_) => TypeTag::U32,
        Value::U128(_) => TypeTag::U128,
        Value::I128(_) => TypeTag::I128,
        Value::I256(_) => TypeTag::I256,
        Value::Dec32(_) => TypeTag::Dec32,
        Value::Dec64(_) => TypeTag::Dec64,
        Value::Dec128(_) => TypeTag::Dec128,
        Value::F32(_) => TypeTag::F32,
        Value::F64(_) => TypeTag::F64,
        Value::Date32(_) => TypeTag::Date32,
        Value::Timestamp64(_) => TypeTag::Timestamp64,
    }
}

/// The brief's semantics of one term on one live record, verbatim.
fn term_holds(term: &Term, cells: &Cells) -> bool {
    let Some(cell) = cells.get(term.name()) else {
        return false;
    };
    if cell.kind != CellKind::Attribute {
        return false;
    }
    let held = &cell.value;
    match term {
        Term::Eq { value, .. } => type_of(held) == type_of(value) && held == value,
        Term::NotEq { value, .. } => type_of(held) == type_of(value) && held != value,
        Term::Range { lower, upper, .. } => in_range(held, lower, upper),
        Term::StartsWith { prefix, .. } => {
            matches!(held, Value::Str(text) if text.as_bytes().starts_with(prefix.as_bytes()))
        }
        Term::Exists { tag, .. } => type_of(held) == *tag,
    }
}

/// `lower <= held <= upper` honouring the bounds, with plain integer
/// comparisons; a bound of another type never admits the value.
fn in_range(held: &Value, lower: &Bound<Value>, upper: &Bound<Value>) -> bool {
    match held {
        Value::I64(value) => {
            let above = match lower {
                Bound::Unbounded => true,
                Bound::Included(Value::I64(low)) => value >= low,
                Bound::Excluded(Value::I64(low)) => value > low,
                _ => false,
            };
            let below = match upper {
                Bound::Unbounded => true,
                Bound::Included(Value::I64(high)) => value <= high,
                Bound::Excluded(Value::I64(high)) => value < high,
                _ => false,
            };
            above && below
        }
        Value::U64(value) => {
            let above = match lower {
                Bound::Unbounded => true,
                Bound::Included(Value::U64(low)) => value >= low,
                Bound::Excluded(Value::U64(low)) => value > low,
                _ => false,
            };
            let below = match upper {
                Bound::Unbounded => true,
                Bound::Included(Value::U64(high)) => value <= high,
                Bound::Excluded(Value::U64(high)) => value < high,
                _ => false,
            };
            above && below
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// The semantics fixture: 45 records with overlapping names.
// ---------------------------------------------------------------------------

/// Created in this order, so `rNN` has id NN.
fn fixture_records() -> Vec<(&'static str, Vec<(&'static str, Cell)>)> {
    let (min, max, umax) = (i64::MIN, i64::MAX, u64::MAX);
    vec![
        // Str attributes of `n`; `m` cycles through every type.
        ("r01", vec![("n", attr(s(""))), ("m", attr(Value::I64(0)))]),
        ("r02", vec![("n", attr(s("a"))), ("m", attr(Value::I64(1)))]),
        (
            "r03",
            vec![("n", attr(s("ab"))), ("m", attr(Value::I64(-1)))],
        ),
        (
            "r04",
            vec![("n", attr(s("abc"))), ("m", attr(Value::I64(min)))],
        ),
        (
            "r05",
            vec![("n", attr(s("b"))), ("m", attr(Value::I64(max)))],
        ),
        ("r06", vec![("n", attr(s("ä"))), ("m", attr(Value::U64(0)))]),
        (
            "r07",
            vec![("n", attr(s("äb"))), ("m", attr(Value::U64(umax)))],
        ),
        ("r08", vec![("n", attr(s("aä"))), ("m", attr(s("a")))]),
        (
            "r09",
            vec![("n", attr(s("abc"))), ("m", attr(Value::Bool(true)))],
        ),
        ("r10", vec![("n", attr(s("a")))]),
        // I64 attributes of `n`.
        (
            "r11",
            vec![("n", attr(Value::I64(min))), ("m", attr(s("ab")))],
        ),
        (
            "r12",
            vec![("n", attr(Value::I64(-1))), ("m", attr(s("abc")))],
        ),
        ("r13", vec![("n", attr(Value::I64(0))), ("m", attr(s("")))]),
        (
            "r14",
            vec![("n", attr(Value::I64(1))), ("m", attr(Value::I64(0)))],
        ),
        (
            "r15",
            vec![("n", attr(Value::I64(7))), ("m", attr(Value::I64(7)))],
        ),
        (
            "r16",
            vec![("n", attr(Value::I64(max))), ("m", attr(Value::U64(7)))],
        ),
        (
            "r17",
            vec![("n", attr(Value::I64(0))), ("m", field(Value::I64(0)))],
        ),
        ("r18", vec![("n", attr(Value::I64(7)))]),
        // U64 attributes of `n`.
        (
            "r19",
            vec![("n", attr(Value::U64(0))), ("m", attr(Value::Bool(false)))],
        ),
        (
            "r20",
            vec![("n", attr(Value::U64(1))), ("m", attr(Value::I64(1)))],
        ),
        (
            "r21",
            vec![("n", attr(Value::U64(7))), ("m", attr(Value::U64(1)))],
        ),
        (
            "r22",
            vec![("n", attr(Value::U64(umax))), ("m", attr(Value::I64(-1)))],
        ),
        ("r23", vec![("n", attr(Value::U64(7)))]),
        // Bool attributes of `n`.
        (
            "r24",
            vec![("n", attr(Value::Bool(true))), ("m", attr(s("b")))],
        ),
        (
            "r25",
            vec![("n", attr(Value::Bool(false))), ("m", attr(s("a")))],
        ),
        (
            "r26",
            vec![("n", attr(Value::Bool(true))), ("m", attr(Value::I64(7)))],
        ),
        // Fields of `n` holding values that attributes of `n` also hold.
        (
            "r27",
            vec![("n", field(s("a"))), ("m", attr(Value::I64(0)))],
        ),
        (
            "r28",
            vec![("n", field(Value::I64(0))), ("m", attr(s("a")))],
        ),
        (
            "r29",
            vec![("n", field(Value::U64(7))), ("m", attr(Value::U64(7)))],
        ),
        (
            "r30",
            vec![
                ("n", field(Value::Bool(true))),
                ("m", attr(Value::Bool(true))),
            ],
        ),
        (
            "r31",
            vec![("n", field(Value::bytes(b"a"))), ("m", attr(Value::I64(1)))],
        ),
        // Records without `n`.
        ("r32", vec![("m", attr(s("abc")))]),
        ("r33", vec![("m", attr(Value::I64(min)))]),
        ("r34", vec![("m", field(s("abc")))]),
        ("r35", vec![("k", attr(s("only")))]),
        // Three-cell records and more duplicates.
        (
            "r36",
            vec![
                ("n", attr(s("abc"))),
                ("m", attr(Value::I64(7))),
                ("k", attr(Value::Bool(true))),
            ],
        ),
        (
            "r37",
            vec![
                ("n", attr(Value::I64(7))),
                ("m", attr(s("abc"))),
                ("k", attr(Value::U64(umax))),
            ],
        ),
        (
            "r38",
            vec![("n", attr(Value::U64(1))), ("m", attr(Value::U64(umax)))],
        ),
        (
            "r39",
            vec![
                ("n", attr(s("a"))),
                ("m", attr(s("a"))),
                ("k", field(Value::bytes(b""))),
            ],
        ),
        (
            "r40",
            vec![
                ("n", attr(Value::Bool(false))),
                ("m", attr(Value::Bool(false))),
            ],
        ),
        (
            "r41",
            vec![("n", attr(s("ab"))), ("m", attr(Value::I64(1)))],
        ),
        (
            "r42",
            vec![
                ("n", attr(Value::I64(1))),
                ("m", attr(Value::I64(-1))),
                ("k", attr(Value::I64(-1))),
            ],
        ),
        (
            "r43",
            vec![("n", attr(Value::U64(0))), ("m", attr(Value::U64(0)))],
        ),
        (
            "r44",
            vec![("n", attr(s("\u{10FFFF}"))), ("m", attr(Value::I64(max)))],
        ),
        (
            "r45",
            vec![("n", attr(s("aä"))), ("m", attr(Value::I64(7)))],
        ),
    ]
}

const FIXTURE_SIZE: usize = 45;

/// A branch and the model kept in step with it; every mutation goes through
/// both and checks the index invariant and the unmetered counts afterwards.
struct World {
    ns: NamespaceId,
    branch: Branch,
    model: Model,
}

impl World {
    fn begin(engine: &Engine, ns: NamespaceId, model: Model) -> Self {
        Self {
            ns,
            branch: engine
                .begin(engine.head(), None)
                .expect("begin on the current head"),
            model,
        }
    }

    fn create(&mut self, key_text: &str, cells: Vec<(&str, Cell)>) -> RecordId {
        let cells = named(cells);
        let op = WriteOp::CreateRecord {
            namespace: self.ns,
            key: key(key_text),
            cells: cells.clone(),
        };
        let executed = self
            .branch
            .execute(ALICE, op, Budget::unlimited())
            .unwrap_or_else(|rejected| panic!("create {key_text}: {rejected}"));
        let expected = self.model.create(key(key_text), cells);
        assert_eq!(executed.effect, Effect::RecordCreated { id: expected });
        self.check_invariants();
        expected
    }

    fn patch(&mut self, key_text: &str, set: Vec<(&str, Cell)>, remove: &[&str]) -> RecordId {
        let set = named(set);
        let remove: Vec<CellName> = remove.iter().map(|text| name(text)).collect();
        let op = WriteOp::PatchRecord {
            namespace: self.ns,
            key: key(key_text),
            set: set.clone(),
            remove: remove.clone(),
        };
        let executed = self
            .branch
            .execute(ALICE, op, Budget::unlimited())
            .unwrap_or_else(|rejected| panic!("patch {key_text}: {rejected}"));
        let expected = self.model.patch(&key(key_text), set, &remove);
        assert_eq!(executed.effect, Effect::RecordPatched { id: expected });
        self.check_invariants();
        expected
    }

    fn delete(&mut self, key_text: &str) -> RecordId {
        let op = WriteOp::DeleteRecord {
            namespace: self.ns,
            key: key(key_text),
        };
        let executed = self
            .branch
            .execute(ALICE, op, Budget::unlimited())
            .unwrap_or_else(|rejected| panic!("delete {key_text}: {rejected}"));
        let expected = self.model.delete(&key(key_text));
        assert_eq!(executed.effect, Effect::RecordDeleted { id: expected });
        self.check_invariants();
        expected
    }

    fn check_invariants(&self) {
        assert_eq!(self.branch.index_consistent(self.ns), Some(true));
        assert_eq!(
            self.branch.typed_record_count(self.ns),
            Some(self.model.live.len())
        );
        assert_eq!(
            self.branch.term_count(self.ns),
            Some(self.model.term_count())
        );
    }
}

/// Snapshot 1 holds namespace 1 (owned by ALICE) with the 45 fixture
/// records; the returned model mirrors it.
fn semantics_fixture() -> (Engine, NamespaceId, Model) {
    let mut engine = Engine::new(PriceSchedule::FIXTURE);
    let ns = ns_id(1);
    let mut world = World::begin(&engine, ns, Model::new());
    let created = world
        .branch
        .execute(
            ALICE,
            WriteOp::CreateNamespace {
                name: arkiv_db::NamespaceName::new("test").unwrap(),
            },
            Budget::unlimited(),
        )
        .expect("namespace creation applies");
    assert_eq!(created.effect, Effect::NamespaceCreated(ns));
    for (key_text, cells) in fixture_records() {
        world.create(key_text, cells);
    }
    assert_eq!(world.model.live.len(), FIXTURE_SIZE);
    let published = engine
        .publish(&world.branch.seal())
        .expect("the fixture publishes on genesis");
    assert_eq!(published.sequence(), 1);
    (engine, ns, world.model)
}

// ---------------------------------------------------------------------------
// Query generation: every single-literal term over the value universe, both
// polarities, plus deterministic AND / OR combinations.
// ---------------------------------------------------------------------------

/// `q` never occurs in any record.
const NAMES: [&str; 4] = ["n", "m", "k", "q"];
/// Present and absent text values: the empty string, a chain of prefixes,
/// non-ASCII text, the largest code point, a case variant, and an absent one.
const STRS: [&str; 12] = [
    "",
    "a",
    "ab",
    "abc",
    "abcd",
    "b",
    "ä",
    "äb",
    "aä",
    "\u{10FFFF}",
    "A",
    "zzz",
];
const PREFIXES: [&str; 12] = [
    "",
    "a",
    "ab",
    "abc",
    "abcd",
    "b",
    "ä",
    "äb",
    "aä",
    "Ä",
    "z",
    "\u{10FFFF}",
];
const I64S: [i64; 9] = [i64::MIN, i64::MIN + 1, -2, -1, 0, 1, 2, 7, i64::MAX];
const U64S: [u64; 6] = [0, 1, 2, 7, u64::MAX - 1, u64::MAX];
const INDEXABLE: [TypeTag; 4] = [TypeTag::Bool, TypeTag::I64, TypeTag::U64, TypeTag::Str];

fn universe_values() -> Vec<Value> {
    let mut values: Vec<Value> = STRS.iter().map(|text| s(text)).collect();
    values.extend(I64S.iter().map(|&value| Value::I64(value)));
    values.extend(U64S.iter().map(|&value| Value::U64(value)));
    values.extend([Value::Bool(true), Value::Bool(false)]);
    values
}

/// Every (Included, Excluded) combination over every ordered pair of the
/// bound values, including `lower == upper` and `lower > upper`, plus every
/// one-sided range.
fn all_bound_pairs(values: &[Value]) -> Vec<(Bound<Value>, Bound<Value>)> {
    let mut pairs = Vec::new();
    for low in values {
        for high in values {
            pairs.push((Bound::Included(low.clone()), Bound::Included(high.clone())));
            pairs.push((Bound::Included(low.clone()), Bound::Excluded(high.clone())));
            pairs.push((Bound::Excluded(low.clone()), Bound::Included(high.clone())));
            pairs.push((Bound::Excluded(low.clone()), Bound::Excluded(high.clone())));
        }
        pairs.push((Bound::Unbounded, Bound::Included(low.clone())));
        pairs.push((Bound::Unbounded, Bound::Excluded(low.clone())));
        pairs.push((Bound::Included(low.clone()), Bound::Unbounded));
        pairs.push((Bound::Excluded(low.clone()), Bound::Unbounded));
    }
    pairs
}

/// A short, fixed set of ranges for the per-mutation checks.
fn few_bound_pairs() -> Vec<(Bound<Value>, Bound<Value>)> {
    vec![
        (Bound::Unbounded, Bound::Included(Value::I64(0))),
        (
            Bound::Excluded(Value::I64(-1)),
            Bound::Included(Value::I64(7)),
        ),
        (
            Bound::Included(Value::I64(0)),
            Bound::Excluded(Value::I64(0)),
        ),
        (
            Bound::Included(Value::I64(i64::MIN)),
            Bound::Included(Value::I64(i64::MAX)),
        ),
        (Bound::Excluded(Value::I64(7)), Bound::Unbounded),
        (
            Bound::Included(Value::I64(7)),
            Bound::Included(Value::I64(1)),
        ),
        (Bound::Unbounded, Bound::Excluded(Value::U64(u64::MAX))),
        (Bound::Included(Value::U64(u64::MAX)), Bound::Unbounded),
        (
            Bound::Excluded(Value::U64(0)),
            Bound::Included(Value::U64(7)),
        ),
        (
            Bound::Included(Value::U64(7)),
            Bound::Included(Value::U64(7)),
        ),
    ]
}

fn single_terms(full: bool) -> Vec<Term> {
    let values = universe_values();
    let i64_bounds: Vec<Value> = I64S.iter().map(|&value| Value::I64(value)).collect();
    let u64_bounds: Vec<Value> = U64S.iter().map(|&value| Value::U64(value)).collect();
    let mut terms = Vec::new();
    for text in NAMES {
        let name = name(text);
        for value in &values {
            terms.push(Term::eq(name.clone(), value.clone()));
            terms.push(Term::not_eq(name.clone(), value.clone()));
        }
        for tag in INDEXABLE {
            terms.push(Term::exists(name.clone(), tag));
        }
        for prefix in PREFIXES {
            terms.push(Term::starts_with(name.clone(), prefix));
        }
        let pairs = if full {
            let mut pairs = all_bound_pairs(&i64_bounds);
            pairs.extend(all_bound_pairs(&u64_bounds));
            pairs
        } else {
            few_bound_pairs()
        };
        for (lower, upper) in pairs {
            terms.push(Term::range(name.clone(), lower, upper));
        }
    }
    terms
}

/// Every term as a positive and as a negated literal.
fn literal_pool(full: bool) -> Vec<Literal> {
    single_terms(full)
        .into_iter()
        .flat_map(|term| [Literal::of(term.clone()), Literal::not(term)])
        .collect()
}

fn single_literal_queries(full: bool) -> Vec<Query> {
    literal_pool(full)
        .into_iter()
        .map(|literal| Query::all(vec![literal]))
        .collect()
}

/// xorshift64 with a fixed seed: the combination set is a pure function of
/// the pool order.
struct Generator(u64);

impl Generator {
    fn next(&mut self) -> u64 {
        let mut state = self.0;
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        self.0 = state;
        state
    }

    fn below(&mut self, bound: usize) -> usize {
        let bound = u64::try_from(bound).expect("bounds fit u64");
        usize::try_from(self.next() % bound).expect("remainders fit usize")
    }

    fn group(&mut self, pool: &[Literal], size: usize) -> Vec<Literal> {
        (0..size)
            .map(|_| pool[self.below(pool.len())].clone())
            .collect()
    }
}

fn combined_queries(pool: &[Literal]) -> Vec<Query> {
    let mut generator = Generator(0x2545_f491_4f6c_dd1d);
    let mut queries = Vec::new();
    for _ in 0..300 {
        queries.push(Query::all(generator.group(pool, 2)));
    }
    for _ in 0..200 {
        queries.push(Query::all(generator.group(pool, 3)));
    }
    for _ in 0..200 {
        let first_size = 1 + generator.below(2);
        let first = generator.group(pool, first_size);
        let second_size = 1 + generator.below(2);
        let second = generator.group(pool, second_size);
        queries.push(Query {
            groups: vec![first, second],
        });
    }
    for round in 0..100 {
        let first = generator.group(pool, 2);
        let middle = if round % 5 == 0 {
            Vec::new()
        } else {
            generator.group(pool, 1)
        };
        let last_size = 1 + generator.below(3);
        let last = generator.group(pool, last_size);
        queries.push(Query {
            groups: vec![first, middle, last],
        });
    }
    for _ in 0..50 {
        let group = generator.group(pool, 2);
        queries.push(Query {
            groups: vec![group.clone(), group],
        });
    }
    // Contradictions, tautologies and the empty group in every position.
    let samples = [
        (s("a"), TypeTag::Str),
        (Value::I64(7), TypeTag::I64),
        (Value::U64(7), TypeTag::U64),
        (Value::Bool(true), TypeTag::Bool),
    ];
    for text in NAMES {
        let name = name(text);
        for (value, tag) in &samples {
            let eq = Term::eq(name.clone(), value.clone());
            let not_eq = Term::not_eq(name.clone(), value.clone());
            let exists = Term::exists(name.clone(), *tag);
            queries.push(Query::all(vec![
                Literal::of(eq.clone()),
                Literal::of(not_eq),
            ]));
            queries.push(Query::all(vec![
                Literal::of(eq.clone()),
                Literal::not(eq.clone()),
            ]));
            queries.push(Query {
                groups: vec![
                    vec![Literal::of(eq.clone())],
                    vec![Literal::not(eq.clone())],
                ],
            });
            queries.push(Query {
                groups: vec![
                    vec![Literal::of(exists.clone())],
                    vec![Literal::not(exists.clone())],
                ],
            });
            queries.push(Query {
                groups: vec![Vec::new(), vec![Literal::of(eq.clone())]],
            });
            queries.push(Query {
                groups: vec![vec![Literal::of(eq)], Vec::new()],
            });
        }
    }
    queries.push(Query::match_all());
    queries
}

// ---------------------------------------------------------------------------
// Page walking and comparison with the model
// ---------------------------------------------------------------------------

/// A read handle: a snapshot or a branch.
trait Reader {
    fn page(
        &self,
        ns: NamespaceId,
        query: &Query,
        projection: &Projection,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> ReadResult<QueryPage>;

    fn total(&self, ns: NamespaceId, query: &Query) -> ReadResult<u64>;

    /// The snapshot a cursor of this reader must name.
    fn pinned(&self) -> Option<SnapshotId>;
}

impl Reader for Snapshot {
    fn page(
        &self,
        ns: NamespaceId,
        query: &Query,
        projection: &Projection,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> ReadResult<QueryPage> {
        self.query(ns, query, projection, limit, cursor, Budget::unlimited())
    }

    fn total(&self, ns: NamespaceId, query: &Query) -> ReadResult<u64> {
        self.count(ns, query, Budget::unlimited())
    }

    fn pinned(&self) -> Option<SnapshotId> {
        Some(self.id())
    }
}

impl Reader for Branch {
    fn page(
        &self,
        ns: NamespaceId,
        query: &Query,
        projection: &Projection,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> ReadResult<QueryPage> {
        self.query(ns, query, projection, limit, cursor, Budget::unlimited())
    }

    fn total(&self, ns: NamespaceId, query: &Query) -> ReadResult<u64> {
        self.count(ns, query, Budget::unlimited())
    }

    fn pinned(&self) -> Option<SnapshotId> {
        None
    }
}

/// Walks every page of `query`, page `i` with limit `limits[i % len]`, and
/// checks the paging contract on the way: ids ascend past the cursor, a
/// continued page is never empty, `next` is present only on a full page and
/// names the last row and the request it continues.
fn walk<R: Reader>(
    reader: &R,
    ns: NamespaceId,
    query: &Query,
    projection: &Projection,
    limits: &[usize],
) -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::new();
    let mut cursor: Option<Cursor> = None;
    for page_index in 0.. {
        assert!(page_index < 1_000, "runaway walk of {query:?}");
        let limit = limits[page_index % limits.len()];
        let page = reader
            .page(ns, query, projection, limit, cursor.as_ref())
            .unwrap_or_else(|rejected| panic!("{query:?} page {page_index}: {rejected}"))
            .value;
        assert!(page.rows.len() <= limit, "{query:?}: page over limit");
        let mut last = cursor.as_ref().map(Cursor::after);
        if cursor.is_some() {
            assert!(
                !page.rows.is_empty(),
                "{query:?}: an issued cursor led to an empty page"
            );
        }
        for row in &page.rows {
            assert!(
                last.is_none_or(|after| row.id > after),
                "{query:?}: ids not ascending"
            );
            last = Some(row.id);
        }
        let page_size = page.rows.len();
        rows.extend(page.rows);
        let Some(next) = page.next else {
            return rows;
        };
        assert_eq!(
            page_size, limit,
            "{query:?}: next on a page that is not full"
        );
        assert_eq!(Some(next.after()), last);
        assert_eq!(next.namespace(), ns);
        assert_eq!(next.query(), query);
        assert_eq!(next.projection(), projection);
        assert_eq!(next.snapshot(), reader.pinned());
        cursor = Some(next);
    }
    unreachable!("the walk returns from inside the loop")
}

/// Rows and count of `query` on the engine equal the model's full scan.
fn check_query<R: Reader>(
    reader: &R,
    model: &Model,
    ns: NamespaceId,
    query: &Query,
    projection: &Projection,
    limits: &[usize],
) {
    let expected_ids = model.matches(query);
    let expected_rows: Vec<Row> = expected_ids
        .iter()
        .map(|&id| model.row(id, projection))
        .collect();
    let rows = walk(reader, ns, query, projection, limits);
    assert_eq!(
        ordinals(&rows),
        expected_ids
            .iter()
            .map(|id| id.ordinal())
            .collect::<Vec<_>>(),
        "{query:?} with limits {limits:?}"
    );
    assert_eq!(rows, expected_rows, "{query:?}: projected rows");
    let count = reader
        .total(ns, query)
        .unwrap_or_else(|rejected| panic!("count {query:?}: {rejected}"))
        .value;
    assert_eq!(
        count,
        u64::try_from(expected_ids.len()).expect("counts fit u64"),
        "count {query:?}"
    );
}

const LIMIT_PATTERNS: [&[usize]; 4] = [&[1], &[2, 3], &[5, 1, 200], &[200]];

fn check_queries<R: Reader>(reader: &R, model: &Model, ns: NamespaceId, queries: &[Query]) {
    for (index, query) in queries.iter().enumerate() {
        let limits = LIMIT_PATTERNS[index % LIMIT_PATTERNS.len()];
        check_query(reader, model, ns, query, &Projection::ALL, limits);
    }
}

/// Ids of every match on one page of 200 (the fixture never exceeds it).
fn ids_of<R: Reader>(reader: &R, ns: NamespaceId, query: &Query) -> Vec<u64> {
    let page = reader
        .page(ns, query, &Projection::ID_ONLY, 200, None)
        .unwrap_or_else(|rejected| panic!("{query:?}: {rejected}"))
        .value;
    assert!(page.next.is_none(), "{query:?}: more than 200 matches");
    ordinals(&page.rows)
}

fn eq(text: &str, value: Value) -> Query {
    Query::term(Term::eq(name(text), value))
}

// ---------------------------------------------------------------------------
// Semantics against the reference
// ---------------------------------------------------------------------------

#[test]
fn every_single_literal_matches_the_full_scan_reference() {
    let (engine, ns, model) = semantics_fixture();
    let snapshot = engine.head_snapshot();
    let queries = single_literal_queries(true);
    assert!(queries.len() > 4_000, "the generated set is large");
    check_queries(&snapshot, &model, ns, &queries);
}

#[test]
fn and_groups_and_or_of_groups_match_the_full_scan_reference() {
    let (engine, ns, model) = semantics_fixture();
    let snapshot = engine.head_snapshot();
    let queries = combined_queries(&literal_pool(true));
    check_queries(&snapshot, &model, ns, &queries);
}

#[test]
fn a_branch_answers_like_the_snapshot_it_was_begun_from() {
    let (engine, ns, model) = semantics_fixture();
    let branch = engine
        .begin(engine.head(), None)
        .expect("begin on the current head");
    check_queries(&branch, &model, ns, &single_literal_queries(false));
    check_queries(&branch, &model, ns, &combined_queries(&literal_pool(false)));
}

#[test]
fn hand_picked_operator_cases_hold() {
    let (engine, ns, _) = semantics_fixture();
    let snapshot = engine.head_snapshot();
    let ids = |query: Query| ids_of(&snapshot, ns, &query);
    let n = || name("n");

    // Eq on every type of the same name; fields (r27..r31) never match.
    assert_eq!(ids(eq("n", s("a"))), [2, 10, 39]);
    assert_eq!(ids(eq("n", Value::I64(0))), [13, 17]);
    assert_eq!(ids(eq("n", Value::U64(7))), [21, 23]);
    assert_eq!(ids(eq("n", Value::Bool(true))), [24, 26]);
    assert_eq!(ids(eq("n", s(""))), [1]);
    assert_eq!(ids(eq("n", Value::I64(i64::MIN))), [11]);
    assert_eq!(ids(eq("n", Value::U64(u64::MAX))), [22]);
    assert_eq!(ids(eq("n", s("\u{10FFFF}"))), [44]);
    assert_eq!(ids(eq("n", s("A"))), [] as [u64; 0]);
    assert_eq!(ids(eq("q", s("a"))), [] as [u64; 0]);
    // Same numeric value, other type: absent.
    assert_eq!(ids(eq("n", Value::I64(7))), [15, 18, 37]);
    assert_eq!(ids(eq("n", Value::U64(0))), [19, 43]);

    // NotEq needs an attribute of the value's type; other types, fields and
    // records lacking the name do not match.
    assert_eq!(
        ids(Query::term(Term::not_eq(n(), Value::Bool(true)))),
        [25, 40]
    );
    assert_eq!(
        ids(Query::term(Term::not_eq(n(), Value::U64(7)))),
        [19, 20, 22, 38, 43]
    );
    // Its negation is the complement: everything else, fields included.
    assert_eq!(
        ids(Query::all(vec![Literal::not(Term::not_eq(
            n(),
            Value::U64(7)
        ))]))
        .len(),
        FIXTURE_SIZE - 5
    );

    // Range: every bound combination on the same endpoints; lower > upper
    // is empty; extremes are ordinary values.
    let range =
        |lower: Bound<Value>, upper: Bound<Value>| ids(Query::term(Term::range(n(), lower, upper)));
    let (inc, exc) = (
        |value: i64| Bound::Included(Value::I64(value)),
        |value: i64| Bound::Excluded(Value::I64(value)),
    );
    assert_eq!(range(inc(-1), inc(1)), [12, 13, 14, 17, 42]);
    assert_eq!(range(exc(-1), inc(1)), [13, 14, 17, 42]);
    assert_eq!(range(inc(-1), exc(1)), [12, 13, 17]);
    assert_eq!(range(exc(-1), exc(1)), [13, 17]);
    assert_eq!(range(inc(1), inc(-1)), [] as [u64; 0]);
    assert_eq!(range(exc(0), exc(0)), [] as [u64; 0]);
    assert_eq!(range(inc(0), inc(0)), [13, 17]);
    assert_eq!(range(Bound::Unbounded, exc(0)), [11, 12]);
    assert_eq!(range(inc(i64::MAX), Bound::Unbounded), [16]);
    assert_eq!(range(Bound::Unbounded, inc(i64::MIN)), [11]);
    assert_eq!(
        range(
            Bound::Excluded(Value::U64(0)),
            Bound::Included(Value::U64(u64::MAX))
        ),
        [20, 21, 22, 23, 38]
    );
    assert_eq!(
        range(Bound::Unbounded, Bound::Excluded(Value::U64(u64::MAX))),
        [19, 20, 21, 23, 38, 43]
    );

    // StartsWith: empty prefix is every Str attribute; a prefix that is also
    // a value; non-ASCII prefixes; no other type matches.
    let prefix = |prefix: &str| ids(Query::term(Term::starts_with(n(), prefix)));
    assert_eq!(
        prefix(""),
        [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 36, 39, 41, 44, 45]
    );
    assert_eq!(prefix("a"), [2, 3, 4, 8, 9, 10, 36, 39, 41, 45]);
    assert_eq!(prefix("ab"), [3, 4, 9, 36, 41]);
    assert_eq!(prefix("abc"), [4, 9, 36]);
    assert_eq!(prefix("abcd"), [] as [u64; 0]);
    assert_eq!(prefix("ä"), [6, 7]);
    assert_eq!(prefix("aä"), [8, 45]);
    assert_eq!(prefix("Ä"), [] as [u64; 0]);

    // Exists per type; the Bytes field of r31 is nothing to any query.
    let exists = |tag: TypeTag| ids(Query::term(Term::exists(n(), tag)));
    assert_eq!(exists(TypeTag::Str).len(), 15);
    assert_eq!(
        exists(TypeTag::I64),
        [11, 12, 13, 14, 15, 16, 17, 18, 37, 42]
    );
    assert_eq!(exists(TypeTag::U64), [19, 20, 21, 22, 23, 38, 43]);
    assert_eq!(exists(TypeTag::Bool), [24, 25, 26, 40]);
    assert_eq!(
        ids_of(&snapshot, ns, &Query::match_all()).len(),
        FIXTURE_SIZE
    );
}

#[test]
fn projections_return_only_what_they_name() {
    let (engine, ns, model) = semantics_fixture();
    let snapshot = engine.head_snapshot();
    let queries = [
        eq("n", s("a")),
        Query::term(Term::exists(name("k"), TypeTag::Str)),
        Query::term(Term::exists(name("m"), TypeTag::I64)),
        Query::match_all(),
    ];
    let projections = [
        Projection::ID_ONLY,
        Projection::KEY_ONLY,
        Projection::named(false, [name("n"), name("q")]),
        Projection::named(true, [name("k")]),
        Projection::named(true, []),
    ];
    for query in &queries {
        for projection in &projections {
            check_query(&snapshot, &model, ns, query, projection, &[3, 200]);
        }
    }
    // A named projection skips names the record lacks without charging them.
    let page = snapshot
        .query(
            ns,
            &eq("n", s("only")),
            &Projection::named(true, [name("n"), name("q")]),
            5,
            None,
            Budget::unlimited(),
        )
        .expect("a valid query")
        .value;
    assert!(page.rows.is_empty());
    let row = snapshot
        .get_record(
            ns,
            &key("r35"),
            &Projection::named(false, [name("k"), name("n")]),
            Budget::unlimited(),
        )
        .expect("a valid read")
        .value
        .expect("r35 is live");
    assert_eq!(
        row,
        Row {
            id: id(35),
            key: None,
            cells: vec![(name("k"), attr(s("only")))],
        }
    );
}

// ---------------------------------------------------------------------------
// Mutations: records move between indexes and disappear
// ---------------------------------------------------------------------------

#[test]
fn patches_and_deletes_move_records_between_indexes() {
    let (mut engine, ns, model) = semantics_fixture();
    let original = model.clone();
    let fixture_snapshot = engine.head();
    let mut world = World::begin(&engine, ns, model);
    let probes = single_literal_queries(false);
    let n = || name("n");
    let m = || name("m");

    // Type change: I64 7 -> U64 7 leaves one posting and joins another.
    assert_eq!(
        ids_of(&world.branch, ns, &eq("n", Value::I64(7))),
        [15, 18, 37]
    );
    world.patch("r15", vec![("n", attr(Value::U64(7)))], &[]);
    assert_eq!(ids_of(&world.branch, ns, &eq("n", Value::I64(7))), [18, 37]);
    assert_eq!(
        ids_of(&world.branch, ns, &eq("n", Value::U64(7))),
        [15, 21, 23]
    );
    assert_eq!(
        ids_of(
            &world.branch,
            ns,
            &Query::term(Term::exists(n(), TypeTag::U64))
        ),
        [15, 19, 20, 21, 22, 23, 38, 43]
    );
    check_queries(&world.branch, &world.model, ns, &probes);

    // Attribute -> field: the record keeps the value but leaves the index.
    world.patch("r02", vec![("n", field(s("a")))], &[]);
    assert_eq!(ids_of(&world.branch, ns, &eq("n", s("a"))), [10, 39]);
    // Field -> attribute: it joins.
    world.patch("r27", vec![("n", attr(s("a")))], &[]);
    assert_eq!(ids_of(&world.branch, ns, &eq("n", s("a"))), [10, 27, 39]);
    check_queries(&world.branch, &world.model, ns, &probes);

    // Removal and addition of a name.
    world.patch("r03", vec![], &["n"]);
    assert_eq!(
        ids_of(
            &world.branch,
            ns,
            &Query::term(Term::starts_with(n(), "ab"))
        ),
        [4, 9, 36, 41]
    );
    world.patch("r32", vec![("n", attr(Value::Bool(true)))], &[]);
    assert_eq!(
        ids_of(
            &world.branch,
            ns,
            &Query::term(Term::exists(n(), TypeTag::Bool))
        ),
        [24, 25, 26, 32, 40]
    );
    check_queries(&world.branch, &world.model, ns, &probes);

    // A patch touching two names: m changes type, k is removed.
    world.patch("r36", vec![("m", attr(s("abc")))], &["k"]);
    assert_eq!(
        ids_of(&world.branch, ns, &eq("m", Value::I64(7))),
        [15, 26, 45]
    );
    assert_eq!(
        ids_of(&world.branch, ns, &eq("m", s("abc"))),
        [12, 32, 36, 37]
    );
    assert_eq!(
        ids_of(
            &world.branch,
            ns,
            &Query::term(Term::exists(name("k"), TypeTag::Bool))
        ),
        [] as [u64; 0]
    );
    // Index no-ops: the same value again, a new field, an absent removal.
    let terms_before = world.branch.term_count(ns);
    world.patch("r14", vec![("m", attr(Value::I64(0)))], &[]);
    world.patch("r44", vec![("k", field(Value::bytes(b"\xff")))], &["q"]);
    assert_eq!(world.branch.term_count(ns), terms_before);
    check_queries(&world.branch, &world.model, ns, &probes);

    // Deletes: the records vanish from every source. r22 held the only
    // `n = U64 MAX`, so that value term is dropped, while its existence
    // term and r22's `m = I64 -1` (shared with r03 and r42) survive.
    assert_eq!(
        ids_of(&world.branch, ns, &eq("m", Value::I64(i64::MAX))),
        [5, 44]
    );
    world.delete("r05");
    assert_eq!(ids_of(&world.branch, ns, &eq("n", s("b"))), [] as [u64; 0]);
    assert_eq!(
        ids_of(&world.branch, ns, &eq("m", Value::I64(i64::MAX))),
        [44]
    );
    let terms_before = world.branch.term_count(ns);
    world.delete("r22");
    assert_eq!(
        world.branch.term_count(ns),
        terms_before.map(|count| count - 1)
    );
    assert_eq!(
        ids_of(&world.branch, ns, &eq("n", Value::U64(u64::MAX))),
        [] as [u64; 0]
    );
    assert_eq!(
        ids_of(
            &world.branch,
            ns,
            &Query::term(Term::exists(n(), TypeTag::U64))
        ),
        [15, 19, 20, 21, 23, 38, 43]
    );
    world.delete("r12");
    assert_eq!(
        ids_of(
            &world.branch,
            ns,
            &Query::term(Term::exists(m(), TypeTag::Str))
        ),
        [8, 11, 13, 24, 25, 28, 32, 36, 37, 39]
    );
    assert_eq!(
        world
            .branch
            .get_record(ns, &key("r12"), &Projection::ALL, Budget::unlimited())
            .expect("a valid read")
            .value,
        None
    );
    check_queries(&world.branch, &world.model, ns, &probes);

    // A recreated key gets a fresh id at the end of every order.
    assert_eq!(
        world.create(
            "r05",
            vec![("n", attr(s("b"))), ("m", attr(Value::U64(u64::MAX)))]
        ),
        id(46)
    );
    assert_eq!(ids_of(&world.branch, ns, &eq("n", s("b"))), [46]);
    assert_eq!(
        world.create(
            "r46",
            vec![
                ("n", attr(Value::U64(u64::MAX))),
                ("m", attr(Value::I64(i64::MIN))),
                ("k", attr(s(""))),
            ]
        ),
        id(47)
    );
    world.patch(
        "r18",
        vec![("n", attr(s("7"))), ("m", attr(Value::I64(7)))],
        &[],
    );
    assert_eq!(ids_of(&world.branch, ns, &eq("n", Value::I64(7))), [37]);
    assert_eq!(
        ids_of(&world.branch, ns, &eq("m", Value::I64(7))),
        [15, 18, 26, 45]
    );
    check_queries(
        &world.branch,
        &world.model,
        ns,
        &single_literal_queries(true),
    );
    check_queries(
        &world.branch,
        &world.model,
        ns,
        &combined_queries(&literal_pool(false)),
    );

    // Published, the state answers the same; the earlier snapshot is intact.
    let World { branch, model, .. } = world;
    let published = engine
        .publish(&branch.seal())
        .expect("the mutation block publishes on snapshot 1");
    let after = engine.snapshot(published).expect("just published");
    assert_eq!(after.index_consistent(ns), Some(true));
    assert_eq!(after.typed_record_count(ns), Some(model.live.len()));
    check_queries(&after, &model, ns, &single_literal_queries(true));
    let first = engine
        .snapshot(fixture_snapshot)
        .expect("snapshot 1 is retained");
    assert_eq!(first.typed_record_count(ns), Some(FIXTURE_SIZE));
    check_queries(&first, &original, ns, &probes);
}

#[test]
fn deleted_records_disappear_and_a_rollback_brings_them_back() {
    let (engine, ns, model) = semantics_fixture();
    let mut world = World::begin(&engine, ns, model.clone());
    let query = eq("n", s("b"));
    let checkpoint = world.branch.checkpoint();

    world.delete("r05");
    world.delete("r44");
    assert_eq!(ids_of(&world.branch, ns, &query), [] as [u64; 0]);
    assert_eq!(
        world
            .branch
            .get_record(ns, &key("r05"), &Projection::KEY_ONLY, Budget::unlimited())
            .expect("a valid read")
            .value,
        None
    );
    assert_eq!(
        world
            .branch
            .count(ns, &Query::match_all(), Budget::unlimited())
            .expect("a valid count")
            .value,
        43
    );

    // Rolling back restores the rows, the bindings and the index together.
    world
        .branch
        .rollback_to(checkpoint)
        .expect("the checkpoint is valid");
    world.model = model;
    world.check_invariants();
    assert_eq!(ids_of(&world.branch, ns, &query), [5]);
    assert_eq!(
        world
            .branch
            .get_record(ns, &key("r05"), &Projection::KEY_ONLY, Budget::unlimited())
            .expect("a valid read")
            .value,
        Some(Row {
            id: id(5),
            key: Some(key("r05")),
            cells: Vec::new(),
        })
    );
    check_queries(
        &world.branch,
        &world.model,
        ns,
        &single_literal_queries(false),
    );
    // A key recreated after the rollback still gets a fresh id: the
    // allocator was restored to 46, not to something lower.
    assert_eq!(world.create("r47", vec![("n", attr(s("b")))]), id(46));
    assert_eq!(ids_of(&world.branch, ns, &query), [5, 46]);
}

// ---------------------------------------------------------------------------
// Validation order of every QueryFault (spent 0, receipt issued)
// ---------------------------------------------------------------------------

/// Small limits so every fault is a few literals away.
const SMALL: Limits = Limits {
    max_cells: 3,
    max_value_len: 4,
    max_page: 5,
    max_groups: 2,
    max_literals: 2,
    ..Limits::DEFAULT
};

/// Snapshot 1 of a SMALL-limits engine: namespace 1 with one record.
fn small_fixture() -> (Engine, NamespaceId) {
    let mut engine = Engine::with_limits(PriceSchedule::FIXTURE, SMALL);
    assert_eq!(engine.limits(), SMALL);
    let ns = ns_id(1);
    let mut branch = engine
        .begin(engine.head(), None)
        .expect("genesis is the head");
    branch
        .execute(
            ALICE,
            WriteOp::CreateNamespace {
                name: arkiv_db::NamespaceName::new("test").unwrap(),
            },
            Budget::unlimited(),
        )
        .expect("namespace creation applies");
    branch
        .execute(
            ALICE,
            WriteOp::CreateRecord {
                namespace: ns,
                key: key("one"),
                cells: named(vec![("n", attr(s("abcd"))), ("m", attr(Value::I64(1)))]),
            },
            Budget::unlimited(),
        )
        .expect("the record fits the small limits");
    engine
        .publish(&branch.seal())
        .expect("the fixture publishes on genesis");
    (engine, ns)
}

fn zero_cost_read(reason: Rejection) -> Receipt {
    read_receipt(0, Outcome::Rejected(reason))
}

fn expect_fault<T: Debug>(result: ReadResult<T>, fault: QueryFault) {
    match result {
        Ok(metered) => panic!("expected {fault:?}, got {:?}", metered.value),
        Err(rejected) => {
            assert_eq!(rejected.reason, Rejection::InvalidQuery(fault));
            assert_eq!(
                rejected.receipt,
                zero_cost_read(Rejection::InvalidQuery(fault)),
                "{fault:?}: spent 0, receipt issued"
            );
        }
    }
}

fn lit(term: Term) -> Literal {
    Literal::of(term)
}

#[test]
fn query_faults_are_reported_in_the_validation_order_before_any_charge() {
    let (engine, ns) = small_fixture();
    let snapshot = engine.head_snapshot();
    let n = || name("n");
    let all = Projection::ALL;
    let query = |query: &Query, projection: &Projection, limit: usize| {
        snapshot.query(ns, query, projection, limit, None, Budget::unlimited())
    };
    // Faulty building blocks, each also carrying every later fault.
    let bytes_eq = || lit(Term::eq(n(), Value::bytes(b"x")));
    let long_eq = || lit(Term::eq(n(), s("abcde")));
    let unbounded = || lit(Term::range(n(), Bound::Unbounded, Bound::Unbounded));
    let too_wide = Projection::named(true, [name("a"), name("b"), name("c"), name("d")]);
    let wide_ok = Projection::named(true, [name("a"), name("b"), name("c")]);

    // 1. NoGroups masks a zero limit and an oversized projection.
    expect_fault(
        query(&Query { groups: vec![] }, &too_wide, 0),
        QueryFault::NoGroups,
    );
    // 2. TooManyGroups masks TooManyLiterals, the limit and the literals.
    let three_of = |literal: Literal| vec![literal.clone(), literal.clone(), literal];
    expect_fault(
        query(
            &Query {
                groups: vec![three_of(bytes_eq()), three_of(unbounded()), Vec::new()],
            },
            &too_wide,
            0,
        ),
        QueryFault::TooManyGroups,
    );
    // 3. TooManyLiterals masks the limit and the literals.
    expect_fault(
        query(&Query::all(three_of(bytes_eq())), &too_wide, 0),
        QueryFault::TooManyLiterals,
    );
    // 4. ZeroLimit and 5. LimitTooLarge mask the literals and the projection.
    expect_fault(
        query(&Query::all(vec![bytes_eq()]), &too_wide, 0),
        QueryFault::ZeroLimit,
    );
    expect_fault(
        query(&Query::all(vec![bytes_eq()]), &too_wide, 6),
        QueryFault::LimitTooLarge,
    );
    assert!(
        query(&Query::match_all(), &all, 5).is_ok(),
        "max_page is inclusive"
    );
    // 6. Literals in order: the first faulty literal decides, across groups.
    expect_fault(
        query(&Query::all(vec![bytes_eq(), unbounded()]), &too_wide, 5),
        QueryFault::NotIndexable,
    );
    expect_fault(
        query(&Query::all(vec![unbounded(), bytes_eq()]), &too_wide, 5),
        QueryFault::UnboundedRange,
    );
    expect_fault(
        query(
            &Query {
                groups: vec![vec![lit(Term::eq(n(), s("abcd")))], vec![long_eq()]],
            },
            &too_wide,
            5,
        ),
        QueryFault::ValueTooLarge,
    );
    // Every literal fault on its own.
    let faults: Vec<(Term, QueryFault)> = vec![
        (Term::eq(n(), Value::bytes(b"")), QueryFault::NotIndexable),
        (
            Term::not_eq(n(), Value::bytes(b"x")),
            QueryFault::NotIndexable,
        ),
        (Term::eq(n(), s("abcde")), QueryFault::ValueTooLarge),
        // `ä` is two bytes: five bytes here, four in the accepted list below.
        (Term::not_eq(n(), s("äbcd")), QueryFault::ValueTooLarge),
        (
            Term::range(n(), Bound::Unbounded, Bound::Unbounded),
            QueryFault::UnboundedRange,
        ),
        (
            Term::range(
                n(),
                Bound::Included(Value::I64(1)),
                Bound::Included(Value::U64(1)),
            ),
            QueryFault::MixedBounds,
        ),
        (
            Term::range(
                n(),
                Bound::Excluded(Value::U64(1)),
                Bound::Excluded(Value::bytes(b"x")),
            ),
            QueryFault::MixedBounds,
        ),
        (
            Term::range(n(), Bound::Included(s("a")), Bound::Unbounded),
            QueryFault::NotOrdered,
        ),
        (
            Term::range(
                n(),
                Bound::Included(Value::Bool(false)),
                Bound::Excluded(Value::Bool(true)),
            ),
            QueryFault::NotOrdered,
        ),
        // Range bounds are never ValueTooLarge or NotIndexable.
        (
            Term::range(
                n(),
                Bound::Included(s("abcdefgh")),
                Bound::Included(s("abcdefgh")),
            ),
            QueryFault::NotOrdered,
        ),
        (
            Term::range(
                n(),
                Bound::Unbounded,
                Bound::Included(Value::bytes(b"abcdefgh")),
            ),
            QueryFault::NotOrdered,
        ),
        (Term::starts_with(n(), "abcde"), QueryFault::ValueTooLarge),
        (Term::starts_with(n(), "äbcd"), QueryFault::ValueTooLarge),
        (Term::exists(n(), TypeTag::Bytes), QueryFault::NotIndexable),
    ];
    for (term, fault) in faults {
        expect_fault(query(&Query::term(term.clone()), &too_wide, 5), fault);
        // Negation and position do not change validation.
        expect_fault(
            query(
                &Query::all(vec![
                    lit(Term::exists(n(), TypeTag::Str)),
                    Literal::not(term),
                ]),
                &all,
                5,
            ),
            fault,
        );
    }
    // Values at the limit are accepted.
    for term in [
        Term::eq(n(), s("abcd")),
        Term::not_eq(n(), s("äbc")),
        Term::starts_with(n(), "äbc"),
        Term::range(
            n(),
            Bound::Excluded(Value::U64(0)),
            Bound::Excluded(Value::U64(0)),
        ),
    ] {
        assert!(query(&Query::term(term), &wide_ok, 5).is_ok());
    }
    // 7. ProjectionTooLarge comes after every literal and before the cursor.
    let bogus = query(&Query::match_all(), &Projection::ID_ONLY, 1)
        .expect("one row, one more to come")
        .value
        .next;
    assert_eq!(bogus, None, "the small fixture has one record");
    let foreign = {
        let (other, other_ns) = small_fixture();
        let mut branch = other.begin(other.head(), None).expect("begin on the head");
        branch
            .execute(
                ALICE,
                WriteOp::CreateRecord {
                    namespace: other_ns,
                    key: key("two"),
                    cells: Vec::new(),
                },
                Budget::unlimited(),
            )
            .expect("an empty record applies");
        branch
            .query(
                other_ns,
                &Query::match_all(),
                &all,
                1,
                None,
                Budget::unlimited(),
            )
            .expect("a valid query")
            .value
            .next
            .expect("two records, one per page")
    };
    let result = snapshot.query(
        ns,
        &Query::match_all(),
        &too_wide,
        5,
        Some(&foreign),
        Budget::unlimited(),
    );
    expect_fault(result, QueryFault::ProjectionTooLarge);
    // 8. CursorMismatch is last and also costs nothing.
    let result = snapshot.query(
        ns,
        &Query::match_all(),
        &all,
        5,
        Some(&foreign),
        Budget::unlimited(),
    );
    let rejected = result.expect_err("a branch cursor of another engine");
    assert_eq!(rejected.reason, Rejection::CursorMismatch);
    assert_eq!(rejected.receipt, zero_cost_read(Rejection::CursorMismatch));

    // Validation precedes the directory lookup and ignores the budget.
    expect_fault(
        snapshot.query(
            ns_id(9),
            &Query::all(vec![bytes_eq()]),
            &all,
            5,
            None,
            Budget::limit(0),
        ),
        QueryFault::NotIndexable,
    );
    let unknown = snapshot
        .query(
            ns_id(9),
            &Query::match_all(),
            &all,
            5,
            None,
            Budget::unlimited(),
        )
        .expect_err("namespace 9 does not exist");
    assert_eq!(unknown.reason, Rejection::UnknownNamespace);
    assert_eq!(
        unknown.receipt,
        read_receipt(1, Outcome::Rejected(Rejection::UnknownNamespace)),
        "DirectoryLookup is the only charge"
    );
    // A traced zero-cost receipt carries an empty ledger.
    let rejected = snapshot
        .query(
            ns,
            &Query { groups: vec![] },
            &all,
            5,
            None,
            Budget::unlimited().traced(),
        )
        .expect_err("no groups");
    assert_eq!(
        rejected.receipt,
        traced(
            zero_cost_read(Rejection::InvalidQuery(QueryFault::NoGroups)),
            &[]
        )
    );
}

#[test]
fn count_and_get_record_validate_like_query() {
    let (engine, ns) = small_fixture();
    let snapshot = engine.head_snapshot();
    let n = || name("n");
    let count = |query: &Query| snapshot.count(ns, query, Budget::unlimited());
    let three = |literal: Literal| vec![literal.clone(), literal.clone(), literal];
    let bytes_eq = lit(Term::eq(n(), Value::bytes(b"x")));

    expect_fault(count(&Query { groups: vec![] }), QueryFault::NoGroups);
    expect_fault(
        count(&Query {
            groups: vec![three(bytes_eq.clone()), Vec::new(), Vec::new()],
        }),
        QueryFault::TooManyGroups,
    );
    expect_fault(
        count(&Query::all(three(bytes_eq.clone()))),
        QueryFault::TooManyLiterals,
    );
    // The implicit limit of 1 is valid, so the literals are next.
    expect_fault(count(&Query::all(vec![bytes_eq])), QueryFault::NotIndexable);
    expect_fault(
        count(&Query::term(Term::range(
            n(),
            Bound::Included(Value::I64(0)),
            Bound::Included(Value::U64(0)),
        ))),
        QueryFault::MixedBounds,
    );
    expect_fault(
        count(&Query::term(Term::starts_with(n(), "abcde"))),
        QueryFault::ValueTooLarge,
    );
    assert_eq!(count(&Query::match_all()).expect("a valid count").value, 1);

    // get_record checks only the projection.
    let too_wide = Projection::named(false, [name("a"), name("b"), name("c"), name("d")]);
    expect_fault(
        snapshot.get_record(ns, &key("one"), &too_wide, Budget::unlimited()),
        QueryFault::ProjectionTooLarge,
    );
    expect_fault(
        snapshot.get_record(ns_id(9), &key("one"), &too_wide, Budget::limit(0)),
        QueryFault::ProjectionTooLarge,
    );
    let wide_ok = Projection::named(false, [name("a"), name("b"), name("m")]);
    assert_eq!(
        snapshot
            .get_record(ns, &key("one"), &wide_ok, Budget::unlimited())
            .expect("three names are allowed")
            .value,
        Some(Row {
            id: id(1),
            key: None,
            cells: vec![(name("m"), attr(Value::I64(1)))],
        })
    );
}

#[test]
fn a_cursor_is_bound_to_its_context_query_and_projection() {
    let (mut engine, ns, _) = semantics_fixture();
    let first = engine.head_snapshot();
    let query = Query::term(Term::exists(name("n"), TypeTag::Str));
    let cursor = first
        .query(ns, &query, &Projection::ALL, 2, None, Budget::unlimited())
        .expect("a valid query")
        .value
        .next
        .expect("fifteen Str attributes of n");
    assert_eq!(cursor.after(), id(2));
    assert_eq!(cursor.snapshot(), Some(first.id()));
    let mismatch = |result: ReadResult<QueryPage>| {
        let rejected = result.expect_err("cursor mismatch");
        assert_eq!(rejected.reason, Rejection::CursorMismatch);
        assert_eq!(rejected.receipt, zero_cost_read(Rejection::CursorMismatch));
    };

    // The same request continues; the limit is not bound.
    let page = first
        .query(
            ns,
            &query,
            &Projection::ALL,
            5,
            Some(&cursor),
            Budget::unlimited(),
        )
        .expect("the cursor matches")
        .value;
    assert_eq!(ordinals(&page.rows), [3, 4, 5, 6, 7]);
    // Another projection, query or namespace: rejected before any lookup.
    mismatch(first.query(
        ns,
        &query,
        &Projection::ID_ONLY,
        5,
        Some(&cursor),
        Budget::unlimited(),
    ));
    mismatch(first.query(
        ns,
        &Query::match_all(),
        &Projection::ALL,
        5,
        Some(&cursor),
        Budget::unlimited(),
    ));
    mismatch(first.query(
        ns_id(9),
        &query,
        &Projection::ALL,
        5,
        Some(&cursor),
        Budget::unlimited(),
    ));
    // A branch, even one begun from the pinned snapshot.
    let mut branch = engine
        .begin(engine.head(), None)
        .expect("begin on the head");
    mismatch(branch.query(
        ns,
        &query,
        &Projection::ALL,
        5,
        Some(&cursor),
        Budget::unlimited(),
    ));
    // A branch cursor survives rejected writes, not applied ones.
    let branch_cursor = branch
        .query(ns, &query, &Projection::ALL, 2, None, Budget::unlimited())
        .expect("a valid query")
        .value
        .next
        .expect("more matches");
    assert_eq!(branch_cursor.snapshot(), None);
    mismatch(first.query(
        ns,
        &query,
        &Projection::ALL,
        5,
        Some(&branch_cursor),
        Budget::unlimited(),
    ));
    let rejected = branch.execute(
        ALICE,
        WriteOp::CreateRecord {
            namespace: ns,
            key: key("r01"),
            cells: Vec::new(),
        },
        Budget::unlimited(),
    );
    assert_eq!(
        rejected.expect_err("r01 is bound").reason,
        Rejection::AlreadyExists
    );
    assert!(
        branch
            .query(
                ns,
                &query,
                &Projection::ALL,
                5,
                Some(&branch_cursor),
                Budget::unlimited()
            )
            .is_ok()
    );
    branch
        .execute(
            ALICE,
            WriteOp::DeleteRecord {
                namespace: ns,
                key: key("r45"),
            },
            Budget::unlimited(),
        )
        .expect("r45 is live");
    mismatch(branch.query(
        ns,
        &query,
        &Projection::ALL,
        5,
        Some(&branch_cursor),
        Budget::unlimited(),
    ));
    // A later snapshot does not accept the earlier snapshot's cursor.
    let published = engine
        .publish(&branch.seal())
        .expect("publishes on snapshot 1");
    let second = engine.snapshot(published).expect("just published");
    mismatch(second.query(
        ns,
        &query,
        &Projection::ALL,
        5,
        Some(&cursor),
        Budget::unlimited(),
    ));
    assert!(
        first
            .query(
                ns,
                &query,
                &Projection::ALL,
                5,
                Some(&cursor),
                Budget::unlimited()
            )
            .is_ok()
    );
}

// ---------------------------------------------------------------------------
// Hand-derived charges on a six-record namespace
// ---------------------------------------------------------------------------

/// Snapshot 1 of a separate engine: namespace 1 with, in id order,
///
/// | id | key  | kind (Str) | price       |
/// | -- | ---- | ---------- | ----------- |
/// | 1  | `k1` | `book`     | I64 10      |
/// | 2  | `k2` | `book`     | I64 20      |
/// | 3  | `k3` | `pen`      | I64 5       |
/// | 4  | `k4` | `pen`      | I64 20      |
/// | 5  | `k5` | `ink`      | U64 20      |
/// | 6  | `k6` | (absent)   | I64 30      |
///
/// Term keys in index order: kind/Str/∃, `book`, `ink`, `pen`; price/I64/∃,
/// 5, 10, 20, 30; price/U64/∃, 20. Projected bytes of a record with both
/// cells under `Projection::ALL`: key 2 + (`kind` 4 + 4) + (`price` 5 + 8)
/// = 23.
fn cost_fixture() -> (Engine, NamespaceId) {
    let mut engine = Engine::new(PriceSchedule::FIXTURE);
    let ns = ns_id(1);
    let mut branch = engine
        .begin(engine.head(), None)
        .expect("genesis is the head");
    branch
        .execute(
            ALICE,
            WriteOp::CreateNamespace {
                name: arkiv_db::NamespaceName::new("test").unwrap(),
            },
            Budget::unlimited(),
        )
        .expect("namespace creation applies");
    let records = [
        (
            "k1",
            vec![("kind", attr(s("book"))), ("price", attr(Value::I64(10)))],
        ),
        (
            "k2",
            vec![("kind", attr(s("book"))), ("price", attr(Value::I64(20)))],
        ),
        (
            "k3",
            vec![("kind", attr(s("pen"))), ("price", attr(Value::I64(5)))],
        ),
        (
            "k4",
            vec![("kind", attr(s("pen"))), ("price", attr(Value::I64(20)))],
        ),
        (
            "k5",
            vec![("kind", attr(s("ink"))), ("price", attr(Value::U64(20)))],
        ),
        ("k6", vec![("price", attr(Value::I64(30)))]),
    ];
    for (key_text, cells) in records {
        branch
            .execute(
                ALICE,
                WriteOp::CreateRecord {
                    namespace: ns,
                    key: key(key_text),
                    cells: named(cells),
                },
                Budget::unlimited(),
            )
            .expect("fixture creates apply");
    }
    engine
        .publish(&branch.seal())
        .expect("the fixture publishes on genesis");
    assert_eq!(engine.head_snapshot().term_count(ns), Some(11));
    (engine, ns)
}

fn kind_eq(text: &str) -> Term {
    Term::eq(name("kind"), s(text))
}

fn price_range(lower: Bound<Value>, upper: Bound<Value>) -> Term {
    Term::range(name("price"), lower, upper)
}

fn inc(value: i64) -> Bound<Value> {
    Bound::Included(Value::I64(value))
}

fn exc(value: i64) -> Bound<Value> {
    Bound::Excluded(Value::I64(value))
}

/// Runs one page and returns `(ids, next.after, receipt)`.
fn page(
    snapshot: &Snapshot,
    ns: NamespaceId,
    query: &Query,
    projection: &Projection,
    limit: usize,
    cursor: Option<&Cursor>,
    budget: Budget,
) -> (Vec<u64>, Option<u64>, Receipt, Option<Cursor>) {
    let metered = snapshot
        .query(ns, query, projection, limit, cursor, budget)
        .unwrap_or_else(|rejected| panic!("{query:?}: {rejected}"));
    let after = metered
        .value
        .next
        .as_ref()
        .map(|next| next.after().ordinal());
    (
        ordinals(&metered.value.rows),
        after,
        metered.receipt,
        metered.value.next,
    )
}

#[test]
fn eq_pages_charge_the_head_beyond_the_page() {
    let (engine, ns) = cost_fixture();
    let snapshot = engine.head_snapshot();
    let query = Query::term(kind_eq("book"));
    let unlimited = Budget::unlimited();

    // Whole result: 1 + 5 + 4 + PostingEntry 2 (exhaustion is free) +
    // ResultRow 2 * 3 = 18.
    let (ids, after, receipt, _) = page(
        &snapshot,
        ns,
        &query,
        &Projection::ID_ONLY,
        10,
        None,
        unlimited,
    );
    assert_eq!((ids, after), (vec![1, 2], None));
    assert_eq!(receipt, applied(18));
    // With every cell: + ReadByte 23 per row = 64.
    let (ids, _, receipt, _) = page(
        &snapshot,
        ns,
        &query,
        &Projection::ALL,
        10,
        None,
        unlimited.traced(),
    );
    assert_eq!(ids, [1, 2]);
    assert_eq!(
        receipt,
        traced(
            applied(64),
            &[
                (OperationClass::DirectoryLookup, 1, 1, 1),
                (OperationClass::QueryGroup, 1, 5, 5),
                (OperationClass::TermSeek, 1, 4, 4),
                (OperationClass::PostingEntry, 2, 1, 2),
                (OperationClass::ResultRow, 2, 3, 6),
                (OperationClass::ReadByte, 46, 1, 46),
            ]
        )
    );
    // Page of one: the head beyond the page is pulled (PostingEntry 2) so
    // that `next` is exact: 1 + 5 + 4 + 2 + 3 = 15. The last page charges
    // only the remaining entry: 1 + 5 + 4 + 1 + 3 = 14.
    let (ids, after, receipt, cursor) = page(
        &snapshot,
        ns,
        &query,
        &Projection::ID_ONLY,
        1,
        None,
        unlimited,
    );
    assert_eq!((ids, after), (vec![1], Some(1)));
    assert_eq!(receipt, applied(15));
    let (ids, after, receipt, _) = page(
        &snapshot,
        ns,
        &query,
        &Projection::ID_ONLY,
        1,
        cursor.as_ref(),
        unlimited,
    );
    assert_eq!((ids, after), (vec![2], None));
    assert_eq!(receipt, applied(14));
    // A missing term costs the seek only: 1 + 5 + 4 = 10.
    let (ids, after, receipt, _) = page(
        &snapshot,
        ns,
        &Query::term(kind_eq("none")),
        &Projection::ALL,
        10,
        None,
        unlimited,
    );
    assert_eq!((ids, after), (vec![], None));
    assert_eq!(receipt, applied(10));
    // Exists reads the existence posting of (price, I64): 5 entries, 5 rows:
    // 1 + 5 + 4 + 5 + 15 = 30; count stops before the rows: 15.
    let exists = Query::term(Term::exists(name("price"), TypeTag::I64));
    let (ids, _, receipt, _) = page(
        &snapshot,
        ns,
        &exists,
        &Projection::ID_ONLY,
        10,
        None,
        unlimited,
    );
    assert_eq!(ids, [1, 2, 3, 4, 6]);
    assert_eq!(receipt, applied(30));
    let count = snapshot
        .count(ns, &exists, unlimited)
        .expect("a valid count");
    assert_eq!((count.value, count.receipt), (5, applied(15)));
    // Budget end anywhere: OutOfBudget with the units spent so far, no
    // partial page. 1 + 5 + 4 + 1 + 3 + 1 = 15 is spent when the second
    // ResultRow group (3) would exceed 17.
    let rejected = snapshot
        .query(
            ns,
            &query,
            &Projection::ID_ONLY,
            10,
            None,
            Budget::limit(17),
        )
        .expect_err("18 units are needed");
    assert_eq!(rejected.reason, Rejection::OutOfBudget);
    assert_eq!(
        rejected.receipt,
        read_receipt(15, Outcome::Rejected(Rejection::OutOfBudget))
    );
    let (ids, _, receipt, _) = page(
        &snapshot,
        ns,
        &query,
        &Projection::ID_ONLY,
        10,
        None,
        Budget::limit(18),
    );
    assert_eq!((ids, receipt), (vec![1, 2], applied(18)));
}

#[test]
fn range_runs_charge_only_the_qualifying_term_keys() {
    let (engine, ns) = cost_fixture();
    let snapshot = engine.head_snapshot();
    let unlimited = Budget::unlimited();
    let run = |lower: Bound<Value>, upper: Bound<Value>, budget: Budget| {
        page(
            &snapshot,
            ns,
            &Query::term(price_range(lower, upper)),
            &Projection::ID_ONLY,
            10,
            None,
            budget,
        )
    };

    // Excluded lower on the existing key 10 (stepped over, uncharged); 20
    // qualifies (TermVisit 1); the run ends on the existing key 30, which is
    // not charged: 1 + 5 + 4 + 3 + PostingEntry 2 + ResultRow 6 = 21.
    let (ids, after, receipt, _) = run(exc(10), inc(20), unlimited.traced());
    assert_eq!((ids, after), (vec![2, 4], None));
    assert_eq!(
        receipt,
        traced(
            applied(21),
            &[
                (OperationClass::DirectoryLookup, 1, 1, 1),
                (OperationClass::QueryGroup, 1, 5, 5),
                (OperationClass::TermSeek, 1, 4, 4),
                (OperationClass::TermVisit, 1, 3, 3),
                (OperationClass::PostingEntry, 2, 1, 2),
                (OperationClass::ResultRow, 2, 3, 6),
            ]
        )
    );
    // Both endpoints excluded around one gap: an empty run, seek only.
    let (ids, _, receipt, _) = run(exc(10), exc(20), unlimited);
    assert_eq!((ids, receipt), (vec![], applied(10)));
    // lower > upper: empty, seek only.
    let (ids, _, receipt, _) = run(inc(20), inc(10), unlimited);
    assert_eq!((ids, receipt), (vec![], applied(10)));
    // Everything from 5 up: 4 term visits (5, 10, 20, 30); the U64 20 of
    // record 5 sits under another tag and is neither visited nor returned:
    // 1 + 5 + 4 + 12 + PostingEntry 5 + ResultRow 15 = 42.
    let (ids, _, receipt, _) = run(inc(5), Bound::Unbounded, unlimited);
    assert_eq!((ids, receipt), (vec![1, 2, 3, 4, 6], applied(42)));
    // Unbounded below, excluded 10 above: the existence term is skipped for
    // free, 5 is visited, 10 terminates: 1 + 5 + 4 + 3 + 1 + 3 = 17.
    let (ids, _, receipt, _) = run(Bound::Unbounded, exc(10), unlimited);
    assert_eq!((ids, receipt), (vec![3], applied(17)));
    // Excluded 30 with nothing above it: the run stops at the U64 existence
    // term without a visit.
    let (ids, _, receipt, _) = run(exc(30), Bound::Unbounded, unlimited);
    assert_eq!((ids, receipt), (vec![], applied(10)));
}

#[test]
fn prefix_runs_visit_every_matching_str_term() {
    let (engine, ns) = cost_fixture();
    let snapshot = engine.head_snapshot();
    let run = |prefix: &str| {
        page(
            &snapshot,
            ns,
            &Query::term(Term::starts_with(name("kind"), prefix)),
            &Projection::ID_ONLY,
            10,
            None,
            Budget::unlimited(),
        )
    };
    // Empty prefix: `book`, `ink`, `pen` (TermVisit 9), their 5 postings
    // merged by id (PostingEntry 5), 5 rows: 1 + 5 + 4 + 9 + 5 + 15 = 39.
    let (ids, after, receipt, _) = run("");
    assert_eq!((ids, after), (vec![1, 2, 3, 4, 5], None));
    assert_eq!(receipt, applied(39));
    // A prefix that is also the full value `pen`, and a shorter one: the
    // same single visit; the run ends at the next name: 1+5+4+3+2+6 = 21.
    for prefix in ["p", "pen"] {
        let (ids, _, receipt, _) = run(prefix);
        assert_eq!((ids, receipt), (vec![3, 4], applied(21)), "{prefix}");
    }
    // `bo` visits `book` only: 1 + 5 + 4 + 3 + 2 + 6 = 21.
    let (ids, _, receipt, _) = run("bo");
    assert_eq!((ids, receipt), (vec![1, 2], applied(21)));
    // Nothing at or after `z` in the run: seek only.
    let (ids, _, receipt, _) = run("z");
    assert_eq!((ids, receipt), (vec![], applied(10)));
    let (ids, _, receipt, _) = run("pens");
    assert_eq!((ids, receipt), (vec![], applied(10)));
}

#[test]
fn not_eq_and_negation_probe_every_candidate() {
    let (engine, ns) = cost_fixture();
    let snapshot = engine.head_snapshot();
    let unlimited = Budget::unlimited();

    // NotEq(kind, pen): the existence posting of (kind, Str) has 5 entries,
    // each PostingEntry 1 + RecordProbe 2; 1, 2 and 5 pass:
    // 1 + 5 + 4 + 15 + ResultRow 9 = 34.
    let query = Query::term(Term::not_eq(name("kind"), s("pen")));
    let (ids, after, receipt, _) = page(
        &snapshot,
        ns,
        &query,
        &Projection::ID_ONLY,
        10,
        None,
        unlimited.traced(),
    );
    assert_eq!((ids, after), (vec![1, 2, 5], None));
    assert_eq!(
        receipt,
        traced(
            applied(34),
            &[
                (OperationClass::DirectoryLookup, 1, 1, 1),
                (OperationClass::QueryGroup, 1, 5, 5),
                (OperationClass::TermSeek, 1, 4, 4),
                (OperationClass::PostingEntry, 5, 1, 5),
                (OperationClass::RecordProbe, 5, 2, 10),
                (OperationClass::ResultRow, 3, 3, 9),
            ]
        )
    );
    // NotEq(price, I64 20): record 5 (U64) is not even a candidate.
    let query = Query::term(Term::not_eq(name("price"), Value::I64(20)));
    let (ids, _, receipt, _) = page(
        &snapshot,
        ns,
        &query,
        &Projection::ID_ONLY,
        10,
        None,
        unlimited,
    );
    assert_eq!((ids, receipt), (vec![1, 3, 6], applied(34)));
    // NOT Eq(kind, book) with the key: no seek; every live row is visited
    // (RowVisit 6) and probed (RecordProbe 12); 3, 4, 5 and 6 (no kind at
    // all) pass; ResultRow 12 + ReadByte 2 per key = 20: 1 + 5 + 18 + 20 = 44.
    let query = Query::all(vec![Literal::not(kind_eq("book"))]);
    let (ids, after, receipt, _) = page(
        &snapshot,
        ns,
        &query,
        &Projection::KEY_ONLY,
        10,
        None,
        unlimited.traced(),
    );
    assert_eq!((ids, after), (vec![3, 4, 5, 6], None));
    assert_eq!(
        receipt,
        traced(
            applied(44),
            &[
                (OperationClass::DirectoryLookup, 1, 1, 1),
                (OperationClass::QueryGroup, 1, 5, 5),
                (OperationClass::RowVisit, 6, 1, 6),
                (OperationClass::RecordProbe, 6, 2, 12),
                (OperationClass::ResultRow, 4, 3, 12),
                (OperationClass::ReadByte, 8, 1, 8),
            ]
        )
    );
    // A negated first literal with a page of one: rows 1, 2 fail, 3 passes,
    // then 4 is pulled as the head beyond the page: RowVisit 4, RecordProbe
    // 8, ResultRow 3: 1 + 5 + 12 + 3 = 21.
    let (ids, after, receipt, _) = page(
        &snapshot,
        ns,
        &query,
        &Projection::ID_ONLY,
        1,
        None,
        unlimited,
    );
    assert_eq!((ids, after), (vec![3], Some(3)));
    assert_eq!(receipt, applied(21));
}

#[test]
fn match_all_pages_walk_the_row_map() {
    let (engine, ns) = cost_fixture();
    let snapshot = engine.head_snapshot();
    let unlimited = Budget::unlimited();
    let query = Query::match_all();

    // Four rows: rows 1..=4 are visited and emitted, row 5 is visited as
    // the head beyond the page: 1 + 5 + RowVisit 5 + ResultRow 12 = 23.
    let (ids, after, receipt, cursor) = page(
        &snapshot,
        ns,
        &query,
        &Projection::ID_ONLY,
        4,
        None,
        unlimited,
    );
    assert_eq!((ids, after), (vec![1, 2, 3, 4], Some(4)));
    assert_eq!(receipt, applied(23));
    // The rest: 5 and 6, then exhaustion: 1 + 5 + 2 + 6 = 14.
    let (ids, after, receipt, _) = page(
        &snapshot,
        ns,
        &query,
        &Projection::ID_ONLY,
        4,
        cursor.as_ref(),
        unlimited,
    );
    assert_eq!((ids, after), (vec![5, 6], None));
    assert_eq!(receipt, applied(14));
    // count: 1 + 5 + RowVisit 6 = 12.
    let count = snapshot
        .count(ns, &query, unlimited)
        .expect("a valid count");
    assert_eq!((count.value, count.receipt), (6, applied(12)));
}

#[test]
fn and_groups_probe_candidate_major_until_a_literal_fails() {
    let (engine, ns) = cost_fixture();
    let snapshot = engine.head_snapshot();
    let unlimited = Budget::unlimited();

    // Eq(kind, pen) AND Eq(price, 20): candidates 3 (dropped by the probe)
    // and 4: PostingEntry 2, RecordProbe 4, one row: 1+5+4+2+4+3 = 19.
    let query = Query::all(vec![
        lit(kind_eq("pen")),
        lit(Term::eq(name("price"), Value::I64(20))),
    ]);
    let (ids, after, receipt, _) = page(
        &snapshot,
        ns,
        &query,
        &Projection::ID_ONLY,
        10,
        None,
        unlimited.traced(),
    );
    assert_eq!((ids, after), (vec![4], None));
    assert_eq!(
        receipt,
        traced(
            applied(19),
            &[
                (OperationClass::DirectoryLookup, 1, 1, 1),
                (OperationClass::QueryGroup, 1, 5, 5),
                (OperationClass::TermSeek, 1, 4, 4),
                (OperationClass::PostingEntry, 2, 1, 2),
                (OperationClass::RecordProbe, 2, 2, 4),
                (OperationClass::ResultRow, 1, 3, 3),
            ]
        )
    );
    // Three literals: record 1 passes both probes, record 2 fails the first
    // and is not probed by the second: RecordProbe 3: 1+5+4+2+6+3 = 21.
    let query = Query::all(vec![
        lit(kind_eq("book")),
        lit(Term::eq(name("price"), Value::I64(10))),
        lit(Term::exists(name("price"), TypeTag::I64)),
    ]);
    let (ids, _, receipt, _) = page(
        &snapshot,
        ns,
        &query,
        &Projection::ID_ONLY,
        10,
        None,
        unlimited,
    );
    assert_eq!((ids, receipt), (vec![1], applied(21)));
    // A negated further literal is just a probe: Exists(price, I64) AND NOT
    // Eq(kind, pen) probes 5 candidates, 1, 2 and 6 pass: 1+5+4+5+10+9 = 34.
    let query = Query::all(vec![
        lit(Term::exists(name("price"), TypeTag::I64)),
        Literal::not(kind_eq("pen")),
    ]);
    let (ids, _, receipt, _) = page(
        &snapshot,
        ns,
        &query,
        &Projection::ID_ONLY,
        10,
        None,
        unlimited,
    );
    assert_eq!((ids, receipt), (vec![1, 2, 6], applied(34)));
}

#[test]
fn or_groups_merge_by_id_and_advance_only_emitted_heads() {
    let (engine, ns) = cost_fixture();
    let snapshot = engine.head_snapshot();
    let unlimited = Budget::unlimited();

    // book OR pen: QueryGroup 10, TermSeek 8, PostingEntry 4, ResultRow 12:
    // 35, in id order across the groups.
    let query = Query {
        groups: vec![vec![lit(kind_eq("book"))], vec![lit(kind_eq("pen"))]],
    };
    let (ids, after, receipt, _) = page(
        &snapshot,
        ns,
        &query,
        &Projection::ID_ONLY,
        10,
        None,
        unlimited,
    );
    assert_eq!((ids, after), (vec![1, 2, 3, 4], None));
    assert_eq!(receipt, applied(35));
    // Overlapping groups emit an id once and both advance past it:
    // book {1, 2} OR price = 10 {1}: TermVisit 3, PostingEntry 3 (1, 1, 2),
    // ResultRow 6: 1 + 10 + 8 + 3 + 3 + 6 = 31; count 25.
    let query = Query {
        groups: vec![
            vec![lit(kind_eq("book"))],
            vec![lit(price_range(inc(10), inc(10)))],
        ],
    };
    let (ids, _, receipt, _) = page(
        &snapshot,
        ns,
        &query,
        &Projection::ID_ONLY,
        10,
        None,
        unlimited,
    );
    assert_eq!((ids, receipt), (vec![1, 2], applied(31)));
    let count = snapshot
        .count(ns, &query, unlimited)
        .expect("a valid count");
    assert_eq!((count.value, count.receipt), (2, applied(25)));
    // pen OR book with a page of one: the pen head (3) is beyond the page
    // and does no further work; book advances to 2: PostingEntry 3:
    // 1 + 10 + 8 + 3 + 3 = 25. The next page reopens both after 1:
    // PostingEntry 3 (3; 2; then 4 after 3 is emitted), ResultRow 9: 31.
    let query = Query {
        groups: vec![vec![lit(kind_eq("pen"))], vec![lit(kind_eq("book"))]],
    };
    let (ids, after, receipt, cursor) = page(
        &snapshot,
        ns,
        &query,
        &Projection::ID_ONLY,
        1,
        None,
        unlimited,
    );
    assert_eq!((ids, after), (vec![1], Some(1)));
    assert_eq!(receipt, applied(25));
    let (ids, after, receipt, _) = page(
        &snapshot,
        ns,
        &query,
        &Projection::ID_ONLY,
        10,
        cursor.as_ref(),
        unlimited,
    );
    assert_eq!((ids, after), (vec![2, 3, 4], None));
    assert_eq!(receipt, applied(31));
}
