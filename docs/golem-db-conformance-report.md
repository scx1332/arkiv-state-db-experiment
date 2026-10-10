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
run, engines interleaved. Medians of 5 runs; every arkiv/golem difference
below is outside the min–max spread. Full tables:
[golem vs arkiv](benchmarks/2026-10-10-golem-vs-arkiv-nvme.md) and
[decoded-node cache](benchmarks/2026-10-10-decoded-cache-nvme.md).

### arkiv durable (redb) vs golem-db (MDBX)

ops/s, with seal / commit p50 in ms; disk is the database size after the run.

| Scenario | golem-db | arkiv | arkiv / golem | disk golem → arkiv | peak RSS golem → arkiv |
|---|---|---|---|---|---|
| index-layout/1-no-index | 2,901 (172 / 191) | 16,913 (4.6 / 43) | 5.8x | 864 → 129 MiB | 969 → 104 MiB |
| index-layout/2-same-term | 2,833 (174 / 195) | 15,543 (4.8 / 44) | 5.5x | 864 → 129 MiB | 970 → 104 MiB |
| index-layout/3-16-terms | 2,822 (175 / 194) | 14,659 (5.0 / 48) | 5.2x | 864 → 129 MiB | 975 → 107 MiB |
| index-layout/4-unique-terms | 2,329 (213 / 243) | 11,910 (6.3 / 63) | 5.1x | 1,056 → 129 MiB | 1,186 → 136 MiB |
| index-layout/5-16-attributes | 307 (1,798 / 1,700) | 2,218 (39 / 277) | 7.2x | 8,224 → 514 MiB | 9,200 → 327 MiB |
| block-size/1 (10 ops) | 298 (4.7 / 29) | 943 (0.28 / 8.3) | 3.2x | 1,536 → 257 MiB | 2,291 → 244 MiB |
| block-size/2 (100 ops) | 731 (38 / 99) | 3,174 (1.5 / 22) | 4.3x | 1,376 → 257 MiB | 2,132 → 163 MiB |
| block-size/3 (1000 ops) | 1,355 (345 / 402) | 6,986 (9.5 / 85) | 5.2x | 1,456 → 129 MiB | 2,173 → 151 MiB |
| block-size/4 (5000 ops) | 1,853 (1,752 / 951) | 10,456 (43 / 233) | 5.6x | 1,776 → 129 MiB | 2,403 → 196 MiB |
| commit-growth | 975 (1.4 / 13) | 2,133 (0.18 / 6.4) | 2.2x | 768 → 257 MiB | 957 → 180 MiB |
| concurrent-writers (8) | 901 (36 / 92) | 2,149 (1.7 / 26) | 2.4x | 2,080 → 514 MiB | 2,891 → 738 MiB |
| large-2m, 1/10 scale | 1,044 (405 / 610) | 4,803 (11 / 134) | 4.6x | 6,720 → 1,028 MiB | 7,016 → 310 MiB |
| large-realistic, 1/10 scale | 841 (493 / 699) | 2,789 (18 / 191) | 3.3x | 6,688 → 1,028 MiB | 7,056 → 330 MiB |

- **Seal** is where the engines differ most: golem's seal grows steeply with
  block size and attribute count (1.8 s for 5000-op blocks or 16 attributes),
  ours stays at 39–43 ms.
- **commit-growth** is the closest: tiny commits are dominated by fsync for
  both engines.
- arkiv keeps every snapshot and still uses 3–16x less disk; it has no
  pruning, so disk grows with history.
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
