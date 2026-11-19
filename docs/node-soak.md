# Node soak

`scripts/soak/run.py` runs one `arkiv-proposer producer` under load for a
few hours and records how it behaves as its state grows. It complements the
[SDK side-by-side benchmark](benchmarks/2026-11-17-sdk-side-by-side.md), whose
writes last four seconds, and the [engine comparison](benchmarking.md), which
does not run a node.

## Run

```sh
cargo build --release -p arkiv-node --bin arkiv-proposer
(cd scripts/soak && bun install)          # once: SDK 1.0.1 from npm
scripts/soak/run.py --work /data/soak/2026-11-19 --drop-caches
scripts/soak/run.py --work /tmp/soak-trial --quick      # every phase, 3 minutes
```

The work directory must be new and on a real disk; a two-hour run writes
about 15 GiB. `--drop-caches` needs passwordless `sudo`. The node listens on
loopback at `--port` (18700) and the two ports after it.

## Phases

| Phase | Default | Load | What it shows |
| --- | --- | --- | --- |
| cohort | 4 s | 2,000 entities | The fixed set every read probe is aimed at |
| growth | 60 min | Creates at the block cap, lifetime 2,000,000 blocks | Write rate, latency, memory and disk as state only grows |
| churn | 40 min | Per 16 operations: 11 creates, 3 patches, 1 delete, 1 extension; lifetime 10 minutes | Steady state: after 10 minutes the expiry sweep deletes as fast as the workload creates |
| readers | 15 min | Churn at half the block cap, plus 4 callers reading | Read latency beside production, and how many reads are refused with `ServerBusy` |
| restart | 1 min | SIGTERM, optionally drop the page cache, start, probe twice | Time to ready, and that a block at the old head has the same hash and state root |

Writes are `executeBatch` calls of 16 operations, 8 in flight with explicit
nonces from one account: 128 operations per wave, which is
`max_ops_per_block`. A read probe runs every 5 minutes during growth and
churn, beside the writes: 100 requests of each read one at a time, then 100
from 4 callers. Six reads target the cohort, so their answers do not change;
two target the set the growth phase is writing.

## Stop rules

The run ends, keeping the node directory and every record, when a
transaction reverts or is lost, the node exits, free disk falls below
`--min-free-gb` (100), or a start or restart takes longer than
`--ready-min` (60; a restart rebuilds the JSON-RPC index from every block,
see the [2026-11-19 run](benchmarks/2026-11-19-node-soak.md)). Nothing is
retried. A failed read probe is recorded
in `probe-failures.jsonl` and does not stop the run.

## Results directory

| File | Content |
| --- | --- |
| `plan.json` | Commit, binary SHA-256, host, parameters |
| `status.json` | `running`, `completed` or `failed`, the phase, the reason, the restart result |
| `writes.jsonl` | One line per 10 seconds of writes: operations by kind, transaction latency, blocks, gas |
| `probes.jsonl` | One line per read probe |
| `readers.jsonl` | One line per 10 seconds of the readers phase |
| `samples.jsonl` | Every 10 seconds: head, memory, CPU time, file sizes, free disk, selected node metrics |
| `load.err`, `producer.log` | Errors of the workload; the node's output |
| `report.md` | Tables built from the above by `scripts/soak/report.py DIR`, which can be rerun |

`report.md` is raw material. A run worth keeping is written up as a dated
record under `docs/benchmarks/`.

## Limits

It is one node on loopback with one writer account and one run per
invocation, so it shows trends and cliffs, not capacity. The state starts
empty; latencies in a probe include contention with the phase's writes.
