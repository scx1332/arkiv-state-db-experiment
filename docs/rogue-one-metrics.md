# Rogue One node metrics

`arkiv-proposer producer|full|light` accepts `--metrics-listen 127.0.0.1:19600`
and optional `--metrics-interval-secs 300` (minimum 10). The dedicated HTTP
listener serves Prometheus text at `/metrics`. Disabled unless configured.
The exporter listens on loopback in Rogue One. Public Nginx exact `/metrics`
routes expose it at `https://rpc.rogue-one.eu/metrics`,
`https://full-node.rogue-one.eu/metrics`, and
`https://light-node.rogue-one.eu/metrics`, as requested by the operator. RPC and
node panels keep their existing routes; private controls remain blocked.

Metric names use `rogue_one_`: classic Prometheus metric identifiers cannot
contain the requested hyphens. Deployment job names may use `rogue-one-producer`,
`rogue-one-full`, and `rogue-one-light`. Add chain/instance labels in scrape config.

## Storage and overhead

Every DB has a bounded `database` label: `state` (node.redb), `history`
(history.redb), or `headers` (headers.redb). The exporter never exposes paths,
keys, payloads, namespace IDs, wallet addresses, or peer credentials.

- `db_stored_bytes`: encoded keys and values in redb user tables. This already
  includes our indexes, Merkle objects, serialized records, and retained history;
  it is **not** application payload bytes.
- `db_file_apparent_bytes`: file length, including sparse holes.
- `db_file_allocated_bytes`: actual filesystem allocation (`st_blocks * 512`,
  Unix only). This is absent on platforms without that measurement.
- `db_disk_overhead_bytes`: allocated bytes minus stored bytes, floored at zero.
  Includes metadata, partially filled pages and allocated free space.
- `db_table_metadata_bytes` and `db_table_fragmented_bytes`: metadata and unused
  bytes **within user table pages only**. They exclude the database directory,
  allocator and free pages. Do not add these to disk overhead: they overlap it.
- `db_entries` and `db_tables`: retained table entries and table count, not the
  number of live entities. Expired data may remain in historical objects.

A single background worker walks read-only table snapshots, never taking the
node mutex or a redb write transaction. The next interval begins after the scan
finishes; scans cannot overlap within a process. Scans can consume disk/cache
bandwidth and temporarily pin old pages, so the conservative default is five
minutes. Filesystem allocation is sampled just after the table snapshot; concurrent
writes can introduce a small difference between these measurements. Backend read
counters include scan traffic. No compaction or pruning is performed.

`db_stats_timestamp_seconds`, `db_stats_refresh_duration_seconds`,
`db_stats_refresh_success` and `db_stats_refresh_errors_total` expose scan
freshness/cost/failures. Before first success data gauges are absent. Failures
retain last good data with its original timestamp. Always check freshness.

## DB timings and node activity

`db_operation_duration_seconds` is a fixed-bucket histogram with labels
`database`, `layer`, `operation`:

- `layer="store"`: `read_object`, `read_record`, `head`, `commit`, `recover`.
  End-to-end API wall time includes lock waiting, transaction setup, checks,
  hashing/copying and, for successful commits, immediate durability barriers.
- `layer="backend"`: `read`, `write`, `sync_data`, `set_len`. Time in the backend
  API below redb's page cache; OS cache still applies. This is not raw device
  latency. Store timings include backend timings, so do not add the two layers.

Buckets: 1us, 10us, 100us, 500us, 1ms, 5ms, 10ms, 50ms, 100ms, 500ms, 1s, 10s,
+Inf. Count/sum include failures; `db_operation_errors_total` counts failures
(including indeterminate commits). Durations use a monotonic clock. Hot paths use
fixed atomic counters without dynamic labels or heap allocation. Snapshots are
approximate under concurrent updates. Counters reset on process restart.
`db_backend_{read,write}_bytes_total` count successful backend bytes;
`db_commits_total`, `db_commit_not_committed_total` and
`db_commit_indeterminate_total` distinguish commit outcomes.

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
  - job_name: rogue-one-producer
    scheme: https
    static_configs:
      - targets: ['rpc.rogue-one.eu']
  - job_name: rogue-one-full
    scheme: https
    static_configs:
      - targets: ['full-node.rogue-one.eu']
  - job_name: rogue-one-light
    scheme: https
    static_configs:
      - targets: ['light-node.rogue-one.eu']
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

# Storage sample age (seconds); default interval is 300 plus scan duration.
time() - rogue_one_db_stats_timestamp_seconds
```
