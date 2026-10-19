# Bulk prefill scaling: matched 100k and 200k samples

This experiment checks how startup cost changes when the selected contract count
doubles. The Rust runtime remains `b0f05c7`, using release binary SHA-256
`dbfb2e720683f2f07affadd17ad1e84ba7c3d7825fd803feec4919d81096142e`.
No cache, sorter, batching or memory-limit tuning is part of the comparison.

## Sample selection

The available R2 seed inputs include the previously tested uploaded 100k sample
and an imported 1m sample, but no prepared 200k sample. Their selection policies
differ. Comparing their contract counts alone would confound dataset composition
with scaling.

The source is
`s3://arkiv-sourcify-preload/seed-inputs/sourcify-imported-1m-contracts-never.tar.zst`,
16,863,877,008 archive bytes, SHA-256
`2cf8a1edc7ad4fa0c39b21c84e77377ffdb4b2d92b445a40b464a6da11c9132f`.
Its native `arkiv-prefill.postgres-imported-seed.v1` provenance records an
October 3, 2026 snapshot with 1,000,000 contracts and 4,238,890 entities.
The archive hash and all extracted checksum pins were verified.

The matched comparison selects the first 100,000 and first 200,000 audited
contract ordinals from the same pinned imported source and includes each set's
complete dependency closure. Source contract selection is nested; canonical
streams are not necessarily byte prefixes because selected creators receive
fresh contiguous nonces and dependency-safe expiry heights are recalculated.
Shared dependencies mean twice as many contracts need not mean twice as many
entities, input bytes, term memberships or storage units.

The source adapter verifies the full pinned input inventory and reports it
separately from the selected inventory. Selected rows receive payload, staging
identity, dependency and content validation before publication. No duplicate
copies of the old 100k stream are used to simulate a larger sample.

## Measurement method

Preparation and producer startup run as separate sequential phases. Each producer
uses a fresh database, the same runtime binary, a 32 GiB cgroup cap, no swap and
an 80 GiB free-space reserve. The producer measurement checks the expected
height-zero genesis and state roots before recording readiness and stopping the
node. Process CPU, RSS, kernel I/O accounting and cgroup memory/events are sampled
at two-second intervals. JSON events separate stream audits, validation/sort-run
production, index construction, and the final installation interval.

Compare phase times against actual canonical bytes, entity counts and term
memberships, as well as against contract counts. Kernel `write_bytes` and
`cancelled_write_bytes` are task accounting, not device-level write measurements.
Filesystem caches were not dropped between bundle authoring and producer startup;
these are single prepared-startup runs, not repeated cold-storage trials.
Two sample sizes can reveal a throughput regression over this interval; they
cannot establish asymptotic linearity or predict full-corpus performance.

## Code-derived scaling expectations

There is no special 100k/200k contract boundary. The four sorters split a 64 MiB
budget and merge with fan-in 16. Crossing a merge-level boundary can add another
read/write/hash pass; online merges occur during normalization, so compare
normalization and index construction together. Index entries expand each indexed
attribute into existence and value memberships. Sorted trie construction streams
completed subtrees with a bounded frontier.

Object insertion still uses hash-keyed redb B-trees and immediate-durability
batches with fixed caches. Final installation performs a second read/hash/write
pass; its 16,384-slot posting-root cache can repeat traversals after collisions.
These are possible sources of nonlinear timing, not conclusions about the samples.
Implementation references: `crates/arkiv-db/src/durable/bulk.rs`,
`crates/arkiv-db-trie/src/sorted.rs`, `crates/arkiv-db-redb/src/store.rs`, and
`crates/arkiv-db/src/durable/streaming.rs`.

## Results

Both producers reached height zero with the expected genesis and state roots,
then stopped successfully. The 200k run took **17m 54.545s**, versus **10m
08.312s** for 100k. Startup grew **1.766x**, while entities grew **1.636x**,
canonical bytes **1.527x**, and index memberships **1.699x**.

| Measurement | Matched 100k | Matched 200k | Ratio |
| --- | ---: | ---: | ---: |
| Contracts | 100,000 | 200,000 | 2.000x |
| Entities | 445,600 | 728,907 | 1.636x |
| Canonical bytes | 1,853,670,399 | 2,831,182,628 | 1.527x |
| Index memberships | 18,574,090 | 31,560,648 | 1.699x |
| Normalize / initial sort, seconds | 103.430 | 169.176 | 1.636x |
| Index construction, seconds | 238.619 | 440.848 | 1.847x |
| Combined bulk construction, seconds | 342.049 | 610.024 | 1.783x |
| Final installation interval, seconds | 211.907 | 375.505 | 1.772x |
| Stream audits, seconds | 51.821 | 83.086 | 1.603x |
| Total time to ready, seconds | 608.312 | 1,074.545 | 1.766x |
| User CPU, seconds | 458.24 | 796.13 | 1.737x |
| System CPU, seconds | 144.72 | 266.52 | 1.842x |
| Peak process RSS, KiB | 693,324 | 678,676 | 0.979x |
| Peak cgroup memory, bytes | 13,137,604,608 | 19,593,039,872 | 1.491x |
| Kernel write_bytes | 68,570,955,776 | 120,331,550,720 | 1.755x |
| Kernel cancelled_write_bytes | 2,837,651,456 | 3,854,086,144 | 1.358x |

Peak RSS was 677.1 versus 662.8 MiB; peak cgroup memory, including cache, was
12.24 versus 18.25 GiB. Both used a 32 GiB cap, zero swap, and had zero
memory-limit and OOM events. CPU time was 603.0/608.3 and 1,062.7/1,074.5
seconds respectively: these runs spent most of their wall time doing CPU work.
Physical read accounting was only 303,104 and 90,112 bytes respectively, which
also demonstrates the warm-cache limitation of this comparison.

This is modest superlinear growth relative to actual work, rather than evidence
of a severe scaling cliff over this interval: startup time per entity increased
about **8.0%**, per canonical byte **15.7%**, and per index membership **4.0%**.
The membership denominator is useful but incomplete: payload sizes, tree shape,
term sharing and distinct-term counts also affect cost. Do not call this proof
of linear scaling, or treat doubling contracts as doubling the workload.

Index construction alone grew 1.847x, but combined normalization and index work
grew 1.783x. The final installation interval grew 1.772x and still consumed about 35% of
startup. This interval includes staging finalization and open/install overhead,
so it does not isolate copy time alone.
The next producer optimization candidates remain bulk object insertion and the
second traversal/write during installation. This test does not isolate an exact
merge boundary or posting-cache collision as the cause of the throughput loss;
that would require further instrumentation. There was no 32 GiB memory-pressure
cliff in either producer run.

## What the membership count means for storage

The 31,560,648 count is the number of posting memberships processed, not the
number of contracts, entities, unique terms or full payload copies. Each indexed
attribute contributes existence and value memberships, including indexed system
fields. Posting trees contain record IDs and membership markers; term and trie
structure add storage overhead. Content-addressed objects may be shared, so the
logical membership count is not a physical-object count or a fixed byte total.

Final file allocation was measured after the producers stopped and private
staging was removed. GB below is decimal; allocated bytes use filesystem blocks,
not just apparent sparse-file length.

| Disk measurement | Matched 100k | Matched 200k |
| --- | ---: | ---: |
| Live node.redb allocated bytes | 5,105,946,624 | 7,760,429,056 |
| Live node.redb apparent bytes | 6,445,076,480 | 8,589,938,688 |
| Retained prefill.bin allocated bytes | 1,853,677,568 | 2,831,187,968 |
| All node files allocated bytes | 6,959,722,496 | 10,591,715,328 |

Thus the 200k node uses **7.76 GB for the live database**, plus **2.83 GB retained
input**, or **10.59 GB total**, including small history/genesis files. Live DB
allocation grew 1.520x. This includes entity data and all database/index overhead;
it does not isolate the size of posting indexes alone. The difference between
canonical-stream bytes and DB allocation is not a precise index-size estimate.

During final installation, a snapshot observed **18,345,562,112 allocated bytes**
in the 200k node directory, including a 7,753,834,496-byte staged database. This
is an observed temporary footprint, not a continuously sampled disk peak. Other
source archives, converted streams and bundles outside the node directory are
excluded. Cumulative kernel writes (120.33 GB) describe repeated write activity,
not final disk capacity, and must not be conflated with the 10.59 GB retained
footprint. Scratch was removed after successful installation.

## Preparation and validation

| Preparation phase | Matched 100k | Matched 200k |
| --- | ---: | ---: |
| Source conversion, seconds | 2,106.45 | 2,444.52 |
| Bundle authoring, seconds | 380.09 | 670.15 |
| Selected dependency edges | 1,122,598 | 1,837,772 |

These costs are excluded from producer readiness. Both conversions verify and
ingest the full 4,238,890-entity source inventory, audit all source contracts,
validate the selected dependency closure, prune unselected private rows and emit
the canonical stream. This preparation implementation is intentionally bounded
but expensive; the smaller final subset does not eliminate the whole-source scan.

The converter adapter passed its fixture suite and the optional bounded real
imported-source closure test; the unrelated processor integration was skipped
without its optional fixture. Both full-size conversions and bundle builds
passed. No Rust runtime changes were made for this comparison. This benchmark
validates producer genesis readiness and matching roots, not a new full/light
acceptance, query throughput or broad-proof-capacity test.

| Identity | Matched 100k | Matched 200k |
| --- | --- | --- |
| Prefill SHA-256 | `94b10c509992f22804fcacd38df019b2db65d3c3c8e1b4871c4ce21e734bec02` | `462578cc3d8da4a08e46ae57d2268bcf6912a8af386b235f6e3c1d3dec5bdd51` |
| State root | `0x8074f966a7984e696b911829c4c180d7f12d9b11ced7c63f108513715589aec0` | `0xc3ae5668224ea28777e15ad99fef731fdece811daf6ebfb411cca40908f381c2` |

Raw evidence is retained in the parent workspace under
`operations/runs/bulk-prefill-200k`: source report and pinned input, conversion
reports, bundle reports, preparation logs/resource samples, producer
`events.jsonl`, `samples.jsonl`, `result.json`, `comparison.json`, `ratios.json`
and `final-disk.json`. Producer 200k also has `disk-snapshots.jsonl`.

Reproduce selection with the converter's `--contract-limit 100000` or `200000`
against the same source; use the same funded base genesis and sponsor
`0x1a642f0e3c3af545e7acbd38b07251b3990914f1`. The evidence directory contains
`measure-producer.py`, `run-phase.py`, and `benchmark-plan.json` for the exact
measurement setup. Each producer must receive a fresh run directory and run
sequentially under `MemoryMax=32G`, `MemorySwapMax=0`, and
`ARKIV_PREFILL_RESERVE_GIB=80`.
