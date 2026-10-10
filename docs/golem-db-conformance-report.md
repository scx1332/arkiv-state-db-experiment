# golem-db conformance report

This report checks our engine against
[golemfactory/golem-db-conformance-testing](https://github.com/golemfactory/golem-db-conformance-testing).
That repository is a load harness written against golem-db's public
`golemdb-api` crate. We use golem-db `57c48ca`: `feature/golem-db-api`
rebased onto golem-db `main` `9dfd120`, which adds the one-pass trie update
at seal. The harness itself still pins the earlier `95a24ea`; the
`golemdb-api` crate is the same at both. It is not a wire protocol. The harness's only calls are
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
  keep history and return the data. With `ARKIV_STATE_RETENTION=0` (head
  only, see [pruning design](pruning-design.md)) the old-commit read is
  `CommitUnavailable` too and only the system-record divergences remain.
  All user-visible record, branch, opening and storage-failure behaviour
  passes.
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
| Harness `table_log` | redb tables per engine space, other tables, free file space, the decoded-node cache, and live record, term and snapshot counts at head. |

## Engine changes made for the conformance work

1. **Durable decoded-node cache** (`crates/arkiv-db/src/durable/cache.rs`).
   The harness exposed a linear scan in the old encoded-object cache. The
   cache now holds decoded trie nodes keyed by (space, map kind, object id),
   sharded, with CLOCK eviction and no lock held during storage reads, so
   readers run in parallel with the producer.
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

Measured on 2026-10-10 with `scripts/bench-compare.sh`
([docs/benchmarking.md](benchmarking.md)) on an idle machine: databases on
NVMe (ext4, fsync on), runs pinned to 8 cores, page cache dropped before each
run, engines interleaved. Medians of 3 runs; every difference below is
outside the min–max spread. Full tables:
[golem-db `95a24ea` / `57c48ca` vs arkiv `51acc60`](benchmarks/2026-10-10-golem-new-vs-arkiv-nvme.md),
the earlier [golem-db `95a24ea` vs arkiv](benchmarks/2026-10-10-golem-vs-arkiv-nvme.md)
and [decoded-node cache](benchmarks/2026-10-10-decoded-cache-nvme.md).

### arkiv durable (redb) vs golem-db (MDBX)

golem-db `57c48ca` against arkiv `51acc60`. ops/s, with seal / commit p50
in ms; disk is the database size after the run. The last column is golem-db's
own speedup from `95a24ea` to `57c48ca`.

| Scenario | golem-db | arkiv | arkiv / golem | disk golem → arkiv | peak RSS golem → arkiv | golem `57c48ca` / `95a24ea` |
|---|---|---|---|---|---|---|
| index-layout/1-no-index | 6,839 (88 / 65) | 17,444 (4.6 / 46) | 2.6x | 224 → 129 MiB | 270 → 108 MiB | 2.4x |
| index-layout/2-same-term | 6,864 (89 / 61) | 14,737 (5.0 / 51) | 2.1x | 224 → 129 MiB | 270 → 109 MiB | 2.4x |
| index-layout/3-16-terms | 6,706 (89 / 67) | 15,349 (5.1 / 46) | 2.3x | 240 → 129 MiB | 274 → 111 MiB | 2.4x |
| index-layout/4-unique-terms | 5,530 (111 / 76) | 11,760 (6.5 / 65) | 2.1x | 288 → 129 MiB | 336 → 140 MiB | 2.3x |
| index-layout/5-16-attributes | 820 (868 / 434) | 2,217 (40 / 273) | 2.7x | 1,792 → 514 MiB | 2,078 → 330 MiB | 2.7x |
| block-size/1 (10 ops) | 571 (3.7 / 13) | 806 (0.28 / 8.7) | 1.4x | 704 → 257 MiB | 821 → 246 MiB | 2.1x |
| block-size/2 (100 ops) | 1,591 (25 / 36) | 3,084 (1.4 / 23) | 1.9x | 448 → 257 MiB | 549 → 164 MiB | 2.1x |
| block-size/3 (1000 ops) | 3,506 (177 / 114) | 6,974 (9.9 / 86) | 2.0x | 304 → 129 MiB | 405 → 154 MiB | 2.6x |
| block-size/4 (5000 ops) | 5,937 (671 / 142) | 10,779 (44 / 227) | 1.8x | 240 → 129 MiB | 360 → 199 MiB | 3.1x |
| commit-growth | 1,515 (1.2 / 9.6) | 2,201 (0.18 / 6.4) | 1.5x | 288 → 257 MiB | 336 → 181 MiB | 1.7x |
| concurrent-writers (8) | 1,514 (23 / 49) | 2,816 (1.9 / 27) | 1.9x | 1,008 → 514 MiB | 1,195 → 577 MiB | 1.8x |
| large-2m, 1/10 scale | 2,273 (230 / 228) | 4,841 (11 / 137) | 2.1x | 2,144 → 1,028 MiB | 2,277 → 311 MiB | 2.2x |
| large-realistic, 1/10 scale | 1,904 (282 / 263) | 2,897 (18 / 199) | 1.5x | 1,968 → 1,028 MiB | 2,093 → 325 MiB | 2.3x |

- **golem-db got 1.7–3.1x faster** between `95a24ea` and `57c48ca`. The
  change is golem-db `main` `9dfd120` ("perf: update tries in one pass at
  seal"): each cell, index and bitmap trie is updated in one batch per seal
  instead of once per change, so far fewer intermediate trie nodes are
  hashed and written. Seal p50 fell 15–60%, commit p50 1.6–6.7x, and the
  database and peak RSS shrank 2–7x. The gain grows with block size.
- **arkiv is still faster in all 13 scenarios**, but by 1.4–2.7x instead
  of 2.4–7.2x. The smallest margins are tiny blocks (block-size/1,
  commit-growth), where fsync dominates for both, and large-realistic.
- **Seal** is still where the engines differ most: golem's seal is
  7–20x ours (671 ms against 44 ms for 5000-op blocks). Our seal only
  computes the state root; the writes happen in commit. For 5000-op blocks
  golem's commit p50 (142 ms) is now below ours (227 ms), but seal plus
  commit is 813 ms against 271 ms.
- **Disk:** golem-db is now 1.1–3.5x larger than arkiv, down from 3–16x.
  arkiv keeps every snapshot and still uses less; it has no pruning, so its
  disk grows with history. golem-db keeps only the head (history is
  deferred) but does not yet reclaim obsolete trie nodes; garbage
  collection for them is on a separate branch
  (`fix/trie-garbage-collection`) that is not part of this build.
- golem-db ran with the harness defaults; we did not tune it.

### Decoded-node cache (`1e1d4d7` → `774d3ef`)

Medians of 3 interleaved runs, same setup.

| Workload | before | after | change |
|---|---|---|---|
| index-layout/4, ops/s | 11,024 | 11,867 | +8% |
| commit-growth, ops/s | 1,683 | 1,738 | +3% |
| large-2m 1/10, ops/s | 4,695 | 4,800 | +2% |
| producer + 1 reader, reads/s (100-op blocks) | 9,728 | 33,303 | 3.4x |
| producer + 4 readers, reads/s | 15,819 | 99,761 | 6.3x |
| producer + 8 readers, reads/s | 17,104 | 133,996 | 7.8x |
| producer ops/s with 8 readers | 1,429 | 2,070 | +45% |
| read p99 with 4 readers | 577 µs | 96 µs | 6x lower |

Write throughput is bound by fsync and barely moves; reads alongside the
single producer scale with threads and no longer slow the producer down.
A larger cache does not pay off for these workloads: 32 and 128 MiB were
2–6% slower than the 8 MiB default on large-2m, so the default stays
8 MiB / 16,384 entries.

### Concurrent writers: a semantics check only

Arkiv has a single writer, the block producer, so racing writers are not
an Arkiv workload. The scenario only checks that first-committer-wins
works: losers get `HandleInvalid` or `Conflict` and retry on the new head.
Both backends finish the scenario with no errors.

Attempts per commit (tmpfs runs of 2026-10-09):

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
- **History.** Under the default archive retention old commits stay
  readable where golem returns `CommitUnavailable`. A retention window
  (`ARKIV_STATE_RETENTION=<N>`) refuses commits below `head - N` with
  `CommitUnavailable`; `0` matches golem.
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
- **No physical pruning yet.** A retention window refuses old reads, but
  nothing is deleted, so disk use still grows with history (see
  [pruning design](pruning-design.md)).
