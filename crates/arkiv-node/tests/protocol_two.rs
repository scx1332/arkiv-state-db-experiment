#![cfg(not(feature = "arkiv-v2"))]
// These retained fixtures pin the legacy generic/signed protocol.
// Arkiv v2 has separate host, profile and crash acceptance suites.
mod support;
use arkiv_db::{B256, decode_exact, encode};
use arkiv_node::{BlockInputs, ChainSpec, Genesis, MemoryChain};

#[test]
fn protocol_two_commits_genesis_and_terminal_state_without_changing_legacy() {
    let legacy = support::genesis();
    let mut genesis = legacy.clone();
    genesis.protocol_version = 2;
    assert_eq!(decode_exact::<Genesis>(&encode(&genesis)).unwrap(), genesis);
    let mut chain = MemoryChain::new(ChainSpec::new(genesis.clone()).unwrap());
    assert_eq!(chain.head().version(), 2);
    assert_eq!(
        chain.head().state_root,
        Some(chain.snapshot(0).unwrap().state_root())
    );
    let initial = *chain.head();
    let inputs = BlockInputs::default();
    let discarded = chain.prepare(2000, inputs.clone()).unwrap();
    assert_eq!(*chain.head(), initial);
    drop(discarded);
    assert_eq!(*chain.head(), initial);
    let block = chain.produce(2000, inputs).unwrap();
    assert_eq!(block.header.state_root, initial.state_root);
    let mut follower = MemoryChain::new(ChainSpec::new(genesis).unwrap());
    let mut false_root = block.clone();
    false_root.header.state_root = Some(B256::ZERO);
    assert!(follower.apply(&false_root).is_err());
    assert_eq!(follower.height(), 0);
    follower.apply(&block).unwrap();
    assert_eq!(chain.head(), follower.head());
    let mut old = MemoryChain::new(ChainSpec::new(legacy).unwrap());
    assert_eq!(old.head().version(), 1);
    assert!(old.apply(&block).is_err());
}
