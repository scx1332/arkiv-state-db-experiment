use arkiv_db::{Budget, Cell, CellName, Projection, RecordKey, Value, durable::Options};
use arkiv_db_memory::MemoryVersionedStore;
use arkiv_node::{
    Expiry, OpOutcome, TxEnvelope, TxOp, TxStatus,
    arkiv::{self, ArkivOperation},
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
    let g = genesis();
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
    SignedTx::sign(
        &h.genesis,
        nonce,
        TxEnvelope {
            chain_id: 9200,
            actor: signer().address(),
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
        &signer(),
    )
    .unwrap()
}
fn key(nonce: u64, salt: u128) -> RecordKey {
    RecordKey::new(&arkiv::derive_entity_key(
        9200,
        signer().address().as_bytes(),
        nonce,
        salt,
    ))
    .unwrap()
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
fn permanent_rejected_batch_rollback_retains_gas_and_nonce() {
    let mut p = host(true);
    p.produce().unwrap();
    let mut permanent = create(1, 1024, 0);
    if let ArkivOperation::Create { expiry, .. } = &mut permanent {
        *expiry = Expiry::PERMANENT;
    }
    p.submit(tx(&p, 1, 1_000_000, vec![permanent])).unwrap();
    p.produce().unwrap();
    assert!(matches!(
        p.block(2).unwrap().outcomes.txs[0].ops[0],
        OpOutcome::HostRejected { .. }
    ));
    assert_eq!(p.block(2).unwrap().outcomes.txs[0].spent, 21000);
    let missing = ArkivOperation::Delete {
        key: RecordKey::new(&[0; 32]).unwrap(),
    };
    p.submit(tx(&p, 2, 1_000_000, vec![create(2, 1024, 17280), missing]))
        .unwrap();
    p.produce().unwrap();
    let b = p.block(3).unwrap();
    assert!(matches!(
        b.outcomes.txs[0].ops[0],
        OpOutcome::RolledBack { .. }
    ));
    assert!(b.outcomes.spent > 50_000);
    assert!(cells(&p, &key(0, 2)).is_none());
    assert_eq!(p.committed_nonce(signer().address()).unwrap(), 3);
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
    let grow_rent = gas::rent(initial_units, grown, Some(17282), Some(17282), 3)
        .unwrap()
        .gas;
    assert!(grow_gas > 21000 + grow_rent);
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
    let extension_rent = gas::rent(grown, grown, Some(17282), Some(34562), 4)
        .unwrap()
        .gas;
    assert!(extend_gas > 21000 + extension_rent);
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
    assert!(shrink_gas < grow_gas);
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
    )
    .unwrap()
    .gas;
    assert!(p.block(3).unwrap().outcomes.txs[0].spent > gas::TX_INTRINSIC_GAS + rent);
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
