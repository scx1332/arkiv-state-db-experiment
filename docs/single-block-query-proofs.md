# Queries and proofs at one block

## Scope and decision

This is the implementation contract for `arkiv-db-pure-astra`. It records
decisions for this repository and does not imply that the upstream
source-of-truth repository has adopted them.

Clarified on 2026-09-23: a query may select any finalized block number, but
must evaluate against exactly one block. Its results and proofs describe the
same completed state. This is historical snapshot querying, not a second
valid-time axis: no `validAt` or backdated validity intervals are required.

This document defines the Arkiv host/RPC contract and the corresponding
requirements on [Golem-DB](https://github.com/Arkiv-Network/arkiv-source-of-truth/blob/main/golem-db.md). The engine targets Arkiv's block
workload and may expose block-oriented execution APIs. It can still use
separate snapshot/commit identities internally; the host establishes finality.
Implementation has begun entirely in memory as described below. The
namespace catalog, immutable snapshots, branches with checkpoint/rollback,
atomic in-process publication, deterministic metering, a host block map,
typed records with per-namespace indexes, the declared query subset with
snapshot-pinned keyset pagination and host-executed expiry are implemented,
and since node step N3b every state has a deterministic logical state root;
complete equality-page proofs, verification and state roots in every block
header now exist. N3c persists engine snapshots and the same
proof material; durable node/block integration and broader proof profiles
remain open. Requirements in this
document are targets; where the prototype meets one, the
[plan](snapshot-query-plan.md) says so.

The additional [node and storage plan](node-storage-plan.md) implements this
contract through a running append-only host and then atomic durable block
publication. The immediate [full/light-node demo](full-light-demo.md) adds
continuous following and a bounded complete equality-proof profile before
durability, with no pruning. Its pinned-producer signatures
authenticate development headers; they do not prove execution validity or
public-chain consensus/finality, which remains a separate integration.

Every ordinary query also selects one immutable project namespace ID, as
defined by the [namespace contract](project-namespaces.md). Names and current
owner addresses are not storage identities. The selection is one namespace
and one block, not a global query filtered by owner afterward.

## Snapshot selection

- `atBlock: B` selects the state after all operations of finalized block B.
  Operations execute in deterministic order; failed execution spans contribute
  no changes. Intermediate states within B are not public block snapshots.
- Resolve B once to an immutable snapshot. Record existence, key bindings,
  cells, indexes, sort values, counts, engine state, and proof witnesses must
  all use that snapshot. Later writes or expiry cannot change its answers.
  Implemented: every read takes a `Snapshot` handle, which owns the complete
  state (rows, key bindings and term index of every namespace) and never
  consults the head; a cursor names that snapshot.
- Existing requests without `atBlock` may select the latest finalized block,
  resolved once on the first request. Return the resolved block explicitly and
  keep it fixed for subsequent pages. There is no live pagination in this
  query-with-proof contract. The engine offers no "latest" resolution of its
  own: the host resolves once and pins the handle.
- Expiry is evaluated at B, not at the current head. An entity expiring at E
  is absent from snapshot E and later snapshots; retained earlier snapshots
  preserve its earlier state. A later expiry extension does not rewrite an
  earlier snapshot's expiry. Implemented mechanism: the engine has no clock;
  the host runs `host::expire_records` in block E's branch, which sweeps the
  expiry attribute (an ordinary `U64` cell under a host-chosen name) height
  by height: `terms` reads the smallest live height, and while it is at
  most E the rows holding it are read as `Eq` pages and deleted through the
  host path until that height is empty, then the next height; so snapshot E
  lacks the record while snapshot E−1 keeps it. Deletes run in (height, id)
  order, the work is linear in the distinct expired heights plus the
  deleted rows, read receipts are discarded, and a halted run resumes at
  the smallest live height in a later block, so the host reserves block
  budget for expiry. An extension is a patch of that cell in an earlier
  block. The engine assigns `$`-names no meaning, so the host must keep
  users from writing the cells it relies on.

The running node has a stricter completion rule than this low-level expiry
helper: it reserves expiry capacity and discards the whole candidate if the
sweep halts. It must not advertise block E as complete with records still
live past their expiry at E. See the node plan's execution sequence.

The host maintains an explicit mapping:

```text
(chain identity, finalized block number, block hash)
    -> (terminal engine snapshot/commit identity, state root)
```

Do not assume block height equals engine commit number. Admin and data
transitions may be published as separate engine snapshots (the tests do so)
or carried in one candidate; the host must define their order and record the
block mapping only for the block's complete terminal snapshot.
Queries must never resolve a block to an intermediate administrative or data
state. Empty blocks also receive a mapping, even if state is unchanged.

The root must be authenticated by the chain's commitment rules. A root or hash
supplied by the serving RPC is not itself a trust anchor. The host adapter must
specify which consensus evidence authenticates the state after B, including
the AppHash timing issue recorded in Golem-DB's open question 10. Do not assume
that the header at B contains the root after B, and do not put a block's own
hash inside the state root on which that hash depends. Extra header evidence
does not introduce a second query snapshot.

## Query and proof contract

Conceptual RPC shape; field names and wire encoding are not yet frozen:

```text
query(namespaceId, expression, {
    atBlock: B, select, order, limit, cursor?, proof: "complete", budget?, debug?
})
-> {
    data,
    snapshot: { chainId, namespaceId, blockNumber: B, blockHash, stateRoot },
    proof,
    costReceipt,
    cursor?
}
```

The implemented engine call behind it is
`Snapshot::query(namespace, &Query, &Projection, limit, cursor, budget)`,
returning `QueryPage { rows, next }` with a read receipt. `expression` is a
`Query`: an ordered flat DNF of typed literals (`Eq`, `NotEq`, `Range`,
`StartsWith`, `Exists`, each optionally negated) over attribute cells;
`select` is the `Projection` (id always; key and all, none or named cells);
`order` is the record id, the only order; `limit` is bounded by the instance
`max_page`; `cursor` is the opaque continuation bound to the snapshot,
namespace, query and projection; `budget` and `debug` are the `Budget` and
its trace flag. Unsupported predicates (`Bytes` terms, unbounded or mixed
ranges, ranges on unordered types, oversized values, limits or projections)
are rejected explicitly at admission. This general query API has no `proof`
option. Separate `Snapshot::prove_eq` and `Snapshot::prove_range` APIs provide
complete equality-page and unpaginated single-attribute numeric-range proofs;
they do not prove the general DNF query language. See
[numeric range proofs](range-proofs.md) for the current range profile.

The verifier takes the expected query, requested block, a trusted chain
commitment, and the response. It must establish:

1. The selected state root belongs to the requested chain and finalized block.
   A directory witness binds the requested namespace ID and descriptor to
   that root; query evidence is checked against that namespace's roots.
2. Returned values, types, identities, and existence claims match that root.
3. Every returned row satisfies the predicate, including the specified
   semantics of missing attributes, type differences, and negation.
4. No matching row within the requested page is omitted or invented. For an
   unpaginated result this covers the entire matching set.
5. Ordering, deterministic ties, continuation boundaries, and the claim that
   no further page exists are correct. Any advertised exact count is also
   established by the evidence.

Membership proofs for returned rows alone do not satisfy this contract.
Indexes, their complete postings or authenticated range coverage, existence
domains, and any values needed for predicate/sort evaluation must be provable
against the same root. A verifier must check that index membership agrees with
the authenticated record data relevant to the answer.

An initial implementation may support a declared subset of predicates and
ordering. If it cannot produce complete evidence for a requested operation,
it must reject the proof request explicitly. It must not silently downgrade
to row proofs, an unproved answer, or an incomplete successful result. Query
and proof work are bounded; exhausting a budget is an error, not evidence of
an empty result. Proof encoding, canonical query encoding, verification rules,
and resource limits must be versioned before interoperability is claimed.

The predicate semantics the engine implements, and which the verifier must
reproduce, are: a term matches only an attribute cell (never a field) of
exactly the term's type; a record lacking the attribute or holding another
type matches neither `Eq` nor `NotEq`; a negated literal is the complement
over all live records of the namespace; a group is the AND of its literals
and the query the OR of its groups; the answer is the matching live records
in ascending record id. `next` is present if and only if a further match
exists, which the engine establishes by positioning every group at its first
match beyond the page; a page never carries a total, and `count` is a
separate metered read.

## Operation costs and budgets

The distinction in Golem-DB continues to apply: **writes are gas-priced by
Arkiv; reads and queries are metered for local RPC limits or API billing**.
A query receipt never enters consensus. The database counts deterministic
abstract operation units, not execution time; a provider decides how to bill
those units. In-memory storage does not remove the metering requirement.

Write/read meters, budget checks, cost receipts and optional debug ledgers
are implemented with the first snapshot milestone, and the second milestone
extended them to record writes, index maintenance, record reads, every query
step, counts and term listings under model version 2 (counting rules in the
[plan](snapshot-query-plan.md#operation-cost-is-part-of-the-first-milestone)).
Model version 3 (node step N0b-1) makes every write prepay root maintenance
and reserves the `ProofNode`/`ProofByte` classes for point proofs; extend the
same mechanism to proof generation when it is introduced. The
meter stops before work exceeds the available budget and restores a failed
write's logical changes while preserving its spent-cost receipt. A query that
runs out of budget returns no partial page. Exhaustion must not look like a
successful empty query or a complete proof.

Query work is lazy and positional for positive `Eq`/`Exists` sources, the
row map (match-all, negation) and the existence posting (`NotEq`): the
charge covers the candidates scanned to fill the page plus each group's
first match beyond it. A `Range`/`StartsWith` source additionally charges a
term seek and a term visit for every qualifying term of its run on every
page, so a walk of P matches over T terms with page limit L charges
ceil(P/L) × T term visits in total, T bounded by the namespace's own
distinct values; match-all, negated and `NotEq` first literals are linear
in the namespace's own live rows or existence posting per page scanned;
nothing depends on other namespaces. Index-term creation is priced above
posting membership, and only projected bytes are charged for returned rows.

Writes use the pricing context fixed at their branch base; reads at a
snapshot currently use that snapshot's schedule. Specify the historical-read
pricing policy through RPC and proof-generation counting rules before exposing
those receipts; neither may change the selected data snapshot. A complete
query proof does not by itself prove a cost receipt.
Canonical counts must be independent of caches and physical storage, with
integer arithmetic, versioned counting rules and reproducible failure points.
Production weights and block budgets still require commit-fit calibration.

## Pagination and caches

A cursor binds namespace ID, chain/snapshot/root, query, selection, ordering,
and continuation position. Its semantic context is validated on continuation;
changing any bound input is an error. All pages use the original snapshot
even when the head advances. The SDK must check response identity against the
requested/pinned snapshot instead of accepting a different returned block.

Implemented: a `Cursor` is the last emitted record id bound to the engine
instance and snapshot id (or, for a branch query, the branch and its mutation
serial), the namespace, the query and the projection; ordering is implied
because record-id order is the only order. Continuing with any bound input
changed is `CursorMismatch` before any charge. The page limit is not bound,
so a walk may change its page size. The chain, block hash and root are host
context that the host adds around the snapshot id once roots exist.

A complete verified walk begins at the first page. Later page boundaries must
be justified by the prior verified continuation or an independently verifiable
cursor; an opaque server token alone is not proof that no rows were skipped.
Cursor correctness cannot depend on a warm cache on one RPC node.

Result/proof cache keys include namespace ID, resolved snapshot and all query
inputs affecting the answer. Cache eviction never deletes authoritative history or
changes answers. A proof is verified against the trusted root even when its
result came from a cache. Cached results do not alter canonical metering.
The engine has no caches today; every page is evaluated from the snapshot's
own maps.

## Retention and availability

The product target is query and proof availability for every finalized block
from genesis through an archival service. This does not require every validator
to retain the entire history. Ordinary nodes keep the guaranteed engine
retention window; the archival surface serves older snapshots without changing
the consensus-path retention rule.

Keeping a root alone is insufficient. Retain the historical namespace
directory, ownership descriptors and referenced namespace roots. For every
advertised snapshot, retain or reconstruct its values, historical index
postings, key/existence domains,
engine state, and reachable proof nodes. Snapshot handles protect their
dependencies from pruning while a request is executing. Between pages, a
pruned snapshot yields explicit unavailability; a cursor is not a storage lease.

Distinguish future/unknown blocks, pruned history, history not yet synchronized,
unsupported proof requests, and invalid cursors from a proven empty answer.
The exact error codes remain an API design task. Never fall back to head.
Nodes advertise actual historical coverage; archival routing must preserve B.
Snapshot import cannot claim coverage for history it did not import or backfill.
Before a node participates in consensus-path historical reads it must possess
the full required window; local incomplete coverage cannot change consensus.

## Implementation stages

Tests started with block packets: one ordered batch of accepted mutations
per block, checked before submission and after application, with many
operations on the same key, empty blocks, discarded candidates and rejected
batches. Those raw KV tests use block numbers only as workload labels. The
engine tests establish retained snapshot identity, historical reads and
queries, cost accounting and transaction-level rollback through real
branches and a host block map; keep the two kinds distinct as the execution
API grows.

The Rust prototype keeps its KV store, snapshots, indexes and (later) proof
nodes and caches **entirely in memory for now**. Published snapshots are
immutable and internally consistent, but process termination loses them.
Call this a volatile reference implementation, not durable commits or a
production archive. Every published snapshot is retained for its process
lifetime; pruning is deferred and there is no snapshot capacity limit yet. A
bounded capacity failure must not partly publish a new snapshot or silently
evict promised history.

The feature stages below retain their original numbers. Execution order is
now the [demo's D0–D3](full-light-demo.md#implementation-order-and-review-gates),
then the remaining [node/storage steps](node-storage-plan.md#ordered-implementation-steps-and-exit-criteria):
the bounded complete-Eq profile moves ahead of durability, broader proofs
and SDK support follow it, and pruning/archive distinctions remain later.

0. Done: namespace IDs, ownership/transfer transitions and local addressing
   are specified and implemented; the directory-work bound is defined and
   measured for every implemented operation.
1. Done in memory: metered writes/basic reads with budgets, receipts and
   debug ledgers, the namespace catalog, immutable snapshots, branches and
   atomic in-process publication with a host test harness mapping blocks to
   snapshots. Pricing is an uncalibrated fixture.
2. Done in memory: namespace-local records/cells and historical indexes,
   deterministic queries in record-id order with snapshot-pinned keyset
   pagination, host-executed expiry, and model-version-2 accounting of index
   and query work. Not done: attribute sorting, descending order, offset
   paging and engine-assigned keys. The SDK 32-byte key encoding was settled
   later, in node step N0b-1 (`RecordKey` encoding version 2).
3. Namespace/directory roots and point inclusion/absence proofs using the
   vendored trie over the frozen encodings, with bounded proof generation and
   prepaid write-side root maintenance (charged since model version 3). The
   roots are done in memory (node step N3b: the state root composes every
   namespace's map roots through the directory); point proofs are node step
   N3d, and point proofs alone are not complete query proofs.
4. Complete query proofs for the declared query subset, an independent
   verifier, and SDK/RPC integration preserving `atBlock`.
5. Persistent atomic publication, recovery, retained-history management,
   snapshot transfer/backfill, and archive operation.

The final stage retains Golem-DB's durability and bounded recovery requirements.
Before production use, records, indexes, roots, and block mapping must recover
as one complete published block. A lost acknowledgement requires reconciliation
of commit identity; it is not proof that a durable commit did not happen.

## Acceptance requirements

Each item is a target; those the in-memory prototype meets are marked.

- Namespace transfer preserves IDs and content without reindexing; earlier
  blocks preserve the previous owner. Mixed namespace updates publish together.
  Met in memory.
- Growing other projects leaves local query work and charges unchanged, with
  directory visits and proof size inside the specified bound. Check namespace
  binding in cursors, caches and verification; no global scans are hidden by
  a constant cost charge. See the namespace contract's adversarial tests.
  Met in memory for every operator and engine cursor, and for the demo's
  Eq proofs and bounded verified-continuation cache.
- Query before creation, after creation, after patch, after deletion, and on
  either side of expiry. Repeat old queries after later mutations; results
  and roots are unchanged. Met in memory for results and, since node step
  N3b, for state roots (`roots.rs`).
- Identical state, operations and pricing yield identical costs and abort
  points with cold/warm caches. Exact-budget execution succeeds; insufficient
  budget rolls back the call while retaining its spent receipt. Estimation
  matches execution on the same starting state and pricing context. Met in
  memory (there are no caches).
- Debug mode preserves costs; checked counters cannot wrap. Index-term
  creation, projection and proof work have explicit counting vectors. Read
  receipts remain outside consensus and cannot be confused with gas charges.
  Met in memory for index-term creation and projection; Eq proof work has
  [model-4 charge vectors](eq-proof-charges.md).
- Historical predicates still find records removed from current indexes.
  Key reuse never joins histories of distinct record identities. Met in
  memory.
- A failed span restores its changes; losing speculative candidates cannot
  alter any published snapshot. All successful operations in a block appear
  together at its publication boundary. Met in memory.
- Mixed admin/data transitions and empty blocks resolve to the intended
  complete state, without assuming block number equals commit number. Met in
  memory.
- Pagination stays unchanged while newer blocks arrive. Altering snapshot,
  query, selection, ordering, or continuation evidence is rejected. Met in
  memory for snapshot, namespace, query and selection (ordering is fixed);
  verified continuation evidence is implemented for the Eq proof profile.
- Verification rejects altered values, missing/extra matches, wrong ordering,
  false end-of-results claims, and proofs substituted from another state.
  Implemented for the bounded positive Eq profile. Broader operators,
  including negation, remain later proof work.
- Proof verification uses an independently trusted root. Corrupted witness
  nodes or an unauthenticated replacement root cannot make a result valid.
  Planned.
- Availability errors never appear as empty query results. Import and pruning
  preserve all advertised answers and proof material. Met for storage
  errors beneath engine state over a test object reader (node step N3b,
  `storage_faults.rs`); import, pruning and durable storage are planned.
- In the durable stage, fault injection exposes a complete old or new state;
  acknowledged publications survive restart and uncertain outcomes reconcile
  without duplicate commits. Planned.
