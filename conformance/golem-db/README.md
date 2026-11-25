# golem-db conformance kit

Runs the test suites written against golem-db's public API on our engine,
through [`golem-api`](../../golem-api/README.md): golem-db's API served by
the arkiv engine, a crate of this repository with the library name
`golemdb_api`. A suite switches engines by renaming its `golemdb-api`
dependency; no test is changed.

What the crate serves exactly, with a stand-in or not at all is in its
README. The comparison with golem-db `main` and the results are in
[golem-db API comparison](../../docs/golem-db-api-comparison.md).

## The suites

| Suite | Where it comes from | Tests |
| --- | --- | --- |
| golem-db `crates/conformance` | golem-db's own conformance crate, still a stack of draft pull requests (issue #140, tip `ev/conformance-errors`) | 108: 100 run, 8 ignored as spec gaps |
| External harness, `crates/conformance` | [golem-db-conformance-testing](https://github.com/golemfactory/golem-db-conformance-testing), the suite the crate above is being ported from; a spec suite and an SQLite oracle | 201: 186 run, 15 ignored as spec gaps |
| External harness, `crates/harness` | the same repository's load tool (`cargo load`) | scenarios, run as a smoke test |
| `golem-api`'s own tests | this repository | 25 |

Each conformance suite runs twice: on golem-db's own engine through
`golem-api` (`GOLEMDB_BACKEND=golemdb`), as a control, and on the arkiv
engine (`GOLEMDB_BACKEND=arkiv`).

## Pinned revisions

| What | Revision | Where it is pinned |
| --- | --- | --- |
| golem-db that `golem-api` builds against | `main`, `f6dae1dd9a321ccd2fc7bc3540e9818649193f44` | `golem-api/Cargo.toml` and `Cargo.lock` |
| golem-db `crates/conformance` | `ef6269afc03acd1b6eec5e53506216446663eca0` (`f6dae1d` plus the conformance stack) | `GOLEM_DB_REV` in `.github/workflows/golem-conformance.yml` |
| External harness | `develop`, `a1b6b6b6b7b6f6c5af3578df4d2e647fc443520c` | `HARNESS_REV` in the same workflow |

The stack tip is a commit of a pull-request branch. If upstream rewrites the
branch, GitHub may stop serving that commit; the weekly run of the pinned
workflow notices, and the pins must then move.

## Results at the pinned revisions

| Suite | On golem-db (control) | On the arkiv engine |
| --- | --- | --- |
| golem-db `crates/conformance` | 100 pass | 94 pass, 6 expected failures |
| External harness, spec suite and oracle | 181 pass, 5 expected failures | 178 pass, 8 expected failures |

The expected failures, by reason:

| Reason | Tests | Meaning |
| --- | --- | --- |
| `metering` | 4 in-repo | cost is 0, the ledger is empty, budgets are not enforced |
| `blake3` | 1 in-repo, 2 external | the engine hashes with keccak-256 only |
| `index-root` | 1 external | `index_root` is zeros; that test also opens a Blake3 database |
| `size-cap` | 1 in-repo | `MdbxOptions::max_map_size` has no counterpart on `RedbStore` |
| `stale-upstream` | 5 external | the external harness has not caught up with golem-db `main`; they fail on golem-db too |

They are listed test by test, each with its reason, in
[`expected-failures/`](expected-failures). `check-tests.py` fails a run when
a test outside the list fails, when a listed test passes or is not run, and
when the number of tests run or ignored changes.

## Running it

You need Rust (rustup installs golem-db's toolchain from
`golem-api/rust-toolchain.toml`), clang for MDBX's bindings, and Python 3.

```sh
conformance/golem-db/ci.sh setup ef6269afc03acd1b6eec5e53506216446663eca0 a1b6b6b6b7b6f6c5af3578df4d2e647fc443520c
conformance/golem-db/ci.sh kit        # golem-api: rustfmt, clippy, its tests
conformance/golem-db/ci.sh in-repo    # golem-db crates/conformance
conformance/golem-db/ci.sh external   # external harness: spec suite and oracle
conformance/golem-db/ci.sh smoke      # load tool, three backends
```

`setup` checks the suites out into `conformance/golem-db/work/` (ignored by
git), where everything is also built. It changes two things in them, and
fails clearly when either no longer applies:

- the `golemdb-api` dependency line of each suite crate, to point at
  `golem-api`;
- in the external harness only, [`external-harness.patch`](external-harness.patch):
  the 18 call sites that harness needs to compile against golem-db `main` at
  all (`Genesis::new` gained a pricing argument, 17 sites; `SortKey::asc`
  takes a type marker, 1 site). No assertion is changed.

On a machine whose `/tmp` is a small tmpfs, set `TMPDIR` to a directory on
disk: the suites create a database per test.

To run one suite by hand, in `work/golemdb` or `work/harness`:

```sh
GOLEMDB_BACKEND=arkiv cargo test -p golemdb-conformance
GOLEMDB_BACKEND=arkiv cargo load scenarios/block-size/2-blocks-of-100.toml   # in work/harness
```

`ARKIV_STATE_RETENTION` and `ARKIV_SERVE_HISTORY` are described in the
crate's README.

### Load tool

`cargo load` runs on the arkiv backends through `golem-api` like any other
caller. One feature does not: `table_log` reads MDBX's file directly, not
through the API, so it only works with `GOLEMDB_BACKEND=golemdb`.
`ArkivDb::table_stats` returns the corresponding numbers for our store.

[`scenarios-scaled/`](scenarios-scaled) holds the two `scenarios/large`
files scaled to a tenth, without `profile_at` and `table_log`, and
`queries-small.toml`, the Arkiv query scenario at 2,000 records for the
smoke run.

## CI

Two workflows.

**`.github/workflows/golem-conformance.yml`, gating.** On every push to
`main`, on pull requests, weekly and on demand: `setup` at the pinned
revisions, then `kit`, `in-repo`, `external` and `smoke`. Any unexpected
failure, unexpected pass or changed test count fails it.

**`.github/workflows/golem-conformance-latest.yml`, weekly and on demand.**
The same steps against what upstream has now. It is a separate workflow so
that its result is visible: it is not a gate for this repository's commits,
and nothing in it is allowed to fail quietly. Two jobs:

- *golem-db main*: `ci.sh track main` rewrites `golem-api/Cargo.toml` to
  golem-db's current `main`; then `kit`, the external harness at its default
  branch, and the smoke run. It also runs `in-repo` once `main` has
  `crates/conformance`.
- *golem-db conformance stack*: the same with golem-db at
  `ev/conformance-errors`, for the conformance crate while it is a
  pull-request stack.

The test counts are not compared there (`CONFORMANCE_COUNTS=any`), because
upstream adds tests; a new test that fails on the arkiv engine still fails
the run.

When that workflow is red:

| Step that failed | What it means | What to do |
| --- | --- | --- |
| "Fetch the suites", cannot fetch `ev/conformance-errors` | the stack was merged or renamed | point the in-repo suite at `main` (the matrix in the workflow), re-pin |
| "Fetch the suites", a dependency line or the patch no longer applies | the suite's manifest or the patched call sites changed | adjust `ci.sh` or regenerate `external-harness.patch` |
| "golem-api", a compile error | golem-db's `Api` or its types changed | port `golem-api`, then move the pins |
| A suite, an unexpected failure | a new or changed test fails on our engine | fix `golem-api`, or list the test with its reason |
| A suite, a listed test passed | upstream changed the test, or we gained the feature | remove it from the list |

To move the pins: change the revision in `golem-api/Cargo.toml` (every
golem-db dependency, or `ci.sh track <ref>`), build once so `Cargo.lock`
follows, change `GOLEM_DB_REV` and `HARNESS_REV` in the gating workflow and
the table above, and update the counts in `ci.sh` if they changed.
