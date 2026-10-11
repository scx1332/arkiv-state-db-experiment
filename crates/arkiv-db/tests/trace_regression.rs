//! Physical traces of queries and counts that stop part way (node step N0a).
//!
//! A read's `PhysicalTrace` is not canonical, but the isolation tests and the
//! planned disk-adapter measurements read it, so a rejected read must report
//! the nodes it actually visited. Before N0a, a rejection while a query was
//! opening a later group or pulling a head dropped every source opened so
//! far, and a `Range`/`StartsWith` run stopped at a `TermVisit` dropped the
//! postings it had positioned. A rejection during an advance or a row
//! emission was already reported in full.
//!
//! What this file establishes, on one small published namespace:
//! - Exact traces at every charge point. Each read below runs under every
//!   budget from 0 to its success total: a three-group page (an `Eq` posting
//!   with a probe, a three-term `StartsWith` run, a negated row-map group)
//!   and the `count` of the same query; a three-group page whose later
//!   groups fail to open after the earlier ones looked up their terms; a
//!   `Range` and a `StartsWith` run that fail at each `TermVisit`; a
//!   `NotEq` page whose probes reject candidates and a match-all page (head
//!   pulls and advances failing at `PostingEntry`, `RecordProbe` and
//!   `RowVisit`); an `Eq` page failing at its row emissions. Each rejection
//!   reports exactly the directory, record and index nodes visited before
//!   the charge that failed. The success reports the whole traversal.
//! - Monotonicity: for budgets b1 < b2 that both fail, every trace component
//!   at b1 is at most the one at b2, and every failing trace is at most the
//!   success trace.
//! - No double count: every success trace is a literal written as the sum
//!   of independently derived parts (a lookup path, a whole posting, a term
//!   run, the row map, row loads), each counted once. A run's postings are
//!   counted once whether the run completes or stops at a `TermVisit`.
//! - Receipts are the charge table's: `spent` is the units charged before
//!   the failing charge with `OutOfBudget`, or the total with `Applied`,
//!   hand-derived from the plan's charge points under
//!   `PriceSchedule::FIXTURE` (DirectoryLookup 1, QueryGroup 5, TermSeek 4,
//!   TermVisit 3, PostingEntry 1, RowVisit 1, RecordProbe 2, ResultRow 3,
//!   ReadByte 1). Each table checks itself: every charge starts where the
//!   previous one ended and the last ends at the total. They are the same
//!   receipts as before the engine moved to tries (node step N3b).
//!
//! Every expected count is a literal derived by hand from the Merkle
//! Patricia trie shapes documented at the fixture (node step N3b re-derived
//! them from the AVL-era literals of N0a), never by running the engine
//! first. Expected rows come from evaluating the fixture by hand.
//!
//! What it does not establish: charge rules beyond the receipts repeated
//! here (`query_metering.rs` owns them), query semantics (`queries.rs`), the
//! traces of writes, and anything about a durable backend. The counts
//! describe the trie layout of state format 2 and change with it.

use std::{fmt::Debug, ops::Bound};

use arkiv_db::{
    Budget, Cell, CellName, Cost, Effect, Engine, Literal, Metered, NamespaceId, NamespaceName,
    Outcome, OwnerId, PhysicalTrace, PriceSchedule, Projection, Query, QueryPage, ReadResult,
    Receipt, ReceiptKind, RecordId, RecordKey, Rejection, Snapshot, Term, TypeTag, Value, WriteOp,
};

const ALICE: OwnerId = OwnerId::new([0xa1; 20]);

/// Counting-rule version recorded in every receipt.
const MODEL_VERSION: u32 = 3;
/// The id `PriceSchedule::FIXTURE` carries.
const FIXTURE_ID: u32 = 1;

/// Every page in this file fits in one page of this size.
const LIMIT: usize = 10;

// ---------------------------------------------------------------------------
// The fixture and its tree shapes
// ---------------------------------------------------------------------------

/// The records, created in this order in one block: ids 1 to 4, each with
/// the attributes `c: Str` and `n: I64`.
const RECORDS: [(&str, &str, i64); 4] = [
    ("a", "red", 1),
    ("b", "rise", 2),
    ("c", "red", 3),
    ("d", "ruby", 4),
];

/// Snapshot 1: namespace 1 (the only namespace) holding [`RECORDS`].
///
/// The shapes every count below is derived from. Every map is a Merkle
/// Patricia trie over fixed-length keys (see `docs/state-trie.md`): a branch
/// has at least two children and consumes one nibble, an extension holds
/// the nibbles all keys below it share and precedes a branch, and a leaf
/// holds the rest of its key. The shape depends only on the key set. A node
/// visit counts once per step that examines it, inline children included.
///
/// Directory: one namespace, so the catalog is a single leaf. Every read
/// that gets past the `DirectoryLookup` charge visits it:
/// `directory_nodes = 1`.
///
/// Row map, keys are record ids as 8 big-endian bytes (16 nibbles): ids 1
/// to 4 share their first 15 nibbles and differ in the last, so the trie is
/// an extension over 15 zero nibbles, a branch with slots 1 to 4 and four
/// leaves (6 nodes). A row load is a lookup of the extension, the branch and
/// the leaf: 3 nodes for every id.
///
/// Term map, keys (state layout 2) are `name ‖ 00 ‖ tag ‖ presence` for an
/// existence term and `name ‖ 00 ‖ tag ‖ 01 ‖ value` for a value, a `Str`
/// value as its bytes plus one and a `00` terminator. T1 = c:Str:∅, T2 =
/// c:"red", T3 = c:"rise", T4 = c:"ruby", T5 = n:I64:∅, T6 to T9 = n:1 to
/// n:4. `c` is 0x63 and `n` 0x6e: every key starts with nibble 6, then 3 or
/// e. Within a name, the terminator, the tag and the presence byte's high
/// nibble are shared (5 nibbles); the presence byte's low nibble is 0 for
/// the existence term and 1 for values. The shifted `c` values `s f e`,
/// `s j t f`, `s v c z` (0x73 0x66 0x65, 0x73 0x6a 0x74 0x66, 0x73 0x76 0x63
/// 0x7a, each then 00) share 0x73 (nibbles 7, 3), then split on 6 (`f`,
/// `j`) or 7 (`v`), and `red`/`rise` split on 6 or a. The I64 term bytes of
/// 1 to 4 are 0x80 00 00 00 00 00 00 0k: 15 shared nibbles, then k. (The
/// fixture uses `rise` rather than layout 1's `rose`: shifted, `o` is 0x70
/// and would move `rose` under `ruby`'s branch; `rise` keeps the shape the
/// counts below were derived for.)
///
/// ```text
/// R  extension [6]
/// └ B0 branch
///   ├ 3: Ec extension (5 nibbles)
///   │    └ Bc branch
///   │      ├ 0: T1 c:∅ leaf
///   │      └ 1: Ecv extension [7, 3]
///   │           └ Bcv branch
///   │             ├ 6: Bcv6 branch
///   │             │    ├ 6: T2 c:red leaf
///   │             │    └ a: T3 c:rise leaf
///   │             └ 7: T4 c:ruby leaf
///   └ e: En extension (5 nibbles)
///        └ Bn branch
///          ├ 0: T5 n:∅ leaf
///          └ 1: Env extension [8, 0 × 14]
///               └ Bnv branch
///                 ├ 1: T6 n:1 leaf   ├ 2: T7 n:2 leaf
///                 ├ 3: T8 n:3 leaf   └ 4: T9 n:4 leaf
/// ```
///
/// Postings (sets of ids, keyed like the rows): c:red = {1, 3} is an
/// extension, a branch and two leaves (4 nodes); c:rise = {2}, c:ruby = {4}
/// and n:1 to n:4 are single leaves; c:∅ = n:∅ = {1, 2, 3, 4} have the row
/// map's shape (6 nodes).
///
/// Traversal counts. A term lookup (`Eq`, `Exists`, `NotEq`) visits its
/// path: c:red visits R, B0, Ec, Bc, Ecv, Bcv, Bcv6 and T2 (8), c:∅ R, B0,
/// Ec, Bc and T1 (5), n:∅ R, B0, En, Bn and T5 (5). An iterator does
/// nothing until its first step; that step seeks along its lower bound,
/// visiting each node on the way, and every later step visits the nodes
/// between one leaf and the next, each once. An iterator run to its end has
/// visited every node at or above its lower bound once. A step happens
/// before the charge of the entry it reveals, so a charge that fails leaves
/// that step's nodes visited. From id 1: a four-id map's first step visits
/// 3 nodes (extension, branch, leaf 1) and each later step 1; c:red's first
/// step 3 and its second 1; a one-id posting's only step 1.
///
/// Where the engine notes them: the directory lookup, term lookups and row
/// loads are noted as they happen. A term run's own traversal is noted when
/// the group finishes opening or stops; a run steps each posting of the run
/// once while opening (its seek and first entry, to prime the merge). A
/// single posting and the row map are stepped first at the group's first
/// head pull. Source traversals (postings, the row map) are noted once when
/// the read stops, whether it succeeded or was rejected.
fn fixture() -> (Snapshot, NamespaceId) {
    let mut engine = Engine::new(PriceSchedule::FIXTURE);
    let mut branch = engine
        .begin(engine.head(), None)
        .expect("genesis is the head");
    let created = branch
        .execute(
            ALICE,
            WriteOp::CreateNamespace {
                name: NamespaceName::new("shop").expect("a valid name"),
            },
            Budget::unlimited(),
        )
        .expect("an unlimited creation applies");
    let Effect::NamespaceCreated(ns) = created.effect else {
        panic!("a namespace creation creates a namespace");
    };
    for (ordinal, (key, colour, number)) in (1..).zip(RECORDS) {
        let created = branch
            .execute(
                ALICE,
                WriteOp::CreateRecord {
                    namespace: ns,
                    key: RecordKey::new(key.as_bytes()).expect("a valid key"),
                    cells: vec![
                        (name("c"), Cell::attribute(Value::str(colour))),
                        (name("n"), Cell::attribute(Value::I64(number))),
                    ],
                },
                Budget::unlimited(),
            )
            .expect("fixture creates apply");
        assert_eq!(created.effect, Effect::RecordCreated { id: id(ordinal) });
    }
    engine
        .publish(&branch.seal())
        .expect("the fixture publishes on genesis");
    let snapshot = engine.head_snapshot();
    // The sizes the shapes above assume.
    assert_eq!(snapshot.namespace_count(), 1);
    assert_eq!(snapshot.typed_record_count(ns), Some(4));
    assert_eq!(snapshot.term_count(ns), Some(9));
    (snapshot, ns)
}

fn name(text: &str) -> CellName {
    CellName::new(text).expect("fixture names follow the grammar")
}

fn id(ordinal: u64) -> RecordId {
    RecordId::from_ordinal(ordinal).expect("ordinals start at 1")
}

fn eq_c(value: &str) -> Literal {
    Literal::of(Term::eq(name("c"), Value::str(value)))
}

// ---------------------------------------------------------------------------
// Charge-point tables
// ---------------------------------------------------------------------------

/// `PhysicalTrace` as `(directory, record, index)` node counts.
const fn nodes(directory_nodes: u64, record_nodes: u64, index_nodes: u64) -> PhysicalTrace {
    PhysicalTrace {
        directory_nodes,
        record_nodes,
        index_nodes,
    }
}

/// One charge point of a read, in charge order.
struct Point {
    /// The charge, for failure messages.
    charge: &'static str,
    /// Its FIXTURE price (the total of an atomic group).
    cost: u64,
    /// Units charged before it: the receipt's `spent` when it rejects.
    spent: u64,
    /// The trace of a rejection at this charge: every node visited before
    /// the charge, once.
    trace: PhysicalTrace,
}

const fn point(charge: &'static str, cost: u64, spent: u64, trace: PhysicalTrace) -> Point {
    Point {
        charge,
        cost,
        spent,
        trace,
    }
}

/// How a read ends when every charge fits.
struct Success<T> {
    total: u64,
    trace: PhysicalTrace,
    value: T,
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

/// Whether every component of `smaller` is at most that of `larger`.
fn within(smaller: PhysicalTrace, larger: PhysicalTrace) -> bool {
    smaller.directory_nodes <= larger.directory_nodes
        && smaller.record_nodes <= larger.record_nodes
        && smaller.index_nodes <= larger.index_nodes
}

/// Runs `read` under every budget from 0 to the success total, then
/// unlimited. A budget below the total must reject at the first charge that
/// does not fit (spent = the units before it, `OutOfBudget`) with that
/// point's exact trace. The total and an unlimited budget must succeed with
/// the success trace and value. Then checks monotonicity over the failing
/// budgets.
fn assert_every_budget<T: Debug + PartialEq>(
    what: &str,
    points: &[Point],
    success: &Success<T>,
    read: impl Fn(Budget) -> ReadResult<T>,
) {
    let mut end = 0;
    for point in points {
        assert_eq!(
            point.spent, end,
            "{what}: the table's {} starts where the previous charge ended",
            point.charge
        );
        end += point.cost;
    }
    assert_eq!(end, success.total, "{what}: the table ends at the total");

    let mut failing: Vec<(u64, PhysicalTrace)> = Vec::new();
    for budget in 0..=success.total {
        let result = read(Budget::limit(budget));
        // The abort-point rule: every earlier charge fit, this one does not.
        match points
            .iter()
            .find(|point| budget < point.spent + point.cost)
        {
            Some(point) => {
                let rejected = result.expect_err("a budget below the total rejects");
                assert_eq!(
                    rejected.reason,
                    Rejection::OutOfBudget,
                    "{what}: budget {budget}"
                );
                assert_eq!(
                    rejected.receipt,
                    read_receipt(point.spent, Outcome::Rejected(Rejection::OutOfBudget)),
                    "{what}: budget {budget} rejects at {}",
                    point.charge
                );
                assert_eq!(
                    rejected.physical, point.trace,
                    "{what}: budget {budget} rejects at {}",
                    point.charge
                );
                failing.push((budget, rejected.physical));
            }
            None => check_success(what, success, result),
        }
    }
    check_success(what, success, read(Budget::unlimited()));

    for (index, &(small_budget, small)) in failing.iter().enumerate() {
        assert!(
            within(small, success.trace),
            "{what}: budget {small_budget}'s {small:?} exceeds the success {:?}",
            success.trace
        );
        for &(large_budget, large) in &failing[index + 1..] {
            assert!(
                within(small, large),
                "{what}: budget {small_budget}'s {small:?} exceeds budget \
                 {large_budget}'s {large:?}"
            );
        }
    }
}

fn check_success<T: Debug + PartialEq>(what: &str, success: &Success<T>, result: ReadResult<T>) {
    let metered = result.unwrap_or_else(|rejected| panic!("{what}: rejected {rejected:?}"));
    assert_eq!(
        metered.receipt,
        read_receipt(success.total, Outcome::Applied),
        "{what}: success receipt"
    );
    assert_eq!(metered.physical, success.trace, "{what}: success trace");
    assert_eq!(metered.value, success.value, "{what}: success value");
}

/// A page as `(id, projected key)` rows and whether a continuation exists.
type Page = (Vec<(u64, Option<Vec<u8>>)>, bool);

fn page(result: ReadResult<QueryPage>) -> ReadResult<Page> {
    result.map(|metered| Metered {
        value: (
            metered
                .value
                .rows
                .iter()
                .map(|row| {
                    (
                        row.id.ordinal(),
                        row.key.as_ref().map(|key| key.as_bytes().to_vec()),
                    )
                })
                .collect(),
            metered.value.next.is_some(),
        ),
        receipt: metered.receipt,
        physical: metered.physical,
    })
}

/// One page of `query` from the start, with at most [`LIMIT`] rows.
fn first_page(
    snapshot: &Snapshot,
    ns: NamespaceId,
    query: &Query,
    projection: &Projection,
) -> impl Fn(Budget) -> ReadResult<Page> {
    move |budget| page(snapshot.query(ns, query, projection, LIMIT, None, budget))
}

/// Rows of the given ids, with keys when the projection has them.
fn rows(ids: &[u64], keys: bool) -> Page {
    let rows = ids
        .iter()
        .map(|&ordinal| {
            let key = RECORDS[usize::try_from(ordinal - 1).expect("small")].0;
            (ordinal, keys.then(|| key.as_bytes().to_vec()))
        })
        .collect();
    (rows, false)
}

// ---------------------------------------------------------------------------
// A three-group page and count, every budget
// ---------------------------------------------------------------------------

/// `[Eq(c, "red"), Range(n, >= 2)] OR [StartsWith(c, "r")] OR
/// [NOT Eq(c, "red")]`. G1's source is c:red's posting with one probe; G2's
/// is the run c:red, c:rise, c:ruby; G3's is the row map, probed by its own
/// negated literal. G1 matches {3}, G2 {1, 2, 3, 4}, G3 {2, 4}.
fn three_groups() -> Query {
    Query {
        groups: vec![
            vec![
                eq_c("red"),
                Literal::of(Term::range(
                    name("n"),
                    Bound::Included(Value::I64(2)),
                    Bound::Unbounded,
                )),
            ],
            vec![Literal::of(Term::starts_with(name("c"), "r"))],
            vec![Literal::not(Term::eq(name("c"), Value::str("red")))],
        ],
    }
}

/// The charges of [`three_groups`] up to its primed heads, shared by the
/// page and the count. Index counts are cumulative: G1's lookup, then G2's
/// run and postings, then the head steps.
fn three_groups_opening_and_heads() -> Vec<Point> {
    vec![
        // Nothing is resolved before the lookup charge.
        point("DirectoryLookup", 1, 0, nodes(0, 0, 0)),
        point("G1 QueryGroup", 5, 1, nodes(1, 0, 0)),
        point("G1 TermSeek", 4, 6, nodes(1, 0, 0)),
        // G1 is open: the lookup of c:red (8). Its posting is not stepped
        // until G1's first head pull.
        point("G2 QueryGroup", 5, 10, nodes(1, 0, 8)),
        point("G2 TermSeek", 4, 15, nodes(1, 0, 8)),
        // The run's seek for c:"r" descends R, B0, Ec, Bc, Ecv and Bcv,
        // whose slot 0 (the terminator after "r") is empty (6); the step that
        // reveals c:red visits Bcv6 and T2 (run 8).
        point("G2 TermVisit c:red", 3, 19, nodes(1, 0, 8 + 8)),
        // c:red's posting is positioned (its first step: 3) and the step
        // revealing c:rise visits T3 (run 9). The positioned posting was
        // lost before N0a.
        point("G2 TermVisit c:rise", 3, 22, nodes(1, 0, 8 + 9 + 3)),
        // c:rise's posting is positioned (1); revealing c:ruby pops Bcv6
        // and visits T4 (run 10).
        point("G2 TermVisit c:ruby", 3, 25, nodes(1, 0, 8 + 10 + 3 + 1)),
        // c:ruby's posting is positioned (1); the step to T5, which ends
        // the run, visits En, Bn and T5 (run 13). G2 is open with 13 + 5
        // nodes; its positioned postings were lost before N0a when a later
        // group failed to open.
        point("G3 QueryGroup", 5, 28, nodes(1, 0, 8 + 13 + 5)),
        // G3 is open with no seek. G1's first pull steps its posting (3).
        point("G1 PostingEntry id 1", 1, 33, nodes(1, 0, 26 + 3)),
        point("G1 RecordProbe id 1", 2, 34, nodes(1, 0, 29)),
        // The probe loaded row 1 (3) and rejected it (n = 1); the next step
        // visits id 3's leaf (G1's posting: 4).
        point("G1 PostingEntry id 3", 1, 36, nodes(1, 3, 30)),
        point("G1 RecordProbe id 3", 2, 37, nodes(1, 3, 30)),
        // Row 3 loaded (3) and accepted: G1's head is 3. G2 pops id 1 from
        // its merge, charged before c:red's posting steps to id 3.
        point("G2 PostingEntry id 1", 1, 39, nodes(1, 6, 30)),
        // c:red's posting stepped to id 3 (1). G3's first pull steps the
        // row map from id 1 (3).
        point("G3 RowVisit id 1", 1, 40, nodes(1, 6 + 3, 31)),
        point("G3 RecordProbe id 1", 2, 41, nodes(1, 9, 31)),
        // The row map hands row 1 to the probe (no load); the next step
        // visits row 2's leaf.
        point("G3 RowVisit id 2", 1, 43, nodes(1, 10, 31)),
        point("G3 RecordProbe id 2", 2, 44, nodes(1, 10, 31)),
    ]
}

#[test]
fn every_budget_of_a_three_group_page_notes_each_visited_node_once() {
    let (snapshot, ns) = fixture();
    let query = three_groups();
    let mut points = three_groups_opening_and_heads();
    points.extend([
        // Heads 3, 1, 2. The head of id 1 (G2) holds no row: the page loads
        // row 1 for its emission (3) before charging it. KEY_ONLY projects
        // the 1-byte key: {ResultRow 1, ReadByte 1} = 4.
        point("emit id 1", 4, 46, nodes(1, 13, 31)),
        point("G2 PostingEntry id 2", 1, 50, nodes(1, 13, 31)),
        // G3 holds row 2. c:rise's posting had no further step to visit.
        point("emit id 2", 4, 51, nodes(1, 13, 31)),
        point("G2 PostingEntry id 3", 1, 55, nodes(1, 13, 31)),
        // The row map steps to row 3, then row 4: all 6 nodes visited.
        point("G3 RowVisit id 3", 1, 56, nodes(1, 14, 31)),
        point("G3 RecordProbe id 3", 2, 57, nodes(1, 14, 31)),
        point("G3 RowVisit id 4", 1, 59, nodes(1, 15, 31)),
        point("G3 RecordProbe id 4", 2, 60, nodes(1, 15, 31)),
        // G1 holds row 3; advancing G1 finds its posting exhausted (no charge).
        point("emit id 3", 4, 62, nodes(1, 15, 31)),
        point("G2 PostingEntry id 4", 1, 66, nodes(1, 15, 31)),
        // G3 holds row 4; advancing G2 and G3 finds both exhausted.
        point("emit id 4", 4, 67, nodes(1, 15, 31)),
    ]);
    let success = Success {
        // 1 + 5 × 3 groups + 4 × 2 seeks + 3 × 3 terms + 10 candidates
        // (PostingEntry 2 + 4, RowVisit 4) + 2 × 6 probes + 4 × 4 rows.
        total: 1 + 15 + 8 + 9 + 10 + 12 + 16,
        trace: nodes(
            1,
            // The whole row map (6) + rows 1 and 3 loaded by G1's probes
            // (3 + 3) + row 1 loaded for emission (3).
            6 + 3 + 3 + 3,
            // G1: lookup (8) + the whole c:red posting (4); G2: the run (13)
            // + the whole c:red, c:rise and c:ruby postings (4 + 1 + 1).
            (8 + 4) + 13 + (4 + 1 + 1),
        ),
        value: rows(&[1, 2, 3, 4], true),
    };
    assert_eq!(success.total, 71);
    assert_eq!(success.trace, nodes(1, 15, 31));
    assert_every_budget(
        "three-group page",
        &points,
        &success,
        first_page(&snapshot, ns, &query, &Projection::KEY_ONLY),
    );
}

#[test]
fn every_budget_of_a_three_group_count_notes_each_visited_node_once() {
    let (snapshot, ns) = fixture();
    let query = three_groups();
    // The same enumeration as the page, with no emission and so no row load
    // for it.
    let mut points = three_groups_opening_and_heads();
    points.extend([
        point("G2 PostingEntry id 2", 1, 46, nodes(1, 10, 31)),
        point("G2 PostingEntry id 3", 1, 47, nodes(1, 10, 31)),
        // The row map steps to row 3, then row 4 (row map 6).
        point("G3 RowVisit id 3", 1, 48, nodes(1, 11, 31)),
        point("G3 RecordProbe id 3", 2, 49, nodes(1, 11, 31)),
        point("G3 RowVisit id 4", 1, 51, nodes(1, 12, 31)),
        point("G3 RecordProbe id 4", 2, 52, nodes(1, 12, 31)),
        point("G2 PostingEntry id 4", 1, 54, nodes(1, 12, 31)),
    ]);
    let success = Success {
        // The page's total without its four emissions: 71 − 16.
        total: 1 + 15 + 8 + 9 + 10 + 12,
        // The page's parts minus the emission load of row 1.
        trace: nodes(1, 6 + 3 + 3, (8 + 4) + 13 + (4 + 1 + 1)),
        value: 4,
    };
    assert_eq!(success.total, 55);
    assert_eq!(success.trace, nodes(1, 12, 31));
    assert_every_budget("three-group count", &points, &success, |budget| {
        snapshot.count(ns, &query, budget)
    });
}

// ---------------------------------------------------------------------------
// Later groups failing to open
// ---------------------------------------------------------------------------

#[test]
fn a_later_group_failing_to_open_keeps_the_earlier_groups_visits() {
    let (snapshot, ns) = fixture();
    // [Exists(c, Str)] OR [NotEq(n, 1)] OR [StartsWith(c, "ri")]: G1 walks
    // c:∅'s posting, G2 n:∅'s posting probed by its own literal, and G3 the
    // one-term run c:rise. G1 matches {1, 2, 3, 4}, G2 {2, 3, 4}, G3 {2}.
    // G1 and G2 look their terms up while opening; their postings are
    // stepped at their first head pulls.
    let query = Query {
        groups: vec![
            vec![Literal::of(Term::exists(name("c"), TypeTag::Str))],
            vec![Literal::of(Term::not_eq(name("n"), Value::I64(1)))],
            vec![Literal::of(Term::starts_with(name("c"), "ri"))],
        ],
    };
    let points = [
        point("DirectoryLookup", 1, 0, nodes(0, 0, 0)),
        point("G1 QueryGroup", 5, 1, nodes(1, 0, 0)),
        point("G1 TermSeek", 4, 6, nodes(1, 0, 0)),
        // G1 is open: the lookup of c:∅ (R, B0, Ec, Bc, T1: 5).
        point("G2 QueryGroup", 5, 10, nodes(1, 0, 5)),
        point("G2 TermSeek", 4, 15, nodes(1, 0, 5)),
        // G2 is open: the lookup of n:∅ (R, B0, En, Bn, T5: 5).
        point("G3 QueryGroup", 5, 19, nodes(1, 0, 5 + 5)),
        point("G3 TermSeek", 4, 24, nodes(1, 0, 10)),
        // The run's seek for c:"ri" descends R, B0, Ec, Bc, Ecv, Bcv (slot
        // 6), Bcv6 (slot a) and T3, whose rest is above the terminator after
        // "ri", so T3 is the first entry (8).
        point("G3 TermVisit c:rise", 3, 28, nodes(1, 0, 10 + 8)),
        // c:rise's posting is positioned (1). The step to c:ruby (T4),
        // which ends the run, pops Bcv6 and visits T4 (run 9). G1's first
        // pull steps c:∅'s posting (3).
        point("G1 PostingEntry id 1", 1, 31, nodes(1, 0, 10 + 9 + 1 + 3)),
        // G2's first pull steps n:∅'s posting (3).
        point("G2 PostingEntry id 1", 1, 32, nodes(1, 0, 23 + 3)),
        point("G2 RecordProbe id 1", 2, 33, nodes(1, 0, 26)),
        // Row 1 loaded (3) and rejected (n = 1); G2 steps to id 2 (1).
        point("G2 PostingEntry id 2", 1, 35, nodes(1, 3, 27)),
        point("G2 RecordProbe id 2", 2, 36, nodes(1, 3, 27)),
        // Row 2 loaded (3) and accepted.
        point("G3 PostingEntry id 2", 1, 38, nodes(1, 6, 27)),
        // Heads 1, 2, 2. G1's head holds no row: row 1 is loaded for
        // emission (3). ID_ONLY: {ResultRow 1, ReadByte 0} = 3.
        point("emit id 1", 3, 39, nodes(1, 9, 27)),
        // G1 steps to id 2 (1).
        point("G1 PostingEntry id 2", 1, 42, nodes(1, 9, 28)),
        // G2 holds row 2.
        point("emit id 2", 3, 43, nodes(1, 9, 28)),
        // G1 steps to id 3 (1).
        point("G1 PostingEntry id 3", 1, 46, nodes(1, 9, 29)),
        // G2 likewise (1). G3's merge is then empty.
        point("G2 PostingEntry id 3", 1, 47, nodes(1, 9, 30)),
        point("G2 RecordProbe id 3", 2, 48, nodes(1, 9, 30)),
        // Row 3 loaded by the probe (3); the emission reuses it.
        point("emit id 3", 3, 50, nodes(1, 12, 30)),
        // G1 and G2 step to id 4: both postings fully visited (6 each).
        point("G1 PostingEntry id 4", 1, 53, nodes(1, 12, 31)),
        point("G2 PostingEntry id 4", 1, 54, nodes(1, 12, 32)),
        point("G2 RecordProbe id 4", 2, 55, nodes(1, 12, 32)),
        // Row 4 loaded by the probe (3); the emission reuses it.
        point("emit id 4", 3, 57, nodes(1, 15, 32)),
    ];
    let success = Success {
        // 1 + 5 × 3 + 4 × 3 seeks + 3 × 1 term + 9 candidates (4 + 4 + 1)
        // + 2 × 4 probes + 3 × 4 rows.
        total: 1 + 15 + 12 + 3 + 9 + 8 + 12,
        trace: nodes(
            1,
            // Rows 1, 2, 3, 4 loaded by G2's probes (4 × 3) and row 1
            // loaded for emission (3).
            4 * 3 + 3,
            // G1: lookup (5) + whole c:∅ posting (6); G2: lookup (5) + whole
            // n:∅ posting (6); G3: run (9) + c:rise's posting (1).
            (5 + 6) + (5 + 6) + (9 + 1),
        ),
        value: rows(&[1, 2, 3, 4], false),
    };
    assert_eq!(success.total, 60);
    assert_eq!(success.trace, nodes(1, 15, 32));
    assert_every_budget(
        "later groups failing to open",
        &points,
        &success,
        first_page(&snapshot, ns, &query, &Projection::ID_ONLY),
    );
}

// ---------------------------------------------------------------------------
// Term runs failing at the k-th TermVisit
// ---------------------------------------------------------------------------

#[test]
fn a_prefix_run_failing_at_each_term_visit_keeps_its_positioned_postings() {
    let (snapshot, ns) = fixture();
    let query = Query::term(Term::starts_with(name("c"), "r"));
    let points = [
        point("DirectoryLookup", 1, 0, nodes(0, 0, 0)),
        point("QueryGroup", 5, 1, nodes(1, 0, 0)),
        point("TermSeek", 4, 6, nodes(1, 0, 0)),
        // Seek for c:"r": R, B0, Ec, Bc, Ecv, Bcv (6); revealing c:red
        // visits Bcv6 and T2 (run 8).
        point("TermVisit c:red", 3, 10, nodes(1, 0, 8)),
        // c:red positioned (3); revealing c:rise visits T3 (run 9). Before
        // N0a only the run's nodes were reported.
        point("TermVisit c:rise", 3, 13, nodes(1, 0, 9 + 3)),
        // c:rise positioned (1); revealing c:ruby visits T4 (run 10).
        point("TermVisit c:ruby", 3, 16, nodes(1, 0, 10 + 3 + 1)),
        // c:ruby positioned (1); the step to T5 ending the run visits En,
        // Bn and T5 (run 13). Before N0a a failing first head pull
        // reported the run's nodes only.
        point("PostingEntry id 1", 1, 19, nodes(1, 0, 13 + 3 + 1 + 1)),
        // c:red steps to id 3 (1) after the charge. Every head holds no
        // row: each emission loads its row (3) first.
        point("emit id 1", 3, 20, nodes(1, 3, 19)),
        point("PostingEntry id 2", 1, 23, nodes(1, 3, 19)),
        point("emit id 2", 3, 24, nodes(1, 6, 19)),
        point("PostingEntry id 3", 1, 27, nodes(1, 6, 19)),
        point("emit id 3", 3, 28, nodes(1, 9, 19)),
        point("PostingEntry id 4", 1, 31, nodes(1, 9, 19)),
        point("emit id 4", 3, 32, nodes(1, 12, 19)),
    ];
    let success = Success {
        // 1 + 5 + 4 + 3 × 3 terms + 4 candidates + 3 × 4 rows.
        total: 1 + 5 + 4 + 9 + 4 + 12,
        // Rows 1 to 4 loaded for emission; the run (13) and the whole
        // c:red, c:rise and c:ruby postings.
        trace: nodes(1, 4 * 3, 13 + (4 + 1 + 1)),
        value: rows(&[1, 2, 3, 4], false),
    };
    assert_eq!(success.total, 35);
    assert_eq!(success.trace, nodes(1, 12, 19));
    assert_every_budget(
        "prefix run",
        &points,
        &success,
        first_page(&snapshot, ns, &query, &Projection::ID_ONLY),
    );
}

#[test]
fn a_range_run_failing_at_each_term_visit_keeps_its_positioned_postings() {
    let (snapshot, ns) = fixture();
    let query = Query::term(Term::range(
        name("n"),
        Bound::Included(Value::I64(1)),
        Bound::Excluded(Value::I64(4)),
    ));
    let points = [
        point("DirectoryLookup", 1, 0, nodes(0, 0, 0)),
        point("QueryGroup", 5, 1, nodes(1, 0, 0)),
        point("TermSeek", 4, 6, nodes(1, 0, 0)),
        // Seek for n:1: R, B0 (slot e), En, Bn (slot 1), Env, Bnv (slot 1)
        // and T6, whose rest equals the bound's (7); n:1 is the first entry.
        point("TermVisit n:1", 3, 10, nodes(1, 0, 7)),
        // n:1 positioned (1); revealing n:2 visits T7 (run 8).
        point("TermVisit n:2", 3, 13, nodes(1, 0, 8 + 1)),
        // n:2 positioned (1); revealing n:3 visits T8 (run 9).
        point("TermVisit n:3", 3, 16, nodes(1, 0, 9 + 1 + 1)),
        // n:3 positioned (1); n:4 (T9, run 10) is excluded and ends the
        // run. Before N0a a failing first head pull reported the run only.
        point("PostingEntry id 1", 1, 19, nodes(1, 0, 10 + 3)),
        point("emit id 1", 3, 20, nodes(1, 3, 13)),
        point("PostingEntry id 2", 1, 23, nodes(1, 3, 13)),
        point("emit id 2", 3, 24, nodes(1, 6, 13)),
        point("PostingEntry id 3", 1, 27, nodes(1, 6, 13)),
        point("emit id 3", 3, 28, nodes(1, 9, 13)),
    ];
    let success = Success {
        // 1 + 5 + 4 + 3 × 3 terms + 3 candidates + 3 × 3 rows.
        total: 1 + 5 + 4 + 9 + 3 + 9,
        // Rows 1 to 3 loaded for emission; the run (10) and three one-node
        // postings.
        trace: nodes(1, 3 * 3, 10 + 3),
        value: rows(&[1, 2, 3], false),
    };
    assert_eq!(success.total, 31);
    assert_eq!(success.trace, nodes(1, 9, 13));
    assert_every_budget(
        "range run",
        &points,
        &success,
        first_page(&snapshot, ns, &query, &Projection::ID_ONLY),
    );
}

// ---------------------------------------------------------------------------
// Head pulls and row emission
// ---------------------------------------------------------------------------

#[test]
fn pulls_failing_at_posting_entries_probes_and_row_visits_keep_the_source() {
    let (snapshot, ns) = fixture();

    // NotEq(c, "red"): the source is c:∅'s posting and every candidate is
    // probed. Candidates 1 and 3 (red) are rejected, 2 and 4 accepted.
    let query = Query::term(Term::not_eq(name("c"), Value::str("red")));
    let points = [
        point("DirectoryLookup", 1, 0, nodes(0, 0, 0)),
        point("QueryGroup", 5, 1, nodes(1, 0, 0)),
        point("TermSeek", 4, 6, nodes(1, 0, 0)),
        // The lookup of c:∅ (5) and the posting's first step (3). Before
        // N0a only the lookup was reported.
        point("PostingEntry id 1", 1, 10, nodes(1, 0, 5 + 3)),
        point("RecordProbe id 1", 2, 11, nodes(1, 0, 8)),
        // Row 1 loaded (3) and rejected; the posting steps to id 2 (1).
        point("PostingEntry id 2", 1, 13, nodes(1, 3, 9)),
        point("RecordProbe id 2", 2, 14, nodes(1, 3, 9)),
        // Row 2 loaded (3) and accepted; the emission reuses it.
        point("emit id 2", 3, 16, nodes(1, 6, 9)),
        // Advancing: the posting steps to id 3 (1).
        point("PostingEntry id 3", 1, 19, nodes(1, 6, 10)),
        point("RecordProbe id 3", 2, 20, nodes(1, 6, 10)),
        // Row 3 loaded (3) and rejected; the posting steps to id 4 (the
        // whole posting: 6).
        point("PostingEntry id 4", 1, 22, nodes(1, 9, 11)),
        point("RecordProbe id 4", 2, 23, nodes(1, 9, 11)),
        // Row 4 loaded (3) and accepted; the emission reuses it.
        point("emit id 4", 3, 25, nodes(1, 12, 11)),
    ];
    let success = Success {
        // 1 + 5 + 4 + 4 candidates + 2 × 4 probes + 3 × 2 rows.
        total: 1 + 5 + 4 + 4 + 8 + 6,
        // Rows 1 to 4 loaded by the probes; the lookup (5) and the whole
        // posting (6).
        trace: nodes(1, 4 * 3, 5 + 6),
        value: rows(&[2, 4], false),
    };
    assert_eq!(success.total, 28);
    assert_eq!(success.trace, nodes(1, 12, 11));
    assert_every_budget(
        "NotEq page",
        &points,
        &success,
        first_page(&snapshot, ns, &query, &Projection::ID_ONLY),
    );

    // Match-all: the source is the row map, which hands its rows over, so
    // nothing is loaded and nothing is probed.
    let query = Query::match_all();
    let points = [
        point("DirectoryLookup", 1, 0, nodes(0, 0, 0)),
        point("QueryGroup", 5, 1, nodes(1, 0, 0)),
        // The row map's first step (3). Before N0a a match-all read failing
        // here reported no record node at all.
        point("RowVisit id 1", 1, 6, nodes(1, 3, 0)),
        point("emit id 1", 3, 7, nodes(1, 3, 0)),
        // Each later step visits one leaf.
        point("RowVisit id 2", 1, 10, nodes(1, 4, 0)),
        point("emit id 2", 3, 11, nodes(1, 4, 0)),
        point("RowVisit id 3", 1, 14, nodes(1, 5, 0)),
        point("emit id 3", 3, 15, nodes(1, 5, 0)),
        point("RowVisit id 4", 1, 18, nodes(1, 6, 0)),
        point("emit id 4", 3, 19, nodes(1, 6, 0)),
    ];
    let success = Success {
        // 1 + 5 + 4 row visits + 3 × 4 rows.
        total: 1 + 5 + 4 + 12,
        // The whole row map, once.
        trace: nodes(1, 6, 0),
        value: rows(&[1, 2, 3, 4], false),
    };
    assert_eq!(success.total, 22);
    assert_every_budget(
        "match-all page",
        &points,
        &success,
        first_page(&snapshot, ns, &query, &Projection::ID_ONLY),
    );
}

#[test]
fn row_emission_failures_keep_every_source_and_row_load() {
    let (snapshot, ns) = fixture();
    // Eq(c, "red") with KEY_ONLY: each emission loads its row (the head of
    // a posting holds none) and then charges {ResultRow 1, ReadByte 1}.
    let query = Query::all(vec![eq_c("red")]);
    let points = [
        point("DirectoryLookup", 1, 0, nodes(0, 0, 0)),
        point("QueryGroup", 5, 1, nodes(1, 0, 0)),
        point("TermSeek", 4, 6, nodes(1, 0, 0)),
        // The lookup of c:red (8) and the posting's first step to id 1
        // (3). Before N0a only the lookup was reported.
        point("PostingEntry id 1", 1, 10, nodes(1, 0, 8 + 3)),
        // Row 1 loaded for emission (3), before the emission charge.
        point("emit id 1", 4, 11, nodes(1, 3, 11)),
        // The posting steps to id 3 (1).
        point("PostingEntry id 3", 1, 15, nodes(1, 3, 12)),
        // Row 3 loaded for emission (3).
        point("emit id 3", 4, 16, nodes(1, 3 + 3, 12)),
    ];
    let success = Success {
        // 1 + 5 + 4 + 2 candidates + 4 × 2 rows.
        total: 1 + 5 + 4 + 2 + 8,
        // Rows 1 and 3 loaded once each; the lookup (8) and the whole c:red
        // posting (4).
        trace: nodes(1, 3 + 3, 8 + 4),
        value: rows(&[1, 3], true),
    };
    assert_eq!(success.total, 20);
    assert_eq!(success.trace, nodes(1, 6, 12));
    assert_every_budget(
        "Eq page emission",
        &points,
        &success,
        first_page(&snapshot, ns, &query, &Projection::KEY_ONLY),
    );
}
