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
  --sponsor 0xYOUR_FUNDED_SPONSOR --batch-bytes 65536
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

A replay batch defaults to 64 KiB of canonical row bytes, plus at most one
bounded row. A durable preparation limit triggers a retry from the last
committed cursor with a smaller batch. Failure at one-row granularity is explicit.
The existing decoded-node and redb caches remain separately bounded. No
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
file growth. The replacement contains only current reachable objects and its
checkpoint. Its database and genesis description are synced before switching
`CURRENT`; old generations are removed after the switch. A crash leaves the
selected generation authoritative and any unselected successor disposable.

Final installation copies reachable objects in bounded durable batches into a
separate headless store, then atomically writes the sole height-zero manifest and
node metadata. A failed copy can restart idempotently; it currently traverses
again from the beginning. Normal node open refuses an unfinished headless store.
Successful installation removes importer scratch. Bundle publication uses synced
files and an atomic no-replace directory rename.

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

## Validation evidence, 2026-10-19

The isolated `scripts/streaming-prefill-smoke.py` accepts `--prefill` and a new
`--work-dir`, creates disposable fixture credentials, and stops every process on
exit. It passed on the real 16-contract / 263-entity Sourcify fixture:

- Independent full-follower download and replay; producer/full/light head agreement.
- Verified account and entity query proofs, including all 16 contract entities.
- Live signed write at block one, persisted receipt, and restart of all roles.
- No prefill file in the light-node directory.

Debug-build streaming construction of synthetic 29-attribute entities with
1 KiB payloads, on the local host, produced these preliminary measurements:

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

Larger representative inputs, disk amplification and long-running import
throughput remain the next scale gate.

The complete Rust workspace test run passed 1,174 tests with four intentionally
ignored tests and no failures. All 11 converter tests passed, including the
real fixture. Workspace formatting and Clippy with warnings denied passed.
