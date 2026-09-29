#![cfg(not(feature = "arkiv-v2"))]
// These retained fixtures pin the legacy generic/signed protocol.
// Arkiv v2 has separate host, profile and crash acceptance suites.
//! The request-identity state machine: every transition of the pending and
//! inclusion indexes, scope separation, in-flight identities and block
//! validity checks.

mod support;

use arkiv_db::{B256, PriceSchedule};
use arkiv_node::{
    BlockFault, BlockInputs, ChainSpec, Inclusion, MemoryChain, RequestIdentity, RequestIndex,
    RequestStatus, Scope, Submission,
};
use support::*;

fn identity(scope: Scope, actor: arkiv_db::OwnerId, n: u64) -> RequestIdentity {
    RequestIdentity {
        scope,
        chain_id: CHAIN,
        actor,
        request_id: request(n),
    }
}

fn digest(byte: u8) -> B256 {
    B256::new([byte; 32])
}

#[test]
fn unknown_identities_are_accepted_once_then_pending() {
    let mut index = RequestIndex::new();
    let id = identity(Scope::Tx, ALICE, 1);
    assert_eq!(index.status(&id), RequestStatus::Unknown);
    assert_eq!(index.submit(id, digest(1)), Submission::Accepted);
    assert_eq!(
        index.status(&id),
        RequestStatus::Pending { digest: digest(1) }
    );
    // Pending + same digest: pending, nothing changes.
    assert_eq!(index.submit(id, digest(1)), Submission::Pending);
    // Pending + different digest: conflict, the pending digest stays.
    assert_eq!(
        index.submit(id, digest(2)),
        Submission::Conflict {
            existing: digest(1)
        }
    );
    assert_eq!(
        index.status(&id),
        RequestStatus::Pending { digest: digest(1) }
    );
    assert_eq!((index.pending_len(), index.included_len()), (1, 0));
}

#[test]
fn dropped_identities_are_forgotten() {
    let mut index = RequestIndex::new();
    let id = identity(Scope::Tx, ALICE, 1);
    assert!(!index.drop_pending(&id));
    assert_eq!(index.submit(id, digest(1)), Submission::Accepted);
    assert!(index.drop_pending(&id));
    assert_eq!(index.status(&id), RequestStatus::Unknown);
    // Once dropped, other content may use the identity.
    assert_eq!(index.submit(id, digest(2)), Submission::Accepted);
}

#[test]
fn committed_identities_move_to_the_inclusion_index() {
    let mut index = RequestIndex::new();
    let envelope = tx(ALICE, 1, 10, vec![create_namespace("alpha")]);
    let admin = set_pricing(1, PriceSchedule::FIXTURE.with_id(2));
    let tx_id = identity(Scope::Tx, ALICE, 1);
    let admin_id = identity(Scope::Admin, ADMIN, 1);
    assert_eq!(index.submit(tx_id, envelope.digest()), Submission::Accepted);
    assert_eq!(index.submit(admin_id, admin.digest()), Submission::Accepted);
    // An identity that was never pending (a replayed block) is included too.
    let other = tx(BOB, 5, 10, vec![create_namespace("beta")]);
    let inputs = BlockInputs {
        admin: vec![admin.clone()],
        txs: vec![envelope.clone(), other.clone()],
    };
    index.commit_block(CHAIN, 4, &inputs).expect("valid block");
    assert_eq!((index.pending_len(), index.included_len()), (0, 3));
    let tx_inclusion = Inclusion {
        height: 4,
        scope: Scope::Tx,
        index: 0,
        digest: envelope.digest(),
    };
    assert_eq!(index.status(&tx_id), RequestStatus::Included(tx_inclusion));
    assert_eq!(
        index.status(&admin_id),
        RequestStatus::Included(Inclusion {
            height: 4,
            scope: Scope::Admin,
            index: 0,
            digest: admin.digest(),
        })
    );
    assert_eq!(
        index.status(&identity(Scope::Tx, BOB, 5)),
        RequestStatus::Included(Inclusion {
            height: 4,
            scope: Scope::Tx,
            index: 1,
            digest: other.digest(),
        })
    );
    // Included + same digest: the inclusion; included + different: conflict.
    assert_eq!(
        index.submit(tx_id, envelope.digest()),
        Submission::Included(tx_inclusion)
    );
    assert_eq!(
        index.submit(tx_id, digest(9)),
        Submission::Conflict {
            existing: envelope.digest()
        }
    );
    // Included identities are never dropped.
    assert!(!index.drop_pending(&tx_id));
    assert_eq!(index.status(&tx_id), RequestStatus::Included(tx_inclusion));
}

#[test]
fn scopes_chains_and_actors_separate_identities() {
    let mut index = RequestIndex::new();
    let base = identity(Scope::Tx, ALICE, 1);
    assert_eq!(index.submit(base, digest(1)), Submission::Accepted);
    for other in [
        RequestIdentity {
            scope: Scope::Admin,
            ..base
        },
        RequestIdentity {
            chain_id: CHAIN + 1,
            ..base
        },
        RequestIdentity { actor: BOB, ..base },
        RequestIdentity {
            request_id: request(2),
            ..base
        },
    ] {
        assert_eq!(index.submit(other, digest(2)), Submission::Accepted);
    }
    assert_eq!(index.pending_len(), 5);
}

/// A request inside a block being produced stays pending: a resubmission
/// with the same content is `Pending`, with other content `Conflict`, until
/// the block commits; a candidate that is discarded (never committed)
/// leaves it pending.
#[test]
fn in_flight_identities_stay_pending() {
    let mut index = RequestIndex::new();
    let envelope = tx(ALICE, 1, 10, vec![create_namespace("alpha")]);
    let id = identity(Scope::Tx, ALICE, 1);
    assert_eq!(index.submit(id, envelope.digest()), Submission::Accepted);
    // The producer takes it into a candidate: nothing changes in the index.
    let inputs = BlockInputs {
        admin: Vec::new(),
        txs: vec![envelope.clone()],
    };
    assert_eq!(index.submit(id, envelope.digest()), Submission::Pending);
    assert_eq!(
        index.submit(id, digest(3)),
        Submission::Conflict {
            existing: envelope.digest()
        }
    );
    // The candidate is discarded: still pending.
    assert_eq!(
        index.status(&id),
        RequestStatus::Pending {
            digest: envelope.digest()
        }
    );
    // The next candidate commits.
    index.commit_block(CHAIN, 1, &inputs).expect("valid block");
    assert!(matches!(index.status(&id), RequestStatus::Included(_)));
}

#[test]
fn blocks_with_repeated_or_included_identities_are_invalid() {
    let mut index = RequestIndex::new();
    let first = tx(ALICE, 1, 10, vec![create_namespace("alpha")]);
    let block = BlockInputs {
        admin: vec![set_pricing(1, PriceSchedule::FIXTURE.with_id(2))],
        txs: vec![first.clone()],
    };
    index.commit_block(CHAIN, 1, &block).expect("valid block");
    // Already included, with the same or other content.
    for envelope in [
        first.clone(),
        tx(ALICE, 1, 11, vec![create_namespace("beta")]),
    ] {
        let inputs = BlockInputs {
            admin: Vec::new(),
            txs: vec![tx(BOB, 1, 10, vec![create_namespace("b")]), envelope],
        };
        assert_eq!(
            index.check_block(CHAIN, &inputs),
            Err(BlockFault::AlreadyIncluded {
                scope: Scope::Tx,
                index: 1
            })
        );
        assert_eq!(
            index.commit_block(CHAIN, 2, &inputs),
            Err(BlockFault::AlreadyIncluded {
                scope: Scope::Tx,
                index: 1
            })
        );
    }
    let admin_again = BlockInputs {
        admin: vec![set_pricing(1, PriceSchedule::FIXTURE.with_id(3))],
        txs: Vec::new(),
    };
    assert_eq!(
        index.check_block(CHAIN, &admin_again),
        Err(BlockFault::AlreadyIncluded {
            scope: Scope::Admin,
            index: 0
        })
    );
    // Repeated within one block.
    let twice = BlockInputs {
        admin: vec![
            set_pricing(7, PriceSchedule::FIXTURE.with_id(4)),
            set_pricing(7, PriceSchedule::FIXTURE.with_id(5)),
        ],
        txs: Vec::new(),
    };
    assert_eq!(
        index.check_block(CHAIN, &twice),
        Err(BlockFault::DuplicateIdentity {
            scope: Scope::Admin,
            index: 1
        })
    );
    // A failed commit changes nothing.
    assert_eq!(index.included_len(), 2);
    // The same actor and request id in both scopes is one of each.
    let both = BlockInputs {
        admin: vec![set_pricing(9, PriceSchedule::FIXTURE.with_id(6))],
        txs: vec![tx(ADMIN, 9, 10, vec![create_namespace("c")])],
    };
    assert_eq!(index.check_block(CHAIN, &both), Ok(()));
}

/// The memory chain records inclusions with their heights.
#[test]
fn chains_record_inclusions() {
    let mut chain = MemoryChain::new(ChainSpec::new(genesis()).expect("valid genesis"));
    let envelope = tx(ALICE, 1, 1_000, vec![create_namespace("alpha")]);
    chain.produce(2_000, txs(vec![])).expect("empty block");
    chain
        .produce(3_000, txs(vec![envelope.clone()]))
        .expect("block 2");
    assert_eq!(
        chain.requests().status(&identity(Scope::Tx, ALICE, 1)),
        RequestStatus::Included(Inclusion {
            height: 2,
            scope: Scope::Tx,
            index: 0,
            digest: envelope.digest(),
        })
    );
}
