//! Garbage collection of the durable engine, differential against an
//! archive: two engines run the same random blocks over tiny value domains
//! (so posting maps, rows and whole subtrees are shared between terms,
//! namespaces and blocks, and objects die and come back), one keeping
//! everything and one pruning to a window with garbage collection. Inside
//! the window every receipt, root, read, query page, count and proof must
//! be identical; below it the pruned engine refuses with `HistoryPruned`.
//! After every commit the counts are verified against the store and every
//! snapshot the horizon protects is read in full.

use std::sync::Arc;

use arkiv_db::{
    Budget, EngineError, Projection, Query, ReadResult, SealedCandidate, Snapshot, Term, Value,
    WriteOp,
    durable::{
        DurableEngine, DurableError, Options, Publication, Retention,
        gc::{self, check_snapshot},
    },
    proof::{EqQuery, EqRequest, ProofLimits},
};
use arkiv_db_memory::MemoryVersionedStore;
use arkiv_db_storage::versioned::VersionedStore;

use crate::support::{
    durable::{genesis, initialize, snapshot_id},
    proofs::snapshot_ref,
    random::Generator64,
    vocab::{
        OWNER, add_namespace, attr, create_record, delete, delete_record, field, name, ns,
        patch_record, put,
    },
};

fn options(retention: Retention, budget: u64) -> Options {
    Options {
        retention,
        gc_release_budget: budget,
        gc_release_per_object: if budget < 10 { 0 } else { 4 },
        gc_delete_budget: budget,
        ..Options::default()
    }
}

/// One random write over tiny domains: four record keys per namespace,
/// three values per attribute, and two attributes that often hold the same
/// value, so a one-member posting moves between terms and is shared by
/// several of them.
fn random_op(rng: &mut Generator64, namespaces: u64) -> WriteOp {
    let namespace = ns(1 + rng.below(namespaces as usize) as u64);
    let record = [rng.below(4) as u8 + 1];
    let colour = Value::str(["red", "green", "blue"][rng.below(3)]);
    let shade = if rng.below(2) == 0 {
        colour.clone()
    } else {
        Value::str(["red", "teal"][rng.below(2)])
    };
    let size = Value::U64(rng.below(3) as u64);
    match rng.below(10) {
        0..=2 => create_record(
            namespace,
            record,
            vec![
                (name("colour"), attr(colour)),
                (name("shade"), attr(shade)),
                (name("size"), attr(size)),
                (name("note"), field(Value::bytes(&[rng.below(2) as u8; 40]))),
            ],
        ),
        3..=5 => patch_record(
            namespace,
            record,
            vec![(name("colour"), attr(colour))],
            vec![],
        ),
        6 => patch_record(
            namespace,
            record,
            vec![(name("shade"), attr(shade))],
            vec![name("size")],
        ),
        7 => delete_record(namespace, record),
        // A raw value of 36 bytes encodes to 40: it looks like a map handle.
        8 => put(namespace, &record, &[rng.below(3) as u8; 36]),
        _ => delete(namespace, &record),
    }
}

/// The same block on both engines: receipts must agree.
fn block(
    archive: &DurableEngine,
    pruned: &DurableEngine,
    ops: &[WriteOp],
    namespaces: &mut u64,
    rng: &mut Generator64,
) -> (SealedCandidate, SealedCandidate) {
    let mut a = archive.begin(archive.head(), None).unwrap();
    let mut p = pruned.begin(pruned.head(), None).unwrap();
    if *namespaces < 3 && (archive.head() == snapshot_id(0) || rng.below(20) == 0) {
        assert_eq!(
            add_namespace(&mut a, OWNER, &format!("n{namespaces}")),
            add_namespace(&mut p, OWNER, &format!("n{namespaces}"))
        );
        *namespaces += 1;
    }
    for op in ops {
        let left = a.execute(OWNER, op.clone(), Budget::unlimited());
        let right = p.execute(OWNER, op.clone(), Budget::unlimited());
        match (left, right) {
            (Ok(l), Ok(r)) => assert_eq!((l.effect, l.receipt), (r.effect, r.receipt)),
            (Err(l), Err(r)) => assert_eq!(l, r),
            (l, r) => panic!("engines disagree: {l:?} / {r:?}"),
        }
    }
    if rng.below(4) == 0 {
        let key = [rng.below(3) as u8; 32];
        let value = [rng.below(2) as u8 + 1; 3];
        assert_eq!(
            a.try_host_put(key, &value).unwrap(),
            p.try_host_put(key, &value).unwrap()
        );
    }
    (a.try_seal().unwrap(), p.try_seal().unwrap())
}

fn commit(engine: &mut DurableEngine, candidate: SealedCandidate) {
    let info = engine.prepare(candidate).unwrap();
    assert_eq!(engine.commit().unwrap(), Publication::Committed(info));
}

fn strip<T: PartialEq + std::fmt::Debug>(
    result: ReadResult<T>,
) -> Result<(T, arkiv_db::Receipt), arkiv_db::Rejected> {
    result.map(|metered| (metered.value, metered.receipt))
}

/// Every read the window promises, compared between the two snapshots.
fn same_reads(a: &Snapshot, p: &Snapshot, namespaces: u64) {
    assert_eq!(a.state_root(), p.state_root());
    let budget = Budget::unlimited;
    for n in 1..=namespaces {
        let namespace = ns(n);
        for record in 1..=4u8 {
            assert_eq!(
                strip(a.try_get(namespace, &[record], budget()).unwrap()),
                strip(p.try_get(namespace, &[record], budget()).unwrap())
            );
        }
        let queries = [
            Query::term(Term::Eq {
                name: name("colour"),
                value: Value::str("red"),
            }),
            Query::term(Term::Eq {
                name: name("shade"),
                value: Value::str("teal"),
            }),
            Query::term(Term::Range {
                name: name("size"),
                lower: std::ops::Bound::Included(Value::U64(1)),
                upper: std::ops::Bound::Unbounded,
            }),
            Query::match_all(),
        ];
        for query in &queries {
            assert_eq!(
                strip(a.try_count(namespace, query, budget()).unwrap()),
                strip(p.try_count(namespace, query, budget()).unwrap())
            );
            // Page through with a cursor, two rows at a time.
            let (mut ca, mut cp) = (None, None);
            loop {
                let pa = a
                    .try_query(namespace, query, &Projection::ALL, 2, ca.as_ref(), budget())
                    .unwrap();
                let pp = p
                    .try_query(namespace, query, &Projection::ALL, 2, cp.as_ref(), budget())
                    .unwrap();
                let (pa, pp) = match (pa, pp) {
                    (Ok(pa), Ok(pp)) => (pa, pp),
                    (pa, pp) => {
                        // Refused alike (a namespace not created yet).
                        assert_eq!(pa.err(), pp.err());
                        break;
                    }
                };
                assert_eq!(pa.value.rows, pp.value.rows);
                assert_eq!(pa.receipt, pp.receipt);
                assert_eq!(pa.value.next.is_some(), pp.value.next.is_some());
                match (pa.value.next, pp.value.next) {
                    (Some(x), Some(y)) => {
                        ca = Some(x);
                        cp = Some(y);
                    }
                    _ => break,
                }
            }
        }
        for colour in ["red", "green", "blue"] {
            let request = |s: &Snapshot| EqRequest {
                snapshot: snapshot_ref(s),
                query: EqQuery {
                    namespace,
                    attribute: name("colour"),
                    value: Value::str(colour),
                },
                limit: 3,
                continuation: None,
            };
            let left = a.prove_eq(&request(a), ProofLimits::default(), budget());
            let right = p.prove_eq(&request(p), ProofLimits::default(), budget());
            assert_eq!(
                left.map(|g| g.bytes).map_err(|e| e.to_string()),
                right.map(|g| g.bytes).map_err(|e| e.to_string())
            );
        }
    }
    for key in 0..3u8 {
        assert_eq!(
            a.try_host_get(&[key; 32]).unwrap(),
            p.try_host_get(&[key; 32]).unwrap()
        );
    }
}

struct Run {
    pruned_store: Arc<MemoryVersionedStore>,
    archive_store: Arc<MemoryVersionedStore>,
    /// The largest table sizes seen after any commit.
    seen: arkiv_db_storage::gc::GcState,
}

fn run(seed: u64, blocks: u64, window: u64, budget: u64) -> Run {
    let mut rng = Generator64(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
    let archive_store = Arc::new(MemoryVersionedStore::new());
    let pruned_store = Arc::new(MemoryVersionedStore::new());
    let mut archive = initialize(archive_store.clone(), options(Retention::Archive, budget));
    let mut pruned = initialize(
        pruned_store.clone(),
        options(Retention::Window(window), budget),
    );
    pruned.enable_gc(None).unwrap();
    let mut namespaces = 0u64;
    let mut seen = arkiv_db_storage::gc::GcState::default();
    // Handles a reader keeps open for a while: deletion must wait for them.
    let mut held: Vec<(u64, Snapshot, Snapshot)> = Vec::new();
    for _ in 0..blocks {
        let ops: Vec<WriteOp> = (0..rng.below(5))
            .map(|_| random_op(&mut rng, namespaces.max(1)))
            .collect();
        let (a, p) = block(&archive, &pruned, &ops, &mut namespaces, &mut rng);
        commit(&mut archive, a);
        commit(&mut pruned, p);
        assert_eq!(archive.head_info(), pruned.head_info());
        gc::verify(pruned_store.as_ref())
            .unwrap_or_else(|e| panic!("seed {seed} head {}: {e}", pruned.head().sequence()));
        let state = pruned_store.gc_state().unwrap().unwrap();
        assert!(!state.suspended, "seed {seed}: {state:?}");
        seen.shared = seen.shared.max(state.shared);
        seen.phantoms = seen.phantoms.max(state.phantoms);
        seen.pending = seen.pending.max(state.pending);
        let head = pruned.head().sequence();
        let floor = pruned.state_floor().sequence();
        // Every snapshot a reader may still open is whole.
        for sequence in pruned.gc_horizon()..=head {
            check_snapshot(pruned_store.as_ref(), sequence)
                .unwrap_or_else(|e| panic!("seed {seed}: snapshot {sequence} at head {head}: {e}"));
        }
        // Inside the window: identical answers. Below: refused.
        for sequence in floor..=head {
            let a = archive.snapshot(snapshot_id(sequence)).unwrap();
            let p = pruned.snapshot(snapshot_id(sequence)).unwrap();
            if sequence == head || rng.below(3) == 0 {
                same_reads(&a, &p, namespaces);
            }
        }
        for sequence in floor.saturating_sub(3)..floor {
            assert!(matches!(
                pruned.snapshot(snapshot_id(sequence)),
                Err(DurableError::Engine(EngineError::HistoryPruned { .. }))
            ));
            archive.snapshot(snapshot_id(sequence)).unwrap();
        }
        for (_, a, p) in &held {
            same_reads(a, p, namespaces);
        }
        if rng.below(6) == 0 {
            held.push((
                head,
                archive.snapshot(snapshot_id(head)).unwrap(),
                pruned.snapshot(snapshot_id(head)).unwrap(),
            ));
        }
        held.retain(|(at, _, _)| at + 12 > head);
    }
    Run {
        pruned_store,
        archive_store,
        seen,
    }
}

#[test]
fn pruned_and_archive_engines_answer_alike_inside_the_window() {
    let mut totals = arkiv_db_storage::gc::GcState::default();
    for seed in 0..16 {
        let window = [0, 1, 2, 5][seed as usize % 4];
        let budget = [0, 1, 2, 4096][seed as usize / 4 % 4];
        let run = run(seed, 50, window, budget);
        let state = run.pruned_store.gc_state().unwrap().unwrap();
        totals.resurrected_total += state.resurrected_total;
        totals.deleted_total += state.deleted_total;
        totals.shared = totals.shared.max(run.seen.shared);
        totals.phantoms = totals.phantoms.max(run.seen.phantoms);
        totals.pending = totals.pending.max(run.seen.pending);
    }
    // Sharing, resurrection, phantoms (36-byte raw values that look like
    // posting handles) and backlogs all happened.
    for (what, value) in [
        ("resurrections", totals.resurrected_total),
        ("deletions", totals.deleted_total),
        ("shared rows", totals.shared),
        ("phantom rows", totals.phantoms),
        ("pending releases", totals.pending),
    ] {
        assert!(value > 0, "no {what}: {totals:?}");
    }
}

#[test]
fn a_window_bounds_the_stored_objects() {
    let run = run(99, 160, 2, 4096);
    let pruned = run.pruned_store.gc_state().unwrap().unwrap();
    let archive_objects: u64 = run
        .archive_store
        .stats()
        .spaces
        .values()
        .map(|s| s.objects_written)
        .sum();
    assert!(pruned.deleted_total > 0, "{pruned:?}");
    assert!(
        pruned.stored_objects * 3 < archive_objects,
        "pruned keeps {} of {archive_objects}: {pruned:?}",
        pruned.stored_objects
    );
}

/// Count-only mode keeps counting and releasing, and deletes nothing.
#[test]
fn count_only_mode_deletes_nothing() {
    let store = Arc::new(MemoryVersionedStore::new());
    let mut engine = initialize(
        store.clone(),
        Options {
            gc_count_only: true,
            ..options(Retention::Window(1), 4096)
        },
    );
    engine.enable_gc(None).unwrap();
    for n in 0..30u64 {
        let mut branch = engine.begin(engine.head(), None).unwrap();
        if n == 0 {
            add_namespace(&mut branch, OWNER, "count");
        }
        branch
            .execute(
                OWNER,
                put(ns(1), b"key", &n.to_be_bytes()),
                Budget::unlimited(),
            )
            .unwrap();
        commit(&mut engine, branch.try_seal().unwrap());
        gc::verify(store.as_ref()).unwrap();
    }
    let state = store.gc_state().unwrap().unwrap();
    assert!(
        state.died_total > 20 && state.released_total > 20,
        "{state:?}"
    );
    assert_eq!(state.deleted_total, 0);
    assert_eq!(state.queued, state.dead_objects);
}

#[test]
fn archive_retention_is_refused_on_a_collecting_store() {
    let store = Arc::new(MemoryVersionedStore::new());
    let mut engine = initialize(store.clone(), options(Retention::Archive, 4096));
    assert!(engine.enable_gc(None).is_err());
    drop(engine);
    let mut engine = DurableEngine::open(
        store.clone(),
        genesis(),
        options(Retention::Window(3), 4096),
    )
    .unwrap();
    engine.enable_gc(None).unwrap();
    drop(engine);
    let error = DurableEngine::open(store.clone(), genesis(), options(Retention::Archive, 4096))
        .unwrap_err();
    assert!(error.to_string().contains("garbage collection"), "{error}");
    // A plain commit would add objects nobody counts.
    let batch = arkiv_db_storage::versioned::CommitBatch::new(
        store.head().unwrap(),
        store
            .head()
            .unwrap()
            .map(|h| arkiv_db_storage::versioned::Head {
                height: h.height + 1,
                hash: [9; 32],
            })
            .unwrap(),
    );
    assert!(matches!(
        store.commit(&batch),
        arkiv_db_storage::versioned::CommitOutcome::NotCommitted(_)
    ));
}
