//! Configurable cell limits (golem-db's genesis `cell_limits`): the cell
//! name, `Str`, `Bytes` and indexed-string limits are part of [`Limits`],
//! enforced at admission, persisted in a durable genesis and checked when
//! the database is reopened.

use std::{ops::Bound, sync::Arc};

use arkiv_db::{
    ArgumentFault, Budget, Cell, CellName, Engine, Limits, NamespaceId, NamespaceName, OwnerId,
    PriceSchedule, Projection, Query, QueryFault, RecordKey, Rejection, StorageErrorKind, Term,
    TypeTag, Value, WriteOp, decode_exact,
    durable::{DurableEngine, DurableError, EngineGenesis, Initialization, Options},
    encode,
};
use arkiv_db_memory::MemoryVersionedStore;

const OWNER: OwnerId = OwnerId::new([3; 20]);

/// golem-db's facade test limits, with shorter names and indexed strings.
const SMALL: Limits = Limits {
    max_cell_name_len: 10,
    max_str_len: 64,
    max_bytes_len: 128,
    max_indexed_len: 16,
    ..Limits::DEFAULT
};

fn ns() -> NamespaceId {
    NamespaceId::from_ordinal(1).unwrap()
}

fn name(text: &str) -> CellName {
    CellName::new(text).unwrap()
}

fn engine(limits: Limits) -> (Engine, arkiv_db::Branch) {
    let engine = Engine::with_limits(PriceSchedule::FIXTURE, limits);
    let mut branch = engine.begin(engine.head(), None).unwrap();
    branch
        .execute(
            OWNER,
            WriteOp::CreateNamespace {
                name: NamespaceName::new("limits").unwrap(),
            },
            Budget::unlimited(),
        )
        .unwrap();
    (engine, branch)
}

fn create(branch: &mut arkiv_db::Branch, key: &[u8], cells: Vec<(CellName, Cell)>) -> Result<(), Rejection> {
    branch
        .execute(
            OWNER,
            WriteOp::CreateRecord {
                namespace: ns(),
                key: RecordKey::new(key).unwrap(),
                cells,
            },
            Budget::unlimited(),
        )
        .map(|_| ())
        .map_err(|rejected| rejected.reason)
}

fn kind(error: DurableError) -> StorageErrorKind {
    match error {
        DurableError::Storage(error) => error.kind,
        other => panic!("not a storage error: {other:?}"),
    }
}

fn invalid(fault: ArgumentFault) -> Result<(), Rejection> {
    Err(Rejection::InvalidArgument(fault))
}

#[test]
fn defaults_are_the_hard_caps() {
    assert_eq!(Limits::DEFAULT.max_cell_name_len, CellName::MAX_LEN);
    assert_eq!(Limits::DEFAULT.max_indexed_len, Value::MAX_INDEXED_LEN);
    assert_eq!((CellName::MAX_LEN, Value::MAX_INDEXED_LEN), (32, 128));
    assert_eq!(Limits::DEFAULT.max_str_len, 4096);
    assert_eq!(Limits::DEFAULT.max_bytes_len, 4096);
    assert_eq!(Limits::DEFAULT.validate(), Ok(()));
    assert_eq!(SMALL.validate(), Ok(()));
    assert_eq!(SMALL.indexed_value_len(), 16);
    // The indexed bound never exceeds the Str limit.
    let tight = Limits {
        max_str_len: 8,
        ..SMALL
    };
    assert_eq!(tight.indexed_value_len(), 8);
}

#[test]
fn limits_above_the_caps_are_invalid() {
    for (limits, reason) in [
        (
            Limits {
                max_cell_name_len: 0,
                ..Limits::DEFAULT
            },
            "max_cell_name_len must be positive",
        ),
        (
            Limits {
                max_cell_name_len: CellName::MAX_LEN + 1,
                ..Limits::DEFAULT
            },
            "max_cell_name_len exceeds the term-key name width (32)",
        ),
        (
            Limits {
                max_indexed_len: Value::MAX_INDEXED_LEN + 1,
                ..Limits::DEFAULT
            },
            "max_indexed_len exceeds the term-key value width (128)",
        ),
    ] {
        assert_eq!(limits.validate(), Err(reason));
        assert!(std::panic::catch_unwind(|| Engine::with_limits(PriceSchedule::FIXTURE, limits)).is_err());
        let mut genesis = EngineGenesis::new([1; 32], PriceSchedule::FIXTURE);
        genesis.limits = limits;
        assert_eq!(kind(genesis.validate().unwrap_err()), StorageErrorKind::Incompatible);
    }
    // Zero is a valid string, bytes or indexed limit, as in golem-db.
    let zero = Limits {
        max_str_len: 0,
        max_bytes_len: 0,
        max_indexed_len: 0,
        ..Limits::DEFAULT
    };
    assert_eq!(zero.validate(), Ok(()));
}

#[test]
fn names_longer_than_the_limit_are_rejected_in_every_write() {
    let (_engine, mut branch) = engine(SMALL);
    let ok = name(&"n".repeat(10));
    let long = name(&"n".repeat(11));
    assert_eq!(
        create(&mut branch, b"a", vec![(ok.clone(), Cell::field(Value::Bool(true)))]),
        Ok(())
    );
    assert_eq!(
        create(&mut branch, b"b", vec![(long.clone(), Cell::field(Value::Bool(true)))]),
        invalid(ArgumentFault::NameTooLong)
    );
    for (set, remove) in [
        (vec![(long.clone(), Cell::attribute(Value::U64(1)))], vec![]),
        (vec![], vec![long.clone()]),
    ] {
        let result = branch.execute(
            OWNER,
            WriteOp::PatchRecord {
                namespace: ns(),
                key: RecordKey::new(b"a").unwrap(),
                set,
                remove,
            },
            Budget::unlimited(),
        );
        assert_eq!(
            result.map(|_| ()).map_err(|rejected| rejected.reason),
            invalid(ArgumentFault::NameTooLong)
        );
    }
    // The name limit counts the `$` prefix, as golem-db does.
    assert_eq!(
        create(
            &mut branch,
            b"c",
            vec![(name(&format!("${}", "n".repeat(10))), Cell::field(Value::Bool(true)))]
        ),
        invalid(ArgumentFault::NameTooLong)
    );
}

#[test]
fn str_bytes_and_indexed_strings_have_separate_limits() {
    let (_engine, mut branch) = engine(SMALL);
    let cases = [
        (Cell::field(Value::str(&"s".repeat(64))), Ok(())),
        (
            Cell::field(Value::str(&"s".repeat(65))),
            invalid(ArgumentFault::ValueTooLarge),
        ),
        (Cell::field(Value::bytes(&[1; 128])), Ok(())),
        (
            Cell::field(Value::bytes(&[1; 129])),
            invalid(ArgumentFault::ValueTooLarge),
        ),
        (Cell::attribute(Value::str(&"s".repeat(16))), Ok(())),
        (
            Cell::attribute(Value::str(&"s".repeat(17))),
            invalid(ArgumentFault::ValueTooLarge),
        ),
        // A 17-byte string is a valid field: only indexing is bounded.
        (Cell::field(Value::str(&"s".repeat(17))), Ok(())),
        // Fixed-width types have no configurable limit.
        (Cell::attribute(Value::Bytes32([1; 32])), Ok(())),
    ];
    for (index, (cell, expected)) in cases.into_iter().enumerate() {
        assert_eq!(
            create(&mut branch, &[index as u8 + 1], vec![(name("c"), cell.clone())]),
            expected,
            "{cell:?}"
        );
    }
}

#[test]
fn query_strings_obey_the_indexed_limit() {
    let (mut engine, mut branch) = engine(SMALL);
    create(&mut branch, b"a", vec![(name("c"), Cell::attribute(Value::str("abc")))]).unwrap();
    engine.publish(&branch.seal()).unwrap();
    let snapshot = engine.head_snapshot();
    let long = "x".repeat(17);
    for term in [
        Term::eq(name("c"), Value::str(&long)),
        Term::starts_with(name("c"), &long),
        Term::range(name("c"), Bound::Included(Value::str(&long)), Bound::Unbounded),
    ] {
        let rejected = snapshot
            .query(
                ns(),
                &Query::term(term),
                &Projection::ID_ONLY,
                5,
                None,
                Budget::unlimited(),
            )
            .unwrap_err();
        assert_eq!(
            rejected.reason,
            Rejection::InvalidQuery(QueryFault::ValueTooLarge)
        );
    }
    let rejected = snapshot
        .terms(
            ns(),
            &name("c"),
            TypeTag::Str,
            Bound::Included(Value::str(&long)),
            5,
            Budget::unlimited(),
        )
        .unwrap_err();
    assert_eq!(
        rejected.reason,
        Rejection::InvalidQuery(QueryFault::ValueTooLarge)
    );
}

#[test]
fn durable_genesis_persists_the_limits_and_rejects_a_mismatch() {
    let store = Arc::new(MemoryVersionedStore::new());
    let mut genesis = EngineGenesis::new([5; 32], PriceSchedule::FIXTURE);
    genesis.limits = SMALL;
    assert_eq!(decode_exact::<EngineGenesis>(&encode(&genesis)).unwrap(), genesis);
    let Initialization::Committed(engine) =
        DurableEngine::initialize(store.clone(), genesis.clone(), Options::default()).unwrap()
    else {
        panic!("initialization commits");
    };
    assert_eq!(engine.genesis().limits, SMALL);
    drop(engine);
    let reopened = DurableEngine::open(store.clone(), genesis.clone(), Options::default()).unwrap();
    assert_eq!(reopened.genesis().limits, SMALL);
    drop(reopened);
    for changed in [
        Limits {
            max_cell_name_len: 11,
            ..SMALL
        },
        Limits {
            max_str_len: 65,
            ..SMALL
        },
        Limits {
            max_bytes_len: 129,
            ..SMALL
        },
        Limits {
            max_indexed_len: 17,
            ..SMALL
        },
    ] {
        let mut other = genesis.clone();
        other.limits = changed;
        let error = DurableEngine::open(store.clone(), other, Options::default()).unwrap_err();
        assert_eq!(kind(error), StorageErrorKind::Incompatible, "{changed:?}");
    }
}
