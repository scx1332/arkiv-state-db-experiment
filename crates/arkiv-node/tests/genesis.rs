#![cfg(not(feature = "arkiv-v2"))]
// These retained fixtures pin the legacy generic/signed protocol.
// Arkiv v2 has separate host, profile and crash acceptance suites.
//! Genesis invariants, the expiry reservation and the sweep read bound,
//! against hand-derived values.

mod support;

use arkiv_db::{Limits, OperationClass, PriceSchedule};
use arkiv_node::{
    ChainParams, ChainSpec, GenesisError, ReservationFault,
    params::{check_reservation, max_expiry_delete_cost, max_expiry_read_cost, sweep_read_bound},
};
use support::*;

fn spec(params: ChainParams, limits: Limits) -> Result<ChainSpec, GenesisError> {
    ChainSpec::new(genesis_with(params, limits, PriceSchedule::FIXTURE))
}

#[test]
fn fixture_costs_are_the_hand_derived_ones() {
    // DirectoryLookup 1 + PointRead 3 + RecordDelete 20 + 8 × CellRemove 4
    // + 3 × RootUpdate 2 + 16 × (IndexRemove 6 + 2 × RootUpdate 2) = 222.
    assert_eq!(
        max_expiry_delete_cost(&PriceSchedule::FIXTURE, &LIMITS),
        Some(222)
    );
    // terms: 1 + TermSeek 4 + TermVisit 3 + 8 × ReadByte 1 = 16; a page of
    // four: 1 + QueryGroup 5 + TermSeek 4 + 5 × PostingEntry 1 + 4 ×
    // (ResultRow 3 + 32 × ReadByte 1) = 155.
    assert_eq!(
        sweep_read_bound(&PriceSchedule::FIXTURE, &LIMITS),
        Some(155)
    );
    // Pages of one row: 1 + 5 + 4 + 2 × 1 + (3 + 32) = 47 against the terms
    // read's 16.
    let one = Limits {
        max_page: 1,
        ..LIMITS
    };
    assert_eq!(sweep_read_bound(&PriceSchedule::FIXTURE, &one), Some(47));
    // A dear TermVisit makes the terms read the larger: 1 + 4 + 1,000 + 8 =
    // 1,013 against the page's 155 (a page charges no TermVisit).
    let dear_terms = PriceSchedule::FIXTURE.with_price(OperationClass::TermVisit, 1_000);
    assert_eq!(sweep_read_bound(&dear_terms, &LIMITS), Some(1_013));
    // The host's expiry pre-read: DirectoryLookup 1 + PointRead 3 +
    // ResultRow 3 + 18 × ReadByte 1 (`$expiresAt` 10 bytes, a U64 8) = 25.
    assert_eq!(max_expiry_read_cost(&PriceSchedule::FIXTURE), Some(25));
}

/// The host's expiry pre-read runs with an unlimited budget, so the
/// reservation also requires its cost to be representable. At the edge
/// the deletion reserve and the sweep bound fit easily; only the pre-read
/// decides.
#[test]
fn the_expiry_read_must_be_representable() {
    let params = ChainParams {
        block_budget: u64::MAX,
        expiry_per_block: 1,
        ..PARAMS
    };
    let limits = Limits {
        max_page: 1,
        ..LIMITS
    };
    // 1 + (2^63 − 20) + 2^63 + 18 × 1 = 2^64 − 1.
    let edge = PriceSchedule::FIXTURE
        .with_price(OperationClass::PointRead, (1 << 63) - 20)
        .with_price(OperationClass::ResultRow, 1 << 63);
    assert_eq!(max_expiry_read_cost(&edge), Some(u64::MAX));
    assert_eq!(check_reservation(&params, &limits, &edge), Ok(()));
    let over = edge.with_price(OperationClass::PointRead, (1 << 63) - 19);
    assert_eq!(max_expiry_read_cost(&over), None);
    // Deletion: 1 + (2^63 − 19) + 20 + 8 × 4 + 3 × 2 + 16 × (6 + 2 × 2) =
    // 2^63 + 200. Sweep: max(1 + 4 + 3 + 8, 1 + 5 + 4 + 2 × 1 + (2^63 +
    // 32 × 1)) = 2^63 + 44.
    assert_eq!(
        max_expiry_delete_cost(&over, &limits),
        Some((1 << 63) + 200)
    );
    assert_eq!(sweep_read_bound(&over, &limits), Some((1 << 63) + 44));
    assert_eq!(
        check_reservation(&params, &limits, &over),
        Err(ReservationFault::Overflow)
    );
    assert_eq!(
        ChainSpec::new(genesis_with(params, limits, over)).err(),
        Some(GenesisError::Reservation(ReservationFault::Overflow))
    );
}

#[test]
fn the_reservation_is_exact() {
    // PARAMS reserves exactly 4 × 222 = 888 on top of the user budget.
    assert_eq!(
        check_reservation(&PARAMS, &LIMITS, &PriceSchedule::FIXTURE),
        Ok(())
    );
    let short = ChainParams {
        block_budget: 100_887,
        ..PARAMS
    };
    assert_eq!(
        check_reservation(&short, &LIMITS, &PriceSchedule::FIXTURE),
        Err(ReservationFault::Violated {
            required: 100_888,
            block_budget: 100_887,
        })
    );
    assert_eq!(
        spec(short, LIMITS).err(),
        Some(GenesisError::Reservation(ReservationFault::Violated {
            required: 100_888,
            block_budget: 100_887,
        }))
    );
    let huge = PriceSchedule::FIXTURE.with_price(OperationClass::CellRemove, u64::MAX / 4);
    assert_eq!(
        check_reservation(&PARAMS, &LIMITS, &huge),
        Err(ReservationFault::Overflow)
    );
    let huge_read = PriceSchedule::FIXTURE.with_price(OperationClass::ReadByte, u64::MAX / 8);
    assert_eq!(
        check_reservation(&PARAMS, &LIMITS, &huge_read),
        Err(ReservationFault::Overflow)
    );
}

#[test]
fn every_invariant_is_checked() {
    assert!(spec(PARAMS, LIMITS).is_ok());
    let cases = [
        (
            ChainParams {
                max_ops_per_tx: 0,
                ..PARAMS
            },
            GenesisError::OpsPerTx,
        ),
        (
            ChainParams {
                max_ops_per_tx: 65,
                ..PARAMS
            },
            GenesisError::OpsPerTx,
        ),
        (
            ChainParams {
                max_txs_per_block: 0,
                ..PARAMS
            },
            GenesisError::TxsPerBlock,
        ),
        (
            ChainParams {
                expiry_per_block: 0,
                ..PARAMS
            },
            GenesisError::ExpiryPerBlock,
        ),
        (
            ChainParams {
                user_budget: 0,
                ..PARAMS
            },
            GenesisError::UserBudget,
        ),
        // 8 + 2 × 309 + 4096 = 4722 is the least that fits.
        (
            ChainParams {
                max_block_input_bytes: 4_721,
                ..PARAMS
            },
            GenesisError::InputBytes,
        ),
        (
            ChainParams {
                max_admin_per_block: u32::MAX,
                ..PARAMS
            },
            GenesisError::InputBytes,
        ),
    ];
    for (params, error) in cases {
        assert_eq!(spec(params, LIMITS).err(), Some(error), "{params:?}");
    }
    assert!(
        spec(
            ChainParams {
                max_ops_per_tx: 64,
                ..PARAMS
            },
            LIMITS
        )
        .is_ok()
    );
    let limit_cases = [
        (
            Limits {
                max_cells: 0,
                ..LIMITS
            },
            GenesisError::MaxCells,
        ),
        (
            Limits {
                max_page: 0,
                ..LIMITS
            },
            GenesisError::MaxPage,
        ),
        (
            Limits {
                max_groups: 0,
                ..LIMITS
            },
            GenesisError::MaxGroups,
        ),
        (
            Limits {
                max_literals: 0,
                ..LIMITS
            },
            GenesisError::MaxLiterals,
        ),
    ];
    for (limits, error) in limit_cases {
        assert_eq!(spec(PARAMS, limits).err(), Some(error), "{limits:?}");
    }
    // One of each is enough: the sweep's query has one group of one
    // literal, and the host reads one named cell.
    let smallest = Limits {
        max_cells: 1,
        max_page: 1,
        max_groups: 1,
        max_literals: 1,
        ..LIMITS
    };
    assert!(spec(PARAMS, smallest).is_ok());
}

#[test]
fn versions_are_checked() {
    let mut bad = genesis();
    bad.format_version = 2;
    assert_eq!(
        ChainSpec::new(bad).err(),
        Some(GenesisError::UnsupportedFormat(2))
    );
    for protocol in [0, 3] {
        let mut bad = genesis();
        bad.protocol_version = protocol;
        assert_eq!(
            ChainSpec::new(bad).err(),
            Some(GenesisError::UnsupportedProtocol(protocol))
        );
    }
}

#[test]
fn the_spec_exposes_the_genesis() {
    let spec = spec(PARAMS, LIMITS).expect("valid");
    assert_eq!(spec.chain_id(), CHAIN);
    assert_eq!(spec.protocol_version(), 1);
    assert_eq!(spec.header_version(), 1);
    assert!(spec.is_admin(&ADMIN));
    assert!(!spec.is_admin(&ALICE));
    assert_eq!(spec.genesis_header(), &genesis().header());
    assert_eq!(spec.genesis_hash(), genesis().hash());
    assert_eq!(spec.genesis_header().height, 0);
}
