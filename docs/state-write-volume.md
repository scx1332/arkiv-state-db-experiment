# Write volume of the state database

Measured 2026-11-27 on rogue-one, on copies of the Kenobi state database
(chain 61122026, `node.redb` 114 GiB in 27 redb regions, 256-block state
window, 2 s blocks). A node at this size writes about 14 MiB to disk per
block, about 590 GiB a day, while its state changes by a few KiB per block
and the file does not grow. This page records what was tried against it,
what each option costs, and what was chosen.

## Result

| | Now | Durable state every 64 blocks | Quick repair off |
| --- | ---: | ---: | ---: |
| Written per block while syncing | 13.8 MiB | 0.67 MiB | 0.74 MiB |
| Written per block at the head | 14.0 MiB | 0.89 MiB | not measured |
| Written per day at 2 s blocks | 590 GiB | 37 GiB | not measured |
| Sync speed from a peer | 20 blocks/s | 43 to 63 blocks/s | 32 blocks/s |
| State commit, mean | 32 to 36 ms | 5 ms | 12 ms |
| Restart after `kill -9` | 1 s | 1 s, replays up to 63 blocks | 58 min |
| Restart after a clean stop | 1 s | 1 s | 0.4 s |
| A served block survives a crash | yes | yes | yes |

**Chosen: `--state-durable-every N`** (new flag, producer and full nodes,
default 1, which is today's behaviour). It makes only every N-th state
commit durable and rebuilds the rest from `history.redb` after a crash.
Recommended value: 64.

Quick repair off is rejected: after any unclean stop the node is away for
about an hour at this size, and for a single producer that is an hour
without blocks.

A change inside redb would remove most of the cost while keeping every
state commit durable (see [What redb saves](#what-redb-saves)). It was
measured, not built: it needs a patched redb.

## Where the bytes go

redb's quick repair saves its allocator state (which pages of the file are
free) with every commit, so that a crashed database opens without walking
the file. It saves all of it, every time:

| Per state commit, 1,200 commits, counter build of redb | |
| --- | ---: |
| Allocator state saved | 6.75 MiB (27 regions of 256 KiB and a few bytes) |
| Regions whose state differs from the previous commit | 2.75 |
| 4 KiB pages of it that differ | 39 (154 KiB) |
| Bytes that differ | 221 |
| Bytes reaching the device per block, all files | 13.85 MiB |

The device sees twice the saved bytes because of page rounding: redb's
pages come in powers of two, a region's state is just over 256 KiB, so each
one is stored in a 512 KiB page that is written whole (27 of them are
13.5 MiB; a system-call trace shows the 512 KiB writes). The cost is
proportional to the size of the file, not to the traffic: about 0.12 MiB
per GiB of file per commit.

`history.redb` (one region so far) pays the same way on a small scale:
61 KiB saved per commit while the file was 0.84 GiB, two commits per block
on a follower. It grows (see [Not solved](#not-solved)).

## The options

### 1. Durable state every N blocks (chosen)

Each block still commits to `node.redb`, and readers see it at once, but
only every N-th commit is synced (redb's `Durability::None` between). A
non-durable redb commit does not save the allocator state and never writes
to a page a durable commit uses, so the durable state on disk is always a
complete earlier block.

Nothing a node has acknowledged depends on those commits. The body of every
block is committed durably to `history.redb` before its state commit, as
before. `node.redb` becomes a checkpoint of executing the history:

- After a crash the state is up to N-1 blocks behind the history. The node
  executes those blocks again from `history.redb` before it serves,
  produces or syncs. A replayed block is authenticated and must reproduce
  its signed header, exactly as in an import.
- A clean stop (SIGTERM) closes both databases, which makes every commit
  durable. Nothing is replayed after it.

Measured on a fresh install of the snapshot of block 167,734, syncing the
same 2,000 blocks from the local follower, page cache dropped before each
start, then `kill -9` in mid-sync:

| N | Written per block | Sync, blocks/s | State commit, mean | Replayed after the kill | Restart |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 13.84 MiB | 20.3 | 32.4 ms | 0 | 1.5 s |
| 4 | 3.86 MiB | 32.6 | 13.2 ms | 4 | 1.1 s |
| 8 | 2.18 MiB | 40.9 | 7.9 ms | 2 | 0.4 s |
| 16 | 1.32 MiB | 54.8 | 6.1 ms | 0 | 0.8 s |
| 32 | 0.90 MiB | 60.7 | 5.2 ms | 25 | 1.1 s |
| 64 | 0.67 MiB | 42.6 | 5.1 ms | 3 | 1.3 s |
| 128 | 0.56 MiB | 63.0 | 4.3 ms | 7 | 0.8 s |
| 256 | 0.50 MiB | 61.9 | 4.4 ms | 206 | 1.9 s |

No run lost a block. The written volume follows 0.44 + 13.4 / N MiB per
block: the saved allocator state is divided by N, and 0.44 MiB of data
pages and history writes remain. The sync speed above N = 8 is set by the
two history commits per block, whose time varied between runs (4.0 to
7.1 ms each, the 64 row is a slow one), not by N.

64 is the recommended value: a replay of at most 63 blocks (well under a
second), and little left to gain beyond it.

The whole way with N = 64, on another fresh install: synced 33,938 blocks
to the head of the chain while being killed with `kill -9` at twelve random
moments, then ten minutes at the head, then a clean stop.

| | |
| --- | ---: |
| Kills, blocks lost | 12, 0 |
| Blocks replayed from history per restart | 13 to 57 (318 in all) |
| Slowest restart | 1.3 s |
| Sync including the restarts | 43.3 blocks/s, 0.72 MiB per block |
| Block 201,727 against the follower and against Kenobi | identical |
| At the head, 300 blocks | 0.89 MiB per block, state commit 2.9 ms |
| The stock follower at the head, same evening | 14.0 MiB per block, state commit 38 ms |
| Restart after the clean stop | 0.3 s, nothing replayed |

The at-head figure is higher than the sync figure because `history.redb`
doubled its file from 0.84 to 1.68 GiB during this run, and with it its own
saved allocator state.

What it costs:

- **The producer depends on the replay.** All the blocks above the last
  durable state commit, except possibly the last, were served. A producer
  that came back at the checkpoint and simply continued would sign other
  blocks at those heights. It therefore replays first, and refuses to start
  if a block cannot be executed again. The test
  `without_the_replay_a_killed_producer_signs_a_served_height_again` shows
  the failure that the replay prevents.
- **A body above the state head is adopted, not an orphan.** With every
  commit durable, such a body belongs to a block that was never published
  and the producer builds another one at its height
  ([proposer spec](signed-proposer-v1.md)). That rule cannot hold here, and
  does not: this is the one place where the node behaves differently.
- **The state file remembers.** Before its first non-durable commit the
  store durably marks the file (`store/shared_durability`). A node replays
  whenever the mark is there, whatever its flags. A node started with N = 1
  again replays once more and removes the mark.
- **Older binaries.** A binary without the replay (0.1.0-alpha.9 and
  earlier) must not open such a file as a producer after an unclean stop:
  it would sign over served heights. After a clean stop any binary may open
  it. A follower on an old binary is safe either way: it fetches the blocks
  from its peer.
- **N is at most the state window.** The bodies to replay must still be in
  `history.redb`, and `--body-retention window` deletes them below the
  window. The flag is refused above the window, and above 1,024.
- **In-process recovery.** After an I/O error the store reopens at its last
  durable commit, which is older than what the engine holds. The engine
  refuses that ("unexpected head"), the node stays fenced, and a restart
  replays. Nothing continues on a state it cannot account for.
- **The checkpoint still costs 13 MiB**, and grows with the file. Per block
  it is divided by N.

### 2. Quick repair off (rejected)

`ARKIV_STATE_QUICK_REPAIR=0` (a measurement switch, not a flag). Commits
stop saving the allocator state. Two-phase commit stays on
(`ARKIV_STATE_TWO_PHASE`, default 1), so a committed root never depends on
page checksums.

| | |
| --- | ---: |
| Written per block | 0.74 MiB |
| Sync | 32.0 blocks/s |
| State commit, mean | 12.2 ms |
| Restart after `kill -9`, page cache dropped | 3,464 s (58 min) |
| Read during that restart | about 285 GiB, 2.5 times the file |
| Restart after SIGTERM | 0.4 s |

The price is redb's full repair after every unclean stop: it verifies and
walks the whole file before the node answers anything, and the time grows
with the file. It also needs the clean close that this work added: before
it, the node exited on SIGTERM without closing its databases, which with
quick repair off would have meant a full repair on every restart.

One-phase commit was not measured. redb documents it as open to a crafted
partial commit by whoever controls the data written, and a node stores data
that its users choose.

### 3. redb saves only what changed (not built)

<a id="what-redb-saves"></a>
The counter above says what a smarter save could do, with every state
commit still durable and the restart still instant:

| redb change | Allocator state written per commit | Estimated total per block |
| --- | ---: | ---: |
| None (redb 4.3.0 to 4.6.0) | 6.75 MiB | 13.8 MiB (measured) |
| Rewrite only the regions that changed | 0.72 MiB, in 512 KiB pages 1.4 MiB | about 1.9 MiB |
| Rewrite only the 4 KiB pages that changed | 0.15 MiB | about 0.6 MiB |

The estimates add the 0.44 MiB that does not depend on the allocator
state. Neither was built. Both change redb's commit path, so they mean carrying a
patched redb through its crash tests and ours, or getting the change
accepted upstream; redb 4.6.0 (2026-11-13) still saves every region. Sync
speed would stay near today's, since every commit would still sync twice.

This is the option to pursue if per-block durability of the state file
itself ever matters. With option 1 in place it is not urgent.

## What was added

- `RedbConfig::durable_every`, `RedbConfig::two_phase_commit`,
  `RedbStore::checkpoint`, `close`, `shared_durability`,
  `clear_shared_durability` (`arkiv-db-redb`).
- `Host::replay_history` (`arkiv-node`), called at start for a marked state
  file, by the producer and by full nodes.
- `--state-durable-every N` on `arkiv-proposer producer` and `full`.
- The node closes its databases on SIGTERM and SIGINT. A start that takes a
  second or more to open a database logs `store_open` with the time and
  whether redb repaired.

Tests:

- `crates/arkiv-db-redb/tests/it/checkpoint.rs`: after every commit, the
  image a power cut would leave (synced data only, all unsynced writes, and
  torn subsets of them) reopens at the last durable commit, complete, with
  every later commit absent; `checkpoint` and `close`; the mark.
- `crates/arkiv-node/tests/signed_checkpoint.rs`: a killed producer and a
  killed follower replay to the head they had, with the same blocks; the
  producer's next block extends the chain for a follower that never
  stopped; without the replay the follower reports a chain conflict.
- `scripts/state-durable-every-smoke.py`: real processes, a producer at
  50 ms blocks and a follower, eight `kill -9` rounds, then a clean stop,
  then a restart without the flag.

## Not solved

- **`history.redb`.** A follower wrote about 0.25 MiB per block to it
  while the file was 0.84 GiB (a system-call trace of 20 blocks), most of
  it the same saved allocator state, in two commits per block. History
  commits must stay durable, so option 1 does not apply. The file grows
  with the bodies it keeps, in doubling steps: it was 0.84 GiB for five
  days and became 1.68 GiB on the evening of the measurements, which is
  about 0.16 GiB of bodies a day. Past one region (about 4 GiB) every GiB
  of history adds 0.12 MiB per commit. At this rate a follower that keeps
  every body writes about 2.4 MiB per block to its history in two months
  and today's 14 MiB in about a year. `--body-retention window` stops the
  growth (it does not shrink a file that has grown), one history commit per
  block instead of two would halve the cost, and option 3 would remove it.
- **The 0.44 MiB of data pages per block**, the random page writes per tree
  node described in the [load benchmark](golem-db-load-benchmark.md).
- A producer was tested as local processes and in unit tests, not on a
  chain of Kenobi's size. Kenobi itself was not touched.

## Reproducing

`scripts/state-write-volume.sh` (its header lists what it needs) runs a
second full node on a fresh `snapshot install` of a snapshot, syncing from a
local full node, with `/proc/PID/io` for bytes and the node's own metrics
for commit times. `run` gave the rows of the tables above, `torture` the
whole-way run. The counter build of redb is a 60-line measurement patch to
`try_save_allocator_state` and is not in the repository. The raw results
are on the measuring machine (`~/scx1332/wv-test/out`).
