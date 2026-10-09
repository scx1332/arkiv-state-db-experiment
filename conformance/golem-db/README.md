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
| `harness.patch` | Changes to the harness's `crates/harness`. `Cargo.toml` adds the path dependencies on this repository and the `arkiv-v2` feature. `main.rs` adds backend selection with `GOLEMDB_BACKEND` and the `ARKIV_*` variables. `workloads.rs` skips the MDBX-only `table_log` for the arkiv backends. |
| `src/arkiv.rs` | The adapter: `ArkivDb` implements `golemdb_api::Api` on `arkiv-db`. |
| `src/golem_tests.rs` | golem-db's facade API tests ported to the adapter, with soft checks so that one run lists every divergence. |
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

- `GOLEMDB_DB_DIR`: database directory for every scenario. It is wiped first.
  The durable backend writes `arkiv.redb` there.
- `ARKIV_REDB_CACHE_MIB`: redb page cache. Default 256.
- `ARKIV_OBJECT_CACHE_MIB` and `ARKIV_OBJECT_CACHE_ENTRIES`: the engine's
  encoded-object cache. They default to `durable::Options::default()`,
  8 MiB and 16,384 entries.

Build with `--features arkiv-v2` to use the deployed v2 profile:

- cell names up to 32 bytes;
- i32, u256, dec256, bytes32 and address scalars;
- indexed values up to 128 bytes.

The load scenarios use only v1 types, so they run on either profile.

## Ported golem-db tests

```sh
cargo test -p golemdb-harness --features arkiv-v2 golem_tests -- --test-threads=1
```

Each test runs on both arkiv backends. The tests need `arkiv-v2`, because the
fixture uses an `i32` cell. Expected result: 4 tests pass and 5 fail. The
failures come only from these divergences:

- **golem-internal system records.** `#alloc` and its next record id, the
  `#recordKeys` binding and the `#roots` cell have no engine counterpart.
- **Old commits stay readable.** Reading an old `Commit(n)` returns data
  where golem expects `CommitUnavailable`, because this engine keeps every
  snapshot.

Every user-visible check passes. That includes reads of sealed branches,
which use `Branch::try_state_root`. A failing test prints each divergent check
with its line number.

## Adapter notes

- **Namespace and commit numbering.** The adapter uses one namespace,
  `golemdb`, created in a setup commit, so golem commit N is engine snapshot
  N + 1.
- **Seal.** `seal` takes the root from `Branch::try_state_root`, so the
  branch stays open for reads. `commit` then calls `try_seal` and publishes
  the candidate. On the durable backend publishing means `prepare` then
  `commit`.
- **Empty patches.** golem treats an empty patch as a no-op on an existing
  record. The engine rejects it (`ArgumentFault::EmptyPatch`), so the adapter
  emulates golem with an existence read. This repository keeps that
  rejection as protocol semantics.
- **Not supported.** `SealInfo::index_root` is all zeros, and the
  immutable-data calls return `NotImplemented`, as in golem-db.
