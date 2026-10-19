# Sourcify 100,000-contract streaming prefill validation

Recorded on 2026-10-19. Conversion of the actual uploaded Sourcify sample
completed, and the experimental genesis builder published a bundle after two
SIGKILL interruptions and checkpoint recoveries. The converter and builder
agree exactly on canonical identity, logical storage units and sponsored rent.
Producer/full/light acceptance passed on a resumed run, including selective
light proofs, a live write and restart. The initial acceptance attempt exposed
the existing broad equality-proof limit; that query still returns an explicit
resource limit. No light-verified total of 100,000 contracts is claimed.

This is a 100,000-contract sample containing 445,583 entities, not an import of
the entire Sourcify dataset. The run used the experimental repository at
`6d436bd` plus the local helper-adapter, progress, batch/cache tuning and
acceptance-harness changes accompanying this document. It was not an unchanged
build of that commit. The final containing commit identifies the resulting
implementation; the interrupted stages used different importer settings.

## Source and evidence

Local evidence is retained under:

```text
/home/ubuntu/arkiv-network/operations/runs/experiment-prefill-100k
```

The input is
`s3://arkiv-sourcify-preload/seed-inputs/sourcify-uploaded-100k-contracts-never.tar.zst`.
The archive is 1,669,206,175 bytes with SHA-256:

```text
d9d7e3c416dce70eea9a95d383284784657cd81e5a8fd0e4f6d1c12d92038b7e
```

`source-report.json` records the archive pin, extracted directory and successful
verification. The downloaded archive's checksum and extracted `SHA256SUMS`
were checked before conversion. The bundle is the helper's permanent derivation
of an uploaded PostgreSQL snapshot, not the newer offline processor format:

- Outer provenance: `arkiv-prefill.derived-permanent-seed.v1`.
- Embedded export: `arkiv-prefill.postgres-uploaded-seed.v1`.
- Source snapshot: `2026-10-02 12:31:36.852007+00`.
- Destination chain: `7706815`; 45 entity parts and 40 historical publishers.
- Selected contracts: Ethereum mainnet, 100,000 audited addresses/root keys.

The source's `never` expiry is converted to the experiment's finite,
dependency-safe expiry policy. Original uploaded keys are remapped to derived
experimental keys. Payload bytes, typed attributes, staging identities and
content locators are preserved and validated under the helper-specific schema.
This does not establish the historical snapshot's present-day RPC state.

| Evidence | What it establishes |
| --- | --- |
| `source-report.json`, source `provenance.json` and `SHA256SUMS` | Input identity and exported inventory |
| `converted/source-lock.json`, `converted/conversion-report.json` | Consumed file pins and completed conversion results |
| `convert.log`, `convert.time`, `convert.result.json`, `convert.samples.jsonl` | Converter success, process measurements and cgroup samples |
| `build.log`, `build-resume.log`, `build-tuned.log` | Interrupted replay, prefix recovery and final publication |
| `build-tuned.time`, `build-tuned.result.json`, `build*.samples.jsonl` | Final invocation measurements and sampled resource state |
| `bundle/report.json`, `bundle/genesis.bin`, `bundle/prefill.bin` | Published genesis identity and canonical input |
| `acceptance.log`, `acceptance.result.json`, `acceptance.time`, `acceptance.samples.jsonl` | Independent node initialization and the original broad-proof failure |
| `acceptance/acceptance.json`, `acceptance-resume.result.json`, `acceptance-resume.time`, `acceptance-resume.samples.jsonl` | Completed resumed acceptance and its measurements |

The separate `harness-check` and `harness-review` directories contain harness
checks. They are not the 445,583-entity acceptance result.

## Conversion result

| Entity kind | Count |
| --- | ---: |
| Verified contract | 100,000 |
| Code | 174,418 |
| Source file | 88,766 |
| Compilation | 79,112 |
| Spill blob | 3,287 |
| Total | 445,583 |

The completed converter validated 1,122,585 dependency edges. This helper
sample has at most 27 user attributes per entity; the experimental v2 profile
still permits 55 user attributes / 64 total cells. It is distinct from the
processor smoke fixture, which has 29 user attributes.

| Measurement | Result |
| --- | ---: |
| Original wire payload bytes | 1,611,736,246 |
| Decoded per-entity payload bytes | 5,617,368,482 |
| Canonical stream bytes | 1,858,260,091 |
| Logical storage units | 3,324,006,410 |
| Sponsored rent, wei | 9,684,277,486,347,000,000,000 |
| Earliest expiry height | 6,279,352 |
| Latest expiry height | 6,307,200 |
| Maximum scheduled expirations per height | 16 |

SQLite's configured page cache was 64 MiB. Input lines were bounded to 1 MiB;
one reconstructed logical spill payload was bounded to 64 MiB. Dataset-sized
rows, reference maps, contract audit, sort indexes and expiry queues remained
on disk. The converter completed with exit status zero and published its output
atomically. It does not provide resumable conversion checkpoints.

The output identities are:

```text
Canonical stream SHA-256:
77e99e2c109e7abd3d99c0176aeca17ef5f9d85ded78f851cfa7c9b2fd39fee0

Canonical stream commitment:
0x58691b67799592d2e3c8fd8520c65a0014230a0c5bdaff323f9adb5341bd1eb6

Source-lock canonical JSON SHA-256:
077547e55d3a3cba475d7e0f47c0e7f25a92245f92e85383b65d08b3d0706c20
```

## Builder recovery and publication

The first attempt used 64 KiB replay batches and was killed during replay.
The second used 1 MiB batches, recovered 31,919 committed rows covering
134,780,827 canonical bytes, then was also killed during replay. Its prefix
verification completed in 1.098 seconds according to `build-resume.log`.

The final invocation used 1 MiB batches with the tuned import caches: a decoded
node cache capped at 256 MiB / 262,144 entries and a 256 MiB redb cache per
import handle. It recovered 113,404 committed rows covering 472,645,498 bytes.
That prefix verification completed in 4.323 seconds, after another complete
canonical audit. Recovery verified the prefix rather than replaying those rows.

`build-tuned.log` records generation-3 compaction from an 8,589,938,688-byte
source file to a 4,294,971,392-byte output file in 168.427 seconds. These are
file lengths reported by the compactor, not allocated-disk or peak-space
measurements. The final replay completed all 445,583 rows and the command
returned zero with the same result as `bundle/report.json`:

```text
Genesis hash:
0x612bab0b9ee7e20f1a0fc99a4d6bda5db26291b69e0218eef1ac57df67d747ae

State root:
0xf91cbca6523233b6214baa3da89bab61144c7c817126122bb433d3fe94e7a458

Recipe hash:
0xf12114105b7fbfc7c23cf6d7ce007f6825f111c86cb62cf857211f57db99d480
```

The builder independently computed exactly the converter's 3,324,006,410
storage units and 9,684,277,486,347,000,000,000 wei rent. Its report also matches
the converter's row count, canonical byte length and stream commitment.

The published bundle contains the 1,049-byte genesis, canonical stream and
report. It is not a prebuilt producer/full database. Each full-state node still
replays the stream and checks the committed state root. Two successful resume
paths do not exhaust power-loss, disk-full or final-publication failure windows,
and this run has no separate uninterrupted builder result for root comparison.

## Measured resources and limits of the timing

Heavy phases were run sequentially with a 32 GiB cgroup memory limit, no swap
allowance and an 80 GiB disk-reserve policy. Builder samples explicitly record
`memory.max = 34359738368`; all sampled `memory.events` counters, including
`oom` and `oom_kill`, were zero. The earlier converter sampler retained
`memory.current` and `memory.peak` but not those event/limit fields.

| Phase | GNU time wall duration | Maximum process RSS | Highest recorded cgroup `memory.peak` |
| --- | ---: | ---: | ---: |
| Complete converter | 480.17 s | 109,496 KiB | 5,445,160,960 bytes |
| Final tuned builder invocation, resuming at row 113,404 | 884.87 s | 954,460 KiB | 9,677,180,928 bytes |
| Initial node acceptance, ending at broad-proof failure | 2,836.07 s | 960,044 KiB | 18,231,504,896 bytes |
| Resumed node acceptance, passed using installed databases | 105.25 s | 29,040 KiB | 1,703,219,200 bytes |

The measurement wrapper reported 482.08 and 886.21 seconds respectively;
its two-second sampling loop adds observation overhead. Its maximum sampled
`memory.current` values were 5,402,693,632 and 9,677,176,832 bytes. These differ
from the cgroup's recorded high-water mark. Cgroup memory includes charged file
cache and kernel memory; it is not interchangeable with process RSS. Both
successful commands reported zero swaps.

The initial node acceptance took 2,836.69 seconds in its wrapper, about 47m16s,
and exited with status one at the broad-query assertion. It had already
initialized the producer and full follower independently. Process appearances
in the resource samples put producer startup at approximately 22 minutes and
full-follower download/startup at approximately 25 minutes; these are coarse
phase estimates, not isolated benchmarks. The resumed acceptance reused those
installed databases and completed with status zero in 106.02 wrapper seconds.
It is not another full dataset import. Maximum sampled `memory.current` was
18,231,267,328 bytes initially and 1,691,582,464 on resume. Both acceptance
sample sets record the 32 GiB limit and zero sampled OOM events; both GNU time
reports record zero swaps.

**The builder timing is mixed and interrupted, not a clean end-to-end benchmark.**
The final invocation excludes the first 113,404 rows' original replay, includes
prefix verification, compaction and publication, and ran after two earlier
attempts with changed settings. Its 842.619-second replay-phase counter also
includes work after recovery, not a complete cold replay. The empty
`build.time` and `build-resume.time` files cannot supply complete GNU time
results for the killed processes. Do not sum sampled durations into a claimed
controlled benchmark or extrapolate full-dataset throughput from them.

At inspection after publication, `du -B1 -s` reported 2,049,384,448 allocated
bytes for `converted` and 1,858,277,376 for `bundle`. These exclude source,
scratch and the independently constructed acceptance databases. The lowest
recorded free-space sample during the final builder invocation was
1,187,959,402,496 bytes. There was ample headroom, but these observations do not
measure the peak combined disk amplification or prove full-dataset fit.

After final acceptance and shutdown, allocated directory sizes were
6,981,832,704 bytes for the producer, 6,981,836,800 for the full follower and
122,880 for the light follower. These totals include retained canonical input
and logs where present; they are not database-only sizes. All test nodes were
confirmed stopped after the run.

## Reproduction commands

Use the already verified source or independently fetch and verify the pinned
archive above. R2 credentials are intentionally absent from these commands.
Use a fresh output directory for a new conversion/build: completed outputs
are never overwritten. These commands record the relevant executable arguments;
the original builder base came from an earlier disposable harness fixture.

```sh
cd /home/ubuntu/arkiv-network/arkiv-state-db-experiment
PREFILL_RUN=/home/ubuntu/arkiv-network/operations/runs/experiment-prefill-100k
PREFILL_SOURCE="$PREFILL_RUN/source/sourcify-uploaded-100k-contracts-never"
PREFILL_BASE=/home/ubuntu/arkiv-network/operations/runs/experiment-prefill-smoke/acceptance-3/base/genesis.bin
export ARKIV_PREFILL_RESERVE_GIB=80

/usr/bin/time -v python3 scripts/sourcify-prefill.py convert \
  --bundle "$PREFILL_SOURCE" --out "$PREFILL_RUN/converted"

/usr/bin/time -v target/release/arkiv-proposer genesis stream-build \
  "$PREFILL_BASE" --prefill "$PREFILL_RUN/converted/prefill.bin" \
  --out "$PREFILL_RUN/bundle" \
  --sponsor 0x1a642f0e3c3af545e7acbd38b07251b3990914f1
```

The final command used the tuned 1 MiB default; `--batch-bytes 1048576` makes
that choice explicit. Repeat a failed builder invocation with the same base,
input and output path to recover its private checkpoint. A finished bundle
requires a new output path for another build.

Run heavy commands within the external resource envelope, for example:

```sh
sudo systemd-run --unit=arkiv-prefill-100k-acceptance-reproduce \
  --wait --pipe --collect --uid=ubuntu \
  --property=MemoryMax=32G --property=MemorySwapMax=0 \
  --setenv=ARKIV_PREFILL_RESERVE_GIB=80 \
  --working-directory=/home/ubuntu/arkiv-network/arkiv-state-db-experiment \
  /usr/bin/python3 scripts/streaming-prefill-smoke.py \
  --bundle "$PREFILL_RUN/bundle" \
  --work-dir "$PREFILL_RUN/acceptance-reproduce" \
  --bin /home/ubuntu/arkiv-network/arkiv-state-db-experiment/target/release/arkiv-proposer \
  --timeout 7200 --expected-entities 445583 --expect-contract-proof-limit \
  --query 'address = addr(0x00000000000009ca62911fdd88c8cf50565ab183)'
```

The harness creates disposable local credentials itself and terminates its
nodes on exit. No private key or bearer token needs to be copied into a command
or this document. For a fresh reproducible fixture base and bundle, replace
`--bundle "$PREFILL_RUN/bundle"` with
`--prefill "$PREFILL_RUN/converted/prefill.bin"`; the harness creates its base,
builds/verifies the stream bundle, then runs acceptance. Use a new work directory
each time. This is a local test harness, not a public deployment procedure.

The original failing run used `--expected-contracts 100000`. The corrected
command deliberately uses `--expect-contract-proof-limit`, which requires
`ProofLimitExceeded` with `resource_limit` both before and after restart and
does not claim a verified broad-query total. The successful recovery used the
same command with `--work-dir "$PREFILL_RUN/acceptance" --resume`, reusing the
initially installed databases. Resume only accepts an incomplete height-zero
fixture before live transaction preparation; it refuses a completed run.

## Producer/full/light acceptance: passed with the broad-proof limit recorded

The first large-sample attempt initialized the producer and full follower, then
stopped at a light query for `kind = str('verified_contract')` with `limit=1`.
`acceptance.log` records `ProofLimitExceeded`; the harness stopped its processes
and retained the databases. The resumed harness made that expected resource
limit explicit, completed the remaining checks and exited successfully.

This exposed an existing proof-profile limit, not an import failure. Equality
profile `eq-page-v1` carries the **entire matching posting set on every page**,
even when one row is requested. Its default maximum is 4,096 posting IDs; the
100,000-contract query exceeds that cap and is rejected before the set is
materialized. The verifier reconstructs the complete posting trie and also
applies cumulative work/allocation limits. Raising only the posting cap would
not provide a bounded page-proof implementation. No proof caps or protocol
were changed for this validation. See [equality proof resource rules](eq-proof-charges.md).

`acceptance/acceptance.json` records `result: "passed"` and `resumed: true`:

- The producer and full follower independently initialized from the same
  canonical stream and matched the published genesis identity. The full
  follower downloaded the stream, and its SHA-256 matched the original input.
- The canonical row count was 445,583. The light node verified account proofs
  and one entity proof for
  `address = addr(0x00000000000009ca62911fdd88c8cf50565ab183)`.
- A signed live write committed at height one, with a committed receipt and
  verified account proof. All three roles restarted, retained the same head,
  and preserved the receipt, historical account proof and selective entity
  query results.
- The broad contract query returned the expected `ProofLimitExceeded` /
  `resource_limit` before and after restart. The report explicitly records
  `authenticatedVerifiedContractCount: null`, `expectedContracts: null` and
  `broadContractProof: "resource_limit"`.
- The light node had no prefill file. It did not import the dataset or establish
  dataset completeness independently.

The final agreed height-one head is:

```text
Block hash:
0xec19a11d667e416c005546d8c6b015bf1691988e5bb0b89b72293ce507296ee2

State root:
0x669a260f8658004d6d9feaa4129cf7544096747df305525e9e7a8675be51be75
```

The 100,000-contract inventory is established by source/converter audit, not
by a light-verified 100,000-match count or a broad one-row equality page. The
successful selective proof stays within the existing posting and work limits.

## Code validation

The full Rust workspace run passed 1,174 tests with four intentionally ignored
tests before cache tuning. After tuning, the node suite passed 328 tests with
no failures; eight focused tests, formatting and Clippy with warnings denied
also passed. All 15 converter tests passed with both real source fixtures
enabled. The actual resumed 445,583-entity harness run supplies the integration
evidence above. These checks do not replace an uninterrupted scale benchmark
or exhaustive crash/failure-window testing.

See [streaming genesis](streaming-genesis-v2.md) for protocol/recovery details
and [the converter](sourcify-prefill-converter.md) for source validation and
finite expiry policy. The previous 263-row and synthetic tests remain separate
evidence. No full Sourcify import, permanent expiry policy, live-history physical
pruning, or public deployment is established by this sample run.
