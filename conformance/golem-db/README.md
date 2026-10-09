# golem-db conformance kit

Results and gap analysis: [golem-db conformance report](../../docs/golem-db-conformance-report.md).

Runs this repository's engine behind golem-db's public `golemdb_api::Api`
trait inside
[golem-db-conformance-testing](https://github.com/golemfactory/golem-db-conformance-testing),
the load harness for golem-db. One harness then runs three backends on the
same scenarios:

- golem-db on MDBX (the harness default);
- `DurableEngine` on redb (`GOLEMDB_BACKEND=arkiv`);
- the in-memory `Engine` (`GOLEMDB_BACKEND=arkiv-memory`).

These files are not a Cargo package or workspace member of this repository.
They are copied into a checkout of the harness and built there.

| File | Purpose |
| --- | --- |
| `harness.patch` | Changes to the harness's `crates/harness`. `Cargo.toml` adds the path dependencies on this repository. `main.rs` adds backend selection (`GOLEMDB_BACKEND`, the `ARKIV_*` variables), an optional golem genesis YAML (`GOLEMDB_GENESIS`) and picks each backend's `table_log` source. `workloads.rs` takes that source as a parameter instead of always reading MDBX. |
| `src/arkiv.rs` | The adapter: `ArkivDb` implements `golemdb_api::Api` on `arkiv-db`. |
| `src/golem_tests.rs` | golem-db's API tests ported to the adapter, with soft checks so that one run lists every divergence, plus adapter tests. |
| `scenarios-scaled/` | `scenarios/large/*` scaled to 1/10 of the commits, with `profile_at` removed. They have the same seeds and shapes as the originals. |

The patch was made against harness commit `686ddfc` ("allow overriding
location of db file"), with the `golem-db` submodule at `95a24ea`.

## Setup

You need Rust, which the harness's `rust-toolchain.toml` pins, and clang,
because the MDBX bindings are generated at build time.

The patch expects the harness checkout to sit next to this repository:

```
<parent>/
  arkiv-state-db-experiment/      # this repository
  golem-db-conformance-testing/   # the harness
```

```sh
cd <parent>
git clone --recurse-submodules https://github.com/golemfactory/golem-db-conformance-testing.git
cd golem-db-conformance-testing
git checkout 686ddfc && git submodule update --init    # the tested revision (optional)

KIT=../arkiv-state-db-experiment/conformance/golem-db
git apply "$KIT/harness.patch"
cp "$KIT/src/arkiv.rs" "$KIT/src/golem_tests.rs" crates/harness/src/
cp -r "$KIT/scenarios-scaled" .
```

If this repository lives somewhere else, point the three path dependencies
at it after applying the patch:

```sh
sed -i 's|\.\./\.\./\.\./arkiv-state-db-experiment|/abs/path/to/arkiv-state-db-experiment|' \
    crates/harness/Cargo.toml
```

You can also symlink it into place:
`ln -s /abs/path/to/arkiv-state-db-experiment ../arkiv-state-db-experiment`.

Cargo resolves the arkiv crates' dependencies in the harness's lockfile on
the first build. Their `workspace = true` settings still come from this
repository's root `Cargo.toml`.

To update the patch after changing the harness copy, run
`git diff 686ddfc -- crates/harness/Cargo.toml crates/harness/src/main.rs crates/harness/src/workloads.rs`
there, with the three path dependencies in their relative form.

## Running scenarios

`cargo load` is the harness's release-build alias:

```sh
GOLEMDB_BACKEND=arkiv cargo load scenarios/commit-growth.toml        # durable engine on redb
GOLEMDB_BACKEND=arkiv cargo load scenarios/block-size/2-blocks-of-100.toml
GOLEMDB_BACKEND=arkiv-memory cargo load scenarios/index-layout       # volatile engine
cargo load scenarios/index-layout                                    # golem-db (MDBX) baseline
```

The full set used for the reports:

- `scenarios/index-layout`
- `scenarios/block-size`
- `scenarios/commit-growth.toml`
- `scenarios/concurrent-writers.toml`
- `scenarios-scaled/large-2m-small.toml`
- `scenarios-scaled/large-realistic-small.toml`

The scaled files replace `scenarios/large`. Its `profile_at` needs `perf`, and
at full size the scenarios run for hours.

Environment variables:

- `GOLEMDB_BACKEND`: `golemdb` (default), `arkiv` or `arkiv-memory`.
- `GOLEMDB_DB_DIR`: database directory for every scenario. It is wiped first.
  The durable backend writes `arkiv.redb` there.
- `GOLEMDB_GENESIS`: a golem genesis YAML file (golem's `Genesis::load`
  format) used instead of each scenario's `[database]` hash and limits, for
  every backend.
- `ARKIV_REDB_CACHE_MIB`: redb page cache. Default 256.
- `ARKIV_OBJECT_CACHE_MIB` and `ARKIV_OBJECT_CACHE_ENTRIES`: the engine's
  encoded-object cache. They default to `durable::Options::default()`,
  8 MiB and 16,384 entries.

### `table_log` on the arkiv backends

Scenarios with `table_log` (the `large` ones) get one CSV block per commit
in the harness's `table,entries,depth,pages,mib` shape:

| row | meaning |
| --- | --- |
| `objects:catalog`, `objects:host`, `objects:namespace` | redb object table of each engine space: entries, B-tree height, pages, MiB |
| `records` | the other redb tables (metadata, manifests, block index) |
| `unaccounted` | allocated pages outside the tables: redb's table directory, allocator state, pages freed by recent commits |
| `free`, `file` | file pages not allocated, and the whole file |
| `object-cache` | the engine's encoded-object cache: entries and payload MiB |
| `live:records`, `live:terms`, `snapshots` | engine counts at head: records, index terms, retained snapshots (entries only) |

The memory backend logs only the last three rows.

## Ported golem-db tests

```sh
cargo test -p golemdb-harness golem_tests -- --test-threads=1
```

Expected result: 20 tests, 15 pass and 5 fail.

Ported from golem-db (`facade.rs`, `facade_errors.rs`, `opening.rs`,
`immutable_data.rs`): 14 tests. They run on both arkiv backends, or on the
durable backend over redb's simulated `CrashDisk` for the storage-fault
ports. The 5 failures come only from these divergences:

- **golem-internal system records.** `#alloc` and its next record id, the
  `#recordKeys` binding and the `#roots` cell have no engine counterpart.
- **Old commits stay readable.** Reading an old `Commit(n)` returns data
  where golem expects `CommitUnavailable`, because this engine keeps every
  snapshot.

Adapter tests (6, all pass):

- `branch_version_and_outcomes_match_golem_db_on_random_scripts` runs the
  same seeded random scripts (creates, patches, removes, empty patches,
  deletes, checkpoints, rollbacks) on golem-db's own memory `Database` and
  on both arkiv backends, and compares every call's outcome and
  `BranchInfo` after every step, then the committed records.
- `every_golem_cell_type_round_trips_as_attribute_and_field`,
  `empty_patch_is_a_no_op_or_not_found_without_an_adapter_read`,
  `blake3_genesis_is_refused_at_open`,
  `genesis_cell_limits_are_enforced_by_the_engine` and
  `table_stats_report_redb_tables_and_engine_counts`.

A failing test prints each divergent check with its line number.

## Adapter notes

- **Opening.** `ArkivDb::open(dir, &OpenConfig, options)` follows golem's
  opening modes (`CreateIfMissing`, `ExistingOnly`, `CreateNew`) and errors
  (`NotInitialized`, `AlreadyInitialized`, `GenesisMismatch`,
  `CorruptState`, `InvalidConfig`), and `info()` returns golem's `OpenInfo`:
  `created`, the head commit, the engine state root, and the engine genesis
  id as `genesis_id`. Memory and durable databases of one genesis report the
  same `OpenInfo`. `index_root` is all zeros (see below).
- **Genesis.** golem's `Genesis` (from its YAML) becomes the engine genesis.
  `max_cell_name_len`, `max_str_len` and `max_bytes_len` are the engine's
  limits, and `max_str_len` is also its indexed-string limit, because golem
  indexes `str` attributes up to `max_str_len`. Values above the engine's
  caps (32-byte names, 128-byte indexed values) and a Blake3 hash are
  refused with `InvalidConfig`, not narrowed.
- **Namespace and commit numbering.** The adapter uses one namespace,
  `golemdb`, created in a setup commit, so golem commit N is engine snapshot
  N + 1.
- **Branches.** Each branch keeps its engine `Branch` and the log of its
  retained writes; frames are positions in the log, backed by engine
  checkpoints. `BranchInfo::version` (golem's undo-entry count) is computed
  from the log when asked, so no write reads a record for it. The only
  extra read left is a non-empty patch right after a rollback, where golem's
  frame rule depends on whether the patch changes anything.
- **Storage failures.** A full medium is `StoreFull`; any other storage
  failure is `Internal` with the storage error as its source. As in golem, a
  failed call leaves the branch as it was: a branch the engine poisoned is
  rebuilt from its log, after the durable engine recovers the store. A
  failed commit keeps its seal and can be retried or discarded, and other
  branches are not blocked. A poisoned lock is `Internal`, and a panic
  inside a call drops only that call's work. Branch ids are process-wide,
  as in golem.
- **Seal.** `seal` takes the root from `Branch::try_state_root`, so the
  branch stays open for reads. `commit` then calls `try_seal` and publishes
  the candidate. On the durable backend publishing means `prepare` then
  `commit`.
- **Types.** Every golem cell type maps to an engine type with the same
  value domain, order and term bytes. The engine's `Key` (an Arkiv entity
  reference) has no golem counterpart and reads back as `bytes32`.
- **Empty patches.** The engine implements golem's no-op itself (`NotFound`
  for a missing record, otherwise applied with no state change).
- **Not supported.** `SealInfo::index_root` and `OpenInfo::index_root` are
  all zeros, because the index is part of the namespace state, and the
  immutable-data calls return `NotImplemented`, as in golem-db.
