#![cfg(not(feature = "arkiv-v2"))]
// These retained fixtures pin the legacy generic/signed protocol.
// Arkiv v2 has separate host, profile and crash acceptance suites.
//! Durable simulator model, payload whitelist, restart, uncertainty and light tests.
use arkiv_db::{
    Budget, CellName, NamespaceId, Value,
    durable::Options,
    encode,
    proof::{EqQuery, EqRequest, SnapshotRef, VerifiedEq, verify_eq},
};
use arkiv_db_memory::MemoryVersionedStore;
use arkiv_db_storage::{
    testing::{CommitFault, FaultyStore},
    versioned::{StorageErrorKind, VersionedStore},
};
use arkiv_node::{
    ChainSpec, MemoryChain,
    simulator::{
        host::{Control, Host, RunConfig},
        light::{Light, VerifiedRequest},
        wire::*,
        workload::Workload,
    },
};
use std::sync::Arc;

fn run() -> RunConfig {
    RunConfig {
        genesis: arkiv_node::demo::workload::genesis(9001, 1_700_000_000_000),
        workload: Workload::default(),
    }
}
fn host(store: Arc<dyn VersionedStore>) -> Host {
    Host::create(store, run(), [1; 16], [7; 16], Options::default()).unwrap()
}
fn request(host: &Host, height: u64, limit: u32) -> EqRequest {
    let h = host.header(height).unwrap();
    EqRequest {
        snapshot: SnapshotRef {
            genesis_hash: host.durable_identity.node_genesis_hash,
            height,
            block_hash: h.hash(),
            state_root: h.state_root.unwrap(),
        },
        query: EqQuery {
            namespace: NamespaceId::from_ordinal(1).unwrap(),
            attribute: CellName::new("group").unwrap(),
            value: Value::U64(1),
        },
        limit,
        continuation: None,
    }
}
fn control(h: &Host, id: u8, action: &str) -> Control {
    Control {
        command_id: hex(&[id; 32]),
        run_id: h.identity.run_id.clone(),
        expected_revision: h.revision.to_string(),
        expected_height: Some(h.head.height.to_string()),
        action: action.into(),
        config: None,
    }
}

#[test]
fn two_hundred_blocks_match_memory_roots_receipts_history_and_proof_bytes() {
    let store = Arc::new(MemoryVersionedStore::new());
    let mut h = host(store.clone());
    let mut memory = MemoryChain::new(ChainSpec::new(run().genesis).unwrap());
    let follower_store = Arc::new(MemoryVersionedStore::new());
    let mut follower = host(follower_store);
    let light_store = Arc::new(MemoryVersionedStore::new());
    let mut light = Light::create(light_store.clone(), h.durable_identity.clone()).unwrap();
    for height in 1..=200 {
        h.produce(None).unwrap();
        let b = h.block(height).unwrap();
        memory.apply(&b.block).unwrap();
        assert_eq!(encode(memory.head()), encode(&h.head));
        assert_eq!(
            h.engine.head_snapshot().state_root(),
            memory.snapshot(height).unwrap().state_root()
        );
        if height >= 30 && height % 10 == 0 {
            for n in follower.head.height + 1..=height {
                follower.import(h.block(n).unwrap()).unwrap();
            }
            assert_eq!(follower.head, h.head);
        }
        if height >= 20 && height % 5 == 0 {
            let headers = (light.head.height + 1..=height)
                .map(|n| h.header(n).unwrap())
                .collect::<Vec<_>>();
            light.append(&headers, &h.head).unwrap();
        }
        let encoded = json_bytes(&b.feed, FEED_CAP).unwrap();
        assert!(
            !String::from_utf8(encoded)
                .unwrap()
                .contains(&"S".repeat(32))
        );
        let sum: u64 = b
            .feed
            .operations
            .iter()
            .map(|o| {
                o.receipt
                    .as_ref()
                    .map_or(0, |r| number(&r.spent_units).unwrap())
            })
            .sum();
        assert_eq!(sum, b.block.outcomes.spent);
    }
    assert_eq!(follower.head, h.head);
    assert_eq!(light.head, h.head);
    let b7 = h.block(7).unwrap();
    assert!(b7.feed.changes.is_empty());
    assert_eq!(b7.feed.operations[0].outcome, "rolledBack");
    assert!(
        h.block(8)
            .unwrap()
            .feed
            .changes
            .iter()
            .any(|c| matches!(c, Change::RecordDelete { .. }))
    );
    assert!(
        h.block(9)
            .unwrap()
            .feed
            .changes
            .iter()
            .any(|c| matches!(c, Change::RecordUpsert { .. }))
    );
    assert!(
        h.block(14)
            .unwrap()
            .feed
            .operations
            .iter()
            .any(|o| o.phase == "expiry" && o.record_key.is_some())
    );
    for height in [0, 1, 6, 7, 8, 9, 14, 20, 100, 200] {
        for limit in [1, 3, 64] {
            let mut q = request(&h, height, limit);
            let mut prior = None;
            let mut ids = Vec::new();
            loop {
                let bytes = h.proof(&q).unwrap();
                let other = memory
                    .snapshot(height)
                    .unwrap()
                    .prove_eq(&q, proof_limits(), Budget::limit(10_000_000))
                    .unwrap()
                    .bytes;
                assert_eq!(bytes, other);
                match verify_eq(
                    &q,
                    prior.as_ref(),
                    &bytes,
                    proof_limits(),
                    h.run.genesis.limits,
                )
                .unwrap()
                {
                    VerifiedEq::NamespaceAbsent => {
                        assert_eq!(height, 0);
                        break;
                    }
                    VerifiedEq::Page(page) => {
                        ids.extend(page.rows.iter().map(|r| r.id));
                        let Some(next) = page.next else { break };
                        q.continuation = Some(next.cursor().clone());
                        prior = Some(next);
                    }
                }
            }
            assert!(ids.windows(2).all(|w| w[0] < w[1]));
        }
    }
    let identity = h.identity.clone();
    drop(h);
    let before = store.stats();
    let reopened = Host::open(store.clone(), Some(&identity), Options::default()).unwrap();
    let reads = store.stats().since(&before);
    assert_eq!(reopened.head.height, 200);
    assert_eq!(reads.object_reads(), 0);
    assert!(
        reads.record_reads <= 20,
        "cold open reads {}",
        reads.record_reads
    );
    assert_eq!(reopened.engine.stats().resident_manifests, 1);
    assert!(reopened.paused);
    drop(light);
    let before = light_store.stats();
    let light = Light::open(light_store.clone(), &identity).unwrap();
    assert_eq!(light.head, reopened.head);
    let reads = light_store.stats().since(&before);
    assert!(reads.record_reads <= 5);
    assert_eq!(reads.object_reads(), 0);
}

#[test]
fn durable_controls_replay_exactly_after_restart_and_configuration_is_adopted_in_block() {
    let store = Arc::new(MemoryVersionedStore::new());
    let mut h = host(store.clone());
    let step = control(&h, 1, "step");
    h.control(step.clone()).unwrap();
    assert_eq!(h.head.height, 1);
    let mut config = control(&h, 2, "configure");
    let mut next = h.workload.clone();
    next.payload_bytes = 1024;
    next.extra_rows_per_block = 4;
    config.config = Some(next.clone());
    h.control(config.clone()).unwrap();
    assert_eq!(h.revision, 0);
    assert_eq!(h.workload.payload_bytes, 64);
    h.produce(None).unwrap();
    assert_eq!(h.revision, 1);
    assert_eq!(h.workload, next);
    let committed = h.head;
    let id = h.identity.clone();
    drop(h);
    let mut h = Host::open(store.clone(), Some(&id), Options::default()).unwrap();
    assert_eq!(h.control(step.clone()).unwrap()["height"], "1");
    assert_eq!(h.control(config.clone()).unwrap()["height"], "2");
    assert_eq!(h.head, committed);
    let mut conflicting = step;
    conflicting.action = "resume".into();
    assert_eq!(h.control(conflicting).unwrap_err().0, "RequestConflict");
    let mut wrong = control(&h, 3, "step");
    wrong.run_id = "11".repeat(16);
    assert_eq!(h.control(wrong).unwrap_err().0, "IdentityMismatch");
    let mut wrong = control(&h, 4, "configure");
    let mut cfg = next;
    cfg.seed = "8".into();
    wrong.config = Some(cfg);
    assert_eq!(h.control(wrong).unwrap_err().0, "RequestConflict");
}

#[test]
fn failure_outcomes_never_expose_uncertain_body_or_head_and_exact_retry_keeps_command() {
    for fault in [
        CommitFault::NotCommitted(StorageErrorKind::Io),
        CommitFault::IndeterminateNotApplied(StorageErrorKind::Io),
        CommitFault::IndeterminateApplied(StorageErrorKind::Io),
        CommitFault::AckLost,
    ] {
        let store = Arc::new(FaultyStore::latching(MemoryVersionedStore::new()));
        let mut h = host(store.clone());
        let step = control(&h, 1, "step");
        store.push_commit_fault(fault);
        assert!(h.control(step.clone()).is_err());
        assert_eq!(h.head.height, 0);
        assert_eq!(h.health, "storage-fenced");
        assert_eq!(h.block(1).unwrap_err().0, "UnknownBlock");
        let applied = matches!(
            fault,
            CommitFault::IndeterminateApplied(_) | CommitFault::AckLost
        );
        let retry = h.retry_pending();
        if applied {
            assert_eq!(retry.unwrap_err().0, "CommitUncertain");
            assert_eq!(h.head.height, 0);
            assert_eq!(h.block(1).unwrap_err().0, "UnknownBlock");
        } else {
            assert_eq!(retry.unwrap().height, 1);
        }
        drop(h);
        let mut h = Host::open(store, None, Options::default()).unwrap();
        assert_eq!(h.head.height, 1);
        assert_eq!(h.control(step).unwrap()["height"], "1");
        assert_eq!(h.head.height, 1);
    }
}

#[test]
fn light_pins_history_verifies_pages_rejects_mutations_and_persists_conflict() {
    let mut h = host(Arc::new(MemoryVersionedStore::new()));
    for _ in 0..40 {
        h.produce(None).unwrap();
    }
    let store = Arc::new(MemoryVersionedStore::new());
    let mut light = Light::create(store.clone(), h.durable_identity.clone()).unwrap();
    let gap = vec![h.header(2).unwrap(), h.header(3).unwrap()];
    assert_eq!(light.append(&gap, &h.head).unwrap_err().0, "BlockNotSynced");
    assert_eq!(light.head.height, 0);
    let headers = (1..=40).map(|n| h.header(n).unwrap()).collect::<Vec<_>>();
    light.append(&headers, &h.head).unwrap();
    light.append(&headers[..3], &h.header(3).unwrap()).unwrap();
    assert_eq!(light.head.height, 40);
    let mut r = VerifiedRequest {
        height: "20".into(),
        namespace: "1".into(),
        attribute: "group".into(),
        value_type: "u64".into(),
        value: serde_json::json!("1"),
        limit: 1,
        cursor: None,
    };
    let mut pages = 0;
    loop {
        let (q, prior) = light.request(&r).unwrap();
        let mut bytes = h.proof(&q).unwrap();
        let position = bytes.len() / 2;
        bytes[position] ^= 1;
        assert!(light.verify(&r, &q, prior.as_ref(), &bytes).is_err());
        let bytes = h.proof(&q).unwrap();
        let page = light.verify(&r, &q, prior.as_ref(), &bytes).unwrap();
        pages += 1;
        r.cursor = page["continuation"].as_str().map(str::to_owned);
        if r.cursor.is_none() {
            break;
        }
        let mut wrong = r.clone();
        wrong.limit = 3;
        assert_eq!(light.request(&wrong).unwrap_err().0, "CursorMismatch");
    }
    assert!(pages >= 4);
    assert!(light.continuation_count() <= 64);
    let mut bad = h.header(1).unwrap();
    bad.timestamp_ms += 1;
    assert_eq!(
        light.append(&[bad], &h.head).unwrap_err().0,
        "ChainConflict"
    );
    assert_eq!(light.header(20).unwrap_err().0, "ChainConflict");
    drop(light);
    assert!(matches!(Light::open(store,&h.identity),Err(e) if e.0=="ChainConflict"));
}

#[test]
fn bounded_serialization_and_profile_reject_unserviceable_genesis() {
    assert_eq!(json_bytes(&"123", 5).unwrap(), b"\"123\"");
    assert_eq!(json_bytes(&"1234", 5).unwrap_err().0, "CapacityReached");
    let mut bad = run();
    bad.genesis.limits.max_cells = 17;
    assert!(bad.validate().is_err());
    let mut bad = Workload {
        payload_bytes: 1025,
        ..Default::default()
    };
    assert!(bad.validate().is_err());
    bad.payload_bytes = 1024;
    bad.extra_rows_per_block = 5;
    assert!(bad.validate().is_err());
    assert!(unhex("0xAB", 1).is_err());
    assert!(number("01").is_err());
    let fields = [
        Value::Bool(true),
        Value::I64(i64::MIN),
        Value::U64(u64::MAX),
        Value::str("payload-secret"),
        Value::bytes(b"payload-secret"),
    ];
    for value in fields {
        let descriptor = arkiv_node::simulator::metadata::value_digest(&value);
        assert_eq!(descriptor.len(), 66);
        assert!(!descriptor.contains("payload-secret"));
    }
}

#[test]
fn metadata_binary_codec_matches_typescript_golden_and_detects_tampering() {
    let value = serde_json::json!({"z":[null,false,true,4294967295u32],"é":"a\u{0}","a":"9007199254740993"});
    assert_eq!(
        hex(&metadata_bytes(&value).unwrap()),
        "0x060000000304000000016104000000103930303731393932353437343039393304000000017a050000000400010203ffffffff0400000002c3a904000000026100"
    );
    assert_eq!(
        hex(metadata_digest(&value).unwrap().as_slice()),
        "0xf0db2b45a68f5c2f427351f89e2aa35072cff97bea42f76262038225be3341b6"
    );
    assert!(metadata_bytes(&serde_json::json!(-1)).is_err());
    assert!(metadata_bytes(&serde_json::json!(4294967296u64)).is_err());
    let h = host(Arc::new(MemoryVersionedStore::new()));
    let mut b = h.block(0).unwrap();
    b.feed.validate_digest().unwrap();
    b.feed.spent_units = "1".into();
    assert!(b.feed.validate_digest().is_err());
    assert!(
        arkiv_node::simulator::host::StoredBlock::from_bytes(
            &b.bytes().unwrap(),
            &h.identity,
            &h.run.genesis
        )
        .is_err()
    );
}

#[test]
fn collector_handles_same_block_recreation_rollback_id_reuse_and_every_field_type() {
    use arkiv_db::{Cell, Cost, RecordKey, Value};
    use arkiv_node::simulator::{host::StoredBlock, metadata};
    use arkiv_node::{Block, BlockInputs, TxEnvelope, TxOp};
    let mut h = host(Arc::new(MemoryVersionedStore::new()));
    h.produce(None).unwrap();
    let ns = NamespaceId::from_ordinal(1).unwrap();
    let key = RecordKey::new(b"same-key").unwrap();
    let fields = [
        Value::Bool(true),
        Value::I64(i64::MIN),
        Value::U64(u64::MAX),
        Value::str("NEVER_EXPOSE_FIELD"),
        Value::bytes(b"NEVER_EXPOSE_BYTES"),
    ];
    let mut cells = fields
        .into_iter()
        .enumerate()
        .map(|(i, v)| (CellName::new(&format!("field{i}")).unwrap(), Cell::field(v)))
        .collect::<Vec<_>>();
    cells.push((
        CellName::new("group").unwrap(),
        Cell::attribute(Value::U64(u64::MAX)),
    ));
    cells.push((
        CellName::new("signed").unwrap(),
        Cell::attribute(Value::I64(i64::MIN)),
    ));
    cells.push((
        CellName::new("text").unwrap(),
        Cell::attribute(Value::str("é\0")),
    ));
    let create = |key: RecordKey| TxOp::CreateRecord {
        namespace: ns,
        key,
        cells: cells.clone(),
        expiry: None,
    };
    let tx = |id: u8, ops| TxEnvelope {
        chain_id: h.spec.chain_id(),
        actor: arkiv_node::demo::workload::ALICE,
        request_id: [id; 32],
        tx_budget: 30000,
        ops,
    };
    let inputs = BlockInputs {
        admin: vec![],
        txs: vec![
            tx(
                1,
                vec![
                    create(key.clone()),
                    TxOp::DeleteRecord {
                        namespace: ns,
                        key: key.clone(),
                    },
                    create(key.clone()),
                ],
            ),
            tx(
                2,
                vec![
                    create(RecordKey::new(b"rolled-back").unwrap()),
                    TxOp::DeleteRecord {
                        namespace: ns,
                        key: RecordKey::new(b"absent").unwrap(),
                    },
                ],
            ),
            tx(
                3,
                vec![
                    create(RecordKey::new(b"reuses-id").unwrap()),
                    TxOp::Put {
                        namespace: ns,
                        key: b"raw".to_vec(),
                        value: b"NEVER_EXPOSE_RAW".to_vec(),
                    },
                ],
            ),
        ],
    };
    let branch = h
        .engine
        .begin(
            h.engine.head(),
            Some(Cost::new(h.spec.params().block_budget)),
        )
        .unwrap();
    let mut captured = None;
    let timestamp = h.workload.timestamp(h.run.genesis.timestamp_ms, 2).unwrap();
    let executed = arkiv_node::exec::execute_block_observed(
        &h.spec,
        branch,
        &h.head,
        2,
        timestamp,
        &inputs,
        |b, o, e| {
            captured = Some(metadata::capture(b, &inputs, o, e)?);
            Ok(())
        },
    )
    .unwrap();
    let feed = FeedBlock::new(&h.identity, &executed.header, captured.unwrap()).unwrap();
    let bytes = json_bytes(&feed, FEED_CAP).unwrap();
    let text = String::from_utf8(bytes).unwrap();
    assert!(!text.contains("NEVER_EXPOSE"));
    assert!(!text.contains("rolled-back"));
    assert!(
        feed.changes
            .iter()
            .any(|c| matches!(c,Change::RecordDelete{record_id,..} if record_id=="1"))
    );
    let rows = feed
        .changes
        .iter()
        .filter_map(|c| {
            if let Change::RecordUpsert {
                record_id,
                fields,
                attributes,
                ..
            } = c
            {
                Some((record_id, fields, attributes))
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(
        rows.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(),
        ["2", "3"]
    );
    assert_eq!(rows[0].1.len(), 5);
    assert!(
        rows[0]
            .2
            .iter()
            .any(|a| a.value == serde_json::json!(u64::MAX.to_string()))
    );
    assert!(
        rows[0]
            .2
            .iter()
            .any(|a| a.value == serde_json::json!(i64::MIN.to_string()))
    );
    h.import(StoredBlock {
        block: Block {
            header: executed.header,
            inputs,
            outcomes: executed.outcomes,
        },
        feed,
        workload: h.workload.clone(),
        revision: 0,
        controls: vec![],
    })
    .unwrap();
    assert_eq!(h.head.height, 2);
}
