# Functional test strategy

## Engine storage v1 (N3c)

The [durable facade](engine-storage-run.md) preserves the ordinary memory
engine and adds its own format/lifecycle tests. The
[measured report](engine-storage-v1-report.md) contains reproducible commands,
workload sizes, backend counters, memory accounting and limitations.

- `arkiv-db/tests/durable.rs`: literal genesis/manifest fields and digest
  preimages; stored-root head after 10,000 publications; lazy historical
  opens, metadata-only transfer and cursor identity across reacquisition,
  in-place recovery and fresh open. An internal weak-reference test checks
  that preparation releases dirty values after callers drop their clones.
- `tests/support/durable_history.rs`: one seeded history shared by ordinary
  memory `Engine`, `DurableEngine` over the memory store, and actual redb
  in `arkiv-db-redb/tests/engine_history.rs`. It compares all value types,
  raw/typed writes, ownership, rollback, pricing and host state; after real
  close/reopen it compares roots, receipts, probes, pagination and complete
  Eq proof bytes at historical snapshots under three cache sizes.
- `arkiv-db/tests/durable_failures.rs`: all commit/initialization outcomes,
  failed-health cache gating and recovery, exact retry ownership versus
  stale caller candidates, invalid/missing metadata and correctly hashed
  malformed objects, sequence overflow, resource-cap boundaries, cache
  payload/entry/container bounds and namespace isolation.
- Unit tests in `arkiv-db/src/durable/cache.rs`: the object cache's O(1)
  key index beside its FIFO. A hit after an insert, entry- and byte-limit
  eviction removing exactly the evicted key's slot, a duplicate insert
  skipped without changing entries or accounting, recovery clearing the
  index and restarting the FIFO, and hits equal to a linear scan through
  long churn with the FIFO and index capacities bounded.
- `arkiv-db-trie/tests/bounded_stage.rs` and private ownership tests:
  preflight before encoding/hashing, full RLP size bounds, bounded new-node
  walks, fallible sinks, nested-map traversal despite object deduplication,
  no stored-subtree loads, dirty-node release and read-only stored handles.
- `arkiv-db-redb/tests/engine_crashes.rs` and `engine_process.rs`: actual
  engine object transactions through CrashDisk cuts, growth/spills, I/O and
  disk-full failures, lost final sync/page-cache visibility, and SIGKILL
  before commit and after durable commit but before facade publication.
  Large-commit cuts are sampled as stated in the report; the small commit's
  mutating-call boundaries are exhaustive. Generic observed application
  remains durability-unknown, never a successful acknowledgement.

These tests supplement the N2 adapter suite; they do not claim durable
protocol blocks or producer restart. Performance/RSS values are
measurements, while CI asserts structural and counter bounds.

## Running the suite

Run the complete suite without network access:

```sh
cargo test --workspace --offline
```

The tests use the Rust standard library and workspace crates. The workspace
also runs tests from the vendored Reth/Alloy trie sources and their Cargo
dependencies. Integration tests live in `crates/arkiv-db/tests/`: four raw-KV
files that test the public database API and storage extension boundary, six
milestone-1 engine files that test namespaces, snapshots, metering and
publication, seven milestone-2 engine files that test typed records,
indexes, queries, pagination, term listings, expiry and their metering,
one node-step-N0a file that pins the physical traces of queries and counts
rejected part way, three node-step-N0b-2 files for the canonical codec's
golden vectors, the host-state map and the fallible `try_` API, and three
node-step-N3b files for state roots, storage failures and rehashing. Unit
tests inside `arkiv-db-memory` check the AVL persistent map (a reference
structure since N3b) against a `BTreeMap` model, and unit tests inside
`arkiv-db` pin the frozen record encodings (including both `RecordKey`
encoding versions), the trie key orders and leaf layouts, the meter's
atomic groups and the restoration of a branch after a storage error. Node step N2 adds
the versioned storage contract suite (run against the memory store in
`crates/arkiv-db-memory/tests/` and against every store kind in
`crates/arkiv-db-redb/tests/`) and the redb experiment tests, which use
redb and real files on disk. The trie crate `arkiv-db-trie` (node step
N3a) has its own unit and integration tests in `crates/arkiv-db-trie`,
described below; every engine map is built on it since node step N3b. The
node library `arkiv-node` (node step N0b-3) has eight integration test
files in `crates/arkiv-node/tests/`, also described below. The proof and
availability acceptance cases further down are planned and do not describe
guarantees established by the current suite.

## Current coverage

### Raw KV tests (unchanged)

| File | What it checks |
| --- | --- |
| [`kv.rs`](../crates/arkiv-db/tests/kv.rs) | Consecutive block packets, repeated-key operation ordering, empty blocks, binary scans, owned results, and replacement size changes |
| [`semantics.rs`](../crates/arkiv-db/tests/semantics.rs) | Binary boundaries in block packets, input/output ownership, discarded candidates, backend wrapping, and instance isolation |
| [`backend.rs`](../crates/arkiv-db/tests/backend.rs) | Exact backend errors, rejected blocks and retry, and forwarding every whole block once in order, including empty blocks |
| [`model.rs`](../crates/arkiv-db/tests/model.rs) | Four deterministic chains compared at every block boundary with an independent mutation-log oracle |

The model test uses four fixed seeds and 256 blocks per seed: **1,024 blocks
and 28,772 submitted mutations**. Block sizes cycle through 0, 1, 2, 8, 32
and 128 operations. Packets deliberately replace, delete and recreate repeated
keys alongside unrelated writes. Each block submits one `WriteBatch`; point
reads and prefix scans are checked before submission and after the complete
block applies. Discarded candidates are also checked.

The reference state comes from searching an append-only list of intended
mutations backward; it does not use `BTreeMap` or consume the production batch.
This avoids reproducing the backend's data structure or hiding packet-to-batch
conversion bugs in the oracle. Failures report seed, block number, phase and
key or prefix. Focused storage/ownership tests retain small individual calls
where they isolate the public KV contract.

In these files block numbers identify test inputs only. They cover ordered
application of already accepted mutations through the raw facade; the engine
tests below are where rollback, historical reads and cost accounting are
established. An earlier owned scan through the facade is a copied result,
not a retained snapshot.

The failing test backend rejects writes before changing its state. This tests
error propagation through the facade and the usability of the public backend
interface. It does not establish rollback after partial I/O or crash safety.

### Engine tests: milestone 1

These files were unchanged by milestone 2 apart from the `model_version: 2`
literal in their receipts and the match arms `history.rs` needs for the new
operation and rejection variants (which its oracle never issues). Node step
N0b-1 re-derived their write literals for model version 3 (see below).

| File | What it checks | Evidence it gives |
| --- | --- | --- |
| [`blocks.rs`](../crates/arkiv-db/tests/blocks.rs) | The six-step flow with a `BlockMap`; `begin`/`execute`/`seal`/`publish`; repeated-key operations inside one block; `StaleHead` for a stale expected head and for a candidate whose base moved, with the loser keeping its receipts; `ForeignCandidate` for a candidate sealed by another engine; a candidate touching records of two namespaces and the descriptor of a third becomes visible all at once and never partially, from a retained handle and from a fresh head handle; sequential snapshot IDs distinct from block numbers (an admin publication and a data publication in one block, empty blocks); `BlockMap` rules (first block any number, then consecutive; strictly increasing snapshots; no re-recording); fifty retained snapshots answer unchanged | Only one of competing candidates wins; discarded work changes no published snapshot; a block resolves to exactly one complete terminal state |
| [`rollback.rs`](../crates/arkiv-db/tests/rollback.rs) | `checkpoint`/`rollback_to` restores records, descriptors, the ordinal allocator and pending pricing; returns the index range of discarded receipts; receipts, spent and remaining are untouched; later checkpoints become invalid while the target stays valid; a stale handle issued before a truncating rollback is `InvalidCheckpoint` even if its stack index is occupied again, as is a checkpoint from another branch; a rejected operation leaves state unchanged; an ordinal released by rollback is reused | Transaction-level rollback inside a block with receipts never withdrawn |
| [`namespaces.rs`](../crates/arkiv-db/tests/namespaces.rs) | Ordinals from 1, 8-byte big-endian encoding, zero invalid; `CreateNamespace` is permissionless with owner = actor and revision 0; transfer checks `UnknownNamespace`, then `Unauthorized`, then `StaleRevision`, each with spent 1; revision increments; an old owner's write after a same-block transfer is `Unauthorized`; identical keys in two namespaces never alias; a transfer of an empty and of a populated namespace visits zero record nodes and leaves `records_shared_with` true; `DirectoryFull` at the cap after the lookup charge | Ownership transfer is a metadata operation; namespaces are separate keyspaces; authority is checked at the operation's position |
| [`metering.rs`](../crates/arkiv-db/tests/metering.rs) | Literal per-operation totals from the byte-level rows of the charge-point table (the version-1 charge points plus the model-3 `RootUpdate` in each write group) under `FIXTURE`, checked for every budget from zero to the total; ledger entries in first-charge order with zero-count classes omitted; traced and untraced charges equal; atomic groups charge nothing on `OutOfBudget`; exact budgets succeed; insufficient budgets reject with spent retained and state unchanged; `CostOverflow` at the subtotal and at the running total; zero-cost rejections; block budgets: `ExceedsBlockBudget` admission is not recorded, an unlimited operation is capped by the block remainder, rejected operations drain the block, reads never deduct, the aggregate equals an independently summed literal; pricing pinned at the base after `set_pricing`, `StaleSchedule` for a non-increasing id; `estimate` equals `execute` on the same state and records nothing; repeated reads give identical receipts | Deterministic, checked, hand-verifiable receipts |
| [`history.rs`](../crates/arkiv-db/tests/history.rs) | Four fixed seeds of 192 real blocks over namespaces created along the way, with transactions, rolled-back spans, budget failures, stale losers published after the winner, admin pricing candidates and empty blocks. The oracle predicts every verdict itself (authorization, revision, admission, budget abort point and cost from a charge table copied from the plan) and compares whole receipts and effects before recording an operation; the branch view is checked after every operation, the head before publication, and at the end every block's `get`, `scan_prefix` and `descriptor` for every key, prefix and namespace against the mutation log, including keys deleted later, previous owners and namespaces that did not exist yet. Read receipts at each boundary are re-read at the end, and each seed replays to an identical transcript on a fresh engine | Published snapshots are immutable, retained and independently predictable; namespace IDs are deterministic under replay |
| [`isolation.rs`](../crates/arkiv-db/tests/isolation.rs) | Freeze namespace A's state, operations and pricing; grow namespace B's records and the number of namespaces; repeat A's put/delete/get/scan/descriptor and compare receipts, abort points and `record_nodes`; check `record_nodes` and `directory_nodes` against counts derived from the trie shapes (node step N3b; the AVL height bound before) | Cost isolation between namespaces for the byte-level operations, in memory, without proofs |

### Engine tests: milestone 2

Each file states in its top comment what it establishes and what it does
not. Expected receipts are hand-derived from the charge-point table (version
2 rows when the files were written, model-version-3 rows since node step
N0b-1) and the fixture weights in the
[plan](snapshot-query-plan.md#operation-cost-is-part-of-the-first-milestone);
expected results come from an independent reference (a `BTreeMap` model of
the live records and a full-scan evaluator of the query semantics written
from the plan), never from the engine's own index or a second engine call.

| File | What it checks | Evidence it gives |
| --- | --- | --- |
| [`records.rs`](../crates/arkiv-db/tests/records.rs) | The frozen encodings against their vectors (`RecordKey` encoding version 2: the length-prefixed 33-byte form for keys of 1 to 32 bytes, including 32-byte keys, and its rejections; the 32-byte version-1 migration form and its rejections; `CellName` grammar, `TypeTag` ids, `Value` term bytes and order); create/patch/delete semantics: dense ids from 1 never reused, a deleted key recreated gets a new id, cells stored sorted by name, `set` replacing or adding cells with kind and type changes, `remove` of an absent name as a charged no-op, `AlreadyExists`, `NotFound`, `RecordFull` after the key lookup; argument validation faults and their check order, rejected before any charge and never recorded; the host path accepting record operations only, skipping the owner check and otherwise charging identically; `get_record` projections (id only, key, all cells, named cells including missing names); rollback restoring rows, key bindings, the id allocator and the index, so that a term the transaction created is new again while a term it had removed is held again and charges no creation; and `index_consistent` true after every write | The record model behaves as specified and the term index is exactly derived from the live rows at every point |
| [`queries.rs`](../crates/arkiv-db/tests/queries.rs) | Every operator of the declared subset against the full-scan reference over fixtures that mix types, kinds and missing cells: `Eq`, `NotEq` (a missing or differently typed attribute matches neither), `Range` with each bound combination including empty and reversed ranges and the signed/unsigned encodings, `StartsWith` including the empty prefix, `Exists` per type, negated literals as complements over the live rows, AND-groups, OR of groups with overlapping matches emitted once, match-all, field cells never matching, results in record-id order, `count` agreeing with the walk; query validation faults (`NoGroups` through `ProjectionTooLarge`) in their stated order, rejected before any charge | The query semantics are the ones written in the plan, independently of the index that answers them |
| [`pagination.rs`](../crates/arkiv-db/tests/pagination.rs) | Keyset walks with page limits from one to above the result size produce one identical sequence; `next` is `Some` exactly when a further match exists and `None` on the last page, including a page that ends exactly at the last match; a cursor from a snapshot is refused by another snapshot, another engine instance, another namespace, another query or another projection, and a branch cursor by the same branch after any applied write, rollback or pricing change; every mismatch is `CursorMismatch` with spent 0 and nothing recorded; the page limit may change between pages; pages are unchanged while new snapshots are published between them | Pagination is pinned to one snapshot and exact, and a cursor cannot cross contexts |
| [`query_metering.rs`](../crates/arkiv-db/tests/query_metering.rs) | Literal receipts and ledgers for `CreateRecord`, `PatchRecord`, `DeleteRecord`, `get_record`, `query` and `count` under `FIXTURE`, including the plan's counting vectors, with the abort point and the units spent so far at insufficient budgets; term-creation charged only for a new term (the +30-per-new-term vector), made new again by deletion and patching; atomic groups charging nothing on `OutOfBudget`; lazy evaluation charging `limit + 1` posting entries for a full page and exactly the remainder on the last page, no work for a group whose head lies beyond the page, per-entry charges of one unit before use, and no partial page on exhaustion; `estimate`/`execute` and owner/host path parity; reads never deducting from the block | Index and query work is charged deterministically, before the work, and only for the candidates actually scanned |
| [`query_history.rs`](../crates/arkiv-db/tests/query_history.rs) | Fixed seeds of real blocks with record creation, patches that change values, kinds and types, deletions, key recreation and host expiry through `expire_records`; a mutation-log oracle reconstructs the live records at every published snapshot and the full-scan reference answers a fixed set of queries and counts there, compared with the engine's answers after every later block, including records that a later block deleted or expired and are absent from the current index; a full walk repeated at the end, after every later block, is identical to the walk taken at the boundary (the between-pages property is `pagination.rs`'s); the indexes are consistent at every snapshot | Historical index queries match the reference at every snapshot, expiry is a historical fact, and key reuse never joins histories |
| [`query_isolation.rs`](../crates/arkiv-db/tests/query_isolation.rs) | Freeze namespace A's typed records, operations and pricing; grow namespace B with records that carry the same attribute names, types and values as A's and then grow the number of namespaces; repeat A's record writes, record reads, equality, range, prefix, existence, inequality, negated, match-all, multi-group, paged and counted queries and compare whole receipts, results, abort points, `record_nodes` and `index_nodes`; check `directory_nodes` against counts derived from the directory trie's shape (the AVL bound before N3b) | Query-cost isolation between namespaces for every implemented operator, in memory, without proofs |
| [`terms.rs`](../crates/arkiv-db/tests/terms.rs) | The `terms` read against a `BTreeMap` model of the live records (the distinct values of one attribute and type in value order from each lower-bound shape, limits and missing terms); its validation faults in their stated order (`NotIndexable`, `MixedBounds`, `ValueTooLarge`, `ZeroLimit`, `LimitTooLarge`), rejected before any charge; literal positional receipts under `FIXTURE` (the seek plus `TermVisit` 1 and the value bytes per returned term, and nothing for a stepped-over `Excluded` bound, the run end or a missing term); receipts unchanged while a second namespace holds identical terms; and `expire_records` sweeping height by height: deletes in (height, id) order, halts at a rejection with the applied deletes kept and later resumption, and rows above the block surviving | Term listings are positional and namespace-local, and expiry is a height-ordered sweep whose work is linear in the expired heights plus the deleted rows |

Three rules apply across the engine tests:

- **Oracle independence for costs.** Expected receipts are literal
  hand-derived numbers taken from the fixture weights and charge-point table
  in the [plan](snapshot-query-plan.md#operation-cost-is-part-of-the-first-milestone),
  or from a copy of that table inside the test. A test never computes an
  expectation by calling `PriceSchedule::price` or any other engine pricing
  API, and never derives an expectation by running the engine first, so a
  wrong charge point cannot be confirmed by the code under test.
- **Oracle independence for results.** Expected query answers come from a
  `BTreeMap` model of the live records and a full-scan evaluator of the
  semantics in the plan, or from the mutation-log oracle; never from the
  engine's index or a second engine call. Where the engine contradicts the
  plan, the test keeps the plan's expectation and is marked ignored with the
  reason rather than adjusted to the engine.
- **Freeze A in discarded branches.** The isolation tests run A's workload
  on a branch from the current head and drop that branch without sealing or
  publishing it, so A's published records, rows, index and descriptor are
  identical in every phase. Between the phases they publish B's growth (in
  `isolation.rs` 10,000 byte records in phase 1 and 500 additional namespaces
  in phase 2; in `query_isolation.rs` records sharing A's terms, then
  namespaces) and rerun the same workload from the new head. Record growth
  must leave every receipt, effect and physical count identical; directory
  growth must leave receipts and namespace-local node visits identical while
  directory visits follow the directory trie's shape (derived by hand since
  node step N3b). Absolute counts derived from the shape of A's own maps
  hold in every phase, and a budget-aborted scan or query proves lazy early
  stop rather than materialize-then-charge.

The engine tests establish work and cost guarantees, not constant-latency
promises on shared hardware. They do not test proofs, sorting, or a durable
backend, because none exist. Cost vectors use the declared fixture weights
until production calibration exists; the two schedule invariants
(`IndexTermCreate > 0`, symmetric delete weights) are documented, not
enforced by the engine. Publication is an O(1) append of structurally
shared state. Model version 3 prepays root maintenance in the write
receipts; the roots exist since node step N3b and the nodes a block
rehashes are measured against the prepaid entry writes (`rehash.rs`), while
the durable commit (N4) and its commit-fit measurements remain.

### Engine tests: node step N0a

Step N0a fixed the loss of physical visit counters when a `query` or `count`
is rejected part way. Before the fix, a rejection while a query opened a
later group or pulled a group's first head dropped every source opened so
far. A `Range`/`StartsWith` run stopped at a `TermVisit` also dropped the
postings it had already positioned. Their node visits never reached the
`PhysicalTrace`. A rejection during an advance or a row emission was already
reported in full. The fix changes no charge, charge order or receipt, and no
existing test changed. The file follows the three rules above: every count
and receipt is a hand-derived literal, and the expected rows are evaluated by
hand.

| File | What it checks | Evidence it gives |
| --- | --- | --- |
| [`trace_regression.rs`](../crates/arkiv-db/tests/trace_regression.rs) | One four-record namespace whose map shapes (row map, term map, postings; trie shapes since N3b) are derived by hand in the file. Seven reads run under every budget from 0 to their success total: a three-group page (`Eq` with a probe, a three-term `StartsWith` run, a negated row-map group; its third group failing to open keeps the postings the run already positioned) and its `count`; a three-group page whose later groups fail to open after the earlier ones looked up their terms; a `Range` and a `StartsWith` run failing at each `TermVisit`; a `NotEq` page and a match-all page failing at every `PostingEntry`, `RecordProbe` and `RowVisit`; an `Eq` page failing at its row emissions. Each rejection's trace equals the literal count of nodes visited before the failing charge. For failing budgets b1 < b2 every component is ≤, and every failing trace is ≤ the success trace. Each success trace is a literal written as the sum of independently derived parts. Every receipt is a FIXTURE literal: the units spent before the failing charge with `OutOfBudget`, or the total with `Applied` | A read rejected at any charge reports each node its opened sources visited, exactly once. Successful traces and receipts are unchanged |

All seven tests fail against the evaluator before N0a. For example, the
three-group page rejected at its second group's `QueryGroup` reported 2
index nodes, not 3: the first group's posting seek was lost. Removing only
the run half of the fix fails four of them. Noting a completed run's
postings a second time fails five. Those counts described the AVL layout;
node step N3b derived them again for the trie (below).

### Engine tests: node step N0b-1

Step N0b-1 introduced counting-rule version 3 and `RecordKey` encoding
version 2. No new test file was added; the change is in the literals:

- Every hand-derived write receipt, ledger, block total, budget boundary
  and `model_version` literal in `blocks.rs`, `rollback.rs`,
  `namespaces.rs`, `metering.rs`, `isolation.rs`, `records.rs`,
  `query_metering.rs`, `query_isolation.rs`, `terms.rs` and
  `examples/records.rs` was derived again by hand from the model-3 rows of
  the [charge table](snapshot-query-plan.md#operation-cost-is-part-of-the-first-milestone),
  with the arithmetic next to each changed literal (for example create `a`:
  302 + `RowByte` 47 + `RootUpdate` 2 × (3 + 2·6) = 379). Read receipts
  did not change; `pagination.rs`, `queries.rs` and `trace_regression.rs`
  changed only their model-version constant.
- The oracles that transcribe the charge table (`history.rs`,
  `query_history.rs`, `query_metering.rs`, `query_isolation.rs`) gained
  the `RootUpdate` and `RowByte` weights and charge points in their own
  transcription, and the literal totals that check each transcription
  (the plan's worked vectors, block totals) were re-derived by hand; they
  still never ask the engine what an operation costs.
- Where a model-2 budget had been chosen to stop an operation at a
  particular charge (an abort between index groups, a halted expiry
  delete), the budget was re-chosen so the operation stops at the
  corresponding charge under model 3, and the new abort point is derived by
  hand.
- `records.rs` and the `record` unit tests check the version-2 key vectors
  (1-, 31- and 32-byte keys, rejection of length 0, length 33 and non-zero
  padding) and the version-1 migration helpers (`encoded_v1` is `None` for
  a 32-byte key; `decode_v1` rejects length 32).
- The engine is exercised with the longest key. `query_metering.rs`
  creates a record under a 32-byte key and reads it back under every
  budget. The hand-derived receipts count the raw key, not its 33-byte
  encoding: `RowByte` 32 + 46 = 78 and a `KEY_ONLY` `ReadByte` of 32. The
  `query_history.rs` workload's longest key is now 32 bytes (it was 31),
  and every seed must apply writes under it.

One arithmetic slip was made during the re-derivation: the namespace-cap
block total in `namespaces.rs` (53 + 53 + 1 + 53 + 1) was first written as
163. Its test failed, and the sum was redone by hand (161), not copied from
the engine's output.

### Engine tests: node step N0b-2

Step N0b-2 added the canonical codec (`arkiv_db::codec`, codec version 1,
specified in the [block format](block-format.md)), the engine's host-state
map and the fallible `try_` surface. The codec vectors follow the rule for
independent oracles: every expected byte string is written in the test from
the specification's pieces (literal bytes, `u32`/`u64` big-endian,
`u32 length ‖ bytes`, the tag tables), never produced by the encoder, and
every expected digest is `keccak256` (a trusted primitive, itself pinned by
its empty-input vector) over a hand-assembled preimage. The host-state and
fallible-API files assert equality with independent runs rather than costs:
their claim is that host state and the `try_` forms change nothing.

| File | What it checks | Evidence it gives |
| --- | --- | --- |
| [`codec.rs`](../crates/arkiv-db/tests/codec.rs) | Golden vectors for the primitives, text, options, sequences, pairs, arrays and `B256`, and for every covered engine type: `NamespaceId`, `OwnerId`, `Revision`, `Cost`, `RecordId`, `NamespaceName`, `CellName`, `NamespaceDescriptor`, `RecordKey` (1, 31 and 32 bytes), `TypeTag`, every `Value` type, `CellKind`, `Cell`, every `WriteOp` and `Effect` variant, all 8 `ArgumentFault`, 11 `QueryFault` and 14 `Rejection` tags, `Outcome`, `ReceiptKind`, `Receipt` (a traced receipt encodes like an untraced one and decodes without a ledger), `PriceSchedule` (the FIXTURE and a modified schedule) and `Limits`. Each vector decodes back, re-encodes identically, is rejected with a trailing byte and is `Truncated` at every cut. Rejections: bool bytes 2 and `ff` and option byte 2, unknown tags of every enum, invalid names (grammar, empty, 21 bytes, non-UTF-8), zero ids, key lengths 0, 33 and 255, class counts 0, 27, 30, 32 and `u32::MAX`, huge counts and lengths with little input (including nested sequences and 256-byte items), and a `WriteOp` count whose 6-byte minimum items cannot fit. The `codec` unit tests check that the value tags are the type ids and that a sequence decoder reserves no more bytes than its remaining input. Every 1-byte input for twelve types and every 2-byte input for nine, and 1,000 seeded mutations of each of 17 vectors: whatever is accepted re-encodes to itself, and the accepted short inputs are exactly as many as the tag tables allow. Digests over hand-built preimages, domain separation, the 255-byte domain and the four invalid-domain panics. Operations over the engine's admission bounds decode and are rejected by the engine | Codec version 1 is exactly the documented layouts, canonical (one encoding per value, one value per encoding, except that receipts differing only in their unencoded debug ledger share an encoding), strict, and on hostile counts reserves no more than its input before failing |
| [`host_state.rs`](../crates/arkiv-db/tests/host_state.rs) | Host-map put/replace/remove/get, the empty and the 256-byte value, 257-, 300- and 4096-byte values refused with no change (not even to the branch mutation serial); bounded, ordered ranges from present and absent keys on branches and snapshots; nested checkpoints and rollbacks; snapshots keeping their own versions across three blocks, a losing competitor, a rolled-back write and a discarded candidate; every successful host write (same value, absent key) invalidating branch cursors while host reads do not. Isolation: one 19-step workload (namespace creation, raw puts and deletes, typed creates, patches and deletes, the host path, a transfer, five rejections, estimates, an expiry sweep) on two engines, one writing host entries that spell the workload's namespace ids, raw keys and record keys between every step; every result, estimate, receipt list, spend, metered read and statistic of both branches and of the published snapshots is identical, and the host map equals a `BTreeMap` model of the host writes | Host state is rolled back, published and retained with the state, and is invisible to and untouched by the data plane; it changes no receipt |
| [`fallible_api.rs`](../crates/arkiv-db/tests/fallible_api.rs) | A 17-step workload (four rejections) run in lockstep on two engines, one through the infallible methods and one through the `try_` forms: every write and estimate result, receipt list and spend; then on the branch and on two published snapshots every metered read under an unlimited and a starved budget (`get` and `scan_prefix` including oversized keys, `descriptor`, `get_record`, `query` pages with cursors handed between the forms, `count`, `terms` including an admission rejection), every statistic, the sharing statistics and the hidden index test aid; `try_expire_records` against `expire_records`, completed and halted | The `try_` forms return exactly the infallible results; `StorageError` is the storage contract's type |

### Engine tests: node step N3b

Step N3b moved every engine map to `arkiv-db-trie` (state format 1), added
the state root, and made the `try_` surface fallible. Charges did not
change: every receipt, ledger, block total and budget literal of every
engine test passes as it was. What changed:

**Re-derived physical literals.** Every structure-specific physical count
was derived again by hand from the trie shapes of its fixture, with the
derivation in comments next to the literals (which nibbles branch, which
paths are extensions, which nodes each step visits), never copied from
engine output; all of them matched the engine at the first run.

| File | Literals | Derivation |
| --- | --- | --- |
| `trace_regression.rs` | The exact `(directory, record, index)` trace at every charge point of all seven reads (for example the three-group page now succeeds with `(1, 15, 31)`, was `(1, 10, 15)`) | The fixture's row map is an extension over 15 zero nibbles, a four-slot branch and four leaves (6 nodes; a row load visits 3); the term map is drawn node by node in the file (20 nodes: c:red's lookup visits 8, c:∅'s and n:∅'s 5); postings: c:red 4 nodes, the four-id postings 6, single ids 1. Iterators seek on their first step (postings and the row map at the group's first head pull, run postings as the run opens) |
| `isolation.rs` | A's record-node counts replace the AVL height bounds: get present 8, absent 5, foreign 2, full scan 81, scan `a/1` 20, foreign prefix 3, aborted scan 11, put existing 8, put new 4, delete existing 9, delete missing 1; directory nodes per step exactly 3 (reads) / 6 (applied writes) with three namespaces and 5 / 10 with 503 | A's 64 raw keys, encoded `key ‖ pad to 20 ‖ len`, give a root branch on the first nibble (`\x00`, `a…`, `\xff…`), an extension over `1`, a branch on 0/2/f, an extension over `f 3`, a branch on the tens digit and per tens digit an extension over `3` and a ten-leaf branch: 81 nodes. Directory: ids 1–3 share 15 nibbles (extension, branch, leaf); ids 1–503 share 13 (extension, then branches on nibbles 13, 14 and 15, then the leaf) |
| `query_isolation.rs` | Directory nodes per step exactly 3 / 6 with two namespaces and 5 / 10 with 502, 0 before the first charge | Same directory shapes as above |
| `history.rs` | Directory reads at most 17, writes at most 34 (was the AVL height bound) | `2·8 + 1` for 8-byte ids; lookup plus insert path for writes |

Structure-independent properties stayed as they were: monotonicity of
failing traces, failing ≤ success, single counting, unchanged receipts, and
equality across isolation phases. `query_history.rs` changed one oracle:
typed sharing is root identity now, so it holds exactly when the model's
rows are unchanged (a touched namespace whose writes cancel out counts as
shared).

| File | What it checks | Evidence it gives |
| --- | --- | --- |
| [`roots.rs`](../crates/arkiv-db/tests/roots.rs) | A seeded 40-block history over four namespaces (raw puts and deletes including the empty key, creates, patches changing values, types and kinds, deletes, transfers, host puts and removes, pricing changes, rolled-back transactions, empty blocks; each kind asserted to occur): after every block and again after all of them, each namespace's four map roots, the catalog and host roots with their lengths, the encoded state header and the state root equal the model's, whose leaves are built by hand in the file from the state-format-1 layouts, whose roots come from the trie crate's yellow-paper reference construction (included by path) cross-checked with `HashBuilder` for keys of at most 32 bytes, and whose state root is `keccak256` over a hand-encoded header. Two histories of one logical content (other orders and block boundaries, a rolled-back transaction, transfers through different owners, a key deleted and rewritten, a patch undone) give one root. Owner, pricing, record allocator, host entry, cell value and raw value each change the root alone. An open branch's `try_state_root` equals the model's root after every transaction and the root of its later seal | The state root and every map root are the documented function of the logical content, independent of history, retained unchanged, and sensitive to every logical field |
| [`storage_faults.rs`](../crates/arkiv-db/tests/storage_faults.rs) | A state staged into a test object store and reopened with the hidden `Engine::from_state`: nothing is read at open, reads load lazily and equal the memory engine (values, receipts, physical visits). Every snapshot read failed at each of its node loads, with `Io` and with `Full`, returns that error and answers again after healing; lost and damaged objects give `Missing` and `Corrupt` only for reads that reach them. Thirteen branch calls (writes of every kind, estimates, host writes, a query, the expiry sweep), each failed at each of its node loads: the error, no receipt or spend for the failed operation, a poisoned branch whose every later call fails even after healing, `Poisoned` from `try_checkpoint`, `rollback_to`, `set_pricing`, `try_state_root` and `try_seal`; with enough reads the memory engine's receipts. Crafted leaves: a catalog leaf under another namespace's key and a key binding whose row holds another key are `Corrupt`; a leaf with unequal key and row counts does not decode; a row whose attribute holds a `Bytes` value or a 65-byte `Str` is `Corrupt` for `get_record`, `DeleteRecord` and `PatchRecord` (which poison the branch), while the same values as fields read; invalid headers are refused | Storage failures are availability errors, never absent records, empty results, rejections or receipts, and poison a branch; physical visits do not depend on residency |
| [`rehash.rs`](../crates/arkiv-db/tests/rehash.rs) | Each block of a chain committed to an object store and reopened: per map, the new nodes it hashes (every new root and new node of 32 bytes or more, counted before deduplication) against its `RootUpdate`-charged entry writes (checked against every ledger) times `2L + 2`, and the distinct objects it stages, never more than the hashed nodes, over a 20-block mixed workload; crafted deep paths with hand-derived exact counts: 66 record keys (one more key hashes and stages exactly 66 key-map nodes, removals 66 and 65) and 214 terms (one more record on the 175-node spine term, the longest a valid term allows, hashes and stages exactly 176 term nodes) | Rehashing per block is within the prepaid bound; the deep-path fixtures reach it as closely as valid keys and terms allow |

Unit tests: `layout.rs` compares the raw and term key orders with Rust's
slice and tuple ordering (empty keys, trailing zeros, prefixes, 20-byte
keys; negative and positive `I64`, `Bool`, empty and 64-byte `Str`,
existence next to an empty value) and checks the leaf layouts and their
rejections; `engine.rs` checks that a branch operation failed at any node
load leaves the working state exactly as before and that the engine
handles are `Send + Sync`.

Performance: on this machine (32 cores, debug profile, 2026-09-24),
`cargo test --workspace --offline` took 34.1 s before N3b and 40.2 s after
(both after building; the redb crash tests take 24 s of either). The
`arkiv-db` tests alone (`cargo test -p arkiv-db --offline`) took 4.5 s
before and 10.7–12.4 s after, the new files included; in release builds
1.2 s and 2.2 s. Timing the slowest test found one avoidable cost: inserting
a value that holds nested maps encoded it to check that it was not empty,
which hashed every nested node built so far on every operation instead of
once per root; `TrieValue::encodes_empty` removed that (that test went from
5.6 s to 2.3 s). The rest is the trie's inherent extra work (longer paths
of wider nodes, key encodings, hashing).

### Storage contract and redb experiment: node step N2

The versioned storage contract (`arkiv_db_storage::versioned`) is checked by
one behavioural suite, `arkiv_db_storage::testing::contract` (27 cases,
instantiated per store with `versioned_store_contract!`). Its expected
answers are written into the cases — the exact bytes, heads and outcomes the
contract defines — never predicted by running another store. The redb
measurements behind the gates are in the [redb experiment](redb-experiment.md);
the tests below assert what holds at debug scale, and
`cargo run --release -p arkiv-db-redb --example redb_experiment` produces
the release report.

Measurement files are created on a real disk (Cargo's target temporary
directory, or `ARKIV_BENCH_DIR`), never in `std::env::temp_dir()`, which is
a RAM tmpfs on the development machine where `fsync` does nothing. Debug
builds of redb walk every page on open (a `debug_assertions` check), so
tests never assert open I/O or cold-cache page counts; the release example
reports them. The heavy variants are `#[ignore]`d:
`cargo test --release -p arkiv-db-redb --test atomicity -- --ignored`.

| File | What it checks | Evidence it gives |
| --- | --- | --- |
| [`versioned_contract.rs`](../crates/arkiv-db-memory/tests/versioned_contract.rs) and unit tests in `arkiv-db-memory` | The whole suite against `MemoryVersionedStore` and `FaultyStore<MemoryVersionedStore>`; unit tests of put-if-absent objects per space, insert-only records, the head compare-and-set, conflicts rejecting whole batches, and identical retries never applied twice | The volatile reference implements the contract |
| [`contract.rs`](../crates/arkiv-db-redb/tests/contract.rs) | The same suite against `RedbStore` on a real file (reopened from its path), `RedbStore` on a `CrashDisk`, `FaultyStore<RedbStore>`, and the memory store: empty-store answers, every table and space, head CAS, put-if-absent, insert-only, batch duplicates, malformed batches, empty batches, retries, `reconcile`, history, reopen, per-space statistics, concurrent reads during commits, `Missing`, idempotent `recover`; the `faulty_*` cases: latching of every call until `recover`, which failures latch (`Io`/`Full` on any call and every `Indeterminate` outcome, not `Corrupt` unless enabled, not a refusal before the commit or a lost acknowledgement — `RedbStore`'s rule), `NotCommitted` before apply, `Indeterminate` applied and not applied, lost acknowledgement, per-space read faults, disk full before and inside the commit, and a failed recovery latching its own error, from a failed and from a healthy store | The durable store and the reference satisfy one contract, and `FaultyStore`'s latching mode behaves like `RedbStore` |
| [`atomicity.rs`](../crates/arkiv-db-redb/tests/atomicity.rs) | A ten-table commit cut off at every mutating backend call, as the first commit after a restart (49 calls with quick repair, 44 without) and in steady state after twelve blocks committed through the same store (54 calls); each frozen device yields 18 crash images (synced only, all unsynced, 8 sector-torn and 8 byte-torn seeded subsets); every image passes redb's integrity check and reopens to exactly the parent or exactly the candidate, `reconcile` agrees, an absent candidate then commits exactly once, and one more block commits on top, after which every block reads back — also after a power cut — and the image passes the integrity check again; `Committed` survives a synced-only image; quick repair never needs redb's full repair. A commit larger than the cache that grows the file (2,322 calls after a restart, 2,348 in steady state; pages spilled before `commit()`) at every sync and length change and the calls around them, the first 16, the last 32 and every 97th (61st) call. A monotonicity check: once a crash point publishes the candidate, every later one does | Gate 1 (atomicity) under the documented power-loss model |
| [`durability.rs`](../crates/arkiv-db-redb/tests/durability.rs) | After every `Committed`, a power cut taken at that instant keeps it (and all earlier blocks), with quick repair on and off; two syncs per commit with quick repair, one without; the initialization commit is durable (same database id after a power cut); a process kill after `Committed` on a real file loses nothing and needs no full repair | Gate 4 (immediate durability) |
| [`initialization.rs`](../crates/arkiv-db-redb/tests/initialization.rs) | Store creation cut off at each of its 23 mutating calls (104 distinct crash images): reopened in place, 8 images are refused as `Incompatible` (redb writes its magic number last); left behind as `create`'s temporary file, with the target absent or empty, every one is discarded and a fresh store created, and no temporary file remains; a complete image already renamed into place is opened unchanged; two concurrent `create`s of one path create exactly one store, the other is `Locked` | Creation is crash-safe on a path; in-place creation (`with_backend`) is not |
| [`recovery.rs`](../crates/arkiv-db-redb/tests/recovery.rs) | Files grown to 1, 4 and 12 MiB, killed and reopened: with quick repair never a full repair, and exactly four store-level reads for the open sequence (two metadata entries, head, manifest) — a count fixed by the code, asserted to catch any traversal; without quick repair always a full repair | Gate 3's constant application reads; the size scaling is measured by the example |
| [`history.rs`](../crates/arkiv-db-redb/tests/history.rs) | 60 blocks of new object versions reopened with a zero-byte and a 64 KiB cache: every object and record identical; four readers verifying random committed blocks while 80 more blocks commit; no redb savepoint exists | Gate 5 (historical objects) |
| [`reconciliation.rs`](../crates/arkiv-db-redb/tests/reconciliation.rs) | An I/O error injected at each of the 31 mutating calls of a commit: the store latches, reconciliation after `recover` finds the parent (the identical batch then commits once) or, when the final sync failed after the commit record was written, the candidate; `NotCommitted` never turns out committed; the same failure followed by a power cut instead reconciles as absent, and after a committed reconciliation a power cut keeps the block; a final sync that fails and loses its writes (reads still return them, as Linux can after an `fsync` error) is still read back as committed: with quick repair a power cut afterwards keeps the block, without it redb refuses the file as corrupt; a commit whose acknowledgement died with the process reconciles as committed and a retry is refused; a `FaultyStore` applied-but-indeterminate fault reconciles through a real reopen | Gate 6 (lost-acknowledgement reconciliation), and the limit of reading a commit back after a failed sync |
| [`latching.rs`](../crates/arkiv-db-redb/tests/latching.rs) | A read error latches every call (object, record, head reads, commits) with the same error until `recover`; a write error in a commit latches until `recover`; `recover` fails while the device fails and leaves the store `Failed` with the recovery's error, which every call then returns, then succeeds, twice in a row; `recover` waits for a read held inside the backend; a killed handle fails every call; damaged object bytes are `Corrupt`, do not latch and leave other objects readable; a missing catalog, host or record table is `Corrupt`, never absent; missing, foreign, other-format and non-redb files are refused with `Missing` or `Incompatible` | Gate 7 (failed-handle latching) and the corruption and format errors |
| [`disk_full.rs`](../crates/arkiv-db-redb/tests/disk_full.rs) | 32 quotas between the space the parent uses and the space the candidate needs, under a length quota (32 `NotCommitted(Full)`: redb grows the file before `commit()`) and an allocation quota where growing the length is free and writes allocate blocks (23 `NotCommitted(Full)`, 9 `Indeterminate(Full)`): every failure latches `Full`, reconciles absent while still full, fails again on retry, leaves the parent after a power cut, and after the quota is lifted the identical batch commits exactly once; `StorageFull` injected at every mutating call of a commit is always `Full`-kinded and reconciles, including one indeterminate commit that did commit | Gate 8 (disk full) under both space models |
| [`isolation.rs`](../crates/arkiv-db-redb/tests/isolation.rs) | Namespace A (200 objects) frozen while B grows 1×/10×/100×, namespaces grow to 100 and 400, and 100 blocks of history are added; each phase reopens with a zero-byte cache and reads all of A: identical application reads touching only A; exactly `directory height + A's height` backend page reads per object read; identical page reads under B and history growth; one more page per read when the table directory gains a level | Gate 2 (isolation), with the physical O(log namespaces) directory term |
| [`write_amplification.rs`](../crates/arkiv-db-redb/tests/write_amplification.rs) | 1,024 objects in blocks of 1, 16, 256 and 1,024, plus records-only blocks: two syncs per commit (plus at most one per file growth), and bytes written per logical byte strictly falling as blocks grow | Gate 9's ordering; the numbers come from the example |
| [`backends.rs`](../crates/arkiv-db-redb/tests/backends.rs) | `CountingBackend` forwards every lock method (second opens are `Locked` or `DatabaseAlreadyOpen`; a wrapper without forwarded locks lets the second open through, as a control); its counters see syncs and survive `recover`; a killed store releases its file lock | The instrumentation measures without changing redb's behaviour |

Unit tests in `arkiv-db-storage` cover the head encoding, record shapes, batch
validation and statistics differences; unit tests in `arkiv-db-redb` cover
the `CrashDisk` model itself (sync, crash images per mode, tearing inside
sector bounds, cut-off, crash points, the length and allocation quotas,
injected faults, and a failed sync that loses its writes while reads still
return them).

## Trie crate tests: node step N3a

`crates/arkiv-db-trie` has its own unit and integration tests (run with
`cargo test -p arkiv-db-trie --offline`, and as part of the workspace run).
They test the Merkle Patricia map on its own; the engine's use of it is
tested in `crates/arkiv-db/tests` (node step N3b, above). Oracles are independent of the crate: `BTreeMap` models,
`alloy_trie::HashBuilder` and `ProofRetainer` for keys of at most 32 bytes
(alloy's `Nibbles` holds no more), and a reference Merkle Patricia
construction in `tests/support/mod.rs`, written from the Ethereum yellow
paper with its own RLP and hex-prefix code and checked against
`HashBuilder`, for longer keys. Visit counts are hand-derived literals.

| File | What it checks | Evidence it gives |
| --- | --- | --- |
| `src/node.rs`, `src/nibbles.rs`, `src/order.rs` (unit) | Node encodings byte-equal to alloy's leaf, extension and branch encoders; 176-nibble paths; strict decoding (noncanonical RLP, hex-prefix padding, branch values, one-child branches, oversized inline children); 20,000 mutated inputs; alloy's own node decoder panicking on a 35-byte path; hex-prefix and padded-field vectors | The encoding is alloy's, extended to long paths, and decoding is total and canonical |
| [`map.rs`](../crates/arkiv-db-trie/tests/map.rs) | Random insert/replace/remove histories for key lengths 1, 2, 8, 21, 32, 33 and 88 bytes against a `BTreeMap` and the root oracles; permuted insertion orders with decoys; retained versions; crafted deep-path key sets; empty and single-entry maps; handle validation; empty values | Roots depend only on the key/value set; paths visit at most `2L + 1` nodes (reached exactly), removals at most one more; `insert` rejects a value with an empty encoding before any visit or read |
| [`proofs.rs`](../crates/arkiv-db-trie/tests/proofs.rs) | Proofs against `ProofRetainer` and the reference; `walk` against the model; the alloy `verify_proof` cross-check where valid; three forgeries alloy's verifier accepts; randomized mutations; self-consistent adversarial tries and garbage; keys of the wrong length | The strict verifier is sound on these inputs and never panics; alloy's verifier alone is not sound; a key of the wrong length is never proven included |
| [`store.rs`](../crates/arkiv-db-trie/tests/store.rs) | Staging against the reference object set; reopening from a root; lazy reads, updates and restaging; missing, corrupt and failing objects; a host error type; a caching loader; nested maps, including a sink shared with a map of another value type; the per-update creation bounds; hash-valid noncanonical tries | Stored maps load lazily and fallibly, visit counts do not depend on residency, staging emits every object the store lacks and nothing outside the new trie (nested maps too, whatever the sink saw first), and an update creates at most `2L + 2` nodes (reached exactly) |
| [`encoding.rs`](../crates/arkiv-db-trie/tests/encoding.rs) | Padded and composite fixed-width encodings against Rust slice and tuple ordering | The key encoders preserve order and are unique |

See [the state trie notes](state-trie.md) for the claims these establish
and what remains for steps N3c–N3d.

## Node library tests: node step N0b-3

`crates/arkiv-node/tests/` (run with `cargo test -p arkiv-node --offline`,
and as part of the workspace run) tests the block format, the executor,
static validation, selection, request identity and replay of the
[block format](block-format.md#part-2-block-format-version-1). The oracles
follow the rules above: golden byte strings are assembled from the
specification in the test, digests are `keccak256` from
`alloy-primitives` over hand-built preimages (never the codec's `digest`),
receipts are derived by hand in comments or predicted by an oracle with
its own transcription of the charge table, and query answers come from a
full-scan evaluator over the oracle's record model. Golden block hashes
are recorded literals: they pin determinism (any changed byte of a
header, input or outcome changes them), not correctness. The tests share
builders in `tests/support/mod.rs`; because this crate's `clippy.toml`
bans the engine's infallible methods, they read the memory engine through
its `try_` forms.

| File | What it checks | Evidence it gives |
| --- | --- | --- |
| [`block_format.rs`](../crates/arkiv-node/tests/block_format.rs) | Golden vectors for `ChainParams`, `Genesis` (404 bytes), both header versions (124 and 156 bytes), `AdminAction`/`AdminInput` (309 bytes = `MAX_ADMIN_BYTES`), `Expiry`, all nine `TxOp` shapes (every tag, expiry present and absent, permanent), `TxEnvelope`, empty and full `BlockInputs`, every `AdminRejection`, `AdminOutcome`, `ExpiryFault`, `HostFault`, `OpOutcome` and `TxStatus` variant, `TxOutcome`, `ExpiryDeletion`, empty and full `BlockOutcomes`, `Block`, `Scope`, `RequestIdentity`, `Inclusion`; each decodes back, re-encodes identically, and is refused with a trailing byte and at every truncation. The block hash under both header domains, the inputs, outcomes, transaction, admin and genesis digests and the genesis header and hash against `keccak256` over hand-built preimages. Refused: format versions 0, 2 and `u32::MAX`, protocol versions 0, 2 and 3, header versions 0, 3 and `u32::MAX`, a v1 header with a root and a v2 header without one, a 30-class schedule in a genesis, unknown tags of every enum (0 and one past the last), an expiry option byte of 2, hostile counts, a zero record id and an invalid name nested in a block | Block format version 1 is exactly the documented layouts and hashes, canonical and strict |
| [`replay.rs`](../crates/arkiv-node/tests/replay.rs) | The 14 fixtures listed in the [block format](block-format.md#replay-fixtures): multi-operation transaction, rollback with receipts kept, exact and one-short budgets, same-block transfer, expiry at the boundary (present at E−1, gone at E), expiry at capacity with 8-cell 32-byte-key records in three namespaces under an exact reservation, a ×100 read pricing change with expiry completing, permanent and extend-only expiry, unresolvable expiries, empty blocks, duplicate and already-included identities and statically invalid, mis-selected, misplaced or tampered blocks refused by replay, and the invariant and expiry-halt errors publishing nothing; plus the version-1 genesis header layout. Each produced chain is checked against hand-derived outcomes and states, replayed on a fresh engine to identical headers, round-tripped through the codec and pinned by golden block hashes. The expected transaction spends are literals, and block 3 of the boundary fixture carries transactions that find the due records already deleted (so a sweep run after the transactions fails it) | The executor's semantics on hand-checkable cases, and deterministic replay |
| [`block_model.rs`](../crates/arkiv-node/tests/block_model.rs) | 4 seeds × 60 generated blocks (records of at most 5 cells, 16-byte values, pages of 2, expiry capacity 3): random and scripted transactions (mid-transaction failures, same-block transfers, creations past a height's capacity, a patch keeping its full height), exact and one-short budgets, and admin schedules that apply, are stale, break the reservation or overflow. An independent oracle — its own weight tables, the model-3 charge groups with their semantic check points, a `BTreeMap` model of namespaces, raw entries and records, term newness from the live records, its own due schedule, expiry resolution, extend-only and capacity rules, and the reservation arithmetic — predicts every block's complete `BlockOutcomes` before it is produced. After every block, and again for every height after the run on both the original chain and a full replay: the pricing in force, every descriptor, raw map and record (including absent keys), a catalogue of eight queries (every operator, negation, AND, OR, match-all) walked with cursors and counted, and the host-state due schedule entry by entry. Coverage counters require every admin outcome, expiry deletions and a full-capacity sweep, a mid-transaction failure, a committed exact budget, an expiry-only patch, `RolledBack`, `NotExecuted`, the rejections `OutOfBudget`, `Unauthorized`, `UnknownNamespace`, `AlreadyExists`, `NotFound` and `StaleRevision`, every host fault except `Overflow` (unreachable with this `max_lifetime`), and every applied effect; no `AdmissionRejected` ever appears. As a manual check during development (not an automated test), seven deliberate executor bugs (capacity off by one, no `[old == E]` term, non-strict shortening, ignored `min_lifetime`, a dead-on-arrival bound off by one, no `RolledBack` marking, deletions not unscheduled) were each caught by this file or by the fixtures | The executor matches the brief's semantics on generated histories, and history and replay agree with the model at every height |
| [`exec.rs`](../crates/arkiv-node/tests/exec.rs) | Planted host-state entries: a due count without its records, a total disagreeing with its counts, a total without counts, malformed values and keys (`ScheduleInconsistent`); later entries and nonexistent namespaces left alone; a record created through the host path without a due entry whose user deletion underflows; a `$expiresAt` field or string (`Invariant::MalformedExpiryCell`); branches that are unbounded, used or from an engine with other limits (`Invariant::Branch`); the successor faults the executor decides (height, timestamp, chain id, header version; a wrong parent hash is refused by the chain and tested in `replay.rs`); identical outcomes at two timestamps; the host's expiry pre-read under a schedule whose pre-read costs exactly `u64::MAX` (applied, and deletions by a non-owner and by an owner out of budget execute), with a schedule one unit dearer `ReservationOverflow` | The failure paths discard the candidate, execution never reads the timestamp, and no accepted schedule can make the host's own read overflow |
| [`genesis.rs`](../crates/arkiv-node/tests/genesis.rs) | The deletion reserve (222), sweep bounds (155, 47, 1,013) and expiry pre-read cost (25) under hand-derived schedules; the exact reservation and its first violation; reservation overflow from deletions, from the read bound and from the pre-read (accepted at exactly `u64::MAX`, refused one unit above); every genesis invariant (including `max_cells`, `max_page`, `max_groups` and `max_literals` of at least 1) and version check | The genesis invariants and the reservation arithmetic |
| [`validation.rs`](../crates/arkiv-node/tests/validation.rs) | Envelope limits at their edges (chain id, budget 0 and above the user budget, 0 and 9 operations, 4,097 versus 4,096 bytes); `$` cells in creations, patch sets and remove lists; expiry pairs `(0, 0)` and an excessive `min_lifetime`; operations validated as they will execute (8 cells without and 7 with an expiry, expiry-only patches, the placeholder in a full patch); every engine argument fault; admin membership | Static validation is exactly the engine's argument validation plus the host's rules |
| [`selection.rs`](../crates/arkiv-node/tests/selection.rs) | Encoded sizes against hand-derived layout sizes; admin inputs first up to their limit; head-of-line stops on budget, operations and count; the byte limit at and around exact block sizes; a transaction of the largest valid shape (`max_tx_bytes` bytes, `max_ops_per_tx` operations, the whole user budget) fits beside a full admin section under three parameter sets at the invariants' edges; the replay check reports each limit | Selection and its replay check implement the documented formulas |
| [`identity.rs`](../crates/arkiv-node/tests/identity.rs) | Every transition: unknown → accepted, pending with the same or another digest, dropping, commit moving pending and never-pending identities to the inclusion index, included with the same or another digest, included identities never dropped; scope, chain, actor and request id each separating identities; identities in a discarded candidate staying pending; blocks repeating or reusing an identity refused without change; inclusions recorded by the memory chain | The request-identity state machine |

What these files do not establish: behaviour under storage errors (the
memory engine never fails, so `ExecError::Storage` is unexercised until
the persistent engine of N3b/N3c) or durable node recovery (N4).

The full/light demo adds these gates:

- `protocol_two.rs` and `demo_auth.rs`: root timing, strict authentication
  vectors, wrong signer/chain/root/linkage, equivocation, atomic candidate
  discard, body bounds, queue caps and request deduplication.
- `block_model.rs::continuous_protocol_two_200_blocks_match_independent_roots_outcomes_and_pages`:
  200 blocks, two independently executing full followers and two header-only
  light followers. An independent logical model predicts all outcomes and
  builds state roots with hand-encoded leaves and independent trie/RLP code;
  historical complete pages of sizes 1, 3 and 64 match its rows.
- `arkiv-db/tests/eq_proofs.rs`: independent proof fixtures, complete posting
  reconstruction, semantic forgeries, truncation, absence, historical expiry,
  rollback/key reuse, typed values, strict continuation and resource limits.
- `python3 scripts/test-full-light-demo.py`: separate processes, continuous
  catch-up, pause/late join/restart, historical comparison and adversarial
  transport. See [run instructions](full-light-demo-run.md).

These checks establish the development trust model, not public consensus
or durable recovery.

## Lessons from redb

Reviewed upstream redb at
[`32920ec63fcc97215f83a76617360a006f6784ab`](https://github.com/cberner/redb/tree/32920ec63fcc97215f83a76617360a006f6784ab).
These are behavioral ideas adapted to this database's API; no redb code or
dependency is included.

| Upstream test idea | Adaptation here |
| --- | --- |
| [Reference-model fuzzing](https://github.com/cberner/redb/blob/32920ec63fcc97215f83a76617360a006f6784ab/fuzz/fuzz_targets/fuzz_redb.rs#L508-L545) | Bounded deterministic programs against a mutation-log oracle, with repeated-key batches and binary keys; for queries, a full-scan evaluator over a `BTreeMap` model |
| [Shared-prefix ordering](https://github.com/cberner/redb/blob/32920ec63fcc97215f83a76617360a006f6784ab/tests/basic_tests.rs#L3330-L3394) | Binary prefix scans that include descendants and exclude immediate neighbors, including `0xff` boundaries; term runs for string prefixes and signed/unsigned ranges |
| [Overwrite behavior](https://github.com/cberner/redb/blob/32920ec63fcc97215f83a76617360a006f6784ab/tests/basic_tests.rs#L1574-L1600) | Same-size, larger, shorter, empty, and regrown values while preserving a neighboring key; patches that change a cell's value, kind or type |
| [Aborted work](https://github.com/cberner/redb/blob/32920ec63fcc97215f83a76617360a006f6784ab/tests/basic_tests.rs#L1544-L1571) | Dropping an unsubmitted batch leaves every entry unchanged; this is staging isolation, not transactional rollback |
| [Earlier views surviving overwrites](https://github.com/cberner/redb/blob/32920ec63fcc97215f83a76617360a006f6784ab/tests/basic_tests.rs#L1602-L1644) | Owned read/scan buffers remain independent in both directions in the raw facade; the engine's retained snapshots are the MVCC-style guarantee, tested in `history.rs` and `query_history.rs` |

## Planned proof and availability acceptance tests

Implement these alongside the remaining milestones in the
[snapshot/query plan](snapshot-query-plan.md) and the
[node/storage sequence](node-storage-plan.md#ordered-implementation-steps-and-exit-criteria),
using both the memory reference and the durable backend as it arrives.
The rows marked done are covered by the milestone-2 files above.

| Milestone | Required behavioral evidence | Status |
| --- | --- | --- |
| Namespace boundaries (indexes) | Identical attribute terms in different projects never mix postings, counts, negation domains, cursors, caches or proofs; unscoped input cannot trigger an implicit global scan | Done for postings, counts, negation domains and cursors; caches and proofs do not exist |
| Cross-namespace interference (per operator) | Extend the isolation test to each query operator and to proof overhead as it is implemented, with warm and cold caches; directory work and proof size obey a stated bound | Done for every operator; proof overhead and caches planned |
| Historical queries | Compare indexed queries to a full-scan reference at each snapshot, including type changes, expiry boundaries, key recreation and records absent from current indexes | Done |
| Pagination | Publish new states between pages; a full walk remains identical; reject changed snapshot/query/selection/order and invalid continuation | Done (order is fixed; continuation evidence comes with proofs) |
| Point proofs | Verify inclusion and absence against independently supplied historical roots; detect modified values, witness nodes and wrong roots | Planned (milestone 3, step N3d). The single-map proof format and the strict verifier exist in `arkiv-db-trie` and are tested there (`proofs.rs`); engine state roots exist and are checked against an independent model (`roots.rs`, N3b); engine point proofs do not exist |
| Complete query proofs | Independent verifier rejects omitted/extra matches, bad predicate evidence, wrong order/count, skipped page boundaries and false terminal pages; cover every supported operator and empty results | Planned (milestone 4) |
| Query/proof costs | Check index-term versus membership pricing, reduced projection work, incremental scan/proof budget checks and no successful partial proof after exhaustion; read receipts never become consensus gas receipts | Done for index-term pricing, projection work, incremental query budgets and read receipts; proof budgets planned |
| Availability | Unknown/pruned/not-yet-imported snapshots and unsupported proof requests return errors, never empty results or a head substitution | Planned (unknown snapshots are `UnknownSnapshot` today; pruning and import do not exist) |

Use existing SDK historical create/patch and exclusive-expiry expectations as
compatibility fixtures once the node's JSON adapter exists; the 32-byte key
encoding they need is settled (node step N0b-1). Unit mocks
do not prove node behavior or proof correctness; run host/RPC integration
cases when those components exist. No network endpoint is required for the
in-memory reference tests.

Extend the namespace harness to directory/proof bounds as those components
arrive. Owner filtering and correct results alone are insufficient evidence of
isolation; the namespace adversarial benchmark required by the source of
truth is implemented for logical work and charges and still needs a
wall-clock variant on shared hardware.

## Planned node and durable backend tests

The [node plan](node-storage-plan.md#acceptance-workloads) defines the additional
process workloads and failure matrix. Durability now comes before complete
query proofs. Add the following alongside node steps N2–N4:

1. Close and reopen after inserts, replacements, and deletes; compare all point
   reads and scans. See redb's
   [persistence test](https://github.com/cberner/redb/blob/32920ec63fcc97215f83a76617360a006f6784ab/tests/integration_tests.rs#L866-L901).
2. Inject failures around write, flush, and publication boundaries; reopening
   must expose no partial batch and preserve earlier durable data. See redb's
   [crash during a growing commit](https://github.com/cberner/redb/blob/32920ec63fcc97215f83a76617360a006f6784ab/tests/crash_consistency.rs#L128-L167).
3. Run the same logical operation streams against memory and durable backends,
   including reopen boundaries. The backend interface allows this without
   changing the expected KV semantics.
4. Inject lost acknowledgements and retry/reconcile by commit identity. Do not
   duplicate an already published commit or infer noncommit from a lost reply.
5. Check that data, historical indexes, roots/proof nodes and host publication
   marks all recover to a consistent snapshot. Pruning and snapshot import
   preserve every advertised historical answer and proof.

At the storage level, items 1 to 4 are covered by the N2 files above for
the versioned contract (reopen, injected failures and crash points, one suite
over the memory and redb stores, reconciliation by commit identity); the
engine and node versions of all five remain a backlog, not currently passing
guarantees.
Step N0a fixed the early-query-error loss of physical visit counters
(`trace_regression.rs`), so rejected reads now report every node they
visited. The counters still count in-memory tree nodes, not storage I/O,
which needs its own measurement. Node tests must also
cover manual/timer production through one writer, queue acceptance versus
durable inclusion, duplicate-request reconciliation, all-or-none expiry,
single-block pagination during publication, and reopen with a small cache.
Of these, node step N0b-3 covers the library parts in memory: all-or-none
expiry (a halted sweep publishes nothing), the request-identity state
machine without a queue, and deterministic replay of recorded blocks.
Measure backend recovery reads as well as application startup; successful
replay from a block log does not establish direct durable snapshot access.

## Name, key and index-value boundaries

`bounds.rs` exercises 20/21-character names, namespace-name history and
transfer preservation, rollback of named creation, 20/21-byte raw keys and
prefixes, and the original 8 MiB key regression. Oversized raw operations
reject with zero charges and zero physical visits on branches and snapshots.
It also checks 64/65-byte indexed strings (including multibyte UTF-8),
create/patch/host/estimate admission, equality/inequality/prefix/count/term
reads, and larger field payloads under independently configured limits.
