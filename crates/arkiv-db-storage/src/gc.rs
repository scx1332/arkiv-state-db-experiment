//! Garbage collection of objects no retained snapshot reaches.
//!
//! Objects are content-addressed and shared: one object can be reachable
//! from many snapshots, from several parents of one snapshot, and can be
//! created again after it became unreachable. A store with garbage
//! collection keeps, in the same atomic transaction as every head move:
//!
//! - **Reference counts taken from the head only.** The count of a live
//!   object is the number of edges into it from live objects and from dead
//!   objects whose own edges are not yet released ("pending"), plus the
//!   number of head roots naming it. Counts are sparse: an object that is
//!   present, not dead and has no `shared` row has count one, so the common
//!   case writes no count at all. Edges are a pure function of an object's
//!   space and bytes ([`ObjectGraph`]); the caller supplies the graph.
//! - **A death queue tagged by commit epoch.** An object whose count reaches
//!   zero is dead at the epoch of that commit (a store commit counter that
//!   is never reused). A dead object is not reachable from any snapshot
//!   committed at or after its death epoch. It is deleted, again inside a
//!   commit, once the *horizon* passed with the commit (the lowest snapshot
//!   any reader may still open) was committed at or after that epoch.
//! - **Bounded work that cannot fall behind.** Releasing a dead object's
//!   edges (the cascade) and deleting queued objects each have a per-commit
//!   budget that is a floor, not a cap: see [`GcCommit`].
//!
//! Nothing here enters a logical hash: counts, queues and epochs are local
//! to one store. A violated count (a decrement below zero, a strict edge to
//! a missing object) is reported as [`GcError::Invariant`]; stores then
//! commit the batch without garbage collection and mark it suspended, so
//! production continues and an operator rebuilds the counts offline.
//!
//! [`plan`] computes everything one commit changes from reads only, so a
//! store can apply it atomically. [`seed`] builds the counts of an existing
//! store from one scan of its objects, and [`verify`] checks every
//! invariant against a full [`GcDump`] (tests and small stores).

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet, hash_map::Entry},
    fmt,
    io::{self, BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
};

use crate::versioned::{ObjectId, SpaceId, StorageError, StorageErrorKind};

/// Metadata name of the garbage-collection state ([`GcState::encode`]).
pub const GC_STATE_KEY: &str = "store/gc/state";

/// `store/format` of a store with garbage collection. Binaries that only
/// know format 1 refuse such a file, because they would add objects
/// without counting them.
pub const GC_STORE_FORMAT: u32 = 2;

/// Length of a [`GcKey`].
pub const GC_KEY_LEN: usize = 41;

/// One object's identity in the GC tables: its space as 9 bytes (tag,
/// then a big-endian namespace ordinal) followed by its id. Keys order
/// like `(SpaceId, ObjectId)`.
pub type GcKey = [u8; GC_KEY_LEN];

/// A death-queue entry: the death epoch (big-endian) and the object.
pub type QueueKey = [u8; 8 + GC_KEY_LEN];

/// Encodes `(space, id)` as a [`GcKey`].
pub fn gc_key(space: SpaceId, id: &ObjectId) -> GcKey {
    let mut key = [0u8; GC_KEY_LEN];
    let (tag, ordinal) = match space {
        SpaceId::Catalog => (0, 0),
        SpaceId::Host => (1, 0),
        SpaceId::Namespace(n) => (2, n),
    };
    key[0] = tag;
    key[1..9].copy_from_slice(&ordinal.to_be_bytes());
    key[9..].copy_from_slice(id);
    key
}

/// Decodes a [`GcKey`]; `Corrupt` on an unknown space tag.
pub fn split_key(key: &GcKey) -> Result<(SpaceId, ObjectId), StorageError> {
    let ordinal = u64::from_be_bytes(key[1..9].try_into().expect("8 bytes"));
    let space = match (key[0], ordinal) {
        (0, 0) => SpaceId::Catalog,
        (1, 0) => SpaceId::Host,
        (2, n) => SpaceId::Namespace(n),
        _ => return Err(corrupt("gc key with an unknown space")),
    };
    let mut id = [0u8; 32];
    id.copy_from_slice(&key[9..]);
    Ok((space, id))
}

/// Encodes a death-queue entry.
pub fn queue_key(epoch: u64, key: &GcKey) -> QueueKey {
    let mut out = [0u8; 8 + GC_KEY_LEN];
    out[..8].copy_from_slice(&epoch.to_be_bytes());
    out[8..].copy_from_slice(key);
    out
}

/// Decodes a death-queue entry into its epoch and object key.
pub fn split_queue_key(key: &QueueKey) -> (u64, GcKey) {
    let epoch = u64::from_be_bytes(key[..8].try_into().expect("8 bytes"));
    let mut object = [0u8; GC_KEY_LEN];
    object.copy_from_slice(&key[8..]);
    (epoch, object)
}

fn corrupt(detail: impl Into<String>) -> StorageError {
    StorageError::new(StorageErrorKind::Corrupt, detail)
}

/// One reference from an object to another.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Edge {
    pub space: SpaceId,
    pub id: ObjectId,
    /// A strict edge names an object that must exist. A speculative edge
    /// is one the bytes alone cannot confirm (a value that merely looks
    /// like a reference); its target may be absent, and such references
    /// are counted as phantoms until the object appears.
    pub strict: bool,
}

/// The edges of every object, as a pure function of its space and bytes.
///
/// Incrementing and decrementing must use the same edges, so the function
/// must not change for a store's lifetime: a store records the
/// [`version`](Self::version) it was counted with and refuses another.
pub trait ObjectGraph: Send + Sync {
    /// The version recorded in the store's GC state.
    fn version(&self) -> u32;
    /// Appends the references of `bytes`, an object of `space`. An error
    /// means the bytes are not an object of this graph.
    fn edges(&self, space: SpaceId, bytes: &[u8], out: &mut Vec<Edge>) -> Result<(), String>;
}

/// A dead object not yet deleted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DeadEntry {
    /// The epoch of the commit in which its count reached zero.
    pub epoch: u64,
    /// `Some(seq)` while its own edges are not released (its position on
    /// the release stack); `None` once released and queued for deletion.
    pub pending: Option<u64>,
    /// Its encoded length, known once released (zero while pending).
    pub size: u64,
}

impl DeadEntry {
    /// Length of [`DeadEntry::encode`].
    pub const ENCODED_LEN: usize = 24;

    /// `epoch ‖ seq (zero once released) ‖ size`, big-endian.
    pub fn encode(&self) -> [u8; Self::ENCODED_LEN] {
        let mut out = [0u8; Self::ENCODED_LEN];
        out[..8].copy_from_slice(&self.epoch.to_be_bytes());
        out[8..16].copy_from_slice(&self.pending.unwrap_or(0).to_be_bytes());
        out[16..].copy_from_slice(&self.size.to_be_bytes());
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, StorageError> {
        let bytes: [u8; Self::ENCODED_LEN] = bytes
            .try_into()
            .map_err(|_| corrupt("malformed gc dead entry"))?;
        let word = |i: usize| u64::from_be_bytes(bytes[8 * i..8 * i + 8].try_into().expect("8"));
        let seq = word(1);
        Ok(Self {
            epoch: word(0),
            pending: (seq != 0).then_some(seq),
            size: word(2),
        })
    }
}

/// The durable garbage-collection state of one store, kept under
/// [`GC_STATE_KEY`] and rewritten by every GC commit.
///
/// Every counter is kept exact by the commits themselves, so a metrics
/// exporter reads them without walking the database.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GcState {
    /// [`ObjectGraph::version`] the counts were built with.
    pub graph_version: u32,
    /// An invariant failed; commits no longer count or delete anything
    /// until the counts are rebuilt.
    pub suspended: bool,
    /// Epoch of the last commit (the head's).
    pub epoch: u64,
    /// Next release-stack sequence number (starts at one).
    pub next_seq: u64,
    /// Objects present in the object tables, live or dead.
    pub stored_objects: u64,
    /// Their encoded bytes (values only).
    pub stored_bytes: u64,
    /// Present objects that are dead: pending release or queued.
    pub dead_objects: u64,
    /// Bytes of the queued (released) dead objects.
    pub queued_bytes: u64,
    /// Dead objects whose edges are not released yet.
    pub pending: u64,
    /// Released dead objects waiting for the horizon.
    pub queued: u64,
    /// Rows of the sparse count table (objects with count two or more).
    pub shared: u64,
    /// Rows of the phantom table (speculative references to absent ids).
    pub phantoms: u64,
    /// Objects inserted by GC commits since the counts were built.
    pub born_total: u64,
    /// Objects whose count reached zero.
    pub died_total: u64,
    /// Dead objects whose edges were released.
    pub released_total: u64,
    /// Dead objects deleted, and their bytes.
    pub deleted_total: u64,
    pub deleted_bytes_total: u64,
    /// Dead objects brought back by a later commit.
    pub resurrected_total: u64,
    /// Commits that released or deleted more than their budget because a
    /// backlog had built up.
    pub over_budget_total: u64,
}

const STATE_VERSION: u8 = 1;
const STATE_FIELDS: usize = 19;

impl GcState {
    /// A new state counted with `graph_version`, at epoch `epoch`.
    pub fn new(graph_version: u32, epoch: u64) -> Self {
        Self {
            graph_version,
            epoch,
            next_seq: 1,
            ..Self::default()
        }
    }

    /// Present objects that are live.
    pub fn live_objects(&self) -> u64 {
        self.stored_objects.saturating_sub(self.dead_objects)
    }

    /// Bytes of present objects that are not queued for deletion. Pending
    /// objects are included: their size is read only when they are
    /// released.
    pub fn live_bytes(&self) -> u64 {
        self.stored_bytes.saturating_sub(self.queued_bytes)
    }

    fn fields(&self) -> [u64; STATE_FIELDS] {
        [
            u64::from(self.graph_version),
            u64::from(self.suspended),
            self.epoch,
            self.next_seq,
            self.stored_objects,
            self.stored_bytes,
            self.dead_objects,
            self.queued_bytes,
            self.pending,
            self.queued,
            self.shared,
            self.phantoms,
            self.born_total,
            self.died_total,
            self.released_total,
            self.deleted_total,
            self.deleted_bytes_total,
            self.resurrected_total,
            self.over_budget_total,
        ]
    }

    /// `version byte ‖ 19 big-endian u64 fields`.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(1 + 8 * STATE_FIELDS);
        out.push(STATE_VERSION);
        for field in self.fields() {
            out.extend_from_slice(&field.to_be_bytes());
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, StorageError> {
        if bytes.len() != 1 + 8 * STATE_FIELDS || bytes[0] != STATE_VERSION {
            return Err(corrupt("malformed gc state record"));
        }
        let field =
            |i: usize| u64::from_be_bytes(bytes[1 + 8 * i..9 + 8 * i].try_into().expect("8"));
        let graph_version =
            u32::try_from(field(0)).map_err(|_| corrupt("gc graph version out of range"))?;
        let suspended = match field(1) {
            0 => false,
            1 => true,
            _ => return Err(corrupt("malformed gc suspended flag")),
        };
        Ok(Self {
            graph_version,
            suspended,
            epoch: field(2),
            next_seq: field(3),
            stored_objects: field(4),
            stored_bytes: field(5),
            dead_objects: field(6),
            queued_bytes: field(7),
            pending: field(8),
            queued: field(9),
            shared: field(10),
            phantoms: field(11),
            born_total: field(12),
            died_total: field(13),
            released_total: field(14),
            deleted_total: field(15),
            deleted_bytes_total: field(16),
            resurrected_total: field(17),
            over_budget_total: field(18),
        })
    }
}

/// What one commit asks the collector to do besides storing its objects.
///
/// Budgets are floors, not caps. A commit releases the edges of up to
/// `release_budget` dead objects (all of them if fewer are pending) and
/// deletes up to `delete_budget` deletable objects plus twice the number
/// that died in it. When the pending backlog at the start of the commit
/// already exceeds the release budget, the commit keeps releasing until
/// the backlog is below where it started; when more than sixteen times
/// the delete floor are deletable, the commit deletes a sixteenth of
/// them. So under any steady load the backlog stays within a fixed
/// multiple of one commit's work: commit time grows instead of the file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GcCommit {
    /// Roots of the new head (each named once per reference).
    pub acquire: Vec<(SpaceId, ObjectId)>,
    /// Roots of the head this commit replaces.
    pub release: Vec<(SpaceId, ObjectId)>,
    /// The lowest snapshot height a reader may still open: the durable
    /// floor before this commit, or a lower pinned snapshot. Objects that
    /// died at or before the epoch this height was committed in are
    /// deleted.
    pub horizon: u64,
    /// Release floor, in objects.
    pub release_budget: u64,
    /// Delete floor, in objects.
    pub delete_budget: u64,
    /// Count and release but delete nothing: a measurement mode that shows
    /// what counting costs on its own.
    pub count_only: bool,
}

/// What went wrong while planning a GC commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GcError {
    /// The store failed, or holds an object conflicting with the batch:
    /// the commit fails as it would without GC.
    Storage(StorageError),
    /// The counts are inconsistent with the objects. The store commits
    /// the batch without GC and suspends it.
    Invariant(String),
}

impl fmt::Display for GcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage(error) => error.fmt(f),
            Self::Invariant(detail) => write!(f, "gc invariant: {detail}"),
        }
    }
}

impl From<StorageError> for GcError {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}

fn invariant(detail: impl Into<String>) -> GcError {
    GcError::Invariant(detail.into())
}

/// Read access to a store's objects and GC tables as they were before the
/// commit being planned.
pub trait GcRead {
    /// `None` if absent, else whether the stored bytes equal `bytes`.
    fn object_matches(
        &mut self,
        space: SpaceId,
        id: &ObjectId,
        bytes: &[u8],
    ) -> Result<Option<bool>, StorageError>;
    fn object(&mut self, space: SpaceId, id: &ObjectId) -> Result<Option<Vec<u8>>, StorageError>;
    fn shared(&mut self, key: &GcKey) -> Result<Option<u64>, StorageError>;
    fn dead(&mut self, key: &GcKey) -> Result<Option<DeadEntry>, StorageError>;
    fn phantom(&mut self, key: &GcKey) -> Result<Option<u64>, StorageError>;
    /// The highest mapped height at or below `height`, with its epoch.
    fn epoch_at(&mut self, height: u64) -> Result<Option<(u64, u64)>, StorageError>;
    /// Mapped heights strictly below `height`, in order.
    fn epochs_below(&mut self, height: u64) -> Result<Vec<u64>, StorageError>;
    /// The first queue entry after `after` (from the start when `None`).
    fn queue_next(&mut self, after: Option<&QueueKey>) -> Result<Option<QueueKey>, StorageError>;
    /// The release-stack entry with the highest sequence below `below`.
    fn release_prev(&mut self, below: u64) -> Result<Option<(u64, GcKey)>, StorageError>;
    /// Queue entries counted per epoch, for epochs at or below `epoch`.
    fn epoch_counts_through(&mut self, epoch: u64) -> Result<Vec<(u64, u64)>, StorageError>;
    /// Queue entries counted for one epoch.
    fn epoch_count(&mut self, epoch: u64) -> Result<Option<u64>, StorageError>;
}

/// Write access used to apply a [`Plan`] inside the commit's transaction.
pub trait GcWrite {
    fn insert_object(
        &mut self,
        space: SpaceId,
        id: &ObjectId,
        bytes: &[u8],
    ) -> Result<(), StorageError>;
    fn remove_object(&mut self, space: SpaceId, id: &ObjectId) -> Result<(), StorageError>;
    fn set_shared(&mut self, key: &GcKey, count: Option<u64>) -> Result<(), StorageError>;
    fn set_dead(&mut self, key: &GcKey, entry: Option<DeadEntry>) -> Result<(), StorageError>;
    fn set_phantom(&mut self, key: &GcKey, count: Option<u64>) -> Result<(), StorageError>;
    fn set_queue(&mut self, key: &QueueKey, present: bool) -> Result<(), StorageError>;
    fn set_release(&mut self, seq: u64, key: Option<&GcKey>) -> Result<(), StorageError>;
    fn set_epoch(&mut self, height: u64, epoch: Option<u64>) -> Result<(), StorageError>;
    fn set_epoch_count(&mut self, epoch: u64, count: Option<u64>) -> Result<(), StorageError>;
}

/// The figures of one planned commit (not persisted; for metrics and
/// tests).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GcReport {
    pub born: u64,
    pub died: u64,
    pub released: u64,
    pub deleted: u64,
    pub deleted_bytes: u64,
    pub resurrected: u64,
    /// The horizon's epoch, if a height at or below the horizon is mapped.
    pub horizon_epoch: Option<u64>,
    /// Deletable queue entries when the commit started.
    pub eligible: u64,
    /// Released or deleted past the budget to shrink a backlog.
    pub over_budget: bool,
}

/// Every change one commit makes to the objects and GC tables.
#[derive(Debug, Default)]
pub struct Plan<'a> {
    /// Objects the batch stores that were absent.
    pub inserts: Vec<(SpaceId, ObjectId, &'a [u8])>,
    /// Dead objects deleted.
    pub removes: Vec<(SpaceId, ObjectId)>,
    shared: BTreeMap<GcKey, Option<u64>>,
    dead: BTreeMap<GcKey, Option<DeadEntry>>,
    phantom: BTreeMap<GcKey, Option<u64>>,
    queue: BTreeMap<QueueKey, bool>,
    release: BTreeMap<u64, Option<GcKey>>,
    epochs: BTreeMap<u64, Option<u64>>,
    epoch_counts: BTreeMap<u64, Option<u64>>,
    /// The state after the commit.
    pub state: GcState,
    pub report: GcReport,
}

impl Plan<'_> {
    /// Applies every change through `write`.
    pub fn apply(&self, write: &mut dyn GcWrite) -> Result<(), StorageError> {
        for (space, id, bytes) in &self.inserts {
            write.insert_object(*space, id, bytes)?;
        }
        for (space, id) in &self.removes {
            write.remove_object(*space, id)?;
        }
        for (key, count) in &self.shared {
            write.set_shared(key, *count)?;
        }
        for (key, entry) in &self.dead {
            write.set_dead(key, *entry)?;
        }
        for (key, count) in &self.phantom {
            write.set_phantom(key, *count)?;
        }
        for (key, present) in &self.queue {
            write.set_queue(key, *present)?;
        }
        for (seq, key) in &self.release {
            write.set_release(*seq, key.as_ref())?;
        }
        for (height, epoch) in &self.epochs {
            write.set_epoch(*height, *epoch)?;
        }
        for (epoch, count) in &self.epoch_counts {
            write.set_epoch_count(*epoch, *count)?;
        }
        Ok(())
    }
}

/// The status of one touched object while a commit is planned.
#[derive(Clone, Copy, Debug)]
enum Status {
    /// Present (or inserted by this commit) and not dead. `base` is the
    /// count before this commit once known (the phantom count for an
    /// object this commit inserts, zero for one it brings back), `delta`
    /// the change so far.
    Live { base: Option<u64>, delta: i64 },
    /// Dead before this commit or by it. `pending` is the base stack
    /// position while unreleased; `now` marks a death in this commit.
    Dead {
        epoch: u64,
        pending: Option<u64>,
        now: bool,
        size: u64,
    },
    /// Not present: references to it are phantoms.
    Absent { phantom: u64 },
}

#[derive(Clone, Copy, Debug)]
struct Touched {
    status: Status,
    /// Rows the store held before this commit, once read.
    base_dead: Option<DeadEntry>,
    base_shared: Option<Option<u64>>,
    base_phantom: Option<Option<u64>>,
    /// Present before this commit; `None` until checked.
    present: Option<bool>,
    /// Inserted by this commit.
    new: bool,
    /// A strict edge names it.
    strict: bool,
}

struct Planner<'r, 'b> {
    read: &'r mut dyn GcRead,
    graph: &'r dyn ObjectGraph,
    state: &'r GcState,
    touched: HashMap<GcKey, Touched>,
    /// Bytes of staged objects.
    staged: HashMap<GcKey, &'b [u8]>,
    /// Bytes read from the store during this commit.
    loaded: HashMap<GcKey, Vec<u8>>,
    epoch: u64,
    /// Objects that died in this commit and are not released yet (top
    /// last).
    stack: Vec<GcKey>,
    /// Base release-stack entries released or brought back.
    unstacked: BTreeSet<u64>,
    /// Base queue entries brought back.
    unqueued: BTreeMap<QueueKey, u64>,
    report: GcReport,
}

impl Planner<'_, '_> {
    /// The touched entry for `key`, reading its dead row on first touch.
    fn load(&mut self, key: &GcKey) -> Result<&mut Touched, GcError> {
        if let Entry::Vacant(slot) = self.touched.entry(*key) {
            let dead = self.read.dead(key)?;
            slot.insert(Touched {
                status: match dead {
                    Some(entry) => Status::Dead {
                        epoch: entry.epoch,
                        pending: entry.pending,
                        now: false,
                        size: entry.size,
                    },
                    None => Status::Live {
                        base: None,
                        delta: 0,
                    },
                },
                base_dead: dead,
                base_shared: None,
                base_phantom: None,
                // Only a present object has a dead row.
                present: dead.map(|_| true),
                new: false,
                strict: false,
            });
        }
        Ok(self.touched.get_mut(key).expect("loaded"))
    }

    fn touched(&mut self, key: &GcKey) -> &mut Touched {
        self.touched.get_mut(key).expect("loaded")
    }

    /// The bytes of a present object, staged or read once per commit.
    fn bytes(&mut self, key: &GcKey) -> Result<Vec<u8>, GcError> {
        if let Some(bytes) = self.staged.get(key) {
            return Ok(bytes.to_vec());
        }
        if let Some(bytes) = self.loaded.get(key) {
            return Ok(bytes.clone());
        }
        let (space, id) = split_key(key)?;
        let bytes = self
            .read
            .object(space, &id)?
            .ok_or_else(|| invariant("an object whose edges are needed is missing"))?;
        self.loaded.insert(*key, bytes.clone());
        Ok(bytes)
    }

    fn edges(&mut self, key: &GcKey) -> Result<(Vec<Edge>, u64), GcError> {
        let (space, _) = split_key(key)?;
        let bytes = self.bytes(key)?;
        let mut edges = Vec::new();
        self.graph
            .edges(space, &bytes, &mut edges)
            .map_err(|error| invariant(format!("object does not decode: {error}")))?;
        Ok((edges, bytes.len() as u64))
    }

    /// Brings back a dead object: its count restarts at zero, and a
    /// released one has its edges counted again (pushed on `work`).
    fn resurrect(&mut self, key: &GcKey, work: &mut Vec<GcKey>) {
        let touched = self.touched(key);
        let Status::Dead {
            epoch,
            pending,
            size,
            ..
        } = touched.status
        else {
            unreachable!("only a dead object is brought back")
        };
        touched.status = Status::Live {
            base: Some(0),
            delta: 0,
        };
        self.report.resurrected += 1;
        match pending {
            Some(seq) => {
                self.unstacked.insert(seq);
            }
            None => {
                self.unqueued.insert(queue_key(epoch, key), size);
                work.push(*key);
            }
        }
    }

    /// Whether `key` is present, and its count or phantom count before
    /// this commit, for a live-looking entry whose base is unknown.
    fn resolve(&mut self, key: &GcKey) -> Result<(), GcError> {
        let touched = self.touched[key];
        let Status::Live { base: None, delta } = touched.status else {
            return Ok(());
        };
        let present = match touched.present {
            Some(present) => present,
            None if self.staged.contains_key(key) || self.loaded.contains_key(key) => true,
            None => {
                // Read the bytes, not just presence: an object whose count
                // may reach zero is released next, which needs them.
                let (space, id) = split_key(key)?;
                match self.read.object(space, &id)? {
                    Some(bytes) => {
                        self.loaded.insert(*key, bytes);
                        true
                    }
                    None => false,
                }
            }
        };
        if present {
            let shared = self.read.shared(key)?;
            let touched = self.touched(key);
            touched.present = Some(true);
            touched.base_shared = Some(shared);
            touched.status = Status::Live {
                base: Some(shared.unwrap_or(1)),
                delta,
            };
            return Ok(());
        }
        if touched.strict {
            return Err(invariant("a strict reference names a missing object"));
        }
        let phantom = self.read.phantom(key)?;
        let count = phantom.unwrap_or(0) as i64 + delta;
        if count < 0 {
            return Err(invariant("a phantom count went below zero"));
        }
        let touched = self.touched(key);
        touched.present = Some(false);
        touched.base_phantom = Some(phantom);
        touched.status = Status::Absent {
            phantom: count as u64,
        };
        Ok(())
    }

    /// Adds one reference (increment phase).
    fn acquire(&mut self, edge: &Edge, work: &mut Vec<GcKey>) -> Result<(), GcError> {
        let key = gc_key(edge.space, &edge.id);
        let touched = self.load(&key)?;
        touched.strict |= edge.strict;
        if let Status::Dead { .. } = touched.status {
            self.resurrect(&key, work);
        }
        match &mut self.touched(&key).status {
            Status::Live { delta, .. } => *delta += 1,
            Status::Absent { phantom } => *phantom += 1,
            Status::Dead { .. } => unreachable!("brought back above"),
        }
        Ok(())
    }

    /// Removes one reference (release phase); a count reaching zero is a
    /// death in this commit's epoch.
    fn release(&mut self, edge: &Edge) -> Result<(), GcError> {
        let key = gc_key(edge.space, &edge.id);
        let touched = self.load(&key)?;
        touched.strict |= edge.strict;
        match &mut touched.status {
            Status::Dead { .. } => {
                return Err(invariant("a reference to a dead object was released"));
            }
            Status::Absent { phantom } => {
                *phantom = phantom
                    .checked_sub(1)
                    .ok_or_else(|| invariant("a phantom count went below zero"))?;
                return Ok(());
            }
            Status::Live { delta, .. } => *delta -= 1,
        }
        // A live object reaches zero only once releases exceed additions.
        if let Status::Live { base: None, delta } = self.touched[&key].status
            && delta < 0
        {
            self.resolve(&key)?;
        }
        let epoch = self.epoch;
        let touched = self.touched(&key);
        if let Status::Live {
            base: Some(base),
            delta,
        } = touched.status
        {
            let count = base as i64 + delta;
            if count < 0 {
                return Err(invariant("a reference count went below zero"));
            }
            if count == 0 {
                touched.status = Status::Dead {
                    epoch,
                    pending: None,
                    now: true,
                    size: 0,
                };
                self.report.died += 1;
                self.stack.push(key);
            }
        }
        Ok(())
    }

    /// Releases the edges of one dead object, which becomes queued.
    fn release_edges(&mut self, key: &GcKey) -> Result<(), GcError> {
        let (edges, len) = self.edges(key)?;
        for edge in &edges {
            self.release(edge)?;
        }
        if let Status::Dead { pending, size, .. } = &mut self.touched(key).status {
            *pending = None;
            *size = len;
        }
        self.report.released += 1;
        Ok(())
    }

    /// The next object to release: this commit's deaths first (deepest
    /// first), then the base stack, newest first.
    fn next_release(&mut self, cursor: &mut u64) -> Result<Option<GcKey>, GcError> {
        if let Some(key) = self.stack.pop() {
            return Ok(Some(key));
        }
        while let Some((seq, key)) = self.read.release_prev(*cursor)? {
            *cursor = seq;
            if self.unstacked.contains(&seq) {
                continue;
            }
            let touched = self.load(&key)?;
            if !matches!(touched.status, Status::Dead { pending: Some(found), .. } if found == seq)
            {
                return Err(invariant("release stack entry without a pending dead row"));
            }
            self.unstacked.insert(seq);
            return Ok(Some(key));
        }
        Ok(None)
    }

    fn pending_now(&self) -> u64 {
        (self.state.pending + self.stack.len() as u64).saturating_sub(self.unstacked.len() as u64)
    }
}

/// Plans one commit: stores `objects` (put if absent), counts the new
/// head's references, releases the old head's, cascades within the
/// budget, and deletes what the horizon allows. `height` is the new
/// head's height and `state` the store's GC state before the commit.
///
/// Reads only; the store applies the returned [`Plan`] in the same
/// transaction as the head.
pub fn plan<'b>(
    read: &mut dyn GcRead,
    graph: &dyn ObjectGraph,
    state: &GcState,
    objects: &'b [(SpaceId, ObjectId, Vec<u8>)],
    commit: &GcCommit,
    height: u64,
) -> Result<Plan<'b>, GcError> {
    if graph.version() != state.graph_version {
        return Err(GcError::Storage(StorageError::new(
            StorageErrorKind::Incompatible,
            format!(
                "store counted with object graph v{}, caller has v{}",
                state.graph_version,
                graph.version()
            ),
        )));
    }
    let epoch = state
        .epoch
        .checked_add(1)
        .ok_or_else(|| invariant("gc epoch exhausted"))?;
    let mut p = Planner {
        read,
        graph,
        state,
        touched: HashMap::new(),
        staged: HashMap::new(),
        loaded: HashMap::new(),
        epoch,
        stack: Vec::new(),
        unstacked: BTreeSet::new(),
        unqueued: BTreeMap::new(),
        report: GcReport::default(),
    };
    let mut plan = Plan::default();

    // 1. Staged objects: insert the absent ones, bring back the dead ones.
    // Objects that become live have their edges counted (`work`).
    let mut work: Vec<GcKey> = Vec::new();
    for (space, id, bytes) in objects {
        let key = gc_key(*space, id);
        if p.staged.insert(key, bytes).is_some() {
            continue;
        }
        match p.read.object_matches(*space, id, bytes)? {
            Some(false) => {
                return Err(GcError::Storage(crate::versioned::object_conflict(
                    *space, id,
                )));
            }
            None => {
                let phantom = if state.phantoms > 0 {
                    p.read.phantom(&key)?
                } else {
                    None
                };
                p.touched.insert(
                    key,
                    Touched {
                        status: Status::Live {
                            base: Some(phantom.unwrap_or(0)),
                            delta: 0,
                        },
                        base_dead: None,
                        base_shared: Some(None),
                        base_phantom: Some(phantom),
                        present: Some(false),
                        new: true,
                        strict: false,
                    },
                );
                plan.inserts.push((*space, *id, bytes.as_slice()));
                p.report.born += 1;
                work.push(key);
            }
            Some(true) => {
                let touched = p.load(&key)?;
                touched.present = Some(true);
                if let Status::Dead { .. } = touched.status {
                    p.resurrect(&key, &mut work);
                }
            }
        }
    }

    // 2. Increments: the new roots, then the edges of every object that
    // became live, transitively through released objects brought back.
    for (space, id) in &commit.acquire {
        let edge = Edge {
            space: *space,
            id: *id,
            strict: true,
        };
        p.acquire(&edge, &mut work)?;
    }
    while let Some(key) = work.pop() {
        for edge in p.edges(&key)?.0 {
            p.acquire(&edge, &mut work)?;
        }
    }

    // 3. Decrements: the old roots, then the release cascade.
    for (space, id) in &commit.release {
        let edge = Edge {
            space: *space,
            id: *id,
            strict: true,
        };
        p.release(&edge)?;
    }
    let budget = commit.release_budget;
    let pending_start = state.pending;
    let mut cursor = u64::MAX;
    loop {
        let released = p.report.released;
        let pending = p.pending_now();
        let shrink = pending > budget && pending >= pending_start;
        if released >= budget && !shrink {
            break;
        }
        let Some(key) = p.next_release(&mut cursor)? else {
            break;
        };
        p.release_edges(&key)?;
    }
    if p.report.released > budget {
        p.report.over_budget = true;
    }

    // 4. Deletion of queued objects that died at or before the horizon's
    // epoch.
    let horizon = p.read.epoch_at(commit.horizon)?;
    p.report.horizon_epoch = horizon.map(|(_, e)| e);
    let mut deleted: Vec<(GcKey, u64, u64)> = Vec::new();
    if let Some((_, horizon_epoch)) = horizon.filter(|_| !commit.count_only) {
        let eligible: u64 = p
            .read
            .epoch_counts_through(horizon_epoch)?
            .iter()
            .map(|(_, n)| n)
            .sum();
        p.report.eligible = eligible;
        let floor = commit
            .delete_budget
            .saturating_add(p.report.died.saturating_mul(2));
        if eligible / 16 > floor {
            p.report.over_budget = true;
        }
        let target = floor.max(eligible / 16);
        let mut after: Option<QueueKey> = None;
        while (deleted.len() as u64) < target {
            let Some(entry) = p.read.queue_next(after.as_ref())? else {
                break;
            };
            after = Some(entry);
            let (died, key) = split_queue_key(&entry);
            if died > horizon_epoch {
                break;
            }
            if p.unqueued.contains_key(&entry) {
                continue;
            }
            let touched = p.load(&key)?;
            let Status::Dead {
                epoch,
                pending: None,
                now: false,
                size,
            } = touched.status
            else {
                return Err(invariant("queue entry without a released dead row"));
            };
            if epoch != died {
                return Err(invariant("queue entry and dead row disagree on the epoch"));
            }
            deleted.push((key, died, size));
        }
    }

    // 5. The diff against the store.
    let mut next = *state;
    next.epoch = epoch;
    let mut epoch_delta: BTreeMap<u64, i64> = BTreeMap::new();
    let mut deleted_keys = HashSet::new();
    for (key, died, size) in &deleted {
        deleted_keys.insert(*key);
        let (space, id) = split_key(key)?;
        plan.removes.push((space, id));
        plan.dead.insert(*key, None);
        plan.queue.insert(queue_key(*died, key), false);
        *epoch_delta.entry(*died).or_default() -= 1;
        p.report.deleted += 1;
        p.report.deleted_bytes += size;
    }
    for seq in &p.unstacked {
        plan.release.insert(*seq, None);
    }
    for (entry, size) in &p.unqueued {
        plan.queue.insert(*entry, false);
        *epoch_delta.entry(split_queue_key(entry).0).or_default() -= 1;
        next.queued -= 1;
        next.queued_bytes -= size;
    }
    // Deaths still unreleased get stack positions, the last popped first
    // so that the next commit continues depth first.
    let mut new_seq: HashMap<GcKey, u64> = HashMap::new();
    for key in p.stack.clone() {
        new_seq.insert(key, next.next_seq);
        plan.release.insert(next.next_seq, Some(key));
        next.next_seq += 1;
    }
    let keys: Vec<GcKey> = p.touched.keys().copied().collect();
    for key in keys {
        if deleted_keys.contains(&key) {
            continue;
        }
        let touched = p.touched[&key];
        match touched.status {
            Status::Live { base, delta } => {
                if delta == 0 && touched.base_dead.is_none() && !touched.new {
                    continue;
                }
                if base.is_none() {
                    p.resolve(&key)?;
                }
                let touched = p.touched[&key];
                match touched.status {
                    Status::Live {
                        base: Some(base),
                        delta,
                    } => {
                        let count = base as i64 + delta;
                        if count <= 0 {
                            return Err(invariant("a live object ended with no reference"));
                        }
                        let want = (count >= 2).then_some(count as u64);
                        let had = match touched.base_shared {
                            Some(had) => had,
                            None => p.read.shared(&key)?,
                        };
                        if want != had {
                            plan.shared.insert(key, want);
                            next.shared =
                                next.shared + u64::from(want.is_some()) - u64::from(had.is_some());
                        }
                        if touched.new && touched.base_phantom.flatten().is_some() {
                            plan.phantom.insert(key, None);
                            next.phantoms -= 1;
                        }
                        if touched.base_dead.is_some() {
                            plan.dead.insert(key, None);
                        }
                    }
                    Status::Absent { phantom } => {
                        let want = (phantom > 0).then_some(phantom);
                        let had = touched.base_phantom.flatten();
                        if want != had {
                            plan.phantom.insert(key, want);
                            next.phantoms = next.phantoms + u64::from(want.is_some())
                                - u64::from(had.is_some());
                        }
                    }
                    Status::Live { base: None, .. } | Status::Dead { .. } => {
                        unreachable!("resolved above")
                    }
                }
            }
            Status::Absent { phantom } => {
                let want = (phantom > 0).then_some(phantom);
                let had = match touched.base_phantom {
                    Some(had) => had,
                    None => p.read.phantom(&key)?,
                };
                if want != had {
                    plan.phantom.insert(key, want);
                    next.phantoms =
                        next.phantoms + u64::from(want.is_some()) - u64::from(had.is_some());
                }
            }
            Status::Dead {
                epoch: died,
                pending,
                now,
                size,
            } => {
                let pending = if now {
                    new_seq.get(&key).copied()
                } else {
                    pending
                };
                let entry = DeadEntry {
                    epoch: died,
                    pending,
                    size,
                };
                if touched.base_dead != Some(entry) {
                    plan.dead.insert(key, Some(entry));
                }
                let was_queued = !now && touched.base_dead.is_some_and(|b| b.pending.is_none());
                if pending.is_none() && !was_queued {
                    // Released in this commit.
                    plan.queue.insert(queue_key(died, &key), true);
                    *epoch_delta.entry(died).or_default() += 1;
                    next.queued += 1;
                    next.queued_bytes += size;
                }
                if pending.is_some() && now {
                    next.pending += 1;
                }
                if now {
                    // It was live before this commit: a count row goes.
                    let had = match touched.base_shared {
                        Some(had) => had,
                        None if touched.new || touched.base_dead.is_some() => None,
                        None => p.read.shared(&key)?,
                    };
                    if had.is_some() {
                        plan.shared.insert(key, None);
                        next.shared -= 1;
                    }
                    if touched.new && touched.base_phantom.flatten().is_some() {
                        plan.phantom.insert(key, None);
                        next.phantoms -= 1;
                    }
                }
            }
        }
    }
    for (died, delta) in epoch_delta {
        if delta == 0 {
            continue;
        }
        let before = p.read.epoch_count(died)?.unwrap_or(0) as i64;
        let after = before + delta;
        if after < 0 {
            return Err(invariant("an epoch queue count went below zero"));
        }
        plan.epoch_counts
            .insert(died, (after > 0).then_some(after as u64));
    }
    plan.epochs.insert(height, Some(epoch));
    if let Some((mapped, _)) = horizon {
        for old in p.read.epochs_below(mapped)? {
            plan.epochs.insert(old, None);
        }
    }

    // 6. Counters.
    let report = p.report;
    let inserted_bytes: u64 = plan.inserts.iter().map(|(_, _, b)| b.len() as u64).sum();
    next.pending -= p.unstacked.len() as u64;
    next.stored_objects = next.stored_objects + report.born - report.deleted;
    next.stored_bytes = next.stored_bytes + inserted_bytes - report.deleted_bytes;
    next.dead_objects = next.dead_objects + report.died - report.resurrected - report.deleted;
    next.queued -= report.deleted;
    next.queued_bytes -= report.deleted_bytes;
    next.born_total += report.born;
    next.died_total += report.died;
    next.released_total += report.released;
    next.deleted_total += report.deleted;
    next.deleted_bytes_total += report.deleted_bytes;
    next.resurrected_total += report.resurrected;
    next.over_budget_total += u64::from(report.over_budget);
    plan.state = next;
    plan.report = report;
    Ok(plan)
}

/// The GC tables held in memory: the memory store's tables, and a full
/// dump of a disk store's for [`verify`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GcTables {
    pub shared: BTreeMap<GcKey, u64>,
    pub dead: BTreeMap<GcKey, DeadEntry>,
    pub phantom: BTreeMap<GcKey, u64>,
    pub queue: BTreeSet<QueueKey>,
    pub release: BTreeMap<u64, GcKey>,
    pub epochs: BTreeMap<u64, u64>,
    pub epoch_counts: BTreeMap<u64, u64>,
}

impl GcTables {
    pub fn epoch_at(&self, height: u64) -> Option<(u64, u64)> {
        self.epochs
            .range(..=height)
            .next_back()
            .map(|(h, e)| (*h, *e))
    }

    pub fn epochs_below(&self, height: u64) -> Vec<u64> {
        self.epochs.range(..height).map(|(h, _)| *h).collect()
    }

    pub fn queue_next(&self, after: Option<&QueueKey>) -> Option<QueueKey> {
        match after {
            None => self.queue.iter().next().copied(),
            Some(after) => self
                .queue
                .range((
                    std::ops::Bound::Excluded(*after),
                    std::ops::Bound::Unbounded,
                ))
                .next()
                .copied(),
        }
    }

    pub fn release_prev(&self, below: u64) -> Option<(u64, GcKey)> {
        self.release
            .range(..below)
            .next_back()
            .map(|(s, k)| (*s, *k))
    }

    pub fn epoch_counts_through(&self, epoch: u64) -> Vec<(u64, u64)> {
        self.epoch_counts
            .range(..=epoch)
            .map(|(e, n)| (*e, *n))
            .collect()
    }

    fn set<K: Ord, V>(map: &mut BTreeMap<K, V>, key: K, value: Option<V>) {
        match value {
            Some(value) => {
                map.insert(key, value);
            }
            None => {
                map.remove(&key);
            }
        }
    }

    pub fn set_shared(&mut self, key: &GcKey, count: Option<u64>) {
        Self::set(&mut self.shared, *key, count);
    }
    pub fn set_dead(&mut self, key: &GcKey, entry: Option<DeadEntry>) {
        Self::set(&mut self.dead, *key, entry);
    }
    pub fn set_phantom(&mut self, key: &GcKey, count: Option<u64>) {
        Self::set(&mut self.phantom, *key, count);
    }
    pub fn set_queue(&mut self, key: &QueueKey, present: bool) {
        if present {
            self.queue.insert(*key);
        } else {
            self.queue.remove(key);
        }
    }
    pub fn set_release(&mut self, seq: u64, key: Option<&GcKey>) {
        Self::set(&mut self.release, seq, key.copied());
    }
    pub fn set_epoch(&mut self, height: u64, epoch: Option<u64>) {
        Self::set(&mut self.epochs, height, epoch);
    }
    pub fn set_epoch_count(&mut self, epoch: u64, count: Option<u64>) {
        Self::set(&mut self.epoch_counts, epoch, count);
    }
}

// ---------------------------------------------------------------- seeding --

/// Kinds of seeding records, in sort order within one key.
const SEED_PRESENT: u8 = 0;
const SEED_ROOT: u8 = 1;
const SEED_STRICT: u8 = 2;
const SEED_SPECULATIVE: u8 = 3;
const SEED_RECORD: usize = GC_KEY_LEN + 1;
const SEED_BUCKETS: usize = 256;

/// Where seeding writes the counts it builds.
pub trait SeedSink {
    fn shared(&mut self, key: &GcKey, count: u64) -> Result<(), StorageError>;
    /// A present object nothing references: dead at the seeding epoch,
    /// pending the release of its own edges.
    fn dead(&mut self, key: &GcKey, entry: DeadEntry) -> Result<(), StorageError>;
    fn phantom(&mut self, key: &GcKey, count: u64) -> Result<(), StorageError>;
}

enum Buckets {
    Memory(Vec<[u8; SEED_RECORD]>),
    Files {
        dir: PathBuf,
        writers: Vec<BufWriter<std::fs::File>>,
    },
}

/// Builds the counts of an existing store from one scan of its objects.
///
/// Every edge of every present object is counted, plus each head root.
/// Present objects that end with no reference are dead at the seeding
/// epoch and pending: releasing them later removes their edges again, so
/// the store need not contain only live objects (an archive's history is
/// collected this way). An edge from a present object to an absent one is
/// a phantom if speculative and an error if strict.
///
/// With a spill directory, the edge records go to 256 bucket files there
/// (about 42 bytes per object and per edge) and are sorted one bucket at a
/// time, so memory stays near 1/256 of the records.
pub struct Seeder<'g> {
    graph: &'g dyn ObjectGraph,
    buckets: Buckets,
    edges: Vec<Edge>,
    stored_objects: u64,
    stored_bytes: u64,
}

fn seed_record(key: &GcKey, kind: u8) -> [u8; SEED_RECORD] {
    let mut out = [0u8; SEED_RECORD];
    out[..GC_KEY_LEN].copy_from_slice(key);
    out[GC_KEY_LEN] = kind;
    out
}

fn io_error(error: io::Error) -> StorageError {
    StorageError::new(StorageErrorKind::Io, format!("gc seeding scratch: {error}"))
}

impl<'g> Seeder<'g> {
    /// A seeder for a head with `roots`; `spill` names an empty scratch
    /// directory for bucket files, or `None` to keep everything in memory.
    pub fn new(
        graph: &'g dyn ObjectGraph,
        roots: &[(SpaceId, ObjectId)],
        spill: Option<&Path>,
    ) -> Result<Self, StorageError> {
        let buckets = match spill {
            None => Buckets::Memory(Vec::new()),
            Some(dir) => {
                std::fs::create_dir_all(dir).map_err(io_error)?;
                let writers = (0..SEED_BUCKETS)
                    .map(|i| {
                        std::fs::File::create(dir.join(format!("bucket-{i:02x}")))
                            .map(|file| BufWriter::with_capacity(64 * 1024, file))
                    })
                    .collect::<io::Result<_>>()
                    .map_err(io_error)?;
                Buckets::Files {
                    dir: dir.to_path_buf(),
                    writers,
                }
            }
        };
        let mut seeder = Self {
            graph,
            buckets,
            edges: Vec::new(),
            stored_objects: 0,
            stored_bytes: 0,
        };
        for (space, id) in roots {
            seeder.push(&gc_key(*space, id), SEED_ROOT)?;
        }
        Ok(seeder)
    }

    fn push(&mut self, key: &GcKey, kind: u8) -> Result<(), StorageError> {
        let record = seed_record(key, kind);
        match &mut self.buckets {
            Buckets::Memory(records) => records.push(record),
            Buckets::Files { writers, .. } => writers[usize::from(key[9])]
                .write_all(&record)
                .map_err(io_error)?,
        }
        Ok(())
    }

    /// Records one present object (any order).
    pub fn object(&mut self, space: SpaceId, id: &ObjectId, bytes: &[u8]) -> Result<(), GcError> {
        let key = gc_key(space, id);
        self.push(&key, SEED_PRESENT)?;
        self.stored_objects += 1;
        self.stored_bytes += bytes.len() as u64;
        let mut edges = std::mem::take(&mut self.edges);
        edges.clear();
        self.graph
            .edges(space, bytes, &mut edges)
            .map_err(|error| invariant(format!("object does not decode: {error}")))?;
        for edge in &edges {
            let kind = if edge.strict {
                SEED_STRICT
            } else {
                SEED_SPECULATIVE
            };
            self.push(&gc_key(edge.space, &edge.id), kind)?;
        }
        self.edges = edges;
        Ok(())
    }

    /// Sorts the records bucket by bucket and writes the counts. Returns
    /// the state of a store seeded at `epoch`.
    pub fn finish(self, sink: &mut dyn SeedSink, epoch: u64) -> Result<GcState, GcError> {
        let mut state = GcState::new(self.graph.version(), epoch);
        state.stored_objects = self.stored_objects;
        state.stored_bytes = self.stored_bytes;
        match self.buckets {
            Buckets::Memory(mut records) => {
                records.sort_unstable();
                merge(&records, sink, &mut state)?;
            }
            Buckets::Files { dir, writers } => {
                for writer in writers {
                    writer.into_inner().map_err(|e| io_error(e.into_error()))?;
                }
                for i in 0..SEED_BUCKETS {
                    let path = dir.join(format!("bucket-{i:02x}"));
                    let mut records = Vec::new();
                    let mut reader = BufReader::with_capacity(
                        1 << 20,
                        std::fs::File::open(&path).map_err(io_error)?,
                    );
                    let mut record = [0u8; SEED_RECORD];
                    loop {
                        match reader.read_exact(&mut record) {
                            Ok(()) => records.push(record),
                            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                            Err(e) => return Err(io_error(e).into()),
                        }
                    }
                    records.sort_unstable();
                    merge(&records, sink, &mut state)?;
                    drop(records);
                    std::fs::remove_file(&path).map_err(io_error)?;
                }
                let _ = std::fs::remove_dir(&dir);
            }
        }
        Ok(state)
    }
}

/// Counts one sorted run of seeding records into `sink`.
fn merge(
    records: &[[u8; SEED_RECORD]],
    sink: &mut dyn SeedSink,
    state: &mut GcState,
) -> Result<(), GcError> {
    let mut i = 0;
    while i < records.len() {
        let mut key = [0u8; GC_KEY_LEN];
        key.copy_from_slice(&records[i][..GC_KEY_LEN]);
        let (mut present, mut strict, mut speculative) = (false, 0u64, 0u64);
        while i < records.len() && records[i][..GC_KEY_LEN] == key {
            match records[i][GC_KEY_LEN] {
                SEED_PRESENT => present = true,
                SEED_ROOT | SEED_STRICT => strict += 1,
                _ => speculative += 1,
            }
            i += 1;
        }
        let count = strict + speculative;
        if present {
            if count >= 2 {
                sink.shared(&key, count)?;
                state.shared += 1;
            } else if count == 0 {
                let seq = state.next_seq;
                state.next_seq += 1;
                sink.dead(
                    &key,
                    DeadEntry {
                        epoch: state.epoch,
                        pending: Some(seq),
                        size: 0,
                    },
                )?;
                state.dead_objects += 1;
                state.pending += 1;
            }
        } else if strict > 0 {
            let (space, id) = split_key(&key)?;
            return Err(invariant(format!(
                "a strict reference names a missing object {space:?}/{:02x}{:02x}{:02x}{:02x}…",
                id[0], id[1], id[2], id[3]
            )));
        } else {
            sink.phantom(&key, speculative)?;
            state.phantoms += 1;
        }
    }
    Ok(())
}

impl SeedSink for GcTables {
    fn shared(&mut self, key: &GcKey, count: u64) -> Result<(), StorageError> {
        self.shared.insert(*key, count);
        Ok(())
    }
    fn dead(&mut self, key: &GcKey, entry: DeadEntry) -> Result<(), StorageError> {
        self.dead.insert(*key, entry);
        if let Some(seq) = entry.pending {
            self.release.insert(seq, *key);
        }
        Ok(())
    }
    fn phantom(&mut self, key: &GcKey, count: u64) -> Result<(), StorageError> {
        self.phantom.insert(*key, count);
        Ok(())
    }
}

// --------------------------------------------------------------- verifier --

/// Checks every invariant of a store's GC tables against its objects and
/// head roots; returns the first violation found.
///
/// - The *counted set* is every object reachable from the roots and from
///   the edges of pending dead objects. Each of its objects is present,
///   not dead, and has exactly the count that edges from the counted set
///   and from pending objects (plus root references) give it: a `shared`
///   row when two or more, none when one.
/// - Every present object is counted or dead; dead rows name present
///   objects outside the counted set. Pending rows match the release
///   stack, released rows the queue, and the per-epoch counts the queue.
/// - Phantom rows count exactly the speculative edges to absent objects.
/// - The state's counters match the tables.
pub fn verify(
    objects: &BTreeMap<GcKey, Vec<u8>>,
    tables: &GcTables,
    state: &GcState,
    graph: &dyn ObjectGraph,
    roots: &[(SpaceId, ObjectId)],
) -> Result<(), String> {
    let edges_of = |key: &GcKey| -> Result<Vec<Edge>, String> {
        let bytes = objects
            .get(key)
            .ok_or_else(|| format!("object {} missing", short(key)))?;
        let (space, _) = split_key(key).map_err(|e| e.to_string())?;
        let mut edges = Vec::new();
        graph.edges(space, bytes, &mut edges)?;
        Ok(edges)
    };
    let pending: BTreeMap<GcKey, u64> = tables
        .dead
        .iter()
        .filter_map(|(k, d)| d.pending.map(|seq| (*k, seq)))
        .collect();
    // Expected counts: references from roots, counted objects and pending
    // objects. Walk the counted set from the roots and pending edges.
    let mut expected: HashMap<GcKey, (u64, bool)> = HashMap::new();
    let mut counted: HashSet<GcKey> = HashSet::new();
    let mut work: Vec<GcKey> = Vec::new();
    let mut add = |key: GcKey, strict: bool, work: &mut Vec<GcKey>| {
        let entry = expected.entry(key).or_insert((0, false));
        entry.0 += 1;
        entry.1 |= strict;
        if objects.contains_key(&key) && counted.insert(key) {
            work.push(key);
        }
    };
    for (space, id) in roots {
        add(gc_key(*space, id), true, &mut work);
    }
    for key in pending.keys() {
        for edge in edges_of(key)? {
            add(gc_key(edge.space, &edge.id), edge.strict, &mut work);
        }
    }
    while let Some(key) = work.pop() {
        if tables.dead.contains_key(&key) {
            return Err(format!("counted object {} is dead", short(&key)));
        }
        for edge in edges_of(&key)? {
            add(gc_key(edge.space, &edge.id), edge.strict, &mut work);
        }
    }
    let mut phantoms = BTreeMap::new();
    for (key, (count, strict)) in &expected {
        if objects.contains_key(key) {
            let stored = tables.shared.get(key).copied();
            if stored == Some(1) || stored == Some(0) {
                return Err(format!("shared row of {} holds {stored:?}", short(key)));
            }
            let stored = stored.unwrap_or(1);
            if stored != *count {
                return Err(format!(
                    "object {} has count {stored}, expected {count}",
                    short(key)
                ));
            }
        } else if *strict {
            return Err(format!("strict reference to missing {}", short(key)));
        } else {
            phantoms.insert(*key, *count);
        }
    }
    if phantoms != tables.phantom {
        return Err(format!(
            "phantom rows {:?} differ from expected {:?}",
            tables.phantom.len(),
            phantoms.len()
        ));
    }
    for key in tables.shared.keys() {
        if !counted.contains(key) {
            return Err(format!("shared row for uncounted {}", short(key)));
        }
    }
    let mut queue = BTreeSet::new();
    let mut epoch_counts: BTreeMap<u64, u64> = BTreeMap::new();
    let mut queued_bytes = 0;
    for (key, dead) in &tables.dead {
        if !objects.contains_key(key) {
            return Err(format!("dead row for missing {}", short(key)));
        }
        if counted.contains(key) {
            return Err(format!("dead row for counted {}", short(key)));
        }
        match dead.pending {
            Some(seq) => {
                if tables.release.get(&seq) != Some(key) {
                    return Err(format!("pending {} not on the release stack", short(key)));
                }
            }
            None => {
                queue.insert(queue_key(dead.epoch, key));
                *epoch_counts.entry(dead.epoch).or_default() += 1;
                if dead.size != objects[key].len() as u64 {
                    return Err(format!("dead row of {} has the wrong size", short(key)));
                }
                queued_bytes += dead.size;
            }
        }
    }
    if queue != tables.queue {
        return Err("queue differs from the released dead rows".into());
    }
    if epoch_counts != tables.epoch_counts {
        return Err("per-epoch queue counts differ from the queue".into());
    }
    if tables.release.len() != pending.len() {
        return Err("release stack holds entries without a pending row".into());
    }
    for key in objects.keys() {
        if !counted.contains(key) && !tables.dead.contains_key(key) {
            return Err(format!("object {} is neither counted nor dead", short(key)));
        }
    }
    let bytes: u64 = objects.values().map(|b| b.len() as u64).sum();
    let checks = [
        ("stored objects", state.stored_objects, objects.len() as u64),
        ("stored bytes", state.stored_bytes, bytes),
        ("dead objects", state.dead_objects, tables.dead.len() as u64),
        ("pending", state.pending, pending.len() as u64),
        ("queued", state.queued, tables.queue.len() as u64),
        ("queued bytes", state.queued_bytes, queued_bytes),
        ("shared rows", state.shared, tables.shared.len() as u64),
        ("phantom rows", state.phantoms, tables.phantom.len() as u64),
    ];
    for (name, stored, actual) in checks {
        if stored != actual {
            return Err(format!("state {name} is {stored}, tables hold {actual}"));
        }
    }
    if tables.release.keys().any(|seq| *seq >= state.next_seq) {
        return Err("release sequence at or above next_seq".into());
    }
    Ok(())
}

fn short(key: &GcKey) -> String {
    match split_key(key) {
        Ok((space, id)) => format!(
            "{space:?}/{:02x}{:02x}{:02x}{:02x}",
            id[0], id[1], id[2], id[3]
        ),
        Err(_) => "<bad key>".into(),
    }
}

#[cfg(test)]
mod tests {
    //! The planner against a toy object graph over tiny domains, so shared
    //! children, resurrection, phantoms and backlogs are the common case.
    //! Every commit is checked by [`verify`] and by reading every snapshot
    //! the horizon still protects.
    use super::*;
    use std::hash::{Hash, Hasher};

    /// `kind ‖ n ‖ n child ids ‖ payload`: kind 0 children are strict,
    /// kind 1 children speculative (they may name nothing).
    struct Toy;

    impl ObjectGraph for Toy {
        fn version(&self) -> u32 {
            7
        }
        fn edges(&self, space: SpaceId, bytes: &[u8], out: &mut Vec<Edge>) -> Result<(), String> {
            let (kind, n) = (bytes[0], bytes[1] as usize);
            for i in 0..n {
                let mut id = [0u8; 32];
                id.copy_from_slice(&bytes[2 + 32 * i..34 + 32 * i]);
                out.push(Edge {
                    space,
                    id,
                    strict: kind == 0,
                });
            }
            Ok(())
        }
    }

    fn id_of(bytes: &[u8]) -> ObjectId {
        let mut id = [0u8; 32];
        for (i, chunk) in id.chunks_mut(8).enumerate() {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            (i, bytes).hash(&mut hasher);
            chunk.copy_from_slice(&hasher.finish().to_be_bytes());
        }
        id
    }

    fn object(kind: u8, children: &[ObjectId], payload: u8) -> Vec<u8> {
        let mut bytes = vec![kind, children.len() as u8];
        for child in children {
            bytes.extend_from_slice(child);
        }
        bytes.push(payload);
        bytes
    }

    #[derive(Default)]
    struct Toystore {
        objects: BTreeMap<GcKey, Vec<u8>>,
        tables: GcTables,
        state: GcState,
    }

    impl GcRead for Toystore {
        fn object_matches(
            &mut self,
            space: SpaceId,
            id: &ObjectId,
            bytes: &[u8],
        ) -> Result<Option<bool>, StorageError> {
            Ok(self
                .objects
                .get(&gc_key(space, id))
                .map(|b| b.as_slice() == bytes))
        }
        fn object(
            &mut self,
            space: SpaceId,
            id: &ObjectId,
        ) -> Result<Option<Vec<u8>>, StorageError> {
            Ok(self.objects.get(&gc_key(space, id)).cloned())
        }
        fn shared(&mut self, key: &GcKey) -> Result<Option<u64>, StorageError> {
            Ok(self.tables.shared.get(key).copied())
        }
        fn dead(&mut self, key: &GcKey) -> Result<Option<DeadEntry>, StorageError> {
            Ok(self.tables.dead.get(key).copied())
        }
        fn phantom(&mut self, key: &GcKey) -> Result<Option<u64>, StorageError> {
            Ok(self.tables.phantom.get(key).copied())
        }
        fn epoch_at(&mut self, height: u64) -> Result<Option<(u64, u64)>, StorageError> {
            Ok(self.tables.epoch_at(height))
        }
        fn epochs_below(&mut self, height: u64) -> Result<Vec<u64>, StorageError> {
            Ok(self.tables.epochs_below(height))
        }
        fn queue_next(
            &mut self,
            after: Option<&QueueKey>,
        ) -> Result<Option<QueueKey>, StorageError> {
            Ok(self.tables.queue_next(after))
        }
        fn release_prev(&mut self, below: u64) -> Result<Option<(u64, GcKey)>, StorageError> {
            Ok(self.tables.release_prev(below))
        }
        fn epoch_counts_through(&mut self, epoch: u64) -> Result<Vec<(u64, u64)>, StorageError> {
            Ok(self.tables.epoch_counts_through(epoch))
        }
        fn epoch_count(&mut self, epoch: u64) -> Result<Option<u64>, StorageError> {
            Ok(self.tables.epoch_counts.get(&epoch).copied())
        }
    }

    impl GcWrite for Toystore {
        fn insert_object(
            &mut self,
            space: SpaceId,
            id: &ObjectId,
            bytes: &[u8],
        ) -> Result<(), StorageError> {
            assert!(
                self.objects
                    .insert(gc_key(space, id), bytes.to_vec())
                    .is_none()
            );
            Ok(())
        }
        fn remove_object(&mut self, space: SpaceId, id: &ObjectId) -> Result<(), StorageError> {
            assert!(self.objects.remove(&gc_key(space, id)).is_some());
            Ok(())
        }
        fn set_shared(&mut self, key: &GcKey, count: Option<u64>) -> Result<(), StorageError> {
            self.tables.set_shared(key, count);
            Ok(())
        }
        fn set_dead(&mut self, key: &GcKey, entry: Option<DeadEntry>) -> Result<(), StorageError> {
            self.tables.set_dead(key, entry);
            Ok(())
        }
        fn set_phantom(&mut self, key: &GcKey, count: Option<u64>) -> Result<(), StorageError> {
            self.tables.set_phantom(key, count);
            Ok(())
        }
        fn set_queue(&mut self, key: &QueueKey, present: bool) -> Result<(), StorageError> {
            self.tables.set_queue(key, present);
            Ok(())
        }
        fn set_release(&mut self, seq: u64, key: Option<&GcKey>) -> Result<(), StorageError> {
            self.tables.set_release(seq, key);
            Ok(())
        }
        fn set_epoch(&mut self, height: u64, epoch: Option<u64>) -> Result<(), StorageError> {
            self.tables.set_epoch(height, epoch);
            Ok(())
        }
        fn set_epoch_count(&mut self, epoch: u64, count: Option<u64>) -> Result<(), StorageError> {
            self.tables.set_epoch_count(epoch, count);
            Ok(())
        }
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    /// The objects one head reaches through strict edges.
    fn closure(
        universe: &BTreeMap<ObjectId, Vec<u8>>,
        roots: &[ObjectId],
    ) -> BTreeMap<ObjectId, Vec<u8>> {
        let mut out = BTreeMap::new();
        let mut work: Vec<ObjectId> = roots.to_vec();
        while let Some(id) = work.pop() {
            let bytes = &universe[&id];
            if out.insert(id, bytes.clone()).is_some() {
                continue;
            }
            if bytes[0] == 0 {
                let mut edges = Vec::new();
                Toy.edges(SpaceId::Host, bytes, &mut edges).unwrap();
                work.extend(edges.iter().map(|e| e.id));
            }
        }
        out
    }

    const SPACE: SpaceId = SpaceId::Namespace(3);

    /// Runs `steps` commits of random heads over a universe of `leaves`
    /// leaves; returns the store for further checks.
    fn run(seed: u64, steps: usize, leaves: u8, window: u64, budgets: (u64, u64)) -> Toystore {
        let mut rng = Rng(seed);
        let mut universe: BTreeMap<ObjectId, Vec<u8>> = BTreeMap::new();
        let mut ids: Vec<ObjectId> = Vec::new();
        for payload in 0..leaves {
            let bytes = object(0, &[], payload);
            let id = id_of(&bytes);
            universe.insert(id, bytes);
            ids.push(id);
        }
        let mut store = Toystore {
            state: GcState::new(Toy.version(), 0),
            ..Toystore::default()
        };
        let mut heads: BTreeMap<u64, Vec<ObjectId>> = BTreeMap::new();
        let mut floor = 0u64;
        let mut pins: Vec<u64> = Vec::new();
        let mut previous: Vec<ObjectId> = Vec::new();
        for height in 0..steps as u64 {
            // Grow the universe with parents over few existing objects (so
            // equal objects recur) and speculative objects naming ids that
            // may not exist yet.
            for _ in 0..rng.below(3) {
                let n = 1 + rng.below(3);
                let children: Vec<ObjectId> = (0..n).map(|_| ids[rng.below(ids.len())]).collect();
                let kind = u8::from(rng.below(5) == 0);
                let children = if kind == 1 && rng.below(2) == 0 {
                    // A reference to an object that is not (yet) built.
                    vec![id_of(&object(0, &[], 200 + rng.below(4) as u8))]
                } else {
                    children
                };
                let bytes = object(kind, &children, rng.below(2) as u8);
                let id = id_of(&bytes);
                if universe.insert(id, bytes).is_none() {
                    ids.push(id);
                }
            }
            if rng.below(4) == 0 {
                let bytes = object(0, &[], 200 + rng.below(4) as u8);
                let id = id_of(&bytes);
                if universe.insert(id, bytes).is_none() {
                    ids.push(id);
                }
            }
            let roots: Vec<ObjectId> = if rng.below(6) == 0 {
                previous.clone()
            } else {
                (0..1 + rng.below(3))
                    .map(|_| ids[rng.below(ids.len())])
                    .collect()
            };
            // Stage what the new head reaches and the store does not hold
            // live, plus some live objects again.
            let reach = closure(&universe, &roots);
            let mut objects = Vec::new();
            for (id, bytes) in &reach {
                let key = gc_key(SPACE, id);
                let live =
                    store.objects.contains_key(&key) && !store.tables.dead.contains_key(&key);
                if !live || rng.below(3) == 0 {
                    objects.push((SPACE, *id, bytes.clone()));
                }
            }
            let horizon = pins.iter().copied().chain([floor]).min().unwrap();
            let commit = GcCommit {
                acquire: roots.iter().map(|id| (SPACE, *id)).collect(),
                release: previous.iter().map(|id| (SPACE, *id)).collect(),
                horizon,
                release_budget: budgets.0,
                delete_budget: budgets.1,
                count_only: false,
            };
            let state = store.state;
            let plan = plan(&mut store, &Toy, &state, &objects, &commit, height)
                .unwrap_or_else(|e| panic!("seed {seed} height {height}: {e}"));
            plan.apply(&mut store).unwrap();
            store.state = plan.state;
            heads.insert(height, roots.clone());
            previous = roots;
            verify(
                &store.objects,
                &store.tables,
                &store.state,
                &Toy,
                &previous.iter().map(|id| (SPACE, *id)).collect::<Vec<_>>(),
            )
            .unwrap_or_else(|e| panic!("seed {seed} height {height}: {e}"));
            // Every snapshot the horizon protects is still whole.
            for (h, roots) in heads.range(horizon..) {
                for id in closure(&universe, roots).keys() {
                    assert!(
                        store.objects.contains_key(&gc_key(SPACE, id)),
                        "seed {seed}: snapshot {h} lost an object at height {height}"
                    );
                }
            }
            floor = floor.max(height.saturating_sub(window));
            if rng.below(4) == 0 {
                pins.push(floor + rng.below((height - floor + 1) as usize) as u64);
            }
            if !pins.is_empty() && rng.below(3) == 0 {
                pins.remove(rng.below(pins.len()));
            }
            pins.retain(|pin| *pin + 20 > height);
        }
        store
    }

    #[test]
    fn random_heads_over_tiny_domains_keep_every_invariant() {
        let mut totals = GcState::default();
        for seed in 0..300 {
            let window = [0, 1, 2, 5][seed as usize % 4];
            let budgets = [(0, 0), (1, 1), (3, 2), (1_000, 1_000)][seed as usize / 4 % 4];
            let state = run(seed, 60, 3 + (seed % 4) as u8, window, budgets).state;
            totals.resurrected_total += state.resurrected_total;
            totals.deleted_total += state.deleted_total;
            totals.over_budget_total += state.over_budget_total;
            totals.shared += state.shared;
            totals.phantoms += state.phantoms;
            totals.pending += state.pending;
        }
        // The domains are small enough that every mechanism is exercised.
        for (name, value) in [
            ("resurrected", totals.resurrected_total),
            ("deleted", totals.deleted_total),
            ("over budget", totals.over_budget_total),
            ("shared rows", totals.shared),
            ("phantom rows", totals.phantoms),
            ("pending", totals.pending),
        ] {
            assert!(value > 0, "no {name}: {totals:?}");
        }
    }

    #[test]
    fn garbage_is_deleted_once_the_horizon_passes() {
        let store = run(42, 200, 4, 2, (1_000, 1_000));
        // With a window of two and generous budgets only recent garbage is
        // left, however many objects were born.
        assert!(store.state.born_total > 50, "{:?}", store.state);
        assert!(store.state.deleted_total > 0);
        assert!(store.state.dead_objects < store.state.born_total / 2);
        assert_eq!(store.state.pending, 0);
    }

    /// One wide object dies each commit; a zero budget still releases and
    /// deletes, and the backlog does not grow without bound.
    #[test]
    fn a_zero_budget_is_a_floor_and_the_backlog_stays_bounded() {
        let mut store = Toystore {
            state: GcState::new(Toy.version(), 0),
            ..Toystore::default()
        };
        let mut previous: Vec<ObjectId> = Vec::new();
        let mut worst = 0;
        for height in 0..400u64 {
            // A root over 8 fresh leaves: 9 objects born, 9 die next commit.
            let leaves: Vec<Vec<u8>> = (0..8)
                .map(|i| {
                    let mut bytes = object(0, &[], i);
                    bytes.extend_from_slice(&height.to_be_bytes());
                    bytes
                })
                .collect();
            let leaf_ids: Vec<ObjectId> = leaves.iter().map(|b| id_of(b)).collect();
            let root = object(0, &leaf_ids, 0);
            let root_id = id_of(&root);
            let mut objects: Vec<_> = leaves
                .into_iter()
                .zip(&leaf_ids)
                .map(|(b, id)| (SPACE, *id, b))
                .collect();
            objects.push((SPACE, root_id, root));
            let commit = GcCommit {
                acquire: vec![(SPACE, root_id)],
                release: previous.iter().map(|id| (SPACE, *id)).collect(),
                horizon: height.saturating_sub(1),
                release_budget: 0,
                delete_budget: 0,
                count_only: false,
            };
            let state = store.state;
            let plan = plan(&mut store, &Toy, &state, &objects, &commit, height).unwrap();
            plan.apply(&mut store).unwrap();
            store.state = plan.state;
            previous = vec![root_id];
            worst = worst.max(store.state.pending + store.state.dead_objects);
            verify(
                &store.objects,
                &store.tables,
                &store.state,
                &Toy,
                &[(SPACE, root_id)],
            )
            .unwrap();
        }
        assert!(worst < 100, "backlog grew to {worst}");
        assert!(store.state.stored_objects < 60, "{:?}", store.state);
    }

    #[test]
    fn seeding_counts_present_objects_and_marks_garbage_dead() {
        let mut rng = Rng(9);
        let mut universe = BTreeMap::new();
        let mut ids = Vec::new();
        for payload in 0..5u8 {
            let bytes = object(0, &[], payload);
            ids.push(id_of(&bytes));
            universe.insert(id_of(&bytes), bytes);
        }
        for _ in 0..40 {
            let children: Vec<ObjectId> = (0..1 + rng.below(3))
                .map(|_| ids[rng.below(ids.len())])
                .collect();
            let kind = u8::from(rng.below(6) == 0);
            let bytes = object(kind, &children, rng.below(3) as u8);
            ids.push(id_of(&bytes));
            universe.insert(id_of(&bytes), bytes);
        }
        let ghost = id_of(&object(0, &[], 99));
        let speculative = object(1, &[ghost], 1);
        universe.insert(id_of(&speculative), speculative.clone());
        ids.push(id_of(&speculative));
        let roots = vec![
            (SPACE, ids[ids.len() - 1]),
            (SPACE, ids[ids.len() - 2]),
            (SPACE, ids[ids.len() - 2]),
        ];
        let dir = std::env::temp_dir().join(format!("gc-seed-{}", std::process::id()));
        for spill in [None, Some(dir.as_path())] {
            let mut seeder = Seeder::new(&Toy, &roots, spill).unwrap();
            let mut objects = BTreeMap::new();
            for (id, bytes) in &universe {
                seeder.object(SPACE, id, bytes).unwrap();
                objects.insert(gc_key(SPACE, id), bytes.clone());
            }
            let mut tables = GcTables::default();
            let state = seeder.finish(&mut tables, 1).unwrap();
            tables.epochs.insert(10, 1);
            assert!(state.pending > 0 && state.phantoms == 1, "{state:?}");
            verify(&objects, &tables, &state, &Toy, &roots).unwrap();
            // Commits then release and delete the garbage.
            let mut store = Toystore {
                objects,
                tables,
                state,
            };
            for height in 11..30 {
                let commit = GcCommit {
                    acquire: roots.clone(),
                    release: roots.clone(),
                    horizon: height - 1,
                    release_budget: 2,
                    delete_budget: 2,
                    count_only: false,
                };
                let state = store.state;
                let plan = plan(&mut store, &Toy, &state, &[], &commit, height).unwrap();
                plan.apply(&mut store).unwrap();
                store.state = plan.state;
                verify(&store.objects, &store.tables, &store.state, &Toy, &roots).unwrap();
            }
            assert_eq!(store.state.dead_objects, 0, "{:?}", store.state);
        }
        assert!(!dir.exists(), "seeding removes its scratch files");
    }

    #[test]
    fn records_round_trip() {
        let state = GcState {
            graph_version: 3,
            suspended: true,
            epoch: 9,
            over_budget_total: 77,
            ..GcState::new(3, 9)
        };
        assert_eq!(GcState::decode(&state.encode()), Ok(state));
        let dead = DeadEntry {
            epoch: 4,
            pending: Some(8),
            size: 12,
        };
        assert_eq!(DeadEntry::decode(&dead.encode()), Ok(dead));
        for space in [
            SpaceId::Catalog,
            SpaceId::Host,
            SpaceId::Namespace(u64::MAX),
        ] {
            assert_eq!(split_key(&gc_key(space, &[7; 32])), Ok((space, [7; 32])));
        }
        assert!(gc_key(SpaceId::Host, &[0; 32]) < gc_key(SpaceId::Namespace(0), &[0; 32]));
    }
}
