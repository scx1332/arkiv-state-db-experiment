use alloy_primitives::U256;
use arkiv_db::{Budget, Cell, CellName, OwnerId, Projection, RecordKey, Value, durable::Options};
use arkiv_db_memory::MemoryVersionedStore;
use arkiv_node::{
    Expiry, HostFault, OpOutcome, TxEnvelope, TxOp, TxStatus,
    accounts::{self, AccountConfig},
    arkiv::{self, ArkivFault, ArkivOperation},
    gas,
    signed::{
        crypto::Signer,
        history::History,
        host::Host,
        protocol::{SignedGenesis, SignedHeader, SignedTx},
    },
};
use std::sync::Arc;
mod signed_support;
fn signer() -> Signer {
    Signer::from_bytes([1; 32]).unwrap()
}
fn genesis() -> SignedGenesis {
    signed_support::genesis(9200, 1000, [8; 16], 1, 5000, &[1, 2])
}
fn host(producer: bool) -> Host {
    host_of(genesis(), producer)
}
fn host_of(g: SignedGenesis, producer: bool) -> Host {
    let cert = SignedHeader::sign(&g, g.header(), &signer()).unwrap();
    Host::create(
        Arc::new(MemoryVersionedStore::new()),
        History::create(Arc::new(MemoryVersionedStore::new()), g.hash()).unwrap(),
        g,
        [3; 16],
        cert,
        producer.then(signer),
        Options::default(),
    )
    .unwrap()
}
/// An entity with an indexed `group` and `body` bytes of unindexed data.
fn create(salt: u128, body: usize, lifetime: u64) -> ArkivOperation {
    ArkivOperation::Create {
        salt,
        flags: 0,
        expiry: Expiry {
            expires_at: 0,
            min_lifetime: lifetime,
        },
        attributes: vec![
            (
                CellName::new("body").unwrap(),
                Cell::field(Value::bytes(&vec![42; body])),
            ),
            (
                CellName::new("group").unwrap(),
                Cell::attribute(Value::U64(1)),
            ),
        ],
    }
}
fn tx(h: &Host, nonce: u64, budget: u64, ops: Vec<ArkivOperation>) -> SignedTx {
    tx_by(h, &signer(), nonce, budget, ops)
}
fn tx_by(h: &Host, by: &Signer, nonce: u64, budget: u64, ops: Vec<ArkivOperation>) -> SignedTx {
    SignedTx::sign(
        &h.genesis,
        nonce,
        TxEnvelope {
            chain_id: 9200,
            actor: by.address(),
            request_id: [nonce as u8; 32],
            tx_budget: budget,
            ops: ops
                .into_iter()
                .map(|operation| TxOp::Arkiv {
                    namespace: arkiv::entity_namespace(),
                    operation,
                })
                .collect(),
        },
        by,
    )
    .unwrap()
}
fn key(nonce: u64, salt: u128) -> RecordKey {
    key_of(&signer(), nonce, salt)
}
fn key_of(by: &Signer, nonce: u64, salt: u128) -> RecordKey {
    RecordKey::new(&arkiv::derive_entity_key(
        9200,
        by.address().as_bytes(),
        nonce,
        salt,
    ))
    .unwrap()
}
/// The chain's rent clock: 5-second blocks, a one-year cap.
fn clock(h: &Host) -> gas::RentClock {
    gas::RentClock::of(&h.genesis.genesis.params).unwrap()
}
fn balance(h: &Host, actor: OwnerId) -> U256 {
    h.account(h.head.height, actor).unwrap().balance
}
fn burnt(h: &Host) -> U256 {
    accounts::burned(
        h.engine
            .head_snapshot()
            .try_host_get(&accounts::BURN_KEY)
            .unwrap(),
    )
    .unwrap()
}
fn permanent(salt: u128, body: usize) -> ArkivOperation {
    let mut op = create(salt, body, 0);
    if let ArkivOperation::Create { expiry, .. } = &mut op {
        *expiry = Expiry::PERMANENT;
    }
    op
}
/// A patch adding an unindexed `more` column of `bytes` bytes.
fn grow(key: &RecordKey, bytes: usize) -> ArkivOperation {
    ArkivOperation::Patch {
        key: key.clone(),
        set: vec![(
            CellName::new("more").unwrap(),
            Cell::field(Value::bytes(&vec![7; bytes])),
        )],
        remove: vec![],
    }
}
fn cells(h: &Host, key: &RecordKey) -> Option<Vec<(CellName, arkiv_db::Cell)>> {
    h.engine
        .head_snapshot()
        .try_get_record(
            arkiv::entity_namespace(),
            key,
            &Projection::ALL,
            Budget::unlimited(),
        )
        .unwrap()
        .unwrap()
        .value
        .map(|r| r.cells)
}
#[test]
fn costs_rent_replay_and_exact_budget() {
    let mut p = host(true);
    let mut f = host(false);
    p.produce().unwrap();
    f.import(p.block(1).unwrap()).unwrap();
    for (nonce, days) in [(1, 1), (2, 7), (3, 30)] {
        p.submit(tx(
            &p,
            nonce,
            1_000_000,
            vec![create(nonce as u128, 1024, days * 17280)],
        ))
        .unwrap();
        p.produce().unwrap();
        let b = p.block(p.head.height).unwrap();
        assert_eq!(b.outcomes.txs[0].status, TxStatus::Committed);
        let gas = b.outcomes.txs[0].spent;
        println!("1KiB {days} days: {gas} gas");
        assert!((50_000..=1_000_000).contains(&gas));
        assert_eq!(
            gas,
            gas::TX_INTRINSIC_GAS
                + b.outcomes.txs[0]
                    .ops
                    .iter()
                    .map(OpOutcome::spent)
                    .sum::<u64>()
        );
        assert_eq!(b.outcomes.spent, gas);
        // Rent is recorded and burned beside the gas, never inside it.
        let row = cells(&p, &key(nonce - 1, nonce as u128)).unwrap();
        let units = gas::storage_units(&row).unwrap().total;
        let rent = gas::rent(
            0,
            units,
            None,
            Some(p.head.height + days * 17280),
            p.head.height,
            clock(&p),
        )
        .unwrap()
        .wei;
        assert_eq!(b.outcomes.txs[0].rent_wei, U256::from(rent));
        f.import(b).unwrap();
        assert_eq!(p.head, f.head);
    }
    let charge = p.block(2).unwrap().outcomes.txs[0].spent;
    p.submit(tx(&p, 4, charge - 1, vec![create(4, 1024, 17280)]))
        .unwrap();
    p.produce().unwrap();
    assert!(matches!(
        p.block(5).unwrap().outcomes.txs[0].status,
        TxStatus::Failed { .. }
    ));
    assert!(cells(&p, &key(3, 4)).is_none());
    p.submit(tx(&p, 5, charge, vec![create(5, 1024, 17280)]))
        .unwrap();
    p.produce().unwrap();
    assert_eq!(
        p.block(6).unwrap().outcomes.txs[0].status,
        TxStatus::Committed
    );
}
#[test]
fn permanent_create_pays_rent_outside_gas_and_rollback_retains_gas_and_nonce() {
    let mut p = host(true);
    let mut f = host(false);
    p.produce().unwrap();
    f.import(p.block(1).unwrap()).unwrap();
    let actor = signer().address();
    // A live permanent create fits an ordinary gas budget: rent is not gas.
    let cap = clock(&p).cap_blocks();
    assert_eq!(cap, 6_307_200);
    let at_cap = {
        let mut op = create(9, 1024, 0);
        if let ArkivOperation::Create { expiry, .. } = &mut op {
            *expiry = Expiry {
                expires_at: 0,
                min_lifetime: cap,
            };
        }
        op
    };
    let (balance0, burnt0) = (balance(&p, actor), burnt(&p));
    p.submit(tx(&p, 1, 1_000_000, vec![permanent(1, 1024)]))
        .unwrap();
    p.produce().unwrap();
    p.submit(tx(&p, 2, 1_000_000, vec![at_cap])).unwrap();
    p.produce().unwrap();
    let (permanent_tx, at_cap_tx) = (
        p.block(2).unwrap().outcomes.txs[0].clone(),
        p.block(3).unwrap().outcomes.txs[0].clone(),
    );
    assert_eq!(permanent_tx.status, TxStatus::Committed);
    assert_eq!(at_cap_tx.status, TxStatus::Committed);
    // The same execution gas and the same capped rent: a year of the entity.
    assert_eq!(permanent_tx.spent, at_cap_tx.spent);
    assert!(permanent_tx.spent < 200_000);
    assert_eq!(permanent_tx.rent_wei, at_cap_tx.rent_wei);
    let entity = key(0, 1);
    let units = gas::storage_units(&cells(&p, &entity).unwrap())
        .unwrap()
        .total;
    let year = U256::from(units) * U256::from(7_300_000_000_000u64);
    assert_eq!(permanent_tx.rent_wei, year);
    // The sender paid gas at 1 gwei plus the rent; both were burned.
    let paid = accounts::fee(permanent_tx.spent + at_cap_tx.spent)
        + permanent_tx.rent_wei
        + at_cap_tx.rent_wei;
    assert_eq!(balance(&p, actor), balance0 - paid);
    assert_eq!(burnt(&p), burnt0 + paid);
    for height in 2..=3 {
        f.import(p.block(height).unwrap()).unwrap();
    }
    assert_eq!(f.head, p.head);
    // Many blocks later it is still there and was never due.
    for _ in 0..20 {
        p.produce().unwrap();
    }
    assert!(cells(&p, &entity).is_some());
    let missing = ArkivOperation::Delete {
        key: RecordKey::new(&[0; 32]).unwrap(),
    };
    let before = (balance(&p, actor), burnt(&p));
    p.submit(tx(&p, 3, 1_000_000, vec![permanent(2, 1024), missing]))
        .unwrap();
    p.produce().unwrap();
    let b = p.block(p.head.height).unwrap();
    assert!(matches!(
        b.outcomes.txs[0].ops[0],
        OpOutcome::RolledBack { .. }
    ));
    // The rollback undid the rent debit; only the gas remains charged.
    assert_eq!(b.outcomes.txs[0].rent_wei, U256::ZERO);
    let gas_fee = accounts::fee(b.outcomes.txs[0].spent);
    assert_eq!(balance(&p, actor), before.0 - gas_fee);
    assert_eq!(burnt(&p), before.1 + gas_fee);
    assert!(b.outcomes.spent > 50_000);
    assert!(cells(&p, &key(2, 2)).is_none());
    assert_eq!(p.committed_nonce(actor).unwrap(), 4);
}
#[test]
fn a_128_kib_permanent_entity_fits_one_transaction() {
    let mut p = host(true);
    p.produce().unwrap();
    let actor = signer().address();
    let before = balance(&p, actor);
    let op = permanent(1, arkiv::MAX_ENTITY_BYTES);
    p.submit(tx(&p, 1, 2_000_000, vec![op])).unwrap();
    p.produce().unwrap();
    let outcome = p.block(2).unwrap().outcomes.txs[0].clone();
    assert_eq!(outcome.status, TxStatus::Committed);
    // Under a million gas of execution, far inside the 90M user budget ...
    assert!(outcome.spent < 1_000_000, "{}", outcome.spent);
    // ... and about one GLM of rent for the year the cap charges.
    let units = gas::storage_units(&cells(&p, &key(0, 1)).unwrap())
        .unwrap()
        .total;
    assert!(units > 131_072);
    assert_eq!(
        outcome.rent_wei,
        U256::from(units) * U256::from(7_300_000_000_000u64)
    );
    let glm = U256::from(10u64).pow(U256::from(18u64));
    assert!(outcome.rent_wei > glm * U256::from(9u64) / U256::from(10u64));
    assert!(outcome.rent_wei < glm);
    assert_eq!(
        balance(&p, actor),
        before - accounts::fee(outcome.spent) - outcome.rent_wei
    );
}
/// A chain funding the proposer and `tight` with exactly `balance`.
fn tight_host(tight: &Signer, funds: U256) -> Host {
    let base = genesis();
    let g = SignedGenesis::new(
        base.genesis.clone(),
        base.run_id,
        base.proposer,
        base.block_period_ms,
        AccountConfig::new(vec![
            (signer().address(), signed_support::balance()),
            (tight.address(), funds),
        ])
        .unwrap(),
    )
    .unwrap();
    let mut h = host_of(g, true);
    h.produce().unwrap();
    h
}
/// The rent and gas of `txs` when `by` runs them in block 2 of a rich chain:
/// execution is deterministic, so a tight chain charges the same.
fn dry_run(by: &Signer, txs: &[(u64, Vec<ArkivOperation>)]) -> Vec<(U256, u64)> {
    let mut h = tight_host(by, signed_support::balance());
    for (budget, ops) in txs {
        let nonce = h.committed_nonce(by.address()).unwrap() + h.pool.len() as u64;
        h.submit(tx_by(&h, by, nonce, *budget, ops.clone()))
            .unwrap();
    }
    h.produce().unwrap();
    h.block(2)
        .unwrap()
        .outcomes
        .txs
        .iter()
        .map(|t| {
            assert_eq!(t.status, TxStatus::Committed);
            (t.rent_wei, t.spent)
        })
        .collect()
}
#[test]
fn admission_refuses_a_sender_who_cannot_pay_gas_and_rent() {
    let tight = signed_support::signer(7);
    let op = permanent(1, 4096);
    let (rent, _) = dry_run(&tight, &[(1_000_000, vec![op.clone()])])[0];
    assert!(rent > U256::ZERO);
    let need = accounts::fee(1_000_000) + rent;
    // One wei short of gas plus rent: refused at admission, nothing queued.
    let mut h = tight_host(&tight, need - U256::from(1u64));
    assert_eq!(
        h.submit(tx_by(&h, &tight, 0, 1_000_000, vec![op.clone()]))
            .unwrap_err()
            .0,
        "InsufficientBalance"
    );
    assert!(h.pool.is_empty());
    // Enough for the gas reservation alone is not enough either.
    assert!(accounts::fee(1_000_000) < need - U256::from(1u64));
    // Exactly gas plus rent: admitted, committed, and the unused gas refunded.
    let mut h = tight_host(&tight, need);
    h.submit(tx_by(&h, &tight, 0, 1_000_000, vec![op])).unwrap();
    // The pool reserves the quoted rent beside the fee, so a second
    // transaction from the same sender needs funds of its own.
    assert_eq!(h.pool.reserved(tight.address()).unwrap(), need);
    assert_eq!(
        h.submit(tx_by(&h, &tight, 1, 21_000, vec![permanent(2, 0)]))
            .unwrap_err()
            .0,
        "InsufficientBalance"
    );
    h.produce().unwrap();
    let outcome = &h.block(2).unwrap().outcomes.txs[0];
    assert_eq!(outcome.status, TxStatus::Committed);
    assert_eq!(outcome.rent_wei, rent);
    assert_eq!(
        balance(&h, tight.address()),
        accounts::fee(1_000_000 - outcome.spent)
    );
}
#[test]
fn rent_the_sender_cannot_pay_at_execution_fails_its_transaction_atomically() {
    let tight = signed_support::signer(7);
    let entity = key_of(&tight, 0, 1);
    let dry = dry_run(
        &tight,
        &[
            (1_000_000, vec![permanent(1, 1024)]),
            (1_000_000, vec![grow(&entity, 4096)]),
        ],
    );
    let rents = [dry[0].0, dry[1].0];
    // The patch builds on the pending create, so its admission simulation
    // fails and quotes no rent: execution must check it. The sender holds
    // the create's gas and rent and the patch's gas reservation, and one wei
    // less than the patch's rent.
    let funds =
        accounts::fee(dry[0].1) + rents[0] + accounts::fee(1_000_000) + rents[1] - U256::from(1u64);
    let mut h = tight_host(&tight, funds);
    let mut f = host_of(h.genesis.clone(), false);
    f.import(h.block(1).unwrap()).unwrap();
    h.submit(tx_by(&h, &tight, 0, 1_000_000, vec![permanent(1, 1024)]))
        .unwrap();
    h.submit(tx_by(&h, &tight, 1, 1_000_000, vec![grow(&entity, 4096)]))
        .unwrap();
    h.produce().unwrap();
    let b = h.block(2).unwrap();
    assert_eq!(b.outcomes.txs.len(), 2);
    assert_eq!(b.outcomes.txs[0].status, TxStatus::Committed);
    let refused = &b.outcomes.txs[1];
    assert_eq!(refused.status, TxStatus::Failed { op_index: 0 });
    assert_eq!(
        refused.ops[0],
        OpOutcome::HostRejected {
            fault: HostFault::Arkiv(ArkivFault::InsufficientRentBalance)
        }
    );
    assert_eq!(refused.rent_wei, U256::ZERO);
    // Gas is charged as for any failed transaction; the entity kept its size.
    assert!(refused.spent >= gas::TX_INTRINSIC_GAS);
    let row = cells(&h, &entity).unwrap();
    assert!(row.iter().all(|(name, _)| name.as_str() != "more"));
    assert_eq!(
        balance(&h, tight.address()),
        funds - accounts::fee(b.outcomes.txs[0].spent + refused.spent) - rents[0]
    );
    assert_eq!(h.committed_nonce(tight.address()).unwrap(), 2);
    // A follower replays the failure exactly.
    f.import(b).unwrap();
    assert_eq!(f.head, h.head);
}
#[test]
fn the_producer_drops_a_transaction_whose_reservation_rent_consumed() {
    let tight = signed_support::signer(7);
    let entity = key_of(&tight, 0, 1);
    let dry = dry_run(
        &tight,
        &[
            (1_000_000, vec![permanent(1, 1024)]),
            (1_000_000, vec![grow(&entity, 4096)]),
        ],
    );
    let rents = [dry[0].0, dry[1].0];
    // Enough for both rents and two reservations, so the unquoted growth
    // pays; a third transaction's 3M-gas reservation then no longer fits.
    let funds = accounts::fee(2_000_000) + rents[0] + rents[1];
    let mut h = tight_host(&tight, funds);
    let mut f = host_of(h.genesis.clone(), false);
    f.import(h.block(1).unwrap()).unwrap();
    h.submit(tx_by(&h, &tight, 0, 1_000_000, vec![permanent(1, 1024)]))
        .unwrap();
    h.submit(tx_by(&h, &tight, 1, 1_000_000, vec![grow(&entity, 4096)]))
        .unwrap();
    let third = tx_by(
        &h,
        &tight,
        2,
        3_000_000,
        vec![ArkivOperation::ExtendExpiry {
            key: entity.clone(),
            expiry: Expiry::PERMANENT,
        }],
    );
    let third_hash = third.hash();
    h.submit(third).unwrap();
    assert_eq!(h.pool.len(), 3);
    // Including it would invalidate the block: the producer evicts it and
    // builds the block without it.
    h.produce().unwrap();
    let b = h.block(2).unwrap();
    assert_eq!(b.inputs.txs.len(), 2);
    assert!(b.inputs.txs.iter().all(|t| t.hash() != third_hash));
    assert!(
        b.outcomes
            .txs
            .iter()
            .all(|t| t.status == TxStatus::Committed)
    );
    assert_eq!(b.outcomes.txs[1].rent_wei, rents[1]);
    assert!(h.pool.is_empty());
    assert_eq!(h.committed_nonce(tight.address()).unwrap(), 2);
    f.import(b).unwrap();
    assert_eq!(f.head, h.head);
    // The chain goes on.
    h.produce().unwrap();
    assert_eq!(h.head.height, 3);
}
#[test]
fn forged_schedule_and_low_intrinsic_are_rejected() {
    let mut g = genesis();
    g.genesis.pricing = arkiv_db::PriceSchedule::new(gas::SCHEDULE_ID, [1; 31]);
    assert!(g.validate().is_err());
    let g = genesis();
    let tx = TxEnvelope {
        chain_id: 9200,
        actor: signer().address(),
        request_id: [1; 32],
        tx_budget: 20_999,
        ops: vec![TxOp::Arkiv {
            namespace: arkiv::entity_namespace(),
            operation: create(1, 0, 1),
        }],
    };
    assert_eq!(
        SignedTx::sign(&g, 1, tx, &signer()).unwrap_err().0,
        "GasLimitBelowIntrinsic"
    );
}
#[test]
fn lifecycle_growth_extension_shrink_and_delete() {
    let mut p = host(true);
    p.produce().unwrap();
    p.submit(tx(&p, 1, 1_000_000, vec![create(1, 1024, 17280)]))
        .unwrap();
    p.produce().unwrap();
    let entity = key(0, 1);
    let initial = cells(&p, &entity).unwrap();
    let initial_units = gas::storage_units(&initial).unwrap().total;
    p.submit(tx(
        &p,
        2,
        1_000_000,
        vec![ArkivOperation::Patch {
            key: entity.clone(),
            set: vec![(
                CellName::new("body").unwrap(),
                Cell::field(Value::bytes(&vec![42; 2048])),
            )],
            remove: vec![],
        }],
    ))
    .unwrap();
    p.produce().unwrap();
    let grown = gas::storage_units(&cells(&p, &entity).unwrap())
        .unwrap()
        .total;
    assert_eq!(grown - initial_units, 1024);
    let grow_gas = p.block(3).unwrap().outcomes.txs[0].spent;
    let grow_rent = gas::rent(initial_units, grown, Some(17282), Some(17282), 3, clock(&p))
        .unwrap()
        .wei;
    assert!(grow_gas > 21000);
    assert_eq!(
        p.block(3).unwrap().outcomes.txs[0].rent_wei,
        U256::from(grow_rent)
    );
    p.submit(tx(
        &p,
        3,
        1_000_000,
        vec![ArkivOperation::ExtendExpiry {
            key: entity.clone(),
            expiry: Expiry {
                expires_at: 34562,
                min_lifetime: 0,
            },
        }],
    ))
    .unwrap();
    p.produce().unwrap();
    let extend_gas = p.block(4).unwrap().outcomes.txs[0].spent;
    let extension_rent = gas::rent(grown, grown, Some(17282), Some(34562), 4, clock(&p))
        .unwrap()
        .wei;
    assert!(extend_gas > 21000);
    assert_eq!(
        p.block(4).unwrap().outcomes.txs[0].rent_wei,
        U256::from(extension_rent)
    );
    p.submit(tx(
        &p,
        4,
        1_000_000,
        vec![ArkivOperation::Patch {
            key: entity.clone(),
            set: vec![(
                CellName::new("body").unwrap(),
                Cell::field(Value::bytes(&[])),
            )],
            remove: vec![],
        }],
    ))
    .unwrap();
    p.produce().unwrap();
    let shrink_gas = p.block(5).unwrap().outcomes.txs[0].spent;
    assert!(shrink_gas >= 21000);
    // No refunds: a shrink pays no rent and gets none back.
    assert_eq!(p.block(5).unwrap().outcomes.txs[0].rent_wei, U256::ZERO);
    p.submit(tx(
        &p,
        5,
        1_000_000,
        vec![ArkivOperation::Delete {
            key: entity.clone(),
        }],
    ))
    .unwrap();
    p.produce().unwrap();
    assert!(p.block(6).unwrap().outcomes.txs[0].spent >= 21000);
    assert!(cells(&p, &entity).is_none());
}

#[test]
fn signed_genesis_rejects_other_wire_versions() {
    let g = genesis();
    let mut bytes = arkiv_db::encode(&g);
    assert_eq!(&bytes[..4], &1u32.to_be_bytes());
    assert_eq!(arkiv_db::decode_exact::<SignedGenesis>(&bytes).unwrap(), g);
    for version in [0u32, 2, 3, 4] {
        bytes[..4].copy_from_slice(&version.to_be_bytes());
        assert!(arkiv_db::decode_exact::<SignedGenesis>(&bytes).is_err());
    }
}
#[test]
fn durable_reopen_light_proof_and_reserved_expiry_sweep() {
    use arkiv_db::proof::{EqQuery, EqRequest, SnapshotRef, VerifiedEq};
    use arkiv_node::signed::light::Light;
    let state = Arc::new(MemoryVersionedStore::new());
    let history = Arc::new(MemoryVersionedStore::new());
    let light_store = Arc::new(MemoryVersionedStore::new());
    let g = genesis();
    let cert = SignedHeader::sign(&g, g.header(), &signer()).unwrap();
    let mut p = Host::create(
        state.clone(),
        History::create(history.clone(), g.hash()).unwrap(),
        g.clone(),
        [3; 16],
        cert.clone(),
        Some(signer()),
        Options::default(),
    )
    .unwrap();
    let mut light = Light::create(light_store.clone(), g.clone(), cert).unwrap();
    p.produce().unwrap();
    let header = p.certificate(1).unwrap();
    light
        .append(std::slice::from_ref(&header), &header)
        .unwrap();
    p.submit(tx(&p, 1, 1_000_000, vec![create(1, 1024, 1)]))
        .unwrap();
    p.produce().unwrap();
    let header = p.certificate(2).unwrap();
    light
        .append(std::slice::from_ref(&header), &header)
        .unwrap();
    let before = p.head;
    drop(p);
    drop(light);
    let mut p = Host::open(
        state,
        History::open(history, g.hash()).unwrap(),
        &g,
        Some(signer()),
        Options::default(),
    )
    .unwrap();
    let mut light = Light::open(light_store, &g).unwrap();
    assert_eq!(p.head, before);
    assert_eq!(light.head, before);
    let request = EqRequest {
        snapshot: SnapshotRef {
            genesis_hash: g.hash(),
            height: 2,
            block_hash: before.hash(),
            state_root: before.state_root,
        },
        query: EqQuery {
            namespace: arkiv::entity_namespace(),
            attribute: CellName::new("group").unwrap(),
            value: Value::U64(1),
        },
        limit: 16,
        continuation: None,
    };
    let proof = p.proof(&request).unwrap();
    let VerifiedEq::Page(page) = light.verify(&request, None, &proof).unwrap() else {
        panic!("missing namespace")
    };
    assert_eq!(page.rows.len(), 1);
    p.submit(tx(&p, 2, 90_000_000, vec![create(2, 0, 17280)]))
        .unwrap();
    p.produce().unwrap();
    let b = p.block(3).unwrap();
    assert_eq!(b.outcomes.expiry.len(), 1);
    assert_eq!(b.outcomes.txs[0].status, TxStatus::Committed);
    assert_eq!(
        b.outcomes.spent,
        b.outcomes.txs[0].spent + b.outcomes.expiry[0].receipt.spent.units()
    );
    assert!(b.outcomes.spent <= 100_000_000);
    light
        .append(std::slice::from_ref(&b.header), &b.header)
        .unwrap();
    assert!(cells(&p, &key(0, 1)).is_none());
    assert!(
        light.verify(&request, None, &proof).is_ok(),
        "historical proof remains valid after expiry"
    );
}

#[test]
fn named_namespace_entities_preserve_rent_authority_proofs_and_expiry() {
    use arkiv_db::proof::{EqQuery, EqRequest, SnapshotRef, VerifiedEq};
    use arkiv_db::{Cell, NamespaceId, NamespaceName, OwnerId};
    use arkiv_node::{HostFault, arkiv::ArkivFault};
    let mut p = host(true);
    let mut f = host(false);
    p.produce().unwrap();
    f.import(p.block(1).unwrap()).unwrap();
    let ns = NamespaceId::from_bytes(2u64.to_be_bytes()).unwrap();
    let submit = |p: &mut Host, nonce, actor: &Signer, ops| {
        let signed = SignedTx::sign(
            &p.genesis,
            nonce,
            TxEnvelope {
                chain_id: 9200,
                actor: actor.address(),
                request_id: [nonce as u8; 32],
                tx_budget: 1_000_000,
                ops,
            },
            actor,
        )
        .unwrap();
        p.submit(signed).unwrap();
        p.produce().unwrap();
    };
    submit(
        &mut p,
        1,
        &signer(),
        vec![TxOp::CreateNamespace {
            name: NamespaceName::new("bricks").unwrap(),
        }],
    );
    submit(
        &mut p,
        2,
        &signer(),
        vec![TxOp::Arkiv {
            namespace: ns,
            operation: create(22, 1024, 8),
        }],
    );
    assert_eq!(
        p.block(3).unwrap().outcomes.txs[0].status,
        TxStatus::Committed
    );
    let entity = key(0, 22);
    assert!(
        cells(&p, &entity).is_none(),
        "default namespace stays isolated"
    );
    let row = p
        .engine
        .head_snapshot()
        .try_get_record(ns, &entity, &Projection::ALL, Budget::unlimited())
        .unwrap()
        .unwrap()
        .value
        .unwrap();
    let projected = arkiv::project(&row).unwrap();
    assert_eq!(projected.owner, *signer().address().as_bytes());
    let rent = gas::rent(
        0,
        gas::storage_units(&row.cells).unwrap().total,
        None,
        Some(11),
        3,
        clock(&p),
    )
    .unwrap()
    .wei;
    assert!(p.block(3).unwrap().outcomes.txs[0].spent > gas::TX_INTRINSIC_GAS);
    assert_eq!(
        p.block(3).unwrap().outcomes.txs[0].rent_wei,
        U256::from(rent)
    );
    let request = EqRequest {
        snapshot: SnapshotRef {
            genesis_hash: p.genesis.hash(),
            height: 3,
            block_hash: p.head.hash(),
            state_root: p.head.state_root,
        },
        query: EqQuery {
            namespace: ns,
            attribute: CellName::new("group").unwrap(),
            value: Value::U64(1),
        },
        limit: 16,
        continuation: None,
    };
    let proof = p.proof(&request).unwrap();
    let VerifiedEq::Page(page) = arkiv_db::proof::verify_eq(
        &request,
        None,
        &proof,
        Default::default(),
        p.genesis.genesis.limits,
    )
    .unwrap() else {
        panic!("missing namespace")
    };
    assert_eq!(page.rows.len(), 1);
    arkiv::validate_proven_row(&page.rows[0]).unwrap();
    let other = Signer::from_bytes([2; 32]).unwrap();
    submit(
        &mut p,
        0,
        &other,
        vec![TxOp::Arkiv {
            namespace: ns,
            operation: ArkivOperation::Delete {
                key: entity.clone(),
            },
        }],
    );
    assert_eq!(
        p.block(4).unwrap().outcomes.txs[0].ops[0],
        OpOutcome::HostRejected {
            fault: HostFault::Arkiv(ArkivFault::NotOwner)
        }
    );
    for op in [
        TxOp::PatchRecord {
            namespace: ns,
            key: entity.clone(),
            set: vec![(
                CellName::new("group").unwrap(),
                Cell::attribute(Value::U64(9)),
            )],
            remove: vec![],
            expiry: None,
        },
        TxOp::TransferNamespace {
            namespace: ns,
            expected_revision: arkiv_db::Revision(0),
            new_owner: OwnerId::new([9; 20]),
        },
    ] {
        let signed = SignedTx::sign(
            &p.genesis,
            3,
            TxEnvelope {
                chain_id: 9200,
                actor: signer().address(),
                request_id: [3; 32],
                tx_budget: 1_000_000,
                ops: vec![op],
            },
            &signer(),
        )
        .unwrap_err();
        assert_eq!(signed.0, "UnsupportedGasOperation");
    }
    let unknown = NamespaceId::from_bytes(999u64.to_be_bytes()).unwrap();
    submit(
        &mut p,
        3,
        &signer(),
        vec![TxOp::Arkiv {
            namespace: unknown,
            operation: create(99, 0, 1),
        }],
    );
    assert!(matches!(
        p.block(5).unwrap().outcomes.txs[0].status,
        TxStatus::Failed { .. }
    ));
    while p.head.height < 11 {
        p.produce().unwrap();
    }
    assert_eq!(p.block(11).unwrap().outcomes.expiry.len(), 1);
    assert!(
        p.engine
            .head_snapshot()
            .try_get_record(ns, &entity, &Projection::ALL, Budget::unlimited())
            .unwrap()
            .unwrap()
            .value
            .is_none()
    );
    for h in 2..=11 {
        f.import(p.block(h).unwrap()).unwrap();
    }
    assert_eq!(p.head, f.head);
}
