# golem-db's load benchmark on our engine

Measured 2026-11-27. golem-db's load benchmark (`crates/bench`, golem-db pull
request 182, a draft, head `514ccc9`) drives a database through the public
`Api`: one writer commits blocks of creates, updates and deletes with pruning
on, and queries run between blocks. This page records how it runs on our
engine through [`golem-api`](../golem-api/README.md), what it measured, and
what was decided from it.

## Running it

The benchmark is not written against the API alone. It reaches into MDBX in
three places, so it needs a patch, [`load-bench.patch`](../conformance/golem-db/load-bench.patch)
(65 changed lines, no scenario or query touched):

| What the benchmark does | With an engine that is not golem-db's |
| --- | --- |
| Opens the MDBX store itself and calls `Database::from_store` | `Database::open_with_options` |
| Reads MDBX's page counters for bytes written and syncs per commit | Bytes from `/proc/self/io` (`write_bytes`); syncs are reported as 0 |
| Reads MDBX's table sizes in a child process | One line per file of the database directory |

The two "bytes written" columns therefore come from different sources and
compare only roughly.

```sh
conformance/golem-db/ci.sh setup-bench 514ccc95c816fcd17935b7c39da87ad44d835e17
conformance/golem-db/ci.sh bench-smoke                      # both engines; match counts must agree
BENCH_MEMORY_MAX=8G conformance/golem-db/ci.sh bench arkiv golemdb /data/out-golemdb /data/db
BENCH_MEMORY_MAX=8G conformance/golem-db/ci.sh bench arkiv arkiv   /data/out-arkiv   /data/db
```

`GOLEMDB_BACKEND` selects the engine, as everywhere in the kit. The
benchmark is not in CI: it is a draft upstream.

## Setup

- Machine: Ryzen 5 3600 (6 cores, 12 threads), 62 GiB of memory, RAID 0 of
  three NVMe drives, ext4. The Kenobi follower was running throughout (one
  small commit every 2 s).
- Each run in its own 8 GiB memory cgroup, page cache dropped first, one
  run at a time.
- golem-db `f6dae1d` on MDBX; our engine at `5f4a542` on redb with the
  adapter's defaults: state window from the scenario (1,000 commits) with
  state garbage collection on, as the node runs; redb page cache 256 MiB;
  object cache 8 MiB and 16,384 entries; quick repair on.
- The first runs of our engine were made before the adapter turned garbage
  collection on: it set the retention window but never deleted anything,
  which the node does not do. Those runs are marked "collection off" below.
  Collection does not change query times; it doubles the write cost.
- Scenario: `arkiv.toml` of the pull request (records with eight indexed
  attributes and a payload of 1 KiB to 100 KiB), cut to 10,000 commits:
  8,000 blocks of 17 creates, then 2,000 blocks of 26 operations (35%
  creates, 30% updates, 35% deletes). The 15 queries run after each phase,
  at 136,000 records. Same seed, so both engines get the same operations.

## Result: 10,000 commits

Every query matched and returned the same number of records on both engines
in both rounds.

| | golem-db | Ours | Ours / theirs |
| --- | ---: | ---: | ---: |
| **Fill, 8,000 blocks** | | | |
| Time in blocks | 158 s | 474 s | 3.0 |
| Seal p50 | 5.0 ms | 1.2 ms | 0.24 |
| Commit p50 | 10.9 ms | 56.2 ms | 5.2 |
| Block p50 | 17.1 ms | 62.2 ms | 3.6 |
| Block p99 | 93.6 ms | 82.6 ms | 0.9 |
| **Mixed, 2,000 blocks** | | | |
| Time in blocks | 68 s | 268 s | 4.0 |
| Seal p50 | 9.9 ms | 2.6 ms | 0.26 |
| Commit p50 | 17.5 ms | 118.5 ms | 6.8 |
| Block p50 | 29.2 ms | 133.7 ms | 4.6 |
| Block p99 | 121.7 ms | 164.8 ms | 1.4 |
| **Storage** | | | |
| Written per commit, fill | 1.2 MiB | 16.8 MiB at a 4 GiB file | 14 |
| Written per commit, mixed | 2.2 MiB | 36.0 MiB | 16 |
| File at the end | 2.3 GiB | 4.0 GiB, 2.7 GiB of it in tables | 1.7 |

golem-db's bytes are MDBX's page counters (p50). Ours are the store's own
count of what it handed to the file (mean over 1,000 commits, from the run
with the statistics log); the kernel's per-process count for the same
commits is higher (30.9 MiB and 52.6 MiB, p50), and the two were not
reconciled.

With collection off, as first measured: commit p50 23.6 ms and 44.6 ms,
block p50 29.7 ms and 61.4 ms, 8.0 MiB and 14.8 MiB written per commit, and
a 6.0 GiB file.

Queries, p50 after commit 10,000 (100 rows and the total, unless noted):

| Query | Matches | golem-db | Ours | Ours / theirs |
| --- | ---: | ---: | ---: | ---: |
| `status` (25%) | 34,245 | 0.89 ms | 0.41 ms | 0.45 |
| `status`, 10 rows | 34,245 | 0.50 ms | 0.05 ms | 0.09 |
| top `project` | 30,218 | 0.84 ms | 0.39 ms | 0.47 |
| top `$owner` | 13,904 | 0.64 ms | 0.42 ms | 0.65 |
| random `$owner` | 171 | 0.56 ms | 3.4 ms | 6.0 |
| random `ref` | 453 | 0.55 ms | 2.7 ms | 4.9 |
| one record by `id` | 1 | 0.009 ms | 0.11 ms | 12 |
| top `project` and `status` | 7,423 | 1.3 ms | 54.9 ms | 42 |
| `project` 40 and `status` | 187 | 0.92 ms | 12.8 ms | 14 |
| random `project` and `status` | 2,522 | 1.1 ms | 41.7 ms | 37 |
| random `project` and `entityType` | 735 | 0.88 ms | 29.5 ms | 34 |
| `$expiresAt` range, 1% | 1,354 | 1.7 ms | 1.9 ms | 1.1 |
| `$expiresAt` range, 10% | 13,486 | 27.8 ms | 192 ms | 6.9 |
| `$expiresAt` range, 50% | 67,857 | 64.9 ms | 1,571 ms | 24 |
| random `project` and range 10% | 340 | 27.7 ms | 284 ms | 10 |

What the numbers say:

- **Seal** is four times faster on our engine.
- **Commit** is 5 to 7 times slower and writes about 15 times as many
  bytes. [What a commit writes](#what-a-commit-writes) has the cause.
- **Tail**: our block time is steadier (p99 is 1.2 to 1.3 times our p50;
  theirs is 4 to 5.5 times its p50), but with our slower median the p99s are
  about level.
- **Single hot term**: we are 1.5 to 11 times faster. The size of a term's
  posting list is stored with the term, so the total costs nothing.
- **Two terms with AND**: 14 to 42 times slower. golem-db keeps each term's
  postings as roaring bitmap containers in a flat table that is overwritten
  in place and readable at the head only, so an AND is a bitmap
  intersection. Our postings are one entry per record in the versioned
  state tree, which is what lets a query run at any retained block; an AND
  merges those entries. In a small in-memory run the count was nearly all
  of the time and the page itself was cheap (count 0.46 ms, one row plus
  total 0.47 ms, 100 rows plus total 0.89 ms; golem-db 0.04, 0.04 and
  0.33 ms). The benchmark asks for the total on every page.
- **Wide ranges**: 7 to 24 times slower, growing with the share matched.
- **Small lookups** (one record, a rare owner or ref): 5 to 12 times
  slower. Not investigated.

## What a commit writes

From the store's counters (`ARKIV_STATS_LOG`), per commit of 17 creates:

| File | Collection | Objects written | Object bytes | Page writes | Bytes written |
| ---: | --- | ---: | ---: | ---: | ---: |
| 0.5 GiB | off | 525 | 203 KiB | 885 | 3.7 MiB |
| 2 GiB | off | 580 | 240 KiB | 1,475 | 6.3 MiB |
| 4 GiB | off | 635 | 245 KiB | 1,849 | 8.0 MiB |
| 0.5 GiB | on | 525 | 203 KiB | 1,402 | 5.7 MiB |
| 2 GiB | on | 580 | 240 KiB | 3,359 | 13.7 MiB |
| 4 GiB | on | 636 | 245 KiB | 4,110 | 16.8 MiB |

Mixed blocks of 26 operations at the end: 1,164 objects (413 KiB); 3,516
page writes (14.8 MiB) with collection off, 8,934 (36.0 MiB) with it on.

- **The engine's output is modest.** A created record with eight indexed
  attributes becomes 31 to 37 tree nodes of about 400 bytes: 1.6 to 1.9
  times the payload.
- **The store multiplies it.** Every tree node is a row of one redb table
  keyed by its 32-byte hash (`objects_ns_…`), so each goes to a random
  place in a copy-on-write B-tree and rewrites its leaf page and the branch
  pages above it: 1.7 pages per object at 0.5 GiB, 2.9 at 4 GiB.
- **Collection doubles it.** A node that leaves the retention window is
  deleted from another random leaf, and its bookkeeping rows (`gc_dead`,
  `gc_queue`, both keyed by the same hash) are written and deleted too.
  With collection on a 400-byte object costs 6.5 page writes at 4 GiB.
- **It grows with the table**, because fewer branch pages are shared
  between the inserts of one commit. This is why our commit time rises with
  the file while golem-db's stays flat: golem-db keeps record cells and
  postings in flat tables that are overwritten in place.
- The table at commit 10,000: 2.1 million objects with collection on
  (1.56 GiB stored, 2.53 GiB of pages), 7.1 million without.

## Two settings of our engine

Measured with collection off. The same scenario on our engine with one
thing changed
(`ARKIV_REDB_QUICK_REPAIR=0`; `ARKIV_REDB_CACHE_MIB=4096` with
`ARKIV_OBJECT_CACHE_MIB=1024` and `ARKIV_OBJECT_CACHE_ENTRIES=2000000`):

| | Defaults | Quick repair off | Large caches |
| --- | ---: | ---: | ---: |
| Commit p50, fill | 23.6 ms | 22.3 ms | 25.1 ms |
| Commit p50, mixed | 44.6 ms | 43.8 ms | 45.3 ms |
| Written per commit, fill | 10.8 MiB | 10.4 MiB | 10.8 MiB |
| top `project` and `status` | 57.6 ms | 59.7 ms | 31.5 ms |
| `project` 40 and `status` | 13.2 ms | 13.9 ms | 2.3 ms |
| range 10% | 207 ms | 208 ms | 38.4 ms |
| range 50% | 1,963 ms | 1,873 ms | 193 ms |
| `status` (25%) | 0.40 ms | 0.43 ms | 0.70 ms |

- Quick repair explains about 4% of the bytes written at this file size
  (4 GiB to 6 GiB) and none of the commit time. At Kenobi's size it
  dominates ([below](#write-volume-of-the-node-at-kenobi-size)).
- The caches do nothing for writes. They make the wide queries 2 to 10
  times faster; with them a 10% range is 1.4 times golem-db's time and a
  50% range 3 times. The two caches were raised together, so which one
  matters is not determined. The cheap single-term queries got slower
  (0.40 ms to 0.70 ms); not investigated.

## The longer run, as far as it went

The full `arkiv.toml` (75,000 commits, the file ends at twice the memory
cap) was started on both engines and stopped; the two partial runs overlap
on the first 46,000 commits.

| At commit | golem-db commit p50 | Ours | golem-db file | Ours |
| ---: | ---: | ---: | ---: | ---: |
| 10,000 | 11.4 ms | 25.7 ms | 2.4 GiB | 4.0 GiB |
| 20,000 | 11.3 ms | 30.7 ms | 4.6 GiB | 9.0 GiB |
| 40,000 | 12.0 ms | 50.9 ms | 9.2 GiB | 18.0 GiB |
| 46,000 | 13.7 ms | 79.6 ms | 10.6 GiB | 20.0 GiB |

Our run was made with collection off. Our commit time grows with the file;
theirs stays flat while the file fits in memory. Our run used a 128 KiB readahead and theirs 3 MiB (next
paragraph), which does not matter before the file outgrows the cap.

golem-db did not get through its run in reasonable time on this machine.
Once its file passed the 8 GiB cap, its block time rose from about 20 ms
to about 780 ms (seal 570 ms) with the array reading 1.15 GB/s, and it was
at commit 67,044 after 194 minutes. The array's readahead is 3 MiB
(`/sys/block/md2/queue/read_ahead_kb` = 3072; a single drive has 128), and
MDBX reads through a memory mapping, so every page fault pulled in 3 MiB
and pushed useful pages out of the cgroup. A rerun with 128 KiB was started
with our engine first and stopped before golem-db's turn, so how golem-db
does past memory with a small readahead is not measured.

## Write volume of the node at Kenobi size

Looking for the cause of the bytes written led to the Kenobi follower
(`node.redb` 106 GiB, 2 s blocks, a handful of records per block):

| Measured on the follower | Value |
| --- | ---: |
| State objects written per block (engine counter) | 4.4 KiB |
| Bytes reaching the device per block (`/sys/block/md2/stat`, 30 blocks) | 14 MiB |
| The same from `/proc/PID/io` `write_bytes` (50 blocks) | 14.9 MiB |
| State commit time, mean | 38 ms |

A system-call trace of three blocks shows where it goes: each state commit
writes about 78 chunks of 512 KiB at 52 fixed places spread evenly over the
file, 2 GiB apart, and about 35 pages of 4 KiB. The 4 KiB pages are the
data. The 512 KiB chunks are redb's saved allocator state, one per 2 GiB
region of the file, written in full on every commit whether the region
changed or not. That is what quick repair does (redb 4.3.0,
`commit_inner`: the allocator-state table is deleted and rebuilt with every
region's state), and its cost is proportional to the file size, not to the
block: about 0.13 MiB per GiB of file per commit. The earlier measurement of quick repair
([redb experiment](redb-experiment.md)) saw the growth but stopped at
1 GiB.

At 2 s blocks this is 7 MiB/s, about 600 GiB a day, on a 106 GiB database
that changes by a few KiB per block. The producer was not measured; it
runs the same store on a file of the same size.

## Options

There are two costs with different remedies. The first one was measured
and decided afterwards: see [write volume of the state database](state-write-volume.md)
(durable state commit every N blocks, `--state-durable-every`). The second
is open.

Quick repair (14 MiB per block at Kenobi's size, whatever the traffic):

| Option | Effect | Price |
| --- | --- | --- |
| Turn quick repair off | The cost goes away | An unclean stop is followed by redb's full repair, which reads the whole file at least once (measured up to 1 GiB, not at 106 GiB) |
| Make the state commit durable every N blocks | The cost divided by N, and N times fewer syncs | A crash loses up to N blocks of state, which the node must rebuild from its block history; whether it can today was not checked |
| Change redb to save only the regions that changed | The cost follows the traffic | A patch to carry or to get upstream |

Random pages per tree node (grows with traffic and with the file):

| Option | Effect | Price |
| --- | --- | --- |
| Durable commit every N blocks (as above) | Pages dirtied by several blocks are written once; estimated 45% fewer pages at N = 10, not measured | As above |
| Fewer, larger tree nodes | The store's cost is per object, not per byte | An engine format change; the gain is not measured |
| A log-structured store for the objects, behind the store interface | A commit becomes a sequential append of about 250 KiB, plus compaction in the background | A new store crate; the crash and recovery validation done for redb has to be repeated |
| Key tree nodes by position instead of by hash | Nodes written together are stored together; writes approach the logical size | Engine, store, garbage collection and on-disk format all change, and it interacts with the 256-block history |

Both cheap ones were then measured on copies of the Kenobi database
([results](state-write-volume.md)): quick repair off costs a 58-minute
restart after a kill, and a durable commit every 64 blocks (the node's
default since 0.1.0-alpha.11) brings the node from 13.8 to 0.67 MiB per
block. The page estimate in the second table (45%
fewer pages at N = 10) did not hold for the node: the data pages stay at
about 0.44 MiB per block whatever N is.

## Decisions

- **No bitmap postings for now** (2026-11-27). Postings stay one entry per
  record in the versioned state tree. The known cost is the AND and
  wide-range gap above. It may make sense later.
- Carried over: no metering, no Blake3
  ([comparison](golem-db-api-comparison.md)).

## Not determined

- Why the kernel counts up to twice the bytes the store hands to the file.
- How the page writes per object continue to grow past 4 GiB with
  collection on (the 46,000-commit run was made with collection off).
- Why one-record and rare-term lookups are 6 to 14 times slower.
- The same queries without the total.
- Which of the two caches helps the wide queries, and what larger caches
  cost the node in memory at Kenobi size.
- The full 75,000-commit scenario on either engine with a 128 KiB
  readahead.
- The places in the adapter that a single-writer benchmark cannot show:
  readers waiting for a commit, sorted queries, offset paging, immutable
  data.

The result files are on the measuring machine only
(`~/scx1332/api-compare/bench-out`).
