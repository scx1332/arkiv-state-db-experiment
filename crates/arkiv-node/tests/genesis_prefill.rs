use alloy_primitives::U256;
use arkiv_db::{
    Budget, CellName, Projection, RecordKey, SnapshotId, Value, decode_exact, durable::Options,
    encode,
};
use arkiv_db_memory::MemoryVersionedStore;
use arkiv_node::{
    Expiry, TxEnvelope, TxOp,
    accounts::{self, AccountConfig},
    arkiv::{self, ArkivOperation},
    genesis_bundle::*,
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
/// The unprefilled base, validated by `build_genesis` once the prefill is
/// attached. The proposer intentionally has no allocation: a prefill chain
/// has no bootstrap transaction to pay for.
fn base() -> SignedGenesis {
    SignedGenesis {
        genesis: default_native_genesis(9450, 1000),
        run_id: [7; 16],
        proposer: *signer(1).address().as_bytes(),
        block_period_ms: 1000,
        limits: SignedLimits::default(),
        accounts: AccountConfig::new(vec![(
            signer(2).address(),
            U256::from(1_000_000_000_000_000_000u64),
        )])
        .unwrap(),
        prefill: None,
    }
}
fn limits() -> PrefillLimits {
    PrefillLimits {
        max_entities: 256,
        max_canonical_bytes: MAX_CANONICAL_BYTES,
        max_storage_units: MAX_STORAGE_UNITS,
    }
}
fn row(n: u64, height: u64) -> PrefillEntity {
    PrefillEntity {
        creator: *signer(2).address().as_bytes(),
        create_nonce: n,
        salt: 10 + n as u128,
        expires_at_height: height,
        flags: 0,
        content_type: "text/plain".into(),
        payload: b"hello".to_vec(),
        attributes: vec![(CellName::new("label").unwrap(), Value::str("hello"))],
    }
}
fn build(rows: &[PrefillEntity]) -> SignedGenesis {
    build_genesis(base(), limits(), *signer(2).address().as_bytes(), rows).unwrap()
}
fn host(
    g: &SignedGenesis,
    stream: &[u8],
    producer: bool,
    state: Arc<MemoryVersionedStore>,
    history: Arc<MemoryVersionedStore>,
) -> Host {
    Host::create_with_prefill(
        state,
        History::create(history, g.hash()).unwrap(),
        g.clone(),
        [3; 16],
        SignedHeader::sign(g, g.header(), &signer(1)).unwrap(),
        producer.then(|| signer(1)),
        Options::default(),
        Some(stream.to_vec()),
    )
    .unwrap()
}
fn account_proof(h: &Host, l: &Light, who: u8, height: u64) -> (accounts::Account, U256, U256) {
    let snap = h
        .engine
        .snapshot(SnapshotId::from_sequence(height))
        .unwrap();
    verify_account(
        &h.genesis,
        &l.header(height).unwrap().header,
        signer(who).address(),
        &snap
            .prove_host(&accounts::account_key(signer(who).address()))
            .unwrap(),
        &snap.prove_host(&accounts::BURN_KEY).unwrap(),
    )
    .unwrap()
}
#[test]
fn canonical_stream_and_launch_are_order_independent_and_strict() {
    let mut rows = vec![row(0, 4), row(1, 5)];
    rows[0]
        .attributes
        .push((CellName::new("alpha").unwrap(), Value::U64(42)));
    let stream = encode_stream(&rows).unwrap();
    let g = build(&rows);
    rows.reverse();
    rows[1].attributes.reverse();
    assert_eq!(encode_stream(&rows).unwrap(), stream);
    assert_eq!(build(&rows), g);
    assert_eq!(&encode(&g)[..4], &VERSION.to_be_bytes());
    assert_eq!(decode_exact::<SignedGenesis>(&encode(&g)).unwrap(), g);
    for cut in [0, 8, 12, 19, 20, stream.len() - 1] {
        assert!(decode_stream(&stream[..cut]).is_err());
    }
    let mut bad = stream.clone();
    bad.push(0);
    assert!(decode_stream(&bad).is_err());
    let mut bad = stream.clone();
    *bad.last_mut().unwrap() ^= 1;
    assert!(verify_genesis(&g, &bad).is_err());
    let mut badg = g.clone();
    badg.prefill.as_mut().unwrap().storage_units += 1;
    assert!(verify_genesis(&badg, &stream).is_err());
    let mut badg = g.clone();
    badg.prefill.as_mut().unwrap().burned_wei += U256::from(1);
    assert!(verify_genesis(&badg, &stream).is_err());
    let mut bytes = encode(&g);
    bytes[..4].copy_from_slice(&2u32.to_be_bytes());
    assert!(decode_exact::<SignedGenesis>(&bytes).is_err());
    let mut changed = base();
    changed.run_id[0] ^= 1;
    let changed = build_genesis(changed, limits(), *signer(2).address().as_bytes(), &rows).unwrap();
    assert_eq!(
        changed.prefill.as_ref().unwrap().state,
        g.prefill.as_ref().unwrap().state
    );
    assert_ne!(changed.hash(), g.hash());
}
#[test]
fn invalid_nonces_system_cells_capacity_and_sponsor_fail() {
    assert!(encode_stream(&[row(1, 4)]).is_err());
    assert!(encode_stream(&[row(0, 4), row(0, 5)]).is_err());
    for height in [0, u64::MAX, 6_307_201] {
        assert!(
            build_genesis(
                base(),
                limits(),
                *signer(2).address().as_bytes(),
                &[row(0, height)]
            )
            .is_err()
        );
    }
    let mut system = row(0, 4);
    system
        .attributes
        .push((CellName::new("$owner").unwrap(), Value::Addr([1; 20])));
    assert!(encode_stream(&[system]).is_err());
    let rows = (0..16).map(|n| row(n, 1)).collect::<Vec<_>>();
    assert!(build_genesis(base(), limits(), *signer(2).address().as_bytes(), &rows).is_ok());
    let mut rows = rows;
    rows.push(row(16, 1));
    assert!(build_genesis(base(), limits(), *signer(2).address().as_bytes(), &rows).is_err());
    let rows = [row(0, 4)];
    let g = build(&rows);
    let debit = g.prefill.as_ref().unwrap().burned_wei;
    let mut exact = base();
    exact.accounts = AccountConfig::new(vec![(signer(2).address(), debit)]).unwrap();
    assert!(
        build_genesis(
            exact.clone(),
            limits(),
            *signer(2).address().as_bytes(),
            &rows
        )
        .is_ok()
    );
    exact.accounts =
        AccountConfig::new(vec![(signer(2).address(), debit - U256::from(1))]).unwrap();
    assert!(build_genesis(exact, limits(), *signer(2).address().as_bytes(), &rows).is_err());
    let mut cap = limits();
    cap.max_canonical_bytes = 20;
    assert!(build_genesis(base(), cap, *signer(2).address().as_bytes(), &rows).is_err());
    let mut huge = row(0, 4);
    huge.payload = vec![0; MAX_CANONICAL_BYTES as usize];
    assert!(encode_stream(&[huge]).is_err());
}

#[test]
fn full_small_row_envelope_installs_without_truncation() {
    let rows = (0..256).map(|n| row(n, 1 + n / 16)).collect::<Vec<_>>();
    let g = build(&rows);
    let stream = encode_stream(&rows).unwrap();
    assert_eq!(decode_stream(&stream).unwrap().len(), 256);
    let h = host(
        &g,
        &stream,
        true,
        Arc::new(MemoryVersionedStore::new()),
        Arc::new(MemoryVersionedStore::new()),
    );
    assert_eq!(
        h.engine
            .head_snapshot()
            .try_typed_record_count(arkiv::entity_namespace())
            .unwrap(),
        Some(256)
    );
    let mut excess = rows;
    excess.push(row(256, 17));
    assert!(encode_stream(&excess).is_err());
}
#[test]
fn empty_prefill_has_namespace_and_ordinary_block_one() {
    let g = build(&[]);
    let stream = encode_stream(&[]).unwrap();
    let p = g.prefill.as_ref().unwrap();
    assert_eq!(p.state.catalog_len, 1);
    assert_eq!(p.state.next_namespace, 2);
    assert_eq!(p.state.host_len, 0);
    assert_eq!(p.burned_wei, U256::ZERO);
    assert_eq!(
        p.state.root().to_string(),
        "0x45f7562a51b1af8412561201c2a9078d852539808047a98a0f5cad0214c81ea2"
    );
    assert_eq!(
        g.hash().to_string(),
        "0x3513dc68c22d1b097df6a65706f568ab9806d2244b88ee491c439407e1b8dcf4"
    );
    assert_eq!(
        p.stream_hash.to_string(),
        "0x19b1e7a1632c13ccecf41f294948d27f600161c4254b341a03c7a6fc7a6b0510"
    );
    let mut h = host(
        &g,
        &stream,
        true,
        Arc::new(MemoryVersionedStore::new()),
        Arc::new(MemoryVersionedStore::new()),
    );
    h.produce().unwrap();
    assert!(h.block(1).unwrap().inputs.txs.is_empty());
    assert_eq!(h.committed_nonce(signer(1).address()).unwrap(), 0);
    assert_eq!(h.head.state_root, g.header().state_root);
    // Prefill already owns entities; new named namespaces are ordinary paid
    // transactions, never a repeated implicit bootstrap.
    let namespace = SignedTx::sign(
        &g,
        0,
        TxEnvelope {
            chain_id: g.genesis.chain_id,
            actor: signer(2).address(),
            request_id: [42; 32],
            tx_budget: 100_000,
            ops: vec![TxOp::CreateNamespace {
                name: arkiv_db::NamespaceName::new("bricks").unwrap(),
            }],
        },
        &signer(2),
    )
    .unwrap();
    h.submit(namespace).unwrap();
    h.produce().unwrap();
    assert_eq!(
        h.block(2).unwrap().outcomes.txs[0].status,
        arkiv_node::TxStatus::Committed
    );
    assert_eq!(h.engine.head_snapshot().try_namespace_count().unwrap(), 2);
}
#[test]
fn prefills_replay_proofs_next_create_expiry_and_restart() {
    let rows = vec![row(0, 4), row(1, 5)];
    let stream = encode_stream(&rows).unwrap();
    let g = build(&rows);
    let launch = g.prefill.as_ref().unwrap();
    let state = Arc::new(MemoryVersionedStore::new());
    let history = Arc::new(MemoryVersionedStore::new());
    let mut producer = host(&g, &stream, true, state.clone(), history.clone());
    let mut full = host(
        &g,
        &stream,
        false,
        Arc::new(MemoryVersionedStore::new()),
        Arc::new(MemoryVersionedStore::new()),
    );
    let mut light = Light::create(
        Arc::new(MemoryVersionedStore::new()),
        g.clone(),
        producer.certificate(0).unwrap(),
    )
    .unwrap();
    let (account, burn, supply) = account_proof(&producer, &light, 2, 0);
    assert_eq!(account.nonce, 0);
    assert_eq!(burn, launch.burned_wei);
    assert_eq!(account.balance + burn, supply);
    let (account, _, _) = account_proof(&producer, &light, 1, 0);
    assert_eq!(account.balance, U256::ZERO);
    assert_eq!(account.nonce, 0);
    let genesis_snap = producer
        .engine
        .snapshot(SnapshotId::from_sequence(0))
        .unwrap();
    assert_eq!(
        genesis_snap
            .try_host_get(&arkiv::nonce_key(signer(2).address().as_bytes()))
            .unwrap()
            .unwrap(),
        2u64.to_be_bytes()
    );
    for row in &rows {
        let key =
            arkiv::derive_entity_key(g.genesis.chain_id, &row.creator, row.create_nonce, row.salt);
        let read = genesis_snap
            .try_get_record(
                arkiv::entity_namespace(),
                &RecordKey::new(&key).unwrap(),
                &Projection::ALL,
                Budget::unlimited(),
            )
            .unwrap()
            .unwrap()
            .value
            .unwrap();
        let projection = arkiv::project(&read).unwrap();
        assert_eq!(projection.created_at, 0);
        assert_eq!(projection.updated_at, 0);
        assert_eq!(projection.owner, row.creator);
        assert_eq!(projection.payload, row.payload);
        assert_eq!(projection.expires_at, row.expires_at_height);
        assert_eq!(
            genesis_snap
                .try_host_get(&arkiv_node::exec::total_key(row.expires_at_height))
                .unwrap()
                .unwrap(),
            1u64.to_be_bytes()
        );
    }
    let live = SignedTx::sign(
        &g,
        0,
        TxEnvelope {
            chain_id: g.genesis.chain_id,
            actor: signer(2).address(),
            request_id: [9; 32],
            tx_budget: 1_000_000,
            ops: vec![TxOp::Arkiv {
                namespace: arkiv::entity_namespace(),
                operation: ArkivOperation::Create {
                    salt: 99,
                    flags: 0,
                    expiry: Expiry {
                        expires_at: 10,
                        min_lifetime: 0,
                    },
                    attributes: vec![],
                },
            }],
        },
        &signer(2),
    )
    .unwrap();
    producer.submit(live).unwrap();
    for height in 1..=5 {
        producer.produce().unwrap();
        let block = producer.block(height).unwrap();
        full.import(block.clone()).unwrap();
        light
            .append(std::slice::from_ref(&block.header), &block.header)
            .unwrap();
        assert_eq!(producer.head, full.head);
        assert_eq!(producer.head, light.head);
        if height == 1 {
            assert_eq!(block.inputs.txs.len(), 1);
            assert_eq!(producer.committed_nonce(signer(2).address()).unwrap(), 1);
            assert_eq!(producer.committed_nonce(signer(1).address()).unwrap(), 0);
            let key =
                arkiv::derive_entity_key(g.genesis.chain_id, signer(2).address().as_bytes(), 2, 99);
            assert!(
                producer
                    .engine
                    .head_snapshot()
                    .try_get_record(
                        arkiv::entity_namespace(),
                        &RecordKey::new(&key).unwrap(),
                        &Projection::ALL,
                        Budget::unlimited()
                    )
                    .unwrap()
                    .unwrap()
                    .value
                    .is_some()
            );
        }
        if height == 3 {
            drop(producer);
            producer = Host::open(
                state.clone(),
                History::open(history.clone(), g.hash()).unwrap(),
                &g,
                Some(signer(1)),
                Options::default(),
            )
            .unwrap();
        }
        assert_eq!(block.outcomes.expiry.len(), usize::from(height >= 4));
    }
    assert_eq!(
        producer
            .engine
            .head_snapshot()
            .try_typed_record_count(arkiv::entity_namespace())
            .unwrap(),
        Some(1)
    );
    assert_eq!(
        producer
            .engine
            .snapshot(SnapshotId::from_sequence(0))
            .unwrap()
            .try_typed_record_count(arkiv::entity_namespace())
            .unwrap(),
        Some(2)
    );
    assert_eq!(account_proof(&producer, &light, 2, 0).1, launch.burned_wei);
}
