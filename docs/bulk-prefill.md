# Bulk construction for streaming genesis

Version-two prefill must construct the final authenticated state without running
one normal entity write per input row. The incremental implementation took
21m 10.6s to start a producer from 445,583 entities (100,000 contracts), writing
168.5 GB for 1.86 GB of canonical input. Those process-wide writes include
compaction and final installation; they are not a measurement of index writes
alone. Increasing replay batch sizes was abandoned before experiments ran.

## Construction contract

The input format, signed genesis, record IDs, state root, sponsor debit, rent and
storage units must remain unchanged. Normal live transaction execution and the
small version-one builder retain their existing behavior. Incremental replay is
an equivalence oracle for tests, not a fallback for version-two import.

Construction has four parts:

1. Stream and validate canonical frames, assigning record IDs in input order.
   Validate entity attributes, expiry and engine admission limits, calculate
   storage units and rent, and retain only the current creator's nonce count.
2. Spill bounded sorted runs for key bindings, term/record memberships and host
   entries. Merge with bounded fan-in. Reduce expiry counts with checked sums
   and enforce the per-height expiry capacity. Reject duplicate entity keys.
3. Build the canonical Merkle Patricia maps from sorted entries. Complete each
   posting list before inserting its root into the term map. Keep only bounded
   tree frontiers and object-write buffers, including for very common terms.
4. Flush final objects and atomically publish the completed private genesis.
   Check the derived commitments before installing the node's height-zero state.

Internal sort order must use the existing trie key encodings, not incidental
Rust ordering. Catalog and host state matter as much as query indexes: creator
nonces, expiry counters, namespace/record allocators and sponsor accounting all
contribute to the final state.

## Resource and recovery requirements

Memory usage is bounded by configured sorting, merging and object-write buffers,
trie frontiers, and storage caches. Whole-dataset entity arrays, posting lists,
creator maps and expiry maps are not allowed. Temporary disk usage and free-space
reserve remain operational constraints.

Incomplete bulk scratch can be rebuilt from the canonical input. This is safe
restart behavior, not the incremental importer's committed-prefix resumption.
Completed scratch may be reopened only when bound to the same input and genesis;
partial state must never become a live node. Existing incremental scratch must
not be mistaken for completed bulk state.

Final installation may still copy reachable objects into the headless node
store in bounded batches. Removing that copy requires separate treatment of
node identity and atomic metadata publication; bulk construction alone does not
claim to eliminate it. Bulk construction itself now runs once per launch:
`stream-build --state-out` hands its completed store to `producer
--prebuilt-state` (see [Build once](streaming-genesis-v2.md#build-once-prebuilt-state)).

## Validation and measurements

The workspace suite passed 1,201 tests, with four intentionally ignored and no
failures. Workspace Clippy with warnings denied and formatting checks passed.
Coverage includes exact trie root/object-set equality and proofs, multiple
external-merge levels, shared postings, row IDs, multiple creators and expiry
heights, supported value types, admission and accounting failures, truncated or
modified sort runs, interrupted object writes, incomplete scratch rebuilding,
completion-receipt corruption and completed-state reopening.

Release acceptance on the 263-entity / 16-contract fixture passed bundle creation,
producer/full/light head agreement, full-node stream download, account/entity
proofs, a live signed write and restart. The light node retained no prefill. The
newly authored bundle report exactly matched the historical incremental bundle,
including genesis/state roots, storage units and rent. This isolated acceptance
run took 2.013 seconds and peaked at 54.1 MiB cgroup memory with no swap; it is a
correctness check, not a large-import speed estimate.

A fresh producer imported the original 100,000-contract / 445,583-entity bundle
and reached the expected height-zero genesis and state roots in **612.267 seconds
(10m 12.3s)**. The run used the same canonical bundle as the historical comparison,
under a 32 GiB cgroup cap, no swap and an 80 GiB disk reserve. All memory-event
counters stayed zero. The diagnostic producer was stopped after readiness.

| Measurement | Previous incremental run | Bulk run |
| --- | ---: | ---: |
| Producer ready | 1,270.554 s | 612.267 s |
| State construction, including compaction where applicable | 975.555 s | 342.884 s |
| Final installation interval | 238.226 s | 212.553 s |
| Standalone stream audits | 53.036 s | 53.818 s |
| Process peak RSS | 943.0 MiB | 643.3 MiB |
| Cgroup peak including file cache | 14.30 GiB | 13.64 GiB |
| Kernel process `write_bytes` counter | 168,471,179,264 | 67,520,536,576 |
| Kernel `cancelled_write_bytes` counter | 0 | 2,841,800,704 |

Bulk construction comprised 104.583 seconds of validation and sorted-run
production, followed by 238.301 seconds merging and building indexes. The index
phase processed 18,973,546 term memberships. Final installation is measured from
bulk completion to the last audit; it includes surrounding metadata/cleanup
work and is not an isolated inner-copy timer. CPU time was 460.09 seconds user
and 144.07 seconds system.

Observed startup time fell 51.8%, construction time about 64.9%, and the kernel
write counter 59.9%. These are single-run historical comparisons, not repeated
controlled A/B measurements. The write counters are kernel task I/O accounting,
not device-level measurements; cancelled writes include deleted dirty scratch
pages. The final copy still costs roughly three and a half minutes, and write
amplification remains substantial. The 100k run checked producer construction
and readiness, not a repeat of the three-role acceptance at that scale.

The verified commitments remain:

- Genesis: `0x612bab0b9ee7e20f1a0fc99a4d6bda5db26291b69e0218eef1ac57df67d747ae`
- State root: `0xf91cbca6523233b6214baa3da89bab61144c7c817126122bb433d3fe94e7a458`
- Storage units: `3324006410`
- Burned rent: `9684277486347000000000` wei

Evidence is retained in the parent workspace under `operations/runs/bulk-prefill`:
`workspace-tests.log`, `workspace-clippy.log`, `smoke/acceptance.json`,
`measurement-build.json`, `comparison.json`, and the `producer-100k` result,
events and resource samples. The original input and acceptance are described in
[the 100k validation report](streaming-prefill-100k-validation.md).

To reproduce the producer measurement on this workspace:

```sh
sudo systemd-run --unit=arkiv-bulk-prefill-measure --collect --wait --pipe \
  -p User=ubuntu -p MemoryMax=32G -p MemorySwapMax=0 \
  --setenv=ARKIV_PREFILL_RESERVE_GIB=80 \
  /usr/bin/python3 /home/ubuntu/arkiv-network/operations/runs/bulk-prefill/measure-producer.py \
  --bin /home/ubuntu/arkiv-network/arkiv-state-db-experiment/target/release/arkiv-proposer \
  --bundle /home/ubuntu/arkiv-network/operations/runs/experiment-prefill-100k/bundle \
  --work /home/ubuntu/arkiv-network/operations/runs/bulk-prefill/producer-100k-repeat \
  --key /home/ubuntu/arkiv-network/operations/runs/experiment-prefill-100k/acceptance/fixture.key
```

Use a fresh work directory each time. The fixture key is only for isolated local
tests. No production deployment is part of this measurement.
