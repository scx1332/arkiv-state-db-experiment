//! The typed record model: keys, ids, names, values and cells; the three
//! record writes; and `get_record`, under model version 3.
//!
//! What this file establishes:
//! - The frozen encodings: `RecordKey` (`len ‖ bytes ‖ zero padding`, 33
//!   bytes, keys of 1..=32 bytes), the term bytes of every indexable
//!   `Value`, the `TypeTag` ids and capabilities, and the `CellName`
//!   grammar, checked against literal vectors.
//! - `CreateRecord`, `PatchRecord` and `DeleteRecord` effects and whole
//!   receipts (ledgers included) as hand-derived literals under the FIXTURE
//!   weights, including the charge order under every budget (a budget sweep
//!   with the state observed after each rejection), term newness with
//!   existing terms, and the four patch shapes: value only, type only,
//!   attribute -> field, field -> attribute, plus removal of an absent name.
//! - The post-charge rejections `AlreadyExists`, `NotFound` and `RecordFull`
//!   (spent 4, recorded) and `Unauthorized`/`UnknownNamespace` (spent 1,
//!   recorded), against the admission faults (every `ArgumentFault`,
//!   `ProjectionTooLarge`: spent 0, receipt issued, nothing recorded).
//! - Dense ids are never reused across delete + recreate, and rollback
//!   releases ids and restores rows, keys and terms together.
//! - The host path: `execute_host` applies a record write in a namespace the
//!   caller does not own with a receipt identical to the owner path,
//!   `estimate_host == execute_host`, and every non-record op is
//!   `NotHostOperation` with nothing recorded.
//! - A transfer shares the typed state physically and visits no record or
//!   index node; the raw byte keyspace and the typed records of a namespace
//!   never interact; `get_record` projections charge only projected bytes;
//!   a pinned snapshot answers `get_record` after later patches and deletes.
//!
//! Every expected row and term count comes from a `BTreeMap` reference model
//! of the record semantics, never from the engine's own index; every cost is
//! a literal derived by hand (the derivation is next to it). What it does not
//! establish: query evaluation and its charges beyond a match-all page,
//! cursor binding, expiry, and anything about durability or roots.

use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    fmt::Debug,
};

use arkiv_db::{
    ArgumentFault, Branch, Budget, Cell, CellKind, CellName, CellSelect, Cost,
    DEFAULT_NAMESPACE_CAP, Effect, Engine, Entry, Executed, Ledger, LedgerEntry, Limits, NameFault,
    NamespaceId, OperationClass, Outcome, OwnerId, PhysicalTrace, PriceSchedule, Projection, Query,
    QueryFault, ReadResult, Receipt, ReceiptKind, RecordId, RecordKey, Rejected, Rejection,
    Revision, Row, Snapshot, SnapshotId, TypeTag, Value, WriteOp, WriteResult,
};

const ALICE: OwnerId = OwnerId::new([0xa1; 20]);
const BOB: OwnerId = OwnerId::new([0xb0; 20]);

/// Counting-rule version recorded in every receipt.
const MODEL_VERSION: u32 = 3;
/// The id `PriceSchedule::FIXTURE` carries.
const FIXTURE_ID: u32 = 1;

// ---------------------------------------------------------------------------
// FIXTURE weights, transcribed from the brief (unit prices of the classes
// this file charges). Used for ledger lines and the derivations below.
// ---------------------------------------------------------------------------

const DIRECTORY_LOOKUP: u64 = 1;
const NAMESPACE_TRANSFER: u64 = 20;
const POINT_READ: u64 = 3;
const READ_BYTE: u64 = 1;
const RECORD_CREATE: u64 = 20;
const RECORD_PATCH: u64 = 10;
const RECORD_DELETE: u64 = 20;
const CELL_WRITE: u64 = 4;
const CELL_BYTE: u64 = 1;
const CELL_REMOVE: u64 = 4;
const INDEX_INSERT: u64 = 6;
const INDEX_TERM_CREATE: u64 = 30;
const INDEX_REMOVE: u64 = 6;
const RESULT_ROW: u64 = 3;
const ROOT_UPDATE: u64 = 2;
const ROW_BYTE: u64 = 1;

// ---------------------------------------------------------------------------
// The brief's worked fixture: records a, b, c created in that order, each
// with attributes kind: Str, price: I64, $expiresAt: U64(5) and a field
// note: Bytes("xyz"); kinds book, book, pen; prices 10, 20, 5.
//
// Name order (bytewise, `$` = 0x24 sorts before letters): $expiresAt, kind,
// note, price. Cell bytes = name bytes + encoded value bytes:
//   $expiresAt: 10 + 8 = 18   kind "book": 4 + 4 = 8   kind "pen": 4 + 3 = 7
//   note: 4 + 3 = 7           price: 5 + 8 = 13
// so b(a) = 18 + 8 + 7 + 13 = 46, b(b) = 46, b(c) = 45; c = 4 cells, a = 3
// attribute cells (6 terms: a value term and an existence term each). The
// stored row adds the 1-byte key: n(a) = n(b) = 47, n(c) = 46 (RowByte).
// Model 3 adds RootUpdate 3 to the record group (key binding, row, catalog
// leaf) and RootUpdate 2 to every index group (posting entry, term leaf),
// each unit priced 2.
// ---------------------------------------------------------------------------

/// create a, every term new: 1 + 3 + (20 + 4*4 + 46 + RowByte 47 +
/// RootUpdate 3*2) + 6*(6 + 2*2) + 6*30 = 1 + 3 + 135 + 60 + 180 = 379.
const CREATE_A: u64 = 379;
/// create b, only the value term price=20 new: 1 + 3 + 135 + 60 + 30 = 229.
const CREATE_B: u64 = 229;
/// create c, kind=pen and price=5 new: 1 + 3 + (20 + 16 + 45 + 46 + 6) + 60
/// + 60 = 257.
const CREATE_C: u64 = 257;
/// Distinct terms after a, b, c: kind {exist, book, pen}, price {exist, 10,
/// 20, 5}, $expiresAt {exist, 5} = 9.
const ABC_TERMS: usize = 9;
/// The fixture's namespace has ordinal 1.
const NS: u64 = 1;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn ns_id(ordinal: u64) -> NamespaceId {
    NamespaceId::from_ordinal(ordinal).expect("ordinals start at 1")
}

fn name(text: &str) -> CellName {
    CellName::new(text).unwrap_or_else(|fault| panic!("{text:?} is a valid name: {fault}"))
}

fn key(text: &str) -> RecordKey {
    RecordKey::new(text.as_bytes()).expect("fixture keys are 1..=32 bytes")
}

fn rid(ordinal: u64) -> RecordId {
    RecordId::from_ordinal(ordinal).expect("ids start at 1")
}

fn attr(value: Value) -> Cell {
    Cell::attribute(value)
}

fn field(value: Value) -> Cell {
    Cell::field(value)
}

type Cells = Vec<(CellName, Cell)>;

/// The four cells of a fixture record.
fn fixture_cells(kind: &str, price: i64) -> Cells {
    vec![
        (name("kind"), attr(Value::str(kind))),
        (name("price"), attr(Value::I64(price))),
        (name("$expiresAt"), attr(Value::U64(5))),
        (name("note"), field(Value::bytes(b"xyz"))),
    ]
}

fn create(namespace: NamespaceId, key: RecordKey, cells: Cells) -> WriteOp {
    WriteOp::CreateRecord {
        namespace,
        key,
        cells,
    }
}

fn patch(namespace: NamespaceId, key: RecordKey, set: Cells, remove: Vec<CellName>) -> WriteOp {
    WriteOp::PatchRecord {
        namespace,
        key,
        set,
        remove,
    }
}

fn delete_record(namespace: NamespaceId, key: RecordKey) -> WriteOp {
    WriteOp::DeleteRecord { namespace, key }
}

fn put(namespace: NamespaceId, key: &[u8], value: &[u8]) -> WriteOp {
    WriteOp::Put {
        namespace,
        key: key.to_vec(),
        value: value.to_vec(),
    }
}

fn raw_delete(namespace: NamespaceId, key: &[u8]) -> WriteOp {
    WriteOp::Delete {
        namespace,
        key: key.to_vec(),
    }
}

fn transfer(namespace: NamespaceId, expected_revision: u64, new_owner: OwnerId) -> WriteOp {
    WriteOp::TransferNamespace {
        namespace,
        expected_revision: Revision(expected_revision),
        new_owner,
    }
}

fn receipt(kind: ReceiptKind, spent: u64, outcome: Outcome) -> Receipt {
    Receipt {
        kind,
        model_version: MODEL_VERSION,
        schedule_id: FIXTURE_ID,
        spent: Cost::new(spent),
        outcome,
        ledger: None,
    }
}

fn applied(spent: u64) -> Receipt {
    receipt(ReceiptKind::Write, spent, Outcome::Applied)
}

fn rejected(spent: u64, reason: Rejection) -> Receipt {
    receipt(ReceiptKind::Write, spent, Outcome::Rejected(reason))
}

fn read_applied(spent: u64) -> Receipt {
    receipt(ReceiptKind::Read, spent, Outcome::Applied)
}

fn read_rejected(spent: u64, reason: Rejection) -> Receipt {
    receipt(ReceiptKind::Read, spent, Outcome::Rejected(reason))
}

/// Ledger lines as `(class, count, unit_price)`; the subtotal follows.
fn ledger(lines: &[(OperationClass, u64, u64)]) -> Ledger {
    Ledger {
        entries: lines
            .iter()
            .map(|&(class, count, unit_price)| LedgerEntry {
                class,
                count,
                unit_price,
                subtotal: Cost::new(count * unit_price),
            })
            .collect(),
    }
}

fn traced(receipt: Receipt, lines: &[(OperationClass, u64, u64)]) -> Receipt {
    Receipt {
        ledger: Some(ledger(lines)),
        ..receipt
    }
}

/// An admission rejection: nothing charged, nothing visited.
fn admission(reason: Rejection) -> Rejected {
    Rejected {
        reason,
        receipt: rejected(0, reason),
        physical: PhysicalTrace::default(),
    }
}

/// The canonical part of a write result (effect or reason plus receipt).
type Canonical = Result<(Effect, Receipt), (Rejection, Receipt)>;

fn canonical(result: &WriteResult) -> Canonical {
    match result {
        Ok(executed) => Ok((executed.effect, executed.receipt.clone())),
        Err(rejected) => Err((rejected.reason, rejected.receipt.clone())),
    }
}

fn apply(branch: &mut Branch, actor: OwnerId, op: WriteOp) -> Executed {
    branch
        .execute(actor, op, Budget::unlimited())
        .unwrap_or_else(|rejected| panic!("an unlimited write applies: {rejected}"))
}

fn reject(branch: &mut Branch, actor: OwnerId, op: WriteOp) -> Rejected {
    match branch.execute(actor, op, Budget::unlimited()) {
        Ok(executed) => panic!("expected a rejection, got {:?}", executed.effect),
        Err(rejected) => rejected,
    }
}

fn fresh_branch(engine: &Engine) -> Branch {
    engine
        .begin(engine.head(), None)
        .expect("begin on the current head")
}

/// Snapshot 1: one namespace (ordinal 1, owned by ALICE), no records.
fn fixture() -> (Engine, NamespaceId) {
    let mut engine = Engine::new(PriceSchedule::FIXTURE);
    let mut branch = fresh_branch(&engine);
    let created = apply(
        &mut branch,
        ALICE,
        WriteOp::CreateNamespace {
            name: arkiv_db::NamespaceName::new("test").unwrap(),
        },
    );
    assert_eq!(created.effect, Effect::NamespaceCreated(ns_id(NS)));
    let published = engine
        .publish(&branch.seal())
        .expect("publishes on genesis");
    assert_eq!(published.sequence(), 1);
    (engine, ns_id(NS))
}

/// Creates a, b, c on `branch`, asserting the brief's worked receipts, and
/// mirrors them into `model`.
fn create_abc(branch: &mut Branch, ns: NamespaceId, model: &mut Model) {
    let records = [
        ("a", "book", 10, CREATE_A),
        ("b", "book", 20, CREATE_B),
        ("c", "pen", 5, CREATE_C),
    ];
    for (ordinal, (text, kind, price, cost)) in (1..).zip(records) {
        let cells = fixture_cells(kind, price);
        let executed = apply(branch, ALICE, create(ns, key(text), cells.clone()));
        assert_eq!(executed.effect, Effect::RecordCreated { id: rid(ordinal) });
        assert_eq!(executed.receipt, applied(cost), "create {text}");
        assert_eq!(model.create(key(text), cells), rid(ordinal));
    }
}

/// Snapshot 2: the fixture namespace holding a, b, c (ids 1, 2, 3).
fn fixture_abc() -> (Engine, NamespaceId, Model) {
    let (mut engine, ns) = fixture();
    let mut model = Model::default();
    let mut branch = fresh_branch(&engine);
    create_abc(&mut branch, ns, &mut model);
    let published = engine
        .publish(&branch.seal())
        .expect("publishes on snapshot 1");
    assert_eq!(published.sequence(), 2);
    (engine, ns, model)
}

// ---------------------------------------------------------------------------
// Reference model: the record semantics of the brief over a BTreeMap. It
// knows nothing about charges or the engine's index.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
struct Model {
    next_id: u64,
    live: BTreeMap<RecordKey, (RecordId, BTreeMap<CellName, Cell>)>,
}

impl Model {
    /// Binds an unbound key to the next id and the cells.
    fn create(&mut self, key: RecordKey, cells: Cells) -> RecordId {
        assert!(!self.live.contains_key(&key), "model: {key:?} is bound");
        self.next_id += 1;
        let id = rid(self.next_id);
        let cells: BTreeMap<CellName, Cell> = cells.into_iter().collect();
        self.live.insert(key, (id, cells));
        id
    }

    /// Replaces or adds the set cells, then drops the removed names.
    fn patch(&mut self, key: &RecordKey, set: Cells, remove: &[CellName]) {
        let (_, cells) = self.live.get_mut(key).expect("model: patch of a bound key");
        for (name, cell) in set {
            cells.insert(name, cell);
        }
        for name in remove {
            cells.remove(name);
        }
    }

    /// Unbinds the key; the id is never handed out again.
    fn delete(&mut self, key: &RecordKey) -> RecordId {
        self.live
            .remove(key)
            .expect("model: delete of a bound key")
            .0
    }

    fn row(&self, key: &RecordKey, projection: &Projection) -> Option<Row> {
        let (id, cells) = self.live.get(key)?;
        let cells = match &projection.cells {
            CellSelect::None => Vec::new(),
            CellSelect::All => cells.iter().map(|(n, c)| (n.clone(), c.clone())).collect(),
            CellSelect::Named(names) => cells
                .iter()
                .filter(|(name, _)| names.contains(*name))
                .map(|(n, c)| (n.clone(), c.clone()))
                .collect(),
        };
        Some(Row {
            id: *id,
            key: projection.key.then(|| key.clone()),
            cells,
        })
    }

    /// Every live record in ascending id order: the match-all result.
    fn all_rows(&self, projection: &Projection) -> Vec<Row> {
        let mut rows: Vec<Row> = self
            .live
            .keys()
            .filter_map(|key| self.row(key, projection))
            .collect();
        rows.sort_by_key(|row| row.id);
        rows
    }

    fn record_count(&self) -> usize {
        self.live.len()
    }

    /// Distinct value terms (name, tag, term bytes) plus distinct existence
    /// terms (name, tag) over the attribute cells of live records.
    fn term_count(&self) -> usize {
        let mut values = BTreeSet::new();
        let mut existence = BTreeSet::new();
        for (_, cells) in self.live.values() {
            for (name, cell) in cells {
                if !cell.is_attribute() {
                    continue;
                }
                let bytes = cell.value.term_bytes().expect("attributes are indexable");
                values.insert((name.clone(), cell.value.tag(), bytes));
                existence.insert((name.clone(), cell.value.tag()));
            }
        }
        values.len() + existence.len()
    }
}

/// The unmetered statistics of a namespace's typed state.
#[derive(Clone, Debug, PartialEq, Eq)]
struct TypedStats {
    records: Option<usize>,
    terms: Option<usize>,
    consistent: Option<bool>,
}

/// Reads shared by branches and snapshots.
trait TypedView {
    fn stats(&self, ns: NamespaceId) -> TypedStats;
    fn record(
        &self,
        ns: NamespaceId,
        key: &RecordKey,
        projection: &Projection,
    ) -> ReadResult<Option<Row>>;
    fn raw_count(&self, ns: NamespaceId) -> Option<usize>;
}

impl TypedView for Branch {
    fn stats(&self, ns: NamespaceId) -> TypedStats {
        TypedStats {
            records: self.typed_record_count(ns),
            terms: self.term_count(ns),
            consistent: self.index_consistent(ns),
        }
    }

    fn record(
        &self,
        ns: NamespaceId,
        key: &RecordKey,
        projection: &Projection,
    ) -> ReadResult<Option<Row>> {
        self.get_record(ns, key, projection, Budget::unlimited())
    }

    fn raw_count(&self, ns: NamespaceId) -> Option<usize> {
        self.record_count(ns)
    }
}

impl TypedView for Snapshot {
    fn stats(&self, ns: NamespaceId) -> TypedStats {
        TypedStats {
            records: self.typed_record_count(ns),
            terms: self.term_count(ns),
            consistent: self.index_consistent(ns),
        }
    }

    fn record(
        &self,
        ns: NamespaceId,
        key: &RecordKey,
        projection: &Projection,
    ) -> ReadResult<Option<Row>> {
        self.get_record(ns, key, projection, Budget::unlimited())
    }

    fn raw_count(&self, ns: NamespaceId) -> Option<usize> {
        self.record_count(ns)
    }
}

/// The stats the model implies for a consistent namespace.
fn expected_stats(model: &Model) -> TypedStats {
    TypedStats {
        records: Some(model.record_count()),
        terms: Some(model.term_count()),
        consistent: Some(true),
    }
}

/// Asserts that `view` holds exactly the model's records (by key, full
/// projection) and its statistics.
fn assert_matches_model(view: &impl TypedView, ns: NamespaceId, model: &Model, keys: &[&str]) {
    assert_eq!(view.stats(ns), expected_stats(model));
    for text in keys {
        let read = view
            .record(ns, &key(text), &Projection::ALL)
            .unwrap_or_else(|rejected| panic!("unlimited get_record {text}: {rejected}"));
        assert_eq!(
            read.value,
            model.row(&key(text), &Projection::ALL),
            "record {text}"
        );
    }
}

/// What an operation has spent when its budget runs out: the largest
/// cumulative charge point that still fits (zero before the first one).
fn largest_prefix_sum_at_most(prefix_sums: &[u64], budget: u64) -> u64 {
    prefix_sums
        .iter()
        .copied()
        .filter(|&point| point <= budget)
        .max()
        .unwrap_or(0)
}

/// One write's hand-derived charge vector.
struct WriteVector<'a> {
    label: &'a str,
    op: &'a WriteOp,
    /// Cumulative charge points; the last one is the total.
    prefix_sums: &'a [u64],
    effect: Effect,
}

/// Runs the vector's op as ALICE on a fresh branch of `engine` under every
/// budget in `0..=total`: the receipt is the prefix-sum literal, a rejection
/// leaves the namespace exactly as `before` describes it (checked through
/// `keys`), and success applies the effect with the total.
fn assert_write_sweep(
    engine: &Engine,
    ns: NamespaceId,
    vector: &WriteVector<'_>,
    before: &Model,
    keys: &[&str],
) {
    let WriteVector {
        label,
        op,
        prefix_sums,
        effect,
    } = *vector;
    let total = *prefix_sums.last().expect("a vector has a total");
    for budget in 0..=total {
        let mut branch = fresh_branch(engine);
        let result = branch.execute(ALICE, op.clone(), Budget::limit(budget));
        let expected_spent = largest_prefix_sum_at_most(prefix_sums, budget);
        if budget >= total {
            let executed = result.unwrap_or_else(|rejected| {
                panic!("{label}: rejected at budget {budget}: {rejected}")
            });
            assert_eq!(
                executed.effect, effect,
                "{label}: effect at budget {budget}"
            );
            assert_eq!(
                executed.receipt,
                applied(total),
                "{label}: receipt at budget {budget}"
            );
        } else {
            let rejected = match result {
                Ok(executed) => {
                    panic!("{label}: applied at budget {budget}: {:?}", executed.effect)
                }
                Err(rejected) => rejected,
            };
            assert_eq!(
                rejected.receipt,
                self::rejected(expected_spent, Rejection::OutOfBudget),
                "{label}: receipt at budget {budget}"
            );
            assert_matches_model(&branch, ns, before, keys);
        }
        assert_eq!(
            branch.receipts().len(),
            1,
            "{label}: recorded at budget {budget}"
        );
        assert_eq!(
            branch.spent(),
            Cost::new(expected_spent),
            "{label}: block spent at {budget}"
        );
    }
}

/// Runs a read under every budget in `0..=total` plus unlimited and asserts
/// the whole receipt and, on success, the value.
fn assert_read_sweep<T: Debug + PartialEq>(
    label: &str,
    prefix_sums: &[u64],
    expected: &T,
    read: impl Fn(Budget) -> ReadResult<T>,
) {
    let total = *prefix_sums.last().expect("a vector has a total");
    let budgets = (0..=total).map(Budget::limit).chain([Budget::unlimited()]);
    for budget in budgets {
        let limit = budget.limit_units().map_or(u64::MAX, Cost::units);
        match read(budget) {
            Ok(metered) => {
                assert!(
                    limit >= total,
                    "{label}: applied at budget {limit} below {total}"
                );
                assert_eq!(&metered.value, expected, "{label}: value at budget {limit}");
                assert_eq!(
                    metered.receipt,
                    read_applied(total),
                    "{label}: receipt at {limit}"
                );
            }
            Err(rejected) => {
                assert!(
                    limit < total,
                    "{label}: rejected at budget {limit}: {rejected}"
                );
                let spent = largest_prefix_sum_at_most(prefix_sums, limit);
                assert_eq!(
                    rejected.receipt,
                    read_rejected(spent, Rejection::OutOfBudget),
                    "{label}: receipt at budget {limit}"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 1. The record model: constructors and frozen encodings
// ---------------------------------------------------------------------------

#[test]
fn record_keys_take_1_to_32_bytes_and_encode_as_length_and_bytes() {
    assert_eq!(RecordKey::MAX_LEN, 32);
    assert_eq!(RecordKey::new(b""), None, "empty keys are refused");
    assert_eq!(RecordKey::new(&[7u8; 33]), None, "33-byte keys are refused");
    // The one canonical form, in the codec and as the key-binding trie key:
    // `len ‖ bytes`, no padding. b"a" -> 01 61.
    let short = RecordKey::new(b"a").expect("one byte is a key");
    assert_eq!(short.as_bytes(), b"a");
    assert_eq!(arkiv_db::encode(&short), [0x01, 0x61]);
    // 31 zero bytes differ from one zero byte in the length byte.
    let zeros = RecordKey::new(&[0u8; 31]).expect("31 bytes are a key");
    let one_zero = RecordKey::new(&[0u8]).expect("one zero byte is a key");
    assert_ne!(one_zero, zeros);
    assert_eq!(arkiv_db::encode(&one_zero), [0x01, 0x00]);
    // Every accepted length round-trips.
    for len in 1..=32usize {
        let bytes: Vec<u8> = (0..len).map(|i| u8::try_from(i).unwrap() + 1).collect();
        let key = RecordKey::new(&bytes).unwrap_or_else(|| panic!("{len} bytes are a key"));
        let encoded = arkiv_db::encode(&key);
        assert_eq!(usize::from(encoded[0]), len);
        assert_eq!(&encoded[1..], bytes.as_slice());
        assert_eq!(arkiv_db::decode_exact::<RecordKey>(&encoded).unwrap(), key);
    }
}

#[test]
fn record_ids_are_positive_ordinals() {
    assert_eq!(RecordId::from_ordinal(0), None);
    let seven = rid(7);
    assert_eq!(seven.ordinal(), 7);
    assert_eq!(seven.to_string(), "rec:7");
    assert!(rid(1) < rid(2) && rid(2) < rid(10), "ids order by ordinal");
    assert_eq!(rid(7), seven);
}

#[test]
fn cell_names_follow_the_grammar_and_order_bytewise() {
    assert_eq!(CellName::MAX_LEN, 32);
    for good in [
        "a",
        "Z",
        "A1",
        "$owner",
        "$e",
        "x_y-z.w:v",
        "kind",
        &"n".repeat(20),
    ] {
        let name = CellName::new(good).unwrap_or_else(|fault| panic!("{good:?}: {fault}"));
        assert_eq!(name.as_str(), good);
        assert_eq!(name.len(), good.len());
        assert!(!name.is_empty());
    }
    assert_eq!(CellName::new(""), Err(NameFault::Empty));
    assert_eq!(
        CellName::new(&"n".repeat(CellName::MAX_LEN + 1)),
        Err(NameFault::TooLong)
    );
    for bad in [
        "$", "$$a", "$1", "$_", "1a", "_a", "-a", ".a", ":a", "a b", " a", "a#b", "a@b", "é",
        "a/b", "a\0", "a\n",
    ] {
        assert_eq!(CellName::new(bad), Err(NameFault::Grammar), "{bad:?}");
    }
    // Case-sensitive, no normalisation; Ord is bytewise so upper case sorts first.
    assert_ne!(name("Kind"), name("kind"));
    assert!(name("B") < name("a"));
    assert!(
        name("$z") < name("a"),
        "`$` (0x24) sorts before every letter"
    );
    assert!(name("abc") < name("abd"));
    assert!(name("ab") < name("abc"));
}

#[test]
fn type_tags_have_frozen_ids_and_capabilities() {
    let ids: Vec<u8> = TypeTag::ALL[..4].iter().map(|tag| tag.id()).collect();
    assert_eq!(ids, vec![1, 2, 3, 4]);
    assert_eq!(
        [
            TypeTag::Bool,
            TypeTag::Str,
            TypeTag::Bytes,
            TypeTag::U64,
            TypeTag::I64
        ]
        .map(TypeTag::id),
        [1, 2, 3, 13, 17]
    );
    let indexable: Vec<TypeTag> = TypeTag::ALL
        .into_iter()
        .filter(|t| !t.is_indexable())
        .collect();
    assert_eq!(indexable, vec![TypeTag::Bytes]);
    // Ordered is the same set as indexable: every attribute type.
    for tag in TypeTag::ALL {
        assert_eq!(tag.is_ordered(), tag.is_indexable(), "{tag}");
    }
    let prefix: Vec<TypeTag> = TypeTag::ALL
        .into_iter()
        .filter(|t| t.supports_prefix())
        .collect();
    assert_eq!(prefix, vec![TypeTag::Str]);
}

#[test]
fn values_have_frozen_term_bytes_and_encoded_lengths() {
    let term = |value: &Value| value.term_bytes().expect("indexable");
    assert_eq!(
        term(&Value::I64(-1)),
        vec![0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]
    );
    assert_eq!(term(&Value::I64(0)), vec![0x80, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(term(&Value::I64(i64::MIN)), vec![0u8; 8]);
    assert_eq!(term(&Value::I64(i64::MAX)), vec![0xff; 8]);
    assert_eq!(term(&Value::U64(258)), vec![0, 0, 0, 0, 0, 0, 0x01, 0x02]);
    assert_eq!(term(&Value::str("ab")), vec![0x61, 0x62]);
    assert_eq!(term(&Value::Bool(false)), vec![0x00]);
    assert_eq!(term(&Value::Bool(true)), vec![0x01]);
    assert_eq!(Value::bytes(b"ab").term_bytes(), None, "Bytes has no term");

    // The I64 encoding orders as the integers do: sort a sample by term bytes
    // and compare with the numeric order, independently of the engine.
    let sample = [i64::MAX, 1, i64::MIN, -1, 0, -2, 2, 1 << 40, -(1 << 40)];
    let mut by_bytes: Vec<i64> = sample.to_vec();
    by_bytes.sort_by_key(|value| term(&Value::I64(*value)));
    let mut numeric = sample.to_vec();
    numeric.sort_unstable();
    assert_eq!(by_bytes, numeric);

    assert_eq!(Value::Bool(true).encoded_len(), 1);
    assert_eq!(Value::I64(-5).encoded_len(), 8);
    assert_eq!(Value::U64(5).encoded_len(), 8);
    assert_eq!(Value::str("").encoded_len(), 0);
    assert_eq!(
        Value::str("héllo").encoded_len(),
        6,
        "UTF-8 byte length, not chars"
    );
    assert_eq!(Value::bytes(b"xyz").encoded_len(), 3);

    assert_eq!(Value::Bool(true).tag(), TypeTag::Bool);
    assert_eq!(Value::I64(1).tag(), TypeTag::I64);
    assert_eq!(Value::U64(1).tag(), TypeTag::U64);
    assert_eq!(Value::str("a").tag(), TypeTag::Str);
    assert_eq!(Value::bytes(b"a").tag(), TypeTag::Bytes);

    assert_eq!(
        Value::I64(-1).cmp_ordered(&Value::I64(1)),
        Some(Ordering::Less)
    );
    assert_eq!(
        Value::U64(2).cmp_ordered(&Value::U64(2)),
        Some(Ordering::Equal)
    );
    assert_eq!(
        Value::I64(1).cmp_ordered(&Value::U64(1)),
        None,
        "tags differ"
    );
    assert_eq!(
        Value::str("a").cmp_ordered(&Value::str("b")),
        Some(Ordering::Less),
        "Str orders by its UTF-8 bytes"
    );
    assert_eq!(
        Value::Bool(false).cmp_ordered(&Value::Bool(true)),
        Some(Ordering::Less)
    );
    assert_eq!(Value::bytes(b"a").cmp_ordered(&Value::bytes(b"b")), None);
    assert_ne!(
        Value::I64(1),
        Value::U64(1),
        "values of different types never compare equal"
    );
}

#[test]
fn cells_carry_a_kind_and_a_value() {
    let indexed = attr(Value::str("book"));
    assert_eq!(indexed.kind, CellKind::Attribute);
    assert_eq!(indexed.value, Value::str("book"));
    assert!(indexed.is_attribute());
    let stored = field(Value::bytes(b"xyz"));
    assert_eq!(stored.kind, CellKind::Field);
    assert!(!stored.is_attribute());
    assert_ne!(
        attr(Value::I64(1)),
        field(Value::I64(1)),
        "kind is part of the cell"
    );
}

#[test]
fn limits_are_fixed_per_engine() {
    let defaults = Limits {
        namespace_cap: 1 << 32,
        max_cells: 64,
        max_str_len: 4096,
        max_bytes_len: 4096,
        max_page: 200,
        max_groups: 8,
        max_literals: 8,
        max_cell_name_len: CellName::MAX_LEN,
        max_indexed_len: arkiv_db::Limits::DEFAULT_INDEXED_LEN,
    };
    assert_eq!(Limits::DEFAULT, defaults);
    assert_eq!(DEFAULT_NAMESPACE_CAP, 1 << 32);
    assert_eq!(Engine::new(PriceSchedule::FIXTURE).limits(), defaults);
    assert_eq!(
        Engine::with_namespace_cap(PriceSchedule::FIXTURE, 5).limits(),
        Limits {
            namespace_cap: 5,
            ..defaults
        }
    );
    let custom = Limits {
        max_cells: 4,
        max_page: 3,
        ..defaults
    };
    let engine = Engine::with_limits(PriceSchedule::FIXTURE, custom);
    assert_eq!(engine.limits(), custom);
    assert_eq!(fresh_branch(&engine).limits(), custom);
}

// ---------------------------------------------------------------------------
// 2. Write vectors: effects, whole receipts and charge order
// ---------------------------------------------------------------------------

const ABC_KEYS: [&str; 3] = ["a", "b", "c"];

#[test]
fn create_with_every_term_new_charges_lookup_record_group_then_each_term() {
    let (engine, ns) = fixture();
    let op = create(ns, key("a"), fixture_cells("book", 10));
    // DirectoryLookup 1 -> 1; PointRead 3 -> 4; {RecordCreate 20 + CellWrite
    // 4*4 + CellByte 46 + RowByte 47 + RootUpdate 3*2} = 135 -> 139; then six
    // new terms in name order ($expiresAt value, existence; kind value,
    // existence; price value, existence), each {IndexInsert 6 +
    // IndexTermCreate 30 + RootUpdate 2*2} = 40: 179, 219, 259, 299, 339, 379.
    assert_write_sweep(
        &engine,
        ns,
        &WriteVector {
            label: "create a",
            op: &op,
            prefix_sums: &[1, 4, 139, 179, 219, 259, 299, 339, 379],
            effect: Effect::RecordCreated { id: rid(1) },
        },
        &Model::default(),
        &["a"],
    );

    let mut branch = fresh_branch(&engine);
    let executed = branch
        .execute(ALICE, op, Budget::unlimited().traced())
        .expect("applies");
    assert_eq!(
        executed.receipt,
        traced(
            applied(CREATE_A),
            &[
                (OperationClass::DirectoryLookup, 1, DIRECTORY_LOOKUP),
                (OperationClass::PointRead, 1, POINT_READ),
                (OperationClass::RecordCreate, 1, RECORD_CREATE),
                (OperationClass::CellWrite, 4, CELL_WRITE),
                (OperationClass::CellByte, 46, CELL_BYTE),
                (OperationClass::RowByte, 47, ROW_BYTE),
                // 3 (record group) + 6 * 2 (index groups).
                (OperationClass::RootUpdate, 15, ROOT_UPDATE),
                (OperationClass::IndexInsert, 6, INDEX_INSERT),
                (OperationClass::IndexTermCreate, 6, INDEX_TERM_CREATE),
            ],
        )
    );
    let mut model = Model::default();
    model.create(key("a"), fixture_cells("book", 10));
    assert_matches_model(&branch, ns, &model, &["a", "b"]);
    assert_eq!(
        branch.term_count(ns),
        Some(6),
        "six terms, all created by a"
    );
    assert_eq!(
        branch.record_count(ns),
        Some(0),
        "the raw keyspace is untouched"
    );
}

#[test]
fn create_with_existing_terms_charges_creation_only_for_the_new_one() {
    let (mut engine, ns) = fixture();
    let mut model = Model::default();
    let mut branch = fresh_branch(&engine);
    apply(
        &mut branch,
        ALICE,
        create(ns, key("a"), fixture_cells("book", 10)),
    );
    model.create(key("a"), fixture_cells("book", 10));
    engine.publish(&branch.seal()).expect("publishes a");

    // create b after a: 1 -> 4 -> 139 as for a; then in name order
    // $expiresAt value (held by a: {IndexInsert 6 + RootUpdate 2*2} = 10) 149,
    // existence 159, kind=book 169, kind existence 179, price=20 (new: 40)
    // 219, price existence 229.
    let op = create(ns, key("b"), fixture_cells("book", 20));
    assert_write_sweep(
        &engine,
        ns,
        &WriteVector {
            label: "create b",
            op: &op,
            prefix_sums: &[1, 4, 139, 149, 159, 169, 179, 219, 229],
            effect: Effect::RecordCreated { id: rid(2) },
        },
        &model,
        &["a", "b"],
    );

    let mut branch = fresh_branch(&engine);
    let executed = branch
        .execute(ALICE, op, Budget::unlimited().traced())
        .expect("applies");
    assert_eq!(
        executed.receipt,
        traced(
            applied(CREATE_B),
            &[
                (OperationClass::DirectoryLookup, 1, DIRECTORY_LOOKUP),
                (OperationClass::PointRead, 1, POINT_READ),
                (OperationClass::RecordCreate, 1, RECORD_CREATE),
                (OperationClass::CellWrite, 4, CELL_WRITE),
                (OperationClass::CellByte, 46, CELL_BYTE),
                (OperationClass::RowByte, 47, ROW_BYTE),
                (OperationClass::RootUpdate, 15, ROOT_UPDATE),
                (OperationClass::IndexInsert, 6, INDEX_INSERT),
                (OperationClass::IndexTermCreate, 1, INDEX_TERM_CREATE),
            ],
        )
    );
    model.create(key("b"), fixture_cells("book", 20));
    assert_matches_model(&branch, ns, &model, &["a", "b"]);
    assert_eq!(branch.term_count(ns), Some(7));

    // create c: kind=pen and price=5 are new; b(c) = 45.
    let executed = apply(
        &mut branch,
        ALICE,
        create(ns, key("c"), fixture_cells("pen", 5)),
    );
    assert_eq!(executed.effect, Effect::RecordCreated { id: rid(3) });
    assert_eq!(executed.receipt, applied(CREATE_C));
    model.create(key("c"), fixture_cells("pen", 5));
    assert_matches_model(&branch, ns, &model, &ABC_KEYS);
    assert_eq!(branch.term_count(ns), Some(ABC_TERMS));
}

#[test]
fn patch_changing_only_the_value_replaces_the_value_term_and_keeps_existence() {
    let (engine, ns, mut model) = fixture_abc();
    // set price := I64(11) on a: 1 -> 4 -> {RecordPatch 10 + CellWrite 4 +
    // CellByte (5 + 8) = 13 + RowByte 47 (the row keeps 46 cell bytes plus
    // key 1) + RootUpdate 2*2} = 78 -> 82; price: old [I64 10, I64 exist],
    // new [I64 11, I64 exist]: {IndexRemove 6 + RootUpdate 4} -> 92;
    // {IndexInsert 6 + IndexTermCreate 30 + RootUpdate 4} (price=11 is new)
    // -> 132. The existence term is untouched.
    let set = vec![(name("price"), attr(Value::I64(11)))];
    let op = patch(ns, key("a"), set.clone(), vec![]);
    assert_write_sweep(
        &engine,
        ns,
        &WriteVector {
            label: "patch value only",
            op: &op,
            prefix_sums: &[1, 4, 82, 92, 132],
            effect: Effect::RecordPatched { id: rid(1) },
        },
        &model,
        &ABC_KEYS,
    );

    let mut branch = fresh_branch(&engine);
    let executed = branch
        .execute(ALICE, op, Budget::unlimited().traced())
        .expect("applies");
    assert_eq!(
        executed.receipt,
        traced(
            applied(132),
            &[
                (OperationClass::DirectoryLookup, 1, DIRECTORY_LOOKUP),
                (OperationClass::PointRead, 1, POINT_READ),
                (OperationClass::RecordPatch, 1, RECORD_PATCH),
                (OperationClass::CellWrite, 1, CELL_WRITE),
                (OperationClass::CellByte, 13, CELL_BYTE),
                (OperationClass::RowByte, 47, ROW_BYTE),
                (OperationClass::RootUpdate, 6, ROOT_UPDATE),
                (OperationClass::IndexRemove, 1, INDEX_REMOVE),
                (OperationClass::IndexInsert, 1, INDEX_INSERT),
                (OperationClass::IndexTermCreate, 1, INDEX_TERM_CREATE),
            ],
        )
    );
    model.patch(&key("a"), set, &[]);
    assert_matches_model(&branch, ns, &model, &ABC_KEYS);
    assert_eq!(
        branch.term_count(ns),
        Some(ABC_TERMS),
        "price=10 dropped, price=11 added"
    );

    // Setting the value the cell already holds is a charged no-op for the
    // index: 1 + 3 + 78 = 82 and nothing else (the row is still rewritten).
    let same = vec![(name("price"), attr(Value::I64(11)))];
    let executed = branch
        .execute(
            ALICE,
            patch(ns, key("a"), same.clone(), vec![]),
            Budget::unlimited().traced(),
        )
        .expect("applies");
    assert_eq!(executed.effect, Effect::RecordPatched { id: rid(1) });
    assert_eq!(
        executed.receipt,
        traced(
            applied(82),
            &[
                (OperationClass::DirectoryLookup, 1, DIRECTORY_LOOKUP),
                (OperationClass::PointRead, 1, POINT_READ),
                (OperationClass::RecordPatch, 1, RECORD_PATCH),
                (OperationClass::CellWrite, 1, CELL_WRITE),
                (OperationClass::CellByte, 13, CELL_BYTE),
                (OperationClass::RowByte, 47, ROW_BYTE),
                (OperationClass::RootUpdate, 2, ROOT_UPDATE),
            ],
        )
    );
    model.patch(&key("a"), same, &[]);
    assert_matches_model(&branch, ns, &model, &ABC_KEYS);
}

#[test]
fn patch_changing_the_type_replaces_both_terms_and_removes_are_charged_per_name() {
    let (engine, ns, mut model) = fixture_abc();
    // The brief's worked vector. set price := U64(10), remove [note,
    // missing] on a: 1 -> 4 -> {RecordPatch 10 + CellWrite 4 + CellByte 13 +
    // CellRemove 2*4 + RowByte 40 (key 1 + $expiresAt 18 + kind 8 + price
    // 13) + RootUpdate 2*2} = 79 -> 83; names in order: missing (absent:
    // nothing), note (field: nothing), price: old [I64 10, I64 exist] - new
    // [U64 10, U64 exist]: {IndexRemove 6 + RootUpdate 4} (value) -> 93,
    // (existence) -> 103; new - old: {6 + 30 + 4} (U64 10 is new) -> 143,
    // {6 + 30 + 4} (U64 existence is new) -> 183.
    let set = vec![(name("price"), attr(Value::U64(10)))];
    let remove = vec![name("note"), name("missing")];
    let op = patch(ns, key("a"), set.clone(), remove.clone());
    assert_write_sweep(
        &engine,
        ns,
        &WriteVector {
            label: "patch type only",
            op: &op,
            prefix_sums: &[1, 4, 83, 93, 103, 143, 183],
            effect: Effect::RecordPatched { id: rid(1) },
        },
        &model,
        &ABC_KEYS,
    );

    let mut branch = fresh_branch(&engine);
    let executed = branch
        .execute(ALICE, op, Budget::unlimited().traced())
        .expect("applies");
    assert_eq!(
        executed.receipt,
        traced(
            applied(183),
            &[
                (OperationClass::DirectoryLookup, 1, DIRECTORY_LOOKUP),
                (OperationClass::PointRead, 1, POINT_READ),
                (OperationClass::RecordPatch, 1, RECORD_PATCH),
                (OperationClass::CellWrite, 1, CELL_WRITE),
                (OperationClass::CellByte, 13, CELL_BYTE),
                (OperationClass::CellRemove, 2, CELL_REMOVE),
                (OperationClass::RowByte, 40, ROW_BYTE),
                // 2 (record group) + 4 * 2 (index groups).
                (OperationClass::RootUpdate, 10, ROOT_UPDATE),
                (OperationClass::IndexRemove, 2, INDEX_REMOVE),
                (OperationClass::IndexInsert, 2, INDEX_INSERT),
                (OperationClass::IndexTermCreate, 2, INDEX_TERM_CREATE),
            ],
        )
    );
    model.patch(&key("a"), set, &remove);
    assert_matches_model(&branch, ns, &model, &ABC_KEYS);
    // price=10 (I64) was only a's and is dropped; the I64 existence term
    // survives through b and c; two U64 terms appear: 9 - 1 + 2.
    assert_eq!(branch.term_count(ns), Some(10));
    let row = branch
        .record(ns, &key("a"), &Projection::ALL)
        .unwrap()
        .value
        .unwrap();
    assert_eq!(row.cells.len(), 3, "note is gone, missing never existed");
}

#[test]
fn patch_turning_an_attribute_into_a_field_removes_both_terms() {
    let (engine, ns, mut model) = fixture_abc();
    // set kind := field Str("pen") on c (same value, kind changes): 1 -> 4 ->
    // {RecordPatch 10 + CellWrite 4 + CellByte (4 + 3) + RowByte 46 (c's row
    // keeps its 45 cell bytes plus key 1) + RootUpdate 2*2} = 71 -> 75; kind:
    // old [pen, Str exist], new []: {IndexRemove 6 + RootUpdate 4} -> 85,
    // -> 95.
    let set = vec![(name("kind"), field(Value::str("pen")))];
    let op = patch(ns, key("c"), set.clone(), vec![]);
    assert_write_sweep(
        &engine,
        ns,
        &WriteVector {
            label: "patch attribute -> field",
            op: &op,
            prefix_sums: &[1, 4, 75, 85, 95],
            effect: Effect::RecordPatched { id: rid(3) },
        },
        &model,
        &ABC_KEYS,
    );

    let mut branch = fresh_branch(&engine);
    let executed = branch
        .execute(ALICE, op, Budget::unlimited().traced())
        .expect("applies");
    assert_eq!(
        executed.receipt,
        traced(
            applied(95),
            &[
                (OperationClass::DirectoryLookup, 1, DIRECTORY_LOOKUP),
                (OperationClass::PointRead, 1, POINT_READ),
                (OperationClass::RecordPatch, 1, RECORD_PATCH),
                (OperationClass::CellWrite, 1, CELL_WRITE),
                (OperationClass::CellByte, 7, CELL_BYTE),
                (OperationClass::RowByte, 46, ROW_BYTE),
                (OperationClass::RootUpdate, 6, ROOT_UPDATE),
                (OperationClass::IndexRemove, 2, INDEX_REMOVE),
            ],
        )
    );
    model.patch(&key("c"), set, &[]);
    assert_matches_model(&branch, ns, &model, &ABC_KEYS);
    // kind=pen was only c's and is dropped; kind's Str existence term stays
    // through a and b: 9 - 1.
    assert_eq!(branch.term_count(ns), Some(8));
}

#[test]
fn patch_turning_a_field_into_an_attribute_inserts_both_terms() {
    let (engine, ns, mut model) = fixture_abc();
    // set note := attribute Str("memo") on a: 1 -> 4 -> {RecordPatch 10 +
    // CellWrite 4 + CellByte (4 + 4) + RowByte 48 (key 1 + $expiresAt 18 +
    // kind 8 + note 8 + price 13) + RootUpdate 2*2} = 74 -> 78; note: old []
    // (field), new [memo, Str exist], both new: {6 + 30 + 4} -> 118,
    // {6 + 30 + 4} -> 158.
    let set = vec![(name("note"), attr(Value::str("memo")))];
    let op = patch(ns, key("a"), set.clone(), vec![]);
    assert_write_sweep(
        &engine,
        ns,
        &WriteVector {
            label: "patch field -> attribute",
            op: &op,
            prefix_sums: &[1, 4, 78, 118, 158],
            effect: Effect::RecordPatched { id: rid(1) },
        },
        &model,
        &ABC_KEYS,
    );

    let mut branch = fresh_branch(&engine);
    let executed = branch
        .execute(ALICE, op, Budget::unlimited().traced())
        .expect("applies");
    assert_eq!(
        executed.receipt,
        traced(
            applied(158),
            &[
                (OperationClass::DirectoryLookup, 1, DIRECTORY_LOOKUP),
                (OperationClass::PointRead, 1, POINT_READ),
                (OperationClass::RecordPatch, 1, RECORD_PATCH),
                (OperationClass::CellWrite, 1, CELL_WRITE),
                (OperationClass::CellByte, 8, CELL_BYTE),
                (OperationClass::RowByte, 48, ROW_BYTE),
                (OperationClass::RootUpdate, 6, ROOT_UPDATE),
                (OperationClass::IndexInsert, 2, INDEX_INSERT),
                (OperationClass::IndexTermCreate, 2, INDEX_TERM_CREATE),
            ],
        )
    );
    model.patch(&key("a"), set, &[]);
    assert_matches_model(&branch, ns, &model, &ABC_KEYS);
    assert_eq!(branch.term_count(ns), Some(ABC_TERMS + 2));
}

#[test]
fn patch_removing_names_charges_each_name_and_only_held_terms() {
    let (engine, ns, mut model) = fixture_abc();
    let mut branch = fresh_branch(&engine);
    // The lines every remove-only patch charges; the stored row after the
    // patch sets its RowByte count.
    let base_lines = |row_bytes: u64| {
        [
            (OperationClass::DirectoryLookup, 1, DIRECTORY_LOOKUP),
            (OperationClass::PointRead, 1, POINT_READ),
            (OperationClass::RecordPatch, 1, RECORD_PATCH),
            (OperationClass::CellRemove, 1, CELL_REMOVE),
            (OperationClass::RowByte, row_bytes, ROW_BYTE),
            (OperationClass::RootUpdate, 2, ROOT_UPDATE),
        ]
    };
    // remove [missing] on a: 1 + 3 + {RecordPatch 10 + CellRemove 4 +
    // RowByte 47 (the unchanged row) + RootUpdate 2*2} = 69; an absent name
    // is a charged no-op and the row is unchanged.
    let executed = branch
        .execute(
            ALICE,
            patch(ns, key("a"), vec![], vec![name("missing")]),
            Budget::unlimited().traced(),
        )
        .expect("applies");
    assert_eq!(executed.effect, Effect::RecordPatched { id: rid(1) });
    assert_eq!(executed.receipt, traced(applied(69), &base_lines(47)));
    assert_matches_model(&branch, ns, &model, &ABC_KEYS);

    // remove [note] (a field): no index work; the row shrinks to 47 - 7 =
    // 40 bytes: 1 + 3 + {10 + 4 + 40 + 4} = 62.
    let executed = branch
        .execute(
            ALICE,
            patch(ns, key("a"), vec![], vec![name("note")]),
            Budget::unlimited().traced(),
        )
        .expect("applies");
    assert_eq!(executed.receipt, traced(applied(62), &base_lines(40)));
    model.patch(&key("a"), vec![], &[name("note")]);
    assert_matches_model(&branch, ns, &model, &ABC_KEYS);
    assert_eq!(branch.term_count(ns), Some(ABC_TERMS));

    // remove [kind] (an attribute): the row shrinks to 40 - 8 = 32 bytes:
    // 1 + 3 + {10 + 4 + 32 + 4} = 54, then {IndexRemove 6 + RootUpdate 4}
    // twice -> 74; kind=book and the Str existence term survive through b.
    let executed = branch
        .execute(
            ALICE,
            patch(ns, key("a"), vec![], vec![name("kind")]),
            Budget::unlimited().traced(),
        )
        .expect("applies");
    let mut lines = base_lines(32).to_vec();
    lines[5] = (OperationClass::RootUpdate, 2 + 2 * 2, ROOT_UPDATE);
    lines.push((OperationClass::IndexRemove, 2, INDEX_REMOVE));
    assert_eq!(executed.receipt, traced(applied(74), &lines));
    model.patch(&key("a"), vec![], &[name("kind")]);
    assert_matches_model(&branch, ns, &model, &ABC_KEYS);
    assert_eq!(branch.term_count(ns), Some(ABC_TERMS));
    assert_eq!(branch.spent(), Cost::new(69 + 62 + 74));
}

#[test]
fn delete_charges_lookup_delete_group_then_each_held_term() {
    let (engine, ns, mut model) = fixture_abc();
    // delete a: 1 -> 4 -> {RecordDelete 20 + CellRemove 4*4 + RootUpdate
    // 3*2} = 42 -> 46; then per attribute in name order {IndexRemove 6 +
    // RootUpdate 2*2} = 10 for the value term and 10 for the existence term:
    // 56, 66, 76, 86, 96, 106.
    let op = delete_record(ns, key("a"));
    assert_write_sweep(
        &engine,
        ns,
        &WriteVector {
            label: "delete a",
            op: &op,
            prefix_sums: &[1, 4, 46, 56, 66, 76, 86, 96, 106],
            effect: Effect::RecordDeleted { id: rid(1) },
        },
        &model,
        &ABC_KEYS,
    );

    let mut branch = fresh_branch(&engine);
    let executed = branch
        .execute(ALICE, op, Budget::unlimited().traced())
        .expect("applies");
    assert_eq!(
        executed.receipt,
        traced(
            applied(106),
            &[
                (OperationClass::DirectoryLookup, 1, DIRECTORY_LOOKUP),
                (OperationClass::PointRead, 1, POINT_READ),
                (OperationClass::RecordDelete, 1, RECORD_DELETE),
                (OperationClass::CellRemove, 4, CELL_REMOVE),
                // 3 (record group) + 6 * 2 (index groups).
                (OperationClass::RootUpdate, 15, ROOT_UPDATE),
                (OperationClass::IndexRemove, 6, INDEX_REMOVE),
            ],
        )
    );
    assert_eq!(model.delete(&key("a")), rid(1));
    assert_matches_model(&branch, ns, &model, &ABC_KEYS);
    assert_eq!(
        branch.term_count(ns),
        Some(ABC_TERMS - 1),
        "only price=10 was a's alone"
    );

    // delete c (kind=pen and price=5 were c's alone): 106 again; then b.
    let executed = apply(&mut branch, ALICE, delete_record(ns, key("c")));
    assert_eq!(executed.effect, Effect::RecordDeleted { id: rid(3) });
    assert_eq!(executed.receipt, applied(106));
    model.delete(&key("c"));
    assert_matches_model(&branch, ns, &model, &ABC_KEYS);
    assert_eq!(branch.term_count(ns), Some(6));
    let executed = apply(&mut branch, ALICE, delete_record(ns, key("b")));
    assert_eq!(executed.effect, Effect::RecordDeleted { id: rid(2) });
    assert_eq!(executed.receipt, applied(106));
    model.delete(&key("b"));
    assert_matches_model(&branch, ns, &model, &ABC_KEYS);
    assert_eq!(
        branch.stats(ns),
        TypedStats {
            records: Some(0),
            terms: Some(0),
            consistent: Some(true)
        },
        "an emptied namespace holds no term"
    );
}

// ---------------------------------------------------------------------------
// 3. Post-charge rejections versus admission faults
// ---------------------------------------------------------------------------

#[test]
fn already_exists_is_charged_after_the_key_lookup_and_recorded() {
    let (engine, ns, model) = fixture_abc();
    let mut branch = fresh_branch(&engine);
    // Different cells under a bound key: DirectoryLookup 1, PointRead 3, then
    // AlreadyExists with 4 spent, whatever the cells would have cost.
    let op = create(ns, key("a"), vec![(name("kind"), attr(Value::str("pen")))]);
    let estimated = branch.estimate(ALICE, op.clone(), Budget::unlimited().traced());
    assert!(
        branch.receipts().is_empty(),
        "an estimate is never recorded"
    );
    let executed = branch.execute(ALICE, op, Budget::unlimited().traced());
    assert_eq!(canonical(&estimated), canonical(&executed));
    let rejected = executed.expect_err("the key is bound");
    assert_eq!(rejected.reason, Rejection::AlreadyExists);
    assert_eq!(
        rejected.receipt,
        traced(
            self::rejected(4, Rejection::AlreadyExists),
            &[
                (OperationClass::DirectoryLookup, 1, DIRECTORY_LOOKUP),
                (OperationClass::PointRead, 1, POINT_READ),
            ],
        )
    );
    assert_eq!(branch.receipts(), std::slice::from_ref(&rejected.receipt));
    assert_eq!(branch.spent(), Cost::new(4));
    assert_matches_model(&branch, ns, &model, &ABC_KEYS);
}

#[test]
fn not_found_is_charged_after_the_key_lookup_for_patch_and_delete() {
    let (engine, ns, model) = fixture_abc();
    let mut branch = fresh_branch(&engine);
    let absent = key("zz");
    let ops = [
        patch(
            ns,
            absent.clone(),
            vec![(name("kind"), attr(Value::str("pen")))],
            vec![],
        ),
        patch(ns, absent.clone(), vec![], vec![name("kind")]),
        delete_record(ns, absent),
    ];
    for op in ops {
        let estimated = branch.estimate(ALICE, op.clone(), Budget::unlimited().traced());
        let executed = branch.execute(ALICE, op, Budget::unlimited().traced());
        assert_eq!(canonical(&estimated), canonical(&executed));
        let rejected = executed.expect_err("the key is unbound");
        assert_eq!(
            rejected.receipt,
            traced(
                self::rejected(4, Rejection::NotFound),
                &[
                    (OperationClass::DirectoryLookup, 1, DIRECTORY_LOOKUP),
                    (OperationClass::PointRead, 1, POINT_READ),
                ],
            )
        );
    }
    assert_eq!(branch.receipts().len(), 3);
    assert_eq!(branch.spent(), Cost::new(12));
    assert_matches_model(&branch, ns, &model, &ABC_KEYS);
}

#[test]
fn record_full_is_charged_after_the_lookup_but_too_many_cells_is_an_admission_fault() {
    let limits = Limits {
        max_cells: 4,
        ..Limits::DEFAULT
    };
    let mut engine = Engine::with_limits(PriceSchedule::FIXTURE, limits);
    let ns = ns_id(NS);
    let mut model = Model::default();
    let mut branch = fresh_branch(&engine);
    apply(
        &mut branch,
        ALICE,
        WriteOp::CreateNamespace {
            name: arkiv_db::NamespaceName::new("test").unwrap(),
        },
    );
    // Exactly max_cells cells are admitted: the worked create a.
    let executed = apply(
        &mut branch,
        ALICE,
        create(ns, key("a"), fixture_cells("book", 10)),
    );
    assert_eq!(executed.receipt, applied(CREATE_A));
    model.create(key("a"), fixture_cells("book", 10));
    engine.publish(&branch.seal()).expect("publishes");

    let mut branch = fresh_branch(&engine);
    // Five cells in a create: refused at admission, spent 0, not recorded.
    let mut five = fixture_cells("book", 10);
    five.push((name("x"), attr(Value::Bool(true))));
    let result = branch.execute(ALICE, create(ns, key("d"), five), Budget::unlimited());
    assert_eq!(
        result.expect_err("too many cells"),
        admission(Rejection::InvalidArgument(ArgumentFault::TooManyCells))
    );
    assert!(branch.receipts().is_empty());

    // A patch whose result would hold five cells: |existing| 4 - |remove ∩
    // existing| 0 + |set \ existing| 1 = 5 > 4: RecordFull after the lookup
    // (spent 4), recorded like any other post-charge rejection.
    let extra = vec![(name("x"), attr(Value::Bool(true)))];
    let op = patch(ns, key("a"), extra.clone(), vec![]);
    let estimated = branch.estimate(ALICE, op.clone(), Budget::unlimited());
    let executed = branch.execute(ALICE, op, Budget::unlimited());
    assert_eq!(canonical(&estimated), canonical(&executed));
    let full = executed.expect_err("the record would be full");
    assert_eq!(full.reason, Rejection::RecordFull);
    assert_eq!(full.receipt, rejected(4, Rejection::RecordFull));
    // 4 - 1 (note) + 2 (x, y) = 5: still full.
    let two = vec![
        (name("x"), attr(Value::Bool(true))),
        (name("y"), attr(Value::Bool(false))),
    ];
    let full_again = reject(
        &mut branch,
        ALICE,
        patch(ns, key("a"), two, vec![name("note")]),
    );
    assert_eq!(full_again.receipt, rejected(4, Rejection::RecordFull));
    // 4 - 1 (a removed field) + 1 (x) = 4: admitted and applied.
    // 1 + 3 + {RecordPatch 10 + CellWrite 4 + CellByte (1 + 1) + CellRemove 4
    // + RowByte 42 (key 1 + $expiresAt 18 + kind 8 + price 13 + x 2) +
    // RootUpdate 2*2} = 70; note: nothing; x: two new terms {6 + 30 + 4}
    // each -> 150.
    let executed = apply(
        &mut branch,
        ALICE,
        patch(ns, key("a"), extra.clone(), vec![name("note")]),
    );
    assert_eq!(executed.effect, Effect::RecordPatched { id: rid(1) });
    assert_eq!(executed.receipt, applied(150));
    model.patch(&key("a"), extra, &[name("note")]);
    // Replacing an existing name keeps the count: 4 -> 4. kind attribute ->
    // field (the row stays 42 bytes): 1 + 3 + {10 + 4 + 8 + 42 + 4} = 72,
    // then {IndexRemove 6 + RootUpdate 4} twice -> 92.
    let replace = vec![(name("kind"), field(Value::str("book")))];
    let executed = apply(
        &mut branch,
        ALICE,
        patch(ns, key("a"), replace.clone(), vec![]),
    );
    assert_eq!(executed.receipt, applied(92));
    model.patch(&key("a"), replace, &[]);

    assert_eq!(
        branch.receipts(),
        &[
            rejected(4, Rejection::RecordFull),
            rejected(4, Rejection::RecordFull),
            applied(150),
            applied(92),
        ]
    );
    assert_eq!(branch.spent(), Cost::new(4 + 4 + 150 + 92));
    assert_matches_model(&branch, ns, &model, &["a", "d"]);
    // The same limit bounds a named projection: five names are refused, four
    // (absent ones included) are read for the fixed price 1 + 3 + 3 = 7.
    let five_names = Projection::named(false, (0..5).map(|i| name(&format!("n{i}"))));
    let refused = branch
        .get_record(ns, &key("a"), &five_names, Budget::unlimited())
        .expect_err("five names exceed max_cells 4");
    assert_eq!(
        refused,
        read_admission(Rejection::InvalidQuery(QueryFault::ProjectionTooLarge))
    );
    let four_names = Projection::named(false, (0..4).map(|i| name(&format!("n{i}"))));
    let read = branch
        .get_record(ns, &key("a"), &four_names, Budget::unlimited())
        .expect("four names are admitted");
    assert_eq!(read.receipt, read_applied(7));
    assert_eq!(read.value, model.row(&key("a"), &four_names));
}

/// Sixty-five distinct names, `c00`..`c64`, each an indexed `Bool`.
fn many_cells(count: usize) -> Cells {
    (0..count)
        .map(|i| (name(&format!("c{i:02}")), attr(Value::Bool(true))))
        .collect()
}

#[test]
fn argument_faults_are_admission_rejections_with_nothing_recorded() {
    let (engine, ns, model) = fixture_abc();
    let mut branch = fresh_branch(&engine);
    let big_str = "x".repeat(4097);
    let big_bytes = vec![0u8; 4097];
    let ok = attr(Value::Bool(true));
    let bytes_attr = attr(Value::bytes(b"x"));
    use ArgumentFault::{
        BytesNotIndexable, DuplicateCell, SetAndRemoveOverlap, TooManyCells, ValueTooLarge,
    };
    let cases: Vec<(&str, WriteOp, ArgumentFault)> = vec![
        (
            "create: 65 cells",
            create(ns, key("d"), many_cells(65)),
            TooManyCells,
        ),
        (
            "create: duplicate name",
            create(
                ns,
                key("d"),
                vec![(name("k"), ok.clone()), (name("k"), ok.clone())],
            ),
            DuplicateCell,
        ),
        (
            "create: Str field of 4097 bytes",
            create(ns, key("d"), vec![(name("v"), field(Value::str(&big_str)))]),
            ValueTooLarge,
        ),
        (
            "create: Str attribute of 4097 bytes",
            create(ns, key("d"), vec![(name("v"), attr(Value::str(&big_str)))]),
            ValueTooLarge,
        ),
        (
            "create: Bytes field of 4097 bytes",
            create(
                ns,
                key("d"),
                vec![(name("v"), field(Value::bytes(&big_bytes)))],
            ),
            ValueTooLarge,
        ),
        (
            "create: Bytes attribute",
            create(ns, key("d"), vec![(name("v"), bytes_attr.clone())]),
            BytesNotIndexable,
        ),
        // Per-cell order: DuplicateCell, ValueTooLarge, BytesNotIndexable;
        // the first faulting cell in submitted order wins, and TooManyCells
        // precedes every per-cell check.
        (
            "create: duplicate beats bytes attribute in the second cell",
            create(
                ns,
                key("d"),
                vec![(name("k"), ok.clone()), (name("k"), bytes_attr.clone())],
            ),
            DuplicateCell,
        ),
        (
            "create: too large beats bytes attribute in one cell",
            create(
                ns,
                key("d"),
                vec![(name("v"), attr(Value::bytes(&big_bytes)))],
            ),
            ValueTooLarge,
        ),
        (
            "create: bytes attribute in the first cell beats a later duplicate",
            create(
                ns,
                key("d"),
                vec![
                    (name("a"), bytes_attr.clone()),
                    (name("b"), ok.clone()),
                    (name("b"), ok.clone()),
                ],
            ),
            BytesNotIndexable,
        ),
        (
            "create: too many cells beats a bytes attribute",
            create(ns, key("d"), {
                let mut cells = many_cells(64);
                cells.insert(0, (name("v"), bytes_attr.clone()));
                cells
            }),
            TooManyCells,
        ),
        (
            "patch: 33 set + 32 remove names",
            patch(
                ns,
                key("a"),
                many_cells(33),
                (33..65).map(|i| name(&format!("c{i:02}"))).collect(),
            ),
            TooManyCells,
        ),
        (
            "patch: too many names beats a bytes attribute in set",
            patch(
                ns,
                key("a"),
                {
                    let mut cells = many_cells(64);
                    cells.insert(0, (name("v"), bytes_attr.clone()));
                    cells
                },
                vec![name("w")],
            ),
            TooManyCells,
        ),
        (
            "patch: duplicate in set",
            patch(
                ns,
                key("a"),
                vec![(name("k"), ok.clone()), (name("k"), ok.clone())],
                vec![],
            ),
            DuplicateCell,
        ),
        (
            "patch: bytes attribute in set",
            patch(ns, key("a"), vec![(name("v"), bytes_attr.clone())], vec![]),
            BytesNotIndexable,
        ),
        (
            "patch: set is validated before the remove list",
            patch(
                ns,
                key("a"),
                vec![(name("v"), bytes_attr.clone())],
                vec![name("w"), name("w")],
            ),
            BytesNotIndexable,
        ),
        (
            "patch: duplicate in remove",
            patch(ns, key("a"), vec![], vec![name("k"), name("k")]),
            DuplicateCell,
        ),
        (
            "patch: duplicate in remove beats the overlap",
            patch(
                ns,
                key("a"),
                vec![(name("k"), ok.clone())],
                vec![name("k"), name("k")],
            ),
            DuplicateCell,
        ),
        (
            "patch: a name in both lists",
            patch(ns, key("a"), vec![(name("k"), ok.clone())], vec![name("k")]),
            SetAndRemoveOverlap,
        ),
    ];
    for (label, op, fault) in cases {
        let expected = admission(Rejection::InvalidArgument(fault));
        for budget in [
            Budget::unlimited(),
            Budget::limit(0),
            Budget::limit(1_000_000),
        ] {
            assert_eq!(
                branch.estimate(ALICE, op.clone(), budget),
                Err(expected.clone()),
                "{label}"
            );
            assert_eq!(
                branch.execute(ALICE, op.clone(), budget),
                Err(expected.clone()),
                "{label}"
            );
        }
        // A traced budget adds an empty ledger to the zero-cost receipt.
        let traced_rejection = branch
            .execute(ALICE, op, Budget::unlimited().traced())
            .expect_err(label);
        assert_eq!(traced_rejection.receipt.spent, Cost::ZERO, "{label}");
        assert_eq!(
            traced_rejection.receipt.ledger,
            Some(Ledger::default()),
            "{label}"
        );
    }
    assert!(
        branch.receipts().is_empty(),
        "admission faults are never recorded"
    );
    assert_eq!(branch.spent(), Cost::ZERO);
    assert_matches_model(&branch, ns, &model, &ABC_KEYS);

    // The boundaries are admitted: a 4096-byte Str field costs 1 + 3 + {20 +
    // 4 + CellByte (1 + 4096) + RowByte (1 + 4097) + RootUpdate 3*2} = 8229;
    // 64 indexed Bool cells cost 1 + 3 + {20 + 64*4 + 64*(3 + 1) + RowByte
    // (1 + 256) + RootUpdate 3*2} + 128 new terms * (36 + RootUpdate 2*2) =
    // 4 + 795 + 5120 = 5919.
    let boundary = vec![(name("v"), field(Value::str(&"x".repeat(4096))))];
    let executed = apply(&mut branch, ALICE, create(ns, key("d"), boundary));
    assert_eq!(executed.receipt, applied(8229));
    let executed = apply(&mut branch, ALICE, create(ns, key("e"), many_cells(64)));
    assert_eq!(executed.receipt, applied(5919));
    assert_eq!(executed.effect, Effect::RecordCreated { id: rid(5) });
}

#[test]
fn argument_validation_precedes_every_charge_and_the_block_budget_check() {
    let (engine, ns, model) = fixture_abc();
    let invalid = create(ns, key("d"), vec![(name("v"), attr(Value::bytes(b"x")))]);
    let bad_args = admission(Rejection::InvalidArgument(ArgumentFault::BytesNotIndexable));
    let valid = create(ns, key("d"), vec![(name("v"), attr(Value::Bool(true)))]);

    // Block budget 10: a valid op with budget 20 is ExceedsBlockBudget; an
    // invalid one is refused for its arguments first. Neither is recorded.
    let mut branch = engine
        .begin(engine.head(), Some(Cost::new(10)))
        .expect("begin on head");
    assert_eq!(
        branch.execute(ALICE, invalid.clone(), Budget::limit(20)),
        Err(bad_args.clone())
    );
    assert_eq!(
        branch.execute(ALICE, valid.clone(), Budget::limit(20)),
        Err(admission(Rejection::ExceedsBlockBudget {
            remaining: Cost::new(10)
        }))
    );
    assert!(branch.receipts().is_empty());

    let mut branch = fresh_branch(&engine);
    // Before the directory charge: an unknown namespace and a non-owner both
    // see the argument fault, not UnknownNamespace or Unauthorized.
    let elsewhere = create(
        ns_id(9),
        key("d"),
        vec![(name("v"), attr(Value::bytes(b"x")))],
    );
    assert_eq!(
        branch.execute(ALICE, elsewhere, Budget::unlimited()),
        Err(bad_args.clone())
    );
    assert_eq!(
        branch.execute(BOB, invalid.clone(), Budget::unlimited()),
        Err(bad_args.clone())
    );
    // Before the first charge: a zero budget still reports the fault, while
    // the same zero budget on valid arguments is a recorded OutOfBudget.
    assert_eq!(
        branch.execute(ALICE, invalid, Budget::limit(0)),
        Err(bad_args)
    );
    assert!(branch.receipts().is_empty());
    let starved = branch
        .execute(ALICE, valid, Budget::limit(0))
        .expect_err("nothing fits a zero budget");
    assert_eq!(starved.receipt, rejected(0, Rejection::OutOfBudget));
    assert_eq!(branch.receipts(), &[starved.receipt]);
    assert_eq!(branch.spent(), Cost::ZERO);
    assert_matches_model(&branch, ns, &model, &ABC_KEYS);
}

#[test]
fn unauthorized_and_unknown_namespace_are_charged_the_directory_lookup() {
    let (engine, ns, model) = fixture_abc();
    let mut branch = fresh_branch(&engine);
    let cells = vec![(name("kind"), attr(Value::str("pen")))];
    let denied = [
        create(ns, key("d"), cells.clone()),
        patch(ns, key("a"), cells.clone(), vec![]),
        delete_record(ns, key("a")),
    ];
    for op in denied {
        let rejected = branch
            .execute(BOB, op, Budget::unlimited().traced())
            .expect_err("BOB does not own the namespace");
        assert_eq!(
            rejected.receipt,
            traced(
                self::rejected(1, Rejection::Unauthorized),
                &[(OperationClass::DirectoryLookup, 1, DIRECTORY_LOOKUP)],
            )
        );
    }
    let unknown = ns_id(2);
    let missing = [
        create(unknown, key("d"), cells.clone()),
        patch(unknown, key("a"), cells, vec![]),
        delete_record(unknown, key("a")),
    ];
    for op in missing {
        let rejected = branch
            .execute(ALICE, op, Budget::unlimited().traced())
            .expect_err("namespace 2 does not exist");
        assert_eq!(
            rejected.receipt,
            traced(
                self::rejected(1, Rejection::UnknownNamespace),
                &[(OperationClass::DirectoryLookup, 1, DIRECTORY_LOOKUP)],
            )
        );
    }
    assert_eq!(
        branch.receipts().len(),
        6,
        "post-charge rejections are recorded"
    );
    assert_eq!(branch.spent(), Cost::new(6));
    assert_matches_model(&branch, ns, &model, &ABC_KEYS);
    assert_eq!(branch.typed_record_count(unknown), None);
}

// ---------------------------------------------------------------------------
// 4. Dense ids, rollback, the host path and transfers
// ---------------------------------------------------------------------------

#[test]
fn ids_are_dense_and_never_reused_across_delete_and_recreate() {
    let (mut engine, ns, mut model) = fixture_abc();
    let mut branch = fresh_branch(&engine);
    let deleted = apply(&mut branch, ALICE, delete_record(ns, key("a")));
    assert_eq!(deleted.effect, Effect::RecordDeleted { id: rid(1) });
    assert_eq!(model.delete(&key("a")), rid(1));
    // The same key again: a new id, and price=10 (a's alone) is new again:
    // 379 - 6*30 + 30 = 229.
    let recreated = apply(
        &mut branch,
        ALICE,
        create(ns, key("a"), fixture_cells("book", 10)),
    );
    assert_eq!(recreated.effect, Effect::RecordCreated { id: rid(4) });
    assert_eq!(recreated.receipt, applied(229));
    assert_eq!(model.create(key("a"), fixture_cells("book", 10)), rid(4));
    let read = branch
        .get_record(ns, &key("a"), &Projection::ID_ONLY, Budget::unlimited())
        .expect("reads");
    assert_eq!(read.value, model.row(&key("a"), &Projection::ID_ONLY));
    assert_eq!(read.receipt, read_applied(7));

    // Rejected creates mint nothing: a bound key and a starved budget.
    let bound = reject(
        &mut branch,
        ALICE,
        create(ns, key("b"), fixture_cells("book", 20)),
    );
    assert_eq!(bound.reason, Rejection::AlreadyExists);
    let starved = branch
        .execute(
            ALICE,
            create(ns, key("e"), fixture_cells("pen", 1)),
            Budget::limit(4),
        )
        .expect_err("the record group needs 133 more");
    assert_eq!(starved.receipt, rejected(4, Rejection::OutOfBudget));
    let next = apply(
        &mut branch,
        ALICE,
        create(ns, key("d"), fixture_cells("pen", 1)),
    );
    assert_eq!(next.effect, Effect::RecordCreated { id: rid(5) });
    model.create(key("d"), fixture_cells("pen", 1));
    assert_matches_model(&branch, ns, &model, &["a", "b", "c", "d", "e"]);
    engine.publish(&branch.seal()).expect("publishes");

    // Across blocks the allocator continues; a deleted id stays retired.
    let mut branch = fresh_branch(&engine);
    let deleted = apply(&mut branch, ALICE, delete_record(ns, key("a")));
    assert_eq!(deleted.effect, Effect::RecordDeleted { id: rid(4) });
    model.delete(&key("a"));
    let recreated = apply(
        &mut branch,
        ALICE,
        create(ns, key("a"), fixture_cells("book", 10)),
    );
    assert_eq!(recreated.effect, Effect::RecordCreated { id: rid(6) });
    model.create(key("a"), fixture_cells("book", 10));
    let deleted = apply(&mut branch, ALICE, delete_record(ns, key("d")));
    assert_eq!(deleted.effect, Effect::RecordDeleted { id: rid(5) });
    model.delete(&key("d"));
    let recreated = apply(
        &mut branch,
        ALICE,
        create(ns, key("d"), fixture_cells("pen", 1)),
    );
    assert_eq!(recreated.effect, Effect::RecordCreated { id: rid(7) });
    model.create(key("d"), fixture_cells("pen", 1));
    assert_matches_model(&branch, ns, &model, &["a", "b", "c", "d", "e"]);
    let ids: Vec<RecordId> = branch
        .query(
            ns,
            &Query::match_all(),
            &Projection::ID_ONLY,
            10,
            None,
            Budget::unlimited(),
        )
        .expect("reads")
        .value
        .rows
        .iter()
        .map(|row| row.id)
        .collect();
    assert_eq!(ids, vec![rid(2), rid(3), rid(6), rid(7)]);
}

#[test]
fn rollback_releases_ids_and_restores_rows_keys_and_terms_together() {
    let (engine, ns, model) = fixture_abc();
    let mut branch = fresh_branch(&engine);
    let checkpoint = branch.checkpoint();

    // A transaction: delete a (106), retype b's price to U64 (1 + 3 + {10 +
    // 4 + 13 + RowByte 47 + RootUpdate 2*2} = 82, two index removes at 10 ->
    // 102, two new terms at 40 -> 182), create d with one cell kind=pen (1 +
    // 3 + {20 + 4 + 7 + RowByte (1 + 7) + RootUpdate 3*2} = 49, kind=pen and
    // the Str existence term exist through c: {6 + 4} each -> 59, 69).
    let mut changed = model.clone();
    apply(&mut branch, ALICE, delete_record(ns, key("a")));
    changed.delete(&key("a"));
    let retype = vec![(name("price"), attr(Value::U64(20)))];
    let patched = apply(
        &mut branch,
        ALICE,
        patch(ns, key("b"), retype.clone(), vec![]),
    );
    assert_eq!(patched.receipt, applied(182));
    changed.patch(&key("b"), retype, &[]);
    let pen = vec![(name("kind"), attr(Value::str("pen")))];
    let created = apply(&mut branch, ALICE, create(ns, key("d"), pen.clone()));
    assert_eq!(created.effect, Effect::RecordCreated { id: rid(4) });
    assert_eq!(created.receipt, applied(69));
    changed.create(key("d"), pen.clone());
    let keys = ["a", "b", "c", "d", "e", "f"];
    assert_matches_model(&branch, ns, &changed, &keys);

    assert_eq!(branch.rollback_to(checkpoint), Ok(0..3));
    assert_matches_model(&branch, ns, &model, &keys);
    assert_eq!(branch.term_count(ns), Some(ABC_TERMS));
    // Receipts and the block's spent units survive the rollback.
    assert_eq!(branch.receipts().len(), 3);
    assert_eq!(branch.spent(), Cost::new(106 + 182 + 69));

    // The terms are restored, not only the rows: price=10 is held again (by
    // the restored a), so a record holding only it creates no term: 1 + 3 +
    // {20 + 4 + 13 + RowByte (1 + 13) + RootUpdate 3*2} = 61, then {IndexInsert
    // 6 + RootUpdate 4} twice -> 81. And the released id 4 is handed out
    // again.
    let original = model.clone();
    let mut model = model;
    let ten = vec![(name("price"), attr(Value::I64(10)))];
    let created = apply(&mut branch, ALICE, create(ns, key("f"), ten.clone()));
    assert_eq!(created.effect, Effect::RecordCreated { id: rid(4) });
    assert_eq!(created.receipt, applied(81));
    model.create(key("f"), ten);
    let created = apply(&mut branch, ALICE, create(ns, key("d"), pen.clone()));
    assert_eq!(created.effect, Effect::RecordCreated { id: rid(5) });
    assert_eq!(created.receipt, applied(69));
    model.create(key("d"), pen.clone());
    assert_matches_model(&branch, ns, &model, &keys);

    // Nested: an inner checkpoint releases only its own id, and a rejected
    // op after it is inside the discarded receipt range.
    let inner = branch.checkpoint();
    let created = apply(&mut branch, ALICE, create(ns, key("e"), pen.clone()));
    assert_eq!(created.effect, Effect::RecordCreated { id: rid(6) });
    let bound = reject(&mut branch, ALICE, create(ns, key("a"), pen.clone()));
    assert_eq!(bound.reason, Rejection::AlreadyExists);
    assert_eq!(branch.rollback_to(inner), Ok(5..7));
    assert_matches_model(&branch, ns, &model, &keys);
    let created = apply(&mut branch, ALICE, create(ns, key("e"), pen.clone()));
    assert_eq!(created.effect, Effect::RecordCreated { id: rid(6) });
    model.create(key("e"), pen);
    assert_matches_model(&branch, ns, &model, &keys);
    // Rolling back to the outer checkpoint again discards everything since.
    assert_eq!(branch.rollback_to(checkpoint), Ok(0..8));
    assert_matches_model(&branch, ns, &original, &keys);
    assert_eq!(branch.receipts().len(), 8);
}

#[test]
fn host_record_writes_match_the_owner_path_and_skip_only_the_owner_check() {
    let (engine, ns, mut model) = fixture_abc();
    let ops = [
        delete_record(ns, key("a")),
        // After a is gone price=10 is new again: 229.
        create(ns, key("d"), fixture_cells("book", 10)),
        patch(
            ns,
            key("b"),
            vec![(name("price"), attr(Value::U64(20)))],
            vec![],
        ),
        // NotFound on the host path is charged and recorded the same way.
        delete_record(ns, key("a")),
    ];
    let expected: [Canonical; 4] = [
        Ok((Effect::RecordDeleted { id: rid(1) }, applied(106))),
        Ok((Effect::RecordCreated { id: rid(4) }, applied(229))),
        Ok((Effect::RecordPatched { id: rid(2) }, applied(182))),
        Err((Rejection::NotFound, rejected(4, Rejection::NotFound))),
    ];
    let mut owner = fresh_branch(&engine);
    let mut host = fresh_branch(&engine);
    for (op, expected) in ops.into_iter().zip(expected) {
        // The non-owner is refused after the directory charge on the same
        // branch the host then writes to.
        let denied = host
            .execute(BOB, op.clone(), Budget::unlimited().traced())
            .expect_err("BOB is not the owner");
        assert_eq!(
            denied.receipt,
            traced(
                rejected(1, Rejection::Unauthorized),
                &[(OperationClass::DirectoryLookup, 1, DIRECTORY_LOOKUP)],
            )
        );
        let recorded = host.receipts().len();
        let estimated = host.estimate_host(op.clone(), Budget::unlimited());
        assert_eq!(
            host.receipts().len(),
            recorded,
            "estimates are not recorded"
        );
        let by_host = host.execute_host(op.clone(), Budget::unlimited());
        assert_eq!(estimated, by_host, "estimate_host == execute_host");
        let by_owner = owner.execute(ALICE, op, Budget::unlimited());
        assert_eq!(canonical(&by_host), canonical(&by_owner));
        assert_eq!(canonical(&by_host), expected);
        assert_eq!(
            host.receipts().len(),
            recorded + 1,
            "host writes are recorded"
        );
    }
    model.delete(&key("a"));
    model.create(key("d"), fixture_cells("book", 10));
    model.patch(&key("b"), vec![(name("price"), attr(Value::U64(20)))], &[]);
    assert_matches_model(&host, ns, &model, &["a", "b", "c", "d"]);
    assert_matches_model(&owner, ns, &model, &["a", "b", "c", "d"]);
    let owner_receipts = owner.receipts().to_vec();
    let host_receipts: Vec<Receipt> = host
        .receipts()
        .iter()
        .filter(|receipt| receipt.outcome != Outcome::Rejected(Rejection::Unauthorized))
        .cloned()
        .collect();
    assert_eq!(host_receipts, owner_receipts);
    assert_eq!(owner.spent(), Cost::new(106 + 229 + 182 + 4));
    assert_eq!(host.spent(), Cost::new(106 + 229 + 182 + 4 + 4));
}

#[test]
fn namespace_and_raw_ops_on_the_host_path_are_not_host_operations() {
    let (engine, ns, model) = fixture_abc();
    // Block budget 0: the host-path restriction is decided before the block
    // budget check and before any charge.
    let mut branch = engine
        .begin(engine.head(), Some(Cost::ZERO))
        .expect("begin on head");
    let refused = admission(Rejection::InvalidArgument(ArgumentFault::NotHostOperation));
    let ops = [
        WriteOp::CreateNamespace {
            name: arkiv_db::NamespaceName::new("test").unwrap(),
        },
        transfer(ns, 0, BOB),
        put(ns, b"a", b"raw"),
        raw_delete(ns, b"a"),
        put(ns_id(9), b"a", b"raw"),
    ];
    for op in ops {
        for budget in [Budget::unlimited(), Budget::limit(0), Budget::limit(5)] {
            assert_eq!(
                branch.estimate_host(op.clone(), budget),
                Err(refused.clone())
            );
            assert_eq!(
                branch.execute_host(op.clone(), budget),
                Err(refused.clone())
            );
        }
    }
    // Record ops on the host path go on to the argument and block checks.
    let bad_cells = create(ns, key("d"), vec![(name("v"), attr(Value::bytes(b"x")))]);
    assert_eq!(
        branch.execute_host(bad_cells, Budget::limit(5)),
        Err(admission(Rejection::InvalidArgument(
            ArgumentFault::BytesNotIndexable
        )))
    );
    assert_eq!(
        branch.execute_host(delete_record(ns, key("a")), Budget::limit(5)),
        Err(admission(Rejection::ExceedsBlockBudget {
            remaining: Cost::ZERO
        }))
    );
    assert!(branch.receipts().is_empty());
    assert_eq!(branch.spent(), Cost::ZERO);
    assert_eq!(branch.namespace_count(), 1);
    assert_eq!(branch.record_count(ns), Some(0));
    let descriptor = branch
        .descriptor(ns, Budget::unlimited())
        .expect("exists")
        .value;
    assert_eq!(
        (descriptor.owner, descriptor.revision),
        (ALICE, Revision(0))
    );
    assert_matches_model(&branch, ns, &model, &ABC_KEYS);
}

#[test]
fn a_transfer_shares_the_typed_state_and_visits_no_record_or_index_node() {
    let (mut engine, ns, model) = fixture_abc();
    let before = engine.head_snapshot();
    let mut branch = fresh_branch(&engine);
    let moved = apply(&mut branch, ALICE, transfer(ns, 0, BOB));
    assert_eq!(
        moved.effect,
        Effect::Transferred {
            revision: Revision(1)
        }
    );
    // DirectoryLookup 1 + NamespaceTransfer 20 + RootUpdate 1*2.
    assert_eq!(
        moved.receipt,
        applied(DIRECTORY_LOOKUP + NAMESPACE_TRANSFER + ROOT_UPDATE)
    );
    assert_eq!(
        moved.physical.record_nodes, 0,
        "no key or row map node is visited"
    );
    assert_eq!(
        moved.physical.index_nodes, 0,
        "no term or posting node is visited"
    );
    assert_matches_model(&branch, ns, &model, &ABC_KEYS);
    engine
        .publish(&branch.seal())
        .expect("publishes the transfer");
    let after = engine.head_snapshot();

    assert!(after.typed_shared_with(&before, ns));
    assert!(before.typed_shared_with(&after, ns));
    assert!(after.records_shared_with(&before, ns));
    assert!(after.typed_shared_with(&after, ns));
    assert!(
        !after.typed_shared_with(&before, ns_id(2)),
        "unknown namespaces share nothing"
    );
    assert_matches_model(&after, ns, &model, &ABC_KEYS);

    // The new owner writes and the old owner is refused after the directory
    // charge; the write copies a path, so the states no longer share.
    let mut changed = model.clone();
    let mut branch = fresh_branch(&engine);
    let refused = reject(&mut branch, ALICE, delete_record(ns, key("a")));
    assert_eq!(refused.receipt, rejected(1, Rejection::Unauthorized));
    let deleted = apply(&mut branch, BOB, delete_record(ns, key("a")));
    assert_eq!(deleted.receipt, applied(106));
    changed.delete(&key("a"));
    engine
        .publish(&branch.seal())
        .expect("publishes the delete");
    let later = engine.head_snapshot();
    assert!(!later.typed_shared_with(&after, ns));
    assert_matches_model(&later, ns, &changed, &ABC_KEYS);
    assert_matches_model(&after, ns, &model, &ABC_KEYS);
}

// ---------------------------------------------------------------------------
// 5. The raw keyspace, projections and historical reads
// ---------------------------------------------------------------------------

fn entry(key: &[u8], value: &[u8]) -> Entry {
    Entry {
        key: key.to_vec(),
        value: value.to_vec(),
    }
}

#[test]
fn raw_bytes_and_typed_records_share_a_namespace_but_never_a_key() {
    let (engine, ns) = fixture();
    let mut model = Model::default();
    let mut branch = fresh_branch(&engine);
    let record = key("a");
    // Even the typed key's own encoding (zero-extended to the 20-byte raw
    // key cap), used as a raw key, cannot reach typed data.
    let mut encoded = arkiv_db::encode(&record);
    encoded.resize(20, 0);
    let cells = vec![(name("kind"), attr(Value::str("book")))];
    // create: 1 + 3 + {20 + 4 + (4 + 4) + RowByte (1 + 8) + RootUpdate 3*2}
    // + two new terms * (36 + RootUpdate 2*2) = 4 + 47 + 80 = 131.
    const CREATE: u64 = 131;
    // get_record ALL: 1 + 3 + {3 + (1 + 8)} = 16; match-all page of one row:
    // 1 + QueryGroup 5 + RowVisit 1 + {3 + 9} = 19; of no row: 1 + 5 = 6.
    const GET_RECORD: u64 = 16;
    const PAGE_ONE: u64 = 19;
    const PAGE_NONE: u64 = 6;
    // raw put "a" (1 byte) = "raw": 1 + 10 + 2 + 3 + RootUpdate 2*2 = 20;
    // put of the 20-byte prefix = "enc": 1 + 10 + 40 + 3 + 4 = 58; raw
    // deletes 1 + 5 + 2 + 4 = 12 and 1 + 5 + 40 + 4 = 50; get present
    // 1 + 3 + 3 = 7, missing 4; scan of both entries 1 + 4 + (2 + 20 + 3) +
    // (2 + 1 + 3) = 36.
    const PUT_SHORT: u64 = 20;
    const PUT_ENCODED: u64 = 58;
    const DELETE_SHORT: u64 = 12;
    const DELETE_ENCODED: u64 = 50;
    const GET_HIT: u64 = 7;
    const GET_MISS: u64 = 4;
    const SCAN_BOTH: u64 = 36;

    // A raw entry under the key bytes does not bind the record key.
    let executed = apply(&mut branch, ALICE, put(ns, b"a", b"raw"));
    assert_eq!(executed.effect, Effect::Put { replaced: false });
    assert_eq!(executed.receipt, applied(PUT_SHORT));
    let executed = apply(
        &mut branch,
        ALICE,
        create(ns, record.clone(), cells.clone()),
    );
    assert_eq!(executed.effect, Effect::RecordCreated { id: rid(1) });
    assert_eq!(executed.receipt, applied(CREATE));
    model.create(record.clone(), cells.clone());
    let executed = apply(&mut branch, ALICE, put(ns, &encoded, b"enc"));
    assert_eq!(executed.effect, Effect::Put { replaced: false });
    assert_eq!(executed.receipt, applied(PUT_ENCODED));

    let get =
        |branch: &Branch, key: &[u8]| branch.get(ns, key, Budget::unlimited()).expect("reads");
    let read = get(&branch, b"a");
    assert_eq!(
        (read.value, read.receipt),
        (Some(b"raw".to_vec()), read_applied(GET_HIT))
    );
    let read = get(&branch, &encoded);
    assert_eq!(
        (read.value, read.receipt),
        (Some(b"enc".to_vec()), read_applied(GET_HIT))
    );
    let scan = branch
        .scan_prefix(ns, b"", Budget::unlimited())
        .expect("scans");
    assert_eq!(
        scan.value,
        vec![entry(&encoded, b"enc"), entry(b"a", b"raw")]
    );
    assert_eq!(scan.receipt, read_applied(SCAN_BOTH));
    let read = branch.record(ns, &record, &Projection::ALL).expect("reads");
    assert_eq!(read.value, model.row(&record, &Projection::ALL));
    assert_eq!(read.receipt, read_applied(GET_RECORD));
    let page = branch
        .query(
            ns,
            &Query::match_all(),
            &Projection::ALL,
            10,
            None,
            Budget::unlimited(),
        )
        .expect("reads");
    assert_eq!(page.value.rows, model.all_rows(&Projection::ALL));
    assert!(page.value.next.is_none());
    assert_eq!(page.receipt, read_applied(PAGE_ONE));
    assert_eq!(branch.raw_count(ns), Some(2));
    assert_matches_model(&branch, ns, &model, &["a"]);
    assert_eq!(branch.term_count(ns), Some(2));

    // Raw deletes of both spellings leave the typed record alone.
    let executed = apply(&mut branch, ALICE, raw_delete(ns, b"a"));
    assert_eq!(executed.effect, Effect::Deleted { existed: true });
    assert_eq!(executed.receipt, applied(DELETE_SHORT));
    let read = get(&branch, b"a");
    assert_eq!((read.value, read.receipt), (None, read_applied(GET_MISS)));
    assert_matches_model(&branch, ns, &model, &["a"]);
    let executed = apply(&mut branch, ALICE, raw_delete(ns, &encoded));
    assert_eq!(executed.effect, Effect::Deleted { existed: true });
    assert_eq!(executed.receipt, applied(DELETE_ENCODED));
    assert_eq!(branch.raw_count(ns), Some(0));
    assert_matches_model(&branch, ns, &model, &["a"]);
    let executed = apply(&mut branch, ALICE, raw_delete(ns, b"a"));
    assert_eq!(
        executed.effect,
        Effect::Deleted { existed: false },
        "the record is not a raw entry"
    );
    assert_eq!(executed.receipt, applied(DELETE_SHORT));

    // A record delete leaves raw entries alone.
    apply(&mut branch, ALICE, put(ns, b"a", b"raw"));
    apply(&mut branch, ALICE, put(ns, &encoded, b"enc"));
    assert_eq!(branch.raw_count(ns), Some(2));
    // delete: 1 + 3 + {20 + 4 + RootUpdate 3*2} + {IndexRemove 6 + RootUpdate
    // 2*2} * 2 = 4 + 30 + 20 = 54.
    let executed = apply(&mut branch, ALICE, delete_record(ns, record.clone()));
    assert_eq!(executed.effect, Effect::RecordDeleted { id: rid(1) });
    assert_eq!(executed.receipt, applied(54));
    model.delete(&record);
    assert_matches_model(&branch, ns, &model, &["a"]);
    assert_eq!(branch.raw_count(ns), Some(2));
    let read = get(&branch, b"a");
    assert_eq!(
        (read.value, read.receipt),
        (Some(b"raw".to_vec()), read_applied(GET_HIT))
    );
    let read = get(&branch, &encoded);
    assert_eq!(
        (read.value, read.receipt),
        (Some(b"enc".to_vec()), read_applied(GET_HIT))
    );
    let read = branch.record(ns, &record, &Projection::ALL).expect("reads");
    assert_eq!((read.value, read.receipt), (None, read_applied(GET_MISS)));
    let page = branch
        .query(
            ns,
            &Query::match_all(),
            &Projection::ALL,
            10,
            None,
            Budget::unlimited(),
        )
        .expect("reads");
    assert!(page.value.rows.is_empty() && page.value.next.is_none());
    assert_eq!(page.receipt, read_applied(PAGE_NONE));

    // Recreating the record mints a new id and recreates its terms.
    let executed = apply(
        &mut branch,
        ALICE,
        create(ns, record.clone(), cells.clone()),
    );
    assert_eq!(executed.effect, Effect::RecordCreated { id: rid(2) });
    assert_eq!(executed.receipt, applied(CREATE));
    model.create(record, cells);
    assert_matches_model(&branch, ns, &model, &["a"]);
    assert_eq!(branch.raw_count(ns), Some(2));
}

/// An admission rejection of a read: nothing charged, nothing visited.
fn read_admission(reason: Rejection) -> Rejected {
    Rejected {
        reason,
        receipt: read_rejected(0, reason),
        physical: PhysicalTrace::default(),
    }
}

#[test]
fn get_record_projections_charge_only_the_projected_bytes() {
    let (engine, ns, model) = fixture_abc();
    let snapshot = engine.head_snapshot();
    let branch = fresh_branch(&engine);
    let a = key("a");
    // Record a: key 1 byte; cells $expiresAt 18, kind 8, note 7, price 13.
    // Every read: DirectoryLookup 1 -> 1, PointRead 3 -> 4, then {ResultRow 3
    // + ReadByte p}: ALL p = 47 -> 54; KEY_ONLY p = 1 -> 8; ID_ONLY p = 0
    // -> 7; Named [price, missing] p = 13 -> 20; Named with key [kind, note]
    // p = 1 + 8 + 7 = 16 -> 23; an empty or fully absent selection -> 7 / 8.
    let named_price = Projection::named(false, [name("price"), name("missing")]);
    let named_kind_note = Projection::named(true, [name("kind"), name("note")]);
    let named_nothing = Projection::named(false, []);
    let named_absent = Projection::named(true, [name("missing"), name("zzz")]);
    let vectors: [(&str, &Projection, u64); 7] = [
        ("ALL", &Projection::ALL, 54),
        ("KEY_ONLY", &Projection::KEY_ONLY, 8),
        ("ID_ONLY", &Projection::ID_ONLY, 7),
        ("Named [price, missing]", &named_price, 20),
        ("Named key [kind, note]", &named_kind_note, 23),
        ("Named []", &named_nothing, 7),
        ("Named key [absent]", &named_absent, 8),
    ];
    for (label, projection, total) in vectors {
        let expected = model.row(&a, projection);
        assert!(expected.is_some());
        assert_read_sweep(label, &[1, 4, total], &expected, |budget| {
            snapshot.get_record(ns, &a, projection, budget)
        });
        let on_branch = branch
            .get_record(ns, &a, projection, Budget::unlimited())
            .expect("the branch reads its base");
        assert_eq!(
            (on_branch.value, on_branch.receipt),
            (expected, read_applied(total)),
            "{label}"
        );
    }
    let row = snapshot
        .get_record(ns, &a, &named_kind_note, Budget::unlimited())
        .expect("reads")
        .value
        .expect("bound");
    assert_eq!(row.key, Some(a.clone()));
    let names: Vec<&str> = row.cells.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        names,
        vec!["kind", "note"],
        "projected cells stay in name order"
    );
    let traced_read = snapshot
        .get_record(ns, &a, &Projection::ALL, Budget::unlimited().traced())
        .expect("reads");
    assert_eq!(
        traced_read.receipt,
        traced(
            read_applied(54),
            &[
                (OperationClass::DirectoryLookup, 1, DIRECTORY_LOOKUP),
                (OperationClass::PointRead, 1, POINT_READ),
                (OperationClass::ResultRow, 1, RESULT_ROW),
                (OperationClass::ReadByte, 47, READ_BYTE),
            ],
        )
    );

    // An unbound key is Ok(None) after the lookup, whatever the projection.
    for projection in [&Projection::ALL, &Projection::ID_ONLY, &named_kind_note] {
        assert_read_sweep("unbound", &[1, 4], &None, |budget| {
            snapshot.get_record(ns, &key("zz"), projection, budget)
        });
    }
    // An unknown namespace is charged the directory lookup only.
    let unknown = snapshot
        .get_record(ns_id(2), &a, &Projection::ALL, Budget::unlimited())
        .expect_err("namespace 2 does not exist");
    assert_eq!(
        unknown.receipt,
        read_rejected(1, Rejection::UnknownNamespace)
    );

    // ProjectionTooLarge: 65 names are refused before any charge, even for an
    // unknown namespace; 64 absent names read for the fixed 7.
    let too_many = Projection::named(false, (0..65).map(|i| name(&format!("n{i:02}"))));
    let refused = read_admission(Rejection::InvalidQuery(QueryFault::ProjectionTooLarge));
    for budget in [Budget::unlimited(), Budget::limit(0)] {
        assert_eq!(
            snapshot.get_record(ns, &a, &too_many, budget),
            Err(refused.clone())
        );
        assert_eq!(
            snapshot.get_record(ns_id(2), &a, &too_many, budget),
            Err(refused.clone())
        );
        assert_eq!(
            branch.get_record(ns, &a, &too_many, budget),
            Err(refused.clone())
        );
    }
    let at_limit = Projection::named(false, (0..64).map(|i| name(&format!("n{i:02}"))));
    let read = snapshot
        .get_record(ns, &a, &at_limit, Budget::unlimited())
        .expect("64 names are admitted");
    assert_eq!(read.value, model.row(&a, &at_limit));
    assert_eq!(read.receipt, read_applied(7));
}

#[test]
fn a_pinned_snapshot_answers_get_record_after_later_patches_and_deletes() {
    let (mut engine, ns, model2) = fixture_abc();
    let s2 = engine.head_snapshot();
    let s1 = engine
        .snapshot(s2.base().expect("snapshot 2 has a base"))
        .expect("snapshot 1 is retained");
    let genesis = engine.snapshot(SnapshotId::GENESIS).expect("retained");
    let keys = ["a", "b", "c"];

    // Block 3: patch a (the worked vector, 183) and delete b (106).
    let mut model3 = model2.clone();
    let mut branch = fresh_branch(&engine);
    let retype = vec![(name("price"), attr(Value::U64(10)))];
    let remove = vec![name("note"), name("missing")];
    let patched = apply(
        &mut branch,
        ALICE,
        patch(ns, key("a"), retype.clone(), remove.clone()),
    );
    assert_eq!(patched.receipt, applied(183));
    model3.patch(&key("a"), retype, &remove);
    let deleted = apply(&mut branch, ALICE, delete_record(ns, key("b")));
    assert_eq!(deleted.receipt, applied(106));
    model3.delete(&key("b"));
    engine.publish(&branch.seal()).expect("publishes block 3");
    let s3 = engine.head_snapshot();

    // Block 4: delete a (now 3 cells, 3 attributes: 4 + 20 + 12 + RootUpdate
    // 3*2 + 6*(6 + 4) = 102) and recreate b (id 4): only c is live, so
    // kind=book and price=20 are new: 4 + 20 + 16 + 46 + RowByte 47 +
    // RootUpdate 3*2 + 6*(6 + 4) + 2*30 = 259.
    let mut model4 = model3.clone();
    let mut branch = fresh_branch(&engine);
    let deleted = apply(&mut branch, ALICE, delete_record(ns, key("a")));
    assert_eq!(deleted.effect, Effect::RecordDeleted { id: rid(1) });
    assert_eq!(deleted.receipt, applied(102));
    model4.delete(&key("a"));
    let created = apply(
        &mut branch,
        ALICE,
        create(ns, key("b"), fixture_cells("book", 20)),
    );
    assert_eq!(created.effect, Effect::RecordCreated { id: rid(4) });
    assert_eq!(created.receipt, applied(259));
    assert_eq!(model4.create(key("b"), fixture_cells("book", 20)), rid(4));
    engine.publish(&branch.seal()).expect("publishes block 4");
    let s4 = engine.head_snapshot();

    // Every snapshot answers from its own state with its own charges.
    assert_eq!(genesis.typed_record_count(ns), None);
    let unknown = genesis
        .get_record(ns, &key("a"), &Projection::ALL, Budget::unlimited())
        .expect_err("no namespace at genesis");
    assert_eq!(
        unknown.receipt,
        read_rejected(1, Rejection::UnknownNamespace)
    );
    assert_matches_model(&s1, ns, &Model::default(), &keys);
    assert_eq!(s1.term_count(ns), Some(0));

    assert_matches_model(&s2, ns, &model2, &keys);
    let read = s2
        .get_record(ns, &key("a"), &Projection::ALL, Budget::unlimited())
        .expect("reads");
    assert_eq!(read.receipt, read_applied(54));
    assert_eq!(read.value.map(|row| row.id), Some(rid(1)));

    assert_matches_model(&s3, ns, &model3, &keys);
    // a at snapshot 3: key 1 + $expiresAt 18 + kind 8 + price (5 + 8) = 40
    // projected bytes: 1 + 3 + 3 + 40 = 47.
    let read = s3
        .get_record(ns, &key("a"), &Projection::ALL, Budget::unlimited())
        .expect("reads");
    assert_eq!(read.receipt, read_applied(47));
    assert_eq!(read.value, model3.row(&key("a"), &Projection::ALL));
    let read = s3
        .get_record(ns, &key("b"), &Projection::ALL, Budget::unlimited())
        .expect("reads");
    assert_eq!((read.value, read.receipt), (None, read_applied(4)));

    assert_matches_model(&s4, ns, &model4, &keys);
    let read = s4
        .get_record(ns, &key("a"), &Projection::ALL, Budget::unlimited())
        .expect("reads");
    assert_eq!((read.value, read.receipt), (None, read_applied(4)));
    let read = s4
        .get_record(ns, &key("b"), &Projection::ALL, Budget::unlimited())
        .expect("reads");
    assert_eq!(
        read.receipt,
        read_applied(54),
        "the recreated b has the same bytes"
    );
    assert_eq!(read.value.as_ref().map(|row| row.id), Some(rid(4)));
    assert_eq!(read.value, model4.row(&key("b"), &Projection::ALL));
    let ids: Vec<RecordId> = s4
        .query(
            ns,
            &Query::match_all(),
            &Projection::ID_ONLY,
            10,
            None,
            Budget::unlimited(),
        )
        .expect("reads")
        .value
        .rows
        .iter()
        .map(|row| row.id)
        .collect();
    assert_eq!(ids, vec![rid(3), rid(4)]);

    // The older snapshots are unchanged by everything published after them,
    // and a branch from head sees the head state.
    assert_matches_model(&s2, ns, &model2, &keys);
    assert_matches_model(&s3, ns, &model3, &keys);
    assert_matches_model(&fresh_branch(&engine), ns, &model4, &keys);
    assert!(s4.typed_shared_with(&engine.head_snapshot(), ns));
    assert!(!s4.typed_shared_with(&s3, ns));
}

/// A term created inside a rolled-back transaction is new again afterwards.
///
/// Retyping b's price to U64 creates the (price, U64, 20) value term and the
/// (price, U64) existence term, costing 1 + 3 + {10 + 4 + 13 + RowByte 47 +
/// RootUpdate 2×2} + 2 × {IndexRemove 6 + RootUpdate 2×2} + 2 × {6 + 30 +
/// RootUpdate 2×2} = 82 + 20 + 80 = 182. After the rollback a record holding
/// only price U64(20) pays both creations again, 1 + 3 + {20 + 4 + 13 +
/// RowByte (1 + 13) + RootUpdate 3×2} + 2 × {6 + 30 + 4} = 61 + 80 = 141,
/// whereas without the rollback it would pay 61 + 2 × {6 + 4} = 81.
#[test]
fn rollback_makes_terms_created_inside_the_transaction_new_again() {
    let (engine, ns, _model) = fixture_abc();
    let mut branch = fresh_branch(&engine);
    let retype = vec![(name("price"), attr(Value::U64(20)))];
    let twenty = vec![(name("price"), attr(Value::U64(20)))];

    // Without a rollback the terms are held by b, so the create pays 81.
    let mut probe = fresh_branch(&engine);
    let patched = apply(
        &mut probe,
        ALICE,
        patch(ns, key("b"), retype.clone(), vec![]),
    );
    assert_eq!(patched.receipt, applied(182));
    let held = apply(&mut probe, ALICE, create(ns, key("g"), twenty.clone()));
    assert_eq!(held.receipt, applied(81));
    drop(probe);

    let checkpoint = branch.checkpoint();
    let patched = apply(&mut branch, ALICE, patch(ns, key("b"), retype, vec![]));
    assert_eq!(patched.receipt, applied(182));
    assert_eq!(branch.rollback_to(checkpoint), Ok(0..1));
    let created = apply(&mut branch, ALICE, create(ns, key("g"), twenty));
    assert_eq!(created.effect, Effect::RecordCreated { id: rid(4) });
    assert_eq!(created.receipt, applied(141));
    assert_eq!(branch.index_consistent(ns), Some(true));
}
