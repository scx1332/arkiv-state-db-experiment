//! Shared builders for the signed-chain tests: every signed chain runs the
//! `glm-v2` profile, so a test genesis funds each actor it uses.

#![allow(dead_code)]

use alloy_primitives::U256;
use arkiv_db::OwnerId;
use arkiv_node::{
    Genesis,
    accounts::AccountConfig,
    signed::{
        crypto::Signer,
        protocol::{SignedGenesis, default_native_genesis},
    },
};

/// The balance each funded test actor starts with: one million GLM.
pub fn balance() -> U256 {
    U256::from(10u64).pow(U256::from(24u64))
}

/// Allocations giving every actor in `funded` [`balance`].
pub fn allocations(funded: &[OwnerId]) -> AccountConfig {
    AccountConfig::new(funded.iter().map(|actor| (*actor, balance())).collect()).unwrap()
}

/// The deterministic test signer for `byte`.
pub fn signer(byte: u8) -> Signer {
    Signer::from_bytes([byte; 32]).unwrap()
}

/// The addresses of the signers for `bytes`.
pub fn actors(bytes: &[u8]) -> Vec<OwnerId> {
    bytes.iter().map(|b| signer(*b).address()).collect()
}

/// A signed genesis over `native`, proposed by `proposer`, funding the
/// signers for `funded` (the proposer is always funded).
pub fn genesis_from(
    native: Genesis,
    run_id: [u8; 16],
    proposer: u8,
    block_period_ms: u64,
    funded: &[u8],
) -> SignedGenesis {
    let mut bytes = funded.to_vec();
    if !bytes.contains(&proposer) {
        bytes.push(proposer);
    }
    SignedGenesis::new(
        native,
        run_id,
        *signer(proposer).address().as_bytes(),
        block_period_ms,
        allocations(&actors(&bytes)),
    )
    .unwrap()
}

/// A signed genesis over the default native genesis.
pub fn genesis(
    chain_id: u64,
    timestamp_ms: u64,
    run_id: [u8; 16],
    proposer: u8,
    block_period_ms: u64,
    funded: &[u8],
) -> SignedGenesis {
    genesis_from(
        default_native_genesis(chain_id, timestamp_ms, block_period_ms),
        run_id,
        proposer,
        block_period_ms,
        funded,
    )
}
