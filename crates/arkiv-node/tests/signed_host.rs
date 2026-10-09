//! Signed execution, independent replay, retained metadata and publication faults.
use arkiv_db::{
    B256, Budget, CellName, OwnerId, Projection, RecordKey, Value,
    durable::{Options, WriterState},
    encode,
    proof::{EqQuery, EqRequest, SnapshotRef, VerifiedEq},
};
use arkiv_db_memory::MemoryVersionedStore;
use arkiv_db_storage::{
    testing::{CommitFault, FaultyStore, ReadScope},
    versioned::{ObjectReader, SpaceId, StorageErrorKind, VersionedStore},
};
use arkiv_node::{
    Expiry, HostFault, OpOutcome, TxEnvelope, TxOp, TxStatus,
    arkiv::{self, ArkivFault, ArkivOperation},
    signed::{
        history::History,
        host::{HeaderRecord, Host},
        light::Light,
        protocol::{SignedBlock, SignedGenesis, SignedHeader, SignedTx},
    },
};
use std::sync::Arc;

mod signed_support;
use signed_support::signer;
fn genesis() -> SignedGenesis {
    signed_support::genesis(9100, 1000, [7; 16], 1, 100, &[1, 2, 3])
}
fn create(
    state: Arc<dyn VersionedStore>,
    history: Arc<dyn VersionedStore>,
    producer: bool,
) -> Host {
    let genesis = genesis();
    let certificate = SignedHeader::sign(&genesis, genesis.header(), &signer(1)).unwrap();
    Host::create(
        state,
        History::create(history, genesis.hash()).unwrap(),
        genesis,
        [3; 16],
        certificate,
        producer.then(|| signer(1)),
        Options::default(),
    )
    .unwrap()
}
fn memory(producer: bool) -> Host {
    create(
        Arc::new(MemoryVersionedStore::new()),
        Arc::new(MemoryVersionedStore::new()),
        producer,
    )
}
fn reopen(
    state: Arc<dyn VersionedStore>,
    history: Arc<dyn VersionedStore>,
    producer: bool,
) -> Host {
    let genesis = genesis();
    Host::open(
        state,
        History::open(history, genesis.hash()).unwrap(),
        &genesis,
        producer.then(|| signer(1)),
        Options::default(),
    )
    .unwrap()
}
fn tx(key: u8, nonce: u64, request: u8, operations: Vec<ArkivOperation>) -> SignedTx {
    let signer = signer(key);
    SignedTx::sign(
        &genesis(),
        nonce,
        TxEnvelope {
            chain_id: genesis().genesis.chain_id,
            actor: signer.address(),
            request_id: [request; 32],
            tx_budget: 10_000_000,
            ops: operations
                .into_iter()
                .map(|operation| TxOp::Arkiv {
                    namespace: arkiv::entity_namespace(),
                    operation,
                })
                .collect(),
        },
        &signer,
    )
    .unwrap()
}
fn create_entity(salt: u128, payload: &[u8]) -> ArkivOperation {
    ArkivOperation::Create {
        salt,
        flags: 0,
        expiry: Expiry {
            expires_at: 0,
            min_lifetime: 1_000,
        },
        attributes: vec![
            (CellName::new("group").unwrap(), Value::U64(1)),
            (CellName::new("$payload").unwrap(), Value::bytes(payload)),
        ],
    }
}
fn entity_key(key: u8, creation_nonce: u64, salt: u128) -> RecordKey {
    RecordKey::new(&arkiv::derive_entity_key(
        genesis().genesis.chain_id,
        signer(key).address().as_bytes(),
        creation_nonce,
        salt,
    ))
    .unwrap()
}
fn entity(host: &Host, key: &RecordKey) -> Option<arkiv::ArkivEntityProjection> {
    host.engine
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
        .map(|row| arkiv::project(&row).unwrap())
}
fn creation_nonce(host: &Host, actor: OwnerId) -> u64 {
    host.engine
        .head_snapshot()
        .try_host_get(&arkiv::nonce_key(actor.as_bytes()))
        .unwrap()
        .map_or(0, |bytes| u64::from_be_bytes(bytes.try_into().unwrap()))
}
fn message<T>(result: arkiv_node::signed::Result<T>) -> &'static str {
    result.err().expect("expected rejection").0
}
fn relay(producer: &Host, full: &mut Host, light: &mut Light) {
    let block = producer.block(producer.head.height).unwrap();
    full.import(block.clone()).unwrap();
    light
        .append(std::slice::from_ref(&block.header), &block.header)
        .unwrap();
    assert_eq!(full.head, producer.head);
    assert_eq!(light.head, producer.head);
    assert_eq!(full.block(full.head.height).unwrap(), block);
    assert_eq!(
        full.feed(full.head.height).unwrap(),
        producer.feed(producer.head.height).unwrap()
    );
}

#[test]
fn signed_lifecycle_replays_bootstrap_ownership_failures_and_native_proofs() {
    let mut producer = memory(true);
    let mut full = memory(false);
    let mut light = Light::create(
        Arc::new(MemoryVersionedStore::new()),
        genesis(),
        producer.certificate(0).unwrap(),
    )
    .unwrap();
    let alice = signer(2).address();
    let bob = signer(3).address();
    let create_tx = tx(2, 0, 1, vec![create_entity(10, b"original payload")]);
    assert_eq!(
        message(producer.submit(create_tx.clone())),
        "ChainInitializing"
    );
    assert_eq!(message(full.submit(create_tx.clone())), "ProducerOnly");
    producer.produce().unwrap();
    let bootstrap = producer.block(1).unwrap();
    assert_eq!(bootstrap.inputs.txs.len(), 1);
    assert_eq!(bootstrap.inputs.txs[0].tx.actor, signer(1).address());
    assert_eq!(bootstrap.inputs.txs[0].nonce, 0);
    assert_eq!(producer.committed_nonce(signer(1).address()).unwrap(), 1);
    assert_eq!(creation_nonce(&producer, signer(1).address()), 0);
    relay(&producer, &mut full, &mut light);
    assert_eq!(
        producer.submit(create_tx.clone()).unwrap()["status"],
        "pending"
    );
    assert_eq!(
        producer.submit(create_tx.clone()).unwrap()["status"],
        "pending"
    );
    assert_eq!(producer.pool.len(), 1);
    producer.produce().unwrap();
    relay(&producer, &mut full, &mut light);
    let key = entity_key(2, 0, 10);
    assert_eq!(
        entity(&producer, &key).unwrap().payload,
        b"original payload"
    );
    assert_eq!(producer.committed_nonce(alice).unwrap(), 1);
    let transfer = tx(
        2,
        1,
        2,
        vec![ArkivOperation::TransferOwnership {
            key: key.clone(),
            owner: *bob.as_bytes(),
        }],
    );
    producer.submit(transfer).unwrap();
    producer.produce().unwrap();
    relay(&producer, &mut full, &mut light);
    let patch = tx(
        3,
        0,
        3,
        vec![ArkivOperation::Patch {
            key: key.clone(),
            set: vec![(
                CellName::new("$payload").unwrap(),
                Value::bytes(b"new owner update"),
            )],
            remove: vec![],
        }],
    );
    producer.submit(patch).unwrap();
    producer.produce().unwrap();
    relay(&producer, &mut full, &mut light);
    let failed = tx(2, 2, 4, vec![ArkivOperation::Delete { key: key.clone() }]);
    producer.submit(failed.clone()).unwrap();
    producer.produce().unwrap();
    relay(&producer, &mut full, &mut light);
    let block = producer.block(5).unwrap();
    assert_eq!(
        block.outcomes.txs[0].status,
        TxStatus::Failed { op_index: 0 }
    );
    assert!(matches!(
        block.outcomes.txs[0].ops[0],
        OpOutcome::HostRejected {
            fault: HostFault::Arkiv(ArkivFault::NotOwner)
        }
    ));
    assert_eq!(producer.committed_nonce(alice).unwrap(), 3);
    assert_eq!(producer.committed_nonce(bob).unwrap(), 1);
    assert_eq!(entity(&producer, &key).unwrap().owner, *bob.as_bytes());
    assert_eq!(entity(&producer, &key).unwrap().creator, *alice.as_bytes());
    assert_eq!(
        entity(&producer, &key).unwrap().payload,
        b"new owner update"
    );
    assert_eq!(producer.submit(failed).unwrap()["status"], "included");
    let request = EqRequest {
        snapshot: SnapshotRef {
            genesis_hash: genesis().hash(),
            height: producer.head.height,
            block_hash: producer.head.hash(),
            state_root: producer.head.state_root,
        },
        query: EqQuery {
            namespace: arkiv::entity_namespace(),
            attribute: CellName::new("group").unwrap(),
            value: Value::U64(1),
        },
        limit: 16,
        continuation: None,
    };
    let proof = producer.proof(&request).unwrap();
    assert_eq!(full.proof(&request).unwrap(), proof);
    let VerifiedEq::Page(page) = light.verify(&request, None, &proof).unwrap() else {
        panic!("namespace missing")
    };
    assert_eq!(page.rows.len(), 1);
    assert_eq!(
        arkiv::project(&page.rows[0]).unwrap().payload,
        b"new owner update"
    );
    producer.produce().unwrap();
    relay(&producer, &mut full, &mut light);
    assert!(producer.block(6).unwrap().inputs.txs.is_empty());
    assert_eq!(producer.committed_nonce(alice).unwrap(), 3);
}

#[test]
fn multi_operation_failure_rolls_back_entity_nonce_but_consumes_transaction_nonce() {
    let mut producer = memory(true);
    producer.produce().unwrap();
    let mut full = memory(false);
    full.import(producer.block(1).unwrap()).unwrap();
    let alice = signer(2).address();
    let two_creates = tx(
        2,
        0,
        1,
        vec![create_entity(10, b"first"), create_entity(11, b"second")],
    );
    producer.submit(two_creates).unwrap();
    producer.produce().unwrap();
    full.import(producer.block(2).unwrap()).unwrap();
    assert_eq!(producer.committed_nonce(alice).unwrap(), 1);
    assert_eq!(creation_nonce(&producer, alice), 2);
    assert!(entity(&producer, &entity_key(2, 0, 10)).is_some());
    assert!(entity(&producer, &entity_key(2, 1, 11)).is_some());
    let rolled_back_key = entity_key(2, 2, 12);
    let failure = tx(
        2,
        1,
        2,
        vec![
            create_entity(12, b"rollback"),
            ArkivOperation::Delete {
                key: RecordKey::new(&[0x99; 32]).unwrap(),
            },
        ],
    );
    producer.submit(failure.clone()).unwrap();
    producer.produce().unwrap();
    let block = producer.block(3).unwrap();
    full.import(block.clone()).unwrap();
    assert_eq!(
        block.outcomes.txs[0].status,
        TxStatus::Failed { op_index: 1 }
    );
    assert!(matches!(
        block.outcomes.txs[0].ops[0],
        OpOutcome::RolledBack { .. }
    ));
    assert!(block.outcomes.txs[0].spent > 0);
    assert_eq!(
        block.outcomes.txs[0].spent,
        arkiv_node::gas::TX_INTRINSIC_GAS
            + block.outcomes.txs[0]
                .ops
                .iter()
                .map(OpOutcome::spent)
                .sum::<u64>()
    );
    assert_eq!(producer.committed_nonce(alice).unwrap(), 2);
    assert_eq!(creation_nonce(&producer, alice), 2);
    assert!(entity(&producer, &rolled_back_key).is_none());
    assert_eq!(
        producer.request_status(alice, [2; 32]).unwrap()["status"],
        "included"
    );
    let retry = tx(2, 2, 3, vec![create_entity(12, b"retained")]);
    producer.submit(retry).unwrap();
    producer.produce().unwrap();
    full.import(producer.block(4).unwrap()).unwrap();
    assert_eq!(
        entity(&producer, &rolled_back_key).unwrap().payload,
        b"retained"
    );
    assert!(entity(&producer, &entity_key(2, 3, 12)).is_none());
    assert_eq!(creation_nonce(&producer, alice), 3);
    assert_eq!(producer.committed_nonce(alice).unwrap(), 3);
    assert_eq!(full.head, producer.head);
    assert_eq!(full.committed_nonce(alice).unwrap(), 3);
}

#[test]
fn restart_preserves_inclusion_nonce_and_old_bodies_but_drops_pending_entries() {
    let state = Arc::new(MemoryVersionedStore::new());
    let history = Arc::new(MemoryVersionedStore::new());
    let mut producer = create(state.clone(), history.clone(), true);
    producer.produce().unwrap();
    let included = tx(2, 0, 1, vec![create_entity(10, b"included payload")]);
    let pending = tx(2, 1, 2, vec![create_entity(11, b"pending payload")]);
    producer.submit(included.clone()).unwrap();
    producer.produce().unwrap();
    let old_body = producer.block(2).unwrap();
    producer.submit(pending.clone()).unwrap();
    assert_eq!(producer.pool.len(), 1);
    drop(producer);
    let mut producer = reopen(state.clone(), history.clone(), true);
    assert!(producer.pool.is_empty());
    assert_eq!(producer.committed_nonce(signer(2).address()).unwrap(), 1);
    assert_eq!(
        producer.submit(included.clone()).unwrap()["status"],
        "included"
    );
    assert_eq!(
        producer
            .request_status(signer(2).address(), [2; 32])
            .unwrap()["status"],
        "unknown"
    );
    let reused_request = tx(2, 1, 1, vec![create_entity(12, b"different signed bytes")]);
    assert_eq!(message(producer.submit(reused_request)), "RequestConflict");
    let used_nonce = tx(2, 0, 3, vec![create_entity(13, b"old nonce")]);
    assert_eq!(message(producer.submit(used_nonce)), "NonceTooLow");
    producer.submit(pending).unwrap();
    producer.produce().unwrap();
    assert_eq!(producer.block(2).unwrap(), old_body);
    let mut full = memory(false);
    for height in 1..=producer.head.height {
        full.import(producer.block(height).unwrap()).unwrap();
    }
    assert_eq!(full.head, producer.head);
    assert_eq!(full.committed_nonce(signer(2).address()).unwrap(), 2);
    assert_eq!(
        full.request_status(signer(2).address(), [1; 32]).unwrap(),
        producer
            .request_status(signer(2).address(), [1; 32])
            .unwrap()
    );
    drop(producer);
    assert_eq!(reopen(state, history, true).head, full.head);
}

#[test]
fn reference_only_engine_envelope_excludes_calldata_and_preserves_intended_entity_state() {
    let state = Arc::new(MemoryVersionedStore::new());
    let history = Arc::new(MemoryVersionedStore::new());
    let mut producer = create(state, history.clone(), true);
    producer.produce().unwrap();
    let sentinel = b"SIGNED_PAYLOAD_SENTINEL_ThisShouldOnlyBeBodyOrEntityState_9100";
    let signed = tx(2, 0, 1, vec![create_entity(10, sentinel)]);
    let hash = signed.hash();
    producer.submit(signed).unwrap();
    producer.produce().unwrap();
    let stored = producer.engine.simulator_block(2).unwrap();
    let record =
        HeaderRecord::from_bytes(&stored.bytes, &producer.genesis, &producer.identity).unwrap();
    assert_eq!(record.bytes().unwrap(), stored.bytes);
    assert!(
        !stored
            .bytes
            .windows(sentinel.len())
            .any(|slice| slice == sentinel)
    );
    let feed = serde_json::to_vec(&record.feed).unwrap();
    assert!(!feed.windows(sentinel.len()).any(|slice| slice == sentinel));
    assert!(
        !String::from_utf8(feed)
            .unwrap()
            .contains(&arkiv_node::simulator::wire::hex(sentinel))
    );
    assert_eq!(
        record.feed.transactions[0].digest,
        arkiv_node::simulator::wire::hex(hash.as_slice())
    );
    let framed = history
        .read_object(SpaceId::Host, &record.body.id.0)
        .unwrap()
        .unwrap();
    assert!(
        framed
            .windows(sentinel.len())
            .any(|slice| slice == sentinel)
    );
    assert!(
        encode(&producer.block(2).unwrap())
            .windows(sentinel.len())
            .any(|slice| slice == sentinel)
    );
    assert_eq!(
        entity(&producer, &entity_key(2, 0, 10)).unwrap().payload,
        sentinel
    );
    // Exact closed decode of the host packet proves its contents, instead of a
    // misleading grep over engine pages that intentionally include entity state.
    let mut decoder = arkiv_db::Decoder::new(&stored.bytes);
    assert_eq!(decoder.array::<8>().unwrap(), *b"ARKSGNR1");
    let certificate: SignedHeader = decoder.get().unwrap();
    let reference: arkiv_node::signed::history::BodyRef = decoder.get().unwrap();
    let metadata = decoder.bytes().unwrap();
    assert!(serde_json::from_slice::<arkiv_node::simulator::wire::FeedBlock>(metadata).is_ok());
    assert_eq!(decoder.remaining(), 0);
    assert_eq!(certificate, record.certificate);
    assert_eq!(reference, record.body);
}

fn recertify(block: &mut SignedBlock) {
    block.header = SignedHeader::sign(&genesis(), block.header.header, &signer(1)).unwrap();
}
#[test]
fn full_replay_rejects_authenticated_but_invalid_body_nonce_root_outcomes_and_parent() {
    let mut producer = memory(true);
    producer.produce().unwrap();
    producer
        .submit(tx(2, 0, 1, vec![create_entity(10, b"authentic")]))
        .unwrap();
    producer.produce().unwrap();
    let original = producer.block(2).unwrap();
    for mutation in 0..7 {
        // Each mutation carries a different authentic certificate. Isolate
        // these cases so rejection is proven by replay, not by equivocation
        // against a certificate already observed in the previous case.
        let mut full = memory(false);
        full.import(producer.block(1).unwrap()).unwrap();
        let mut invalid = original.clone();
        match mutation {
            0 => {
                invalid.inputs.txs[0].signature[0] ^= 1;
                invalid.header.header.inputs_digest = invalid.inputs.digest();
                recertify(&mut invalid);
            }
            1 => {
                invalid.inputs.txs[0] = tx(2, 1, 1, vec![create_entity(10, b"authentic")]);
                invalid.header.header.inputs_digest = invalid.inputs.digest();
                recertify(&mut invalid);
            }
            2 => {
                invalid.header.header.state_root = B256::repeat_byte(0x55);
                recertify(&mut invalid);
            }
            3 => {
                invalid.outcomes.spent += 1;
                invalid.header.header.outcomes_digest = invalid.outcomes.digest();
                recertify(&mut invalid);
            }
            4 => {
                invalid.header.header.parent_hash = B256::repeat_byte(0x66);
                recertify(&mut invalid);
            }
            5 => {
                invalid.header.header.inputs_digest = B256::repeat_byte(0x77);
                recertify(&mut invalid);
            }
            _ => {
                invalid.header.signature = [0; 65];
            }
        }
        assert!(full.import(invalid).is_err(), "mutation {mutation}");
        assert_eq!(full.head.height, 1);
        assert_eq!(full.committed_nonce(signer(2).address()).unwrap(), 0);
        assert_eq!(full.engine.writer_state(), WriterState::Ready);
    }
    let mut full = memory(false);
    full.import(producer.block(1).unwrap()).unwrap();
    full.import(original).unwrap();
    assert_eq!(full.head, producer.head);
}

#[test]
fn pool_gaps_wait_and_contiguous_nonces_execute_in_one_block() {
    let mut producer = memory(true);
    producer.produce().unwrap();
    let first = tx(2, 0, 1, vec![create_entity(10, b"zero")]);
    let second = tx(2, 1, 2, vec![create_entity(11, b"one")]);
    producer.submit(second.clone()).unwrap();
    producer.produce().unwrap();
    assert!(producer.block(2).unwrap().inputs.txs.is_empty());
    assert_eq!(producer.pool.len(), 1);
    producer.submit(first.clone()).unwrap();
    producer.produce().unwrap();
    assert_eq!(producer.block(3).unwrap().inputs.txs, vec![first, second]);
    assert_eq!(producer.committed_nonce(signer(2).address()).unwrap(), 2);
    assert!(producer.pool.is_empty());
}

#[test]
fn included_budget_failure_consumes_nonce_before_the_same_block_successor() {
    let mut producer = memory(true);
    producer.produce().unwrap();
    let mut cheap = tx(2, 0, 1, vec![create_entity(10, b"over budget")]);
    // Only the intrinsic charge fits; the create runs out of budget.
    cheap.tx.tx_budget = arkiv_node::gas::TX_INTRINSIC_GAS;
    cheap = SignedTx::sign(&genesis(), 0, cheap.tx, &signer(2)).unwrap();
    let succeeds = tx(
        2,
        1,
        2,
        vec![create_entity(11, b"following failed transaction")],
    );
    producer.submit(cheap.clone()).unwrap();
    producer.submit(succeeds.clone()).unwrap();
    producer.produce().unwrap();
    let block = producer.block(2).unwrap();
    assert_eq!(block.inputs.txs, vec![cheap, succeeds]);
    assert_eq!(
        block.outcomes.txs[0].status,
        TxStatus::Failed { op_index: 0 }
    );
    assert_eq!(block.outcomes.txs[1].status, TxStatus::Committed);
    assert_eq!(producer.committed_nonce(signer(2).address()).unwrap(), 2);
    assert_eq!(creation_nonce(&producer, signer(2).address()), 1);
    assert!(entity(&producer, &entity_key(2, 0, 10)).is_none());
    assert_eq!(
        entity(&producer, &entity_key(2, 0, 11)).unwrap().payload,
        b"following failed transaction"
    );
    let mut full = memory(false);
    full.import(producer.block(1).unwrap()).unwrap();
    full.import(block).unwrap();
    assert_eq!(full.head, producer.head);
    assert_eq!(full.committed_nonce(signer(2).address()).unwrap(), 2);
}

#[test]
fn full_independently_enforces_authorized_successful_namespace_bootstrap() {
    let mut producer = memory(true);
    producer.produce().unwrap();
    let original = producer.block(1).unwrap();
    for mutation in 0..3 {
        let mut full = memory(false);
        let mut invalid = original.clone();
        let mut envelope = invalid.inputs.txs[0].tx.clone();
        let signing_key = if mutation == 0 { 2 } else { 1 };
        envelope.actor = signer(signing_key).address();
        if mutation == 1 {
            envelope.ops = vec![TxOp::CreateNamespace {
                name: arkiv_db::NamespaceName::new("hijacked").unwrap(),
            }];
        }
        if mutation == 2 {
            envelope.tx_budget = arkiv_node::gas::TX_INTRINSIC_GAS;
        }
        invalid.inputs.txs[0] =
            SignedTx::sign(&genesis(), 0, envelope, &signer(signing_key)).unwrap();
        invalid.header.header.inputs_digest = invalid.inputs.digest();
        recertify(&mut invalid);
        assert!(full.import(invalid).is_err());
        assert_eq!(full.head.height, 0);
        assert_eq!(full.committed_nonce(signer(1).address()).unwrap(), 0);
        assert_eq!(
            full.engine.head_snapshot().try_namespace_count().unwrap(),
            0
        );
    }
    let mut full = memory(false);
    full.import(original).unwrap();
    assert_eq!(full.head, producer.head);
}

#[test]
fn full_authentic_equivocation_freezes_history_and_changed_signer_cannot_reopen() {
    let mut producer = memory(true);
    producer.produce().unwrap();
    producer
        .submit(tx(2, 0, 1, vec![create_entity(10, b"original")]))
        .unwrap();
    producer.produce().unwrap();
    let state = Arc::new(MemoryVersionedStore::new());
    let history = Arc::new(MemoryVersionedStore::new());
    let mut full = create(state.clone(), history.clone(), false);
    full.import(producer.block(1).unwrap()).unwrap();
    full.import(producer.block(2).unwrap()).unwrap();
    let wrong_signer = Host::open(
        state.clone(),
        History::open(history.clone(), genesis().hash()).unwrap(),
        &genesis(),
        Some(signer(3)),
        Options::default(),
    );
    assert_eq!(message(wrong_signer), "IdentityMismatch");
    let mut other = producer.block(2).unwrap();
    other.header.header.state_root = B256::repeat_byte(0x99);
    recertify(&mut other);
    let mut forged = other.clone();
    forged.header.signature = [0; 65];
    assert!(full.import(forged).is_err());
    assert_ne!(full.health, "chain-conflict");
    assert_eq!(message(full.import(other)), "ChainConflict");
    assert_eq!(full.health, "chain-conflict");
    producer.produce().unwrap();
    assert_eq!(
        message(full.import(producer.block(3).unwrap())),
        "ChainConflict"
    );
    drop(full);
    assert_eq!(
        message(History::open(history, genesis().hash())),
        "ChainConflict"
    );
}

#[test]
fn full_future_observation_survives_reopen_and_conflicting_import_freezes() {
    let mut producer = memory(true);
    producer.produce().unwrap();
    producer
        .submit(tx(2, 0, 1, vec![create_entity(10, b"future")]))
        .unwrap();
    producer.produce().unwrap();
    let future = producer.block(2).unwrap();
    let state = Arc::new(MemoryVersionedStore::new());
    let history = Arc::new(MemoryVersionedStore::new());
    let mut full = create(state.clone(), history.clone(), false);
    full.observe_certificate(&future.header).unwrap();
    assert_eq!(full.head.height, 0);
    drop(full);
    let mut full = reopen(state, history.clone(), false);
    assert_eq!(full.head.height, 0);
    let mut other = future;
    other.header.header.inputs_digest = B256::repeat_byte(0xee);
    recertify(&mut other);
    assert_eq!(message(full.import(other)), "ChainConflict");
    assert_eq!(full.head.height, 0);
    assert_eq!(
        message(History::open(history, genesis().hash())),
        "ChainConflict"
    );
}

#[test]
fn failed_history_commit_never_attempts_engine_publication_and_retains_candidate() {
    for fault in [
        CommitFault::NotCommitted(StorageErrorKind::Io),
        CommitFault::IndeterminateNotApplied(StorageErrorKind::Io),
    ] {
        let state = Arc::new(MemoryVersionedStore::new());
        let history = Arc::new(FaultyStore::latching(MemoryVersionedStore::new()));
        let mut producer = create(state.clone(), history.clone(), true);
        producer.produce().unwrap();
        let pending = tx(2, 0, 1, vec![create_entity(10, b"pending")]);
        producer.submit(pending.clone()).unwrap();
        let old_head = producer.head;
        let before = state.stats();
        history.push_commit_fault(fault);
        assert!(producer.produce().is_err());
        assert_eq!(producer.head, old_head);
        assert_eq!(producer.engine.head().sequence(), 1);
        assert_eq!(state.stats().committed, before.committed);
        assert_eq!(producer.engine.writer_state(), WriterState::Prepared);
        assert_eq!(producer.committed_nonce(signer(2).address()).unwrap(), 0);
        assert_eq!(producer.pool.len(), 1);
        assert_eq!(message(producer.submit(pending.clone())), "CommitUncertain");
        assert_eq!(message(producer.produce()), "CommitUncertain");
        producer.retry_pending().unwrap();
        assert_eq!(producer.head.height, 2);
        assert!(producer.pool.is_empty());
        assert_eq!(producer.block(2).unwrap().inputs.txs, vec![pending]);
        assert_eq!(producer.committed_nonce(signer(2).address()).unwrap(), 1);
    }
}

#[test]
fn transient_history_preflight_read_failure_retries_the_identical_host_candidate() {
    for scope in [ReadScope::Head, ReadScope::AllObjects] {
        let state = Arc::new(MemoryVersionedStore::new());
        let history = Arc::new(FaultyStore::latching(MemoryVersionedStore::new()));
        let mut producer = create(state.clone(), history.clone(), true);
        producer.produce().unwrap();
        let pending = tx(2, 0, 1, vec![create_entity(10, b"exact candidate")]);
        producer.submit(pending.clone()).unwrap();
        let commits = state.stats().committed;
        history.fail_reads(scope, StorageErrorKind::Io, Some(1));
        assert_eq!(message(producer.produce()), "StorageUnavailable");
        assert_eq!(producer.head.height, 1);
        assert_eq!(producer.pool.len(), 1);
        assert_eq!(state.stats().committed, commits);
        producer.retry_pending().unwrap();
        assert_eq!(producer.head.height, 2);
        assert_eq!(producer.block(2).unwrap().inputs.txs, vec![pending]);
        assert_eq!(producer.committed_nonce(signer(2).address()).unwrap(), 1);
        assert_eq!(state.stats().committed, commits + 1);
        assert!(producer.pool.is_empty());
    }
}

#[test]
fn pending_engine_candidate_cannot_resume_after_authenticated_conflict() {
    let mut producer = memory(true);
    producer.produce().unwrap();
    let first = producer.block(1).unwrap();
    producer
        .submit(tx(2, 0, 1, vec![create_entity(10, b"candidate")]))
        .unwrap();
    producer.produce().unwrap();
    let state = Arc::new(FaultyStore::new(MemoryVersionedStore::new()));
    let history = Arc::new(MemoryVersionedStore::new());
    let mut full = create(state.clone(), history.clone(), false);
    full.import(first.clone()).unwrap();
    state.push_commit_fault(CommitFault::NotCommitted(StorageErrorKind::Io));
    assert_eq!(
        message(full.import(producer.block(2).unwrap())),
        "StorageUnavailable"
    );
    let commits = state.stats().committed;
    let mut other = first.header;
    other.header.state_root = B256::repeat_byte(0xee);
    other = SignedHeader::sign(&genesis(), other.header, &signer(1)).unwrap();
    assert_eq!(message(full.observe_certificate(&other)), "ChainConflict");
    assert_eq!(message(full.retry_pending()), "ChainConflict");
    assert_eq!(full.head.height, 1);
    assert_eq!(full.engine.head().sequence(), 1);
    assert_eq!(state.stats().committed, commits);
    assert_eq!(
        message(History::open(history, genesis().hash())),
        "ChainConflict"
    );
}

#[test]
fn observed_history_commit_remains_fenced_and_is_only_an_orphan_after_restart() {
    for fault in [
        CommitFault::IndeterminateApplied(StorageErrorKind::Io),
        CommitFault::AckLost,
    ] {
        let state = Arc::new(MemoryVersionedStore::new());
        let history = Arc::new(FaultyStore::latching(MemoryVersionedStore::new()));
        let mut producer = create(state.clone(), history.clone(), true);
        producer.produce().unwrap();
        producer
            .submit(tx(2, 0, 1, vec![create_entity(10, b"orphan")]))
            .unwrap();
        let before = state.stats().committed;
        history.push_commit_fault(fault);
        assert_eq!(message(producer.produce()), "CommitUncertain");
        assert_eq!(message(producer.retry_pending()), "CommitUncertain");
        assert_eq!(producer.head.height, 1);
        assert_eq!(producer.pool.len(), 1);
        assert_eq!(state.stats().committed, before);
        drop(producer);
        history.recover().unwrap();
        let producer = reopen(state, history, true);
        assert_eq!(producer.head.height, 1);
        assert_eq!(producer.committed_nonce(signer(2).address()).unwrap(), 0);
        assert_eq!(
            producer
                .request_status(signer(2).address(), [1; 32])
                .unwrap()["status"],
            "unknown"
        );
        assert!(producer.pool.is_empty());
        assert_eq!(message(producer.block(2)), "BlockNotSynced");
    }
}

#[test]
fn absent_engine_commit_retries_after_durable_history_without_consuming_nonce_twice() {
    for fault in [
        CommitFault::NotCommitted(StorageErrorKind::Io),
        CommitFault::IndeterminateNotApplied(StorageErrorKind::Io),
    ] {
        let state = Arc::new(FaultyStore::latching(MemoryVersionedStore::new()));
        let history = Arc::new(MemoryVersionedStore::new());
        let mut producer = create(state.clone(), history.clone(), true);
        producer.produce().unwrap();
        let pending = tx(2, 0, 1, vec![create_entity(10, b"retained before state")]);
        producer.submit(pending.clone()).unwrap();
        let before = history.stats().committed;
        state.push_commit_fault(fault);
        assert!(producer.produce().is_err());
        assert_eq!(history.stats().committed, before + 1);
        assert_eq!(producer.head.height, 1);
        assert_eq!(producer.engine.head().sequence(), 1);
        assert_eq!(producer.pool.len(), 1);
        assert_eq!(message(producer.produce()), "CommitUncertain");
        producer.retry_pending().unwrap();
        assert_eq!(history.stats().committed, before + 1);
        assert_eq!(producer.head.height, 2);
        assert_eq!(producer.committed_nonce(signer(2).address()).unwrap(), 1);
        assert!(producer.pool.is_empty());
        assert_eq!(producer.block(2).unwrap().inputs.txs, vec![pending]);
        drop(producer);
        assert_eq!(reopen(state, history, true).head.height, 2);
    }
}

#[test]
fn observed_engine_commit_stays_unacknowledged_until_a_fresh_restart() {
    for fault in [
        CommitFault::IndeterminateApplied(StorageErrorKind::Io),
        CommitFault::AckLost,
    ] {
        let state = Arc::new(FaultyStore::latching(MemoryVersionedStore::new()));
        let history = Arc::new(MemoryVersionedStore::new());
        let mut producer = create(state.clone(), history.clone(), true);
        producer.produce().unwrap();
        let pending = tx(2, 0, 1, vec![create_entity(10, b"applied uncertain")]);
        producer.submit(pending.clone()).unwrap();
        state.push_commit_fault(fault);
        assert_eq!(message(producer.produce()), "CommitUncertain");
        assert_eq!(message(producer.retry_pending()), "CommitUncertain");
        assert_eq!(producer.head.height, 1);
        assert_eq!(producer.engine.head().sequence(), 1);
        assert_eq!(producer.pool.len(), 1);
        drop(producer);
        state.recover().unwrap();
        let mut producer = reopen(state, history, true);
        assert_eq!(producer.head.height, 2);
        assert!(producer.pool.is_empty());
        assert_eq!(producer.committed_nonce(signer(2).address()).unwrap(), 1);
        assert_eq!(producer.submit(pending).unwrap()["status"], "included");
    }
}
