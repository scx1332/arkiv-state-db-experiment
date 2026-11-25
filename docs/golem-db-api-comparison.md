# golem-db `main` API compared with our engine

Written 2026-11-25. It answers one question: what does it take to present
golem-db's API one to one, so that the test harness the golem-db team is
building runs against our implementation without large changes.

Earlier work: [golem-db conformance report](golem-db-conformance-report.md)
and the kit in [`conformance/golem-db/`](../conformance/golem-db/README.md).
Both describe golem-db as it was on 2026-10-09. This document describes
golem-db `main` as of 2026-11-24.

Each claim below is marked as **run** (observed in a build or test run made
for this document) or **read** (taken from source, not executed).

## Summary

- **The harness is golem-db's own `crates/conformance`.** Issue #140
  ("Epic: Conformance tests") moves the tests of
  `golem-db-conformance-testing/crates/conformance` into the golem-db
  repository. Eight draft pull requests (#157, #160, #163 to #167, #171)
  hold 108 tests so far. Confidence: high (read).
- **golem-db has no network API.** It is a Rust library. The harness calls
  the concrete type `golemdb_api::Database` in process. "One-to-one API on
  our node" therefore means a Rust crate, not something the node serves.
- **There is no backend plug-in point.** The tests name `Database` and call
  `Database::open` themselves. The way in that needs no upstream change is a
  drop-in crate with the library name `golemdb_api`: it re-exports golem-db's
  types and defines its own `Database` over our engine. The harness then
  changes its `golemdb-api` dependency line and nothing else.
- **A scratch version of that crate exists and was run.** Results on our
  engine, with no assertion changed:

  | Suite | golem-db `main` | our engine |
  | --- | --- | --- |
  | In-repo `crates/conformance` (PR stack tip `ef6269a`), 108 tests | 100 pass, 8 ignored | 94 pass, 6 fail, 8 ignored |
  | External harness `a1b6b6b`, spec suite, 199 tests | 179 pass, 5 fail, 15 ignored | 165 pass, 19 fail, 15 ignored |
  | External harness, SQLite oracle (random sequences) | 2 of 2 | 2 of 2; also at 1000 sequences |

  The external harness needed 18 call sites fixed to compile against
  golem-db `main` at all. The 5 failures on golem-db `main` are stale tests
  in it, and they fail the same way on our engine. So our engine fails 20
  tests that golem-db passes: 11 immutable data, 4 metering, 4 that need
  Blake3 or a separate index root, 1 store size cap.
- **Our kit and its CI are stale.** The pinned kit still passes (21 tests,
  16 pass, 5 expected failures; run). Against the newest revisions the
  harness patch does not apply and the adapter does not compile. The
  `golem-latest` CI job has been failing at setup, and the workflow shows
  green because the job is `continue-on-error`.
- **Of 70 API elements, 49 map exactly, 10 with a filler, 11 are missing**
  ([table](#gap-analysis)).

## Revisions

| What | Revision | Date |
| --- | --- | --- |
| Our repository | `604e398` (`main`, clean) | 2026-11-25 |
| golem-db `main` (`Arkiv-Network/golemdb`) | `f6dae1d` | 2026-11-24 |
| golem-db conformance PR stack, tip `ev/conformance-errors` | `ef6269a` (based on `f6dae1d`) | 2026-11-24 |
| External harness `golemfactory/golem-db-conformance-testing`, `develop` | `a1b6b6b` | 2026-11-19 |
| golem-db revision the external harness pins as submodule | `85b053c` | before `f6dae1d` by 16 commits |
| Kit pin, harness | `686ddfc` | 2026-10-08 |
| Kit pin, golem-db | `57c48ca` | 2026-10-09 |

Toolchain: Rust 1.97 (golem-db's `rust-toolchain.toml`). Our crates build
with it unchanged (run).

## Which harness is meant

Three candidates were checked through the GitHub API and the clones.

1. **golem-db `crates/conformance`, in draft pull requests.** Issue #140,
   opened 2026-11-18 by `v0-e`: "Tests verifying golemdb API matches the
   spec. Pull and update existing tests [link to the external harness's
   `crates/conformance`] here under a new crate". The stack is #157 (base
   crate), #160 (`create` and `get`), #163 (`patch` and `delete`), #164
   (receipts), #165 (data model), #166 (`RecordOp`), #167 (API shape), #171
   (errors). All are drafts, last updated 2026-11-24. Each test quotes the
   sentence of `docs/golemdb-api.md` it checks. The same author filed eight
   issues from it (#173 to #181) where code and spec disagree.
2. **The external harness** `golem-db-conformance-testing`. It has two
   crates: `crates/conformance` (spec tests and an SQLite oracle, 199 + 2
   tests) and `crates/harness` (the load tool behind `cargo load`). Its last
   commit is 2026-11-19. It does not compile against golem-db `main` (see
   [Evidence](#evidence)). The spec tests there are the source the in-repo
   crate is being ported from.
3. **Benches in golem-db `main`** (`crates/integration-tests/benches`:
   `write`, `index`, `query`). The write bench ("seal and commit through
   the public API", #53) calls `Database::from_store` with golem-db's own
   `MdbxStore` and reads its store statistics, and the index bench opens
   `MdbxStore` directly. They measure golem-db's storage layer and are not
   a harness for another implementation. Only the query bench goes through
   `Database::open` alone (read; none of them was run here).

Conclusion: candidate 1, which is candidate 2's spec suite moving house.
What is not yet known is whether the query, branch, opening, immutable-data
and oracle modules will be ported too. The issue says "pull and update
existing tests". So far the stack covers records, receipts, the data model,
errors and the API shape; it has one `count` call and no `query` call.

## golem-db's public API on `main`

Read from `crates/api` at `f6dae1d`. The crate `golemdb-api` is the whole
public surface; it re-exports types from nine internal crates
(`golemdb-cells`, `-record`, `-query`, `-branch`, `-metering`,
`-immutable-data`, `-pruning`, `-storage-mdbx`, `-merkle`).

There is no RPC server, no CLI and no binary target in the workspace. An
offline prune and compact tool is proposed in PR #116. Statistics exist only
on the internal `Store` trait (`golemdb-storage`), not in `golemdb-api`.

### Handle and trait

```rust
pub trait Api {
    fn create(&self, branch: BranchId, op: RecordOp<op::Create>) -> Metered<RecordKey>;
    fn get(&self, target: ReadTarget, op: RecordOp<op::Get>) -> Metered<Record>;
    fn patch(&self, branch: BranchId, op: RecordOp<op::Patch>) -> Metered<()>;
    fn delete(&self, branch: BranchId, op: RecordOp<op::Delete>) -> Metered<()>;
    fn query(&self, commit: Option<CommitId>, op: QueryOp) -> Metered<QueryPage>;
    fn count(&self, commit: Option<CommitId>, op: CountOp) -> Metered<u64>;
    fn metering_model(&self, commit: Option<CommitId>) -> Result<MeteringModelInfo>;
    fn model_weights(&self, commit: Option<CommitId>, version: Option<u64>) -> Result<ModelWeights>;
    fn head(&self) -> Result<CommitId>;
    fn begin(&self) -> Result<BranchId>;
    fn branch_info(&self, branch: BranchId) -> Result<BranchInfo>;
    fn checkpoint(&self, branch: BranchId) -> Result<()>;
    fn rollback(&self, branch: BranchId) -> Result<()>;
    fn seal(&self, branch: BranchId) -> Result<SealInfo>;
    fn commit(&self, branch: BranchId) -> Result<CommitId>;
    fn discard(&self, branch: BranchId) -> Result<()>;
    fn immutable_data_schema(&self, segment: &str) -> Result<ImmutableDataSchema>;
    fn immutable_data_append(&self, branch: BranchId, segment: &str,
        key: Option<ImmutableDataKey>, row: ImmutableDataRow) -> Result<ImmutableDataOrdinal>;
    fn immutable_data_truncate(&self, branch: BranchId, segment: &str,
        from_ordinal: ImmutableDataOrdinal) -> Result<()>;
    fn immutable_data_get(&self, segment: &str, address: ImmutableDataAddress,
        options: &ImmutableDataReadOptions) -> Result<ImmutableDataRow>;
    fn immutable_data_rows_of(&self, segment: &str, commit: CommitId,
        options: &ImmutableDataReadOptions) -> Result<Vec<ImmutableDataRow>>;
}
```

`Database` is a concrete, cloneable struct with no type parameters. Its
private field is an `Arc<dyn Api + Send + Sync>`, and there is no public
constructor that takes one. Constructors: `open(path, &OpenConfig)` (MDBX),
`open_with_options(path, &OpenConfig, MdbxOptions)`,
`open_memory(&Genesis)`, `from_store<S: Store>(store, &OpenConfig)`.
Accessors: `genesis()`, `info() -> &OpenInfo`. Clones share one branch
registry.

### Opening

- `Genesis { hash_function, cell_limits, record_limits, metering,
  immutable_data }`, with `new` (5 arguments), `from_yaml`, `load`. Unknown
  YAML fields are rejected; `metering` and `record_limits` are required.
- `HashAlgorithm`: `Keccak256`, `Blake3`.
- `CellLimits { max_cell_name_len, max_str_len, max_bytes_len }`,
  `RecordLimits { max_cells, max_indexed_cells }`.
- `OpenConfig { genesis, mode, host_schemas, immutable_data_directory,
  pruning }` with `with_mode`, `with_host_schemas`,
  `with_immutable_data_directory`, `with_pruning`.
- `OpenMode`: `CreateIfMissing`, `ExistingOnly`, `CreateNew`.
- `OpenInfo { created, commit_id, state_root, index_root, genesis_id }`.
  `genesis_id` is the hash of `"golemdb/genesis/v1\0"`, the format, hash and
  Roaring ids, and every genesis cell in key order.
- `OpenError`: `Io`, `Yaml`, `InvalidConfig`, `NotInitialized`,
  `AlreadyInitialized`, `GenesisMismatch`, `CorruptState`,
  `UnsupportedFormat`, `UnsupportedHash`, `UnsupportedRoaring`, and
  transparent wrappers of internal errors.
- `MdbxOptions { max_tables, max_map_size, growth_step, page_size }`.
- `PruningConfig::new(window)`: garbage-collect trie rows that only commits
  older than `head - window` reach. Pruning runs inside each commit. There
  is no pruning or GC call on `Api`.

### Records

- `RecordOp::create(key, budget)` with `.attribute(name, value)` and
  `.field(name, value)`; `RecordOp::patch(key, budget)` with `.set`,
  `.attribute`, `.field`, `.remove`; `RecordOp::get(key, budget)` with
  `.only(names)`; `RecordOp::delete(key, budget)`. Builders collect input
  and validate nothing; admission happens when the call runs.
- `ReadTarget`: `Head`, `Branch(id)`, `Commit(id)`. On `main` a commit other
  than the head is `CommitUnavailable`.
- `Record { key, cells: BTreeMap<CellName, CellValue> }`. A full read of a
  user record returns the user cells plus `#key` (bytes32) and `#meta`
  (four `u64` counters: cells, cell bytes, indexed cells, index bytes).
  `Record::meta()` decodes it.
- Cell types: `bool`, `str`, `bytes` (field only), `bytes20`, `bytes4` to
  `bytes32`, `u32` to `u256`, `i32` to `i256`, `dec32` to `dec256`, `f32`,
  `f64`, `date32`, `timestamp64`. Kinds: attribute (indexed) and field.
- Reserved records, readable with `get` and refused for writes
  (`Reserved`): `#params`, `#alloc`, `#roots`, `#recordKeys`, `#rootIndex`,
  `@meteringModel`, `@modelWeight`.
- Order of checks for a write: branch handle, reserved key, each entry in
  caller order (name length, value limits, name grammar, kind, duplicate
  name), then existence, then the per-record caps.

### Receipts, metering and pricing

- `Metered<T> { result, receipt }`, on success and on failure.
- `Receipt { cost, priced_at, details, ledger }`. `details` has nine
  counters of user-cell effects. `ledger` counts charged units per weight
  name and is filled even when every price is zero.
- `Budget`: `Limited(u64)`, `Unlimited`. Writes and `get` enforce it and
  return `OutOfBudget { required }`. `query` and `count` accept it and do
  not enforce it yet.
- `PricingConfig { golemdb_code_version, host_code_version, weights }` in
  genesis; `HostMeteringSchema` in `OpenConfig`. `metering_model` and
  `model_weights` read the stored model. No call changes a model yet.

### Queries

- `QueryOp::from_filter(filter, budget)` with `.sort(keys)`, `.only(names)`,
  `.limit(n)`, `.offset(n)`, `.with_total()`; `CountOp::filter(filter,
  budget)`.
- `Filter`: `Eq`, `In`, `Range` (`Bounds`), `Prefix`, `Exists` (typed or
  any type), `And`, `Or`. Leaves see attribute cells only and are typed by
  their value. `Not` is not implemented (issue #46).
- `SortKey::asc(field, marker)` and `desc`, at most 4 keys. A cell of the
  key's type sorts whatever its kind; an absent value sorts below every
  present one; ties come in ascending record id order in both directions.
  Without sort keys the order is ascending record id (creation order).
- Caps, hardcoded in `database.rs`: filter depth 64, page size 100, sort
  keys 4. `limit` 0 is `InvalidQuery`; above 100 is `LimitExceeded`.
- `QueryPage { records, total_matched, evaluated_at }`. Paging is by
  offset; there is no cursor.

### Branches

`begin` opens a branch on the head. `checkpoint` and `rollback` work on
frames. `seal` freezes the live state and returns `SealInfo { commit_id,
state_root, index_root }`. `commit` publishes; the first committer wins, a
losing branch gets `Conflict`, and a consumed or unknown id gets
`HandleInvalid`. `BranchInfo { commit_id, branch_id, version, sealed }`;
`version` is the number of undo entries the branch holds.

### Immutable data

Append-only typed rows in named segments declared in genesis. Rows are
staged on a branch (also after seal), published with its commit, addressed
by ordinal or by an optional 32-byte key, and read with column projections.
They do not enter the state root.

### Errors

`ApiError`: `StoreFull`, `NotImplemented`, `NotFound`, `CellNotFound`,
`OutOfBudget`, `AlreadyExists`, `Reserved`, `InvalidArgument`,
`InvalidQuery`, `LimitExceeded`, `HandleInvalid`, `Conflict`, `Sealed`,
`NoFrameToRollback`, `CommitUnavailable`, `Internal`.

### Changes since `57c48ca`

Every item except the last breaks an implementation of `Api` or a caller
(read; the compile errors are under [Evidence](#evidence)).

| Change | Effect on our kit |
| --- | --- |
| `create`, `get`, `patch`, `delete` return `Metered<T>`, not `Result<T>` | 4 signature errors |
| `RecordOp` constructors take a `Budget`; builder steps no longer return `Result`; `cells()` and `changes()` are replaced by `entries()` | 3 errors; admission moved from the builder into the call |
| `Api` gains `query`, `count`, `metering_model`, `model_weights`, `immutable_data_schema`, `immutable_data_truncate`; loses `immutable_data_range_of`; the two immutable reads take read options | 4 errors |
| `Genesis::new` takes 5 arguments, not 2; `Genesis` is no longer `Copy`; YAML needs `record_limits` and `metering` | 3 errors |
| `OpenConfig` gains `host_schemas`, `immutable_data_directory`, `pruning` | none at compile time |
| `ApiError` gains `CellNotFound`, `OutOfBudget`, `InvalidQuery`, `LimitExceeded` | none at compile time |
| Records carry `#meta`; patch has `set`, which needs an existing cell; per-record caps | behaviour |
| Immutable data is implemented (it returned `NotImplemented`) | behaviour |
| Stored format 2: flat index (#108), record-id-prefixed cell tries (#69) | golem-db's own root values changed |

The API changed again between the external harness's pin (`85b053c`) and
`main`: `Genesis::new` gained the pricing argument and `SortKey::asc` takes
a type marker.

Open pull requests that will change it further (read): `Api::retention()`,
`Genesis::min_retention` and a `Pruning` enum in `OpenConfig` (branch
`ev/retention-api`, PR #116 builds on it); historical reads for
`ReadTarget::Commit` (#172); public constants for the reserved record keys
(#153); `In` values in strictly ascending order (#169); a `Not` filter
(branch `francesco/query_not`).

## What the harness uses

Read from both conformance crates.

- **How a backend is plugged in: it is not.** `golemdb-conformance` depends
  on `golemdb-api` and `tempfile` only. Its `TestDb` wraps a
  `golemdb_api::Database` opened with `Database::open(tempdir, &OpenConfig)`
  and dereferences to it. Several tests open databases themselves
  (`Database::open`, `open_memory`, `open_with_options`). There is no trait
  parameter, feature flag or factory to select another implementation.
- **Calls made.** In-repo crate: `begin`, `create`, `patch`, `commit`,
  `get`, `delete`, `seal`, `head`, `rollback`, `branch_info`, `discard`,
  `checkpoint`, `count`. The external suite adds `query`,
  `immutable_data_append`, `immutable_data_get`, `immutable_data_rows_of`.
- **Not called by either:** `metering_model`, `model_weights`,
  `immutable_data_schema`, `immutable_data_truncate`,
  `Database::from_store`, `with_pruning`, `with_host_schemas`.
- **What the tests observe beyond results and error kinds:** the cells of
  the seven reserved records (`#nextRecordID`, the `#recordKeys` binding of
  a key, `#roots` against `seal`'s roots), `#meta`, `BranchInfo::version`,
  receipt `details`, `priced_at`, `cost` and `ledger`, admission error
  messages, and `OpenInfo`. Roots are compared with each other, not with
  fixed values, with one exception in the external suite: the genesis
  `index_root` must be the hash of the empty string. A second one, that
  `genesis_id` follows the spec formula, is an ignored spec-gap test.
- **The load tool** (`crates/harness`) calls `begin`, `create`, `patch`,
  `delete`, `seal`, `commit`, `discard`, `query`, `count`, and reads MDBX
  table statistics directly through `libmdbx` for `table_log`.

**What our kit implements today:** `ArkivDb` implements the `Api` trait of
`57c48ca` (CRUD, branches, opening). It is compiled into the load tool by a
patch to `crates/harness` and selected with `GOLEMDB_BACKEND`. It has no
`Metered` results, no queries, no `#meta`, and answers reserved records with
`#key` and the three cell limits only.

## Gap analysis

"Adapter" is the scratch crate described under [Evidence](#evidence). Engine
types are in `crates/arkiv-db`. Status: **exact** means a caller cannot tell
the difference through the API, apart from the hash values themselves;
**filler** means the call works and returns a constant or a derived
stand-in; **missing** means the call fails or the feature is absent.
"Checked" names the evidence: `run` (a passing conformance test exercises
it) or `read`.

| # | API element | Our equivalent | Status | Checked | Difference a test sees |
| --- | --- | --- | --- | --- | --- |
| 1 | `Api` as trait object, shared by clones and threads | `ArkivDb` behind `Arc<dyn Api + Send + Sync>` | exact | run | none |
| 2 | `Database::open` | `DurableEngine` on `RedbStore`, file `arkiv.redb` | exact | run | none |
| 3 | `Database::open_with_options`, `MdbxOptions` | options ignored | filler | run | `max_map_size` does not cap the store: 1 test never sees `StoreFull` |
| 4 | `Database::open_memory` | in-memory `Engine` | exact | run | none |
| 5 | `Database::from_store` | none: `Store` is golem-db's key/value layer | missing | read | cannot be offered; the shim opens golem-db's own engine |
| 6 | `Database::genesis` | kept by the adapter | exact | run | none |
| 7 | `OpenInfo::created`, `commit_id` | engine head minus the namespace commit | exact | run | none |
| 8 | `OpenInfo::state_root` | `Snapshot::state_root` | exact | run | other bytes; equal after reopen and equal to the seal's root |
| 9 | `OpenInfo::index_root` | none: the term index is under the state root | filler | run | 32 zero bytes; golem-db returns the hash of the empty string at genesis |
| 10 | `OpenInfo::genesis_id` | golem-db's formula, computed by the adapter; also used as `EngineGenesis::deployment` | exact | run | none for keccak: equal to golem-db's own value for three genesis variants (the shim's unit test; the suite's formula test is ignored) |
| 11 | `Genesis` type, `new`, `from_yaml`, `load` | golem-db's own code, re-exported | exact | run | none |
| 12 | `hash_function: Keccak256` | the engine's only hash | exact | run | none |
| 13 | `hash_function: Blake3` | none | missing | run | `open` returns `InvalidConfig`; 4 tests |
| 14 | `cell_limits` | `Limits::{max_cell_name_len, max_str_len, max_bytes_len}`; `max_indexed_len` set to `max_str_len` | exact | run | `InvalidConfig` if names exceed 32 bytes or `max_str_len` exceeds 512 |
| 15 | `record_limits` | checked by the adapter; `Limits::max_cells` set to match | exact | run | none up to 1024 cells per record |
| 16 | `metering` (`PricingConfig`) | stored and returned, never charged | filler | run | see 39 to 41 |
| 17 | `immutable_data` | validated, then unused | missing | run | see 64 to 68 |
| 18 | `OpenConfig::mode`, `OpenMode` | adapter logic over `DurableEngine::initialize` and `open` | exact | run | none |
| 19 | `OpenConfig::host_schemas` | passed to golem-db's `PricingConfig::validate` when creating | filler | read | not checked on reopen |
| 20 | `OpenConfig::immutable_data_directory` | ignored | missing | read | none without immutable data |
| 21 | `OpenConfig::pruning`, `PruningConfig` | `Retention::Window(window)` of the durable engine | exact | read | not exercised by any test |
| 22 | `OpenError` kinds | `NotInitialized`, `AlreadyInitialized`, `GenesisMismatch`, `InvalidConfig`, `CorruptState`, `Io`, `Yaml`, `Metering` | exact | run | `Unsupported*` are never returned |
| 23 | `RecordOp` builders | golem-db's own code | exact | run | none |
| 24 | `create` | golem-db's admission copied into the adapter, then `WriteOp::CreateRecord` | exact | run | none; one extra read per create |
| 25 | `get` at `Head` and `Branch` | `Snapshot::try_get_record`, `Branch::try_get_record` | exact | run | none |
| 26 | `get` at `Commit(n)` | head only by default; retained history with `ARKIV_SERVE_HISTORY=1` | exact | run | with history on, 4 tests that expect `CommitUnavailable` fail |
| 27 | `patch`: `attribute`, `field`, `set`, `remove`, no-op rule | adapter reads the row, then `WriteOp::PatchRecord` with the changed cells only | exact | run | none |
| 28 | `delete` | `WriteOp::DeleteRecord` | exact | run | none |
| 29 | `Record`, `#key` | added by the adapter | exact | run | none |
| 30 | `#meta`, `RecordMeta` | computed from the row on every read | exact | run | none |
| 31 | Cell types, kinds, names, value order | `TypeTag`, `Value`, `CellName`: same ids, same order bytes, same name grammar | exact | run | none |
| 32 | Order of checks and admission messages | copied from `golemdb-record` `admission.rs` | exact | run | none |
| 33 | `#params` | from genesis | exact | run | none |
| 34 | `#alloc` (`#nextRecordID`) | `NamespaceRoots::next_record` + 63, plus the creates staged on the branch | exact | run | none; read through `Snapshot::try_namespace_roots`, a `#[doc(hidden)]` test aid |
| 35 | `#recordKeys` | `Row::id` + 63 | exact | run | none |
| 36 | `#roots` | state roots of the retained snapshots below the view, with a zero index half | filler | run | index half is zeros; cells exist only for retained commits |
| 37 | `#rootIndex`, `@meteringModel`, `@modelWeight` | genesis cells | exact | run | none while no call changes a model |
| 38 | Writes to reserved keys return `Reserved` | adapter check | exact | run | none |
| 39 | `Metered`, `Receipt` shape, `priced_at` | adapter | exact | run | none |
| 40 | `Receipt::details` | computed from the before and after cells | exact | run | none |
| 41 | `Receipt::cost` | 0 | filler | run | wrong for any nonzero price; 1 test |
| 42 | `Receipt::ledger` | empty | filler | run | golem-db counts units even at zero prices |
| 43 | Budget enforcement, `OutOfBudget` | none | missing | run | never returned; 3 tests |
| 44 | `metering_model` | constant: genesis model 1 at the selected commit | filler | read | none while no call changes a model |
| 45 | `model_weights` | the genesis weights | filler | read | same |
| 46 | `query`, `QueryPage`, `evaluated_at` | adapter over `Snapshot::try_query` | exact | run | none in results; cost differs (below) |
| 47 | `count` | size of the adapter's match set | exact | run | none |
| 48 | `Filter::Eq`, `In` | `Term::Eq`; `In` is a union | exact | run | none |
| 49 | `Filter::Range` | `Term::Range`; two open sides become `Term::Exists` | exact | run | none |
| 50 | `Filter::Prefix` | `Term::StartsWith` | exact | run | none |
| 51 | `Filter::Exists`, typed and untyped | `Term::Exists`; untyped is a union over the 23 indexable types | exact | run | none |
| 52 | `And`, `Or`, nesting to depth 64 | set operations on record ids in the adapter | exact | run | none; the engine's own query form is a flat DNF of 8 groups by 8 literals |
| 53 | ORDER BY: `SortKey`, absent values, ties, cap of 4 | fetch and sort in the adapter | exact | run | none; the engine orders by record id only |
| 54 | `limit`, `offset`, `with_total`, page cap 100 | adapter | exact | run | none |
| 55 | Query at `Some(commit)` | head only, as 26 | exact | run | as 26 |
| 56 | Query errors: `InvalidQuery`, `LimitExceeded`, `InvalidArgument` | golem-db's term checks (`golemdb-index`) called by the adapter | exact | run | none |
| 57 | `head`, `begin`, `discard` | `Engine::begin`, adapter registry | exact | run | none |
| 58 | `branch_info`, `BranchInfo::version` | sum of the undo entries each logged write stands for | exact | run | none |
| 59 | `checkpoint`, `rollback` | `Branch::try_checkpoint`, `rollback_to`, golem-db's frame rules in the adapter | exact | run | none |
| 60 | `seal`: `commit_id`, `state_root` | `Branch::try_state_root` | exact | run | other bytes |
| 61 | `seal`: `index_root` | none | filler | run | zeros; the tests that compare index roots today assert equality only, so zeros pass them without proving anything; a test that expects a change would fail |
| 62 | `commit`: first committer wins, `Conflict`, `HandleInvalid` | `Engine::publish`, `DurableEngine::prepare` and `commit` | exact | run | none |
| 63 | Record id order as the default query order | `RecordId` creation order | exact | run | none |
| 64 | `immutable_data_schema` | none | missing | read | `NotFound` without segments (as golem-db), else `NotImplemented` |
| 65 | `immutable_data_append` | none | missing | run | 11 tests |
| 66 | `immutable_data_truncate` | none | missing | read | same |
| 67 | `immutable_data_get` | none | missing | run | same |
| 68 | `immutable_data_rows_of` | none | missing | run | same |
| 69 | `ApiError` kinds other than the two below | adapter mapping | exact | run | none |
| 70 | `ApiError::StoreFull` on a size cap | a full disk maps to `StoreFull`; `RedbStore` has no size option | missing | run | 1 test (same as 3) |

Count: 49 exact, 10 filler, 11 missing.

Semantic differences that do not show in the table's statuses:

- **Root values.** Our state root is the digest of the engine's
  `StateHeader`. It never equals golem-db's. Nothing in either suite
  compares a root with a constant, apart from the genesis `index_root`.
- **Query cost.** The adapter walks each filter leaf to its end and sorts
  in memory. Results match; the work does not scale like golem-db's bitmap
  evaluation, and the load tool's query scenarios at a million records were
  not run.
- **Metering numbers.** With `PricingConfig::zero()` golem-db's cost is 0
  and ours is 0, so most receipt tests pass. golem-db's ledger is not empty
  in that case, and ours is.
- **Limits.** Cell names above 32 bytes and `str` limits above 512 bytes are
  refused at open. golem-db's bound depends on the MDBX key size.
- **Durability cost.** Not measured. One smoke run of the load tool is
  under [Evidence](#evidence); it is not a benchmark.

## Evidence

All of it was run on 2026-11-25 in `/home/ubuntu/scx1332/api-compare/`.
Logs are in `logs/` there.

### The pinned kit still passes

`conformance/golem-db/ci.sh setup 686ddfc… 57c48ca…` then `ci.sh tests` in a
scratch copy of this repository: 21 tests, 16 pass, 5 fail, all 5 listed in
`expected-failures.txt`, with the default retention and with
`ARKIV_STATE_RETENTION=0`. Logs: `pinned-setup.log`, `pinned-tests.log`.

### The kit against the newest revisions

- `ci.sh setup HEAD feature/golem-db-api`, which is what the `golem-latest`
  CI job runs: the harness resolves to `a1b6b6b`, golem-db to `9676c40`, and
  setup stops with "harness.patch no longer applies" (`main.rs` and
  `workloads.rs`). Log: `latestjob-setup.log`.
- That job tracks the wrong golem-db ref. `feature/golem-db-api` was merged
  on 2026-10-13 and has not moved; `main` is 36 commits ahead of it.
- The job is not allowed to fail the workflow: it has
  `continue-on-error: true` and runs only on schedule and on demand. The
  scheduled run of 2026-11-23 (run 45198930254) shows "success" while its
  `golem-latest` job failed at "Set up harness at the latest revisions".
- With the patch set aside and the kit sources copied in next to golem-db
  `f6dae1d`: the build has 14 compile errors (13 in `arkiv.rs`, 1 in the
  harness's own `main.rs`), and the test build has 246 (`golem_tests.rs`
  uses the old builders throughout). Logs:
  `latestkit-check.log`, `latestkit-check-tests.log`.

### The harnesses against golem-db itself

| Run | Result |
| --- | --- |
| External `a1b6b6b` with its own pin `85b053c` | spec 184 pass, 15 ignored; oracle 2 pass |
| External `a1b6b6b` with golem-db `main` `f6dae1d` | does not compile: 16 `Genesis::new` calls and 1 `SortKey::asc` call in the conformance crate, 1 more `Genesis::new` in the load tool |
| The same after fixing those 18 call sites | spec 179 pass, 5 fail, 15 ignored; oracle 2 pass |
| In-repo `crates/conformance` at `ef6269a` | 100 pass, 8 ignored |

The 5 failures on `main` are tests the harness has not caught up on: three
YAML tests lack the now required `metering` field, one expects no metering
cells at genesis, one expects sorting to be unimplemented. The 15 and 8
ignored tests are marked as spec gaps; they fail on golem-db (run: the 8 on
`main`, the 15 on the harness's pin). The 8 also fail on our engine.

### The scratch adapter

The patch is `/home/ubuntu/scx1332/api-compare/adapter-latest.patch`
(2960 lines; it applies to this repository at `604e398` and adds
`conformance/golem-db/latest/`). It is not committed anywhere. It contains:

- `src/lib.rs` (255 lines): a crate with library name `golemdb_api`. It has
  `pub use real::*` over golem-db's `golemdb-api` and its own `Database`
  with the same constructors and accessors. `GOLEMDB_BACKEND` selects
  `arkiv` (default), `arkiv-memory`, or `golemdb`, which forwards to
  golem-db's own `Database`.
- `src/arkiv.rs` (2351 lines): the kit's adapter ported to the `Api` trait
  of `f6dae1d`, with the queries, `#meta`, reserved records, receipts
  details and golem-db's admission added.
- `harness-diffs/`: what changed in each harness. In golem-db's in-repo
  crate, two Cargo lines:

  ```toml
  # crates/conformance/Cargo.toml
  golemdb-api = { package = "arkiv-golemdb-api", path = "../../arkiv-shim" }
  # Cargo.toml, [workspace]
  exclude = ["arkiv-shim"]
  ```

  In the external harness, three Cargo lines (the load tool has its own
  dependency line) plus the 18 call sites it needs to compile against
  golem-db `main` at all. No assertion was changed in either.

Our engine crates are used unmodified, as path dependencies.

Check that the shim adds nothing: with `GOLEMDB_BACKEND=golemdb` both suites
give exactly the golem-db numbers above (100 and 8; 179, 5 and 15; oracle
2).

Results with `GOLEMDB_BACKEND=arkiv` (durable engine on redb):

| Suite | Pass | Fail | Ignored |
| --- | --- | --- | --- |
| In-repo `crates/conformance`, `ef6269a` | 94 | 6 | 8 |
| External spec suite, `a1b6b6b` | 165 | 19 | 15 |
| External oracle, 64 sequences (default) | 2 | 0 | 0 |
| External oracle, `PROPTEST_CASES=1000` | 2 | 0 | 0 |

The oracle applies random sequences of creates, patches, deletes, reads,
queries, checkpoints, rollbacks and commits, and compares every result,
error, receipt and the whole state with an SQLite mirror after each step.
The 1000-sequence run used the adapter before its last two edits (the
pruning-window mapping and a comment); every other number in this section
is from the final sources.

The 25 failures, by cause:

| Cause | In-repo | External | Tests |
| --- | --- | --- | --- |
| Stale test, fails on golem-db `main` too | 0 | 5 | `opening::genesis_id_depends_on_settings_only`, `genesis_load_reads_a_file`, `genesis_yaml_rejects_missing_duplicate_and_unknown_fields`, `genesis_writes_only_the_listed_cells`, `query::sort_is_not_implemented_yet` |
| Immutable data | 0 | 11 | 11 of the 12 `immutable_data::*` tests that run |
| Metering: cost, budget, `OutOfBudget` | 4 | 0 | `receipts::valid_calls_are_charged`, `out_of_budget_counts_nothing_and_spends_within_the_budget`, `an_overflowing_cost_is_out_of_budget_even_unlimited`, `handles_and_targets_are_checked_before_the_budget` |
| Blake3 | 1 | 2 | `api_shape::hash_is_hidden_behind_one_type`, `opening::create_new_refuses_an_initialized_store`, `opening::reopening_with_another_genesis_is_genesis_mismatch` |
| Index root, and Blake3 in the same test | 0 | 1 | `opening::genesis_index_root_is_the_hash_of_nothing` |
| Store size cap | 1 | 0 | `errors::a_full_store_is_store_full_and_writes_nothing` |

Everything else passes on our engine, including all record, patch, delete,
`#meta`, reserved-record, receipt-detail, branch, opening-mode and error
tests, and 34 of the 39 query tests (4 are ignored spec gaps, 1 is the
stale sort test).

Variants (external spec suite):

- `ARKIV_SERVE_HISTORY=1`: 161 pass, 23 fail. The 4 extra failures expect
  `CommitUnavailable` for an older commit and get the data. PR #172 will
  turn that expectation around.
- `GOLEMDB_BACKEND=arkiv-memory`: 161 pass, 23 fail. The 4 extra failures
  are reopen tests, which a volatile engine cannot pass.

Load tool (`crates/harness` of the external harness): with the shim and
its one `Genesis::new` fix,
`cargo load scenarios/block-size/2-blocks-of-100.toml` completes on both
backends (golem-db 5313 ops/s, arkiv 3034 ops/s, one run each, on a machine
that was also running a node). This shows the tool runs; it measures
nothing reliable.

Not run: the 15 ignored external tests on our engine, the load tool's query
and large scenarios, and the kit's own `golem_tests.rs`, which was not
ported.

## Node level

The harness drives an embedded Rust API. Nothing in golem-db listens on a
socket, and no test goes through a network. A mapping from golem-db's API
to our node's HTTP and JSON-RPC API ([node API reference](node-api-reference.md))
would have nothing to conform to, so none is proposed.

What "on our node" can mean in practice is that the adapter runs on the
same engine and store the node runs on. It does: `ArkivDb` uses
`arkiv_db::durable::DurableEngine` on `arkiv_db_redb::RedbStore`, the
types `arkiv-node` embeds. It uses its own namespace and its own database
file; it does not attach to a running node's database, and the harness
needs a fresh database per test.

## Recommendation

Ordered by value. Effort is working days for one person who knows the
engine.

| # | Step | Effort | What it buys |
| --- | --- | --- | --- |
| 1 | Replace the kit's harness patch with the drop-in `golemdb_api` crate. Review the scratch adapter, move it into `conformance/golem-db/`, and gate CI on golem-db's `crates/conformance` with an expected-failures list, as today. | 2 to 3 | The golem-db harness runs on our engine with a two-line Cargo change; 94 of 100 today |
| 2 | Fix the `golem-latest` job: track golem-db `main` and the conformance branch, and make a failure visible (a separate workflow whose status is not masked, or an issue opened by the job). | 0.5 | We learn within a week when the API moves; 13 commits changed `crates/api/src/lib.rs` in the six and a half weeks since our pin |
| 3 | Ask the golem-db team for the three things listed below. | 0 for us | Removes the need for the shim trick and for 5 failures we cannot fix cheaply |
| 4 | Metering in the adapter: drive golem-db's public `WriteMeter` and `ReadMeter` with the same planning steps, and enforce budgets. Needs a term-existence read per indexed cell. | 4 to 6 | 4 tests today, and every metering test they add; exact costs and ledgers without touching the engine |
| 5 | A size cap on `RedbStore`, mapped from `MdbxOptions::max_map_size`. | 1 to 2 | 1 test; `StoreFull` behaviour is then tested on our store |
| 6 | Expose the term-map root of a namespace on `Branch` and `SealedCandidate` (it is on `Snapshot` as a hidden test aid), and report it as `index_root`. | 1 to 2 | A real `index_root` in `seal`, `OpenInfo` and `#roots`; needed once they port root tests |
| 7 | Translate filters to the engine's DNF where they fit, use `try_count` for `count`, and page unsorted queries in the engine. | 3 to 4 | The load tool's query scenarios become meaningful |
| 8 | Immutable data: append-only typed segments with ordinals, optional keys, per-commit ranges and projections, published atomically with a commit. | 8 to 12 | 11 tests; only worth it if the product needs the feature |
| 9 | Blake3 as a second engine hash. | large, not estimated | 4 tests; no product need known |

Follow-ups that are cheap when upstream merges them: historical reads (#172)
are a flag in the adapter already; `Api::retention()` maps to
`DurableEngine::state_floor`; a `Not` filter maps to the engine's negated
literal.

What to ask the golem-db team, with reasons:

1. **A stable way to run `crates/conformance` on another implementation.**
   The smallest form: keep the crate depending on `golemdb-api` alone (true
   today), open every database through one function in `src/lib.rs`, and
   add `Database::from_api(Arc<dyn Api + Send + Sync>, Genesis, OpenInfo)`.
   Reason: `Database` is concrete and its inner trait object is private, so
   today the only route is to replace the whole crate by name.
2. **Capability gates on tests** for Blake3, immutable data, metering and
   the MDBX size cap (Cargo features or a capability set on the test
   database). Reason: an implementation that does not offer one of these
   should skip those tests by declaration, not carry a list of expected
   failures that drifts.
3. **Receipts that another implementation can produce.** `Ledger` has no
   public constructor (`Ledger::from_counts` is crate-private), and
   `MdbxOptions` is part of the public API. Ask for a public `Ledger`
   constructor, or the metering rules as test vectors, and for a store
   option that is not named after MDBX.

Also worth asking: a note in `docs/CHANGES.md` for every change to `Api`,
since each one breaks our adapter at compile time.

## Not determined

- Whether the query, branch, opening, immutable-data and oracle modules
  will move into golem-db's in-repo crate, and when the stack leaves draft.
- Whether the owner's harness also means the load tool. It runs through the
  shim, but its `table_log` reads MDBX directly and would need the kit's
  `table_stats` again.
- Relative performance. Nothing here is a benchmark.
- Behaviour of `OpenConfig::pruning` mapped to our retention window, and of
  `host_schemas` on reopen: mapped by reading the code, exercised by no
  test.
- How `#roots` should look once golem-db prunes it (`#rootIndex`): golem-db
  writes identity only for `#rootIndex` today.
