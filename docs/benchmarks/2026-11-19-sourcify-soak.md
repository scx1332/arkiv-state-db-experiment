# Node soak on the Sourcify dataset (2026-11-19)

The soak harness (`scripts/soak/`, see `docs/node-soak.md`) run on a chain
whose genesis holds the Sourcify v4 bundle (16.8 million entities, 9.2 million
contracts), rekeyed to an owner whose key the harness holds, so that it can
patch the dataset's contracts.

- Commit `ec98851`, binary SHA-256 `dd24bbc590207854…`, clean tree.
- Host: AMD Ryzen 5 3600, 12 threads, 62.7 GiB RAM, ext4 on md RAID.
- 250 ms blocks; batches of 16 operations, 8 transactions in flight; SDK
  1.0.1. The node's metrics exporter ran with `--metrics-interval-secs 60`.
- Start: `--prebuilt-state` with `--genesis-install adopt`. Adopting the
  106 GiB state took about 50 minutes, bound by disk reads at about 73 MB/s.
- Result: **stopped in `growth` after 76 minutes**, by the rule "free disk
  below 100 GiB". `node.redb` had grown from 106 GiB to 650 GiB.

## Throughput

| Phase | Minutes | Operations | Operations/s | Tx p50 ms | Tx p95 ms | Tx p95 ms (worst window) | Gas/operation |
| --- | --- | --- | --- | --- | --- | --- | --- |
| cohort (2,000 creates) | 0.2 | 2,000 | 158 | 744 | 863 | 974 | 41,750 |
| `mutate-verifiedat` | 20 | 174,208 | 145 | 691 | 839 | 1,780 | 19,310 |
| `mutate-reverify` | 40 | 253,056 | 105 | 952 | 1,319 | 7,830 | 22,466 |
| growth (creates) | 15.7 | 67,968 | 72 | 1,542 | 2,176 | 6,030 | 41,750 |

- On the empty state the same cohort ran at 504 creates/s with p50 242 ms
  (`2026-11-19-node-soak.md`). The large state costs about a factor of three
  from the first block.
- Each patch reads its contract first. Those reads took 1.4 ms at p50 in
  both mutate phases.
- `verifiedat` patches set one attribute with a unique value. `reverify`
  patches also flip `match` and rewrite five attributes whose values
  millions of contracts share, and run about 30% slower.
- The mean state-store commit grew from 104 ms to 208 ms over the run.

## Reads

p50 / p95 in milliseconds, 100 requests one at a time while writes continue:

| Read | Minute 4 | Minute 10 | Minute 60 |
| --- | --- | --- | --- |
| Sourcify contract by key | 1.4 / 20.7 | 1.2 / 1.6 | 1.3 / 24.8 |
| Vyper contracts, limit 100 | 4.8 / 31 | 4.9 / 57.7 | 5.2 / 136.1 |
| `matchid` window of 50,000, limit 100 | 348.8 / 12,158 | 378.9 / 12,339 | 320 / 9,959 |
| Count Vyper (5,777) | 128.2 / 471.5 | 128 / 528.4 | 184.5 / 599.4 |

- Point reads and equality queries stay fast on the full dataset.
- A range over `matchid` takes 10 to 12 seconds at p95. The range filter
  goes through the `arkiv.schema` posting list of 9.2 million contracts,
  beyond the query planner's cap. Under 4 concurrent callers it served 1 to
  2 requests/s, and most were refused with ServerBusy.

## Disk growth: the metrics scan pins every freed page

The state file grew by about 730 KiB per patch and 1.9 MiB per create. That
is write volume, not retained data:

| | Size |
| --- | --- |
| File | 650.0 GiB |
| Pages allocated by redb | 648.6 GiB |
| Stored in tables | 62.4 GiB |
| Table metadata | 1.9 GiB |
| Slack inside table pages | 48.2 GiB |
| **Allocated, in no table** | **536 GiB** |
| Free pages in the file | 1.4 GiB |

Read with `WriteTransaction::stats` and per-table `stats()` on the stopped
file. The walk took 69 minutes.

The 536 GiB are pages freed by commits whose release redb had deferred. redb
4.3 releases a freed page only after every read transaction older than the
freeing commit has ended (`process_data_freed_pages_after_commit`, which
frees up to `oldest_local_referenced_transaction`).

The node's metrics exporter holds such a transaction:

- `Metrics::refresh` (`crates/arkiv-node/src/signed/metrics.rs:286`) calls
  `RedbStore::data_stats`, which walks every page of every table inside one
  read transaction.
- The next walk starts 60 seconds after the last one ends.
- On this state a walk takes tens of minutes. While it runs, no page freed
  by any commit is released, and every commit grows the file.

The soak's own timeline shows it:

- The first walk, started with the node, finished at minute 36. It is the
  only change of `db_stored_bytes` in the samples.
- From minute 36 to minute 57 the file stayed at 382.5 GiB while writes
  continued. They reused the pages that walk had pinned.
- The second walk started about a minute later and was still running at the
  stop. The file grew from 382.5 GiB to 650 GiB.

Retained data grew far less. Stored bytes went from 57.9 GiB at minute 36
to 62.4 GiB at the stop.

The overnight soak's 57.6 GiB of free pages (`2026-11-19-node-soak.md`,
Fragmentation) probably have the same cause on a smaller state, where
walks were shorter. That run was not checked for it.

### What follows

- The periodic `data_stats` walk must not run in one long read
  transaction. Either drop it from the default exporter, or walk table by
  table in short read transactions that each cover a bounded number of
  pages.
- Rerun this soak without the walk, to measure the real per-operation
  growth. That needs about 400 GiB of free disk, or a stop rule on retained
  bytes.
- Any long read (a 12-second range query, an RPC snapshot, an export) pins
  freed pages the same way, for its duration. That cost grows with the
  write rate.

## Not verified

- That the 536 GiB would be released by the next commit. Committing to the
  stopped file, then walking it again, takes over an hour.
- The size of each walk. The node does not export when a walk starts, only
  when one finishes.
