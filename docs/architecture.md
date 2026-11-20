# Architecture

## Scope and reference

The upstream [Golem-DB requirements](https://github.com/Arkiv-Network/arkiv-source-of-truth/blob/main/golem-db.md)
are the reference. This repository's [single-block contract](single-block-query-proofs.md)
and [namespace contract](project-namespaces.md) record the direction taken
here. The engine prioritizes Arkiv's block workload and deterministic
execution; block-facing APIs map block numbers to internal snapshots.
Consensus and authorization are host responsibilities.

The block engine provides project namespaces, each with a raw byte map and
typed records, immutable snapshots with branches, checkpoints and atomic
publication, deterministic metering, per-namespace term indexes, and a
declared query subset with keyset pagination pinned to one snapshot. The engine keeps its state in Merkle
Patricia maps (`arkiv-db-trie`) and every state has a logical state root.
Proofs cover complete [equality pages](eq-proof-charges.md) and
[bounded complete ranges](range-proofs.md).

The ordinary `Engine` keeps state, history and indexes in memory. The
separate `durable::DurableEngine` stores the same state through the
versioned storage contract (`arkiv_db_storage::versioned`) and its redb
adapter (`arkiv-db-redb`); see the [durable lifecycle](engine-storage-run.md),
the [engine measurements](engine-storage-v1-report.md) and the
[redb experiment](redb-experiment.md). Durable publication distinguishes
committed, definitely absent and indeterminate outcomes; a memory
publication is in-process visibility only.

Of the node runtimes, the [full/light demo](full-light-demo.md) is volatile;
the [simulator](simulator-node.md) and the
[signed proposer](signed-proposer-v1.md) have durable storage and restart
without replay.

## Crate boundaries

```text
arkiv-db
  Engine / Branch / Snapshot ---> arkiv-db-trie::MerkleMap (every engine map)
                            \---> arkiv-db-storage::Entry   (scan result type)
                            \---> arkiv-db-storage::versioned::{StorageError, ObjectReader, SpaceId}
  DurableEngine ----------------> arkiv-db-storage::versioned::VersionedStore
  codec, state root ------------> alloy-primitives (keccak256, B256)
arkiv-db-memory
  MemoryVersionedStore ---------> arkiv-db-storage::versioned::VersionedStore
arkiv-db-redb
  RedbStore --------------------> arkiv-db-storage::versioned::VersionedStore
                            \---> redb 4.3.0 (pinned), alloy-primitives (keccak256)
  CountingBackend, testing::CrashDisk ---> redb::StorageBackend
arkiv-db-trie                     (no dependency on arkiv-db or arkiv-db-storage)
  MerkleMap / walk / order -----> alloy-trie::nodes::RlpNode, EMPTY_ROOT_HASH
                            \---> alloy-primitives (keccak256, B256), alloy-rlp
  NodeLoader (own trait)          (no dependency on arkiv-db or arkiv-db-storage)
arkiv-node
  block / params / validate / select / identity --> arkiv-db::codec (Encode, Decode, digest)
  exec::execute_block ----------> arkiv-db::{Branch try_ API, try_expire_records}
  replay::MemoryChain ----------> arkiv-db::Engine (memory)
  arkiv / gas / accounts / retention   (the Arkiv entity host: entities, rent, GLM accounts, serving floor)
  genesis_*                            (genesis authoring, bundles, streaming and bulk prefill, adoption)
  demo (arkiv-demo) ------------> arkiv-db::Engine (memory), Ed25519, HTTP
  simulator (arkiv-simulator) --> arkiv-db::durable::DurableEngine, arkiv-db-redb
  signed (arkiv-proposer) ------> arkiv-db::durable::DurableEngine, arkiv-db-redb, k256, JSON-RPC/REST
```

- `arkiv-db-storage` owns the versioned storage contract (`versioned`):
  content-addressed immutable objects per space (catalog, host, one space
  per namespace), insert-only block/manifest/inclusion/index/metadata records and one head,
  committed atomically by compare-and-set with the outcomes `Committed`,
  `NotCommitted` or `Indeterminate`, store health that latches failures
  until `recover()`, and `reconcile` for indeterminate commits. Its
  `testing` module (behind the `testing` feature) holds `FaultyStore`, whose latching mode follows
  `RedbStore`'s rule, and the behavioural suite every implementation runs.
  The crate root also defines `Entry`, the owned key/value pair a scan
  returns. The crate is standard-library-only.
- `arkiv-db-memory` holds `MemoryVersionedStore`, the volatile reference
  implementation of the versioned contract. It has no record, cell, query,
  or host knowledge and is standard-library-only.
- `arkiv-db-redb` implements the versioned contract over one redb file
  (`RedbStore`): one redb transaction per commit with immediate durability
  and quick repair, one object table per namespace, a fresh read transaction
  per read, content hashes verified on read, a swappable database handle
  that latches I/O failures, failed commits and failed recoveries until
  `recover()`, and new files built under a temporary name and renamed into
  place. `CountingBackend` counts backend I/O and forwards redb's file
  locks; `testing::CrashDisk` simulates power loss, torn writes, I/O errors,
  a sync failure that loses its writes, and a full disk under two space
  models. It is not
  standard-library-only (redb uses `libc`). See the
  [redb experiment](redb-experiment.md).
- `arkiv-db-trie` is the persistent Merkle Patricia map every engine map is
  built from: Ethereum MPT node encoding,
  canonical shape (root = function of the key/value set), `Mem` and
  `Stored` links loaded lazily and hash-verified through a `NodeLoader`
  argument (handles hold no store), a staging walk that emits the nodes
  built since the last commit (every object the store lacks, nothing
  outside the new trie), lazy point-proof generation, the strict verifier
  `walk`, and order-preserving fixed-width key encoders. It defines its own
  loader and error types so that it depends on neither the engine nor the
  storage contract. See [the state trie notes](state-trie.md).
- `arkiv-db` provides the block engine in the `engine`,
  `meter`, `namespace`, `record`, `query`, `state`, `host` and the private
  `typed`, `layout` (map key and leaf encodings) and `nodes` (the loader
  seam) modules, the canonical binary codec in `codec`,
  the durable facade/cache/format/lifecycle in `durable`, and end-to-end
  tests and examples. The common engine types are re-exported from the crate
  root; durable lifecycle types live under `arkiv_db::durable`. Its only
  dependency outside the workspace is
  `alloy-primitives`, for `keccak256` and `B256`; the trie crate brings the
  vendored `alloy-trie` and `alloy-rlp`.

- `arkiv-node` is the host above the engine: the block format (genesis,
  headers, inputs, envelopes, outcomes) with canonical encodings and hashes,
  the genesis invariants and the expiry reservation, static validation of
  requests, block selection, request identities with the pending/inclusion
  index, `execute_block` (admin inputs, the expiry sweep over a due schedule
  in the engine's host-state map, then user transactions with
  per-transaction checkpoints), and `MemoryChain`/`replay` on the memory
  engine. It reaches engine state only through the fallible `try_` API; its
  `clippy.toml` bans the infallible forms. See the
  [block format](block-format.md#part-2-block-format). On top of that it
  holds the Arkiv entity host (`arkiv`, `gas`, `accounts`, `retention`),
  genesis authoring and prefill (`genesis_*`), and three runtimes, each with
  its own binary: the volatile `demo` (Ed25519 development authentication),
  the unsigned durable `simulator`, and `signed`, the durable single-proposer
  node with full and light followers, JSON-RPC and REST.

Keep future modules in an existing crate until there is a concrete responsibility
worth separating. Metering, namespaces, records, indexes and queries live as
modules of `arkiv-db`; do not add empty crates for history or storage ahead
of their code. `arkiv-db-redb` is separate because it carries the only
dependency on redb and `libc`, which the engine and the contracts must not
need. The trie is a separate crate because it has working code and tests of
its own and must not depend on the engine or the storage contract. The node
is a separate crate because it is the host: consensus-facing formats,
admission, expiry scheduling and block assembly stay outside the engine,
which keeps no notion of blocks, requests or time.

## Raw byte map semantics

Each namespace holds a raw byte map beside its typed records.

- Keys and values are byte strings, including empty strings and embedded
  zero bytes. These are **storage keys**, not the fixed-width record
  identities in the higher-level Golem-DB model.
- A point read returns `None` for a missing key. An empty value is present.
- A put inserts or replaces a value; deleting a missing key succeeds.
- Operations apply in submission order, so the last operation for a key
  wins.
- A prefix scan returns only matching keys, sorted by unsigned bytewise
  lexicographic order, up to its budget. An empty prefix matches every key
  of the namespace. Results are owned, so subsequent writes do not change an
  earlier result.
- Keys and scan prefixes have a hard 20-byte local limit; their namespace is
  a separate fixed-width ID. Oversized inputs fail admission before
  traversal.

The engine's typed records have their own key rules (see below).

## Engine layer

### Why per-namespace persistent maps

A snapshot layer over a single-version, mutable key-value store would need a
full copy per publication (O(database size)) or an overlay plus undo log.
The engine instead keeps all of its state in `arkiv_db_trie::MerkleMap`:

- Cloning a map is O(1), so a checkpoint, the per-operation rollback copy, an
  estimation scratch copy and a published snapshot each cost a few pointer
  copies rather than a copy of the data.
- An insert or remove rebuilds only the nodes on its key's path (a path
  holds at most `2L + 1` nodes for `L`-byte keys, and an update creates at
  most `2L + 2`) and leaves every earlier version untouched, so retained
  snapshots share all unchanged nodes. Removing an absent key allocates
  nothing.
- `range_from` is lazy: a scan, a posting traversal or a run over term keys
  visits nodes only as entries are requested and stops at its budget without
  materializing the rest, which is what lets the meter charge each entry
  before it is copied out or probed.
- The shape is canonical, so each map's root hash is a function of its
  contents alone, and every node a lookup reads can be loaded lazily from
  committed storage through a loader.

The engine uses `arkiv_db_storage::Entry` as the scan result type.
`DurableEngine` places persistence beneath the engine's state: durable
immutable objects, historical manifests, fallible lazy reads and atomic
block/head publication go through the versioned storage contract.

### State layout

```text
Engine
  instance: u64                          // process-wide engine identity (cursor binding)
  snapshots: Vec<Arc<SnapshotState>>     // index == SnapshotId; head is the last
  limits: Limits                         // instance parameters; namespace_cap = 2^32 by default
  nodes: Nodes                           // where committed map nodes are read: nothing (memory) or an ObjectReader
SnapshotState { id, base: Option<SnapshotId>, instance, limits, state: WorkingState, nodes, root: OnceLock<B256> }
WorkingState {
  catalog: MerkleMap<NamespaceId, NamespaceState>,   // the directory
  next_namespace: u64,                   // ordinal allocator; part of rolled-back state
  pricing: Arc<PriceSchedule>,           // schedule committed in this state
  host: MerkleMap<[u8; 32], Blob>,       // host-owned entries, values ≤ 256 bytes
}
NamespaceState {                          // the catalog leaf ("NamespaceLeaf")
  descriptor: NamespaceDescriptor { id, name, owner, revision },
  records: MerkleMap<RawKey, Blob>,                   // raw byte keyspace (Put/Delete/get/scan_prefix)
  typed: TypedState {                                 // sibling field, never entries in `records`
    next_record: u64,                                 // dense record-id allocator; rolled back with the state
    keys:  MerkleMap<RecordKey, RecordId>,            // live key bindings
    rows:  MerkleMap<RecordId, StoredRecord>,         // live records: key + cells sorted by name
    terms: MerkleMap<TermKey, Posting>,               // one posting per value term and per existence term
  },
}
TermKey { name: CellName, tag: TypeTag, value: Option<term bytes> }  // None = existence term, sorts first
Posting(MerkleMap<RecordId, Member>)                                  // ordered set of record ids
```

Every map has bounded, prefix-free, variable-length keys and orders entries
by the bytes of its key encoding (`layout.rs`, state format 2; see
`docs/experimental-prekucki-v1.md`). `bytes⁰` escapes `00 → 01 01` and
`01 → 01 02` and appends a `00` terminator; `text⁺` adds one to every UTF-8
byte and appends a `00` terminator:

| Map | Key encoding | Bytes | Leaf value (canonical codec) |
| --- | --- | --- | --- |
| catalog | `NamespaceId` big-endian | 8 | `descriptor ‖ next_record u64 ‖ records ‖ keys ‖ rows ‖ terms`, each map as `root ‖ len u64` |
| host | raw `[u8; 32]` | 32 | `u32 len ‖ bytes` |
| records (raw) | `bytes⁰(key)` | 1–41 | `u32 len ‖ bytes` |
| keys | `RecordKey` `len ‖ bytes` (point lookups only) | 2–33 | `RecordId` u64 (nonzero) |
| rows | `RecordId` big-endian | 8 | `RecordKey (u8 len ‖ bytes) ‖ Vec<(CellName, Cell)>`, names strictly ascending, attributes indexable and within the indexed ceiling |
| terms | `bytes⁰(name) ‖ tag ‖ 00` (existence) or `bytes⁰(name) ‖ tag ‖ 01 ‖ value`, value `text⁺(str)` or the scalar's fixed-width term bytes | 4–164 (default limits), at most 548 | `posting root ‖ posting len u64` (len ≥ 1) |
| postings | `RecordId` big-endian | 8 | `01` |

The encodings preserve bytewise order, so the raw map iterates in the
bytewise order of the raw keys (the empty key and trailing zeros included)
and the term map in the order of `(name, type, existence before value, value
bytes)`; a prefix scan seeks to
the encoding of the prefix itself. Unit tests in `layout.rs` compare both
orders with Rust's slice and tuple ordering on edge cases and random inputs.
A nested map handle `(EMPTY_ROOT_HASH, 0)` is the empty map; any other
pairing of the empty root or a zero count is corrupt. Decoding rejects
everything a leaf alone can contradict (codec rules, name grammar,
unsorted cells, an attribute of type `Bytes` or over the indexed ceiling (512 bytes), zero ids,
impossible handles, key and row counts that differ, a record allocator not
above the rows); a catalog leaf whose descriptor names another namespace
than its key, and a row read by key that holds another key, are corrupt
where they are read.

Every namespace has its own maps, so identical keys and identical terms
never alias and a scan, lookup or query in one namespace never visits another
namespace's nodes. Descriptors are fields of `NamespaceState`, outside every
keyspace: only `CreateNamespace` and `TransferNamespace` change them, and no
record operation, on the owner path or the host path, can reach them. The
raw byte map and the typed store are sibling fields; neither is reachable
through the other, and a transfer shares both unchanged (only the catalog
leaf is rewritten). A `Branch` owns an `Arc` to its base snapshot plus a
`WorkingState` clone; `execute` clones the working state (O(1)) before
applying an operation and restores that clone on rejection or on a storage
error; `checkpoint` stores another clone together with the receipt count.

The host map is a sibling of the catalog, outside every
namespace: 32-byte keys to values of at most `HOST_VALUE_MAX` = 256 bytes,
for schedules the host keeps deterministically in state (the node's expiry
due counts). Because it is a field of `WorkingState`, checkpoints,
rollbacks, estimates' scratch copies, sealing and publication treat it like
the catalog: a rollback restores it, a sealed candidate carries it and every
snapshot keeps its own version. It is reachable only through the unmetered
host-internal calls `Branch::{try_host_get, try_host_put, try_host_remove,
try_host_range(from, limit)}` and `Snapshot::{try_host_get,
try_host_range}`; `try_host_range` returns at most `limit` entries from
`from` in key order. A value over 256 bytes is refused with
`HostStateError::ValueTooLarge` and changes nothing. Host writes issue no
receipt and spend nothing; a successful put or remove advances the branch's
mutation serial like any applied write, so branch cursors issued earlier
become `CursorMismatch`. No write operation, host path, estimate, metered
read or statistic touches the map (`host_state.rs`). Its root is committed
in the state root like the catalog's.

### State root

Every state has a logical root (`state.rs`):

```text
StateHeader { format u32 = 2, catalog_root, catalog_len u64, next_namespace u64,
              pricing: PriceSchedule, host_root, host_len u64 }
state_root = digest("arkiv/state/v1", encode(StateHeader))
```

`Snapshot::state_header`/`state_root` and `SealedCandidate::state_header`/
`state_root` expose it. The catalog leaves carry each namespace's
descriptor, record allocator and map roots, and term leaves carry posting
roots, so the root commits to every logical field of the state. Tries have a
canonical shape and physical node ids, cache state, snapshot ids and
process tokens never enter the root, so it is a function of the logical
content alone. It is computed lazily: map nodes cache their hash, and
inserting a value holding nested maps does not hash them
(`TrieValue::encodes_empty`), so computing a root hashes only the nodes
built since the last computation and never reads storage. Evidence:
`roots.rs` (every map root and the state root against an independent
reference construction over hand-built leaves, history independence,
retention, sensitivity to each field). Root-sharing statistics
(`records_shared_with`, `typed_shared_with`) compare map roots, so equal
contents count as shared even when the maps were rebuilt.

Every block header commits the state root, and the proofs compose the catalog,
term, row and key point proofs needed for complete Eq pages. Broader
point-read APIs remain open; a root alone is not query evidence.

### Loaders and storage failures

Every map operation takes a loader (`nodes.rs`) that names the map's object
space (`Catalog`, `Host` or the namespace's) and kind. A memory engine's
loader is the trie crate's `MemoryOnly`, which its maps never consult. The
hidden `Engine::from_state(header, limits, reader)` test aid opens a state
from its header over an `arkiv_db_storage::versioned::ObjectReader`,
reading nothing at open and every node lazily on use, and the hidden
`Snapshot::staged_objects` returns the nodes a state holds only in memory
(`SealedCandidate::hashed_nodes` counts them per map before deduplication, for
the rehash measurements); together they let tests commit a state, reopen it
and inject storage failures. `DurableEngine` adds the store context,
bounded decoded-node cache, manifests and durable head through a separate
facade that retains no historical state vector. Stored roots share one
gated loader; recovery invalidates writer generations and cache entries,
while immutable snapshot/cursor identity survives in-place recovery.
Internal operations
stop with `Fault::Reject` (a metered rejection with a receipt) or
`Fault::Storage` (a `StorageError`, never a rejection or receipt); see the
fallible surface below.

### Typed records and their indexes

A typed record is a caller-assigned `RecordKey` (1 to 32 bytes, the width of
the SDK's entity keys; canonical encoding `len ‖ bytes`, also its trie key)
bound to a dense per-namespace
`RecordId` (from 1, never reused: a deleted key recreated gets a new id) and
holding up to `max_cells` named cells. A `CellName` follows a fixed ASCII
grammar (a letter or `$` plus a letter first, then letters, digits, `_`, `-`,
`.`, `:`; at most 32 bytes); `#` and `@` are unreachable, so engine-reserved
names can never collide with user names. A cell is an attribute (indexed) or
a field (stored only) holding one of 25 scalar types (golem-db's full cell
type grid: `Bool`, `Str`, `Bytes`, fixed bytes of 4/8/16/20/32 bytes,
`u32`–`u256`, `i32`–`i256`, `dec32`–`dec256`, `f32`/`f64`, `date32`,
`timestamp64`, plus `Key`; table in `docs/block-format.md`); `Bytes` is
field-only. Kind and type are bound per record, not per
name. The encodings are frozen in `record.rs`: term bytes (`Bool` `00`/`01`,
signed integers, decimals and times big-endian with the sign bit flipped,
unsigned integers big-endian, floats in IEEE total order, `Str` UTF-8, fixed
bytes as-is) preserve the value order, so numeric ranges and string prefixes are
contiguous runs of the term map, whose keys carry the same bytes and are
therefore committed in the state root.

The term map holds one posting per value term `(name, type, value bytes)` and
one existence posting per `(name, type)`, ordered by name, then type, then
value bytes with the existence term first, so every `(name, type)` is one
contiguous run in value order. The invariant is that the term map is exactly
what the attribute cells of the live rows imply: every create, patch and
delete maintains it inside the same operation, an emptied term is dropped, a
rejected operation restores the whole namespace state, and a rollback
restores the term map together with the rows and the id allocator. The
hidden `index_consistent` test aid rebuilds the expected map from the rows
and compares. The row map is also the all-live-records domain that match-all
and negated literals enumerate.

Query evaluation (`typed.rs`) is lazy and positioned after the cursor. A
`Query` is an ordered flat DNF: groups are opened in order, each group's
first literal chooses its candidate source (a posting for `Eq` and `Exists`,
the existence posting as a domain for `NotEq`, a merged run of postings for
`Range` and `StartsWith`, the row map for an empty group or a negated first
literal), and every further literal is a probe evaluated candidate-major
against the loaded row. Each group is pulled to its first match, the
smallest head is emitted, and only the groups whose head was emitted advance.
Every source is a `range_from` iterator starting after the cursor's id, and
every entry is charged after the node step that reveals it and before it is
used. The cost statements are these. Positive `Eq`/`Exists` sources, the
row map (match-all, negation) and the existence posting (`NotEq`) are
positional: a page charges the candidates scanned to fill it plus each
group's first match beyond it. A `Range`/`StartsWith` source additionally
charges `TermSeek` 1 and `TermVisit` 1 for every qualifying term of the run
on every page, because the id-ordered merge consults every qualifying
term's posting per page, so a walk of P matches over T terms with page
limit L charges ceil(P/L) × T `TermVisit` in total; T is bounded by the
namespace's own distinct values. Match-all, negated and `NotEq` first
literals are linear in the namespace's own live rows or existence posting
per page scanned. Nothing depends on other namespaces. `next` is an exact
claim because a full page has positioned every group at its first match
beyond the page; a page carries no total because a total would require
enumerating every match, so `count` is a separate metered read; and nothing
is reordered or planned, so the charged cost and the abort point are a pure
function of the state and the query text. A local read may opt in to planned
evaluation with `Budget::planned` (smallest index source first, same rows and
order; see [metering](metering.md)); block execution never does. Row emission computes the
projected byte count before cloning any cell, and a row already loaded by a
probe or a row-map source is reused for emission rather than loaded again.
`terms` is a positional read of the same term map: one seek, then
`TermVisit` 1 plus the value bytes per returned term, listing the distinct
indexed values of an attribute in value order from a lower bound.

A `Cursor` is a value, the last emitted record id, never an offset, so the
next page resumes exactly after that record whatever later happened to it.
It binds the engine instance and the snapshot id when issued by
`Snapshot::query`, or the branch token and the branch's mutation serial when
issued by `Branch::query` (the serial advances on every applied write, every
rollback and every pricing change; a rejected write does not advance it),
plus the namespace, the query and the projection. Continuing with any of
them changed is `CursorMismatch` at admission. The page limit is not bound. A
cursor never re-resolves head: the caller keeps the `Snapshot` handle the
cursor names, so a walk is identical whether or not new snapshots are
published between its pages. There are no caches.

`Limits` are instance parameters (`Engine::with_limits`): the namespace cap,
`max_cells` (also the bound on a named projection), the cell limits
(`max_cell_name_len`, `max_str_len`, `max_bytes_len`, `max_indexed_len`,
golem-db's genesis `cell_limits`), `max_page`, `max_groups` and
`max_literals`. They are fixed for the engine's lifetime and
checked at admission before any charge, so validation is
O(`max_groups` × `max_literals` + `max_cells`) and every per-entry step of a
write or query is bounded by them (a term comparison is at most
`min(max_indexed_len, max_str_len)` bytes, 128 by default; a patch's cell merge is one linear pass, O(cells +
set + remove), over at most 2 × `max_cells` entries).

Indexed strings are limited to 128 bytes by default
(`Limits::DEFAULT_INDEXED_LEN`) with a hard ceiling of 512
(`Value::INDEXED_LEN_CEILING`); cell names have a hard 32-byte cap
(`CellName::MAX_LEN`). The configured limits may only be smaller than the
hard caps, and query strings and prefixes obey the indexed-string limit. Namespace names have a
20-character ASCII cap.
A namespace stores its immutable name in its descriptor; addresses still
use the stable ID and name-registration policy stays in the host.

### Host path and expiry

The chain-level rules (capacity per height, the spill to a later height,
rent) are in [entity expiration](entity-expiration.md).

`Branch::execute_host` and `estimate_host` run `CreateRecord`, `PatchRecord`
and `DeleteRecord` with the host's own authority: the namespace owner check
is skipped and everything else (validation, admission, charges, rollback,
receipts) is identical to `execute`. Any other operation is refused at
admission with `InvalidArgument(NotHostOperation)`, so descriptors and the
raw byte maps are never host-writable. This is the trusted path for
chain-driven transitions; a host must never expose it to user requests.

The engine has no clock. Expiry is `host::expire_records`, run in the block
where records expire and sweeping height by height in ascending order:
`terms` reads the smallest live height of the expiry attribute (one seek
and one term); while that height is at most the block number, the rows
holding it are read as `Eq` pages (`KEY_ONLY`, limit `max_page`, no cursor)
and deleted through the host path until the height has no rows, then the
next smallest height is read. Read receipts are discarded. Deletes are
reported in (height, id) order, and the work is linear in the distinct
expired heights plus the deleted rows. So snapshot B lacks the records
while snapshot B−1 keeps them. A rejection halts the run with the deletes
applied so far, which stay applied in the branch; the remaining rows stay
live until a later block, where the sweep again starts at the smallest live
height. The host must reserve block budget for expiry (run it first, or
size the block budget for it). The expiry height is an ordinary `U64`
attribute under a name the host chooses; the engine assigns `$`-names no
meaning, so the host must itself reject user writes of the `$`-cells it
relies on.

### Fallible surface

Every `Branch` and `Snapshot` method that walks state has a `try_` form
returning `Result<T, StorageError>`, where `T` is the
infallible method's return type (`WriteResult`, `ReadResult<_>` or a
statistic) and `StorageError` is `arkiv_db_storage::versioned::StorageError`,
re-exported from `arkiv_db`: `try_execute`, `try_execute_host`,
`try_estimate`, `try_estimate_host`, `try_get`, `try_scan_prefix`,
`try_descriptor`, `try_get_record`, `try_query`, `try_count`, `try_terms`,
the statistics (`try_namespace_count`, `try_record_count`,
`try_typed_record_count`, `try_term_count`, and on snapshots
`try_records_shared_with` and `try_typed_shared_with`), the hidden
`try_index_consistent`, and `host::try_expire_records`, whose result is
`Result<Result<Vec<(RecordId, Receipt)>, ExpiryHalt>, StorageError>`. A
`StorageError` is an availability failure: never a rejection, a receipt, a
missing record or an empty result. They are truly
fallible: a map node that cannot be read, is absent (`Missing`) or does
not verify or decode (`Corrupt`) fails the call, including in the middle of
an iteration. On a memory engine no map holds a stored node, so the `try_`
forms always return `Ok` and the infallible methods delegate to them;
the infallible methods panic on a storage error, so a
host of an engine that reads storage calls only the `try_` forms.

On a snapshot a storage error fails that call only; the snapshot answers
again once storage does. On a branch it **poisons** the branch: the failed
operation's changes are discarded (the working state is restored to its
value before the operation), no receipt is recorded and nothing is added to
the block's spend; every later `try_` call on the branch returns the
poisoning error, and `try_checkpoint`, `rollback_to`, `set_pricing`,
`try_state_root` and `try_seal` return `EngineError::Poisoned`, so a poisoned candidate can never
be sealed or published. A call made of several operations
(`try_expire_records`) keeps the receipts of the operations that completed
before the failure, in a branch that can no longer be sealed. The host
discards a poisoned branch. `checkpoint` and `seal` stay infallible for
memory engines and panic on a poisoned branch. Evidence:
`storage_faults.rs` (every read and every branch operation failed at every
node load) and the `engine` unit test (the working state is unchanged by a
failed operation). `Engine::snapshot`, `head_snapshot` and `begin` walk no
state: pinning a retained snapshot is a pointer copy, and the durable
`DurableEngine::snapshot` is a separate fallible call that reads a
manifest and its index associations without walking trie state.

### Canonical codec

`arkiv_db::codec` is the binary encoding of every engine
type that will be hashed or stored: ids, names, values, cells, record keys,
`WriteOp`, `Effect`, `Rejection` and its faults, `Outcome`, `Receipt`
(without its debug ledger), `PriceSchedule`, `Limits` and
`NamespaceDescriptor`. Big-endian, length-prefixed, `u8` enum tags from
explicit tables, exact consumption and strict decoders that enforce every
type invariant; `digest(domain, bytes) = keccak256(u8 len(domain) ‖ domain
‖ bytes)`. The specification, tag tables and compatibility rules are in the
[block format](block-format.md), whose part 2 builds the block formats of
`arkiv-node` on it. `WriteOp::validate` (the engine's admission argument
checks) is public so that the host's static validation is exactly the
engine's. The trie leaf layouts and the `StateHeader` of state format 2
(above) use the same primitives.

### Publication, retention, durability

The ordinary memory `Engine` follows the lifecycle below;
[`DurableEngine`](engine-storage-run.md) adds explicit preparation and
three-outcome durable commit.

`Branch::seal` freezes the working state, receipts and spent total into a
`SealedCandidate`. `Engine::publish` checks that the candidate was sealed by
this engine instance (`ForeignCandidate` otherwise) and that its base is still
the head (`StaleHead` otherwise), then appends one `Arc<SnapshotState>`
holding an O(1) clone of the candidate's state. Readers pin a snapshot through
that `Arc`; the head is simply the last entry, so advancing it is one value
swap of a complete state. A reader can never observe a partially applied
block or a mix of old and new structures, and a namespace's rows and index
are always published together. A rejected candidate changes nothing and
keeps its receipts. Several branches may exist on the same base; at most one
publishes.

Every published snapshot is retained for the process lifetime and
`Engine::snapshot(id)` resolves any published ID; snapshot IDs are sequential
from `GENESIS` (0) and never reused within an instance. There is no pruning,
no snapshot capacity limit and no durability: nothing is written to disk, and
dropping the engine or ending the process loses every snapshot. Publication
is in-process visibility, not a durable commit.

### Trusted-caller boundary

The host validates signatures and admission policy and passes the
authenticated `actor: OwnerId` (20 opaque bytes) with each write. The engine
never interprets those bytes. It compares the actor with the descriptor's
owner at the operation's position in the branch, so a transfer earlier in the
same block decides who may write next, and a rollback restores the previous
owner. Creation is permissionless: the actor becomes the owner. Pricing
changes (`Branch::set_pricing`) and the host path (`execute_host`) are
host transitions that namespace ownership does not grant. The only metered
data-plane entry points are `execute`, `estimate` (and their `_host`
variants on `Branch`), `get`, `scan_prefix`, `descriptor`, `get_record`,
`query`, `count` and `terms` on `Branch` and `Snapshot` (and their `try_`
forms); no method exposes a catalog, record map, row map or index. The
host-state calls (`try_host_*`) are host-internal and unmetered; a host must
never serve them to users. The statistics `namespace_count`,
`record_count`, `typed_record_count`, `term_count`, `records_shared_with` and
`typed_shared_with` are unmetered observability aids for hosts and tests
(each resolves the namespace through at most one directory path per
snapshot): they reveal counts and map-root identity only, never keys,
values or terms, and a host must not serve them as RPC reads. The hidden
`index_consistent` aid is O(rows × cells) and exists for tests only.

### Directory bound

Every namespace-scoped operation resolves its namespace through the catalog
exactly once, charged as one `DirectoryLookup` unit. The catalog is a Merkle
Patricia trie keyed by 8-byte big-endian `NamespaceId`s (16 nibbles), so
whatever the number of namespaces a lookup visits at most `2·8 + 1` = 17
nodes (every branch consumes a nibble, every extension at least one and is
followed by a branch, and the path ends at one leaf) and a write at most 34
(the lookup path, then the insert path of the updated catalog leaf). The
bound does not grow with the namespace count; for dense ids it is much
lower: with ids 1 to 3 a lookup visits 3 nodes, with 1 to 503 it visits 5
(`isolation.rs`, `query_isolation.rs`). Creation at the cap is rejected with
`DirectoryFull` after the lookup charge. The constant `DirectoryLookup`
price reserves that bounded worst case; the actual visits are reported in
`PhysicalTrace::directory_nodes` for measurement only. All other work of an
operation is over the selected namespace's own maps and is reported as
`record_nodes` (byte map, key map, row map) and `index_nodes` (term map and
postings), so the bound
`query_work ≤ local_work(query, namespace_state) + directory_bound` holds
for every operator, including negation and match-all, whose domain is the
namespace's own row map.

### Physical traces

`PhysicalTrace` counts trie node visits: every node an operation examines
counts once per step that examines it, inline children included (they are
decoded nodes like any other), whether it was already in memory, served by
a cache or loaded. A lookup or an update counts the key's path, a removal
also the sibling it loads when a branch collapses, and an iterator its seek
path plus every node it steps into; nodes an update creates are not
visits. Iterators are lazy: a posting or the row map is first stepped when
its group pulls its first head, while a term run steps each qualifying
posting once as it opens (its seek and first entry), and every entry is
charged after the step that reveals it. The counts therefore depend on the
contents and the operation only, never on cache warmth or on which nodes a
handle holds in memory (`storage_faults.rs` shows a state reopened from its
objects reporting the same visits as the memory state). The exact traces
in `trace_regression.rs` and the absolute counts in `isolation.rs` are
derived by hand from the trie shapes of their fixtures.

### Remaining physical-layout limitations

- The bound is on logical node visits. All namespaces share one heap and the
  CPU caches, so wall-clock latency is not isolated; that needs resource
  scheduling and capacity controls outside this engine.
- The Eq-page proof verifies the directory path and namespace binding;
  broader operator proofs and physical performance guarantees remain open.
- Beyond the namespace cap there is no capacity or pruning policy: memory
  grows with every retained snapshot's unshared nodes and there is no limit
  on records, terms or snapshots.
- `PhysicalTrace` counts are non-canonical and change with the data
  structure; they are kept out of receipts.
- The fixture weights are uncalibrated. Two schedule invariants must survive
  calibration: `IndexTermCreate > 0` (a distinct term costs more than a
  membership) and the symmetric structural delete weights (`RecordDelete =
  RecordCreate`, `CellRemove = CellWrite`, `IndexRemove = IndexInsert`).

## Block-oriented test workload

The engine tests use real blocks: a `Branch` per block, per-operation budgets,
checkpoints, seal and publication; the typed-record tests add record writes,
host expiry and queries compared with a full-scan reference at every
snapshot. See the [testing notes](testing.md).

## Project namespaces

The [namespace contract](project-namespaces.md) is implemented for records,
typed records and indexes: immutable ordinal IDs, protected owner/revision
descriptors, per-namespace byte maps, key bindings, rows, id allocators and
term maps, and a bounded directory. Every engine operation selects one
namespace, and every read selects one snapshot. Ownership transfer changes
metadata without moving, copying or reindexing data; historical snapshots keep
the previous owner. Project-name registration and anti-squatting rules belong
to Arkiv's naming layer.

The directory bound above is measured by the isolation tests for every
implemented operation, including every query operator. Namespace-local
roots are composed through the catalog into the state root;
every block header commits that root, and light nodes verify bounded
catalog paths for Eq pages. Its [proof overhead](eq-proof-charges.md) is
explicit; broader proof profiles retain their own acceptance gates.

## Metering and gas

Metering applies to every data-plane operation. Only write receipts are
converted to gas by Arkiv; read/query receipts support local RPC budgets or
API billing and never enter consensus. Counting is independent of whether
storage is in RAM or on disk, and root maintenance is charged per logical
entry written, not per trie node, so receipts do not depend on the shape of
a structure. See [metering](metering.md) for the charge table.

## Known limits

- One proposer, no consensus, finality or fork choice. A proposer signature
  authenticates the producer, not the execution; full followers re-execute.
- Proofs exist for complete equality pages and bounded single-attribute
  ranges. Other query shapes are answered by a full node without a proof,
  and completeness is always relative to the authenticated index.
- Pruning deletes trie objects only. With `--state-retention N`,
  `arkiv-proposer` turns on garbage collection in a `node.redb` it creates or
  adopts, and objects no snapshot from the floor up reaches are deleted inside
  block commits. Blocks, bodies, manifests and `history.redb` are kept; an
  older state file keeps everything until `state gc-enable` (see the
  [pruning design](pruning-design.md)). The default is archive.
- Corruption of a stored object is found when the object is read, not at
  open.
- Cache and preparation limits bound retained objects and bytes, not the
  process's resident memory.
- Within the query subset there is no attribute sorting, descending order
  or offset paging in the engine: results are in record-id order with keyset
  cursors. Keys are caller-assigned; the Arkiv host derives entity keys from
  the creator and creation nonce.
