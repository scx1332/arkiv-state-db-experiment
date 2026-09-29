# Using the durable engine

`arkiv_db::durable::DurableEngine` persists engine state through
`Arc<dyn VersionedStore>`. It uses the existing `Branch` execution and
`Snapshot` read/proof APIs. The ordinary memory `Engine` keeps its original
behavior. redb remains a dependency of the adapter, not the engine crate.

This is an engine-only file format. It stores namespace descriptors, raw
values, rows, indexes, allocators, pricing, host state and historical roots.
It does not store transaction bodies, receipts, protocol blocks, inclusion
identities or signing state. The full/light demo still runs in memory;
durable node restart is N4. Every historical engine snapshot is retained
on disk; there is no pruning or compaction.

## Lifecycle

Create an `EngineGenesis` with a deployment identity and initial pricing;
set its `limits` and `max_object_bytes` explicitly if defaults do not fit the
application. Save that configuration outside the database so subsequent
`open` calls can check the expected identity. Changing cache/staging options
on reopen is supported; silently changing genesis is not.

Give the facade exclusive authority to write and recover its versioned
store. A caller must not mutate or recover the same store through another
handle behind the facade's context gate. The shared `Arc` permits readers
and instrumentation; it does not coordinate multiple engine writers.

```rust,ignore
use std::sync::Arc;
use arkiv_db::{Budget, NamespaceName, OwnerId, PriceSchedule, WriteOp};
use arkiv_db::durable::{DurableEngine, EngineGenesis, Initialization,
                       Options, Publication};
use arkiv_db_storage::versioned::VersionedStore;

// Open/create RedbStore with quick repair and an explicitly bounded page
// cache, then pass it as Arc<dyn VersionedStore>.
let store: Arc<dyn VersionedStore> = /* configured adapter */;
let genesis = EngineGenesis::new([7; 32], PriceSchedule::FIXTURE);
let mut engine = match DurableEngine::initialize(
    store.clone(), genesis.clone(), Options::default()
)? {
    Initialization::Committed(engine) => engine,
    Initialization::NotCommitted { error, pending } => {
        // Retain pending for explicit recovery/retry; no public head exists.
        return handle_initialization_failure(error, pending);
    }
    Initialization::Indeterminate { error, pending } => {
        return handle_initialization_uncertainty(error, pending);
    }
};
let mut branch = engine.begin(engine.head(), None)?;
branch.try_execute(OwnerId::new([1; 20]), WriteOp::CreateNamespace {
    name: NamespaceName::new("project")?,
}, Budget::unlimited())??;
engine.prepare(branch.try_seal()?)?;
match engine.commit()? {
    Publication::Committed(info) => acknowledge(info),
    Publication::NotCommitted(error) => handle_definite_failure(error),
    Publication::Indeterminate(error) => handle_uncertainty(error),
}
```

The excerpt shows outcome handling, not an automatic retry policy.
Initialization is explicit; never try initialization as a fallback after
an open error. The first successful initialization atomically writes
profile/genesis, manifest/index and head zero. Ordinary startup uses
`DurableEngine::open(store, expected_genesis, options)`.

`prepare` consumes a sealed candidate. On a precommit failure,
`PrepareFailure` returns that candidate together with the lifecycle error.
On success the facade owns one encoded batch and a prebuilt stored-root
snapshot. It drops its dirty tree ownership before commit; any candidate
clones retained by the caller remain the caller's responsibility. Receipts
can be examined before handing over the candidate and are not persisted.

`commit` returns the adapter's three outcomes distinctly. Definite failure
keeps the prepared bytes for retry; `discard_prepared` can discard a healthy
prepared operation. An indeterminate result keeps the old public head and
stops writes. `recover_pending` gates readers, clears the cache, advances
the writer generation, recovers storage and verifies profile/head/records:

- `RetryPrepared`: the exact original parent remains and candidate records
  are absent. Only this facade-owned preparation can retry. Caller-owned
  branches/candidates from the old generation stay stale.
- `ObservedCommittedDurabilityUnknown`: the exact candidate is present,
  but reading it back does not certify failed-sync durability. This handle
  remains unable to write or acknowledge the candidate as durable.
- An error: the context stays fenced. Resolve the failure before retrying
  recovery; never overwrite an unexpected head or reexecute inputs to guess
  what happened.

With no pending publication, `recover` requires the exact already-published
head. It restores reads and new branches, preserving old immutable snapshot
and cursor identity; poisoned branches remain poisoned. A fresh `open`
starts a new instance and invalidates old cursors/candidates. Ordinary
startup trusts the adapter's recovery contract. A mere process reopen is
not a general cure for failed-sync data loss; see the specification's
[recovery policy](engine-storage-v1.md#publication-failures-and-recovery).

## History and proofs

`snapshot(SnapshotId::from_sequence(n))` reads one manifest plus its index
associations and creates stored roots. It performs no trie loads, replay,
namespace scan or index rebuild. A future sequence is `UnknownSnapshot`;
a missing required historical manifest is corruption. Several handles to
the same sequence in one open share cursor identity. Headers/roots are
available without storage access; data/proof reads use the same loader.

`Snapshot::prove_eq` and `verify_eq` retain their existing bytes, pricing
and limits. A caller still resolves protocol block identity and supplies
the proof's `SnapshotRef`; an engine commit id is not a block hash or signed
header. Old values and posting sets survive later changes and reopen.

## Bounds and accounting

The persisted object limit defaults to 1 MiB of encoded trie bytes. redb
checks its value guard before copying; metadata and manifests have separate
fixed-format caps. Preparation first preflights all reachable new nodes,
including nested namespace/posting maps, before leaf encoding or root
hashing. Fallible staging stops at configured object, payload and visit
limits; stored subtrees incur no staging reads.

Runtime defaults are an 8 MiB/16,384-entry FIFO encoded-object cache and a
32 MiB/100,000-object preparation with at most 1,000,000 new-node visits per
preflight/staging pass. Zero cache bytes or entries disables retention.
Cache hits still check the store's health. Recovery holds the context gate
against loads and clears cached bytes before reopening. The cache stores no
decoded trees, misses, errors, or uncommitted objects.

`stats()` separates retained cache buffer capacity, entry/container bytes,
hits/misses, generation, resident manifests, pending payload, and the last
preparation's visits, objects, bytes and map-kind counts. The consuming
adapter commit API currently requires one temporary clone of the bounded
batch; its payload is reported explicitly. The temporary dedup B-tree's
entry payload is reported separately from its allocator/node overhead.
`VersionedStore::stats()` records storage calls independently.

These bounds describe engine-owned committed state and preparation, not
whole-process RSS. Caller branches, checkpoints, receipts, raw inputs,
candidate clones, concurrent read/proof output, adapter tables/allocator/
transaction pages, and OS/allocator caches have separate ownership. A host
must bound those workloads before claiming a total memory budget.

See the [approved specification](engine-storage-v1.md) and the
[measured acceptance report](engine-storage-v1-report.md) for functional
gates, reproducible commands and backend limitations.
