# Sourcify soak with a 256-block state window (2026-11-20)

The same Sourcify v4 genesis as `2026-11-19-sourcify-soak.md` (16.8 million
entities, rebuilt with `genesis stream-build`; the bundle is byte-identical),
run with garbage collection (`--state-retention 256`) and without the
periodic metrics walk.

- Commit `f659c2c`, 250 ms blocks, batches of 16, 8 transactions in flight.
- Phases: 20 minutes of patching dataset contracts (`verifiedat` for a
  third, `reverify` for the rest), 15 of growth, 15 of churn, then a
  restart. Result: **completed**; the restart came back on the same block
  in 373 s.
- Start: the adopt verification took 26 minutes, then the one-off scan that
  seeds the reference counts took 31 minutes (138.4 million objects,
  21.2 million shared-count rows).

## The file stays flat

Allocated bytes of `node.redb` went from 105.1 GiB to 108.1 GiB over 12,000
blocks. The 2026-11-19 run on the same data grew from 106 GiB to 650 GiB in
76 minutes.

Collector counters, first and last sample of each phase:

| Phase | Operations | Live bytes | Deleted | Dead objects waiting, last |
| --- | --- | --- | --- | --- |
| `mutate-verifiedat` | 59,648 patches | +4.2 MB | 341 MB | 53 MB |
| `mutate-reverify` | 89,216 patches | +2.5 MB | 805 MB | 66 MB |
| growth | 124,672 creates | +128.8 MB | 523 MB | 38 MB |
| churn | 112,768 operations | +45.2 MB | 531 MB | 52 MB |

- Patching contracts leaves live state flat. The old versions of the index
  nodes each patch replaces, including those of posting lists millions of
  contracts share, are deleted once they leave the window.
- A new soak entity adds about 1 KB of live state, data and indices
  together. In churn, deletions and expiries remove their entries, so live
  state grows only by the entities still alive.
- The death queue stays near one window's worth, and `over_budget_total`
  stayed 0.

## Where the file is, after the run

`arkiv-proposer state table-stats` on the stopped file (26 minutes):

| Table | Entries | Stored | Pages |
| --- | --- | --- | --- |
| `objects_ns_…01` (state and indices) | 139.6 M | 62.4 GB | 113.0 GB |
| `gc_shared` (counts above one) | 21.3 M | 1.05 GB | 1.68 GB |
| `blocks` | 12,604 | 0.83 GB | 1.16 GB |
| Everything else | | 0.04 GB | 0.04 GB |
| Free pages in the file | | | 2.2 GB |
| File | | | 118.1 GB |

- **The object table is 45% empty space**: 50.6 GB of pages hold nothing.
  It was already there at the start, from the genesis build: keys are
  random hashes, so B-tree pages sit about half full. It is now the
  largest waste on disk. `state compact` or `state migrate` may recover it;
  not tried on this file.
- **`blocks` grows without bound**: about 92 KB of pages per block (66 KB
  stored), mostly feed records, which nothing prunes yet. At 4 blocks/s
  that is about 32 GB a day; `history.redb` adds its bodies on top.
- **Counting costs 1.7 GB** of shared-count rows for this dataset, 1.5% of
  the file.

## Throughput

| Phase | Operations/s | Tx p50 ms | Tx p95 ms |
| --- | --- | --- | --- |
| `mutate-verifiedat` | 149 | 720 | 940 |
| `mutate-reverify` | 112 | 1,021 | 1,288 |
| growth | 139 | 847 | 1,230 |
| churn | 125 | 936 | 1,402 |

Patching runs at the same rate as the archive run of 2026-11-19 (145 and
105 per second); growth is faster (139 against 72 per second), since that
run's file was by then hundreds of GiB. Mean commit was 140–190 ms.

## Not done

- Compacting the object table, and measuring how fast its slack returns.
- Pruning `blocks` records and `history.redb` bodies.
- A run long enough to see the file reach a steady size under patching.
