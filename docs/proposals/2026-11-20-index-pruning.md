# Proposal: prune state to the latest 256 blocks

Status: design only, not implemented (2026-11-20). Written against commit
`e77cf27`. Every `file:line` below refers to that commit. It refines phase 2
of [state history pruning](../pruning-design.md), which is still the
reference for the retention floor, pins and API errors (phases 0 and 1,
implemented). Where this proposal differs from that document, it says so.

Later changes to the index layout are out of scope. Changes to the store
layout are proposed only where pruning needs them.

## Summary

- **Mechanism.** Each object gets a persistent reference count from the
  head only, kept up to date in the block's commit transaction. When an
  object's count reaches zero it is queued with its *death epoch*. It is
  deleted, also inside a later commit transaction and within a per-block
  budget, once every retained snapshot and every open handle is at or
  above that epoch. This is the
  [phase 2](../pruning-design.md#physical-deletion-phase-2-designed-not-implemented)
  design with four changes:
  - **Sparse counts.** An absent count means one, so the common case reads
    no extra cold page.
  - **Bounded release.** The release cascade has a per-block budget and
    carries any overflow to later blocks.
  - **Epochs.** Deaths are tagged with a commit epoch, not a height, so
    rollback stays safe.
  - **Copy-migrate.** An existing archive is migrated by copying it, not
    swept in place.
- **Per-commit "freed node" lists alone are unsafe.** Objects are
  content-addressed and shared. A common case: the root of a one-member
  posting map is the same object for every term of that entity. A node
  that one commit replaced can still be live elsewhere, or come back in the
  next block.
- **Pruning does not fix the 650 GiB file.** In the Sourcify soak, 536 GiB
  of the 650 GiB were pages that redb could not release, because the
  metrics walk held a read transaction open. Stored data grew only
  4.5 GiB over the last 40 minutes. Removing that walk comes first. Pruning
  then removes the growth that is real history, roughly 20 KiB per
  operation (estimate below).
- **A 256-block window costs more than state reads.** Receipts, logs,
  `logsBloom` and so `eth_getBlockByNumber` are rebuilt from the state at
  `h - 1`. With a 256-block window they disappear after 64 s at 250 ms
  blocks. Before 256 becomes a default, receipts must be stored at commit
  time, or that loss must be accepted explicitly.
- **Nothing in consensus depends on pruning.** Counts, queues and floors
  are local to the node and never hashed. A bug in garbage collection (GC)
  surfaces as a storage availability error, never as a different root or
  charge.

## The problem, measured

| Run | File | Stored in tables | Cause of the rest |
| --- | --- | --- | --- |
| Node soak (`2026-11-19-node-soak.md`, Fragmentation) | 146.0 GiB | 40.4 GiB | 57.6 GiB free pages never reused, about 20 GiB B-tree slack, about 27 GiB allocated in no table |
| Sourcify soak (`2026-11-19-sourcify-soak.md`) | 650.0 GiB | 62.4 GiB | 536 GiB allocated in no table: pages freed by commits, held by the metrics walk's read transaction |

What history costs, separately from page pinning:

- In the Sourcify soak, stored bytes went from 57.9 GiB (minute 36) to
  62.4 GiB at the stop (minute 76), over about 220,000 operations: about
  152,000 `reverify` patches at 105 per second, plus 67,968 creates. That
  is about **21 KiB of retained growth per operation**. It includes the
  live data the creates added, so the history share is lower. This is an
  estimate from two samples, not a measurement of superseded nodes.
- At 100 operations per second that is about 7 GiB per hour, or 170 GiB per
  day. An archive node keeps all of it.
- A window of 256 blocks at 250 ms is 64 s, about 6,400 operations at that
  rate. The history inside the window is therefore at most about 130 MiB
  (6,400 × 21 KiB), small next to 60 GiB of live state.
- The phase 0 reachability report on a synthetic churn workload
  ([pruning-design.md](../pruning-design.md#reachability-report-phase-0))
  kept 7% of the objects with a 128-snapshot window. That workload is not
  representative of Sourcify, so the reachability report should be rerun
  on a copy of a soak file (see the test plan).

So the expected steady state of a pruned `node.redb` is live data, plus one
window of garbage, plus B-tree slack (about a third on hash-keyed tables),
plus a few commits' worth of freed pages. For the Sourcify state that is
roughly 100 to 120 GiB. That figure is an estimate: in-table bytes were
112 GiB at the stop, stored data plus slack plus metadata.

## 1. What is versioned today, and what can be deleted

### How old versions are kept

- **Objects** are immutable and content-addressed: the id is the
  `keccak256` of the bytes (`crates/arkiv-db-storage/src/versioned.rs:5-11`,
  `:115-121`). A commit "puts every object if absent"
  (`versioned.rs:284-286`). The redb store implements this as get, then
  insert when absent (`crates/arkiv-db-redb/src/store.rs:566-578`). Nothing
  ever deletes an object. `relabel` is the only operation that removes
  anything, and it removes records only (`versioned.rs:27-28`).
- **Spaces.** Each space is its own redb table: `objects_catalog`,
  `objects_host`, and `objects_ns_<16 hex>` per namespace
  (`store.rs:53-61`).
- **What a commit writes.** It stages only the nodes that an update built
  in memory. It stops at stored links and at nodes decoded from the store
  (`crates/arkiv-db-trie/src/map.rs:500-507`;
  `crates/arkiv-db/src/durable/prepare.rs:113-156`). So a block writes the
  new path from each changed leaf to the root. The previous versions of
  those path nodes stay in the table, unchanged.
- **Roots** of maps are always stored, whatever their size (`map.rs:501`).
  Other nodes shorter than 32 bytes are inlined into their parent
  (`crates/arkiv-db-trie/src/node.rs:5-8`) and are not separate objects.
- **Nested maps** are referenced by handles of 40 bytes (`root ‖ len`)
  inside leaf values (`crates/arkiv-db/src/layout.rs:422-435`):
  - a catalog leaf (`NamespaceState`) holds the roots of `records`, `keys`,
    `rows` and `terms` (`layout.rs:572-583`);
  - a term leaf (`Posting`) holds the root of its posting map
    (`layout.rs:442-460`), whose entries are `RecordId → 01`
    (`layout.rs:406-420`).
- **How an old version is reached.** Through the manifest of its sequence
  (`Table::Manifests`, key = sequence), which holds the catalog root and the
  host root. Manifests are written for every commit
  (`crates/arkiv-db/src/durable/mod.rs:359-373`) and are never deleted. No
  other reference to an old version exists.
- **The decoded-node cache** is keyed by `(space, map kind, id)` and never
  invalidated, because content addressing makes every entry valid for every
  snapshot (`crates/arkiv-db/src/durable/cache.rs:4-8`).
- **Records** (`Meta`, `Blocks`, `Manifests`, `Inclusions`, `BlockIndex`)
  are insert-only. `Blocks` in `node.redb` holds the certificate, the body
  reference and the feed JSON, about 84 KB per block
  (`crates/arkiv-node/src/signed/host.rs:34-38`; 9.1 GiB in the node soak).
- **The retention floor** already exists. It is
  `max(old floor, head - W)`, written as `engine/state-floor/<seq>` in the
  same batch as the manifest (`mod.rs:876-885`) and read on open
  (`mod.rs:642-651`). Snapshot handles pin their sequence before the floor
  check (`mod.rs:762-765`, `crates/arkiv-db/src/durable/retention.rs:143-154`).
  Today the floor refuses reads but deletes nothing.

### What may be deleted once a version is below the floor

With `W = 256`, the floor is `F = head - 256`. Snapshots `F..=head` stay
readable: the head and the 256 before it, 257 states in all, so rollback can
go 256 blocks deep.

| Data | Delete? | Why |
| --- | --- | --- |
| Trie objects reachable from no snapshot in `[F, head]` and from no open handle | **yes** | the only large item: about 98% of the file is the object tables |
| Manifests, block index, floor records | no | about 500 B per height. `read_open` reads manifest 0 (`mod.rs:328`), and `read_manifest` checks the parent's index entry (`mod.rs:277-285`) |
| `Blocks` records (certificate, body reference, feed) | not in this proposal | about 84 KB per block, so not small. See [history.redb](#historyredb-keep-for-now) |
| `Inclusions` | no | replay and duplicate detection |
| `history.redb` bodies | no, for now | see below |

**The exact deletion condition.** Let `D = min(durable floor, lowest pinned
snapshot)`. An object may be deleted when no snapshot at or above `D`
reaches it. `D` uses the floor that is already durable, not the one being
written, as in the existing design.

### Why "freed node" lists alone are not enough

The obvious cheap design: at commit time, record the stored nodes this
commit replaced, then delete them when the commit falls below the floor.
It would delete live data, because one object can be reached from many
places:

- **One-member postings.** The root of a posting map with one member `r`
  is a leaf node `[hp(r's 16 nibbles), 01]` of about 12 bytes. Roots are
  stored whatever their size (`map.rs:501`). Every term whose posting is
  exactly `{r}` points to this same object. An entity with two
  unique-valued attributes (Sourcify's `verifiedat` and its contract id,
  for example) therefore shares that root between two terms. A patch that
  moves `verifiedat` from `v1` to `v2` removes the `{r}` posting under `v1`
  (replaced) and adds one under `v2` (the same bytes). A freed list would
  delete an object that the new head still uses. This follows from reading
  the encoders, not from a measurement.
- **Identical subtrees and leaves** appear in any two maps with the same
  suffix structure. Equal bytes can even be a plain leaf in one map and a
  nested-root holder in another (`crates/arkiv-db-trie/src/reach.rs:18-21`).
- **Resurrection.** A later commit can recreate bytes that an earlier
  commit dropped. Put-if-absent then writes nothing (`store.rs:571-573`).

A freed list says which nodes left one position. It does not say whether
any other reference remains. Something must count, or walk.

### Options

| Option | How garbage is found | Exact with sharing? | Cost per block | Main drawback |
| --- | --- | --- | --- | --- |
| A. Freed-node journal | nodes replaced by commit `h`, deleted at `h + W` | **no** | very low | deletes live shared nodes |
| B. Stale-node index with version-keyed nodes (Jellyfish/Aptos style) | each node stored once per `(version, path)`; a commit lists what became stale | yes, because nothing is shared | low | changes the storage key of every node. Parents embed only hashes (`node.rs:5-8`), so a hash lookup needs an extra index. It is a layout change and drops deduplication |
| C. Head-only reference counts with a death queue | counts are updated in the commit transaction; zero means dead at epoch `e`; delete when `D ≥ e` | yes | proportional to the block's new and dead objects | one more table and some writes per object; invariant to maintain |
| D. Periodic mark-and-sweep over `[F, head]` | walk 257 roots (shared subtrees once), then sweep the table | yes | O(live state) per pass | hours per pass on 16.8 M entities: the 106 GiB adoption read took about 50 min at 73 MB/s (`2026-11-19-sourcify-soak.md`) |
| E. Copying GC (copy reachable state to a new file every `N` blocks) | the copy *is* the mark | yes | O(live state) per pass, plus twice the disk | same cost as D, plus the disk |
| F. Birth generation tags on objects | delete objects born before `F`… | **no** | low | birth says nothing about liveness: an old node can be live at the head |

**Recommendation: C**, which is the existing phase 2 choice. Option D stays
as the offline verifier, and the one-time migration is a single pass of E.
Option B is worth revisiting only together with a layout redesign.

### The recommended mechanism in detail

**Edges.** The children of an object are a pure, versioned function of
`(space, bytes)` (`ObjectGraph` v1, as in phase 2):

- the hash children of a branch or extension, in the same space;
- for a catalog leaf, the four namespace map roots, in that namespace's
  space;
- in a namespace space, every leaf value of 40 bytes that decodes as a
  non-empty `root ‖ len` handle, which is a *speculative* edge.

A `Posting` cannot be told from another 40-byte value by its bytes
(`layout.rs:442-460`). Over-approximating an edge only delays deletion.

**Counts are taken from the head only.** The count of `O` is the number of
edges into `O` from live objects, plus one if `O` is a root of the head
manifest. Snapshots below the head are protected by death epochs, not by
counts. So the cost per commit does not depend on `W`.

**Steps of the commit transaction**, all in the redb write transaction that
already moves the head (`store.rs:506-540`):

1. **Acquire.** For every staged object, check whether it is present (the
   lookup already done at `store.rs:571`):
   - **Absent:** insert it. It is born live with count 1. Increment its
     children.
   - **Present and live:** increment its count. Do not cascade.
   - **Present and dead** (in the dead set): resurrect it. Remove it from
     the queue, set count 1, and increment its children if they were already
     released.

   Then increment the roots of the new manifest.
2. **Release.** Decrement the roots of the old head manifest. When a count
   reaches 0, mark the object `Dead(e)`, where `e` is this commit's epoch.
   Push it to the release cascade, which decrements its children in turn.
   All increments run before decrements, so a moved subtree never touches
   zero. Sibling +1/-1 pairs on the same id cancel in memory before
   anything is written.
3. **Bound the release.** The cascade runs eagerly up to a budget of
   decoded nodes. Overflow is persisted as "dead, children not yet
   released" and continues in later commits. A child released late gets
   the later epoch, which is conservative: it was unreachable from the head
   since the earlier epoch. This bounds the cost of dropping a huge
   subtree, such as removing a namespace or a term whose posting has
   9.2 M members (the `arkiv.schema` posting in the Sourcify soak).
4. **Delete.** Pop death queue entries with `e ≤ D_epoch`, within a
   per-block delete budget. Delete each entry's object and its GC rows.
   Objects resurrected meanwhile have already left the queue.
5. Record the new head, the floor and the GC cursor in the same
   transaction.

**Why deletion is safe.** If `O` died at epoch `e` and is still dead, no
snapshot in `[e, head]` reaches `O`. Every object reachable from the head
is live, by the count invariant. `D` is at most the lowest pin, and
`snapshot()` registers its pin before it checks the floor
(`mod.rs:742-765`). So a reader that has passed the check holds `D` at or
below its own snapshot.

**Epochs instead of heights.** Epoch is a store commit counter that is never
reused. It is recorded with each manifest in a small table
`gc_epochs: height → epoch` that holds only the retained heights. Pins
carry the epoch of their snapshot. Heights would be enough without
rollback. After a rewind, though, heights repeat, and a handle still open
on an orphaned height `y` could have `D ≥ h` for an object that died at a
reused height `h ≤ y`. Using epochs from the start avoids a format change
later.

**Physical layout.** Option (iii) is the recommended one; measure before
deciding.

- **(i) A dense `gc_refs` table**, `space ‖ id → (count, state, epoch)`,
  as in phase 2. It is simple. But each new and each dead object needs a
  lookup in a second table keyed by random hashes. On a state larger than
  the page cache, those lookups are cold reads. The node soak spent about
  300 ms per block in backend reads once the file outgrew the cache.
- **(ii) The count stored in the object value.** It is co-located with
  bytes that are usually just read. But every object read path, the hash
  check (`store.rs:1046-1054`), adopt and bulk genesis would all change,
  and migration would rewrite every object.
- **(iii) Sparse counts, recommended:**
  - `gc_shared: space ‖ id → count` only for counts of 2 or more (absent
    means 1);
  - `gc_dead: space ‖ id → (epoch, released)` for dead objects not yet
    deleted (about one window of garbage, so small and hot);
  - `gc_queue: epoch ‖ space ‖ id → ()`;
  - `store/gc/state`: the graph version, the cursor and a `Suspended`
    flag.

  The common case is a new unshared object (the absent check is already
  done) or an unshared object dying (one lookup in `gc_shared`, then
  `gc_dead`). Neither reads a new cold table. The size of `gc_shared` is
  not known: one-member posting roots may put tens of millions of entries
  in it on Sourcify. The test plan measures it.

**Estimated cost per block.** For each new object: about one count write.
For each dead object: one decode (usually a cache hit, because the update
just walked it), one count lookup, one dead-set insert, and later one
delete. In steady state, deaths per block are about equal to births. Writes
to the object tables would therefore roughly double: deletes dirty random
leaf pages much as inserts do. Today a patch writes about 730 KiB of
copy-on-write pages, and the mean commit grew from 104 to 208 ms over the
Sourcify run. These are estimates, to be measured in count-only mode first
(see the plan).

## 2. How pruning runs

**Where: inside the block commit, not in the background.**

- Counts must describe exactly the durable head. Updating them in the same
  redb transaction as the head (`store.rs:506-510`, `Durability::Immediate`
  with quick repair) makes them atomic with it. A background counter would
  need its own durable cursor, a recovery path and a diff walk over cold
  pages.
- redb has one writer. A background GC transaction would delay the next
  block's commit anyway, and each durable transaction costs its own syncs.
- The alternative kept in reserve is a trailing collector. It computes the
  changes of commit `g+1` by diffing the roots of `g` and `g+1` (both still
  retained), in separate transactions committed with `Durability::None`
  and made durable by the next block commit. redb keeps the freed pages of
  non-durable commits apart (`redb-4.3.0/src/transactions.rs:2266-2285`).
  How soon those pages can be reused is **not verified**. Use this only if
  in-commit GC costs too much on large states.

**Bounded work per block.** There are two budgets, both node options, not
consensus:

- `release_budget`: decoded nodes in the release cascade, for example
  4 × the block's staged objects plus 4,096.
- `delete_budget`: queue entries deleted, for example 2 × the block's
  deaths plus 4,096.

Leftover work stays queued. Empty blocks drain the backlog. Expiry bursts
(up to `expiry_per_block` entity deletions per block,
[expiry spill](expiry-spill.md)) and namespace deletions are smoothed the
same way. The backlog is exported as a metric. A backlog that keeps growing
means the budget is too small, not that the result is wrong.

**Crash safety.**

- Every GC row changes in the same redb transaction as the head. A crash
  before commit leaves the old head and old counts. A crash after leaves
  both new. redb quick repair restores the allocator state.
- An `Indeterminate` commit is resolved as today, by reading the head back
  (`mod.rs:963-1003`). The GC rows follow whichever head is durable.
  `recover_pending` compares batch records only. GC rows are not batch
  records, so it is unaffected.
- Deletion is idempotent: delete if present.
- **Invariant failure.** If a count would go negative, or a live object is
  missing, the commit aborts its GC work. It retries without GC under the
  same writer lock and marks GC `Suspended`. Production continues, the
  file grows, and an operator runs `gc rebuild` offline. This is the phase
  2 rule, kept.

**Store contract.** `VersionedStore` objects are no longer immutable
forever (`versioned.rs:5-11`). The proposal:

- add a GC extension: the commit carries the old and new root sets and the
  deletion horizon `D_epoch`, and the store applies the steps above with
  an `ObjectGraph` edge function given to it at open;
- the memory store implements the same extension, so the storage contract
  suite (`crates/arkiv-db-storage/src/testing/contract.rs`) covers both;
- a store format gate (`store/gc/state`) makes binaries without GC refuse a
  GC file, and refuses archive retention on it.

**Determinism.**

- The state root, manifests, block hashes, receipts and charges are all
  computed before the store sees the batch. None of them reads a GC row.
- Execution reads only the head (and estimates read their own base). The
  head is always fully live.
- Nodes of one chain may run different windows and different budgets, and
  may hold different physical files. This is already the case for the
  floor (`retention.rs:40-43`).
- Suppose a GC bug deletes a node that the head still needs. The next read
  of it fails with `Missing` or `Corrupt`, which is an availability error
  ("must never be read as a missing record, an empty result or a
  deterministic user rejection", `versioned.rs:24-25`). The node stops.
  It does not diverge.

One condition must hold and should be asserted in tests: every stored link
in a candidate is reachable from its base, the head. Staging skips nodes
decoded from the store (`map.rs:501-503`), so a candidate that referenced a
stored node no longer live would commit a dangling link. No path that does
this is known (per-operation rollback restores clones of head maps). This
is **not verified** across all host paths.

## 3. Interaction with redb space reuse

**How redb frees pages.** A page freed by commit `N` returns to the
allocator only during a later durable commit, and only once every read
transaction older than `N` has ended:
`process_data_freed_pages_after_commit` frees up to
`oldest_local_referenced_transaction()`
(`redb-4.3.0/src/transactions.rs:2207-2220`, `durable_commit` at
`:2095-2106`). A deletion therefore lowers the file's growth only if no
long read transaction is open.

**Who holds read transactions in this code:**

| Site | Duration | Effect |
| --- | --- | --- |
| `read_object_bounded` (`store.rs:1026`), `head` (`:1067`), `read_record_bounded` (`:1088`) | one call | none. Each object read opens and drops its own transaction |
| `prepare` at open (`store.rs:947`) | short | none |
| `data_stats` (`store.rs:355-372`), called by `Metrics::refresh` (`crates/arkiv-node/src/signed/metrics.rs:283-286`) every interval | the whole file: 23 min on 146 GiB, more than 60 min on 650 GiB | **pins every page freed meanwhile**. This was the cause of the 536 GiB |
| `layout` (`store.rs:433-475`) | the directory, then an aborted write transaction **under the writer lock** (`:434`) | pins pages, and blocks commits while it runs. CLI only today |

Consequences:

- Engine-level pins and redb transactions are independent. A long query or
  a historical cursor holds an engine pin, which delays *deletion*, but no
  redb transaction, so it does not delay *page reuse*. The Sourcify soak
  record says "Any long read … pins freed pages the same way". By this code
  that holds only for `data_stats` and `layout`, not for queries. It has
  not been checked at runtime.
- **Before any pruning**, the periodic `data_stats` call must leave the
  default exporter. Splitting the walk by table does not help, because one
  object table holds 98% of the data and redb's `stats()` cannot stop
  partway. Replace it with counters that cost nothing:
  - file size and allocated blocks (`stat`, already sampled at
    `metrics.rs:287-301`);
  - live, dead and queued objects and bytes, kept by GC as part of each
    commit;
  - `data_stats` only on demand or offline.
- With short reads and in-commit deletion, the pages freed at commit `N` are
  reused from about commit `N+1`. File size should then plateau at:
  - live data;
  - plus one window of garbage;
  - plus slack (B-tree fill and `gc_*` tables);
  - plus the backlog.
- The file never shrinks by itself. redb truncates only trailing free
  space, and only when that space is at least half of the last region
  (`redb-4.3.0/src/tree_store/page_store/page_manager.rs:2305-2324`).
  Free pages in the middle of the file are reused, not returned to the
  filesystem.

**Already bloated files** (146 GiB, 650 GiB) need a rewrite:

| Option | How | Cost |
| --- | --- | --- |
| `Database::compact` (`redb-4.3.0/src/db.rs:1127`) | offline, in place, needs `&mut Database` with no read transactions or savepoints (`db.rs:1147-1155`). Uses two-phase commit; redb's own comment says cancelling it needs a full repair (`db.rs:1174-1176`) | moves pages repeatedly. Time on 650 GiB not measured. Needs no extra disk |
| **Copy-migrate** (recommended) | copy the records and the objects reachable from `[H - W, H]` into a fresh `node.redb`, seed GC counts during the same walk, then rename atomically, as adoption does (`crates/arkiv-node/src/genesis_adopt.rs:1-23`) | one read of the live state (about 50 min for 106 GiB at 73 MB/s, scaled from adoption) and one sequential write of about the live size. Needs free disk for the new file |

Seeding counts already requires a full walk of the head, so the walk can
write the compact file at the same cost. Copy-migrate is therefore the
migration path, and `compact` is the fallback when disk is short.

## 4. Effects on readers and other consumers

| Consumer | Effect with `W = 256` (64 s at 250 ms) | Change needed |
| --- | --- | --- |
| Historical state reads (`eth_getBalance`, `eth_getTransactionCount`, `eth_call`, `arkiv_getEntityCount` at a tag; signed REST at `?height`) | served for `[F, head]`, refused below with `-32006` or HTTP 410 (`crates/arkiv-node/src/retention.rs:1-60`) | none. Phase 1 already refuses |
| Proofs (`accounts/{a}/proof`, `query/eq`, `query/range`) and light verified reads | the same window. A light client must request a proof within 64 s of the height it verifies | none in code. Document the horizon for light-client and SDK users |
| `arkiv_query` with `atBlock` and cursors | the cursor reopens the snapshot on every page and is refused once the height falls below the floor. Paging through a large result at a fixed block must finish within about 64 s | an open question, see below |
| **Receipts, logs, `logsBloom`, `eth_getBlockByNumber`/`ByHash`, `eth_getTransactionReceipt`, `eth_getLogs`, filter logs** | rebuilt from body `h` plus **state `h-1`** (`crates/arkiv-node/src/signed/rpc/logs.rs:96-100`), so refused below `F + 1`. A transaction receipt older than about 64 s is gone, and so is the block with its bloom | **store per-block logs and the bloom at commit**: derived data, written beside the block record, which makes receipts independent of state retention. The feed already stores a `changes` list (`crates/arkiv-node/src/simulator/wire.rs:225-237`). Whether it is enough to rebuild logs is **not verified** |
| WebSocket `logs`/`newHeads` replay | a replay that falls below the floor ends with a `-32006` notification (phase 1) | none, or solved by stored logs |
| Transaction-hash and block-hash index | built from `history.redb` bodies, not from state (`crates/arkiv-node/src/signed/rpc/mod.rs:46-62`), so not affected | none for pruning. See restart |
| Restart | `rpc::initial_state` (`rpc/mod.rs:66-79`) reads and authenticates every block from 0 (`crates/arkiv-node/src/signed/host.rs:565-596`). That is O(chain): 614 s at 108,056 blocks, about 5.7 ms per block. The index is also capped at 1,000,000 entries (`rpc/mod.rs:33`), and the cap error is ignored. Pruning does not change this. Opening a GC file reads only the head's floor record and `store/gc/state`, with no walk | separate fix, required for "bounded" restarts: persist the index in `history.redb`, written with each body. Entries above the durable head of `node.redb` are ignored and overwritten |
| Metering (`crates/arkiv-db/src/meter.rs:4-11`) | charges count logical units and are "independent of … cache state … or the physical layout of storage". A historical read is priced with that snapshot's schedule (`docs/metering.md:444-447`). Pruning only decides whether a read is admitted. The floor check runs before any charge or storage access (`mod.rs:722-737`), and proof node counts depend only on trie shape, which pruning does not change | none. Add a differential test: same request, same receipt, archive against pruned, inside the window |
| Rollback or reorg (not implemented) | states `[F, head]` stay complete, so a rewind to `t ≥ F` is possible, up to 256 deep | see below |

### Rollback within the window

Neither chain rolls back yet. Today a commit must advance the head
(`versioned.rs:286-288`) and records are insert-only. A rewind therefore
needs a new store operation. For pruning:

- **Rewind to `t`:**
  - re-acquire the roots of manifest `t`, which resurrects dead objects;
  - release the old head's roots;
  - remove records for heights greater than `t`: manifests, blocks, block
    index, inclusions, and the `engine/state-floor/<seq>` records (these are
    keyed by height and would otherwise conflict on reuse).

  It all happens in one transaction with a new epoch.
- **Nothing needed by `t` has been deleted.** Every object reachable from
  `t` died at an epoch after `t`'s. And `D ≤ F ≤ t`.
- **The floor stays where it was**, because it never decreases
  (`retention.rs:138-142`). After a rewind of `k` blocks, the window is
  `256 - k` until the head catches up. A rewind deeper than 256 cannot
  happen by construction.
- **Open handles on orphaned heights** stay safe because deaths and pins
  are tagged with epochs (see above).

### history.redb: keep for now

Keep every body:

- Followers replicate and re-execute from genesis, and nothing else serves
  them. There is no state snapshot sync yet
  ([pruning-design.md](../pruning-design.md#phases), phase 3).
- `eth_getTransactionByHash`, transaction lookup by index, and the hash
  index rebuild all read bodies.
- It is not the problem. The node soak's `history.redb` was 14 GiB, of
  which 10.6 GiB stored, against 146 GiB for `node.redb`. It grows linearly
  (about 100 KB per block under that load), not with write amplification.

Body retention can come later, as its own floor that is made durable in
`node.redb` first. It needs snapshot sync for new nodes and a persisted
transaction index. Separately, the `Blocks` table in `node.redb` (84 KB per
block, mostly feed JSON) should move to `history.redb` or shrink. Then
`node.redb` is bounded by state alone (an open question).

## 5. Migration, configuration, metrics and validation

### Migration of archive databases

1. **Offline tool**, `arkiv-proposer state migrate-gc --window 256`, run
   with the node stopped:
   - Read head `H`. Walk the head with the typed walker of the
     reachability report (`crates/arkiv-db/src/durable/reachability.rs`,
     `MerkleMap::walk_stored` at `crates/arkiv-db-trie/src/reach.rs:22`).
     Count incoming edges, and write each object to a fresh file.
   - Then walk `H-W .. H-1`, skipping objects already seen. Copy the
     objects reached only from those snapshots as `Dead(epoch of H)`, so
     they are deleted once the floor passes `H`, 256 blocks later.
   - Copy every record, set the floor record to `max(durable floor, H-W)`,
     write `store/gc/state`, sync, and rename (the adoption pattern).
   - The old file is kept until the operator deletes it.
2. **New chains** seed counts at genesis:
   - the bulk and streaming builders and adoption already walk or verify
     every object (`crates/arkiv-db/src/durable/mod.rs:413-513`,
     `genesis_adopt.rs`), so they emit counts in the same pass;
   - a fresh empty genesis starts with counts from its first commit.
3. **No in-place sweep.** Phase 2's legacy sweep would delete objects
   without a count but leave a 650 GiB file. Copy-migrate replaces it.
4. **Disk.** The new file needs about the live size free (plus `Blocks`).
   When disk is short, compact first, or migrate onto another volume.

### Configuration

- **The engine** keeps a generic `Retention { Archive, Window(w) }`
  (`retention.rs:44-52`). `Window(0)` stays for golem-db conformance.
- **The node** keeps `--state-retention archive|N`, and adds a constant
  `ROLLBACK_DEPTH = 256`:
  - once rollback exists, producer and full roles on a GC file refuse
    `N < ROLLBACK_DEPTH`;
  - the default stays `archive` until stored receipts and copy-migrate
    land, then becomes `256` for producer and full roles;
  - archive stays an opt-in for explorers. An archive file has no GC
    tables.
- **Genesis**, later: when rollback depth becomes a protocol property
  (every producer must be able to rewind the same depth), move the minimum
  into genesis, as golem-db does with `#minRetention`. It is not a genesis
  field now: adding one changes the `EngineGenesis` identity and gains
  nothing until rollback exists.
- **GC itself** is a file property, set by create, adopt or migrate, not a
  flag. The budgets are node options with defaults.

### Metrics (Prometheus, no walks)

| Metric | Meaning |
| --- | --- |
| `state_floor`, `state_gc_horizon_epoch` (`D`), `state_lowest_pin_height`, `state_oldest_pin_age_seconds` | what bounds deletion. A stuck pin shows here |
| `state_gc_live_objects`, `state_gc_live_bytes` | maintained in the commit, so stored data is known without `data_stats` |
| `state_gc_dead_objects`, `state_gc_dead_bytes`, `state_gc_queue_oldest_epoch_age_blocks` | backlog |
| `state_gc_released_total`, `state_gc_deleted_total`, `state_gc_resurrected_total`, `state_gc_shared_entries` | activity, and the size of the sparse table |
| `state_gc_commit_seconds` (histogram), `state_gc_budget_exhausted_total` | cost per block |
| `state_gc_state` (`enabled`, `suspended`) | invariant failures |
| `db_file_bytes`, `db_allocated_bytes` | `stat` only (`metrics.rs:287-301`) |

### Test and soak plan

**Correctness**, mostly from phase 2 plus the new cases:

- Run the storage contract suite on the memory, redb and fault-injecting
  stores with GC fault points.
- In debug builds, check the count invariant after every commit (recount
  from the head).
- Run differential tests, archive against pruned: identical reads, queries,
  proofs and **receipts and charges** in `[F, head]`, and `HistoryPruned`
  below.
- Run property tests with tiny key domains, so that sharing and
  resurrection are common:
  - **one-member postings that move between terms**, the case from
    section 1;
  - a namespace or a large posting dropped with a small `release_budget`;
  - windows 0, 1, 2 and 256, and random pins.
- Inject a crash at every sync of commit, release, delete and migration.
  Also test indeterminate commits, `Suspended`, and `gc rebuild`.
- Test readers that pin between the pin and the floor check (existing
  hook, `mod.rs:751-760`). Once rewind exists, test it with handles open
  on orphaned heights.
- Run the offline verifier: a typed walk of every retained snapshot finds
  every object, and present objects minus reachable objects equals the
  dead set.

**Soaks** (rogue-one, Sourcify state, the harness in `scripts/soak/`):

1. **Baseline, no pruning.** Run with the periodic `data_stats` removed.
   Measure archive growth per operation without page pinning. Rerun the
   reachability report (`--recent 1,257,…`) on a copy of the stopped file,
   to get the real window size.
2. **Count-only.** Maintain counts but delete nothing. Measure commit time
   and write volume against step 1, and the size of `gc_shared`. This is
   the go/no-go for in-commit GC.
3. **Pruned, `W = 256`.** Run at least 2 h of `mutate-reverify` plus
   growth, with the readers phase on (4 callers and the 12 s `matchid`
   range query). Pass criteria:
   - after warm-up, file size and allocated bytes change by less than 2%
     over the last half of each mutate phase;
   - `state_gc_dead_objects` stays bounded;
   - `state_gc_live_bytes` matches an offline `data_stats` at the stop
     within slack;
   - block p95 and commit time stay within an agreed margin of step 1.
4. **Expiry-heavy and namespace-delete runs**: the budget smooths the
   bursts, and the backlog drains.
5. **SIGKILL during the run, then restart**: the verifier is clean, and
   restart time does not depend on chain length once the index is
   persisted.
6. **Migration** of copies of the 146 GiB (node soak) and 650 GiB
   (Sourcify) files: time, peak disk, resulting size, then step 3 on the
   result.

### Plan of work

0. Drop the periodic `data_stats` from the exporter and rerun the Sourcify
   soak (soak step 1). This is independent of pruning and stops most of the
   growth seen.
1. Store per-block logs and the bloom; persist the transaction and block
   hash index. Both are needed before a small window is useful to SDK users.
2. Add the GC extension to the store contract, with counts in count-only
   mode, the verifier and metrics (soak step 2).
3. Turn on deletion with budgets (soak steps 3 to 5).
4. Write copy-migrate (soak step 6). Then switch the producer and full
   default to 256.
5. Later: rewind, the genesis minimum, body retention, and snapshot sync.

## 6. Open questions and risks

- **Exact window semantics.** Does "the latest 256 blocks" mean `W = 256`
  (257 states, rollback 256 deep) or 256 states including the head
  (`W = 255`)? This proposal assumes `W = 256`.
- **Receipts.** Store logs (recommended), or accept that receipts expire
  after 64 s on pruned nodes and send SDK and explorer users to archive
  nodes? Is the feed's `changes` list enough to rebuild logs without state?
  (Not verified.)
- **Query pagination.** 64 s may be too short for paging large `atBlock`
  results. Options:
  - a time-based or larger window;
  - cursors that hold a pin, and so a lease, for a bounded time;
  - documenting the limit.
- **Commit cost** on states larger than RAM. Commit time is already up to
  208 ms against 250 ms blocks. If count-only mode adds more than about
  20%, use the trailing collector, or fold counts into object values
  (layout ii).
- **Size of `gc_shared`.** One-member posting roots may make sharing common
  on Sourcify-like data. If the table nears the size of the object table,
  layout (iii) loses its advantage over (i).
- **Speculative edges.** Every 40-byte leaf value in a namespace space that
  parses as a handle is followed. A value that only looks like a handle,
  for example a 40-byte blob, could pin an unrelated object forever. That
  is safe, but it leaks. How often it happens is unknown. The verifier
  should report objects that are present and unreachable but not dead.
- **Dangling-link invariant.** Is every stored link in a candidate
  reachable from its base? Needs a test or a debug assertion (section 2).
- **Stuck pins.** One handle that is never dropped blocks all deletion.
  Should the node force-expire pins older than some limit (the reader then
  fails with `HistoryPruned`) or only alert?
- **Disk for migration** of a 650 GiB file on a nearly full disk. Compact
  in place first, or use another volume. Neither has been timed.
- **`Blocks` in `node.redb`** (84 KB per block): move it to `history.redb`
  or trim it, so the state file is bounded by state alone?
- **Background or `Durability::None` GC transactions.** Not verified: when
  redb lets pages freed by non-durable commits be reused.
- **Format gates.** A GC file must be refused by older binaries, and an
  archive file must not be opened by a GC binary as if it had counts.
  Mistakes here lose data, so the gate must be tested in both directions.
- **Rollback design** is assumed, not agreed: a rewind store operation, the
  removal of records above `t`, and epochs. If rollback instead replays
  forward from a retained snapshot into a new file, epochs are not needed.
