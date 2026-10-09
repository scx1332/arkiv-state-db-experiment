use alloy_primitives::U256;
use arkiv_db::{
    Budget, CellName, OwnerId, Projection, SnapshotId, Value, decode_exact, durable::Options,
    encode,
};
use arkiv_db_memory::MemoryVersionedStore;
use arkiv_node::{
    Expiry, OpOutcome, TxEnvelope, TxOp, TxStatus,
    accounts::{self, AccountConfig, AccountFault},
    arkiv::{self, ArkivOperation},
    gas,
    signed::{
        crypto::Signer,
        history::History,
        host::Host,
        light::{Light, verify_account},
        protocol::*,
    },
};
use std::sync::Arc;
fn signer(n: u8) -> Signer {
    Signer::from_bytes([n; 32]).unwrap()
}
fn glm(n: u64) -> U256 {
    U256::from(n) * U256::from(1_000_000_000_000_000_000u64)
}
fn genesis() -> SignedGenesis {
    SignedGenesis::new(
        default_native_genesis(9300, 1000),
        [7; 16],
        *signer(1).address().as_bytes(),
        1000,
        AccountConfig::new(vec![
            (signer(1).address(), glm(10)),
            (signer(2).address(), glm(2)),
        ])
        .unwrap(),
    )
    .unwrap()
}
fn create_host(
    g: &SignedGenesis,
    p: bool,
    state: Arc<MemoryVersionedStore>,
    history: Arc<MemoryVersionedStore>,
) -> Host {
    let cert = SignedHeader::sign(g, g.header(), &signer(1)).unwrap();
    Host::create(
        state,
        History::create(history, g.hash()).unwrap(),
        g.clone(),
        [3; 16],
        cert,
        p.then(|| signer(1)),
        Options::default(),
    )
    .unwrap()
}
fn host(g: &SignedGenesis, p: bool) -> Host {
    create_host(
        g,
        p,
        Arc::new(MemoryVersionedStore::new()),
        Arc::new(MemoryVersionedStore::new()),
    )
}
fn tx(g: &SignedGenesis, who: u8, nonce: u64, budget: u64, ops: Vec<TxOp>) -> SignedTx {
    let mut request = [who; 32];
    request[24..].copy_from_slice(&nonce.to_be_bytes());
    SignedTx::sign(
        g,
        nonce,
        TxEnvelope {
            chain_id: g.genesis.chain_id,
            actor: signer(who).address(),
            request_id: request,
            tx_budget: budget,
            ops,
        },
        &signer(who),
    )
    .unwrap()
}
fn transfer(who: u8, amount: U256) -> TxOp {
    TxOp::Transfer {
        to: signer(who).address(),
        amount,
    }
}
fn create(salt: u128) -> TxOp {
    TxOp::Arkiv {
        namespace: arkiv::entity_namespace(),
        operation: ArkivOperation::Create {
            salt,
            flags: 0,
            expiry: Expiry {
                expires_at: 0,
                min_lifetime: 17280,
            },
            attributes: vec![
                (CellName::new("group").unwrap(), Value::U64(1)),
                (
                    CellName::new("$payload").unwrap(),
                    Value::bytes(b"fee-test"),
                ),
            ],
        },
    }
}
fn balance(h: &Host, n: u8) -> U256 {
    h.account(h.head.height, signer(n).address())
        .unwrap()
        .balance
}
fn proofs(h: &Host, l: &Light, n: u8, height: u64) -> (accounts::Account, U256, U256) {
    let snapshot = h
        .engine
        .snapshot(SnapshotId::from_sequence(height))
        .unwrap();
    verify_account(
        &h.genesis,
        &l.header(height).unwrap().header,
        signer(n).address(),
        &snapshot
            .prove_host(&accounts::account_key(signer(n).address()))
            .unwrap(),
        &snapshot.prove_host(&accounts::BURN_KEY).unwrap(),
    )
    .unwrap()
}
fn relay(p: &Host, f: &mut Host, l: &mut Light) {
    let block = p.block(p.head.height).unwrap();
    f.import(block.clone()).unwrap();
    l.append(std::slice::from_ref(&block.header), &block.header)
        .unwrap();
    assert_eq!(p.head, f.head);
    assert_eq!(p.head, l.head);
}
#[test]
fn funded_lifecycle_fee_burn_replay_proofs_and_restart() {
    let g = genesis();
    let state = Arc::new(MemoryVersionedStore::new());
    let history = Arc::new(MemoryVersionedStore::new());
    let ls = Arc::new(MemoryVersionedStore::new());
    let mut p = create_host(&g, true, state.clone(), history.clone());
    let mut f = host(&g, false);
    let mut l = Light::create(ls.clone(), g.clone(), p.certificate(0).unwrap()).unwrap();
    assert_eq!(proofs(&p, &l, 2, 0).0.balance, glm(2));
    assert_eq!(proofs(&p, &l, 3, 0).0.balance, U256::ZERO);
    p.produce().unwrap();
    relay(&p, &mut f, &mut l);
    let bootstrap_fee = accounts::fee(p.block(1).unwrap().outcomes.spent);
    assert_eq!(balance(&p, 1), glm(10) - bootstrap_fee);
    assert_eq!(p.committed_nonce(signer(1).address()).unwrap(), 1);
    let send = tx(&g, 2, 0, 100_000, vec![transfer(3, glm(1))]);
    p.submit(send.clone()).unwrap();
    p.produce().unwrap();
    relay(&p, &mut f, &mut l);
    assert_eq!(p.block(2).unwrap().outcomes.txs[0].spent, 21_000);
    assert_eq!(balance(&p, 2), glm(1) - accounts::fee(21_000));
    assert_eq!(balance(&p, 3), glm(1));
    let status = p
        .request_status(signer(2).address(), send.tx.request_id)
        .unwrap();
    assert_eq!(status["feeChargedWei"], accounts::fee(21_000).to_string());
    assert_eq!(status["gasPriceWei"], "1000000000");
    assert_eq!(p.submit(send.clone()).unwrap(), status);
    let create = tx(&g, 3, 0, 1_000_000, vec![create(3)]);
    p.submit(create).unwrap();
    p.produce().unwrap();
    relay(&p, &mut f, &mut l);
    let create_gas = p.block(3).unwrap().outcomes.txs[0].spent;
    assert!(create_gas > 21_000 && create_gas < 1_000_000);
    let (account, burnt, supply) = proofs(&p, &l, 3, 3);
    assert_eq!(account.balance, glm(1) - accounts::fee(create_gas));
    assert_eq!(burnt, bootstrap_fee + accounts::fee(21_000 + create_gas));
    assert_eq!(supply, glm(12));
    assert_eq!(
        balance(&p, 1) + balance(&p, 2) + balance(&p, 3),
        supply - burnt
    );
    // Verify a historical nonmembership default after this account has changed.
    assert_eq!(proofs(&p, &l, 3, 1).0.balance, U256::ZERO);
    let root = p.head;
    drop(p);
    drop(l);
    let mut p = Host::open(
        state,
        History::open(history, g.hash()).unwrap(),
        &g,
        Some(signer(1)),
        Options::default(),
    )
    .unwrap();
    let l = Light::open(ls, &g).unwrap();
    assert_eq!(p.head, root);
    assert_eq!(p.submit(send).unwrap(), status);
    assert_eq!(proofs(&p, &l, 3, 3).0, account);
}
#[test]
fn insufficient_funds_pending_reservation_and_exact_zero_balance() {
    let mut g = genesis();
    let mut allocations = g.accounts.allocations.clone();
    allocations.push((signer(4).address(), accounts::fee(21_000)));
    g.accounts = AccountConfig::new(allocations).unwrap();
    let mut p = host(&g, true);
    p.produce().unwrap();
    let head = p.head;
    assert_eq!(
        p.submit(tx(&g, 3, 0, 21_000, vec![transfer(2, U256::ZERO)]))
            .unwrap_err()
            .0,
        "InsufficientBalance"
    );
    assert_eq!(p.head, head);
    assert_eq!(p.committed_nonce(signer(3).address()).unwrap(), 0);
    let one = tx(&g, 2, 0, 21_000, vec![transfer(3, glm(1))]);
    p.submit(one.clone()).unwrap();
    assert_eq!(p.submit(one).unwrap()["status"], "pending");
    assert_eq!(
        p.submit(tx(&g, 2, 1, 21_000, vec![transfer(3, glm(1))]))
            .unwrap_err()
            .0,
        "InsufficientBalance"
    );
    // Gap transactions also reserve fees and outgoing value.
    p.submit(tx(
        &g,
        2,
        2,
        21_000,
        vec![transfer(3, glm(1) - accounts::fee(42_000))],
    ))
    .unwrap();
    assert_eq!(p.pool.reserved(signer(2).address()).unwrap(), glm(2));
    p.produce().unwrap();
    assert_eq!(p.pool.len(), 1);
    assert_eq!(balance(&p, 2), glm(1) - accounts::fee(21_000));
    p.submit(tx(&g, 4, 0, 21_000, vec![transfer(4, U256::ZERO)]))
        .unwrap();
    p.produce().unwrap();
    assert_eq!(balance(&p, 4), U256::ZERO);
    assert_eq!(p.committed_nonce(signer(4).address()).unwrap(), 1);
    let snap = p.engine.head_snapshot();
    let bytes = snap
        .prove_host(&accounts::account_key(signer(4).address()))
        .unwrap();
    let proved = arkiv_db::proof::verify_host(
        p.head.state_root,
        &accounts::account_key(signer(4).address()),
        &bytes,
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        decode_exact::<accounts::Account>(&proved).unwrap().balance,
        U256::ZERO
    );
}
#[test]
fn failed_operations_rollback_value_and_entities_but_pay_and_consume_nonce() {
    let g = genesis();
    let mut p = host(&g, true);
    let mut f = host(&g, false);
    let mut l = Light::create(
        Arc::new(MemoryVersionedStore::new()),
        g.clone(),
        p.certificate(0).unwrap(),
    )
    .unwrap();
    p.produce().unwrap();
    relay(&p, &mut f, &mut l);
    let before = balance(&p, 2);
    let missing = TxOp::Arkiv {
        namespace: arkiv::entity_namespace(),
        operation: ArkivOperation::Delete {
            key: arkiv_db::RecordKey::new(&[7; 32]).unwrap(),
        },
    };
    p.submit(tx(
        &g,
        2,
        0,
        1_000_000,
        vec![transfer(3, glm(1)), create(8), missing],
    ))
    .unwrap();
    p.produce().unwrap();
    relay(&p, &mut f, &mut l);
    let b = p.block(2).unwrap();
    assert!(matches!(
        b.outcomes.txs[0].status,
        TxStatus::Failed { op_index: 2 }
    ));
    assert!(matches!(
        b.outcomes.txs[0].ops[0],
        OpOutcome::RolledBack { .. }
    ));
    assert_eq!(balance(&p, 3), U256::ZERO);
    assert_eq!(
        balance(&p, 2),
        before - accounts::fee(b.outcomes.txs[0].spent)
    );
    assert_eq!(p.committed_nonce(signer(2).address()).unwrap(), 1);
    let key = arkiv_db::RecordKey::new(&arkiv::derive_entity_key(
        g.genesis.chain_id,
        signer(2).address().as_bytes(),
        0,
        8,
    ))
    .unwrap();
    assert!(
        p.engine
            .head_snapshot()
            .try_get_record(
                arkiv::entity_namespace(),
                &key,
                &Projection::ALL,
                Budget::unlimited()
            )
            .unwrap()
            .unwrap()
            .value
            .is_none()
    );
    // Intrinsic is charged when an operation does not fit; unspent reservation returns.
    let before = balance(&p, 2);
    p.submit(tx(
        &g,
        2,
        1,
        29_999,
        vec![transfer(3, glm(1)), transfer(3, U256::ZERO)],
    ))
    .unwrap();
    p.produce().unwrap();
    relay(&p, &mut f, &mut l);
    assert_eq!(p.block(3).unwrap().outcomes.txs[0].spent, 21_000);
    assert_eq!(balance(&p, 2), before - accounts::fee(21_000));
    assert_eq!(balance(&p, 3), U256::ZERO);
    // Self transfer is a no-op on value, including under the largest safe value.
    let before = balance(&p, 2);
    p.submit(tx(
        &g,
        2,
        2,
        21_000,
        vec![transfer(2, before - accounts::fee(21_000))],
    ))
    .unwrap();
    p.produce().unwrap();
    relay(&p, &mut f, &mut l);
    assert_eq!(balance(&p, 2), before - accounts::fee(21_000));
}
#[test]
fn genesis_and_signature_commit_accounts() {
    let g = genesis();
    let bytes = encode(&g);
    assert_eq!(&bytes[..4], &VERSION.to_be_bytes());
    assert_eq!(decode_exact::<SignedGenesis>(&bytes).unwrap(), g);
    let signed = tx(&g, 2, 0, 21_000, vec![transfer(3, U256::from(1))]);
    let mut changed = g.clone();
    changed.accounts.allocations[0].1 += U256::from(1);
    assert_ne!(changed.hash(), g.hash());
    assert_eq!(signed.validate(&changed).unwrap_err().0, "IdentityMismatch");
    let mut signed = signed;
    signed.tx.ops = vec![transfer(3, U256::from(2))];
    assert_eq!(signed.validate(&g).unwrap_err().0, "InvalidSignature");
    assert!(
        AccountConfig::new(vec![
            (signer(2).address(), U256::MAX),
            (signer(3).address(), U256::from(1))
        ])
        .is_err()
    );
    assert!(
        AccountConfig::new(vec![
            (signer(2).address(), glm(1)),
            (signer(2).address(), glm(1))
        ])
        .is_err()
    );
    let mut bad = g.clone();
    bad.accounts.gas_price_wei = 0;
    assert!(bad.validate().is_err());
    let mut bad = g;
    bad.accounts = AccountConfig::new(vec![(signer(2).address(), glm(1))]).unwrap();
    assert_eq!(bad.validate().unwrap_err().0, "UnfundedBootstrap");
}
#[test]
fn proofs_reject_wrong_key_root_truncation_and_tampering() {
    let g = genesis();
    let mut p = host(&g, true);
    p.produce().unwrap();
    let snap = p.engine.head_snapshot();
    let key = accounts::account_key(signer(1).address());
    let mut bytes = snap.prove_host(&key).unwrap();
    let root = p.head.state_root;
    assert!(
        arkiv_db::proof::verify_host(root, &key, &bytes)
            .unwrap()
            .is_some()
    );
    assert!(arkiv_db::proof::verify_host(g.header().state_root, &key, &bytes).is_err());
    assert!(arkiv_db::proof::verify_host(root, &key, &bytes[..bytes.len() - 1]).is_err());
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    assert!(arkiv_db::proof::verify_host(root, &key, &bytes).is_err());
    let mut p2 = host(&g, true);
    p2.produce().unwrap();
    let tx = tx(&g, 2, 0, 21_000, vec![transfer(3, glm(1))]);
    p2.submit(tx).unwrap();
    p2.produce().unwrap();
    let mut block = p2.block(2).unwrap();
    block.outcomes.txs[0].spent -= 1;
    block.outcomes.spent -= 1;
    block.header.header.outcomes_digest = block.outcomes.digest();
    block.header = SignedHeader::sign(&g, block.header.header, &signer(1)).unwrap();
    let mut f = host(&g, false);
    f.import(p2.block(1).unwrap()).unwrap();
    assert!(f.import(block).is_err());
    assert_eq!(f.head.height, 1);
}
#[test]
fn overflow_and_insufficient_value_are_atomic_with_charged_account_receipt() {
    // Deliberately construct impossible supply state in a standalone engine to
    // exercise the receiver overflow guard; valid genesis forbids this sum.
    let config = AccountConfig::new(vec![(OwnerId::new([1; 20]), U256::from(5))]).unwrap();
    let engine = arkiv_db::Engine::new(gas::schedule());
    let mut branch = engine.begin(engine.head(), None).unwrap();
    let a = OwnerId::new([1; 20]);
    let b = OwnerId::new([2; 20]);
    accounts::put(
        &mut branch,
        b,
        accounts::Account {
            nonce: 0,
            balance: U256::MAX,
        },
    )
    .unwrap();
    let before = config.read(&branch, a).unwrap();
    let outcome = config
        .transfer(&mut branch, a, b, U256::from(1), 21_000, false)
        .unwrap();
    assert!(matches!(
        outcome,
        OpOutcome::AccountRejected {
            fault: AccountFault::Overflow,
            ..
        }
    ));
    assert_eq!(outcome.spent(), 9_000);
    assert_eq!(config.read(&branch, a).unwrap(), before);
    assert_eq!(config.read(&branch, b).unwrap().balance, U256::MAX);
    let outcome = config
        .transfer(&mut branch, a, b, U256::from(6), 21_000, false)
        .unwrap();
    assert!(matches!(
        outcome,
        OpOutcome::AccountRejected {
            fault: AccountFault::InsufficientBalance,
            ..
        }
    ));
    assert_eq!(config.read(&branch, a).unwrap(), before);
}
#[test]
fn full_u256_balance_can_be_spent_without_overflow() {
    let mut g = genesis();
    g.accounts = AccountConfig::new(vec![(signer(1).address(), U256::MAX)]).unwrap();
    let mut p = host(&g, true);
    p.produce().unwrap();
    let before = balance(&p, 1);
    let sent = before - accounts::fee(21_000);
    p.submit(tx(&g, 1, 1, 21_000, vec![transfer(3, sent)]))
        .unwrap();
    p.produce().unwrap();
    assert_eq!(balance(&p, 1), U256::ZERO);
    assert_eq!(balance(&p, 3), sent);
    let response = p.account_response(2, signer(3).address(), false).unwrap();
    assert_eq!(response["initialSupplyWei"], U256::MAX.to_string());
    assert_eq!(response["totalSupplyWei"], sent.to_string());
}
#[test]
fn publication_retry_and_lost_ack_charge_fee_once() {
    use arkiv_db_storage::{
        testing::{CommitFault, FaultyStore},
        versioned::{StorageErrorKind, VersionedStore},
    };
    for applied in [false, true] {
        let g = genesis();
        let state = Arc::new(FaultyStore::latching(MemoryVersionedStore::new()));
        let hs = Arc::new(MemoryVersionedStore::new());
        let cert = SignedHeader::sign(&g, g.header(), &signer(1)).unwrap();
        let mut p = Host::create(
            state.clone(),
            History::create(hs.clone(), g.hash()).unwrap(),
            g.clone(),
            [3; 16],
            cert,
            Some(signer(1)),
            Options::default(),
        )
        .unwrap();
        p.produce().unwrap();
        let signed = tx(&g, 2, 0, 100_000, vec![transfer(3, glm(1))]);
        p.submit(signed.clone()).unwrap();
        state.push_commit_fault(if applied {
            CommitFault::AckLost
        } else {
            CommitFault::IndeterminateNotApplied(StorageErrorKind::Io)
        });
        assert!(p.produce().is_err());
        assert_eq!(p.head.height, 1);
        assert_eq!(p.pool.len(), 1);
        if applied {
            assert!(p.retry_pending().is_err());
            drop(p);
            state.recover().unwrap();
            p = Host::open(
                state,
                History::open(hs, g.hash()).unwrap(),
                &g,
                Some(signer(1)),
                Options::default(),
            )
            .unwrap();
        } else {
            p.retry_pending().unwrap();
        }
        assert_eq!(p.head.height, 2);
        assert_eq!(balance(&p, 2), glm(1) - accounts::fee(21_000));
        assert_eq!(balance(&p, 3), glm(1));
        assert_eq!(p.committed_nonce(signer(2).address()).unwrap(), 1);
        assert_eq!(
            p.submit(signed).unwrap()["feeChargedWei"],
            accounts::fee(21_000).to_string()
        );
    }
}

#[test]
fn batch_transfers_retain_per_operation_gas_and_rollback() {
    let g = genesis();
    let mut p = host(&g, true);
    p.produce().unwrap();
    let before = balance(&p, 2);
    p.submit(tx(
        &g,
        2,
        0,
        39_000,
        vec![transfer(3, U256::from(1)), transfer(3, U256::from(2))],
    ))
    .unwrap();
    p.produce().unwrap();
    assert_eq!(p.block(2).unwrap().outcomes.txs[0].spent, 39_000);
    assert_eq!(balance(&p, 3), U256::from(3));
    assert_eq!(
        balance(&p, 2),
        before - U256::from(3) - accounts::fee(39_000)
    );
    let before = balance(&p, 2);
    p.submit(tx(
        &g,
        2,
        1,
        38_999,
        vec![transfer(3, U256::from(1)), transfer(3, U256::from(2))],
    ))
    .unwrap();
    p.produce().unwrap();
    assert!(matches!(
        p.block(3).unwrap().outcomes.txs[0].status,
        TxStatus::Failed { op_index: 1 }
    ));
    assert_eq!(p.block(3).unwrap().outcomes.txs[0].spent, 30_000);
    assert_eq!(balance(&p, 3), U256::from(3));
    assert_eq!(balance(&p, 2), before - accounts::fee(30_000));
}

#[test]
fn named_namespace_charges_fees_replays_and_rolls_back_on_low_budget() {
    let g = genesis();
    let mut p = host(&g, true);
    let mut f = host(&g, false);
    let mut l = Light::create(
        Arc::new(MemoryVersionedStore::new()),
        g.clone(),
        p.certificate(0).unwrap(),
    )
    .unwrap();
    p.produce().unwrap();
    relay(&p, &mut f, &mut l);
    let create_namespace = || TxOp::CreateNamespace {
        name: arkiv_db::NamespaceName::new("bricks").unwrap(),
    };
    // Only the intrinsic fee fits. The failed namespace allocation must not
    // consume ID 2, while the account nonce and charged fee remain committed.
    p.submit(tx(
        &g,
        2,
        0,
        gas::TX_INTRINSIC_GAS,
        vec![create_namespace()],
    ))
    .unwrap();
    p.produce().unwrap();
    relay(&p, &mut f, &mut l);
    assert_eq!(
        p.block(2).unwrap().outcomes.txs[0].status,
        TxStatus::Failed { op_index: 0 }
    );
    assert_eq!(p.engine.head_snapshot().try_namespace_count().unwrap(), 1);
    assert_eq!(p.committed_nonce(signer(2).address()).unwrap(), 1);
    let before = balance(&p, 2);
    p.submit(tx(&g, 2, 1, 1_000_000, vec![create_namespace()]))
        .unwrap();
    p.produce().unwrap();
    relay(&p, &mut f, &mut l);
    let block = p.block(3).unwrap();
    let outcome = &block.outcomes.txs[0];
    assert_eq!(outcome.status, TxStatus::Committed);
    assert!(outcome.spent > gas::TX_INTRINSIC_GAS);
    assert_eq!(balance(&p, 2), before - accounts::fee(outcome.spent));
    assert_eq!(proofs(&p, &l, 2, 3).0.balance, balance(&p, 2));
    let ns = arkiv_db::NamespaceId::from_ordinal(2).unwrap();
    let descriptor = p
        .engine
        .head_snapshot()
        .try_descriptor(ns, Budget::unlimited())
        .unwrap()
        .unwrap()
        .value;
    assert_eq!(descriptor.name.as_str(), "bricks");
    assert_eq!(descriptor.owner, signer(2).address());
    assert_eq!(
        p.engine.head_snapshot().try_record_count(ns).unwrap(),
        Some(0)
    );
    // This does not admit generic data writes without Arkiv storage rent.
    let mut envelope = tx(&g, 2, 2, 1_000_000, vec![create_namespace()]).tx;
    envelope.ops = vec![TxOp::Put {
        namespace: ns,
        key: b"brick".to_vec(),
        value: b"red".to_vec(),
    }];
    assert_eq!(
        SignedTx::sign(&g, 2, envelope, &signer(2)).unwrap_err().0,
        "UnsupportedGasOperation"
    );
}
