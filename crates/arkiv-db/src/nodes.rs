//! The seam through which every engine map reads committed trie nodes, and
//! the engine's internal failure type.
//!
//! Every map of the engine state is an [`arkiv_db_trie::MerkleMap`], whose
//! handles hold no store: each operation that may follow a stored link takes
//! a loader. [`Nodes`] is what an engine, its snapshots and its branches
//! hold to make those loaders: nothing for a state built entirely in memory
//! (the trie crate's [`MemoryOnly`] loader, which a memory map never
//! consults), an [`ObjectReader`] for a state opened from committed roots
//! (read and decoded on every load, uncached), or the durable engine's
//! gated store context with its bounded decoded-node cache. A [`Loader`] is
//! one map's view of it: the object space the map's nodes live in and the
//! kind of map, which together with the object id key the cache, since the
//! maps of a namespace share its space and decode to different node types.
//!
//! [`Fault`] is how internal operations stop: a deterministic
//! [`Rejection`], which is metered and recorded in a receipt, or a
//! [`StorageError`], which is an availability failure of the storage
//! beneath the state and is never a rejection, a receipt, a missing record
//! or an empty result.

use std::{fmt, sync::Arc};

use arkiv_db_storage::versioned::{ObjectReader, SpaceId, StorageError, StorageErrorKind};
use arkiv_db_trie::{MemoryOnly, NodeBox, NodeLoader, ObjectId, TrieError, TrieValue};

use crate::{
    durable::cache::{Context, LoadFailure},
    meter::Rejection,
    namespace::NamespaceId,
    state::MapKind,
};

/// Where the maps of one engine read committed nodes from.
///
/// Cloning is O(1). A memory engine has no source: its maps are built by
/// updates and never contain a stored link.
#[derive(Clone, Default)]
pub(crate) struct Nodes {
    source: Source,
}

#[derive(Clone, Default)]
enum Source {
    #[default]
    Memory,
    /// Every load reads and decodes; nothing is retained.
    Reader(Arc<dyn ObjectReader>),
    /// The durable engine's gated context and decoded-node cache.
    Durable(Arc<Context>),
}

impl fmt::Debug for Nodes {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let source = match self.source {
            Source::Memory => "Memory",
            Source::Reader(_) => "ObjectReader",
            Source::Durable(_) => "Durable",
        };
        formatter
            .debug_struct("Nodes")
            .field("source", &source)
            .finish()
    }
}

impl Nodes {
    /// Nodes of a state that lives entirely in memory.
    pub(crate) const fn memory() -> Self {
        Self {
            source: Source::Memory,
        }
    }

    /// Nodes of a state whose committed nodes `reader` supplies, uncached.
    pub(crate) fn stored(reader: Arc<dyn ObjectReader>) -> Self {
        Self {
            source: Source::Reader(reader),
        }
    }

    /// Nodes of a durable engine state, loaded through its context's
    /// decoded-node cache.
    pub(crate) fn durable(context: Arc<Context>) -> Self {
        Self {
            source: Source::Durable(context),
        }
    }

    /// The loader of the namespace catalog.
    pub(crate) const fn catalog(&self) -> Loader<'_> {
        Loader {
            nodes: self,
            space: SpaceId::Catalog,
            map: MapKind::Catalog,
        }
    }

    /// The loader of the host-state map.
    pub(crate) const fn host(&self) -> Loader<'_> {
        Loader {
            nodes: self,
            space: SpaceId::Host,
            map: MapKind::Host,
        }
    }

    /// The loaders of one namespace's maps, which share its object space.
    pub(crate) fn namespace(&self, namespace: NamespaceId) -> NamespaceLoaders<'_> {
        let space = SpaceId::Namespace(namespace.ordinal());
        let loader = |map| Loader {
            nodes: self,
            space,
            map,
        };
        NamespaceLoaders {
            records: loader(MapKind::Records),
            keys: loader(MapKind::Keys),
            rows: loader(MapKind::Rows),
            terms: loader(MapKind::Terms),
            postings: loader(MapKind::Postings),
        }
    }
}

/// The loader of one map: the engine's [`Nodes`], the map's object space
/// and its kind. `Copy`, so iterators can borrow it for as long as they run.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Loader<'n> {
    nodes: &'n Nodes,
    space: SpaceId,
    map: MapKind,
}

impl Loader<'_> {
    fn failure(&self, kind: StorageErrorKind, id: &ObjectId, what: &str) -> LoadError {
        LoadError(StorageError::new(
            kind,
            format!("{:?} node {id} in {:?}: {what}", self.map, self.space),
        ))
    }
}

impl<V: TrieValue + Send + Sync + 'static> NodeLoader<V> for Loader<'_> {
    type Error = LoadError;

    fn read(&self, id: &ObjectId) -> Result<Option<Vec<u8>>, LoadError> {
        match &self.nodes.source {
            Source::Memory => NodeLoader::<V>::read(&MemoryOnly, id).map_err(LoadError::from),
            Source::Reader(reader) => reader.read_object(self.space, &id.0).map_err(LoadError),
            Source::Durable(context) => context.read_object(self.space, &id.0).map_err(LoadError),
        }
    }

    /// Reads, verifies and decodes a node (or takes it from the durable
    /// cache), naming the map and space in any failure: an absent node is
    /// `Missing`, bytes that do not hash to the id or do not decode are
    /// `Corrupt`.
    fn load(&self, id: &ObjectId) -> Result<Arc<NodeBox<V>>, LoadError> {
        let corrupt =
            |error: TrieError| self.failure(StorageErrorKind::Corrupt, id, &error.to_string());
        let node =
            match &self.nodes.source {
                Source::Durable(context) => context
                    .load::<V>(self.space, self.map, &id.0)
                    .map_err(|failure| match failure {
                        LoadFailure::Storage(error) => LoadError(error),
                        LoadFailure::Decode(error) => corrupt(error),
                    })?,
                Source::Memory | Source::Reader(_) => match NodeLoader::<V>::read(self, id)? {
                    Some(bytes) => Some(NodeBox::decode_stored(id, &bytes).map_err(corrupt)?),
                    None => None,
                },
            };
        node.ok_or_else(|| self.failure(StorageErrorKind::Missing, id, "referenced but absent"))
    }
}

/// The loaders of the maps of one namespace.
#[derive(Clone, Copy, Debug)]
pub(crate) struct NamespaceLoaders<'n> {
    pub(crate) records: Loader<'n>,
    pub(crate) keys: Loader<'n>,
    pub(crate) rows: Loader<'n>,
    pub(crate) terms: Loader<'n>,
    pub(crate) postings: Loader<'n>,
}

/// A storage failure reported by a map operation.
///
/// A distinct type (not [`StorageError`] itself) so that trie integrity
/// failures can convert into it: an absent object is `Missing`, every other
/// trie failure `Corrupt`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LoadError(pub(crate) StorageError);

impl From<TrieError> for LoadError {
    fn from(error: TrieError) -> Self {
        let kind = match error {
            TrieError::Missing { .. } => StorageErrorKind::Missing,
            _ => StorageErrorKind::Corrupt,
        };
        Self(StorageError::new(kind, error.to_string()))
    }
}

/// A `Corrupt` storage error: committed state that contradicts itself.
pub(crate) fn corrupt(detail: impl Into<String>) -> StorageError {
    StorageError::new(StorageErrorKind::Corrupt, detail)
}

/// Why an internal engine operation stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Fault {
    /// A deterministic rejection: metered, recorded in the receipt.
    Reject(Rejection),
    /// The storage beneath the state failed: no receipt, no rejection.
    Storage(StorageError),
}

impl From<Rejection> for Fault {
    fn from(reason: Rejection) -> Self {
        Self::Reject(reason)
    }
}

impl From<LoadError> for Fault {
    fn from(error: LoadError) -> Self {
        Self::Storage(error.0)
    }
}

impl From<StorageError> for Fault {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}

impl From<TrieError> for Fault {
    fn from(error: TrieError) -> Self {
        LoadError::from(error).into()
    }
}
