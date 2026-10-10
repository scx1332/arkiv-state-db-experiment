# State history pruning

This document describes how the durable engine and the nodes bound the state
history they serve and, later, store. Phases 0 (reachability report) and 1
(retention floor and API semantics) are implemented. Phase 2 (physical
deletion) is designed and not implemented: nothing is deleted today.

## Goals

- Keep the default unchanged: a node keeps and serves every snapshot
  (archive) unless an operator configures a window.
- Make retention a runtime choice of one node, so it can be tried without a
  new genesis. The policy type is shaped so it can move into genesis later
  (a minimum window shared by every node, golem-db's `#minRetention`).
- Strict, deterministic answers: with a window `W`, every read of state below
  `head - W` is refused with one explicit error, decided before any storage
  access, whether or not the data is still on disk. A read is never answered
  from the head, never empty, never `StorageCorrupt`.
- Bound disk use by retained state rather than by total history (phase 2).
- Keep everything that is small or needed to serve and replicate the chain:
  headers, certificates, block bodies, manifests, the block index and
  inclusion records. Only trie state is pruned.

## Comparison with golem-db

| | golem-db (`feature/golem-db-api` 57c48ca) | golem-db GC branch (`fix/trie-garbage-collection`) | golem-db spec | Arkiv (this design) |
| --- | --- | --- | --- | --- |
| History served | head only: `Commit(n)` succeeds iff `n` is the head, else `CommitUnavailable` | head only | every commit in `[head - #minRetention, head]`, identical on every node; beyond it `Pruned` on every node | `[floor, head]`, `floor = max(previous floor, head - W)`; archive serves all |
| Retention setting | none | none | `#minRetention`, genesis parameter (value open) | runtime `Retention { Archive, Window(W) }`; genesis later |
| Superseded trie nodes | never deleted (garbage: leaves are virtual, so old roots cannot be read) | persistent refcounts in the commit transaction (`NodeRefs`); a zero count deletes the node at once | GC of orphans at the window boundary | phase 2: persistent head-only refcounts, deletion deferred by death height |
| Readers of old state | MDBX read transactions only | same | — | pinned `Snapshot` handles |

With `Window(0)` Arkiv serves only the head, which is golem-db's behaviour; the
golem-db conformance kit runs in that mode with `ARKIV_STATE_RETENTION=0`.

## Retention floor (phase 1)

### Engine

`durable::Options::retention` is `Retention::Archive` (default) or
`Retention::Window(w)`. The engine keeps a **state floor**:

- At every commit the new floor is `max(old floor, sequence - w)` (saturating
  at zero; always zero under archive). With `Window(0)` the floor is the head.
- `DurableEngine::snapshot(id)` registers a pin on `id`, then checks the floor,
  then reads the manifest. `id < floor` is refused with
  `EngineError::HistoryPruned { requested, floor }` (displayed as
  `history pruned: requested N, floor F`) before any manifest, trie object or
  decoded-cache access; the pin is released with the error. `id > head` stays
  `UnknownSnapshot`. `check_retained(id)` is the same check without opening.
- `state_floor()`, `retention_status()` and `DurableStats::retention` expose
  the policy, the live floor, the durable floor, the lowest pinned sequence
  and the number of open handles.

**Durability.** A commit whose floor is above zero writes the insert-only
metadata record `engine/state-floor/<sequence as 16 hex digits>` (8-byte
big-endian floor) in the same atomic batch as its manifest. Opening reads the
record of the durable head. Archive commits write nothing, so an archive file
is unchanged by this feature.

**Monotonicity across restarts.**

- Opening with a smaller window raises the floor at once for reads. The raised
  floor becomes durable with the next commit.
- Opening with a larger window keeps the durable floor: history once declared
  pruned stays pruned, even though the data is still on disk in phase 1.
  Restarting before the next commit returns to the durable floor (never
  lower).
- Opening a file whose durable floor is above zero with `Archive` is refused
  (`Incompatible`: "state history below snapshot F is declared pruned; archive
  retention cannot be restored, configure a window"). A window file that never
  pruned (floor still zero) can switch back to archive.

**Pins.** Every `Snapshot` handle issued by the durable engine (the head, a
historical snapshot, a prepared publication) registers its sequence until its
last clone, including branches built on it, is dropped. Pins do not hold the
logical floor back, so answers depend only on head and window. A handle opened
before the floor passed it keeps reading until dropped (nothing is deleted in
phase 1, and phase 2 bounds deletion by the lowest pin). Cursors, filters and
subscriptions are not leases: they re-open their snapshot for every page or
step and are refused once it is below the floor.

**Golem-db adapter.** `ArkivOptions::retention` (harness and ported tests:
`ARKIV_STATE_RETENTION=archive|<N>`). `ReadTarget::Commit(n)` below the floor
returns `CommitUnavailable { requested, head }`, checked before any read; the
in-memory backend applies the same floor to its reads. `table_log` reports
`snapshots` as the retained count, head to floor.

### What is retained

| Data | Retained | Why |
| --- | --- | --- |
| Trie objects of snapshots `>= floor` | yes | served state |
| Trie objects only reachable below the floor | yes in phase 1; deleted in phase 2 | |
| Manifests, block index | all | about 500 B per height; `read_open` reads manifest 0, `read_manifest` checks the parent's index entry, inclusion lookups read the manifest at the inclusion height (producer hot path) |
| Headers, certificates, feed records (`Blocks`) | all | light bootstrap from genesis, `genesis` endpoint, replication |
| Transaction bodies (`history.redb`, simulator `Blocks`) | all | replication and re-execution of followers, transaction lookup, hash index rebuild |
| Inclusion records | all | duplicate and replay detection |

### Node consumers and errors

**Configuration.** `--state-retention archive|<N>` (default `archive`) on
`arkiv-proposer producer|full` and `arkiv-simulator producer|full|inspect`.
Light roles have no state and reject the flag, as does a malformed value.

**Two floors.**

- `stateFloor = F`: state at height `h` is served iff `F <= h <= head`.
- `receiptFloor`: logs, receipts and `logsBloom` of block `h > 0` are derived
  from body `h` plus state `h - 1` (entity nonces and previous owners), so
  they are served iff `h >= F + 1`. `receiptFloor` is `F + 1` when `F > 0`,
  else `0`. Block 0 has no logs and is always served. A receipt refusal
  reports the requested block height and the receipt floor.

Every consumer checks the floor before opening a snapshot (host helpers
`require_state`, `require_receipts`, `state`); an engine refusal is also
recorded as a backstop.

| Consumer | Rule |
| --- | --- |
| `eth_getBalance`, `eth_getTransactionCount`, `eth_call` `entityNonce(address)` at a height, `arkiv_getEntityCount` | state floor |
| `arkiv_query` with `atBlock` | state floor, checked on every page before the cursor lookup (a cursor is not a lease) |
| `eth_getLogs` | receipt floor, the whole range up front |
| `eth_getTransactionReceipt`, `eth_getBlockByNumber`, `eth_getBlockByHash`, `eth_getFilterLogs`, `eth_getFilterChanges` | receipt floor |
| WebSocket `logs`/`newHeads` replay | if the next height is refused, one `eth_subscription` notification carries `params.error` (code -32001) and the subscription is dropped |
| Signed REST `accounts/{a}[?height]`, `accounts/{a}/proof`, `query/eq`, `query/range`, `state/explore` | state floor |
| Simulator REST `query/eq` | state floor |
| Light verified, inspect and account endpoints | relay the full peer's refusal as `HistoryPruned` |
| Every height | `eth_getTransactionByHash`, `eth_getTransactionByBlockNumberAndIndex`, `eth_getBlockTransactionCountByNumber`, `eth_feeHistory`, `eth_getCode`, block filters, the hash index, headers, feed, genesis, replication, request status |

**Errors.**

- JSON-RPC: code `-32001`, message `history pruned: requested N, floor F`
  (decimal), `data: {"requested": "0x..", "floor": "0x.."}`.
- REST (signed and simulator): HTTP `410 Gone`,
  `{"error":"HistoryPruned","requestedHeight":"N","stateFloor":"F"}`. A light
  node relaying a peer's refusal answers `410 {"error":"HistoryPruned"}`.
- Engine: `EngineError::HistoryPruned`; golem-db adapter: `CommitUnavailable`.

**Status.** `GET /signed/v1/status` and the simulator status keep
`bodyRetention` (`all` on full nodes, `headers-only` on light nodes) and add
`stateRetention` (`archive` or `window`), `stateRetentionWindow` (`"N"` or
`null`), `stateFloor` and `receiptFloor` (decimal strings). All four are
`null` on light nodes, whose reads inherit the peer's retention.

## Reachability report (phase 0)

`durable::reachability::reachability(store, &[n...])` walks the head and then
older snapshots through their typed maps (catalog, then per namespace records,
keys, rows, terms and each term's posting map; host), visiting subtrees shared
between snapshots once (the visited set is keyed by space, map kind and id,
because equal bytes can be a plain leaf in one map and a nested-root holder in
another). For each requested `n` it reports per space the distinct objects and
bytes reachable from the union of the `n` newest snapshots. Levels are clamped
to the state floor recorded with the head. `DurableEngine::reachability`
does the same on an open engine, clamped to the live floor, and pins the
oldest walked snapshot for the duration of the walk. The underlying trie walk
is `MerkleMap::walk_stored`. Store totals come from the backend; the CLI
uses `RedbStore::layout`.

```sh
# On a copy of an engine, simulator or signed-node file:
cargo run --release --offline -p arkiv-db-redb --example reachability_report -- \
    node.redb --recent 1,16,128,1024
# On a generated churn workload:
cargo run --release --offline -p arkiv-db-redb --example reachability_report -- \
    --demo 2000 --recent 1,16,128,1024,100000
```

Measured on the demo workload (2,000 publications after a base block; each
rewrites 1 of 64 raw keys, creates one typed row, deletes the row from 32
blocks earlier, rewrites one host key; head 2001, archive; walk 0.7 s):

| space | stored objects | stored bytes | head only | last 16 | last 128 | last 1024 | all |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| catalog | 8,004 | 1,382,691 | 4 | 64 | 512 | 4,096 | 8,004 |
| host | 4,002 | 388,165 | 3 | 33 | 257 | 2,049 | 4,002 |
| namespace 1 (raw keys) | 8,059 | 1,727,967 | 85 | 145 | 593 | 4,177 | 8,059 |
| namespace 2 (typed rows) | 37,509 | 6,228,554 | 107 | 422 | 2,648 | 19,626 | 37,509 |
| **all objects** | **57,574** | **9,727,377** | **199 (0.3 %)** | **664 (1.2 %)** | **4,010 (7.0 %)** | **29,948 (52.0 %)** | **57,574 (100 %)** |
| reachable bytes (value + 32-byte key) | | | 27,240 | 103,681 | 665,809 | 5,050,313 | 9,727,377 |

The archive union equals the stored totals exactly (objects and key+value
bytes), which cross-checks the walker. A 128-snapshot window would keep 7 % of
the objects; about 28 objects per block are history. The checked-in rogue-one
files on this machine predate the current state format and cannot be opened.

## Physical deletion (phase 2, designed, not implemented)

Trie objects are content-addressed and shared: one object can be reached from
many snapshots, from several maps of one namespace and from different
positions, and a later commit can re-create bytes that were already deleted
("resurrection"). A commit stages only the nodes it created and references
older objects by hash. Deleting what an old snapshot reached therefore needs
proof that no retained snapshot and no open reader reaches it.

**Chosen mechanism: persistent head-only reference counts maintained in the
commit transaction, with deletion deferred by death height.**

- Counts track references from the head only: each manifest root acquired by
  the new head and released from the old head, plus edges from *active*
  (counted) parents. All increments run before decrements, so moved subtrees
  never touch zero. An object whose count reaches zero is marked `Dead(h)` at
  the commit height `h` and queued; its out-edges are released.
- Lemma: an object that died at `h` and stayed dead is not reachable from any
  snapshot in `[h, head]`. It is deleted once
  `D = min(durable floor, lowest pinned sequence) >= h`. `D` uses the floor
  that is **already durable**, not the one the same batch records, and the
  lowest pin, so no retained or pinned reader can reach a deleted object.
  That is why `snapshot()` pins before it checks the floor.
- Resurrection is safe by construction: a commit can only reference an
  inactive object by re-creating its bytes, which puts them in the batch. If
  the object was deleted, the commit re-inserts it (put-if-absent); if not, it
  is re-activated and leaves the death queue. Deletion and commits share the
  store's writer lock.
- Edges are a pure, versioned function of `(space, bytes)` (`ObjectGraph`
  v1), so increments and decrements stay symmetric: hash children in the same
  space; catalog leaves give strict edges to their four namespace map roots;
  in namespace spaces every 40-byte leaf value that is a valid non-empty
  `root || len` handle gives a speculative edge (a `Posting` cannot be told
  apart from other 40-byte values by bytes alone). Over-approximation only
  delays deletion.
- Storage in `node.redb` only: `gc_refs` (`space || id` → count, state, death
  height) and `gc_dead` (death-height queue), a `store/gc/state` marker with
  the graph version, and a store format gate so binaries that do not maintain
  counts refuse the file. `history.redb` is never GC-enabled.
- A count invariant violation does not halt production: the GC transaction is
  aborted and the commit is retried without GC under the same writer lock,
  marking GC `Suspended`; an operator rebuilds the counts.
- Offline tooling (node stopped): enable/rebuild (seed counts by a walk from
  the head, cost proportional to live state), verify, disable. An existing
  archive is migrated by seeding at head `H` and then a legacy sweep that
  deletes objects without a count once `D >= H`.
- In-place deletion lets redb reuse freed pages but does not shrink the file.
  Shrinking is a copy-migrate (copy records plus objects reachable from
  `[floor, head]` into a new file, seed counts, atomic rename) or one offline
  `Database::compact`.
- Mark-and-sweep is kept only as the verifier: a typed walk of every retained
  snapshot must find every object present, a recount from the head must equal
  the stored counts, and present-minus-reachable must be explained by the
  death queue.
- Estimated cost per block is proportional to the block's garbage: one decode
  and at most 16 count reads per new or dying node; sibling +1/-1 pairs cancel
  before writing.
- Test plan: the storage contract suite on memory, redb and the fault-injecting
  store with GC fault points; an exact-graph check after every commit;
  differential tests of an archive engine against a pruned one (identical
  reads, queries and proofs in `[floor, head]`, `HistoryPruned` below);
  property tests with tiny domains forcing sharing and resurrection, windows
  0, 1, 2 and 5, small delete budgets and random pins; crash injection at
  every sync of commit, prune, seed and sweep; indeterminate commits;
  suspension and rebuild; readers on pinned snapshots during deletion,
  including a delay between pin and floor check; migration of an archive;
  an expiry-heavy soak where allocated pages must plateau.

## Node roles

- **Archive** (default): every snapshot, body and header; serves any height,
  replication from genesis and, later, snapshot export.
- **Pruned full or producer**: `--state-retention <W>`. Serves state reads at
  `[floor, head]`, receipts and logs at `[floor + 1, head]`, bodies and
  headers at every height. Producers and full followers need only the head to
  execute, so a window does not affect block production or replay. The window
  should cover the longest expected query pagination, light-proof horizon,
  `eth_getLogs`/subscription replay and follower lag.
- **Light**: headers only, unaffected. Its verified reads need a full node
  whose floor is at or below the requested height.

## Phases

0. Reachability report (done).
1. Retention floor, pins, explicit errors and status advertisement (done; no
   data deleted).
2. Physical deletion of trie objects block by block (designed above).
3. Later: file shrink by copy-migrate or redb compaction; separate body and
   receipt retention with their own floors (advance the floor durably in
   `node.redb` first, then sweep `history.redb`; rebuild the hash index from
   the body floor); snapshot export and bootstrap so new nodes need no
   archive peer; moving the minimum window into genesis.
