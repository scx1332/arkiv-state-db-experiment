# Compacting the Sourcify state file (2026-11-21)

Does packing redb's pages pay off on the pruned Sourcify state
(`2026-11-20-sourcify-pruned-soak.md`, 16.8 million entities, head 12,563),
and how fast does the empty space come back under patching?

Binary `f659c2c` (state pruning with a 256-block window; per-height records
not yet expiring). Same host as the earlier soaks.

## How redb fills pages

One million 32-byte random keys with 415-byte values, redb 4.3, written in
batches of 50,000 (a scratch test, not in the repository):

| Load | Page fill |
| --- | --- |
| Random key order | 69% |
| Sorted key order | 96% |
| Sorted, then 5% of keys replaced at random | 74% |
| Sorted, then 10% replaced | 68% |
| Sorted, then 25% or more replaced | 63% |

A packed page splits on its first insert, so the gain of a sorted load is
gone after about a tenth of the keys have changed.

## The real file

A copy of every table in key order into a new file (a scratch tool, one
reader and one writer, 2 million entries per commit) took 42 minutes:

| | Before | Key-order copy |
| --- | --- | --- |
| File | 118.1 GB | 89.3 GB |
| Object table pages | 113.0 GB (55% full) | 87.1 GB (71% full) |
| `gc_shared` pages | 1.68 GB | 1.09 GB |

The object table reaches 71%, not 96%: its values vary in size (mean about
450 bytes), and large values leave the ends of their pages unused. Not
measured further.

## Thirty minutes of patches on each

The same workload on both files: `reverify` patches of Sourcify contracts
from offset 300,000, about 210,000 patches each at 115–120 patches/s.

| | Compacted | Not compacted |
| --- | --- | --- |
| Patches | 209,664 | 207,232 |
| Allocated bytes | 89.6 → 95.2 GB | 116.1 → 117.2 GB |
| Object table pages | 87.1 → 91.8 GB (67% full) | 113.0 → 113.3 GB (55% full) |
| Growth per patch | about 27 KB | about 5 KB |

- The compacted file grows steadily, about 0.2 GB a minute: each patch
  writes about a dozen new trie objects at random keys into full pages.
- The half-full file absorbs the same patches in its existing slack and
  in free pages.
- After 210,000 patches the compacted file is still 22 GB smaller. Its
  growth will slow as its fill falls towards the uncompacted file's.

## Conclusions

- **Compaction is worth it for a mostly static dataset** such as Sourcify,
  where patches touch a small share of the contracts. It saves about a
  quarter of the file at once and keeps most of that for hundreds of
  thousands of patches.
- **It is not a fix for churn.** A file under heavy random writes settles
  at roughly 55–65% fill whatever it started from.
- **The cheapest place to pack is the genesis build.** It leaves the
  object table 55% full today. Writing objects in key order there should
  give the same 89 GB file without a 42-minute copy. Not tried.
- `state compact` (redb's `compact`) relocates pages towards the start of
  the file. Whether it also merges half-full pages was not tested.

## Not done

- Packing in `genesis stream-build` itself.
- The value-size distribution of the object table, to explain the 71% fill.
- A run long enough to see the compacted file's growth level off.
