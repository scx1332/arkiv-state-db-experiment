# Adopting the staged genesis store

`arkiv-proposer producer --genesis-install adopt` installs a streaming (v2)
genesis without copying it. The default, `--genesis-install copy`, is unchanged.

```sh
arkiv-proposer producer --bundle /data/bundle --run-dir /data/producer \
  --key-file /private/proposer.key --listen 127.0.0.1:9500 --genesis-install adopt
python3 scripts/run-full-prefill.py ... --genesis-install adopt
```

The option is accepted only by `producer`. It is ignored once the node is
initialized, so a restart can keep the same command line. A producer whose
genesis has no streaming prefill refuses it.

## What changes

A fresh producer first builds, or reuses, the private bulk store
`prefill-work/bulk-v1/complete/state.redb`. That store is engine-only: it has
one height-zero publication under an import genesis. The copy installation
(`initialize_simulator_from_staged`) walks every object reachable from the
staged state, checks it and writes it into a separate headless `node.redb`.
Then it commits the node records: simulator profile, node genesis, simulator
identity, height-zero manifest and block index, and the signed genesis block
record. On the October 19 full run the copy took about 12 h of the producer's
20 h 28 min and wrote a second 131 GB database next to the staged one.

Adoption keeps the staged store:

1. **Eligibility.** The source must be a ready engine-only engine whose only
   publication is height zero, with the bundle's state header, limits and
   pricing. Its head must be that manifest. The store must hold exactly the
   records `initialize_bulk` writes: the engine profile and import genesis, one
   manifest and its index entry. Later snapshots, import checkpoints, retention
   floors or any simulator record make it ineligible, and so does an
   incremental import or its compaction. Incremental imports keep older states
   and their objects. The store checks the exact set, not a count.
2. **Verification** (`DurableEngine::adopt_staged_simulator`). Every object
   reachable from the staged state is read and checked exactly as the copy
   checks it, but not written. The checks are the content hash, the node's
   object cap and the copy's batch cap, trie decoding, every map's entry count,
   namespace association and allocator, row admission under the genesis limits,
   and host value bounds. The leaf checks are the copy's own functions. The
   walk runs in up to eight workers (`genesis_adopt::verify_threads`). Whole
   maps are parts, and maps with more than 65,536 entries are cut into up to
   256 key ranges at their first two trie branch levels. Every stored node lies
   on the path to some leaf, and each leaf belongs to exactly one part. So every
   reachable object is still read and decoded at least once, and every entry is
   counted exactly once against its map's length. Large postings are scheduled
   once per root, as the copy's posting cache skips repeated postings.
3. **Relabelling** (`VersionedStore::relabel`, implemented by `RedbStore` and
   the memory store). One durable redb transaction checks the head and the exact
   record set, deletes those records and the head, and applies the node records
   as a first commit, producing the same records the copy commits. Objects are
   kept. A refusal leaves the store unchanged.
4. **Move.** `Host::check_adopted` reopens the relabelled file and checks its
   simulator identity, engine genesis, head manifest and signed genesis record
   for this bundle and `--source-id`, reading no trie objects. Then the file is
   renamed over the fresh headless `node.redb` and both directories are synced.
   Before relabelling, a rename probe refuses a staged store that could not be
   moved, for example across mounts.
5. The node opens normally (`Host::open`). The retained stream is re-audited and
   `prefill-work` is removed, as after a copy.

The producer logs `{"event":"prefill_genesis_adopt","stage":"relabelled",
"verifySeconds":…}` and `{"event":"prefill_genesis_adopt","stage":"adopted",
"resumed":…,"elapsedSeconds":…}` to stderr.

## Integrity

| Guarantee | Copy | Adopt |
| --- | --- | --- |
| State root equals the bundle's | staged header check, then copy | the same staged header check; the relabelled manifest carries that header |
| Stream commitment | three stream audits | the same three audits, including the re-audit after installation |
| Reachable objects present, hash-correct, decodable | copy walk | the same checks in the verification walk |
| Entry counts, namespace association, row admission, host bounds, object and batch caps | copy walk | the same check functions |
| No unrelated records or states | fresh destination | exact retired-record check in the relabelling |
| Node records | one commit into the destination | the same records in the relabelling commit; `check_adopted` before the move |

Garbage. The bulk builder emits only the nodes of the final tries. On the
100k-contract sample every stored object was reachable: 1 catalog, 96 host and
5,083,760 namespace objects, the same counts as the reachability walk. So
adoption keeps no unreachable objects, and the copy removed none. The staged
file had 24.7% unallocated pages, left by the bulk build's transactions. The
copied `node.redb` had the same size (6,445,076,480 bytes against 6,444,814,336),
so the copy compacted nothing either.

The database id of the staged store is kept. It identifies a live database for
caches and handles and never enters a hash. After the move no other database
has it.

## Crash safety

The relabelling is atomic and durable before the rename. The rename replaces
the headless `node.redb` atomically and is followed by directory syncs. The
producer holds its open handle on the headless `node.redb` until the rename, so
no other producer can initialize that name meanwhile. A cut leaves one of these
states, and the next start with `--genesis-install adopt` resumes it:

| State after the cut | Next start |
| --- | --- |
| staged store unchanged (any time before the relabelling commit, including a failed or refused commit) | reuse it without rebuilding, verify, relabel, move |
| staged store relabelled, `node.redb` still headless | `check_adopted`, move, open (`"resumed":true`) |
| `node.redb` is the adopted database | normal open; small scratch leftovers are harmless |

`node.redb` is never a partially relabelled store. An indeterminate relabelling
commit is resolved the same way, by the profile the reopened file has. Adoption
consumes the staged store, so the copy installation refuses a relabelled store
("unknown engine profile/version") instead of rebuilding or serving it. A store
that fails verification is not relabelled: every retry fails the same way until
the import scratch is removed. The redb crash-point test
(`crates/arkiv-db-redb/tests/relabel.rs`) checks every mutating backend call of
the relabelling commit, under durable-only, all-unsynced and torn crash images.
Each image passes redb's integrity check and holds exactly the old or exactly the
new publication, never a mix.

## With build-once (`--prebuilt-state`) and permanent prefill

Build-once (PR #4) moves the `stream-build --state-out` directory into
`prefill-work/bulk-v1` before the existing staged-state path runs. Adoption
starts from that same path, so the two compose without further changes. The
prebuilt directory is already consumed by that move. In the cross-filesystem
copy case the original stays untouched and only the run directory's copy is
relabelled. Adoption also needs the staged store on the node directory's
filesystem, which holds for `prefill-work` inside the run directory. Permanent
prefill (PR #3) changes what the staged state contains, not how it is stored.
Adoption selects only `PrefillSource::Streaming` and compares staged and signed
state headers, so it does not depend on the launch version.

Both combinations were checked on local merges of the branches as of October 21,
2026. On the 100k-contract bundle, `stream-build --state-out` followed by
`producer --prebuilt-state --genesis-install adopt` moved the store, did not
rebuild it, adopted it and served the bundle's head. With #3, a version-4
permanent genesis adopted in place behaved like the copied one through
permanent-entity blocks and expiries.

## Measurements

The 100k-contract bundle has 445,600 entities and a 1.85 GB stream, and its staged
store is 6.0 GiB. The machine has NVMe RAID0 and 12 cores. The staged file's page
cache was dropped (`fsync` plus `POSIX_FADV_DONTNEED`) before each install. The
times cover only the install step:

| Install | Time | Bytes written | Second database |
| --- | --- | --- | --- |
| copy | 263.2 s | about 29 GB of file-system output | 6.0 GiB |
| adopt, 1 verification worker | 151.5 s | 2.6 MB | none |
| adopt, 8 verification workers | 25.4 s | 2.6 MB | none |

End to end, the producer binary on the same bundle builds the staged store and
then installs it, so the cache is warm:

| Producer | Ready | Install step | Peak RSS | Height-zero head and query proofs |
| --- | --- | --- | --- | --- |
| `--genesis-install copy` | 608.5 s | 220.5 s | 0.66 GiB | reference |
| `--genesis-install adopt` | 410.6 s | 25.5 s (verify and relabel 15.1 s) | 0.91 GiB | identical |

The adopted node restarted in 38.7 s, the stream re-audit, and produced block 1.
A producer killed with SIGKILL during verification left its staged store
untouched. The next start reused it without rebuilding and adopted it.

The copy costs 0.59 ms per entity here, but 2.8 ms per entity on the full run,
where neither database fits in the page cache. Adoption is bound by random reads
of one database instead of reads and writes of two. These sample figures do not
establish a full-dataset time.

## Limitations

- Followers (`full`) still copy. Their staged store is built the same way, so
  adoption would apply. That is left for a follow-up.
- The verification walk still reads every reachable object. A cheaper proof
  of completeness would need a build-time object count or set digest in the
  bulk receipt, which is a format change.
- The walk reports no progress, only the `relabelled` event at its end.
- The adopted file keeps the bulk build's free pages. redb reuses them for later
  blocks, and `Database::compact` could reclaim them offline.
