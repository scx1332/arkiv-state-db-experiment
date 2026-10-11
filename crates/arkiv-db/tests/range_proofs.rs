//! Complete range results use an independent row model, never Engine::query.
use std::ops::Bound::{self, Excluded, Included, Unbounded};

use arkiv_db::{
    B256, Budget, Cell, CellName, Decoder, Encoder, Engine, Limits, NamespaceId, NamespaceName,
    OwnerId, PriceSchedule, RecordId, RecordKey, Row, Snapshot, StateHeader, Value, WriteOp,
    decode_exact, encode,
    proof::{
        ProofError, RangeProofLimits, RangeQuery, RangeRequest, SnapshotRef, VerifiedRange,
        verify_range, verify_range_inspected,
    },
};

fn ns(n: u64) -> NamespaceId {
    NamespaceId::from_ordinal(n).unwrap()
}
fn name(text: &str) -> CellName {
    CellName::new(text).unwrap()
}

fn fixture(values: Vec<(Value, bool)>) -> (Engine, Vec<Row>) {
    let mut engine = Engine::new(PriceSchedule::FIXTURE);
    let mut branch = engine.begin(engine.head(), None).unwrap();
    let owner = OwnerId::new([7; 20]);
    branch
        .execute(
            owner,
            WriteOp::CreateNamespace {
                name: NamespaceName::new("ranges").unwrap(),
            },
            Budget::unlimited(),
        )
        .unwrap();
    let mut rows = Vec::new();
    for (i, (value, attribute)) in values.into_iter().enumerate() {
        let id = RecordId::from_ordinal(i as u64 + 1).unwrap();
        let key = RecordKey::new(&(i as u64).to_be_bytes()).unwrap();
        let cells = vec![(
            name("score"),
            if attribute {
                Cell::attribute(value)
            } else {
                Cell::field(value)
            },
        )];
        branch
            .execute(
                owner,
                WriteOp::CreateRecord {
                    namespace: ns(1),
                    key: key.clone(),
                    cells: cells.clone(),
                },
                Budget::unlimited(),
            )
            .unwrap();
        rows.push(Row {
            id,
            key: Some(key),
            cells,
        });
    }
    engine.publish(&branch.seal()).unwrap();
    (engine, rows)
}
fn request(snapshot: &Snapshot, lower: Bound<Value>, upper: Bound<Value>) -> RangeRequest {
    RangeRequest {
        snapshot: SnapshotRef {
            genesis_hash: B256::repeat_byte(1),
            height: 7,
            block_hash: B256::repeat_byte(2),
            state_root: snapshot.state_root(),
        },
        query: RangeQuery {
            namespace: ns(1),
            attribute: name("score"),
            lower,
            upper,
        },
    }
}
fn proof(snapshot: &Snapshot, r: &RangeRequest) -> Vec<u8> {
    snapshot
        .prove_range(r, RangeProofLimits::default(), Budget::unlimited())
        .unwrap()
        .bytes
}
fn verified(r: &RangeRequest, bytes: &[u8]) -> Vec<Row> {
    let VerifiedRange::Complete(result) =
        verify_range(r, bytes, RangeProofLimits::default(), Limits::DEFAULT).unwrap()
    else {
        panic!("missing namespace")
    };
    assert_eq!(result.posting_count, result.rows.len());
    let (inspected, _) =
        verify_range_inspected(r, bytes, RangeProofLimits::default(), Limits::DEFAULT).unwrap();
    assert_eq!(inspected, VerifiedRange::Complete(result.clone()));
    result.rows
}
fn matches(value: &Value, lower: &Bound<Value>, upper: &Bound<Value>) -> bool {
    use std::cmp::Ordering::*;
    (match lower {
        Unbounded => true,
        Included(v) => matches!(value.cmp_ordered(v), Some(Equal | Greater)),
        Excluded(v) => value.cmp_ordered(v) == Some(Greater),
    }) && (match upper {
        Unbounded => true,
        Included(v) => matches!(value.cmp_ordered(v), Some(Equal | Less)),
        Excluded(v) => value.cmp_ordered(v) == Some(Less),
    })
}

#[test]
fn typed_ranges_are_complete_in_record_order_with_exact_boundaries() {
    let values = [11, 0, 9, 20, 10, 15, 11, u64::MAX];
    let mut input = values
        .into_iter()
        .map(|n| (Value::U64(n), true))
        .collect::<Vec<_>>();
    input.extend([
        (Value::U64(11), false),
        (Value::str("11"), true),
        (Value::I64(11), true),
    ]);
    let (engine, model) = fixture(input);
    let snapshot = engine.snapshot(engine.head()).unwrap();
    let bounds = [
        Unbounded,
        Included(Value::U64(0)),
        Excluded(Value::U64(0)),
        Included(Value::U64(10)),
        Excluded(Value::U64(11)),
        Included(Value::U64(12)),
        Included(Value::U64(20)),
        Excluded(Value::U64(u64::MAX)),
    ];
    for lower in &bounds {
        for upper in &bounds {
            if *lower == Unbounded && *upper == Unbounded {
                continue;
            }
            let r = request(&snapshot, lower.clone(), upper.clone());
            assert_eq!(decode_exact::<RangeRequest>(&encode(&r)).unwrap(), r);
            let expected = model
                .iter()
                .filter(|row| {
                    row.cells.iter().any(|(_, cell)| {
                        cell.is_attribute()
                            && cell.value.tag() == arkiv_db::TypeTag::U64
                            && matches(&cell.value, lower, upper)
                    })
                })
                .cloned()
                .collect::<Vec<_>>();
            assert_eq!(
                verified(&r, &proof(&snapshot, &r)),
                expected,
                "{lower:?}..{upper:?}"
            );
        }
    }
}

#[test]
fn signed_numeric_extremes_and_each_supported_numeric_index() {
    let mut groups = vec![
        vec![
            Value::I64(i64::MAX),
            Value::I64(-1),
            Value::I64(i64::MIN),
            Value::I64(0),
        ],
        vec![Value::U64(u64::MAX), Value::U64(1), Value::U64(0)],
    ];
    groups.push(vec![
        Value::I32(i32::MAX),
        Value::I32(-1),
        Value::I32(i32::MIN),
        Value::I32(0),
    ]);
    groups.push(vec![
        Value::U256([255; 32]),
        Value::U256([1; 32]),
        Value::U256([0; 32]),
    ]);
    let mut min = [0; 32];
    min[0] = 128;
    let mut max = [255; 32];
    max[0] = 127;
    groups.push(vec![
        Value::Dec(max),
        Value::Dec([255; 32]),
        Value::Dec(min),
        Value::Dec([0; 32]),
    ]);
    for values in groups {
        let (engine, model) = fixture(values.iter().cloned().map(|v| (v, true)).collect());
        let snapshot = engine.snapshot(engine.head()).unwrap();
        for value in values {
            for inclusive in [false, true] {
                let bound = if inclusive {
                    Included(value.clone())
                } else {
                    Excluded(value.clone())
                };
                for (lower, upper) in [
                    (bound.clone(), Unbounded),
                    (Unbounded, bound.clone()),
                    (bound.clone(), bound.clone()),
                ] {
                    let r = request(&snapshot, lower.clone(), upper.clone());
                    let expected = model
                        .iter()
                        .filter(|row| matches(&row.cells[0].1.value, &lower, &upper))
                        .cloned()
                        .collect::<Vec<_>>();
                    assert_eq!(verified(&r, &proof(&snapshot, &r)), expected);
                }
            }
        }
    }
}

#[test]
fn absent_namespace_attribute_type_and_empty_index_are_authenticated() {
    for input in [vec![], vec![(Value::U64(10), true), (Value::U64(20), true)]] {
        let (engine, _) = fixture(input);
        let snapshot = engine.snapshot(engine.head()).unwrap();
        let mut r = request(
            &snapshot,
            Included(Value::U64(12)),
            Included(Value::U64(18)),
        );
        assert!(verified(&r, &proof(&snapshot, &r)).is_empty());
        r.query.attribute = name("missing");
        assert!(verified(&r, &proof(&snapshot, &r)).is_empty());
        r.query.attribute = name("score");
        r.query.lower = Included(Value::I64(-2));
        r.query.upper = Unbounded;
        assert!(verified(&r, &proof(&snapshot, &r)).is_empty());
        r.query.namespace = ns(2);
        assert_eq!(
            verify_range(
                &r,
                &proof(&snapshot, &r),
                RangeProofLimits::default(),
                Limits::DEFAULT
            )
            .unwrap(),
            VerifiedRange::NamespaceAbsent
        );
    }
}

#[test]
fn unsupported_and_mixed_bounds_fail_before_reading() {
    let (engine, _) = fixture(vec![]);
    let snapshot = engine.snapshot(engine.head()).unwrap();
    for (lower, upper) in [
        (Unbounded, Unbounded),
        (Included(Value::U64(1)), Included(Value::I64(2))),
        (Included(Value::str("a")), Included(Value::Bool(true))),
        (Included(Value::bytes(b"a")), Unbounded),
        (
            Unbounded,
            Excluded(Value::str(
                &"x".repeat(arkiv_db::Limits::DEFAULT_INDEXED_LEN + 1),
            )),
        ),
    ] {
        let r = request(&snapshot, lower, upper);
        assert!(
            snapshot
                .prove_range(&r, RangeProofLimits::default(), Budget::unlimited())
                .is_err()
        );
        assert!(verify_range(&r, &[], RangeProofLimits::default(), Limits::DEFAULT).is_err());
    }
}

#[test]
fn every_limit_fails_closed_and_never_becomes_a_partial_page() {
    let (engine, _) = fixture((0..65).map(|n| (Value::U64(n), true)).collect());
    let snapshot = engine.snapshot(engine.head()).unwrap();
    let all = request(&snapshot, Included(Value::U64(0)), Unbounded);
    assert_eq!(
        snapshot
            .prove_range(
                &all,
                RangeProofLimits {
                    max_terms: 64,
                    max_rows: 64,
                    ..RangeProofLimits::default()
                },
                Budget::unlimited()
            )
            .unwrap_err(),
        ProofError::ProofLimitExceeded
    );
    let r = request(
        &snapshot,
        Included(Value::U64(10)),
        Included(Value::U64(12)),
    );
    let bytes = proof(&snapshot, &r);
    let mut limits = Vec::new();
    let default = RangeProofLimits::default();
    limits.push(RangeProofLimits {
        max_terms: 2,
        ..default
    });
    limits.push(RangeProofLimits {
        max_rows: 2,
        ..default
    });
    limits.push(RangeProofLimits {
        max_posting_ids: 2,
        ..default
    });
    limits.push(RangeProofLimits {
        max_proof_bytes: bytes.len() - 1,
        ..default
    });
    limits.push(RangeProofLimits {
        max_nodes: 0,
        ..default
    });
    limits.push(RangeProofLimits {
        max_allocated_bytes: 1,
        ..default
    });
    limits.push(RangeProofLimits {
        max_verification_work: 1,
        ..default
    });
    for caps in limits {
        assert!(
            snapshot.prove_range(&r, caps, Budget::unlimited()).is_err(),
            "{caps:?}"
        );
        assert!(
            verify_range(&r, &bytes, caps, Limits::DEFAULT).is_err(),
            "{caps:?}"
        );
    }
    assert_eq!(
        snapshot
            .prove_range(&r, default, Budget::limit(1))
            .unwrap_err(),
        ProofError::BudgetExceeded
    );
    // Neither max_page nor any continuation exists in this profile.
    let caps = RangeProofLimits {
        max_terms: 80,
        max_rows: 80,
        ..default
    };
    let bytes = snapshot
        .prove_range(&all, caps, Budget::unlimited())
        .unwrap()
        .bytes;
    assert!(
        verify_range(
            &all,
            &bytes,
            caps,
            Limits {
                max_page: 1,
                ..Limits::DEFAULT
            }
        )
        .is_ok()
    );
    let extreme = RangeProofLimits {
        max_nodes: usize::MAX,
        ..default
    };
    assert!(verify_range(&r, &proof(&snapshot, &r), extreme, Limits::DEFAULT).is_ok());
}

type RowWitness = (RecordId, Vec<Vec<u8>>, Vec<Vec<u8>>);
#[derive(Clone)]
struct Wire {
    version: u32,
    tag: u8,
    snapshot: SnapshotRef,
    digest: B256,
    header: StateHeader,
    catalog: Vec<Vec<u8>>,
    interval: Vec<Vec<u8>>,
    postings: Vec<Vec<RecordId>>,
    rows: Vec<RowWitness>,
}
impl Wire {
    fn read(bytes: &[u8], term_count: usize) -> Self {
        let mut d = Decoder::new(bytes);
        let mut wire = Self {
            version: d.u32().unwrap(),
            tag: d.u8().unwrap(),
            snapshot: d.get().unwrap(),
            digest: d.get().unwrap(),
            header: d.get().unwrap(),
            catalog: d.get().unwrap(),
            interval: d.get().unwrap(),
            postings: vec![],
            rows: vec![],
        };
        for _ in 0..term_count {
            wire.postings.push(d.get().unwrap());
        }
        let count = d.length().unwrap();
        for _ in 0..count {
            wire.rows
                .push((d.get().unwrap(), d.get().unwrap(), d.get().unwrap()));
        }
        d.finish().unwrap();
        wire
    }
    fn bytes(&self) -> Vec<u8> {
        let mut e = Encoder::new();
        e.u32(self.version);
        e.u8(self.tag);
        e.put(&self.snapshot);
        e.put(&self.digest);
        e.put(&self.header);
        e.put(&self.catalog);
        e.put(&self.interval);
        for posting in &self.postings {
            e.put(posting);
        }
        e.length(self.rows.len());
        for (id, row, key) in &self.rows {
            e.put(id);
            e.put(row);
            e.put(key);
        }
        e.into_bytes()
    }
}

#[test]
fn omitted_terms_postings_rows_and_tampered_bindings_cannot_verify() {
    let (engine, _) = fixture(
        [90, 10, 30, 20, 70, 30]
            .into_iter()
            .map(|n| (Value::U64(n), true))
            .collect(),
    );
    let snapshot = engine.snapshot(engine.head()).unwrap();
    let r = request(
        &snapshot,
        Included(Value::U64(15)),
        Included(Value::U64(75)),
    );
    let bytes = proof(&snapshot, &r);
    let wire = Wire::read(&bytes, 3);
    assert_eq!(wire.bytes(), bytes);
    let reject = |wire: Wire| {
        assert!(
            verify_range_inspected(
                &r,
                &wire.bytes(),
                RangeProofLimits::default(),
                Limits::DEFAULT
            )
            .is_err()
        );
        assert!(
            verify_range(
                &r,
                &wire.bytes(),
                RangeProofLimits::default(),
                Limits::DEFAULT
            )
            .is_err()
        )
    };
    for i in 0..wire.interval.len() {
        let mut w = wire.clone();
        w.interval.remove(i);
        reject(w);
    }
    for i in 0..wire.postings.len() {
        let mut w = wire.clone();
        w.postings[i].pop();
        reject(w);
    }
    for i in 0..wire.rows.len() {
        let mut w = wire.clone();
        w.rows.remove(i);
        reject(w);
    }
    let mut w = wire.clone();
    w.interval.push(w.interval[0].clone());
    reject(w);
    let mut w = wire.clone();
    w.postings[1].reverse();
    reject(w);
    let mut w = wire.clone();
    w.rows.swap(0, 1);
    reject(w);
    let mut w = wire.clone();
    // With unpadded 9-byte key encodings the key-binding leaves are inline,
    // so two bindings can share one path; then cut the path instead.
    if w.rows[0].2 == w.rows[1].2 {
        w.rows[0].2.pop();
    } else {
        w.rows[0].2 = w.rows[1].2.clone();
    }
    reject(w);
    let mut w = wire.clone();
    w.rows[0].1 = w.rows[1].1.clone();
    reject(w);
    let mut w = wire.clone();
    w.postings[1][0] = w.postings[0][0];
    reject(w);
    let mut w = wire.clone();
    w.header.catalog_len += 1;
    reject(w);
    let mut w = wire.clone();
    w.snapshot.height += 1;
    reject(w);
    let mut w = wire.clone();
    w.digest = B256::ZERO;
    reject(w);
    for end in 0..bytes.len() {
        assert!(
            verify_range(
                &r,
                &bytes[..end],
                RangeProofLimits::default(),
                Limits::DEFAULT
            )
            .is_err()
        );
    }
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(verify_range(&r, &trailing, RangeProofLimits::default(), Limits::DEFAULT).is_err());
    for index in (0..bytes.len()).step_by(31) {
        let mut changed = bytes.clone();
        changed[index] ^= 128;
        assert!(verify_range(&r, &changed, RangeProofLimits::default(), Limits::DEFAULT).is_err());
    }
    let mut changed = r.clone();
    changed.query.lower = Excluded(Value::U64(15));
    assert!(
        verify_range(
            &changed,
            &bytes,
            RangeProofLimits::default(),
            Limits::DEFAULT
        )
        .is_err()
    );
    changed = r.clone();
    changed.query.namespace = ns(2);
    assert!(
        verify_range(
            &changed,
            &bytes,
            RangeProofLimits::default(),
            Limits::DEFAULT
        )
        .is_err()
    );
    changed = r.clone();
    changed.snapshot.state_root = B256::ZERO;
    assert!(
        verify_range(
            &changed,
            &bytes,
            RangeProofLimits::default(),
            Limits::DEFAULT
        )
        .is_err()
    );
    // An empty gap still requires its boundary/interior-absence witness.
    let empty = request(
        &snapshot,
        Included(Value::U64(31)),
        Included(Value::U64(69)),
    );
    let mut w = Wire::read(&proof(&snapshot, &empty), 0);
    w.interval.clear();
    assert!(
        verify_range(
            &empty,
            &w.bytes(),
            RangeProofLimits::default(),
            Limits::DEFAULT
        )
        .is_err()
    );
}

#[test]
fn reopened_proof_reads_only_matching_index_region_and_selected_rows() {
    use arkiv_db::{MapKind, OperationClass as Op};
    use arkiv_db_storage::versioned::{ObjectId, ObjectReader, SpaceId, StorageError};
    use std::{
        collections::{BTreeMap, BTreeSet},
        sync::{Arc, Mutex},
    };
    struct Store(Mutex<BTreeMap<(SpaceId, ObjectId), Vec<u8>>>);
    impl ObjectReader for Store {
        fn read_object(
            &self,
            space: SpaceId,
            id: &ObjectId,
        ) -> Result<Option<Vec<u8>>, StorageError> {
            Ok(self.0.lock().unwrap().get(&(space, *id)).cloned())
        }

        fn read_object_bounded(
            &self,
            space: SpaceId,
            id: &ObjectId,
            _max_bytes: usize,
        ) -> Result<Option<Vec<u8>>, StorageError> {
            self.read_object(space, id)
        }
    }
    let (engine, _) = fixture((0..300).map(|n| (Value::U64(n * 100), true)).collect());
    let original = engine.head_snapshot();
    let r = request(
        &original,
        Included(Value::U64(12300)),
        Included(Value::U64(12500)),
    );
    let generated = original
        .prove_range(
            &r,
            RangeProofLimits::default(),
            Budget::unlimited().traced(),
        )
        .unwrap();
    let ledger = generated.receipt.ledger.unwrap();
    let count = |class| {
        ledger
            .entries
            .iter()
            .find(|e| e.class == class)
            .map_or(0, |e| e.count)
    };
    assert_eq!(count(Op::RecordProbe), 3);
    assert_eq!(count(Op::PostingEntry), 3);
    assert_eq!(count(Op::TermVisit), 3);
    assert_eq!(count(Op::ProofByte), generated.bytes.len() as u64);
    let wire = Wire::read(&generated.bytes, 3);
    let needed_terms = wire
        .interval
        .iter()
        .map(alloy_primitives::keccak256)
        .collect::<BTreeSet<_>>();
    let needed_rows = wire
        .rows
        .iter()
        .flat_map(|(_, path, _)| path)
        .map(alloy_primitives::keccak256)
        .collect::<BTreeSet<_>>();
    let mut removed = 0;
    let objects = original
        .staged_objects()
        .into_iter()
        .filter_map(|o| {
            if (o.map == MapKind::Terms && !needed_terms.contains(&o.id))
                || (o.map == MapKind::Rows && !needed_rows.contains(&o.id))
            {
                removed += 1;
                None
            } else {
                Some(((o.space, o.id), o.bytes))
            }
        })
        .collect();
    assert!(
        removed > 500,
        "unrelated term and row leaves must be unavailable"
    );
    let store = Arc::new(Store(Mutex::new(objects)));
    let reopened =
        Engine::from_state(&original.state_header(), Limits::DEFAULT, store.clone()).unwrap();
    let snapshot = reopened.head_snapshot();
    assert_eq!(proof(&snapshot, &r), generated.bytes);
    let required = *needed_terms.last().unwrap();
    store
        .0
        .lock()
        .unwrap()
        .remove(&(SpaceId::Namespace(1), required.0));
    assert_eq!(
        snapshot
            .prove_range(&r, RangeProofLimits::default(), Budget::unlimited())
            .unwrap_err(),
        ProofError::StorageUnavailable
    );
}

#[test]
fn inspected_range_connects_exact_boundary_nodes_postings_and_rows() {
    let (engine, _) = fixture(
        [9, 10, 15, 20, 21]
            .into_iter()
            .map(|n| (Value::U64(n), true))
            .collect(),
    );
    let snapshot = engine.snapshot(engine.head()).unwrap();
    let r = request(
        &snapshot,
        Included(Value::U64(10)),
        Excluded(Value::U64(20)),
    );
    let bytes = proof(&snapshot, &r);
    let (_, inspection) =
        verify_range_inspected(&r, &bytes, RangeProofLimits::default(), Limits::DEFAULT).unwrap();
    assert_eq!(inspection.state_header.root(), r.snapshot.state_root);
    let roots = inspection.namespace.unwrap();
    let interval = inspection.interval.unwrap();
    assert_eq!(interval.root, roots.terms.0);
    let leaves = interval.leaves.iter().flatten().collect::<Vec<_>>();
    // The excluded endpoint 20 is never included. Its leaf is not even
    // opened: exclusive endpoints compare whole keys, so the subtree whose
    // smallest key is the endpoint itself is outside (the old fixed-width
    // predecessor arithmetic had to open it).
    assert!(
        !leaves
            .iter()
            .any(|l| l.value == Some(Value::U64(20)) && l.included)
    );
    assert!(
        !leaves
            .iter()
            .any(|l| l.value == Some(Value::U64(9)) || l.value == Some(Value::U64(21)))
    );
    assert_eq!(leaves.iter().filter(|l| l.included).count(), 2);
    assert_eq!(inspection.posting_sets.len(), 2);
    for (term, expected) in inspection.posting_sets.iter().zip([2, 3]) {
        let leaf = interval.leaves[term.term_node_index].as_ref().unwrap();
        assert_eq!(leaf.value.as_ref(), Some(&term.value));
        assert_eq!(leaf.key, term.term_key);
        assert_eq!(Some(leaf.posting_root), term.posting.authenticated_root);
        assert_eq!(leaf.posting_root, term.posting.reconstructed_root);
        assert_eq!(
            term.posting.ids,
            vec![RecordId::from_ordinal(expected).unwrap()]
        );
        assert_eq!(term.posting.ids, term.posting.selected_ids);
        for map in [arkiv_db::MapKind::Rows, arkiv_db::MapKind::Keys] {
            let path = inspection
                .point_paths
                .iter()
                .find(|p| p.map == map && p.record_id == Some(term.posting.ids[0]))
                .unwrap();
            assert_eq!(
                path.root,
                if map == arkiv_db::MapKind::Rows {
                    roots.rows.0
                } else {
                    roots.keys.0
                }
            );
            assert_eq!(path.trace.terminal, arkiv_db_trie::PathTerminal::Inclusion);
        }
    }
    let wire = Wire::read(&bytes, 2);
    assert_eq!(interval.supplied_nodes, wire.interval.len());
    for node in &interval.trace.nodes {
        if node.path_node.source != arkiv_db_trie::PathSource::Inline {
            assert_eq!(
                node.path_node.rlp,
                wire.interval[node.path_node.proof_index]
            );
        }
    }
    for caps in [
        RangeProofLimits {
            max_proof_bytes: bytes.len() - 1,
            ..RangeProofLimits::default()
        },
        RangeProofLimits {
            max_allocated_bytes: 1,
            ..RangeProofLimits::default()
        },
        RangeProofLimits {
            max_verification_work: 1,
            ..RangeProofLimits::default()
        },
    ] {
        assert!(verify_range_inspected(&r, &bytes, caps, Limits::DEFAULT).is_err());
    }
    // Inspection consumes additional metered resources: find a budget just
    // sufficient for ordinary verification and ensure tracing cannot bypass it.
    let mut low = 0;
    let mut high = RangeProofLimits::default().max_allocated_bytes;
    while low < high {
        let middle = low + (high - low) / 2;
        let caps = RangeProofLimits {
            max_allocated_bytes: middle,
            ..RangeProofLimits::default()
        };
        if verify_range(&r, &bytes, caps, Limits::DEFAULT).is_ok() {
            high = middle;
        } else {
            low = middle + 1;
        }
    }
    let caps = RangeProofLimits {
        max_allocated_bytes: low,
        ..RangeProofLimits::default()
    };
    assert!(verify_range(&r, &bytes, caps, Limits::DEFAULT).is_ok());
    assert_eq!(
        verify_range_inspected(&r, &bytes, caps, Limits::DEFAULT).unwrap_err(),
        ProofError::ProofLimitExceeded
    );
    // Empty and reversed ranges retain authenticated structure where required.
    for (lower, upper) in [
        (Excluded(Value::U64(10)), Excluded(Value::U64(15))),
        (Included(Value::U64(20)), Included(Value::U64(10))),
        (Unbounded, Excluded(Value::U64(9))),
        (Excluded(Value::U64(21)), Unbounded),
    ] {
        let empty = request(&snapshot, lower, upper);
        let bytes = proof(&snapshot, &empty);
        let (result, trace) =
            verify_range_inspected(&empty, &bytes, RangeProofLimits::default(), Limits::DEFAULT)
                .unwrap();
        assert!(matches!(result,VerifiedRange::Complete(ref r) if r.rows.is_empty()));
        assert!(trace.posting_sets.is_empty());
        assert!(
            trace
                .interval
                .unwrap()
                .leaves
                .iter()
                .flatten()
                .all(|l| !l.included)
        );
    }
}

#[test]
fn thousand_row_ranges_are_complete_and_overflow_fails_closed() {
    for distinct in [false, true] {
        let (engine, expected) = fixture(
            (0..1_001)
                .map(|n| (Value::U64(if distinct { n } else { 7 }), true))
                .collect(),
        );
        let snapshot = engine.snapshot(engine.head()).unwrap();
        let all = request(&snapshot, Included(Value::U64(0)), Unbounded);
        let caps = RangeProofLimits::default();
        assert_eq!(caps.max_rows, 1_000);
        assert_eq!(caps.max_terms, 1_000);
        assert_eq!(
            snapshot
                .prove_range(&all, caps, Budget::unlimited())
                .unwrap_err(),
            ProofError::ProofLimitExceeded
        );
        let (engine, expected) = if distinct {
            (engine, expected)
        } else {
            fixture(vec![(Value::U64(7), true); 1_000])
        };
        let snapshot = engine.snapshot(engine.head()).unwrap();
        let query = request(
            &snapshot,
            Included(Value::U64(0)),
            Included(Value::U64(999)),
        );
        let started = std::time::Instant::now();
        let bytes = snapshot
            .prove_range(
                &query,
                RangeProofLimits {
                    max_proof_bytes: (8 * 1024 * 1024 - 4096) / 2,
                    ..caps
                },
                Budget::limit(10_000_000),
            )
            .unwrap()
            .bytes;
        let generation = started.elapsed();
        let started = std::time::Instant::now();
        let VerifiedRange::Complete(result) =
            verify_range(&query, &bytes, caps, Limits::DEFAULT).unwrap()
        else {
            panic!("missing namespace")
        };
        let rows = result.rows;
        assert_eq!(result.term_count, if distinct { 1_000 } else { 1 });
        eprintln!(
            "1000 rows distinct={distinct}: {} bytes; generation={generation:?}; verification={:?}",
            bytes.len(),
            started.elapsed()
        );
        assert_eq!(rows, expected[..1_000]);
        assert!(
            verify_range(
                &query,
                &bytes,
                RangeProofLimits {
                    max_rows: 999,
                    ..caps
                },
                Limits::DEFAULT
            )
            .is_err()
        );
        if distinct {
            assert!(
                verify_range(
                    &query,
                    &bytes,
                    RangeProofLimits {
                        max_terms: 999,
                        ..caps
                    },
                    Limits::DEFAULT
                )
                .is_err()
            );
        }
    }
}

/// Explicit experiment outside the hosted transport limits. Never runs in CI.
#[test]
#[ignore = "opt-in scaling experiment: RANGE_BENCH_ROWS=10000 cargo test --release -p arkiv-db --test range_proofs range_scaling_experiment -- --ignored --nocapture"]
fn range_scaling_experiment() {
    let count: usize = std::env::var("RANGE_BENCH_ROWS")
        .unwrap_or_else(|_| "10000".into())
        .parse()
        .unwrap();
    assert!(count > 0);
    for distinct in [false, true] {
        let (engine, expected) = fixture(
            (0..count)
                .map(|n| (Value::U64(if distinct { n as u64 } else { 7 }), true))
                .collect(),
        );
        let snapshot = engine.snapshot(engine.head()).unwrap();
        let query = request(&snapshot, Included(Value::U64(0)), Unbounded);
        let caps = RangeProofLimits {
            max_rows: count,
            max_terms: count,
            max_posting_ids: count,
            max_nodes: count.checked_mul(100).unwrap(),
            max_proof_bytes: count.checked_mul(16_384).unwrap(),
            max_allocated_bytes: count.checked_mul(131_072).unwrap(),
            max_verification_work: (count as u64).checked_mul(2_000_000).unwrap(),
        };
        let started = std::time::Instant::now();
        let artifact = snapshot
            .prove_range(&query, caps, Budget::unlimited())
            .unwrap();
        let generation = started.elapsed();
        let started = std::time::Instant::now();
        let VerifiedRange::Complete(result) =
            verify_range(&query, &artifact.bytes, caps, Limits::DEFAULT).unwrap()
        else {
            panic!("missing namespace")
        };
        let verification = started.elapsed();
        assert_eq!(result.rows, expected);
        eprintln!(
            "rows={count} distinct={distinct} bytes={} generation={generation:?} verification={verification:?}",
            artifact.bytes.len()
        );
    }
}

/// Every golem-grid ordered type proves complete ranges in value order, in
/// both profiles; other types in the same attribute never leak in.
#[test]
fn golem_grid_ordered_types_prove_complete_ranges() {
    let f64v = |x: f64| Value::f64(x).unwrap();
    let f32v = |x: f32| Value::f32(x).unwrap();
    let mut neg = [0xff; 32];
    neg[31] = 0xfe;
    let families: Vec<Vec<Value>> = vec![
        [f64::NEG_INFINITY, -2.5, -0.0, 1e-300, 3.0, f64::INFINITY]
            .map(f64v)
            .to_vec(),
        [f32::MIN, -1.0, 0.0, 0.5, f32::MAX].map(f32v).to_vec(),
        [i32::MIN, -1, 0, 7, i32::MAX].map(Value::Dec32).to_vec(),
        [-5, 0, 5].map(Value::Dec64).to_vec(),
        [i128::MIN, 0, i128::MAX].map(Value::Dec128).to_vec(),
        [i128::MIN, -1, 1].map(Value::I128).to_vec(),
        [neg, [0; 32], [0x7f; 32]].map(Value::I256).to_vec(),
        [0, 9, u32::MAX].map(Value::U32).to_vec(),
        [0, 9, u128::MAX].map(Value::U128).to_vec(),
        [-719_162, 0, 20_000].map(Value::Date32).to_vec(),
        [i64::MIN, 0, 1_700_000_000_000_000]
            .map(Value::Timestamp64)
            .to_vec(),
    ];
    let mut input: Vec<(Value, bool)> = families
        .iter()
        .flatten()
        .map(|value| (value.clone(), true))
        .collect();
    input.push((Value::I64(0), true));
    input.push((Value::Bytes4([0; 4]), true));
    let (engine, rows) = fixture(input);
    let snapshot = engine.head_snapshot();
    for family in &families {
        let lower = Included(family[1].clone());
        let upper = Excluded(family[family.len() - 1].clone());
        let r = request(&snapshot, lower.clone(), upper.clone());
        let expected: Vec<Row> = rows
            .iter()
            .filter(|row| matches(&row.cells[0].1.value, &lower, &upper))
            .cloned()
            .collect();
        assert_eq!(expected.len(), family.len() - 2, "{:?}", family[1]);
        assert_eq!(verified(&r, &proof(&snapshot, &r)), expected);
    }
}
