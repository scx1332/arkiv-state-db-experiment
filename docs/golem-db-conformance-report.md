# golem-db conformance report

This report checks our engine against
[golemfactory/golem-db-conformance-testing](https://github.com/golemfactory/golem-db-conformance-testing).
That repository is a load harness written against golem-db's public
`golemdb-api` crate (the golem-db submodule is on `feature/golem-db-api` at
`95a24ea`). It is not a wire protocol. The harness's only calls are
`begin`, `create`, `patch`, `delete`, `seal`, `commit` and `discard` on the
in-process `Api` trait. It treats `HandleInvalid` and `Conflict` as a lost
race and treats any other error as fatal.

To run the harness on our engine, we wrote an adapter that implements the
real `golemdb_api::Api` trait over our engine. It is in
[`conformance/golem-db/`](../conformance/golem-db/README.md), together with
the harness patch, the scaled scenarios and the tests.

## Verdict

- **Every harness scenario runs on our engine with no errors**, on both the
  durable (redb) and memory backends. Those are 13 scenarios; the two
  `large/*` ones were scaled to 1/10.
- **The `golemdb-api` semantics can all be adapted** without a deep engine
  change: create, patch and delete, read targets, the branch lifecycle,
  first-committer-wins, checkpoint/rollback frames, seal/commit/discard,
  exact `BranchInfo::version`, golem's genesis YAML, opening modes and
  `OpenInfo`, and golem's storage-error mapping with retry after a failed
  call.
- **Ported golem API tests: 9 of 14 pass.** Every failure is either a
  golem-internal system record (`#alloc`, `#recordKeys`, `#roots`) or a
  read of an old commit, where golem returns `CommitUnavailable` and we
  keep history and return the data. All user-visible record, branch,
  opening and storage-failure behaviour passes.
- **Adapter tests: 6 of 6 pass.** One of them runs seeded random scripts
  on golem-db itself and on both of our backends and finds no difference
  in any call's outcome or `BranchInfo`.
- **Every golem cell type is native,** with golem's type ids and byte
  order, and every attribute type supports range queries.

## The adapter

`conformance/golem-db/src/arkiv.rs`; details in the kit's README.

| golem-db API | adapter |
|---|---|
| `Genesis` (YAML via `Genesis::from_yaml`/`load`) | Becomes the engine genesis. `max_cell_name_len`, `max_str_len` and `max_bytes_len` are the engine's `Limits`; `max_str_len` is also the indexed-string limit, because golem indexes `str` attributes up to it. Values above the engine's caps (32-byte names, 128-byte indexed values) and Blake3 are refused with `InvalidConfig`, not narrowed. The harness takes a YAML file through `GOLEMDB_GENESIS`. |
| Opening modes and errors | `CreateIfMissing`, `ExistingOnly`, `CreateNew`; `NotInitialized`, `AlreadyInitialized`, `GenesisMismatch` (checked before the mode, as in golem), `CorruptState`. |
| `OpenInfo` | `created`, the head commit, the engine state root, and the engine genesis id. Memory and durable databases of one genesis report the same `OpenInfo`. `index_root` is zeros (see below). |
| `BranchInfo::version` | golem's undo-entry count, computed exactly from the branch's write log when asked. No write reads a record for it. The one extra read left is a non-empty patch right after a rollback, where golem's frame rule depends on whether the patch changes anything. |
| Storage failures | A full medium is `StoreFull`; any other failure is `Internal` with the storage error as source; a poisoned lock is `Internal`. A failed call leaves the branch as it was, so the call can be retried: a branch the engine poisoned is rebuilt from its log after the durable engine recovers the store. A failed commit keeps its seal, can be retried or discarded, and does not block other branches. |
| Branch ids | Process-wide, as in golem's branch manager. |
| Harness `table_log` | redb tables per engine space, other tables, free file space, the object cache, and live record, term and snapshot counts at head. |

## Engine changes made for the conformance work

1. **Durable object cache: O(1) lookups**
   (`crates/arkiv-db/src/durable/cache.rs`). Every lookup used to scan the
   whole FIFO, and while the cache filled, `reserve_exact(1)` copied the
   whole ring on every insert. That made durable creates 3–6x slower than
   they needed to be. The fix puts a key → sequence index beside the FIFO.
2. **`Branch::try_state_root(&self)`.** It returns the root a seal would
   produce without consuming the branch and keeps the hashes for the seal.
   The adapter uses it to keep a sealed branch readable, as golem does.
3. **golem's full cell type grid** (`record.rs`, `codec.rs`). The engine's
   type id is golem's cell type id, and each type's term bytes equal golem's
   stored value bytes, so index order is golem's order. Encodings and golden
   vectors are in `docs/block-format.md` ("Value types and order").
4. **Empty patch is golem's no-op** (`typed.rs`): `NotFound` for a missing
   record; otherwise it charges the lookups and writes nothing.
5. **Ranges on every attribute type** in golem's byte order, with range
   proofs (`docs/range-proofs.md`).
6. **Genesis-configurable cell limits** (`Limits::max_cell_name_len`,
   `max_str_len`, `max_bytes_len`, `max_indexed_len`).

### Type mapping

The engine id equals golem's id for every golem type.

| golem type (id) | engine type | attribute | range queries | notes |
|---|---|---|---|---|
| `bool` (1) | `Bool` | yes | yes | `false < true` |
| `str` (2) | `Str` | yes | yes (and prefix) | UTF-8 bytewise; indexed length ≤ `max_indexed_len` (≤ 128 B) |
| `bytes` (3) | `Bytes` | no (field-only) | – | same rule as golem |
| `bytes20` (4) | `Addr` | yes | yes | golem's design names it Arkiv's `addr` |
| `bytes4/8/16/32` (8–11) | `Bytes4/8/16/32` | yes | yes | unsigned bytewise; engine `Key` (64) reads back as golem `bytes32` |
| `u32`–`u256` (12–15) | `U32`–`U256` | yes | yes | |
| `i32`–`i256` (16–19) | `I32`–`I256` | yes | yes | |
| `dec32`–`dec256` (20–23) | `Dec32`, `Dec64`, `Dec128`, `Dec` | yes | yes | scales 4, 6, 18, 18 |
| `f32` / `f64` (24, 25) | `F32` / `F64` | yes | yes | NaN rejected, −0 → +0, infinities kept, IEEE total order |
| `date32` (28) | `Date32` | yes | yes | days since epoch |
| `timestamp64` (29) | `Timestamp64` | yes | yes | µs since epoch |

golem's API has no queries, so "range queries" is our engine's query
subset.

## Benchmarks

All runs used tmpfs, so fsync cost almost nothing for either engine. Both
engines ran on the same machine. Treat cross-engine ratios as rough. Our
durable runs use the default object cache (8 MiB / 16384 entries).
`S` below is the session scratchpad.

### Runs of 2026-10-09 with the current adapter

Engine at `305d79e`; ops/s, with seal / commit p50 in ms. Logs:
`S/adapter-runs`.

| Scenario | arkiv durable | arkiv memory | golem-db (MDBX) |
|---|---|---|---|
| commit-growth | 5479 (0.18 / 1.31) | 26214 (0.21 / 0.001) | – |
| block-size/2 (100 ops) | 5161 (1.41 / 9.96) | 21128 (1.55 / 0.005) | – |
| concurrent-writers (8) | 2626 (2.19 / 13.2) | 15668 (2.96 / 0.013) | 1343 (35.6 / 23.8) |
| large-2m, 1/10 scale | 5661 (12.9 / 97.1) | 20838 (14.7 / 0.14) | – |

- **No cost from the adapter changes.** commit-growth earlier gave 5368,
  5477 and 5613 ops/s on durable.
- **`table_log` now works on both arkiv backends.** It runs between
  commits, outside the timers. Durable large-2m without `table_log` gave
  5973 ops/s. After the last commit the table log shows:
  `objects:namespace` with 1,711,346 objects in 551 MiB (B-tree height 4),
  catalog 0.08 MiB, other tables 0.13 MiB, 476 MiB free in a 1028 MiB file,
  and 200,000 live records with 209,711 index terms over 202 snapshots.

### Runs of 2026-10-09 before the adapter changes

Engine before the cell-type grid and the configurable limits. These are the
runs that found and checked the object-cache fix (logs `S/conf/runs` and
`S/final-runs`).

| Scenario | golem-db (MDBX) | arkiv durable, linear-scan cache | arkiv durable, indexed cache |
|---|---|---|---|
| index-layout/1-no-index | 3674 (177 / 99.5) | 9468 (4.6 / 25.2) | 23286 (4.7 / 25.7) |
| index-layout/2-same-term | 3702 (172 / 97.5) | 8691 (5.0 / 24.2) | 21369 (4.9 / 24.4) |
| index-layout/3-16-terms | 3689 (174 / 96.3) | 8176 (6.0 / 28.1) | 19921 (5.3 / 27.5) |
| index-layout/4-unique-terms | 2953 (219 / 125) | 7666 (7.0 / 40.8) | 16300 (6.2 / 38.1) |
| index-layout/5-16-attributes | 221 (1930 / 2714) | 2304 (35.1 / 193) | 2879 (33.6 / 184) |
| block-size/1 (10 ops) | 1170 (4.6 / 2.6) | 956 (0.26 / 2.2) | 2658 (0.26 / 2.2) |
| block-size/2 (100 ops) | 1432 (38.5 / 22.0) | 2114 (1.4 / 10.1) | 5377 (1.3 / 9.9) |
| block-size/3 (1000 ops) | 1120 (475 / 343) | 3813 (9.6 / 56.1) | 9003 (9.1 / 53.8) |
| block-size/4 (5000 ops) | 1458 (1873 / 1332) | 6053 (41.3 / 188) | 11607 (42.5 / 187) |
| commit-growth | 2878 (1.3 / 0.8) | 1843 (0.17 / 1.4) | 5449 (0.17 / 1.3) |
| concurrent-writers (8) | 1487 (35.1 / 23.9) | 387 (1.6 / 11.4) | 2879 (2.1 / 14.0) |
| large-2m, 1/10 scale | 961 (476 / 581) | 3109 (11.2 / 83.8) | 6743 (11.0 / 81.3) |
| large-realistic, 1/10 scale | 768 (563 / 737) | 1504 (20.7 / 115) | 3649 (21.1 / 115) |

- **Disk:** our durable database used 5–16x less disk than golem's MDBX
  file, even though we keep every snapshot.

### Concurrent writers: a semantics check only

Arkiv has a single writer, the block producer, so racing writers are not
an Arkiv workload. The scenario only checks that first-committer-wins
works: losers get `HandleInvalid` or `Conflict` and retry on the new head.
Both backends finish the scenario with no errors.

Attempts per commit in the runs above:

| | attempts | commits | attempts per commit |
|---|---|---|---|
| arkiv durable | 6304 | 788 | 8.0 |
| arkiv memory | 37606 | 4701 | 8.0 |
| golem-db | 1064 | 407 | 2.6 |

With W identical writers on one head, one commit wins each round and the
other W − 1 branches lose, so (W − 1)/W of attempts fail: 7/8 = 87.5% for
W = 8, which is 8 attempts per commit. That is what we measure. golem-db
loses fewer attempts (62%) because its commits are slower: seal p50 36 ms
and commit 24 ms against our 2 and 13 ms. Losing commits wait up to
seconds behind the writer lock (`commit lost` p50 251 ms), so the losers
stay alive across rounds instead of racing every round. Fewer branches
start per round, so fewer of them lose. The percentage says how the
writers interleave, not how good the engine is.

## Remaining differences

- **System records.** golem's `#alloc`, `#recordKeys` and `#roots` have no
  equivalent. `#params` and the other system keys are answered with their
  identity, and `#params` with the genesis limits.
- **History.** Old commits stay readable. golem returns `CommitUnavailable`.
- **Roots.** We have one state root with no separate index root, because
  the index is part of the namespace state. `SealInfo::index_root` and
  `OpenInfo::index_root` are zeros. Reporting the namespace's term-map root
  there would need a `Branch` accessor for namespace roots; `Snapshot` has
  one (`try_namespace_roots`).
- **Hash and limits.** Keccak only. Blake3 is refused at open, and so are
  limits above the engine's caps: names over 32 bytes, and `max_str_len`
  over 128 bytes, because golem indexes `str` attributes up to
  `max_str_len`. golem has no per-record cell limit; the engine's default
  of 64 cells applies.
- **`head()`** reads the adapter's copy of the head, so it cannot fail.
  golem reads storage, so a failing store fails `head()`.
- **Storage failure model.** The redb store latches an I/O or full-medium
  error and refuses every call until it recovers. The adapter recovers on
  the next call, so golem's retry contract holds. A commit whose outcome is
  indeterminate has no golem counterpart: the adapter resolves it at once
  if it can. If it cannot, the next call resolves it, and a commit found
  applied then makes the committing branch's retry a `Conflict`.
- **Engine branches across recovery.** After a store recovery, engine
  branches made before it cannot publish (the cache generation changed).
  The adapter replays each branch's log on the new generation, and keeps
  that log for the branch's lifetime, one copy of every applied write.
- **Adapter genesis commit.** The adapter's namespace is created by a
  commit after the engine's genesis, so golem commit N is engine snapshot
  N + 1. A crash between the two leaves a database that reopens as
  `CorruptState`. An engine-only prefilled genesis
  (`DurableEngine::initialize` with an initial state, which today only the
  simulator profile exposes) would make it atomic and remove the offset.
- **Not supported.** Immutable-data calls return `NotImplemented`, as in
  golem-db. `profile_at` needs `perf`; the scaled scenarios drop it.
- **No pruning.** Our disk use grows with history.
