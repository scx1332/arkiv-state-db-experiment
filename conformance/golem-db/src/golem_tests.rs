//! golem-db's own API behavioral tests, ported to run through the arkiv
//! adapter (`ArkivDb`). Sources:
//!   golem-db/crates/integration-tests/tests/api/facade.rs
//!   golem-db/crates/integration-tests/tests/api/facade_errors.rs
//!   golem-db/crates/integration-tests/tests/api/opening.rs
//!   golem-db/crates/integration-tests/tests/api/immutable_data.rs
//!
//! Porting rules:
//! - `each_backend` runs the arkiv memory engine and the arkiv durable
//!   (redb) engine instead of golem memory/MDBX x {keccak, blake3}; the
//!   arkiv engine supports keccak-256 only (a Blake3 genesis is refused at
//!   open, see `blake3_genesis_is_refused_at_open`), so the hash loop is
//!   dropped.
//! - Every `assert!`/`assert_eq!` became a *soft* check (`check!`,
//!   `check_eq!`): a failing check is recorded with its line and the test
//!   continues, so one run lists every divergent assertion. `.unwrap()`s are
//!   kept: an unexpected error still stops the test.
//! - Assertions on golem-internal structure (system records `#alloc`,
//!   `#recordKeys`, `#roots`) are kept where they can be expressed through
//!   the Api, marked `// golem-internal`, and indexing that would panic was
//!   turned into `.get()` so the check can fail softly.
//! - golem's `Database::info()` / `genesis()` are `ArkivDb::info()` /
//!   `genesis()`; golem's storage-fault stores are a `CrashDisk` under the
//!   durable backend's redb store.

use std::cell::RefCell;
use std::sync::{Arc, Barrier};

use golemdb_api::*;
use golemdb_cells::system;

use crate::arkiv::{self, ArkivDb, ArkivOptions};

thread_local! {
    static FAILURES: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    static BACKEND: RefCell<&'static str> = const { RefCell::new("") };
}

fn record_failure(line: u32, what: String) {
    let backend = BACKEND.with(|b| *b.borrow());
    FAILURES.with(|f| {
        f.borrow_mut()
            .push(format!("[{backend}] line {line}: {what}"))
    });
}

macro_rules! check {
    ($cond:expr) => {
        if !($cond) {
            record_failure(line!(), stringify!($cond).to_string());
        }
    };
}

macro_rules! check_eq {
    ($a:expr, $b:expr) => {{
        let (a, b) = (&$a, &$b);
        if a != b {
            record_failure(
                line!(),
                format!(
                    "{} == {}: left {:?}, right {:?}",
                    stringify!($a),
                    stringify!($b),
                    a,
                    b
                ),
            );
        }
    }};
}

/// Panics with every recorded soft failure.
fn finish() {
    let failures = FAILURES.with(|f| std::mem::take(&mut *f.borrow_mut()));
    if !failures.is_empty() {
        panic!(
            "{} divergent check(s):\n  {}",
            failures.len(),
            failures.join("\n  ")
        );
    }
}

type Db = Arc<ArkivDb>;

const KEY: RecordKey = RecordKey([0x42; 32]);
const MISSING: RecordKey = RecordKey([0x43; 32]);

fn limits() -> CellLimits {
    CellLimits {
        max_cell_name_len: 32,
        max_str_len: 64,
        max_bytes_len: 128,
    }
}

fn genesis() -> Genesis {
    Genesis::new(HashAlgorithm::Keccak256, limits())
}

fn config() -> OpenConfig {
    OpenConfig::new(genesis())
}

fn options() -> ArkivOptions {
    ArkivOptions {
        redb_cache: 8 << 20,
        object_cache: 8 << 20,
        object_cache_entries: 16_384,
        retention: ArkivOptions::retention_from_env(),
    }
}

/// golem's `Database::open(dir, config)`: the durable backend.
fn open(dir: &std::path::Path, config: &OpenConfig) -> OpenResult<ArkivDb> {
    ArkivDb::open(dir, config, options())
}

/// golem's reopen of an existing database.
fn reopen(dir: &std::path::Path, genesis: &Genesis) -> OpenResult<ArkivDb> {
    open(
        dir,
        &OpenConfig::new(*genesis).with_mode(OpenMode::ExistingOnly),
    )
}

fn input(key: RecordKey, price: i32) -> RecordOp<op::Create> {
    RecordOp::create(key)
        .attribute("price", CellValue::from_i32(price))
        .unwrap()
        .field("description", CellValue::from_str("product"))
        .unwrap()
}

fn each_backend(contract: fn(Db)) {
    BACKEND.with(|b| *b.borrow_mut() = "arkiv-memory");
    contract(Arc::new(
        ArkivDb::open_memory(&genesis(), options()).unwrap(),
    ));
    BACKEND.with(|b| *b.borrow_mut() = "arkiv-redb");
    {
        let dir = tempfile::tempdir().unwrap();
        contract(Arc::new(open(dir.path(), &config()).unwrap()));
    }
    finish();
}

fn get(api: &dyn Api, target: ReadTarget) -> Record {
    api.get(target, RecordOp::get(KEY)).unwrap()
}

// golem-internal: the #alloc system record.
fn allocator(api: &dyn Api, target: ReadTarget) -> Option<u64> {
    api.get(
        target,
        RecordOp::get(RecordKey(system::ALLOC.key)).only(["#nextRecordID"]),
    )
    .unwrap()
    .cells
    .get(b"#nextRecordID".as_slice())
    .and_then(|v| v.as_u64())
}

#[test]
fn crud_pending_reads_projections_and_recreation() {
    each_backend(|db| {
        let api: Arc<dyn Api + Send + Sync> = db.clone();
        check_eq!(api.head().unwrap(), CommitId::new(0));
        let branch = api.begin().unwrap();
        let other = api.begin().unwrap();
        check_eq!(api.create(branch, input(KEY, 50)).unwrap(), KEY);
        check!(matches!(
            api.create(branch, input(KEY, 60)),
            Err(ApiError::AlreadyExists)
        ));
        for target in [
            ReadTarget::Head,
            ReadTarget::Commit(CommitId::new(0)),
            ReadTarget::Branch(other),
        ] {
            check!(matches!(
                api.get(target, RecordOp::get(KEY)),
                Err(ApiError::NotFound)
            ));
        }
        let pending = get(api.as_ref(), ReadTarget::Branch(branch));
        check_eq!(pending.key, KEY);
        check_eq!(pending.cells.len(), 3);
        check_eq!(
            pending
                .cells
                .get(b"#key".as_slice())
                .and_then(|v| v.as_bytes32()),
            Some(KEY.0)
        );
        check!(pending.cells[b"price".as_slice()].is_indexable());
        let selected = api
            .get(
                ReadTarget::Branch(branch),
                RecordOp::get(KEY).only(["price", "missing", "price"]),
            )
            .unwrap();
        check_eq!(selected.cells.len(), 1);
        check_eq!(selected.cells[b"price".as_slice()].as_i32(), Some(50));
        check!(
            api.get(
                ReadTarget::Branch(branch),
                RecordOp::get(KEY).only([] as [&str; 0])
            )
            .unwrap()
            .cells
            .is_empty()
        );
        check!(matches!(
            api.get(
                ReadTarget::Branch(branch),
                RecordOp::get(MISSING).only([] as [&str; 0])
            ),
            Err(ApiError::NotFound)
        ));
        // golem-internal: the #recordKeys binding record.
        let bindings = api
            .get(
                ReadTarget::Branch(branch),
                RecordOp::get(RecordKey(system::RECORD_KEYS.key)).only([KEY.0]),
            )
            .unwrap();
        check_eq!(
            bindings
                .cells
                .get(KEY.0.as_slice())
                .and_then(|v| v.as_u64()),
            Some(64)
        );

        api.patch(
            branch,
            RecordOp::patch(KEY)
                .field("price", CellValue::from_i32(75))
                .unwrap()
                .remove("description")
                .unwrap(),
        )
        .unwrap();
        let edited = get(api.as_ref(), ReadTarget::Branch(branch));
        check_eq!(edited.cells[b"price".as_slice()].as_i32(), Some(75));
        check!(!edited.cells[b"price".as_slice()].is_indexable());
        let before = api.branch_info(branch).unwrap();
        api.patch(branch, RecordOp::patch(KEY).remove("absent").unwrap())
            .unwrap();
        check_eq!(api.branch_info(branch).unwrap(), before);
        check_eq!(api.commit(branch).unwrap(), CommitId::new(1));
        check_eq!(get(api.as_ref(), ReadTarget::Head), edited);
        check_eq!(
            get(api.as_ref(), ReadTarget::Commit(CommitId::new(1))),
            edited
        );
        check_eq!(db.info().commit_id, CommitId::new(0)); // opening metadata is not a live head
        check_eq!(db.head().unwrap(), CommitId::new(1));
        // golem-db has no history: reading the older commit 0 must fail.
        let old = api.get(ReadTarget::Commit(CommitId::new(0)), RecordOp::get(KEY));
        check!(matches!(
            old,
            Err(ApiError::CommitUnavailable {
                requested, head }) if requested.get() == 0 && head.get() == 1
        ));
        check!(matches!(api.commit(other), Err(ApiError::Conflict)));

        let deleting = api.begin().unwrap();
        api.delete(deleting, RecordOp::delete(KEY)).unwrap();
        check!(matches!(
            api.get(ReadTarget::Branch(deleting), RecordOp::get(KEY)),
            Err(ApiError::NotFound)
        ));
        check_eq!(get(api.as_ref(), ReadTarget::Head), edited);
        api.commit(deleting).unwrap();
        check!(matches!(
            api.get(ReadTarget::Head, RecordOp::get(KEY)),
            Err(ApiError::NotFound)
        ));
        let recreated = api.begin().unwrap();
        api.create(recreated, input(KEY, 100)).unwrap();
        check_eq!(
            allocator(api.as_ref(), ReadTarget::Branch(recreated)),
            Some(66)
        ); // golem-internal
        api.commit(recreated).unwrap();
        check_eq!(
            get(api.as_ref(), ReadTarget::Head).cells[b"price".as_slice()].as_i32(),
            Some(100)
        );
    });
}

#[test]
fn checkpoints_rollback_seal_and_discard_share_state_across_clones() {
    each_backend(|db| {
        let clone = db.clone();
        let branch = db.begin().unwrap();
        clone.create(branch, input(KEY, 50)).unwrap();
        let before = get(db.as_ref(), ReadTarget::Branch(branch));
        db.checkpoint(branch).unwrap();
        clone
            .patch(
                branch,
                RecordOp::patch(KEY)
                    .attribute("price", CellValue::from_i32(90))
                    .unwrap(),
            )
            .unwrap();
        db.rollback(branch).unwrap();
        check_eq!(get(clone.as_ref(), ReadTarget::Branch(branch)), before);
        clone.rollback(branch).unwrap();
        check!(matches!(
            db.get(ReadTarget::Branch(branch), RecordOp::get(KEY)),
            Err(ApiError::NotFound)
        ));
        check_eq!(allocator(db.as_ref(), ReadTarget::Branch(branch)), Some(64)); // golem-internal
        check!(matches!(
            db.rollback(branch),
            Err(ApiError::NoFrameToRollback)
        ));
        clone.create(branch, input(KEY, 50)).unwrap();
        let sealed = db.seal(branch).unwrap();
        check_eq!(clone.seal(branch).unwrap(), sealed);
        check_eq!(sealed.commit_id, CommitId::new(1));
        check_eq!(db.head().unwrap(), CommitId::new(0));
        check!(clone.branch_info(branch).unwrap().sealed);
        match db.get(ReadTarget::Branch(branch), RecordOp::get(KEY)) {
            Ok(record) => check_eq!(record, before),
            Err(e) => record_failure(line!(), format!("read of a sealed branch: {e:?}")),
        }
        // golem-internal: #roots gains the previous commit's roots at commit.
        let roots_key = RecordKey(system::ROOTS.key);
        let roots = db
            .get(ReadTarget::Branch(branch), RecordOp::get(roots_key))
            .unwrap();
        check!(!roots.cells.contains_key(0u64.to_be_bytes().as_slice()));
        check!(matches!(
            db.patch(branch, RecordOp::patch(KEY)),
            Err(ApiError::Sealed)
        ));
        check!(matches!(db.checkpoint(branch), Err(ApiError::Sealed)));
        check!(matches!(db.rollback(branch), Err(ApiError::Sealed)));
        check_eq!(clone.commit(branch).unwrap(), sealed.commit_id);
        let roots = db.get(ReadTarget::Head, RecordOp::get(roots_key)).unwrap();
        check!(roots.cells.contains_key(0u64.to_be_bytes().as_slice())); // golem-internal
        check!(matches!(db.commit(branch), Err(ApiError::HandleInvalid)));
        let discarded = db.begin().unwrap();
        db.delete(discarded, RecordOp::delete(KEY)).unwrap();
        db.seal(discarded).unwrap();
        clone.discard(discarded).unwrap();
        check!(matches!(
            db.branch_info(discarded),
            Err(ApiError::HandleInvalid)
        ));
        check_eq!(get(db.as_ref(), ReadTarget::Head), before);
    });
}

#[test]
fn reserved_records_limits_and_failed_mutations_preserve_state() {
    each_backend(|db| {
        let branch = db.begin().unwrap();
        let original = db.branch_info(branch).unwrap();
        for system in system::ALL {
            let key = RecordKey(system.key);
            check!(db.get(ReadTarget::Head, RecordOp::get(key)).is_ok());
            check!(matches!(
                db.create(branch, input(key, 1)),
                Err(ApiError::Reserved)
            ));
            check!(matches!(
                db.patch(branch, RecordOp::patch(key)),
                Err(ApiError::Reserved)
            ));
            check!(matches!(
                db.delete(branch, RecordOp::delete(key)),
                Err(ApiError::Reserved)
            ));
        }
        check_eq!(db.branch_info(branch).unwrap(), original);
        for bad in [
            RecordOp::create(KEY)
                .field(&"a".repeat(33), CellValue::from_bool(true))
                .unwrap(),
            RecordOp::create(KEY)
                .field("long", CellValue::from_str(&"v".repeat(65)))
                .unwrap(),
            RecordOp::create(KEY)
                .field("long", CellValue::from_bytes(&[0; 129]))
                .unwrap(),
        ] {
            check!(matches!(
                db.create(branch, bad),
                Err(ApiError::InvalidArgument { .. })
            ));
            check_eq!(db.branch_info(branch).unwrap(), original);
            check_eq!(allocator(db.as_ref(), ReadTarget::Branch(branch)), Some(64)); // golem-internal
        }
        check!(matches!(
            db.patch(branch, RecordOp::patch(MISSING)),
            Err(ApiError::NotFound)
        ));
        check!(matches!(
            db.delete(branch, RecordOp::delete(MISSING)),
            Err(ApiError::NotFound)
        ));
        db.create(branch, input(KEY, 50)).unwrap();
        let before = get(db.as_ref(), ReadTarget::Branch(branch));
        let info = db.branch_info(branch).unwrap();
        let bad = RecordOp::patch(KEY)
            .field("aaa", CellValue::from_bool(true))
            .unwrap()
            .field("zzz", CellValue::from_str(&"v".repeat(65)))
            .unwrap();
        check!(matches!(
            db.patch(branch, bad),
            Err(ApiError::InvalidArgument { .. })
        ));
        check_eq!(get(db.as_ref(), ReadTarget::Branch(branch)), before);
        check_eq!(db.branch_info(branch).unwrap(), info);
        db.commit(branch).unwrap();
    });
}

#[test]
fn competing_commits_and_cross_thread_clones_use_one_registry() {
    each_backend(|db| {
        let first = db.begin().unwrap();
        let clone = db.clone();
        std::thread::spawn(move || clone.create(first, input(KEY, 10)).unwrap())
            .join()
            .unwrap();
        let second = db.begin().unwrap();
        db.create(second, input(KEY, 20)).unwrap();
        let gate = Arc::new(Barrier::new(2));
        let results = std::thread::scope(|scope| {
            let run = |branch| {
                gate.wait();
                db.commit(branch)
            };
            let one = scope.spawn(move || run(first));
            let two = scope.spawn(move || run(second));
            (one.join().unwrap(), two.join().unwrap())
        });
        let price = match results {
            (Ok(commit), Err(ApiError::Conflict)) if commit.get() == 1 => 10,
            (Err(ApiError::Conflict), Ok(commit)) if commit.get() == 1 => 20,
            other => panic!("unexpected commit results: {other:?}"),
        };
        check_eq!(
            get(db.as_ref(), ReadTarget::Head).cells[b"price".as_slice()].as_i32(),
            Some(price)
        );
        check!(matches!(db.commit(first), Err(ApiError::HandleInvalid)));
        check!(matches!(db.commit(second), Err(ApiError::HandleInvalid)));
        let survivor = db.clone();
        drop(db);
        check_eq!(survivor.head().unwrap(), CommitId::new(1));
    });
}

/// Port of `opening_setup_conversion_and_separate_engines_do_not_share_branch_handles`:
/// golem opens two `Database`s over one shared MemoryStore. The arkiv
/// engines own their store exclusively (one writer per store), so only the
/// "handles are per engine" half is portable; the shared-storage half
/// (second engine sees the first's commit) is not.
#[test]
fn separate_engines_do_not_share_branch_handles() {
    BACKEND.with(|b| *b.borrow_mut() = "arkiv-memory");
    let first = ArkivDb::open_memory(&genesis(), options()).unwrap();
    let second = ArkivDb::open_memory(&genesis(), options()).unwrap();
    let branch = first.begin().unwrap();
    first.create(branch, input(KEY, 50)).unwrap();
    check!(matches!(
        second.branch_info(branch),
        Err(ApiError::HandleInvalid)
    ));
    check!(matches!(
        second.get(ReadTarget::Head, RecordOp::get(KEY)),
        Err(ApiError::NotFound)
    ));
    first.commit(branch).unwrap();
    finish();
}

#[test]
fn durable_facade_reopens_committed_state_and_discards_pending_work() {
    BACKEND.with(|b| *b.borrow_mut() = "arkiv-redb");
    let dir = tempfile::tempdir().unwrap();
    let (sealed, pending, genesis_id) = {
        let db = open(dir.path(), &config()).unwrap();
        let branch = db.begin().unwrap();
        db.create(branch, input(KEY, 50)).unwrap();
        let sealed = db.seal(branch).unwrap();
        db.commit(branch).unwrap();
        let pending = db.begin().unwrap();
        db.delete(pending, RecordOp::delete(KEY)).unwrap();
        (sealed, pending, db.info().genesis_id)
    };
    let db = reopen(dir.path(), &genesis()).unwrap();
    check!(!db.info().created);
    check_eq!(db.info().genesis_id, genesis_id);
    check_eq!(db.info().commit_id, sealed.commit_id);
    check_eq!(db.info().state_root, sealed.state_root);
    check_eq!(db.info().index_root, sealed.index_root);
    check_eq!(db.head().unwrap(), sealed.commit_id);
    check!(matches!(
        db.branch_info(pending),
        Err(ApiError::HandleInvalid)
    ));
    check_eq!(
        get(&db, ReadTarget::Head).cells[b"price".as_slice()].as_i32(),
        Some(50)
    );
    let branch = db.begin().unwrap();
    db.patch(
        branch,
        RecordOp::patch(KEY)
            .attribute("price", CellValue::from_i32(70))
            .unwrap(),
    )
    .unwrap();
    check_eq!(db.commit(branch).unwrap(), CommitId::new(2));
    finish();
}

#[test]
fn empty_records_keep_identity_through_patch_rollback_commit_and_delete() {
    each_backend(|db| {
        let db = db.as_ref();
        let branch = db.begin().unwrap();
        db.create(branch, RecordOp::create(KEY)).unwrap();
        let empty = get(db, ReadTarget::Branch(branch));
        check_eq!(empty.key, KEY);
        check_eq!(empty.cells.len(), 1);
        check_eq!(
            empty
                .cells
                .get(b"#key".as_slice())
                .and_then(|v| v.as_bytes32()),
            Some(KEY.0)
        );
        check_eq!(allocator(db, ReadTarget::Branch(branch)), Some(65)); // golem-internal
        check!(matches!(
            db.create(branch, RecordOp::create(KEY)),
            Err(ApiError::AlreadyExists)
        ));
        check!(
            db.get(
                ReadTarget::Branch(branch),
                RecordOp::get(KEY).only([] as [&str; 0])
            )
            .unwrap()
            .cells
            .is_empty()
        );
        check!(matches!(
            db.get(ReadTarget::Head, RecordOp::get(KEY)),
            Err(ApiError::NotFound)
        ));
        db.commit(branch).unwrap();
        check_eq!(get(db, ReadTarget::Head), empty);

        let branch = db.begin().unwrap();
        let info = db.branch_info(branch).unwrap();
        db.patch(branch, RecordOp::patch(KEY)).unwrap();
        db.patch(branch, RecordOp::patch(KEY).remove("missing").unwrap())
            .unwrap();
        check_eq!(db.branch_info(branch).unwrap(), info);
        db.patch(
            branch,
            RecordOp::patch(KEY)
                .attribute("price", CellValue::from_i32(50))
                .unwrap(),
        )
        .unwrap();
        db.commit(branch).unwrap();
        let populated = get(db, ReadTarget::Head);

        let branch = db.begin().unwrap();
        db.checkpoint(branch).unwrap();
        db.patch(branch, RecordOp::patch(KEY).remove("price").unwrap())
            .unwrap();
        check_eq!(get(db, ReadTarget::Branch(branch)), empty);
        check_eq!(get(db, ReadTarget::Head), populated);
        db.rollback(branch).unwrap();
        check_eq!(get(db, ReadTarget::Branch(branch)), populated);
        db.patch(branch, RecordOp::patch(KEY).remove("price").unwrap())
            .unwrap();
        let seal = db.seal(branch).unwrap();
        // Removing the last attribute empties the index.
        check_eq!(seal.index_root, db.info().index_root);
        match db.get(ReadTarget::Branch(branch), RecordOp::get(KEY)) {
            Ok(record) => check_eq!(record, empty),
            Err(e) => record_failure(line!(), format!("read of a sealed branch: {e:?}")),
        }
        db.commit(branch).unwrap();
        check_eq!(get(db, ReadTarget::Head), empty);

        let branch = db.begin().unwrap();
        db.checkpoint(branch).unwrap();
        db.delete(branch, RecordOp::delete(KEY)).unwrap();
        check!(matches!(
            db.get(ReadTarget::Branch(branch), RecordOp::get(KEY)),
            Err(ApiError::NotFound)
        ));
        db.rollback(branch).unwrap();
        check_eq!(get(db, ReadTarget::Branch(branch)), empty);
        db.delete(branch, RecordOp::delete(KEY)).unwrap();
        db.commit(branch).unwrap();
        check!(matches!(
            db.get(ReadTarget::Head, RecordOp::get(KEY)),
            Err(ApiError::NotFound)
        ));
        let branch = db.begin().unwrap();
        db.create(branch, RecordOp::create(KEY)).unwrap();
        check_eq!(allocator(db, ReadTarget::Branch(branch)), Some(66)); // golem-internal
        db.commit(branch).unwrap();
        check_eq!(get(db, ReadTarget::Head), empty);
    });
}

#[test]
fn durable_reopens_empty_records_created_directly_or_by_removing_all_cells() {
    BACKEND.with(|b| *b.borrow_mut() = "arkiv-redb");
    let dir = tempfile::tempdir().unwrap();
    {
        let db = open(dir.path(), &config()).unwrap();
        let branch = db.begin().unwrap();
        db.create(branch, RecordOp::create(KEY)).unwrap();
        db.create(branch, input(MISSING, 50)).unwrap();
        db.commit(branch).unwrap();
        let branch = db.begin().unwrap();
        db.patch(
            branch,
            RecordOp::patch(MISSING)
                .remove("price")
                .unwrap()
                .remove("description")
                .unwrap(),
        )
        .unwrap();
        db.commit(branch).unwrap();
    }
    let db = reopen(dir.path(), &genesis()).unwrap();
    for key in [KEY, MISSING] {
        let record = db.get(ReadTarget::Head, RecordOp::get(key)).unwrap();
        check_eq!(record.cells.len(), 1);
        check_eq!(
            record
                .cells
                .get(b"#key".as_slice())
                .and_then(|v| v.as_bytes32()),
            Some(key.0)
        );
    }
    check_eq!(allocator(&db, ReadTarget::Head), Some(66)); // golem-internal
    finish();
}

// ---- immutable_data.rs (the IO guard is golem-Store-specific and dropped) ----

fn unsupported<T: std::fmt::Debug>(result: Result<T>, expected: &str) {
    match result.unwrap_err() {
        ApiError::NotImplemented { operation } => check_eq!(operation, expected),
        other => record_failure(line!(), format!("expected NotImplemented, got {other:?}")),
    }
}

fn check_stubs(api: &dyn Api, branch: BranchId) {
    for segment in ["headers", "unknown", ""] {
        for key in [None, Some(ImmutableDataKey([0x42; 32]))] {
            for row in [vec![], vec![b"header".to_vec(), vec![0, 255]]] {
                unsupported(
                    api.immutable_data_append(branch, segment, key, row),
                    "immutable_data_append",
                );
            }
        }
        for address in [
            ImmutableDataAddress::Ordinal(0),
            ImmutableDataAddress::Ordinal(u64::MAX),
            ImmutableDataAddress::Key(ImmutableDataKey([0x42; 32])),
        ] {
            unsupported(
                api.immutable_data_get(segment, address),
                "immutable_data_get",
            );
        }
        for commit in [0, u64::MAX] {
            unsupported(
                api.immutable_data_range_of(segment, CommitId::new(commit)),
                "immutable_data_range_of",
            );
            unsupported(
                api.immutable_data_rows_of(segment, CommitId::new(commit)),
                "immutable_data_rows_of",
            );
        }
    }
}

#[test]
fn immutable_data_stubs_return_explicit_errors_without_state_changes() {
    each_backend(|db| {
        let key = RecordKey([1; 32]);
        let branch = db.begin().unwrap();
        db.create(
            branch,
            RecordOp::create(key)
                .field("price", CellValue::from_i32(50))
                .unwrap(),
        )
        .unwrap();
        let record = db
            .get(ReadTarget::Branch(branch), RecordOp::get(key))
            .unwrap();
        let info = db.branch_info(branch).unwrap();
        let sealed_branch = db.begin().unwrap();
        let seal = db.seal(sealed_branch).unwrap();
        let sealed_info = db.branch_info(sealed_branch).unwrap();
        let api: Arc<dyn Api + Send + Sync> = db.clone();
        for id in [branch, sealed_branch, BranchId::new(u64::MAX)] {
            check_stubs(api.as_ref(), id);
        }
        check_eq!(db.head().unwrap(), CommitId::new(0));
        check_eq!(db.branch_info(branch).unwrap(), info);
        check_eq!(db.branch_info(sealed_branch).unwrap(), sealed_info);
        check_eq!(db.seal(sealed_branch).unwrap(), seal);
        check_eq!(
            db.get(ReadTarget::Branch(branch), RecordOp::get(key))
                .unwrap(),
            record
        );
        check_eq!(db.commit(sealed_branch).unwrap(), CommitId::new(1));
        check_stubs(api.as_ref(), branch);
        check_stubs(api.as_ref(), sealed_branch);
        check!(matches!(db.commit(branch), Err(ApiError::Conflict)));
        check_eq!(db.head().unwrap(), CommitId::new(1));
    });
}

// ---- adapter-specific: golem's full cell type grid and empty patches ----

/// One value of every golem cell type, with its type name.
fn every_golem_type() -> Vec<(&'static str, CellValue)> {
    let mut i256 = [0xff; 32];
    i256[31] = 0xfe; // -2
    let mut dec256 = [0; 32];
    dec256[31] = 7;
    vec![
        ("bool", CellValue::from_bool(true)),
        ("str", CellValue::from_str("golem")),
        ("bytes", CellValue::from_bytes(&[0, 1, 2, 0xff])),
        ("bytes4", CellValue::from_bytes4([4; 4])),
        ("bytes8", CellValue::from_bytes8([8; 8])),
        ("bytes16", CellValue::from_bytes16([16; 16])),
        ("bytes20", CellValue::from_bytes20([20; 20])),
        ("bytes32", CellValue::from_bytes32([32; 32])),
        ("u32", CellValue::from_u32(u32::MAX)),
        ("u64", CellValue::from_u64(u64::MAX - 1)),
        ("u128", CellValue::from_u128(u128::MAX - 2)),
        ("u256", CellValue::from_u256_be([0xee; 32])),
        ("i32", CellValue::from_i32(i32::MIN)),
        ("i64", CellValue::from_i64(-64)),
        ("i128", CellValue::from_i128(i128::MIN + 1)),
        ("i256", CellValue::from_i256_be(i256)),
        ("dec32", CellValue::from_dec32_unscaled(-123_450)),
        ("dec64", CellValue::from_dec64_unscaled(1_500_000)),
        ("dec128", CellValue::from_dec128_unscaled(-1)),
        ("dec256", CellValue::from_dec256_unscaled_be(dec256)),
        ("f32", CellValue::from_f32(-1.5).unwrap()),
        ("f64", CellValue::from_f64(f64::INFINITY).unwrap()),
        ("date32", CellValue::from_date32(-719_162)),
        (
            "timestamp64",
            CellValue::from_timestamp64(1_700_000_000_000_000),
        ),
    ]
}

#[test]
fn every_golem_cell_type_round_trips_as_attribute_and_field() {
    each_backend(|db| {
        let db = db.as_ref();
        let values = every_golem_type();
        let mut op = RecordOp::create(KEY);
        for (name, value) in &values {
            if !matches!(value.cell_type(), CellType::Bytes) {
                op = op
                    .attribute(format!("a_{name}").as_str(), value.clone())
                    .unwrap();
            }
            op = op
                .field(format!("f_{name}").as_str(), value.clone())
                .unwrap();
        }
        let branch = db.begin().unwrap();
        db.create(branch, op).unwrap();
        let pending = get(db, ReadTarget::Branch(branch));
        db.commit(branch).unwrap();
        let committed = get(db, ReadTarget::Head);
        check_eq!(pending, committed);
        for (name, value) in &values {
            let field = committed.cells.get(format!("f_{name}").as_bytes());
            check_eq!(
                field,
                Some(&value.clone().with_kind(CellKind::Field).unwrap())
            );
            if matches!(value.cell_type(), CellType::Bytes) {
                continue;
            }
            let attribute = committed.cells.get(format!("a_{name}").as_bytes());
            check_eq!(
                attribute,
                Some(&value.clone().with_kind(CellKind::Attribute).unwrap())
            );
            check_eq!(
                attribute.map(|cell| cell.encoded_bytes().to_vec()),
                Some(
                    value
                        .clone()
                        .with_kind(CellKind::Attribute)
                        .unwrap()
                        .encoded_bytes()
                        .to_vec()
                )
            );
        }
        // Retyping a cell to every other golem type also round-trips.
        for (_, value) in &values {
            let branch = db.begin().unwrap();
            db.patch(
                branch,
                RecordOp::patch(KEY).field("f_str", value.clone()).unwrap(),
            )
            .unwrap();
            db.commit(branch).unwrap();
            let head = get(db, ReadTarget::Head);
            check_eq!(
                head.cells.get(b"f_str".as_slice()),
                Some(&value.clone().with_kind(CellKind::Field).unwrap())
            );
        }
    });
}

#[test]
fn empty_patch_is_a_no_op_or_not_found_without_an_adapter_read() {
    BACKEND.with(|b| *b.borrow_mut() = "arkiv-memory");
    let db = ArkivDb::open_memory(&genesis(), options()).unwrap();
    let branch = db.begin().unwrap();
    db.create(branch, input(KEY, 50)).unwrap();
    db.commit(branch).unwrap();
    let before = get(&db, ReadTarget::Head);
    let branch = db.begin().unwrap();
    let info = db.branch_info(branch).unwrap();
    db.patch(branch, RecordOp::patch(KEY)).unwrap();
    check_eq!(db.branch_info(branch).unwrap(), info);
    check!(matches!(
        db.patch(branch, RecordOp::patch(MISSING)),
        Err(ApiError::NotFound)
    ));
    db.seal(branch).unwrap();
    db.commit(branch).unwrap();
    check_eq!(get(&db, ReadTarget::Head), before);
    finish();
}

// ---- adapter-specific: genesis settings are the engine's ----

#[test]
fn blake3_genesis_is_refused_at_open() {
    let blake3 = Genesis::new(HashAlgorithm::Blake3, limits());
    let refused = |result: OpenResult<ArkivDb>| match result {
        Err(OpenError::InvalidConfig(message)) => message.contains("only keccak-256"),
        _ => false,
    };
    BACKEND.with(|b| *b.borrow_mut() = "arkiv-memory");
    check!(refused(ArkivDb::open_memory(&blake3, options())));
    check!(ArkivDb::open_memory(&genesis(), options()).is_ok());
    BACKEND.with(|b| *b.borrow_mut() = "arkiv-redb");
    let dir = tempfile::tempdir().unwrap();
    check!(refused(open(dir.path(), &OpenConfig::new(blake3))));
    check!(!dir.path().join(arkiv::FILE).exists()); // refused before touching storage
    check!(open(dir.path(), &config()).is_ok());
    check!(refused(reopen(dir.path(), &blake3)));
    // golem's genesis YAML selecting Blake3 parses, and is refused at open.
    let yaml = Genesis::from_yaml(&YAML.replace("keccak-256", "blake3")).unwrap();
    check!(refused(ArkivDb::open_memory(&yaml, options())));
    finish();
}

/// golem's cell limits pass through to the engine, which enforces them: a
/// genesis tighter than the engine's hard caps (32-byte names) shows the
/// rejection comes from the engine's admission, not from the adapter.
#[test]
fn genesis_cell_limits_are_enforced_by_the_engine() {
    let tight = Genesis::new(
        HashAlgorithm::Keccak256,
        CellLimits {
            max_cell_name_len: 10,
            max_str_len: 8,
            max_bytes_len: 16,
        },
    );
    let engine_rejects = |result: Result<()>, fault: &str| match result {
        Err(ApiError::InvalidArgument { message, .. }) => {
            message.contains("arkiv engine") && message.contains(fault)
        }
        _ => false,
    };
    BACKEND.with(|b| *b.borrow_mut() = "arkiv-memory");
    let db = ArkivDb::open_memory(&tight, options()).unwrap();
    let branch = db.begin().unwrap();
    let ok = RecordOp::create(KEY)
        .field(&"n".repeat(10), CellValue::from_bool(true))
        .unwrap()
        .attribute("s", CellValue::from_str(&"v".repeat(8)))
        .unwrap()
        .field("b", CellValue::from_bytes(&[0; 16]))
        .unwrap();
    check!(db.create(branch, ok).is_ok());
    for (bad, fault) in [
        (
            RecordOp::create(MISSING)
                .field(&"n".repeat(11), CellValue::from_bool(true))
                .unwrap(),
            "name",
        ),
        (
            RecordOp::create(MISSING)
                .attribute("s", CellValue::from_str(&"v".repeat(9)))
                .unwrap(),
            "too large",
        ),
        (
            RecordOp::create(MISSING)
                .field("b", CellValue::from_bytes(&[0; 17]))
                .unwrap(),
            "too large",
        ),
    ] {
        check!(engine_rejects(db.create(branch, bad).map(|_| ()), fault));
    }
    // golem's limits: 65-byte strings and 129-byte bytes are the engine's
    // rejections too.
    let db = ArkivDb::open_memory(&genesis(), options()).unwrap();
    let branch = db.begin().unwrap();
    for bad in [
        RecordOp::create(KEY)
            .field("long", CellValue::from_str(&"v".repeat(65)))
            .unwrap(),
        RecordOp::create(KEY)
            .field("long", CellValue::from_bytes(&[0; 129]))
            .unwrap(),
    ] {
        check!(engine_rejects(
            db.create(branch, bad).map(|_| ()),
            "too large"
        ));
    }
    // A durable database keeps its limits: reopening with others fails.
    BACKEND.with(|b| *b.borrow_mut() = "arkiv-redb");
    let dir = tempfile::tempdir().unwrap();
    drop(open(dir.path(), &OpenConfig::new(tight)).unwrap());
    check!(reopen(dir.path(), &tight).is_ok());
    check!(matches!(
        reopen(dir.path(), &genesis()),
        Err(OpenError::GenesisMismatch)
    ));
    // Limits above the engine's caps (32-byte names; golem indexes str
    // attributes up to max_str_len, the engine up to 128 bytes) are refused
    // at open rather than narrowed.
    for wide in [
        CellLimits {
            max_cell_name_len: 33,
            ..limits()
        },
        CellLimits {
            max_str_len: 129,
            ..limits()
        },
        CellLimits {
            max_cell_name_len: 0,
            ..limits()
        },
    ] {
        check!(matches!(
            ArkivDb::open_memory(&Genesis::new(HashAlgorithm::Keccak256, wide), options()),
            Err(OpenError::InvalidConfig(_))
        ));
    }
    // At the caps, both limits are usable.
    let max = Genesis::new(
        HashAlgorithm::Keccak256,
        CellLimits {
            max_cell_name_len: 32,
            max_str_len: 128,
            max_bytes_len: 128,
        },
    );
    let db = ArkivDb::open_memory(&max, options()).unwrap();
    let branch = db.begin().unwrap();
    check!(
        db.create(
            branch,
            RecordOp::create(KEY)
                .attribute(&"n".repeat(32), CellValue::from_str(&"v".repeat(128)))
                .unwrap()
        )
        .is_ok()
    );
    finish();
}

// ---- opening.rs: golem's genesis YAML, OpenInfo and opening modes ----

const YAML: &str = "hash_function: keccak-256\ncell_limits:\n  max_cell_name_len: 32\n  max_str_len: 64\n  max_bytes_len: 128\n";

/// Port of `yaml_is_explicit_strict_and_has_canonical_identity` (the YAML
/// parsing itself is golem's `Genesis::from_yaml`/`load`).
#[test]
fn yaml_genesis_opens_with_canonical_identity() {
    BACKEND.with(|b| *b.borrow_mut() = "arkiv-memory");
    let first = Genesis::from_yaml(YAML).unwrap();
    let reordered = Genesis::from_yaml(
        "# same deployment\ncell_limits: {max_bytes_len: 128, max_str_len: 64, max_cell_name_len: 32}\nhash_function: keccak-256\n",
    )
    .unwrap();
    check_eq!(first, genesis());
    check_eq!(first, reordered);
    let (one, two) = (
        *ArkivDb::open_memory(&first, options()).unwrap().info(),
        *ArkivDb::open_memory(&reordered, options()).unwrap().info(),
    );
    check_eq!(one, two);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("genesis.yaml");
    std::fs::write(&path, YAML).unwrap();
    let db = ArkivDb::open_memory(&Genesis::load(&path).unwrap(), options()).unwrap();
    check_eq!(db.genesis(), &first);
    // golem's #params record reports the loaded limits.
    let params = db
        .get(
            ReadTarget::Head,
            RecordOp::get(RecordKey(system::PARAMS.key)),
        )
        .unwrap();
    check_eq!(
        params
            .cells
            .get(b"#maxStrLen".as_slice())
            .and_then(|v| v.as_u32()),
        Some(64)
    );
    finish();
}

/// Port of `lifecycle` / `mdbx_genesis_matches_memory_and_reopens_from_disk_after_commit`:
/// opening reports OpenInfo, the memory and durable backends agree on it,
/// and a reopen reports the committed head.
#[test]
fn open_info_matches_between_backends_and_follows_reopen() {
    BACKEND.with(|b| *b.borrow_mut() = "arkiv-redb");
    let memory = *ArkivDb::open_memory(&genesis(), options()).unwrap().info();
    check!(memory.created);
    check_eq!(memory.commit_id, CommitId::new(0));
    check!(memory.state_root != [0; 32]);
    let dir = tempfile::tempdir().unwrap();
    let created = {
        let db = open(dir.path(), &config()).unwrap();
        check_eq!(db.info(), &memory);
        check_eq!(db.genesis(), &genesis());
        *db.info()
    };
    let reopened = reopen(dir.path(), &genesis()).unwrap();
    check_eq!(
        *reopened.info(),
        OpenInfo {
            created: false,
            ..created
        }
    );
    let branch = reopened.begin().unwrap();
    reopened.create(branch, input(KEY, 50)).unwrap();
    let sealed = reopened.seal(branch).unwrap();
    check_eq!(reopened.commit(branch).unwrap(), CommitId::new(1));
    drop(reopened);
    // CreateIfMissing reopens an initialized database.
    let db = open(dir.path(), &config()).unwrap();
    check!(!db.info().created);
    check_eq!(db.info().commit_id, CommitId::new(1));
    check_eq!(db.info().state_root, sealed.state_root);
    check_eq!(db.info().genesis_id, created.genesis_id);
    check_eq!(
        get(&db, ReadTarget::Head).cells[b"price".as_slice()].as_i32(),
        Some(50)
    );
    finish();
}

/// Port of `modes`: golem's opening modes, and a genesis mismatch, which
/// golem checks before the mode. Nothing changes the stored database.
#[test]
fn opening_modes_and_genesis_mismatch() {
    BACKEND.with(|b| *b.borrow_mut() = "arkiv-redb");
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("absent");
    check!(matches!(
        open(&missing, &config().with_mode(OpenMode::ExistingOnly)),
        Err(OpenError::NotInitialized)
    ));
    check!(!missing.exists());
    let created = *open(dir.path(), &config().with_mode(OpenMode::CreateNew))
        .unwrap()
        .info();
    check!(created.created);
    check!(matches!(
        open(dir.path(), &config().with_mode(OpenMode::CreateNew)),
        Err(OpenError::AlreadyInitialized)
    ));
    for change in 0..3 {
        let mut different = config();
        match change {
            0 => different.genesis.cell_limits.max_cell_name_len -= 1,
            1 => different.genesis.cell_limits.max_str_len += 1,
            _ => different.genesis.cell_limits.max_bytes_len += 1,
        }
        check!(matches!(
            open(dir.path(), &different),
            Err(OpenError::GenesisMismatch)
        ));
        check!(matches!(
            open(dir.path(), &different.with_mode(OpenMode::CreateNew)),
            Err(OpenError::GenesisMismatch)
        ));
    }
    let reopened = *reopen(dir.path(), &genesis()).unwrap().info();
    check_eq!(
        reopened,
        OpenInfo {
            created: false,
            ..created
        }
    );
    finish();
}

// ---- facade_errors.rs: storage failures, error mapping and retry ----

use arkiv_db_redb::testing::{CrashDisk, DiskOp};
use arkiv_db_redb::{RedbConfig, RedbStore};

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
        retention: ArkivOptions::retention_from_env(),
    };
    ArkivDb::open_store(
        Arc::new(store),
        "crash-disk".into(),
        &OpenConfig::new(genesis),
        options,
    )
    .unwrap()
}

/// golem's `diagnostic`: an `Internal` error whose source chain keeps the
/// storage failure (here the store's own I/O error).
fn diagnostic(line: u32, error: ApiError) {
    use std::error::Error;
    let kept = matches!(error, ApiError::Internal { .. })
        && error
            .source()
            .and_then(|s| s.downcast_ref::<arkiv_db::StorageError>())
            .is_some_and(|s| s.kind == arkiv_db_storage::versioned::StorageErrorKind::Io);
    if !kept {
        record_failure(
            line,
            format!("expected Internal(storage I/O), got {error:?}"),
        );
    }
}

/// Port of `failure_contract`: failed reads and writes are `Internal` with
/// the storage error as source, leave the branch as it was, and a failed
/// commit keeps its seal and can be retried.
#[test]
fn storage_failures_keep_the_branch_and_a_failed_commit_can_be_retried() {
    BACKEND.with(|b| *b.borrow_mut() = "arkiv-crashdisk");
    let disk = CrashDisk::new();
    let db = crash_db(&disk, genesis());
    // A committed record, so the failing reads must reach the device.
    let setup = db.begin().unwrap();
    db.create(setup, input(MISSING, 1)).unwrap();
    db.commit(setup).unwrap();
    let branch = db.begin().unwrap();
    let other = db.begin().unwrap();
    db.create(
        branch,
        RecordOp::create(KEY)
            .attribute("price", CellValue::from_i32(50))
            .unwrap(),
    )
    .unwrap();
    db.create(other, input(RecordKey([7; 32]), 7)).unwrap();
    let pending = db
        .get(ReadTarget::Branch(branch), RecordOp::get(KEY))
        .unwrap();
    let info = db.branch_info(branch).unwrap();

    disk.fail_all(DiskOp::Read, std::io::ErrorKind::Other);
    diagnostic(
        line!(),
        db.get(ReadTarget::Head, RecordOp::get(MISSING))
            .unwrap_err(),
    );
    diagnostic(
        line!(),
        db.patch(branch, RecordOp::patch(MISSING).remove("price").unwrap())
            .unwrap_err(),
    );
    // not portable: golem's head() reads storage; the adapter's is in memory.
    disk.clear_faults();
    check_eq!(db.branch_info(branch).unwrap(), info);
    check_eq!(
        db.get(ReadTarget::Branch(branch), RecordOp::get(KEY))
            .unwrap(),
        pending
    );

    disk.fail_all(DiskOp::Write, std::io::ErrorKind::Other);
    diagnostic(line!(), db.commit(branch).unwrap_err());
    check_eq!(db.head().unwrap(), CommitId::new(1));
    check!(db.branch_info(branch).unwrap().sealed);
    let sealed = db.seal(branch).unwrap();
    disk.clear_faults();
    check_eq!(db.commit(branch).unwrap(), sealed.commit_id);
    check_eq!(
        db.get(ReadTarget::Head, RecordOp::get(KEY)).unwrap(),
        pending
    );
    // The branch begun before the failures was untouched by them; it only
    // lost the race.
    check!(matches!(db.commit(other), Err(ApiError::Conflict)));
    finish();
}

/// Port of `full_mdbx_store_preserves_head_tables_and_sealed_branch`.
#[test]
fn full_store_preserves_head_and_sealed_branch() {
    BACKEND.with(|b| *b.borrow_mut() = "arkiv-crashdisk");
    let disk = CrashDisk::new();
    let big = Genesis::new(
        HashAlgorithm::Keccak256,
        CellLimits {
            max_cell_name_len: 32,
            max_str_len: 64,
            max_bytes_len: 2 * 1024 * 1024,
        },
    );
    let db = crash_db(&disk, big);
    disk.set_quota(Some(disk.visible_image().len() as u64 + (1 << 20)));
    let branch = db.begin().unwrap();
    db.create(
        branch,
        RecordOp::create(KEY)
            .field("payload", CellValue::from_bytes(&vec![1; 2 * 1024 * 1024]))
            .unwrap(),
    )
    .unwrap();
    let sealed = db.seal(branch).unwrap();
    for _ in 0..2 {
        let result = db.commit(branch);
        check!(matches!(result, Err(ApiError::StoreFull)));
        check_eq!(db.head().unwrap(), CommitId::GENESIS);
        check_eq!(db.seal(branch).unwrap(), sealed);
        check!(db.branch_info(branch).unwrap().sealed);
        match db.get(ReadTarget::Branch(branch), RecordOp::get(KEY)) {
            Ok(record) => check_eq!(
                record.cells[b"payload".as_slice()]
                    .as_bytes()
                    .map(|b| b.len()),
                Some(2 * 1024 * 1024)
            ),
            Err(e) => record_failure(line!(), format!("read of the sealed branch: {e:?}")),
        }
        check!(matches!(
            db.get(ReadTarget::Head, RecordOp::get(KEY)),
            Err(ApiError::NotFound)
        ));
    }
    db.discard(branch).unwrap();
    let branch = db.begin().unwrap();
    db.create(
        branch,
        RecordOp::create(KEY)
            .field("payload", CellValue::from_bytes(b"small"))
            .unwrap(),
    )
    .unwrap();
    check_eq!(db.commit(branch).ok(), Some(CommitId::new(1)));
    finish();
}

// ---- adapter-specific: BranchInfo::version against golem-db itself ----

fn kind(result: &Result<()>) -> &'static str {
    match result {
        Ok(()) => "Ok",
        Err(ApiError::NotFound) => "NotFound",
        Err(ApiError::AlreadyExists) => "AlreadyExists",
        Err(ApiError::NoFrameToRollback) => "NoFrameToRollback",
        Err(ApiError::InvalidArgument { .. }) => "InvalidArgument",
        Err(ApiError::Sealed) => "Sealed",
        Err(ApiError::HandleInvalid) => "HandleInvalid",
        Err(_) => "other",
    }
}

/// The same seeded random scripts of writes, checkpoints and rollbacks run
/// on golem-db's own memory `Database` and on the adapter; every call's
/// outcome and `BranchInfo` (golem's undo-entry `version`) must match after
/// every step, and the committed records must be equal.
#[test]
fn branch_version_and_outcomes_match_golem_db_on_random_scripts() {
    use rand::{Rng, SeedableRng};
    let keys = [KEY, MISSING, RecordKey([0x44; 32])];
    let names = ["price", "description", "x"];
    let values = [
        CellValue::from_i32(1),
        CellValue::from_i32(2),
        CellValue::from_str("product"),
    ];
    for seed in 0..40u64 {
        let golem = Database::open_memory(&genesis()).unwrap();
        let memory = ArkivDb::open_memory(&genesis(), options()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let durable = open(dir.path(), &config()).unwrap();
        let apis: [(&'static str, &dyn Api); 3] = [
            ("golem-db", &golem),
            ("arkiv-memory", &memory),
            ("arkiv-redb", &durable),
        ];
        // Committed records, so writes also hit records of the base.
        for (_, api) in apis {
            let branch = api.begin().unwrap();
            api.create(branch, input(KEY, 1)).unwrap();
            api.create(branch, RecordOp::create(MISSING)).unwrap();
            api.commit(branch).unwrap();
        }
        let branches: Vec<BranchId> = apis.iter().map(|(_, api)| api.begin().unwrap()).collect();
        let mut rng = rand_chacha::ChaCha8Rng::seed_from_u64(seed);
        for step in 0..60 {
            let action = rng.random_range(0..8);
            let key = keys[rng.random_range(0..keys.len())];
            let name = names[rng.random_range(0..names.len())];
            let value = values[rng.random_range(0..values.len())].clone();
            let attribute = rng.random_bool(0.5);
            let run = |api: &dyn Api, branch: BranchId| -> Result<()> {
                match action {
                    0 => {
                        let op = if attribute {
                            RecordOp::create(key).attribute(name, value.clone())
                        } else {
                            RecordOp::create(key).field(name, value.clone())
                        };
                        api.create(branch, op.unwrap()).map(drop)
                    }
                    1 | 2 => {
                        let op = if attribute {
                            RecordOp::patch(key).attribute(name, value.clone())
                        } else {
                            RecordOp::patch(key).field(name, value.clone())
                        };
                        api.patch(branch, op.unwrap())
                    }
                    3 => api.patch(branch, RecordOp::patch(key).remove(name).unwrap()),
                    4 => api.patch(branch, RecordOp::patch(key)),
                    5 => api.delete(branch, RecordOp::delete(key)),
                    6 => api.checkpoint(branch),
                    _ => api.rollback(branch),
                }
            };
            let observed: Vec<(&str, Option<u64>)> = apis
                .iter()
                .zip(&branches)
                .map(|((_, api), &branch)| {
                    let outcome = run(*api, branch);
                    let version = api.branch_info(branch).ok().map(|info| info.version);
                    (kind(&outcome), version)
                })
                .collect();
            for (at, (name, _)) in apis.iter().enumerate().skip(1) {
                if observed[at] != observed[0] {
                    record_failure(
                        line!(),
                        format!(
                            "seed {seed} step {step} action {action}: {name} {:?}, golem-db {:?}",
                            observed[at], observed[0]
                        ),
                    );
                }
            }
        }
        for ((_, api), &branch) in apis.iter().zip(&branches) {
            api.commit(branch).unwrap();
        }
        for key in keys {
            let read = |api: &dyn Api| api.get(ReadTarget::Head, RecordOp::get(key)).ok();
            let expected = read(&golem);
            for (name, api) in &apis[1..] {
                if read(*api) != expected {
                    record_failure(
                        line!(),
                        format!("seed {seed}: {name} committed {key:?} differs"),
                    );
                }
            }
        }
    }
    finish();
}

// ---- adapter-specific: the harness's table_log ----

#[test]
fn table_stats_report_redb_tables_and_engine_counts() {
    let is_row = |lines: &[String], name: &str| {
        lines
            .iter()
            .find(|line| line.split(',').next() == Some(name))
            .cloned()
    };
    BACKEND.with(|b| *b.borrow_mut() = "arkiv-redb");
    let dir = tempfile::tempdir().unwrap();
    let durable = open(dir.path(), &config()).unwrap();
    BACKEND.with(|b| *b.borrow_mut() = "arkiv-memory");
    let memory = ArkivDb::open_memory(&genesis(), options()).unwrap();
    for db in [&durable, &memory] {
        let branch = db.begin().unwrap();
        db.create(branch, input(KEY, 50)).unwrap();
        db.commit(branch).unwrap();
    }
    // Snapshots 0..=2 (adapter genesis, commit 0, commit 1), as retained.
    let retained = options().retention.window().map_or(3, |w| (w + 1).min(3));
    let lines = durable.table_stats().unwrap();
    // table,entries,depth,pages,mib
    check!(lines.iter().all(|line| line.split(',').count() == 5));
    check!(is_row(&lines, "objects:namespace").is_some());
    check!(is_row(&lines, "records").is_some());
    check!(is_row(&lines, "file").is_some_and(|line| !line.ends_with(",0,0.000")));
    check!(is_row(&lines, "object-cache").is_some());
    check_eq!(
        is_row(&lines, "live:records"),
        Some("live:records,1,,,".to_string())
    );
    check_eq!(
        is_row(&lines, "snapshots"),
        Some(format!("snapshots,{retained},,,"))
    );
    let lines = memory.table_stats().unwrap();
    check_eq!(
        is_row(&lines, "live:records"),
        Some("live:records,1,,,".to_string())
    );
    check_eq!(
        is_row(&lines, "snapshots"),
        Some(format!("snapshots,{retained},,,"))
    );
    check!(is_row(&lines, "file").is_none());
    finish();
}

#[test]
fn state_retention_window_refuses_commits_below_the_floor() {
    let options = ArkivOptions {
        retention: arkiv_db::durable::Retention::Window(1),
        ..options()
    };
    let dir = tempfile::tempdir().unwrap();
    for backend in ["arkiv-redb", "arkiv-memory"] {
        BACKEND.with(|b| *b.borrow_mut() = backend);
        let db = if backend == "arkiv-redb" {
            ArkivDb::open(dir.path(), &config(), options).unwrap()
        } else {
            ArkivDb::open_memory(&genesis(), options).unwrap()
        };
        for price in [1, 2, 3] {
            let branch = db.begin().unwrap();
            if price == 1 {
                db.create(branch, input(KEY, price)).unwrap();
            } else {
                db.patch(
                    branch,
                    RecordOp::patch(KEY)
                        .field("price", CellValue::from_i32(price))
                        .unwrap(),
                )
                .unwrap();
            }
            db.commit(branch).unwrap();
        }
        check_eq!(db.head().unwrap(), CommitId::new(3));
        // Head and one commit before it are served; older commits are
        // refused before any read, as golem-db refuses every non-head commit.
        for served in [3, 2] {
            check!(
                db.get(
                    ReadTarget::Commit(CommitId::new(served)),
                    RecordOp::get(KEY)
                )
                .is_ok()
            );
        }
        for refused in [1, 0] {
            check!(matches!(
                db.get(ReadTarget::Commit(CommitId::new(refused)), RecordOp::get(KEY)),
                Err(ApiError::CommitUnavailable { requested, head })
                    if requested.get() == refused && head.get() == 3
            ));
        }
        let lines = db.table_stats().unwrap();
        check!(lines.iter().any(|line| line == "snapshots,2,,,"));
    }
    finish();
}
