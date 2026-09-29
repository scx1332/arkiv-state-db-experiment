# Full node storage and recovery review — 2026-09-27

## Assessment

The storage path is a strong part of this experimental node. The implementation separates execution, preparation, durable publication, and observation of a committed head. Failure handling distinguishes a definite non-commit from an uncertain commit; neither becomes a successful block acknowledgement. This review found no clear localized correctness defect in the durable engine or redb adapter that warranted a code change. The signed-node reviewer independently found a header-observation issue in synchronization; that finding belongs to the node review, not the adapter's atomicity contract.

Scope: versioned storage contract, redb commit/open/recovery, durable engine preparation/cache/recovery, and the signed host's ordering of body persistence and state publication. This is a code review and focused fault-test run, not a formal proof or a claim of production readiness.

## How publication works

1. Execution builds an immutable candidate against the current engine snapshot. It does not change the public head.
2. The engine checks the candidate's instance, base snapshot and recovery generation. Bounded preflight and staging collect changed content-addressed objects, a manifest, and node records into one prepared batch. Unchanged tree links remain references to stored objects.
3. The signed host durably saves the complete block body in a separate content-addressed history database. Its address is computed before publication; its durable write must succeed before the engine is committed.
4. One redb transaction compares the expected engine head, inserts the immutable objects and records, and changes the head. Immediate durability and quick repair are enabled. A successful commit is the boundary that permits publication.
5. Only then does the signed host advance its advertised head and remove included transactions from its queue.

This ordering intentionally permits an orphaned retained body if the process stops between the history and engine commits. A retained body alone is not proof of inclusion. The converse—a successfully published engine reference whose body was never durably saved—is prevented by the ordering, under the storage adapter's durability contract.

Evidence:

- [Immutable objects, insert-only records, atomic head contract](../crates/arkiv-db-storage/src/versioned.rs#L1).
- [Candidate checks and bounded preparation](../crates/arkiv-db/src/durable/mod.rs#L677).
- [Preflight and staging](../crates/arkiv-db/src/durable/prepare.rs#L88).
- [Redb commit outcomes](../crates/arkiv-db-redb/src/store.rs#L461) and [single-transaction staging](../crates/arkiv-db-redb/src/store.rs#L513).
- [Signed host publication ordering](../crates/arkiv-node/src/signed/host.rs#L748).
- [Body framing and content-addressed history](../crates/arkiv-node/src/signed/history.rs#L1).

## Failure and recovery behavior

| Event | Behavior |
| --- | --- |
| Preparation exceeds a cap | No storage commit; candidate is returned to the caller. |
| Expected head differs | Commit refuses to overwrite it; engine writer becomes unavailable. |
| Definite non-commit | Retain the exact prepared batch; retry without executing transactions again. |
| Commit outcome uncertain | Retain the pending batch and fence publication. |
| Recovery finds old parent | Validate that pending records are absent, then allow the exact prepared retry. |
| Recovery reads back pending head | Report observed committed data with durability unknown; do not turn that observation into durable success or authorize another write on the handle. |
| Store read or I/O failure | Return an availability error, not absence or an empty query answer. |
| Recovery fails or unexpected head appears | Shared engine context remains unavailable; cached reads cannot bypass this state. |
| Stale candidate survives recovery | Recovery generation rejects its later publication. Existing historical snapshots can still be read after successful validated recovery. |

The distinction between “readable now” and “durably acknowledged” is deliberate. A lost final sync cannot be repaired by merely seeing the candidate in the same filesystem cache. Fresh process opening relies on the adapter and operational medium/recovery decision; it is not evidence that an earlier failed acknowledgement succeeded.

Redb recovery takes the writer lock and exclusive database-slot lock, waits for in-flight reads, drops the old handle and reopens the same database identity. Engine recovery clears encoded-object caches and advances a generation before validating stored metadata. The object cache is checked against store health before a hit can be returned.

New file creation uses a locked temporary file, initializes and syncs it, renames it into place, then syncs the parent directory on Unix. Interrupted initialization is never used as a fallback reason to overwrite an existing non-empty database. Non-Unix directory-sync behavior is explicitly weaker and should not be described as identical to the Linux deployment.

Evidence: [engine outcomes/reconciliation](../crates/arkiv-db/src/durable/mod.rs#L771), [cache health and recovery gate](../crates/arkiv-db/src/durable/cache.rs#L74), [adapter reopen](../crates/arkiv-db-redb/src/store.rs#L1021), [atomic file initialization](../crates/arkiv-db-redb/src/store.rs#L699).

## Findings and limits

### S1 — Medium operational priority: retained history has no deletion policy

**Design limitation; no correctness patch proposed.** Objects and historical manifests are insert-only and every retained version remains available. Expiring or deleting a logical entity does not delete its previous trie objects or retained transaction bodies. Orphan bodies from interrupted publication also remain retained. Therefore disk usage grows with history, even though the engine's resident cache and prepared batch are bounded.

For the present experiment, monitor free bytes and file growth and preserve a restart/backup runbook. Before long-running production use, define a retained-snapshot horizon and design crash-safe reachability-based garbage collection. Simply deleting old manifests or namespace files is unsafe because newer snapshots share immutable nodes with old versions. No pruning was added as part of this review.

Evidence: [storage contract explicitly has no deletion](../crates/arkiv-db-storage/src/versioned.rs#L14); [snapshot lookup opens any retained manifest](../crates/arkiv-db/src/durable/mod.rs#L618); [existing storage report acknowledges growing disk and RSS](engine-storage-v1-report.md#L170).

### S2 — Low performance priority: encoded-object cache lookup is linear and serialized

**Confirmed implementation characteristic; impact needs measurement.** The cache searches a `VecDeque` with `iter().position` while holding one mutex; a cache miss holds that gate through the backend read. The configured count is bounded (default 16,384 entries), so this is not unbounded memory growth, but lookup work and concurrent-reader contention can grow with the configured cache and workload.

Keep the current simple design until profiles show it matters. A bounded lookup index could remove the linear scan without changing hashes or the storage format, but changing lock ownership requires preserving the recovery/fencing invariant. This review does not claim a measured throughput regression.

Evidence: [cache lookup and backend read under the gate](../crates/arkiv-db/src/durable/cache.rs#L124); [default cache limits](../crates/arkiv-db/src/durable/mod.rs#L37).

### S3 — Informational: bounded open is not a full integrity scrub

Startup checks a bounded set of profile, genesis, manifest, index and current-body associations. It intentionally does not traverse every old trie or body. Corruption in old retained data can therefore be discovered later when that historical snapshot or body is requested. Those reads fail with corruption/storage errors, rather than returning fabricated absence. Keep this behavior explicit in operations documentation; a separate offline scrub could be added without slowing every normal startup.

Evidence: [engine open](../crates/arkiv-db/src/durable/mod.rs#L239); [history open explicitly validates older bodies lazily](../crates/arkiv-node/src/signed/history.rs#L90); [bounded integrity-checked object reads](../crates/arkiv-db-redb/src/store.rs#L889).

### S4 — Informational: cache limits are not a whole-process memory limit

Default limits bound 8 MiB of retained encoded cache payload, 16,384 cache entries and a 32 MiB prepared payload. Container overhead, one commit-batch clone, backend page cache, caller-held branches and concurrent query output are separate allocations. The instrumentation reports several of these categories explicitly. Do not label cache payload as process RSS or infer constant total memory from a bounded cache.

Evidence: [options](../crates/arkiv-db/src/durable/mod.rs#L28), [clone accounting at commit](../crates/arkiv-db/src/durable/mod.rs#L781), [preparation counters](../crates/arkiv-db/src/durable/prepare.rs#L12), [prior measured memory discussion](engine-storage-v1-report.md#L149).

## Validation

Completed initial focused run:

```text
cargo test -p arkiv-db --test durable --test durable_failures \
  -p arkiv-db-redb --test recovery --test disk_full

33 passed, 0 failed, 0 ignored
```

This includes 10,000 publications with bounded resident engine state; warm-cache failure fencing; stale candidate rejection after recovery; missing/corrupt/oversized objects; manifest identity and semantic damage; exact resource boundaries; lost acknowledgements; storage-full faults; and quick-repair reopen at several file sizes.

Completed additional focused run:

```text
cargo test -p arkiv-db-redb --test reconciliation --test latching \
  --test initialization --test engine_process --test atomicity

25 passed, 0 failed, 3 ignored
```

This checks crash-point atomicity with and without quick repair, process death before commit and after disk commit but before publication, interrupted and concurrent file initialization, I/O failure latching, waiting for in-flight reads during recovery, and lost-sync reconciliation.

Combined result: **58 passed, 0 failed, 3 ignored**. The three ignored tests are explicitly heavy crash/torn-write matrices; they were not run. Debug recovery tests assert bounded application-level reads, not constant physical I/O or startup wall time. No production database was mutated and no deployment was performed.
