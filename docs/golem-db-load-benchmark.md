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
  adapter's defaults, which are the node's: state window from the scenario
  (1,000 commits), redb page cache 256 MiB, object cache 8 MiB and 16,384
  entries, quick repair on.
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
| Time in blocks | 158 s | 241 s | 1.5 |
| Seal p50 | 5.0 ms | 1.2 ms | 0.25 |
| Commit p50 | 10.9 ms | 23.6 ms | 2.2 |
| Block p50 | 17.1 ms | 29.7 ms | 1.7 |
| Block p99 | 93.6 ms | 56.7 ms | 0.6 |
| **Mixed, 2,000 blocks** | | | |
| Time in blocks | 68 s | 123 s | 1.8 |
| Seal p50 | 9.9 ms | 2.6 ms | 0.27 |
| Commit p50 | 17.5 ms | 44.6 ms | 2.5 |
| Block p50 | 29.2 ms | 61.4 ms | 2.1 |
| Block p99 | 121.7 ms | 77.3 ms | 0.6 |
| **Storage** | | | |
| Written per commit, fill (p50) | 1.2 MiB | 10.8 MiB | about 9 |
| Written per commit, mixed (p50) | 2.2 MiB | 19.8 MiB | about 9 |
| File at the end | 2.3 GiB | 6.0 GiB | 2.6 |

Queries, p50 after commit 10,000 (100 rows and the total, unless noted):

| Query | Matches | golem-db | Ours | Ours / theirs |
| --- | ---: | ---: | ---: | ---: |
| `status` (25%) | 34,245 | 0.89 ms | 0.40 ms | 0.45 |
| `status`, 10 rows | 34,245 | 0.50 ms | 0.05 ms | 0.09 |
| top `project` | 30,218 | 0.84 ms | 0.39 ms | 0.46 |
| top `$owner` | 13,904 | 0.64 ms | 0.43 ms | 0.67 |
| random `$owner` | 171 | 0.56 ms | 3.6 ms | 6.4 |
| random `ref` | 453 | 0.55 ms | 3.1 ms | 5.6 |
| one record by `id` | 1 | 0.009 ms | 0.12 ms | 14 |
| top `project` and `status` | 7,423 | 1.3 ms | 57.6 ms | 44 |
| `project` 40 and `status` | 187 | 0.92 ms | 13.2 ms | 14 |
| random `project` and `status` | 2,522 | 1.1 ms | 43.4 ms | 39 |
| random `project` and `entityType` | 735 | 0.88 ms | 32.6 ms | 37 |
| `$expiresAt` range, 1% | 1,354 | 1.7 ms | 2.1 ms | 1.2 |
| `$expiresAt` range, 10% | 13,486 | 27.8 ms | 207 ms | 7.5 |
| `$expiresAt` range, 50% | 67,857 | 64.9 ms | 1,963 ms | 30 |
| random `project` and range 10% | 340 | 27.7 ms | 290 ms | 10 |

What the numbers say:

- **Seal** is four times faster on our engine.
- **Commit** is 2.2 to 2.5 times slower and writes about nine times as many
  bytes. Why is not determined (see below); it is not quick repair.
- **Tail**: our p99 block time is 0.6 of theirs.
- **Single hot term**: we are 1.5 to 11 times faster. The size of a term's
  posting list is stored with the term, so the total costs nothing.
- **Two terms with AND**: 14 to 44 times slower. golem-db keeps each term's
  postings as roaring bitmap containers in a flat table that is overwritten
  in place and readable at the head only, so an AND is a bitmap
  intersection. Our postings are one entry per record in the versioned
  state tree, which is what lets a query run at any retained block; an AND
  merges those entries. In a small in-memory run the count was nearly all
  of the time and the page itself was cheap (count 0.46 ms, one row plus
  total 0.47 ms, 100 rows plus total 0.89 ms; golem-db 0.04, 0.04 and
  0.33 ms). The benchmark asks for the total on every page.
- **Wide ranges**: 7 to 30 times slower, growing with the share matched.
- **Small lookups** (one record, a rare owner or ref): 6 to 14 times
  slower. Not investigated.

## Two settings of our engine

The same scenario on our engine with one thing changed
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
  (4 GiB to 6 GiB) and none of the commit time.
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

Our commit time grows with the file; theirs stays flat while the file fits
in memory. Our run used a 128 KiB readahead and theirs 3 MiB (next
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

Options, none decided:

1. Turn quick repair off. An unclean stop is then followed by redb's full
   repair, which reads the whole file at least once (measured up to 1 GiB;
   not measured at 106 GiB).
2. Save the allocator state less often than every commit, or only for
   regions that changed. Both need a change in redb.
3. Leave it and accept the write volume.

## Decisions

- **No bitmap postings for now** (2026-11-27). Postings stay one entry per
  record in the versioned state tree. The known cost is the AND and
  wide-range gap above. It may make sense later.
- Carried over: no metering, no Blake3
  ([comparison](golem-db-api-comparison.md)).

## Not determined

- What our commit writes: about 11 MiB per block of 17 creates of 8 indexed
  attributes each, growing to 20 MiB with updates and deletes. Quick repair
  is 4% of it at this size.
- Why our commit time grows with the file (25 ms at 4 GiB, 80 ms at
  20 GiB).
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
