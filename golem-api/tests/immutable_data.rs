//! Immutable data: golem-db's behaviour, in memory and in files, across
//! reopening and across a failure between the two stores a commit writes.
//!
//! `contract` and `invalid_inputs…` are golem-db's own API tests
//! (`crates/integration-tests/tests/api/immutable_data.rs` at `f6dae1d`)
//! without the parts that reach into golem-db's files or stores. The random
//! scripts compare every call with golem-db's memory `Database`.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use arkiv_db_redb::testing::{CrashDisk, DiskOp};
use arkiv_db_redb::{RedbConfig, RedbStore};
use common::*;

fn genesis() -> Genesis {
    Genesis::new(
        HashAlgorithm::Keccak256,
        CellLimits {
            max_cell_name_len: 32,
            max_str_len: 4,
            max_bytes_len: 4,
        },
        RecordLimits {
            max_cells: 100,
            max_indexed_cells: 100,
        },
        Some(ImmutableDataConfig::new(
            2,
            vec![
                ImmutableDataSegment {
                    name: "events".into(),
                    columns: vec![
                        ImmutableDataColumn::new("height").with_type(CellType::Uint(Width::W8)),
                        ImmutableDataColumn::new("hash")
                            .with_type(CellType::FixedBytes(Width::W32)),
                        ImmutableDataColumn::new("payload"),
                    ],
                    compression: ImmutableDataCompression::Lz4,
                },
                ImmutableDataSegment {
                    name: "receipts".into(),
                    columns: vec![ImmutableDataColumn::new("value")],
                    compression: ImmutableDataCompression::None,
                },
            ],
        )),
        PricingConfig::zero(),
    )
}

fn row(n: u8) -> ImmutableDataRow {
    ImmutableDataRow::new(vec![
        CellValue::from_u64(u64::from(n)),
        CellValue::from_bytes32([n; 32]),
        // Longer than `max_bytes_len`: the cell limits do not apply to rows.
        CellValue::from_bytes(&[n; 128]),
    ])
}

fn rkey(n: u8) -> ImmutableDataKey {
    ImmutableDataKey([n; 32])
}

fn ordinal(n: u64) -> ImmutableDataAddress {
    ImmutableDataAddress::Ordinal(n)
}

fn by_key(n: u8) -> ImmutableDataAddress {
    ImmutableDataAddress::Key(rkey(n))
}

fn all() -> ImmutableDataReadOptions {
    ImmutableDataReadOptions::all()
}

fn assert_row(actual: ImmutableDataRow, n: u8) {
    assert_eq!(actual, row(n));
    assert_eq!(actual.get::<u64>("height").unwrap(), u64::from(n));
    assert_eq!(actual.get::<[u8; 32]>("hash").unwrap(), [n; 32]);
    assert_eq!(actual.get::<Vec<u8>>("payload").unwrap(), vec![n; 128]);
}

fn open(dir: &std::path::Path, config: &OpenConfig) -> ArkivDb {
    ArkivDb::open(dir, config, ArkivOptions::default()).unwrap()
}

/// golem-db's `contract`: schema, staging on competing branches, frames and
/// the seal, truncation, publication, projections, empty commits.
fn contract(api: &dyn Api) {
    let schema = api.immutable_data_schema("events").unwrap();
    assert_eq!(schema.columns().len(), 3);
    assert_eq!(
        schema.columns()[1].value_type,
        CellType::FixedBytes(Width::W32)
    );
    assert!(
        api.immutable_data_rows_of("events", CommitId::GENESIS, &all())
            .unwrap()
            .is_empty()
    );
    let branch = api.begin().unwrap();
    let loser = api.begin().unwrap();
    let append = |branch, key: Option<u8>, n| {
        api.immutable_data_append(branch, "events", key.map(rkey), row(n))
            .unwrap()
    };
    assert_eq!(append(branch, Some(1), 1), 0);
    assert_eq!(append(loser, Some(1), 9), 0);
    for address in [ordinal(0), by_key(1)] {
        assert!(matches!(
            api.immutable_data_get("events", address, &all()),
            Err(ApiError::NotFound)
        ));
    }
    api.checkpoint(branch).unwrap();
    assert_eq!(append(branch, Some(2), 2), 1);
    api.rollback(branch).unwrap(); // immutable rows are independent of cell frames
    let seal = api.seal(branch).unwrap();
    assert_eq!(append(branch, None, 3), 2);
    api.immutable_data_truncate(branch, "events", 1).unwrap();
    assert_eq!(append(branch, Some(2), 4), 1);
    assert_eq!(api.seal(branch).unwrap(), seal);
    let first = api.commit(branch).unwrap();
    assert!(matches!(api.commit(loser), Err(ApiError::Conflict)));
    assert!(matches!(
        api.immutable_data_truncate(branch, "events", 0),
        Err(ApiError::HandleInvalid)
    ));
    assert_row(
        api.immutable_data_get("events", ordinal(0), &all())
            .unwrap(),
        1,
    );
    assert_row(
        api.immutable_data_get("events", by_key(2), &all()).unwrap(),
        4,
    );
    let rows = api.immutable_data_rows_of("events", first, &all()).unwrap();
    assert_eq!(rows.len(), 2);
    assert_row(rows[0].clone(), 1);
    assert_row(rows[1].clone(), 4);
    assert!(
        api.immutable_data_rows_of("receipts", first, &all())
            .unwrap()
            .is_empty()
    );

    let projection = ImmutableDataReadOptions::columns(["hash", "height"]);
    let projected = api
        .immutable_data_get("events", ordinal(0), &projection)
        .unwrap();
    assert_eq!(projected.get::<[u8; 32]>("hash").unwrap(), [1; 32]);
    assert_eq!(
        projected
            .iter()
            .unwrap()
            .map(|(name, _)| name)
            .collect::<Vec<_>>(),
        vec!["hash", "height"]
    );
    assert!(matches!(
        projected.value("payload"),
        Err(ImmutableDataError::ColumnNotSelected { .. })
    ));
    assert!(projected.get::<u32>("height").is_err());
    let rows = api
        .immutable_data_rows_of("events", first, &projection)
        .unwrap();
    assert_eq!(rows[1].get::<u64>("height").unwrap(), 4);
    assert_eq!(rows[1].values().len(), 2);

    let empty = api.commit(api.begin().unwrap()).unwrap();
    assert!(
        api.immutable_data_rows_of("events", empty, &all())
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        api.immutable_data_rows_of("events", CommitId::new(empty.get() + 1), &all()),
        Err(ApiError::CommitUnavailable { requested, head })
            if requested.get() == empty.get() + 1 && head == empty
    ));
    let branch = api.begin().unwrap();
    assert_eq!(append(branch, None, 5), 2);
    api.commit(branch).unwrap();
    assert_row(
        api.immutable_data_get("events", ordinal(2), &all())
            .unwrap(),
        5,
    );
    assert_eq!(
        api.immutable_data_rows_of("events", first, &all())
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn typed_rows_projection_and_branch_lifecycle_in_memory_and_files() {
    let genesis = genesis();
    // The reference itself, then the adapter in memory and in files.
    contract(&real::Database::open_memory(&genesis).unwrap());
    contract(&ArkivDb::open_memory(&genesis, ArkivOptions::default()).unwrap());
    let dir = tempfile::tempdir().unwrap();
    let config = OpenConfig::new(genesis);
    contract(&open(dir.path(), &config));
    // golem-db's default place, beside the database.
    assert!(
        dir.path()
            .join("immutable-data")
            .join(IMMUTABLE_FILE)
            .is_file()
    );
    let db = open(
        dir.path(),
        &config.clone().with_mode(OpenMode::ExistingOnly),
    );
    assert_row(
        db.immutable_data_get("events", ordinal(0), &all()).unwrap(),
        1,
    );
    assert_row(
        db.immutable_data_get("events", by_key(2), &all()).unwrap(),
        4,
    );
    assert_row(
        db.immutable_data_get("events", ordinal(2), &all()).unwrap(),
        5,
    );
    assert_eq!(
        db.immutable_data_rows_of("events", CommitId::new(1), &all())
            .unwrap()
            .len(),
        2
    );
    // Ordinals and keys continue where the stored rows end.
    let branch = db.begin().unwrap();
    assert!(matches!(
        db.immutable_data_append(branch, "events", Some(rkey(2)), row(6)),
        Err(ApiError::AlreadyExists)
    ));
    assert_eq!(
        db.immutable_data_append(branch, "events", Some(rkey(6)), row(6))
            .unwrap(),
        3
    );
}

/// golem-db's test of the same name, on golem-db and on the adapter.
#[test]
fn invalid_inputs_duplicates_and_disabled_segments_have_public_errors() {
    let mut without = genesis();
    without.immutable_data = None;
    let dir = tempfile::tempdir().unwrap();
    let disabled_arkiv = open(dir.path(), &OpenConfig::new(without.clone()));
    // No segment declared: no directory, no file.
    assert!(!dir.path().join("immutable-data").exists());
    let pairs: [(Box<dyn Api>, Box<dyn Api>); 2] = [
        (
            Box::new(real::Database::open_memory(&genesis()).unwrap()),
            Box::new(real::Database::open_memory(&without).unwrap()),
        ),
        (
            Box::new(ArkivDb::open_memory(&genesis(), ArkivOptions::default()).unwrap()),
            Box::new(disabled_arkiv),
        ),
    ];
    for (db, disabled) in pairs {
        let id = db.begin().unwrap();
        for invalid in [
            ImmutableDataRow::new(vec![]),
            ImmutableDataRow::new(vec![
                CellValue::from_u32(1),
                CellValue::from_bytes32([1; 32]),
                CellValue::from_bytes(&[]),
            ]),
        ] {
            assert!(matches!(
                db.immutable_data_append(id, "events", None, invalid),
                Err(ApiError::InvalidArgument { .. })
            ));
        }
        db.immutable_data_append(id, "events", Some(rkey(1)), row(1))
            .unwrap();
        assert!(matches!(
            db.immutable_data_append(id, "events", Some(rkey(1)), row(2)),
            Err(ApiError::AlreadyExists)
        ));
        assert!(matches!(
            db.immutable_data_truncate(id, "events", 2),
            Err(ApiError::InvalidArgument { .. })
        ));
        db.commit(id).unwrap();
        let id = db.begin().unwrap();
        assert!(matches!(
            db.immutable_data_append(id, "events", Some(rkey(1)), row(2)),
            Err(ApiError::AlreadyExists)
        ));
        assert!(matches!(
            db.immutable_data_truncate(id, "events", 0),
            Err(ApiError::InvalidArgument { .. })
        ));
        for options in [
            ImmutableDataReadOptions::columns(["missing"]),
            ImmutableDataReadOptions::columns(["height", "height"]),
            ImmutableDataReadOptions::columns(Vec::<String>::new()),
        ] {
            assert!(matches!(
                db.immutable_data_get("events", ordinal(0), &options),
                Err(ApiError::InvalidArgument { .. })
            ));
            assert!(matches!(
                db.immutable_data_rows_of("events", CommitId::new(1), &options),
                Err(ApiError::InvalidArgument { .. })
            ));
        }
        assert!(matches!(
            db.immutable_data_get("events", ordinal(u64::MAX), &all()),
            Err(ApiError::NotFound)
        ));
        db.immutable_data_append(id, "events", Some(rkey(2)), row(2))
            .unwrap();
        db.discard(id).unwrap();
        let id = db.begin().unwrap();
        assert_eq!(
            db.immutable_data_append(id, "events", Some(rkey(2)), row(2))
                .unwrap(),
            1
        );
        db.commit(id).unwrap();

        for db in [&db, &disabled] {
            assert!(matches!(
                db.immutable_data_schema("unknown"),
                Err(ApiError::NotFound)
            ));
            assert!(matches!(
                db.immutable_data_get("unknown", ordinal(0), &all()),
                Err(ApiError::NotFound)
            ));
            assert!(matches!(
                db.immutable_data_rows_of("unknown", CommitId::GENESIS, &all()),
                Err(ApiError::NotFound)
            ));
            let branch = db.begin().unwrap();
            assert!(matches!(
                db.immutable_data_append(branch, "unknown", None, row(1)),
                Err(ApiError::NotFound)
            ));
            assert!(matches!(
                db.immutable_data_truncate(branch, "unknown", 0),
                Err(ApiError::NotFound)
            ));
            db.discard(branch).unwrap();
        }
    }
}

/// A row is readable by its key as soon as the head names its commit, while
/// another thread keeps committing.
#[test]
fn keyed_get_succeeds_while_another_thread_commits() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(open(dir.path(), &OpenConfig::new(genesis())));
    let done = Arc::new(AtomicBool::new(false));
    let reader = {
        let (db, done) = (db.clone(), done.clone());
        std::thread::spawn(move || {
            let mut errors = Vec::new();
            let mut reads = 0u32;
            while !done.load(Ordering::Relaxed) {
                let head = db.head().unwrap().get() as u8;
                if head > 0 {
                    reads += 1;
                    if let Err(error) = db.immutable_data_get("events", by_key(head), &all()) {
                        errors.push(error);
                    }
                    // And never a row of a commit the head has not reached.
                    let ahead = db.immutable_data_rows_of(
                        "events",
                        CommitId::new(u64::from(head) + 2),
                        &all(),
                    );
                    if !matches!(ahead, Err(ApiError::CommitUnavailable { .. })) {
                        errors.push(ApiError::invalid_argument(format!("read ahead: {ahead:?}")));
                    }
                }
            }
            (errors, reads)
        })
    };
    for n in 1..=120 {
        let branch = db.begin().unwrap();
        db.immutable_data_append(branch, "events", Some(rkey(n)), row(n))
            .unwrap();
        db.commit(branch).unwrap();
    }
    done.store(true, Ordering::Relaxed);
    let (errors, reads) = reader.join().unwrap();
    assert!(
        errors.is_empty(),
        "{} reads failed: {errors:?}",
        errors.len()
    );
    assert!(reads > 0);
}

/// The durable engine on a simulated device and immutable data in a real
/// directory: the two stores of one database, of which only the engine's
/// can be made to fail.
fn two_stores(disk: &CrashDisk, files: &std::path::Path) -> OpenResult<ArkivDb> {
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
    let config = OpenConfig::new(genesis()).with_immutable_data_directory(files);
    ArkivDb::open_store(Arc::new(store), "crash-disk".into(), &config, options)
}

/// A crash between the two stores: the rows of the commit are durable, the
/// engine's head never moved. On open that tail is dropped: its ordinals and
/// keys are free again, and nothing of it is readable.
#[test]
fn an_unpublished_tail_is_dropped_on_open() {
    let disk = CrashDisk::new();
    let files = tempfile::tempdir().unwrap();
    let db = two_stores(&disk, files.path()).unwrap();
    let first = db.begin().unwrap();
    db.immutable_data_append(first, "events", Some(rkey(1)), row(1))
        .unwrap();
    assert_eq!(db.commit(first).unwrap(), CommitId::new(1));

    // Step 1 succeeds (the rows of commit 2 are written), step 2 fails.
    let lost = db.begin().unwrap();
    db.immutable_data_append(lost, "events", Some(rkey(2)), row(2))
        .unwrap();
    db.immutable_data_append(lost, "receipts", None, receipt(7))
        .unwrap();
    disk.fail_all(DiskOp::Write, std::io::ErrorKind::Other);
    assert!(db.commit(lost).is_err());
    assert_eq!(db.head().unwrap(), CommitId::new(1));
    // Not readable in the process that wrote it either.
    assert!(matches!(
        db.immutable_data_get("events", ordinal(1), &all()),
        Err(ApiError::NotFound)
    ));
    assert!(matches!(
        db.immutable_data_get("events", by_key(2), &all()),
        Err(ApiError::NotFound)
    ));
    // The process dies here.
    drop(db);
    disk.clear_faults();

    let db = two_stores(&disk, files.path()).unwrap();
    assert_eq!(db.head().unwrap(), CommitId::new(1));
    assert_row(
        db.immutable_data_get("events", ordinal(0), &all()).unwrap(),
        1,
    );
    for address in [ordinal(1), by_key(2)] {
        assert!(matches!(
            db.immutable_data_get("events", address, &all()),
            Err(ApiError::NotFound)
        ));
    }
    assert!(matches!(
        db.immutable_data_get("receipts", ordinal(0), &all()),
        Err(ApiError::NotFound)
    ));
    // A commit without rows takes the number the lost one had: it must not
    // inherit the lost rows.
    assert_eq!(db.commit(db.begin().unwrap()).unwrap(), CommitId::new(2));
    assert!(
        db.immutable_data_rows_of("events", CommitId::new(2), &all())
            .unwrap()
            .is_empty()
    );
    // The ordinal and the key are free.
    let again = db.begin().unwrap();
    assert_eq!(
        db.immutable_data_append(again, "events", Some(rkey(2)), row(3))
            .unwrap(),
        1
    );
    assert_eq!(db.commit(again).unwrap(), CommitId::new(3));
    assert_row(
        db.immutable_data_get("events", by_key(2), &all()).unwrap(),
        3,
    );
    assert_eq!(
        db.immutable_data_rows_of("events", CommitId::new(3), &all())
            .unwrap(),
        vec![row(3)]
    );
}

fn receipt(n: u8) -> ImmutableDataRow {
    ImmutableDataRow::new(vec![CellValue::from_bytes(&[n; 3])])
}

/// The same failure without a crash: the branch keeps its rows and its seal,
/// and its retry publishes them once.
#[test]
fn a_failed_commit_keeps_the_staged_rows_and_can_be_retried() {
    let disk = CrashDisk::new();
    let files = tempfile::tempdir().unwrap();
    let db = two_stores(&disk, files.path()).unwrap();
    let branch = db.begin().unwrap();
    db.immutable_data_append(branch, "events", Some(rkey(1)), row(1))
        .unwrap();
    db.immutable_data_append(branch, "events", None, row(2))
        .unwrap();
    let sealed = db.seal(branch).unwrap();
    disk.fail_all(DiskOp::Write, std::io::ErrorKind::Other);
    for _ in 0..2 {
        assert!(db.commit(branch).is_err());
        assert_eq!(db.head().unwrap(), CommitId::GENESIS);
        assert!(matches!(
            db.immutable_data_get("events", ordinal(0), &all()),
            Err(ApiError::NotFound)
        ));
    }
    disk.clear_faults();
    // Still staged, still editable.
    assert_eq!(db.seal(branch).unwrap(), sealed);
    db.immutable_data_truncate(branch, "events", 1).unwrap();
    assert_eq!(
        db.immutable_data_append(branch, "events", None, row(3))
            .unwrap(),
        1
    );
    assert_eq!(db.commit(branch).unwrap(), CommitId::new(1));
    assert_eq!(
        db.immutable_data_rows_of("events", CommitId::new(1), &all())
            .unwrap(),
        vec![row(1), row(3)]
    );
    assert!(matches!(
        db.immutable_data_get("events", ordinal(2), &all()),
        Err(ApiError::NotFound)
    ));
}

/// After a failed commit, another branch that stages no rows commits under
/// the same number: the failed branch's rows must not surface with it.
#[test]
fn a_failed_commits_rows_never_surface_under_another_commit() {
    let disk = CrashDisk::new();
    let files = tempfile::tempdir().unwrap();
    let db = two_stores(&disk, files.path()).unwrap();
    let failing = db.begin().unwrap();
    let other = db.begin().unwrap();
    db.immutable_data_append(failing, "events", Some(rkey(1)), row(1))
        .unwrap();
    disk.fail_all(DiskOp::Write, std::io::ErrorKind::Other);
    assert!(db.commit(failing).is_err());
    disk.clear_faults();
    assert_eq!(db.commit(other).unwrap(), CommitId::new(1));
    assert!(
        db.immutable_data_rows_of("events", CommitId::new(1), &all())
            .unwrap()
            .is_empty()
    );
    for address in [ordinal(0), by_key(1)] {
        assert!(matches!(
            db.immutable_data_get("events", address, &all()),
            Err(ApiError::NotFound)
        ));
    }
    // The loser is stale now, and the key it staged is free.
    assert!(matches!(db.commit(failing), Err(ApiError::Conflict)));
    let next = db.begin().unwrap();
    assert_eq!(
        db.immutable_data_append(next, "events", Some(rkey(1)), row(4))
            .unwrap(),
        0
    );
    assert_eq!(db.commit(next).unwrap(), CommitId::new(2));
    assert_row(
        db.immutable_data_get("events", by_key(1), &all()).unwrap(),
        4,
    );
}

/// The directory is one deployment's, held by one database at a time, and
/// `immutable_data_directory` moves it.
#[test]
fn the_directory_belongs_to_one_open_database_of_one_deployment() {
    let dir = tempfile::tempdir().unwrap();
    let files = dir.path().join("elsewhere");
    let config = OpenConfig::new(genesis()).with_immutable_data_directory(&files);
    let db = open(&dir.path().join("a"), &config);
    assert!(files.join(IMMUTABLE_FILE).is_file());
    assert!(!dir.path().join("a").join("immutable-data").exists());
    let branch = db.begin().unwrap();
    db.immutable_data_append(branch, "events", Some(rkey(1)), row(1))
        .unwrap();
    db.commit(branch).unwrap();

    // A second database, another deployment, on the same directory.
    let mut other = genesis();
    other.cell_limits.max_str_len = 5;
    let other = OpenConfig::new(other).with_immutable_data_directory(&files);
    let held = ArkivDb::open(&dir.path().join("b"), &other, ArkivOptions::default());
    assert!(matches!(held, Err(OpenError::ImmutableData(_))), "held");
    drop(db);
    let foreign = ArkivDb::open(&dir.path().join("c"), &other, ArkivOptions::default());
    assert!(
        matches!(foreign, Err(OpenError::ImmutableData(_))),
        "foreign"
    );
    // Its owner still opens it.
    let db = open(&dir.path().join("a"), &config);
    assert_row(
        db.immutable_data_get("events", by_key(1), &all()).unwrap(),
        1,
    );
}

/// The declarations are part of the genesis identity, as on golem-db, and
/// `#params` carries them.
#[test]
fn declarations_are_part_of_the_genesis_identity() {
    const YAML: &str = r#"
hash_function: keccak-256
cell_limits: {max_cell_name_len: 1, max_str_len: 0, max_bytes_len: 0}
record_limits: {max_cells: 100, max_indexed_cells: 100}
metering: {golemdb_code_version: 1, host_code_version: 0, weights: {}}
immutable_data:
  shard_span: 4294967297
  segments:
    - name: receipts
      compression: lz4
      columns: [{name: payload}]
    - name: headers
      compression: none
      columns:
        - {name: height, type: u64}
        - {name: body}
"#;
    let mut original = Genesis::from_yaml(YAML).unwrap();
    // golem-db cannot price a read without weights (`OutOfBudget`); the
    // adapter prices nothing. Give the reference its zero prices.
    original.metering = PricingConfig::zero();
    let id = |genesis: &Genesis| {
        let golem = real::Database::open_memory(genesis).unwrap();
        let arkiv = ArkivDb::open_memory(genesis, ArkivOptions::default()).unwrap();
        assert_eq!(arkiv.info().genesis_id, golem.info().genesis_id);
        let params = |api: &dyn Api| {
            api.get(ReadTarget::Head, RecordOp::get(reserved_key("#params"), B))
                .result
                .unwrap()
        };
        assert_eq!(params(&arkiv), params(&golem));
        arkiv.info().genesis_id
    };
    let expected = id(&original);
    // Segment order in the file is not part of the identity.
    let mut reordered = original.clone();
    reordered
        .immutable_data
        .as_mut()
        .unwrap()
        .segments
        .reverse();
    assert_eq!(id(&reordered), expected);

    let dir = tempfile::tempdir().unwrap();
    drop(open(dir.path(), &OpenConfig::new(original.clone())));
    let reopen = |genesis: &Genesis| {
        ArkivDb::open(
            dir.path(),
            &OpenConfig::new(genesis.clone()),
            ArkivOptions::default(),
        )
        .map(|db| db.info().created)
    };
    assert!(matches!(reopen(&reordered), Ok(false)));
    let mut disabled = original.clone();
    disabled.immutable_data = None;
    assert_ne!(id(&disabled), expected);
    assert!(matches!(reopen(&disabled), Err(OpenError::GenesisMismatch)));
    for change in 0..9 {
        let mut different = original.clone();
        let config = different.immutable_data.as_mut().unwrap();
        match change {
            0 => config.shard_span += 1,
            1 => config.segments[0].name.push('x'),
            2 => config.segments[0].columns[0].name.push('x'),
            3 => config.segments[0].columns[0].value_type = CellType::Str,
            4 => config.segments[0].compression = ImmutableDataCompression::Zstd,
            5 => config.segments[1].columns.reverse(),
            6 => {
                config.segments.remove(0);
            }
            7 => config.segments.push(ImmutableDataSegment {
                name: "extra".into(),
                columns: vec![ImmutableDataColumn::new("v")],
                compression: ImmutableDataCompression::None,
            }),
            _ => config.segments[1].columns[0].value_type = CellType::Uint(Width::W4),
        }
        assert_ne!(id(&different), expected, "change {change}");
        assert!(
            matches!(reopen(&different), Err(OpenError::GenesisMismatch)),
            "change {change}"
        );
    }
    // An invalid declaration is refused before anything is created.
    let mut empty = original.clone();
    empty.immutable_data.as_mut().unwrap().segments.clear();
    let fresh = tempfile::tempdir().unwrap();
    let refused = ArkivDb::open(
        fresh.path(),
        &OpenConfig::new(empty),
        ArkivOptions::default(),
    );
    assert!(matches!(refused, Err(OpenError::InvalidConfig(_))));
    assert!(!fresh.path().join(FILE).exists());
}

/// Seeded random scripts of every immutable-data call, with the branch
/// calls around them, on golem-db's memory `Database` and on the adapter in
/// memory and in files: every result and error is the same.
#[test]
fn random_scripts_agree_with_golem_db() {
    for seed in 0..25 {
        script(seed, 220);
    }
}

fn script(seed: u64, steps: usize) {
    let genesis = genesis();
    let dir = tempfile::tempdir().unwrap();
    let golem = real::Database::open_memory(&genesis).unwrap();
    let memory = ArkivDb::open_memory(&genesis, ArkivOptions::default()).unwrap();
    let durable = open(dir.path(), &OpenConfig::new(genesis));
    let all_dbs: [(&str, &dyn Api); 3] = [
        ("golem-db", &golem),
        ("arkiv-memory", &memory),
        ("arkiv", &durable),
    ];
    let mut rng = Rng(seed);
    let mut branches: Vec<[BranchId; 3]> = Vec::new();
    for step in 0..steps {
        let live = |branch: &[BranchId; 3]| golem.branch_info(branch[0]).is_ok();
        if rng.chance(85) {
            branches.retain(live);
        }
        if branches.is_empty() || rng.chance(8) {
            branches.push(all_dbs.map(|(_, api)| api.begin().expect("begin")));
            continue;
        }
        let branch = branches[rng.below(branches.len())];
        let choice = rng.below(100);
        let call_seed = rng.next();
        let head = golem.head().unwrap().get();
        let run = |i: usize| -> String {
            let (_, api) = all_dbs[i];
            let b = branch[i];
            let mut rng = Rng(call_seed);
            let segment = *rng.pick(&["events", "events", "receipts", "unknown"]);
            let options = match rng.below(6) {
                0 => ImmutableDataReadOptions::columns(["payload"]),
                1 => ImmutableDataReadOptions::columns(["hash", "height"]),
                2 => ImmutableDataReadOptions::columns(["value"]),
                3 => ImmutableDataReadOptions::columns(["height", "height"]),
                _ => all(),
            };
            match choice {
                0..=34 => {
                    let n = rng.below(6) as u8;
                    let key = rng.chance(60).then(|| rkey(rng.below(8) as u8));
                    let row = match rng.below(10) {
                        0 => ImmutableDataRow::new(vec![CellValue::from_u64(1)]),
                        1 => receipt(n),
                        _ => row(n),
                    };
                    format!("{:?}", api.immutable_data_append(b, segment, key, row))
                }
                35..=44 => {
                    let from = rng.below(8) as u64;
                    format!("{:?}", api.immutable_data_truncate(b, segment, from))
                }
                45..=59 => {
                    let address = if rng.chance(50) {
                        ordinal(rng.below(10) as u64)
                    } else {
                        by_key(rng.below(8) as u8)
                    };
                    format!("{:?}", api.immutable_data_get(segment, address, &options))
                }
                60..=69 => {
                    let commit = CommitId::new((head + 2).saturating_sub(rng.below(5) as u64));
                    format!(
                        "{:?}",
                        api.immutable_data_rows_of(segment, commit, &options)
                    )
                }
                70..=72 => format!("{:?}", api.immutable_data_schema(segment)),
                73..=76 => format!("{:?}", api.checkpoint(b)),
                77..=80 => format!("{:?}", api.rollback(b)),
                81..=84 => format!("{:?}", api.seal(b).map(|sealed| sealed.commit_id)),
                85..=94 => format!("{:?}", api.commit(b)),
                95..=96 => format!("{:?}", api.discard(b)),
                _ => {
                    let op = RecordOp::create(key(1 + rng.below(3) as u8), B);
                    outcome(api.create(b, op))
                }
            }
        };
        let expected = run(0);
        for (i, (name, _)) in all_dbs.iter().enumerate().skip(1) {
            assert_eq!(
                run(i),
                expected,
                "seed {seed}, step {step}, call {choice} on {name}"
            );
        }
    }
    // Everything published reads the same at the end.
    let head = golem.head().unwrap();
    for commit in 0..=head.get() {
        for segment in ["events", "receipts"] {
            let rows = |api: &dyn Api| {
                format!(
                    "{:?}",
                    api.immutable_data_rows_of(segment, CommitId::new(commit), &all())
                )
            };
            for (name, api) in &all_dbs[1..] {
                assert_eq!(
                    rows(*api),
                    rows(&golem),
                    "seed {seed}, {segment} at {commit} on {name}"
                );
            }
        }
    }
}
