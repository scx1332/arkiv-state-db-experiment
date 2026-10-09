# golem-db conformance report (2026-10-09)

This report checks our engine against
[golemfactory/golem-db-conformance-testing](https://github.com/golemfactory/golem-db-conformance-testing).
That repository is a load harness written against golem-db's public
`golemdb-api` crate (the golem-db submodule is on `feature/golem-db-api` at
`95a24ea`). It is not a wire protocol. The harness's only calls are
`begin`, `create`, `patch`, `delete`, `seal`, `commit` and `discard` on the
in-process `Api` trait. It treats `HandleInvalid` and `Conflict` as a lost
race and treats any other error as fatal.

To run the harness on our engine, we wrote an adapter that implements the
real `golemdb_api::Api` trait over our `Engine`. It is in
[`conformance/golem-db/`](../conformance/golem-db/README.md), together with
the harness patch, the scaled scenarios and the ported golem API tests.

## Verdict

- **Every harness scenario runs on our engine with no errors**, on both the
  durable (redb) and memory backends. Those are 13 scenarios; the two
  `large/*` ones were scaled to 1/10.
- **The `golemdb-api` semantics the harness uses can all be adapted.** That
  covers create, patch and delete, read targets, the branch lifecycle,
  first-committer-wins, checkpoint/rollback frames and seal/commit/discard.
  None of it needs a deep engine change.
- **Ported golem API tests: 4 pass and 5 fail.** Every failure is either
  a golem-internal system record (`#alloc`, `#recordKeys`, `#roots`) or a
  read of an old commit. golem returns `CommitUnavailable` for an old
  commit, while we keep history and return the data. All user-visible
  record and branch behaviour passes. Two adapter tests (every golem cell
  type round-trips; empty patch semantics) also pass.
- **Every golem cell type is native,** with golem's type ids and byte
  order, every attribute type supports range queries, golem's genesis
  `cell_limits` are the engine's `Limits`, and an empty patch is golem's
  no-op in the engine itself.
- **The run found one engine performance bug,** which is fixed here: the
  durable object cache did a linear scan on every lookup.

## Changes made in this repository

1. **Durable object cache: O(1) lookups**
   (`crates/arkiv-db/src/durable/cache.rs`).
   - **The bug:** every lookup scanned the whole FIFO, and while the cache
     filled, `reserve_exact(1)` copied the whole ring on every insert.
     Durable creates were 3–6x slower than they needed to be. A larger
     cache made it much worse, down to 49 ops/s with single creates taking
     up to 380 ms.
   - **The fix:** a key → sequence index sits beside the FIFO, and the FIFO
     grows geometrically up to the entry limit.
   - **Duplicate inserts** are skipped. Recovery clears the index, and a
     load is cached only if the cache generation is unchanged.
   - **Stats:** the new `CacheStats::index_bytes` reports the index's size.
2. **`Branch::try_state_root(&self)`** (`crates/arkiv-db/src/engine.rs`).
   - Returns the root a seal would produce, without consuming the branch.
   - The node hashes it computes are kept, so a later seal does not redo
     that work.
   - golem-db's API keeps a sealed branch readable. The adapter needs this
     call to provide that, and two ported checks went from failing to
     passing.
3. **The conformance kit** under `conformance/golem-db/`. It is not a
   workspace member.
4. **golem's full cell type grid** (`crates/arkiv-db/src/record.rs`,
   `codec.rs`). The engine's type id is golem's cell type id, and each
   type's term bytes equal golem's stored value bytes, so index order is
   golem's order. Encodings and golden vectors are in
   `docs/block-format.md` ("Value types and order"). The node's JSON forms
   are in `crates/arkiv-node/src/value_json.rs`.
5. **Empty patch is golem's no-op** (`crates/arkiv-db/src/typed.rs`).
   `NotFound` for a missing record; otherwise it is applied, charges the
   lookups and the patch base weight, writes nothing, and leaves the state
   root unchanged.
6. **Ranges on every attribute type** in golem's byte order, with range
   proofs (`docs/range-proofs.md`).
7. **Genesis-configurable cell limits** (`Limits::max_cell_name_len`,
   `max_str_len`, `max_bytes_len`, `max_indexed_len`, within the hard caps
   of 32-byte names and 128-byte indexed values). The adapter maps golem's
   genesis `cell_limits` onto them.

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
subset. The index order matches golem's bytewise attribute order for every
type, and fixed-width values are stored as golem's value bytes.

## Results (ops/s; seal / commit p50 in ms)

All runs used tmpfs, so fsync cost almost nothing for either engine, and
both engines ran on the same machine. Treat the cross-engine ratios as
rough. Our durable runs use the default object cache (8 MiB / 16384
entries). Raw logs: `S/conf/runs` for the unmodified engine and
`S/final-runs` after the fixes, where `S` is the scratchpad.

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
- **Concurrent writers:** about 87% of branch attempts lose the race on our
  engine, against 64% on golem-db.
- **With the type grid and empty-patch change** (durable, same setup):
  commit-growth 5613 ops/s (seal/commit p50 0.17 / 1.33 ms),
  block-size/2 4975, 5309 and 5407 ops/s over three runs (seal/commit p50
  1.31–1.40 / 9.7–10.6 ms). That is within run-to-run noise of the
  indexed-cache column. Logs: `S/types-runs`.
- **golem's own tests:** on golem-db itself, `cargo test -p golemdb-api`
  passes 15 tests and the integration `api` target passes 26.

## Remaining differences

- **Key type.** The engine-only `Key` type (id 64) has no golem
  counterpart; golem sees it as `bytes32`.
- **Hash.** We support Keccak only. golem can also use Blake3, which the
  harness never selects; the adapter refuses a Blake3 genesis.
- **Roots and system records.** We have one state root, with no separate
  index root. golem's `#alloc`, `#recordKeys` and `#roots` records have no
  equivalent. These are different designs, not bugs.
- **Durable engine performance.**
  - **No decoded-node cache:** every access re-decodes the stored trie
    nodes. A durable create costs 4–5x a memory create, and throughput
    drops as the database grows.
  - **One mutex for all reads:** it serializes every durable read and is
    held during the redb read itself, which limits concurrent writers.
- **No pruning.** Our disk use grows with history.
- **Harness tooling.** `table_log` reads MDBX statistics and `profile_at`
  needs perf, so neither applies to our backend. Both were skipped.
