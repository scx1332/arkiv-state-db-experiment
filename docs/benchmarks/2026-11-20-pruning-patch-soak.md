# Pruning soak: a fixed state under sustained patches (2026-11-20)

Two producers, one archive and one with `--state-retention 256` (garbage
collection on), ran the same load side by side for 12 minutes: 2,000 items
created, then patches of two indexed attributes of those items at the block
cap. The state stays the same size; only history accumulates. It checks the
[pruning proposal](../proposals/2026-11-20-index-pruning.md) on a small state
before the Sourcify run.

- Binary built from the tree committed as `8ad573d` (`plan.json` records its
  parent `8a1f471` with the node changes not yet committed), release.
- Host: AMD Ryzen 5 3600, 12 threads, 62.7 GiB RAM, ext4 on md RAID. Both
  runs at once, on ports 18700 and 18710.
- `scripts/soak/run.py --patch-min 12 --growth-min 0 --churn-min 0
  --readers-min 0`, `--state-retention archive` and `256`. 250 ms blocks,
  batches of 16, 8 in flight: 128 patches per block. This run patched the
  probes' own cohort, so the read probes during it found changed values and
  were recorded as failures; the harness now patches a second set of items.
- Results: `~/scx1332/soak-work/2026-11-20-patch-{archive,pruned}`.

## Throughput

| | Archive | Pruned (256) |
| --- | ---: | ---: |
| Patches | 368,640 | 368,640 |
| Patches per second | 512 | 512 |
| Transaction p95, median of 10 s windows | 251 ms | 251 ms |
| Transaction p95, worst window | 277 ms | 272 ms |
| State commit, mean (second half) | 12.8 ms | 23.7 ms |
| Backend writes per commit (second half) | 3.9 MiB | 6.3 MiB |
| Ready after restart (head 2,910) | 24.2 s | 24.4 s |

Garbage collection about doubles the commit here and adds 60% to the bytes
written, without changing block time or latency at this load.

## The state file

`node.redb` allocated bytes, by head height:

| Head | Archive | Pruned | Pruned: stored object bytes | Pruned: dead objects |
| ---: | ---: | ---: | ---: | ---: |
| 42 | 19 MiB | 21 MiB | 5.0 MB | 10,221 |
| 402 | 162 MiB | 158 MiB | 25.4 MB | 49,187 |
| 1,122 | 445 MiB | 342 MiB | 25.4 MB | 49,789 |
| 1,842 | 724 MiB | 524 MiB | 25.3 MB | 48,829 |
| 2,882 | 1,131 MiB | 788 MiB | 25.4 MB | 49,541 |

Per table at the stop (`arkiv-proposer state table-stats`, a walk of the
stopped file):

| Table | Archive entries | Archive pages | Pruned entries | Pruned pages |
| --- | ---: | ---: | ---: | ---: |
| `objects_ns_…01` (the items' trie objects) | 503,012 | 397 MiB | 59,155 | 42.5 MiB |
| `blocks` (certificate, body reference, feed JSON) | 2,915 | 724 MiB | 2,914 | 724 MiB |
| `inclusions` | 23,165 | 8.7 MiB | 23,165 | 8.7 MiB |
| `gc_*` (seven tables) | — | — | 97,000 rows | 7.9 MiB |
| everything else | | 6.6 MiB | | 2.3 MiB |

- **The trie objects are bounded.** After the window filled (head ≈ 280),
  stored object bytes stayed at 25.4 MB ± 0.5% and dead objects at about
  49,500: one window of garbage, about 190 objects per block. Nothing
  waited for release (`state_gc_pending_release` 0 throughout); 490,740
  objects were deleted. The sparse count table held about 2,040 rows,
  3.5% of the objects.
- **The file is not bounded.** It still grows 0.25 MiB per block (archive:
  0.39 MiB). All of that is the `blocks` table: about 180 KiB of record per
  block, almost all feed JSON, kept for every height. `history.redb` grew
  the same in both runs (362 MiB, the bodies). Neither is pruned by this
  work (see the proposal, "Not done").

## Offline tools on copies of the stopped files

| Tool | Input | Result | Time |
| --- | --- | --- | ---: |
| `state migrate --state-retention 256` | archive, 2,056 MiB file, head 2,914 | 1,028 MiB file (772 MiB allocated): 60,938 objects of the snapshots 2,658–2,914 and every record | 2.2 s |
| `state gc-enable` | archive copy, 532,177 objects | 5,792 dead roots pending release, 168,460 shared rows (old versions still count until released) | 0.9 s |
| `state compact` | pruned, 880 MiB | 791 MiB | 0.9 s |

A producer started on the migrated run directory without
`--state-retention` came up with a window of 256, `stateGc` `on` and the
floor at 2,681, and `state gc-verify` passed after it ran. The migration
peaked at 1.4 GiB resident: the copy is opened with redb's default 1 GiB
cache.

## Not verified

- Anything on a state larger than RAM: commit cost, seeding time and spill
  size at adoption, and `gc_shared` on Sourcify.
- Long runs: 12 minutes is about eleven windows.
