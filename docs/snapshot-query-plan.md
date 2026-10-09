# Implementation plan: queries and proofs at one snapshot

## Contract and current status

Follow this repository's
[single-block contract](single-block-query-proofs.md)
with the upstream [Golem-DB requirements](https://github.com/Arkiv-Network/arkiv-source-of-truth/blob/main/golem-db.md)
as a reference. The local contract records the 2026-09-23 clarification:
one query, one finalized block, one state
root, with a complete proof of its answer. Native valid-time/bitemporal
intervals are not part of this work.

The implementation targets Arkiv's blockchain workload: ordered block packets
with many operations and one externally visible completed state per block.
Block-facing APIs and optimizations are allowed; general-purpose database
compatibility is not the primary goal. Keep consensus and authorization in
the host and byte-storage mechanics in the KV layer.

The [project namespace contract](project-namespaces.md)
is a required foundation: stable namespace IDs, transferable ownership and
namespace-local data/indexes. Names are validated descriptor metadata; human-readable registration,
uniqueness, resolution and squatting prevention remain host policy.

Current implementation (milestones 0, 1 and 2, in memory):

- The raw byte KV facade (`Database` over `KvStore`) and its functional tests
  are unchanged. It has no namespaces, snapshots or metering and shares no
  state with the engine.
- `Engine` keeps every published state as an immutable snapshot built from
  structurally shared Merkle Patricia maps (node step N3b; AVL maps
  before): a namespace catalog keyed by ordinal
  `NamespaceId`; per namespace a descriptor, a byte-key record map and, as
  sibling fields, a typed store (key bindings, rows and a term index); the
  ordinal allocator and the committed pricing schedule.
- A `Branch` executes namespace-scoped writes in order on top of the head
  with per-operation and per-block budgets, supports checkpoints and
  rollback, estimation on scratch state, metered reads and queries of its
  working state, a host path for chain-driven record transitions and a
  pricing admin transition, then seals; `publish` appends the candidate
  atomically if its base is still head.
- Typed records: caller-assigned keys, dense per-namespace record ids,
  validated cell names, typed values with frozen order-preserving encodings,
  attribute cells indexed in the same operation that writes them, field
  cells stored only.
- Queries: an ordered flat DNF over typed equality, inequality, integer
  ranges, string prefixes, existence and negation, evaluated lazily in
  record-id order with projection, keyset pagination bound to one snapshot
  or one branch state, a separate `count`, and a positional `terms` read
  listing the distinct indexed values of an attribute in value order.
- Every data-plane operation is metered with checked integer arithmetic under
  model version 3 and issues a canonical receipt; ledgers are optional.
  Version 3 (node step N0b-1) prepays root maintenance on every write; the
  engine computes those roots since node step N3b.
- `host::BlockMap` maps consecutive block numbers to strictly increasing
  terminal snapshots; `host::expire_records` runs expiry as host-authorized
  deletion inside a block's branch, sweeping expired heights in ascending
  order.
- Node step N0b-2: the canonical binary codec `arkiv_db::codec` (codec
  version 1) with golden vectors and the domain-separated `digest`, specified
  in the [block format](block-format.md); a host-owned state map (32-byte
  keys, values of at most 256 bytes) inside every state, rolled back,
  published and retained with it and unreachable from the data plane; and a
  `try_` form of every state-walking `Branch`/`Snapshot` method returning
  `Result<_, StorageError>`.
- Node step N3b: every engine map is an `arkiv_db_trie::MerkleMap` with the
  frozen state-format-1 encodings (see `docs/architecture.md`), every state
  has a state root (`digest("arkiv/state/v1", encode(StateHeader))`) that
  commits the catalog (descriptors, allocators, every namespace's map
  roots) and the host map, and the `try_` forms are truly fallible: over
  stored nodes (reached in N3b through a test object reader) a storage
  error fails the call and poisons a branch. Receipts are unchanged.

Not implemented: point and query proofs, the root in block headers,
durable commits, production pricing, and the SDK proof API. Within the query subset:
attribute sorting, descending order and offset paging (record-id order and
keyset cursors are the only order and continuation), and an engine-assigned
key mode. Record keys accept the SDK's 32-byte width (see the record-key
encoding below), but no SDK adapter exists, so wire compatibility is not
claimed.
Two departures from the upstream reference are deliberate: a page carries
no free total, because a total would need the full walk the lazy rules
avoid, and `count` is the metered replacement; and negation evaluates as
the complement over the namespace's live rows, this repository's resolution
of upstream open question 6. The vendored Alloy trie is used through
`arkiv-db-trie` (its node references and empty root; its `HashBuilder` as a
test oracle); the vendored Reth components are not used.

**Updated sequence (2026-09-24): bounded full/light-node demo implemented.**
The [demo specification](full-light-demo.md) brings a bounded complete
equality-query proof profile and the point-proof composition it needs ahead
of durability, alongside the running producer and independent followers.
All history remains retained; no pruning is introduced. N3c
[engine storage v1](engine-storage-v1.md) now persists engine state and
historical proof data. Next in the [node and storage plan](node-storage-plan.md):
N4 durable node integration before the remaining milestone-4 proofs and SDK
work. Milestones 0–2 remain the memory reference. The durable node must retain
query state, indexes and proof data on disk and reopen without full-chain
replay.

## Namespace boundary and index partitioning

Implemented in milestones 0 to 2 (details in the namespace contract):

- A namespace ID is an immutable ordinal, independent of project name and
  owner, allocated from committed state and encoded as 8 big-endian bytes.
- Names are immutable `NamespaceName` values, 1–20 ASCII characters with
  the cell-name grammar, supplied by `CreateNamespace { name }`. They are
  snapshot metadata and remain unchanged across transfers. They do not
  replace the stable ID; duplicate labels are permitted at this layer.
- Namespace descriptors `{id, name, owner, revision}` live outside the record
  keyspace. Only `CreateNamespace` and `TransferNamespace` modify them;
  transfer checks existence, ownership and the expected revision in that
  order, changes metadata atomically and shares the record map and the typed
  store unchanged.
- Every record address is `(namespace_id, key)`; each namespace has its own
  byte map, key bindings, rows, record-id allocator and term index. Every
  read and write takes one namespace, and every read takes one snapshot.
- The engine checks authority against the current branch state, including
  earlier transfers in the same block. Namespace ownership does not grant the
  pricing admin transition or the host path.
- The only metered data-plane entry points are the `Branch` and `Snapshot`
  methods `execute`, `estimate` (with their `_host` variants on `Branch`),
  `get`, `scan_prefix`, `descriptor`, `get_record`, `query`, `count` and
  `terms`. The count and map-identity statistics (`namespace_count`,
  `record_count`, `typed_record_count`, `term_count`, `records_shared_with`,
  `typed_shared_with`) and the hidden `index_consistent` test aid are
  unmetered observability aids that return no keys or values; the raw byte
  KV facade is separate and reaches no engine state.
- The catalog is a Merkle Patricia map keyed by 8-byte ids with one lookup
  per operation: at most 17 nodes per lookup and 34 per write whatever the
  namespace count (see the namespace contract), with the namespace count
  capped by `DEFAULT_NAMESPACE_CAP`.
- Indexes and existence domains are partitioned by namespace: the term map,
  its postings and the all-live-records domain (the row map) belong to one
  namespace, so identical terms in two namespaces never share a posting and
  a negated literal enumerates one namespace's rows only. The interference
  test is extended to every operator (`query_isolation.rs`).

The descriptor is committed through the catalog into the state root (node
step N3b). Still to do: carry that root in the block header (header v2) and
prove descriptors to a verifier (point proofs, N3d). An owner filter or a
prefix applied after global enumeration
does not provide isolation, and the shared-directory allowance stays explicit
and bounded. Wall-clock latency remains subject to shared hardware
contention. Do not mark isolation complete because results alone stay
correct.

## Record model and query subset

Frozen for this repository (`record.rs`, `query.rs`; every type is
re-exported from `arkiv_db`). Term bytes decide index order, and the term
map's keys carry the same bytes, so the state root commits to them (node
step N3b).

- `RecordKey`: 1 to 32 caller-assigned bytes (`RecordKey::MAX_LEN = 32`),
  unique among the live records of a namespace. The canonical encoding is
  33 bytes (`RecordKey::ENCODED_LEN`): `len ‖ bytes ‖ zero padding`, a bijection:
  `decode` rejects a length byte of 0 or above 32 and any non-zero padding.
  Vectors: `b"a"` → `01 61 00…00` (33 bytes); 31 zero bytes → `1f 00…00`;
  a 32-byte key `k` → `20 ‖ k` with no padding; an empty key and a 33-byte
  key are not keys. Charges count the raw key length, never the encoding.
- `RecordId`: the dense per-namespace creation ordinal from 1, never reused.
  A deleted key recreated gets a new id, so a reused key never joins two
  histories. Ids are provisional until publication: rollback restores the
  allocator like the namespace ordinal. Display `rec:{n}`. The id is the
  only result order of queries.
- `CellName`: ASCII, 1 to 32 bytes (writes: `Limits::max_cell_name_len`); the first character is a letter or `$`
  followed by a letter; every later character is a letter, digit, `_`, `-`,
  `.` or `:`; case-sensitive, ordered bytewise. `#`, `@`, spaces and
  non-ASCII are invalid, so engine-reserved names are unreachable from the
  data plane. A `$`-name is an ordinary name to the engine.
- `TypeTag`: golem-db's cell type ids (`Bool = 1`, `Str = 2`, `Bytes = 3`,
  `U64 = 13`, `I64 = 17`, …; see `docs/block-format.md`), not the SDK's.
  `Bytes` is field-only; every other type is ordered by its term bytes;
  `Str` also supports prefixes.
- `Value`: encoded length 1 (`Bool`), 8 (`I64`, `U64`) or the byte length
  (`Str`, `Bytes`). Term bytes: `Bool` `00`/`01`; `I64` big-endian with the
  sign bit flipped; `U64` big-endian; `Str` its UTF-8 bytes; none for
  `Bytes`. Vectors: `I64(-1)` → `7f ff ff ff ff ff ff ff`, `I64(0)` →
  `80 00 00 00 00 00 00 00`, `I64(i64::MIN)` → `00…00`, `I64(i64::MAX)` →
  `ff…ff`, `U64(258)` → `00 00 00 00 00 00 01 02`, `Str("ab")` → `61 62`.
  Values of different types never compare equal or order against each other.
  Indexed strings obey `Limits::indexed_value_len()` =
  `min(max_indexed_len, max_str_len)` (at most the hard 128-byte
  `Value::MAX_INDEXED_LEN`), measured in UTF-8 bytes, on writes,
  predicates, range bounds, prefixes and term bounds. Field values obey
  `max_str_len` and `max_bytes_len` (4096 by default).
- `Cell { kind: Attribute | Field, value }`. Attributes are indexed and
  filterable; fields are stored and returned only. Kind and type are bound
  per record, not per name. An attribute holding `Bytes` is invalid.
- `Row { id, key: Option<RecordKey>, cells }`: the id is always returned; the
  key and the cells are projected; cells are sorted by name.
- `Limits { namespace_cap: 2^32, max_cells: 64, max_cell_name_len: 32,
  max_str_len: 4096, max_bytes_len: 4096, max_indexed_len: 128,
  max_page: 200, max_groups: 8, max_literals: 8 }` (`Limits::DEFAULT`) are
  instance parameters, fixed for an engine's lifetime
  (`Engine::with_limits`, `Engine::limits`, `Branch::limits`;
  `Engine::with_namespace_cap` changes only the cap) and checked at admission
  before any charge. `max_cells` also bounds the names of a `Named`
  projection. `Limits::MAX_RAW_KEY_LEN = 20` is a hard bound on the local
  raw byte key and scan prefix: the namespace ID is passed separately. Raw
  `Put`, `Delete`, `get` and `scan_prefix` reject longer inputs with
  `InvalidArgument(KeyTooLong)` before lookup, charging or recording a write.
  Raw values keep their byte charges; the separate low-level `Database`
  facade keeps its arbitrary-byte storage contract.
- `Term::{Eq { name, value }, NotEq { name, value }, Range { name, lower,
  upper }, StartsWith { name, prefix }, Exists { name, tag }}`; `Literal {
  term, negated }`; `Query { groups: Vec<Vec<Literal>> }`, an ordered flat DNF
  (`Query::match_all()` is one empty group); `Projection { key, cells:
  CellSelect::{None, All, Named} }` with `ALL`, `ID_ONLY` and `KEY_ONLY`;
  `Cursor` (opaque; exposes `after`, `snapshot`, `namespace`, `query`,
  `projection`); `QueryPage { rows, next: Option<Cursor> }`.

Semantics, for one live record with cell `c` under the term's name:

- `Eq`: `c` is an attribute of the value's type holding that value. `NotEq`:
  an attribute of that type holding another value; a record lacking the
  attribute, or holding another type, does not match. `Range`: an attribute
  of the bounds' type inside the bounds, honouring `Included`, `Excluded` and
  `Unbounded`; a lower bound above the upper bound is simply empty.
  `StartsWith`: a `Str` attribute whose UTF-8 bytes start with the prefix
  bytes; an empty prefix matches every `Str` attribute of that name.
  `Exists`: an attribute of that type.
- Field cells never satisfy a term. A negated literal is the complement over
  all live records of the namespace. A group is the AND of its literals, the
  query the OR of its groups, an empty group matches every live record. The
  result is the matching live records in ascending id, the only order.
- A page holds the first `limit` matches with an id greater than the
  cursor's `after` (all matches from the start without a cursor). `next` is
  `Some`, with `after` set to the last row's id, if and only if at least one
  further match exists: an exact claim. A page carries no total;
  `count(namespace, query, budget)` is a separate metered read returning the
  number of distinct matches. The page limit is not bound by the cursor, so
  the pages of one walk may use different limits.
- `get_record(namespace, key, projection, budget)` returns `None` for an
  unbound key; `Named` projects only the names the record has.
- `terms(namespace, name, tag, lower, limit, budget)` returns the distinct
  indexed values of attribute `name` with type `tag` held by live records,
  in value order starting at `lower` (`Included`, `Excluded` or
  `Unbounded`), at most `limit` of them: a positional read of the term map
  that charges the seek and the returned terms only (see the charge table;
  `Value::from_term_bytes` is the inverse of `term_bytes`).
- Create: `AlreadyExists` if the key is bound; otherwise the next id is
  minted, the cells stored sorted by name and the attribute cells indexed.
  Patch: `NotFound` if unbound; `set` replaces or adds cells (kind and type
  may change), `remove` drops names (an absent name is a charged no-op); the
  result may hold at most `max_cells` cells (`RecordFull`, decided after the
  key lookup and therefore recorded); id and key never change. Delete:
  `NotFound` if unbound; unbinds the key and removes the row and every index
  membership. An emptied term is dropped from the term map, so deletes,
  patches and rollback make terms new again.
- Cursor binding: a cursor from `Snapshot::query` binds the engine instance
  and the snapshot id; one from `Branch::query` binds the branch and its
  mutation serial, which advances on every applied `execute`/`execute_host`
  (rejected operations do not advance it), every `rollback_to` and every
  `set_pricing`. Both also bind the namespace, query and projection. Any
  mismatch is `Rejection::CursorMismatch` at admission: spent 0, receipt
  issued, nothing recorded. A cursor never re-resolves head: the caller pins
  the snapshot the cursor names, so a walk is identical whether or not new
  snapshots are published between its pages.
- Host path: `Branch::execute_host` and `estimate_host` accept
  `CreateRecord`, `PatchRecord` and `DeleteRecord` only; any other operation
  is `InvalidArgument(NotHostOperation)` at admission (spent 0, not
  recorded). The owner check is skipped; every charge, admission check and
  receipt is identical to `execute`. Descriptors are never host-writable.
- Expiry: the engine has no clock. `host::expire_records(branch, namespace,
  name, BlockNumber(b), query_budget, delete_budget)` sweeps height by
  height: `terms(namespace, name, U64, Unbounded, 1, query_budget)` reads
  the smallest live height; the run stops when there is none or it exceeds
  `b`; otherwise the rows of that height are read as `Eq(name, U64(h))`
  pages (`Projection::KEY_ONLY`, limit `max_page`, no cursor,
  `query_budget`) and every row of each page is deleted through
  `execute_host(DeleteRecord)` with `delete_budget`, until a page is empty,
  and the next smallest height is read. Read receipts are discarded.
  Deletes are reported in (height, id) order, and the work is linear in the
  distinct expired heights plus the deleted rows. Any rejection (read or
  delete) returns `ExpiryHalt { applied, rejected }` with the deletes
  applied so far, which stay applied in the branch and keep their receipts
  there; a halted run resumes at the smallest live height in a later block,
  so the host reserves block budget for expiry (runs it first, or sizes the
  block budget for it). Run in the block where records expire, snapshot B
  lacks them while snapshot B−1 keeps them, and no read consults the head.
  The host stores the expiry height as an ordinary `U64` attribute and must
  itself reject user writes of the `$`-cells it relies on; the engine
  assigns `$`-names no meaning.

Admission validation, before any charge (spent 0, receipt issued, not
recorded):

- Writes, in order: the host-path restriction, then the arguments.
  `CreateRecord`: more than `max_cells` cells → `TooManyCells`; then per cell
  in submitted order `DuplicateCell`, a name longer than
  `max_cell_name_len` → `NameTooLong`, a `Str`/`Bytes` value longer than
  `max_str_len`/`max_bytes_len` or a `Str` attribute longer than the
  indexed-string bound → `ValueTooLarge`,
  an attribute holding `Bytes` →
  `BytesNotIndexable`. `PatchRecord`: both lists empty is valid (golem-db's
  no-op: `NotFound` for a missing key, otherwise applied with DirectoryLookup,
  PointRead and RecordPatch 1 charged and nothing written); `set.len() + remove.len() > max_cells` → `TooManyCells`; `set` validated
  like create cells; a duplicate in `remove` → `DuplicateCell`, a name in
  `remove` longer than `max_cell_name_len` → `NameTooLong`; a name in
  both lists → `SetAndRemoveOverlap`. Then the milestone-1 block admission
  (`ExceedsBlockBudget`).
- Reads: `get_record` checks a `Named` projection's size
  (`ProjectionTooLarge`). `query` checks, in order: `NoGroups`,
  `TooManyGroups`, `TooManyLiterals`, `ZeroLimit`, `LimitTooLarge`; per
  literal in order: `Eq`/`NotEq` with a `Bytes` value → `NotIndexable`, with
  a `Str` value longer than the indexed-string bound → `ValueTooLarge`; `Range` with
  both bounds `Unbounded` → `UnboundedRange`, bounds of different types →
  `MixedBounds`, a `Bytes` bound → `NotOrdered`, a `Str` bound longer than the
  indexed-string bound → `ValueTooLarge`; `StartsWith` with a prefix longer than the indexed-string bound →
  `ValueTooLarge`; `Exists` with `Bytes` → `NotIndexable`; then
  `ProjectionTooLarge`; then the cursor check. `count` validates the query
  the same way with an implicit limit of 1. `terms` checks, in order: a
  type that is not indexable → `NotIndexable`; a bound value of another
  type → `MixedBounds`; a `Str` bound longer than the indexed-string bound →
  `ValueTooLarge`; `ZeroLimit`; `LimitTooLarge`.

Record-key encoding (settled in node step N0b-1). The SDK addresses entities
by 32-byte hashed keys, so `RecordKey::MAX_LEN` is 32 and the canonical
encoding is 33 bytes wide, as listed above. The encoding does not enter any
charge: `RowByte`, `ReadByte` and projected bytes count the raw key length.
The fixed-width form is the key of the key-binding map since node step
N3b.

There is no engine-assigned key mode: every key is caller-assigned.

## Operation cost is part of the first milestone

Golem-DB distinguishes **metering** from **gas pricing**. Every data-plane
operation counts deterministic cost units. Arkiv converts write receipts to
gas and requires a budget for consensus-path writes. Read/query receipts are
for local RPC limits or optional API billing; they never enter consensus and
do not imply a gas charge for reading. Proof generation also needs bounded
work as part of the read service; its counting rules must be specified before
that API is exposed.

The implemented meter (counting-rule `MODEL_VERSION = 3`) sits at the logical
operation boundary above the maps, not at elapsed time, allocator calls or
node comparisons. Version 2 added the typed record, index and query classes;
every version-1 charge point is unchanged. Version 3 (node step N0b-1) adds
the root-maintenance and proof classes below: the new write charges join the
atomic groups each write already charged, and every other version-2 charge
point, check order and weight is unchanged.

Classes (`OperationClass`, 31 entries in ledger order): the version-1 classes
`DirectoryLookup`, `NamespaceCreate`, `NamespaceTransfer`, `PutRecord`,
`DeleteRecord`, `WriteKeyByte`, `WriteValueByte`, `PointRead`, `ScanSeek`,
`ScanEntry`, `ReadByte`, then the version-2 classes `RecordCreate`,
`RecordPatch`, `RecordDelete`, `CellWrite`, `CellByte`, `CellRemove`,
`IndexInsert`, `IndexTermCreate`, `IndexRemove`, `QueryGroup`, `TermSeek`,
`TermVisit`, `PostingEntry`, `RowVisit`, `RecordProbe`, `ResultRow`, then
the version-3 classes `RootUpdate`, `RowByte`, `ProofNode`, `ProofByte`.

Fixture weights (`PriceSchedule::FIXTURE`, schedule id 1). **Uncalibrated**:
they exist so receipts can be checked by hand and must not be read as
production prices.

| Class | Weight | Class | Weight | Class | Weight |
| --- | --- | --- | --- | --- | --- |
| `DirectoryLookup` | 1 | `RecordCreate` | 20 | `QueryGroup` | 5 |
| `NamespaceCreate` | 50 | `RecordPatch` | 10 | `TermSeek` | 4 |
| `NamespaceTransfer` | 20 | `RecordDelete` | 20 | `TermVisit` | 3 |
| `PutRecord` | 10 | `CellWrite` | 4 | `PostingEntry` | 1 |
| `DeleteRecord` | 5 | `CellByte` | 1 | `RowVisit` | 1 |
| `WriteKeyByte` | 2 | `CellRemove` | 4 | `RecordProbe` | 2 |
| `WriteValueByte` | 1 | `IndexInsert` | 6 | `ResultRow` | 3 |
| `PointRead` | 3 | `IndexTermCreate` | 30 | `RootUpdate` | 2 |
| `ScanSeek` | 4 | `IndexRemove` | 6 | `RowByte` | 1 |
| `ScanEntry` | 2 | | | `ProofNode` | 2 |
| `ReadByte` | 1 | | | `ProofByte` | 1 |

The version-3 classes:

| Class | Counts | Charged by |
| --- | --- | --- |
| `RootUpdate` | Each logical map-entry write that authenticated state must rehash: a raw entry, a key binding, a row, a posting entry, a term leaf or the namespace's catalog leaf | Every write, inside the group it already charged (table below) |
| `RowByte` | Each byte of the stored row a typed record write leaves behind: key bytes + Σ over the row's cells after the write of (name bytes + encoded value bytes), the same measure as a full-projection `p` | `CreateRecord` and `PatchRecord` record groups |
| `ProofNode` | Reserved in model 3; model-4 Eq proofs count visited witness nodes including inline nodes | [Eq proof reads](eq-proof-charges.md) |
| `ProofByte` | Reserved in model 3; model-4 Eq proofs count all emitted binary evidence bytes | [Eq proof reads](eq-proof-charges.md) |

Why these two write charges. Root maintenance is prepaid per logical entry
write: `RootUpdate` counts entries, not trie nodes, so a receipt depends only
on the operation and the logical state, never on the shape of a structure,
a cache or the disk layout. `RowByte` prepays re-encoding and rewriting the
whole row leaf: a patch rewrites every byte of its row even when it sets one
small cell, which `CellByte` (the bytes written) does not cover. The
`RootUpdate` counts are the logical entry writes: a namespace creation or
transfer writes the catalog leaf (1); a raw put or delete writes the raw
entry and the catalog leaf (2), a delete whether or not the key existed; a
record creation or deletion writes the key binding, the row and the catalog
leaf (3); a patch writes the row and the catalog leaf (2), the key binding
being unchanged; an index insert or removal writes the posting entry and
the term leaf (2). Since node step N3b the engine computes the roots these
charges prepay, under the same receipts (every receipt literal of the test
suite is unchanged). The
host-owned map (node step N0b-2, `Branch::try_host_put` and its siblings)
is unmetered, and the engine does not limit how often the host writes it.
The bound is the host's own rule: the block executor of node step N0b-3
(`arkiv_node::execute_block`) writes at most 4 due-count entries per user
operation (a patch moving a record's expiry height updates the old and new
due and total entries) and at most 2 per swept (height, namespace) (the
removed due entry and, once per height, the removed total).

The work behind one `RootUpdate` is bounded by the fixed key length of each
map (`L` bytes, `2L` nibbles). A lookup path holds at most `2L + 1` nodes,
but the prepaid work is the nodes an entry write **creates**, which the next
root computation encodes and, at 32 bytes or more, hashes: an insert creates
at most `2L + 2` (the path plus a leaf or extension split below the new
branch), a replacement `2L + 1`, a removal `2L` (see the
[state trie notes](state-trie.md#nodes-an-update-creates), where the trie
crate reaches each bound exactly). An earlier version of this table quoted
the path bounds; the creation bounds replace them:

| Map | Key bytes | Nodes created per entry write, at most (`2L + 2`) |
| --- | --- | --- |
| Catalog (`NamespaceId`) | 8 | 18 |
| Rows and postings (`RecordId`) | 8 | 18 |
| Raw records (key padded to 20 + length) | 21 | 44 |
| Host map (raw `[u8; 32]`, uncharged) | 32 | 66 |
| Key bindings (`RecordKey` encoding v2) | 33 | 68 |
| Terms (name, tag, presence, value, fixed widths) | 88 | 178 |

Every namespace-local operation also rewrites its namespace's catalog leaf,
which is one of its `RootUpdate` units (the catalog row above), and a block
adds one state-header digest. Measured (node step N3b, `rehash.rs`, which
commits every block to an object store, reopens it, and counts per map the
new nodes each block hashes, one keccak256 each when the root is computed,
and the distinct objects it stages): in every block of a 20-block mixed
workload each map's hashed nodes stay below its entry writes times its
bound (typically a few percent of it: for example 24 term nodes against
1,068 allowed, 3 key nodes against 68), and its staged objects never exceed
its hashed nodes (identical nodes built by different writes, such as the
one-entry posting maps of a new record's terms, are hashed each time and
stored once). Crafted deep paths reach the bounds closely: 66 record keys
that branch at every possible nibble make one more key rehash exactly 66
key-map nodes (bound 68; the new leaf is inline), and 214 terms whose spine
term branches at every nibble a valid term can branch on (175 path nodes
of the 177 possible: the name length and the tag and presence high
nibbles cannot differ while the bytes before them agree; the value length
can, because a `Str` value may end in zero bytes) make one more record
holding the spine term rehash exactly 176 term nodes (two term writes,
bound 356). These are logical counts, not time: hashing cost per node
grows with node size and no throughput is claimed.

Two properties of the fixture that a production schedule must keep:

- `IndexTermCreate > 0`. Creating a distinct term must cost more than adding
  a membership to an existing posting; the engine charges the class whenever
  a term is new, but only a positive weight makes the rule hold. This is a
  schedule invariant, not something the counting rules can enforce.
- Symmetric structural delete weights: `RecordDelete = RecordCreate`,
  `CellRemove = CellWrite` and `IndexRemove = IndexInsert`, so a delete costs
  the create minus its bytes (`CellByte`, `RowByte`) and term creation; the
  `RootUpdate` counts of a create and of the delete of the same record are
  equal. Expiry deletion runs through the same charges under host authority;
  Arkiv funds it through its create/lifetime policy.

Charge points, in order; each charge precedes the work it covers. Braces mark
an atomic group: either every class in it fits and is charged, or nothing in
the group is charged and the operation stops. `k` and `v` are key and value
lengths in bytes; `c` = cells of the record, `a` = its attribute cells, `s`
and `r` = a patch's set and remove counts, `b` = Σ over the cells written of
(name bytes + encoded value bytes), `n` = the stored row after the write =
key bytes + Σ over the row's cells after the write of (name bytes + encoded
value bytes), `p` = projected bytes = key bytes if the key is projected + Σ
over projected cells of (name bytes + encoded value bytes), `g` = groups of
a query. The table is model version 3; the version-2 row of a write is the
same row without its `RowByte` and `RootUpdate` charges.

| Operation | Charges and checks, in order | Total under FIXTURE |
| --- | --- | --- |
| `CreateNamespace` | `DirectoryLookup` 1; `DirectoryFull` check; {`NamespaceCreate` 1, `RootUpdate` 1}; allocate ordinal, store validated name, owner = actor, revision 0 | 53 |
| `TransferNamespace` | `DirectoryLookup` 1; `UnknownNamespace`, then `Unauthorized`, then `StaleRevision` (each rejects with spent 1); {`NamespaceTransfer` 1, `RootUpdate` 1}; owner := new owner, revision += 1; record map and typed store shared | 23 |
| `Put` | `DirectoryLookup` 1; `UnknownNamespace`/`Unauthorized` (spent 1); {`PutRecord` 1, `WriteKeyByte` k, `WriteValueByte` v, `RootUpdate` 2}; insert | 15 + 2k + v |
| `Delete` | `DirectoryLookup` 1; `UnknownNamespace`/`Unauthorized`; {`DeleteRecord` 1, `WriteKeyByte` k, `RootUpdate` 2}, charged even if the key is missing; remove (a missing key succeeds with `existed = false`) | 10 + 2k |
| `get` | `DirectoryLookup` 1; `UnknownNamespace`; `PointRead` 1; lookup; if present `ReadByte` v, charged before the value is copied | present 4 + v; missing 4 |
| `scan_prefix` | `DirectoryLookup` 1; `UnknownNamespace`; `ScanSeek` 1; seek; per matching entry {`ScanEntry` 1, `ReadByte` k_i + v_i} charged before the entry is materialized | 5 + Σ(2 + k_i + v_i) |
| `descriptor` | `DirectoryLookup` 1; `UnknownNamespace` | 1 |
| `CreateRecord` | `DirectoryLookup` 1; `UnknownNamespace`/`Unauthorized` (spent 1); `PointRead` 1 (key binding); `AlreadyExists` (spent 4); `CounterExhausted` (spent 4); {`RecordCreate` 1, `CellWrite` c, `CellByte` b, `RowByte` n, `RootUpdate` 3}; per attribute cell in name order, its value term then its existence term: lookup, {`IndexInsert` 1, `IndexTermCreate` 1 if the term is new, `RootUpdate` 2}, insert; bind key, store row. No key-byte charge beyond `RowByte` | 30 + 4c + b + n + 20a, + 30 per new term |
| `PatchRecord` | `DirectoryLookup` 1; `UnknownNamespace`/`Unauthorized`; `PointRead` 1; `NotFound` (spent 4); `RecordFull` (spent 4, recorded) if the result would exceed `max_cells`; {`RecordPatch` 1, `CellWrite` s, `CellByte` b, `CellRemove` r, `RowByte` n, `RootUpdate` 2}; per name in sorted order over set ∪ remove, with old = the terms the record's cell holds and new = the terms the resulting cell holds: {`IndexRemove` 1, `RootUpdate` 2} per term in old − new (value term first), then per term in new − old: lookup, {`IndexInsert` 1, `IndexTermCreate` 1 if new, `RootUpdate` 2}, insert; store row | 18 + 4s + b + 4r + n, + 10 per removed term, + 10 per inserted term, + 30 per new term |
| `DeleteRecord` | `DirectoryLookup` 1; `UnknownNamespace`/`Unauthorized`; `PointRead` 1; `NotFound` (spent 4); {`RecordDelete` 1, `CellRemove` c, `RootUpdate` 3}; per attribute cell in name order: {`IndexRemove` 1, `RootUpdate` 2} (value term), {`IndexRemove` 1, `RootUpdate` 2} (existence term); unbind key, remove row | 30 + 4c + 20a |
| `get_record` | `DirectoryLookup` 1; `UnknownNamespace`; `PointRead` 1; unbound → `Ok(None)` (spent 4); {`ResultRow` 1, `ReadByte` p}; materialize the row | bound 7 + p; unbound 4 |
| `query` | `DirectoryLookup` 1; `UnknownNamespace`; per group `QueryGroup` 1 plus the source opening and enumeration below; per emitted row {`ResultRow` 1, `ReadByte` p} | 1 + 5g + 4 per seek + 3 per term visited + 1 per candidate + 2 per probe + Σ(3 + p) over rows |
| `count` | as `query` with no cursor and no row charges, until every group is exhausted | 1 + 5g + seeks + term visits + candidates + probes |
| `terms` | `DirectoryLookup` 1; `UnknownNamespace`; `TermSeek` 1; per returned term, after the node step that reveals it and before it is decoded, {`TermVisit` 1, `ReadByte` encoded value bytes (1 `Bool`, 8 `I64`/`U64`, byte length `Str`)}; an `Excluded` lower bound equal to an existing term and the key that ends the `(name, type)` run are uncharged; a missing `(name, type)` costs the seek only; stops after `limit` terms with no peek charge | 5 + Σ(3 + bytes) |

Unchanged terms of a patch cost nothing: the same value charges nothing; the
same `(name, type)` with a new value removes and inserts the value term and
leaves the existence term untouched; a type or kind change removes and
inserts both terms. Term newness follows one rule: a value term `(name,
type, value)` is new iff no live record of the namespace holds an attribute
with exactly that name, type and value at the moment of the insert; an
existence term `(name, type)` is new iff no live record holds an attribute
of that name and type.

Query evaluation is lazy and positioned after the cursor; every source yields
ids greater than `after` only:

1. `DirectoryLookup` 1; `UnknownNamespace`.
2. For each group in order, open it: `QueryGroup` 1, then by the group's
   first literal. An empty group (match-all) or a negated first literal has
   no seek: the row map is its source. `Eq`: `TermSeek` 1 on the value term.
   `Exists`: `TermSeek` 1 on the existence term. `NotEq`: `TermSeek` 1 on the
   existence term of `(name, value type)`. `Range` and `StartsWith`:
   `TermSeek` 1, then `TermVisit` 1 for each term key of `(name, type)` whose
   value satisfies the bounds or starts with the prefix, in key order; the
   existence term, a stepped-over `Excluded` endpoint, the key that ends the
   run and other types are never charged. A missing term or an empty run
   costs the seek only.
3. For each group in order, pull its first head. Candidates come from the
   source in id order, each charged `PostingEntry` 1 (a posting or a merged
   run) or `RowVisit` 1 (the row map) as it is revealed. Each candidate is
   probed candidate-major by the group's probe list in order until a probe
   fails, `RecordProbe` 1 per probe. The probe list is the first literal
   itself when it is `NotEq` or negated (its source is only a domain),
   followed by every further literal of the group in order; a negated
   further literal is just a probe. A candidate that fails a probe is not
   probed by later literals. The head is the first candidate passing every
   probe.
4. Merge loop: let m be the smallest head over the groups. If there is
   none, the page is done with `next = None`. If the page already holds
   `limit` rows, it is done with `next = Some`, `after` = the last row's
   id. Otherwise charge {`ResultRow` 1, `ReadByte` p}, materialize row m,
   then advance every group whose head equals m, in group order (each pull
   charges as in step 3 until the group's next head or its exhaustion), and
   repeat.

`count` runs steps 1 to 3 with no cursor, then the merge loop without row
charges until every head is exhausted; the total is the number of distinct
ids emitted.

Consequences. An `Eq` page of `limit` rows over a posting with more entries
charges `limit + 1` `PostingEntry` (the head beyond the page); the last page
charges exactly the remaining entries. A group is advanced only when its head
is emitted, so a group whose head lies beyond the page end does no further
work. The cost statements are these. Positive `Eq`/`Exists` sources, the
row map (match-all, negation) and the existence posting (`NotEq`) are
positional: a page charges the candidates scanned to fill it plus each
group's first match beyond it. A `Range`/`StartsWith` source additionally
charges `TermSeek` 1 and `TermVisit` 1 for every qualifying term of the run
on every page (the id-ordered merge consults every qualifying term's
posting per page), so a walk of P matches over T qualifying terms with page
limit L charges ceil(P/L) × T `TermVisit` in total; T is bounded by the
namespace's own distinct values. Match-all, negated and `NotEq` first
literals are linear in the namespace's own live rows or existence posting
per page scanned. None of this depends on other namespaces. A page carries
no total because a total would need the full walk these rules avoid;
`count` is the separate metered read for it. Nothing is reordered or planned:
the charged cost and the abort point are a pure function of the state and the
query text, so a caller chooses each group's index source by putting the
selective positive literal first. A budget end anywhere rejects with
`OutOfBudget`, the units spent so far and no partial page. Every per-entry
class (`PostingEntry`, `TermVisit`, `RowVisit`, `RecordProbe`) is charged one
unit at a time immediately before the entry is used, after the node step that
reveals it, like the milestone-1 scan: an `OutOfBudget` at entry k has charged
exactly k−1 entries of that kind.

Covered work. The following bounded steps are not charged separately; each
is covered by the charge next to it, and none can grow with another
namespace or with uncharged data:

- The term-map lookup that decides whether `IndexTermCreate` applies precedes
  the `IndexInsert` group and is covered by it: one O(log T) lookup over the
  namespace's own terms.
- The row-map load after a key lookup is covered by `PointRead`: two
  O(log n) lookups over the namespace's key and row maps for one unit.
- The patch cell merge (the record's sorted cells with the sorted `set`,
  minus `remove`, and the resulting-size check) is one linear merge,
  O(cells + set + remove) with at most 2 × `max_cells` entries, and
  precedes the `RecordPatch` group; the `RowByte` count `n` is a sum over
  the merged cells, computed before the group is charged. A create sums `n`
  over its own sorted cells before its group in the same way.
- Row emission computes p before cloning any cell, and a row already loaded
  by a probe or by a row-map source is reused for emission rather than
  loaded again (fewer `record_nodes`; charges unchanged).
- Each `TermVisit` covers one comparison of at most 128 bytes (the indexed
  `Str` cap; fixed-width terms are at most 32 bytes) and the
  positioning of that posting after the cursor.
- The run merge (a heap over the run's postings) is O(log k) per charged
  `PostingEntry`, with k at most the charged `TermVisit`s.
- The union over groups (the minimum over at most `max_groups` heads per
  emitted row, and the advance of every group whose head was emitted) works
  only over charged candidates.
- Admission validation is O(`max_groups` × `max_literals` + `max_cells`)
  before any charge, bounded by the instance parameters.
- The node step that ends a run (the first key outside it) and a stepped-over
  `Excluded` endpoint are one uncharged step each.

Counting vectors (a namespace with three records created in order `a`, `b`,
`c` under 1-byte keys, each with attributes `kind: Str`, `price: I64`,
`$expiresAt: U64(5)` and a field `note: Bytes("xyz")`; kinds `book`, `book`,
`pen`; prices 10, 20, 5):

- create `a` (4 cells, b = 46, n = 1 + 46 = 47, all 6 terms new): 1 + 3 +
  20 + 16 + 46 + `RowByte` 47 + 36 + 180 + `RootUpdate` 2 × (3 + 2·6) =
  379 (the model-2 total 302 + 47 + 30); create `b` (only `price: 20` new):
  152 + 47 + 30 = 229; create `c` (b = 45, n = 46; `kind: pen` and
  `price: 5` new): 181 + 46 + 30 = 257.
- query `Eq(kind, "book")`, `Projection::ALL`, limit 1, no cursor: 1 + 5 + 4
  + `PostingEntry` 2 + 3 + `ReadByte` 47 = 62, rows [1], `next` after 1; the
  second page with that cursor: 1 + 5 + 4 + `PostingEntry` 1 + 3 + 47 = 61,
  rows [2], `next = None`.
- query `Range(price, Excluded(I64(5)), Unbounded)`, `ID_ONLY`, limit 10:
  1 + 5 + 4 + `TermVisit` 2 (10, 20) + `PostingEntry` 2 + `ResultRow` 2 =
  24, rows [1, 2].
- query `NOT StartsWith(kind, "bo")`, `KEY_ONLY`, limit 10: 1 + 5 +
  `RowVisit` 3 + `RecordProbe` 3 + `ResultRow` 1 + `ReadByte` 1 = 19, rows
  [3].
- count match-all: 1 + 5 + 3 = 9; count `Exists(price, I64)`: 1 + 5 + 4 + 3
  = 13.
- patch `a`: set `price := attribute U64(10)`, remove `[note, missing]`:
  1 + 3 + 10 + 4 + 13 + 8 + `RowByte` 40 (key 1 + `$expiresAt` 18 + `kind`
  8 + `price` 13) + `IndexRemove` 12 + `IndexInsert` 12 + `IndexTermCreate`
  60 (the `U64` value and existence terms are new) + `RootUpdate` 2 × (2 +
  4·2) = 183.
- `expire_records(ns, $expiresAt, block 5)` afterwards deletes 1 (now 3
  cells, 3 attributes: 4 + 20 + 12 + 36 + `RootUpdate` 2 × (3 + 6·2) =
  102), then 2 and 3 (4 cells, 3 attributes: 76 + 30 = 106 each), in
  (height, id) order (one height, 5, here; the `terms` and `Eq` page read
  receipts are discarded and unchanged from model version 2).
- `get_record` of `a` at the first snapshot, `ALL`: 1 + 3 + 3 + 47 = 54.

Meter rule: `subtotal = count × price` (checked; overflow rejects with
`CostOverflow`); `next = spent + subtotal` (checked; overflow rejects with
`CostOverflow`); if `next > limit` reject with `OutOfBudget` and leave `spent`
unchanged; otherwise `spent = next`. An exact budget succeeds. A rejection at
the first charge is a zero-cost rejection. A budget end in the middle of a
scan or query rejects with `OutOfBudget`, the units spent so far and no
partial result. Every `execute` mutates an O(1) clone of the working state;
a rejected operation restores it, so its logical changes never apply.

Receipts and budgets:

- `Receipt { kind: Write | Read, model_version, schedule_id, spent, outcome:
  Applied | Rejected(reason), ledger: Option<Ledger> }` is canonical, derives
  `Eq`, and is issued for every operation, rejected or not. Receipts are never
  withdrawn by rollback or a discarded candidate.
- `Budget::limit(n)`, `Budget::unlimited()`, `.traced()`. A ledger is present
  only when tracing was requested; it aggregates entries per class in
  first-charge order and omits zero-count classes. Within an atomic group
  the classes are charged in the order the table lists them, so a record
  create's ledger shows `RowByte` and `RootUpdate` before its first
  `IndexInsert`, and later `RootUpdate` charges add to that line. Tracing
  never changes charges.
- Effective write limit: admission first. Malformed arguments
  (`InvalidArgument`) and a non-record operation on the host path are
  rejected before any charge with a receipt of spent 0 that is not recorded.
  Then, if the operation budget has a limit `l` and `l > remaining`, the
  operation is rejected with `ExceedsBlockBudget { remaining }` before any
  charge, with a receipt of spent 0 that is **not** recorded in the branch's
  receipts or spent total; the operation belongs in a later block. Otherwise
  the limit is the operation's limit if it has one, else the block's
  remaining budget. `remaining = block_limit − spent`, where `block_limit` is
  `u64::MAX` when `begin` was given no block budget.
- Block aggregate: the branch's `spent()` equals the sum of its recorded
  write receipts, including rejected operations, and equals the sealed
  candidate's `spent()`. Rejected operations' spent units count against the
  block. `RecordFull` is a post-charge rejection and is recorded like
  `NotFound` or `AlreadyExists`.
- Reads (`get`, `scan_prefix`, `descriptor`, `get_record`, `query`, `count`
  and `terms` on a branch or snapshot) are limited by their own budget
  only; they never deduct from the block and are never stored. Their
  receipts have `kind = Read` and are not gas. An invalid query or `terms`
  request, an oversized projection or a mismatched cursor is rejected at
  admission with a read receipt of spent 0.
- Pricing is pinned at the branch base: writes, estimates and branch reads
  use the base snapshot's schedule; reads at a snapshot use that snapshot's
  schedule. `Branch::set_pricing(schedule)` commits a new schedule into the
  candidate for branches based on the published result; its id must exceed
  the currently committed id (`StaleSchedule` otherwise), so every receipt
  names a unique pricing context, and it is rolled back by checkpoints like
  any other state.
- Estimation parity: `estimate` and `estimate_host` run the same code as
  `execute` and `execute_host`, including the admission checks and the
  block's current remaining budget, on a scratch O(1) copy; nothing is
  changed, recorded or charged to the block. They equal execution only for
  the same starting state, operation and pricing.
- Physical trace: `PhysicalTrace { directory_nodes, record_nodes,
  index_nodes }` is returned next to every receipt but is not part of it. It
  is non-canonical instrumentation of the trie layout for isolation
  measurements and changes with the data structure (node step N3b moved it
  from AVL to trie shapes). Every trie node an operation examines counts
  once per step that examines it, inline children included, whether it was
  in memory, served by a cache or loaded: a lookup or update counts the
  key's path (a write's directory count is its lookup plus its update path),
  a removal also the sibling it loads when a branch collapses, and an
  iterator its seek path plus every node it steps into. `record_nodes`
  counts the selected namespace's byte map, key map and row map visits;
  `index_nodes` counts its term map and posting map visits (term lookups,
  run scans and posting traversals); a transfer visits zero record and
  index nodes. Iterators are lazy: a posting or the row map is stepped first
  at its group's first head pull, a term run steps each qualifying posting
  once as it opens. A scan or posting traversal that runs out of budget at
  entry k has charged k−1 entries; the nodes stepped into to reveal entry k
  were visited but its entry was neither charged nor returned. A traversal
  run to its end visits every node at or above its seek position once. An
  aborted `get` has the same trace as a successful one. A `query` or `count` rejected at
  any charge reports every node visited before that charge, once: the lookup
  path and the posting or row-map traversal of every group already open; the
  term run and the postings positioned by a `Range`/`StartsWith` run that
  stopped at a `TermVisit`; the steps of head pulls and advances; and the rows
  loaded for probes and emission. This was fixed in node step N0a. Before it,
  a rejection while opening a later group or pulling a first head dropped the
  sources already open, and a stopped run dropped its positioned postings.

Remaining rules for later milestones:

- Projection is charged for the projected bytes only, and predicates are
  evaluated on loaded rows under `RecordProbe`; sorting does not exist.
  Point-proof construction reserves `ProofNode` and `ProofByte` (model
  version 3); their charge points are specified with the point proofs of
  node step N3d, and complete query proofs need their own rules later.
- `begin`, checkpoint/rollback, seal and publication remain host-controlled
  lifecycle operations, with work covered by the write schedule rather than
  separate caller charges. With structural sharing each of them is an O(1)
  clone or append in memory. Root maintenance is prepaid by `RootUpdate` and
  `RowByte` since model version 3; the roots exist since node step N3b and
  the nodes they rehash per block are measured against the prepaid entry
  writes (see the bound table above). The durable commit (N4) is not
  implemented, so commit-fit bounds and hashing time are unmeasured.
- For historical reads, the implemented default prices a read with the
  selected snapshot's schedule. Any different RPC pricing policy must be
  explicit and must not change which block supplies the query data or proof.
  A query result proof does not automatically authenticate its cost receipt.

Production calibration, historical read pricing policy through RPC and
proof-specific counts remain explicit later gates; none permits shipping an
unmetered new data-plane operation.

## Boundaries and implemented interfaces

Block numbers and ordered packets belong to the host; the host supplies
validated block context and establishes finality. Internal snapshot
identities stay distinct from heights. The implemented API (all types are
re-exported from `arkiv_db`; see `crates/arkiv-db/examples/blocks.rs` for
the block flow and `examples/records.rs` for the typed flow):

```text
Engine::new(PriceSchedule) / Engine::with_namespace_cap(schedule, cap) / Engine::with_limits(schedule, Limits)
Engine::{head, head_snapshot, snapshot(id), snapshot_count, namespace_cap, limits}
Engine::begin(expected_head, block_budget: Option<Cost>) -> Result<Branch, EngineError>
Branch::execute(actor: OwnerId, WriteOp, Budget)  -> Result<Executed, Rejected>
Branch::estimate(actor, WriteOp, Budget)          -> Result<Executed, Rejected>   // scratch state
Branch::execute_host(WriteOp, Budget) / Branch::estimate_host(WriteOp, Budget)  // record ops only, no owner check
Branch::checkpoint() -> Checkpoint / Branch::try_checkpoint() -> Result<Checkpoint, EngineError>
Branch::rollback_to(Checkpoint) -> Result<Range<usize>, EngineError>            // discarded receipt indexes
Branch::set_pricing(PriceSchedule) -> Result<(), EngineError>
Branch::is_poisoned() -> bool                                                   // a storage error was seen (N3b)
Branch::{get, scan_prefix, descriptor}(namespace, .., Budget) -> Result<Metered<T>, Rejected>
Branch::get_record(namespace, &RecordKey, &Projection, Budget) -> ReadResult<Option<Row>>
Branch::query(namespace, &Query, &Projection, limit, Option<&Cursor>, Budget) -> ReadResult<QueryPage>
Branch::count(namespace, &Query, Budget) -> ReadResult<u64>
Branch::terms(namespace, &CellName, TypeTag, Bound<Value>, limit, Budget) -> ReadResult<Vec<Value>>
Branch::{base, pricing, block_limit, spent, remaining, receipts, limits}
Branch::{namespace_count, record_count, typed_record_count, term_count}       // unmetered statistics
Branch::seal(self) -> SealedCandidate               // { base, receipts, spent }
Branch::try_seal(self) -> Result<SealedCandidate, EngineError>                // Poisoned on a poisoned branch
Branch::try_state_root(&self) -> Result<B256, EngineError>                    // the root a seal now would have; Poisoned on a poisoned branch
SealedCandidate::{base, receipts, spent, state_header, state_root}
Engine::publish(&SealedCandidate) -> Result<SnapshotId, EngineError>          // in-process only
Snapshot::{id, base, pricing}
Snapshot::{state_header() -> StateHeader, state_root() -> B256}               // node step N3b
Snapshot::{get, scan_prefix, descriptor, get_record, query, count, terms}(namespace, .., Budget)
Snapshot::{namespace_count, record_count, typed_record_count, term_count,
           records_shared_with, typed_shared_with}                             // unmetered statistics
WriteOp::{CreateNamespace { name: NamespaceName },
          TransferNamespace { namespace, expected_revision, new_owner },
          Put { namespace, key, value }, Delete { namespace, key },
          CreateRecord { namespace, key: RecordKey, cells: Vec<(CellName, Cell)> },
          PatchRecord { namespace, key, set: Vec<(CellName, Cell)>, remove: Vec<CellName> },
          DeleteRecord { namespace, key }}
Effect::{NamespaceCreated(id), Transferred { revision }, Put { replaced }, Deleted { existed },
         RecordCreated { id }, RecordPatched { id }, RecordDeleted { id }}
EngineError::{StaleHead { expected, head }, ForeignCandidate, UnknownSnapshot,
              InvalidCheckpoint, StaleSchedule { current }, Poisoned}         // no receipt
Rejection::{UnknownNamespace, Unauthorized, StaleRevision { expected, current },
            ExceedsBlockBudget { remaining }, OutOfBudget, CostOverflow,
            DirectoryFull, CounterExhausted, AlreadyExists, NotFound, RecordFull,
            InvalidArgument(ArgumentFault), InvalidQuery(QueryFault), CursorMismatch}  // receipt issued
host::BlockMap::{record(block, snapshot), resolve(block), at(&engine, block), next_block, latest}
host::expire_records(&mut Branch, namespace, &CellName, BlockNumber, query_budget, delete_budget)
    -> Result<Vec<(RecordId, Receipt)>, ExpiryHalt { applied, rejected }>

// Node step N0b-2: the fallible surface. Each returns Ok(what the infallible
// method returns); Err(StorageError) is an availability failure (never on a
// memory engine; on an engine reading stored nodes since N3b, where it
// poisons a branch). StorageError = arkiv_db_storage::versioned::StorageError.
Branch::{try_execute(actor, op, budget), try_execute_host(op, budget)}           -> Result<WriteResult, StorageError>
Branch::{try_estimate(actor, op, budget), try_estimate_host(op, budget)}         -> Result<WriteResult, StorageError>
Branch::/Snapshot::{try_get, try_scan_prefix, try_descriptor, try_get_record,
                    try_query, try_count, try_terms}(..same arguments..)          -> Result<ReadResult<T>, StorageError>
Branch::/Snapshot::{try_namespace_count, try_record_count, try_typed_record_count,
                    try_term_count}                                              -> Result<usize | Option<usize>, StorageError>
Snapshot::{try_records_shared_with, try_typed_shared_with}(&other, namespace)    -> Result<bool, StorageError>
host::try_expire_records(..same arguments..)
    -> Result<Result<Vec<(RecordId, Receipt)>, ExpiryHalt>, StorageError>
// The host-state map: unmetered, host-internal, never a data-plane read.
Branch::try_host_get(&[u8; 32]) / Snapshot::try_host_get(&[u8; 32])          -> Result<Option<Vec<u8>>, StorageError>
Branch::try_host_put([u8; 32], &[u8])   // value ≤ HOST_VALUE_MAX = 256 bytes
    -> Result<Result<Option<Vec<u8>>, HostStateError::ValueTooLarge { len }>, StorageError>  // previous value
Branch::try_host_remove(&[u8; 32])                                             -> Result<Option<Vec<u8>>, StorageError>
Branch::/Snapshot::try_host_range(from: &[u8; 32], limit)                      -> Result<Vec<([u8; 32], Vec<u8>)>, StorageError>
// The canonical codec (docs/block-format.md).
codec::{Encode::encode_to(&self, &mut Encoder), Decode::decode_from(&mut Decoder) -> Result<Self, DecodeError>}
codec::{encode(&T) -> Vec<u8>, decode_exact::<T>(&[u8]) -> Result<T, DecodeError>,
        digest(domain: &str, bytes: &[u8]) -> B256, CODEC_VERSION = 1}
PriceSchedule::new(id, [u64; 31])        // weights in OperationClass::ALL order
```

The infallible methods remain and delegate to their `try_` forms. There is
no `Engine::try_snapshot` or `try_head_snapshot`: pinning a retained
snapshot is a pointer copy that touches no storage, and the durable
`DurableEngine::snapshot(SnapshotId)` is a separate fallible call reading
its manifest and index associations without trie loads. `begin`,
`checkpoint`, `rollback_to`, `set_pricing`, `seal` and `publish` walk no
state either; on a poisoned branch `try_checkpoint`, `rollback_to`,
`set_pricing`, `try_state_root` and `try_seal` return `EngineError::Poisoned`. Host writes
advance a branch's mutation serial, so a branch cursor issued before one is
a `CursorMismatch`.

Planned, not implemented: `prove_query`, `verify_query(trusted_root,
namespace_id, request, response) -> VerifiedPage`. The design's scoped
`Branch::namespace(id)` handle was not adopted; every operation names its
namespace explicitly and the engine resolves it once through the directory.
The conceptual RPC's `select` is the `Projection`, its `order` is the record
id (the only order), and an omitted snapshot is resolved by the host once to
a `Snapshot` handle that the cursor then names.

`publish` means in-process publication. It does not carry the production
contract's durable `commit` semantics. Adding snapshots did not change
`KvStore::write_batch` into a versioned/durable commit API: the snapshot
boundary is above KV, and the raw KV tests and facade behaviour are unchanged.

A snapshot handle owns everything it reads: it holds an `Arc` to the
published state, and no read consults the mutable head. Shared immutable
ownership does not imply a concurrent writer API. Only publication advances
head, and `Engine::publish` takes `&mut self`. The raw KV facade cannot reach
engine state, so it cannot bypass the snapshot layer.

Each published state is a set of structurally shared Merkle Patricia maps,
so publication is an O(1) append rather than an O(database size) copy; a
branch is a working clone plus a stack of checkpoint clones over its base.
This removes the full-copy reference mechanism the design allowed. The
nodes a block creates and must hash are measured (`rehash.rs`); the
production commit-fit targets are still unmeasured.

Every published snapshot is kept during the process lifetime; pruning is
deferred and there is no snapshot capacity limit yet. IDs are never reused
within an engine instance; restarting this volatile implementation starts a
new instance, not a recovered history.

The host resolves a block once to its terminal snapshot and records that
mapping in `BlockMap` only after all data/admin transitions are ready: blocks
after the first must be consecutive, snapshots strictly increasing, and a
block can be recorded once. Empty blocks resolve too. Snapshot IDs are not
block heights. Real block/header root authentication belongs to the host;
local fixtures may supply trusted test roots but cannot establish chain
finality.

## Milestones and exit criteria

The milestone numbers below identify existing feature groups. Their original
strict order is superseded by [steps N0–N6](node-storage-plan.md#ordered-implementation-steps-and-exit-criteria)
and the immediate [demo steps D0–D3](full-light-demo.md#implementation-order-and-review-gates).
The demo implements one bounded complete equality-proof profile before
durability; atomic storage/recovery precedes the rest of milestone 4.
Archive/retention work follows, with no pruning in the demo.

### 0. Namespace contract and isolation harness — met in memory

Namespace IDs/addressing, the catalog/descriptor layout, ownership transition
preconditions and the shared-directory bound are frozen and implemented; the
cross-namespace test harness exists for the implemented operations. Exact
public SDK API and migration remain versioned design work; existing unscoped
calls are not assumed compatible.

Evidence: `crates/arkiv-db/tests/namespaces.rs` and `isolation.rs`. The
storage layout has no dependency on other projects' record enumeration and
ownership transfer is a metadata operation. This gate approves the design and
the measured in-memory bound for put, delete, transfer, get, scan and
descriptor; it is not a claim for proofs. (The directory is committed in the
state root since node step N3b, and its bound is the trie path bound: 17
nodes per lookup, 34 per write.)

### 1. Metered operations, immutable KV snapshots and publication — met in memory

- In-memory namespace catalog, ordinal allocator and namespace-scoped
  operations, covering creation, stale/unauthorized transfers, same-block
  authority changes, rollback and stable data identity across a transfer.
- Shared integer meter, write/read budgets, receipts and optional debug
  ledger, exposed only through the metered data plane; the raw KV facade
  cannot bypass accounting.
- Retained snapshots, read handles and isolated write branches.
- Checkpoints and rollback; a failed span leaves its base unchanged.
- Seal freezes a candidate; publish checks that its base is still head, so
  only one of competing candidates wins and discarded work changes no
  published snapshot. Roots are added in milestone 3.
- Publication appends the complete state before advancing head; no reader
  observes a partially applied block or a mix of old and new structures.
- Host block map with distinct block/snapshot numbering, empty blocks and
  separate admin/data stages.

Evidence: the six engine test files `blocks.rs`, `rollback.rs`,
`namespaces.rs`, `metering.rs`, `history.rs` and `isolation.rs` under
`crates/arkiv-db/tests/`, alongside the unchanged raw KV tests. An
independent mutation-log oracle agrees with get/scan at every published
snapshot after later puts, deletes, replacements, rollbacks and competing
publication attempts; historical scans include subsequently deleted keys;
hand-derived vectors verify receipts, exact-budget success, budget-abort
rollback and estimation parity. See the [test strategy](testing.md).

Remaining: production cost calibration and broader directory-proof APIs.
The demo proves the directory for complete Eq pages; N3c now persists the
engine state with the [crash/recovery evidence](engine-storage-v1-report.md).
Durable protocol-block publication remains N4.

### 2. Record/cell state and historical queries — met in memory

- Namespace-local encodings, record identity and key bindings, engine state
  and typed cells above the record maps are frozen as listed above. The SDK
  key-encoding gap left open by this milestone is settled by node step
  N0b-1 (32-byte keys, encoding version 2); SDK wire compatibility is still
  not claimed.
- Data and indexes update in the same operation of the same branch and are
  published together. Existence postings, key bindings, the all-live-records
  domain and the record-id allocator live in one namespace and one snapshot
  as sibling fields of the record map in `NamespaceState`.
- The declared query subset is typed equality, inequality, integer ranges,
  string prefixes, existence and negation in an ordered flat DNF, with
  record-id ordering, projection and keyset pagination. Everything outside
  it (`Bytes` terms, unbounded or mixed ranges, ranges on unordered types,
  oversized values, limits and projections) is rejected explicitly at
  admission. Attribute sorting and other orders are not implemented.
- A cursor binds the engine instance and snapshot (or the branch and its
  mutation serial), the namespace, the query and the projection, and never
  re-resolves head; a page limit is not bound.
- Expiry is a host-executed deletion in the block where the record expires.
  Historical reads use historical liveness; no read consults a clock or the
  head.
- The meter covers index changes, predicates, projection and pagination
  under model version 2 (version 3 since node step N0b-1) with a fixed
  evaluation order and a fixed read pricing context. The encoded-object
  cache in N3c changes physical loading, never logical metering.
  The interference test covers every operator; no operator evaluates a
  global index and filters by namespace afterwards.

Evidence: the seven engine test files `records.rs`, `queries.rs`,
`pagination.rs`, `query_metering.rs`, `query_history.rs`,
`query_isolation.rs` and `terms.rs` under `crates/arkiv-db/tests/`, the
example `examples/records.rs`, and the unchanged milestone-0/1 tests (only
the model-version literal and, in `history.rs`, exhaustive match arms for
the new variants changed). Indexed queries agree with a full-scan reference
evaluator over an independent record model at every published snapshot,
including patch and type changes, deletion, expiry and key recreation;
a page walk is identical whether or not new snapshots are published between
pages; values and predicates use one snapshot; receipts, abort points and
ledgers are hand-derived from the table above; namespace A's query charges
and node visits are unchanged while other namespaces grow records, identical
terms and directory entries. See the [test strategy](testing.md).

Remaining: engine-assigned keys, sorting, descending order and offset
paging are not implemented; weights are uncalibrated; the directory is
committed in the state root and proven to Eq-page verifiers; CPU and heap
are shared. Broader proof profiles and N4 node durability remain open;
N3c persists this record/query state.

### 3. Deterministic roots and point proofs

Status: **roots and the demo Eq-page point-proof composition are implemented
in memory and on reopened durable snapshots**. N3c engine storage v1 is
done; other N3d point-read APIs remain open.
The engine keeps every map in `arkiv-db-trie`, the frozen state-format-1
key and leaf encodings commit cells, index entries, allocators, descriptors
and the host map, and `state_root = digest("arkiv/state/v1",
encode(StateHeader))` is exposed on snapshots and sealed candidates.
Evidence: `roots.rs` (every map root and the state root equal an
independent construction over hand-built leaves; identical content gives
one root whatever the history; old roots never change; every logical field
changes the root) and `rehash.rs` (rehashing per block against the prepaid
`RootUpdate` writes). The demo activates header v2; N3c adds durable
manifests, bounded loading/staging and the same historical proof bytes,
with [measured evidence](engine-storage-v1-report.md). Remaining: broader
point-read APIs and N4 durable node integration.

- Freeze versioned canonical key/value encodings and hash domains with test
  vectors before integrating trie roots into the public contract. The record
  key, cell name, value and term encodings above are the frozen inputs.
- Integrate the vendored trie components. Store immutable proof nodes and
  retain values needed to reconstruct historical leaves. Validate the memory
  implementation, then persist the same logical evidence in node step N3;
  physical storage IDs and page layout must not affect roots.
- Commit cells, logical index entries and local engine state per namespace;
  bind their roots and owner descriptors through the namespace directory,
  and combine that with global engine state. Snapshot IDs and
  a block's own hash are not substitutes for authenticated logical content.
- Seal computes the candidate roots (lazily, on request, from cached node
  hashes); publish makes state, indexes and roots visible together. Reads
  and proofs use the selected snapshot's roots.
- Supply point inclusion and absence proofs and verification from a supplied
  trusted root. Label these as point proofs, not complete query proofs.
- Account for proof-generation work and reserve write-side root-maintenance
  work in the cost model. No unbounded hashing or witness construction may run
  after the operation's budget has already been consumed. Model version 3
  (N0b-1) reserves both: `RootUpdate`/`RowByte` are charged by every write
  now, and `ProofNode`/`ProofByte` wait for the proof API.

Exit: deterministic vectors agree for identical complete logical state;
old roots/proofs still verify after later mutations. Changed witness bytes,
wrong values, and substitution of a different root fail verification.

### 4. Complete query proofs and SDK integration

- Specify the proof format and verifier for the milestone-2 query subset.
  Authenticate complete equality postings (including absence), predicate
  combinations, returned cells and the exact ordered page. Providing whole
  relevant posting sets initially is acceptable; bound work and proof size.
- The verifier independently derives the matching IDs, ordering and page
  boundaries from authenticated evidence. It checks omissions, extra rows,
  false terminal pages and any advertised exact count. It must not ask the
  serving database whether its own result is correct.
- Negation needs an authenticated domain; ranges/prefixes need complete
  authenticated coverage. Point proofs for whichever terms the server chose
  to reveal are not enough. Unsupported proof requests fail explicitly.
- Bind continuation to a previously verified page or independently verified
  cursor evidence. An opaque cursor is transport state, not proof of coverage.
- Preserve SDK `atBlock` and add namespace context. Add proof requests and
  snapshot identity/root/evidence; reject a different namespace or block.
  `getEntity`/count historical options must share the same selection rules.
  Wire names, error codes and protocol negotiation need agreement before SDK
  implementation; the 32-byte key encoding is settled (node step N0b-1).
  Namespace transfer and existing entity ownership need an explicit
  authorization compatibility rule. No second time selector is added.
- Carry budgets and receipts through the RPC/SDK surface. Bound complete-proof
  construction and verification inputs; query/proof budget failure cannot
  return a successful partial proof or silently remove the proof request.

Exit: an independent verifier accepts valid historical pages and rejects
tampering with values, matches, order, counts, cursor boundaries and root
identity. Existing SDK historical/expiry/pagination behavior still passes.
Real chain integration additionally authenticates the root for the state
after the selected block under the host's consensus rules.

### 5. Durable storage and archive operation

This milestone is now split by the [node and storage plan](node-storage-plan.md).
Steps N2–N4 deliver a versioned storage format, transactional block/state/head
publication, reopen/recovery and idempotent outcome reconciliation, alongside
milestone 3 and before the rest of milestone 4 after the equality demo.
Step N6 adds retention management,
snapshot transfer and backfill. redb is the first adapter candidate, subject
to measured recovery and namespace-isolation gates; since step N2 it is a
pinned dependency of `arkiv-db-redb` only (see the
[redb experiment](redb-experiment.md)).
A restart that replays the entire database does not satisfy the final
requirement that recovery be bounded by the last commit. Backend allocator
recovery costs must also be stated rather than hidden by an application-level
no-replay claim.

Data, indexes, historical values, roots/proof nodes and host publication marks
must recover consistently. Lost acknowledgement may mean the commit succeeded;
reconcile its identity before retrying. Fault injection must prove all-or-none
publication and survival of acknowledged writes.

Add an explicit retained window and archive coverage. GC respects retained
roots and active handles; keeping roots alone does not preserve queries or
proofs. Import must not advertise history it did not restore. Archive operation
must retain or reconstruct every finalized block promised by the product.

## Decision gates

Before implementation of each affected boundary, document these concrete
choices and test vectors:

- Decided for milestone 2: the canonical record key, cell name, value and
  term encodings with vectors, the index layout (one posting per value term
  and per existence term, ordered by name, type and value bytes), and the
  record-id scope (dense per namespace, never reused). Open: index
  serialization for roots, state-root composition and query/proof versions.
- Decided: namespace ID allocation and encoding, descriptor layout,
  transfer preconditions and the bounded directory (see the namespace
  contract). Open: the authenticated directory evidence.
- Decided for milestones 1 and 2, revised in node step N0b-1: operation
  classes, counting rules (model version 3), fixture weights and the two
  schedule invariants, checked arithmetic, budget charge points, admission
  validation, rollback accounting and the receipt type, as listed above.
- Decided in node step N0b-1: the record-key width (1 to 32 bytes), its
  canonical encoding version 2 and the version-1 compatibility rules.
- Decided in node step N0b-2: the canonical codec version 1 (rules, layouts
  and tag tables of the engine types, strict decoders enforcing type
  invariants but not admission limits, receipts without ledgers, price
  schedules with exactly `OperationClass::ALL.len()` weights), the digest
  `keccak256(u8 len(domain) ‖ domain ‖ bytes)` with `arkiv/<object>/v<n>`
  domains, and the compatibility rules (unknown tags and versions rejected,
  tags never reused); see the [block format](block-format.md). Also the
  host-state map (32-byte keys, values ≤ 256 bytes, part of the rolled-back
  and published state, outside the data plane, advancing the branch
  mutation serial) and the fallible `try_` surface with `StorageError`
  from the storage contract. Settled in N3b (below).
- Decided in node step N0b-3: block format version 1 (genesis, chain
  parameters, header version 1 without a state root, block inputs,
  transaction envelopes, outcomes, their tag tables and the
  `arkiv/<object>/v1` hash domains), the genesis invariants and the expiry
  reservation (deletion reserve, sweep read bound, host expiry read),
  the successor rule, execution order (admin inputs, expiry sweep, user
  transactions), per-transaction rollback, the host-state due schedule and
  expiry capacity, static validation, selection, request identity and
  replay validity; see [part 2 of the block format](block-format.md#part-2-block-format-version-1).
  Implemented in the demo: header version 2 with the state root (protocol 2).
- Decided in node step N3b: the trie key encodings and leaf layouts of
  every engine map, the `StateHeader` (state format 1) and the state-root
  domain `arkiv/state/v1`; storage errors poison a branch.
- Decided for milestone 2: the first supported query subset, its evaluation
  order, the instance limits and cursor binding. Open: the proof subset and
  work/size limits, compatibility with the proposed canonical cost model for
  cached execution, historical pricing context and proof-generation counts.
- The node plan specifies initial host admin/expiry/data sequencing and
  atomic terminal-block publication. Real consensus evidence authenticating
  the state after a block remains a later host integration requirement.
- RPC fields/error codes, verified pagination and cross-node cursor
  behavior.
- The [full/light demo](full-light-demo.md) and
  [engine storage v1](engine-storage-v1.md) are implemented. N3c persists
  engine state and historical proofs; N4 remains responsible for durable
  protocol block identity, node publication, inclusion and signing restart.
  Later: retention/window sizes, archival availability and historical backfill.

Later gates do not block the running memory node. The [test strategy](testing.md)
distinguishes current tests from planned acceptance cases; milestones 3 to 5
and node steps N0–N6 are not completed by these documents.
