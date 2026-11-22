//! Signed execution, independent replay, retained metadata and publication faults.

use arkiv_db::{
    B256, Budget, Cell, CellName, OwnerId, Projection, RecordKey, Value,
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
    Expiry, HostFault, OpOutcome, TxOp, TxStatus,
    arkiv::{self, ArkivFault, ArkivOperation},
    signed::{
        history::History,
        host::{HeaderRecord, Host, Prefill},
        light::Light,
        protocol::{SignedBlock, SignedGenesis, SignedHeader, SignedTx, ValidTx},
    },
};
use std::sync::Arc;

mod signed_support;
use signed_support::{arkiv_ops, launch, memory_host, sign_tx, signer};

fn genesis() -> SignedGenesis {
    signed_support::genesis(9100, 1000, [7; 16], 1, 100, &[1, 2, 3])
}

fn create(
    state: Arc<dyn VersionedStore>,
    history: Arc<dyn VersionedStore>,
    producer: bool,
) -> Host {
    let launch = launch(&genesis(), signer(1), [3; 16], history, producer);
    Host::create(state, Options::default(), launch, Prefill::None).unwrap()
}

fn memory(producer: bool) -> Host {
    chain(&genesis(), producer)
}

fn chain(genesis: &SignedGenesis, producer: bool) -> Host {
    memory_host(genesis, signer(1), [3; 16], producer)
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
    let ops = arkiv_ops(operations);
    sign_tx(
        &genesis(),
        &signer(key),
        nonce,
        [request; 32],
        10_000_000,
        ops,
    )
}

fn tx_on(
    genesis: &SignedGenesis,
    key: u8,
    nonce: u64,
    operations: Vec<ArkivOperation>,
) -> SignedTx {
    let mut request_id = [key; 32];
    request_id[..8].copy_from_slice(&nonce.to_be_bytes());
    let ops = arkiv_ops(operations);
    sign_tx(genesis, &signer(key), nonce, request_id, 10_000_000, ops)
}

/// An entity with an indexed `group` and its data in an unindexed `body`.
fn create_entity(salt: u128, body: &[u8]) -> ArkivOperation {
    ArkivOperation::Create {
        salt,
        flags: 0,
        expiry: Expiry {
            expires_at: 0,
            min_lifetime: 1_000,
        },
        attributes: vec![
            (
                CellName::new("body").unwrap(),
                Cell::field(Value::bytes(body)),
            ),
            (
                CellName::new("group").unwrap(),
                Cell::attribute(Value::U64(1)),
            ),
        ],
    }
}

fn body(e: &arkiv::ArkivEntityProjection) -> Vec<u8> {
    match e.attribute("body").map(|c| &c.value) {
        Some(Value::Bytes(b)) => b.to_vec(),
        other => panic!("no bytes body: {other:?}"),
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
    result.err().expect("expected rejection").code.as_str()
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
    assert_eq!(producer.submit(create_tx).unwrap()["status"], "pending");
    assert_eq!(producer.pool.len(), 1);
    producer.produce().unwrap();
    relay(&producer, &mut full, &mut light);
    let key = entity_key(2, 0, 10);
    assert_eq!(body(&entity(&producer, &key).unwrap()), b"original payload");
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
                CellName::new("body").unwrap(),
                Cell::field(Value::bytes(b"new owner update")),
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
    assert_eq!(body(&entity(&producer, &key).unwrap()), b"new owner update");
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
        body(&arkiv::project(&page.rows[0]).unwrap()),
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
    producer.submit(failure).unwrap();
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
        body(&entity(&producer, &rolled_back_key).unwrap()),
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
    assert_eq!(producer.submit(included).unwrap()["status"], "included");
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
        body(&entity(&producer, &entity_key(2, 0, 10)).unwrap()),
        sentinel
    );
    // Decode the host packet exactly; engine pages hold entity state, so
    // searching them proves nothing.
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
fn admission_takes_only_a_validated_transaction() {
    let mut producer = memory(true);
    producer.produce().unwrap();
    let signed = tx(2, 0, 1, vec![create_entity(10, b"zero")]);
    let mut forged = signed.clone();
    forged.nonce += 1;
    assert_eq!(
        message(ValidTx::new(forged, &producer.genesis)),
        "InvalidSignature"
    );
    let valid = ValidTx::new(signed.clone(), &producer.genesis).unwrap();
    assert_eq!(producer.admit(valid).unwrap()["status"], "pending");
    producer.produce().unwrap();
    assert_eq!(producer.block(2).unwrap().inputs.txs, vec![signed]);
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
        body(&entity(&producer, &entity_key(2, 0, 11)).unwrap()),
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
        state,
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
    assert_ne!(full.health.as_str(), "chain-conflict");
    assert_eq!(message(full.import(other)), "ChainConflict");
    assert_eq!(full.health.as_str(), "chain-conflict");
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

/// A create that asks to expire at exactly `height`.
fn create_until(salt: u128, height: u64) -> ArkivOperation {
    ArkivOperation::Create {
        salt,
        flags: 0,
        expiry: Expiry {
            expires_at: height,
            min_lifetime: 0,
        },
        attributes: vec![(
            CellName::new("group").unwrap(),
            Cell::attribute(Value::U64(1)),
        )],
    }
}

fn expiry_total(host: &Host, height: u64) -> u64 {
    host.engine
        .head_snapshot()
        .try_host_get(&arkiv_node::exec::total_key(height))
        .unwrap()
        .map_or(0, |bytes| u64::from_be_bytes(bytes.try_into().unwrap()))
}

/// The expiry height each `EntityCreated`/`ExpiryExtended` event of a block
/// reports, by entity key.
fn logged_expiries(host: &Host, height: u64) -> Vec<(String, String, u64)> {
    let created = arkiv_node::text::hex(
        alloy_primitives::keccak256(b"EntityCreated(bytes32,address,uint64,uint8)").as_slice(),
    );
    arkiv_node::signed::rpc::block_logs(host, height)
        .unwrap()
        .iter()
        .filter(|log| log["data"].as_str().unwrap().len() >= 66)
        .map(|log| {
            let data = log["data"].as_str().unwrap();
            (
                if log["topics"][0] == created.as_str() {
                    "created".to_owned()
                } else {
                    "extended".to_owned()
                },
                log["topics"][1].as_str().unwrap().to_owned(),
                u64::from_str_radix(&data[2 + 48..66], 16).unwrap(),
            )
        })
        .collect()
}

/// A block holds at most 128 operations and a height at most 128 expiring
/// entities (the default): 8 transactions of 16 creates sharing one lifetime
/// all commit. Further creates and an extension aimed at the full height
/// take the following heights in execution order, events report the height
/// each entity was given, a full follower re-executes the same heights, a
/// light node verifies them by proof, and the sweep purges every entity in
/// the block of its recorded height.
#[test]
fn creates_sharing_a_lifetime_fill_a_height_and_spill_to_the_next() {
    const E: u64 = 12;
    let genesis = genesis();
    assert_eq!(genesis.genesis.params.expiry_per_block, 128);
    assert_eq!(genesis.genesis.params.max_ops_per_block, 128);
    let mut producer = memory(true);
    let mut full = memory(false);
    let mut light = Light::create(
        Arc::new(MemoryVersionedStore::new()),
        genesis.clone(),
        producer.certificate(0).unwrap(),
    )
    .unwrap();
    producer.produce().unwrap();
    relay(&producer, &mut full, &mut light);
    // Blocks 2 and 3: 8 transactions of 16 creates each, all asking for E.
    let mut keys = Vec::new();
    for block in 0..2u64 {
        for t in 0..8u64 {
            let ops = (0..16u64)
                .map(|i| create_until(u128::from(i), E))
                .collect::<Vec<_>>();
            producer
                .submit(tx_on(&genesis, 2, block * 8 + t, ops))
                .unwrap();
            for i in 0..16u64 {
                let nonce = block * 128 + t * 16 + i;
                keys.push(entity_key(2, nonce, u128::from(i)));
            }
        }
        producer.produce().unwrap();
        relay(&producer, &mut full, &mut light);
        let produced = producer.block(2 + block).unwrap();
        assert_eq!(produced.inputs.txs.len(), 8);
        assert!(
            produced
                .outcomes
                .txs
                .iter()
                .all(|tx| tx.status == TxStatus::Committed && tx.ops.len() == 16)
        );
    }
    // The first 128 got the height they asked for, the next 128 the one after.
    for (index, key) in keys.iter().enumerate() {
        let expected = if index < 128 { E } else { E + 1 };
        assert_eq!(entity(&producer, key).unwrap().expires_at, expected);
        assert_eq!(entity(&full, key).unwrap().expires_at, expected);
    }
    assert_eq!(
        (expiry_total(&producer, E), expiry_total(&producer, E + 1)),
        (128, 128)
    );
    // Rent is for the lifetime asked: block 3's creates asked for one block
    // less than block 2's and pay less, although they live as long.
    let rent = |height: u64| producer.block(height).unwrap().outcomes.txs[0].rent_wei;
    assert!(rent(3) < rent(2) && !rent(3).is_zero());
    // Block 4: a 257th create goes two heights on, and so does an extension
    // of an entity created in the same transaction onto the full height.
    producer
        .submit(tx_on(&genesis, 2, 16, vec![create_until(500, E)]))
        .unwrap();
    producer
        .submit(tx_on(
            &genesis,
            3,
            0,
            vec![
                create_until(501, E - 1),
                ArkivOperation::ExtendExpiry {
                    key: entity_key(3, 0, 501),
                    expiry: Expiry {
                        expires_at: E,
                        min_lifetime: 0,
                    },
                },
            ],
        ))
        .unwrap();
    producer.produce().unwrap();
    relay(&producer, &mut full, &mut light);
    let (late, extended) = (entity_key(2, 256, 500), entity_key(3, 0, 501));
    for key in [&late, &extended] {
        assert_eq!(entity(&producer, key).unwrap().expires_at, E + 2);
        assert_eq!(entity(&full, key).unwrap().expires_at, E + 2);
    }
    assert_eq!(
        (
            expiry_total(&producer, E - 1),
            expiry_total(&producer, E + 2)
        ),
        (0, 2)
    );
    // Events report the recorded heights, on the producer and the follower.
    for host in [&producer, &full] {
        for (height, expected) in [(2, E), (3, E + 1)] {
            let logs = logged_expiries(host, height);
            assert_eq!(logs.len(), 128);
            assert!(logs.iter().all(|(_, _, expiry)| *expiry == expected));
        }
        let key_hex = |key: &RecordKey| arkiv_node::text::hex(key.as_bytes());
        assert_eq!(
            logged_expiries(host, 4),
            vec![
                ("created".to_owned(), key_hex(&late), E + 2),
                ("created".to_owned(), key_hex(&extended), E - 1),
                ("extended".to_owned(), key_hex(&extended), E + 2),
            ]
        );
    }
    // A light node verifies the spilled height by proof against the header.
    let request = EqRequest {
        snapshot: SnapshotRef {
            genesis_hash: genesis.hash(),
            height: producer.head.height,
            block_hash: producer.head.hash(),
            state_root: producer.head.state_root,
        },
        query: EqQuery {
            namespace: arkiv::entity_namespace(),
            attribute: CellName::new("$expiresAt").unwrap(),
            value: Value::U64(E + 2),
        },
        limit: 16,
        continuation: None,
    };
    let proof = producer.proof(&request).unwrap();
    assert_eq!(full.proof(&request).unwrap(), proof);
    let VerifiedEq::Page(page) = light.verify(&request, None, &proof).unwrap() else {
        panic!("namespace missing")
    };
    let mut proven: Vec<_> = page
        .rows
        .iter()
        .map(|row| arkiv::project(row).unwrap().key)
        .collect();
    proven.sort();
    let mut expected = [late.as_bytes().to_vec(), extended.as_bytes().to_vec()];
    expected.sort();
    assert_eq!(
        proven.iter().map(|k| k.to_vec()).collect::<Vec<_>>(),
        expected
    );
    // Each entity is purged in the block of its recorded height. Block E is
    // the largest a chain produces: 128 deletions beside 128 operations.
    while producer.head.height < E + 2 {
        let next = producer.head.height + 1;
        if next == E {
            for t in 0..8u64 {
                let ops = (0..16u64)
                    .map(|i| create_until(1_000 + u128::from(i), E + 100))
                    .collect::<Vec<_>>();
                producer.submit(tx_on(&genesis, 2, 17 + t, ops)).unwrap();
            }
        }
        producer.produce().unwrap();
        relay(&producer, &mut full, &mut light);
        let block = producer.block(next).unwrap();
        let expected = match next {
            E => 128,
            n if n == E + 1 => 128,
            n if n == E + 2 => 2,
            _ => 0,
        };
        assert_eq!(block.outcomes.expiry.len(), expected, "block {next}");
        if next == E {
            assert_eq!(block.outcomes.txs.len(), 8);
            assert!(
                block
                    .outcomes
                    .txs
                    .iter()
                    .all(|tx| tx.status == TxStatus::Committed)
            );
            // The outcomes of the largest block stay far below the 64 KiB
            // the body cap reserves for them.
            let outcomes = encode(&block.outcomes).len();
            assert!(outcomes < 16 * 1024, "{outcomes} outcome bytes");
        }
        assert_eq!(expiry_total(&producer, next), 0);
    }
    for key in keys.iter().chain([&late, &extended]) {
        assert!(entity(&producer, key).is_none());
        assert!(entity(&full, key).is_none());
    }
}

/// The search for a free height is bounded: with one expiry per height, a
/// create takes the height asked or one of the 64 after it, and the next one
/// fails with `ExpiryCapacityFull`, rolling its transaction back.
#[test]
fn a_spill_stops_after_the_search_window() {
    const E: u64 = 100;
    let window = arkiv_node::exec::EXPIRY_SPILL_WINDOW;
    assert_eq!(window, 64);
    let mut native = arkiv_node::signed::protocol::default_native_genesis(9100, 1000, 100);
    native.params.expiry_per_block = 1;
    let genesis = signed_support::genesis_from(native, [7; 16], 1, 100, &[1, 2, 3]);
    let mut producer = chain(&genesis, true);
    let mut full = chain(&genesis, false);
    producer.produce().unwrap();
    full.import(producer.block(1).unwrap()).unwrap();
    // 65 creates fill E ..= E + 64; the 66th finds no room.
    let mut salt = 0u128;
    let mut batch = |n: usize| {
        (0..n)
            .map(|_| {
                salt += 1;
                create_until(salt, E)
            })
            .collect::<Vec<_>>()
    };
    for nonce in 0..4 {
        producer
            .submit(tx_on(&genesis, 2, nonce, batch(16)))
            .unwrap();
    }
    producer.submit(tx_on(&genesis, 2, 4, batch(1))).unwrap();
    producer.submit(tx_on(&genesis, 2, 5, batch(2))).unwrap();
    producer.produce().unwrap();
    full.import(producer.block(2).unwrap()).unwrap();
    assert_eq!(full.head, producer.head);
    let block = producer.block(2).unwrap();
    assert!(
        block.outcomes.txs[..5]
            .iter()
            .all(|tx| tx.status == TxStatus::Committed)
    );
    for offset in 0..=window {
        assert_eq!(expiry_total(&producer, E + offset), 1);
        let key = RecordKey::new(&arkiv::derive_entity_key(
            9100,
            signer(2).address().as_bytes(),
            offset,
            u128::from(offset) + 1,
        ))
        .unwrap();
        assert_eq!(entity(&producer, &key).unwrap().expires_at, E + offset);
    }
    let failed = &block.outcomes.txs[5];
    assert_eq!(failed.status, TxStatus::Failed { op_index: 0 });
    assert_eq!(
        failed.ops[0],
        OpOutcome::HostRejected {
            fault: HostFault::ExpiryCapacityFull
        }
    );
    assert!(failed.rent_wei.is_zero());
    assert_eq!(expiry_total(&producer, E + window + 1), 0);
    assert_eq!(creation_nonce(&producer, signer(2).address()), 65);
}

/// Measures the expiry sweep: the time `produce` takes for the block in
/// which a full height of entities expires, on durable redb stores, commit
/// included, beside the same chain's empty blocks. Opt-in:
///
/// ```text
/// ARKIV_BENCH_DIR=/path/on/a/real/disk cargo test --release -p arkiv-node \
///     --test signed_host expiry_sweep_timing -- --ignored --nocapture
/// ```
///
/// "typical" is the SDK benchmark's entity (4 indexed attributes and a
/// 256-byte body); "worst" has 31 indexed 128-byte strings and one 128 KiB
/// `bytes` value, the most an entity may hold; "indexed" is the worst entity
/// with a 256-byte body, to tell the index work from the value's.
#[test]
#[ignore = "timing experiment: see the doc comment"]
fn expiry_sweep_timing() {
    use arkiv_db_redb::{RedbConfig, RedbStore};
    use std::time::Instant;
    const E: u64 = 60;
    let root = std::env::var_os("ARKIV_BENCH_DIR")
        .map_or_else(std::env::temp_dir, std::path::PathBuf::from)
        .join(format!("arkiv-expiry-sweep-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let entity = |kind: &str, n: u64| {
        let text = |i: u64, len: usize| {
            let mut s = format!("{n:08}-{i:02}-");
            while s.len() < len {
                s.push('x');
            }
            Value::Str(s.into())
        };
        let mut attributes = Vec::new();
        let (strings, len, body) = match kind {
            "worst" => (31, 128, 128 * 1024),
            "indexed" => (31, 128, 256),
            _ => (4, 24, 256),
        };
        for i in 0..strings {
            attributes.push((
                CellName::new(&format!("attr{i:02}")).unwrap(),
                Cell::attribute(text(i, len)),
            ));
        }
        attributes.push((
            CellName::new("body").unwrap(),
            Cell::field(Value::bytes(&vec![n as u8; body])),
        ));
        ArkivOperation::Create {
            salt: u128::from(n),
            flags: 0,
            expiry: Expiry {
                expires_at: E,
                min_lifetime: 0,
            },
            attributes,
        }
    };
    println!("kind     expiring  sweep block ms  empty block ms  sweep units  body bytes");
    for kind in ["typical", "indexed", "worst"] {
        for capacity in [16u32, 32, 64, 128] {
            let dir = root.join(format!("{kind}-{capacity}"));
            std::fs::create_dir_all(&dir).unwrap();
            let mut native = arkiv_node::signed::protocol::default_native_genesis(9100, 1000, 250);
            native.params.expiry_per_block = capacity;
            let genesis = signed_support::genesis_from(native, [7; 16], 1, 250, &[1, 2]);
            let store = |name: &str| -> Arc<dyn VersionedStore> {
                Arc::new(RedbStore::create(dir.join(name), RedbConfig::default()).unwrap())
            };
            let launch = launch(&genesis, signer(1), [3; 16], store("history.redb"), true);
            let mut producer = Host::create(
                store("node.redb"),
                Options::default(),
                launch,
                Prefill::None,
            )
            .unwrap();
            producer.produce().unwrap();
            // Creation: 16 typical entities per transaction, or one worst
            // entity per transaction and four per block (the input cap).
            let (per_tx, per_block) = match kind {
                "worst" => (1, 4),
                "indexed" => (4, 16),
                _ => (16, 128),
            };
            let (mut made, mut nonce) = (0u64, 0u64);
            while made < u64::from(capacity) {
                let mut in_block = 0;
                while in_block < per_block && made < u64::from(capacity) {
                    let count = per_tx.min(u64::from(capacity) - made);
                    let ops = (made..made + count).map(|n| entity(kind, n)).collect();
                    let mut request_id = [9; 32];
                    request_id[..8].copy_from_slice(&nonce.to_be_bytes());
                    let budget = if kind == "typical" {
                        10_000_000
                    } else {
                        20_000_000
                    };
                    let tx = sign_tx(
                        &genesis,
                        &signer(2),
                        nonce,
                        request_id,
                        budget,
                        arkiv_ops(ops),
                    );
                    producer.submit(tx).unwrap();
                    nonce += 1;
                    made += count;
                    in_block += count;
                }
                producer.produce().unwrap();
                let block = producer.block(producer.head.height).unwrap();
                assert!(!block.inputs.txs.is_empty() && producer.pool.is_empty());
                assert!(
                    block
                        .outcomes
                        .txs
                        .iter()
                        .all(|tx| tx.status == TxStatus::Committed),
                    "{:?}",
                    block.outcomes.txs
                );
            }
            assert_eq!(expiry_total(&producer, E), u64::from(capacity));
            assert!(producer.head.height < E - 12, "{}", producer.head.height);
            let mut empty = Vec::new();
            while producer.head.height < E - 1 {
                let start = Instant::now();
                producer.produce().unwrap();
                empty.push(start.elapsed());
            }
            let start = Instant::now();
            producer.produce().unwrap();
            let sweep = start.elapsed();
            let block = producer.block(E).unwrap();
            assert_eq!(block.outcomes.expiry.len(), capacity as usize);
            assert_eq!(expiry_total(&producer, E), 0);
            empty.sort();
            println!(
                "{:8} {:8}  {:14.2}  {:14.2}  {:11}  {:10}",
                kind,
                capacity,
                sweep.as_secs_f64() * 1e3,
                empty[empty.len() / 2].as_secs_f64() * 1e3,
                block.outcomes.spent,
                encode(&block).len()
            );
        }
    }
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn block_timestamps_follow_the_wall_clock_and_followers_check_their_spacing() {
    // Genesis start 1,000 ms, block period 100 ms.
    let g = genesis();
    let mut producer = memory(true);
    // A clock before the schedule: the floor, start + height × period.
    producer.set_clock(|| 0);
    producer.produce().unwrap();
    assert_eq!(producer.head.timestamp_ms, g.earliest_timestamp(1).unwrap());
    assert_eq!(producer.head.timestamp_ms, 1_100);
    // After downtime: the block takes the wall clock, not the schedule.
    producer.set_clock(|| 5_000_000);
    producer.produce().unwrap();
    assert_eq!(producer.head.timestamp_ms, 5_000_000);
    // A clock behind the parent: one period after the parent.
    producer.set_clock(|| 4_000_000);
    producer.produce().unwrap();
    assert_eq!(producer.head.timestamp_ms, 5_000_100);

    let mut full = memory(false);
    let mut light = Light::create(
        Arc::new(MemoryVersionedStore::new()),
        genesis(),
        producer.certificate(0).unwrap(),
    )
    .unwrap();
    for height in 1..=2 {
        let block = producer.block(height).unwrap();
        full.import(block.clone()).unwrap();
        light
            .append(std::slice::from_ref(&block.header), &block.header)
            .unwrap();
    }
    // Block 3 restamped closer than one period to its parent, but well
    // after the floor and authentically signed: refused by both. (A
    // separate follower: having seen this authentic header, `full` would
    // treat the real block 3 as equivocation.)
    let original = producer.block(3).unwrap();
    let mut early = original.clone();
    early.header.header.timestamp_ms = 5_000_099;
    recertify(&mut early);
    let mut other = memory(false);
    for height in 1..=2 {
        other.import(producer.block(height).unwrap()).unwrap();
    }
    assert_eq!(message(other.import(early.clone())), "InvalidHeader");
    assert_eq!(other.head.height, 2);
    assert_eq!(
        message(light.append(std::slice::from_ref(&early.header), &early.header)),
        "InvalidHeader"
    );
    // Before the floor a header is refused even without its parent (signing
    // authenticates the result, so it cannot even be signed).
    let mut floor = original.header.header;
    floor.timestamp_ms = g.earliest_timestamp(3).unwrap() - 1;
    assert_eq!(
        message(SignedHeader::sign(&g, floor, &signer(1))),
        "InvalidHeader"
    );
    // The block as produced imports.
    full.import(original.clone()).unwrap();
    assert_eq!(full.head, producer.head);
}
