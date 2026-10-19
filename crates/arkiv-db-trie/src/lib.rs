//! A persistent Merkle Patricia map for authenticated, lazily loaded state.
//!
//! [`MerkleMap`] is a hexary Patricia trie over bounded, prefix-free,
//! variable-length keys whose node
//! encoding is the Ethereum MPT encoding of `alloy_trie::nodes` (RLP leaf,
//! extension and branch nodes, hex-prefix paths, children referenced inline
//! below 32 bytes and by keccak256 otherwise). Its shape is canonical, so
//! the root hash is a function of the key/value set alone and matches
//! `alloy_trie::HashBuilder` over the same sorted leaves. It is the one
//! structure the engine's state is built from (node step N3a of
//! `docs/node-storage-plan.md`; the engine uses it since N3b, and its
//! persistence and proofs are N3c–N3d).
//!
//! - **Persistence and sharing.** A handle is a root [`Link`] plus the
//!   entry count. Updates copy the nodes on one key path and share the
//!   rest, so earlier handles stay valid and cheap.
//! - **Stored and in-memory nodes.** A link is [`Link::Empty`],
//!   [`Link::Mem`] (a node in memory) or [`Link::Stored`] (the id of a
//!   committed node). Handles and nodes hold no store; operations take a
//!   [`NodeLoader`] by reference, load stored nodes on demand, verify
//!   `keccak256(bytes) == id`, and are fallible: a failed read is an error,
//!   never an absent key. [`MerkleMap::from_root`] opens a committed map
//!   without reading anything.
//! - **Staging.** [`MerkleMap::stage`] emits the objects to commit: the
//!   root (stored by hash even when shorter than 32 bytes) and every other
//!   new node of at least 32 bytes, stopping at stored and already
//!   committed nodes, deduplicated per sink. Nested maps inside leaf values
//!   are staged through [`TrieValue::stage_nested`] or
//!   [`MerkleMap::stage_with`]. For bounded durable preparation,
//!   [`MerkleMap::preflight_with`] checks new nodes before encoding/hashing,
//!   then [`MerkleMap::try_stage_with`] stops on sink or traversal failures.
//!   A [`StageBudget`] is shared across nested maps in each phase.
//! - **Sorted bulk construction.** [`SortedTrieBuilder`] emits the same
//!   canonical objects directly from sorted encoded leaves while retaining
//!   only the current key's branch frontier and one value. It supports the
//!   same variable-length keys as the persistent map.
//! - **Proofs.** [`MerkleMap::proof`] yields the root node and every
//!   hash-referenced node on a key's path; [`walk`] is the strict verifier
//!   that returns the proven value or rejects the proof.
//! - **Visit counting.** Every operation reports a [`Probe`]: nodes
//!   examined (independent of what was cached or loaded) and loads made.
//! - **Key encodings.** [`TrieKey`] bounds a key's length and fixes its
//!   order; encodings must be prefix-free, because branches hold no values.
//!   [`order`] has order-preserving, prefix-free encoders for composite
//!   keys with variable-length parts.
//!
//! What this crate does not do: it has no durable store (the
//! [`MemoryNodeStore`] is a test double), assigns no object spaces, keeps
//! no cache, and meters nothing. The engine supplies those. See
//! `docs/state-trie.md` for the design and its evidence.
//!
//! ```
//! use arkiv_db_trie::{EMPTY_ROOT_HASH, MemoryNodeStore, MerkleMap, Probe, Staged, walk};
//!
//! let mut store = MemoryNodeStore::new();
//! let mut probe = Probe::new();
//! let empty: MerkleMap<[u8; 4], Vec<u8>> = MerkleMap::new();
//! assert_eq!(empty.root_hash(), EMPTY_ROOT_HASH);
//! let (map, _) = empty.insert(&*b"key1", b"one".to_vec(), &store, &mut probe)?;
//! let (map, _) = map.insert(&*b"key2", b"two".to_vec(), &store, &mut probe)?;
//!
//! // Commit the new nodes and reopen the map from its root alone.
//! let mut staged = Staged::new();
//! map.stage(&mut staged);
//! store.commit(&staged);
//! let reopened: MerkleMap<[u8; 4], Vec<u8>> = MerkleMap::from_root(map.root_hash(), 2)?;
//! assert_eq!(reopened.get(b"key2", &store, &mut probe)?, Some(b"two".to_vec()));
//!
//! // Prove the entry and verify the proof against the root alone.
//! let proof = reopened.prove(b"key1", &store, &mut probe)?;
//! assert_eq!(walk(map.root_hash(), b"key1", &proof), Ok(Some(b"one".to_vec())));
//! # Ok::<(), arkiv_db_trie::TrieError>(())
//! ```

mod codec;
mod error;
mod explore;
mod interval;
mod loader;
mod map;
mod nibbles;
mod node;
pub mod order;
mod proof;
mod range;
mod reach;
mod sorted;
mod stage;

pub use alloy_trie::EMPTY_ROOT_HASH;
pub use codec::{CodecError, TrieKey, TrieValue, is_prefix_free};
pub use error::TrieError;
pub use explore::{ExploreChild, ExploreNode};
pub use interval::{
    IntervalDecision, IntervalEntries, IntervalLimits, IntervalNode, IntervalProofIter, KeyRange,
    MAX_INTERVAL_TRACE_NODES, VerifiedInterval, inspect_interval, walk_interval,
};
pub use loader::{MemoryNodeStore, MemoryOnly, NodeLoader};
pub use map::{MerkleMap, Probe};
pub use node::{Link, NodeBox, NodeKind, ObjectId};
pub use proof::{
    PathChild, PathNode, PathSource, PathTerminal, ProofError, ProofIter, VerifiedPath,
    inspect_path, walk,
};
pub use range::Range;
pub use sorted::{SortedTrieBuilder, SortedTrieError};
pub use stage::{NestedStageVisitor, ObjectSink, StageBudget, StageError, Staged, TryObjectSink};
