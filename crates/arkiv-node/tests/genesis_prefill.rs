use alloy_primitives::U256;
use arkiv_db::{
    Budget, Cell, CellName, Projection, RecordKey, SnapshotId, Value, decode_exact,
    durable::Options, encode,
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
        attributes: vec![
            (
                CellName::new("body").unwrap(),
                Cell::field(Value::bytes(b"hello")),
            ),
            (
                CellName::new("label").unwrap(),
                Cell::attribute(Value::str("hello")),
            ),
        ],
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
    rows[0].attributes.push((
        CellName::new("alpha").unwrap(),
        Cell::attribute(Value::U64(42)),
    ));
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
    for height in [0, u64::MAX] {
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
    // Beyond max_lifetime is no longer too far, and every finite expiry at
    // or beyond the rent cap burns the same capped rent.
    let cap = base().genesis.params.max_rent_lifetime;
    let burn = |height| build(&[row(0, height)]).prefill.unwrap().burned_wei;
    assert!(burn(6_307_201) > U256::ZERO);
    assert_eq!(burn(cap), burn(2 * cap));
    assert_eq!(burn(cap), burn(1_000_000_000_000));
    assert!(burn(cap / 2) < burn(cap));
    let mut system = row(0, 4);
    system.attributes.push((
        CellName::new("$owner").unwrap(),
        Cell::attribute(Value::Addr([1; 20])),
    ));
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
    huge.attributes[0].1 = Cell::field(Value::bytes(&vec![0; 128 * 1024 + 1]));
    assert!(encode_stream(&[huge]).is_err());
    // Rows within every per-value and per-entity cap still meet the 1 MiB
    // canonical stream cap: seven full 128 KiB rows fit, nine do not.
    let full = |n: usize| {
        (0..n)
            .map(|i| {
                let mut r = row(i as u64, 4);
                r.attributes[0].1 = Cell::field(Value::bytes(&vec![i as u8; 128 * 1024]));
                r
            })
            .collect::<Vec<_>>()
    };
    let seven = encode_stream(&full(7)).unwrap();
    assert!(seven.len() as u64 <= MAX_CANONICAL_BYTES);
    assert_eq!(decode_stream(&seven).unwrap().len(), 7);
    let error = encode_stream(&full(9)).unwrap_err();
    assert!(error.contains("exceeds 1 MiB"), "{error}");
    let error = decode_stream(&vec![0; MAX_CANONICAL_BYTES as usize + 1]).unwrap_err();
    assert!(error.contains("exceeds 1 MiB"), "{error}");
    // Payload-era rows are not a spelling of anything: a stream with the old
    // codec is refused, and so are the removed system names.
    let mut old = encode_stream(&rows).unwrap();
    old[8..12].copy_from_slice(&1u32.to_be_bytes());
    assert!(decode_stream(&old).is_err());
    for removed in ["$payload", "$contentType"] {
        let mut r = row(0, 4);
        r.attributes.push((
            CellName::new(removed).unwrap(),
            Cell::field(Value::str("x")),
        ));
        assert!(encode_stream(&[r]).is_err(), "{removed}");
    }
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
        "0x7fe4f2cf5c222157f18f974fe4664e51611d2e097ca31e6247a72a3c1d6beaaf"
    );
    assert_eq!(
        p.stream_hash.to_string(),
        "0xbbcd0cc07104e6cdbb52cd26f82c7ae1466474a03dc96f3cff38244fb4ec16dc"
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
        assert_eq!(projection.attributes, row.attributes);
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

fn permanent_row(n: u64) -> PrefillEntity {
    row(n, PERMANENT_EXPIRY)
}
fn build_permanent(rows: &[PrefillEntity]) -> SignedGenesis {
    build_genesis_with(
        base(),
        limits(),
        *signer(2).address().as_bytes(),
        rows,
        true,
    )
    .unwrap()
}
fn entity_tx(g: &SignedGenesis, nonce: u64, operation: ArkivOperation) -> SignedTx {
    SignedTx::sign(
        g,
        nonce,
        TxEnvelope {
            chain_id: g.genesis.chain_id,
            actor: signer(2).address(),
            request_id: [100 + nonce as u8; 32],
            // Enough for a permanent create's one-year rent; six fit a block.
            tx_budget: 12_000_000,
            ops: vec![TxOp::Arkiv {
                namespace: arkiv::entity_namespace(),
                operation,
            }],
        },
        &signer(2),
    )
    .unwrap()
}
#[test]
fn permanent_policy_is_explicit_and_changes_identity_only() {
    // The finite policy keeps rejecting the sentinel; the permanent one
    // admits it with the same stream bytes and a distinct launch version.
    assert!(encode_stream(&[permanent_row(0)]).is_err());
    let stream = encode_stream_with(&[permanent_row(0)], true).unwrap();
    assert!(decode_stream(&stream).is_err());
    assert_eq!(
        decode_stream_with(&stream, true).unwrap(),
        [permanent_row(0)]
    );
    let rows = [row(0, 4), row(1, 5)];
    let finite = build(&rows);
    let permanent = build_permanent(&rows);
    let (f, p) = (
        finite.prefill.as_ref().unwrap(),
        permanent.prefill.as_ref().unwrap(),
    );
    assert_eq!((f.version, p.version), (1, 3));
    assert!(!f.permanent_prefill() && p.permanent_prefill());
    assert_eq!(
        (&f.state, f.burned_wei, f.storage_units, f.stream_hash),
        (&p.state, p.burned_wei, p.storage_units, p.stream_hash)
    );
    assert_ne!(f.recipe_hash, p.recipe_hash);
    assert_ne!(finite.hash(), permanent.hash());
    assert_eq!(
        decode_exact::<SignedGenesis>(&encode(&permanent)).unwrap(),
        permanent
    );
    // A version-1 commitment cannot be relabelled as permanent.
    let mut relabelled = finite.clone();
    relabelled.prefill.as_mut().unwrap().version = 3;
    assert!(relabelled.validate().is_err());
    // Rows beyond the 16-per-height capacity are admitted only when permanent.
    let many: Vec<_> = (0..40).map(permanent_row).collect();
    let g = build_permanent(&many);
    assert_eq!(g.prefill.as_ref().unwrap().burned_wei, U256::ZERO);
    assert!(g.prefill.as_ref().unwrap().storage_units > 40 * 512);
    let report = report(&g, &encode_stream_with(&many, true).unwrap());
    assert_eq!(report["permanentPrefill"], true);
    assert_eq!(report["prefillVersion"], 3);
    assert_eq!(
        report["entities"][0]["expiresAtHeight"],
        u64::MAX.to_string()
    );
    assert_eq!(report["entities"][0]["rentGas"], "0");
    assert!(
        has_no_permanent_fields(&finite, &encode_stream(&rows).unwrap()),
        "finite reports carry no permanent fields"
    );
}
fn has_no_permanent_fields(g: &SignedGenesis, stream: &[u8]) -> bool {
    let value = report(g, stream);
    value.get("permanentPrefill").is_none() && value.get("prefillVersion").is_none()
}
#[test]
fn permanent_genesis_entities_survive_blocks_and_take_capped_live_updates() {
    let rows = permanent_chain_rows();
    let stream = encode_stream_with(&rows, true).unwrap();
    let g = build_permanent(&rows);
    let make = |producer| {
        host(
            &g,
            &stream,
            producer,
            Arc::new(MemoryVersionedStore::new()),
            Arc::new(MemoryVersionedStore::new()),
        )
    };
    exercise_permanent_chain(&g, &rows, make(true), make(false));
}
/// The same rows as a version-4 (file-backed) launch: bulk construction,
/// staged installation, then the same live behaviour as version 3.
#[test]
fn streaming_permanent_genesis_installs_staged_state_and_behaves_the_same() {
    use arkiv_node::genesis_streaming::{self as streaming, VerifiedStream};
    let rows = permanent_chain_rows();
    let small = build_permanent(&rows);
    let mut bytes = b"ARKPFL02".to_vec();
    bytes.extend_from_slice(&arkiv_node::genesis_bundle::ROW_CODEC.to_be_bytes());
    bytes.extend_from_slice(&(rows.len() as u64).to_be_bytes());
    for row in &rows {
        let frame = encode(row);
        bytes.extend_from_slice(&(frame.len() as u32).to_be_bytes());
        bytes.extend(frame);
    }
    let mut g = small.clone();
    let mut launch = g.prefill.take().unwrap();
    launch.version = 4;
    launch.limits = PrefillLimits {
        max_entities: streaming::MAX_ENTITIES,
        max_canonical_bytes: streaming::MAX_CANONICAL_BYTES,
        max_storage_units: streaming::MAX_STORAGE_UNITS,
    };
    launch.stream_hash = arkiv_db::digest(streaming::STREAM_DOMAIN, &bytes);
    launch.canonical_bytes = bytes.len() as u64;
    launch.recipe_hash = recipe_hash(&g, &launch);
    g.prefill = Some(launch);
    g.validate().unwrap();
    assert_ne!(g.hash(), small.hash());
    let dir = std::env::temp_dir().join(format!("arkiv-permanent-v4-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir(&dir).unwrap();
    let path = dir.join("prefill.bin");
    std::fs::write(&path, &bytes).unwrap();
    let make = |producer: bool| {
        let staged = streaming::prepare_verified_streaming(
            &g,
            VerifiedStream::open(&g, &path).unwrap(),
            &dir.join(format!("work-{producer}")),
            Options::default(),
        )
        .unwrap();
        Host::create_from_staged(
            Arc::new(MemoryVersionedStore::new()),
            History::create(Arc::new(MemoryVersionedStore::new()), g.hash()).unwrap(),
            g.clone(),
            [3; 16],
            SignedHeader::sign(&g, g.header(), &signer(1)).unwrap(),
            producer.then(|| signer(1)),
            Options::default(),
            &staged,
            path.clone(),
        )
        .unwrap()
    };
    let (producer, full) = (make(true), make(false));
    // Bulk and the small v3 builder agree on state, burn and storage units.
    let (p4, p3) = (g.prefill.as_ref().unwrap(), small.prefill.as_ref().unwrap());
    assert_eq!(
        (&p4.state, p4.burned_wei, p4.storage_units),
        (&p3.state, p3.burned_wei, p3.storage_units)
    );
    exercise_permanent_chain(&g, &rows, producer, full);
    std::fs::remove_dir_all(&dir).unwrap();
}
/// Nonce 0 expires at height 3; nonces 1 and 2 never expire.
fn permanent_chain_rows() -> Vec<PrefillEntity> {
    vec![row(0, 3), permanent_row(1), permanent_row(2)]
}
fn exercise_permanent_chain(
    g: &SignedGenesis,
    rows: &[PrefillEntity],
    mut producer: Host,
    mut full: Host,
) {
    use arkiv_node::{HostFault, OpOutcome, TxStatus, signed::runtime::Node};
    let launch = g.prefill.clone().unwrap();
    let mut light = Light::create(
        Arc::new(MemoryVersionedStore::new()),
        g.clone(),
        producer.certificate(0).unwrap(),
    )
    .unwrap();
    // Only the finite row is scheduled and charged.
    let genesis = producer
        .engine
        .snapshot(SnapshotId::from_sequence(0))
        .unwrap();
    assert_eq!(
        genesis
            .try_host_get(&arkiv_node::exec::total_key(3))
            .unwrap(),
        Some(1u64.to_be_bytes().to_vec())
    );
    assert_eq!(
        genesis
            .try_host_get(&arkiv_node::exec::total_key(u64::MAX))
            .unwrap(),
        None
    );
    let finite_only = build(&rows[..1]);
    assert_eq!(launch.burned_wei, finite_only.prefill.unwrap().burned_wei);
    assert_eq!(account_proof(&producer, &light, 2, 0).1, launch.burned_wei);
    let keys: Vec<_> = rows
        .iter()
        .map(|r| {
            RecordKey::new(&arkiv::derive_entity_key(
                g.genesis.chain_id,
                &r.creator,
                r.create_nonce,
                r.salt,
            ))
            .unwrap()
        })
        .collect();
    let live = |h: &Host, k: &RecordKey| {
        h.engine
            .head_snapshot()
            .try_get_record(
                arkiv::entity_namespace(),
                k,
                &Projection::ALL,
                Budget::unlimited(),
            )
            .unwrap()
            .unwrap()
            .value
            .map(|r| arkiv::project(&r).unwrap())
    };
    // Live updates of a permanent entity pay the capped rent and keep it
    // permanent; only a shortening extension is refused. A live permanent
    // create is admitted too. The owner transfers last, after its updates.
    let set = vec![(
        CellName::new("label").unwrap(),
        Cell::attribute(Value::str("changed")),
    )];
    let attempts = [
        ArkivOperation::Patch {
            key: keys[1].clone(),
            set,
            remove: vec![],
        },
        ArkivOperation::ExtendExpiry {
            key: keys[1].clone(),
            expiry: Expiry::PERMANENT,
        },
        ArkivOperation::ExtendExpiry {
            key: keys[1].clone(),
            expiry: Expiry {
                expires_at: 1_000,
                min_lifetime: 0,
            },
        },
        ArkivOperation::Create {
            salt: 77,
            flags: 0,
            expiry: Expiry::PERMANENT,
            attributes: vec![],
        },
        ArkivOperation::Delete {
            key: keys[2].clone(),
        },
        ArkivOperation::TransferOwnership {
            key: keys[1].clone(),
            owner: [9; 20],
        },
    ];
    for (nonce, operation) in attempts.into_iter().enumerate() {
        producer
            .submit(entity_tx(g, nonce as u64, operation))
            .unwrap();
    }
    for height in 1..=6 {
        producer.produce().unwrap();
        let block = producer.block(height).unwrap();
        full.import(block.clone()).unwrap();
        light
            .append(std::slice::from_ref(&block.header), &block.header)
            .unwrap();
        assert_eq!((producer.head, light.head), (full.head, full.head));
        // Only the finite genesis row is ever swept.
        assert_eq!(block.outcomes.expiry.len(), usize::from(height == 3));
        if height == 1 {
            let faults: Vec<_> = block
                .outcomes
                .txs
                .iter()
                .map(|tx| match (&tx.status, &tx.ops[0]) {
                    (TxStatus::Committed, _) => None,
                    (_, OpOutcome::HostRejected { fault }) => Some(*fault),
                    other => panic!("{other:?}"),
                })
                .collect();
            assert_eq!(
                faults,
                [
                    None,
                    None,
                    Some(HostFault::ExpiryShortened),
                    None,
                    None,
                    None
                ]
            );
        }
    }
    let kept = live(&producer, &keys[1]).unwrap();
    assert_eq!((kept.expires_at, kept.owner), (u64::MAX, [9; 20]));
    assert_eq!(kept.updated_at, 1);
    assert!(live(&producer, &keys[0]).is_none(), "finite row expired");
    assert!(live(&producer, &keys[2]).is_none(), "owner deleted it");
    let created = RecordKey::new(&arkiv::derive_entity_key(
        g.genesis.chain_id,
        signer(2).address().as_bytes(),
        rows.iter()
            .filter(|r| r.creator == *signer(2).address().as_bytes())
            .count() as u64,
        77,
    ))
    .unwrap();
    assert_eq!(live(&producer, &created).unwrap().expires_at, u64::MAX);
    assert_eq!(
        producer
            .engine
            .head_snapshot()
            .try_typed_record_count(arkiv::entity_namespace())
            .unwrap(),
        Some(2)
    );
    // Nothing is due at the sentinel after these live writes either.
    assert_eq!(
        producer
            .engine
            .head_snapshot()
            .try_host_get(&arkiv_node::exec::total_key(u64::MAX))
            .unwrap(),
        None
    );
    // Reads report the sentinel, as the SDK's U64_MAX bigint.
    let page = arkiv_node::signed::rpc_reads::dispatch(
        &producer,
        "arkiv_query",
        &serde_json::json!(["*", {"atBlock":"0x0","select":{"key":true,"expiresAt":true}}]),
    )
    .unwrap()
    .unwrap();
    let expiries: Vec<_> = page["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["expiresAt"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(expiries.len(), 3);
    assert_eq!(
        expiries
            .iter()
            .filter(|e| *e == "0xffffffffffffffff")
            .count(),
        2
    );
    // Status reports live permanence and the sponsored genesis rows.
    let status = arkiv_node::signed::runtime::status(&Node::Full(Box::new(producer)));
    let gas = &status["capabilities"]["gas"];
    assert_eq!(gas["permanentEntitiesSupported"], true);
    assert_eq!(gas["permanentEntities"]["liveCreate"], true);
    assert_eq!(
        gas["permanentEntities"]["genesisRows"],
        "sponsored-without-rent"
    );
    assert_eq!(
        gas["maxRentLifetimeBlocks"],
        g.genesis.params.max_rent_lifetime.to_string()
    );
    let unsupported = status["capabilities"]["sdkRpcProfile"]["unsupported"].to_string();
    assert!(!unsupported.contains("permanent"), "{unsupported}");
    let finite = build(&rows[..1]);
    let finite_host = host(
        &finite,
        &encode_stream(&rows[..1]).unwrap(),
        true,
        Arc::new(MemoryVersionedStore::new()),
        Arc::new(MemoryVersionedStore::new()),
    );
    let status = arkiv_node::signed::runtime::status(&Node::Full(Box::new(finite_host)));
    let gas = &status["capabilities"]["gas"];
    assert_eq!(gas["permanentEntitiesSupported"], true);
    assert_eq!(gas["permanentEntities"]["genesisRows"], "none");
}
