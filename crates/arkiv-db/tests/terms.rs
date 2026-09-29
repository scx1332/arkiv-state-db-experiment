//! The `terms` read and the height-by-height expiry sweep, under model
//! version 3.
//!
//! What this file establishes:
//! - `terms(namespace, name, tag, lower, limit)` returns the distinct
//!   attribute values of one `(name, tag)` held by live records, in value
//!   order from the lower bound (`Unbounded`, `Included` or `Excluded`, at
//!   existing and non-existing values including `i64::MIN`, `i64::MAX`,
//!   negative integers, `u64::MAX` and the empty string), truncated to the
//!   limit and decoded exactly as written. Field cells never appear even
//!   when they hold the same value; a missing `(name, tag)` is empty;
//!   deletes and patches (type change, kind flip both ways, value change,
//!   removal) move the distinct set and an emptied value disappears; a
//!   pinned snapshot keeps its terms after later mutations; `Snapshot` and
//!   `Branch` agree.
//! - The admission faults in the brief's order (`NotIndexable`,
//!   `MixedBounds`, `ValueTooLarge`, `ZeroLimit`, `LimitTooLarge`): spent 0,
//!   a Read receipt, nothing recorded, decided before the directory lookup;
//!   a valid read of an unknown namespace is `UnknownNamespace` with spent 1.
//! - Whole receipts as hand-derived FIXTURE literals: DirectoryLookup 1 +
//!   TermSeek 4 + per returned term `{TermVisit 3, ReadByte encoded bytes}`;
//!   an `Excluded` existing bound and the key ending the run are free; a
//!   missing run costs the seek only (5); the traced ledger in first-charge
//!   order; the charge point under every budget with no partial result; and
//!   positional cost (terms before the bound are never charged).
//! - Isolation: identical terms in another namespace never appear and leave
//!   the first namespace's receipts and index nodes unchanged.
//! - `expire_records` sweeps height by height: with heights 3, 3, 5, 7, 7, 7
//!   it deletes exactly the attribute rows in (height, id) order with the
//!   literal delete receipts, a field-kind and an absent `$expiresAt`
//!   survive, only the deletes enter the block, a lower block stops at the
//!   first height above it, and a rejection (a starved delete budget, a
//!   budget admitting one row only, a query budget one unit short of the
//!   `terms` read) halts with the deletes so far applied and staying applied.
//!
//! Every expected value comes from a `BTreeMap` reference model that orders
//! values numerically or bytewise on its own, never from the engine's index;
//! every cost is a literal derived by hand next to it (the model-side cost
//! check inside the semantics sweep uses only the transcribed FIXTURE
//! weights). What it does not establish: query evaluation and its charges
//! (the `Eq` pages inside the sweep are observed only through their
//! deletes), cursor binding, and anything about durability or roots.

use std::{cmp::Ordering, collections::BTreeMap, fmt::Debug, ops::Bound};

use arkiv_db::{
    BlockNumber, Branch, Budget, Cell, CellName, Cost, Effect, Engine, Executed, ExpiryHalt,
    Ledger, LedgerEntry, Limits, Metered, NamespaceId, OperationClass, Outcome, OwnerId,
    PhysicalTrace, PriceSchedule, Projection, QueryFault, ReadResult, Receipt, ReceiptKind,
    RecordId, RecordKey, Rejected, Rejection, Row, Snapshot, SnapshotId, TypeTag, Value, WriteOp,
    expire_records,
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
const POINT_READ: u64 = 3;
const READ_BYTE: u64 = 1;
const RECORD_DELETE: u64 = 20;
const CELL_REMOVE: u64 = 4;
const INDEX_REMOVE: u64 = 6;
const TERM_SEEK: u64 = 4;
const TERM_VISIT: u64 = 3;
const ROOT_UPDATE: u64 = 2;

/// The fixture namespace has ordinal 1.
const NS: u64 = 1;
/// Every tag that has an index.
const INDEXABLE: [TypeTag; 4] = [TypeTag::Bool, TypeTag::I64, TypeTag::U64, TypeTag::Str];
/// The page limits the semantics sweep uses.
const LIMITS: [usize; 4] = [1, 2, 5, 200];

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

fn i64s(values: &[i64]) -> Vec<Value> {
    values.iter().map(|&value| Value::I64(value)).collect()
}

fn u64s(values: &[u64]) -> Vec<Value> {
    values.iter().map(|&value| Value::U64(value)).collect()
}

fn strs(values: &[&str]) -> Vec<Value> {
    values.iter().map(|&value| Value::str(value)).collect()
}

fn bools(values: &[bool]) -> Vec<Value> {
    values.iter().map(|&value| Value::Bool(value)).collect()
}

type Cells = Vec<(CellName, Cell)>;

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

/// A read admission rejection: nothing charged, nothing visited.
fn read_admission(reason: Rejection) -> Rejected {
    Rejected {
        reason,
        receipt: read_rejected(0, reason),
        physical: PhysicalTrace::default(),
    }
}

fn apply(branch: &mut Branch, actor: OwnerId, op: WriteOp) -> Executed {
    branch
        .execute(actor, op, Budget::unlimited())
        .unwrap_or_else(|rejected| panic!("an unlimited write applies: {rejected}"))
}

fn fresh_branch(engine: &Engine) -> Branch {
    engine
        .begin(engine.head(), None)
        .expect("begin on the current head")
}

fn publish(engine: &mut Engine, branch: Branch) -> SnapshotId {
    engine
        .publish(&branch.seal())
        .expect("publishes on the current head")
}

/// Snapshot 1: one namespace (ordinal 1, owned by ALICE), no records, on an
/// engine with `limits`.
fn fixture_with_limits(limits: Limits) -> (Engine, NamespaceId) {
    let mut engine = Engine::with_limits(PriceSchedule::FIXTURE, limits);
    let mut branch = fresh_branch(&engine);
    let created = apply(
        &mut branch,
        ALICE,
        WriteOp::CreateNamespace {
            name: arkiv_db::NamespaceName::new("test").unwrap(),
        },
    );
    assert_eq!(created.effect, Effect::NamespaceCreated(ns_id(NS)));
    assert_eq!(publish(&mut engine, branch).sequence(), 1);
    (engine, ns_id(NS))
}

/// Snapshot 1 with the default limits.
fn fixture() -> (Engine, NamespaceId) {
    fixture_with_limits(Limits::DEFAULT)
}

/// Creates `records` in one fresh branch as `actor`, mirrors them into
/// `model` (ids in creation order) and publishes the result.
fn populate(
    engine: &mut Engine,
    actor: OwnerId,
    ns: NamespaceId,
    records: &[(&str, Cells)],
    model: &mut Model,
) -> SnapshotId {
    let mut branch = fresh_branch(engine);
    for (text, cells) in records {
        let executed = apply(&mut branch, actor, create(ns, key(text), cells.clone()));
        let id = model.create(key(text), cells.clone());
        assert_eq!(
            executed.effect,
            Effect::RecordCreated { id },
            "create {text}"
        );
    }
    publish(engine, branch)
}

// ---------------------------------------------------------------------------
// Reference model: the record semantics of the brief over a BTreeMap. Value
// order is the model's own (numeric for the integers, bytewise for text,
// false < true); it never looks at term bytes or at the engine's index.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
struct Model {
    next_id: u64,
    live: BTreeMap<RecordKey, (RecordId, BTreeMap<CellName, Cell>)>,
}

/// Orders two attribute values of the same indexable type.
fn value_order(left: &Value, right: &Value) -> Ordering {
    match (left, right) {
        (Value::Bool(left), Value::Bool(right)) => left.cmp(right),
        (Value::I64(left), Value::I64(right)) => left.cmp(right),
        (Value::U64(left), Value::U64(right)) => left.cmp(right),
        (Value::Str(left), Value::Str(right)) => left.as_bytes().cmp(right.as_bytes()),
        _ => panic!("model: {left:?} and {right:?} are not ordered together"),
    }
}

/// The encoded bytes the brief charges per returned term.
fn value_bytes(value: &Value) -> u64 {
    match value {
        Value::I32(_) => 4,
        Value::Addr(_) => 20,
        Value::U256(_) | Value::Dec(_) | Value::Bytes32(_) | Value::Key(_) => 32,
        Value::Bool(_) => 1,
        Value::I64(_) | Value::U64(_) => 8,
        Value::Str(text) => u64::try_from(text.len()).expect("lengths fit"),
        Value::Bytes(_) => panic!("model: bytes are never terms"),
    }
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

    fn id(&self, key: &RecordKey) -> RecordId {
        self.live.get(key).expect("model: id of a bound key").0
    }

    /// The fully projected row of a live key.
    fn row(&self, key: &RecordKey) -> Option<Row> {
        let (id, cells) = self.live.get(key)?;
        Some(Row {
            id: *id,
            key: Some(key.clone()),
            cells: cells.iter().map(|(n, c)| (n.clone(), c.clone())).collect(),
        })
    }

    fn keys(&self) -> Vec<RecordKey> {
        self.live.keys().cloned().collect()
    }

    fn record_count(&self) -> usize {
        self.live.len()
    }

    /// The distinct attribute values of `(name, tag)` over the live
    /// records, in value order. Fields and other types do not count.
    fn distinct(&self, name: &CellName, tag: TypeTag) -> Vec<Value> {
        let mut values: Vec<Value> = self
            .live
            .values()
            .filter_map(|(_, cells)| cells.get(name))
            .filter(|cell| cell.is_attribute() && cell.value.tag() == tag)
            .map(|cell| cell.value.clone())
            .collect();
        values.sort_by(value_order);
        values.dedup();
        values
    }

    /// The `terms` result: the distinct values at or after `lower`, at most
    /// `limit` of them.
    fn terms(
        &self,
        name: &CellName,
        tag: TypeTag,
        lower: &Bound<Value>,
        limit: usize,
    ) -> Vec<Value> {
        self.distinct(name, tag)
            .into_iter()
            .filter(|value| match lower {
                Bound::Unbounded => true,
                Bound::Included(bound) => value_order(value, bound) != Ordering::Less,
                Bound::Excluded(bound) => value_order(value, bound) == Ordering::Greater,
            })
            .take(limit)
            .collect()
    }

    /// What a `terms` result costs under FIXTURE: the lookup, the seek and
    /// one `{TermVisit, ReadByte bytes}` group per returned value.
    fn terms_cost(values: &[Value]) -> u64 {
        DIRECTORY_LOOKUP
            + TERM_SEEK
            + values
                .iter()
                .map(|value| TERM_VISIT + READ_BYTE * value_bytes(value))
                .sum::<u64>()
    }

    /// Distinct value terms plus distinct existence terms over the
    /// attribute cells of live records.
    fn term_count(&self) -> usize {
        let mut pairs: Vec<(CellName, TypeTag)> = self
            .live
            .values()
            .flat_map(|(_, cells)| cells.iter())
            .filter(|(_, cell)| cell.is_attribute())
            .map(|(name, cell)| (name.clone(), cell.value.tag()))
            .collect();
        pairs.sort();
        pairs.dedup();
        pairs
            .iter()
            .map(|(name, tag)| self.distinct(name, *tag).len() + 1)
            .sum()
    }
}

/// The `terms` read shared by branches and snapshots.
trait TermsView {
    fn terms_read(
        &self,
        ns: NamespaceId,
        name: &CellName,
        tag: TypeTag,
        lower: Bound<Value>,
        limit: usize,
        budget: Budget,
    ) -> ReadResult<Vec<Value>>;
}

impl TermsView for Snapshot {
    fn terms_read(
        &self,
        ns: NamespaceId,
        name: &CellName,
        tag: TypeTag,
        lower: Bound<Value>,
        limit: usize,
        budget: Budget,
    ) -> ReadResult<Vec<Value>> {
        self.terms(ns, name, tag, lower, limit, budget)
    }
}

impl TermsView for Branch {
    fn terms_read(
        &self,
        ns: NamespaceId,
        name: &CellName,
        tag: TypeTag,
        lower: Bound<Value>,
        limit: usize,
        budget: Budget,
    ) -> ReadResult<Vec<Value>> {
        self.terms(ns, name, tag, lower, limit, budget)
    }
}

/// Runs `check` on the head snapshot and on a fresh branch of `engine`.
fn for_each_view(engine: &Engine, mut check: impl FnMut(&dyn TermsView, &str)) {
    check(&engine.head_snapshot(), "snapshot");
    check(&fresh_branch(engine), "branch");
}

/// An unlimited `terms` read that must apply.
fn read(
    view: &dyn TermsView,
    ns: NamespaceId,
    name: &CellName,
    tag: TypeTag,
    lower: Bound<Value>,
    limit: usize,
) -> Metered<Vec<Value>> {
    view.terms_read(ns, name, tag, lower, limit, Budget::unlimited())
        .unwrap_or_else(|rejected| panic!("an unlimited terms read applies: {rejected}"))
}

/// The lower bounds the semantics sweep probes for `tag`: unbounded, then
/// `Included` and `Excluded` at values that do and do not exist in the
/// mixed fixture, including the extremes of each type.
fn probes(tag: TypeTag) -> Vec<Bound<Value>> {
    let values = match tag {
        TypeTag::Bool => bools(&[false, true]),
        TypeTag::I64 => i64s(&[i64::MIN, -5, -3, -1, 0, 1, 2, 7, 42, 100, i64::MAX]),
        TypeTag::U64 => u64s(&[0, 1, 5, 258, 259, 1000, u64::MAX]),
        TypeTag::Str => strs(&[
            "", "a", "aa", "ab", "abc", "b", "ba", "c", "hello", "héllo", "zz",
        ]),
        TypeTag::Bytes => panic!("bytes has no terms"),
        _ => vec![],
    };
    let mut bounds = vec![Bound::Unbounded];
    for value in values {
        bounds.push(Bound::Included(value.clone()));
        bounds.push(Bound::Excluded(value));
    }
    bounds
}

/// Asserts that `view` answers every (name, tag, lower, limit) probe over
/// the mixed fixture's names exactly as the model does, with a Read receipt
/// of the model's cost and no record-map visit.
fn assert_terms_match_model(view: &dyn TermsView, ns: NamespaceId, model: &Model, label: &str) {
    for text in ["v", "w", "u"] {
        let name = name(text);
        for tag in INDEXABLE {
            for lower in probes(tag) {
                for limit in LIMITS {
                    let got = read(view, ns, &name, tag, lower.clone(), limit);
                    let want = model.terms(&name, tag, &lower, limit);
                    let context = format!("{label}: terms({text}, {tag}, {lower:?}, {limit})");
                    assert_eq!(got.value, want, "{context}");
                    assert!(got.value.len() <= limit, "{context}");
                    assert!(
                        got.value.iter().all(|value| value.tag() == tag),
                        "{context}"
                    );
                    assert_eq!(
                        got.receipt,
                        read_applied(Model::terms_cost(&want)),
                        "{context}"
                    );
                    assert_eq!(got.physical.record_nodes, 0, "{context}");
                }
            }
        }
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
// The mixed fixture: 40 records keyed k01..k40 in creation order. `v` is the
// probed name; it appears as attributes of every indexable type (with
// duplicates), as fields holding the very same values (plus values held by
// no attribute), and not at all. `w` holds one attribute per type; `other`
// is never probed; `u` never exists.
// ---------------------------------------------------------------------------

fn mixed_records() -> Vec<(&'static str, Cells)> {
    let v = |cell: Cell| vec![(name("v"), cell)];
    let vw = |cell: Cell, w: Cell| vec![(name("v"), cell), (name("w"), w)];
    vec![
        // v as I64 attributes: distinct MIN, -5, -1, 0, 1, 7, MAX.
        ("k01", v(attr(Value::I64(-5)))),
        ("k02", v(attr(Value::I64(-1)))),
        ("k03", v(attr(Value::I64(0)))),
        ("k04", v(attr(Value::I64(1)))),
        ("k05", v(attr(Value::I64(7)))),
        ("k06", v(attr(Value::I64(i64::MIN)))),
        ("k07", v(attr(Value::I64(i64::MAX)))),
        ("k08", v(attr(Value::I64(-1)))),
        ("k09", vw(attr(Value::I64(7)), attr(Value::I64(3)))),
        ("k10", vw(attr(Value::I64(0)), attr(Value::str("w")))),
        // v as U64 attributes: distinct 0, 1, 258, 1000, MAX.
        ("k11", v(attr(Value::U64(0)))),
        ("k12", v(attr(Value::U64(1)))),
        ("k13", v(attr(Value::U64(258)))),
        ("k14", v(attr(Value::U64(1000)))),
        ("k15", v(attr(Value::U64(u64::MAX)))),
        ("k16", v(attr(Value::U64(258)))),
        ("k17", vw(attr(Value::U64(u64::MAX)), attr(Value::U64(9)))),
        // v as Str attributes: distinct "", "a", "ab", "b", "ba", "héllo".
        ("k18", v(attr(Value::str("")))),
        ("k19", v(attr(Value::str("a")))),
        ("k20", v(attr(Value::str("ab")))),
        ("k21", v(attr(Value::str("b")))),
        ("k22", v(attr(Value::str("ba")))),
        ("k23", v(attr(Value::str("héllo")))),
        ("k24", v(attr(Value::str("a")))),
        ("k25", vw(attr(Value::str("")), attr(Value::Bool(true)))),
        // v as Bool attributes: distinct false, true.
        ("k26", v(attr(Value::Bool(true)))),
        ("k27", v(attr(Value::Bool(false)))),
        ("k28", v(attr(Value::Bool(true)))),
        // v as fields: the same values as above, then values no attribute
        // holds. None of them is ever a term.
        ("k29", v(field(Value::I64(-5)))),
        ("k30", v(field(Value::I64(i64::MIN)))),
        ("k31", v(field(Value::U64(258)))),
        ("k32", v(field(Value::U64(u64::MAX)))),
        ("k33", v(field(Value::str("a")))),
        ("k34", v(field(Value::str("")))),
        ("k35", v(field(Value::Bool(true)))),
        ("k36", v(field(Value::bytes(b"xyz")))),
        ("k37", v(field(Value::I64(42)))),
        ("k38", v(field(Value::str("zzz")))),
        // v absent.
        ("k39", vec![(name("w"), attr(Value::I64(3)))]),
        ("k40", vec![(name("other"), attr(Value::str("x")))]),
    ]
}

/// Snapshot 2: the mixed fixture in namespace 1, with its model.
fn mixed_fixture() -> (Engine, NamespaceId, Model) {
    let (mut engine, ns) = fixture();
    let mut model = Model::default();
    let published = populate(&mut engine, ALICE, ns, &mixed_records(), &mut model);
    assert_eq!(published.sequence(), 2);
    assert_eq!(model.record_count(), 40);
    (engine, ns, model)
}

/// Mutates the mixed fixture on `branch` and mirrors it into `model`: two
/// deletes (one emptying a unique value, one leaving a duplicate), a type
/// change, a kind flip each way, two value changes (one emptying a value)
/// and a removal (emptying a value).
fn mutate_mixed(branch: &mut Branch, ns: NamespaceId, model: &mut Model) {
    for text in ["k07", "k05"] {
        let id = model.delete(&key(text));
        let executed = apply(branch, ALICE, delete_record(ns, key(text)));
        assert_eq!(
            executed.effect,
            Effect::RecordDeleted { id },
            "delete {text}"
        );
    }
    let v = name("v");
    let patches: Vec<(&str, Cells, Vec<CellName>)> = vec![
        // type change: I64(-5) -> U64(5); -5 empties, 5 appears.
        ("k01", vec![(v.clone(), attr(Value::U64(5)))], vec![]),
        // kind flip attribute -> field: "ba" empties.
        ("k22", vec![(v.clone(), field(Value::str("ba")))], vec![]),
        // kind flip field -> attribute: 42 appears.
        ("k37", vec![(v.clone(), attr(Value::I64(42)))], vec![]),
        // value change: "héllo" empties, "hello" appears.
        ("k23", vec![(v.clone(), attr(Value::str("hello")))], vec![]),
        // value change onto an existing value: false empties.
        ("k27", vec![(v.clone(), attr(Value::Bool(true)))], vec![]),
        // removal: 1000 empties.
        ("k14", vec![], vec![v.clone()]),
    ];
    for (text, set, remove) in patches {
        let id = model.id(&key(text));
        let executed = apply(
            branch,
            ALICE,
            patch(ns, key(text), set.clone(), remove.clone()),
        );
        assert_eq!(
            executed.effect,
            Effect::RecordPatched { id },
            "patch {text}"
        );
        model.patch(&key(text), set, &remove);
    }
}

// ---------------------------------------------------------------------------
// The cost fixture: a, b, c (ids 1, 2, 3) with price: I64 10, 20, 5 and
// kind: Str "", "a", "ab". Runs: price/I64 = [5, 10, 20] (8 bytes each),
// kind/Str = ["", "a", "ab"] (0, 1, 2 bytes).
// ---------------------------------------------------------------------------

fn cost_records() -> Vec<(&'static str, Cells)> {
    let record = |price: i64, kind: &str| {
        vec![
            (name("price"), attr(Value::I64(price))),
            (name("kind"), attr(Value::str(kind))),
        ]
    };
    vec![
        ("a", record(10, "")),
        ("b", record(20, "a")),
        ("c", record(5, "ab")),
    ]
}

/// Snapshot 2: the cost fixture in namespace 1.
fn cost_fixture() -> (Engine, NamespaceId) {
    let (mut engine, ns) = fixture();
    let mut model = Model::default();
    let published = populate(&mut engine, ALICE, ns, &cost_records(), &mut model);
    assert_eq!(published.sequence(), 2);
    assert_eq!(
        model.distinct(&name("price"), TypeTag::I64),
        i64s(&[5, 10, 20])
    );
    assert_eq!(
        model.distinct(&name("kind"), TypeTag::Str),
        strs(&["", "a", "ab"])
    );
    (engine, ns)
}

/// Cumulative charge points of the price/I64 run, `Unbounded`, limit 200:
/// lookup 1, seek 4, then 11 per term: 1, 5, 16, 27, 38.
const I64_RUN: [u64; 5] = [1, 5, 16, 27, 38];
/// The same run with limit 1: 16.
const I64_RUN_LIMIT_1: [u64; 3] = [1, 5, 16];
/// The same run from `Excluded(5)`: 5 is stepped over free: 1, 5, 16, 27.
const I64_RUN_EXCLUDED_5: [u64; 4] = [1, 5, 16, 27];
/// The kind/Str run, `Unbounded`, limit 200: 1, 5, then 3 + 0, 3 + 1, 3 + 2:
/// 1, 5, 8, 12, 17.
const STR_RUN: [u64; 5] = [1, 5, 8, 12, 17];
/// A missing (name, tag): the lookup and the seek only: 1, 5.
const MISSING_RUN: [u64; 2] = [1, 5];

// ---------------------------------------------------------------------------
// The expiry fixture: r1..r6 (ids 1..6) hold `$expiresAt` as a U64
// attribute with heights 7, 3, 5, 7, 3, 7 (so (height, id) order differs
// from id order); f (id 7) holds it as a field; n (id 8) lacks it.
//
// Delete cost (model 3) = DirectoryLookup 1 + PointRead 3 + {RecordDelete
// 20 + CellRemove 4c + RootUpdate 3*2} + {IndexRemove 6 + RootUpdate 2*2}
// per term held (two per attribute cell) = 30 + 4c + 20a:
//   r1: c 2, a 2 -> 78   r2: c 1, a 1 -> 54   r3: c 3, a 2 -> 82
//   r4: c 3, a 3 -> 102  r5: c 2, a 1 -> 58   r6: c 1, a 1 -> 54
// ---------------------------------------------------------------------------

fn expiry_records() -> Vec<(&'static str, Cells)> {
    let expires = |height: u64| (name("$expiresAt"), attr(Value::U64(height)));
    let kind = |text: &str| (name("kind"), attr(Value::str(text)));
    let note = || (name("note"), field(Value::bytes(b"xyz")));
    vec![
        ("r1", vec![expires(7), kind("a")]),
        ("r2", vec![expires(3)]),
        ("r3", vec![expires(5), kind("b"), note()]),
        (
            "r4",
            vec![expires(7), kind("a"), (name("score"), attr(Value::I64(1)))],
        ),
        ("r5", vec![expires(3), note()]),
        ("r6", vec![expires(7)]),
        (
            "f",
            vec![(name("$expiresAt"), field(Value::U64(3))), kind("f")],
        ),
        ("n", vec![kind("n")]),
    ]
}

/// The deletes a sweep to block 7 applies, in (height, id) order, with the
/// literal cost of each: heights 3 (ids 2, 5), 5 (id 3), 7 (ids 1, 4, 6).
const EXPIRY_DELETES: [(u64, u64); 6] = [(2, 54), (5, 58), (3, 82), (1, 78), (4, 102), (6, 54)];
/// The sum of the six delete receipts: 54 + 58 + 82 + 78 + 102 + 54.
const EXPIRY_TOTAL: u64 = 428;
/// The `terms` read that opens every sweep iteration: lookup 1, seek 4,
/// one U64 term {3 + 8}: 16.
const EXPIRY_TERMS_READ: u64 = 16;
/// Cumulative charge points of the delete of r2 (c 1, a 1): lookup 1, point
/// read 4, {20 + 4 + 6} 34, {IndexRemove 6 + RootUpdate 4} 44, 54.
const DELETE_R2: [u64; 5] = [1, 4, 34, 44, 54];
/// Cumulative charge points of the delete of r5 (c 2, a 1): 1, 4, {20 + 8
/// + 6} 38, 48, 58.
const DELETE_R5: [u64; 5] = [1, 4, 38, 48, 58];

/// Snapshot 2: the expiry fixture in namespace 1, with its model.
fn expiry_fixture() -> (Engine, NamespaceId, Model) {
    let (mut engine, ns) = fixture();
    let mut model = Model::default();
    let published = populate(&mut engine, ALICE, ns, &expiry_records(), &mut model);
    assert_eq!(published.sequence(), 2);
    assert_eq!(model.record_count(), 8);
    assert_eq!(
        model.distinct(&name("$expiresAt"), TypeTag::U64),
        u64s(&[3, 5, 7])
    );
    (engine, ns, model)
}

fn expiry_name() -> CellName {
    name("$expiresAt")
}

/// `expire_records` on the fixture name; the large `Err` mirrors the engine's own signature.
#[allow(clippy::result_large_err)]
fn sweep(
    branch: &mut Branch,
    ns: NamespaceId,
    block: u64,
    query_budget: Budget,
    delete_budget: Budget,
) -> Result<Vec<(RecordId, Receipt)>, ExpiryHalt> {
    expire_records(
        branch,
        ns,
        &expiry_name(),
        BlockNumber(block),
        query_budget,
        delete_budget,
    )
}

/// The deletes `EXPIRY_DELETES[range]` as the sweep reports them.
fn expected_deletes(deletes: &[(u64, u64)]) -> Vec<(RecordId, Receipt)> {
    deletes
        .iter()
        .map(|&(ordinal, cost)| (rid(ordinal), applied(cost)))
        .collect()
}

/// Asserts that `branch` holds exactly the model's records (full projection
/// by key) and statistics.
fn assert_branch_matches_model(branch: &Branch, ns: NamespaceId, model: &Model, label: &str) {
    assert_eq!(
        branch.typed_record_count(ns),
        Some(model.record_count()),
        "{label}"
    );
    assert_eq!(branch.term_count(ns), Some(model.term_count()), "{label}");
    assert_eq!(branch.index_consistent(ns), Some(true), "{label}");
    for (text, _) in expiry_records() {
        let got = branch
            .get_record(ns, &key(text), &Projection::ALL, Budget::unlimited())
            .unwrap_or_else(|rejected| panic!("{label}: get_record {text}: {rejected}"))
            .value;
        assert_eq!(got, model.row(&key(text)), "{label}: record {text}");
    }
}

// ---------------------------------------------------------------------------
// 1. Semantics against the model
// ---------------------------------------------------------------------------

#[test]
fn terms_equal_the_models_distinct_attribute_values_from_every_bound_and_limit() {
    let (engine, ns, model) = mixed_fixture();
    let (v, w, u) = (name("v"), name("w"), name("u"));
    // The model's own reading of the fixture, spelled out.
    assert_eq!(
        model.distinct(&v, TypeTag::I64),
        i64s(&[i64::MIN, -5, -1, 0, 1, 7, i64::MAX])
    );
    assert_eq!(
        model.distinct(&v, TypeTag::U64),
        u64s(&[0, 1, 258, 1000, u64::MAX])
    );
    assert_eq!(
        model.distinct(&v, TypeTag::Str),
        strs(&["", "a", "ab", "b", "ba", "héllo"])
    );
    assert_eq!(model.distinct(&v, TypeTag::Bool), bools(&[false, true]));
    assert_eq!(model.distinct(&w, TypeTag::I64), i64s(&[3]));
    assert_eq!(model.distinct(&w, TypeTag::U64), u64s(&[9]));
    assert_eq!(model.distinct(&w, TypeTag::Str), strs(&["w"]));
    assert_eq!(model.distinct(&w, TypeTag::Bool), bools(&[true]));
    for tag in INDEXABLE {
        assert_eq!(model.distinct(&u, tag), Vec::<Value>::new());
    }
    // 7 + 5 + 6 + 2 value terms + 4 existence terms for v, 1 + 1 for each
    // of w's four types, 1 + 1 for other/Str = 24 + 8 + 2 = 34.
    assert_eq!(model.term_count(), 34);
    assert_eq!(engine.head_snapshot().term_count(ns), Some(34));
    assert_eq!(engine.head_snapshot().index_consistent(ns), Some(true));

    for_each_view(&engine, |view, label| {
        assert_terms_match_model(view, ns, &model, label);
        // Field-only values never appear, whatever the bound.
        let all = read(view, ns, &v, TypeTag::I64, Bound::Unbounded, 200).value;
        assert!(!all.contains(&Value::I64(42)), "{label}");
        let all = read(view, ns, &v, TypeTag::Str, Bound::Unbounded, 200).value;
        assert!(!all.contains(&Value::str("zzz")), "{label}");
        // The extremes round-trip and order numerically, not bytewise on
        // the two's complement.
        assert_eq!(
            read(view, ns, &v, TypeTag::I64, Bound::Unbounded, 2).value,
            i64s(&[i64::MIN, -5]),
            "{label}"
        );
        assert_eq!(
            read(
                view,
                ns,
                &v,
                TypeTag::I64,
                Bound::Excluded(Value::I64(7)),
                200
            )
            .value,
            i64s(&[i64::MAX]),
            "{label}"
        );
        assert_eq!(
            read(
                view,
                ns,
                &v,
                TypeTag::U64,
                Bound::Excluded(Value::U64(1000)),
                200
            )
            .value,
            u64s(&[u64::MAX]),
            "{label}"
        );
        assert_eq!(
            read(
                view,
                ns,
                &v,
                TypeTag::Str,
                Bound::Included(Value::str("")),
                1
            )
            .value,
            strs(&[""]),
            "{label}"
        );
    });
}

#[test]
fn terms_follow_deletes_and_patches_and_an_emptied_value_disappears() {
    let (mut engine, ns, mut model) = mixed_fixture();
    let v = name("v");
    let mut branch = fresh_branch(&engine);
    mutate_mixed(&mut branch, ns, &mut model);
    // The model after the mutations, spelled out.
    assert_eq!(
        model.distinct(&v, TypeTag::I64),
        i64s(&[i64::MIN, -1, 0, 1, 7, 42])
    );
    assert_eq!(
        model.distinct(&v, TypeTag::U64),
        u64s(&[0, 1, 5, 258, u64::MAX])
    );
    assert_eq!(
        model.distinct(&v, TypeTag::Str),
        strs(&["", "a", "ab", "b", "hello"])
    );
    assert_eq!(model.distinct(&v, TypeTag::Bool), bools(&[true]));
    // 6 + 5 + 5 + 1 value terms + 4 existence terms for v, 8 for w, 2 for
    // other = 21 + 8 + 2 = 31.
    assert_eq!(model.term_count(), 31);

    assert_terms_match_model(&branch, ns, &model, "branch after the mutations");
    assert_eq!(branch.term_count(ns), Some(31));
    assert_eq!(branch.index_consistent(ns), Some(true));

    let published = publish(&mut engine, branch);
    assert_eq!(published.sequence(), 3);
    for_each_view(&engine, |view, label| {
        assert_terms_match_model(view, ns, &model, label);
        let terms = |tag, lower| read(view, ns, &v, tag, lower, 200).value;
        // Emptied values are gone.
        assert!(
            !terms(TypeTag::I64, Bound::Unbounded).contains(&Value::I64(i64::MAX)),
            "{label}"
        );
        assert!(
            !terms(TypeTag::I64, Bound::Unbounded).contains(&Value::I64(-5)),
            "{label}"
        );
        assert!(
            !terms(TypeTag::U64, Bound::Unbounded).contains(&Value::U64(1000)),
            "{label}"
        );
        assert!(
            !terms(TypeTag::Str, Bound::Unbounded).contains(&Value::str("ba")),
            "{label}"
        );
        assert!(
            !terms(TypeTag::Str, Bound::Unbounded).contains(&Value::str("héllo")),
            "{label}"
        );
        assert_eq!(
            terms(TypeTag::Bool, Bound::Unbounded),
            bools(&[true]),
            "{label}"
        );
        // A duplicate survives its partner's deletion; new values appear.
        assert_eq!(
            terms(TypeTag::I64, Bound::Included(Value::I64(7))),
            i64s(&[7, 42]),
            "{label}"
        );
        assert_eq!(
            terms(TypeTag::U64, Bound::Included(Value::U64(2))),
            u64s(&[5, 258, u64::MAX]),
            "{label}"
        );
        assert_eq!(
            terms(TypeTag::Str, Bound::Included(Value::str("b"))),
            strs(&["b", "hello"]),
            "{label}"
        );
    });
}

#[test]
fn a_historical_snapshot_keeps_its_terms_after_later_mutations() {
    let (mut engine, ns, mut model) = mixed_fixture();
    let before = model.clone();
    let pinned = engine.head_snapshot();
    assert_eq!(pinned.id().sequence(), 2);

    let mut branch = fresh_branch(&engine);
    mutate_mixed(&mut branch, ns, &mut model);
    assert_terms_match_model(&pinned, ns, &before, "pinned while a branch mutates");
    let published = publish(&mut engine, branch);
    assert_eq!(published.sequence(), 3);
    assert_terms_match_model(&pinned, ns, &before, "pinned after publication");
    let repinned = engine
        .snapshot(pinned.id())
        .expect("snapshot 2 is retained");
    assert_terms_match_model(&repinned, ns, &before, "snapshot 2 pinned again");
    assert_eq!(pinned.term_count(ns), Some(34));

    // A branch that empties the namespace and is discarded changes nothing.
    let mut scratch = fresh_branch(&engine);
    for key in model.keys() {
        apply(&mut scratch, ALICE, delete_record(ns, key));
    }
    let empty = Model::default();
    assert_terms_match_model(&scratch, ns, &empty, "emptied scratch branch");
    assert_eq!(scratch.term_count(ns), Some(0));
    drop(scratch);
    assert_terms_match_model(&pinned, ns, &before, "pinned after a discarded branch");
    assert_terms_match_model(
        &engine.head_snapshot(),
        ns,
        &model,
        "head after a discarded branch",
    );
}

// ---------------------------------------------------------------------------
// 2. Admission validation
// ---------------------------------------------------------------------------

#[test]
fn terms_validation_faults_are_admission_rejections_in_the_briefs_order() {
    // max_value_len 4: "abcd" is the longest admissible Str bound.
    let limits = Limits {
        max_value_len: 4,
        ..Limits::DEFAULT
    };
    let (mut engine, ns) = fixture_with_limits(limits);
    let mut model = Model::default();
    let records = vec![("k", vec![(name("v"), attr(Value::str("abcd")))])];
    assert_eq!(
        populate(&mut engine, ALICE, ns, &records, &mut model).sequence(),
        2
    );
    let v = name("v");
    let long = Value::str("abcde");
    let fits = Value::str("abcd");

    // Each request carries every later fault too; the first in the brief's
    // order wins.
    let chain: [(TypeTag, Bound<Value>, usize, QueryFault); 5] = [
        (
            TypeTag::Bytes,
            Bound::Included(long.clone()),
            0,
            QueryFault::NotIndexable,
        ),
        (
            TypeTag::I64,
            Bound::Included(long.clone()),
            0,
            QueryFault::MixedBounds,
        ),
        (
            TypeTag::Str,
            Bound::Included(long.clone()),
            0,
            QueryFault::ValueTooLarge,
        ),
        (
            TypeTag::Str,
            Bound::Included(fits.clone()),
            0,
            QueryFault::ZeroLimit,
        ),
        (
            TypeTag::Str,
            Bound::Included(fits.clone()),
            201,
            QueryFault::LimitTooLarge,
        ),
    ];
    // Each fault alone.
    let single: [(TypeTag, Bound<Value>, usize, QueryFault); 7] = [
        (
            TypeTag::Bytes,
            Bound::Unbounded,
            1,
            QueryFault::NotIndexable,
        ),
        (
            TypeTag::I64,
            Bound::Excluded(Value::U64(1)),
            1,
            QueryFault::MixedBounds,
        ),
        (
            TypeTag::Bool,
            Bound::Included(Value::I64(0)),
            1,
            QueryFault::MixedBounds,
        ),
        (
            TypeTag::Str,
            Bound::Excluded(long.clone()),
            1,
            QueryFault::ValueTooLarge,
        ),
        (TypeTag::I64, Bound::Unbounded, 0, QueryFault::ZeroLimit),
        (
            TypeTag::U64,
            Bound::Unbounded,
            201,
            QueryFault::LimitTooLarge,
        ),
        (
            TypeTag::Str,
            Bound::Included(fits.clone()),
            usize::MAX,
            QueryFault::LimitTooLarge,
        ),
    ];

    let branch = fresh_branch(&engine);
    let snapshot = engine.head_snapshot();
    let views: [(&dyn TermsView, &str); 2] = [(&snapshot, "snapshot"), (&branch, "branch")];
    for (view, label) in views {
        for (tag, lower, limit, fault) in chain.iter().chain(single.iter()).cloned() {
            let context = format!("{label}: terms(v, {tag}, {lower:?}, {limit})");
            let result = view.terms_read(ns, &v, tag, lower.clone(), limit, Budget::unlimited());
            let reason = Rejection::InvalidQuery(fault);
            assert_eq!(result.unwrap_err(), read_admission(reason), "{context}");
            // A traced budget adds an empty ledger to the zero-cost receipt.
            let traced_rejection = view
                .terms_read(
                    ns,
                    &v,
                    tag,
                    lower.clone(),
                    limit,
                    Budget::unlimited().traced(),
                )
                .unwrap_err();
            assert_eq!(traced_rejection.reason, reason, "{context}");
            assert_eq!(
                traced_rejection.receipt,
                traced(read_rejected(0, reason), &[]),
                "{context}"
            );
            // A zero budget changes nothing: the fault precedes every charge.
            let starved = view
                .terms_read(ns, &v, tag, lower, limit, Budget::limit(0))
                .unwrap_err();
            assert_eq!(starved, read_admission(reason), "{context}");
        }
        // The boundaries are admissible: a 4-byte bound, limits 1 and 200.
        // 1 + 4 + {3 + 4} = 12.
        for limit in [1, 200] {
            let got = read(
                view,
                ns,
                &v,
                TypeTag::Str,
                Bound::Included(fits.clone()),
                limit,
            );
            assert_eq!(got.value, strs(&["abcd"]), "{label}: limit {limit}");
            assert_eq!(got.receipt, read_applied(12), "{label}: limit {limit}");
        }
        // Integer bounds are never too large.
        let got = read(
            view,
            ns,
            &v,
            TypeTag::I64,
            Bound::Included(Value::I64(i64::MAX)),
            1,
        );
        assert_eq!(got.value, Vec::<Value>::new(), "{label}");
        assert_eq!(got.receipt, read_applied(5), "{label}");
    }
    // Nothing was recorded on the branch.
    assert!(branch.receipts().is_empty());
    assert_eq!(branch.spent(), Cost::ZERO);
}

#[test]
fn terms_validation_precedes_the_directory_lookup() {
    let (engine, ns) = cost_fixture();
    let price = name("price");
    let unknown = ns_id(99);
    for_each_view(&engine, |view, label| {
        // An invalid request on an unknown namespace is still the fault, at
        // spent 0 and with no directory node visited.
        let result = view.terms_read(
            unknown,
            &price,
            TypeTag::Bytes,
            Bound::Unbounded,
            1,
            Budget::unlimited(),
        );
        assert_eq!(
            result.unwrap_err(),
            read_admission(Rejection::InvalidQuery(QueryFault::NotIndexable)),
            "{label}"
        );
        let result = view.terms_read(
            unknown,
            &price,
            TypeTag::I64,
            Bound::Unbounded,
            0,
            Budget::unlimited(),
        );
        assert_eq!(
            result.unwrap_err(),
            read_admission(Rejection::InvalidQuery(QueryFault::ZeroLimit)),
            "{label}"
        );
        // A valid request on an unknown namespace is charged the lookup.
        let rejected = view
            .terms_read(
                unknown,
                &price,
                TypeTag::I64,
                Bound::Unbounded,
                1,
                Budget::unlimited(),
            )
            .unwrap_err();
        assert_eq!(rejected.reason, Rejection::UnknownNamespace, "{label}");
        assert_eq!(
            rejected.receipt,
            read_rejected(1, Rejection::UnknownNamespace),
            "{label}"
        );
        assert_eq!(rejected.physical.record_nodes, 0, "{label}");
        assert_eq!(rejected.physical.index_nodes, 0, "{label}");
        // With a budget of exactly the lookup the answer is the same; one
        // unit short is OutOfBudget before the lookup.
        let rejected = view
            .terms_read(
                unknown,
                &price,
                TypeTag::I64,
                Bound::Unbounded,
                1,
                Budget::limit(1),
            )
            .unwrap_err();
        assert_eq!(
            rejected.receipt,
            read_rejected(1, Rejection::UnknownNamespace),
            "{label}"
        );
        let rejected = view
            .terms_read(
                unknown,
                &price,
                TypeTag::I64,
                Bound::Unbounded,
                1,
                Budget::limit(0),
            )
            .unwrap_err();
        assert_eq!(
            rejected.receipt,
            read_rejected(0, Rejection::OutOfBudget),
            "{label}"
        );
        // The known namespace still answers.
        assert_eq!(
            read(view, ns, &price, TypeTag::I64, Bound::Unbounded, 200).value,
            i64s(&[5, 10, 20]),
            "{label}"
        );
    });
    // A genesis engine has no namespace at all.
    let genesis = Engine::new(PriceSchedule::FIXTURE);
    let rejected = genesis
        .head_snapshot()
        .terms(
            ns,
            &price,
            TypeTag::I64,
            Bound::Unbounded,
            1,
            Budget::unlimited(),
        )
        .unwrap_err();
    assert_eq!(rejected.reason, Rejection::UnknownNamespace);
    assert_eq!(
        rejected.receipt,
        read_rejected(1, Rejection::UnknownNamespace)
    );
}

// ---------------------------------------------------------------------------
// 3. Hand-derived costs under FIXTURE
// ---------------------------------------------------------------------------

#[test]
fn an_i64_run_charges_the_lookup_the_seek_and_each_returned_term() {
    let (engine, ns) = cost_fixture();
    let price = name("price");
    for_each_view(&engine, |view, label| {
        // Unbounded, limit 200: 1 + 4 + 3 × (3 + 8) = 38.
        let got = read(view, ns, &price, TypeTag::I64, Bound::Unbounded, 200);
        assert_eq!(got.value, i64s(&[5, 10, 20]), "{label}");
        assert_eq!(got.receipt, read_applied(38), "{label}");
        assert_eq!(got.physical.record_nodes, 0, "{label}");
        assert!(got.physical.index_nodes > 0, "{label}: {:?}", got.physical);
        // Exactly the run's length: no peek charge beyond the third term.
        let got = read(view, ns, &price, TypeTag::I64, Bound::Unbounded, 3);
        assert_eq!(got.value, i64s(&[5, 10, 20]), "{label}");
        assert_eq!(got.receipt, read_applied(38), "{label}");
        // limit 2: 1 + 4 + 2 × 11 = 27.
        let got = read(view, ns, &price, TypeTag::I64, Bound::Unbounded, 2);
        assert_eq!(got.value, i64s(&[5, 10]), "{label}");
        assert_eq!(got.receipt, read_applied(27), "{label}");
        // limit 1: 1 + 4 + 11 = 16.
        let got = read(view, ns, &price, TypeTag::I64, Bound::Unbounded, 1);
        assert_eq!(got.value, i64s(&[5]), "{label}");
        assert_eq!(got.receipt, read_applied(16), "{label}");
        // Included at an existing value is charged like any term: 38.
        let got = read(
            view,
            ns,
            &price,
            TypeTag::I64,
            Bound::Included(Value::I64(5)),
            200,
        );
        assert_eq!(got.value, i64s(&[5, 10, 20]), "{label}");
        assert_eq!(got.receipt, read_applied(38), "{label}");
        // Included at the last term: 16; Excluded there: the seek only.
        let got = read(
            view,
            ns,
            &price,
            TypeTag::I64,
            Bound::Included(Value::I64(20)),
            200,
        );
        assert_eq!(got.value, i64s(&[20]), "{label}");
        assert_eq!(got.receipt, read_applied(16), "{label}");
        let got = read(
            view,
            ns,
            &price,
            TypeTag::I64,
            Bound::Excluded(Value::I64(20)),
            200,
        );
        assert_eq!(got.value, Vec::<Value>::new(), "{label}");
        assert_eq!(got.receipt, read_applied(5), "{label}");
        // The extremes: from i64::MIN everything, from i64::MAX nothing.
        let got = read(
            view,
            ns,
            &price,
            TypeTag::I64,
            Bound::Included(Value::I64(i64::MIN)),
            200,
        );
        assert_eq!(got.value, i64s(&[5, 10, 20]), "{label}");
        assert_eq!(got.receipt, read_applied(38), "{label}");
        for lower in [
            Bound::Included(Value::I64(i64::MAX)),
            Bound::Excluded(Value::I64(i64::MAX)),
            Bound::Included(Value::I64(21)),
        ] {
            let got = read(view, ns, &price, TypeTag::I64, lower.clone(), 200);
            assert_eq!(got.value, Vec::<Value>::new(), "{label}: {lower:?}");
            assert_eq!(got.receipt, read_applied(5), "{label}: {lower:?}");
        }
    });
}

#[test]
fn an_excluded_existing_bound_is_free_and_an_included_missing_bound_starts_at_the_next() {
    let (engine, ns) = cost_fixture();
    let price = name("price");
    for_each_view(&engine, |view, label| {
        // Excluded(5): 5 is stepped over without charge: 1 + 4 + 2 × 11 = 27.
        let got = read(
            view,
            ns,
            &price,
            TypeTag::I64,
            Bound::Excluded(Value::I64(5)),
            200,
        );
        assert_eq!(got.value, i64s(&[10, 20]), "{label}");
        assert_eq!(got.receipt, read_applied(27), "{label}");
        // Excluded(5), limit 1: the skipped term does not count: 16.
        let got = read(
            view,
            ns,
            &price,
            TypeTag::I64,
            Bound::Excluded(Value::I64(5)),
            1,
        );
        assert_eq!(got.value, i64s(&[10]), "{label}");
        assert_eq!(got.receipt, read_applied(16), "{label}");
        // Included(7), a value no record holds: starts at 10: 27.
        let got = read(
            view,
            ns,
            &price,
            TypeTag::I64,
            Bound::Included(Value::I64(7)),
            200,
        );
        assert_eq!(got.value, i64s(&[10, 20]), "{label}");
        assert_eq!(got.receipt, read_applied(27), "{label}");
        let got = read(
            view,
            ns,
            &price,
            TypeTag::I64,
            Bound::Included(Value::I64(7)),
            1,
        );
        assert_eq!(got.value, i64s(&[10]), "{label}");
        assert_eq!(got.receipt, read_applied(16), "{label}");
        // Excluded(7) is the same run as Included(7): nothing to step over.
        let got = read(
            view,
            ns,
            &price,
            TypeTag::I64,
            Bound::Excluded(Value::I64(7)),
            200,
        );
        assert_eq!(got.value, i64s(&[10, 20]), "{label}");
        assert_eq!(got.receipt, read_applied(27), "{label}");
        // A negative missing bound starts at the first term: 38.
        let got = read(
            view,
            ns,
            &price,
            TypeTag::I64,
            Bound::Excluded(Value::I64(-1)),
            200,
        );
        assert_eq!(got.value, i64s(&[5, 10, 20]), "{label}");
        assert_eq!(got.receipt, read_applied(38), "{label}");
    });
}

#[test]
fn a_str_run_charges_read_bytes_per_value_length() {
    let (engine, ns) = cost_fixture();
    let kind = name("kind");
    for_each_view(&engine, |view, label| {
        // "", "a", "ab": 1 + 4 + (3 + 0) + (3 + 1) + (3 + 2) = 17.
        let got = read(view, ns, &kind, TypeTag::Str, Bound::Unbounded, 200);
        assert_eq!(got.value, strs(&["", "a", "ab"]), "{label}");
        assert_eq!(got.receipt, read_applied(17), "{label}");
        // limit 1: the empty string costs its visit only: 1 + 4 + 3 = 8.
        let got = read(view, ns, &kind, TypeTag::Str, Bound::Unbounded, 1);
        assert_eq!(got.value, strs(&[""]), "{label}");
        assert_eq!(got.receipt, read_applied(8), "{label}");
        // limit 2: 8 + 4 = 12.
        let got = read(view, ns, &kind, TypeTag::Str, Bound::Unbounded, 2);
        assert_eq!(got.value, strs(&["", "a"]), "{label}");
        assert_eq!(got.receipt, read_applied(12), "{label}");
        // Included(""): the whole run again: 17; Excluded(""): 1 + 4 + 4 + 5 = 14.
        let got = read(
            view,
            ns,
            &kind,
            TypeTag::Str,
            Bound::Included(Value::str("")),
            200,
        );
        assert_eq!(got.value, strs(&["", "a", "ab"]), "{label}");
        assert_eq!(got.receipt, read_applied(17), "{label}");
        let got = read(
            view,
            ns,
            &kind,
            TypeTag::Str,
            Bound::Excluded(Value::str("")),
            200,
        );
        assert_eq!(got.value, strs(&["a", "ab"]), "{label}");
        assert_eq!(got.receipt, read_applied(14), "{label}");
        // Included("a"), limit 1: 1 + 4 + (3 + 1) = 9.
        let got = read(
            view,
            ns,
            &kind,
            TypeTag::Str,
            Bound::Included(Value::str("a")),
            1,
        );
        assert_eq!(got.value, strs(&["a"]), "{label}");
        assert_eq!(got.receipt, read_applied(9), "{label}");
        // Excluded("a") and Included("aa") both start at "ab": 1 + 4 + 5 = 10.
        for lower in [
            Bound::Excluded(Value::str("a")),
            Bound::Included(Value::str("aa")),
        ] {
            let got = read(view, ns, &kind, TypeTag::Str, lower.clone(), 200);
            assert_eq!(got.value, strs(&["ab"]), "{label}: {lower:?}");
            assert_eq!(got.receipt, read_applied(10), "{label}: {lower:?}");
        }
        // Past the run: the seek only.
        for lower in [
            Bound::Excluded(Value::str("ab")),
            Bound::Included(Value::str("b")),
        ] {
            let got = read(view, ns, &kind, TypeTag::Str, lower.clone(), 200);
            assert_eq!(got.value, Vec::<Value>::new(), "{label}: {lower:?}");
            assert_eq!(got.receipt, read_applied(5), "{label}: {lower:?}");
        }
    });
}

#[test]
fn a_missing_name_or_tag_costs_the_seek_only() {
    let (engine, ns) = cost_fixture();
    let cases = [
        ("price", TypeTag::U64),
        ("price", TypeTag::Str),
        ("price", TypeTag::Bool),
        ("kind", TypeTag::I64),
        ("kind", TypeTag::Bool),
        ("nothing", TypeTag::Str),
        ("nothing", TypeTag::I64),
    ];
    for_each_view(&engine, |view, label| {
        for (text, tag) in cases {
            for lower in [
                Bound::Unbounded,
                Bound::Included(match tag {
                    TypeTag::Bool => Value::Bool(false),
                    TypeTag::I64 => Value::I64(0),
                    TypeTag::U64 => Value::U64(0),
                    TypeTag::Str => Value::str(""),
                    TypeTag::Bytes => unreachable!(),
                    _ => unreachable!("legacy fixture"),
                }),
            ] {
                let got = read(view, ns, &name(text), tag, lower.clone(), 200);
                assert_eq!(
                    got.value,
                    Vec::<Value>::new(),
                    "{label}: {text} {tag} {lower:?}"
                );
                assert_eq!(
                    got.receipt,
                    read_applied(5),
                    "{label}: {text} {tag} {lower:?}"
                );
                assert_eq!(got.physical.record_nodes, 0, "{label}: {text} {tag}");
            }
        }
    });
    // A namespace with no records at all: the same 5.
    let (engine, empty) = fixture();
    for_each_view(&engine, |view, label| {
        let got = read(
            view,
            empty,
            &name("price"),
            TypeTag::I64,
            Bound::Unbounded,
            200,
        );
        assert_eq!(got.value, Vec::<Value>::new(), "{label}");
        assert_eq!(got.receipt, read_applied(5), "{label}");
        assert_eq!(got.physical.record_nodes, 0, "{label}");
    });
}

#[test]
fn a_traced_terms_read_lists_classes_in_first_charge_order() {
    let (engine, ns) = cost_fixture();
    let (price, kind) = (name("price"), name("kind"));
    for_each_view(&engine, |view, label| {
        // The I64 run: DirectoryLookup, TermSeek, TermVisit 3, ReadByte 24.
        let got = view
            .terms_read(
                ns,
                &price,
                TypeTag::I64,
                Bound::Unbounded,
                200,
                Budget::unlimited().traced(),
            )
            .unwrap_or_else(|rejected| panic!("{label}: {rejected}"));
        assert_eq!(got.value, i64s(&[5, 10, 20]), "{label}");
        assert_eq!(
            got.receipt,
            traced(
                read_applied(38),
                &[
                    (OperationClass::DirectoryLookup, 1, DIRECTORY_LOOKUP),
                    (OperationClass::TermSeek, 1, TERM_SEEK),
                    (OperationClass::TermVisit, 3, TERM_VISIT),
                    (OperationClass::ReadByte, 24, READ_BYTE),
                ]
            ),
            "{label}"
        );
        // The Str run: ReadByte 0 + 1 + 2 = 3, first charged at the second
        // term, still after TermVisit.
        let got = view
            .terms_read(
                ns,
                &kind,
                TypeTag::Str,
                Bound::Unbounded,
                200,
                Budget::unlimited().traced(),
            )
            .unwrap_or_else(|rejected| panic!("{label}: {rejected}"));
        assert_eq!(got.value, strs(&["", "a", "ab"]), "{label}");
        assert_eq!(
            got.receipt,
            traced(
                read_applied(17),
                &[
                    (OperationClass::DirectoryLookup, 1, DIRECTORY_LOOKUP),
                    (OperationClass::TermSeek, 1, TERM_SEEK),
                    (OperationClass::TermVisit, 3, TERM_VISIT),
                    (OperationClass::ReadByte, 3, READ_BYTE),
                ]
            ),
            "{label}"
        );
        // Only the empty string: no ReadByte line at all.
        let got = view
            .terms_read(
                ns,
                &kind,
                TypeTag::Str,
                Bound::Unbounded,
                1,
                Budget::unlimited().traced(),
            )
            .unwrap_or_else(|rejected| panic!("{label}: {rejected}"));
        assert_eq!(got.value, strs(&[""]), "{label}");
        assert_eq!(
            got.receipt,
            traced(
                read_applied(8),
                &[
                    (OperationClass::DirectoryLookup, 1, DIRECTORY_LOOKUP),
                    (OperationClass::TermSeek, 1, TERM_SEEK),
                    (OperationClass::TermVisit, 1, TERM_VISIT),
                ]
            ),
            "{label}"
        );
        // A missing run: the lookup and the seek.
        let got = view
            .terms_read(
                ns,
                &price,
                TypeTag::U64,
                Bound::Unbounded,
                200,
                Budget::unlimited().traced(),
            )
            .unwrap_or_else(|rejected| panic!("{label}: {rejected}"));
        assert_eq!(got.value, Vec::<Value>::new(), "{label}");
        assert_eq!(
            got.receipt,
            traced(
                read_applied(5),
                &[
                    (OperationClass::DirectoryLookup, 1, DIRECTORY_LOOKUP),
                    (OperationClass::TermSeek, 1, TERM_SEEK),
                ]
            ),
            "{label}"
        );
        // Out of budget inside the first term group: the ledger holds what
        // was charged before it.
        let rejected = view
            .terms_read(
                ns,
                &price,
                TypeTag::I64,
                Bound::Unbounded,
                200,
                Budget::limit(15).traced(),
            )
            .unwrap_err();
        assert_eq!(rejected.reason, Rejection::OutOfBudget, "{label}");
        assert_eq!(
            rejected.receipt,
            traced(
                read_rejected(5, Rejection::OutOfBudget),
                &[
                    (OperationClass::DirectoryLookup, 1, DIRECTORY_LOOKUP),
                    (OperationClass::TermSeek, 1, TERM_SEEK),
                ]
            ),
            "{label}"
        );
    });
}

#[test]
fn every_budget_below_the_total_rejects_at_the_predicted_charge_point_with_no_partial_result() {
    let (engine, ns) = cost_fixture();
    let (price, kind) = (name("price"), name("kind"));
    for_each_view(&engine, |view, label| {
        assert_read_sweep(
            &format!("{label}: price/I64 unbounded 200"),
            &I64_RUN,
            &i64s(&[5, 10, 20]),
            |budget| view.terms_read(ns, &price, TypeTag::I64, Bound::Unbounded, 200, budget),
        );
        assert_read_sweep(
            &format!("{label}: price/I64 unbounded 1"),
            &I64_RUN_LIMIT_1,
            &i64s(&[5]),
            |budget| view.terms_read(ns, &price, TypeTag::I64, Bound::Unbounded, 1, budget),
        );
        assert_read_sweep(
            &format!("{label}: price/I64 excluded 5"),
            &I64_RUN_EXCLUDED_5,
            &i64s(&[10, 20]),
            |budget| {
                view.terms_read(
                    ns,
                    &price,
                    TypeTag::I64,
                    Bound::Excluded(Value::I64(5)),
                    200,
                    budget,
                )
            },
        );
        assert_read_sweep(
            &format!("{label}: kind/Str unbounded 200"),
            &STR_RUN,
            &strs(&["", "a", "ab"]),
            |budget| view.terms_read(ns, &kind, TypeTag::Str, Bound::Unbounded, 200, budget),
        );
        assert_read_sweep(
            &format!("{label}: price/U64 missing"),
            &MISSING_RUN,
            &Vec::<Value>::new(),
            |budget| view.terms_read(ns, &price, TypeTag::U64, Bound::Unbounded, 200, budget),
        );
    });
}

#[test]
fn terms_cost_is_positional() {
    // The cost fixture alone: no term lies before Included(5).
    let (engine, ns) = cost_fixture();
    let price = name("price");
    for_each_view(&engine, |view, label| {
        let got = read(
            view,
            ns,
            &price,
            TypeTag::I64,
            Bound::Included(Value::I64(5)),
            1,
        );
        assert_eq!(got.value, i64s(&[5]), "{label}");
        assert_eq!(got.receipt, read_applied(16), "{label}");
    });
    // Thirty more terms -30..=-1 before the bound: the same read, the same 16.
    let (mut engine, ns) = cost_fixture();
    let mut model = Model::default();
    for (text, cells) in cost_records() {
        model.create(key(text), cells);
    }
    let below: Vec<(String, Cells)> = (1..=30)
        .map(|offset: i64| {
            (
                format!("p{offset:02}"),
                vec![(name("price"), attr(Value::I64(-offset)))],
            )
        })
        .collect();
    let below: Vec<(&str, Cells)> = below
        .iter()
        .map(|(text, cells)| (text.as_str(), cells.clone()))
        .collect();
    assert_eq!(
        populate(&mut engine, ALICE, ns, &below, &mut model).sequence(),
        3
    );
    assert_eq!(engine.head_snapshot().typed_record_count(ns), Some(33));
    for_each_view(&engine, |view, label| {
        let got = read(
            view,
            ns,
            &price,
            TypeTag::I64,
            Bound::Included(Value::I64(5)),
            1,
        );
        assert_eq!(got.value, i64s(&[5]), "{label}");
        assert_eq!(got.receipt, read_applied(16), "{label}");
        // Excluded at the last term below the bound: it is skipped free.
        let got = read(
            view,
            ns,
            &price,
            TypeTag::I64,
            Bound::Excluded(Value::I64(-1)),
            1,
        );
        assert_eq!(got.value, i64s(&[5]), "{label}");
        assert_eq!(got.receipt, read_applied(16), "{label}");
        // Unbounded, limit 1 now starts at -30 for the same 16.
        let got = read(view, ns, &price, TypeTag::I64, Bound::Unbounded, 1);
        assert_eq!(got.value, i64s(&[-30]), "{label}");
        assert_eq!(got.receipt, read_applied(16), "{label}");
        // The whole run: 33 terms: 1 + 4 + 33 × 11 = 368.
        let got = read(view, ns, &price, TypeTag::I64, Bound::Unbounded, 200);
        let mut want: Vec<i64> = (-30..=-1).collect();
        want.extend([5, 10, 20]);
        assert_eq!(got.value, i64s(&want), "{label}");
        assert_eq!(got.receipt, read_applied(368), "{label}");
    });
}

// ---------------------------------------------------------------------------
// 4. Isolation
// ---------------------------------------------------------------------------

#[test]
fn identical_terms_in_another_namespace_never_appear_and_leave_receipts_and_index_nodes_unchanged()
{
    let (mut engine, first) = cost_fixture();
    let (price, kind) = (name("price"), name("kind"));
    let probes: Vec<(&str, CellName, TypeTag, Bound<Value>, usize)> = vec![
        (
            "price all",
            price.clone(),
            TypeTag::I64,
            Bound::Unbounded,
            200,
        ),
        (
            "price after 5",
            price.clone(),
            TypeTag::I64,
            Bound::Excluded(Value::I64(5)),
            1,
        ),
        (
            "price from 12",
            price.clone(),
            TypeTag::I64,
            Bound::Included(Value::I64(12)),
            200,
        ),
        (
            "kind from a",
            kind.clone(),
            TypeTag::Str,
            Bound::Included(Value::str("a")),
            200,
        ),
        (
            "kind all 1",
            kind.clone(),
            TypeTag::Str,
            Bound::Unbounded,
            1,
        ),
        (
            "price U64",
            price.clone(),
            TypeTag::U64,
            Bound::Unbounded,
            200,
        ),
    ];
    let observe = |view: &dyn TermsView, ns: NamespaceId| -> Vec<Metered<Vec<Value>>> {
        probes
            .iter()
            .map(|(_, name, tag, lower, limit)| read(view, ns, name, *tag, lower.clone(), *limit))
            .collect()
    };
    let baseline = observe(&engine.head_snapshot(), first);
    let expected: [(Vec<Value>, u64); 6] = [
        (i64s(&[5, 10, 20]), 38),
        (i64s(&[10]), 16),
        (i64s(&[20]), 16),
        (strs(&["a", "ab"]), 14),
        (strs(&[""]), 8),
        (Vec::new(), 5),
    ];
    for ((probe, got), (want, spent)) in probes.iter().zip(&baseline).zip(&expected) {
        assert_eq!(got.value, *want, "{}", probe.0);
        assert_eq!(got.receipt, read_applied(*spent), "{}", probe.0);
    }

    // BOB's namespace 2 holds the same records under the same keys, plus a
    // value between 10 and 20 and a string between "a" and "ab".
    let mut branch = fresh_branch(&engine);
    let created = apply(
        &mut branch,
        BOB,
        WriteOp::CreateNamespace {
            name: arkiv_db::NamespaceName::new("test").unwrap(),
        },
    );
    let second = ns_id(2);
    assert_eq!(created.effect, Effect::NamespaceCreated(second));
    assert_eq!(publish(&mut engine, branch).sequence(), 3);
    let mut model = Model::default();
    let mut records = cost_records();
    records.push((
        "d",
        vec![
            (name("price"), attr(Value::I64(15))),
            (name("kind"), attr(Value::str("aa"))),
        ],
    ));
    assert_eq!(
        populate(&mut engine, BOB, second, &records, &mut model).sequence(),
        4
    );

    for_each_view(&engine, |view, label| {
        let after = observe(view, first);
        for ((probe, before), now) in probes.iter().zip(&baseline).zip(&after) {
            assert_eq!(now.value, before.value, "{label}: {}", probe.0);
            assert_eq!(now.receipt, before.receipt, "{label}: {}", probe.0);
            assert_eq!(
                now.physical.index_nodes, before.physical.index_nodes,
                "{label}: {}",
                probe.0
            );
            assert_eq!(now.physical.record_nodes, 0, "{label}: {}", probe.0);
        }
        // The second namespace sees its own extra terms, at its own cost.
        let got = read(view, second, &price, TypeTag::I64, Bound::Unbounded, 200);
        assert_eq!(got.value, i64s(&[5, 10, 15, 20]), "{label}");
        // 1 + 4 + 4 × 11 = 49.
        assert_eq!(got.receipt, read_applied(49), "{label}");
        let got = read(
            view,
            second,
            &kind,
            TypeTag::Str,
            Bound::Included(Value::str("a")),
            200,
        );
        assert_eq!(got.value, strs(&["a", "aa", "ab"]), "{label}");
        // 1 + 4 + (3 + 1) + (3 + 2) + (3 + 2) = 19.
        assert_eq!(got.receipt, read_applied(19), "{label}");
        // And never the first namespace's answer for the same probe.
        let got = read(
            view,
            second,
            &price,
            TypeTag::I64,
            Bound::Included(Value::I64(12)),
            200,
        );
        assert_eq!(got.value, i64s(&[15, 20]), "{label}");
        assert_eq!(got.receipt, read_applied(27), "{label}");
    });
    assert_eq!(engine.head_snapshot().term_count(first), Some(8));
    assert_eq!(engine.head_snapshot().term_count(second), Some(10));
}

// ---------------------------------------------------------------------------
// 5. The expiry sweep
// ---------------------------------------------------------------------------

#[test]
fn expire_records_deletes_attribute_rows_height_by_height_in_height_then_id_order() {
    let (engine, ns, mut model) = expiry_fixture();
    // The literals agree with the model-3 delete formula 30 + 4c + 20a for
    // the shapes r1 (2, 2), r2 (1, 1), r3 (3, 2), r4 (3, 3), r5 (2, 1),
    // r6 (1, 1).
    let delete_cost = |cells: u64, attributes: u64| {
        DIRECTORY_LOOKUP
            + POINT_READ
            + RECORD_DELETE
            + CELL_REMOVE * cells
            + ROOT_UPDATE * 3
            + (INDEX_REMOVE + ROOT_UPDATE * 2) * 2 * attributes
    };
    assert_eq!(
        [(2, 2), (1, 1), (3, 2), (3, 3), (2, 1), (1, 1)].map(|(c, a)| delete_cost(c, a)),
        [78, 54, 82, 102, 58, 54]
    );
    let mut branch = fresh_branch(&engine);
    let deleted = sweep(&mut branch, ns, 7, Budget::unlimited(), Budget::unlimited())
        .unwrap_or_else(|halt| panic!("an unlimited sweep completes: {halt}"));
    assert_eq!(deleted, expected_deletes(&EXPIRY_DELETES));

    // Only the six deletes entered the block: the terms and query reads
    // never do.
    let receipts: Vec<Receipt> = EXPIRY_DELETES
        .iter()
        .map(|&(_, cost)| applied(cost))
        .collect();
    assert_eq!(branch.receipts(), receipts.as_slice());
    assert_eq!(branch.spent(), Cost::new(EXPIRY_TOTAL));
    assert_eq!(
        EXPIRY_DELETES.iter().map(|&(_, cost)| cost).sum::<u64>(),
        EXPIRY_TOTAL
    );

    // The field-kind and the absent row survive, unchanged.
    for text in ["r1", "r2", "r3", "r4", "r5", "r6"] {
        model.delete(&key(text));
    }
    assert_branch_matches_model(&branch, ns, &model, "after the sweep");
    assert_eq!(branch.typed_record_count(ns), Some(2));
    let expires = expiry_name();
    let got = read(&branch, ns, &expires, TypeTag::U64, Bound::Unbounded, 200);
    assert_eq!(got.value, Vec::<Value>::new());
    assert_eq!(got.receipt, read_applied(5));
    let got = read(
        &branch,
        ns,
        &name("kind"),
        TypeTag::Str,
        Bound::Unbounded,
        200,
    );
    assert_eq!(got.value, strs(&["f", "n"]));

    // A second sweep of the same block finds nothing and records nothing.
    let again = sweep(&mut branch, ns, 7, Budget::unlimited(), Budget::unlimited())
        .unwrap_or_else(|halt| panic!("an unlimited sweep completes: {halt}"));
    assert_eq!(again, Vec::new());
    assert_eq!(branch.receipts().len(), 6);
    assert_eq!(branch.spent(), Cost::new(EXPIRY_TOTAL));

    // The base snapshot still holds all eight.
    assert_eq!(engine.head_snapshot().typed_record_count(ns), Some(8));
}

#[test]
fn expire_records_stops_at_the_first_height_above_the_block() {
    let (engine, ns, mut model) = expiry_fixture();
    // Block 4: the two height-3 rows only.
    let mut branch = fresh_branch(&engine);
    let deleted = sweep(&mut branch, ns, 4, Budget::unlimited(), Budget::unlimited())
        .unwrap_or_else(|halt| panic!("an unlimited sweep completes: {halt}"));
    assert_eq!(deleted, expected_deletes(&EXPIRY_DELETES[..2]));
    assert_eq!(branch.receipts(), [applied(54), applied(58)].as_slice());
    assert_eq!(branch.spent(), Cost::new(112));
    for text in ["r2", "r5"] {
        model.delete(&key(text));
    }
    assert_branch_matches_model(&branch, ns, &model, "after block 4");
    let got = read(
        &branch,
        ns,
        &expiry_name(),
        TypeTag::U64,
        Bound::Unbounded,
        200,
    );
    assert_eq!(got.value, u64s(&[5, 7]));
    // Block 5 on the same branch: the height-5 row; block 6: nothing.
    let deleted = sweep(&mut branch, ns, 5, Budget::unlimited(), Budget::unlimited())
        .unwrap_or_else(|halt| panic!("an unlimited sweep completes: {halt}"));
    assert_eq!(deleted, expected_deletes(&EXPIRY_DELETES[2..3]));
    let deleted = sweep(&mut branch, ns, 6, Budget::unlimited(), Budget::unlimited())
        .unwrap_or_else(|halt| panic!("an unlimited sweep completes: {halt}"));
    assert_eq!(deleted, Vec::new());
    assert_eq!(branch.receipts().len(), 3);
    assert_eq!(branch.spent(), Cost::new(112 + 82));

    // Block 2 on a fresh branch: nothing expires, nothing is recorded.
    let mut branch = fresh_branch(&engine);
    let deleted = sweep(&mut branch, ns, 2, Budget::unlimited(), Budget::unlimited())
        .unwrap_or_else(|halt| panic!("an unlimited sweep completes: {halt}"));
    assert_eq!(deleted, Vec::new());
    assert!(branch.receipts().is_empty());
    assert_eq!(branch.spent(), Cost::ZERO);
    assert_eq!(branch.typed_record_count(ns), Some(8));
}

#[test]
fn a_starved_delete_budget_halts_before_any_delete() {
    let (engine, ns, model) = expiry_fixture();
    // Budget 0: the delete stops at its directory lookup, spent 0.
    let mut branch = fresh_branch(&engine);
    let halt = sweep(&mut branch, ns, 7, Budget::unlimited(), Budget::limit(0)).unwrap_err();
    assert_eq!(halt.applied, Vec::new());
    assert_eq!(halt.rejected.reason, Rejection::OutOfBudget);
    assert_eq!(halt.rejected.receipt, rejected(0, Rejection::OutOfBudget));
    // The rejected delete is a write: it is recorded, at zero.
    assert_eq!(
        branch.receipts(),
        [rejected(0, Rejection::OutOfBudget)].as_slice()
    );
    assert_eq!(branch.spent(), Cost::ZERO);
    assert_branch_matches_model(&branch, ns, &model, "starved at 0");

    // Budget 53, one below the first delete (r2, 54): it charges the lookup,
    // the point read, the delete group and the value term's removal (44)
    // and stops at the existence term's removal; nothing is deleted.
    let group = DIRECTORY_LOOKUP + POINT_READ + RECORD_DELETE + CELL_REMOVE + 3 * ROOT_UPDATE;
    let index_remove = INDEX_REMOVE + 2 * ROOT_UPDATE;
    assert_eq!(
        DELETE_R2,
        [
            DIRECTORY_LOOKUP,
            DIRECTORY_LOOKUP + POINT_READ,
            group,
            group + index_remove,
            group + 2 * index_remove,
        ]
    );
    let mut branch = fresh_branch(&engine);
    let halt = sweep(&mut branch, ns, 7, Budget::unlimited(), Budget::limit(53)).unwrap_err();
    assert_eq!(halt.applied, Vec::new());
    assert_eq!(halt.rejected.reason, Rejection::OutOfBudget);
    assert_eq!(halt.rejected.receipt, rejected(44, Rejection::OutOfBudget));
    assert_eq!(
        branch.receipts(),
        [rejected(44, Rejection::OutOfBudget)].as_slice()
    );
    assert_eq!(branch.spent(), Cost::new(44));
    assert_branch_matches_model(&branch, ns, &model, "starved at 53");
    // Every budget below 54 halts with the largest charge point that fits
    // and leaves the namespace untouched.
    for budget in 0..54 {
        let mut branch = fresh_branch(&engine);
        let halt = sweep(
            &mut branch,
            ns,
            7,
            Budget::unlimited(),
            Budget::limit(budget),
        )
        .unwrap_err();
        let spent = largest_prefix_sum_at_most(&DELETE_R2, budget);
        assert_eq!(halt.applied, Vec::new(), "budget {budget}");
        assert_eq!(
            halt.rejected.receipt,
            rejected(spent, Rejection::OutOfBudget),
            "budget {budget}"
        );
        assert_eq!(branch.spent(), Cost::new(spent), "budget {budget}");
        assert_eq!(branch.typed_record_count(ns), Some(8), "budget {budget}");
        assert_eq!(branch.index_consistent(ns), Some(true), "budget {budget}");
    }
}

#[test]
fn a_delete_budget_admitting_the_first_row_but_not_the_second_halts_with_one_applied_delete() {
    let (engine, ns, mut model) = expiry_fixture();
    // Budget 54 pays r2 (54) exactly; r5 (58) stops at its last index
    // removal with 48 spent: 1, 4, {20 + 8 + 6}, {6 + 4}, then 10 more would
    // make 58.
    let group = DIRECTORY_LOOKUP + POINT_READ + RECORD_DELETE + 2 * CELL_REMOVE + 3 * ROOT_UPDATE;
    let index_remove = INDEX_REMOVE + 2 * ROOT_UPDATE;
    assert_eq!(
        DELETE_R5,
        [
            DIRECTORY_LOOKUP,
            DIRECTORY_LOOKUP + POINT_READ,
            group,
            group + index_remove,
            group + 2 * index_remove,
        ]
    );
    let mut branch = fresh_branch(&engine);
    let halt = sweep(&mut branch, ns, 7, Budget::unlimited(), Budget::limit(54)).unwrap_err();
    assert_eq!(halt.applied, expected_deletes(&EXPIRY_DELETES[..1]));
    assert_eq!(halt.rejected.reason, Rejection::OutOfBudget);
    assert_eq!(halt.rejected.receipt, rejected(48, Rejection::OutOfBudget));
    assert_eq!(
        branch.receipts(),
        [applied(54), rejected(48, Rejection::OutOfBudget)].as_slice()
    );
    assert_eq!(branch.spent(), Cost::new(102));

    // r2 stays deleted; r5 and everything else is still live.
    model.delete(&key("r2"));
    assert_branch_matches_model(&branch, ns, &model, "after the halt");
    assert_eq!(branch.typed_record_count(ns), Some(7));
    let got = read(
        &branch,
        ns,
        &expiry_name(),
        TypeTag::U64,
        Bound::Unbounded,
        200,
    );
    assert_eq!(got.value, u64s(&[3, 5, 7]));

    // Catching up with an unlimited budget resumes at height 3 and deletes
    // the remaining five in (height, id) order.
    let deleted = sweep(&mut branch, ns, 7, Budget::unlimited(), Budget::unlimited())
        .unwrap_or_else(|halt| panic!("an unlimited sweep completes: {halt}"));
    assert_eq!(deleted, expected_deletes(&EXPIRY_DELETES[1..]));
    assert_eq!(branch.receipts().len(), 7);
    assert_eq!(branch.spent(), Cost::new(102 + EXPIRY_TOTAL - 54));
    for text in ["r1", "r3", "r4", "r5", "r6"] {
        model.delete(&key(text));
    }
    assert_branch_matches_model(&branch, ns, &model, "after catching up");
}

#[test]
fn a_query_budget_one_below_the_terms_read_halts_before_any_delete() {
    let (engine, ns, model) = expiry_fixture();
    // The opening terms read costs 1 + 4 + {3 + 8} = 16; at 15 it stops at
    // the term group with 5 spent.
    assert_eq!(EXPIRY_TERMS_READ, 16);
    let mut branch = fresh_branch(&engine);
    let halt = sweep(&mut branch, ns, 7, Budget::limit(15), Budget::unlimited()).unwrap_err();
    assert_eq!(halt.applied, Vec::new());
    assert_eq!(halt.rejected.reason, Rejection::OutOfBudget);
    assert_eq!(
        halt.rejected.receipt,
        read_rejected(5, Rejection::OutOfBudget)
    );
    assert_eq!(halt.rejected.physical.record_nodes, 0);
    // A read rejection is never recorded: the block is untouched.
    assert!(branch.receipts().is_empty());
    assert_eq!(branch.spent(), Cost::ZERO);
    assert_branch_matches_model(&branch, ns, &model, "after the halted terms read");

    // The same budget is enough once nothing is due: the terms read of an
    // empty run costs 5 and the sweep completes.
    let mut branch = fresh_branch(&engine);
    for text in ["r1", "r2", "r3", "r4", "r5", "r6"] {
        apply(&mut branch, ALICE, delete_record(ns, key(text)));
    }
    let deleted = sweep(&mut branch, ns, 7, Budget::limit(5), Budget::unlimited())
        .unwrap_or_else(|halt| panic!("an empty sweep completes: {halt}"));
    assert_eq!(deleted, Vec::new());
    // And one unit short of that is a halt before anything else.
    let halt = sweep(&mut branch, ns, 7, Budget::limit(4), Budget::unlimited()).unwrap_err();
    assert_eq!(halt.applied, Vec::new());
    assert_eq!(
        halt.rejected.receipt,
        read_rejected(1, Rejection::OutOfBudget)
    );
    assert_eq!(branch.receipts().len(), 6);
}
