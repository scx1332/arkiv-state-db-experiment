# golem-api: golem-db's API on the arkiv engine

This crate presents the public API of
[golem-db](https://github.com/Arkiv-Network/golemdb) (`golemdb-api`, `main`
at `f6dae1d`) on our engine (`crates/arkiv-db`). Code written against
golem-db, its conformance tests in particular, runs on our engine by
renaming one dependency:

```toml
golemdb-api = { package = "arkiv-golem-api", path = "<this repository>/golem-api" }
```

The library is named `golemdb_api`. Every type, trait and builder is
golem-db's own, re-exported from the real crate, so the caller compiles
unchanged. Only `Database` is defined here, with golem-db's constructors and
accessors.

golem-db has no network API; this is a Rust library like the one it mirrors.

## Where it sits

```text
golem-api ──> crates/arkiv-db, arkiv-db-redb, arkiv-db-storage   (path, unmodified)
          ──> golem-db: golemdb-api and five of its crates        (git, pinned revision)
          ──> redb                                                (its own immutable-data file)
```

Nothing in the node or in the engine crates depends on this crate, and it
is not a member of the node's workspace. It has its own `Cargo.lock`, its
own toolchain file (golem-db needs Rust 1.97; the node builds with 1.90) and
its own CI job. The reason is the golem-db dependency: it is a git
dependency that brings MDBX (C, built with clang) and a pinned commit of
`reth`. In the node's workspace it would enter the node's `Cargo.lock`,
break `cargo --offline --locked` and come under the node's `cargo deny` and
`cargo audit`. It lives outside `crates/`, which is what the node's image
build copies.

`Cargo.lock` is committed, so the golem-db revision and its dependencies are
pinned. CI builds without `--locked` on purpose: when an engine crate gains
a dependency, cargo adds it here and keeps every other pin.

## Using it

```rust
use golemdb_api::{Api, Database, OpenConfig};

let db = Database::open(path, &OpenConfig::new(genesis))?; // golem-db's call
```

`GOLEMDB_BACKEND` selects the engine behind `Database` at run time:

| Value | `open`, `open_with_options` | `open_memory` |
| --- | --- | --- |
| `arkiv` (default) | `DurableEngine` on redb, file `arkiv.redb` in the directory | in-memory `Engine` |
| `arkiv-memory` | in-memory `Engine` | in-memory `Engine` |
| `golemdb` | golem-db's own `Database` | golem-db's own |

The `golemdb` backend exists so that a suite can be run on golem-db through
this crate: it must give golem-db's own results, which shows the crate adds
nothing.

`ArkivDb` is the adapter itself, for callers that want its options
(`ArkivOptions`), its query statistics or its table statistics.

Other environment variables, read when a `Database` is opened:

- `ARKIV_STATE_RETENTION`: `archive` (default) or a number of commits the
  engine keeps state for besides the head.
- `ARKIV_SERVE_HISTORY=1`: serve `ReadTarget::Commit(n)` and
  `query(Some(n))` for retained commits other than the head. golem-db
  `main` serves the head only, which is the default here.
- `ARKIV_REDB_CACHE_MIB` (default 256), `ARKIV_OBJECT_CACHE_MIB` (8) and
  `ARKIV_OBJECT_CACHE_ENTRIES` (16,384): the store's page cache and the
  engine's object cache.
- `ARKIV_REDB_QUICK_REPAIR=0`: do not save redb's allocator state with
  every commit. For measurements only; the node's store keeps it on.

What these change is measured in the
[load benchmark results](../docs/golem-db-load-benchmark.md).

## What is exact, what is a stand-in, what is not supported

"Exact" means a caller cannot tell the difference through the API, apart
from the hash values themselves. The evidence is the test suites listed
under [Tests](#tests).

### Exact

- **Opening.** `open`, `open_with_options`, `open_memory`, the three open
  modes, `OpenInfo::{created, commit_id, genesis_id}`, `GenesisMismatch` for
  any changed genesis setting. `genesis_id` is golem-db's formula, computed
  here; it is also the engine's deployment id.
- **Records.** `create`, `get`, `patch` (`attribute`, `field`, `set`,
  `remove`, the no-op rule), `delete`; golem-db's order of checks and its
  admission messages (copied from `golemdb-record`); every cell type, with
  golem-db's type ids and value order; `#key` and `#meta`; the per-record
  caps.
- **Reserved records.** `#params`, `#alloc`, `#recordKeys`, `#rootIndex`,
  `@meteringModel` and `@modelWeight` read as on golem-db; writes are
  `Reserved`. They are derived on read: golem-db stores them as cells, the
  engine does not.
- **Branches.** `begin`, `checkpoint`, `rollback`, `seal`, `commit`,
  `discard`, `branch_info` with golem-db's `version`, first committer wins.
- **Receipts.** `priced_at` and the nine `details` counters.
- **Queries.** Every operator of `main` (`Eq`, `In`, `Range`, `Prefix`,
  `Exists`, `And`, `Or`), sort keys, `limit`, `offset`, `with_total`,
  projection, `count`, and golem-db's admission errors. See
  [Queries](#queries).
- **Immutable data.** All five calls, the genesis declaration and
  `immutable_data_directory`. See [Immutable data](#immutable-data).
- **Errors.** Every `ApiError` kind except the two listed below.
- **Pruning window.** `OpenConfig::pruning` is the engine's state retention
  window.

### Stand-ins

| What | What the crate returns | What a caller could notice |
| --- | --- | --- |
| `Receipt::cost` | 0 | wrong under any nonzero price |
| `Receipt::ledger` | empty | golem-db counts charged units even at zero prices |
| `metering_model`, `model_weights` | the genesis model | nothing while golem-db has no call that changes a model |
| `OpenInfo::index_root`, `SealInfo::index_root` | 32 zero bytes | golem-db's is the hash of the empty string at genesis and changes with the index |
| `#roots` | the state roots of the retained commits below the view, each with a zero index half | the index half; cells only for retained commits |
| `OpenInfo::state_root`, `SealInfo::state_root` | the engine's state root | other bytes than golem-db's, same role |
| `OpenConfig::host_schemas` | checked when a database is created | not checked on reopen |
| `MdbxOptions` | ignored | `max_map_size` does not cap the store |
| `shard_span`, segment `compression` | part of the genesis identity, no effect on storage | nothing through the API |

Metering is a deliberate stand-in: the engine has its own metering, and
golem-db's pricing model is not reproduced.

### Not supported

| What | What happens |
| --- | --- |
| `HashAlgorithm::Blake3` | `open` returns `InvalidConfig`. The engine hashes with keccak-256 only. |
| Budgets | never enforced: `OutOfBudget` is never returned. golem-db also returns it when a weight is missing from the genesis prices. |
| `Database::from_store` | takes one of golem-db's own key/value stores; `InvalidConfig` on the arkiv backends. |
| A store size cap | a full disk is `StoreFull`, but nothing can be configured to fill early. |
| Cell names over 32 bytes, `max_str_len` over 512 | `open` returns `InvalidConfig`. |

## Queries

A golem-db filter is a tree of `And` and `Or` over typed leaves. The
engine's query is an ordered flat DNF over the term index, in record-id
order with keyset cursors, and a separate count. A filter is translated to
one engine query; pages, counts and totals come from the engine.

| golem-db | Engine |
| --- | --- |
| `Eq` | `Term::Eq` |
| `In` | one group per value, each `Term::Eq` |
| `Range` | `Term::Range`; with both sides open, `Term::Exists` of the type |
| `Prefix` | `Term::StartsWith` |
| `Exists` with a type | `Term::Exists` |
| `Exists` with no type | one `Term::Exists` group for each of the 23 indexable types |
| `And` | the product of its children's groups |
| `Or` | the concatenation of its children's groups |
| no sort keys | the engine's order, record id ascending, which is creation order |
| `limit` | the engine page limit (golem-db's cap of 100 is checked first) |
| `offset` | an id-only engine walk past the offset, then `Snapshot::cursor_after` |
| `with_total`, `count` | the engine's `count` |
| projection | the engine's named projection; every cell when `#meta` is wanted |

Reads use the engine's planned evaluation and read-side bounds, as the
node's SDK reads do: at most 1024 groups, 256 literals in a group, pages of
16,384 rows.

What the engine cannot express, and what the crate does instead:

| Case | What happens | Result |
| --- | --- | --- |
| ORDER BY (any sort key) | The engine orders by record id only. The filter is still one engine query. The crate reads the key and the sort cells of every match, keeps the first `offset + limit` in golem-db's order in a bounded heap, then reads that window's rows by key. golem-db's own strategy (`FetchAndSort`) and the node's `ORDER BY` do the same. | exact |
| A filter whose DNF exceeds the bounds | Evaluated node by node: each subtree that fits is one engine query walked to its end; `And` and `Or` above it are set operations on record ids. An `In` with more than 1024 values is walked in chunks. | exact |
| A `str` range bound longer than `max_str_len` | The engine refuses such a bound, and it cannot be dropped (a long upper bound admits every shorter string). The leaf is answered by walking `Exists(name, str)` and comparing in the crate. | exact |

A leaf that can match no record is answered without the engine: an `In` with
no value, a range whose bounds admit nothing, an `Eq` value or a prefix
longer than `max_str_len`, a name the engine's name type cannot hold.

`ArkivDb::query_stats()` counts how queries were answered (`native`,
`empty`, `fallback`, `scans`, `sorted`), and `Translator::plan` shows the
engine query a filter becomes. The tests use both.

No ordering or limit difference is known. The page cap (100), the sort key
cap (4) and the depth cap (64) are golem-db's hardcoded values, checked here
in golem-db's order.

## Immutable data

Append-only typed rows in named segments, declared in the genesis. Rows are
staged on a branch, published with its commit, addressed by ordinal or by an
optional 32-byte key, and read with column projections.

### Storage

The rows live in a store of this crate's own: one redb file,
`immutable.redb`, in the immutable-data directory (`<database>/immutable-data`
unless `OpenConfig::immutable_data_directory` names another), or the same
tables in memory for `open_memory`. The engine never sees a row. Nothing
derived from a row is under the state root, and no file of the node is
touched or reused.

| Table | Key | Value |
| --- | --- | --- |
| `meta` | `"genesis"` | the genesis id the store belongs to |
| `rows` | segment id, ordinal | the row's optional key and its column payloads |
| `keys` | segment id, key | ordinal |
| `ranges` | segment id, commit | first and end ordinal of the rows that commit appended |

golem-db's own storage crate (`golemdb-immutable-data`) was not reused for
storage. Its files need golem-db's key/value store for their keys and commit
ranges, and the publication logic around them is private to golem-db's
branch manager, so reusing it would have meant running a second golem-db
store beside the engine. Its types, row checks, projection and errors are
reused, which is where behaviour is decided. The on-disk format is therefore
not golem-db's.

### Publication with a commit, and recovery

A commit writes two stores. The order is fixed:

1. The immutable store commits the staged rows as the rows of commit `N`.
2. The engine publishes commit `N`.

The engine's head is the only authority on what is published. Every read is
bounded by the head the reader observed, so rows of a commit the engine has
not published are invisible, whether they come from a crash between the two
steps or from a failed step 2. Such a tail is deleted before anything else
is written: on open, against the engine's head, and at the start of the next
publication in the same process. A branch whose commit failed keeps its
staged rows and can retry.

The opposite order would not be safe: the engine could hold a commit whose
rows were lost.

Before step 1 the engine's head is read again, after any recovery of the
engine's store: if an earlier publication with an unknown outcome did move
it, the committing branch has lost (`Conflict`) and nothing is written.

### Semantics followed

golem-db `main`'s actual behaviour, where it differs from the API spec
(`docs/golemdb-api.md`):

| Point | The spec says | `main` does, and so does this crate |
| --- | --- | --- |
| `rollback` and staged rows | rollback drops the rows the undone frame staged | staged rows are independent of frames; `immutable_data_truncate` removes a staged suffix |
| A repeated key | the newest row wins | `AlreadyExists`, for a key bound by a committed row or staged on the same branch |

The external harness marks its three tests of the spec's variant as ignored
spec gaps; they fail on golem-db too.

## Tests

Run with `cargo test` in this directory (Rust 1.97 from `rust-toolchain.toml`,
clang for MDBX). The first build fetches golem-db and its `reth` dependency.

| File | What it checks |
| --- | --- |
| `tests/differential.rs` | seeded random scripts of every record and branch call on golem-db's memory `Database` and on both arkiv backends: every result, error message, receipt detail and `BranchInfo` equal |
| `tests/queries.rs` | generated and hand-written queries against golem-db: pages, counts, totals, order, errors; which path answered; the shape of the translated engine query |
| `tests/immutable_data.rs` | golem-db's own API tests for immutable data, random scripts against golem-db, reopening, a crash between the two stores, a failed commit and its retry |
| `tests/adapter.rs` | storage failures, history and retention, genesis identity, and the stand-ins above, pinned |

The conformance suites of golem-db are run by
[`conformance/golem-db`](../conformance/golem-db/README.md).
