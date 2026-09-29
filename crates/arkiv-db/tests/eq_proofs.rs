//! Complete Eq proofs against independent row and Patricia-trie oracles.
//! No expected answer is obtained from Engine::query or the proof producer.
#[path = "../../arkiv-db-trie/tests/support/mod.rs"]
mod trie_support;

use arkiv_db::{
    B256, Budget, Cell, CellName, Decode, Decoder, Effect, Encoder, Engine, Limits, NamespaceId,
    NamespaceName, OperationClass as Op, OwnerId, PriceSchedule, RecordId, RecordKey, Row,
    Snapshot, StateHeader, Value, WriteOp,
    proof::{
        EqCursor, EqQuery, EqRequest, ProofError, ProofLimits, SnapshotRef, VerifiedContinuation,
        VerifiedEq, verify_eq,
    },
};
use std::collections::BTreeMap;
use trie_support::{hash_builder_root, reference};

const OWNER: OwnerId = OwnerId::new([7; 20]);
fn ns(n: u64) -> NamespaceId {
    NamespaceId::from_ordinal(n).unwrap()
}
fn id(n: u64) -> RecordId {
    RecordId::from_ordinal(n).unwrap()
}
fn name(s: &str) -> CellName {
    CellName::new(s).unwrap()
}
fn key(n: u64) -> RecordKey {
    RecordKey::new(&n.to_be_bytes()).unwrap()
}
fn request(snapshot: &Snapshot, value: Value, limit: u32) -> EqRequest {
    EqRequest {
        snapshot: SnapshotRef {
            genesis_hash: B256::repeat_byte(0x11),
            height: snapshot.id().sequence(),
            block_hash: B256::repeat_byte(0x22),
            state_root: snapshot.state_root(),
        },
        query: EqQuery {
            namespace: ns(1),
            attribute: name("kind"),
            value,
        },
        limit,
        continuation: None,
    }
}
fn produce(snapshot: &Snapshot, request: &EqRequest) -> Vec<u8> {
    snapshot
        .prove_eq(request, ProofLimits::default(), Budget::unlimited())
        .unwrap()
        .bytes
}
fn verify(
    request: &EqRequest,
    previous: Option<&VerifiedContinuation>,
    bytes: &[u8],
) -> Result<VerifiedEq, ProofError> {
    let result = verify_eq(
        request,
        previous,
        bytes,
        ProofLimits::default(),
        Limits::DEFAULT,
    );
    // Run every honest and adversarial test through the inspecting entrypoint
    // as well: partial point traces must never escape a failed Eq witness.
    let inspected = arkiv_db::proof::verify_eq_inspected(
        request,
        previous,
        bytes,
        ProofLimits::default(),
        Limits::DEFAULT,
    );
    assert_eq!(result, inspected.map(|(result, _)| result));
    result
}
fn cells(value: Value) -> Vec<(CellName, Cell)> {
    vec![
        (name("kind"), Cell::attribute(value)),
        (
            name("secret"),
            Cell::field(Value::bytes(b"all cells included")),
        ),
    ]
}
fn fixture(count: u64) -> (Engine, Vec<Row>) {
    let mut engine = Engine::new(PriceSchedule::FIXTURE);
    let mut branch = engine.begin(engine.head(), None).unwrap();
    for _ in 0..2 {
        branch
            .execute(
                OWNER,
                WriteOp::CreateNamespace {
                    name: NamespaceName::new("proof").unwrap(),
                },
                Budget::unlimited(),
            )
            .unwrap();
    }
    let mut model = Vec::new();
    for ordinal in 1..=count {
        let value = if ordinal % 4 == 0 {
            Value::U64(7)
        } else {
            Value::str("book")
        };
        let cells = cells(value);
        let key = key(ordinal);
        let effect = branch
            .execute(
                OWNER,
                WriteOp::CreateRecord {
                    namespace: ns(1),
                    key: key.clone(),
                    cells: cells.clone(),
                },
                Budget::unlimited(),
            )
            .unwrap()
            .effect;
        assert_eq!(effect, Effect::RecordCreated { id: id(ordinal) });
        model.push(Row {
            id: id(ordinal),
            key: Some(key),
            cells,
        });
    }
    engine.publish(&branch.seal()).unwrap();
    (engine, model)
}
fn expected(model: &[Row], query: &EqQuery) -> Vec<Row> {
    model
        .iter()
        .filter(|row| {
            row.cells.iter().any(|(name, cell)| {
                name == &query.attribute && cell.is_attribute() && cell.value == query.value
            })
        })
        .cloned()
        .collect()
}
fn walk(snapshot: &Snapshot, mut request: EqRequest, sizes: &[u32]) -> Vec<Row> {
    let mut all = Vec::new();
    let mut previous = None;
    let mut index = 0;
    loop {
        request.limit = sizes[index % sizes.len()];
        let bytes = produce(snapshot, &request);
        let VerifiedEq::Page(page) = verify(&request, previous.as_ref(), &bytes).unwrap() else {
            panic!("namespace exists");
        };
        all.extend(page.rows);
        previous = page.next;
        request.continuation = previous.as_ref().map(|p| p.cursor().clone());
        index += 1;
        if previous.is_none() {
            return all;
        }
    }
}

type RowWitness = (RecordId, Vec<Vec<u8>>, Vec<Vec<u8>>);

#[derive(Clone)]
struct Wire {
    snapshot: SnapshotRef,
    digest: B256,
    tag: u8,
    header: StateHeader,
    catalog: Vec<Vec<u8>>,
    term: Vec<Vec<u8>>,
    ids: Vec<RecordId>,
    rows: Vec<RowWitness>,
    next: Option<EqCursor>,
}
impl Wire {
    fn parse(bytes: &[u8]) -> Self {
        let mut d = Decoder::new(bytes);
        assert_eq!(
            d.u32().unwrap(),
            if cfg!(feature = "arkiv-v2") { 2 } else { 1 }
        );
        let snapshot = d.get().unwrap();
        let digest = d.get().unwrap();
        let tag = d.u8().unwrap();
        let header = d.get().unwrap();
        let catalog = d.get().unwrap();
        let (term, ids, rows, next) = if tag == 2 {
            let term = d.get().unwrap();
            let ids = d.get().unwrap();
            let count = d.length().unwrap();
            let rows = (0..count)
                .map(|_| (d.get().unwrap(), d.get().unwrap(), d.get().unwrap()))
                .collect();
            (term, ids, rows, d.get().unwrap())
        } else {
            (vec![], vec![], vec![], None)
        };
        d.finish().unwrap();
        Self {
            snapshot,
            digest,
            tag,
            header,
            catalog,
            term,
            ids,
            rows,
            next,
        }
    }
    fn bytes(&self) -> Vec<u8> {
        let mut e = Encoder::new();
        e.u32(if cfg!(feature = "arkiv-v2") { 2 } else { 1 });
        e.put(&self.snapshot);
        e.put(&self.digest);
        e.u8(self.tag);
        e.put(&self.header);
        e.put(&self.catalog);
        if self.tag == 2 {
            e.put(&self.term);
            e.put(&self.ids);
            e.length(self.rows.len());
            for (id, row, binding) in &self.rows {
                e.put(id);
                e.put(row);
                e.put(binding);
            }
            e.put(&self.next);
        }
        e.into_bytes()
    }
}

#[test]
fn pages_match_independent_scan_and_posting_root_with_changing_sizes() {
    let (engine, model) = fixture(93);
    let snapshot = engine.head_snapshot();
    for value in [
        Value::str("book"),
        Value::U64(7),
        Value::Bool(false),
        Value::I64(7),
        Value::str(""),
    ] {
        let request = request(&snapshot, value, 3);
        let expected = expected(&model, &request.query);
        for sizes in [&[1][..], &[3][..], &[64][..], &[1, 3, 64][..]] {
            assert_eq!(walk(&snapshot, request.clone(), sizes), expected);
        }
        let wire = Wire::parse(&produce(&snapshot, &request));
        let wanted: Vec<_> = expected.iter().map(|row| row.id).collect();
        assert_eq!(wire.ids, wanted);
        if !wanted.is_empty() {
            let leaves = wanted
                .iter()
                .map(|id| (id.ordinal().to_be_bytes().to_vec(), vec![1]))
                .collect();
            let posting_root = hash_builder_root(&leaves);
            assert_eq!(posting_root, reference::root(&leaves));
            // An independent 88-byte term key; no layout::TermKey reuse.
            let mut term_key = vec![0; CellName::MAX_LEN + Value::MAX_INDEXED_LEN + 4];
            term_key[..4].copy_from_slice(b"kind");
            term_key[CellName::MAX_LEN] = 4;
            let (tag, value) = match &request.query.value {
                Value::Str(s) => (4, s.as_bytes().to_vec()),
                Value::U64(n) => (3, n.to_be_bytes().to_vec()),
                _ => unreachable!(),
            };
            term_key[CellName::MAX_LEN + 1] = tag;
            term_key[CellName::MAX_LEN + 2] = 1;
            term_key[CellName::MAX_LEN + 3..CellName::MAX_LEN + 3 + value.len()]
                .copy_from_slice(&value);
            term_key[CellName::MAX_LEN + Value::MAX_INDEXED_LEN + 3] = value.len() as u8;
            let roots = snapshot.try_namespace_roots(ns(1)).unwrap().unwrap();
            let leaf = arkiv_db_trie::walk(roots.terms.0, &term_key, &wire.term)
                .unwrap()
                .unwrap();
            assert_eq!(&leaf[..32], posting_root.as_slice());
            assert_eq!(&leaf[32..], &(wanted.len() as u64).to_be_bytes());
        }
    }
}

#[test]
fn namespace_absence_term_absence_and_genesis_are_distinct() {
    let engine = Engine::new(PriceSchedule::FIXTURE);
    let genesis = engine.head_snapshot();
    let req = request(&genesis, Value::Bool(true), 1);
    assert_eq!(
        verify(&req, None, &produce(&genesis, &req)),
        Ok(VerifiedEq::NamespaceAbsent)
    );
    let (engine, _) = fixture(0);
    let snapshot = engine.head_snapshot();
    let mut req = request(&snapshot, Value::Bool(true), 1);
    let VerifiedEq::Page(page) = verify(&req, None, &produce(&snapshot, &req)).unwrap() else {
        panic!()
    };
    assert!(page.rows.is_empty());
    assert!(page.next.is_none());
    req.query.namespace = ns(999);
    assert_eq!(
        verify(&req, None, &produce(&snapshot, &req)),
        Ok(VerifiedEq::NamespaceAbsent)
    );
}

#[test]
fn forged_cursors_cannot_skip_first_page_or_change_identity() {
    let (engine, _) = fixture(9);
    let snapshot = engine.head_snapshot();
    let mut req = request(&snapshot, Value::str("book"), 1);
    let VerifiedEq::Page(page) = verify(&req, None, &produce(&snapshot, &req)).unwrap() else {
        panic!()
    };
    let previous = page.next.unwrap();
    req.continuation = Some(previous.cursor().clone());
    let bytes = produce(&snapshot, &req);
    assert_eq!(verify(&req, None, &bytes), Err(ProofError::CursorMismatch));
    assert!(verify(&req, Some(&previous), &bytes).is_ok());
    for mutation in 0..6 {
        let mut bad = req.clone();
        match mutation {
            0 => bad.continuation.as_mut().unwrap().last_id = id(3),
            1 => bad.query.namespace = ns(2),
            2 => bad.query.value = Value::U64(7),
            3 => bad.snapshot.height += 1,
            4 => bad.snapshot.block_hash = B256::ZERO,
            _ => bad.snapshot.state_root = B256::ZERO,
        }
        assert_eq!(
            verify(&bad, Some(&previous), &bytes),
            Err(ProofError::CursorMismatch)
        );
    }
    req.continuation = None;
    assert_eq!(
        verify(&req, Some(&previous), &bytes),
        Err(ProofError::CursorMismatch)
    );
}

#[test]
fn fixed_root_rejects_omissions_membership_only_pages_and_all_witness_mutations() {
    let (engine, _) = fixture(9);
    let snapshot = engine.head_snapshot();
    let req = request(&snapshot, Value::str("book"), 3);
    let original = Wire::parse(&produce(&snapshot, &req));
    for mutation in 0..20 {
        let mut wire = original.clone();
        match mutation {
            0 => {
                wire.ids.pop();
            } // valid row membership does not prove completeness
            1 => wire.ids.swap(0, 1),
            2 => wire.ids[1] = wire.ids[0],
            3 => wire.ids[0] = id(4), // nonmember replacing valid id
            4 => {
                wire.rows.remove(1);
            }
            5 => wire.rows.swap(0, 1),
            6 => wire.rows[1] = wire.rows[0].clone(),
            7 => wire.rows[0].0 = id(4),
            8 => wire.rows[0].1.clear(),
            9 => wire.rows[0].2.clear(),
            10 => wire.term.clear(),
            11 => wire.catalog.clear(),
            12 => wire.header.next_namespace += 1,
            13 => wire.next = None, // false terminal claim
            14 => wire.next.as_mut().unwrap().last_id = id(7),
            15 => {
                let node = wire.rows[0].1[0].clone();
                wire.rows[0].1.push(node);
            }
            16 => wire.rows[0].1[0][0] ^= 1,
            17 => wire.rows[0].2[0][0] ^= 1,
            18 => wire.digest = B256::ZERO,
            _ => wire.tag = 1,
        }
        assert!(
            verify(&req, None, &wire.bytes()).is_err(),
            "accepted mutation {mutation}"
        );
    }
    let bytes = original.bytes();
    for end in 0..bytes.len() {
        assert!(
            verify(&req, None, &bytes[..end]).is_err(),
            "accepted prefix {end}"
        );
    }
    let mut extra = bytes.clone();
    extra.push(0);
    assert!(verify(&req, None, &extra).is_err());
    let mut extra_node = original.clone();
    extra_node.catalog.push(vec![0x80]);
    assert!(verify(&req, None, &extra_node.bytes()).is_err());
    for byte in [0, 3, 7, 50, bytes.len() - 1] {
        let mut bad = bytes.clone();
        bad[byte] ^= 0xff;
        assert!(verify(&req, None, &bad).is_err());
    }
}

#[test]
fn history_retains_pages_across_patch_delete_recreate_rollback_and_new_namespaces() {
    let (mut engine, mut model) = fixture(18);
    let old = engine.head_snapshot();
    let old_model = model.clone();
    let old_req = request(&old, Value::str("book"), 3);
    let old_bytes = produce(&old, &old_req);
    let mut branch = engine.begin(engine.head(), None).unwrap();
    let mark = branch.checkpoint();
    branch
        .execute(
            OWNER,
            WriteOp::DeleteRecord {
                namespace: ns(1),
                key: key(1),
            },
            Budget::unlimited(),
        )
        .unwrap();
    branch.rollback_to(mark).unwrap();
    branch
        .execute(
            OWNER,
            WriteOp::PatchRecord {
                namespace: ns(1),
                key: key(2),
                set: vec![(name("kind"), Cell::field(Value::str("book")))],
                remove: vec![],
            },
            Budget::unlimited(),
        )
        .unwrap();
    model[1].cells[0].1 = Cell::field(Value::str("book"));
    branch
        .execute(
            OWNER,
            WriteOp::DeleteRecord {
                namespace: ns(1),
                key: key(3),
            },
            Budget::unlimited(),
        )
        .unwrap();
    model.retain(|row| row.id != id(3));
    branch
        .execute(
            OWNER,
            WriteOp::CreateRecord {
                namespace: ns(1),
                key: key(3),
                cells: cells(Value::str("book")),
            },
            Budget::unlimited(),
        )
        .unwrap();
    model.push(Row {
        id: id(19),
        key: Some(key(3)),
        cells: cells(Value::str("book")),
    });
    for i in 0..100 {
        branch
            .execute(
                OWNER,
                WriteOp::CreateRecord {
                    namespace: ns(2),
                    key: key(i),
                    cells: cells(Value::Bool(true)),
                },
                Budget::unlimited(),
            )
            .unwrap();
    }
    engine.publish(&branch.seal()).unwrap();
    let latest = engine.head_snapshot();
    assert_eq!(produce(&old, &old_req), old_bytes);
    assert_eq!(
        walk(&old, old_req.clone(), &[1, 3, 64]),
        expected(&old_model, &old_req.query)
    );
    let req = request(&latest, Value::str("book"), 3);
    assert_eq!(
        walk(&latest, req.clone(), &[3]),
        expected(&model, &req.query)
    );
}

#[test]
fn resource_caps_reject_before_large_allocation_and_never_return_partial_rows() {
    let (engine, _) = fixture(9);
    let snapshot = engine.head_snapshot();
    let req = request(&snapshot, Value::str("book"), 3);
    let bytes = produce(&snapshot, &req);
    for caps in [
        ProofLimits {
            max_proof_bytes: bytes.len() - 1,
            ..ProofLimits::default()
        },
        ProofLimits {
            max_posting_ids: 6,
            ..ProofLimits::default()
        },
        ProofLimits {
            max_nodes: 0,
            ..ProofLimits::default()
        },
        ProofLimits {
            max_allocated_bytes: 8,
            ..ProofLimits::default()
        },
    ] {
        assert_eq!(
            verify_eq(&req, None, &bytes, caps, Limits::DEFAULT),
            Err(ProofError::ProofLimitExceeded)
        );
    }
    assert_eq!(
        verify_eq(
            &req,
            None,
            &bytes,
            ProofLimits {
                max_verification_work: 1,
                ..ProofLimits::default()
            },
            Limits::DEFAULT
        ),
        Err(ProofError::BudgetExceeded)
    );
    assert_eq!(
        snapshot
            .prove_eq(&req, ProofLimits::default(), Budget::limit(1))
            .unwrap_err(),
        ProofError::BudgetExceeded
    );
    assert_eq!(
        snapshot
            .prove_eq(
                &req,
                ProofLimits {
                    max_posting_ids: 6,
                    ..ProofLimits::default()
                },
                Budget::unlimited()
            )
            .unwrap_err(),
        ProofError::ProofLimitExceeded
    );
    for limit in [0, 65, u32::MAX] {
        let mut bad = req.clone();
        bad.limit = limit;
        assert_eq!(verify(&bad, None, &bytes), Err(ProofError::InvalidRequest));
    }
    let mut bad = req.clone();
    bad.query.value = Value::bytes(b"x");
    assert_eq!(
        verify(&bad, None, &bytes),
        Err(ProofError::UnsupportedProof)
    );
    // A huge path count with almost no payload cannot allocate its vector.
    let mut bad = bytes.clone();
    let count_at = 4 + 108 + 32 + 1 + StateHeader::MIN_ENCODED_LEN;
    bad[count_at..count_at + 4].copy_from_slice(&u32::MAX.to_be_bytes());
    assert_eq!(
        verify(&req, None, &bad),
        Err(ProofError::ProofLimitExceeded)
    );
    let mut bad = bytes.clone();
    bad[..4].copy_from_slice(&999u32.to_be_bytes());
    assert_eq!(
        verify(&req, None, &bad),
        Err(ProofError::UnsupportedVersion)
    );
}

#[test]
#[cfg(not(feature = "arkiv-v2"))]
fn codecs_have_literal_version_one_vectors_and_reject_unknown_tags() {
    let query = EqQuery {
        namespace: ns(1),
        attribute: name("a"),
        value: Value::I64(-2),
    };
    let expected = [
        0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 1, b'a', 2, 255, 255, 255, 255, 255, 255, 255,
        254, 1, 1,
    ];
    assert_eq!(encode(&query), expected);
    assert_eq!(decode_exact::<EqQuery>(&expected).unwrap(), query);
    let snapshot = SnapshotRef {
        genesis_hash: B256::repeat_byte(0x11),
        height: 9,
        block_hash: B256::repeat_byte(0x22),
        state_root: B256::repeat_byte(0x33),
    };
    let mut expected_snapshot = vec![0, 0, 0, 1];
    expected_snapshot.extend([0x11; 32]);
    expected_snapshot.extend([0, 0, 0, 0, 0, 0, 0, 9]);
    expected_snapshot.extend([0x22; 32]);
    expected_snapshot.extend([0x33; 32]);
    assert_eq!(encode(&snapshot), expected_snapshot);
    let cursor = EqCursor {
        snapshot,
        query_digest: B256::repeat_byte(0x44),
        last_id: id(7),
    };
    let mut expected_cursor = vec![0, 0, 0, 1];
    expected_cursor.extend(expected_snapshot);
    expected_cursor.extend([0x44; 32]);
    expected_cursor.extend([0, 0, 0, 0, 0, 0, 0, 7]);
    assert_eq!(encode(&cursor), expected_cursor);
    assert_eq!(decode_exact::<EqCursor>(&expected_cursor).unwrap(), cursor);
    for at in [3, 17, 26, 27] {
        if at < expected.len() {
            let mut bytes = expected;
            bytes[at] = 99;
            assert!(decode_exact::<EqQuery>(&bytes).is_err());
        }
    }
    let mut extra = expected_cursor;
    extra.push(0);
    assert!(decode_exact::<EqCursor>(&extra).is_err());
}

#[test]
fn literal_charge_vectors_cover_full_posting_and_all_output_bytes() {
    // Empty-catalog proof is 497 bytes: envelope 145 + state header 348 +
    // catalog path count 4. No trie node exists.
    let engine = Engine::new(PriceSchedule::FIXTURE);
    let snapshot = engine.head_snapshot();
    let req = request(&snapshot, Value::Bool(true), 1);
    let result = snapshot
        .prove_eq(&req, ProofLimits::default(), Budget::unlimited().traced())
        .unwrap();
    assert_eq!(result.bytes.len(), 497);
    assert_eq!(result.receipt.model_version, 4);
    assert_eq!(result.receipt.spent.units(), 498);
    let ledger = result.receipt.ledger.unwrap();
    assert_eq!(
        ledger
            .entries
            .iter()
            .map(|e| (e.class, e.count))
            .collect::<Vec<_>>(),
        vec![(Op::ProofByte, 497), (Op::DirectoryLookup, 1)]
    );
    let (engine, _) = fixture(9);
    let snapshot = engine.head_snapshot();
    let req = request(&snapshot, Value::str("book"), 1);
    let result = snapshot
        .prove_eq(&req, ProofLimits::default(), Budget::unlimited().traced())
        .unwrap();
    let ledger = result.receipt.ledger.unwrap();
    let count = |class| {
        ledger
            .entries
            .iter()
            .find(|e| e.class == class)
            .map_or(0, |e| e.count)
    };
    assert_eq!(count(Op::PostingEntry), 7); // full set, even for one row
    assert_eq!(count(Op::RecordProbe), 1);
    assert_eq!(count(Op::ResultRow), 1);
    assert_eq!(count(Op::ReadByte), 40); // key 8 + kind 4 + book 4 + secret 6 + payload 18
    assert_eq!(count(Op::ProofByte), result.bytes.len() as u64);
    assert_eq!(count(Op::TermVisit), 0);
    assert!(count(Op::ProofNode) >= result.supplied_nodes as u64);
}

/// Independently encode all four map levels. `mutation` commits an
/// inconsistent state under a new trusted root, to exercise semantic checks
/// beyond detecting a changed node hash. This fixture has no Engine.
fn independent_witness(mutation: u8) -> (EqRequest, Wire) {
    let mut rows = BTreeMap::new();
    let mut keys = BTreeMap::new();
    let mut posting = BTreeMap::new();
    for n in 1..=2u64 {
        let raw_key = [b'k', n as u8];
        let mut row = vec![
            2, raw_key[0], raw_key[1], 0, 0, 0, 1, 0, 0, 0, 1, b'a', 1, 1, 1,
        ];
        if n == 1 {
            match mutation {
                1 => row[12] = 2, // field
                2 => row[14] = 0, // value false
                3 => {
                    row[13] = 3;
                    row.pop();
                    row.extend(1u64.to_be_bytes());
                } // U64
                4 => row[2] = 9,  // row's key lacks binding
                5 => row[7..11].copy_from_slice(&u32::MAX.to_be_bytes()), // oversized name
                6 => row[3..7].copy_from_slice(&65u32.to_be_bytes()), // too many cells
                7 => row.push(0), // leftover stored-row bytes
                _ => {}
            }
        }
        rows.insert(n.to_be_bytes().to_vec(), row);
        let mut key = vec![0; 33];
        key[0] = 2;
        key[1..3].copy_from_slice(&raw_key);
        let bound = if mutation == 8 && n == 1 { 2u64 } else { n };
        keys.insert(key, bound.to_be_bytes().to_vec());
        posting.insert(n.to_be_bytes().to_vec(), vec![1]);
    }
    let posting_root = reference::root(&posting);
    assert_eq!(posting_root, hash_builder_root(&posting));
    let mut term_key = vec![0; CellName::MAX_LEN + Value::MAX_INDEXED_LEN + 4];
    term_key[0] = b'a';
    term_key[CellName::MAX_LEN] = 1;
    term_key[CellName::MAX_LEN + 1] = 1;
    term_key[CellName::MAX_LEN + 2] = 1;
    term_key[CellName::MAX_LEN + 3] = 1;
    term_key[CellName::MAX_LEN + Value::MAX_INDEXED_LEN + 3] = 1;
    let mut posting_leaf = posting_root.to_vec();
    let count = match mutation {
        10 => 3u64,
        11 => 4097,
        _ => 2,
    };
    posting_leaf.extend(count.to_be_bytes());
    if mutation == 12 {
        posting_leaf[0] ^= 1;
    }
    let term_map = BTreeMap::from([(term_key.clone(), posting_leaf)]);
    // Descriptor: id 1, name n, owner [7;20], revision 0, allocator 3.
    let mut catalog_leaf = (if mutation == 9 { 2u64 } else { 1 })
        .to_be_bytes()
        .to_vec();
    catalog_leaf.extend([0, 0, 0, 1, b'n']);
    catalog_leaf.extend([7; 20]);
    catalog_leaf.extend([0; 8]);
    catalog_leaf.extend(3u64.to_be_bytes());
    for (root, count) in [
        (alloy_trie::EMPTY_ROOT_HASH, 0u64),
        (reference::root(&keys), 2),
        (reference::root(&rows), 2),
        (reference::root(&term_map), 1),
    ] {
        catalog_leaf.extend(root.as_slice());
        catalog_leaf.extend(count.to_be_bytes());
    }
    let catalog = BTreeMap::from([(1u64.to_be_bytes().to_vec(), catalog_leaf)]);
    let header = StateHeader {
        format: if cfg!(feature = "arkiv-v2") { 2 } else { 1 },
        catalog_root: reference::root(&catalog),
        catalog_len: 1,
        next_namespace: 2,
        pricing: PriceSchedule::FIXTURE,
        host_root: alloy_trie::EMPTY_ROOT_HASH,
        host_len: 0,
    };
    let query = EqQuery {
        namespace: ns(1),
        attribute: name("a"),
        value: Value::Bool(true),
    };
    let snapshot = SnapshotRef {
        genesis_hash: B256::repeat_byte(0x11),
        height: 9,
        block_hash: B256::repeat_byte(0x22),
        state_root: header.root(),
    };
    let req = EqRequest {
        snapshot: snapshot.clone(),
        query: query.clone(),
        limit: 2,
        continuation: None,
    };
    let witnesses = (1..=2u64)
        .map(|n| {
            let mut key = vec![0; 33];
            key[0] = 2;
            key[1] = b'k';
            key[2] = n as u8;
            (
                id(n),
                reference::proof(&rows, &n.to_be_bytes()),
                reference::proof(&keys, &key),
            )
        })
        .collect();
    let wire = Wire {
        snapshot,
        digest: query.digest(),
        tag: 2,
        header,
        catalog: reference::proof(&catalog, &1u64.to_be_bytes()),
        term: reference::proof(&term_map, &term_key),
        ids: vec![id(1), id(2)],
        rows: witnesses,
        next: None,
    };
    (req, wire)
}

#[test]
fn independent_trie_fixture_verifies_without_engine_and_rejects_inconsistent_authenticated_rows() {
    let (req, wire) = independent_witness(0);
    let VerifiedEq::Page(page) = verify(&req, None, &wire.bytes()).unwrap() else {
        panic!()
    };
    assert_eq!(page.rows.len(), 2);
    assert_eq!(
        page.rows[0],
        Row {
            id: id(1),
            key: Some(RecordKey::new(&[b'k', 1]).unwrap()),
            cells: vec![(name("a"), Cell::attribute(Value::Bool(true)))]
        }
    );
    for mutation in 1..=12 {
        let (req, wire) = independent_witness(mutation);
        assert!(
            verify(&req, None, &wire.bytes()).is_err(),
            "accepted independent inconsistent state {mutation}"
        );
    }
}

#[test]
fn unavailable_storage_is_an_error_and_memory_and_stored_proof_bytes_match() {
    use arkiv_db_storage::versioned::{ObjectId, ObjectReader, SpaceId, StorageError};
    use std::sync::{Arc, Mutex};
    struct Store(Mutex<BTreeMap<(SpaceId, ObjectId), Vec<u8>>>);
    impl ObjectReader for Store {
        fn read_object(
            &self,
            space: SpaceId,
            id: &ObjectId,
        ) -> Result<Option<Vec<u8>>, StorageError> {
            Ok(self.0.lock().unwrap().get(&(space, *id)).cloned())
        }
    }
    let (engine, _) = fixture(9);
    let original = engine.head_snapshot();
    let objects = original
        .staged_objects()
        .into_iter()
        .map(|o| ((o.space, o.id), o.bytes))
        .collect();
    let store = Arc::new(Store(Mutex::new(objects)));
    let reopened =
        Engine::from_state(&original.state_header(), Limits::DEFAULT, store.clone()).unwrap();
    let snapshot = reopened.head_snapshot();
    let req = request(&original, Value::str("book"), 3);
    assert_eq!(produce(&snapshot, &req), produce(&original, &req));
    store.0.lock().unwrap().clear();
    assert_eq!(
        snapshot
            .prove_eq(&req, ProofLimits::default(), Budget::unlimited())
            .unwrap_err(),
        ProofError::StorageUnavailable
    );
}

#[test]
fn unrelated_namespace_growth_does_not_add_posting_or_row_work() {
    let (mut engine, _) = fixture(9);
    let first = engine.head_snapshot();
    let req = request(&first, Value::str("book"), 3);
    let before = first
        .prove_eq(&req, ProofLimits::default(), Budget::unlimited().traced())
        .unwrap();
    let mut branch = engine.begin(engine.head(), None).unwrap();
    for n in 0..300 {
        branch
            .execute(
                OWNER,
                WriteOp::CreateRecord {
                    namespace: ns(2),
                    key: key(n),
                    cells: cells(Value::str("book")),
                },
                Budget::unlimited(),
            )
            .unwrap();
    }
    engine.publish(&branch.seal()).unwrap();
    let after_snapshot = engine.head_snapshot();
    let req = request(&after_snapshot, Value::str("book"), 3);
    let after = after_snapshot
        .prove_eq(&req, ProofLimits::default(), Budget::unlimited().traced())
        .unwrap();
    assert_eq!(before.receipt, after.receipt);
    assert_eq!(before.bytes.len(), after.bytes.len());
    let before = Wire::parse(&before.bytes);
    let after = Wire::parse(&after.bytes);
    assert_eq!(before.term, after.term);
    assert_eq!(before.ids, after.ids);
    assert_eq!(before.rows, after.rows);
}

#[test]
fn largest_rows_are_bounded_before_materialization_and_respect_trusted_genesis_limits() {
    let mut engine = Engine::new(PriceSchedule::FIXTURE);
    let mut branch = engine.begin(engine.head(), None).unwrap();
    branch
        .execute(
            OWNER,
            WriteOp::CreateNamespace {
                name: NamespaceName::new("large").unwrap(),
            },
            Budget::unlimited(),
        )
        .unwrap();
    let mut fields = vec![(name("kind"), Cell::attribute(Value::Bool(true)))];
    for i in 0..63 {
        fields.push((
            name(&format!("f{i:02}")),
            Cell::field(Value::bytes(&vec![i; 4096])),
        ));
    }
    branch
        .execute(
            OWNER,
            WriteOp::CreateRecord {
                namespace: ns(1),
                key: key(1),
                cells: fields,
            },
            Budget::unlimited(),
        )
        .unwrap();
    engine.publish(&branch.seal()).unwrap();
    let snapshot = engine.head_snapshot();
    let req = request(&snapshot, Value::Bool(true), 1);
    let bytes = produce(&snapshot, &req);
    let VerifiedEq::Page(page) = verify(&req, None, &bytes).unwrap() else {
        panic!()
    };
    assert_eq!(page.rows[0].cells.len(), 64);
    assert_eq!(page.rows[0].cells[0].1.value, Value::bytes(&vec![0; 4096]));
    for limits in [
        Limits {
            max_cells: 63,
            ..Limits::DEFAULT
        },
        Limits {
            max_value_len: 4095,
            ..Limits::DEFAULT
        },
    ] {
        assert!(verify_eq(&req, None, &bytes, ProofLimits::default(), limits).is_err());
    }
    // Meter budget cannot fund a full row, so generation fails before the
    // row lookup/proof serialization and returns no partial evidence.
    assert_eq!(
        snapshot
            .prove_eq(&req, ProofLimits::default(), Budget::limit(200_000))
            .unwrap_err(),
        ProofError::BudgetExceeded
    );
    assert_eq!(
        snapshot
            .prove_eq(
                &req,
                ProofLimits {
                    max_proof_bytes: 200_000,
                    ..ProofLimits::default()
                },
                Budget::unlimited()
            )
            .unwrap_err(),
        ProofError::ProofLimitExceeded
    );
}

#[test]
fn complete_posting_cap_admits_4096_and_rejects_one_more_before_iteration() {
    let mut engine = Engine::new(PriceSchedule::FIXTURE);
    let mut branch = engine.begin(engine.head(), None).unwrap();
    branch
        .execute(
            OWNER,
            WriteOp::CreateNamespace {
                name: NamespaceName::new("limit").unwrap(),
            },
            Budget::unlimited(),
        )
        .unwrap();
    for n in 1..=4096 {
        branch
            .execute(
                OWNER,
                WriteOp::CreateRecord {
                    namespace: ns(1),
                    key: key(n),
                    cells: cells(Value::Bool(true)),
                },
                Budget::unlimited(),
            )
            .unwrap();
    }
    engine.publish(&branch.seal()).unwrap();
    let snapshot = engine.head_snapshot();
    let req = request(&snapshot, Value::Bool(true), 1);
    let VerifiedEq::Page(page) = verify(&req, None, &produce(&snapshot, &req)).unwrap() else {
        panic!()
    };
    assert_eq!(page.posting_count, 4096);
    assert_eq!(page.rows[0].id, id(1));
    assert!(page.next.is_some());
    let mut branch = engine.begin(engine.head(), None).unwrap();
    branch
        .execute(
            OWNER,
            WriteOp::CreateRecord {
                namespace: ns(1),
                key: key(4097),
                cells: cells(Value::Bool(true)),
            },
            Budget::unlimited(),
        )
        .unwrap();
    engine.publish(&branch.seal()).unwrap();
    let snapshot = engine.head_snapshot();
    let req = request(&snapshot, Value::Bool(true), 1);
    assert_eq!(
        snapshot
            .prove_eq(&req, ProofLimits::default(), Budget::unlimited())
            .unwrap_err(),
        ProofError::ProofLimitExceeded
    );
}

#[test]
fn every_supported_type_and_historical_expiry_use_selected_state_only() {
    let mut engine = Engine::new(PriceSchedule::FIXTURE);
    let mut branch = engine.begin(engine.head(), None).unwrap();
    branch
        .execute(
            OWNER,
            WriteOp::CreateNamespace {
                name: NamespaceName::new("types").unwrap(),
            },
            Budget::unlimited(),
        )
        .unwrap();
    let values = [
        Value::Bool(false),
        Value::Bool(true),
        Value::I64(i64::MIN),
        Value::I64(-1),
        Value::I64(0),
        Value::I64(i64::MAX),
        Value::U64(0),
        Value::U64(u64::MAX),
        Value::str(""),
        Value::str(&"é".repeat(32)),
    ];
    let mut model = Vec::new();
    for (index, value) in values.iter().enumerate() {
        let ordinal = index as u64 + 1;
        let mut fields = cells(value.clone());
        fields.push((name("$expiresAt"), Cell::attribute(Value::U64(2))));
        branch
            .execute(
                OWNER,
                WriteOp::CreateRecord {
                    namespace: ns(1),
                    key: key(ordinal),
                    cells: fields.clone(),
                },
                Budget::unlimited(),
            )
            .unwrap();
        fields.sort_by(|a, b| a.0.cmp(&b.0));
        model.push(Row {
            id: id(ordinal),
            key: Some(key(ordinal)),
            cells: fields,
        });
    }
    engine.publish(&branch.seal()).unwrap();
    let before = engine.head_snapshot();
    let mut branch = engine.begin(engine.head(), None).unwrap();
    let expired = arkiv_db::expire_records(
        &mut branch,
        ns(1),
        &name("$expiresAt"),
        arkiv_db::BlockNumber(2),
        Budget::unlimited(),
        Budget::unlimited(),
    )
    .unwrap();
    assert_eq!(expired.len(), values.len());
    engine.publish(&branch.seal()).unwrap();
    let after = engine.head_snapshot();
    for value in values {
        let req = request(&before, value.clone(), 3);
        assert_eq!(
            walk(&before, req.clone(), &[1, 3, 64]),
            expected(&model, &req.query)
        );
        assert!(walk(&after, request(&after, value, 3), &[1, 3, 64]).is_empty());
    }
}

#[test]
#[cfg(not(feature = "arkiv-v2"))]
fn empty_namespace_proof_has_hand_built_canonical_bytes() {
    fn independent_digest(domain: &[u8], bytes: &[u8]) -> B256 {
        let mut input = vec![domain.len() as u8];
        input.extend(domain);
        input.extend(bytes);
        alloy_primitives::keccak256(input)
    }
    // Header: format 1, empty catalog, allocator 1, schedule 7 with
    // thirty-one weights of one, empty host map. No production codecs.
    let mut header = vec![0, 0, 0, 1];
    header.extend(alloy_trie::EMPTY_ROOT_HASH.as_slice());
    header.extend([0; 8]);
    header.extend([0, 0, 0, 0, 0, 0, 0, 1]);
    header.extend([0, 0, 0, 7, 0, 0, 0, 31]);
    for _ in 0..31 {
        header.extend([0, 0, 0, 0, 0, 0, 0, 1]);
    }
    header.extend(alloy_trie::EMPTY_ROOT_HASH.as_slice());
    header.extend([0; 8]);
    let root = independent_digest(b"arkiv/state/v1", &header);
    let query_bytes = [
        0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 1, b'a', 1, 1, 1, 1,
    ];
    let query_digest = independent_digest(b"arkiv/eq-query/v1", &query_bytes);
    let mut expected = vec![0, 0, 0, 1, 0, 0, 0, 1]; // response version, snapshot version
    expected.extend([0x11; 32]);
    expected.extend([0; 8]);
    expected.extend([0x22; 32]);
    expected.extend(root.as_slice());
    expected.extend(query_digest.as_slice());
    expected.push(1);
    expected.extend(header);
    expected.extend([0; 4]);
    let engine = Engine::new(PriceSchedule::uniform(7, 1));
    let snapshot = engine.head_snapshot();
    let mut req = request(&snapshot, Value::Bool(true), 1);
    req.query.attribute = name("a");
    assert_eq!(req.snapshot.state_root, root);
    assert_eq!(req.query.digest(), query_digest);
    assert_eq!(produce(&snapshot, &req), expected);
    assert_eq!(
        verify(&req, None, &expected),
        Ok(VerifiedEq::NamespaceAbsent)
    );
}

#[test]
fn inspection_binds_map_roots_keys_and_complete_posting_reconstruction() {
    use arkiv_db::MapKind;
    use arkiv_db_trie::PathTerminal;
    let (req, wire) = independent_witness(0);
    let (result, inspection) = arkiv_db::proof::verify_eq_inspected(
        &req,
        None,
        &wire.bytes(),
        ProofLimits::default(),
        Limits::DEFAULT,
    )
    .unwrap();
    assert_eq!(inspection.state_header.root(), req.snapshot.state_root);
    assert!(matches!(result, VerifiedEq::Page(_)));
    assert_eq!(
        inspection
            .point_paths
            .iter()
            .map(|p| p.map)
            .collect::<Vec<_>>(),
        vec![
            MapKind::Catalog,
            MapKind::Terms,
            MapKind::Rows,
            MapKind::Keys,
            MapKind::Rows,
            MapKind::Keys
        ]
    );
    assert_eq!(inspection.point_paths[0].root, wire.header.catalog_root);
    assert_eq!(
        inspection.point_paths[0].key,
        req.query.namespace.to_bytes()
    );
    let roots = inspection.namespace.unwrap();
    for path in &inspection.point_paths {
        assert_eq!(path.trace.terminal, PathTerminal::Inclusion);
        assert_eq!(path.trace.nodes[0].hash, path.root);
        match path.map {
            MapKind::Terms => assert_eq!(path.root, roots.terms.0),
            MapKind::Rows => {
                assert_eq!(path.root, roots.rows.0);
                assert_eq!(path.key, path.record_id.unwrap().ordinal().to_be_bytes());
            }
            MapKind::Keys => assert_eq!(path.root, roots.keys.0),
            _ => {}
        }
    }
    let posting = inspection.posting_set.unwrap();
    assert_eq!(posting.ids, vec![id(1), id(2)]);
    assert_eq!(posting.selected_ids, posting.ids);
    assert_eq!(posting.authenticated_root, Some(posting.reconstructed_root));
    // Full inspector also honors local resource policy.
    let caps = ProofLimits {
        max_allocated_bytes: 1,
        ..ProofLimits::default()
    };
    assert_eq!(
        arkiv_db::proof::verify_eq_inspected(&req, None, &wire.bytes(), caps, Limits::DEFAULT),
        Err(ProofError::ProofLimitExceeded)
    );
}

#[cfg(not(feature = "arkiv-v2"))]
use arkiv_db::{decode_exact, encode};
