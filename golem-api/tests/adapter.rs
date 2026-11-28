//! What is the adapter's own: storage failures, history and retention,
//! reopening, and the stand-ins it documents.

mod common;

use std::error::Error;
use std::sync::Arc;

use arkiv_db::durable::Retention;
use arkiv_db_redb::testing::{CrashDisk, DiskOp};
use arkiv_db_redb::{RedbConfig, RedbStore};
use common::*;

const KEY: RecordKey = RecordKey([0x42; 32]);
const OTHER: RecordKey = RecordKey([0x43; 32]);

fn priced(key: RecordKey, price: i32) -> RecordOp<op::Create> {
    RecordOp::create(key, B).attribute("price", CellValue::from_i32(price))
}

/// The durable backend on a simulated device, with every cache off so each
/// read reaches the device.
fn crash_db(disk: &CrashDisk, genesis: Genesis) -> ArkivDb {
    let device = disk.clone();
    let store =
        RedbStore::with_backend(move || Ok(device.backend()), RedbConfig::with_cache_size(0))
            .unwrap();
    let options = ArkivOptions {
        redb_cache: 0,
        object_cache: 0,
        object_cache_entries: 0,
        ..ArkivOptions::default()
    };
    ArkivDb::open_store(
        Arc::new(store),
        "crash-disk".into(),
        &OpenConfig::new(genesis),
        options,
    )
    .unwrap()
}

/// golem-db's contract for a storage failure: `Internal`, with the storage
/// error kept as the source.
fn assert_io(error: ApiError) {
    let kept = matches!(error, ApiError::Internal { .. })
        && error
            .source()
            .and_then(|source| source.downcast_ref::<arkiv_db::StorageError>())
            .is_some_and(|e| e.kind == arkiv_db_storage::versioned::StorageErrorKind::Io);
    assert!(kept, "expected Internal(storage I/O), got {error:?}");
}

/// Port of golem-db's `failure_contract`: failed reads and writes leave the
/// branch as it was, and a failed commit keeps its seal and can be retried.
#[test]
fn storage_failures_keep_the_branch_and_a_failed_commit_can_be_retried() {
    let disk = CrashDisk::new();
    let db = crash_db(&disk, genesis());
    // A committed record, so the failing reads must reach the device.
    let setup = db.begin().unwrap();
    db.create(setup, priced(OTHER, 1)).result.unwrap();
    db.commit(setup).unwrap();
    let branch = db.begin().unwrap();
    let other = db.begin().unwrap();
    db.create(branch, priced(KEY, 50)).result.unwrap();
    db.create(other, priced(RecordKey([7; 32]), 7))
        .result
        .unwrap();
    let pending = db
        .get(ReadTarget::Branch(branch), RecordOp::get(KEY, B))
        .result
        .unwrap();
    let info = db.branch_info(branch).unwrap();

    disk.fail_all(DiskOp::Read, std::io::ErrorKind::Other);
    assert_io(
        db.get(ReadTarget::Head, RecordOp::get(OTHER, B))
            .result
            .unwrap_err(),
    );
    assert_io(
        db.patch(branch, RecordOp::patch(OTHER, B).remove("price"))
            .result
            .unwrap_err(),
    );
    assert_io(
        db.query(
            None,
            QueryOp::from_filter(
                Filter::Exists {
                    field: CellNameRef::raw(b"price").into(),
                    ty: None,
                },
                B,
            ),
        )
        .result
        .unwrap_err(),
    );
    disk.clear_faults();
    assert_eq!(db.branch_info(branch).unwrap(), info);
    assert_eq!(
        db.get(ReadTarget::Branch(branch), RecordOp::get(KEY, B))
            .result
            .unwrap(),
        pending
    );

    disk.fail_all(DiskOp::Write, std::io::ErrorKind::Other);
    assert_io(db.commit(branch).unwrap_err());
    assert_eq!(db.head().unwrap(), CommitId::new(1));
    assert!(db.branch_info(branch).unwrap().sealed);
    let sealed = db.seal(branch).unwrap();
    disk.clear_faults();
    assert_eq!(db.commit(branch).unwrap(), sealed.commit_id);
    assert_eq!(
        db.get(ReadTarget::Head, RecordOp::get(KEY, B))
            .result
            .unwrap(),
        pending
    );
    // The branch begun before the failures was untouched by them; it only
    // lost the race.
    assert!(matches!(db.commit(other), Err(ApiError::Conflict)));
}

/// Port of golem-db's `full_mdbx_store_preserves_head_tables_and_sealed_branch`,
/// with the device's quota in place of MDBX's map size.
#[test]
fn a_full_store_is_store_full_and_keeps_the_head_and_the_sealed_branch() {
    const BIG: usize = 2 * 1024 * 1024;
    let disk = CrashDisk::new();
    let limits = CellLimits {
        max_bytes_len: BIG as u32,
        ..LIMITS
    };
    let big = Genesis::new(
        HashAlgorithm::Keccak256,
        limits,
        CAPS,
        None,
        PricingConfig::zero(),
    );
    let db = crash_db(&disk, big);
    disk.set_quota(Some(disk.visible_image().len() as u64 + (1 << 20)));
    let branch = db.begin().unwrap();
    let payload = RecordOp::create(KEY, B).field("payload", CellValue::from_bytes(&vec![1; BIG]));
    db.create(branch, payload).result.unwrap();
    let sealed = db.seal(branch).unwrap();
    for _ in 0..2 {
        assert!(matches!(db.commit(branch), Err(ApiError::StoreFull)));
        assert_eq!(db.head().unwrap(), CommitId::GENESIS);
        assert_eq!(db.seal(branch).unwrap(), sealed);
        assert!(db.branch_info(branch).unwrap().sealed);
        let record = db
            .get(ReadTarget::Branch(branch), RecordOp::get(KEY, B))
            .result
            .unwrap();
        assert_eq!(
            record.cells[b"payload".as_slice()]
                .as_bytes()
                .map(<[u8]>::len),
            Some(BIG)
        );
        assert!(matches!(
            db.get(ReadTarget::Head, RecordOp::get(KEY, B)).result,
            Err(ApiError::NotFound)
        ));
    }
    db.discard(branch).unwrap();
    let branch = db.begin().unwrap();
    let small = RecordOp::create(KEY, B).field("payload", CellValue::from_bytes(b"small"));
    db.create(branch, small).result.unwrap();
    assert_eq!(db.commit(branch).ok(), Some(CommitId::new(1)));
}

fn commit_price(db: &ArkivDb, price: i32) -> CommitId {
    let branch = db.begin().unwrap();
    let existing = db
        .get(ReadTarget::Head, RecordOp::get(KEY, B))
        .result
        .is_ok();
    if existing {
        let patch = RecordOp::patch(KEY, B).attribute("price", CellValue::from_i32(price));
        db.patch(branch, patch).result.unwrap();
    } else {
        db.create(branch, priced(KEY, price)).result.unwrap();
    }
    db.commit(branch).unwrap()
}

fn price_at(db: &ArkivDb, commit: u64) -> std::result::Result<i32, String> {
    db.get(
        ReadTarget::Commit(CommitId::new(commit)),
        RecordOp::get(KEY, B),
    )
    .result
    .map(|record| record.cells[b"price".as_slice()].as_i32().unwrap())
    .map_err(|error| format!("{error:?}"))
}

fn count_at(db: &ArkivDb, commit: u64, price: i32) -> std::result::Result<u64, String> {
    let filter = Filter::Eq {
        field: CellNameRef::raw(b"price").into(),
        value: EqValue::try_from(CellValue::from_i32(price)).unwrap(),
    };
    db.count(Some(CommitId::new(commit)), CountOp::filter(filter, B))
        .result
        .map_err(|error| format!("{error:?}"))
}

/// golem-db `main` serves the head only, and so does the adapter by
/// default. With `serve_history` it serves what the engine retains: with a
/// window of 1, the head and one commit before it, on both backends.
#[test]
fn history_is_served_only_on_request_and_within_the_retention_window() {
    let dir = tempfile::tempdir().unwrap();
    let head_only = ArkivDb::open(
        dir.path(),
        &OpenConfig::new(genesis()),
        ArkivOptions::default(),
    )
    .unwrap();
    for price in 1..=3 {
        commit_price(&head_only, price);
    }
    assert_eq!(price_at(&head_only, 3), Ok(3));
    assert!(
        price_at(&head_only, 2)
            .unwrap_err()
            .contains("CommitUnavailable")
    );
    assert!(
        count_at(&head_only, 2, 2)
            .unwrap_err()
            .contains("CommitUnavailable")
    );

    let options = ArkivOptions {
        retention: Retention::Window(1),
        serve_history: true,
        ..ArkivOptions::default()
    };
    let dir = tempfile::tempdir().unwrap();
    let durable = ArkivDb::open(dir.path(), &OpenConfig::new(genesis()), options).unwrap();
    let memory = ArkivDb::open_memory(&genesis(), options).unwrap();
    for db in [&durable, &memory] {
        for price in 1..=4 {
            commit_price(db, price);
        }
        assert_eq!(price_at(db, 4), Ok(4), "{}", db.description);
        assert_eq!(price_at(db, 3), Ok(3), "{}", db.description);
        assert_eq!(count_at(db, 3, 3), Ok(1), "{}", db.description);
        assert_eq!(count_at(db, 3, 4), Ok(0), "{}", db.description);
        for gone in [2, 1, 0, 5] {
            let error = price_at(db, gone).unwrap_err();
            assert!(
                error.contains("CommitUnavailable"),
                "{}: {error}",
                db.description
            );
        }
    }

    // golem-db's node-local pruning window is the engine's retention window.
    let dir = tempfile::tempdir().unwrap();
    let config = OpenConfig::new(genesis()).with_pruning(PruningConfig::new(2));
    let pruned = ArkivDb::open(
        dir.path(),
        &config,
        ArkivOptions {
            serve_history: true,
            ..ArkivOptions::default()
        },
    )
    .unwrap();
    for price in 1..=5 {
        commit_price(&pruned, price);
    }
    assert_eq!(price_at(&pruned, 3), Ok(3));
    assert!(
        price_at(&pruned, 2)
            .unwrap_err()
            .contains("CommitUnavailable")
    );
}

/// Commits that share a durable commit (`redb_durable_every`) are all
/// there after the database is dropped and opened again.
#[test]
fn shared_durable_commits_survive_a_normal_reopen() {
    let options = ArkivOptions {
        redb_durable_every: 8,
        ..ArkivOptions::default()
    };
    let dir = tempfile::tempdir().unwrap();
    let config = OpenConfig::new(genesis());
    let db = ArkivDb::open(dir.path(), &config, options).unwrap();
    for price in 1..=11 {
        commit_price(&db, price);
    }
    drop(db);
    for options in [options, ArkivOptions::default()] {
        let db = ArkivDb::open(dir.path(), &config, options).unwrap();
        assert!(!db.info().created);
        assert_eq!(db.info().commit_id, CommitId::new(11));
        assert_eq!(price_at(&db, 11), Ok(11));
    }
}

/// The stand-ins the kit documents, pinned so that a change is deliberate.
#[test]
fn documented_stand_ins() {
    let backends = Backends::new();
    for db in backends.adapters() {
        // No separate index root, no price, no ledger.
        assert_eq!(db.info().index_root, [0; 32]);
        let branch = db.begin().unwrap();
        let created = db.create(branch, priced(KEY, 1));
        assert_eq!(created.receipt.cost, 0);
        assert!(created.receipt.ledger.is_empty());
        assert_eq!(created.receipt.details.cells_created, 1);
        let sealed = db.seal(branch).unwrap();
        assert_eq!(sealed.index_root, [0; 32]);
        assert_ne!(sealed.state_root, db.info().state_root);
        db.commit(branch).unwrap();
        // The genesis model, as golem-db reports it while none is activated.
        let model = db.metering_model(None).unwrap();
        let golem = backends.golem.metering_model(None).unwrap();
        assert_eq!(model.active, golem.active);
        assert_eq!(model.pending, None);
        assert_eq!(
            db.model_weights(None, None).unwrap().weights,
            backends.golem.model_weights(None, None).unwrap().weights
        );
        assert!(matches!(
            db.model_weights(None, Some(2)),
            Err(ApiError::NotFound)
        ));
        // Immutable data is not declared: `NotFound`, as on golem-db.
        assert!(matches!(
            db.immutable_data_schema("log"),
            Err(ApiError::NotFound)
        ));
        assert!(matches!(
            backends.golem.immutable_data_schema("log"),
            Err(ApiError::NotFound)
        ));
    }
}

/// Blake3 is refused at open, on both backends, before anything is written.
#[test]
fn a_blake3_genesis_is_refused() {
    let blake3 = Genesis::new(
        HashAlgorithm::Blake3,
        LIMITS,
        CAPS,
        None,
        PricingConfig::zero(),
    );
    let dir = tempfile::tempdir().unwrap();
    let durable = ArkivDb::open(
        dir.path(),
        &OpenConfig::new(blake3.clone()),
        ArkivOptions::default(),
    );
    assert!(matches!(durable, Err(OpenError::InvalidConfig(_))));
    assert!(!dir.path().join(FILE).exists());
    let memory = ArkivDb::open_memory(&blake3, ArkivOptions::default());
    assert!(matches!(memory, Err(OpenError::InvalidConfig(_))));
}

/// The adapter computes golem-db's genesis identity itself; it is the value
/// golem-db's own engine reports, and it takes part in `GenesisMismatch`.
#[test]
fn genesis_id_is_golem_dbs_and_reopening_checks_every_setting() {
    let mut priced = PricingConfig::zero();
    for weight in priced.weights.values_mut() {
        *weight = 3;
    }
    let with =
        |limits, caps, pricing| Genesis::new(HashAlgorithm::Keccak256, limits, caps, None, pricing);
    let fewer = RecordLimits {
        max_cells: 5,
        max_indexed_cells: 0,
    };
    let variants = [
        genesis(),
        with(LIMITS, CAPS, priced),
        with(LIMITS, fewer, PricingConfig::zero()),
        with(
            CellLimits {
                max_str_len: 100,
                ..LIMITS
            },
            CAPS,
            PricingConfig::zero(),
        ),
    ];
    let mut ids = Vec::new();
    for genesis in &variants {
        let golem = real::Database::open_memory(genesis).unwrap();
        let arkiv = ArkivDb::open_memory(genesis, ArkivOptions::default()).unwrap();
        assert_eq!(arkiv.info().genesis_id, golem.info().genesis_id);
        assert_eq!(arkiv.info().commit_id, golem.info().commit_id);
        assert!(arkiv.info().created);
        ids.push(arkiv.info().genesis_id);
    }
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), variants.len());

    let dir = tempfile::tempdir().unwrap();
    let open = |genesis: &Genesis, mode| {
        let config = OpenConfig::new(genesis.clone()).with_mode(mode);
        ArkivDb::open(dir.path(), &config, ArkivOptions::default()).map(|db| *db.info())
    };
    let created = open(&variants[0], OpenMode::CreateNew).unwrap();
    for other in &variants[1..] {
        assert!(matches!(
            open(other, OpenMode::CreateIfMissing),
            Err(OpenError::GenesisMismatch)
        ));
    }
    let reopened = open(&variants[0], OpenMode::ExistingOnly).unwrap();
    assert!(!reopened.created);
    assert_eq!(reopened.genesis_id, created.genesis_id);
    assert_eq!(reopened.state_root, created.state_root);
}

/// `table_stats` reports redb's tables and the engine's counts in the load
/// tool's `table,entries,depth,pages,mib` shape.
#[test]
fn table_stats_report_redb_tables_and_engine_counts() {
    let backends = Backends::new();
    for db in backends.adapters() {
        let branch = db.begin().unwrap();
        for n in 1..=3 {
            db.create(branch, priced(key(n), n as i32)).result.unwrap();
        }
        db.commit(branch).unwrap();
        let lines = db.table_stats().unwrap();
        let entries = |name: &str| {
            lines
                .iter()
                .find_map(|line| line.strip_prefix(&format!("{name},")))
                .and_then(|rest| rest.split(',').next()?.parse::<u64>().ok())
        };
        assert_eq!(entries("live:records"), Some(3), "{lines:?}");
        // One value term and one existence term per distinct price.
        assert_eq!(entries("live:terms"), Some(4), "{lines:?}");
        assert!(
            lines.iter().all(|line| line.split(',').count() == 5),
            "{lines:?}"
        );
    }
    let durable = backends.durable.table_stats().unwrap();
    for table in [
        "objects:catalog",
        "objects:namespace",
        "records",
        "file",
        "object-cache",
    ] {
        assert!(
            durable.iter().any(|line| line.starts_with(table)),
            "{durable:?}"
        );
    }
}
