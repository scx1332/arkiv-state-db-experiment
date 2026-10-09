//! Nonce ordering, byte accounting and capacity invariants of signed admission.
use alloy_primitives::U256;
use arkiv_db::{NamespaceName, OwnerId, encode};
use arkiv_node::{
    TxEnvelope, TxOp,
    accounts::AccountConfig,
    gas,
    signed::{Error, crypto::Signer, pool::*, protocol::*},
};

fn signer(n: u16) -> Signer {
    let mut key = [0; 32];
    key[30..].copy_from_slice(&n.to_be_bytes());
    Signer::from_bytes(key).unwrap()
}
fn genesis() -> SignedGenesis {
    SignedGenesis::new(
        default_native_genesis(7, 1_000),
        [0x42; 16],
        *signer(1).address().as_bytes(),
        250,
        AccountConfig::new(vec![(
            signer(1).address(),
            U256::from(1_000_000_000_000_000_000u64),
        )])
        .unwrap(),
    )
    .unwrap()
}
/// Admission against a balance that covers every reservation here, so these
/// tests isolate nonce ordering, byte accounting and capacity.
trait Admit {
    fn admit(&mut self, tx: SignedTx, next: u64) -> Result<Admission, Error>;
}
impl Admit for Pool {
    fn admit(&mut self, tx: SignedTx, next: u64) -> Result<Admission, Error> {
        self.insert_funded(tx, next, U256::from(10u64).pow(U256::from(30u64)))
    }
}
/// A namespace creation with a budget of `budget` intrinsic-gas units.
fn tx(g: &SignedGenesis, who: u16, nonce: u64, budget: u64) -> SignedTx {
    let s = signer(who);
    let mut request_id = [0; 32];
    request_id[24..].copy_from_slice(&nonce.to_be_bytes());
    SignedTx::sign(
        g,
        nonce,
        TxEnvelope {
            chain_id: 7,
            actor: s.address(),
            request_id,
            tx_budget: budget * gas::TX_INTRINSIC_GAS,
            ops: vec![TxOp::CreateNamespace {
                name: NamespaceName::new("entities").unwrap(),
            }],
        },
        &s,
    )
    .unwrap()
}
fn nonces(inputs: &SignedInputs) -> Vec<(OwnerId, u64)> {
    inputs.txs.iter().map(|t| (t.tx.actor, t.nonce)).collect()
}

#[test]
fn duplicates_conflicts_and_failed_admission_preserve_pending() {
    let g = genesis();
    let mut pool = Pool::new(&g).unwrap();
    let first = tx(&g, 1, 0, 10);
    assert_eq!(pool.admit(first.clone(), 0), Ok(Admission::Inserted));
    let bytes = pool.bytes();
    assert_eq!(pool.admit(first.clone(), 0), Ok(Admission::Duplicate));
    assert_eq!(pool.bytes(), bytes);
    assert_eq!(pool.len(), 1);
    let mut changed = first.tx.clone();
    changed.request_id[0] = 1;
    let changed = SignedTx::sign(&g, 0, changed, &signer(1)).unwrap();
    assert_eq!(pool.admit(changed, 0), Err(Error("NonceConflict")));
    let changed = SignedTx::sign(&g, 1, first.tx.clone(), &signer(1)).unwrap();
    assert_eq!(pool.admit(changed, 0), Err(Error("RequestConflict")));
    assert_eq!(
        pool.pending(first.tx.actor, first.tx.request_id),
        Some(&first)
    );
    assert_eq!(pool.len(), 1);
    assert_eq!(pool.bytes(), bytes);
    // Capacity is checked only after exact pending duplicates.
    let mut full = Pool::with_limits(
        &g,
        PoolLimits {
            max_entries: 1,
            ..PoolLimits::default()
        },
    )
    .unwrap();
    full.admit(first.clone(), 0).unwrap();
    assert_eq!(full.admit(tx(&g, 2, 0, 10), 0), Err(Error("PoolFull")));
    assert_eq!(full.admit(first, 0), Ok(Admission::Duplicate));
}

#[test]
fn gaps_become_ready_only_after_the_missing_nonce_and_order_by_arrival() {
    let g = genesis();
    let mut pool = Pool::new(&g).unwrap();
    let a = signer(1).address();
    let b = signer(2).address();
    pool.admit(tx(&g, 1, 1, 10), 0).unwrap();
    pool.admit(tx(&g, 2, 0, 10), 0).unwrap();
    assert_eq!(nonces(&pool.select(|_| Ok(0)).unwrap()), vec![(b, 0)]);
    assert_eq!(pool.contiguous_next(a, 0), 0);
    pool.admit(tx(&g, 1, 0, 10), 0).unwrap();
    pool.admit(tx(&g, 2, 1, 10), 0).unwrap();
    assert_eq!(
        nonces(&pool.select(|_| Ok(0)).unwrap()),
        vec![(b, 0), (a, 0), (a, 1), (b, 1)]
    );
    assert_eq!(pool.contiguous_next(a, 0), 2);
    assert_eq!(pool.len(), 4); // repeated construction is non-destructive
    let one = pool.select(|_| Ok(0)).unwrap();
    let two = pool.select(|_| Ok(0)).unwrap();
    assert_eq!(one, two);
    pool.remove_committed(&one);
    assert!(pool.is_empty());
    assert_eq!(pool.bytes(), 0);
    assert_eq!(pool.sender_count(), 0);
}

#[test]
fn no_fit_defers_a_sender_and_its_descendants_but_keeps_other_senders() {
    let mut g = genesis();
    g.genesis.params.user_budget = 100 * gas::TX_INTRINSIC_GAS;
    let mut pool = Pool::new(&g).unwrap();
    pool.admit(tx(&g, 1, 0, 80), 0).unwrap();
    pool.admit(tx(&g, 2, 0, 50), 0).unwrap();
    pool.admit(tx(&g, 2, 1, 1), 0).unwrap();
    pool.admit(tx(&g, 3, 0, 10), 0).unwrap();
    let chosen = pool.select(|_| Ok(0)).unwrap();
    assert_eq!(
        nonces(&chosen),
        vec![(signer(1).address(), 0), (signer(3).address(), 0)]
    );
    assert_eq!(pool.len(), 4);
    pool.remove_committed(&chosen);
    let next = pool.select(|_| Ok(0)).unwrap();
    assert_eq!(
        nonces(&next),
        vec![(signer(2).address(), 0), (signer(2).address(), 1)]
    );
}

#[test]
fn signed_wrappers_count_toward_block_bytes() {
    let mut g = genesis();
    let length = encode(&tx(&g, 1, 0, 10)).len();
    g.genesis.params.max_tx_bytes = 250;
    g.genesis.params.max_block_input_bytes = (length * 2 + 7) as u32;
    g.limits.max_tx_bytes = 250 + SIGNED_TX_OVERHEAD;
    g.limits.max_input_bytes = (length * 2 + 7) as u32;
    g.validate().unwrap();
    let mut pool = Pool::new(&g).unwrap();
    pool.admit(tx(&g, 1, 0, 10), 0).unwrap();
    pool.admit(tx(&g, 1, 1, 10), 0).unwrap();
    let selected = pool.select(|_| Ok(0)).unwrap();
    assert_eq!(selected.txs.len(), 1);
    let both = SignedInputs {
        txs: vec![tx(&g, 1, 0, 10), tx(&g, 1, 1, 10)],
    };
    assert!(encode(&both.native()).len() < g.genesis.params.max_block_input_bytes as usize);
    assert_eq!(encode(&both).len(), g.limits.max_input_bytes as usize + 1);
    assert!(both.validate(&g, 2).is_err());
    g.limits.max_input_bytes += 1;
    g.genesis.params.max_block_input_bytes += 1;
    let mut exact = Pool::new(&g).unwrap();
    exact.admit(tx(&g, 1, 0, 10), 0).unwrap();
    exact.admit(tx(&g, 1, 1, 10), 0).unwrap();
    assert_eq!(exact.select(|_| Ok(0)).unwrap().txs.len(), 2);
}

#[test]
fn byte_sender_entry_and_per_sender_limits_are_inclusive_and_do_not_evict() {
    let g = genesis();
    let first = tx(&g, 1, 0, 10);
    let bytes = encode(&first).len();
    let cases = [
        PoolLimits {
            max_entries: 1,
            ..PoolLimits::default()
        },
        PoolLimits {
            max_bytes: bytes,
            ..PoolLimits::default()
        },
        PoolLimits {
            max_senders: 1,
            ..PoolLimits::default()
        },
    ];
    for limits in cases {
        let mut pool = Pool::with_limits(&g, limits).unwrap();
        pool.admit(first.clone(), 0).unwrap();
        assert_eq!(pool.admit(tx(&g, 2, 0, 10), 0), Err(Error("PoolFull")));
        assert_eq!(pool.len(), 1);
        assert_eq!(pool.bytes(), bytes);
        assert_eq!(
            pool.pending(first.tx.actor, first.tx.request_id),
            Some(&first)
        );
    }
    let mut pool = Pool::with_limits(
        &g,
        PoolLimits {
            max_per_sender: 2,
            ..PoolLimits::default()
        },
    )
    .unwrap();
    pool.admit(first.clone(), 0).unwrap();
    pool.admit(tx(&g, 1, 1, 10), 0).unwrap();
    assert_eq!(pool.admit(tx(&g, 1, 2, 10), 0), Err(Error("PoolFull")));
    pool.admit(tx(&g, 2, 0, 10), 0).unwrap();
    assert_eq!(pool.len(), 3);
    let mut small = Pool::with_limits(
        &g,
        PoolLimits {
            max_bytes: bytes - 1,
            ..PoolLimits::default()
        },
    )
    .unwrap();
    assert_eq!(small.admit(first, 0), Err(Error("PoolFull")));
    assert!(small.is_empty());
    assert_eq!(
        PoolLimits::default(),
        PoolLimits {
            max_entries: 1024,
            max_bytes: 16 * 1024 * 1024,
            max_senders: 256,
            max_per_sender: 16
        }
    );
    for limits in [
        PoolLimits {
            max_entries: 1025,
            ..PoolLimits::default()
        },
        PoolLimits {
            max_bytes: MAX_BYTES + 1,
            ..PoolLimits::default()
        },
        PoolLimits {
            max_senders: 257,
            ..PoolLimits::default()
        },
        PoolLimits {
            max_per_sender: 17,
            ..PoolLimits::default()
        },
    ] {
        assert!(Pool::with_limits(&g, limits).is_err());
    }
}

#[test]
fn nonce_window_and_exhaustion_do_not_wrap() {
    let g = genesis();
    let mut pool = Pool::new(&g).unwrap();
    pool.admit(tx(&g, 1, 15, 10), 0).unwrap();
    assert_eq!(pool.admit(tx(&g, 1, 16, 10), 0), Err(Error("NonceTooHigh")));
    assert_eq!(pool.admit(tx(&g, 2, 0, 10), 1), Err(Error("NonceTooLow")));
    assert!(pool.select(|_| Ok(0)).unwrap().txs.is_empty());
    let last = tx(&g, 2, u64::MAX - 1, 10);
    pool.admit(last.clone(), u64::MAX - 1).unwrap();
    assert_eq!(pool.contiguous_next(last.tx.actor, u64::MAX - 1), u64::MAX);
    assert_eq!(
        pool.select(|actor| Ok(if actor == last.tx.actor {
            u64::MAX - 1
        } else {
            0
        }))
        .unwrap()
        .txs,
        vec![last]
    );
    assert!(pool.select(|_| Ok(u64::MAX)).unwrap().txs.is_empty());
}

#[test]
fn acknowledged_nonces_are_sampled_once_per_sender_and_errors_propagate() {
    let g = genesis();
    let mut pool = Pool::new(&g).unwrap();
    for n in 0..16 {
        pool.admit(tx(&g, 1, n, 10), 0).unwrap();
    }
    let mut calls = 0;
    let selected = pool
        .select(|_| {
            calls += 1;
            Ok(1)
        })
        .unwrap();
    assert_eq!(calls, 1);
    assert_eq!(selected.txs.len(), 15);
    assert_eq!(selected.txs[0].nonce, 1);
    assert_eq!(
        pool.select(|_| Err(Error("StorageUnavailable"))),
        Err(Error("StorageUnavailable"))
    );
    assert_eq!(pool.len(), 16);
    let original = pool.pending(signer(1).address(), [0; 32]).unwrap().clone();
    // A different body at a nonce cannot remove the original held candidate.
    let mut changed = original.tx.clone();
    changed.tx_budget += 1;
    let changed = SignedTx::sign(&g, 0, changed, &signer(1)).unwrap();
    pool.remove_committed(&SignedInputs { txs: vec![changed] });
    assert_eq!(pool.len(), 16);
    pool.remove_committed(&SignedInputs {
        txs: vec![original],
    });
    assert_eq!(pool.len(), 15);
}

#[test]
fn default_entry_and_sender_ceilings_are_enforced_at_full_capacity() {
    let g = genesis();
    let mut pool = Pool::new(&g).unwrap();
    for who in 1..=64 {
        for nonce in 0..16 {
            pool.admit(tx(&g, who, nonce, 10), 0).unwrap();
        }
    }
    assert_eq!(pool.len(), 1024);
    assert_eq!(pool.admit(tx(&g, 65, 0, 10), 0), Err(Error("PoolFull")));
    let selected = pool.select(|_| Ok(0)).unwrap();
    assert_eq!(selected.txs.len(), 16);
    assert_eq!(pool.len(), 1024);
    let mut senders = Pool::new(&g).unwrap();
    for who in 1..=256 {
        senders.admit(tx(&g, who, 0, 10), 0).unwrap();
    }
    assert_eq!(senders.sender_count(), 256);
    assert_eq!(senders.admit(tx(&g, 257, 0, 10), 0), Err(Error("PoolFull")));
    senders.admit(tx(&g, 1, 1, 10), 0).unwrap();
    assert_eq!(senders.len(), 257);
}

#[test]
fn default_retained_byte_cap_rejects_without_eviction() {
    let g = genesis();
    let mut pool = Pool::new(&g).unwrap();
    let mut filled = 0usize;
    'outer: for who in 1..=16 {
        for nonce in 0..16 {
            let signer = signer(who);
            let mut envelope = tx(&g, who, nonce, 10).tx;
            envelope.ops = vec![TxOp::Arkiv {
                namespace: arkiv_node::arkiv::entity_namespace(),
                operation: arkiv_node::arkiv::ArkivOperation::Create {
                    salt: 0,
                    flags: 0,
                    expiry: arkiv_node::Expiry {
                        expires_at: 0,
                        min_lifetime: 1_000,
                    },
                    attributes: vec![(
                        arkiv_db::CellName::new("$payload").unwrap(),
                        arkiv_db::Value::bytes(&vec![7; 128 * 1024]),
                    )],
                },
            }];
            let signed = SignedTx::sign(&g, nonce, envelope, &signer).unwrap();
            let size = encode(&signed).len();
            let old = pool.len();
            let bytes = pool.bytes();
            if bytes + size > MAX_BYTES {
                assert_eq!(pool.admit(signed, 0), Err(Error("PoolFull")));
                assert_eq!(pool.len(), old);
                assert_eq!(pool.bytes(), bytes);
                assert!(MAX_BYTES - bytes < size);
                break 'outer;
            }
            pool.admit(signed, 0).unwrap();
            filled += 1;
        }
    }
    assert_eq!(filled, 127);
    assert!(pool.bytes() <= 16 * 1024 * 1024);
}
