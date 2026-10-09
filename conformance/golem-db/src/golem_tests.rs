//! golem-db's own API behavioral tests, ported to run through the arkiv
//! adapter (`ArkivDb`). Sources:
//!   golem-db/crates/integration-tests/tests/api/facade.rs
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
//!   `#recordKeys`, `#roots`; `Database::info()`) are kept where they can be
//!   expressed through the Api, marked `// golem-internal`, and indexing that
//!   would panic was turned into `.get()` so the check can fail softly.

use std::cell::RefCell;
use std::sync::{Arc, Barrier};

use golemdb_api::*;
use golemdb_cells::system;

use crate::arkiv::{ArkivDb, ArkivOptions};

thread_local! {
    static FAILURES: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    static BACKEND: RefCell<&'static str> = const { RefCell::new("") };
}

fn record_failure(line: u32, what: String) {
    let backend = BACKEND.with(|b| *b.borrow());
    FAILURES.with(|f| f.borrow_mut().push(format!("[{backend}] line {line}: {what}")));
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
                format!("{} == {}: left {:?}, right {:?}", stringify!($a), stringify!($b), a, b),
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

fn options() -> ArkivOptions {
    ArkivOptions {
        exact_version: true,
        redb_cache: 8 << 20,
        object_cache: 8 << 20,
        object_cache_entries: 16_384,
    }
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
    contract(Arc::new(ArkivDb::open_memory(&genesis(), options()).unwrap()));
    BACKEND.with(|b| *b.borrow_mut() = "arkiv-redb");
    {
        let dir = tempfile::tempdir().unwrap();
        contract(Arc::new(
            ArkivDb::open_durable(dir.path(), &genesis(), options()).unwrap(),
        ));
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
            pending.cells.get(b"#key".as_slice()).and_then(|v| v.as_bytes32()),
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
            bindings.cells.get(KEY.0.as_slice()).and_then(|v| v.as_u64()),
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
        // not portable: db.info().commit_id == 0 (golem OpenInfo).
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
        check_eq!(allocator(api.as_ref(), ReadTarget::Branch(recreated)), Some(66)); // golem-internal
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
    let (sealed, pending) = {
        let db = ArkivDb::open_durable(dir.path(), &genesis(), options()).unwrap();
        let branch = db.begin().unwrap();
        db.create(branch, input(KEY, 50)).unwrap();
        let sealed = db.seal(branch).unwrap();
        db.commit(branch).unwrap();
        let pending = db.begin().unwrap();
        db.delete(pending, RecordOp::delete(KEY)).unwrap();
        (sealed, pending)
    };
    let db = ArkivDb::reopen_durable(dir.path(), &genesis(), options()).unwrap();
    // not portable: info().created / genesis_id / state_root / index_root.
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
            empty.cells.get(b"#key".as_slice()).and_then(|v| v.as_bytes32()),
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
        let _seal = db.seal(branch).unwrap();
        // not portable: seal.index_root == db.info().index_root (golem OpenInfo).
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
        let db = ArkivDb::open_durable(dir.path(), &genesis(), options()).unwrap();
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
    let db = ArkivDb::reopen_durable(dir.path(), &genesis(), options()).unwrap();
    for key in [KEY, MISSING] {
        let record = db.get(ReadTarget::Head, RecordOp::get(key)).unwrap();
        check_eq!(record.cells.len(), 1);
        check_eq!(
            record.cells.get(b"#key".as_slice()).and_then(|v| v.as_bytes32()),
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
        ("timestamp64", CellValue::from_timestamp64(1_700_000_000_000_000)),
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
            op = op.field(format!("f_{name}").as_str(), value.clone()).unwrap();
        }
        let branch = db.begin().unwrap();
        db.create(branch, op).unwrap();
        let pending = get(db, ReadTarget::Branch(branch));
        db.commit(branch).unwrap();
        let committed = get(db, ReadTarget::Head);
        check_eq!(pending, committed);
        for (name, value) in &values {
            let field = committed.cells.get(format!("f_{name}").as_bytes());
            check_eq!(field, Some(&value.clone().with_kind(CellKind::Field).unwrap()));
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
            db.patch(branch, RecordOp::patch(KEY).field("f_str", value.clone()).unwrap())
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
    for exact_version in [true, false] {
        let options = ArkivOptions {
            exact_version,
            ..options()
        };
        BACKEND.with(|b| *b.borrow_mut() = "arkiv-memory");
        let db = ArkivDb::open_memory(&genesis(), options).unwrap();
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
    }
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
    check!(refused(ArkivDb::open_durable(dir.path(), &blake3, options())));
    check!(ArkivDb::open_durable(dir.path(), &genesis(), options()).is_ok());
    check!(refused(ArkivDb::reopen_durable(dir.path(), &blake3, options())));
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
        check!(engine_rejects(db.create(branch, bad).map(|_| ()), "too large"));
    }
    // A durable database keeps its limits: reopening with others fails.
    BACKEND.with(|b| *b.borrow_mut() = "arkiv-redb");
    let dir = tempfile::tempdir().unwrap();
    drop(ArkivDb::open_durable(dir.path(), &tight, options()).unwrap());
    check!(ArkivDb::reopen_durable(dir.path(), &tight, options()).is_ok());
    check!(ArkivDb::reopen_durable(dir.path(), &genesis(), options()).is_err());
    // A cell-name limit above the engine's 32-byte cap is refused at open.
    let wide = Genesis::new(
        HashAlgorithm::Keccak256,
        CellLimits {
            max_cell_name_len: 33,
            ..limits()
        },
    );
    check!(matches!(
        ArkivDb::open_memory(&wide, options()),
        Err(OpenError::InvalidConfig(_))
    ));
    finish();
}
