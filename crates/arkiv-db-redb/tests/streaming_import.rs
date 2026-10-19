mod common;
use arkiv_db::{
    PriceSchedule,
    durable::{DurableEngine, EngineGenesis, Initialization, Options, Publication},
};
use arkiv_db_redb::{RedbConfig, RedbStore};
use arkiv_db_storage::versioned::{CommitOutcome, ObjectReader, SpaceId, Table, VersionedStore};
use common::{BenchDir, keccak};
use std::sync::Arc;
fn committed(i: Initialization) -> DurableEngine {
    match i {
        Initialization::Committed(e) => e,
        other => panic!("{other:?}"),
    }
}
#[test]
fn headless_objects_survive_reopen_and_cannot_mutate_published_store() {
    let dir = BenchDir::new("headless-import");
    let path = dir.file("target.redb");
    let bytes = b"immutable staging object".to_vec();
    let id = keccak(&bytes);
    let batch = vec![(SpaceId::Host, id, bytes.clone())];
    let store = RedbStore::create(&path, RedbConfig::default()).unwrap();
    assert_eq!(store.stage_objects(&batch), CommitOutcome::Committed);
    assert_eq!(store.head().unwrap(), None);
    drop(store);
    let store = Arc::new(RedbStore::open(&path, RedbConfig::default()).unwrap());
    assert_eq!(store.read_object(SpaceId::Host, &id).unwrap(), Some(bytes));
    assert_eq!(store.stage_objects(&batch), CommitOutcome::Committed);
    let genesis = EngineGenesis::new([1; 32], PriceSchedule::FIXTURE);
    assert!(DurableEngine::open(store.clone(), genesis.clone(), Options::default()).is_err());
    let _engine =
        committed(DurableEngine::initialize(store.clone(), genesis, Options::default()).unwrap());
    assert!(matches!(
        store.stage_objects(&batch),
        CommitOutcome::NotCommitted(_)
    ));
}
#[test]
fn redb_compacts_streamed_state_and_reopens_at_height_zero() {
    let dir = BenchDir::new("bounded-import");
    let source =
        Arc::new(RedbStore::create(dir.file("source.redb"), RedbConfig::default()).unwrap());
    let genesis = EngineGenesis::new([2; 32], PriceSchedule::FIXTURE);
    let mut engine =
        committed(DurableEngine::initialize(source.clone(), genesis, Options::default()).unwrap());
    for i in 0..10u8 {
        let mut branch = engine.begin(engine.head(), None).unwrap();
        for j in 0..20u8 {
            branch.try_host_put([j; 32], &[i; 128]).unwrap().unwrap();
        }
        engine.prepare_import(branch.seal(), &[i]).unwrap();
        assert!(matches!(
            engine.commit().unwrap(),
            Publication::Committed(_)
        ));
    }
    let header = engine.head_snapshot().state_header();
    let target_path = dir.file("target.redb");
    let target = Arc::new(RedbStore::create(&target_path, RedbConfig::default()).unwrap());
    let mut final_genesis = EngineGenesis::new([3; 32], PriceSchedule::FIXTURE);
    final_genesis.initial_state = Some(header.clone());
    let options = Options {
        prepare_bytes: 4096,
        prepare_objects: 2,
        ..Options::default()
    };
    let compact = committed(
        engine
            .initialize_from_staged(target.clone(), final_genesis.clone(), options.clone())
            .unwrap(),
    );
    assert_eq!(compact.head().sequence(), 0);
    assert!(
        target
            .read_record(Table::Manifests, &1u64.to_be_bytes())
            .unwrap()
            .is_none()
    );
    assert!(
        target
            .stats()
            .spaces
            .values()
            .map(|s| s.objects_written)
            .sum::<u64>()
            < source
                .stats()
                .spaces
                .values()
                .map(|s| s.objects_written)
                .sum::<u64>()
    );
    drop((compact, target));
    let target = Arc::new(RedbStore::open(&target_path, RedbConfig::default()).unwrap());
    let compact = DurableEngine::open(target, final_genesis, options).unwrap();
    assert_eq!(compact.head_snapshot().state_header(), header);
    assert_eq!(compact.import_checkpoint(1).unwrap(), Some(vec![9]));
    assert_eq!(
        compact
            .head_snapshot()
            .try_host_get(&[5; 32])
            .unwrap()
            .unwrap(),
        vec![9; 128]
    );
}
