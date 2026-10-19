# Streaming genesis v2

The signed node now supports a file-backed, bounded-memory genesis prefill in
addition to the original small v1 builder. Both paths initialize the same entity
engine. Import checkpoints are private storage metadata, never chain blocks or
part of a state root. The final node starts at height zero.

## Build, verify and start

First convert a completed processor bundle using the
[Sourcify adapter](sourcify-prefill-converter.md). Preserve the original source.
Create a base `genesis.bin` using the existing TOML genesis commands; its
allocations must fund the selected rent sponsor, and its proposer must match the
key used to start the producer. The streaming builder discards the base's old
prefill, selects 64 total cells and recalculates the expiry reservation.

```sh
export ARKIV_PREFILL_RESERVE_GIB=80
arkiv-proposer genesis stream-build /data/base/genesis.bin \
  --prefill /data/converted/prefill.bin --out /data/bundle \
  --sponsor 0xYOUR_FUNDED_SPONSOR --batch-bytes 1048576
arkiv-proposer genesis stream-verify /data/bundle
arkiv-proposer producer --bundle /data/bundle --run-dir /data/producer \
  --key-file /private/proposer.key --listen 127.0.0.1:9500
```

Use the normal signed-node flags for the bearer-protected private listener,
full replication and explicitly pinned full/light identities. The producer
starts paused unless `--start running` is given. Do not reuse fixture keys for a
public deployment. No existing explorer or indexer needs to be redeployed to
bootstrap the node.

The full follower can use `--bundle` or download the pinned canonical stream
from its peer, independently replay it, and compare the result with genesis.
`GET /signed/v1/prefill/chunk?offset=N&length=M` serves at most 1 MiB binary
bytes per request under the existing four-worker limit. Downloads retain a
partial file for resumption; the complete hash and canonical framing are checked
before installation. Corrupt completed partial downloads are discarded for a
fresh retry. The original small-v1 JSON endpoint is retained.

The light follower downloads only bounded genesis/certificate metadata and
headers. It verifies requested proofs against authenticated roots; it does not
import the dataset or independently establish dataset completeness.

## Canonical identity and memory

V2 has an explicit launch version, `ARKPFL02` stream framing and separate hash
domains. V1 encodings, admission caps and builder behavior remain unchanged.
Each frame is bounded to 8 MiB and decoded independently. Frames have increasing
creator addresses and consecutive per-creator creation nonces. Attribute order,
value encoding, row counts, total length, trailing bytes and the incremental
Keccak commitment are checked.

A replay batch defaults to 1 MiB of canonical row bytes, plus at most one
bounded row. A durable preparation limit triggers a retry from the last
committed cursor with a smaller batch. Failure at one-row granularity is explicit.
Import-only decoded-node caching is capped at 256 MiB and 262,144 entries; each
import redb handle has a 256 MiB page cache, including a headless streaming
import's final destination. During object traversal, the two handles used by a
copy plus the source engine's decoded cache have a combined configured ceiling
of 768 MiB. Staging compaction also keeps the final destination open, bringing
this to 1 GiB. At generation publication, the replacement engine's 256 MiB
decoded-cache allowance briefly coexists with the source engine's allowance
until the source is dropped. The conservative simultaneous configured ceiling
is therefore 1.25 GiB: three redb caches and two decoded caches, excluding
ordinary history/runtime caches, batch buffers, traversal stacks and allocator
overhead. The replacement decoded cache starts cold; its configured allowance
does not imply immediate allocation of 256 MiB. A copy also uses a fixed
16,384-slot, 768 KiB cache of
successfully traversed posting roots, namespace ordinals and validated lengths.
Collisions only repeat work; they cannot omit objects or bypass count checks.
Existing node opens and history/header stores keep the default 8 MiB redb cache.
The process that installs a streaming genesis retains its 256 MiB destination
cache until it exits; subsequent opens use the normal default. No
whole-dataset entity array, key set, report array or reachable-object visited set
is retained in RAM. The importer reuses ordinary height-zero entity operations,
including typed indexes, expiry accounting, record allocators and creator nonces.

Batch/cache settings do not enter genesis identity. Roots and rent are tested
against the existing v1 builder on compatible fixtures and across batch sizes.
The v2 64-cell profile admits up to 55 user attributes, preserving the 29 user
attributes in the real Sourcify fixture. Lifetime remains finite; a new permanent
storage policy is not introduced.

## Recovery and publication

Each commit atomically stores its roots and bounded cursor metadata: input
identity, consumed rows/bytes, prefix hash, logical units, cumulative rent and
final sponsor-debit status. On restart the canonical prefix is streamed and
rehashed without re-executing its entities. This is bounded memory but takes time
proportional to the consumed input; it is not an O(1) seek-only recovery promise.

Staging generations compact after at least 1,024 batches and geometric physical
file growth. Each compaction emits one begin and one completion event with
generation, source/output file sizes and elapsed time. The replacement contains
only current reachable objects and its
checkpoint. Its database and genesis description are synced before switching
`CURRENT`; old generations are removed after the switch. A crash leaves the
selected generation authoritative and any unselected successor disposable.

Final installation copies reachable objects in bounded durable batches into a
separate headless store, then atomically writes the sole height-zero manifest and
node metadata. A failed copy can restart idempotently; it currently traverses
again from the beginning. Normal node open refuses an unfinished headless store.
Successful installation removes importer scratch. Bundle publication uses synced
files and an atomic no-replace directory rename.

A fresh producer supplied with `--bundle` performs three standalone stream
audits: the supplied bundle, the actual local copy, and the retained serving
file after final database installation. The local audit retains an open file
descriptor and a private summary bound to the signed genesis. An inode check
ensures the published hard link names that descriptor, and replay consumes that
same descriptor instead of reopening its pathname. Replay still checks every
frame, the full stream hash/count/length, recovered checkpoint prefixes, and
derived state, rent and storage units. The final audit catches pathname or
content changes during the final copy. Public standalone verification continues
to audit its input on every call. These checks replace the former immediate
duplicate local-file audit and the audit at replay entry; no filename or
timestamp is accepted as proof of file contents.

## Resource policy

Run heavy phases one at a time under an external 32 GiB cgroup limit, with swap
disabled. For example, on a systemd host use `systemd-run` with `MemoryMax=32G`,
`MemorySwapMax=0`, an explicit unprivileged user and
`--setenv=ARKIV_PREFILL_RESERVE_GIB=80`. The code's caches and batches provide the
memory bounds; the cgroup is an additional backstop.

The Rust library's portable default disk reserve is 1 GiB. **Set the environment
variable to 80 or higher for the real dataset.** It checks before replay batches,
stream copies/downloads and generation compaction, and projects another staging
file's size before final installation. These projections are conservative
checks, not a disk quota or proof of full-dataset fit. Account for input, converter
SQLite scratch, canonical output, staging, the next compacted/final database,
transport copies and reserve concurrently. Live history still has no physical
pruning.

The processor-to-stream converter has atomic publication but is not resumable.
The earlier Parquet-to-processor export also has its own memory characteristics;
this work does not establish bounded memory for that separate executable.

## Progress and measurement

The importer writes one-line JSON progress records to **stderr**, leaving stdout
for the final result JSON. Records use `event: "prefill_progress"` and stages
`audit`, `recover_prefix`, and `replay`. Each phase reports at its start, roughly
every ten seconds while processing, and on successful completion. Fields include
`rows`, `totalRows`, `bytes`, `totalBytes`, `elapsedSeconds`,
`checkpointSequence`, and `complete`. No source paths or credentials are logged.
Automatic preparation retries emit `prefill_batch_retry` with the smaller
`batchBytes` value and `reason: "prepare_resource_limit"`.

Replay counters advance only after a durable checkpoint commits. Recovery counts
prefix rows checked, not entities written again. Elapsed time restarts per phase
and retry; checkpoint sequence is private to a staging generation and resets on
compaction. A phase's `complete` record does not assert bundle publication or
full benchmark acceptance: require the command's successful exit and final JSON.
Compaction and final object copying can delay progress records while they run.

Capture stderr alongside external measurements of cgroup memory/peak and events,
process RSS, wall time, free disk, and allocated bytes for the staging and output
directories. Keep raw samples and the exact command, binary revision, input hash,
cache/batch settings, and restart history with the result. Measure physical disk
allocation separately from logical storage units. Run one heavy phase at a time;
progress rates from small or synthetic samples do not establish a full-dataset
completion estimate.

## Validation evidence, 2026-10-19

The isolated `scripts/streaming-prefill-smoke.py` accepts `--prefill` and a new
`--work-dir`, creates disposable fixture credentials, and stops every process on
exit. It passed on the real 16-contract / 263-entity Sourcify fixture:

- Independent full-follower download and replay; producer/full/light head agreement.
- Verified account and entity query proofs, including all 16 contract entities.
- Live signed write at block one, persisted receipt, and restart of all roles.
- No prefill file in the light-node directory.

After bounded posting reuse, the 256 MiB final-destination cache, and the
five-to-three producer audit reduction, the same release-build fixture passed
again with the unchanged genesis/state commitments. The isolated acceptance
run took 1.762 seconds and peaked at 52.9 MiB cgroup memory with no swap; this
small fixture does not measure large-import speedup. Focused validation also
passed 46 database library tests, three database streaming-import tests, two
redb streaming-import tests, twelve streaming/recovery tests and three proposer
tests. New cases cover cache collisions and namespace separation, malformed
posting counts, missing/corrupt objects, copy-flush failures and retry, retained
inode replacement, same-inode mutation/truncation, genesis binding and finalized
checkpoint recovery. Clippy with warnings denied, formatting and diff checks
passed. Logs and the smoke acceptance result are retained under
`operations/runs/producer-optimization` in the parent workspace.

A fresh producer then imported the original 100,000-contract / 445,583-entity
bundle (1,858,260,091 canonical bytes) and reached the expected height-zero
genesis hash and state root in **1,270.554 seconds (21m 10.6s)**. This producer
measurement used a fresh destination and the unchanged canonical bundle under
the 32 GiB memory cap, no swap and 80 GiB disk reserve. The process was stopped
after readiness; it did not rerun the full three-role acceptance at this scale.

| Measurement | Historical baseline | Optimized run |
| --- | ---: | ---: |
| Producer readiness | approximately 1,300–1,310 s | 1,270.554 s |
| Standalone audits | 5 passes, 89.944 s | 3 passes, 53.036 s |
| Replay including compaction | 958.354 s | 975.555 s |
| Compaction | 133.363 s | 132.430 s |
| Replay excluding compaction | 824.990 s | 843.125 s |
| Process physical bytes written | 169,436,819,456 | 168,471,179,264 |

The interval from replay completion to the final audit was 238.226 seconds.
It includes final installation and surrounding overhead; the inner object copy
was not separately timed. Peak process RSS was 965,676 KiB (943.0 MiB), while
peak cgroup memory was 15,356,203,008 bytes (14.30 GiB), including file cache.
All memory-event counters remained zero and no swap was used.

The observed readiness improvement is only about 2–3%, and physical writes fell
about 0.6%. These are historical comparisons, not a controlled repeated A/B
benchmark. The audit reduction is clear, but replay excluding compaction took
longer in this run and compaction was nearly unchanged. These changes do not
solve the main import bottleneck. Raw measurements are in the parent workspace's
`operations/runs/producer-optimization/producer-100k/result.json` and
`events.jsonl`; the input and original acceptance are documented in
[the 100,000-contract validation report](streaming-prefill-100k-validation.md).

Debug-build streaming construction of synthetic 29-attribute entities with
1 KiB payloads, on the local host, produced these preliminary measurements
using the original 64 KiB batches and 8 MiB caches (before the import tuning):

| Entities | Canonical bytes | Wall time | Peak process RSS | Peak cgroup memory |
| --- | ---: | ---: | ---: | ---: |
| 1,000 | 1,620,980 | 4.51 s | 35,916 KiB | 30.3 MiB |
| 10,000 | 16,349,480 | 61.62 s | 44,212 KiB | 121 MiB |

The time utility and cgroup use different accounting; cgroup values can include
file-cache charges and are not interchangeable with RSS. These small-scale
results are not a capacity estimate for the full Sourcify dataset.

A separate 10,000-row run was killed with SIGKILL during replay; no bundle was
published at interruption. Resumption produced the exact same genesis hash as
the uninterrupted run and removed completed scratch. This does not exhaust
all power-loss or disk-full timing windows.

Inputs beyond this 445,583-entity sample, disk amplification and sustained import
throughput remain the next scale gate.

The complete Rust workspace test run passed 1,174 tests with four intentionally
ignored tests and no failures. All 11 converter tests passed, including the
real fixture. Workspace formatting and Clippy with warnings denied passed.
