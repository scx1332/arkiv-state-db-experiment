use arkiv_db::{
    Budget, Cell, CellName, Literal, NamespaceId, NamespaceName, OwnerId, PriceSchedule,
    Projection, Query, RecordKey, Term, Value, WriteOp,
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
                    calls: std::sync::atomic::AtomicUsize::new(0),
                    fail_on: 2,
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

#[test]
fn shared_postings_survive_copy_interruptions_and_reopen() {
    let source = Arc::new(MemoryVersionedStore::new());
    let original = EngineGenesis::new([12; 32], PriceSchedule::FIXTURE);
    let mut stage =
        committed(DurableEngine::initialize(source, original, Options::default()).unwrap());
    let owner = OwnerId::new([1; 20]);
    let mut branch = stage.begin(stage.head(), None).unwrap();
    for ordinal in 1..=2 {
        let namespace = NamespaceId::from_ordinal(ordinal).unwrap();
        branch
            .execute(
                owner,
                WriteOp::CreateNamespace {
                    name: NamespaceName::new(&format!("copy{ordinal}")).unwrap(),
                },
                Budget::unlimited(),
            )
            .unwrap();
        for i in 0..32u8 {
            branch
                .execute(
                    owner,
                    WriteOp::CreateRecord {
                        namespace,
                        key: RecordKey::new(&[i]).unwrap(),
                        // Both existence and value terms share the same tree,
                        // and the second namespace has identical posting roots.
                        cells: ["first", "second"]
                            .into_iter()
                            .map(|name| {
                                (CellName::new(name).unwrap(), Cell::attribute(Value::U64(7)))
                            })
                            .collect(),
                    },
                    Budget::unlimited(),
                )
                .unwrap();
        }
    }
    stage.prepare_import(branch.seal(), b"shared").unwrap();
    assert!(matches!(stage.commit().unwrap(), Publication::Committed(_)));
    let mut genesis = EngineGenesis::new([13; 32], PriceSchedule::FIXTURE);
    genesis.initial_state = Some(stage.head_snapshot().state_header());
    let options = Options {
        prepare_bytes: 4096,
        prepare_objects: 2,
        ..Options::default()
    };
    let complete = Arc::new(InterruptCopy {
        store: Arc::new(MemoryVersionedStore::new()),
        calls: std::sync::atomic::AtomicUsize::new(0),
        fail_on: usize::MAX,
    });
    committed(
        stage
            .initialize_from_staged(complete.clone(), genesis.clone(), options.clone())
            .unwrap(),
    );
    let calls = complete.calls.load(std::sync::atomic::Ordering::SeqCst);
    assert!(calls > 2);
    // Include the final flush, after the repeated posting references were seen.
    for fail_on in [0, calls / 2, calls - 1] {
        let target = Arc::new(MemoryVersionedStore::new());
        assert!(
            stage
                .initialize_from_staged(
                    Arc::new(InterruptCopy {
                        store: target.clone(),
                        calls: std::sync::atomic::AtomicUsize::new(0),
                        fail_on,
                    }),
                    genesis.clone(),
                    options.clone(),
                )
                .is_err()
        );
        assert_eq!(target.head().unwrap(), None);
        assert!(
            target
                .read_record(Table::Meta, b"engine/genesis")
                .unwrap()
                .is_none()
        );
        committed(
            stage
                .initialize_from_staged(target.clone(), genesis.clone(), options.clone())
                .unwrap(),
        );
        let copied = DurableEngine::open(target, genesis.clone(), options.clone()).unwrap();
        assert_eq!(
            copied.head_snapshot().state_header(),
            stage.head_snapshot().state_header()
        );
        assert_eq!(
            copied.import_checkpoint(6).unwrap(),
            Some(b"shared".to_vec())
        );
        for ordinal in 1..=2 {
            let namespace = NamespaceId::from_ordinal(ordinal).unwrap();
            for name in ["first", "second"] {
                let query = Query::all(vec![Literal::of(Term::eq(
                    CellName::new(name).unwrap(),
                    Value::U64(7),
                ))]);
                assert_eq!(
                    copied
                        .head_snapshot()
                        .try_count(namespace, &query, Budget::unlimited())
                        .unwrap()
                        .unwrap()
                        .value,
                    32
                );
            }
            let key = RecordKey::new(&[31]).unwrap();
            let original = stage
                .head_snapshot()
                .try_get_record(namespace, &key, &Projection::ALL, Budget::unlimited())
                .unwrap()
                .unwrap()
                .value;
            let actual = copied
                .head_snapshot()
                .try_get_record(namespace, &key, &Projection::ALL, Budget::unlimited())
                .unwrap()
                .unwrap()
                .value;
            assert_eq!(actual, original);
        }
    }
}

struct InterruptCopy {
    store: Arc<MemoryVersionedStore>,
    calls: std::sync::atomic::AtomicUsize,
    fail_on: usize,
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
        if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == self.fail_on {
            return CommitOutcome::NotCommitted(StorageError::new(
                StorageErrorKind::Io,
                "interrupted copy",
            ));
        }
        self.store.stage_objects(objects)
    }
}

#[test]
fn bulk_stage_interruptions_never_publish_and_clean_rebuild_succeeds() {
    use arkiv_db::{Limits, NamespaceDescriptor, Revision, durable::BulkGenesisBuilder};
    let directory =
        std::env::temp_dir().join(format!("arkiv-bulk-interrupt-{}", std::process::id()));
    std::fs::create_dir(&directory).unwrap();
    let descriptor = NamespaceDescriptor {
        id: NamespaceId::from_ordinal(1).unwrap(),
        name: NamespaceName::new("entities").unwrap(),
        owner: OwnerId::new([1; 20]),
        revision: Revision::INITIAL,
    };
    let options = Options {
        prepare_objects: 2,
        prepare_bytes: 4096,
        ..Options::default()
    };
    let make = |name: &str| {
        let mut builder =
            BulkGenesisBuilder::new(&directory.join(name), descriptor, Limits::DEFAULT, 4096)
                .unwrap();
        for n in 0..32u8 {
            builder
                .add_record(
                    RecordKey::new(&[n]).unwrap(),
                    vec![(
                        CellName::new("shared").unwrap(),
                        Cell::attribute(Value::U64(7)),
                    )],
                )
                .unwrap();
        }
        builder
    };
    let complete = Arc::new(InterruptCopy {
        store: Arc::new(MemoryVersionedStore::new()),
        calls: std::sync::atomic::AtomicUsize::new(0),
        fail_on: usize::MAX,
    });
    let expected = make("complete")
        .finish(complete.clone(), PriceSchedule::FIXTURE, options.clone())
        .unwrap()
        .state_header()
        .clone();
    let calls = complete.calls.load(std::sync::atomic::Ordering::SeqCst);
    assert!(calls > 2);
    for (i, fail_on) in [0, calls / 2, calls - 1].into_iter().enumerate() {
        let target = Arc::new(MemoryVersionedStore::new());
        let failing = Arc::new(InterruptCopy {
            store: target.clone(),
            calls: std::sync::atomic::AtomicUsize::new(0),
            fail_on,
        });
        assert!(
            make(&format!("failed{i}"))
                .finish(failing, PriceSchedule::FIXTURE, options.clone())
                .is_err()
        );
        assert!(target.head().unwrap().is_none());
        assert!(
            target
                .read_record(Table::Meta, b"engine/genesis")
                .unwrap()
                .is_none()
        );
        let build = make(&format!("retry{i}"))
            .finish(target.clone(), PriceSchedule::FIXTURE, options.clone())
            .unwrap();
        assert_eq!(build.state_header(), &expected);
        let mut genesis = EngineGenesis::new([42; 32], PriceSchedule::FIXTURE);
        genesis.initial_state = Some(expected.clone());
        committed(DurableEngine::initialize_bulk(build, genesis.clone(), options.clone()).unwrap());
        assert_eq!(
            DurableEngine::open(target, genesis, options.clone())
                .unwrap()
                .head_snapshot()
                .state_header(),
            expected
        );
    }
    std::fs::remove_dir_all(directory).unwrap();
}
