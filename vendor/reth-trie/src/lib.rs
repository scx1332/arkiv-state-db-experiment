//! Generic trie components extracted from Reth, backed by its Alloy MPT core.
//!
//! This local crate is a focused extraction, not the complete upstream
//! `reth-trie` API. See `vendor/README.md` for source revisions and scope.

/// Reth's forward-only cursor over pre-sorted entries.
#[rustfmt::skip]
pub mod forward_cursor;

#[rustfmt::skip]
mod key;
pub use key::{KeccakKeyHasher, KeyHasher};

pub use alloy_primitives::B256;
pub use alloy_trie::{BranchNodeCompact, EMPTY_ROOT_HASH, HashBuilder, Nibbles, TrieMask, nodes};

/// The MPT hash builder re-exported by Reth's trie-common crate.
pub mod hash_builder {
    pub use alloy_trie::hash_builder::*;
}
