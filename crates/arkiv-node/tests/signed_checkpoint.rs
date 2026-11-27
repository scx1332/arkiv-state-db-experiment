//! State commits that share a durable commit (`RedbConfig::durable_every`):
//! a crash leaves `node.redb` several blocks behind `history.redb`, whose
//! every commit stays durable, and the node executes those blocks again
//! from the history before it does anything else.
//!
//! - A killed producer comes back at its last durable state commit and
//!   replays to the head it had served: same blocks, same hashes. The next
//!   block it produces extends that chain for a follower that never
//!   stopped.
//! - Without the replay the same producer signs a second block at a height
//!   it already served, which a follower rejects as a chain conflict. This
//!   is what the replay prevents.
//! - A killed follower replays the same way, without its peer.
//! - A clean close makes every commit durable: nothing is replayed.
//!
//! Scope: the crash is a process kill on real files (redb's close writes
//! nothing), not a power cut; the power-cut model of the store is covered by
//! the store's own `checkpoint` tests.

use arkiv_db::durable::Options;
use arkiv_db::{Cell, CellName, Value};
use arkiv_db_redb::{RedbConfig, RedbStore};
use arkiv_db_storage::versioned::VersionedStore;
use arkiv_node::{
    Expiry,
    arkiv::ArkivOperation,
    signed::{
        history::History,
        host::{Host, Prefill},
        protocol::{SignedGenesis, SignedTx},
    },
};
use std::{path::PathBuf, sync::Arc};

mod signed_support;
use signed_support::{arkiv_ops, launch, memory_host, sign_tx, signer};

const EVERY: u32 = 8;

fn genesis() -> SignedGenesis {
    signed_support::genesis(9100, 1000, [7; 16], 1, 100, &[1, 2, 3])
}

fn shared() -> RedbConfig {
    RedbConfig {
        durable_every: EVERY,
        ..RedbConfig::default()
    }
}

struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("checkpoint-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn create(&self, producer: bool) -> (Host, Arc<RedbStore>, Arc<RedbStore>) {
        let state = Arc::new(RedbStore::create(self.0.join("node.redb"), shared()).unwrap());
        let history = Arc::new(
            RedbStore::create(self.0.join("history.redb"), RedbConfig::default()).unwrap(),
        );
        let launch = launch(&genesis(), signer(1), [3; 16], history.clone(), producer);
        let host = Host::create(state.clone(), Options::default(), launch, Prefill::None).unwrap();
        (host, state, history)
    }

    /// The host as a restarted node finds it, before any replay.
    fn open(&self, producer: bool) -> (Host, Arc<RedbStore>) {
        let state = Arc::new(RedbStore::open(self.0.join("node.redb"), shared()).unwrap());
        let history: Arc<dyn VersionedStore> =
            Arc::new(RedbStore::open(self.0.join("history.redb"), RedbConfig::default()).unwrap());
        let genesis = genesis();
        let host = Host::open(
            state.clone(),
            History::open(history, genesis.hash()).unwrap(),
            &genesis,
            producer.then(|| signer(1)),
            Options::default(),
        )
        .unwrap();
        (host, state)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Ends the process as a kill would: nothing more reaches either file.
fn kill(host: Host, state: Arc<RedbStore>, history: Arc<RedbStore>) {
    state.probe().kill();
    history.probe().kill();
    drop((host, state, history));
}

fn tx(nonce: u64) -> SignedTx {
    let mut request_id = [2; 32];
    request_id[..8].copy_from_slice(&nonce.to_be_bytes());
    let create = ArkivOperation::Create {
        salt: u128::from(nonce) + 1,
        flags: 0,
        expiry: Expiry {
            expires_at: 0,
            min_lifetime: 1_000,
        },
        attributes: vec![
            (
                CellName::new("body").unwrap(),
                Cell::field(Value::bytes(format!("entity {nonce}").as_bytes())),
            ),
            (
                CellName::new("group").unwrap(),
                Cell::attribute(Value::U64(nonce % 3)),
            ),
        ],
    };
    sign_tx(
        &genesis(),
        &signer(2),
        nonce,
        request_id,
        10_000_000,
        arkiv_ops(vec![create]),
    )
}

/// Produces up to `height`, one transaction per block after the first, and
/// relays every block to `witness`.
fn produce_to(producer: &mut Host, witness: &mut Host, height: u64) {
    while producer.head.height < height {
        if producer.head.height >= 1 {
            let nonce = producer.committed_nonce(signer(2).address()).unwrap();
            producer.submit(tx(nonce)).unwrap();
        }
        producer.produce().unwrap();
        witness
            .import(producer.block(producer.head.height).unwrap())
            .unwrap();
    }
    assert_eq!(witness.head, producer.head);
}

fn message<T>(result: arkiv_node::signed::Result<T>) -> &'static str {
    result.err().expect("expected rejection").code.as_str()
}

#[test]
fn a_killed_producer_replays_the_blocks_it_served_and_extends_them() {
    let dir = Dir::new("producer");
    let mut witness = memory_host(&genesis(), signer(1), [4; 16], false);
    let (mut producer, state, history) = dir.create(true);
    produce_to(&mut producer, &mut witness, 11);
    let served = producer.head;
    assert!(state.undurable_commits() > 0);
    kill(producer, state, history);

    let (mut producer, state) = dir.open(true);
    assert!(state.shared_durability().unwrap());
    let durable = producer.head.height;
    assert!(durable < served.height, "the kill lost state commits");
    assert!(durable >= served.height - u64::from(EVERY));
    assert_eq!(producer.replay_history().unwrap(), served.height - durable);
    assert_eq!(producer.head, served);
    for height in durable..=served.height {
        assert_eq!(
            producer.block(height).unwrap(),
            witness.block(height).unwrap()
        );
    }
    // The transactions of the replayed blocks are included once.
    assert_eq!(
        producer.committed_nonce(signer(2).address()).unwrap(),
        served.height - 1
    );
    producer.paused = false;
    produce_to(&mut producer, &mut witness, served.height + 3);
    // Nothing is left to replay.
    assert_eq!(producer.replay_history().unwrap(), 0);
}

#[test]
fn without_the_replay_a_killed_producer_signs_a_served_height_again() {
    let dir = Dir::new("equivocation");
    let mut witness = memory_host(&genesis(), signer(1), [4; 16], false);
    let (mut producer, state, history) = dir.create(true);
    produce_to(&mut producer, &mut witness, 11);
    kill(producer, state, history);

    let (mut producer, _state) = dir.open(true);
    let durable = producer.head.height;
    assert!(durable < 11);
    producer.paused = false;
    // A later wall clock, as after any real restart.
    producer.set_clock(|| arkiv_node::signed::host::system_clock_ms() + 60_000);
    producer.produce().unwrap();
    let second = producer.block(durable + 1).unwrap();
    assert_ne!(second.header, witness.block(durable + 1).unwrap().header);
    assert_eq!(message(witness.import(second)), "ChainConflict");
}

#[test]
fn a_killed_follower_replays_without_its_peer() {
    let dir = Dir::new("follower");
    let mut producer = memory_host(&genesis(), signer(1), [3; 16], true);
    let (mut follower, state, history) = dir.create(false);
    produce_to(&mut producer, &mut follower, 13);
    kill(follower, state, history);

    let (mut follower, _state) = dir.open(false);
    let durable = follower.head.height;
    assert!(durable < 13);
    assert_eq!(follower.replay_history().unwrap(), 13 - durable);
    assert_eq!(follower.head, producer.head);
    produce_to(&mut producer, &mut follower, 15);
}

#[test]
fn a_clean_close_leaves_nothing_to_replay() {
    let dir = Dir::new("close");
    let mut witness = memory_host(&genesis(), signer(1), [4; 16], false);
    let (mut producer, state, history) = dir.create(true);
    produce_to(&mut producer, &mut witness, 11);
    let served = producer.head;
    assert!(state.undurable_commits() > 0);
    state.close().unwrap();
    history.close().unwrap();
    drop((producer, state, history));

    let (mut producer, _state) = dir.open(true);
    assert_eq!(producer.head, served);
    assert_eq!(producer.replay_history().unwrap(), 0);
}
