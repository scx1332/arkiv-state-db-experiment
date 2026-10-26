# Streaming genesis v2

The signed node now supports a file-backed, bounded-memory genesis prefill in
addition to the original small v1 builder. Both paths initialize the same entity
engine. Version two uses dedicated bulk construction, not incremental entity
replay. Private scratch is never chain history. The final node starts at height
zero. See [the bulk construction design](bulk-prefill.md).

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
  --sponsor 0xYOUR_FUNDED_SPONSOR
arkiv-proposer genesis stream-verify /data/bundle
arkiv-proposer producer --bundle /data/bundle --run-dir /data/producer \
  --key-file /private/proposer.key --listen 127.0.0.1:9500
```

To build the genesis state only once, keep the bundle builder's verified state
and hand it to the producer. The bundle is byte-identical either way:

```sh
arkiv-proposer genesis stream-build /data/base/genesis.bin \
  --prefill /data/converted/prefill.bin --out /data/bundle \
  --sponsor 0xYOUR_FUNDED_SPONSOR --state-out /data/genesis-state
arkiv-proposer producer --bundle /data/bundle --prebuilt-state /data/genesis-state \
  --run-dir /data/producer --key-file /private/proposer.key --listen 127.0.0.1:9500
```

The producer then skips normalization and index construction; see
[Build once](#build-once-prebuilt-state). Without `--prebuilt-state` it rebuilds
the state from the stream as before.

Use the normal signed-node flags for the bearer-protected private listener,
full replication and explicitly pinned full/light identities. The producer
starts paused unless `--start running` is given. Do not reuse fixture keys for a
public deployment. No existing explorer or indexer needs to be redeployed to
bootstrap the node.

The full follower can use `--bundle` or download the pinned canonical stream
from its peer, independently build its state, and compare the result with genesis.
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
domains. Its header is `ARKPFL02 ‖ u32(3) ‖ u64(row_count)`: row codec 3, the
user-defined-columns row of [the v1 contract](genesis-bundle-v1.md), which both
framings share (codec 2, the payload-era row, is refused). Each frame is
`u32(frame_length) ‖ Entity`, exactly the small stream's row encoding.
Each frame is bounded to 8 MiB and decoded independently. Frames have increasing
creator addresses and consecutive per-creator creation nonces. Attribute order,
value encoding, row counts, total length, trailing bytes and the incremental
Keccak commitment are checked.

The bulk builder assigns record IDs in canonical input order, validates entity
cells and expiry, and calculates rent and storage units. It spills key bindings,
rows, term/record memberships and host entries into sorted runs. The sorting
budget is 64 MiB total across four sorters, plus bounded individual entries and
merge buffers. Each merge opens at most sixteen runs. Run lengths and streaming
Keccak digests are verified before completion. A common posting list is streamed;
it is never accumulated as a whole in memory.

A sorted Merkle Patricia builder retains only its current key/value and a bounded
frontier of child references. Completed nodes are emitted to bounded object
batches (normally 32 MiB and 100,000 objects). This avoids incremental per-entity
index updates and intermediate committed roots. Existing codecs determine the
same final node encodings and state root.

Import redb handles have 256 MiB page caches. Decoded-node caching for the opened
staged engine is capped at 256 MiB and 262,144 entries. Final copying uses two
redb handles and one source decoded cache: a combined configured ceiling of
768 MiB, excluding object buffers, traversal stacks, ordinary history/runtime
caches and allocator overhead. A copy also uses a fixed 16,384-slot, 768 KiB
cache of successfully traversed posting roots, namespaces and validated lengths.
Collisions repeat work without omitting validation. Existing node opens and
history/header stores keep the default 8 MiB redb cache. The installing process
retains its larger destination cache until exit.

Buffer/cache settings do not enter genesis identity. Roots and accounting are
compared with the existing v1 builder on compatible fixtures. A frame carries
at most 32 user attributes, as a live create does; with no payload or content
type cells, the seven system cells leave all 32 to the user. Indexed
attributes (cell kind `01`) are posted; unindexed ones (`02`, and every
`bytes` value) are stored only. A row's `bytes` values hold at most 128 KiB
together.
Lifetime is finite in a version-2 launch. A version-4 launch, built on a base
genesis authored with `[prefill] allow_permanent = true`, also admits rows whose
expiry is the `u64::MAX` never-expires sentinel. They are created permanent,
never enter the expiry schedule (the 16-per-height capacity does not apply) and
prepay no rent, while their storage units still count. Finite rows are built and
charged exactly as in version 2. The version and recipe domain enter chain
identity; the stream format and bulk algorithm are unchanged. See
[unlimited expiry](sourcify-prefill-converter.md#unlimited-expiry---expiry-policy-unlimited).

## Recovery and publication

Bulk construction uses separate `bulk-v1` scratch with an input/genesis identity
and an exclusive lock. Incomplete scratch is rebuilt from the beginning after an
interruption. This is safe restart behavior, not committed-prefix resumption.
Completed state has a durable engine head and a checksummed receipt binding the
genesis, entity count, storage units and rent. It is selected by a directory
rename only after metadata is synced. Restart reaudits the canonical stream and
validates the receipt and engine association before reusing completed state.
Old incremental checkpoints are never interpreted as bulk output.

Final installation copies reachable objects in bounded durable batches into a
separate headless store, then atomically writes the sole height-zero manifest and
node metadata. A failed copy can restart idempotently; it currently traverses
again from the beginning. Normal node open refuses an unfinished headless store.
Successful installation removes importer scratch. Bundle publication uses synced
files and an atomic no-replace directory rename.

`producer --genesis-install adopt` installs without the copy. It reads and checks
the same reachable objects in parallel without writing them, relabels the staged
store in one durable commit, and renames it over the headless `node.redb`. No
second database copy is written or stored. See
[Adopting the staged genesis store](genesis-adoption.md).

A fresh producer supplied with `--bundle` performs three standalone stream
audits: the supplied bundle, the actual local copy, and the retained serving
file after final database installation. The local audit retains an open file
descriptor and a private summary bound to the signed genesis. An inode check
ensures the published hard link names that descriptor, and construction consumes that
same descriptor instead of reopening its pathname. Construction checks every
frame, the full stream hash/count/length, and derived state, rent and storage
units. The final audit catches pathname or
content changes during the final copy. Public standalone verification continues
to audit its input on every call. These checks replace the former immediate
duplicate local-file audit and the audit at replay entry; no filename or
timestamp is accepted as proof of file contents.

## Build once (prebuilt state)

Bundle authoring already constructs the complete genesis state database to
derive the signed state commitment. Without a handoff, the producer audits the
same stream and constructs the same database again: on the full Ethereum run,
`stream-build` took 7 h 29 m and the producer repeats its roughly 7.2 h of
normalization and index construction before installation even starts.

`stream-build --state-out DIR` keeps the completed private bulk directory
instead of deleting it. Before the stream audit begins, it checks that `DIR`
does not exist, that its parent exists, and that it does not overlap the bundle
or its work directory. It also probes a real rename from the work directory's
parent into `DIR`'s parent, so separate mounts are refused within seconds. After
construction, a synced `prebuilt.json` binds the store to the bundle's genesis hash,
state root, stream hash, row count and import identity. The bundle is published
next, then the state directory is renamed into place with no-replace semantics.
If that last rename fails, the bundle is valid and the error names the bound
store left at `.<bundle>.stream-work/bulk-v1`. That directory is itself a valid
prebuilt state directory: move it or pass it to `--prebuilt-state`, or delete it
and let the producer rebuild.

`producer --bundle B --prebuilt-state DIR` is accepted only by a producer with a
streaming bundle. While holding the node database lock, a fresh node checks that
`DIR` is a real directory with the expected import identity, manifest and
completed store files. A mismatched or incomplete store is refused before
anything moves. The store is then renamed into `prefill-work/bulk-v1`, or on
another filesystem copied into an unselected directory, synced and renamed. The
rename consumes `DIR`; a copy leaves it. An interrupted copy is discarded and
repeated, and a restart after the rename reuses the selected store. Once the
node has a head, the option is ignored. If reuse or installation rejects the
selected store (for example `StorageCorrupt` or a receipt mismatch), every restart
fails the same way until the run directory's `prefill-work` is deleted. The
headless `node.redb` may stay. Its copied objects are content-addressed and are
only published by a later successful installation.

From there the producer takes its existing completed-state restart path, so the
checks before serving are unchanged:

1. The canonical stream is audited three times as before (bundle, local copy,
   retained file after installation), plus once when reusing the completed store.
2. The store's import identity binds the base genesis, limits, sponsor and stream
   summary. Its checksummed receipt must match the row count and admit the storage
   units and rent. The engine genesis must match the expected deployment, limits
   and pricing.
3. The store's committed state header, rent and storage units must equal the signed
   genesis commitment.
4. Installation copies every object reachable from that root into the headless
   node database. Every load checks `keccak256(bytes) == id`, map entry counts,
   row admission limits and host value bounds. Only then is the height-zero
   manifest, identity and certificate packet published. A tampered object fails
   with `StorageCorrupt` and no head is ever published.

What the producer no longer does is derive the state from the stream a second
time with the same binary on the same input. That rebuild was a determinism
self-check within the operator's own trust domain, the domain that also produced
the commitment, rather than an independent verification. **Use only a state
directory from your own `stream-build --state-out` run for exactly this bundle.**
Its identity and receipt digests are unkeyed: they catch mix-ups and corruption
of the store, but not a forger who also controls `genesis.bin`. Independent
derivation remains with full followers, `genesis stream-verify`, and any producer
started without `--prebuilt-state`. The prebuilt directory is private local
state. Never distribute it as part of a bundle, and never give it to a follower;
the follower roles refuse the option.

### Build-once measurements, 2026-10-20

These are single sequential runs of one release binary on the shared host, at
`nice 19`/idle I/O priority while the full Ethereum producer was running beside
them. There was no cgroup cap; the free-space reserve was 80 GiB. They are not
repeated controlled trials.

The 263-entity Sourcify fixture went through `scripts/run-full-prefill.py`
(convert, stream-build, producer ready) twice: in default build-once mode and
with `--rebuild-genesis-in-producer`. Both produced identical bundle reports and
heads, with genesis `0x12832bb2…2281` and state root `0x7dd55378…ac27`. All 263
entities, and the 16 `kind = str('verified_contract')` keys, read back through
`arkiv_query` identically. The build-once producer logged one `moved` install and
no normalization or index phase.

On the matched 100k stream (445,600 entities, 1,853,670,399 bytes, base and
sponsor as in [the scaling report](bulk-prefill-scaling.md)):

| Phase | Producer rebuild | Build once |
| --- | ---: | ---: |
| `stream-build` wall time | 406.8 s | 407.3 s |
| Producer ready | 697.2 s | **325.4 s** |
| Producer stream audits | 3 × ~18 s | 4 × ~17.5 s |
| Producer normalization | 108.8 s | none |
| Producer index construction | 252.8 s | none |
| Installation interval | 277.9 s | 251.4 s |
| Producer kernel `write_bytes` | 67.7 GB | 30.6 GB |
| End to end (build + ready) | 1,104.0 s | 732.7 s |

Both producers reached the historical 100k commitment: state root
`0x8074f966…aec0`, genesis `0xa1182828…793f`. The kept state directory used
5.1 GB, and the sampled peak of the node directory was 12.06 GB in both modes.

At full scale (15,298,744 entities), build once removes the producer's repeat of
5,499 s normalization and 20,441 s index construction. It adds one reuse audit
of about 480 s, so the projected saving is about **7.1 h**. The producer still
performs four stream audits (about 32 min at 48.2 GB) and the final installation
copy. That copy is now the dominant producer cost; at 100k it was 77% of
build-once readiness.

## Resource policy

Run heavy phases one at a time under an external 32 GiB cgroup limit, with swap
disabled. For example, on a systemd host use `systemd-run` with `MemoryMax=32G`,
`MemorySwapMax=0`, an explicit unprivileged user and
`--setenv=ARKIV_PREFILL_RESERVE_GIB=80`. The code's caches and batches provide the
memory bounds; the cgroup is an additional backstop.

The Rust library's portable default disk reserve is 1 GiB. **Set the environment
variable to 80 or higher for the real dataset.** Checks cover ingestion, sort
flushes, merges, object batches and stream copies/downloads. Long merges recheck
the reserve periodically; final installation projects another database copy. These projections are conservative
checks, not a disk quota or proof of full-dataset fit. Account for input, converter
SQLite scratch, canonical output, sorted runs, staged/final databases,
transport copies and reserve concurrently. Live history still has no physical
pruning.

The processor-to-stream converter has atomic publication but is not resumable.
The earlier Parquet-to-processor export also has its own memory characteristics;
this work does not establish bounded memory for that separate executable.

## Progress and measurement

The importer writes JSON progress records to **stderr**, leaving stdout for
result JSON. Stream phases use `event: "prefill_progress"` with `audit`,
`bulk_normalize` and `bulk_indexes`. Counts describe phase progress, not committed
chain blocks. A completed phase does not assert bundle publication or full
acceptance: require successful command completion and the final result.
`prefill_bulk_phase` records distinguish keys, rows, postings, host construction
and long sort merges, with phase-local entry counts.

The deprecated `stream-build --batch-bytes` option is accepted for compatibility
and controls ingestion progress sampling only. It does not enable incremental
replay or change the fixed sort-buffer budget.

Capture stderr alongside external measurements of cgroup memory/peak and events,
process RSS, wall time, free disk, and allocated bytes for the staging and output
directories. Keep raw samples and the exact command, binary revision, input hash,
cache/buffer settings, and restart history with the result. Measure physical disk
allocation separately from logical storage units. Run one heavy phase at a time;
progress rates from small or synthetic samples do not establish a full-dataset
completion estimate.

## Historical incremental validation, 2026-10-19

The measurements below describe the previous incremental importer. Bulk results
are recorded separately in [the bulk report](bulk-prefill.md).

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
