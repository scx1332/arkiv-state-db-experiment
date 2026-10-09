//! Cross-namespace isolation of the typed record store, its term indexes and
//! every milestone-2 operator, in the shape of `tests/isolation.rs`.
//!
//! Namespace A holds 40 typed records over five cell names (`kind: Str`,
//! `price: I64`, `hot: Bool` and `$exp: U64` attributes plus a `note: Bytes`
//! field) and two raw byte records. Namespace B holds five typed records
//! carrying exactly the same names and values (plus `seq: I64` and
//! `label: Str`). A's workload runs on a branch from the current head that is
//! dropped, never sealed or published, so A's published state is frozen: it
//! creates records whose terms exist and whose terms are new, patches (value
//! change, type change), deletes, host-path deletes, an expiry sweep,
//! `get_record` hits and misses, and queries with `Eq`, `NotEq`, `Range`,
//! `StartsWith`, `Exists`, a negated literal, match-all, an AND-group with
//! probes, an OR of three groups, a three-page cursor walk, a `Named`
//! projection, counts, and a budget-limited variant of each that aborts
//! mid-enumeration. Phase 1 publishes 10 000 typed records into B carrying
//! A's attribute names and values plus thousands of distinct `seq` and
//! `label` terms, and 2 000 raw records; phase 2 publishes 500 namespaces.
//!
//! What this file establishes:
//! - Every receipt of A's workload (ledgers included), every result, every
//!   abort point and every effect is the hand-derived literal stated below,
//!   and every result agrees with an independent full-scan evaluation of the
//!   query semantics over a `BTreeMap` model of A's records.
//! - All of that is identical after B grows by 10 000 typed records that
//!   share A's terms (phase 1) and after the directory grows by 500
//!   namespaces (phase 2); so are A's namespace-local physical counters
//!   (`record_nodes`, `index_nodes`), because A's key map, row map and term
//!   map are the very same objects in every phase.
//! - The shared directory work (`directory_nodes`) is unchanged by another
//!   namespace's growth and is exactly what the directory trie's shape
//!   implies: A's lookup path (3 nodes with two namespaces, 5 with 502),
//!   twice that for an applied write, none before the first charge; always
//!   within the `2·8 + 1` = 17-node path bound of 8-byte ids.
//! - A snapshot pinned at the end of phase 0 answers the read workload with
//!   the same rows, receipts and physical counters after phases 1 and 2.
//! - B's identical terms are invisible from A: a query in A never returns a
//!   record of B, a term that exists only in B misses in A at the fixed miss
//!   price, a cursor is bound to its namespace, and an expiry sweep deletes
//!   only in the namespace it names.
//!
//! What it does not establish: freedom from shared CPU, memory or cache
//! interference (the physical counters are node visits, not time), the
//! metering vectors of every admission fault (other files cover them), the
//! commitment of the directory and index in the state root (`roots.rs`) or
//! their proofs, and durability.

use std::{
    collections::{BTreeMap, BTreeSet},
    ops::Bound,
};

use arkiv_db::{
    ArgumentFault, BlockNumber, Branch, Budget, Cell, CellName, CellSelect, Cost, Cursor, Effect,
    Engine, Executed, ExpiryHalt, Ledger, LedgerEntry, Literal, NamespaceDescriptor, NamespaceId,
    OperationClass, Outcome, OwnerId, PhysicalTrace, PriceSchedule, Projection, Query, QueryPage,
    ReadResult, Receipt, ReceiptKind, RecordId, RecordKey, Rejection, Revision, Row, Snapshot,
    SnapshotId, Term, TypeTag, Value, WriteOp, WriteResult, expire_records,
};

// ---------------------------------------------------------------------------
// Actors and fixture shape
// ---------------------------------------------------------------------------

const ALICE: OwnerId = OwnerId::new([0xa1; 20]);
const BOB: OwnerId = OwnerId::new([0xb0; 20]);
const DAVE: OwnerId = OwnerId::new([0xd0; 20]);

/// A's typed records `a/00`..`a/39` (4-byte keys) get ids 1..=40 in creation
/// order; see [`a_cells`] for their cells.
const A_RECORDS: usize = 40;
/// A's raw byte records: the raw keyspace is a sibling of the typed store.
const A_RAW_RECORDS: [(&[u8], &[u8]); 2] = [(b"raw/1", b"one"), (b"raw/2", b"two")];
/// Distinct `kind` values; `book` and `bolt` share the prefix `bo`.
const KINDS: [&str; 4] = ["book", "bolt", "pen", "lamp"];
/// The value of every `note` field (3 bytes).
const NOTE: &[u8] = b"xyz";
/// Distinct index terms A holds after phase 0: value and existence terms of
/// kind (4 + 1), price (5 + 1), hot (2 + 1) and $exp (8 + 1).
const A_TERMS: usize = 23;

/// B holds `b/00000`..`b/00004` (7-byte keys) after phase 0 and
/// `b/00005`..`b/10004` on top after phase 1, ids 1..=10005.
const B_INITIAL_RECORDS: usize = 5;
const B_GROWTH_RECORDS: usize = 10_000;
const B_GROWTH_BLOCKS: usize = 5;
const B_TOTAL_RECORDS: usize = B_INITIAL_RECORDS + B_GROWTH_RECORDS;
/// Raw byte records `r/00000`..`r/01999` (7-byte keys, 9-byte values
/// `bee-NNNNN`) published into B in phase 1.
const B_RAW_RECORDS: usize = 2_000;

/// Namespaces created in phase 2, on top of A and B.
const EXTRA_NAMESPACES: usize = 500;
const PHASE0_NAMESPACES: usize = 2;
const PHASE2_NAMESPACES: usize = PHASE0_NAMESPACES + EXTRA_NAMESPACES;

/// `Limits::DEFAULT.max_page`: the page limit of every unpaged query here.
const MAX_PAGE: usize = 200;
/// The block at which the `$exp` values 1, 2 and 3 expire.
const EXPIRY_BLOCK: BlockNumber = BlockNumber(3);

// ---------------------------------------------------------------------------
// Charge table transcribed from decisions2.md (PriceSchedule::FIXTURE, id 1)
// ---------------------------------------------------------------------------

/// Unit price of a class under the fixture schedule, transcribed from the
/// brief. Nothing here asks `PriceSchedule` what a class costs.
fn fixture_price(class: OperationClass) -> u64 {
    match class {
        OperationClass::DirectoryLookup => 1,
        OperationClass::NamespaceCreate => 50,
        OperationClass::NamespaceTransfer => 20,
        OperationClass::PutRecord => 10,
        OperationClass::DeleteRecord => 5,
        OperationClass::WriteKeyByte => 2,
        OperationClass::WriteValueByte => 1,
        OperationClass::PointRead => 3,
        OperationClass::ScanSeek => 4,
        OperationClass::ScanEntry => 2,
        OperationClass::ReadByte => 1,
        OperationClass::RecordCreate => 20,
        OperationClass::RecordPatch => 10,
        OperationClass::RecordDelete => 20,
        OperationClass::CellWrite => 4,
        OperationClass::CellByte => 1,
        OperationClass::CellRemove => 4,
        OperationClass::IndexInsert => 6,
        OperationClass::IndexTermCreate => 30,
        OperationClass::IndexRemove => 6,
        OperationClass::QueryGroup => 5,
        OperationClass::TermSeek => 4,
        OperationClass::TermVisit => 3,
        OperationClass::PostingEntry => 1,
        OperationClass::RowVisit => 1,
        OperationClass::RecordProbe => 2,
        OperationClass::ResultRow => 3,
        OperationClass::RootUpdate => 2,
        OperationClass::RowByte => 1,
        OperationClass::ProofNode => 2,
        OperationClass::ProofByte => 1,
    }
}

/// Canonical encoded length of a value, transcribed from the brief:
/// 1 for `Bool`, 8 for the integers, the byte length for `Str` and `Bytes`.
fn encoded_len(value: &Value) -> u64 {
    match value {
        Value::I32(_) | Value::Bytes4(_) | Value::U32(_) | Value::Dec32(_) => 4,
        Value::F32(_) | Value::Date32(_) => 4,
        Value::Addr(_) => 20,
        Value::U256(_) | Value::Dec(_) | Value::Bytes32(_) | Value::Key(_) | Value::I256(_) => 32,
        Value::Bytes16(_) | Value::U128(_) | Value::I128(_) | Value::Dec128(_) => 16,
        Value::Bool(_) => 1,
        Value::I64(_) | Value::U64(_) | Value::Bytes8(_) | Value::Dec64(_) => 8,
        Value::F64(_) | Value::Timestamp64(_) => 8,
        Value::Str(text) => text.len() as u64,
        Value::Bytes(bytes) => bytes.len() as u64,
    }
}

/// FIXTURE total of a create under `key` with `new_terms` new terms
/// (model 3): `4 + {20 + 4c + b + RowByte n + RootUpdate 3*2} + 2a *
/// {IndexInsert 6 + RootUpdate 2*2} + 30 per new term`, where the stored row
/// is `n = k + b`.
fn create_cost(key: &RecordKey, cells: &[(CellName, Cell)], new_terms: u64) -> u64 {
    let k = key.as_bytes().len() as u64;
    let c = cells.len() as u64;
    let a = cells.iter().filter(|(_, cell)| cell.is_attribute()).count() as u64;
    let b: u64 = cells
        .iter()
        .map(|(name, cell)| name.len() as u64 + encoded_len(&cell.value))
        .sum();
    4 + (20 + 4 * c + b + (k + b) + 3 * 2) + 2 * a * (6 + 2 * 2) + 30 * new_terms
}

/// FIXTURE total of a raw put (model 3): `1 + 10 + 2k + v + RootUpdate 2*2`.
fn put_cost(key: &[u8], value: &[u8]) -> u64 {
    1 + 10 + 2 * key.len() as u64 + value.len() as u64 + 2 * 2
}

/// The oracle rule for term newness: a value term is new iff no live record
/// holds an attribute of that name, type and value; an existence term iff
/// none holds that name and type. The fixture blocks only create records, so
/// the set of terms seen so far is exact.
#[derive(Default)]
struct TermLedger {
    seen: BTreeSet<String>,
}

impl TermLedger {
    /// Counts the terms of `cells` that are new and records them.
    fn admit(&mut self, cells: &[(CellName, Cell)]) -> u64 {
        let mut fresh = 0;
        for (name, cell) in cells {
            if !cell.is_attribute() {
                continue;
            }
            let value_term = format!("{name}:{:?}", cell.value);
            let existence_term = format!("{name}:{:?}", cell.value.tag());
            for term in [value_term, existence_term] {
                if self.seen.insert(term) {
                    fresh += 1;
                }
            }
        }
        fresh
    }
}

/// Phase 0 block (model 3): 2 namespace creates at 1 + 50 + 2 = 53; A's 40
/// creates under 4-byte keys, hand-summed: a plain record (price 13 + note
/// 7 + kind 8 bytes, c 3, a 2) costs 4 + (20 + 12 + 28 + RowByte 32 +
/// RootUpdate 6) + 4 * 10 = 142, or 140 for a `pen` (one byte less in b and
/// in n); `hot` (4 bytes, one attribute) adds 4 + 4 + 4 + 20 = 32; `$exp`
/// (12 bytes) adds 4 + 12 + 12 + 20 = 48. Over 40 records, 10 `pen`, 14
/// `hot` and 8 `$exp`: 40 * 142 - 10 * 2 + 14 * 32 + 8 * 48 = 6492, plus 23
/// new terms at 30 -> 7182. 2 raw puts at 1 + 10 + 10 + 3 + 4 = 28; B's 5
/// creates under 7-byte keys: the model-2 totals 551, 263, 262, 313, 233
/// plus RowByte (7 + b) + RootUpdate 2 * (3 + 2 * 2a) each (two index
/// groups per attribute cell): 551 + (74 + 54), 263 + (58 + 38), 262 +
/// (57 + 38), 313 + (62 + 46), 233 + (58 + 38) = 679 + 359 + 357 + 421 +
/// 329 = 2145. Total 106 + 7182 + 56 + 2145.
const PHASE0_BLOCK_COST: u64 = 9489;
/// Phase 1 growth blocks of 2000 B creates each; the totals differ because
/// the `hot`/`$exp` cycles fall differently into each block. Model 2 gave
/// 442 040 for blocks with 667 `hot` records and 442 020 for those with 666
/// (each block holds 500 `pen` and 400 `$exp` records); model 3 adds per
/// record RowByte (7 + b) + RootUpdate 2 * (3 + 2 * 2a), two index groups
/// per attribute cell: 96 for a plain record (b 51, a 4), -1 for a `pen`,
/// +12 with `hot`, +20 with `$exp`, so
/// 2000 * 96 - 500 + 12 * 667 + 20 * 400 = 207 504 (207 492 with 666 `hot`).
const B_GROWTH_BLOCK_COSTS: [u64; B_GROWTH_BLOCKS] = [649_544, 649_512, 649_544, 649_544, 649_512];
/// Phase 1 raw block: 2000 puts at 1 + 10 + 14 + 9 + 4 = 38.
const B_RAW_BLOCK_COST: u64 = 76_000;
/// Phase 2 block: 500 creates at 53.
const PHASE2_BLOCK_COST: u64 = 26_500;
/// What the workload's recorded writes spend: 176 + 312 + 4 + 182 + 282 +
/// 120 + 166 + 4 + 74 + 106 + 4 + 76 + 82 + 110 + (130 + 106 + 106).
const WORKLOAD_BLOCK_SPENT: u64 = 2040;
/// Recorded write receipts of the workload: 12 user writes, 1 host delete,
/// 1 halted expiry delete, 3 expiry deletes (the `Put` on the host path is
/// refused at admission and never recorded).
const WORKLOAD_RECORDED_WRITES: usize = 17;
/// Live records of A at the end of the workload: 40 + 3 created - 1 deleted
/// - 1 host-deleted - 3 expired.
const A_RECORDS_AFTER_WORKLOAD: usize = 38;

// ---------------------------------------------------------------------------
// Fixture vocabulary
// ---------------------------------------------------------------------------

fn name(text: &str) -> CellName {
    CellName::new(text).expect("fixture names follow the grammar")
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

fn a_key(i: usize) -> RecordKey {
    key(&format!("a/{i:02}"))
}

fn b_key(j: usize) -> RecordKey {
    key(&format!("b/{j:05}"))
}

fn b_raw_key(r: usize) -> Vec<u8> {
    format!("r/{r:05}").into_bytes()
}

fn b_raw_value(r: usize) -> Vec<u8> {
    format!("bee-{r:05}").into_bytes()
}

/// Record `i` of A, submitted out of name order (the engine sorts):
/// `kind` cycles through [`KINDS`] and `price` through -20, -10, 0, 10, 20,
/// so every (kind, price) pair occurs exactly twice among 40 records; `hot`
/// is present on every third record (true on every sixth); `$exp` on every
/// fifth with the values 1..=8; `note` is a field on every record.
fn a_cells(i: usize) -> Vec<(CellName, Cell)> {
    let step = i64::try_from(i % 5).expect("fits");
    let mut cells = vec![
        (name("price"), attr(Value::I64(step * 10 - 20))),
        (name("note"), field(Value::bytes(NOTE))),
        (name("kind"), attr(Value::str(KINDS[i % 4]))),
    ];
    if i.is_multiple_of(3) {
        cells.push((name("hot"), attr(Value::Bool(i.is_multiple_of(6)))));
    }
    if i.is_multiple_of(5) {
        let expiry = u64::try_from(i / 5 + 1).expect("fits");
        cells.push((name("$exp"), attr(Value::U64(expiry))));
    }
    cells
}

/// Record `j` of B: A's layout for index `j` (identical names and values for
/// `j < 40`) plus a distinct `seq` and a distinct `label` term.
fn b_cells(j: usize) -> Vec<(CellName, Cell)> {
    let mut cells = a_cells(j);
    cells.push((
        name("seq"),
        attr(Value::I64(i64::try_from(j).expect("fits"))),
    ));
    cells.push((name("label"), attr(Value::str(&format!("v-{j:05}")))));
    cells
}

/// `a/new1`: every term already exists in A (kind book, price 20, hot true).
fn new1_cells() -> Vec<(CellName, Cell)> {
    vec![
        (name("kind"), attr(Value::str("book"))),
        (name("price"), attr(Value::I64(20))),
        (name("note"), field(Value::bytes(NOTE))),
        (name("hot"), attr(Value::Bool(true))),
    ]
}

/// `a/new2`: kind desk and price 999 are new value terms, `size` is a new
/// name (new value term and new existence term): four new terms.
fn new2_cells() -> Vec<(CellName, Cell)> {
    vec![
        (name("kind"), attr(Value::str("desk"))),
        (name("price"), attr(Value::I64(999))),
        (name("size"), attr(Value::U64(7))),
        (name("note"), field(Value::bytes(NOTE))),
    ]
}

/// `a/new3`: kind sofa, price 777 and size 9 are new value terms; the
/// existence term of `size` exists once `a/new2` is live: three new terms.
fn new3_cells() -> Vec<(CellName, Cell)> {
    vec![
        (name("kind"), attr(Value::str("sofa"))),
        (name("price"), attr(Value::I64(777))),
        (name("size"), attr(Value::U64(9))),
        (name("note"), field(Value::bytes(NOTE))),
    ]
}

fn create_record(namespace: NamespaceId, key: &RecordKey, cells: Vec<(CellName, Cell)>) -> WriteOp {
    WriteOp::CreateRecord {
        namespace,
        key: key.clone(),
        cells,
    }
}

fn patch_record(
    namespace: NamespaceId,
    key: &RecordKey,
    set: Vec<(CellName, Cell)>,
    remove: Vec<CellName>,
) -> WriteOp {
    WriteOp::PatchRecord {
        namespace,
        key: key.clone(),
        set,
        remove,
    }
}

fn delete_record(namespace: NamespaceId, key: &RecordKey) -> WriteOp {
    WriteOp::DeleteRecord {
        namespace,
        key: key.clone(),
    }
}

fn put(namespace: NamespaceId, key: &[u8], value: &[u8]) -> WriteOp {
    WriteOp::Put {
        namespace,
        key: key.to_vec(),
        value: value.to_vec(),
    }
}

// ---------------------------------------------------------------------------
// The queries of the workload
// ---------------------------------------------------------------------------

fn eq(attribute: &str, value: Value) -> Term {
    Term::eq(name(attribute), value)
}

/// `kind = book`: ten records, ids 1, 5, 9, ..., 37.
fn q_eq_book() -> Query {
    Query::term(eq("kind", Value::str("book")))
}

/// `kind != pen` within Str: the 30 records that are not a pen.
fn q_not_eq_pen() -> Query {
    Query::term(Term::not_eq(name("kind"), Value::str("pen")))
}

/// `-10 <= price < 10`: the value terms -10 and 0, sixteen records.
fn q_range_price() -> Query {
    Query::term(Term::range(
        name("price"),
        Bound::Included(Value::I64(-10)),
        Bound::Excluded(Value::I64(10)),
    ))
}

/// `kind starts with "bo"`: the value terms bolt and book, twenty records.
fn q_starts_with_bo() -> Query {
    Query::term(Term::starts_with(name("kind"), "bo"))
}

/// `hot exists as Bool`: fourteen records.
fn q_exists_hot() -> Query {
    Query::term(Term::exists(name("hot"), TypeTag::Bool))
}

/// `NOT kind = book`: the row map is the source, thirty records match.
fn q_negated_book() -> Query {
    Query::all(vec![Literal::not(eq("kind", Value::str("book")))])
}

/// `kind = book AND price >= 0 AND NOT hot = true`: the book posting is the
/// source, the two further literals are probes.
fn q_and_probes() -> Query {
    Query::all(vec![
        Literal::of(eq("kind", Value::str("book"))),
        Literal::of(Term::range(
            name("price"),
            Bound::Included(Value::I64(0)),
            Bound::Unbounded,
        )),
        Literal::not(eq("hot", Value::Bool(true))),
    ])
}

/// `kind = pen OR price = 20 OR ($exp exists AND hot = false)`.
fn q_or_groups() -> Query {
    Query {
        groups: vec![
            vec![Literal::of(eq("kind", Value::str("pen")))],
            vec![Literal::of(eq("price", Value::I64(20)))],
            vec![
                Literal::of(Term::exists(name("$exp"), TypeTag::U64)),
                Literal::of(eq("hot", Value::Bool(false))),
            ],
        ],
    }
}

/// `price = -20`: eight records, ids 1, 6, 11, ..., 36.
fn q_eq_price_minus_20() -> Query {
    Query::term(eq("price", Value::I64(-20)))
}

/// A value no record holds.
fn q_miss_kind() -> Query {
    Query::term(eq("kind", Value::str("zzz")))
}

/// A name that exists only in B.
fn q_foreign_eq() -> Query {
    Query::term(eq("seq", Value::I64(7)))
}

/// A prefix over a name that exists only in B (a thousand labels there).
fn q_foreign_prefix() -> Query {
    Query::term(Term::starts_with(name("label"), "v-00"))
}

/// A range over a name that exists only in B.
fn q_foreign_range() -> Query {
    Query::term(Term::range(
        name("seq"),
        Bound::Included(Value::I64(0)),
        Bound::Included(Value::I64(1000)),
    ))
}

/// `price != U64(0)`: no record of A holds a U64 price, so the existence
/// posting is missing and nothing is probed.
fn q_not_eq_u64_price() -> Query {
    Query::term(Term::not_eq(name("price"), Value::U64(0)))
}

/// The expiry sweep's own query: `$exp <= 3`.
fn q_expiry() -> Query {
    Query::term(Term::range(
        name("$exp"),
        Bound::Unbounded,
        Bound::Included(Value::U64(EXPIRY_BLOCK.0)),
    ))
}

/// `price >= U64(0)`: only the record whose price was patched to U64.
fn q_range_u64_price() -> Query {
    Query::term(Term::range(
        name("price"),
        Bound::Included(Value::U64(0)),
        Bound::Unbounded,
    ))
}

fn q_eq_sofa() -> Query {
    Query::term(eq("kind", Value::str("sofa")))
}

/// Key plus `kind` and `price`; `zzz` names nothing and costs nothing.
fn named_kind_price_zzz() -> Projection {
    Projection::named(true, [name("kind"), name("price"), name("zzz")])
}

/// No key; `kind` and (where present) `hot`.
fn named_kind_hot() -> Projection {
    Projection::named(false, [name("kind"), name("hot")])
}

// ---------------------------------------------------------------------------
// Reference model: a BTreeMap of records and a full-scan evaluator of the
// query semantics in the brief. It never consults the engine.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
struct ModelRecord {
    key: RecordKey,
    cells: BTreeMap<CellName, Cell>,
}

#[derive(Clone, Debug)]
struct Model {
    next_id: u64,
    records: BTreeMap<RecordId, ModelRecord>,
}

/// The ordered value of an `I64`/`U64`, widened so both compare exactly.
fn ordered(value: &Value) -> Option<(TypeTag, i128)> {
    match value {
        Value::I64(v) => Some((TypeTag::I64, i128::from(*v))),
        Value::U64(v) => Some((TypeTag::U64, i128::from(*v))),
        _ => None,
    }
}

fn bound_tag(bound: &Bound<Value>) -> Option<TypeTag> {
    match bound {
        Bound::Included(value) | Bound::Excluded(value) => Some(value.tag()),
        Bound::Unbounded => None,
    }
}

fn above(held: i128, lower: &Bound<Value>) -> bool {
    match lower {
        Bound::Unbounded => true,
        Bound::Included(bound) => ordered(bound).is_some_and(|(_, b)| held >= b),
        Bound::Excluded(bound) => ordered(bound).is_some_and(|(_, b)| held > b),
    }
}

fn below(held: i128, upper: &Bound<Value>) -> bool {
    match upper {
        Bound::Unbounded => true,
        Bound::Included(bound) => ordered(bound).is_some_and(|(_, b)| held <= b),
        Bound::Excluded(bound) => ordered(bound).is_some_and(|(_, b)| held < b),
    }
}

/// The record semantics of one term over one record's cells.
fn term_holds(term: &Term, cells: &BTreeMap<CellName, Cell>) -> bool {
    let Some(cell) = cells.get(term.name()) else {
        return false;
    };
    if !cell.is_attribute() {
        return false;
    }
    let held = &cell.value;
    match term {
        Term::Eq { value, .. } => held == value,
        Term::NotEq { value, .. } => held.tag() == value.tag() && held != value,
        Term::Range { lower, upper, .. } => {
            let tag = bound_tag(lower)
                .or_else(|| bound_tag(upper))
                .expect("a validated range has a bound");
            let Some((held_tag, number)) = ordered(held) else {
                return false;
            };
            held_tag == tag && above(number, lower) && below(number, upper)
        }
        Term::StartsWith { prefix, .. } => match held {
            Value::Str(text) => text.as_bytes().starts_with(prefix.as_bytes()),
            _ => false,
        },
        Term::Exists { tag, .. } => held.tag() == *tag,
    }
}

/// Group = AND of its literals (a negated literal is the complement); query
/// = OR of its groups; an empty group matches every live record.
fn query_matches(query: &Query, cells: &BTreeMap<CellName, Cell>) -> bool {
    query.groups.iter().any(|group| {
        group
            .iter()
            .all(|literal| term_holds(&literal.term, cells) != literal.negated)
    })
}

fn project(id: RecordId, record: &ModelRecord, projection: &Projection) -> Row {
    let cells: Vec<(CellName, Cell)> = match &projection.cells {
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

impl Model {
    fn new() -> Self {
        Self {
            next_id: 1,
            records: BTreeMap::new(),
        }
    }

    fn id_of(&self, key: &RecordKey) -> Option<RecordId> {
        self.records
            .iter()
            .find(|(_, record)| record.key == *key)
            .map(|(id, _)| *id)
    }

    fn create(&mut self, key: &RecordKey, cells: &[(CellName, Cell)]) -> RecordId {
        assert!(self.id_of(key).is_none(), "model: {key:?} is bound");
        let id = rid(self.next_id);
        self.next_id += 1;
        self.records.insert(
            id,
            ModelRecord {
                key: key.clone(),
                cells: cells.iter().cloned().collect(),
            },
        );
        id
    }

    fn patch(
        &mut self,
        key: &RecordKey,
        set: &[(CellName, Cell)],
        remove: &[CellName],
    ) -> RecordId {
        let id = self.id_of(key).expect("model: patched key is bound");
        let record = self.records.get_mut(&id).expect("bound");
        for (name, cell) in set {
            record.cells.insert(name.clone(), cell.clone());
        }
        for name in remove {
            record.cells.remove(name);
        }
        id
    }

    fn delete(&mut self, key: &RecordKey) -> RecordId {
        let id = self.id_of(key).expect("model: deleted key is bound");
        self.records.remove(&id);
        id
    }

    fn get(&self, key: &RecordKey, projection: &Projection) -> Option<Row> {
        let id = self.id_of(key)?;
        Some(project(id, &self.records[&id], projection))
    }

    /// Matching ids in ascending order.
    fn ids(&self, query: &Query) -> Vec<RecordId> {
        self.records
            .iter()
            .filter(|(_, record)| query_matches(query, &record.cells))
            .map(|(id, _)| *id)
            .collect()
    }

    fn count(&self, query: &Query) -> u64 {
        self.ids(query).len() as u64
    }

    /// The first `limit` matches with id > `after`, and the id the cursor
    /// would carry iff a further match exists.
    fn page(
        &self,
        query: &Query,
        projection: &Projection,
        limit: usize,
        after: Option<RecordId>,
    ) -> (Vec<Row>, Option<RecordId>) {
        let mut matches = self.records.iter().filter(|(id, record)| {
            after.is_none_or(|after| **id > after) && query_matches(query, &record.cells)
        });
        let rows: Vec<Row> = matches
            .by_ref()
            .take(limit)
            .map(|(id, record)| project(*id, record, projection))
            .collect();
        let next = if matches.next().is_some() {
            rows.last().map(|row| row.id)
        } else {
            None
        };
        (rows, next)
    }
}

/// A's records as published in phase 0.
fn phase0_model() -> Model {
    let mut model = Model::new();
    for i in 0..A_RECORDS {
        let id = model.create(&a_key(i), &a_cells(i));
        assert_eq!(id, rid(i as u64 + 1));
    }
    model
}

// ---------------------------------------------------------------------------
// Workload steps
// ---------------------------------------------------------------------------

const GET_ALL: &str = "get_record a/00 ALL";
const GET_ABSENT: &str = "get_record a/99 ALL";
const GET_FOREIGN: &str = "get_record b/00000 (present only in B) ALL";
const GET_NAMED: &str = "get_record a/07 named [kind, price, zzz] with key";
const GET_ID_ONLY: &str = "get_record a/07 ID_ONLY";
const EQ_PAGE1: &str = "Eq(kind, book) ALL limit 4, page 1";
const EQ_PAGE2: &str = "Eq(kind, book) ALL limit 4, page 2";
const EQ_PAGE3: &str = "Eq(kind, book) ALL limit 4, page 3";
const NOT_EQ: &str = "NotEq(kind, pen) ID_ONLY";
const RANGE: &str = "Range(price, [-10, 10)) KEY_ONLY";
const STARTS_WITH: &str = "StartsWith(kind, bo) ID_ONLY";
const EXISTS: &str = "Exists(hot, Bool) ID_ONLY";
const NEGATED: &str = "NOT Eq(kind, book) KEY_ONLY";
const MATCH_ALL: &str = "match-all ID_ONLY";
const AND_PROBES: &str = "Eq(kind, book) AND Range(price >= 0) AND NOT Eq(hot, true) ID_ONLY";
const OR_GROUPS: &str =
    "Eq(kind, pen) OR Eq(price, 20) OR (Exists($exp) AND Eq(hot, false)) KEY_ONLY";
const NAMED_QUERY: &str = "Eq(price, -20) named [kind, hot]";
const COUNT_EQ: &str = "count Eq(kind, book)";
const COUNT_ALL: &str = "count match-all";
const COUNT_OR: &str = "count of the OR query";
const MISS_EQ: &str = "Eq(kind, zzz): value held by no record";
const MISS_FOREIGN_EQ: &str = "Eq(seq, 7): name held only in B";
const MISS_FOREIGN_PREFIX: &str = "StartsWith(label, v-00): name held only in B";
const MISS_FOREIGN_RANGE: &str = "Range(seq, [0, 1000]): name held only in B";
const MISS_NOT_EQ_TYPE: &str = "NotEq(price, U64 0): no U64 price in A";
const ABORT_EQ_PAGE: &str = "Eq(kind, book) ALL limit 4 with budget 100";
const ABORT_NOT_EQ: &str = "NotEq(kind, pen) with budget 30";
const ABORT_RANGE_ENTRIES: &str = "Range(price) with budget 30";
const ABORT_RANGE_TERMS: &str = "Range(price) with budget 14";
const ABORT_NEGATED: &str = "NOT Eq(kind, book) with budget 35";
const ABORT_MATCH_ALL: &str = "match-all with budget 50";
const ABORT_STARTS_WITH: &str = "StartsWith(kind, bo) with budget 22";
const ABORT_EXISTS: &str = "Exists(hot, Bool) with budget 12";
const ABORT_COUNT_ALL: &str = "count match-all with budget 20";
const ABORT_OR: &str = "the OR query with budget 38";
const ABORT_AND: &str = "the AND query with budget 44";
const ABORT_GET: &str = "get_record a/00 ALL with budget 54";
const ABORT_CURSOR_PAGE: &str = "Eq(kind, book) page 2 with budget 60";
const ABORT_NAMED: &str = "Eq(price, -20) named [kind, hot] with budget 40";
const CREATE_EXISTING_TERMS: &str = "create a/new1 (every term exists)";
const CREATE_NEW_TERMS: &str = "create a/new2 (four new terms)";
const CREATE_BOUND_KEY: &str = "create a/00 (key already bound)";
const ABORT_CREATE: &str = "create a/new3 with budget 150";
const CREATE_AFTER_ABORT: &str = "create a/new3 (its terms are still new)";
const PATCH: &str = "patch a/01: set price 20, hot false; remove note, zzz";
const PATCH_TYPE_CHANGE: &str = "patch a/02: price becomes U64 5";
const PATCH_MISSING: &str = "patch a/99";
const ABORT_PATCH: &str = "patch a/05 with budget 30";
const DELETE: &str = "delete a/03";
const DELETE_MISSING: &str = "delete a/03 again";
const ABORT_DELETE: &str = "delete a/06 with budget 60";
const HOST_PUT: &str = "execute_host Put";
const HOST_DELETE: &str = "execute_host delete a/04";
const EXPIRE_HALTED: &str = "expire_records $exp <= 3 with delete budget 80";
const EXPIRE: &str = "expire_records $exp <= 3";
const COUNT_ALL_AFTER: &str = "count match-all after the writes";
const COUNT_EXP_AFTER: &str = "count Exists($exp, U64) after the expiry";
const STALE_CURSOR: &str = "Eq(kind, book) page 2 with the pre-write cursor";
const GET_EXPIRED: &str = "get_record a/00 after the expiry";
const EQ_AFTER: &str = "Eq(kind, book) ID_ONLY after the writes";
const RANGE_U64_AFTER: &str = "Range(price, U64 >= 0) after the type change";
const EQ_NEW_TERM_AFTER: &str = "Eq(kind, sofa) KEY_ONLY after the creates";

/// The read steps, in order; the historical-snapshot test replays these.
const READ_STEPS: usize = 39;

/// The logical result of one workload step.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Observed {
    Record(Option<Row>),
    Page {
        rows: Vec<Row>,
        next: Option<RecordId>,
    },
    Count(u64),
    Applied(Effect),
    Rejected(Rejection),
    Expired(Vec<RecordId>),
    ExpiryHalted {
        applied: Vec<RecordId>,
        reason: Rejection,
    },
}

/// One executed step: its result, its receipts (one per metered operation;
/// an expiry sweep yields one per delete) and, when the engine exposes it,
/// the physical trace.
#[derive(Clone, Debug)]
struct Step {
    name: &'static str,
    observed: Observed,
    receipts: Vec<Receipt>,
    physical: Option<PhysicalTrace>,
}

/// The hand-derived expectation for one step.
struct Expected {
    name: &'static str,
    observed: Observed,
    receipts: Vec<Receipt>,
}

/// The typed reads shared by snapshots and branches.
trait Reader {
    fn record(
        &self,
        namespace: NamespaceId,
        key: &RecordKey,
        projection: &Projection,
        budget: Budget,
    ) -> ReadResult<Option<Row>>;
    fn page(
        &self,
        namespace: NamespaceId,
        query: &Query,
        projection: &Projection,
        limit: usize,
        cursor: Option<&Cursor>,
        budget: Budget,
    ) -> ReadResult<QueryPage>;
    fn total(&self, namespace: NamespaceId, query: &Query, budget: Budget) -> ReadResult<u64>;
}

impl Reader for Snapshot {
    fn record(
        &self,
        namespace: NamespaceId,
        key: &RecordKey,
        projection: &Projection,
        budget: Budget,
    ) -> ReadResult<Option<Row>> {
        self.get_record(namespace, key, projection, budget)
    }

    fn page(
        &self,
        namespace: NamespaceId,
        query: &Query,
        projection: &Projection,
        limit: usize,
        cursor: Option<&Cursor>,
        budget: Budget,
    ) -> ReadResult<QueryPage> {
        self.query(namespace, query, projection, limit, cursor, budget)
    }

    fn total(&self, namespace: NamespaceId, query: &Query, budget: Budget) -> ReadResult<u64> {
        self.count(namespace, query, budget)
    }
}

impl Reader for Branch {
    fn record(
        &self,
        namespace: NamespaceId,
        key: &RecordKey,
        projection: &Projection,
        budget: Budget,
    ) -> ReadResult<Option<Row>> {
        self.get_record(namespace, key, projection, budget)
    }

    fn page(
        &self,
        namespace: NamespaceId,
        query: &Query,
        projection: &Projection,
        limit: usize,
        cursor: Option<&Cursor>,
        budget: Budget,
    ) -> ReadResult<QueryPage> {
        self.query(namespace, query, projection, limit, cursor, budget)
    }

    fn total(&self, namespace: NamespaceId, query: &Query, budget: Budget) -> ReadResult<u64> {
        self.count(namespace, query, budget)
    }
}

fn read_step<T>(name: &'static str, result: ReadResult<T>, wrap: fn(T) -> Observed) -> Step {
    match result {
        Ok(metered) => Step {
            name,
            observed: wrap(metered.value),
            receipts: vec![metered.receipt],
            physical: Some(metered.physical),
        },
        Err(rejected) => Step {
            name,
            observed: Observed::Rejected(rejected.reason),
            receipts: vec![rejected.receipt],
            physical: Some(rejected.physical),
        },
    }
}

/// A page step; the cursor is handed back for the next page.
fn page_step(name: &'static str, result: ReadResult<QueryPage>) -> (Step, Option<Cursor>) {
    match result {
        Ok(metered) => {
            let cursor = metered.value.next.clone();
            let step = Step {
                name,
                observed: Observed::Page {
                    rows: metered.value.rows,
                    next: cursor.as_ref().map(Cursor::after),
                },
                receipts: vec![metered.receipt],
                physical: Some(metered.physical),
            };
            (step, cursor)
        }
        Err(rejected) => (
            Step {
                name,
                observed: Observed::Rejected(rejected.reason),
                receipts: vec![rejected.receipt],
                physical: Some(rejected.physical),
            },
            None,
        ),
    }
}

fn write_step(name: &'static str, result: WriteResult) -> Step {
    match result {
        Ok(executed) => Step {
            name,
            observed: Observed::Applied(executed.effect),
            receipts: vec![executed.receipt],
            physical: Some(executed.physical),
        },
        Err(rejected) => Step {
            name,
            observed: Observed::Rejected(rejected.reason),
            receipts: vec![rejected.receipt],
            physical: Some(rejected.physical),
        },
    }
}

/// An expiry sweep: the host helper exposes the delete receipts but no
/// physical trace.
fn expiry_step(name: &'static str, result: Result<Vec<(RecordId, Receipt)>, ExpiryHalt>) -> Step {
    match result {
        Ok(applied) => Step {
            name,
            observed: Observed::Expired(applied.iter().map(|(id, _)| *id).collect()),
            receipts: applied.into_iter().map(|(_, receipt)| receipt).collect(),
            physical: None,
        },
        Err(halt) => {
            let mut receipts: Vec<Receipt> = halt
                .applied
                .iter()
                .map(|(_, receipt)| receipt.clone())
                .collect();
            receipts.push(halt.rejected.receipt.clone());
            Step {
                name,
                observed: Observed::ExpiryHalted {
                    applied: halt.applied.iter().map(|(id, _)| *id).collect(),
                    reason: halt.rejected.reason,
                },
                receipts,
                physical: None,
            }
        }
    }
}

/// The read part of the workload plus the first page's cursor.
struct ReadRun {
    steps: Vec<Step>,
    first_page_cursor: Option<Cursor>,
}

/// The read part of A's workload, in order, against any reader.
fn read_workload(reader: &impl Reader, a: NamespaceId) -> ReadRun {
    let traced = Budget::unlimited().traced();
    let limited = |units| Budget::limit(units).traced();
    let all = &Projection::ALL;
    let id_only = &Projection::ID_ONLY;
    let key_only = &Projection::KEY_ONLY;
    let mut steps = vec![
        read_step(
            GET_ALL,
            reader.record(a, &a_key(0), all, traced),
            Observed::Record,
        ),
        read_step(
            GET_ABSENT,
            reader.record(a, &key("a/99"), all, traced),
            Observed::Record,
        ),
        read_step(
            GET_FOREIGN,
            reader.record(a, &b_key(0), all, traced),
            Observed::Record,
        ),
        read_step(
            GET_NAMED,
            reader.record(a, &a_key(7), &named_kind_price_zzz(), traced),
            Observed::Record,
        ),
        read_step(
            GET_ID_ONLY,
            reader.record(a, &a_key(7), id_only, traced),
            Observed::Record,
        ),
    ];
    let (page1, cursor1) = page_step(EQ_PAGE1, reader.page(a, &q_eq_book(), all, 4, None, traced));
    steps.push(page1);
    let (page2, cursor2) = page_step(
        EQ_PAGE2,
        reader.page(a, &q_eq_book(), all, 4, cursor1.as_ref(), traced),
    );
    steps.push(page2);
    steps.push(
        page_step(
            EQ_PAGE3,
            reader.page(a, &q_eq_book(), all, 4, cursor2.as_ref(), traced),
        )
        .0,
    );
    let pages: [(&str, Query, &Projection); 17] = [
        (NOT_EQ, q_not_eq_pen(), id_only),
        (RANGE, q_range_price(), key_only),
        (STARTS_WITH, q_starts_with_bo(), id_only),
        (EXISTS, q_exists_hot(), id_only),
        (NEGATED, q_negated_book(), key_only),
        (MATCH_ALL, Query::match_all(), id_only),
        (AND_PROBES, q_and_probes(), id_only),
        (OR_GROUPS, q_or_groups(), key_only),
        (NAMED_QUERY, q_eq_price_minus_20(), &named_kind_hot()),
        (MISS_EQ, q_miss_kind(), id_only),
        (MISS_FOREIGN_EQ, q_foreign_eq(), id_only),
        (MISS_FOREIGN_PREFIX, q_foreign_prefix(), id_only),
        (MISS_FOREIGN_RANGE, q_foreign_range(), id_only),
        (MISS_NOT_EQ_TYPE, q_not_eq_u64_price(), id_only),
        (COUNT_EQ, q_eq_book(), id_only),
        (COUNT_ALL, Query::match_all(), id_only),
        (COUNT_OR, q_or_groups(), id_only),
    ];
    for (step, query, projection) in pages {
        if step.starts_with("count") {
            steps.push(read_step(
                step,
                reader.total(a, &query, traced),
                Observed::Count,
            ));
        } else {
            steps.push(
                page_step(
                    step,
                    reader.page(a, &query, projection, MAX_PAGE, None, traced),
                )
                .0,
            );
        }
    }
    let aborted: [(&str, Query, &Projection, u64); 10] = [
        (ABORT_EQ_PAGE, q_eq_book(), all, 100),
        (ABORT_NOT_EQ, q_not_eq_pen(), id_only, 30),
        (ABORT_RANGE_ENTRIES, q_range_price(), key_only, 30),
        (ABORT_RANGE_TERMS, q_range_price(), key_only, 14),
        (ABORT_NEGATED, q_negated_book(), key_only, 35),
        (ABORT_MATCH_ALL, Query::match_all(), id_only, 50),
        (ABORT_STARTS_WITH, q_starts_with_bo(), id_only, 22),
        (ABORT_EXISTS, q_exists_hot(), id_only, 12),
        (ABORT_OR, q_or_groups(), key_only, 38),
        (ABORT_AND, q_and_probes(), id_only, 44),
    ];
    for (step, query, projection, budget) in aborted {
        let limit = if step == ABORT_EQ_PAGE { 4 } else { MAX_PAGE };
        steps.push(
            page_step(
                step,
                reader.page(a, &query, projection, limit, None, limited(budget)),
            )
            .0,
        );
    }
    steps.push(read_step(
        ABORT_COUNT_ALL,
        reader.total(a, &Query::match_all(), limited(20)),
        Observed::Count,
    ));
    steps.push(read_step(
        ABORT_GET,
        reader.record(a, &a_key(0), all, limited(54)),
        Observed::Record,
    ));
    steps.push(
        page_step(
            ABORT_CURSOR_PAGE,
            reader.page(a, &q_eq_book(), all, 4, cursor1.as_ref(), limited(60)),
        )
        .0,
    );
    steps.push(
        page_step(
            ABORT_NAMED,
            reader.page(
                a,
                &q_eq_price_minus_20(),
                &named_kind_hot(),
                MAX_PAGE,
                None,
                limited(40),
            ),
        )
        .0,
    );
    assert_eq!(
        steps.len(),
        READ_STEPS,
        "the read workload has a fixed length"
    );
    ReadRun {
        steps,
        first_page_cursor: cursor1,
    }
}

/// The write part of A's workload plus the reads that observe its effects,
/// on a branch that is dropped afterwards. `stale` is a cursor issued by the
/// branch before any write.
fn write_workload(branch: &mut Branch, a: NamespaceId, stale: Option<&Cursor>) -> Vec<Step> {
    let traced = Budget::unlimited().traced();
    let limited = |units| Budget::limit(units).traced();
    let expiry_name = name("$exp");
    let mut steps = vec![
        write_step(
            CREATE_EXISTING_TERMS,
            branch.execute(
                ALICE,
                create_record(a, &key("a/new1"), new1_cells()),
                traced,
            ),
        ),
        write_step(
            CREATE_NEW_TERMS,
            branch.execute(
                ALICE,
                create_record(a, &key("a/new2"), new2_cells()),
                traced,
            ),
        ),
        write_step(
            CREATE_BOUND_KEY,
            branch.execute(ALICE, create_record(a, &a_key(0), new1_cells()), traced),
        ),
        write_step(
            ABORT_CREATE,
            branch.execute(
                ALICE,
                create_record(a, &key("a/new3"), new3_cells()),
                limited(200),
            ),
        ),
        write_step(
            CREATE_AFTER_ABORT,
            branch.execute(
                ALICE,
                create_record(a, &key("a/new3"), new3_cells()),
                traced,
            ),
        ),
        write_step(
            PATCH,
            branch.execute(
                ALICE,
                patch_record(
                    a,
                    &a_key(1),
                    vec![
                        (name("price"), attr(Value::I64(20))),
                        (name("hot"), attr(Value::Bool(false))),
                    ],
                    vec![name("note"), name("zzz")],
                ),
                traced,
            ),
        ),
        write_step(
            PATCH_TYPE_CHANGE,
            branch.execute(
                ALICE,
                patch_record(
                    a,
                    &a_key(2),
                    vec![(name("price"), attr(Value::U64(5)))],
                    vec![],
                ),
                traced,
            ),
        ),
        write_step(
            PATCH_MISSING,
            branch.execute(
                ALICE,
                patch_record(
                    a,
                    &key("a/99"),
                    vec![(name("price"), attr(Value::U64(5)))],
                    vec![],
                ),
                traced,
            ),
        ),
        write_step(
            ABORT_PATCH,
            branch.execute(
                ALICE,
                patch_record(
                    a,
                    &a_key(5),
                    vec![(name("kind"), attr(Value::str("lamp")))],
                    vec![],
                ),
                limited(80),
            ),
        ),
        write_step(
            DELETE,
            branch.execute(ALICE, delete_record(a, &a_key(3)), traced),
        ),
        write_step(
            DELETE_MISSING,
            branch.execute(ALICE, delete_record(a, &a_key(3)), traced),
        ),
        write_step(
            ABORT_DELETE,
            branch.execute(ALICE, delete_record(a, &a_key(6)), limited(80)),
        ),
        write_step(
            HOST_PUT,
            branch.execute_host(put(a, b"raw/3", b"three"), traced),
        ),
        write_step(
            HOST_DELETE,
            branch.execute_host(delete_record(a, &a_key(4)), traced),
        ),
        expiry_step(
            EXPIRE_HALTED,
            expire_records(branch, a, &expiry_name, EXPIRY_BLOCK, traced, limited(110)),
        ),
        expiry_step(
            EXPIRE,
            expire_records(branch, a, &expiry_name, EXPIRY_BLOCK, traced, traced),
        ),
    ];
    steps.push(read_step(
        COUNT_ALL_AFTER,
        branch.total(a, &Query::match_all(), traced),
        Observed::Count,
    ));
    steps.push(read_step(
        COUNT_EXP_AFTER,
        branch.total(
            a,
            &Query::term(Term::exists(name("$exp"), TypeTag::U64)),
            traced,
        ),
        Observed::Count,
    ));
    steps.push(
        page_step(
            STALE_CURSOR,
            branch.page(a, &q_eq_book(), &Projection::ALL, 4, stale, traced),
        )
        .0,
    );
    steps.push(read_step(
        GET_EXPIRED,
        branch.record(a, &a_key(0), &Projection::ALL, traced),
        Observed::Record,
    ));
    steps.push(
        page_step(
            EQ_AFTER,
            branch.page(
                a,
                &q_eq_book(),
                &Projection::ID_ONLY,
                MAX_PAGE,
                None,
                traced,
            ),
        )
        .0,
    );
    steps.push(
        page_step(
            RANGE_U64_AFTER,
            branch.page(
                a,
                &q_range_u64_price(),
                &Projection::ID_ONLY,
                MAX_PAGE,
                None,
                traced,
            ),
        )
        .0,
    );
    steps.push(
        page_step(
            EQ_NEW_TERM_AFTER,
            branch.page(
                a,
                &q_eq_sofa(),
                &Projection::KEY_ONLY,
                MAX_PAGE,
                None,
                traced,
            ),
        )
        .0,
    );
    steps
}

/// Runs the whole workload on a branch from the current head and drops the
/// branch, so the published state is exactly what it was before.
fn run_workload(world: &World) -> Vec<Step> {
    let head = world.engine.head();
    let mut branch = world.engine.begin(head, None).expect("head is current");
    let b_typed_before = branch.typed_record_count(world.b);
    let read = read_workload(&branch, world.a);
    let mut steps = read.steps;
    steps.extend(write_workload(
        &mut branch,
        world.a,
        read.first_page_cursor.as_ref(),
    ));
    assert_eq!(
        branch.spent(),
        Cost::new(WORKLOAD_BLOCK_SPENT),
        "the recorded writes of the workload"
    );
    assert_eq!(branch.receipts().len(), WORKLOAD_RECORDED_WRITES);
    assert_eq!(
        branch.typed_record_count(world.a),
        Some(A_RECORDS_AFTER_WORKLOAD),
        "A on the branch after the workload"
    );
    assert_eq!(
        branch.index_consistent(world.a),
        Some(true),
        "A's index on the branch is exactly derived from its rows"
    );
    assert_eq!(
        branch.typed_record_count(world.b),
        b_typed_before,
        "A's workload never touches B"
    );
    drop(branch);
    assert_eq!(
        world.engine.head(),
        head,
        "a dropped branch never publishes"
    );
    steps
}

// ---------------------------------------------------------------------------
// The hand-derived baseline
// ---------------------------------------------------------------------------

/// Builds a traced receipt from literal `(class, count)` ledger lines in
/// first-charge order; the unit prices come from the transcribed table and
/// the lines must add up to `spent`.
fn receipt(
    kind: ReceiptKind,
    spent: u64,
    outcome: Outcome,
    lines: &[(OperationClass, u64)],
) -> Receipt {
    let entries: Vec<LedgerEntry> = lines
        .iter()
        .map(|&(class, count)| LedgerEntry {
            class,
            count,
            unit_price: fixture_price(class),
            subtotal: Cost::new(count * fixture_price(class)),
        })
        .collect();
    let total: u64 = entries.iter().map(|entry| entry.subtotal.units()).sum();
    assert_eq!(
        total, spent,
        "the stated ledger lines must add up to the stated total"
    );
    Receipt {
        kind,
        model_version: 3,
        schedule_id: 1,
        spent: Cost::new(spent),
        outcome,
        ledger: Some(Ledger { entries }),
    }
}

fn read_receipt(spent: u64, lines: &[(OperationClass, u64)]) -> Receipt {
    receipt(ReceiptKind::Read, spent, Outcome::Applied, lines)
}

fn aborted_read(spent: u64, lines: &[(OperationClass, u64)]) -> Receipt {
    receipt(
        ReceiptKind::Read,
        spent,
        Outcome::Rejected(Rejection::OutOfBudget),
        lines,
    )
}

fn write_receipt(spent: u64, lines: &[(OperationClass, u64)]) -> Receipt {
    receipt(ReceiptKind::Write, spent, Outcome::Applied, lines)
}

fn rejected_write(spent: u64, reason: Rejection, lines: &[(OperationClass, u64)]) -> Receipt {
    receipt(ReceiptKind::Write, spent, Outcome::Rejected(reason), lines)
}

/// An untraced applied write receipt.
fn plain_write_receipt(spent: u64) -> Receipt {
    Receipt {
        kind: ReceiptKind::Write,
        model_version: 3,
        schedule_id: 1,
        spent: Cost::new(spent),
        outcome: Outcome::Applied,
        ledger: None,
    }
}

fn page_observed(
    model: &Model,
    query: &Query,
    projection: &Projection,
    limit: usize,
    after: Option<RecordId>,
) -> Observed {
    let (rows, next) = model.page(query, projection, limit, after);
    Observed::Page { rows, next }
}

/// Every receipt and result of the workload, derived by hand from the
/// charge points in decisions2.md and the fixture table (id = i + 1):
///
/// | i  | kind | price | hot   | $exp | cells | ALL bytes |
/// |----|------|-------|-------|------|-------|-----------|
/// | 0  | book | -20   | true  | 1    | 5     | 48        |
/// | 1  | bolt | -10   |       |      | 3     | 32        |
/// | 2  | pen  | 0     |       |      | 3     | 31        |
/// | 3  | lamp | 10    | false |      | 4     | 36        |
/// | 4  | book | 20    |       |      | 3     | 32        |
/// | 5  | bolt | -20   |       | 2    | 4     | 44        |
/// | 6  | pen  | -10   | true  |      | 4     | 35        |
/// | 7  | lamp | 0     |       |      | 3     | 32        |
/// | .. | (period 4, 5, 3 and 5 respectively)                |
///
/// ALL bytes = key 4 + kind (4 + 4, or 4 + 3 for pen) + price (5 + 8) +
/// note (4 + 3) + hot (3 + 1 when present) + $exp (4 + 8 when present).
fn expected_baseline() -> Vec<Expected> {
    use OperationClass::{
        CellByte, CellRemove, CellWrite, DirectoryLookup, IndexInsert, IndexRemove,
        IndexTermCreate, PointRead, PostingEntry, QueryGroup, ReadByte, RecordCreate, RecordDelete,
        RecordPatch, RecordProbe, ResultRow, RootUpdate, RowByte, RowVisit, TermSeek, TermVisit,
    };
    let mut model = phase0_model();
    let all = &Projection::ALL;
    let id_only = &Projection::ID_ONLY;
    let key_only = &Projection::KEY_ONLY;
    let miss = |spent: u64, lines: &[(OperationClass, u64)]| read_receipt(spent, lines);
    let mut expected: Vec<Expected> = Vec::new();
    let mut push = |name, observed, receipts| {
        expected.push(Expected {
            name,
            observed,
            receipts,
        });
    };

    // --- point reads
    // a/00 under ALL: 1 + 3 + {3 + 48}.
    push(
        GET_ALL,
        Observed::Record(model.get(&a_key(0), all)),
        vec![read_receipt(
            55,
            &[
                (DirectoryLookup, 1),
                (PointRead, 1),
                (ResultRow, 1),
                (ReadByte, 48),
            ],
        )],
    );
    let get_miss = read_receipt(4, &[(DirectoryLookup, 1), (PointRead, 1)]);
    push(GET_ABSENT, Observed::Record(None), vec![get_miss.clone()]);
    push(GET_FOREIGN, Observed::Record(None), vec![get_miss.clone()]);
    // a/07 (lamp, price 0, no hot): key 4 + kind 8 + price 13 = 25; zzz
    // costs nothing.
    push(
        GET_NAMED,
        Observed::Record(model.get(&a_key(7), &named_kind_price_zzz())),
        vec![read_receipt(
            32,
            &[
                (DirectoryLookup, 1),
                (PointRead, 1),
                (ResultRow, 1),
                (ReadByte, 25),
            ],
        )],
    );
    push(
        GET_ID_ONLY,
        Observed::Record(model.get(&a_key(7), id_only)),
        vec![read_receipt(
            7,
            &[(DirectoryLookup, 1), (PointRead, 1), (ResultRow, 1)],
        )],
    );

    // --- the cursor walk over the book posting (ids 1, 5, 9, ..., 37)
    // Page 1: PostingEntry for 1, 5, 9, 13 and the head 17 beyond the page;
    // rows 48 + 32 + 32 + 36 = 148 bytes. 1 + 5 + 4 + 5 + 12 + 148.
    let (rows1, next1) = model.page(&q_eq_book(), all, 4, None);
    assert_eq!(next1, Some(rid(13)));
    push(
        EQ_PAGE1,
        Observed::Page {
            rows: rows1,
            next: next1,
        },
        vec![read_receipt(
            175,
            &[
                (DirectoryLookup, 1),
                (QueryGroup, 1),
                (TermSeek, 1),
                (PostingEntry, 5),
                (ResultRow, 4),
                (ReadByte, 148),
            ],
        )],
    );
    // Page 2 after 13: entries 17, 21, 25, 29 and the head 33; rows 32 + 44
    // + 36 + 32 = 144. 1 + 5 + 4 + 5 + 12 + 144.
    let (rows2, next2) = model.page(&q_eq_book(), all, 4, next1);
    assert_eq!(next2, Some(rid(29)));
    push(
        EQ_PAGE2,
        Observed::Page {
            rows: rows2,
            next: next2,
        },
        vec![read_receipt(
            171,
            &[
                (DirectoryLookup, 1),
                (QueryGroup, 1),
                (TermSeek, 1),
                (PostingEntry, 5),
                (ResultRow, 4),
                (ReadByte, 144),
            ],
        )],
    );
    // Page 3 after 29: exactly the remaining entries 33, 37 (32 + 36 bytes),
    // then the posting is exhausted without a charge. 1 + 5 + 4 + 2 + 6 + 68.
    let (rows3, next3) = model.page(&q_eq_book(), all, 4, next2);
    assert_eq!(next3, None);
    push(
        EQ_PAGE3,
        Observed::Page {
            rows: rows3,
            next: next3,
        },
        vec![read_receipt(
            86,
            &[
                (DirectoryLookup, 1),
                (QueryGroup, 1),
                (TermSeek, 1),
                (PostingEntry, 2),
                (ResultRow, 2),
                (ReadByte, 68),
            ],
        )],
    );

    // --- one query per operator
    // NotEq: the existence posting of kind:Str holds all 40 ids; every entry
    // is probed by the NotEq literal itself; 30 pass. 1 + 5 + 4 + 40 + 80 + 90.
    push(
        NOT_EQ,
        page_observed(&model, &q_not_eq_pen(), id_only, MAX_PAGE, None),
        vec![read_receipt(
            220,
            &[
                (DirectoryLookup, 1),
                (QueryGroup, 1),
                (TermSeek, 1),
                (PostingEntry, 40),
                (RecordProbe, 40),
                (ResultRow, 30),
            ],
        )],
    );
    // Range [-10, 10): the value terms -10 and 0 are visited (10 terminates
    // the run unpaid); 16 entries, 16 rows of 4 key bytes.
    // 1 + 5 + 4 + 6 + 16 + 48 + 64.
    push(
        RANGE,
        page_observed(&model, &q_range_price(), key_only, MAX_PAGE, None),
        vec![read_receipt(
            144,
            &[
                (DirectoryLookup, 1),
                (QueryGroup, 1),
                (TermSeek, 1),
                (TermVisit, 2),
                (PostingEntry, 16),
                (ResultRow, 16),
                (ReadByte, 64),
            ],
        )],
    );
    // StartsWith "bo": bolt then book are visited, lamp ends the run; 20
    // entries. 1 + 5 + 4 + 6 + 20 + 60.
    push(
        STARTS_WITH,
        page_observed(&model, &q_starts_with_bo(), id_only, MAX_PAGE, None),
        vec![read_receipt(
            96,
            &[
                (DirectoryLookup, 1),
                (QueryGroup, 1),
                (TermSeek, 1),
                (TermVisit, 2),
                (PostingEntry, 20),
                (ResultRow, 20),
            ],
        )],
    );
    // Exists(hot, Bool): the existence posting of 14 ids. 1 + 5 + 4 + 14 + 42.
    push(
        EXISTS,
        page_observed(&model, &q_exists_hot(), id_only, MAX_PAGE, None),
        vec![read_receipt(
            66,
            &[
                (DirectoryLookup, 1),
                (QueryGroup, 1),
                (TermSeek, 1),
                (PostingEntry, 14),
                (ResultRow, 14),
            ],
        )],
    );
    // Negated: every row is visited and probed, 30 pass, 4 key bytes each.
    // 1 + 5 + 40 + 80 + 90 + 120.
    push(
        NEGATED,
        page_observed(&model, &q_negated_book(), key_only, MAX_PAGE, None),
        vec![read_receipt(
            336,
            &[
                (DirectoryLookup, 1),
                (QueryGroup, 1),
                (RowVisit, 40),
                (RecordProbe, 40),
                (ResultRow, 30),
                (ReadByte, 120),
            ],
        )],
    );
    // Match-all: 40 row visits, 40 rows. 1 + 5 + 40 + 120.
    push(
        MATCH_ALL,
        page_observed(&model, &Query::match_all(), id_only, MAX_PAGE, None),
        vec![read_receipt(
            166,
            &[
                (DirectoryLookup, 1),
                (QueryGroup, 1),
                (RowVisit, 40),
                (ResultRow, 40),
            ],
        )],
    );
    // AND: the 10 book entries; the price probe fails for i = 0, 16, 20, 36
    // (1 probe each) and passes for the other six, of which i = 12, 24 fail
    // the hot probe (2 probes each): 4 + 12 = 16 probes, 4 rows (ids 5, 9,
    // 29, 33). 1 + 5 + 4 + 10 + 32 + 12.
    push(
        AND_PROBES,
        page_observed(&model, &q_and_probes(), id_only, MAX_PAGE, None),
        vec![read_receipt(
            64,
            &[
                (DirectoryLookup, 1),
                (QueryGroup, 1),
                (TermSeek, 1),
                (PostingEntry, 10),
                (RecordProbe, 16),
                (ResultRow, 4),
            ],
        )],
    );
    // OR: three groups opened (15 + 12); the pen posting (10 entries) and
    // the price-20 posting (8) need no probe; the $exp posting (8) is probed
    // by Eq(hot, false) and only id 16 passes; 17 distinct rows of 4 key
    // bytes. 1 + 15 + 12 + 26 + 16 + 51 + 68.
    push(
        OR_GROUPS,
        page_observed(&model, &q_or_groups(), key_only, MAX_PAGE, None),
        vec![read_receipt(
            189,
            &[
                (DirectoryLookup, 1),
                (QueryGroup, 3),
                (TermSeek, 3),
                (PostingEntry, 26),
                (RecordProbe, 8),
                (ResultRow, 17),
                (ReadByte, 68),
            ],
        )],
    );
    // Named [kind, hot] over price -20 (i = 0, 5, ..., 35): kind 8 (7 for a
    // pen) plus hot 4 where present: 12 + 8 + 7 + 12 + 8 + 8 + 11 + 8 = 74.
    // 1 + 5 + 4 + 8 + 24 + 74.
    push(
        NAMED_QUERY,
        page_observed(
            &model,
            &q_eq_price_minus_20(),
            &named_kind_hot(),
            MAX_PAGE,
            None,
        ),
        vec![read_receipt(
            116,
            &[
                (DirectoryLookup, 1),
                (QueryGroup, 1),
                (TermSeek, 1),
                (PostingEntry, 8),
                (ResultRow, 8),
                (ReadByte, 74),
            ],
        )],
    );

    // --- misses: a missing term or an empty run costs the seek only, whether
    // the term is held by no one or only by B.
    let empty_page = Observed::Page {
        rows: Vec::new(),
        next: None,
    };
    let miss_receipt = miss(10, &[(DirectoryLookup, 1), (QueryGroup, 1), (TermSeek, 1)]);
    for step in [
        MISS_EQ,
        MISS_FOREIGN_EQ,
        MISS_FOREIGN_PREFIX,
        MISS_FOREIGN_RANGE,
        MISS_NOT_EQ_TYPE,
    ] {
        push(step, empty_page.clone(), vec![miss_receipt.clone()]);
    }

    // --- counts: the same enumeration without row charges
    push(
        COUNT_EQ,
        Observed::Count(model.count(&q_eq_book())),
        vec![read_receipt(
            20,
            &[
                (DirectoryLookup, 1),
                (QueryGroup, 1),
                (TermSeek, 1),
                (PostingEntry, 10),
            ],
        )],
    );
    push(
        COUNT_ALL,
        Observed::Count(model.count(&Query::match_all())),
        vec![read_receipt(
            46,
            &[(DirectoryLookup, 1), (QueryGroup, 1), (RowVisit, 40)],
        )],
    );
    push(
        COUNT_OR,
        Observed::Count(model.count(&q_or_groups())),
        vec![read_receipt(
            70,
            &[
                (DirectoryLookup, 1),
                (QueryGroup, 3),
                (TermSeek, 3),
                (PostingEntry, 26),
                (RecordProbe, 8),
            ],
        )],
    );

    // --- aborted enumerations: every per-entry class is charged just before
    // the entry is used, so the abort point is exact and no partial page is
    // returned.
    let out_of_budget = Observed::Rejected(Rejection::OutOfBudget);
    // Page 1 with budget 100: charge points 1, 6, 10, 11 (entry 1), 62 (row
    // 1: 3 + 48), 63, 98 (row 5), 99 (entry 9); row 9 (35) would reach 134.
    push(
        ABORT_EQ_PAGE,
        out_of_budget.clone(),
        vec![aborted_read(
            99,
            &[
                (DirectoryLookup, 1),
                (QueryGroup, 1),
                (TermSeek, 1),
                (PostingEntry, 3),
                (ResultRow, 2),
                (ReadByte, 80),
            ],
        )],
    );
    // NotEq with budget 30: 10; id 1: 11, 13, 16; id 2: 17, 19, 22; id 3
    // (a pen) 23, 25; id 4: 26, 28; its row (3) would reach 31.
    push(
        ABORT_NOT_EQ,
        out_of_budget.clone(),
        vec![aborted_read(
            28,
            &[
                (DirectoryLookup, 1),
                (QueryGroup, 1),
                (TermSeek, 1),
                (PostingEntry, 4),
                (RecordProbe, 4),
                (ResultRow, 2),
            ],
        )],
    );
    // Range with budget 30: 10, terms 13, 16; entry 17, row 24 (3 + 4);
    // entry 25; the next row (7) would reach 32.
    push(
        ABORT_RANGE_ENTRIES,
        out_of_budget.clone(),
        vec![aborted_read(
            25,
            &[
                (DirectoryLookup, 1),
                (QueryGroup, 1),
                (TermSeek, 1),
                (TermVisit, 2),
                (PostingEntry, 2),
                (ResultRow, 1),
                (ReadByte, 4),
            ],
        )],
    );
    // Range with budget 14: the first term visit reaches 13, the second 16.
    push(
        ABORT_RANGE_TERMS,
        out_of_budget.clone(),
        vec![aborted_read(
            13,
            &[
                (DirectoryLookup, 1),
                (QueryGroup, 1),
                (TermSeek, 1),
                (TermVisit, 1),
            ],
        )],
    );
    // Negated with budget 35: 6; id 1 (a book): 7, 9; id 2: 10, 12, row 19;
    // id 3: 20, 22, row 29; id 4: 30, 32; its row (7) would reach 39.
    push(
        ABORT_NEGATED,
        out_of_budget.clone(),
        vec![aborted_read(
            32,
            &[
                (DirectoryLookup, 1),
                (QueryGroup, 1),
                (RowVisit, 4),
                (RecordProbe, 4),
                (ResultRow, 2),
                (ReadByte, 8),
            ],
        )],
    );
    // Match-all with budget 50: 6 + 4 per row reaches exactly 50 after row
    // 11; the twelfth row visit would reach 51.
    push(
        ABORT_MATCH_ALL,
        out_of_budget.clone(),
        vec![aborted_read(
            50,
            &[
                (DirectoryLookup, 1),
                (QueryGroup, 1),
                (RowVisit, 11),
                (ResultRow, 11),
            ],
        )],
    );
    // StartsWith with budget 22: 10, terms 13, 16; entry 17, row 20, entry
    // 21; the next row would reach 24.
    push(
        ABORT_STARTS_WITH,
        out_of_budget.clone(),
        vec![aborted_read(
            21,
            &[
                (DirectoryLookup, 1),
                (QueryGroup, 1),
                (TermSeek, 1),
                (TermVisit, 2),
                (PostingEntry, 2),
                (ResultRow, 1),
            ],
        )],
    );
    // Exists with budget 12: 10, entry 11; the first row would reach 14.
    push(
        ABORT_EXISTS,
        out_of_budget.clone(),
        vec![aborted_read(
            11,
            &[
                (DirectoryLookup, 1),
                (QueryGroup, 1),
                (TermSeek, 1),
                (PostingEntry, 1),
            ],
        )],
    );
    // OR with budget 38: groups 6, 10, 15, 19, 24, 28; first heads: pen 29,
    // price-20 30, $exp 31 + probe 33 (id 1 fails), 34 + 36 (id 6 fails),
    // 37; the probe of id 11 would reach 39.
    push(
        ABORT_OR,
        out_of_budget.clone(),
        vec![aborted_read(
            37,
            &[
                (DirectoryLookup, 1),
                (QueryGroup, 3),
                (TermSeek, 3),
                (PostingEntry, 5),
                (RecordProbe, 2),
            ],
        )],
    );
    // AND with budget 44: 10; id 1: 11, 13 (fails); id 5: 14, 16, 18, row
    // 21; id 9: 22, 24, 26, row 29; id 13: 30, 32, 34 (fails hot); id 17:
    // 35, 37; id 21: 38, 40; id 25: 41, 43; its second probe would reach 45.
    push(
        ABORT_AND,
        out_of_budget.clone(),
        vec![aborted_read(
            43,
            &[
                (DirectoryLookup, 1),
                (QueryGroup, 1),
                (TermSeek, 1),
                (PostingEntry, 7),
                (RecordProbe, 10),
                (ResultRow, 2),
            ],
        )],
    );
    // count match-all with budget 20: 6 + 14 row visits; the fifteenth would
    // reach 21.
    push(
        ABORT_COUNT_ALL,
        out_of_budget.clone(),
        vec![aborted_read(
            20,
            &[(DirectoryLookup, 1), (QueryGroup, 1), (RowVisit, 14)],
        )],
    );
    // get_record with budget 54: the 51-unit row group does not fit after 4.
    push(
        ABORT_GET,
        out_of_budget.clone(),
        vec![aborted_read(4, &[(DirectoryLookup, 1), (PointRead, 1)])],
    );
    // Page 2 with budget 60: 10, entry 11, row 17 (3 + 32) -> 46, entry 47;
    // row 21 (3 + 44) would reach 94.
    push(
        ABORT_CURSOR_PAGE,
        out_of_budget.clone(),
        vec![aborted_read(
            47,
            &[
                (DirectoryLookup, 1),
                (QueryGroup, 1),
                (TermSeek, 1),
                (PostingEntry, 2),
                (ResultRow, 1),
                (ReadByte, 32),
            ],
        )],
    );
    // Named with budget 40: 10, entry 11, row 1 (3 + 12) -> 26, entry 27,
    // row 6 (3 + 8) -> 38, entry 39; row 11 (3 + 7) would reach 49.
    push(
        ABORT_NAMED,
        out_of_budget.clone(),
        vec![aborted_read(
            39,
            &[
                (DirectoryLookup, 1),
                (QueryGroup, 1),
                (TermSeek, 1),
                (PostingEntry, 3),
                (ResultRow, 2),
                (ReadByte, 20),
            ],
        )],
    );

    // --- writes
    // a/new1: c 4, a 3, b = hot 4 + kind 8 + note 7 + price 13 = 32, row n =
    // key 6 + 32 = 38, no new term: 4 + (20 + 16 + 32 + 38 + RootUpdate 3*2)
    // + 6 * (IndexInsert 6 + RootUpdate 2*2) = 4 + 112 + 60 = 176.
    let new1 = model.create(&key("a/new1"), &new1_cells());
    push(
        CREATE_EXISTING_TERMS,
        Observed::Applied(Effect::RecordCreated { id: new1 }),
        vec![write_receipt(
            176,
            &[
                (DirectoryLookup, 1),
                (PointRead, 1),
                (RecordCreate, 1),
                (CellWrite, 4),
                (CellByte, 32),
                (RowByte, 38),
                (RootUpdate, 3 + 6 * 2),
                (IndexInsert, 6),
            ],
        )],
    );
    // a/new2: c 4, a 3, b = kind 8 + note 7 + price 13 + size 12 = 40, n =
    // 6 + 40 = 46, four new terms: 4 + (20 + 16 + 40 + 46 + 6) + 60 + 120 =
    // 312.
    let new2 = model.create(&key("a/new2"), &new2_cells());
    push(
        CREATE_NEW_TERMS,
        Observed::Applied(Effect::RecordCreated { id: new2 }),
        vec![write_receipt(
            312,
            &[
                (DirectoryLookup, 1),
                (PointRead, 1),
                (RecordCreate, 1),
                (CellWrite, 4),
                (CellByte, 40),
                (RowByte, 46),
                (RootUpdate, 3 + 6 * 2),
                (IndexInsert, 6),
                (IndexTermCreate, 4),
            ],
        )],
    );
    push(
        CREATE_BOUND_KEY,
        Observed::Rejected(Rejection::AlreadyExists),
        vec![rejected_write(
            4,
            Rejection::AlreadyExists,
            &[(DirectoryLookup, 1), (PointRead, 1)],
        )],
    );
    // a/new3 with budget 200: 4, record group {20 + 16 + 40 + RowByte 46 +
    // RootUpdate 6} = 128 -> 132, kind sofa (new, 40) 172, its existence
    // (10) 182; price 777 (new, 40) would reach 222. Nothing is kept.
    push(
        ABORT_CREATE,
        out_of_budget.clone(),
        vec![rejected_write(
            182,
            Rejection::OutOfBudget,
            &[
                (DirectoryLookup, 1),
                (PointRead, 1),
                (RecordCreate, 1),
                (CellWrite, 4),
                (CellByte, 40),
                (RowByte, 46),
                (RootUpdate, 3 + 2 * 2),
                (IndexInsert, 2),
                (IndexTermCreate, 1),
            ],
        )],
    );
    // a/new3 again: the aborted create left no term behind, so sofa, 777 and
    // size 9 are still new (the existence of size is not): 4 + 128 + 60 + 90
    // = 282.
    let new3 = model.create(&key("a/new3"), &new3_cells());
    push(
        CREATE_AFTER_ABORT,
        Observed::Applied(Effect::RecordCreated { id: new3 }),
        vec![write_receipt(
            282,
            &[
                (DirectoryLookup, 1),
                (PointRead, 1),
                (RecordCreate, 1),
                (CellWrite, 4),
                (CellByte, 40),
                (RowByte, 46),
                (RootUpdate, 3 + 6 * 2),
                (IndexInsert, 6),
                (IndexTermCreate, 3),
            ],
        )],
    );
    // a/01 (bolt, -10, note): s 2, b = price 13 + hot 4 = 17, r 2; the row
    // after the patch is key 4 + hot 4 + kind 8 + price 13 = 29 -> group
    // {10 + 8 + 17 + 8 + 29 + RootUpdate 4} = 76; then hot (absent ->
    // false): two inserts of existing terms at 10; note: a field, nothing;
    // price: value -10 removed (10), value 20 inserted (10), the existence
    // term untouched; zzz: nothing. 4 + 76 + 20 + 10 + 10 = 120.
    let patched = model.patch(
        &a_key(1),
        &[
            (name("price"), attr(Value::I64(20))),
            (name("hot"), attr(Value::Bool(false))),
        ],
        &[name("note"), name("zzz")],
    );
    push(
        PATCH,
        Observed::Applied(Effect::RecordPatched { id: patched }),
        vec![write_receipt(
            120,
            &[
                (DirectoryLookup, 1),
                (PointRead, 1),
                (RecordPatch, 1),
                (CellWrite, 2),
                (CellByte, 17),
                (CellRemove, 2),
                (RowByte, 29),
                (RootUpdate, 2 + 4 * 2),
                (IndexInsert, 3),
                (IndexRemove, 1),
            ],
        )],
    );
    // a/02 (pen, price I64 0): s 1, b 13, row n = 4 + kind 7 + note 7 +
    // price 13 = 31 -> group {10 + 4 + 13 + 31 + 4} = 62; the I64 value and
    // existence terms are removed (10 each), the U64 value and existence
    // terms are new (40 each). 4 + 62 + 20 + 80 = 166.
    let retyped = model.patch(&a_key(2), &[(name("price"), attr(Value::U64(5)))], &[]);
    push(
        PATCH_TYPE_CHANGE,
        Observed::Applied(Effect::RecordPatched { id: retyped }),
        vec![write_receipt(
            166,
            &[
                (DirectoryLookup, 1),
                (PointRead, 1),
                (RecordPatch, 1),
                (CellWrite, 1),
                (CellByte, 13),
                (RowByte, 31),
                (RootUpdate, 2 + 4 * 2),
                (IndexRemove, 2),
                (IndexInsert, 2),
                (IndexTermCreate, 2),
            ],
        )],
    );
    push(
        PATCH_MISSING,
        Observed::Rejected(Rejection::NotFound),
        vec![rejected_write(
            4,
            Rejection::NotFound,
            &[(DirectoryLookup, 1), (PointRead, 1)],
        )],
    );
    // a/05 ($exp 2, bolt, note, price) with budget 80: the row after the
    // patch is key 4 + $exp 12 + kind lamp 8 + note 7 + price 13 = 44, so 4 +
    // {10 + 4 + 8 + 44 + RootUpdate 4} = 74; removing bolt (10) would reach
    // 84.
    push(
        ABORT_PATCH,
        out_of_budget.clone(),
        vec![rejected_write(
            74,
            Rejection::OutOfBudget,
            &[
                (DirectoryLookup, 1),
                (PointRead, 1),
                (RecordPatch, 1),
                (CellWrite, 1),
                (CellByte, 8),
                (RowByte, 44),
                (RootUpdate, 2),
            ],
        )],
    );
    // a/03 (hot, lamp, note, price): 4 cells, 3 attributes: 4 + (20 + 16 +
    // RootUpdate 6) + 6 * (IndexRemove 6 + RootUpdate 4) = 106.
    let deleted = model.delete(&a_key(3));
    push(
        DELETE,
        Observed::Applied(Effect::RecordDeleted { id: deleted }),
        vec![write_receipt(
            106,
            &[
                (DirectoryLookup, 1),
                (PointRead, 1),
                (RecordDelete, 1),
                (CellRemove, 4),
                (RootUpdate, 3 + 6 * 2),
                (IndexRemove, 6),
            ],
        )],
    );
    push(
        DELETE_MISSING,
        Observed::Rejected(Rejection::NotFound),
        vec![rejected_write(
            4,
            Rejection::NotFound,
            &[(DirectoryLookup, 1), (PointRead, 1)],
        )],
    );
    // a/06 (hot, pen, note, price) with budget 80: 4, group {20 + 16 + 6}
    // -> 46, three removes at 10 -> 56, 66, 76; the fourth would reach 86.
    // Nothing is removed.
    push(
        ABORT_DELETE,
        out_of_budget.clone(),
        vec![rejected_write(
            76,
            Rejection::OutOfBudget,
            &[
                (DirectoryLookup, 1),
                (PointRead, 1),
                (RecordDelete, 1),
                (CellRemove, 4),
                (RootUpdate, 3 + 3 * 2),
                (IndexRemove, 3),
            ],
        )],
    );
    // The host path refuses a raw put at admission: spent 0, not recorded.
    let not_host = Rejection::InvalidArgument(ArgumentFault::NotHostOperation);
    push(
        HOST_PUT,
        Observed::Rejected(not_host),
        vec![rejected_write(0, not_host, &[])],
    );
    // a/04 (book, note, price 20): 3 cells, 2 attributes: 4 + (20 + 12 + 6)
    // + 4 * 10 = 82.
    let host_deleted = model.delete(&a_key(4));
    push(
        HOST_DELETE,
        Observed::Applied(Effect::RecordDeleted { id: host_deleted }),
        vec![write_receipt(
            82,
            &[
                (DirectoryLookup, 1),
                (PointRead, 1),
                (RecordDelete, 1),
                (CellRemove, 3),
                (RootUpdate, 3 + 4 * 2),
                (IndexRemove, 4),
            ],
        )],
    );
    // Expiry at block 3: the sweep's page is ids 1, 6, 11 ($exp 1, 2, 3).
    let expiring = model.ids(&q_expiry());
    assert_eq!(expiring, vec![rid(1), rid(6), rid(11)]);
    // With a delete budget of 110 the first delete (a/00: 5 cells, 4
    // attributes) charges 4, {20 + 20 + 6} -> 50 and six removes at 10 ->
    // 110; the seventh would reach 120. The halt applies nothing; the
    // rejected delete is recorded with its 110 units.
    push(
        EXPIRE_HALTED,
        Observed::ExpiryHalted {
            applied: Vec::new(),
            reason: Rejection::OutOfBudget,
        },
        vec![rejected_write(
            110,
            Rejection::OutOfBudget,
            &[
                (DirectoryLookup, 1),
                (PointRead, 1),
                (RecordDelete, 1),
                (CellRemove, 5),
                (RootUpdate, 3 + 6 * 2),
                (IndexRemove, 6),
            ],
        )],
    );
    // The full sweep: a/00 (5 cells, 4 attributes) 4 + (20 + 20 + 6) + 8 *
    // 10 = 130; a/05 and a/10 ($exp, kind, note, price) 4 + (20 + 16 + 6) +
    // 6 * 10 = 106 each.
    for id in &expiring {
        let key = &model.records[id].key.clone();
        model.delete(key);
    }
    push(
        EXPIRE,
        Observed::Expired(expiring),
        vec![
            write_receipt(
                130,
                &[
                    (DirectoryLookup, 1),
                    (PointRead, 1),
                    (RecordDelete, 1),
                    (CellRemove, 5),
                    (RootUpdate, 3 + 8 * 2),
                    (IndexRemove, 8),
                ],
            ),
            write_receipt(
                106,
                &[
                    (DirectoryLookup, 1),
                    (PointRead, 1),
                    (RecordDelete, 1),
                    (CellRemove, 4),
                    (RootUpdate, 3 + 6 * 2),
                    (IndexRemove, 6),
                ],
            ),
            write_receipt(
                106,
                &[
                    (DirectoryLookup, 1),
                    (PointRead, 1),
                    (RecordDelete, 1),
                    (CellRemove, 4),
                    (RootUpdate, 3 + 6 * 2),
                    (IndexRemove, 6),
                ],
            ),
        ],
    );

    // --- reads after the writes, on the branch
    assert_eq!(model.records.len(), A_RECORDS_AFTER_WORKLOAD);
    push(
        COUNT_ALL_AFTER,
        Observed::Count(model.count(&Query::match_all())),
        vec![read_receipt(
            44,
            &[(DirectoryLookup, 1), (QueryGroup, 1), (RowVisit, 38)],
        )],
    );
    push(
        COUNT_EXP_AFTER,
        Observed::Count(model.count(&Query::term(Term::exists(name("$exp"), TypeTag::U64)))),
        vec![read_receipt(
            15,
            &[
                (DirectoryLookup, 1),
                (QueryGroup, 1),
                (TermSeek, 1),
                (PostingEntry, 5),
            ],
        )],
    );
    // The branch's mutation serial advanced, so the pre-write cursor is
    // refused at admission: spent 0, receipt issued.
    push(
        STALE_CURSOR,
        Observed::Rejected(Rejection::CursorMismatch),
        vec![receipt(
            ReceiptKind::Read,
            0,
            Outcome::Rejected(Rejection::CursorMismatch),
            &[],
        )],
    );
    push(GET_EXPIRED, Observed::Record(None), vec![get_miss]);
    // book: ids 9, 13, ..., 37 and 41 (ids 1 and 5 are gone): 9 entries.
    push(
        EQ_AFTER,
        page_observed(&model, &q_eq_book(), id_only, MAX_PAGE, None),
        vec![read_receipt(
            46,
            &[
                (DirectoryLookup, 1),
                (QueryGroup, 1),
                (TermSeek, 1),
                (PostingEntry, 9),
                (ResultRow, 9),
            ],
        )],
    );
    // price:U64 holds the single value 5 (id 3). 1 + 5 + 4 + 3 + 1 + 3.
    push(
        RANGE_U64_AFTER,
        page_observed(&model, &q_range_u64_price(), id_only, MAX_PAGE, None),
        vec![read_receipt(
            17,
            &[
                (DirectoryLookup, 1),
                (QueryGroup, 1),
                (TermSeek, 1),
                (TermVisit, 1),
                (PostingEntry, 1),
                (ResultRow, 1),
            ],
        )],
    );
    // sofa: id 43 with its 6-byte key. 1 + 5 + 4 + 1 + 3 + 6.
    push(
        EQ_NEW_TERM_AFTER,
        page_observed(&model, &q_eq_sofa(), key_only, MAX_PAGE, None),
        vec![read_receipt(
            20,
            &[
                (DirectoryLookup, 1),
                (QueryGroup, 1),
                (TermSeek, 1),
                (PostingEntry, 1),
                (ResultRow, 1),
                (ReadByte, 6),
            ],
        )],
    );
    expected
}

// ---------------------------------------------------------------------------
// Assertions
// ---------------------------------------------------------------------------

fn find<'a>(steps: &'a [Step], name: &str) -> &'a Step {
    steps
        .iter()
        .find(|step| step.name == name)
        .unwrap_or_else(|| panic!("workload has no step {name:?}"))
}

fn assert_matches_expected(steps: &[Step], expected: &[Expected], phase: &str) {
    assert_eq!(steps.len(), expected.len(), "{phase}: workload length");
    for (step, want) in steps.iter().zip(expected) {
        assert_eq!(step.name, want.name, "{phase}: workload order");
        assert_eq!(
            step.receipts, want.receipts,
            "{phase}: receipts of {}",
            step.name
        );
        assert_eq!(
            step.observed, want.observed,
            "{phase}: result of {}",
            step.name
        );
    }
}

fn assert_receipts_and_results_equal(baseline: &[Step], rerun: &[Step], phase: &str) {
    assert_eq!(baseline.len(), rerun.len(), "{phase}: workload length");
    for (before, after) in baseline.iter().zip(rerun) {
        assert_eq!(before.name, after.name, "{phase}: workload order");
        assert_eq!(
            after.receipts, before.receipts,
            "{phase}: receipts of {}",
            after.name
        );
        assert_eq!(
            after.observed, before.observed,
            "{phase}: result of {}",
            after.name
        );
    }
}

/// A's own maps are the same objects in every phase, so every key-map,
/// row-map, term-map and posting-map visit count is identical.
fn assert_namespace_local_nodes_equal(baseline: &[Step], rerun: &[Step], phase: &str) {
    for (before, after) in baseline.iter().zip(rerun) {
        let (Some(was), Some(now)) = (before.physical, after.physical) else {
            assert!(before.physical.is_none() && after.physical.is_none());
            continue;
        };
        assert_eq!(
            now.record_nodes, was.record_nodes,
            "{phase}: record nodes of {}",
            after.name
        );
        assert_eq!(
            now.index_nodes, was.index_nodes,
            "{phase}: index nodes of {}",
            after.name
        );
    }
}

fn assert_directory_nodes_equal(baseline: &[Step], rerun: &[Step], phase: &str) {
    for (before, after) in baseline.iter().zip(rerun) {
        let (Some(was), Some(now)) = (before.physical, after.physical) else {
            continue;
        };
        assert_eq!(
            now.directory_nodes, was.directory_nodes,
            "{phase}: directory nodes of {}",
            after.name
        );
    }
}

/// Directory nodes on A's lookup path in phases 0 and 1. The directory is a
/// Merkle Patricia trie keyed by 8-byte big-endian ids (16 nibbles); A and B
/// (ids 1 and 2) share their first 15 nibbles, so the trie is an extension
/// over them, a branch on the last nibble and two leaves: 3 nodes.
const DIRECTORY_PATH_TWO: u64 = 3;

/// Directory nodes on A's lookup path in phase 2, with ids 1 to 502 (0x001
/// to 0x1f6): the ids share 13 zero nibbles (an extension), nibble 13 is 0
/// or 1 (a branch), ids 0x001 to 0x0ff span every value of nibble 14 (a
/// branch) and 0x001 to 0x00f every nonzero value of nibble 15 (a branch)
/// above A's leaf: 5 nodes.
const DIRECTORY_PATH_502: u64 = 5;

/// Any directory path of 8-byte keys: at most `2·8 + 1` = 17 nodes.
const DIRECTORY_PATH_BOUND: u64 = 17;

/// Directory work of each traced step, derived from the trie shape: none
/// when nothing was charged (admission rejections, and a budget that does
/// not cover `DirectoryLookup`, which precedes the lookup), twice the
/// lookup path for an applied write (the lookup, then the insert of the
/// updated catalog leaf along the same path), and the lookup path alone for
/// every read and every write rejected after the lookup.
fn assert_directory_nodes_follow_the_trie(steps: &[Step], path: u64, phase: &str) {
    assert!(path <= DIRECTORY_PATH_BOUND);
    for step in steps {
        let Some(physical) = step.physical else {
            continue;
        };
        assert_eq!(step.receipts.len(), 1, "a traced step has one receipt");
        let receipt = &step.receipts[0];
        let expected = match (receipt.kind, receipt.outcome) {
            _ if receipt.spent == Cost::ZERO => 0,
            (ReceiptKind::Write, Outcome::Applied) => 2 * path,
            _ => path,
        };
        assert_eq!(
            physical.directory_nodes, expected,
            "{phase}: directory nodes of {}",
            step.name
        );
    }
}

/// Which of A's maps each kind of step touches, in every phase.
fn assert_physical_shape(steps: &[Step], phase: &str) {
    let physical = |name| find(steps, name).physical.expect("traced step");
    // Point reads walk the key map and the row map, never the index.
    for name in [GET_ALL, GET_NAMED, GET_ID_ONLY, GET_ABSENT, GET_FOREIGN] {
        assert_eq!(
            physical(name).index_nodes,
            0,
            "{phase}: {name} touches no index node"
        );
        assert!(
            physical(name).record_nodes > 0,
            "{phase}: {name} visits the key map"
        );
    }
    // The row map is the source of match-all and negated groups; probes load
    // rows, so the index is never touched.
    for name in [
        MATCH_ALL,
        NEGATED,
        COUNT_ALL,
        ABORT_MATCH_ALL,
        ABORT_NEGATED,
        ABORT_COUNT_ALL,
    ] {
        assert_eq!(
            physical(name).index_nodes,
            0,
            "{phase}: {name} touches no index node"
        );
        assert!(
            physical(name).record_nodes > 0,
            "{phase}: {name} visits the row map"
        );
    }
    // A miss seeks the term map and loads no row.
    for name in [
        MISS_EQ,
        MISS_FOREIGN_EQ,
        MISS_FOREIGN_PREFIX,
        MISS_FOREIGN_RANGE,
        MISS_NOT_EQ_TYPE,
    ] {
        assert!(
            physical(name).index_nodes > 0,
            "{phase}: {name} seeks the term map"
        );
        assert_eq!(
            physical(name).record_nodes,
            0,
            "{phase}: {name} loads no row"
        );
    }
    // A count over a posting with no probe never loads a row.
    assert_eq!(
        physical(COUNT_EQ).record_nodes,
        0,
        "{phase}: {COUNT_EQ} loads no row"
    );
    assert!(physical(COUNT_EQ).index_nodes > 0);
    // Admission rejections do no physical work at all.
    for name in [HOST_PUT, STALE_CURSOR] {
        assert_eq!(
            physical(name),
            PhysicalTrace::default(),
            "{phase}: {name} is refused before any work"
        );
    }
}

// ---------------------------------------------------------------------------
// Fixture construction
// ---------------------------------------------------------------------------

fn descriptor(id: NamespaceId, owner: OwnerId, revision: u64) -> NamespaceDescriptor {
    NamespaceDescriptor {
        name: arkiv_db::NamespaceName::new("test").unwrap(),
        id,
        owner,
        revision: Revision(revision),
    }
}

fn create_namespace(branch: &mut Branch, owner: OwnerId) -> NamespaceId {
    match branch.execute(
        owner,
        WriteOp::CreateNamespace {
            name: arkiv_db::NamespaceName::new("test").unwrap(),
        },
        Budget::unlimited(),
    ) {
        Ok(Executed {
            effect: Effect::NamespaceCreated(id),
            ..
        }) => id,
        other => panic!("namespace creation failed: {other:?}"),
    }
}

fn apply(branch: &mut Branch, actor: OwnerId, op: WriteOp) -> Effect {
    branch
        .execute(actor, op, Budget::unlimited())
        .expect("fixture write applies")
        .effect
}

/// The engine with namespaces A (alice) and B (bob).
struct World {
    engine: Engine,
    a: NamespaceId,
    b: NamespaceId,
    /// The snapshot published at the end of phase 0.
    phase0: SnapshotId,
    /// B's terms so far, for the newness charge of later creates.
    b_terms: TermLedger,
}

impl World {
    /// Phase 0: one block creates A and B, fills A with 40 typed and 2 raw
    /// records and B with 5 typed records.
    fn publish_phase0() -> Self {
        let mut engine = Engine::new(PriceSchedule::FIXTURE);
        let mut branch = engine.begin(engine.head(), None).expect("genesis is head");
        let a = create_namespace(&mut branch, ALICE);
        let b = create_namespace(&mut branch, BOB);
        let mut expected = 2 * 53;
        let mut a_terms = TermLedger::default();
        for i in 0..A_RECORDS {
            let cells = a_cells(i);
            expected += create_cost(&a_key(i), &cells, a_terms.admit(&cells));
            let effect = apply(&mut branch, ALICE, create_record(a, &a_key(i), cells));
            assert_eq!(
                effect,
                Effect::RecordCreated {
                    id: rid(i as u64 + 1)
                }
            );
        }
        assert_eq!(a_terms.seen.len(), A_TERMS);
        for (key, value) in A_RAW_RECORDS {
            expected += put_cost(key, value);
            assert_eq!(
                apply(&mut branch, ALICE, put(a, key, value)),
                Effect::Put { replaced: false }
            );
        }
        let mut b_terms = TermLedger::default();
        for j in 0..B_INITIAL_RECORDS {
            let cells = b_cells(j);
            expected += create_cost(&b_key(j), &cells, b_terms.admit(&cells));
            let effect = apply(&mut branch, BOB, create_record(b, &b_key(j), cells));
            assert_eq!(
                effect,
                Effect::RecordCreated {
                    id: rid(j as u64 + 1)
                }
            );
        }
        assert_eq!(
            expected, PHASE0_BLOCK_COST,
            "the transcribed table and the hand sum agree"
        );
        let sealed = branch.seal();
        assert_eq!(
            sealed.spent(),
            Cost::new(PHASE0_BLOCK_COST),
            "phase 0 block cost"
        );
        let phase0 = engine
            .publish(&sealed)
            .expect("phase 0 publishes on genesis");
        let world = Self {
            engine,
            a,
            b,
            phase0,
            b_terms,
        };
        let head = world.head();
        assert_eq!(head.id(), phase0);
        assert_eq!(head.namespace_count(), PHASE0_NAMESPACES);
        assert_eq!(head.typed_record_count(a), Some(A_RECORDS));
        assert_eq!(head.term_count(a), Some(A_TERMS));
        assert_eq!(head.record_count(a), Some(A_RAW_RECORDS.len()));
        assert_eq!(head.typed_record_count(b), Some(B_INITIAL_RECORDS));
        assert_eq!(head.index_consistent(a), Some(true));
        assert_eq!(head.index_consistent(b), Some(true));
        world
    }

    /// Phase 1: 10 000 typed records into B across five blocks, then 2 000
    /// raw records in a sixth; no other namespace is touched, none created.
    fn grow_b(&mut self) {
        let per_block = B_GROWTH_RECORDS / B_GROWTH_BLOCKS;
        for (block, &block_cost) in B_GROWTH_BLOCK_COSTS.iter().enumerate() {
            let mut branch = self
                .engine
                .begin(self.engine.head(), None)
                .expect("head is current");
            let first = B_INITIAL_RECORDS + block * per_block;
            let mut expected = 0;
            for j in first..first + per_block {
                let cells = b_cells(j);
                expected += create_cost(&b_key(j), &cells, self.b_terms.admit(&cells));
                let effect = apply(&mut branch, BOB, create_record(self.b, &b_key(j), cells));
                assert_eq!(
                    effect,
                    Effect::RecordCreated {
                        id: rid(j as u64 + 1)
                    },
                    "growth ids are dense"
                );
            }
            assert_eq!(
                expected, block_cost,
                "growth block {block}: table vs literal"
            );
            let sealed = branch.seal();
            assert_eq!(
                sealed.spent(),
                Cost::new(expected),
                "growth block {block} cost"
            );
            self.engine
                .publish(&sealed)
                .expect("growth block publishes");
        }
        let mut branch = self
            .engine
            .begin(self.engine.head(), None)
            .expect("head is current");
        for r in 0..B_RAW_RECORDS {
            let effect = apply(
                &mut branch,
                BOB,
                put(self.b, &b_raw_key(r), &b_raw_value(r)),
            );
            assert_eq!(effect, Effect::Put { replaced: false });
        }
        let sealed = branch.seal();
        assert_eq!(
            sealed.spent(),
            Cost::new(B_RAW_BLOCK_COST),
            "raw block cost"
        );
        self.engine.publish(&sealed).expect("raw block publishes");
        let head = self.head();
        assert_eq!(head.typed_record_count(self.b), Some(B_TOTAL_RECORDS));
        assert_eq!(head.record_count(self.b), Some(B_RAW_RECORDS));
        assert_eq!(head.index_consistent(self.b), Some(true));
    }

    /// Phase 2: one block creates 500 namespaces; no record is written.
    fn create_namespaces(&mut self) {
        let mut branch = self
            .engine
            .begin(self.engine.head(), None)
            .expect("head is current");
        for _ in 0..EXTRA_NAMESPACES {
            create_namespace(&mut branch, DAVE);
        }
        let sealed = branch.seal();
        assert_eq!(
            sealed.spent(),
            Cost::new(PHASE2_BLOCK_COST),
            "phase 2 block cost"
        );
        self.engine
            .publish(&sealed)
            .expect("phase 2 block publishes");
        assert_eq!(self.head().namespace_count(), PHASE2_NAMESPACES);
    }

    fn head(&self) -> Snapshot {
        self.engine.head_snapshot()
    }

    /// The precondition between phases: A's typed maps and raw map are
    /// physically the same objects and its counts and descriptor unchanged.
    fn assert_a_untouched_since(&self, before: &Snapshot, phase: &str) {
        let after = self.head();
        assert!(
            before.typed_shared_with(&after, self.a),
            "{phase}: A's rows, keys and index must be shared with the earlier head"
        );
        assert!(
            before.records_shared_with(&after, self.a),
            "{phase}: A's raw record map must be shared with the earlier head"
        );
        assert_eq!(
            after.typed_record_count(self.a),
            Some(A_RECORDS),
            "{phase}: A's typed size"
        );
        assert_eq!(
            after.term_count(self.a),
            Some(A_TERMS),
            "{phase}: A's term count"
        );
        assert_eq!(
            after.record_count(self.a),
            Some(A_RAW_RECORDS.len()),
            "{phase}: A's raw size"
        );
        assert_eq!(
            after.index_consistent(self.a),
            Some(true),
            "{phase}: A's index"
        );
        let described = after
            .descriptor(self.a, Budget::unlimited())
            .expect("A exists")
            .value;
        assert_eq!(
            described,
            descriptor(self.a, ALICE, 0),
            "{phase}: A's descriptor"
        );
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn baseline_matches_hand_derived_receipts_and_the_reference_model() {
    let world = World::publish_phase0();
    let before = world.head();
    let baseline = run_workload(&world);
    assert_matches_expected(&baseline, &expected_baseline(), "phase 0");
    assert_physical_shape(&baseline, "phase 0");
    assert_directory_nodes_follow_the_trie(&baseline, DIRECTORY_PATH_TWO, "phase 0");
    // The dropped branch changed nothing: not the rows, not the index, not
    // the raw map, not the owner.
    world.assert_a_untouched_since(&before, "phase 0 after the discarded branch");
    assert_eq!(world.head().id(), world.phase0);
    assert_eq!(world.engine.snapshot_count(), 2, "genesis and phase 0 only");
}

#[test]
fn workload_is_unchanged_by_typed_and_raw_growth_of_another_namespace() {
    let mut world = World::publish_phase0();
    let baseline = run_workload(&world);
    let before = world.head();

    world.grow_b();

    let after = world.head();
    assert_eq!(
        after.namespace_count(),
        PHASE0_NAMESPACES,
        "phase 1 creates no namespace"
    );
    assert!(
        !before.typed_shared_with(&after, world.b),
        "B's typed maps did change"
    );
    assert!(
        !before.records_shared_with(&after, world.b),
        "B's raw map did change"
    );
    world.assert_a_untouched_since(&before, "phase 1");

    let rerun = run_workload(&world);
    assert_matches_expected(&rerun, &expected_baseline(), "phase 1 against the literals");
    assert_receipts_and_results_equal(&baseline, &rerun, "phase 1");
    assert_namespace_local_nodes_equal(&baseline, &rerun, "phase 1");
    // Replacing B's state replaces the directory node's value only; the
    // directory keeps its shape, so A's lookup and update paths are the same.
    assert_directory_nodes_equal(&baseline, &rerun, "phase 1");
    assert_physical_shape(&rerun, "phase 1");
    assert_directory_nodes_follow_the_trie(&rerun, DIRECTORY_PATH_TWO, "phase 1");
    world.assert_a_untouched_since(&before, "phase 1 after the discarded branch");
}

#[test]
fn workload_is_unchanged_by_directory_growth_within_the_trie_bound() {
    let mut world = World::publish_phase0();
    let baseline = run_workload(&world);
    world.grow_b();
    let before = world.head();

    world.create_namespaces();

    let after = world.head();
    assert_eq!(
        after.typed_record_count(world.b),
        Some(B_TOTAL_RECORDS),
        "phase 2 writes nothing"
    );
    assert!(
        before.typed_shared_with(&after, world.b),
        "B's typed maps are untouched by namespace creation"
    );
    world.assert_a_untouched_since(&before, "phase 2");

    let rerun = run_workload(&world);
    assert_matches_expected(&rerun, &expected_baseline(), "phase 2 against the literals");
    assert_receipts_and_results_equal(&baseline, &rerun, "phase 2");
    assert_namespace_local_nodes_equal(&baseline, &rerun, "phase 2");
    // The directory did grow, so A's lookup path is longer: the shape for
    // 502 ids, within the 17-node bound of 8-byte keys.
    let namespaces = after.namespace_count();
    assert_eq!(namespaces, PHASE2_NAMESPACES);
    assert_directory_nodes_follow_the_trie(&rerun, DIRECTORY_PATH_502, "phase 2");
    assert_physical_shape(&rerun, "phase 2");
    world.assert_a_untouched_since(&before, "phase 2 after the discarded branch");
}

#[test]
fn pinned_phase0_snapshot_answers_identically_after_later_phases() {
    let mut world = World::publish_phase0();
    let baseline = run_workload(&world);
    world.grow_b();
    world.create_namespaces();

    let historical = world
        .engine
        .snapshot(world.phase0)
        .expect("phase 0 is retained");
    assert_eq!(historical.namespace_count(), PHASE0_NAMESPACES);
    assert_eq!(
        historical.typed_record_count(world.b),
        Some(B_INITIAL_RECORDS)
    );
    let reread = read_workload(&historical, world.a).steps;
    assert_eq!(reread.len(), READ_STEPS);
    for step in &reread {
        let expected = find(&baseline, step.name);
        assert_eq!(
            step.observed, expected.observed,
            "historical result of {}",
            step.name
        );
        assert_eq!(
            step.receipts, expected.receipts,
            "historical receipts of {}",
            step.name
        );
        // The pinned state is the very state the baseline branch was cloned
        // from, so even the physical layout is the same.
        assert_eq!(
            step.physical, expected.physical,
            "historical physical trace of {}",
            step.name
        );
    }
    // The snapshot cursor of page 1 continues on the same snapshot only:
    // the head (a different snapshot of the same engine) refuses it.
    let traced = Budget::unlimited().traced();
    let page1 = historical
        .query(world.a, &q_eq_book(), &Projection::ALL, 4, None, traced)
        .expect("page 1");
    let cursor = page1.value.next.expect("more books follow");
    assert_eq!(cursor.snapshot(), Some(world.phase0));
    let refused = world
        .head()
        .query(
            world.a,
            &q_eq_book(),
            &Projection::ALL,
            4,
            Some(&cursor),
            traced,
        )
        .expect_err("a snapshot cursor binds its snapshot");
    assert_eq!(refused.reason, Rejection::CursorMismatch);
    assert_eq!(
        refused.receipt,
        receipt(
            ReceiptKind::Read,
            0,
            Outcome::Rejected(Rejection::CursorMismatch),
            &[]
        )
    );
    let continued = historical
        .query(
            world.a,
            &q_eq_book(),
            &Projection::ALL,
            4,
            Some(&cursor),
            traced,
        )
        .expect("page 2 on the pinned snapshot");
    assert_eq!(continued.receipt, find(&baseline, EQ_PAGE2).receipts[0]);
    assert!(historical.typed_shared_with(&world.head(), world.a));
}

#[test]
fn identical_terms_of_another_namespace_are_invisible_and_miss_at_fixed_cost() {
    use OperationClass::{
        DirectoryLookup, PointRead, PostingEntry, QueryGroup, ReadByte, ResultRow, ScanEntry,
        ScanSeek, TermSeek, TermVisit,
    };
    let mut world = World::publish_phase0();
    world.grow_b();
    world.create_namespaces();
    let head = world.head();
    let traced = Budget::unlimited().traced();
    let (a, b) = (world.a, world.b);
    let expected = expected_baseline();
    let baseline_receipt = |name: &str| {
        expected
            .iter()
            .find(|want| want.name == name)
            .map(|want| want.receipts[0].clone())
            .expect("baseline step")
    };
    let model = phase0_model();

    // Everything A returns is one of its own 40 ids, while B holds 10 005
    // records under the same attribute names and values.
    let a_all = head
        .query(
            a,
            &Query::match_all(),
            &Projection::ID_ONLY,
            MAX_PAGE,
            None,
            traced,
        )
        .expect("A exists");
    let a_ids: Vec<RecordId> = a_all.value.rows.iter().map(|row| row.id).collect();
    assert_eq!(a_ids, model.ids(&Query::match_all()));
    assert!(a_ids.iter().all(|id| id.ordinal() <= A_RECORDS as u64));
    assert_eq!(a_all.receipt, baseline_receipt(MATCH_ALL));
    assert_eq!(head.typed_record_count(b), Some(B_TOTAL_RECORDS));

    // kind = book: 2502 records in B (j = 0, 4, ..., 10004), 10 in A at
    // the phase-0 price.
    let books_in_b = head.count(b, &q_eq_book(), traced).expect("B exists");
    assert_eq!(books_in_b.value, 2502);
    assert_eq!(
        books_in_b.receipt,
        read_receipt(
            2512,
            &[
                (DirectoryLookup, 1),
                (QueryGroup, 1),
                (TermSeek, 1),
                (PostingEntry, 2502)
            ]
        )
    );
    let books_in_a = head.count(a, &q_eq_book(), traced).expect("A exists");
    assert_eq!(books_in_a.value, 10);
    assert_eq!(books_in_a.receipt, baseline_receipt(COUNT_EQ));

    // seq = 7 exists in B (id 8) and nowhere in A: A misses at the seek
    // price 10, B finds it at 1 + 5 + 4 + 1 + 3.
    let seq_in_b = head
        .query(
            b,
            &q_foreign_eq(),
            &Projection::ID_ONLY,
            MAX_PAGE,
            None,
            traced,
        )
        .expect("B exists");
    assert_eq!(
        seq_in_b.value.rows,
        vec![Row {
            id: rid(8),
            key: None,
            cells: Vec::new()
        }]
    );
    assert_eq!(seq_in_b.value.next, None);
    assert_eq!(
        seq_in_b.receipt,
        read_receipt(
            14,
            &[
                (DirectoryLookup, 1),
                (QueryGroup, 1),
                (TermSeek, 1),
                (PostingEntry, 1),
                (ResultRow, 1)
            ]
        )
    );
    let seq_in_a = head
        .query(
            a,
            &q_foreign_eq(),
            &Projection::ID_ONLY,
            MAX_PAGE,
            None,
            traced,
        )
        .expect("A exists");
    assert!(seq_in_a.value.rows.is_empty());
    assert_eq!(seq_in_a.receipt, baseline_receipt(MISS_FOREIGN_EQ));

    // label starts with "v-00": a thousand distinct terms in B, each visited
    // and each holding one id: 1 + 5 + 4 + 3000 + 1000; an empty run in A.
    let labels_in_b = head
        .count(b, &q_foreign_prefix(), traced)
        .expect("B exists");
    assert_eq!(labels_in_b.value, 1000);
    assert_eq!(
        labels_in_b.receipt,
        read_receipt(
            4010,
            &[
                (DirectoryLookup, 1),
                (QueryGroup, 1),
                (TermSeek, 1),
                (TermVisit, 1000),
                (PostingEntry, 1000)
            ]
        )
    );
    let labels_in_a = head
        .query(
            a,
            &q_foreign_prefix(),
            &Projection::ID_ONLY,
            MAX_PAGE,
            None,
            traced,
        )
        .expect("A exists");
    assert!(labels_in_a.value.rows.is_empty());
    assert_eq!(labels_in_a.receipt, baseline_receipt(MISS_FOREIGN_PREFIX));

    // A key of B through A misses at the fixed miss price; through B it is
    // the record with id 1.
    let through_a = head
        .get_record(a, &b_key(0), &Projection::ID_ONLY, traced)
        .expect("A exists");
    assert_eq!(through_a.value, None);
    assert_eq!(through_a.receipt, baseline_receipt(GET_FOREIGN));
    let through_b = head
        .get_record(b, &b_key(0), &Projection::ID_ONLY, traced)
        .expect("B exists");
    assert_eq!(
        through_b.value,
        Some(Row {
            id: rid(1),
            key: None,
            cells: Vec::new()
        })
    );
    assert_eq!(
        through_b.receipt,
        read_receipt(7, &[(DirectoryLookup, 1), (PointRead, 1), (ResultRow, 1)])
    );
    // The other direction: A's key through B.
    let a_key_through_b = head
        .get_record(b, &a_key(7), &Projection::ALL, traced)
        .expect("B exists");
    assert_eq!(a_key_through_b.value, None);
    assert_eq!(a_key_through_b.receipt, baseline_receipt(GET_ABSENT));

    // A cursor binds its namespace and its projection.
    let page1 = head
        .query(a, &q_eq_book(), &Projection::ALL, 4, None, traced)
        .expect("page 1 in A");
    let cursor = page1.value.next.expect("more books follow");
    let mismatch = receipt(
        ReceiptKind::Read,
        0,
        Outcome::Rejected(Rejection::CursorMismatch),
        &[],
    );
    let in_b = head
        .query(b, &q_eq_book(), &Projection::ALL, 4, Some(&cursor), traced)
        .expect_err("A's cursor is refused by B");
    assert_eq!(in_b.reason, Rejection::CursorMismatch);
    assert_eq!(in_b.receipt, mismatch);
    assert_eq!(in_b.physical, PhysicalTrace::default());
    let other_projection = head
        .query(
            a,
            &q_eq_book(),
            &Projection::ID_ONLY,
            4,
            Some(&cursor),
            traced,
        )
        .expect_err("the projection is bound");
    assert_eq!(other_projection.reason, Rejection::CursorMismatch);
    assert_eq!(other_projection.receipt, mismatch);

    // The raw keyspaces are just as separate, and see nothing of the typed
    // records: A's scan returns its two raw records at 5 + 2 * (2 + 5 + 3),
    // B's `r/` scan its 2000 at 5 + 2000 * (2 + 7 + 9).
    let a_raw = head.scan_prefix(a, b"", traced).expect("A exists");
    assert_eq!(
        a_raw
            .value
            .iter()
            .map(|entry| entry.key.as_slice())
            .collect::<Vec<_>>(),
        vec![b"raw/1".as_slice(), b"raw/2"]
    );
    assert_eq!(
        a_raw.receipt,
        read_receipt(
            25,
            &[
                (DirectoryLookup, 1),
                (ScanSeek, 1),
                (ScanEntry, 2),
                (ReadByte, 16)
            ]
        )
    );
    let b_raw = head.scan_prefix(b, b"r/", traced).expect("B exists");
    assert_eq!(b_raw.value.len(), B_RAW_RECORDS);
    assert_eq!(
        b_raw.receipt,
        read_receipt(
            36_005,
            &[
                (DirectoryLookup, 1),
                (ScanSeek, 1),
                (ScanEntry, 2000),
                (ReadByte, 32_000)
            ]
        )
    );
    let b_raw_through_a = head.scan_prefix(a, b"r/", traced).expect("A exists");
    assert!(b_raw_through_a.value.is_empty());
    assert_eq!(
        b_raw_through_a.receipt,
        read_receipt(5, &[(DirectoryLookup, 1), (ScanSeek, 1)])
    );
}

#[test]
fn expiry_sweeps_only_the_named_namespace() {
    use OperationClass::{DirectoryLookup, PostingEntry, QueryGroup, TermSeek, TermVisit};
    let mut world = World::publish_phase0();
    world.grow_b();
    let before = world.head();
    let (a, b) = (world.a, world.b);
    let traced = Budget::unlimited().traced();
    let unlimited = Budget::unlimited();
    let expiry = name("$exp");
    // Both namespaces hold $exp 1, 2, 3 on their ids 1, 6, 11: the run
    // visits three terms with one entry each. 1 + 5 + 4 + 9 + 3.
    let three = read_receipt(
        22,
        &[
            (DirectoryLookup, 1),
            (QueryGroup, 1),
            (TermSeek, 1),
            (TermVisit, 3),
            (PostingEntry, 3),
        ],
    );
    let none = read_receipt(10, &[(DirectoryLookup, 1), (QueryGroup, 1), (TermSeek, 1)]);
    for namespace in [a, b] {
        let count = before
            .count(namespace, &q_expiry(), traced)
            .expect("exists");
        assert_eq!(count.value, 3);
        assert_eq!(count.receipt, three);
    }

    let mut branch = world
        .engine
        .begin(before.id(), None)
        .expect("head is current");
    // Sweeping A deletes a/00 (130), a/05 and a/10 (106 each) and nothing of
    // B.
    let swept_a = expire_records(&mut branch, a, &expiry, EXPIRY_BLOCK, unlimited, unlimited)
        .expect("A's sweep completes");
    assert_eq!(
        swept_a,
        vec![
            (rid(1), plain_write_receipt(130)),
            (rid(6), plain_write_receipt(106)),
            (rid(11), plain_write_receipt(106)),
        ]
    );
    assert_eq!(branch.typed_record_count(a), Some(A_RECORDS - 3));
    assert_eq!(
        branch.typed_record_count(b),
        Some(B_TOTAL_RECORDS),
        "B is untouched"
    );
    assert_eq!(
        branch
            .count(a, &q_expiry(), traced)
            .expect("A exists")
            .receipt,
        none
    );
    let b_after_a = branch.count(b, &q_expiry(), traced).expect("B exists");
    assert_eq!(b_after_a.value, 3, "B's expiring records are still live");
    assert_eq!(b_after_a.receipt, three);
    assert_eq!(branch.spent(), Cost::new(130 + 106 + 106));

    // Sweeping B deletes its own three: b/00000 has 7 cells and 6 attributes
    // (4 + (20 + 28 + 6) + 12 * 10 = 178), b/00005 and b/00010 6 cells and 5
    // attributes (4 + (20 + 24 + 6) + 10 * 10 = 154).
    let swept_b = expire_records(&mut branch, b, &expiry, EXPIRY_BLOCK, unlimited, unlimited)
        .expect("B's sweep completes");
    assert_eq!(
        swept_b,
        vec![
            (rid(1), plain_write_receipt(178)),
            (rid(6), plain_write_receipt(154)),
            (rid(11), plain_write_receipt(154)),
        ]
    );
    assert_eq!(branch.typed_record_count(b), Some(B_TOTAL_RECORDS - 3));
    assert_eq!(
        branch.typed_record_count(a),
        Some(A_RECORDS - 3),
        "A is untouched"
    );
    assert_eq!(
        branch
            .count(b, &q_expiry(), traced)
            .expect("B exists")
            .receipt,
        none
    );
    assert_eq!(branch.index_consistent(a), Some(true));
    assert_eq!(branch.index_consistent(b), Some(true));
    assert_eq!(branch.receipts().len(), 6);

    // The branch is dropped: both published namespaces are exactly as before.
    drop(branch);
    let after = world.head();
    assert_eq!(after.id(), before.id());
    assert!(before.typed_shared_with(&after, a));
    assert!(before.typed_shared_with(&after, b));
    assert_eq!(after.typed_record_count(a), Some(A_RECORDS));
    assert_eq!(after.typed_record_count(b), Some(B_TOTAL_RECORDS));
}
