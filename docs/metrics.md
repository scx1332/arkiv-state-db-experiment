# Node metrics

`arkiv-proposer producer|full|light` accepts `--metrics-listen 127.0.0.1:19600`
and optional `--metrics-table-stats-secs N` (minimum 10, off by default). The
dedicated HTTP listener serves Prometheus text at `/metrics`. Disabled unless
configured. Keep the exporter on loopback and publish it, if at all, through a
reverse proxy route that exposes exactly `/metrics`; the private control
listener must stay blocked.

Metric names use the prefix `rogue_one_`, after the first deployment
(Prometheus identifiers cannot contain hyphens). Add chain and instance
labels in the scrape configuration.

## Storage and overhead

Every DB has a bounded `database` label: `state` (node.redb), `history`
(history.redb), or `headers` (headers.redb). The exporter never exposes paths,
keys, payloads, namespace IDs, wallet addresses, or peer credentials.

By default nothing walks the database. Each scrape reads:

- `db_file_apparent_bytes`: file length, including sparse holes (`stat`).
- `db_file_allocated_bytes`: actual filesystem allocation (`st_blocks * 512`,
  Unix only). This is absent on platforms without that measurement.
- the operation timings, commit counters and object and record counters
  below, which the store keeps in memory;
- on a state file with garbage collection, the `state_gc_*` counters (see
  [state pruning](#state-pruning)), which every block commit keeps up to date
  in the file itself.

### Opt-in table walk

`--metrics-table-stats-secs N` adds a background walk of every table
(`RedbStore::data_stats`) every `N` seconds after the previous walk ends. **Do
not enable it on a large state.** The walk runs in one redb read transaction,
and redb reuses no page freed while that transaction is open: on the Sourcify
state a walk took over an hour and the file grew from 106 GiB to 650 GiB with
pages nobody could reuse
([2026-11-19 Sourcify soak](benchmarks/2026-11-19-sourcify-soak.md)). Use it
on small states, or run `data_stats` offline on a stopped copy instead. With
the walk enabled the scrape also exports:

- `db_stored_bytes`: encoded keys and values in redb user tables. This already
  includes our indexes, Merkle objects, serialized records, and retained history;
  it is **not** application payload bytes.
- `db_disk_overhead_bytes`: allocated bytes at the walk minus stored bytes,
  floored at zero. Everything in the file that is not an encoded key or
  value: see [what the overhead contains](#what-the-overhead-contains).
- `db_table_metadata_bytes` and `db_table_fragmented_bytes`: metadata and unused
  bytes **within user table pages only**. They exclude the database directory,
  allocator and free pages. Do not add these to disk overhead: they are two
  of its parts.
- `db_entries` and `db_tables`: retained table entries and table count, not the
  number of live entities. Expired data may remain in historical objects.
- `db_stats_timestamp_seconds`, `db_stats_refresh_duration_seconds`,
  `db_stats_refresh_success` and `db_stats_refresh_errors_total`: walk
  freshness, cost and failures. Before the first success the data gauges are
  absent. Failures retain the last good data with its original timestamp.

The walk never takes the node mutex or a redb write transaction, and walks
never overlap within a process. Backend read counters include its traffic.

### What the overhead contains

`db_disk_overhead_bytes` is one number for four different things:

1. **B-tree metadata of user tables** (`db_table_metadata_bytes`): branch
   pages, and the page headers and per-entry offsets of leaf pages.
2. **Unused bytes inside user table pages** (`db_table_fragmented_bytes`):
   the slack of partially filled pages. A value larger than a page takes a
   power-of-two run of pages, so up to half of that run is slack too.
3. **redb's own structures**: the file header, the region headers that hold
   the page allocator, the table directory, and redb's system tables (the
   lists of pages freed by recent commits and, with quick repair, the saved
   allocator state).
4. **Free pages**: pages of the file that belong to no tree. redb reuses
   them for later commits and returns only free pages at the end of the file
   to the filesystem. A deletion (state pruning, record expiry) therefore
   moves table pages into this part: the file keeps its size.

Stored bytes, part 1 and part 2 add up to the page bytes of the user tables
exactly (redb accounts every byte of a table page as one of the three). So
with the walk enabled

```promql
rogue_one_db_disk_overhead_bytes
  - rogue_one_db_table_metadata_bytes
  - rogue_one_db_table_fragmented_bytes
```

is parts 3 and 4 together: the bytes of the file outside user tables. The
three gauges come from one walk; the file allocation is read when the walk
ends, so commits during a long walk shift the result.

**The free page count is not exported on its own, and nothing is exported
without the walk.** redb 4.3.0 keeps the count in memory (its region
allocators, `count_free_pages` and `count_allocated_pages` in
`tree_store/page_store/page_manager.rs`), but both are `pub(crate)`. The
only public reader is `WriteTransaction::stats()`, which first walks every
user and system table (`TableTree::stats`) for its other fields, inside a
write transaction: that would stop block production for the length of a
walk. It is used only by the offline `RedbStore::layout` diagnostic.

Without the walk, `db_file_allocated_bytes{database="state"}` minus
`state_gc_stored_bytes` is an upper bound of the overhead of `node.redb`:
parts 1 to 4 plus the object keys and the tables that hold no objects
(records, GC tables).

## State pruning

A full node exports what bounds deletion (sampled each second from the
engine, without the node lock if it is busy):

- `state_floor`, `state_durable_floor`: the lowest height served, and the one
  recorded with the durable head.
- `state_gc_horizon_height`: `min(durable floor, lowest pin)`. Objects that
  died before the epoch this height was committed in are deleted.
- `state_lowest_pin_height`, `state_pinned_handles`: open snapshot handles. A
  pin that stays low holds deletion back.
- `state_record_floor`: the lowest height (above 0) whose per-height records
  (feed, block, request inclusions) are kept; `state_record_backlog_heights`:
  expired heights whose records commits have not deleted yet (it drains by
  at least a sixteenth per block after a migration).

A `node.redb` with garbage collection (`database="state"`) also exports, from
the GC state the store keeps in each commit:

- gauges `state_gc_stored_objects` and `state_gc_stored_bytes` (all objects in
  the object tables), `state_gc_live_objects`, `state_gc_live_bytes`,
  `state_gc_dead_objects`, `state_gc_pending_release` (dead objects whose
  references are not released yet), `state_gc_queued_objects` and
  `state_gc_queued_bytes` (released, waiting for the horizon),
  `state_gc_shared_entries` (objects referenced twice or more),
  `state_gc_phantom_entries`, `state_gc_epoch`, `state_gc_suspended`;
- gauges of the last commit: `state_gc_last_released`,
  `state_gc_last_deleted`, `state_gc_last_eligible` (deletable objects when
  it started: the deletion backlog);
- counters since the counts were built: `state_gc_born_total`,
  `state_gc_died_total`, `state_gc_released_total`, `state_gc_deleted_total`,
  `state_gc_deleted_bytes_total`, `state_gc_resurrected_total`,
  `state_gc_over_budget_total` (commits that worked past their budget to
  shrink a backlog), and `state_gc_seconds_total` (wall time of the
  collection work inside commits in this process, below).

`state_gc_seconds_total` times what garbage collection adds to a commit
before redb commits it: reading the GC state, counting the new head's
references and releasing the old head's, the release cascade, choosing what
to delete, and applying the deletions and the GC table changes. It excludes
what a store without garbage collection does too (checking and storing the
block's own objects and records), and it excludes redb's commit itself: the
pages the collection dirtied are written and synced together with the
block's, and that share cannot be told apart. It is therefore a lower bound
of the cost, always below the commit time
(`db_operation_duration_seconds_sum{database="state",layer="store",operation="commit"}`).
Through 0.1.0-alpha.8 this counter was the wall time of the whole commit.

`state_gc_stored_bytes` counts encoded object values; with
`db_file_allocated_bytes` it shows how much of the file is B-tree slack and
free pages without a table walk.

## DB timings and node activity

`db_operation_duration_seconds` is a fixed-bucket histogram with labels
`database`, `layer`, `operation`:

- `layer="store"`: `read_object`, `read_record`, `head`, `commit`, `recover`.
  End-to-end API wall time includes lock waiting, transaction setup, checks,
  hashing/copying and, for successful commits, immediate durability barriers.
- `layer="backend"`: `read`, `write`, `sync_data`, `set_len`. Time in the backend
  API below redb's page cache; OS cache still applies. This is not raw device
  latency. Store timings include backend timings, so do not add the two layers.

Buckets (26 and +Inf): 1us, 2.5us, 5us, 10us, 25us, 50us, 100us, 250us, 500us,
1ms, 2.5ms, 5ms, 7.5ms, 10ms, 15ms, 20ms, 30ms, 50ms, 75ms, 100ms, 250ms, 500ms,
1s, 2.5s, 5s, 10s, +Inf. Steps of 1-2.5-5 per decade, with 7.5, 15, 20, 30 and
75 ms added where durable commits fall, so a quantile of the commit latency is
interpolated over a few milliseconds instead of over 10 to 50 ms. The twelve
buckets up to 0.1.0-alpha.8 (1us, 10us, 100us, 500us, 1ms, 5ms, 10ms, 50ms,
100ms, 500ms, 1s, 10s) are all kept with the same `le` values.

Count/sum include failures; `db_operation_errors_total` counts failures
(including indeterminate commits). Durations use a monotonic clock. Hot paths use
fixed atomic counters without dynamic labels or heap allocation. Snapshots are
approximate under concurrent updates. Counters reset on process restart.
`db_backend_{read,write}_bytes_total` count successful backend bytes;
`db_commits_total`, `db_commit_not_committed_total` and
`db_commit_indeterminate_total` distinguish commit outcomes.
The state database's `db_commits_total` counts every commit, of which one
in N is durable (`--state-durable-every N`, 64 by default:
[write volume](state-write-volume.md)).

### Objects and records

Counters of what passed through the storage contract, per `database`. The
store keeps them in memory; a scrape copies them and reads nothing from the
file.

- `db_objects_written_total` and `db_objects_written_bytes_total`, labels
  `database`, `space`: objects newly stored by committed batches, and their
  encoded bytes (values only, the unit of `state_gc_stored_bytes`). An
  object that was already stored is not counted again.
- `db_object_reads_total` and `db_object_read_bytes_total`, labels
  `database`, `space`: answered object reads (found or absent) and the bytes
  returned. These are reads that reached the store: a trie node the engine
  cache holds is never read. Reads inside a commit (garbage collection
  planning) use the write transaction and are not counted.
- `db_record_reads_total`, `db_record_read_bytes_total`,
  `db_records_written_total`, label `database`: the same for records
  (blocks, manifests, inclusions, block index, metadata).
- `db_entries_removed_total`, label `database`: records and objects removed
  by committed batches (record expiry). Objects deleted by garbage
  collection are counted in `state_gc_deleted_total` instead.

`space` is `catalog`, `host` or `namespace`. `namespace` is the sum over
every namespace: namespace ids are never labels. In `node.redb` the objects
are the state's trie nodes. In `history.redb` they are the block bodies, one
per block, in space `host`; with `--body-retention window` the bodies and
records it deletes are counted in `db_entries_removed_total`, in commits of
their own (so `db_commits_total{database="history"}` grows faster than the
head).

The read counters share one mutex, taken once per object or record read.
It is not a measurable cost. Replacing it with atomic counters was tried on
2026-11-25 and not kept: with six clients querying 8 KB objects through a
release node, the mean `read_object` time was 74 us with the mutex (three
runs, as before this change) and 81 us with atomics (five runs); every read
also takes redb's transaction-tracker mutex twice.

Write amplification is `db_backend_write_bytes_total` over
`db_objects_written_bytes_total` (summed over `space`) plus the record
bytes, which are not counted: for a state file the object bytes are most of
the logical write. redb writes whole pages, the path from each changed leaf
to its root, and its own bookkeeping at every durable commit, so small
blocks have a large ratio.

### Engine node cache

The state engine keeps decoded trie nodes in a bounded cache
(`Options::cache_bytes` and `cache_entries`: 8 MiB and 16,384 nodes by
default). A full node exports, sampled each second with the node sample:

- `state_cache_hits_total` and `state_cache_misses_total`, label `map`
  (`catalog`, `host`, `records`, `keys`, `rows`, `terms`, `postings`): node
  loads answered from the cache, and loads that read the store. A miss is
  one object read and one decode.
- `state_cache_evictions_total`: nodes evicted to stay within the limits.
- `state_cache_miss_read_seconds_total`: wall time of the store reads of
  misses, summed over reader threads (it can grow faster than the clock).
  Decoding is not included.

The cache is emptied by a store recovery; the counters are not.

Node metrics include role, uptime, verified head height and timestamp, timestamp
lag, pending transaction count/bytes, and pause state. A nonblocking one-second
sampler reads cheap node metadata. `node_sample_timestamp_seconds` exposes its
freshness. Timestamp lag includes normal block interval plus sampling age; use a
trend or compare `time()` with `head_timestamp_seconds`, not a sub-second alarm.
The light node additionally exports authenticated `peer_observed_height`.

`node_tick_total`, `node_tick_busy_total`, `node_tick_errors_total` and
`node_tick_duration_seconds_total` measure automatic producer attempts or
follower polls (including idle polls). `blocks_produced_total` counts only blocks
produced by the automatic loop; manual control steps are reflected in head height.
Node tick durations include queueing/network time and are not DB operation timings.

## Example scrape configuration and queries

```yaml
scrape_configs:
  - job_name: arkiv-producer
    scheme: https
    static_configs:
      - targets: ['producer.example.org']
  - job_name: arkiv-full
    scheme: https
    static_configs:
      - targets: ['full.example.org']
  - job_name: arkiv-light
    scheme: https
    static_configs:
      - targets: ['light.example.org']
```

Alternatively a host-local Prometheus can scrape HTTP loopback ports 19600
(producer), 19602 (full), and 19603 (light). Suggested queries (choose the relevant
job/database):

```promql
# Filesystem overhead relative to encoded KV bytes (fraction, not payload ratio).
rogue_one_db_disk_overhead_bytes / rogue_one_db_stored_bytes

# p95 end-to-end commit latency in seconds.
histogram_quantile(0.95, sum by (le, job, database) (
  rate(rogue_one_db_operation_duration_seconds_bucket{layer="store",operation="commit"}[5m])
))

# p95 object-read latency (storage API) and fsync latency (backend).
histogram_quantile(0.95, sum by (le, job, database, layer, operation) (
  rate(rogue_one_db_operation_duration_seconds_bucket{operation=~"read_object|sync_data"}[5m])
))

# Mean fsync wall time; guard against idle/zero-count windows in dashboards.
rate(rogue_one_db_operation_duration_seconds_sum{layer="backend",operation="sync_data"}[5m])
/ rate(rogue_one_db_operation_duration_seconds_count{layer="backend",operation="sync_data"}[5m])

# Opt-in table walk only: sample age (seconds), the interval plus the walk.
time() - rogue_one_db_stats_timestamp_seconds

# Opt-in table walk only: bytes of the file outside user tables (free pages
# and redb's own structures).
rogue_one_db_disk_overhead_bytes - rogue_one_db_table_metadata_bytes
  - rogue_one_db_table_fragmented_bytes

# Write amplification of the state file: backend bytes per object byte stored.
sum by (job) (rate(rogue_one_db_backend_write_bytes_total{database="state"}[5m]))
/ sum by (job) (rate(rogue_one_db_objects_written_bytes_total{database="state"}[5m]))

# Engine node cache hit ratio, per map.
rate(rogue_one_state_cache_hits_total[5m])
/ (rate(rogue_one_state_cache_hits_total[5m]) + rate(rogue_one_state_cache_misses_total[5m]))

# Mean store read time of a cache miss, in seconds.
sum by (job) (rate(rogue_one_state_cache_miss_read_seconds_total[5m]))
/ sum by (job) (rate(rogue_one_state_cache_misses_total[5m]))

# Share of the state commit time spent on garbage collection work.
sum by (job) (rate(rogue_one_state_gc_seconds_total[5m]))
/ sum by (job) (rate(rogue_one_db_operation_duration_seconds_sum{database="state",layer="store",operation="commit"}[5m]))
```
