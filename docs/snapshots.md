# Snapshots: starting a full node from a copy of the database

A new full node normally starts from genesis: it downloads the prefill,
builds the genesis state and replays every block. A snapshot replaces that
with a copy of the two database files of a full node at some height `N`.
The new node opens the copy with its usual pins and syncs only the blocks
after `N`. It needs no prefill and no token.

This page is for operators. The design and its history are in the
[snapshot sync proposal](proposals/2026-11-24-snapshot-sync.md).

| Step | Command | Where |
| --- | --- | --- |
| Make a snapshot | `arkiv-proposer snapshot create --run-dir RUN --out DIR` | a stopped full node kept for this |
| Publish it | any static file server | |
| Install it | `arkiv-proposer snapshot install --from DIR --run-dir RUN <pins> --peer URL` | the new node |
| Start the node | `arkiv-proposer full --run-dir RUN --peer URL <pins>` | the new node |

## What a snapshot is

A directory with five files:

| File | Content |
| --- | --- |
| `node.redb` | state and node records, head `N` |
| `history.redb` | the block bodies the node kept, up to `N` |
| `genesis.bin`, `certificate.bin` | the public genesis and its certificate, as every node exports them |
| `snapshot.json` | which chain and head this is, and the size and SHA-256 of the four files |

`snapshot.json` is written last. A directory without it is not a snapshot.
`prefill.bin` and the pin (below) are never part of one.

```json
{
  "formatVersion": 1,
  "chainId": "61122026",
  "runId": "107407c1f817f902e6f8d6e7f0491b15",
  "sourceId": "00000000-0000-4000-8000-000000000001",
  "genesisHash": "0xee6d…",
  "proposer": "0x126d…",
  "height": "71494",
  "headerHash": "0x0e5c…",
  "stateRoot": "0xd36e…",
  "bodyFloor": "0",
  "nodeVersion": "0.1.0-alpha.8",
  "createdAt": "2026-11-24T22:57:28Z",
  "files": [
    { "name": "node.redb", "bytes": 113816076288, "sha256": "58f1…" },
    { "name": "history.redb", "bytes": 966367641, "sha256": "041a…" },
    { "name": "genesis.bin", "bytes": 557, "sha256": "ca48…" },
    { "name": "certificate.bin", "bytes": 257, "sha256": "f970…" }
  ]
}
```

Heights and the chain ID are decimal strings and hashes are `0x` hex, as in
the node's status. `sha256` is plain hex, as `sha256sum` prints it.
`bodyFloor` is the status field of the same name: bodies of heights 1 up to
(excluding) it are not in `history.redb`; `0` means every body is there.

## Create

The node must be stopped (`SIGTERM`, `docker stop`). A database file can
only be copied while no process has it open.

Stop it cleanly: by default only every 64th state commit is durable
(`--state-durable-every N`, [write volume](state-write-volume.md)). A clean stop makes its state as
new as its history. After a kill the state file is up to N-1 blocks behind;
a snapshot of it is still correct, at the lower height, and the node that
installs it replays the missing blocks from the snapshot's `history.redb`
when it starts. Only a build with the replay does that: a snapshot of a
killed node is not for 0.1.0-alpha.9 and earlier producers.

```sh
arkiv-proposer snapshot create --run-dir /srv/follower --out /srv/snapshots/71494
```

No identity flags are needed: the chain is read from the database, as a
producer does on restart. The command:

1. refuses an `--out` that exists and is not empty;
2. opens both databases as a node does and reads the head. A running node
   holds their locks, and the command stops with `a node is running on this
   run directory; stop it first`. The databases are closed again. (Opening
   and closing writes a few pages to the files, as every start of the node
   does.)
3. locks both files against a starting node until it is done, and copies
   each in one pass that also computes its SHA-256. Files and directory are
   synced;
4. writes `genesis.bin` and `certificate.bin`;
5. opens each copy read-only, which leaves its bytes as they were hashed,
   and checks that it has the storage head read in step 2;
6. writes `snapshot.json` through a temporary file and a rename;
7. pins the node at the snapshot height (next section).

Progress is one JSON line on stderr every ten seconds and one per finished
file:

```json
{"event":"snapshot_progress","stage":"copy","file":"node.redb","bytes":53687091200,"totalBytes":113816076288,"elapsedSeconds":61.2,"complete":false}
```

The result is one JSON line on stdout: `snapshot` (the content of
`snapshot.json`), `out`, `pin` and `seconds`. The command refuses to start
the copy without free space for it, and removes what it wrote when it
fails. If it is killed, delete the directory: without `snapshot.json` it is
not a snapshot.

A snapshot is a full node's files. Make it on a follower kept for the
purpose, so that the chain does not stop. A producer's files also work, and
hold nothing specific to the producer (its key is not in them).

## The pin

A node with `--body-retention window` deletes block bodies 256 blocks after
they were produced. A node installed from a snapshot at `N` needs every body
after `N`, usually hours or days later. The node that made the snapshot
therefore keeps them:

- `RUN/snapshot-pin` holds a height. A node started with that file keeps
  every body from that height on, whatever `--body-retention` says. Bodies
  below the pin expire as before.
- `snapshot create` writes the pin (the snapshot height) by default.
- Status reports it as `bodyPin`, next to `bodyFloor`: a decimal string, or
  `null` without a pin.
- The node reads the file when it starts. **A changed pin takes effect at
  the next start.**
- With `--body-retention archive` the pin changes nothing: every body is
  kept anyway.

The pin is the height of the oldest snapshot you still publish.

| Command | Effect |
| --- | --- |
| `snapshot create …` | pin = snapshot height. Bodies between an older pin and this one expire from the next start on. |
| `snapshot create … --keep-pin` | an existing lower pin stays (you still publish the older snapshot); without one the snapshot height is pinned |
| `snapshot create … --no-pin` | the pin is left as it is |
| `snapshot pin --run-dir RUN --height H` | pin = `H` |
| `snapshot unpin --run-dir RUN` | no pin; the kept bodies expire from the next start on |

`snapshot pin` on a stopped node checks the height: not above the head, and
not below `bodyFloor`, because a pin does not bring back a body that is
already deleted. On a running node it writes the file unchecked (`"checked":
false`) for the next start.

Headers and bodies below the record floor are served from the height index
of `history.redb` for as long as the body exists: public `GET
/signed/v1/blocks` and `GET /signed/v1/headers`, and the private
`replication/blocks/:height`. Nothing else is served for those heights:
state, receipts, block JSON and the feed still end at their floors.

A pinned day of bodies is small. On Kenobi (2 s blocks) it is about 28 MB.

A node older than this feature ignores `snapshot-pin`. If you roll the
snapshot node back to such a build with `--body-retention window`, the
pinned bodies expire.

## Publish

The five files are plain files. Serve them from anything that supports
range requests (nginx, an object store); the node does not serve them.

- Upload `snapshot.json` last, and remove it first when you retire a
  snapshot: it is what makes the directory a snapshot.
- Do not publish `prefill.bin` or `snapshot-pin`.
- Publish the pins next to the files (`--genesis-hash`, `--proposer`,
  `--chain-id`, `--source-id`, `--run-id`) and the URL of the node to sync
  from. That node must be the pinned one, or any node that keeps every body
  (`--body-retention archive`).

Downloading is the operator's job, with a tool that resumes:

```sh
for f in snapshot.json genesis.bin certificate.bin history.redb node.redb; do
  curl -fL -C - -O https://snapshots.example/kenobi/71494/$f
done
```

## Install

```sh
arkiv-proposer snapshot install --from ./71494 --run-dir /srv/full \
  --genesis-hash 0xGENESIS_HASH --proposer 0xPROPOSER_ADDRESS \
  --chain-id 61122026 --source-id SOURCE_UUID --run-id RUN_HEX32 \
  --peer https://node.example/rpc
```

The pins are the ones `full` takes. The command:

1. refuses a run directory that holds anything (a `lost+found` of a fresh
   volume is ignored);
2. checks `snapshot.json` against the pins, and `genesis.bin` and
   `certificate.bin` themselves as a follower checks its peer's answer: the
   genesis hashes to the pinned hash and the certificate of the genesis
   header is signed by the pinned proposer. It also compares the sizes of
   the database files. Nothing large is read before this passes;
3. with `--peer`, asks that node whether it can continue the snapshot (see
   below), and stops if not;
4. copies the two database files into the run directory, computing the
   SHA-256 in the same pass. A file that does not match is removed. It
   also writes `genesis.bin`, `certificate.bin` and `trust.json`, as a
   follower retains them;
5. opens the result exactly as `full` does: the stored genesis is the
   pinned one, the head record carries a certificate signed by the pinned
   proposer, the head manifest has the state root of that certificate, and
   the node identity (source and run ID) is the pinned one. The head must
   be the one `snapshot.json` describes;
6. prints the head.

If any step fails the run directory is emptied again.

| Flag | Meaning |
| --- | --- |
| `--move` | check the SHA-256 of each database file first, then rename it into the run directory instead of copying. Same file system only. The snapshot directory loses its database files. If the node then refuses the files (step 5) they are moved back, but opening has changed them: they no longer match their recorded hashes. |
| `--peer URL` | refuse a snapshot that node cannot continue |
| `--allow-http-peers true` | accept an `http://` peer beyond loopback, as on `full` |
| `--verify-state` | read every object of the head state and rebuild the garbage-collection counts (below) |

The switches `--move` and `--verify-state` (and `--no-pin`, `--keep-pin` of
`create`) take no value.

**The peer check.** The peer must be a producer or full node of the same
chain (identity and proposer). It must still hold the snapshot's own block,
which proves the new node's head to it, and every body after it: `height >=
bodyFloor` in its status. If it has reached the snapshot height it is also
asked for the header after the snapshot's, which fails when it has a
different block at that height. A peer that is still behind the snapshot is
accepted (`"behind": true` in the result). The refusal names the heights:

```text
https://node.example/rpc keeps block bodies from height 80211: it cannot continue a
snapshot at height 71494; install a newer snapshot, or sync from the node that pinned this one
arkiv-proposer: HistoryPruned
```

The installed directory has no pin and no `prefill.bin`. Start the node on
it as usual:

```sh
arkiv-proposer full --run-dir /srv/full --listen 0.0.0.0:9500 \
  --peer https://node.example/rpc \
  --genesis-hash 0xGENESIS_HASH --proposer 0xPROPOSER_ADDRESS \
  --chain-id 61122026 --source-id SOURCE_UUID --run-id RUN_HEX32
```

It syncs up to 64 blocks per 50 ms poll from the peer's public `GET
/signed/v1/blocks`, authenticates each header against the pinned proposer
and re-executes each block.

A state file that prunes cannot be opened with `--state-retention archive`.
Without the flag the node keeps the kind of file it finds.

### No prefill on a node that has a head

A `full` node whose `node.redb` already has a head and whose run directory
has no `prefill.bin` neither downloads nor audits the prefill. It logs
`{"event":"prefill_skipped",…}` and serves none: `GET /signed/v1/prefill`
and `GET /signed/v1/prefill/chunk` answer `PrefillBundleRequired` (HTTP
422). Such a node cannot be the peer of a follower that starts from genesis.

If `prefill.bin` is there (a node that was built from genesis), or a
`--bundle` is given, nothing changes: the file is audited at every start and
served. A node without a head starts from the prefill as before. Only `full`
does this; a producer on a chain with a prefill still needs its
`prefill.bin`.

### A node that cannot catch up

If the peer has already deleted the block after the node's head, no retry
helps. The node logs one line and exits with status 3 (a failed start is 1):

```json
{"event":"sync_gap","error":"HistoryPruned","head":"71494","peer":"https://node.example/rpc","message":"peer https://node.example/rpc no longer has block 71495: this node (head 71494) cannot catch up by retrying and must be reinstalled from a newer snapshot (arkiv-proposer snapshot install)"}
```

Do not let a supervisor restart it in a loop (systemd:
`RestartPreventExitStatus=3`). Remove the run directory and install a newer
snapshot, or point the node at a peer that still has the bodies. Any full
node that falls more than its peer's body window behind stops this way, not
only one installed from a snapshot. A light node keeps polling.

## What is and is not verified

Trust stays with the pins. The SHA-256 values protect the transfer; they do
not make a snapshot authentic.

| What | Checked by | When |
| --- | --- | --- |
| The files are the ones the snapshot node wrote | SHA-256 in `snapshot.json` | install |
| The chain is the pinned one | genesis hash, proposer, chain ID, run ID, source ID | install and every start |
| The head is a block of that chain | the head record's certificate, signed by the pinned proposer | install and every start |
| The state belongs to the head | the head manifest has the certificate's state root | install and every start |
| Each state object | stored under the hash of its bytes, linked to the state root, checked on every read | when read; all of the head state with `--verify-state` |
| States of the heights before the head that the window still serves | the same hashes | when read; not by `--verify-state` |
| Each header and body | proposer signature and input digest, by the node that reads or imports it | when read |
| Garbage-collection counts | rebuilt from the objects | only with `--verify-state` |
| Feed and request-inclusion records of the heights in the record window (256 blocks) | nothing | never |

A tampered snapshot can therefore make a node fail a read or stop. It
cannot make it answer a state read or a proof wrongly, serve a block the
proposer did not sign, or accept one. Two things are bound to nothing:

- the garbage-collection counts. A wrong count or a missing object shows
  only later, when the node stops on it. `--verify-state` rebuilds the
  counts and finds a missing object of the head state;
- the feed metadata and request statuses of the last 256 heights before
  the snapshot. From an untrusted snapshot they can be wrong until those
  heights leave the record window, 256 blocks after the install.

**`--verify-state`** is for a snapshot from a source you do not trust. After
the files are opened it:

1. reads every object the head state reaches and checks it, with the walk
   [genesis adoption](genesis-adoption.md) uses: content hash, trie
   structure, entry counts, namespace association, row admission, object
   and host value bounds;
2. on a state file that collects garbage, rebuilds the counts from the
   objects (`state gc-enable` does the same). Objects the head does not
   reach are scheduled for deletion. An archive state file has no counts.

It does not read the states before the head, the records or the bodies (see
the table). It reads the whole state once and scans every object once more
for the counts, and writes about 42 bytes per object and per reference to
`RUN/gc-seed` while it counts. On Kenobi's file (106 GiB, 138.6 M objects,
6 cores with 12 threads, NVMe) the walk took 30 minutes and the count another 30 minutes,
so an install with `--verify-state` took 65 minutes against about 2 minutes
without
([measurements](proposals/2026-11-24-snapshot-sync.md#kenobi-with-the-snapshot-commands-2026-11-24)).

## The timing limit of a stopped snapshot node

While the snapshot node is stopped for the copy, its own peer keeps
producing and pruning. With `--body-retention window` on that peer, the
snapshot node must be back within the peer's body window, 256 blocks: 8.5
minutes at 2 s blocks. If it is not, it stops with `sync_gap` like any other
node that fell behind, and has to be installed again from someone else's
snapshot.

- The copy is sequential: read, SHA-256 and write run side by side. Measure
  it once on your disks. Kenobi's `node.redb` is 106 GiB: on NVMe disks
  `snapshot create` took 119 s and the follower was stopped for 120 s, and
  270 s when another large copy ran on the same disks
  ([measurements](proposals/2026-11-24-snapshot-sync.md#kenobi-with-the-snapshot-commands-2026-11-24)).
- A peer that keeps every body (`--body-retention archive`, Kenobi's
  producer today) has no window, and neither has a pinned peer for the
  heights from its pin on.
- Use a release build. A debug build of redb opens a 106 GiB file in about
  30 minutes.

An online snapshot, which removes the stop, is future work
([proposal](proposals/2026-11-24-snapshot-sync.md#plan)).

## Daily routine

On the snapshot follower, once a day:

```sh
set -e
RUN=/srv/follower
OUT=/srv/snapshots/$(date -u +%F)

systemctl stop arkiv-follower            # or: docker stop
arkiv-proposer snapshot create --run-dir "$RUN" --out "$OUT"
systemctl start arkiv-follower           # back inside the peer's body window

# Publish: snapshot.json last.
rsync -a --exclude snapshot.json "$OUT"/ files.example:/var/www/snapshots/latest.new/
rsync -a "$OUT"/snapshot.json files.example:/var/www/snapshots/latest.new/
# … then switch `latest` to the new directory and delete the old one.
```

`snapshot create` moved the pin to the new snapshot, and the restart applied
it. From then on the bodies of the previous snapshot expire. If the previous
snapshot stays published for a while (downloads in flight, a slow mirror),
create with `--keep-pin` and move the pin yourself when you retire it:

```sh
arkiv-proposer snapshot pin --run-dir "$RUN" --height NEW_HEIGHT   # then restart
```

Check `bodyPin` and `bodyFloor` in `GET /signed/v1/status` of the snapshot
node after the restart: `bodyFloor` must not be above the height of any
snapshot you publish.

## Compatibility

Nothing stored changed. `node.redb` and `history.redb` have the layout of
0.1.0-alpha.8, the pin is a side file, and blocks validate the same. A
snapshot of a 0.1.0-alpha.8 node's files can be created and installed by
0.1.0-alpha.9, the first release with the snapshot commands. 0.1.0-alpha.8 also opens an installed run directory, but it
ignores the pin and, on a chain with a prefill, still downloads and audits
the prefill before it starts.

`scripts/snapshot-smoke.py` runs the whole procedure with the real binary
on a local chain: a `window` producer, a follower that is stopped,
snapshotted, pinned and restarted, more than 256 blocks, a node installed
with `--peer` and `--verify-state` that catches up from the follower, and a
node pointed at the producer that exits.
