//! Planned evaluation (`Budget::planned`) against the written evaluation and
//! an independent full-scan model, on a skewed namespace where planning
//! decisions differ from the written order.
//!
//! The fixture is one namespace of about 3,000 records shaped like the
//! public Sourcify data the planner was built for: a scope attribute every
//! record holds (a tautology), an owner almost every record holds, a few
//! broad categories, rare flags, clustered flags (ids in a few ranges, so a
//! leapfrog can skip), unique addresses, names with shared prefixes, a
//! timestamp with one distinct value per record, and a name whose type
//! differs between records (and that some records hold as a field). Some
//! records are deleted, patched or recreated, so ids have gaps and postings
//! changed after they were built.
//!
//! What this file establishes:
//! - On a deterministic set of random queries (1 to 4 groups of 1 to 8
//!   literals over every operator and both polarities) the planned pages,
//!   their `next` claims and their counts are exactly the written ones and
//!   the model's, page after page under varying limits; the same holds with
//!   every group's literals reversed, and when pages alternate between the
//!   evaluations on one cursor chain.
//! - A group's planned charges do not depend on the order of its literals
//!   when no two sources tie, and the benchmark shape (broad scope first, a
//!   unique address last) costs a planned read what the selective order
//!   does, a small fraction of the written broad-first read.
//! - Count fast paths: one positive posting, match-all, a tautology plus a
//!   posting, a disjoint union of values, `NotEq`, `NOT Exists` and a prefix
//!   run are answered from stored sizes, at the hand-derived charges below.
//! - A planned read still stops at its budget with `OutOfBudget`.
//! - A planned id-only page (an offset walk) returns and charges exactly what
//!   a page that loads each row to project none of its cells does.
//! - Read-side query limits admit groups, literals and page sizes beyond the
//!   instance limits, for planned and written reads, and a wider page is the
//!   concatenation of the narrower ones.
//!
//! FIXTURE weights used by the hand-derived charges: DirectoryLookup 1,
//! QueryGroup 5, TermSeek 4, TermVisit 3, PostingEntry 1, RowVisit 1,
//! RecordProbe 2, ResultRow 3, ReadByte 1.

use std::ops::Bound;

use arkiv_db::{
    Branch, Budget, Cursor, Effect, Engine, Literal, NamespaceId, PriceSchedule, Projection, Query,
    QueryFault, QueryLimits, Rejection, Snapshot, Term, TypeTag, Value,
};

use crate::support::{
    model::Model,
    random::Generator64,
    vocab::{
        ALICE, Cells, add_namespace, apply, attr, begin, create_record, delete_record, field, name,
        names, patch_record, publish,
    },
};

fn record_key(ordinal: u64) -> String {
    format!("k{ordinal}")
}

fn s(text: &str) -> Value {
    Value::str(text)
}

/// The ordinals of the model's matches, ascending.
fn matches(model: &Model, query: &Query) -> Vec<u64> {
    model.ids(query).iter().map(|id| id.ordinal()).collect()
}

const KINDS: [&str; 4] = ["contract", "code", "blob", "x"];
const EVMS: [&str; 6] = [
    "paris", "london", "shanghai", "berlin", "cancun", "istanbul",
];
const NAME_STEMS: [&str; 8] = [
    "Safe", "SafeMath", "SafeERC", "Token", "Tok", "S", "Proxy", "",
];

fn record_cells(ordinal: u64, generator: &mut Generator64) -> Cells {
    let mut cells = vec![(name("scope"), attr(s("all")))];
    // Almost every record has the owner: dense, but not a tautology.
    if ordinal % 97 != 3 {
        cells.push((name("owner"), attr(s("o1"))));
    }
    let kind = match generator.below(100) {
        0..50 => KINDS[0],
        50..80 => KINDS[1],
        80..95 => KINDS[2],
        _ => KINDS[3],
    };
    cells.push((name("kind"), attr(s(kind))));
    if generator.below(10) != 0 {
        let lang = if generator.below(10) < 6 { "sol" } else { "vy" };
        cells.push((name("lang"), attr(s(lang))));
    }
    cells.push((name("opt"), attr(Value::Bool(generator.below(100) < 47))));
    let evm = if generator.below(50) == 0 {
        EVMS[0]
    } else {
        generator.pick(&EVMS[1..])
    };
    cells.push((name("evm"), attr(s(evm))));
    if generator.below(100) == 0 {
        cells.push((name("viair"), attr(Value::Bool(true))));
    }
    // Unique addresses: every posting holds one record.
    cells.push((
        name("addr"),
        attr(Value::U64(ordinal.wrapping_mul(7_919) % 1_000_003)),
    ));
    let stem = generator.pick(&NAME_STEMS);
    let suffix = generator.below(40);
    cells.push((name("name"), attr(s(&format!("{stem}{suffix}")))));
    // One distinct timestamp per record: long runs of one-record terms.
    cells.push((name("at"), attr(Value::U64(ordinal * 10 + 3))));
    // Clustered flags: two id ranges each, overlapping in part.
    if (500..700).contains(&ordinal) || (2_000..2_100).contains(&ordinal) {
        cells.push((name("clus"), attr(Value::Bool(true))));
    }
    if (650..900).contains(&ordinal) || (2_050..2_400).contains(&ordinal) {
        cells.push((name("clus2"), attr(Value::Bool(true))));
    }
    // One name of several types, held by some records as a field.
    let mixed = match generator.below(6) {
        0 => Some(attr(s(&format!("m{}", generator.below(5))))),
        1 => Some(attr(Value::U64(generator.next() % 8))),
        2 => Some(attr(Value::I64(
            i64::try_from(generator.below(8)).unwrap() - 4,
        ))),
        3 => Some(attr(Value::Bool(generator.below(2) == 0))),
        4 => Some(field(s("m1"))),
        _ => None,
    };
    if let Some(cell) = mixed {
        cells.push((name("mixed"), cell));
    }
    cells
}

const RECORDS: u64 = 3_000;

struct Fixture {
    engine: Engine,
    ns: NamespaceId,
    model: Model,
}

fn fixture() -> Fixture {
    let mut engine = Engine::new(PriceSchedule::FIXTURE);
    let mut branch = begin(&engine, None);
    let ns = add_namespace(&mut branch, ALICE, "planner");
    let mut generator = Generator64(0x9e37_79b9_7f4a_7c15);
    let mut model = Model::new();
    let create = |branch: &mut Branch, model: &mut Model, ordinal: u64, cells: Cells| {
        let id = model.create(record_key(ordinal), &cells);
        let op = create_record(ns, record_key(ordinal), cells);
        assert_eq!(
            apply(branch, ALICE, op).effect,
            Effect::RecordCreated { id }
        );
    };
    for ordinal in 1..=RECORDS {
        let cells = record_cells(ordinal, &mut generator);
        create(&mut branch, &mut model, ordinal, cells);
    }
    // Deletes leave gaps, patches move records between postings, and a
    // recreated key gets a fresh id at the end.
    for ordinal in (1..=RECORDS).filter(|ordinal| ordinal % 23 == 5) {
        apply(&mut branch, ALICE, delete_record(ns, record_key(ordinal)));
        model.delete(record_key(ordinal));
    }
    for ordinal in (1..=RECORDS).filter(|ordinal| ordinal % 31 == 7 && ordinal % 23 != 5) {
        let set = vec![
            (name("kind"), attr(s("code"))),
            (name("viair"), attr(Value::Bool(true))),
        ];
        let remove = names(&["lang"]);
        model.patch(record_key(ordinal), &set, &remove);
        let op = patch_record(ns, record_key(ordinal), set, remove);
        apply(&mut branch, ALICE, op);
    }
    for ordinal in (1..=RECORDS).filter(|ordinal| ordinal % 230 == 5) {
        let cells = record_cells(ordinal, &mut generator);
        create(&mut branch, &mut model, ordinal, cells);
    }
    assert_eq!(branch.index_consistent(ns), Some(true));
    publish(&mut engine, branch);
    Fixture { engine, ns, model }
}

fn eq(text: &str, value: Value) -> Literal {
    Literal::of(Term::eq(name(text), value))
}

fn random_term(generator: &mut Generator64) -> Term {
    let str_names = [
        "scope", "owner", "kind", "lang", "evm", "name", "mixed", "none",
    ];
    match generator.below(17) {
        0 => Term::eq(name("scope"), s("all")),
        1 => Term::eq(name("owner"), s("o1")),
        2 => Term::eq(name("kind"), s(generator.pick(&KINDS))),
        3 => Term::eq(name("lang"), s(generator.pick(&["sol", "vy", "go"]))),
        4 => Term::eq(name("opt"), Value::Bool(generator.below(2) == 0)),
        5 => Term::eq(name("evm"), s(generator.pick(&EVMS))),
        6 => Term::eq(
            name(generator.pick(&["viair", "clus", "clus2"])),
            Value::Bool(true),
        ),
        7 => Term::eq(
            name("addr"),
            Value::U64((generator.next() % (RECORDS + 50)).wrapping_mul(7_919) % 1_000_003),
        ),
        8 => Term::eq(
            name("name"),
            s(&format!(
                "{}{}",
                generator.pick(&NAME_STEMS),
                generator.below(45)
            )),
        ),
        9 => Term::eq(
            name("mixed"),
            match generator.below(4) {
                0 => s(&format!("m{}", generator.below(6))),
                1 => Value::U64(generator.next() % 9),
                2 => Value::I64(i64::try_from(generator.below(9)).unwrap() - 4),
                _ => Value::Bool(generator.below(2) == 0),
            },
        ),
        10 => Term::not_eq(
            name(generator.pick(&["kind", "evm", "lang"])),
            s(generator.pick(&["contract", "code", "paris", "sol", "zz"])),
        ),
        11 => Term::not_eq(name("mixed"), Value::U64(generator.next() % 9)),
        12 => Term::exists(
            name(generator.pick(&str_names)),
            generator.pick(&[TypeTag::Str, TypeTag::Str, TypeTag::U64, TypeTag::Bool]),
        ),
        13 => Term::exists(
            name(generator.pick(&["viair", "clus", "opt", "mixed"])),
            TypeTag::Bool,
        ),
        14 => Term::starts_with(
            name(generator.pick(&["name", "name", "kind", "mixed"])),
            generator.pick(&["Safe", "SafeM", "S", "Tok", "", "Proxy1", "zz", "c"]),
        ),
        15 => {
            let low = generator.next() % (RECORDS * 10 + 100);
            let width = generator.pick(&[0, 5, 40, 400, 4_000, 40_000]);
            let lower = match generator.below(3) {
                0 => Bound::Unbounded,
                1 => Bound::Included(Value::U64(low)),
                _ => Bound::Excluded(Value::U64(low)),
            };
            let upper = match (generator.below(3), &lower) {
                (0, Bound::Included(_) | Bound::Excluded(_)) => Bound::Unbounded,
                (1, _) => Bound::Included(Value::U64(low + width)),
                _ => Bound::Excluded(Value::U64(low + width)),
            };
            Term::range(name(generator.pick(&["at", "at", "addr"])), lower, upper)
        }
        _ => {
            let low = i64::try_from(generator.below(9)).unwrap() - 4;
            Term::range(
                name("mixed"),
                Bound::Included(Value::I64(low)),
                Bound::Excluded(Value::I64(low + 3)),
            )
        }
    }
}

fn random_literal(generator: &mut Generator64) -> Literal {
    let term = random_term(generator);
    if generator.below(5) == 0 {
        Literal::not(term)
    } else {
        Literal::of(term)
    }
}

fn random_queries(count: usize) -> Vec<Query> {
    let mut generator = Generator64(0x2545_f491_4f6c_dd1d);
    (0..count)
        .map(|_| {
            let groups = 1 + generator.below(4);
            Query {
                groups: (0..groups)
                    .map(|_| {
                        let literals = 1 + generator.below(8);
                        (0..literals)
                            .map(|_| random_literal(&mut generator))
                            .collect()
                    })
                    .collect(),
            }
        })
        .collect()
}

fn reversed(query: &Query) -> Query {
    Query {
        groups: query
            .groups
            .iter()
            .map(|group| group.iter().rev().cloned().collect())
            .collect(),
    }
}

/// Every page of a walk whose page limits and budgets cycle through
/// `limits` and `budgets`: its row ids and the id its `next` continues after.
fn cycled_walk(
    snapshot: &Snapshot,
    ns: NamespaceId,
    query: &Query,
    limits: &[usize],
    budgets: &[Budget],
) -> Vec<(Vec<u64>, Option<u64>)> {
    let mut pages = Vec::new();
    let mut cursor: Option<Cursor> = None;
    for index in 0.. {
        assert!(index < 10_000, "runaway walk of {query:?}");
        let limit = limits[index % limits.len()];
        let budget = budgets[index % budgets.len()];
        let page = snapshot
            .query(ns, query, &Projection::ALL, limit, cursor.as_ref(), budget)
            .unwrap_or_else(|rejected| panic!("{query:?}: {rejected}"))
            .value;
        let ids: Vec<u64> = page.rows.iter().map(|row| row.id.ordinal()).collect();
        let next = page.next.as_ref().map(|next| next.after().ordinal());
        if next.is_some() {
            assert_eq!(ids.len(), limit, "{query:?}: next on a short page");
            assert_eq!(next, ids.last().copied());
        }
        pages.push((ids, next));
        match page.next {
            Some(next) => cursor = Some(next),
            None => return pages,
        }
    }
    unreachable!()
}

fn count_and_spent(
    snapshot: &Snapshot,
    ns: NamespaceId,
    query: &Query,
    budget: Budget,
) -> (u64, u64) {
    let metered = snapshot
        .count(ns, query, budget)
        .unwrap_or_else(|rejected| panic!("count {query:?}: {rejected}"));
    (metered.value, metered.receipt.spent.units())
}

fn flat(pages: &[(Vec<u64>, Option<u64>)]) -> Vec<u64> {
    pages
        .iter()
        .flat_map(|(ids, _)| ids.iter().copied())
        .collect()
}

const WRITTEN: Budget = Budget::unlimited();
const PLANNED: Budget = Budget::unlimited().planned();

#[test]
fn planned_pages_and_counts_equal_the_written_ones_and_the_model() {
    let Fixture { engine, ns, model } = fixture();
    let snapshot = engine.head_snapshot();
    let limits: [&[usize]; 3] = [&[7], &[1, 50, 200], &[200]];
    for (index, query) in random_queries(400).iter().enumerate() {
        let expected = matches(&model, query);
        let limits = limits[index % limits.len()];
        let written = cycled_walk(&snapshot, ns, query, limits, &[WRITTEN]);
        let planned = cycled_walk(&snapshot, ns, query, limits, &[PLANNED]);
        assert_eq!(flat(&written), expected, "written {query:?}");
        assert_eq!(planned, written, "planned pages {query:?}");
        // Reversed literals: the planned evaluation does not care.
        let backwards = reversed(query);
        assert_eq!(
            cycled_walk(&snapshot, ns, &backwards, limits, &[PLANNED]),
            written,
            "planned pages, reversed {query:?}"
        );
        // One cursor chain, alternating evaluations page by page.
        assert_eq!(
            cycled_walk(&snapshot, ns, query, limits, &[PLANNED, WRITTEN]),
            written,
            "alternating pages {query:?}"
        );
        let total = u64::try_from(expected.len()).unwrap();
        assert_eq!(
            count_and_spent(&snapshot, ns, query, WRITTEN).0,
            total,
            "{query:?}"
        );
        assert_eq!(
            count_and_spent(&snapshot, ns, query, PLANNED).0,
            total,
            "{query:?}"
        );
        assert_eq!(count_and_spent(&snapshot, ns, &backwards, PLANNED).0, total);
    }
}

#[test]
fn projections_and_rows_are_byte_identical() {
    let Fixture { engine, ns, .. } = fixture();
    let snapshot = engine.head_snapshot();
    let projections = [
        Projection::ALL,
        Projection::KEY_ONLY,
        Projection::ID_ONLY,
        Projection::named(false, [name("kind"), name("mixed")]),
    ];
    for (index, query) in random_queries(150).iter().enumerate() {
        let projection = &projections[index % projections.len()];
        let mut cursors: [Option<Cursor>; 2] = [None, None];
        loop {
            let [written, planned] = [WRITTEN, PLANNED].map(|budget| {
                let slot = usize::from(budget.is_planned());
                snapshot
                    .query(ns, query, projection, 13, cursors[slot].as_ref(), budget)
                    .unwrap()
                    .value
            });
            assert_eq!(planned.rows, written.rows, "{query:?}");
            assert_eq!(planned.next, written.next, "{query:?}: cursors");
            let Some(next) = written.next else { break };
            cursors = [Some(next.clone()), Some(next)];
        }
    }
}

#[test]
fn planned_id_only_pages_charge_what_row_loading_pages_do() {
    let Fixture { engine, ns, .. } = fixture();
    let snapshot = engine.head_snapshot();
    // No key and no cells either way; only the id-only projection lets a
    // planned page skip loading the rows.
    let loading = Projection::named(false, []);
    for query in random_queries(150).iter().chain([&Query::match_all()]) {
        let mut cursors: [Option<Cursor>; 2] = [None, None];
        loop {
            let [ids, loaded] = [&Projection::ID_ONLY, &loading].map(|projection| {
                let slot = usize::from(*projection != Projection::ID_ONLY);
                snapshot
                    .query(
                        ns,
                        query,
                        projection,
                        37,
                        cursors[slot].as_ref(),
                        PLANNED.traced(),
                    )
                    .unwrap()
            });
            assert_eq!(ids.value.rows, loaded.value.rows, "{query:?}");
            assert_eq!(ids.receipt, loaded.receipt, "{query:?}: charges");
            let (Some(next), Some(other)) = (ids.value.next, loaded.value.next) else {
                break;
            };
            assert_eq!(next.after(), other.after());
            cursors = [Some(next), Some(other)];
        }
    }
}

fn spent(snapshot: &Snapshot, ns: NamespaceId, query: &Query, limit: usize, budget: Budget) -> u64 {
    snapshot
        .query(ns, query, &Projection::ID_ONLY, limit, None, budget)
        .unwrap()
        .receipt
        .spent
        .units()
}

#[test]
fn planned_charges_ignore_the_written_order() {
    let Fixture { engine, ns, model } = fixture();
    let snapshot = engine.head_snapshot();
    let (target, lang) = model
        .records()
        .map(|(_, record)| &record.cells)
        .find(|cells| {
            cells.get(&name("kind")).map(|cell| &cell.value) == Some(&s("contract"))
                && cells.contains_key(&name("lang"))
                && cells.contains_key(&name("owner"))
        })
        .map(|cells| {
            let value = |text| cells[&name(text)].value.clone();
            (value("addr"), value("lang"))
        })
        .unwrap();
    // The Block Explorer's order: scope first, the unique address last.
    let broad_first = Query::all(vec![
        eq("scope", s("all")),
        eq("owner", s("o1")),
        eq("kind", s("contract")),
        eq("lang", lang),
        eq("addr", target),
    ]);
    let selective_first = reversed(&broad_first);
    let rows = |query: &Query, budget| flat(&cycled_walk(&snapshot, ns, query, &[20], &[budget]));
    assert_eq!(rows(&broad_first, PLANNED).len(), 1);
    assert_eq!(rows(&broad_first, PLANNED), rows(&selective_first, WRITTEN));
    let planned_broad = spent(&snapshot, ns, &broad_first, 20, PLANNED);
    let planned_selective = spent(&snapshot, ns, &selective_first, 20, PLANNED);
    let written_broad = spent(&snapshot, ns, &broad_first, 20, WRITTEN);
    let written_selective = spent(&snapshot, ns, &selective_first, 20, WRITTEN);
    assert_eq!(planned_broad, planned_selective);
    // 1 + 5 + 5 seeks (20); the address posting drives: 1 entry; the
    // tautology `scope` is skipped and kind, lang and owner follow with one
    // repositioning each (6); no further entry; ResultRow 3.
    assert_eq!(planned_broad, 1 + 5 + 20 + 1 + 6 + 3);
    assert!(planned_broad <= written_selective + 16);
    // As written, the scope posting drives every record past five probes.
    assert!(written_broad > 50 * planned_broad, "{written_broad}");

    // Postings of distinct sizes and no row probe: any order, one charge.
    let group = vec![
        eq("kind", s("contract")),
        eq("lang", s("sol")),
        eq("opt", Value::Bool(true)),
        eq("evm", s("paris")),
        Literal::not(Term::eq(name("viair"), Value::Bool(true))),
    ];
    let forwards = Query::all(group.clone());
    let backwards = reversed(&forwards);
    let rotated = Query::all([&group[2..], &group[..2]].concat());
    for limit in [1, 5, 200] {
        let charge = spent(&snapshot, ns, &forwards, limit, PLANNED);
        assert_eq!(spent(&snapshot, ns, &backwards, limit, PLANNED), charge);
        assert_eq!(spent(&snapshot, ns, &rotated, limit, PLANNED), charge);
    }
    assert_eq!(
        count_and_spent(&snapshot, ns, &forwards, PLANNED),
        count_and_spent(&snapshot, ns, &backwards, PLANNED)
    );
}

#[test]
fn counts_are_answered_from_stored_sizes() {
    let Fixture { engine, ns, model } = fixture();
    let snapshot = engine.head_snapshot();
    let total = |query: &Query| u64::try_from(matches(&model, query).len()).unwrap();
    let check = |query: Query, planned_spent: u64| {
        let (value, spent) = count_and_spent(&snapshot, ns, &query, PLANNED);
        assert_eq!(value, total(&query), "{query:?}");
        assert_eq!(spent, planned_spent, "{query:?}");
        let (written, written_spent) = count_and_spent(&snapshot, ns, &query, WRITTEN);
        assert_eq!(written, value);
        (value, written_spent)
    };
    // One posting: DirectoryLookup, QueryGroup, TermSeek, whatever its size.
    let (contracts, written) = check(Query::all(vec![eq("kind", s("contract"))]), 1 + 5 + 4);
    assert!(contracts > 1_000);
    assert_eq!(written, 1 + 5 + 4 + contracts);
    // Match-all: the live row count.
    let (all, _) = check(Query::match_all(), 1 + 5);
    assert_eq!(all, u64::try_from(model.len()).unwrap());
    // A tautology (every record has scope = all) plus a posting.
    check(
        Query::all(vec![eq("scope", s("all")), eq("kind", s("contract"))]),
        1 + 5 + 4 + 4,
    );
    // A disjoint union of values of one name: one group each.
    check(
        Query {
            groups: vec![
                vec![eq("kind", s("contract"))],
                vec![eq("kind", s("code"))],
                vec![eq("kind", s("missing"))],
            ],
        },
        1 + 3 * (5 + 4),
    );
    // NotEq: the existence posting less the value posting.
    check(
        Query::term(Term::not_eq(name("kind"), s("blob"))),
        1 + 5 + 4 + 4,
    );
    // NOT Exists over every type of a name, the SDK's `NOT EXISTS(mixed)`:
    // the live rows less four disjoint existence postings (five types are
    // looked up, one is missing).
    check(
        Query::all(
            [
                TypeTag::Str,
                TypeTag::U64,
                TypeTag::I64,
                TypeTag::Bool,
                TypeTag::Addr,
            ]
            .into_iter()
            .map(|tag| Literal::not(Term::exists(name("mixed"), tag)))
            .collect(),
        ),
        1 + 5 + 5 * 4,
    );
    // A prefix run: its seek and one TermVisit per distinct value.
    let safe_terms = {
        let mut names: Vec<&Value> = model
            .records()
            .map(|(_, record)| &record.cells[&name("name")].value)
            .filter(|value| matches!(value, Value::Str(text) if text.starts_with("Safe")))
            .collect();
        names.sort_by_key(|value| format!("{value:?}"));
        names.dedup();
        u64::try_from(names.len()).unwrap()
    };
    check(
        Query::term(Term::starts_with(name("name"), "Safe")),
        1 + 5 + 4 + 3 * safe_terms,
    );
    // Two ranges on one name are one run; every `at` value is distinct.
    let window = Query::all(vec![
        Literal::of(Term::range(
            name("at"),
            Bound::Included(Value::U64(1_000)),
            Bound::Unbounded,
        )),
        Literal::of(Term::range(
            name("at"),
            Bound::Unbounded,
            Bound::Excluded(Value::U64(1_400)),
        )),
    ]);
    let in_window = total(&window);
    assert!(in_window > 30);
    check(window, 1 + 5 + 4 + 3 * in_window);
    // A contradiction costs its group only.
    check(
        Query::all(vec![eq("kind", s("contract")), eq("kind", s("code"))]),
        1 + 5,
    );
    // Overlapping groups are merged and every id counted once.
    let overlapping = Query {
        groups: vec![
            vec![eq("clus", Value::Bool(true))],
            vec![eq("clus2", Value::Bool(true))],
        ],
    };
    assert_eq!(
        count_and_spent(&snapshot, ns, &overlapping, PLANNED).0,
        total(&overlapping)
    );
}

#[test]
fn intersections_count_without_loading_rows() {
    let Fixture { engine, ns, model } = fixture();
    let snapshot = engine.head_snapshot();
    let query = Query::all(vec![
        eq("kind", s("contract")),
        eq("opt", Value::Bool(true)),
        eq("lang", s("sol")),
    ]);
    let expected = u64::try_from(matches(&model, &query).len()).unwrap();
    let planned = snapshot.count(ns, &query, PLANNED).unwrap();
    let written = snapshot.count(ns, &query, WRITTEN).unwrap();
    assert_eq!(planned.value, expected);
    assert_eq!(written.value, expected);
    // Postings only: no record node is visited.
    assert_eq!(planned.physical.record_nodes, 0);
    assert!(written.physical.record_nodes > 0);
    // Clustered sets: the leapfrog skips the ranges one of them lacks.
    let clustered = Query::all(vec![
        eq("clus", Value::Bool(true)),
        eq("clus2", Value::Bool(true)),
    ]);
    let both = u64::try_from(matches(&model, &clustered).len()).unwrap();
    let planned = snapshot.count(ns, &clustered, PLANNED).unwrap();
    assert_eq!(planned.value, both);
    let clus =
        u64::try_from(matches(&model, &Query::all(vec![eq("clus", Value::Bool(true))])).len())
            .unwrap();
    // At most an entry and a repositioning (3 units) per match plus the
    // jumps between the clusters, where stepping every `clus` entry and
    // probing each would cost 3 per `clus` entry.
    assert!(planned.receipt.spent.units() < 1 + 5 + 8 + 3 * both + 40);
    assert!(3 * clus > 3 * both + 40 + 400);
}

#[test]
fn a_planned_read_stops_at_its_budget() {
    let Fixture { engine, ns, .. } = fixture();
    let snapshot = engine.head_snapshot();
    let query = Query::all(vec![
        eq("owner", s("o1")),
        Literal::of(Term::not_eq(name("kind"), s("blob"))),
        Literal::not(Term::starts_with(name("name"), "S")),
    ]);
    let full = spent(&snapshot, ns, &query, 200, PLANNED);
    for limit in [1, 9, 10, 11, full / 2, full - 1] {
        let rejected = snapshot
            .query(
                ns,
                &query,
                &Projection::ID_ONLY,
                200,
                None,
                Budget::limit(limit).planned(),
            )
            .unwrap_err();
        assert_eq!(rejected.reason, Rejection::OutOfBudget, "limit {limit}");
        assert!(rejected.receipt.spent.units() <= limit);
        let rejected = snapshot
            .count(ns, &query, Budget::limit(limit.min(10)).planned())
            .unwrap_err();
        assert_eq!(rejected.reason, Rejection::OutOfBudget);
    }
    assert!(
        snapshot
            .query(
                ns,
                &query,
                &Projection::ID_ONLY,
                200,
                None,
                Budget::limit(full).planned()
            )
            .is_ok()
    );
}

#[test]
fn read_side_query_limits_admit_wider_queries() {
    let Fixture { engine, ns, model } = fixture();
    let snapshot = engine.head_snapshot();
    let wide_group: Vec<Literal> = vec![
        eq("scope", s("all")),
        eq("owner", s("o1")),
        eq("kind", s("contract")),
        eq("lang", s("sol")),
        eq("opt", Value::Bool(true)),
        Literal::of(Term::exists(name("evm"), TypeTag::Str)),
        Literal::of(Term::exists(name("name"), TypeTag::Str)),
        Literal::of(Term::exists(name("at"), TypeTag::U64)),
        Literal::of(Term::not_eq(name("evm"), s("paris"))),
    ];
    let wide = Query::all(wide_group.clone());
    let many = Query {
        groups: (0..10)
            .map(|index| vec![eq("name", s(&format!("Safe{index}")))])
            .collect(),
    };
    let bounds = QueryLimits {
        max_groups: 16,
        max_literals: 16,
        max_page: 200,
    };
    for budget in [WRITTEN, PLANNED] {
        assert_eq!(
            snapshot
                .query(ns, &wide, &Projection::ID_ONLY, 10, None, budget)
                .unwrap_err()
                .reason,
            Rejection::InvalidQuery(QueryFault::TooManyLiterals)
        );
        assert_eq!(
            snapshot.count(ns, &many, budget).unwrap_err().reason,
            Rejection::InvalidQuery(QueryFault::TooManyGroups)
        );
        let widened = budget.with_query_limits(bounds);
        for query in [&wide, &many] {
            let expected = matches(&model, query);
            assert_eq!(
                snapshot.count(ns, query, widened).unwrap().value,
                u64::try_from(expected.len()).unwrap()
            );
            let pages = cycled_walk(&snapshot, ns, query, &[200], &[widened]);
            assert_eq!(flat(&pages), expected);
        }
        // The page bound is the read's own, here the instance's 200.
        assert_eq!(
            snapshot
                .query(ns, &wide, &Projection::ID_ONLY, 201, None, widened)
                .unwrap_err()
                .reason,
            Rejection::InvalidQuery(QueryFault::LimitTooLarge)
        );
        // A wider page bound admits longer pages, which join the narrower
        // ones: the same rows in the same order, and the same end.
        let long = budget.with_query_limits(QueryLimits {
            max_groups: 8,
            max_literals: 8,
            max_page: 4_096,
        });
        for query in [
            Query::match_all(),
            Query::all(vec![eq("kind", s("contract"))]),
        ] {
            let narrow = cycled_walk(&snapshot, ns, &query, &[200], &[budget]);
            let wide = cycled_walk(&snapshot, ns, &query, &[1_000, 4_096], &[long]);
            assert_eq!(flat(&wide), flat(&narrow), "{query:?}");
            assert_eq!(flat(&wide), matches(&model, &query));
            assert!(wide.len() < narrow.len());
        }
        let narrowed = budget.with_query_limits(QueryLimits {
            max_groups: 1,
            max_literals: 1,
            max_page: 200,
        });
        assert_eq!(
            snapshot
                .count(ns, &Query::all(wide_group[..2].to_vec()), narrowed)
                .unwrap_err()
                .reason,
            Rejection::InvalidQuery(QueryFault::TooManyLiterals)
        );
    }
}
