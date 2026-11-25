//! Query translation against golem-db's own engine.
//!
//! One data set is committed to golem-db's memory `Database` and to the
//! adapter over both arkiv backends. Generated and hand-written queries then
//! run on all three, and every page, count, total and error must be the
//! same. The adapter's [`QueryStats`] show which path answered: the engine
//! alone, or one of the documented fallbacks.

mod common;

use std::ops::Bound;

use common::*;

/// Attribute names of the data set and the type each mostly holds. `mixed`
/// holds a `u64` or a `str`; `fld` is always a field; `nope` is never set.
const FIELDS: [(&str, usize); 8] = [
    ("n", 0),
    ("s", 1),
    ("f", 2),
    ("i", 3),
    ("b", 4),
    ("h", 5),
    ("mixed", 0),
    ("fld", 0),
];

fn field(name: &str) -> CellName {
    CellNameRef::raw(name.as_bytes()).into()
}

/// A record of the data set: each cell present about two times in three,
/// sometimes as a field under an attribute's name, sometimes of another type.
fn record(rng: &mut Rng, key: RecordKey) -> RecordOp<op::Create> {
    let mut op = RecordOp::create(key, B);
    for (name, ty) in FIELDS {
        if rng.chance(35) {
            continue;
        }
        let ty = match name {
            "mixed" if rng.chance(50) => 1,
            _ => ty,
        };
        let value = value(rng, ty);
        op = if name == "fld" || rng.chance(10) {
            op.field(name, value)
        } else {
            op.attribute(name, value)
        };
    }
    op
}

fn record_key(n: u16) -> RecordKey {
    let mut key = [0x51; 32];
    key[..2].copy_from_slice(&n.to_be_bytes());
    RecordKey(key)
}

/// Commits the same history on every backend: creates, patches, deletes and
/// re-creations over several commits, so record ids do not follow keys.
fn populate(backends: &Backends, seed: u64, records: u16) {
    for (name, api) in backends.all() {
        let mut rng = Rng(seed);
        let mut next = 0u16;
        for _commit in 0..6 {
            let branch = api.begin().unwrap();
            for _ in 0..records / 6 {
                let key = record_key(next);
                next += 1;
                api.create(branch, record(&mut rng, key))
                    .result
                    .expect(name);
            }
            for _ in 0..records / 20 {
                let key = record_key(rng.below(next as usize) as u16);
                match rng.below(3) {
                    0 => {
                        let _ = api.delete(branch, RecordOp::delete(key, B)).result;
                    }
                    1 => {
                        let _ = api.delete(branch, RecordOp::delete(key, B)).result;
                        api.create(branch, record(&mut rng, key))
                            .result
                            .expect(name);
                    }
                    _ => {
                        let patch = RecordOp::patch(key, B)
                            .attribute("n", value(&mut rng, 0))
                            .remove("s");
                        let _ = api.patch(branch, patch).result;
                    }
                }
            }
            api.commit(branch).unwrap();
        }
    }
}

fn bound(rng: &mut Rng, ty: usize) -> Bound<CellValue> {
    match rng.below(3) {
        0 => Bound::Unbounded,
        1 => Bound::Included(value(rng, ty)),
        _ => Bound::Excluded(value(rng, ty)),
    }
}

fn leaf(rng: &mut Rng) -> Filter {
    let (name, own) = *rng.pick(&FIELDS);
    let name = if rng.chance(5) { "nope" } else { name };
    // Mostly the field's own type, sometimes another: a typed leaf sees a
    // cell of another type as absent.
    let ty = if rng.chance(85) {
        own
    } else {
        rng.below(TYPES)
    };
    let field = field(name);
    match rng.below(6) {
        0 | 1 => Filter::Eq {
            field,
            value: EqValue::try_from(value(rng, ty)).unwrap(),
        },
        2 => {
            let values = (0..rng.below(5))
                .map(|_| value(rng, ty))
                .collect::<Vec<_>>();
            Filter::In {
                field,
                values: InValues::try_from(values).unwrap(),
            }
        }
        3 => Filter::Range {
            field,
            bounds: Bounds::try_new(cell_type(ty), bound(rng, ty), bound(rng, ty)).unwrap(),
        },
        4 => Filter::Prefix {
            field,
            prefix: (*rng.pick(&["", "a", "ab", "b", "zz", "q"])).into(),
        },
        _ => Filter::Exists {
            field,
            ty: rng
                .chance(60)
                .then(|| ExistsType::try_from(cell_type(ty)).unwrap()),
        },
    }
}

fn filter(rng: &mut Rng, depth: usize) -> Filter {
    if depth == 0 || rng.chance(45) {
        return leaf(rng);
    }
    let children = (0..1 + rng.below(3))
        .map(|_| filter(rng, depth - 1))
        .collect();
    if rng.chance(50) {
        Filter::And(children)
    } else {
        Filter::Or(children)
    }
}

const SORTS: [(&str, usize); 7] = [
    ("n", 0),
    ("s", 1),
    ("f", 2),
    ("b", 4),
    ("fld", 0),
    ("mixed", 1),
    ("nope", 0),
];

fn query(rng: &mut Rng, filter: Filter, sorted: bool) -> QueryOp {
    let mut op = QueryOp::from_filter(filter, B);
    if sorted {
        let keys = (0..1 + rng.below(3)).map(|_| {
            let (name, ty) = *rng.pick(&SORTS);
            let direction = if rng.chance(50) {
                Direction::Asc
            } else {
                Direction::Desc
            };
            SortKey::try_new(name, cell_type(ty), direction).unwrap()
        });
        op = op.sort(keys.collect::<Vec<_>>());
    }
    if rng.chance(60) {
        op = op.limit(*rng.pick(&[1, 3, 10, 100]));
    }
    if rng.chance(50) {
        op = op.offset(*rng.pick(&[0, 1, 5, 40, 100_000]));
    }
    if rng.chance(50) {
        op = op.with_total();
    }
    match rng.below(6) {
        0 => op.only(Vec::<&str>::new()),
        1 => op.only(["n", "#key"]),
        2 => op.only(["#meta"]),
        3 => op.only(["s", "missing", "fld"]),
        _ => op,
    }
}

/// Runs `op` and a count of its filter on every backend and compares them
/// with golem-db's.
fn compare(backends: &Backends, what: &str, op: &QueryOp) {
    let page = |api: &dyn Api| outcome(api.query(None, op.clone()));
    let count = |api: &dyn Api| {
        let filter = op.request().filter.clone();
        outcome(api.count(None, CountOp::filter(filter, B)))
    };
    let all = backends.all();
    let (expected_page, expected_count) = (page(all[0].1), count(all[0].1));
    for (name, api) in &all[1..] {
        assert_eq!(page(*api), expected_page, "{what} on {name}: {op:?}");
        assert_eq!(
            count(*api),
            expected_count,
            "count of {what} on {name}: {op:?}"
        );
    }
}

fn stats(backends: &Backends) -> [QueryStats; 2] {
    backends.adapters().map(ArkivDb::query_stats)
}

#[test]
fn generated_queries_agree_with_golem_db_and_run_on_the_engine() {
    let backends = Backends::new();
    populate(&backends, 11, 240);
    let mut rng = Rng(2026);
    for case in 0..500 {
        let filter = filter(&mut rng, 3);
        let op = query(&mut rng, filter, false);
        compare(&backends, &format!("unsorted case {case}"), &op);
    }
    // Every one of them was one engine query, or needed none: no fallback,
    // no scan and no adapter-side ordering.
    for stats in stats(&backends) {
        assert!(stats.native > 500, "{stats:?}");
        assert_eq!(
            (stats.fallback, stats.scans, stats.sorted),
            (0, 0, 0),
            "{stats:?}"
        );
    }
}

#[test]
fn sorted_queries_agree_with_golem_db() {
    let backends = Backends::new();
    populate(&backends, 12, 240);
    let mut rng = Rng(7);
    for case in 0..300 {
        let filter = filter(&mut rng, 2);
        let op = query(&mut rng, filter, true);
        compare(&backends, &format!("sorted case {case}"), &op);
    }
    // The filter still runs on the engine; only the order is the adapter's.
    for stats in stats(&backends) {
        assert!(stats.sorted > 0, "{stats:?}");
        assert_eq!((stats.fallback, stats.scans), (0, 0), "{stats:?}");
    }
}

/// Every record, ordered by one key: absent values first ascending and last
/// descending, fields sorted like attributes, ties in record-id order.
#[test]
fn sort_order_over_every_record_matches() {
    let backends = Backends::new();
    populate(&backends, 13, 120);
    let all_records = || {
        Filter::Or(vec![
            Filter::Exists {
                field: field("n"),
                ty: None,
            },
            Filter::Exists {
                field: field("s"),
                ty: None,
            },
            Filter::Exists {
                field: field("b"),
                ty: None,
            },
        ])
    };
    for (name, ty) in SORTS {
        for direction in [Direction::Asc, Direction::Desc] {
            for offset in [0, 100] {
                let op = QueryOp::from_filter(all_records(), B)
                    .sort([SortKey::try_new(name, cell_type(ty), direction).unwrap()])
                    .offset(offset)
                    .with_total();
                compare(&backends, &format!("order by {name} {direction:?}"), &op);
            }
        }
    }
}

/// The forms with no single engine query take the documented fallbacks and
/// still agree with golem-db.
#[test]
fn fallbacks_agree_with_golem_db() {
    let backends = Backends::new();
    populate(&backends, 14, 240);
    let any = |name: &str| Filter::Exists {
        field: field(name),
        ty: None,
    };

    // 23 groups each: the product of three is over the engine's bounds.
    let before = stats(&backends);
    let wide = Filter::And(vec![any("n"), any("s"), any("b")]);
    assert_eq!(
        backends.memory.translator().plan(&wide).unwrap(),
        Plan::Fallback(Fallback::Groups)
    );
    compare(
        &backends,
        "wide and",
        &QueryOp::from_filter(wide.clone(), B).with_total(),
    );
    let sorted = QueryOp::from_filter(wide, B)
        .sort([SortKey::try_new("n", cell_type(0), Direction::Desc).unwrap()])
        .limit(7)
        .offset(2);
    compare(&backends, "wide and, sorted", &sorted);

    // More values than groups: walked in chunks.
    let many = (0..3000u64).map(CellValue::from_u64).collect::<Vec<_>>();
    let many = Filter::In {
        field: field("n"),
        values: InValues::try_from(many).unwrap(),
    };
    assert_eq!(
        backends.memory.translator().plan(&many).unwrap(),
        Plan::Fallback(Fallback::Groups)
    );
    compare(
        &backends,
        "in 3000",
        &QueryOp::from_filter(many.clone(), B).limit(5),
    );
    // Inside a tree, only that node falls back.
    let tree = Filter::Or(vec![
        Filter::And(vec![many, any("s")]),
        Filter::Prefix {
            field: field("s"),
            prefix: "zz".into(),
        },
    ]);
    compare(
        &backends,
        "tree with in 3000",
        &QueryOp::from_filter(tree, B),
    );
    let after = stats(&backends);
    for (before, after) in before.iter().zip(after) {
        // Four queries, each a page and a count.
        assert_eq!(after.fallback - before.fallback, 8, "{after:?}");
        assert_eq!(after.scans, before.scans);
        assert_eq!(after.sorted - before.sorted, 1);
    }

    // A 65-byte bound, over `max_str_len`: no attribute is that long, but
    // the bound still splits the shorter ones.
    let long = "ab".repeat(33);
    for (lower, upper) in [
        (Bound::Unbounded, Bound::Excluded(long.as_str())),
        (Bound::Included(long.as_str()), Bound::Unbounded),
        (Bound::Excluded("a"), Bound::Included(long.as_str())),
    ] {
        let before = stats(&backends);
        let to_cell = |bound: Bound<&str>| bound.map(CellValue::from_str);
        let range = Filter::Range {
            field: field("s"),
            bounds: Bounds::try_new(cell_type(1), to_cell(lower), to_cell(upper)).unwrap(),
        };
        assert_eq!(
            backends.memory.translator().plan(&range).unwrap(),
            Plan::Fallback(Fallback::LongBound)
        );
        compare(
            &backends,
            "long bound",
            &QueryOp::from_filter(range, B).with_total(),
        );
        let after = stats(&backends);
        for (before, after) in before.iter().zip(after) {
            assert_eq!(after.scans - before.scans, 2, "{after:?}");
        }
    }
}

/// Leaves no record can match are answered without the engine.
#[test]
fn leaves_that_match_nothing_need_no_engine() {
    let backends = Backends::new();
    populate(&backends, 15, 60);
    let long = "x".repeat(65);
    let empty = [
        Filter::Eq {
            field: field("s"),
            value: EqValue::try_from(CellValue::from_str(&long)).unwrap(),
        },
        Filter::Prefix {
            field: field("s"),
            prefix: long.clone(),
        },
        Filter::In {
            field: field("n"),
            values: InValues::try_from(Vec::new()).unwrap(),
        },
        // Inverted bounds.
        Filter::Range {
            field: field("n"),
            bounds: Bounds::try_new(
                cell_type(0),
                Bound::Included(CellValue::from_u64(9)),
                Bound::Excluded(CellValue::from_u64(3)),
            )
            .unwrap(),
        },
        // A valid name longer than any cell name of this genesis.
        Filter::Exists {
            field: field(&"n".repeat(40)),
            ty: None,
        },
    ];
    for filter in empty {
        assert_eq!(
            backends.memory.translator().plan(&filter).unwrap(),
            Plan::Empty,
            "{filter:?}"
        );
        let within = Filter::And(vec![
            Filter::Exists {
                field: field("n"),
                ty: None,
            },
            filter.clone(),
        ]);
        assert_eq!(
            backends.memory.translator().plan(&within).unwrap(),
            Plan::Empty
        );
        compare(
            &backends,
            "empty leaf",
            &QueryOp::from_filter(filter, B).with_total(),
        );
        compare(
            &backends,
            "and with an empty leaf",
            &QueryOp::from_filter(within, B),
        );
    }
    for stats in stats(&backends) {
        assert_eq!((stats.native, stats.fallback), (0, 0), "{stats:?}");
        assert_eq!(stats.empty, 20);
    }
}

/// Admission errors, in golem-db's order and words.
#[test]
fn refused_queries_agree_with_golem_db() {
    let backends = Backends::new();
    populate(&backends, 16, 30);
    let ok = || Filter::Exists {
        field: field("n"),
        ty: None,
    };
    let bad_name = || Filter::Eq {
        field: field("9bad"),
        value: EqValue::try_from(CellValue::from_u64(1)).unwrap(),
    };
    let key = |name: &str| SortKey::try_new(name, cell_type(0), Direction::Asc).unwrap();
    let mut deep = ok();
    for _ in 0..65 {
        deep = Filter::And(vec![deep]);
    }
    let mut deepest_allowed = ok();
    for _ in 0..64 {
        deepest_allowed = Filter::Or(vec![deepest_allowed]);
    }
    let cases = [
        QueryOp::from_filter(ok(), B).limit(0),
        QueryOp::from_filter(ok(), B).limit(101),
        QueryOp::from_filter(ok(), B).limit(100),
        // The limit is checked before the filter, the filter before the sort.
        QueryOp::from_filter(bad_name(), B).limit(0),
        QueryOp::from_filter(bad_name(), B).sort((0..5).map(|_| key("n")).collect::<Vec<_>>()),
        QueryOp::from_filter(ok(), B).sort((0..5).map(|_| key("n")).collect::<Vec<_>>()),
        QueryOp::from_filter(ok(), B).sort((0..4).map(|_| key("n")).collect::<Vec<_>>()),
        QueryOp::from_filter(ok(), B).sort([key("n"), key("bad name")]),
        QueryOp::from_filter(Filter::And(Vec::new()), B),
        QueryOp::from_filter(Filter::Or(vec![ok(), Filter::Or(Vec::new())]), B),
        QueryOp::from_filter(deep, B),
        QueryOp::from_filter(deepest_allowed, B),
        QueryOp::from_filter(
            Filter::Prefix {
                field: field("s\u{0}"),
                prefix: "a".into(),
            },
            B,
        ),
        QueryOp::from_filter(
            Filter::Exists {
                field: CellNameRef::raw(&[0xff, 0xfe]).into(),
                ty: None,
            },
            B,
        ),
        QueryOp::from_filter(
            Filter::In {
                field: field(""),
                values: InValues::try_from(Vec::new()).unwrap(),
            },
            B,
        ),
    ];
    for op in &cases {
        compare(&backends, "refused", op);
    }
    // Only the head can be queried, as on golem-db `main`.
    let head = backends.golem.head().unwrap();
    for commit in [
        head,
        CommitId::new(head.get() - 1),
        CommitId::new(head.get() + 1),
    ] {
        let page = |api: &dyn Api| outcome(api.query(Some(commit), QueryOp::from_filter(ok(), B)));
        let count = |api: &dyn Api| outcome(api.count(Some(commit), CountOp::filter(ok(), B)));
        for (name, api) in &backends.all()[1..] {
            assert_eq!(page(*api), page(&backends.golem), "{name} at {commit}");
            assert_eq!(count(*api), count(&backends.golem), "{name} at {commit}");
        }
    }
}

/// How each operator is translated: the shape of the engine query.
#[test]
fn operators_translate_to_the_engine_query_form() {
    let translator = Translator { max_str_len: 64 };
    let groups = |filter: &Filter| match translator.plan(filter).unwrap() {
        Plan::Native(query) => query.groups.iter().map(Vec::len).collect::<Vec<_>>(),
        other => panic!("{filter:?} is not native: {other:?}"),
    };
    let eq = |name: &str, n: u64| Filter::Eq {
        field: field(name),
        value: EqValue::try_from(CellValue::from_u64(n)).unwrap(),
    };
    let one_of = |name: &str, values: &[u64]| Filter::In {
        field: field(name),
        values: InValues::try_from(
            values
                .iter()
                .map(|n| CellValue::from_u64(*n))
                .collect::<Vec<_>>(),
        )
        .unwrap(),
    };
    let exists = |name: &str, typed: bool| Filter::Exists {
        field: field(name),
        ty: typed.then(|| ExistsType::try_from(cell_type(0)).unwrap()),
    };
    // One term, one group.
    assert_eq!(groups(&eq("n", 1)), [1]);
    assert_eq!(groups(&exists("n", true)), [1]);
    assert_eq!(
        groups(&Filter::Prefix {
            field: field("s"),
            prefix: String::new(),
        }),
        [1]
    );
    let range = |lower, upper| Filter::Range {
        field: field("n"),
        bounds: Bounds::try_new(cell_type(0), lower, upper).unwrap(),
    };
    assert_eq!(
        groups(&range(
            Bound::Included(CellValue::from_u64(1)),
            Bound::Unbounded
        )),
        [1]
    );
    // Both sides open: the type's existence test.
    assert_eq!(groups(&range(Bound::Unbounded, Bound::Unbounded)), [1]);
    // One group per value, and per indexable type.
    assert_eq!(groups(&one_of("n", &[1, 2, 3])), [1, 1, 1]);
    assert_eq!(groups(&exists("n", false)).len(), 23);
    // `And` multiplies groups and adds literals; `Or` appends groups.
    let and = Filter::And(vec![eq("n", 1), one_of("i", &[1, 2]), exists("b", true)]);
    assert_eq!(groups(&and), [3, 3]);
    let or = Filter::Or(vec![eq("n", 1), and.clone(), one_of("n", &[4, 5])]);
    assert_eq!(groups(&or), [1, 3, 3, 1, 1]);
    let nested = Filter::And(vec![or.clone(), Filter::Or(vec![eq("a", 1), eq("b", 2)])]);
    assert_eq!(groups(&nested), [2, 2, 4, 4, 4, 4, 2, 2, 2, 2]);
    // The bounds: 1024 groups fit, 1025 do not; 256 literals fit, 257 do not.
    let wide = |n: u64| one_of("n", &(0..n).collect::<Vec<_>>());
    assert_eq!(groups(&wide(1024)).len(), 1024);
    assert_eq!(
        translator.plan(&wide(1025)).unwrap(),
        Plan::Fallback(Fallback::Groups)
    );
    assert_eq!(
        groups(&Filter::And(vec![
            wide(32),
            one_of("i", &(0..32).collect::<Vec<_>>())
        ]))
        .len(),
        1024
    );
    let long = |n: u64| Filter::And((0..n).map(|i| eq("n", i)).collect());
    assert_eq!(groups(&long(256)), [256]);
    assert_eq!(
        translator.plan(&long(257)).unwrap(),
        Plan::Fallback(Fallback::Literals)
    );
    // An empty child empties an `And` and vanishes from an `Or`.
    let none = one_of("n", &[]);
    assert_eq!(
        translator
            .plan(&Filter::And(vec![wide(1025), none.clone()]))
            .unwrap(),
        Plan::Empty
    );
    assert_eq!(groups(&Filter::Or(vec![none.clone(), eq("n", 1)])), [1]);
    assert_eq!(
        translator
            .plan(&Filter::Or(vec![none.clone(), none]))
            .unwrap(),
        Plan::Empty
    );
}
