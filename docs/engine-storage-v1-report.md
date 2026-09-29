# Engine storage v1 measured report

Evidence for [engine storage v1](engine-storage-v1.md). The tables below are the output of the checked-in [measurement runner](../crates/arkiv-db-redb/examples/engine_storage_experiment.rs); the integration tests add failure evidence after the measurements. These are engine publications, not persisted node blocks.

- Date: 2026-09-24 10:41 UTC
- Command: `cargo run --release --offline -p arkiv-db-redb --example engine_storage_experiment`
- rustc 1.96.0 (ac68faa20 2026-05-25)
- CPU: 13th Gen Intel(R) Core(TM) i9-13900; logical CPUs available: 32
- Kernel: 7.0.0-31-generic
- Filesystem: ext4; path: /home/ubuntu/arkiv-network/arkiv-db-pure-astra/crates/arkiv-db-redb/../../target/bench
- redb 4.3.0; quick repair on; redb page cache 8 MiB. Object-cache options are stated per experiment.
- This measures engine snapshots, not node block/body/signing restart. No pruning or compaction.

## Retained history and resident state

Each commit changes one of 64 raw keys and one host key; 16 typed rows remain fixed. Both caches start cold after each checkpoint reopen. Candidate/branch owners are dropped each commit.

| sequence | object cache bytes | cache container bytes | cache entries | resident manifests | last staged objects | last payload bytes | commit clone bytes | RSS KiB | process HWM KiB | file bytes | elapsed s |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 100 | 65513 | 50600 | 502 | 1 | 8 | 1934 | 1934 | 5164 | 5164 | 606208 | 0.037 |
| 1000 | 65369 | 40744 | 461 | 1 | 8 | 1934 | 1934 | 7752 | 7752 | 4820992 | 0.325 |
| 10000 | 65369 | 40744 | 461 | 1 | 8 | 1934 | 1934 | 14156 | 14156 | 38539264 | 3.557 |

Peak prepared payload: 10380 bytes; batch-vector capacity: 6048 bytes; dedup entry payload: 4704 bytes; commit clone payload: 10380 bytes. The dedup entry figure excludes bounded B-tree allocator/node overhead. Preparation retains one batch, never a batch per historical snapshot.

At all three quiescent checkpoints, retained decoded trie nodes/bytes are **0/0 by ownership invariant**, not an allocator census: the head contains stored roots, the cache owns encoded bytes, no caller branch/candidate survives, and the runner asserts the head stages zero objects after every publication. Direct weak-reference tests separately check dirty graph/value release and that read-decoded nodes do not become owned by stored handles. Active calls may allocate decoded nodes; their peak is not measured by this invariant.

The following redb layout diagnostics measure disk allocation, including retained history. They run after each checkpoint's counters/RSS and before the cold reopen. They do not attribute resident metadata heap bytes.

| sequence | tables | directory height | page bytes | allocated pages | allocated disk bytes | historical object entries | object key/value bytes |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 100 | 9 | 1 | 4096 | 82 | 335872 | 859 | 129577 |
| 1000 | 9 | 1 | 4096 | 680 | 2785280 | 8059 | 1379677 |
| 10000 | 9 | 1 | 4096 | 6652 | 27246592 | 80059 | 13880677 |

## Cold application open

The OS page cache is warm; application and redb caches are new. `unclean` disables the old handle's backend before drop, preventing clean-close writes; actual child-process SIGKILL cases are tested separately. Backend open figures include redb allocator/repair metadata and are separate from the application's point reads.

| sequence | close | app records | app head reads | app trie objects | app μs | redb open μs | redb reads | redb read bytes | full repair |
| ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 100 | clean | 7 | 1 | 0 | 33 | 60 | 8 | 24905 | false |
| 100 | unclean | 7 | 1 | 0 | 27 | 74 | 8 | 24905 | false |
| 1000 | clean | 7 | 1 | 0 | 36 | 63 | 8 | 24905 | false |
| 1000 | unclean | 7 | 1 | 0 | 27 | 71 | 8 | 24905 | false |
| 10000 | clean | 7 | 1 | 0 | 48 | 83 | 8 | 24905 | false |
| 10000 | unclean | 7 | 1 | 0 | 31 | 79 | 8 | 24905 | false |

## Historical proofs under bounded caches

Snapshot 1 reopens after 10,000 publications. Proof bytes equal the memory engine and verify independently. Cold and warm refer to the engine's encoded-object cache.

| cache bytes | cold object reads | warm object reads | cold backend bytes | warm backend bytes | proof bytes | retained payload | entries |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 0 | 28 | 28 | 126976 | 0 | 3114 | 0 | 0 |
| 65536 | 16 | 0 | 126976 | 0 | 3114 | 2532 | 16 |
| 8388608 | 16 | 0 | 126976 | 0 | 3114 | 2532 | 16 |

Logical path visits are the traced `ProofNode` ledger count, including inline nodes visited while building witness paths. They exclude preliminary value lookups and posting iteration, whose aggregate trie visits are not exposed by the proof API. Witness nodes are emitted encodings; hits plus misses count stored-object loader calls, not all logical visits. Charges, path visits, witnesses and posting count are asserted equal across all six runs. Generation and independent verification are timed separately; single-call microsecond results are observations, not latency benchmarks.

| cache bytes | pass | cache hits | cache misses | logical proof-path visits | witness nodes | posting IDs | proof charge | generate μs | verify μs |
| ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 0 | cold | 0 | 28 | 15 | 15 | 1 | 3251 | 140 | 15 |
| 0 | warm | 0 | 28 | 15 | 15 | 1 | 3251 | 94 | 13 |
| 65536 | cold | 12 | 16 | 15 | 15 | 1 | 3251 | 100 | 15 |
| 65536 | warm | 28 | 0 | 15 | 15 | 1 | 3251 | 36 | 13 |
| 8388608 | cold | 12 | 16 | 15 | 15 | 1 | 3251 | 95 | 14 |
| 8388608 | warm | 28 | 0 | 15 | 15 | 1 | 3251 | 35 | 13 |

## Historical handle ownership and memory

1,000 distinct held historical handles perform zero trie-object reads during creation. Each `Snapshot` occupies 8 inline bytes; the handle vector capacity occupies 8000 bytes. Each additionally owns a private fixed-size root/header/context allocation; its heap layout and allocator overhead are not exposed by the public API.

| phase | process RSS KiB |
| --- | ---: |
| before handles | 10568 |
| after creation, before reads | 10568 |
| after reading through all handles | 12440 |
| after dropping handles | 12440 |

Creation changes process RSS by 0.0 bytes per handle on average. This includes allocator/backend effects and reuse, so it is not an isolated per-handle heap cost. Linux-reported process `VmHWM` changes 12140 → 12440 KiB; it is process-wide and is **not** peak live decode memory. Cache after reads: 65507 bytes/569 entries; engine resident manifests: 1. Handles retain roots/header/context state; allocator RSS need not fall on drop.

## Namespace isolation

Namespace 1 remains at 128 rows; namespace 2 grows from 1,000 to 10,000 rows. Each probe runs after closing/reopening redb and with the object cache disabled.

| snapshot | target object reads | foreign object reads | catalog reads | backend bytes | proof charge | proof bytes |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 11 | 108 | 0 | 6 | 73728 | 18156 | 17226 |
| 103 | 108 | 0 | 6 | 77824 | 18156 | 17226 |

## Incremental fixed-size updates as live state grows

A single field update in namespace 2; namespace 1 is untouched. Empty successors at both sizes wrote zero trie objects. Physical redb write amplification is measured separately.

| growing rows | staged objects | batch payload bytes | preflight visits | staging visits | staging object reads | foreign objects written | namespace object bytes written | backend bytes written |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1000 | 8 | 2517 | 8 | 8 | 0 | 0 | 1370 | 90752 |
| 10000 | 9 | 3049 | 9 | 9 | 0 | 0 | 1870 | 119424 |

The two records are the manifest and manifest-digest index. Their bytes include keys and values, measured by reading those exact bounded records after commit. All staged object IDs are asserted newly written, so encoded object bytes + 32-byte IDs + record bytes must equal the prepared batch payload. Amplification is backend write bytes / this logical payload, following `CommitBatch::logical_bytes`; the separate 40-byte head value is excluded from that denominator. The immutable metadata is not rewritten in a successor.

| growing rows | all encoded object bytes | object ID bytes | record key/value bytes | head bytes | logical payload bytes | backend write calls | backend write bytes | syncs | amplification × |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1000 | 1716 | 256 | 545 | 40 | 2517 | 24 | 90752 | 2 | 36.06 |
| 10000 | 2216 | 288 | 545 | 40 | 3049 | 31 | 119424 | 2 | 39.17 |

## Interpretation and unavailable attribution

All structural/counter assertions passed. Application cold open performed seven record reads, one head read and zero trie-object reads at all history sizes. The engine kept one resident manifest and no pending payload after publication. Read handles retained stored roots; direct weak-reference release tests live in the engine/trie suites.

Isolated peak live decode bytes/nodes, private per-handle heap overhead, and resident redb metadata bytes are **not measured**: the public engine/redb APIs expose no allocation ownership counters. A scoped global allocator would still combine decoder, traversal, result, cache and backend allocations; implementing one here would also require unsafe code forbidden by the workspace. An exact attribution needs dedicated allocation hooks or a separately configured profiler. The sampled `/proc/self/status` RSS and `VmHWM` fields are kernel-reported process values, not allocation counters or a scoped active-call allocation peak. The redb page-cache setting is a configured bound, not a measured resident allocation. Disk layout numbers likewise cannot be used as resident metadata bytes.

RSS includes redb allocator/table metadata, Rust allocator retention and active decode/response buffers as well as the bounded cache. These observations do not establish a universal process-memory bound or cold-device latency. OS page-cache eviction was not performed. Other namespaces caused no namespace-local reads in the fixed target proof, while shared catalog/backend work is separately reported.

## Actual engine crash and recovery evidence

Run:

```sh
cargo test --offline -p arkiv-db-redb --test engine_crashes --test engine_process -- --nocapture
```

[engine_crashes.rs](../crates/arkiv-db-redb/tests/engine_crashes.rs) commits real catalog, namespace, raw-record, key, row, term, posting and host objects. Each reopened image must have exactly the memory reference's complete parent or candidate state. Checks include old snapshots, all typed rows, index consistency, every new raw value, host values, byte-identical equality proofs, and independent proof verification. No object batch is mocked.

| Scenario | Evidence |
| --- | --- |
| Small commit, every mutating backend call | 42 calls, 43 cuts, 172 images: 167 parent, 5 candidate; 2 syncs, no growth |
| Spilling commit with new namespace and 192 × 9,000-byte raw values | 813 calls; 61 selected cuts, 244 images: 239 parent, 5 candidate; 2 growth calls, 4 syncs |
| I/O failure at every small-commit write/sync call | 42 failures: 41 exact prepared retries, 1 observed applied commit kept uncertain; no input reexecution |
| Failed final sync with lost unsynced writeback but visible page-cache bytes | `Indeterminate` → `ObservedCommittedDurabilityUnknown`; public head remains parent and writes stay fenced; quick-repair recovery followed by a durable-only power cut reopens the full candidate in this model |
| Disk full while growing/staging, versus inside commit's sync | `NotCommitted(Full)` versus `Indeterminate(Full)` preserved; clear quota/fault, recover exact parent, retry the same preparation and reopen complete state |
| Actual child-process SIGKILL after prepare, before commit | Restart exposes the complete parent |
| Actual child-process SIGKILL after real redb `Committed`, before facade publication | Restart exposes the complete candidate, including namespace/index/host state; no replay |

Crash images cover durable-only, all-unsynced, torn sectors (seed 17), and torn bytes (seed 29). The large case samples every sixteenth write cut and brackets every growth/sync boundary; it does **not** claim exhaustive testing of all 813 boundaries. All tested reopens avoided redb full repair. Every acknowledged result survived the durable-only image; definite noncommits never appeared applied. The child-process tests use a forwarding wrapper solely to pause delivery of the real adapter's success to the engine, then kill the child from its parent. They do not substitute a fake commit result.

These are a finite fault model and process-kill tests on the stated platform, not evidence for every filesystem, device or failed-fsync behavior. The engine intentionally does not acknowledge durability based on generic readback after an uncertain commit. No power cut of the physical host was performed.

## Resource and scale limits

The history workload has 16 typed rows, 64 changing raw keys and one host key, with up to 10,000 retained publications. The live-state workload compares 1,000 and 10,000 rows in one growing namespace against a fixed 128-row target. The large crash candidate is about 1.7 MiB of raw values. These are bounded acceptance workloads, not production-capacity estimates.

The object cache has a separate 16,384-entry ceiling. Preparation uses the default 32 MiB payload, 100,000-object and 1,000,000-visit limits, with a persisted 1 MiB maximum encoded object. The report includes actual cache container capacity and preparation batch/dedup payloads; B-tree allocation overhead remains bounded by the object cap but is not claimed to be exactly measured. The consuming adapter commit temporarily clones one bounded batch, and that extra payload is reported.

Preflight bounds every reachable new node and traversal work before any
nested root hashing. A node caches its fixed-size `RlpNode` reference
(`ArrayVec<u8, 33>`), not its full encoding. Hashing scratch consists of one
bounded leaf-value encoding and its bounded node encoding, plus traversal
frames bounded by fixed key widths and map nesting. This transient memory
is separate from the staged 32 MiB payload and its commit clone. Hashing
may still visit all new nodes up to the visit cap before staging rejects
the aggregate byte cap; the preparation byte limit is not a claim that
all earlier hashing work was avoided.

The 10,000-publication run asserts one resident manifest, zero pending payload after publication, bounded cache capacity/count, and no new in-memory objects reachable through the published head. The separate engine/trie ownership tests use weak references to distinguish actual release from allocator RSS retention. Read-only historical handles retain their own small headers/root handles and shared context, so their memory scales with caller-held handles rather than the size of the states they name. Caller-held speculative branches, checkpoints, candidate clones and concurrent query/proof output remain caller-controlled allocations.

Specifically, `prepared_and_committed_facade_release_dirty_values_after_callers_drop_clones`
in [durable/mod.rs](../crates/arkiv-db/src/durable/mod.rs) holds a weak
reference to dirty value data: a retained caller candidate clone keeps it
alive, dropping that clone after preparation releases it, and publication
plus reads through 128 handles never revives it.
`stored_handles_release_dirty_nodes_and_do_not_retain_read_nodes` in
[map.rs](../crates/arkiv-db-trie/src/map.rs) tracks more than 512 actual
dirty nodes, proves the entire graph drops, then tracks decoded nodes
through a loader and proves reads leave none retained by stored handles.
These establish ownership release; they do not measure transient decoder
allocation peaks or the exact heap size of a snapshot handle.

RSS grows in this run and is not presented as constant. The redb page cache, allocator/table metadata and process allocator contribute to it. Likewise, target namespace logical work remains unchanged while backend bytes rise from shared catalog/table-directory effects; this is not a constant physical I/O or latency claim. Disk history grows without pruning or compaction.

## Shared differential history

[engine_history.rs](../crates/arkiv-db-redb/tests/engine_history.rs) invokes the
[same seeded runner](../crates/arkiv-db/tests/support/durable_history.rs)
as the durable-memory tests. It compares ordinary `Engine` with actual
redb-backed `DurableEngine` for seeds 1 and 27, each with 65 publications.
The workload includes raw and typed records, every cell value type,
rollback/repeated updates, ownership transfer, pricing changes, host state
and empty publications. Receipts, spent units and roots agree throughout.

The real adapter and engine are dropped before reopening the file.
Snapshots 1, 2, 11, 30, 48 and 65 are then compared against the ordinary
memory engine with 0, 64 KiB and 8 MiB object caches. Each is checked twice
to exercise cold and warm access: descriptors, raw scans, typed rows,
queries/counts, pagination, namespace/state roots and host data agree;
supported historical proof bytes/receipts agree and verify independently.
Snapshot opening loads no trie objects. This is the real-disk third arm of
the same history previously exercised with `MemoryVersionedStore`.

## Validation commands

These commands passed for this evidence slice:

```sh
cargo test --offline -p arkiv-db-redb --test engine_crashes --test engine_process --test engine_history -- --nocapture
cargo clippy --offline -p arkiv-db-redb --test engine_crashes --test engine_process --test engine_history --example engine_storage_experiment -- -D warnings
cargo +1.90.0 check --offline -p arkiv-db-redb --test engine_crashes --test engine_process --test engine_history --example engine_storage_experiment
cargo run --release --offline -p arkiv-db-redb --example engine_storage_experiment
```

The integration binaries contain eight passing tests including their
child-process entry point; the helper does no standalone work outside a
spawned child. These new measurements do not replace the N2 adapter suite.

The measurement expansion reran the release example, example-only Clippy
with warnings denied, and the example's Rust 1.90.0 MSRV check. All passed.
It did not rerun unchanged crash tests; the preceding eight-test results
remain the evidence for those scenarios. Allocation attribution marked
unavailable above remains outside these measurements, rather than being
estimated from RSS or disk layout.

Final integration validation, run by the integration lead after the
cache-disabled edge-case fix, passed with **807 tests, 0 failures, and 3
existing heavy tests ignored**. The total was checked against the local
`target/engine-storage-workspace-tests-final.log`. The exact commands were:

```sh
cargo test --workspace --offline
cargo clippy --offline --all-targets -p arkiv-db -p arkiv-db-storage -p arkiv-db-memory -p arkiv-db-redb -p arkiv-db-trie -p arkiv-node -p reth-trie -- -D warnings
cargo +1.90.0 check --workspace --all-targets --offline
cargo fmt -p arkiv-db -p arkiv-db-storage -p arkiv-db-memory -p arkiv-db-redb -p arkiv-db-trie -p arkiv-node -p reth-trie -- --check
git diff --check
```

The Clippy/format package list covers maintained crates and excludes the
copied Alloy reference crate; tests and the Rust 1.90.0 check cover the
workspace. The three ignored tests are the existing expensive N2 adapter
cases: `every_crash_point_in_steady_state_heavy`,
`every_crash_point_is_parent_or_candidate_heavy`, and
`growing_commit_every_crash_point_heavy`. They were not run for this
milestone; the new engine crash cases above were run and passed.
