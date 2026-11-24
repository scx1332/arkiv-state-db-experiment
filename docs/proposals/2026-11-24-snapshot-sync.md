# Proposal: start full nodes from a database snapshot

Status: proposed 2026-11-24. The transport (plan items 4 and 5) was
implemented on 2026-11-24; the snapshot commands, the pin and the prefill
skip (items 1 to 3) are not. Decided so far: the snapshot is a copy of the
node's database files, genesis and prefill are not part of syncing, the
node that made a snapshot pins the block bodies since it, and the transport
is plain HTTP (TLS is a proxy's job). The remaining choices are listed
under [Open decisions](#open-decisions).

This is the snapshot bootstrap that
[state history pruning](../pruning-design.md#phases) (phase 3) and the
[256-block pruning proposal](2026-11-20-index-pruning.md#followers-light-clients-and-bodies---body-retention)
left open.

## Problem

A full follower starts from genesis: it downloads the prefill stream,
builds the genesis state and replays every block. On Kenobi (chain
61122026) that is a 42 GiB prefill download and several hours of state
building before the first block. It also requires a peer that still has
every block body, which rules out `--body-retention window` on any node
that serves new followers.

A new full node should instead start from the state at a recent block `N`
and replay only the blocks after it.

## What already works

Tested on 2026-11-24 with the 0.1.0-alpha.7 build, on a small local chain
without a prefill (state window 8, record window 256, 250 ms blocks). The
producer was stopped at height `N`, `node.redb` and `history.redb` were
copied into an empty directory, the producer was restarted, and a `full`
node was started on the copy with the usual pins.

| Producer `--body-retention` | Blocks produced after the copy | Result |
| --- | ---: | --- |
| `archive` | 5 | opened at `N`, caught up, same head (height, hash, state root) |
| `archive` | 200 | same |
| `window` | 60 | same |
| `window` | 320 | never syncs; logs `HistoryPruned` on every poll |

The follower fetched `trust.json`, `genesis.bin` and `certificate.bin` from
the peer and needed nothing else. `Host::open` accepts the copy because a
producer's files hold nothing that is specific to the producer: the key is
not stored, and the follower passes the same `--source-id`.

Not tested: a chain with a prefill, and Kenobi's file.

## What the pruning allows

- The **state window** of the peer does not matter. The follower holds the
  state at `N` and re-executes forward. It never reads a peer's state.
- The **bodies** of blocks `N + 1` to the head must be on the peer the
  follower syncs from. With `--body-retention window` they expire with the
  records, 256 blocks after they were produced: 8.5 minutes at 2 s blocks.
- Racing that window (snapshot, copy, start within 8.5 minutes) works on
  one host. Over a network it needs about 1.8 Gbit/s sustained for
  Kenobi's 106 GiB file, and the requirement grows with the state.

## Design

### The snapshot

A snapshot of height `N` is a directory:

| File | Content |
| --- | --- |
| `node.redb` | state and node records with head `N` |
| `history.redb` | the bodies the node kept, up to `N` |
| `genesis.bin`, `certificate.bin` | public genesis and its certificate, as the node exports them |
| `snapshot.json` | chain ID, run ID, source ID, genesis hash, proposer, height, header hash, state root, node version, and the size and SHA-256 of each file |

`prefill.bin` is not part of it.

Version 1 makes it from a stopped node:
`arkiv-proposer snapshot create --run-dir RUN --out DIR`. Opening takes the
file locks, so it is refused while a node runs on `RUN`. It copies the two
database files, checks the copy opens with the head it read, and writes
`snapshot.json` last.

The snapshot is made on a follower kept for that purpose, so the chain does
not stop. The files are published as static files over HTTP with range
requests (nginx or an object store). The node does not serve them.

### The pin (decided 2026-11-24)

The node that made the snapshot keeps every body from `N` on, whatever its
body retention, until a later snapshot replaces the pin.

- `snapshot create` writes `RUN/snapshot-pin` (the height, written to a
  temporary file, synced and renamed). The file is not part of the
  snapshot, so a node installed from it has no pin of its own.
- `Host::expire_bodies` deletes below `min(record floor - 1, pin)`. With
  `--body-retention archive` the pin changes nothing.
- The pin is the height of the oldest snapshot still published. A new
  snapshot moves it; `snapshot unpin` removes it.
- Status reports `bodyPin` beside `bodyFloor`.
- Headers and bodies below the record floor are already served from the
  height index of `history.redb` while the body exists
  (`Host::chain_certificate`, `Host::replication_block`), so the pinned
  heights need no new read path.

A new node downloads the snapshot and syncs blocks `N + 1` onwards from the
node that made it. Every other node, the producer included, can prune at
the window. With one snapshot a day the pinned tail on Kenobi is about a
day of bodies, roughly 28 MB.

One timing limit remains in version 1. While the snapshot node is stopped
for the copy, its own peer keeps pruning. The copy must finish, and the
node must be back, inside the peer's body window, or the snapshot node
cannot catch up either. A local copy of Kenobi's file fits today. The
online snapshot below removes the limit.

### Installing

`arkiv-proposer snapshot install --from DIR --run-dir RUN` with the same
pins as `full` (`--genesis-hash`, `--proposer`, `--chain-id`, `--run-id`,
`--source-id`):

1. Check `snapshot.json` against the pins and each file against its
   SHA-256.
2. Ask the peer for its status and refuse a snapshot it cannot continue:
   `N + 1` must not be below the peer's `bodyFloor`.
3. Move the files into `RUN` and open them as `full` does.

Trust stays with the pins. `Host::open` already checks that the stored
genesis is the pinned one, that the head record carries a certificate
signed by the pinned proposer, and that the head manifest has the state
root of that certificate. Every state object is stored under the
keccak-256 of its bytes, is checked against it on each read, and is linked
to the state root. A tampered snapshot can therefore fail a read, but it
cannot answer one wrongly.

Three things in the files are not bound to the state root: the garbage
collection counts, the records of the last 256 heights (feed, inclusions)
and the bodies. A wrong count or a missing object stops the node later. A
snapshot from an untrusted source is installed with `--verify-state`, which
runs the reachability walk that genesis adoption uses and rebuilds the
counts (`gc::enable`).

### Follower start

A `full` node whose `node.redb` already has a head skips the prefill: it
neither downloads nor audits `prefill.bin`. Today it does both before it
looks at the database, so on Kenobi a snapshot follower would fetch 42 GiB
it never reads. Such a node does not serve `/signed/v1/prefill/chunk`.

A follower that is refused `HistoryPruned` for its next block cannot
recover by retrying. It logs once that it must be reinstalled from a
snapshot and exits, instead of polling forever.

### Transport

- **Plain HTTP peers.** `validate_peer` accepts HTTPS, or HTTP to
  loopback, and requires the path `/`. Add `--allow-http-peers` and accept
  a path prefix (`https://host/rpc`). Headers are checked against the
  pinned proposer, bodies against the header's input digest, and every
  block is re-executed, so the transport adds no trust.
- **Public bodies.** Bodies are served only on the private listener, behind
  the bearer token, which plain HTTP would expose. Add public
  `GET /signed/v1/blocks?afterHeight=H&limit=K`, bounded by count and by
  the 8 MiB response cap. Bodies are public already through JSON-RPC.
  Followers then need no token.
- **Batches.** A follower imports one block per 50 ms poll, about 20
  blocks per second. A day of 2 s blocks takes about 36 minutes. The
  batch endpoint removes the round trip per block.

## Plan

1. Skip the prefill when the database has a head; exit on a `HistoryPruned`
   next block.
2. `snapshot create`, the pin, `bodyPin` in status.
3. `snapshot install` with the SHA-256, pin and body-floor checks, and
   `--verify-state`.
4. `--allow-http-peers` and the path prefix. **Implemented 2026-11-24.**
   A peer is `https://host[:port][/prefix]`, or `http://` to loopback;
   `--allow-http-peers true` on `full` and `light` accepts `http://` to any
   host. Credentials, a query and a fragment in the URL are refused.
5. The public batch endpoint; followers without a token. **Implemented
   2026-11-24.** Public `GET /signed/v1/blocks?afterHeight=H&afterHash=X&limit=K`
   on producer and full nodes answers up to 64 consecutive blocks, cut at
   about 3 MiB of block bytes, from the height index of `history.redb`. A
   `full` node without `--replication-peer` syncs from it, one batch per
   50 ms poll, with no token. Nothing stored and nothing in block validity
   changed. A peer must run this build or later to serve `GET blocks`; with
   `--replication-peer` and the token a new follower still syncs from older
   peers, one block per poll as before.
6. A smoke test: the 2026-11-24 experiment, with `--body-retention window`
   on the producer and a pinned snapshot follower, more than 256 blocks
   between the snapshot and the install.
7. Run it on a copy of Kenobi and record sizes and times.

Later, in no fixed order:

- **Online snapshot.** Begin read transactions on both files under the
  node lock and copy every table into new files while the node keeps
  running. The copy is consistent and has no free pages (Kenobi: 53.9 GiB
  of live objects in a 106 GiB file). It removes the stop, the timing
  limit and the need for a dedicated follower. While the transaction is
  open redb cannot reuse freed pages, so the source file grows by what is
  written meanwhile.
- **Compact offline snapshot** from `arkiv_db_redb::copy_reachable` with
  the head as the only snapshot. Its time on a file of this size is not
  measured.
- Several peers with failover, and followers serving other followers.
- A checkpoint for light clients, which still append headers from genesis.

## Open decisions

| Decision | Proposed |
| --- | --- |
| Where snapshots are made | a dedicated follower, stopped for the copy; the online snapshot later |
| Who installs snapshots | operators we know: SHA-256 by default, `--verify-state` on request |
| Body retention on Kenobi | `archive` as now; the pin makes `window` possible later |
| Where the files are hosted | static files beside the sync page, or R2 |
