# Full Ethereum prefill Docker run

Started October 19, 2026 as detached container
`arkiv-experiment-full-7m-20261019`. This is the full **7,889,852-contract,
15,298,744-entity** processor export, not the imported 1m sample used for the
matched scaling benchmark. At launch the job was downloading; no successful
full conversion, bundle build or producer readiness is claimed here.

## Pinned inputs and limits

- Source: `s3://arkiv-sourcify-preload/seed-inputs/sourcify-ethereum-20261019.tar.zst`
- Archive bytes: `39842366403`
- Archive SHA-256: `b792e563a3048a4e5d60a2ab0c3a3fc1ff777df8d60b35c8adfee5db2429b9e9`
- Runtime image: `sha256:c13a2545a1a3b6169cdfff41abf14336db78e9f1ddc7d1d44ca87ea608217951`
- Producer SHA-256: `dbfb2e720683f2f07affadd17ad1e84ba7c3d7825fd803feec4919d81096142e`
- Docker memory: 32 GiB; memory plus swap: 32 GiB (swap disabled).
- Free disk reserve: 80 GiB; archive expansion ceiling: 256 GiB.
- Up to eight transport download attempts with byte-range resume; no automatic
  container or prefill retries/restarts, no overall time limit, no published ports.

The job runs as uid/gid 1000, with read-only root filesystem, dropped capabilities,
a bounded temporary filesystem, and a persistent bind-mounted job directory.
Scripts, producer binary, base genesis and fixture key were snapshotted before
launch. The R2 credential file is mounted read-only; credentials are not in logs,
tracked files or the plan. Download credentials are removed from the process
environment before handing off to conversion, though the mount remains present.

The job directory on this host is:

```
/home/ubuntu/arkiv-network/operations/runs/full-prefill-20261019
```

Its `plan.json` pins the source and resource settings. `snapshot/bootstrap.py`
streams the download with SHA-256 verification, rejects unsafe archive entries,
checks extracted file hashes and the expected full inventory, and then invokes
`scripts/run-full-prefill.py`. This runner converts the complete source without
`--contract-limit`, builds a v2 genesis bundle, and starts a fresh producer. It
checks height zero, genesis hash and state root before recording readiness.
A successful producer stays alive with automatic block production paused.

The source adapter, schema, payload bounds, funding and full-data capacity remain
subject to validation during the actual run. Failure preserves evidence for
follow-up; it does not trigger tuning, prefill retries or cleanup of the original
input. Transient download failures retry within the bounded acquisition step.

## Download recovery

The first attempt stopped October 19 at 23:57 Warsaw time after a broken R2
response stream. It preserved 29,372,186,624 of 39,842,366,403 bytes; conversion
had not started and Docker reported no OOM. Original status, plan, logs and
container state are retained in `attempt-1/` under the job directory.

The same container was explicitly restarted October 20 after replacing its
bootstrap with `scripts/full-prefill-bootstrap.py`. It hashes the existing
partial in bounded blocks, sends conditional `Range`/`IfMatch` requests for the
remaining bytes, validates response size/range/ETag, and requires the pinned
whole-archive SHA-256 before extraction. Transport/service failures get at most
eight application attempts with exponential backoff capped at 30 seconds;
identity, checksum and local I/O failures stop immediately. A completed valid
archive can be reused. Extraction and the later prefill stages remain fresh-only.

Bootstrap paths default to `/run/job`, `/snapshot`, and `/credentials/r2.env`;
`PREFILL_JOB_ROOT`, `PREFILL_SNAPSHOT_ROOT` and `PREFILL_CREDENTIALS` override
them for other deployments. The job snapshot is copied from the tracked script.

## Inspect the run

```sh
sudo docker ps -a --filter name=arkiv-experiment-full-7m-20261019
sudo docker logs --tail 40 arkiv-experiment-full-7m-20261019
sudo docker inspect arkiv-experiment-full-7m-20261019 \
  --format 'status={{.State.Status}} exit={{.State.ExitCode}} oom={{.State.OOMKilled}}'
```

Persistent evidence below the job directory:

- `bootstrap-status.json`: download/extract/verify progress or failure.
- `source-report.json`: verified source identity once bootstrap passes.
- `prefill/status.json`: active phase, exit/failure, metrics and verified readiness.
- `prefill/convert.log`, `stream-build.log`, `producer.log`: individual phase logs.
- `prefill/*-samples.jsonl`: process RSS/I/O and cgroup memory samples.
- `prefill/converted`, `bundle`, `node`: published artifacts as phases complete.

A Docker OOM or SIGKILL may leave the last status marked running: the container
exit/OOM state is authoritative in that case. Docker stdout is rotated to three
10 MB logs; phase logs and status files remain on the host. Stop intentionally
with `sudo docker stop -t 60 arkiv-experiment-full-7m-20261019` so the runner can
forward the signal and save status. Do not delete or restart the run as a repair
step before inspecting its failure evidence.

## Runner validation

`python3 scripts/test-run-full-prefill.py` covers a conversion failure stopping
later phases, a verified producer staying alive until SIGTERM, and rejection of
an existing output directory. The image was also checked for Ethereum Keccak,
converter dependencies and compatibility with the pinned producer binary.
`python3 scripts/test-full-prefill-bootstrap.py` covers interrupted range
resumption, early EOF, completed archive reuse, completed partial promotion,
invalid range/status/length/ETag responses, checksum failures, source identity
pins and exhausted retry budgets. These checks are not evidence that the full
dataset has imported.
