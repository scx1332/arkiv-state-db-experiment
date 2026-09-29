//! The authenticated state root.
//!
//! Every engine state (a published snapshot or a sealed candidate) is a set
//! of Merkle Patricia tries (see `docs/state-trie.md`): the namespace
//! catalog, whose leaves carry each namespace's descriptor, record
//! allocator and the roots of its raw record, key-binding, row and term
//! maps (term leaves carry their posting roots), and the host-state map.
//! The [`StateHeader`] gathers the global part: the catalog and host roots
//! with their entry counts, the namespace allocator and the committed
//! pricing schedule. The state root is
//!
//! ```text
//! digest("arkiv/state/v1", encode(StateHeader))
//! ```
//!
//! with [`digest`] and the canonical codec. It depends only
//! on the logical content: trie shapes are canonical, and physical node
//! ids, cache state, snapshot ids and process tokens never enter it.
//! Computing it hashes only the trie nodes built since they were last
//! hashed (each node caches its hash), and never reads storage.

use arkiv_db_storage::versioned::{ObjectId, SpaceId};

use crate::{
    codec::{B256, Decode, DecodeError, Decoder, Encode, Encoder, digest, encode},
    layout::valid_handle,
    meter::PriceSchedule,
};

/// Version of the engine state layout: the key and leaf encodings of every
/// map and the [`StateHeader`] layout.
pub const STATE_FORMAT: u32 = crate::LAYOUT_VERSION;

/// The digest domain of the state root.
pub const STATE_ROOT_DOMAIN: &str = "arkiv/state/v1";

/// The global part of one engine state, whose digest is the state root.
///
/// Encoded as `format u32 ‖ catalog_root ‖ catalog_len u64 ‖ next_namespace
/// u64 ‖ pricing ‖ host_root ‖ host_len u64` (roots as their 32 raw bytes,
/// pricing as the codec's `PriceSchedule`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateHeader {
    /// [`STATE_FORMAT`].
    pub format: u32,
    /// Root hash of the namespace catalog (`EMPTY_ROOT_HASH` when empty).
    pub catalog_root: B256,
    /// Number of namespaces in the catalog.
    pub catalog_len: u64,
    /// The ordinal the next namespace creation receives.
    pub next_namespace: u64,
    /// The pricing schedule committed in the state (the one branches based
    /// on it are priced with).
    pub pricing: PriceSchedule,
    /// Root hash of the host-state map.
    pub host_root: B256,
    /// Number of host-state entries.
    pub host_len: u64,
}

impl StateHeader {
    /// The state root: `digest("arkiv/state/v1", encode(self))`.
    pub fn root(&self) -> B256 {
        digest(STATE_ROOT_DOMAIN, &encode(self))
    }
}

impl Encode for StateHeader {
    fn encode_to(&self, encoder: &mut Encoder) {
        encoder.u32(self.format);
        encoder.put(&self.catalog_root);
        encoder.u64(self.catalog_len);
        encoder.u64(self.next_namespace);
        encoder.put(&self.pricing);
        encoder.put(&self.host_root);
        encoder.u64(self.host_len);
    }
}

/// Rejects a format other than [`STATE_FORMAT`], a root and count that no
/// map has (the empty root with entries, or another root without), and a
/// namespace allocator that is not above every catalog entry.
impl Decode for StateHeader {
    const MIN_ENCODED_LEN: usize =
        4 + 32 + 8 + 8 + <PriceSchedule as Decode>::MIN_ENCODED_LEN + 32 + 8;

    fn decode_from(decoder: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        let format = decoder.u32()?;
        if format != STATE_FORMAT {
            return Err(DecodeError::UnsupportedVersion {
                kind: "StateHeader",
                version: format,
            });
        }
        let header = Self {
            format,
            catalog_root: decoder.get()?,
            catalog_len: decoder.u64()?,
            next_namespace: decoder.u64()?,
            pricing: decoder.get()?,
            host_root: decoder.get()?,
            host_len: decoder.u64()?,
        };
        if !valid_handle(&header.catalog_root, header.catalog_len)
            || !valid_handle(&header.host_root, header.host_len)
        {
            return Err(DecodeError::Invalid {
                kind: "StateHeader",
                reason: "a map root and entry count that no map has",
            });
        }
        if header.next_namespace <= header.catalog_len {
            return Err(DecodeError::Invalid {
                kind: "StateHeader",
                reason: "the namespace allocator is not above the catalog",
            });
        }
        Ok(header)
    }
}

/// The roots of one namespace's maps and its record allocator: the content
/// of its catalog leaf besides the descriptor. A test aid; see
/// `Snapshot::try_namespace_roots`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NamespaceRoots {
    /// The id the next record creation receives.
    pub next_record: u64,
    /// Raw record map: root and entry count.
    pub records: (B256, u64),
    /// Key-binding map.
    pub keys: (B256, u64),
    /// Row map.
    pub rows: (B256, u64),
    /// Term map.
    pub terms: (B256, u64),
}

/// The kind of engine map a trie node belongs to.
///
/// The maps of a namespace share its object space, and the decoded node
/// type depends on the map, so a decoded-node cache keys on the kind (node
/// step N3c), and staging measurements report it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MapKind {
    /// The namespace catalog (space `Catalog`).
    Catalog,
    /// The host-state map (space `Host`).
    Host,
    /// A namespace's raw byte records.
    Records,
    /// A namespace's key bindings.
    Keys,
    /// A namespace's rows.
    Rows,
    /// A namespace's term map.
    Terms,
    /// A namespace's posting maps, nested in its term leaves.
    Postings,
}

/// One object of a state that exists only in memory: a trie node a commit
/// must write. A test aid until node step N3c stages commits; see
/// `Snapshot::staged_objects`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StagedObject {
    /// The object space: `Catalog`, `Host` or the namespace's.
    pub space: SpaceId,
    /// The map the node belongs to.
    pub map: MapKind,
    /// `keccak256(bytes)`.
    pub id: ObjectId,
    /// The node's encoding.
    pub bytes: Vec<u8>,
}
