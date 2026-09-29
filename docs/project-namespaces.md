# Project namespaces and query-cost isolation

## Decision and scope

This document specifies the namespace design for `arkiv-db-pure-astra`.
The upstream source of truth remains a reference; these local decisions do
not require a change to that repository.

Golem-DB has first-class project namespaces. A user can create a namespace,
own it, and transfer ownership while keeping the same namespace identity and
data. This is the isolation boundary for the Arkiv workload, chosen on
2026-09-23. It was required before record/index layout was implemented.

Each namespace has an immutable `NamespaceId`, independent of its owner and
human-readable project name. IDs are never reassigned to a different project.
Allocation is deterministic under replay; the encoding and allocation rule
below are frozen and implemented.

Human-readable name registration, uniqueness, squatting prevention, disputes,
and name-to-ID resolution belong to Arkiv's naming layer. Golem-DB receives
the resolved ID. It stores a validated name as descriptor metadata, with no
name-to-ID index or uniqueness policy. Names are immutable in the current
API; external naming aliases do not migrate records or rebuild indexes.

Identity, ownership, per-namespace record maps, typed records with their
term indexes, and the bounded directory are implemented in the in-memory
engine (`arkiv_db::Engine`). Namespaces address the cost-isolation gap from
[PR #2](https://github.com/Arkiv-Network/arkiv-source-of-truth/pull/2); the
implemented bound covers put, delete, transfer, point reads, prefix scans,
descriptor reads, typed record creation, patching and deletion, record reads,
every query operator (equality, inequality, ranges, prefixes, existence,
negation, match-all, AND-groups and OR of groups), pagination and counts.
Roots, proofs and the authenticated directory remain design targets.

## Identity, ownership and authorization

Implemented decisions:

- `NamespaceId` is an ordinal `u64` starting at 1. The *n*-th namespace
  creation published on a chain receives ordinal *n*. Zero is not a valid ID.
  The canonical encoding is the ordinal as 8 big-endian bytes, so byte order
  equals allocation order wherever IDs are used as key prefixes; `Display` is
  `ns:<16 hex digits>`.
- Ordinals are allocated from committed engine state (`next_namespace` in
  the working state). An allocation is provisional until its candidate is
  published: the allocator is part of the state a checkpoint rollback
  restores, and a discarded candidate releases its ordinals, so the same
  ordinal may be handed out again inside that branch or by the next
  candidate. A published ordinal is never reassigned. Record ids inside a
  namespace follow the same rule with their own per-namespace allocator.
- `NamespaceName` is 1–20 ASCII characters using the `CellName` grammar.
  `WriteOp::CreateNamespace { name }` requires it. The validated name is
  stored in every snapshot, survives ownership transfer unchanged, and does
  not replace the ordinal ID in addresses. Duplicate labels are allowed by
  the engine; registration and uniqueness remain host policy.
- A descriptor is `{ id: NamespaceId, owner: OwnerId, revision: Revision }`.
  `OwnerId` is 20 opaque bytes, a host-authenticated principal such as an
  account address; the engine never interprets or verifies them. `Revision`
  counts successful transfers since creation and starts at `Revision::INITIAL`
  (0). Descriptors live in `NamespaceState` next to the record map and the
  typed store, outside every keyspace; every retained snapshot keeps the
  descriptor current at that snapshot.
- `WriteOp::CreateNamespace { name }` is permissionless: any actor may create, the
  actor becomes the owner, revision is 0. It is gated only by its charge and
  the directory capacity (`Rejection::DirectoryFull` when the catalog has
  reached the engine's cap; `CounterExhausted` if the ordinal counter cannot
  advance).
- `WriteOp::TransferNamespace { namespace, expected_revision, new_owner }`
  checks, in this order and after the directory charge: the namespace exists
  (`UnknownNamespace`), the actor is the current owner (`Unauthorized`), and
  `expected_revision` equals the current revision (`StaleRevision { expected,
  current }`). Each rejection has charged only the directory lookup. On
  success the transfer charge is taken, `owner := new_owner` (which may equal
  the current owner) and `revision += 1`, atomically with the descriptor.
- A transfer shares the record map and the typed store as they are: the new
  `NamespaceState` (the directory leaf) holds the same trie roots for the
  byte map, the key bindings, the rows and the term map, so no record, row
  or posting is visited, copied or rewritten and the physical trace reports
  zero record and index nodes; only the directory leaf is rewritten.
  `Snapshot::records_shared_with` (byte map) and
  `Snapshot::typed_shared_with` (key, row and term maps) expose that root
  identity for tests. Records, keys, ids, indexes and history are preserved.
- The engine checks authority against the branch state at the operation's
  execution position, not at the block's start. A write by the old owner
  after a successful transfer in the same block is rejected with
  `Unauthorized`; a rollback restores the preceding owner and revision
  together with the records, rows and index.
- The host validates signatures and any recipient-acceptance policy and passes
  the authenticated actor separately from the request. Namespace ownership
  does not grant access to the engine's pricing admin transition
  (`Branch::set_pricing`) or to the host path (`Branch::execute_host`, which
  accepts typed record operations only and skips the owner check for
  chain-driven transitions such as expiry). Ownership is not
  confidentiality: this design does not introduce private reads of otherwise
  public blockchain data.
- Only the owner may write. Delegated/shared writers require an explicit later
  permission design, not an unrestricted namespace field.
- Namespace ownership is distinct from any entity-level `$owner` attribute;
  the engine assigns `$`-names no meaning. Transferring a project does not
  rewrite any record's cells. Compatibility between project authority and
  existing entity authorization must be specified before exposing namespace
  writes through the SDK.

Descriptors change only through the dedicated namespace operations; neither
a `Put`/`Delete` nor a `CreateRecord`/`PatchRecord`/`DeleteRecord`, on the
owner path or the host path, can reach them. Namespace ownership transfers
are application state transitions, not global pricing/admin changes.

The implemented isolation tests establish that, for every operation that
exists (put, delete, get, prefix scan, descriptor read, record create, patch
and delete, record read, each query operator, pagination and count),
namespace A's charged units, budget-abort points, record-node visits and
index-node visits are unchanged while namespace B's records, its identical
attribute terms and the number of namespaces grow, and that A's directory
visits stay inside the bound below. They do not establish wall-clock
isolation on shared hardware, any bound for proofs (none exist yet), an
authenticated directory, or behaviour on a durable backend.

## Query scope and storage boundary

Every record operation takes a namespace ID. The logical record address is
`(NamespaceId, record key)`. Key uniqueness and internal record-ID allocation
are local to the namespace; identical record keys in separate projects cannot
alias. Namespace-local ordinals avoid coupling posting representations and
allocation work to records created by other projects: a record id is the
dense creation ordinal within its namespace, and a posting is an ordered set
of those ids.

Every ordinary query selects **one namespace and one snapshot**. The
implemented request is

```text
Snapshot::query(namespace, &query, &projection, limit, cursor, budget) -> QueryPage
Snapshot::count(namespace, &query, budget) -> u64
Snapshot::terms(namespace, &name, tag, lower, limit, budget) -> Vec<Value>
```

which the conceptual RPC of the
[single-block contract](single-block-query-proofs.md) wraps after resolving
`atBlock` to the snapshot once. The namespace is a mandatory execution
parameter, not a predicate applied after global filtering. Query
parsing/order semantics are deterministic inside the namespace: an ordered
flat DNF evaluated as submitted, results in record-id order. An omitted
namespace must not mean all projects; SDK compatibility may use an explicitly
configured default resolved before the request. Cross-project search requires
a separate, explicitly budgeted API and is outside the initial contract.
Namespace ownership alone must not make a query implicitly scan every
namespace an account owns.

All query-relevant state is partitioned by namespace:

- Records/cells, key bindings and the local record-ID allocator
  (`TypedState::{rows, keys, next_record}`).
- Attribute terms and their posting lists, existence postings and the
  all-live-records domain (`TypedState::terms` and the row map, which
  match-all and negated literals enumerate). Counts are computed from these.
- Historical versions of those structures: every published snapshot keeps
  its own version of each map through structural sharing.
- Cursor context: a cursor binds the namespace ID together with the
  snapshot (or branch state), query and projection. There are no query or
  result caches and no namespace-local statistics beyond the unmetered
  counts.

Proof material and sort inputs do not exist yet; when they arrive they must
be partitioned the same way.

The implemented layout keeps separate persistent Merkle Patricia maps per
namespace behind a directory keyed by ID, rather than prefixes in one shared
keyspace (key and leaf encodings in `docs/architecture.md`):

```text
catalog: NamespaceId -> NamespaceState
NamespaceState = { descriptor { id, name, owner, revision },
                   records: map<key, value>,                       // raw byte keyspace
                   typed: { next_record,
                            keys:  map<RecordKey, RecordId>,
                            rows:  map<RecordId, record>,
                            terms: map<(name, type, value?), posting> } }
```

The illustrative prefix layout from the design (`namespace/<id>`,
`data/<id>/...`, `index/<id>/...`) is not used and is not a frozen byte
encoding. A prefix in a shared keyspace would not by itself prove bounded
work: global scans, shared term enumeration or growing global lookup paths
can still violate it. `NOT`, ranges, prefixes and match-all stay within the
selected namespace: a negated literal's domain is that namespace's row map,
and a range or prefix is a contiguous run of that namespace's term map.
Sorting is not implemented; when it arrives it must use namespace-local sort
inputs.

## Cost-isolation requirement

For a fixed query, namespace state and pricing context, writes confined to
other namespaces must not increase namespace-local logical work or charges.
The total work must have a documented bound of the form:

```text
query_work <= local_work(query, namespace_state) + directory_bound
```

`directory_bound` is an explicit protocol/structure bound, not an arbitrary
allowance proportional to other projects' records, terms or namespace count.

Implemented bound: every operation resolves its namespace through the
directory exactly once, charged as one `DirectoryLookup` unit. Since node
step N3b the directory is a Merkle Patricia trie keyed by 8-byte big-endian
IDs (16 nibbles), so whatever the number of namespaces a read visits at most
`2·8 + 1` = 17 directory nodes (each branch consumes a nibble, each
extension at least one and precedes a branch, one leaf ends the path) and a
write at most 34 (the lookup path, then the insert path of the updated
directory leaf). Dense ids keep paths far shorter: a lookup visits 3 nodes
with ids 1 to 3 and 5 with ids 1 to 503 (`isolation.rs`,
`query_isolation.rs` derive these from the trie shape and check them
exactly). The engine admits at most `DEFAULT_NAMESPACE_CAP = 2^32`
namespaces (configurable through `Engine::with_namespace_cap` or
`Limits::namespace_cap`); the path bound does not depend on it. The
constant directory charge reserves that worst case, so unrelated directory
growth cannot make an unchanged operation exceed its budget. Directory visits
are reported in `PhysicalTrace::directory_nodes` for measurement only; they
are not canonical and never affect charges. (Before N3b the directory was
an AVL tree with a height bound of 46 visits per read and 92 per write at
the cap.)

`local_work` is the charged work over the selected namespace's own maps:
key and row lookups (`PointRead`), index maintenance (`IndexInsert`,
`IndexTermCreate`, `IndexRemove`), term seeks and visits, posting entries,
row visits, probes and emitted rows. Every one of those steps is bounded by
the namespace's own state and the instance limits, and the uncharged steps
between them (a term lookup before its insert, a row load after a key lookup,
a patch's cell algebra, a run's heap merge, the union over groups, admission
validation) are each covered by an adjacent charge and bounded by
`max_cells`, `max_value_len`, `max_groups` and `max_literals`. Because the
term map is per namespace, an identical attribute term in another namespace
has its own posting: it changes neither the charges nor the `record_nodes`
and `index_nodes` of an unchanged operation, which is what the extended
interference test measures.

The directory is now authenticated by the state root and has a fixed key
width and a bounded depth, but no directory witness is produced or verified
yet: the proof-size bound (the planned constant `ProofNode 17` and
`ProofByte D_MAX` directory witness, node step N3d) must be derived and
measured before claiming isolation for proofs.

Do not hide a global scan behind a constant charge. Actual logical visits,
bytes processed and witness construction must meet the bound as well as the
receipt. Shared physical CPU, cache, storage and network contention still need
resource scheduling/capacity controls; this is not an identical-latency promise.

The guarantee is between namespaces. Writers authorized inside a project can
grow its query workload: a match-all, negated or `NotEq`-led query is
linear in the namespace's own live records or existence posting per page
scanned, and a range or prefix charges every qualifying term of its own run
on every page (T terms over ceil(P/L) pages), with T bounded by the
namespace's own distinct values.
Splitting one project's hostile writers into separate namespaces, or adding
finer admission controls, is a separate requirement. Outsiders cannot inflate
a namespace merely by putting its ID in a write: a write to a namespace the
actor does not own is rejected after the directory charge and changes
nothing, and the host path is not reachable from user requests.

## Snapshots, roots and proofs

One block may touch several namespaces. The engine publishes all of a
candidate's accepted namespace mutations, index updates and ownership changes
together as one snapshot; projects never advance independently. Internal
snapshot IDs are distinct from block heights as described in the single-block
contract, and the host's `BlockMap` records only a block's terminal snapshot.

Roots are implemented (node step N3b); proofs are not. The composition, with
leaves in the canonical codec (see `docs/architecture.md` for the layouts):

```text
state_root = digest("arkiv/state/v1", encode(StateHeader))
StateHeader { format 1, catalog_root, catalog_len, next_namespace (directory
              allocation state), pricing, host_root, host_len }
catalog (directory) trie: NamespaceId -> descriptor (id, name, owner, revision),
    next_record (local allocator), and root ‖ len of the namespace's
    raw records, key bindings, rows and terms (term leaves: posting root ‖ len)
```

The record key, cell name, value and term encodings are frozen and are the
inputs of the namespace's map roots; every field is bound through the
codec's unambiguous encodings under one domain-separated digest. A catalog
leaf is checked against its key when read (a leaf describing another
namespace is corrupt). `roots.rs` checks every map root and the state root
against an independent construction; header v2 (the root in the block
hash) is a later node step. A query proof authenticates
the namespace entry under the block root, then the complete query answer
under that entry's data/index roots. It need not enumerate other namespaces.
The verifier checks the namespace ID as well as chain, block, query, ordering
and continuation context. Root membership alone still does not prove query
completeness.

Namespace ownership transfer changes the descriptor and therefore the state
root; the namespace's content roots stay unchanged when there are no content
mutations, because the record map, rows and index are shared unchanged and
only the catalog leaf is rewritten. Earlier snapshots preserve the old owner
and the old roots (`roots.rs`). Cursors
bind the namespace ID plus the selected snapshot, not the current owner or
name (implemented); caches, when they exist, must do the same. Retention must
preserve the historical namespace directory as well as its referenced content
and proof nodes.

## Implementation order and acceptance gates

1. Done: ID allocation, namespace-local record addressing and owner
   transitions are defined and implemented, together with the in-memory
   catalog, deterministic metering and the first block/snapshot API. The
   low-level KV facade stays byte-oriented.
2. Done: records, record ids and indexes are namespace-local from their first
   implementation, and the cross-namespace interference test is extended to
   every query operator (`query_isolation.rs`).
3. Commit directory descriptors and per-namespace roots: done (node step
   N3b, state root). Measure both local witnesses and the bounded directory
   witness in historical point and query proofs: planned (N3d, milestone 4).
4. Extend SDK requests, cursors, errors and ownership operations with explicit
   namespace context. Resolve the entity-owner compatibility rule before use;
   the 32-byte key encoding is settled (node step N0b-1, `RecordKey`
   encoding version 2).
5. Preserve the same boundaries in durable storage, pruning and archive import.

Required tests, with the current status of each:

- Identical keys in two namespaces never mix reads, writes or scans:
  implemented for records. Identical attribute terms never mix postings,
  counts, negation domains, cursors or term listings: implemented
  (`pagination.rs`, `query_isolation.rs`; `terms.rs` for term listings).
  Caches and proofs: planned with milestone 4 (there are no caches).
- Unauthorized writes/transfers fail; stale transfer revisions fail. A transfer
  followed by an old-owner write in one block uses the updated authority.
  Rollback and rejected candidates restore both owner and content state,
  including rows and index: implemented.
- Transfer an empty and a populated namespace: charges are independent of
  record count and the record map, rows and index are shared (same roots)
  afterwards: implemented. Content roots staying stable while the state
  root changes: implemented (`roots.rs`). Old-block proofs of the former
  owner: planned (N3d).
- Freeze project A's data/query/pricing, grow B's records, writes, identical
  terms and additional namespaces, then repeat A's operations. Measure local
  visits, charged units and abort points separately from bounded directory
  work: implemented for put/delete/get/scan/descriptor
  (`isolation.rs`) and for record writes, record reads, equality, range,
  prefix, existence, inequality, NOT, match-all, paging and count
  (`query_isolation.rs`). Proof sizes, sorting, warm and cold caches: planned
  as each is implemented.
- Tampered namespace IDs, descriptors or cross-namespace witnesses fail proof
  verification: planned (N3d). Missing or corrupt namespace data produces a
  storage error (unavailability), not an empty answer or a switch to another
  namespace, and a directory leaf describing another namespace is corrupt:
  implemented over a test object reader (`storage_faults.rs`); durable
  storage follows in N3c.

These tests are gates on the design. A namespace label, owner filter or a
passing data-correctness test alone does not establish cost isolation.
