#![cfg(not(feature = "arkiv-v2"))]
// These retained fixtures pin the legacy generic/signed protocol.
// Arkiv v2 has separate host, profile and crash acceptance suites.
//! Static validation of transactions and admin inputs, applied to each
//! operation as it will execute (with the placeholder `$expiresAt`).

mod support;

use arkiv_db::{ArgumentFault, PriceSchedule, Value};
use arkiv_node::{
    AdminFault, ChainSpec, Expiry, ExpiryArgumentFault, TxEnvelope, TxFault, TxOp,
    validate::{check_admin, check_tx},
};
use support::*;

fn spec() -> ChainSpec {
    ChainSpec::new(genesis()).expect("valid genesis")
}

fn one(op: TxOp) -> TxEnvelope {
    tx(ALICE, 1, 1_000, vec![op])
}

fn cells(count: usize) -> Vec<(&'static str, arkiv_db::Cell)> {
    ["a", "b", "c", "d", "e", "f", "g", "h", "i"][..count]
        .iter()
        .map(|name| (*name, attr(Value::U64(1))))
        .collect()
}

#[test]
fn envelope_limits() {
    let spec = spec();
    let valid = one(create_namespace("alpha"));
    assert_eq!(check_tx(&spec, &valid), Ok(()));
    let mut other_chain = valid.clone();
    other_chain.chain_id = CHAIN + 1;
    assert_eq!(
        check_tx(&spec, &other_chain),
        Err(TxFault::WrongChain { found: CHAIN + 1 })
    );
    for budget in [0, 100_001] {
        let mut envelope = valid.clone();
        envelope.tx_budget = budget;
        assert_eq!(check_tx(&spec, &envelope), Err(TxFault::Budget));
    }
    let mut full = valid.clone();
    full.tx_budget = 100_000;
    assert_eq!(check_tx(&spec, &full), Ok(()));
    let empty = tx(ALICE, 1, 10, vec![]);
    assert_eq!(check_tx(&spec, &empty), Err(TxFault::OpCount));
    let nine = tx(
        ALICE,
        1,
        10,
        (0..9).map(|_| create_namespace("alpha")).collect(),
    );
    assert_eq!(check_tx(&spec, &nine), Err(TxFault::OpCount));
    // 72 + (1 + 8 + 5 + 4 + 4007) = 4097 bytes > 4096; one byte less fits.
    let big = one(put(1, b"k", &[0; 4007]));
    assert_eq!(
        check_tx(&spec, &big),
        Err(TxFault::TooLarge { bytes: 4097 })
    );
    assert_eq!(check_tx(&spec, &one(put(1, b"k", &[0; 4006]))), Ok(()));
}

#[test]
fn host_owned_cells_are_refused() {
    let spec = spec();
    for op in [
        create(1, b"k", vec![("$expiresAt", attr(Value::U64(5)))], None),
        create(1, b"k", vec![("$other", field(Value::U64(5)))], None),
        patch(
            1,
            b"k",
            vec![("$expiresAt", attr(Value::U64(5)))],
            vec![],
            None,
        ),
        patch(1, b"k", vec![], vec!["$expiresAt"], None),
        patch(1, b"k", vec![], vec!["$x"], Some(until(9))),
    ] {
        assert_eq!(
            check_tx(&spec, &tx(ALICE, 1, 10, vec![create_namespace("a"), op])),
            Err(TxFault::HostOwnedCell { op: 1 })
        );
    }
}

#[test]
fn expiry_pairs_are_checked_statically() {
    let spec = spec();
    let zero = Expiry {
        expires_at: 0,
        min_lifetime: 0,
    };
    assert_eq!(
        check_tx(&spec, &one(create(1, b"k", vec![], Some(zero)))),
        Err(TxFault::Expiry {
            op: 0,
            fault: ExpiryArgumentFault::Zero,
        })
    );
    let long = Expiry {
        expires_at: 5,
        min_lifetime: 101,
    };
    assert_eq!(
        check_tx(&spec, &one(patch(1, b"k", vec![], vec![], Some(long)))),
        Err(TxFault::Expiry {
            op: 0,
            fault: ExpiryArgumentFault::LifetimeTooLong,
        })
    );
    for fine in [
        Expiry {
            expires_at: 0,
            min_lifetime: 100,
        },
        Expiry::PERMANENT,
        until(1),
        // Heights are resolved at execution: these are refused there.
        until(u64::MAX - 1),
    ] {
        assert_eq!(
            check_tx(&spec, &one(create(1, b"k", vec![], Some(fine)))),
            Ok(())
        );
    }
}

/// With an expiry a record gets one cell fewer; an empty patch, with or
/// without an expiry, is valid.
#[test]
fn operations_are_validated_as_they_will_execute() {
    let spec = spec();
    assert_eq!(
        check_tx(&spec, &one(create(1, b"k", cells(8), None))),
        Ok(())
    );
    assert_eq!(
        check_tx(&spec, &one(create(1, b"k", cells(8), Some(until(5))))),
        Err(TxFault::Argument {
            op: 0,
            fault: ArgumentFault::TooManyCells,
        })
    );
    assert_eq!(
        check_tx(&spec, &one(create(1, b"k", cells(7), Some(until(5))))),
        Ok(())
    );
    // An empty patch is golem-db's no-op, valid statically.
    assert_eq!(
        check_tx(&spec, &one(patch(1, b"k", vec![], vec![], None))),
        Ok(())
    );
    assert_eq!(
        check_tx(&spec, &one(patch(1, b"k", vec![], vec![], Some(until(5))))),
        Ok(())
    );
    // A patch of 7 set and 1 removed name fills the 8-cell limit; with an
    // expiry it is one too many.
    let set = cells(7);
    assert_eq!(
        check_tx(&spec, &one(patch(1, b"k", set.clone(), vec!["z"], None))),
        Ok(())
    );
    assert_eq!(
        check_tx(&spec, &one(patch(1, b"k", set, vec!["z"], Some(until(5))))),
        Err(TxFault::Argument {
            op: 0,
            fault: ArgumentFault::TooManyCells,
        })
    );
}

#[test]
fn engine_argument_faults_are_static() {
    let spec = spec();
    let cases = [
        (put(1, &[0; 21], b"v"), ArgumentFault::KeyTooLong),
        (
            TxOp::Delete {
                namespace: ns(1),
                key: vec![0; 21],
            },
            ArgumentFault::KeyTooLong,
        ),
        (
            create(
                1,
                b"k",
                vec![("s", attr(Value::str(&"x".repeat(65))))],
                None,
            ),
            ArgumentFault::ValueTooLarge,
        ),
        (
            create(1, b"k", vec![("s", field(Value::bytes(&[0; 65])))], None),
            ArgumentFault::ValueTooLarge,
        ),
        (
            create(1, b"k", vec![("s", attr(Value::bytes(&[0; 4])))], None),
            ArgumentFault::BytesNotIndexable,
        ),
        (
            create(
                1,
                b"k",
                vec![("s", attr(Value::U64(1))), ("s", attr(Value::U64(2)))],
                None,
            ),
            ArgumentFault::DuplicateCell,
        ),
        (
            patch(1, b"k", vec![("s", attr(Value::U64(1)))], vec!["s"], None),
            ArgumentFault::SetAndRemoveOverlap,
        ),
        (
            patch(1, b"k", vec![], vec!["s", "s"], None),
            ArgumentFault::DuplicateCell,
        ),
    ];
    for (op, fault) in cases {
        assert_eq!(
            check_tx(&spec, &one(op.clone())),
            Err(TxFault::Argument { op: 0, fault }),
            "{op:?}"
        );
    }
    // The limits hold at their edges: a 20-byte raw key, a 64-byte
    // indexed value, a 64-byte field under max_value_len 64.
    for op in [
        put(1, &[0; 20], b"v"),
        create(
            1,
            b"k",
            vec![("s", attr(Value::str(&"x".repeat(64))))],
            None,
        ),
        create(1, b"k", vec![("s", field(Value::bytes(&[0; 64])))], None),
    ] {
        assert_eq!(check_tx(&spec, &one(op)), Ok(()));
    }
}

#[test]
fn admin_inputs_need_a_genesis_admin() {
    let spec = spec();
    let input = set_pricing(1, PriceSchedule::FIXTURE.with_id(2));
    assert_eq!(check_admin(&spec, &input), Ok(()));
    let mut other = input;
    other.actor = ALICE;
    assert_eq!(check_admin(&spec, &other), Err(AdminFault::NotAdmin));
}
