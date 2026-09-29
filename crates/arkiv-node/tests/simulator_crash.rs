//! Actual redb write/sync cuts retain a complete parent or candidate host packet.
use arkiv_db::{
    CellName, NamespaceId, Value,
    durable::Options,
    proof::{EqQuery, EqRequest, SnapshotRef, VerifiedEq, verify_eq},
};
use arkiv_db_redb::{
    RedbConfig, RedbStore,
    testing::{CrashDisk, CrashMode, DiskOp, Granularity},
};
use arkiv_node::simulator::{
    host::{Control, Host, RunConfig},
    wire::{hex, proof_limits},
    workload::Workload,
};
use std::sync::Arc;
fn store(disk: &CrashDisk) -> Arc<RedbStore> {
    let disk = disk.clone();
    Arc::new(
        RedbStore::with_backend(
            move || Ok(disk.backend()),
            RedbConfig::with_cache_size(32 * 1024),
        )
        .unwrap(),
    )
}
fn options() -> Options {
    Options {
        cache_bytes: 0,
        ..Default::default()
    }
}
fn step(h: &Host) -> Control {
    Control {
        command_id: hex(&[19; 32]),
        run_id: h.identity.run_id.clone(),
        expected_revision: "0".into(),
        expected_height: Some("1".into()),
        action: "step".into(),
        config: None,
    }
}
fn open(image: &[u8]) -> (CrashDisk, Arc<RedbStore>, Host) {
    let disk = CrashDisk::from_image(image.to_vec());
    let store = store(&disk);
    let host = Host::open(store.clone(), None, options()).unwrap();
    (disk, store, host)
}

#[test]
fn every_redb_mutating_boundary_atomically_keeps_state_body_outbox_and_inclusions() {
    let disk = CrashDisk::new();
    let storage = store(&disk);
    let mut h = Host::create(
        storage.clone(),
        RunConfig {
            genesis: arkiv_node::demo::workload::genesis(9001, 1700000000000),
            workload: Workload::default(),
        },
        [1; 16],
        [7; 16],
        options(),
    )
    .unwrap();
    h.produce(None).unwrap();
    let parent = h.block(1).unwrap().bytes().unwrap();
    let image = disk.crash((h, storage), CrashMode::DurableOnly);
    let (disk, storage, mut h) = open(&image);
    let start = disk.mutating_trace().len();
    h.control(step(&h)).unwrap();
    let expected = h.block(2).unwrap().bytes().unwrap();
    let trace = disk.mutating_trace()[start..].to_vec();
    drop((h, storage));
    assert!(trace.contains(&DiskOp::Write));
    assert!(trace.contains(&DiskOp::Sync));
    let mut parents = 0;
    let mut candidates = 0;
    for cut in 0..=trace.len() {
        let (disk, storage, mut h) = open(&image);
        disk.crash_after(cut as u64);
        let command = step(&h);
        let outcome = h.control(command.clone());
        if outcome.is_err() {
            assert_eq!(h.head.height, 1);
            assert!(h.block(2).is_err());
        }
        disk.cut_off();
        drop((h, storage));
        for mode in [
            CrashMode::DurableOnly,
            CrashMode::AllUnsynced,
            CrashMode::Torn {
                seed: 37,
                granularity: Granularity::Sector,
            },
        ] {
            let (_, storage, mut h) = open(&disk.crash_image(mode));
            assert_eq!(h.block(1).unwrap().bytes().unwrap(), parent);
            if h.head.height == 1 {
                parents += 1;
                assert!(h.block(2).is_err());
                let mut key = vec![2];
                key.extend_from_slice(&[19; 32]);
                assert!(h.engine.simulator_inclusion(&key).unwrap().is_none());
            } else {
                candidates += 1;
                assert_eq!(h.head.height, 2);
                assert_eq!(h.block(2).unwrap().bytes().unwrap(), expected);
                assert_eq!(
                    h.engine.head_snapshot().state_root(),
                    h.head.state_root.unwrap()
                );
                assert_eq!(h.control(command.clone()).unwrap()["height"], "2");
                assert_eq!(h.head.height, 2);
                let r = EqRequest {
                    snapshot: SnapshotRef {
                        genesis_hash: h.durable_identity.node_genesis_hash,
                        height: 2,
                        block_hash: h.head.hash(),
                        state_root: h.head.state_root.unwrap(),
                    },
                    query: EqQuery {
                        namespace: NamespaceId::from_ordinal(1).unwrap(),
                        attribute: CellName::new("group").unwrap(),
                        value: Value::U64(1),
                    },
                    limit: 64,
                    continuation: None,
                };
                let bytes = h.proof(&r).unwrap();
                let VerifiedEq::Page(page) =
                    verify_eq(&r, None, &bytes, proof_limits(), h.run.genesis.limits).unwrap()
                else {
                    panic!("namespace absent")
                };
                assert_eq!(
                    page.rows.len(),
                    if cfg!(feature = "arkiv-v2") { 5 } else { 1 }
                );
            }
            if outcome.is_ok() {
                assert_eq!(h.head.height, 2);
            }
            assert!(!storage.last_open().full_repair);
        }
    }
    assert!(parents > 0 && candidates > 0);
    eprintln!(
        "simulator redb cuts={} images={} parent={parents} candidate={candidates}",
        trace.len() + 1,
        (trace.len() + 1) * 3
    );
}
