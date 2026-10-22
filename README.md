# Arkiv DB — pure Rust

A database built incrementally in Rust. The storage contracts and the memory
backends use only the standard library; the engine crate uses
`alloy-primitives` for `keccak256` and keeps its state in the `arkiv-db-trie`
Merkle Patricia map (vendored `alloy-trie`, `alloy-rlp`), the node library
(`arkiv-node`) adds Ed25519 development authentication, a signed
single-proposer chain for the Arkiv v2 SDK entity model using Alloy/k256, and bounded HTTP
process orchestration (axum/tokio/reqwest), and the redb adapter
(`arkiv-db-redb`) depends on redb, which uses `libc`.
The trie layer vendors Reth's generic components and the Alloy Patricia
trie core that Reth uses, together with their Cargo dependencies. See
[source provenance and licenses](vendor/README.md).

The explicit `arkiv-proposer` binary accepts signed native transactions,
independently replays them on full followers and authenticates proof roots on
light followers. It retains transaction bodies in a separate `history.redb`;
`node.redb` keeps engine state, headers, sanitized metadata and body references.
The unsigned simulator is a separate binary with its own stores. See the
[signed profile guide](docs/signed-proposer-v1.md) for local commands, limits,
storage recovery and validation evidence. There is no consensus or finality
implementation. Full nodes keep every state snapshot by default;
`--state-retention <N>` serves state only for the head and the `N` snapshots
before it and refuses older reads with `HistoryPruned`. Nothing is deleted
yet; see the [pruning design](docs/pruning-design.md). Entity queries and
counts, proofs and tree exploration run against a pinned snapshot outside the
node lock, bounded by `--read-concurrency` and `--read-deadline-secs`. Entity
queries and counts are planned: each AND-group is driven by its smallest
index posting whatever the clause order, and counts use stored posting sizes;
`--read-max-groups` and `--read-max-literals` raise their genesis query bounds
for these reads only (see the [query plan](docs/snapshot-query-plan.md)).

The target is Arkiv's blockchain workload: many ordered operations per block,
one completed block state, and historical queries with proofs. The engine
executes ordered, metered blocks on immutable in-memory snapshots, keeps
typed records with attribute indexes inside each namespace, answers a
declared query subset with keyset pagination pinned to one snapshot, and a
host harness maps block numbers to the terminal snapshot of each block.

The original KV milestone was a small **in-memory KV database** with
functional tests: get, put (insert/replace), delete, ordered write batches and
bytewise-sorted prefix scans over arbitrary byte keys and values, including
empty values. That raw facade (`Database`) remains the storage foundation and
the boundary a later durable backend implements. It has no namespaces,
snapshots or metering, and shares no state with the engine.

Milestones 0, 1 and 2 of the [implementation plan](docs/snapshot-query-plan.md)
are now implemented in memory as the **block engine** (`Engine`):

- **Project namespaces.** Ordinal `NamespaceId`s allocated from committed
  state, a descriptor `{id, name, owner, revision}` kept outside the record
  keyspace, readable names of 1–20 ASCII characters, permissionless creation,
  and ownership transfer that changes
  metadata only and shares the record map, rows and indexes unchanged. Every
  read and write names one namespace; identical keys and identical index
  terms in different namespaces never alias.
- **Immutable snapshots.** Every publication appends a retained snapshot.
  A branch executes writes in order on top of the head, takes checkpoints for
  transaction rollback, seals, and publishes atomically; a candidate whose
  base is no longer head is rejected. Reads at an older snapshot keep seeing
  keys and records deleted later and the owner recorded at that snapshot.
- **Typed records and indexed queries.** A record is a caller-assigned key
  of 1 to 32 bytes (the SDK's entity keys are 32-byte hashes; the canonical
  key encoding is version 2, 33 bytes `len ‖ bytes ‖ zero padding`) plus up
  to 64 named cells; each cell is an attribute
  (indexed, filterable) or a field (stored and returned only) holding one
  of 25 scalar types, golem-db's full cell type grid (`Bool`, `Str`,
  `Bytes`, fixed bytes, `u32`–`u256`, `i32`–`i256`, `dec32`–`dec256`,
  `f32`/`f64`, `date32`, `timestamp64`, plus `Key`), under frozen canonical
  encodings (`docs/block-format.md`, "Value types"). Cell names are at most 20 ASCII characters; indexed values are
  at most 64 bytes, while field/payload values retain their configurable
  limit (4096 bytes by default). Local raw engine keys and scan prefixes are
  capped at 20 bytes and rejected before lookup when oversized.
  Records get dense per-namespace ids that are never reused. Each
  write maintains the namespace's term index in the same operation; key
  bindings, rows and term postings are sibling fields of the raw byte map,
  never entries in it. Queries are an ordered flat DNF over typed equality,
  inequality, integer ranges, string prefixes, existence and negation,
  answered lazily in record-id order with projection and keyset pagination;
  `count` and a positional `terms` read (the distinct indexed values of an
  attribute in value order) are separate reads. A cursor is bound to one
  snapshot of one engine (or one branch state) plus the namespace, query and
  projection, and never re-resolves head. Expiry is a host-executed deletion
  in the block where a record expires: `expire_records` sweeps the expired
  heights in ascending order, reading each smallest live height with `terms`
  and deleting that height's rows through the host path, so a historical
  snapshot keeps historical liveness.
- **Deterministic metering.** Every data-plane operation, including record
  writes, index maintenance and each step of a query, is charged in abstract
  units (counting-rule version 3) with checked integer arithmetic before the
  work it covers, and issues a canonical receipt whether or not it succeeded.
  Version 3 prepays authenticated-state maintenance: every write charges
  `RootUpdate` per logical map entry it writes and typed record writes
  charge `RowByte` for the row they leave behind; since node step N3b the
  engine computes the roots, and the nodes a block rehashes are measured
  against those entry writes. Write receipts sum to a block total; read
  receipts never enter the block.
  Positive `Eq`/`Exists` sources, the row map (match-all, negation) and the
  existence posting (`NotEq`) are positional: a page charges the candidates
  scanned to fill it plus each group's first match beyond it. A
  `Range`/`StartsWith` source additionally charges one term seek and one
  term visit for every qualifying term of its run on every page, so a walk
  of P matches over T terms with page limit L charges ceil(P/L) × T term
  visits in total, with T bounded by the namespace's own distinct values;
  match-all, negated and `NotEq` first literals are linear in the
  namespace's own live rows or existence posting per page scanned. Nothing
  depends on other namespaces. Prices come from a fixture schedule that is
  not calibrated. The metered reads are `get`, `scan_prefix`, `descriptor`,
  `get_record`, `query`, `count` and `terms`. The only unmetered calls are
  count and map-identity statistics for hosts and tests (namespace, record,
  typed record and term counts, and whether two snapshots have the same
  roots for a namespace's maps) and a hidden index-consistency test aid; they return no
  keys or values. The host-state calls below are unmetered too, but they
  hold only host-owned entries and are never data-plane reads.
- **Canonical codec, host state and a fallible API (node step N0b-2).**
  `arkiv_db::codec` is the versioned binary encoding (codec version 1) of
  the engine's ids, names, values, cells, record keys, write operations,
  effects, rejections, receipts, price schedules and limits, with strict
  decoders, golden vectors and the domain-separated
  `digest = keccak256(len ‖ domain ‖ bytes)`; see the
  [block format](docs/block-format.md). Every engine state holds a small
  host-owned map (32-byte keys, values of at most 256 bytes) that
  checkpoints roll back and snapshots retain, reachable only through
  unmetered host calls and never through the data plane. Every
  state-walking `Branch`/`Snapshot` method has a `try_` form returning
  `Result<_, StorageError>`.
- **State on Merkle Patricia maps and a state root (node step N3b).**
  Every engine map is a `MerkleMap` of the `arkiv-db-trie` crate (node
  step N3a) with frozen key and leaf encodings, and every snapshot and
  sealed candidate has a logical state root,
  `digest("arkiv/state/v1", encode(StateHeader))`, committing the namespace
  directory (descriptors, allocators, every namespace's map roots), pricing
  and the host map; `roots.rs` checks every root against an independent
  construction. Map nodes can be read lazily through a loader: over stored
  nodes (including the durable N3c loader) the `try_` forms fail on storage
  errors, which never become rejections or receipts and poison a branch.
  Receipts are unchanged; physical traces follow the trie, and the
  directory costs at most 17 node visits per lookup.
- **Block format and deterministic block execution (node step N0b-3).**
  The `arkiv-node` library defines block format version 1 — genesis,
  headers, block inputs (admin inputs and transaction envelopes with
  request ids, budgets and operations with SDK-style expiry), outcomes
  (admin outcomes, expiry deletions, per-operation outcomes with receipts,
  rollback and not-executed markers) — with golden vectors and
  domain-separated hashes. One function, `execute_block`, produces and
  replays a block on one engine branch: admin inputs (a pricing change
  takes effect in the next block and must keep the expiry reservation),
  then the expiry sweep driven by a due schedule in host state (with a
  per-height capacity), then user transactions, each rolled back as a
  whole on its first failure with its receipts kept. Static validation,
  FIFO block selection, request identities scoped by chain and actor
  (pending versus included, conflicts on reuse) and `replay` of recorded
  blocks from genesis complete it; see the
  [block format](docs/block-format.md#part-2-block-format-version-1).
  Every header, the genesis header included, commits to its terminal state
  root; the continuous demo retains signed bodies.
- **Continuous full/light demo.** `arkiv-demo` runs a producer, independently
  executing full followers and header-only light followers as separate
  processes. Complete bounded `eq-page-v1` proofs authenticate all matching
  posting ids, then exact full-row pages against a pinned historical root.
  The light verifier owns continuation and never loads the database. See
  [run instructions](docs/full-light-demo-run.md), the
  [trust contract](docs/full-light-demo.md) and
  [proof charge rules](docs/eq-proof-charges.md). Dedicated signed endpoints also
  expose [`range-complete-v1`](docs/range-proofs.md): one typed attribute of any indexable type,
  inclusive/exclusive bounds, index-only selection and a complete result with no
  pagination (initial caps: 64 terms and 64 rows; oversized requests fail closed).
  `POST /signed/v1/query/range/verified` on a light node accepts, for example,
  `{"height":"42","namespace":"1","attribute":"score","valueType":"u64","lower":{"value":"10","inclusive":true},"upper":{"value":"20","inclusive":false}}`.
- **Durable engine storage (N3c).** `durable::DurableEngine` persists the
  actual engine through the versioned store/redb adapter: atomic genesis,
  manifests and head, incremental trie objects, lazy historical snapshots,
  bounded decoded-node caching and preparation, and conservative recovery
  of uncertain commits. Committed handles retain stored roots and no history
  vector. Existing Eq proof bytes and receipts survive reopen unchanged.
  See [usage and lifecycle](docs/engine-storage-run.md) and the
  [measured acceptance report](docs/engine-storage-v1-report.md).
- **Unsigned durable simulator.** `arkiv-simulator` adds one-file atomic
  state/body/inclusion/metadata publication, same-run restart without replay,
  an internal deterministic producer, full replay followers and a durable
  header-only light catalog. Historical Eq verification trusts explicitly pinned
  unsigned roots. Its metadata feed drives an independent PostgreSQL explorer
  in the indexer `experimental` checkout. See [node usage and wire contract](docs/simulator-node.md)
  and [the engine composition profile](docs/simulator-storage-profile.md).

The durable `arkiv-proposer` profile now supports signed accounts, fixed 1 gwei
fees, restart recovery, TOML genesis prefills, and an
[HTTP/WebSocket SDK compatibility adapter](docs/sdk-compatibility.md). Production
consensus and broader complete query proofs remain open. The ordinary `Engine`
and `arkiv-demo` remain volatile; the separate simulator and signed proposer are durable. The proof core composes the trie point proofs
needed for complete equality pages and bounded complete ranges; the rest
of N3d and N5 remain open.
Within the query subset there is no attribute sorting, no descending order
and no offset paging (record-id order and keyset cursors only); keys are
caller-assigned in the raw engine; the signed Arkiv host derives SDK entity
keys from the creator and creation nonce. The SDK adapter includes independent
Ethereum signing fixtures and unchanged-SDK end-to-end tests. Two further departures
from the upstream reference: a page carries no free total, because a total
would need the full walk the lazy rules avoid, and `count` is the metered
replacement; and negation evaluates as the complement over the namespace's
live rows, this repository's resolution of upstream open question 6. Memory
`Engine::publish` still means in-process visibility; durable publication is
an explicit separate facade with three commit outcomes. This is not yet an
implementation of the complete
[Golem-DB contract](https://github.com/Arkiv-Network/arkiv-source-of-truth/blob/main/golem-db.md).

Run the demo with `python3 scripts/full-light-demo.py` (builds the binary
when needed). It visibly distinguishes `execution checked`,
`producer authenticated` and `query proof verified`; signatures do not
prove execution or public consensus. All history is **retained in memory**,
with **no pruning**. Production stops at configured capacity. Every producer
restart requires a new genesis and new local trust configuration; follower
restart rebuilds from genesis.

The [node and storage plan](docs/node-storage-plan.md) now has a bounded N4
slice in `arkiv-simulator`: node state, indexes, receipts, bodies, inclusion
identities, metadata and head commit atomically. Both engine and node reopen
without replay. This separate host profile preserves the engine-only storage
profile. Durable signing and external Ethereum transaction admission are
implemented in the separate `arkiv-proposer` binary. The volatile signed demo
restart rules above do not apply to either durable host.
The rest of point proofs (N3d), broader complete query proofs
retain their existing acceptance gates. See the
[implementation plan](docs/snapshot-query-plan.md), the
[single-block contract](docs/single-block-query-proofs.md) and the
[namespace contract](docs/project-namespaces.md). No second time selector or
native valid-time intervals are required. Only writes are gas-priced by
Arkiv; read/query metering supports local RPC limits or API billing.
Human-readable name registration and resolution remain host policy.

## Workspace

| Crate | Responsibility |
| --- | --- |
| `arkiv-db-storage` | Backend contract, byte entries, and ordered write batches; the versioned storage contract (`versioned`: immutable objects, insert-only records and a durable head with explicit commit outcomes); `FaultyStore` and the contract test suite (`testing`) |
| `arkiv-db-memory` | Deterministic in-memory backend (`MemoryStore` over `BTreeMap`), the structurally shared AVL `PersistentMap` (a reference structure; the engine used it until node step N3b), and the volatile `MemoryVersionedStore` |
| `arkiv-db-redb` | `RedbStore`: the versioned contract over one redb 4.3.0 file; `CountingBackend` I/O instrumentation; `testing::CrashDisk` power-loss, fault and disk-full simulation; the redb experiment tests and example |
| `arkiv-db` | Raw embedded facade (`Database`), the block engine (`Engine`, metering, namespaces, typed records, indexes and queries, host state, host block map and expiry, the state root, the fallible `try_` API), the durable engine facade (`durable::DurableEngine`), the canonical codec (`codec`), examples, and functional integration tests |
| `arkiv-db-trie` | Persistent Merkle Patricia map (`MerkleMap`) with in-memory and lazily loaded stored nodes, staging for commits, point proofs, the strict verifier `walk`, and order-preserving key encoders; every engine map is built from it (see [the state trie notes](docs/state-trie.md)) |
| `arkiv-node` | The pseudo node's library (node step N0b-3): block format version 1 and its hashes (`block`), genesis invariants and the expiry reservation (`params`), static validation (`validate`), block selection (`select`), request identities (`identity`), the block executor (`exec`) an in-memory chain with prepare/finalize and replay (`replay`), and the bounded producer/full/light executable (`demo`, `arkiv-demo`) |
| `reth-trie` | Local extraction of Reth's generic trie interfaces |
| `alloy-trie` | Vendored MPT implementation used by Reth |

Dependencies point from the public API toward storage. The contract does not
depend on either the facade or a concrete backend. The engine keeps its
state in the trie crate's maps and uses the storage crate's `Entry`,
`StorageError` and `ObjectReader` types; it does not go through the mutable
`KvStore` interface. See
[the architecture notes](docs/architecture.md) for boundaries and next steps.

## Development

Rust 1.90 or newer (edition 2024, let chains). The minimum was raised from
1.88 to 1.90 because redb 4.3.0 requires it; `cargo +1.90.0 check --workspace
--all-targets --offline` passes. Commands run from the repository root:

```sh
cargo fetch --locked
cargo test --workspace --offline
cargo clippy --offline --all-targets -p arkiv-db -p arkiv-db-storage -p arkiv-db-memory -p arkiv-db-redb -p arkiv-db-trie -p arkiv-node -p reth-trie -- -D warnings
cargo fmt -p arkiv-db -p arkiv-db-storage -p arkiv-db-memory -p arkiv-db-redb -p arkiv-db-trie -p arkiv-node -p reth-trie -- --check
cargo run -p arkiv-db --example basic --offline
cargo run -p arkiv-db --example blocks --offline
cargo run -p arkiv-db --example records --offline
cargo run --release -p arkiv-db-redb --example redb_experiment --offline -- --quick
cargo run --release -p arkiv-db-redb --example engine_storage_experiment --offline
cargo test -p arkiv-db --offline --test rehash -- --nocapture   # nodes rehashed per block
```

The redb tests and the experiment write their files to a real disk (Cargo's
target directory, or `ARKIV_BENCH_DIR`), not to the system temporary
directory. The full experiment report (`redb_experiment` without `--quick`)
builds files up to 1 GiB and takes several minutes.

Fetching is needed once for a fresh checkout; subsequent tests can run offline.
Copied upstream Alloy source is excluded from workspace formatting and
Clippy; every other crate is maintained and listed in both commands.

The integration tests exercise the public API. The four raw-KV files
(`kv.rs`, `semantics.rs`, `backend.rs`, `model.rs`) are unchanged: CRUD,
replacement sizes, batch ordering, binary prefix boundaries, ownership,
independent databases, backend errors, and deterministic block packets
against an independent model. The milestone-1 engine files cover:

- `blocks.rs`: the six-step flow, the begin/execute/seal/publish lifecycle,
  stale-head and foreign-candidate rejection, mixed-namespace atomic
  publication, retained snapshots, and the host block map (consecutive
  blocks, empty blocks, several publications per block).
- `rollback.rs`: checkpoints and rollback of records, descriptors, the
  namespace allocator and pending pricing, with receipts and block spend
  never withdrawn.
- `namespaces.rs`: ID allocation and encoding, permissionless creation,
  transfer preconditions and their check order, same-block authority changes,
  record-map sharing across a transfer, and the directory capacity.
- `metering.rs`: hand-derived receipts for every byte-level operation under
  the fixture schedule, atomic charge groups, exact and insufficient budgets,
  block budgets and admission, overflow, ledgers, pinned pricing and
  estimation parity.
- `history.rs`: every published snapshot compared with an independent
  mutation-log oracle after later writes, rollbacks and competing candidates.
- `isolation.rs`: namespace A's charges, abort points and record visits are
  unchanged while namespace B and the namespace count grow, with directory
  visits inside the stated bound.

The milestone-2 engine files cover typed records and queries:

- `records.rs`: the frozen key, name and value encodings with their test
  vectors; create, patch and delete semantics (dense ids never reused, key
  recreation minting a new id, cell kind and type changes, `AlreadyExists`,
  `NotFound`, `RecordFull`); argument validation and its check order; the
  host path and its restriction; projections of `get_record`; and the index
  staying exactly derived from the live rows after every write and rollback.
- `queries.rs`: every operator of the declared subset (equality, inequality,
  ranges with each bound shape, prefixes, existence, negation, AND-groups,
  OR of groups, match-all) and `count` against a full-scan reference
  evaluator over an independent record model, including missing attributes,
  type differences and field cells; query validation faults and their order.
- `pagination.rs`: keyset walks with varying page limits give one identical
  result; `next` is present exactly when a further match exists; cursors
  bound to snapshot and engine instance, or to a branch and its mutation
  serial, plus namespace, query and projection, with every mismatch rejected
  before any charge; pages unchanged while new snapshots are published.
- `query_metering.rs`: hand-derived receipts and ledgers for record writes,
  `get_record`, `query` and `count` under the fixture schedule, checked for
  every budget from zero to the total; term-creation versus membership
  pricing; lazy evaluation charging only the candidates scanned plus each
  group's head beyond the page; no partial page on exhaustion; estimation
  and host-path parity.
- `query_history.rs`: seeded blocks of record writes, patches, deletions,
  expiry and key recreation compared at every published snapshot with the
  full-scan reference, including records absent from the current index; a
  full walk repeated at the end, after every later block, identical to the
  walk taken at the boundary.
- `query_isolation.rs`: the isolation harness extended to record writes and
  every query operator: namespace A's receipts, abort points, record and
  index node visits are unchanged while namespace B grows records and
  identical terms and the directory grows, with directory visits inside the
  stated bound.
- `terms.rs`: the `terms` read against a model of the live records
  (semantics, validation order, literal positional costs), its isolation
  from a second namespace's identical terms, and the height-by-height
  expiry sweep with its (height, id) order, halts and survivors.

The node-step-N0b-2 files cover the codec and the engine additions:

- `codec.rs`: golden vectors for every covered type, built by hand from the
  [block format](docs/block-format.md) specification; rejection of
  non-canonical input (bytes other than `00`/`01`, unknown tags, invalid
  names, zero ids, bad key lengths, wrong price-class counts, trailing and
  truncated input, huge counts); every accepted 1-byte input of 12 types,
  2-byte input of 9 types and seeded mutation of 17 vectors re-encoding to
  itself; digests over hand-built preimages.
- `host_state.rs`: the host-state map's API, bounds, ranges, rollback,
  publication and retention, and its isolation: the same workload with and
  without host entries gives identical results, receipts, reads and
  statistics.
- `fallible_api.rs`: every `try_` form returns exactly what the infallible
  method returns, including the expiry sweep.

The node-step-N0b-3 files in `crates/arkiv-node/tests/` cover the block
format and execution:

- `block_format.rs`: golden vectors for every block type and hash, built
  by hand from the [block format](docs/block-format.md); unsupported
  versions, unknown tags and malformed input refused.
- `replay.rs`: fixtures with hand-derived outcomes (a multi-operation
  transaction, rollback, same-block transfer, expiry at the boundary and
  at capacity, a pricing change, permanent and extend-only expiry, empty
  blocks, invalid and duplicate requests refused by replay, invariant and
  halt errors), each replayed to identical bytes and pinned by golden
  block hashes.
- `block_model.rs`: seeded block histories whose every outcome, receipt,
  expiry deletion, admin outcome and pricing change is predicted by an
  independent oracle, with the records, raw maps, query answers and due
  schedule compared at every height and again after a full replay.
- `exec.rs`, `genesis.rs`, `validation.rs`, `selection.rs`, `identity.rs`:
  the executor's failure paths, the genesis invariants and reservation,
  static validation as operations will execute, block selection, and the
  request-identity state machine.

The storage files of node step N2 cover the versioned contract and the redb
adapter: one behavioural suite (`arkiv_db_storage::testing::contract`) runs
against the memory store, the redb store on a real file and on a simulated
disk, and fault-injecting wrappers of both; `crates/arkiv-db-redb/tests/`
checks atomicity at every crash point of a block commit (including torn
writes, after a restart and in steady state), crash-safe store creation,
immediate durability, recovery with and without quick repair, historical
reads after reopen and during commits, reconciliation of indeterminate
commits, failed-handle latching, disk full, namespace isolation and write
amplification.

See [the testing notes](docs/testing.md) for what each file establishes and
lessons from redb. The [basic example](crates/arkiv-db/examples/basic.rs)
shows the raw facade; the [blocks example](crates/arkiv-db/examples/blocks.rs)
runs the whole engine flow: two namespaces, metered blocks, a transfer that
moves no data, rejected operations, a historical read, and one block touching
both namespaces. The [records example](crates/arkiv-db/examples/records.rs)
runs the typed flow: records with attributes and fields written in metered
blocks, queries over the attribute indexes with projection, a patch that
moves a record between type indexes, a keyset walk pinned to one snapshot
while a later block publishes, a historical query through the block table,
and host expiry in the block where the records expire.

Local packages are unpublished. The lockfile is checked in so dependency changes
are visible in review. Work proceeds in small commits on `main`.

Every native signed chain runs the `glm-v2` gas profile: GLM pricing with
[funded accounts and fee burning](docs/glm-accounts-v1.md). Create a producer
from `--allocations-file allocations.json` or from a genesis `--bundle`.

The signed proposer accepts the existing Arkiv SDK over JSON-RPC at `/`; see
[compatibility, limits and validation](docs/sdk-compatibility.md). New chains can
use a [TOML genesis bundle with prefills](docs/examples/genesis-bundle/README.md).
For larger datasets, the [streaming genesis v2 path](docs/streaming-genesis-v2.md)
uses [bulk index construction](docs/bulk-prefill.md) with bounded external
sorting and atomic publication. Incomplete construction restarts from input;
completed private state can be reused. A
[Sourcify converter](docs/sourcify-prefill-converter.md) preserves dataset references.
The [matched 100k/200k bulk scaling report](docs/bulk-prefill-scaling.md) records
producer startup, memory, write activity and final disk allocation.
The [100,000-contract validation report](docs/streaming-prefill-100k-validation.md)
records the historical incremental sample import, recovery and producer/full/light checks,
including the existing broad equality-proof resource limit.

Browse the signed node’s [API reference](docs/node-api-reference.md)
at `/docs` (Scalar), with an OpenAPI specification at `/openapi.json`.
It covers every signed-node HTTP route, all 31 JSON-RPC methods, WebSocket
subscriptions, and the separate private and metrics listeners.
