# Record expiry soak: both files stay flat (2026-11-21)

Checks [per-height record expiry](../proposals/2026-11-20-index-pruning.md#per-height-records-2026-11-21)
on the same load as the [2026-11-20 pruning soak](2026-11-20-pruning-patch-soak.md):
2,000 items created, then patches of two indexed attributes at the block cap
for 12 minutes, so the state stays the same size and only history
accumulates. Two producers ran side by side, both `--state-retention 256`
(garbage collection on):

- **window**: `--body-retention window`, bodies expire with the records;
- **bodies**: the default `--body-retention archive`, bodies kept for
  followers and light clients.

- Binary: the working tree on top of `d42544f` with this change, release
  (stable toolchain), SHA-256 `e414b9fdd021f7fc…`.
- Host: AMD Ryzen 5 3600, 12 threads, 62.7 GiB RAM, ext4 on md RAID.
- `scripts/soak/run.py --patch-min 12 --growth-min 0 --churn-min 0
  --readers-min 0 --state-retention 256 [--body-retention window]`, ports
  18720 and 18730, 250 ms blocks.
- Results: `~/scx1332/soak-work/2026-11-21-records-{window,bodies}`; the
  migration and restart measurements in `~/scx1332/soak-work/2026-11-21-records`.

## Files, allocated bytes, by head

| Head | window: `node.redb` | window: `history.redb` | bodies: `node.redb` | bodies: `history.redb` |
| ---: | ---: | ---: | ---: | ---: |
| 322 | 130.0 MiB | 36.5 MiB | 130.0 MiB | 43.6 MiB |
| 1,002 | 130.0 MiB | 36.5 MiB | 130.9 MiB | 127.9 MiB |
| 2,004 | 131.3 MiB | 36.5 MiB | 131.0 MiB | 252.8 MiB |
| 2,805 | 131.9 MiB | 36.5 MiB | 131.0 MiB | 348.4 MiB |
| 2,956 (after restart) | 132.0 MiB | 36.5 MiB | 131.0 MiB | 362.9 MiB |

The 2026-11-20 pruned run on the same load (trie objects collected, records
kept): `node.redb` 158 MiB at head 402, 342 MiB at 1,122, 524 MiB at 1,842
and 788 MiB at 2,882; `history.redb` 362 MiB at the stop.

- **`node.redb` is flat** from the moment the window filled (head ≈ 300):
  +2 MiB over 2,600 blocks in both runs, against 0.25 MiB per block before.
- **`history.redb` is flat with `--body-retention window`** (36.5 MiB) and
  grows about 125 KiB per block when bodies are kept (the same 362 MiB the
  2026-11-20 runs reached).

Per table at the stop (`state table-stats`, window run; the bodies run is
within 1%):

| Table | Entries | Pages | 2026-11-20 pruned |
| --- | ---: | ---: | ---: |
| `blocks` | 258 | 53.6 MiB | 2,914 entries, 724 MiB |
| `inclusions` | 1,712 | 0.8 MiB | 23,165, 8.7 MiB |
| `manifests` / `block_index` | 258 / 259 | 0.16 MiB | 2,914 each |
| `meta` (floor records, inclusion lists) | 735 | 0.23 MiB | one floor record per height |
| `objects_ns_…01` | 68,533 | 43.6 MiB | 59,155, 42.5 MiB |
| `gc_*` | | 7.6 MiB | 7.9 MiB |

Bytes per block of history: `blocks` about 208 KiB of pages per kept
block (the feed JSON of 125 patches), inclusions about 470 B per kept
block; all of it now exists for 258 heights only, so per-block growth is
zero instead of about 190 KiB (`node.redb`) plus 125 KiB (`history.redb`).

## Throughput and commit

| | window | bodies | 2026-11-20 pruned |
| --- | ---: | ---: | ---: |
| Patches | 358,912 | 361,344 | 368,640 |
| Patches per second | 498 | 502 | 512 |
| Transaction p95, median of 10 s windows | 254 ms | 254 ms | 251 ms |
| State commit, mean per minute (minutes 2–12) | 23.6–32.7 ms, one minute 49 ms | 25.9–34.2 ms, one minute 49 ms | 23.7 ms |

Deleting a height's records in the commit costs little next to garbage
collection: the commit stays about where it was (the 49 ms minute is in
both runs at the same time, so it is the host, not either setting).

## Restart

| | Before (old binary) | After |
| --- | ---: | ---: |
| Soak restart, ready again (head ≈ 2,910) | 24.4 s (2026-11-20 pruned run) | 2.32 s (window), 2.37 s (bodies) |
| Paused producer on a copy of the 2026-11-20 pruned run, head 2,913, until `/status` answers | 25.3 s, 24.9 s | 0.24 s (after the floor rose) |

The old restart re-read and authenticated every block from genesis to
rebuild the hash index (about 8.6 ms per block); the new one reads the 258
kept blocks. "Ready" in the soak also waits for the node to produce, so it
is not all index time.

## Migration of an old file

A copy of the 2026-11-20 pruned run (`node.redb` 880 MiB, `history.redb`
564 MiB, head 2,913), started with the new binary, `--body-retention
window`:

- the start indexed the old files (`records_migrated`): 2,896 inclusion
  lists (23,165 inclusions) and 2,914 history index rows, in 5.6 s;
- a paused start serves at once but keeps `recordFloor` 0 until its first
  commit; with production running, the first commit raised the floor to
  2,660 and the backlog of 2,657 heights was gone within about 20 blocks
  (a sixteenth of the backlog plus 65 per block); bodies followed;
- `state compact` then shrank `node.redb` from 880 MiB to 7.6 MiB.

## Checked by hand during the run

On the window node at head 366: `eth_getBlockByNumber(5)` answered
`-32006` with `{"requested":"0x5","floor":"0x6e"}`, and
`/signed/v1/feed/blocks/5` answered 410. On the bodies node,
`/signed/v1/headers` from genesis still answered 200.

## Not verified

- The Sourcify state (12,604 blocks of records): another experiment uses
  that run directory, so it was not copied or migrated.
- Long runs, and followers syncing from these producers over HTTP (the
  follower path is covered by tests, not by this soak).
- `history.redb` is not compacted by `state compact` (it compacts
  `node.redb` only); freed pages are reused.
