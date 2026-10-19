use arkiv_db::{
    PriceSchedule,
    durable::{DurableEngine, EngineGenesis, Initialization, Options, Publication},
};
use arkiv_db_memory::MemoryVersionedStore;
use arkiv_db_storage::versioned::{Table, VersionedStore};
use std::sync::Arc;
fn committed(i: Initialization) -> DurableEngine {
    match i {
        Initialization::Committed(e) => e,
        other => panic!("{other:?}"),
    }
}
#[test]
fn checkpoint_is_atomic_noncanonical_and_compaction_keeps_only_head_state() {
    let store = Arc::new(MemoryVersionedStore::new());
    let original = EngineGenesis::new([9; 32], PriceSchedule::FIXTURE);
    let mut stage = committed(
        DurableEngine::initialize(store.clone(), original.clone(), Options::default()).unwrap(),
    );
    for i in 1..=20u64 {
        let mut branch = stage.begin(stage.head(), None).unwrap();
        for j in 0..10u8 {
            branch
                .try_host_put([j; 32], &i.to_be_bytes())
                .unwrap()
                .unwrap();
        }
        let candidate = branch.seal();
        let root = candidate.state_header().root();
        stage.prepare_import(candidate, &i.to_be_bytes()).unwrap();
        assert_eq!(
            stage.import_checkpoint(8).unwrap(),
            if i == 1 {
                None
            } else {
                Some((i - 1).to_be_bytes().to_vec())
            }
        );
        assert!(matches!(stage.commit().unwrap(), Publication::Committed(_)));
        assert_eq!(stage.head_snapshot().state_header().root(), root);
    }
    drop(stage);
    let stage = DurableEngine::open(store, original, Options::default()).unwrap();
    assert_eq!(
        stage.import_checkpoint(8).unwrap(),
        Some(20u64.to_be_bytes().to_vec())
    );
    let mut final_genesis = EngineGenesis::new([10; 32], PriceSchedule::FIXTURE);
    final_genesis.initial_state = Some(stage.head_snapshot().state_header());
    let target = Arc::new(MemoryVersionedStore::new());
    let options = Options {
        prepare_objects: 1,
        prepare_bytes: 4096,
        ..Options::default()
    };
    let compact = committed(
        stage
            .initialize_from_staged(target.clone(), final_genesis.clone(), options.clone())
            .unwrap(),
    );
    assert_eq!(compact.head().sequence(), 0);
    assert_eq!(
        compact.head_snapshot().state_header(),
        stage.head_snapshot().state_header()
    );
    assert_eq!(
        compact.import_checkpoint(8).unwrap(),
        Some(20u64.to_be_bytes().to_vec())
    );
    assert!(
        target
            .read_record(Table::Manifests, &1u64.to_be_bytes())
            .unwrap()
            .is_none()
    );
    assert!(
        stage
            .initialize_from_staged(target.clone(), final_genesis.clone(), options.clone())
            .is_err()
    );
    drop(compact);
    let compact = DurableEngine::open(target, final_genesis, options).unwrap();
    assert_eq!(
        compact
            .head_snapshot()
            .try_host_get(&[4; 32])
            .unwrap()
            .unwrap(),
        20u64.to_be_bytes()
    );
}

#[test]
fn incomplete_copy_is_headless_and_restarts_idempotently() {
    let source = Arc::new(MemoryVersionedStore::new());
    let original = EngineGenesis::new([8; 32], PriceSchedule::FIXTURE);
    let mut stage =
        committed(DurableEngine::initialize(source, original, Options::default()).unwrap());
    let mut branch = stage.begin(stage.head(), None).unwrap();
    for i in 0..32u8 {
        branch.try_host_put([i; 32], &[i; 64]).unwrap().unwrap();
    }

    stage.prepare_import(branch.seal(), b"done").unwrap();
    assert!(matches!(stage.commit().unwrap(), Publication::Committed(_)));
    let mut genesis = EngineGenesis::new([7; 32], PriceSchedule::FIXTURE);
    genesis.initial_state = Some(stage.head_snapshot().state_header());
    let target = Arc::new(MemoryVersionedStore::new());
    let small = Options {
        prepare_bytes: 4096,
        prepare_objects: 1,
        ..Options::default()
    };

    assert!(
        stage
            .initialize_from_staged(
                Arc::new(InterruptCopy {
                    store: target.clone(),
                    calls: std::sync::atomic::AtomicUsize::new(0)
                }),
                genesis.clone(),
                small
            )
            .is_err()
    );
    assert_eq!(target.head().unwrap(), None);
    assert!(
        target
            .stats()
            .spaces
            .values()
            .map(|s| s.objects_written)
            .sum::<u64>()
            > 0
    );
    assert!(DurableEngine::open(target.clone(), genesis.clone(), Options::default()).is_err());

    let final_engine = committed(
        stage
            .initialize_from_staged(target, genesis, Options::default())
            .unwrap(),
    );
    assert_eq!(
        final_engine.head_snapshot().state_header(),
        stage.head_snapshot().state_header()
    );
}

struct InterruptCopy {
    store: Arc<MemoryVersionedStore>,
    calls: std::sync::atomic::AtomicUsize,
}
use arkiv_db_storage::versioned::{
    CommitBatch, CommitOutcome, DatabaseId, Head, Health, ObjectId, ObjectReader, SpaceId,
    StorageError, StorageErrorKind, StoreStats,
};
impl ObjectReader for InterruptCopy {
    fn read_object(&self, space: SpaceId, id: &ObjectId) -> Result<Option<Vec<u8>>, StorageError> {
        self.store.read_object(space, id)
    }
    fn read_object_bounded(
        &self,
        space: SpaceId,
        id: &ObjectId,
        cap: usize,
    ) -> Result<Option<Vec<u8>>, StorageError> {
        self.store.read_object_bounded(space, id, cap)
    }
}
impl VersionedStore for InterruptCopy {
    fn database_id(&self) -> DatabaseId {
        self.store.database_id()
    }
    fn head(&self) -> Result<Option<Head>, StorageError> {
        self.store.head()
    }
    fn read_record(&self, t: Table, k: &[u8]) -> Result<Option<Vec<u8>>, StorageError> {
        self.store.read_record(t, k)
    }
    fn read_record_bounded(
        &self,
        t: Table,
        k: &[u8],
        n: usize,
    ) -> Result<Option<Vec<u8>>, StorageError> {
        self.store.read_record_bounded(t, k, n)
    }
    fn commit(&self, b: &CommitBatch) -> CommitOutcome {
        self.store.commit(b)
    }
    fn health(&self) -> Health {
        self.store.health()
    }
    fn recover(&self) -> Result<Option<Head>, StorageError> {
        self.store.recover()
    }
    fn stats(&self) -> StoreStats {
        self.store.stats()
    }
    fn stage_objects(&self, objects: &[(SpaceId, ObjectId, Vec<u8>)]) -> CommitOutcome {
        if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 2 {
            return CommitOutcome::NotCommitted(StorageError::new(
                StorageErrorKind::Io,
                "interrupted copy",
            ));
        }
        self.store.stage_objects(objects)
    }
}
