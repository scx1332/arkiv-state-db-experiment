//! Storage outcomes and trust boundaries for retained bodies and light headers.
use alloy_primitives::keccak256;
use arkiv_db::{
    B256, Budget, CellName, Engine, NamespaceId, Value, decode_exact, encode,
    proof::{EqQuery, EqRequest, SnapshotRef, VerifiedEq},
};
use arkiv_db_memory::MemoryVersionedStore;
use arkiv_db_storage::{
    testing::{CommitFault, FaultyStore, ReadScope},
    versioned::{
        CommitBatch, CommitOutcome, Head, ObjectReader, SpaceId, StorageErrorKind, Table,
        VersionedStore,
    },
};
use arkiv_node::signed::{
    crypto::Signer,
    history::{BODY_CAP, BodyRef, History},
    light::Light,
    protocol::{SignedGenesis, SignedHeader},
};
use std::sync::Arc;

mod signed_support;
fn signed_genesis() -> (SignedGenesis, Signer) {
    (
        signed_support::genesis(9003, 1000, [7; 16], 1, 100, &[1]),
        signed_support::signer(1),
    )
}
fn certificate(genesis: &SignedGenesis, signer: &Signer) -> SignedHeader {
    SignedHeader::sign(genesis, genesis.header(), signer).unwrap()
}
fn next(genesis: &SignedGenesis, signer: &Signer, parent: &SignedHeader, tag: u8) -> SignedHeader {
    let mut header = parent.header;
    header.height += 1;
    header.parent_hash = parent.header.hash();
    header.timestamp_ms = genesis.timestamp(header.height).unwrap();
    header.inputs_digest = B256::repeat_byte(tag);
    SignedHeader::sign(genesis, header, signer).unwrap()
}
fn conflict(genesis: &SignedGenesis, signer: &Signer, header: &SignedHeader) -> SignedHeader {
    let mut native = header.header;
    native.inputs_digest = B256::repeat_byte(0xee);
    SignedHeader::sign(genesis, native, signer).unwrap()
}
fn message<T>(result: arkiv_node::signed::Result<T>) -> &'static str {
    result.err().expect("expected rejection").0
}

#[test]
fn history_content_addressing_retains_orphans_and_retries_without_height_conflicts() {
    let store = Arc::new(MemoryVersionedStore::new());
    let genesis = B256::repeat_byte(9);
    let mut history = History::create(store.clone(), genesis).unwrap();
    let a = b"height 1, candidate A, retained payload sentinel";
    let b = b"height 1, candidate B";
    let expected = history.reference(a).unwrap();
    let first = history.persist(a).unwrap();
    assert_eq!(first, expected);
    let second = history.persist(b).unwrap();
    assert_ne!(first, second);
    assert_eq!(history.persist(a).unwrap(), first);
    assert_eq!(store.head().unwrap().unwrap().height, 2);
    // The adapter uses raw keccak of a framed object, not the protocol digest.
    let mut framed = b"ARKSIGNEDBODY1".to_vec();
    framed.extend_from_slice(genesis.as_slice());
    framed.extend_from_slice(&(a.len() as u32).to_be_bytes());
    framed.extend_from_slice(a);
    assert_eq!(first.id, keccak256(&framed));
    assert_eq!(
        store
            .read_object(SpaceId::Host, &first.id.0)
            .unwrap()
            .unwrap(),
        framed
    );
    assert!(
        store
            .read_record(Table::Blocks, &1u64.to_be_bytes())
            .unwrap()
            .is_none()
    );
    let reopened = History::open(store, genesis).unwrap();
    assert_eq!(reopened.read(&first).unwrap(), a);
    assert_eq!(reopened.read(&second).unwrap(), b);
}

#[test]
fn history_open_checks_head_body_but_reads_older_bodies_lazily() {
    let source = Arc::new(MemoryVersionedStore::new());
    let genesis = B256::repeat_byte(1);
    let mut history = History::create(source.clone(), genesis).unwrap();
    let old = history.persist(b"old body").unwrap();
    let latest = history.persist(b"head body").unwrap();
    let head = source.head().unwrap().unwrap();
    // Simulate an incomplete restore: all reference metadata is retained but
    // body objects can be missing. Opening must not scan old transaction data.
    for retain_head in [false, true] {
        let restored = Arc::new(MemoryVersionedStore::with_database_id(source.database_id()));
        let mut batch = CommitBatch::new(None, head);
        for key in [
            b"signed-history/profile".as_slice(),
            b"signed-history/identity",
        ] {
            batch.put_record(
                Table::Meta,
                key,
                source.read_record(Table::Meta, key).unwrap().unwrap(),
            );
        }
        for sequence in 0..=head.height {
            let key = sequence.to_be_bytes();
            batch.put_record(
                Table::Manifests,
                key,
                source.read_record(Table::Manifests, &key).unwrap().unwrap(),
            );
        }
        if retain_head {
            batch.put_object(
                SpaceId::Host,
                latest.id.0,
                source
                    .read_object(SpaceId::Host, &latest.id.0)
                    .unwrap()
                    .unwrap(),
            );
        }
        assert_eq!(restored.commit(&batch), CommitOutcome::Committed);
        if retain_head {
            let opened = History::open(restored.clone(), genesis).unwrap();
            assert_eq!(restored.stats().object_reads(), 1);
            assert_eq!(opened.read(&latest).unwrap(), b"head body");
            assert_eq!(message(opened.read(&old)), "HistoryUnavailable");
        } else {
            assert_eq!(
                message(History::open(restored, genesis)),
                "HistoryUnavailable"
            );
        }
    }
}

#[test]
fn history_redb_reopen_retains_the_same_content_addresses() {
    use arkiv_db_redb::{RedbConfig, RedbStore};
    let path = std::env::temp_dir().join(format!(
        "arkiv-signed-history-{}.redb",
        arkiv_db_storage::versioned::DatabaseId::random()
    ));
    let genesis = B256::repeat_byte(8);
    let (first, second) = {
        let store = Arc::new(RedbStore::create(&path, RedbConfig::default()).unwrap());
        let mut history = History::create(store, genesis).unwrap();
        (
            history.persist(b"candidate at height 1").unwrap(),
            history.persist(b"replacement at height 1").unwrap(),
        )
    };
    {
        let store = Arc::new(RedbStore::open(&path, RedbConfig::default()).unwrap());
        let mut history = History::open(store.clone(), genesis).unwrap();
        assert_eq!(history.read(&first).unwrap(), b"candidate at height 1");
        assert_eq!(history.read(&second).unwrap(), b"replacement at height 1");
        assert_eq!(history.persist(b"candidate at height 1").unwrap(), first);
        assert_eq!(store.head().unwrap().unwrap().height, 2);
    }
    std::fs::remove_file(path).unwrap();
}

#[test]
fn history_reference_codec_and_limits_are_exact_and_bounded() {
    let store = Arc::new(MemoryVersionedStore::new());
    let mut history = History::create(store.clone(), B256::repeat_byte(1)).unwrap();
    let bytes = vec![0xab; BODY_CAP];
    let reference = history.persist(&bytes).unwrap();
    assert_eq!(history.read(&reference).unwrap(), bytes);
    let encoded = encode(&reference);
    assert_eq!(encoded.len(), 36);
    assert_eq!(decode_exact::<BodyRef>(&encoded).unwrap(), reference);
    for end in 0..encoded.len() {
        assert!(decode_exact::<BodyRef>(&encoded[..end]).is_err());
    }
    let mut trailing = encoded.clone();
    trailing.push(0);
    assert!(decode_exact::<BodyRef>(&trailing).is_err());
    let invalid = BodyRef {
        id: reference.id,
        len: (BODY_CAP + 1) as u32,
    };
    assert!(decode_exact::<BodyRef>(&encode(&invalid)).is_err());
    assert_eq!(message(history.read(&invalid)), "StorageCorrupt");
    assert_eq!(
        message(history.persist(&vec![0; BODY_CAP + 1])),
        "CapacityReached"
    );
    assert_eq!(store.head().unwrap().unwrap().height, 1);
}

#[test]
fn history_rejects_wrong_profile_run_missing_object_length_hash_and_frame() {
    let store = Arc::new(MemoryVersionedStore::new());
    let genesis = B256::repeat_byte(1);
    let mut history = History::create(store.clone(), genesis).unwrap();
    assert_eq!(
        message(History::open(store.clone(), B256::repeat_byte(2))),
        "IdentityMismatch"
    );
    assert_eq!(
        message(History::open(
            Arc::new(MemoryVersionedStore::new()),
            genesis
        )),
        "IdentityMismatch"
    );
    let reference = history.persist(b"body").unwrap();
    assert_eq!(
        message(history.read(&BodyRef {
            id: B256::repeat_byte(8),
            len: 4
        })),
        "HistoryUnavailable"
    );
    assert_eq!(
        message(history.read(&BodyRef {
            len: 3,
            ..reference
        })),
        "StorageCorrupt"
    );
    assert_eq!(
        message(history.read(&BodyRef {
            len: 5,
            ..reference
        })),
        "StorageCorrupt"
    );
    // MemoryStore intentionally does not hash objects; inject a damaged medium
    // to establish that the history layer independently checks both hash/frame.
    for (id, frame) in [
        (B256::repeat_byte(3), vec![0; 4]),
        (keccak256([0u8; 49]), vec![0; 49]),
    ] {
        let old = store.head().unwrap().unwrap();
        let mut batch = CommitBatch::new(
            Some(old),
            Head {
                height: old.height + 1,
                hash: id.0,
            },
        );
        batch.put_object(SpaceId::Host, id.0, frame);
        assert_eq!(store.commit(&batch), CommitOutcome::Committed);
        assert_eq!(
            message(history.read(&BodyRef { id, len: 0 })),
            "StorageCorrupt"
        );
    }
}

#[test]
fn history_absent_pending_body_requires_explicit_exact_retry() {
    for fault in [
        CommitFault::NotCommitted(StorageErrorKind::Io),
        CommitFault::IndeterminateNotApplied(StorageErrorKind::Io),
    ] {
        let store = Arc::new(FaultyStore::latching(MemoryVersionedStore::new()));
        let mut history = History::create(store.clone(), B256::repeat_byte(1)).unwrap();
        let expected = history.reference(b"sole pending body").unwrap();
        store.push_commit_fault(fault);
        assert!(history.persist(b"sole pending body").is_err());
        assert_eq!(message(history.persist(b"replacement")), "CommitUncertain");
        assert_eq!(
            message(history.persist(b"sole pending body")),
            "CommitUncertain"
        );
        assert_eq!(message(history.read(&expected)), "CommitUncertain");
        assert_eq!(history.retry_pending().unwrap(), expected);
        assert_eq!(history.read(&expected).unwrap(), b"sole pending body");
        assert_eq!(store.head().unwrap().unwrap().height, 1);
        assert_eq!(message(history.retry_pending()), "NoPendingHistory");
    }
}

#[test]
fn history_observed_uncertain_body_never_becomes_an_acknowledged_reference() {
    for fault in [
        CommitFault::IndeterminateApplied(StorageErrorKind::Io),
        CommitFault::AckLost,
    ] {
        let store = Arc::new(FaultyStore::latching(MemoryVersionedStore::new()));
        let mut history = History::create(store.clone(), B256::repeat_byte(1)).unwrap();
        let reference = history.reference(b"unknown durability").unwrap();
        store.push_commit_fault(fault);
        assert_eq!(
            message(history.persist(b"unknown durability")),
            "CommitUncertain"
        );
        for _ in 0..2 {
            assert_eq!(message(history.retry_pending()), "CommitUncertain");
            assert_eq!(message(history.read(&reference)), "CommitUncertain");
            assert_eq!(
                message(history.persist(b"another block")),
                "CommitUncertain"
            );
        }
        // Storage observes the object, but this is not durable acknowledgement.
        assert!(
            store
                .read_object(SpaceId::Host, &reference.id.0)
                .unwrap()
                .is_some()
        );
        assert_eq!(store.head().unwrap().unwrap().height, 1);
    }
}

#[test]
fn history_failed_recovery_retains_the_identical_candidate() {
    let store = Arc::new(FaultyStore::latching(MemoryVersionedStore::new()));
    let mut history = History::create(store.clone(), B256::repeat_byte(1)).unwrap();
    let expected = history.reference(b"candidate").unwrap();
    store.push_commit_fault(CommitFault::IndeterminateNotApplied(StorageErrorKind::Io));
    assert!(history.persist(b"candidate").is_err());
    store.fail_next_recoveries(1, StorageErrorKind::Io);
    assert_eq!(message(history.retry_pending()), "StorageUnavailable");
    assert_eq!(message(history.persist(b"replacement")), "CommitUncertain");
    assert_eq!(history.retry_pending().unwrap(), expected);
}

#[test]
fn history_exact_retry_recovers_preflight_reads_and_rejects_pending_replacement() {
    for scope in [ReadScope::Head, ReadScope::AllObjects] {
        let store = Arc::new(FaultyStore::latching(MemoryVersionedStore::new()));
        let mut history = History::create(store.clone(), B256::repeat_byte(1)).unwrap();
        let reference = history.reference(b"exact candidate").unwrap();
        store.fail_reads(scope, StorageErrorKind::Io, Some(1));
        assert_eq!(
            message(history.persist(b"exact candidate")),
            "StorageUnavailable"
        );
        assert_eq!(message(history.retry_pending()), "NoPendingHistory");
        assert_eq!(history.retry_exact(b"exact candidate").unwrap(), reference);
        assert_eq!(history.read(&reference).unwrap(), b"exact candidate");
        assert_eq!(store.head().unwrap().unwrap().height, 1);
    }
    let store = Arc::new(FaultyStore::latching(MemoryVersionedStore::new()));
    let mut history = History::create(store.clone(), B256::repeat_byte(1)).unwrap();
    store.push_commit_fault(CommitFault::IndeterminateNotApplied(StorageErrorKind::Io));
    assert_eq!(
        message(history.persist(b"retained candidate")),
        "CommitUncertain"
    );
    assert_eq!(
        message(history.retry_exact(b"replacement")),
        "PendingHistoryMismatch"
    );
    let reference = history.retry_exact(b"retained candidate").unwrap();
    assert_eq!(history.read(&reference).unwrap(), b"retained candidate");
}

#[test]
fn history_conflict_during_pending_write_blocks_every_retry() {
    let store = Arc::new(FaultyStore::latching(MemoryVersionedStore::new()));
    let mut history = History::create(store.clone(), B256::repeat_byte(1)).unwrap();
    store.push_commit_fault(CommitFault::IndeterminateNotApplied(StorageErrorKind::Io));
    assert_eq!(message(history.persist(b"candidate")), "CommitUncertain");
    assert_eq!(
        message(history.freeze_conflict(b"caller-authenticated evidence")),
        "CommitUncertain"
    );
    assert_eq!(message(history.retry_pending()), "ChainConflict");
    assert_eq!(message(history.retry_exact(b"candidate")), "ChainConflict");
    assert_eq!(message(history.persist(b"candidate")), "ChainConflict");
    store.recover().unwrap();
    assert_eq!(store.head().unwrap().unwrap().height, 0);
}

#[test]
fn history_conflict_marker_has_an_independent_sequence_and_survives_reopen() {
    let store = Arc::new(MemoryVersionedStore::new());
    let (genesis, signer) = signed_genesis();
    let mut history = History::create(store.clone(), genesis.hash()).unwrap();
    history.persist(b"orphan one").unwrap();
    history.persist(b"orphan two").unwrap();
    let first = next(&genesis, &signer, &certificate(&genesis, &signer), 1);
    let second = conflict(&genesis, &signer, &first);
    first.authenticate(&genesis).unwrap();
    second.authenticate(&genesis).unwrap();
    history.freeze_conflict(&encode(&(first, second))).unwrap();
    assert_eq!(store.head().unwrap().unwrap().height, 3);
    assert_eq!(message(history.persist(b"blocked")), "ChainConflict");
    assert_eq!(
        message(History::open(store, genesis.hash())),
        "ChainConflict"
    );
}

#[test]
fn light_authenticates_chain_and_reopens_only_the_pinned_profile() {
    let store = Arc::new(MemoryVersionedStore::new());
    let (genesis, signer) = signed_genesis();
    let root = certificate(&genesis, &signer);
    let mut light = Light::create(store.clone(), genesis.clone(), root.clone()).unwrap();
    let first = next(&genesis, &signer, &root, 1);
    let second = next(&genesis, &signer, &first, 2);
    light
        .append(&[first.clone(), second.clone()], &second)
        .unwrap();
    light
        .append(&[first.clone(), second.clone()], &second)
        .unwrap();
    assert_eq!(light.head, second.header);
    assert_eq!(light.header(1).unwrap(), first);
    let reopened = Light::open(store.clone(), &genesis).unwrap();
    assert_eq!(reopened.head, second.header);
    assert_eq!(reopened.header(0).unwrap(), root);
    let mut other = genesis.clone();
    other.run_id = [8; 16];
    assert_eq!(
        message(Light::open(store.clone(), &other)),
        "IdentityMismatch"
    );
    assert_eq!(
        message(History::open(store, genesis.hash())),
        "IdentityMismatch"
    );
    let history_store = Arc::new(MemoryVersionedStore::new());
    History::create(history_store.clone(), genesis.hash()).unwrap();
    assert_eq!(
        message(Light::open(history_store, &genesis)),
        "IdentityMismatch"
    );
}

#[test]
fn light_rejects_invalid_signature_parent_time_and_gap_without_freezing() {
    let store = Arc::new(MemoryVersionedStore::new());
    let (genesis, signer) = signed_genesis();
    let root = certificate(&genesis, &signer);
    let mut light = Light::create(store.clone(), genesis.clone(), root.clone()).unwrap();
    let first = next(&genesis, &signer, &root, 1);
    let mut bad = first.clone();
    bad.signature = [0; 65];
    assert!(light.append(std::slice::from_ref(&bad), &bad).is_err());
    let mut parent = first.header;
    parent.parent_hash = B256::repeat_byte(4);
    let bad_parent = SignedHeader::sign(&genesis, parent, &signer).unwrap();
    let mut invalid_parent_light = Light::create(
        Arc::new(MemoryVersionedStore::new()),
        genesis.clone(),
        root.clone(),
    )
    .unwrap();
    assert_eq!(
        message(invalid_parent_light.append(std::slice::from_ref(&bad_parent), &bad_parent)),
        "InvalidHeader"
    );
    assert_eq!(invalid_parent_light.head, root.header);
    let mut bad_time = first.clone();
    bad_time.header.timestamp_ms += 1;
    bad_time.signature = signer.sign_digest(bad_time.intent_digest()).unwrap();
    assert_eq!(
        message(light.append(std::slice::from_ref(&bad_time), &bad_time)),
        "InvalidHeader"
    );
    let second = next(&genesis, &signer, &first, 2);
    assert_eq!(
        message(light.append(std::slice::from_ref(&second), &second)),
        "BlockNotSynced"
    );
    assert_eq!(light.head, root.header);
    assert_eq!(light.health, "following");
    light.append(&[first], &second).unwrap();
    assert_eq!(light.health, "stalled");
    light
        .append(std::slice::from_ref(&second), &second)
        .unwrap();
    assert_eq!(Light::open(store, &genesis).unwrap().head, second.header);
}

#[test]
fn light_persists_both_authentic_conflicting_certificates_and_refuses_reopen() {
    let store = Arc::new(MemoryVersionedStore::new());
    let (genesis, signer) = signed_genesis();
    let root = certificate(&genesis, &signer);
    let mut light = Light::create(store.clone(), genesis.clone(), root.clone()).unwrap();
    let first = next(&genesis, &signer, &root, 1);
    light.append(std::slice::from_ref(&first), &first).unwrap();
    let other = conflict(&genesis, &signer, &first);
    let mut forged = other.clone();
    forged.signature[0] ^= 1;
    assert!(light.append(&[], &forged).is_err());
    assert_eq!(light.header(1).unwrap(), first);
    assert_eq!(message(light.append(&[], &other)), "ChainConflict");
    let evidence = store
        .read_record(Table::Meta, b"signed-light/conflict")
        .unwrap()
        .unwrap();
    assert_eq!(
        decode_exact::<(SignedHeader, SignedHeader)>(&evidence).unwrap(),
        (first, other)
    );
    assert_eq!(message(light.header(0)), "ChainConflict");
    assert_eq!(message(Light::open(store, &genesis)), "ChainConflict");
}

#[test]
fn light_accepts_a_different_valid_signature_for_the_same_native_header() {
    use k256::{
        Scalar,
        ecdsa::{SigningKey, hazmat::SignPrimitive},
    };
    let store = Arc::new(MemoryVersionedStore::new());
    let (genesis, signer) = signed_genesis();
    let root = certificate(&genesis, &signer);
    let first = next(&genesis, &signer, &root, 1);
    // Explicit alternate ECDSA nonce, only for this fixed test key/vector.
    let key = SigningKey::from_slice(&[1; 32]).unwrap();
    let (signature, recovery) = key
        .as_nonzero_scalar()
        .as_ref()
        .try_sign_prehashed(Scalar::from(17u64), &first.intent_digest().0.into())
        .unwrap();
    let recovery = recovery.unwrap();
    assert!(!recovery.is_x_reduced());
    let mut alternate = first.clone();
    alternate.signature[..64].copy_from_slice(&signature.to_bytes());
    alternate.signature[64] = u8::from(recovery.is_y_odd());
    assert_ne!(first.signature, alternate.signature);
    alternate.authenticate(&genesis).unwrap();
    let mut light = Light::create(store.clone(), genesis.clone(), root).unwrap();
    light.append(std::slice::from_ref(&first), &first).unwrap();
    light
        .append(std::slice::from_ref(&alternate), &alternate)
        .unwrap();
    assert_eq!(light.head, first.header);
    assert!(
        store
            .read_record(Table::Meta, b"signed-light/conflict")
            .unwrap()
            .is_none()
    );
    assert_eq!(Light::open(store, &genesis).unwrap().head, first.header);
}

#[test]
fn light_detects_conflicts_inside_a_batch_and_between_batch_and_sample() {
    for include_conflict in [true, false] {
        let store = Arc::new(MemoryVersionedStore::new());
        let (genesis, signer) = signed_genesis();
        let root = certificate(&genesis, &signer);
        let first = next(&genesis, &signer, &root, 1);
        let other = conflict(&genesis, &signer, &first);
        let mut light = Light::create(store.clone(), genesis.clone(), root.clone()).unwrap();
        let batch = if include_conflict {
            vec![first, other.clone()]
        } else {
            vec![first]
        };
        assert_eq!(message(light.append(&batch, &other)), "ChainConflict");
        assert_eq!(light.head, root.header);
        assert_eq!(message(Light::open(store, &genesis)), "ChainConflict");
    }
}

#[test]
fn future_sampled_certificates_are_durable_evidence_but_not_accepted_headers() {
    let store = Arc::new(MemoryVersionedStore::new());
    let (genesis, signer) = signed_genesis();
    let root = certificate(&genesis, &signer);
    let mut future = root.header;
    future.height = 10;
    future.timestamp_ms = genesis.timestamp(10).unwrap();
    future.inputs_digest = B256::repeat_byte(1);
    let sampled = SignedHeader::sign(&genesis, future, &signer).unwrap();
    let mut light = Light::create(store.clone(), genesis.clone(), root.clone()).unwrap();
    light.append(&[], &sampled).unwrap();
    assert_eq!(light.head, root.header);
    assert_eq!(light.observed, 10);
    assert_eq!(message(light.header(10)), "BlockNotSynced");
    assert_eq!(store.head().unwrap().unwrap().height, 1);
    drop(light);
    let mut light = Light::open(store.clone(), &genesis).unwrap();
    assert_eq!(light.head, root.header);
    assert_eq!(light.observed, 10);
    assert_eq!(
        message(light.append(&[], &conflict(&genesis, &signer, &sampled))),
        "ChainConflict"
    );
    assert_eq!(message(Light::open(store, &genesis)), "ChainConflict");
}

#[test]
fn light_catalog_commit_sequence_is_independent_of_batched_chain_height() {
    let store = Arc::new(MemoryVersionedStore::new());
    let (genesis, signer) = signed_genesis();
    let root = certificate(&genesis, &signer);
    let first = next(&genesis, &signer, &root, 1);
    let second = next(&genesis, &signer, &first, 2);
    let third = next(&genesis, &signer, &second, 3);
    let mut light = Light::create(store.clone(), genesis.clone(), root.clone()).unwrap();
    light.append(&[], &third).unwrap();
    assert_eq!(light.head, root.header);
    light
        .append(&[first, second, third.clone()], &third)
        .unwrap();
    assert_eq!(light.head.height, 3);
    assert_eq!(store.head().unwrap().unwrap().height, 2);
    let reopened = Light::open(store, &genesis).unwrap();
    assert_eq!(reopened.head, third.header);
    assert_eq!(reopened.header(3).unwrap(), third);
}

#[test]
fn history_retains_future_observations_across_reopen_and_returns_conflicting_prior() {
    let store = Arc::new(MemoryVersionedStore::new());
    let (genesis, signer) = signed_genesis();
    let mut history = History::create(store.clone(), genesis.hash()).unwrap();
    let body = history.persist(b"head body").unwrap();
    let first = next(&genesis, &signer, &certificate(&genesis, &signer), 1);
    let second = next(&genesis, &signer, &first, 2);
    assert!(history.remember_header(&second).unwrap().is_none());
    assert!(history.remember_header(&second).unwrap().is_none());
    assert_eq!(store.head().unwrap().unwrap().height, 2);
    drop(history);
    let mut history = History::open(store.clone(), genesis.hash()).unwrap();
    assert_eq!(history.read(&body).unwrap(), b"head body");
    let other = conflict(&genesis, &signer, &second);
    let prior = history.remember_header(&other).unwrap().unwrap();
    assert_eq!(prior, second);
    prior.authenticate(&genesis).unwrap();
    other.authenticate(&genesis).unwrap();
    history.freeze_conflict(&encode(&(prior, other))).unwrap();
    assert_eq!(
        message(History::open(store, genesis.hash())),
        "ChainConflict"
    );
}

#[test]
fn uncertain_observation_commit_fences_history_body_publication() {
    let store = Arc::new(FaultyStore::latching(MemoryVersionedStore::new()));
    let (genesis, signer) = signed_genesis();
    let mut history = History::create(store.clone(), genesis.hash()).unwrap();
    let first = next(&genesis, &signer, &certificate(&genesis, &signer), 1);
    store.push_commit_fault(CommitFault::IndeterminateApplied(StorageErrorKind::Io));
    assert_eq!(message(history.remember_header(&first)), "CommitUncertain");
    assert_eq!(
        message(history.persist(b"no engine reference")),
        "CommitUncertain"
    );
    assert_eq!(
        message(history.retry_exact(b"no engine reference")),
        "CommitUncertain"
    );
}

#[test]
fn light_storage_failure_never_advances_the_acknowledged_head() {
    for fault in [
        CommitFault::NotCommitted(StorageErrorKind::Io),
        CommitFault::IndeterminateNotApplied(StorageErrorKind::Io),
        CommitFault::IndeterminateApplied(StorageErrorKind::Io),
    ] {
        let store = Arc::new(FaultyStore::latching(MemoryVersionedStore::new()));
        let (genesis, signer) = signed_genesis();
        let root = certificate(&genesis, &signer);
        let mut light = Light::create(store.clone(), genesis.clone(), root.clone()).unwrap();
        let first = next(&genesis, &signer, &root, 1);
        store.push_commit_fault(fault);
        assert!(light.append(std::slice::from_ref(&first), &first).is_err());
        assert_eq!(light.head, root.header);
        assert_eq!(message(light.header(0)), "CommitUncertain");
        assert_eq!(
            message(light.append(std::slice::from_ref(&first), &first)),
            "CommitUncertain"
        );
    }
}

#[test]
fn light_verifies_native_eq_bytes_only_for_the_authenticated_snapshot() {
    let (genesis, signer) = signed_genesis();
    let root = certificate(&genesis, &signer);
    let first = next(&genesis, &signer, &root, 1);
    let mut light =
        Light::create(Arc::new(MemoryVersionedStore::new()), genesis.clone(), root).unwrap();
    light.append(std::slice::from_ref(&first), &first).unwrap();
    let engine = Engine::with_limits(genesis.genesis.pricing.clone(), genesis.genesis.limits);
    let request = EqRequest {
        snapshot: SnapshotRef {
            genesis_hash: genesis.hash(),
            height: 1,
            block_hash: first.header.hash(),
            state_root: first.header.state_root,
        },
        query: EqQuery {
            namespace: NamespaceId::from_ordinal(1).unwrap(),
            attribute: CellName::new("group").unwrap(),
            value: Value::U64(1),
        },
        limit: 16,
        continuation: None,
    };
    let proof = engine
        .head_snapshot()
        .prove_eq(
            &request,
            arkiv_node::simulator::wire::proof_limits(),
            Budget::unlimited(),
        )
        .unwrap();
    assert!(matches!(
        light.verify(&request, None, &proof.bytes).unwrap(),
        VerifiedEq::NamespaceAbsent
    ));
    for part in 0..3 {
        let mut altered = request.clone();
        match part {
            0 => altered.snapshot.genesis_hash = genesis.genesis.hash(),
            1 => altered.snapshot.block_hash = B256::ZERO,
            _ => altered.snapshot.state_root = B256::ZERO,
        }
        assert_eq!(
            message(light.verify(&altered, None, &proof.bytes)),
            "InvalidRequest"
        );
    }
    let mut bad = proof.bytes.clone();
    bad.push(0);
    assert!(light.verify(&request, None, &bad).is_err());
    let other = conflict(&genesis, &signer, &first);
    assert_eq!(message(light.append(&[], &other)), "ChainConflict");
    assert_eq!(
        message(light.verify(&request, None, &proof.bytes)),
        "ChainConflict"
    );
}
