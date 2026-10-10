# Benchmarking: old vs. new engine vs. golem-db

`scripts/bench-compare.sh` is an unattended runner. It compares these
engines on the golem-db conformance harness ([kit](../conformance/golem-db/README.md),
[earlier results](golem-db-conformance-report.md)):

- golem-db on MDBX, at one revision (`golem`) or at two (`golem-old` and
  `golem-new`);
- this engine at an old revision (`arkiv-old`);
- this engine at a new revision (`arkiv-new`, and `arkiv-mem` for its
  in-memory engine).

It also runs an object-cache sweep and a reader/producer workload. It is
built to start from `at` with nobody watching, so a failed run is logged and
the runner moves on to the next one.

## Launch

```sh
scripts/bench-compare.sh --quick --max-wait-load 0     # smoke test, a few minutes
scripts/bench-compare.sh --all --dry-run               # print the plan only
scripts/bench-compare.sh --all --runs 3 --no-cache-sweep --no-readers \
  --golem-old-rev 95a24ea --golem-new-rev 57c48ca \
  --engines golem-old,golem-new,arkiv-new               # two golem-db revisions vs. arkiv
echo '/home/ubuntu/arkiv-network/arkiv-state-db-experiment/scripts/bench-compare.sh --all \
  >>/home/ubuntu/arkiv-bench/at-console.log 2>&1' | at 01:00   # at uses the machine's time zone
```

Options: `--all` (default) | `--quick` | `--scenarios LIST`
(comma-separated scenario ids or prefixes, such as `index-layout,commit-growth`),
`--runs N` (5), `--old-rev REV` (`e230fb3`), `--new-rev REV` (default
`origin/main` after a `git fetch`, or local `main` if the fetch fails),
`--golem-rev REV` (golem-db revision of the `golem` engine; default the
commit the harness pins as its `golem-db` submodule),
`--golem-old-rev REV` and `--golem-new-rev REV` (benchmark two golem-db
revisions as `golem-old` and `golem-new` instead of `golem`; a missing one
defaults to the pinned commit; not combined with `--golem-rev`),
`--engines LIST` (comma-separated; default every engine of the mode:
`golem,arkiv-old,arkiv-new,arkiv-mem` or
`golem-old,golem-new,arkiv-old,arkiv-new,arkiv-mem`),
`--out DIR` (default `/home/ubuntu/arkiv-bench/results/<UTC timestamp>`),
`--resume DIR`, `--max-wait-load MIN` (120), `--load-threshold X` (1.5),
`--cpus LIST` (`2-9`), `--timeout MIN` (60 per run), `--full-large`
(also run the full-size `scenarios/large`, with a 6 h timeout per run),
`--no-cache-sweep`, `--no-readers`, `--dry-run`.

## What it does

1. **Prepare** (no measurement yet). Everything lives under
   `/home/ubuntu/arkiv-bench` on the NVMe disk, not on the `/tmp` tmpfs:
   - `git archive` exports of both revisions into `src/arkiv-<sha>`. The
     main checkout is never touched.
   - A copy of `golem-db-conformance-testing` at `686ddfc` in
     `src/golem-db-conformance-testing`. It is copied from a local clone if
     one exists; otherwise it is cloned with `--recurse-submodules`.
   - One harness copy per pair of arkiv and golem-db revision
     (`harness/h-<arkiv sha>-g<golem sha>`), with **that arkiv revision's
     own** kit applied (`harness.patch`, `src/*.rs` and `scenarios-scaled`)
     and the copy's `golem-db` submodule checked out at the golem-db
     revision. The harness builds golem-db from the submodule as a path
     dependency, so each golem-db revision needs its own copy and build. The
     golem engines run in a harness built with the new arkiv revision; the
     arkiv engines run with the newest configured golem-db (`golem-new` or
     `golem`). Only the pairs that the selected engines need are built.
     golem-db revisions are resolved in the conformance copy's submodule;
     one that is not there is fetched from origin.
   - Release builds into `bin/` (`CARGO_BUILD_JOBS=4`). A build is skipped
     when its revisions, kit and binary have not changed. The harness
     `Cargo.lock` from the first build is pinned and reused, so both
     revisions build against the same dependency versions.
   - The `bench_readers` example for both revisions. If a revision does not
     contain it, the runner adds the current file to that revision's export.
   - A build failure stops the runner with exit code 2 and records it in
     `STATUS`. The one exception is `bench_readers`: if it fails to build,
     only that revision's reader runs are skipped.
2. **Load gate.** The runner waits until the 5-minute load average is below
   the threshold, checking once a minute. After `--max-wait-load` minutes it
   goes ahead anyway and records `load gate timed out` in
   `meta/load-gate.txt` and in the summary.
3. **Measure.** The engines are interleaved: for each round, for each
   scenario, the engine order is shuffled with a fixed seed. Before each
   run the runner does the following:
   - writes `meta.txt`: load, the top 10 processes by CPU, `free -m`, `df`,
     `uname` and the revisions;
   - calls `sync` and drops the page cache with `sudo -n`, if available.

   Each run is pinned with `taskset`, timed with `/usr/bin/time -v` (peak
   RSS) and wrapped in `timeout`. It gets a fresh `db/<key>` directory,
   which is measured with `du -sb` and then deleted.
   - Engines: `golem`, or `golem-old` and `golem-new` (no
     `GOLEMDB_BACKEND`), `arkiv-old`, `arkiv-new`
     (`GOLEMDB_BACKEND=arkiv`), and `arkiv-mem` (`arkiv-memory`, new
     revision, only on commit-growth, block-size/2 and large-2m-small), as
     limited by `--engines`.
   - `--all` scenarios: `index-layout/*`, `block-size/*`, `commit-growth`,
     `concurrent-writers`, `scenarios-scaled/large-2m-small` and
     `large-realistic-small`.
   - Cache sweep (when arkiv-new is selected): arkiv-new with `ARKIV_OBJECT_CACHE_MIB` set to 8, 32 and
     128 (16384 entries per 8 MiB), on commit-growth and large-2m-small,
     3 runs each.
   - Reader workload (`crates/arkiv-db-redb/examples/bench_readers.rs`): one
     producer commits blocks of 100 or 1000 operations (3 creates to 1
     patch) on a redb `DurableEngine`, while 0, 1, 4 or 8 reader threads do
     point reads on the latest head. 3 runs, for whichever of arkiv-old and
     arkiv-new are selected.
4. **Summarize.** `scripts/bench/summarize.py` parses every run directory
   again into `results.jsonl` and writes `summary.md`, which has:
   - per scenario: the median, min and max ops/s, the seal and commit p50,
     the commit p99, disk use and peak RSS for each engine, and the ratios
     between the engines that ran (arkiv new/old, golem new/old, and
     arkiv-new against each golem engine);
   - the cache-sweep table;
   - the reader table;
   - the run conditions.

   A ratio is marked **sig** only when the min..max ranges of the two
   engines do not overlap.

`--quick` runs commit-growth and block-size/1 once each, with one cache
size and a small reader run, and skips the load gate.

## Results directory

| Path | Contents |
| --- | --- |
| `runner.log` | Everything the runner did |
| `STATUS` | `phase`, `progress`, `failures`, `exit_code` |
| `config.env` | Resolved revisions (arkiv and golem-db), engines and options, used by `--resume` |
| `plan.txt` | Every job with its exact environment and command |
| `meta/` | Revisions (arkiv, golem-db, the pinned submodule), engines, rustc, redb version, load-gate outcome, machine state at start and end |
| `prepare/` | Build logs |
| `runs/<variant>__<scenario>__<engine>__r<N>/` | `out.log`, `time.txt`, `meta.txt`, `cmd.txt`, `env.txt`, `exit`, `disk_bytes`, `wall_secs`, harness CSVs under `results/`, and a `done` or `failed` marker |
| `results.jsonl`, `summary.md` | Parsed results and the report |

`--resume DIR` reuses that directory's `config.env`, so it keeps the same
revisions, engines, mode and runs. It skips every run that has a `done` marker and
retries the failed ones. Only `--max-wait-load` and `--dry-run` can be
overridden on resume. A lock file (`/home/ubuntu/arkiv-bench/.bench-compare.lock`)
stops two runners from running at the same time (exit code 3).

Exit codes:

| Code | Meaning |
| --- | --- |
| 0 | Every run succeeded |
| 1 | Some runs failed; the summary is still written |
| 2 | Prepare or build failed |
| 3 | Lock held by another runner |
