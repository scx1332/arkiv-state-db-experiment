# Engine storage v1

Status: implemented and validated, 2026-09-24. This is the N3c engine-only
deliverable of the [node and storage roadmap](node-storage-plan.md), after
the volatile [full/light demo](full-light-demo.md). See the
[usage/lifecycle guide](engine-storage-run.md) and
[measured acceptance report](engine-storage-v1-report.md). The contracts
below remain the implementation requirements; N4 node integration is open.

## Outcome and boundary

Persist the actual engine's immutable state in one redb file, with one
writer and disposable, configurable, bounded caches. Every successful
durable publication survives ordinary process restart. Open the database
and any retained snapshot from small records and root handles, without
replaying operations, loading all namespace descriptors, or rebuilding
rows/indexes. Each publication stores only reachable new trie objects and a
small manifest, together with the durable head in one atomic transaction.
Release the engine's references to committed in-memory trees afterward.

Retain every manifest and every committed object indefinitely. No pruning,
garbage collection, compaction, persistent savepoints, export/import, or
history migration belongs to v1. Disk consumption is deliberately
unbounded with history; engine-owned resident history is not.

This is an **engine** milestone. It does not persist transaction bodies,
receipts, node blocks, inclusion identities, producer signing keys, queues,
or follower checkpoints, and does not wire durable restart into
`arkiv-node`. Host-map contents are engine state and are persisted, but
node expiry/replay/signing orchestration remains N4. Do not expose this
engine file as a restartable full node or call its commit identities block
hashes. Existing equality proofs must work on reopened snapshots; adding
new proof profiles is outside this change.

Keep `Engine::new`, `Engine::with_limits`, memory `publish`, and all current
memory-mode semantics. Preserve state-format-1 bytes, roots, canonical
receipts, charges, namespace boundaries and existing golden fixtures.
Storage resource failures are lifecycle/availability failures, never new
deterministic user rejections or charged receipts.

## Code findings that constrain the implementation

These preimplementation findings constrain the durable facade; the ordinary
memory helpers remain available for their original purposes:

- `WorkingState` is a catalog, namespace allocator, pricing schedule and
  host map. Catalog leaves include descriptors, record allocators and all
  namespace map handles; term leaves contain posting handles.
- `StateHeader` is small and contains catalog/host roots and lengths,
  allocator and full pricing. Its digest is the existing logical state
  root. Namespace and posting leaf decoding already creates stored nested
  map handles.
- `MerkleMap::to_stored` converts a committed map to its root/length without
  reading anything. `NodeBox::decode_stored` checks the content hash and
  strict canonical encoding, including inline nodes.
- Staging stops at `Link::Stored` and `from_store` nodes, visits inline
  leaves for nested maps, and stores a root even when its encoding is less
  than 32 bytes. It must continue visiting nested maps even if the same
  object hash was already emitted under another map kind.
- Current `Engine::publish` appends `Arc<SnapshotState>` holding the entire
  candidate into a growing vector. Reusing it would retain history and
  repeatedly stage nodes that were committed but still marked new.
  `Engine::from_state` also resets sequence/instance to a fresh genesis;
  it is a test aid, not the production history opener.
- `WorkingState::staging` currently collects cloned catalog leaves and
  postings, deduplicates by `(space, map kind, object id)`, and uses
  infallible `ObjectSink`. Calling it and checking size afterward is not
  bounded preparation. Raw `WriteOp::Put` values have no `Limits` byte cap,
  and trie value/node encoders allocate before handing bytes to the sink.
- `VersionedStore` has atomic outcomes, health and recovery, but its
  generic `reconcile` explicitly does not certify durability merely from
  reading an indeterminate write back. `RedbStore::epoch` is concrete, not
  part of the generic trait.

## API and ownership

Add `arkiv_db::durable::DurableEngine` in `arkiv-db`, using
`Arc<dyn VersionedStore>` and shared private engine helpers. Keep redb out of
the engine dependency graph. A small example/test in `arkiv-db-redb` opens
the real file and passes the store into this facade. Do not add an empty
or duplicate engine crate.

The public surface must support the following operations; final Rust
names may follow local conventions without changing their contracts:

| Operation | Contract |
| --- | --- |
| `initialize(store, genesis, options)` | Explicitly initialize an engine-empty store; atomically persist metadata, genesis manifest and head 0. Never reinterpret a nonempty foreign store. |
| `open(store, expected_genesis, options)` | Open an existing engine profile and verify identity/config/versions. Never initialize on an open error. |
| `head`, `head_snapshot`, `begin` | Use the last successfully published immutable head; `begin` requires its expected sequence and a healthy writer. Return the existing `Branch` execution surface. |
| `snapshot(sequence)` | Fallibly read one retained manifest by sequence and return the existing `Snapshot` read/proof surface. No history vector or full manifest scan. |
| `prepare(candidate)` | Check engine instance, writer generation, base and resource caps; construct a bounded owned prepared commit and prebuild its root-only publication handle. |
| `commit(prepared)` | Atomically persist and then publish, reporting committed, definitely not committed, or indeterminate distinctly. |
| `recover_pending` | Reopen/validate storage and reconcile the exact prepared identity under the policy below; never execute the candidate again. |
| `recover` without a pending commit | Recover a failed reader/store, revalidate the same profile and exact previously published head, and restore availability if they agree. |
| `stats` | Noncanonical cache, staging, retained-handle and storage measurements. |

Keep error types separate from the existing small `EngineError`; add a
durable lifecycle error/outcome type wrapping `EngineError`,
`StorageError`, resource-limit failures and uncertainty. Unknown future
snapshot is distinct from a missing required manifest. A missing manifest
at a sequence at or below the retained head is corruption/unavailability,
not `UnknownSnapshot`. A caller can reconstruct a snapshot sequence from
its saved numeric value through a checked constructor or durable lookup;
it must not need an in-memory handle from the preceding process.

The facade owns only its current head, store context, runtime options and
at most one prepared/pending publication. No map of all manifests, weak
handle registry, namespace table handles, or per-snapshot cache may grow
with history. `snapshot_count` is a checked sequence-derived count, not a
vector length; sequence increment overflow fails before staging/writing.
Use a fresh process instance token on open so foreign candidates and
pre-restart query cursors remain invalid even when numeric snapshot IDs
match. Cursor bytes/semantics do not change.
Repeated `snapshot(sequence)` calls within one open use the same instance
token, so pagination can continue through a newly acquired handle to the
same snapshot. In-place recovery keeps immutable snapshot/cursor identity
but advances the separate writer generation; closing and opening again
invalidates old cursors. Test both cases explicitly.

Factor construction of a `SnapshotState` from `(sequence, base, instance,
limits, StateHeader, Nodes)` and construction of a branch from that state.
Do not construct a temporary `Engine` or publish through its history vector.
The head's catalog and host roots are always stored/empty links; decoding
a catalog leaf supplies the other stored handles lazily. Existing pinned
snapshots continue referring to their complete old state across commits.

The caller grants this facade exclusive writer/recovery authority for the
store. It must not independently call `commit` or `recover` through another
shared store reference. redb's file lock excludes a second process; the
facade serializes preparation/commit/recovery in-process and still uses the
store's compare-and-set as the last defense. Multiple speculative branches
may exist, but only one prepared/pending publication is owned at a time.
An unexpected head is a conflict requiring reopen, never permission to
silently rebase a candidate.

## Durable identity and bytes

There are three separate identities:

1. Adapter `DatabaseId`: random identity of the physical database, used to
   separate caches/handles. It never changes logical roots.
2. `EngineGenesisId = digest("arkiv/engine-genesis/v1", encode(genesis))`:
   deterministic identity of an explicitly supplied engine genesis.
3. `EngineCommitId = digest("arkiv/engine-manifest/v1", encode(manifest))`:
   identity of one engine publication, including its sequence and parent.
   Equal states published twice still have different commit identities.

Freeze a small canonical `EngineGenesisV1` with: engine-storage profile and
version, codec version 1, state format 1, counting model version 3,
record-key encoding version 2, trie-layout version 1, caller-selected
32-byte deployment identity, complete `Limits`, initial `PriceSchedule`,
and the persistent maximum encoded object size. Start only from the
ordinary empty engine state (`next_namespace = 1`, empty catalog/host).
There is no implicit import of an arbitrary memory snapshot in v1.

Freeze the field order just listed. Encode the profile as the eight raw
bytes `ARKENG01`, all six version fields as `u32`, deployment identity as
32 raw bytes, `Limits` and pricing with their existing codecs, and the
maximum object bytes as `u64`. `Meta["engine/profile"]` is
`ARKENG01 || u32be(1)`; the genesis record is
`encode(EngineGenesisV1) || EngineGenesisId`, without another length prefix.

The persistent object-size ceiling is a storage admission bound, not part
of the logical state root and not a new `WriteOp` rejection. A prepared
candidate exceeding it cannot be published by this durable profile;
memory execution remains unchanged. Persisting it ensures that objects
accepted at creation remain readable under the same bounded reader after
restart. Initial default: 1 MiB encoded object; callers may explicitly
choose another checked finite limit at genesis. Validate arithmetic and
codec representability of every configured limit; do not silently narrow
`usize`, wrap counters, or substitute defaults. Initial pricing must match
genesis; subsequent pricing comes from each manifest's StateHeader.

Use these existing tables, with no adapter table-format change:

| Location | Bytes |
| --- | --- |
| `Meta["engine/profile"]` | Fixed magic and engine-storage version 1 |
| `Meta["engine/genesis"]` | Canonical `EngineGenesisV1` and its digest |
| `Manifests[sequence_be_u64]` | Canonical manifest envelope described below |
| `BlockIndex[EngineCommitId]` | Sequence, 8-byte big-endian |
| Catalog/Host/Namespace object spaces | Existing trie encodings under existing content hashes |
| Durable `Head` | `height = engine snapshot sequence`, `hash = EngineCommitId` |

`Blocks` and `Inclusions` remain unused. This is an explicit **engine
profile** of the generic commit store: existing fields named height/hash
and table `BlockIndex` carry a commit sequence/digest, not a protocol block.
Update the storage-contract documentation to permit this profile without
changing the existing binary shapes or adapter behavior. Generic
reconciliation compares the records supplied by the caller; it does not
require a protocol block. N4 must introduce an explicit node profile and
atomic block/state contract; it must not open this profile as node storage
or silently reinterpret these heads. No profile migration is promised.

`EngineManifestV1` contains version, genesis digest, sequence,
`Option<(parent_sequence, parent_commit_id)>`, the unchanged `StateHeader`,
and its logical state root. The envelope includes its commit digest, which
is recomputed and checked on every manifest read. Genesis has sequence 0,
no parent and the exact expected empty header. Successors require parent
sequence `sequence - 1`; publication uses exactly `head + 1`. This is a
constant-size record for the current fixed pricing layout, independent of
namespace count, history length, rows and postings. Do not embed namespace
lists, receipts or transaction inputs.
Manifest field order is the order above: `u32 version`, 32-byte genesis
digest, `u64 sequence`, canonical `Option<(u64, B256)>` parent, existing
StateHeader encoding and 32-byte state root. The stored envelope is
`encode(EngineManifestV1) || EngineCommitId`, without another length prefix.

Use the existing canonical codec rules, strict full consumption and checked
decode, with hand-constructed golden vectors for the new types. Unknown
profile/version is `Incompatible`; malformed bytes, bad digests, invalid
root/count or allocator relations, wrong sequence/index/head association,
or inconsistent genesis is `Corrupt` (a caller's different expected
genesis/config is `Incompatible`). Validate header invariants through the
strict decoder, not just `MerkleMap::from_root`; the existing `from_state`
helper does not check every invariant that the StateHeader decoder does.

Initialization is one adapter commit from `None` containing both metadata
records, genesis manifest/index and head 0. An adapter-initialized file
with no engine head/config is recoverable initialization state; existing
head or partial engine metadata without a complete matching genesis is
not a fresh engine. Opening an engine reads its profile/genesis metadata,
genesis manifest, head manifest and relevant index records only: a bounded
number of records, zero trie object loads. A lazy open cannot certify all
historical nodes; absence/corruption is detected when a referenced node is
read. Do not claim an eager whole-file integrity audit.
Initialization also has all three commit outcomes. Return a usable engine
only for a committed genesis; uncertain initialization retains its small
expected metadata/manifest in a recovery token and exposes no public head.
A retry/reopen cannot silently treat uncertainty as an empty new database.

## Loader and bounded cache

V1 uses a bounded **encoded-object cache**, not a decoded-node cache.
This deliberately keeps ownership and accounting simple. Key it by
`(DatabaseId, SpaceId, ObjectId)`; a byte cache has no map type, so no
`MapKind` key is necessary. A future decoded cache must include map kind
and account inline nodes, values and nested allocations separately.

Store only content-hash-verified immutable byte buffers; the `Nodes` loader still
calls `NodeBox::decode_stored` in the correct map context on every load.
No errors, missing-object answers, uncommitted objects, or partially
decoded values are cached. Cross-namespace deduplication is forbidden even
for equal hashes. All query and proof paths use the same loader. Cache
warmth cannot change logical trie probes, receipts or charged work.

Expose runtime cache bytes and entry count limits, including zero to
disable it; recommended defaults are 8 MiB encoded payload and 16,384
entries. Evict before retaining an insertion that would exceed either
bound; bypass caching for an individual larger object. Count owned buffer
capacity and bounded entry/container overhead separately, including key
and eviction metadata. Use a simple bounded policy whose metadata cannot
accumulate obsolete access records. No unbounded negative cache, per-space
registry or manifest cache. redb's independent page cache is separately
configured (default 8 MiB), and quick repair remains on for real durable
use. These options are runtime policy and can change on reopen without
altering genesis or logical results.

Check the context and `VersionedStore::health` before answering a hit and
after an underlying read before admitting its result. Recovery holds a
context gate against in-flight loads, marks the context unavailable,
increments its own generation, and clears the cache before reopening.
Clear/invalidate again on failed recovery; do not insert a result from an
older generation. Candidate generation checks reject work begun before
recovery. Already pinned immutable snapshots may read again through the
new healthy context after successful recovery, but poisoned branches stay
poisoned. Root/header-only accessors need not touch storage. Never serve
cached data as a way around a latched failed store.

Read transient allocations are not cache entries. Add a bounded-object
read seam: the real redb path checks the value guard's length against the
persisted object ceiling before copying into a `Vec`. A default helper
that calls today's allocating `read_object` and checks afterward does not
establish this bound; implement/forward the bounded method for the real
adapter and test wrappers. Validate size before hash/decode; an oversized
stored object is `Corrupt`. Bound manifest/metadata reads similarly by
their format-derived maximum sizes. The ceiling limits an individual
decode; traversal/query/proof output and concurrently active operations
remain separate memory components.

## Incremental preparation and release

Preparation is read-free over newly created nodes; it must not traverse
committed subtrees to discover what to write. Preserve the trie staging
rules, including promotion of roots/inline children and nested postings.

1. Validate engine instance/generation, expected head, sequence arithmetic,
   writer state and immutable profile before creating a batch.
2. Preflight reachable **new** leaf values without first encoding/hashing
   them. Check raw blob and row encoded-size upper bounds with checked
   arithmetic against the persisted object limit. Catalog/term leaves
   recurse through their new nested maps. This handles raw `Put` values
   that are currently unbounded by `Limits`; calling `state_header` first
   can already allocate the oversized encoding.
   Apply the same finite new-node visit allowance during this preflight;
   the preflight itself cannot perform an unbounded walk before the cap.
3. Add a fallible incremental staging visitor/sink so exceeding object,
   byte or traversal-work capacity immediately stops the walk. Preserve
   the current infallible test helpers as wrappers if useful. Do not
   collect all namespace/posting clones up front. Visit nested leaves
   synchronously or with a bounded traversal stack.
4. Deduplicate physical batch entries by `(space, id)`, while continuing
   the nested-map visits of each map kind. Hash collisions/different bytes
   under one id fail before commit. Keep map-kind counters independently
   for measurements. Roots smaller than 32 bytes are emitted; inline
   nonroots are not independent objects unless newly required as roots.
5. Build the manifest/index and complete `CommitBatch`; prebuild the
   root-only `SnapshotState` and success result needed for publication.
   The candidate's final state root must equal the manifest's state root.
6. On acknowledged commit, swap to the prepared stored/empty root handles,
   release the batch, staging dedup set and engine-owned candidate trees.
   Do not mutate shared `from_store` flags or retain the dirty candidate as
   the new head. The next branch is based on stored roots.

Runtime preparation defaults: 32 MiB encoded object/key/record payload,
100,000 distinct staged objects and a finite new-node visit cap (initially
1,000,000). Check capacities before retaining bytes/entries. Record
transient overhead separately; copying the complete batch for `commit`
and reconciliation costs up to another payload cap and must not be hidden.
Prefer moving the batch once and retaining only fixed manifest/expected
identity for reconciliation; if retry needs objects, retain one bounded
owned batch, with an explicitly measured temporary clone at the current
consuming `VersionedStore::commit` boundary. Never keep a batch per history
entry. A cap failure occurs before storage mutation and leaves the
candidate and public head unchanged.
Prefer consuming a sealed candidate into the single pending preparation
slot (returning it on a precommit failure); its externally retained clones
remain caller-owned. Do not store a second dirty state clone alongside the
prepared root-only handle merely to produce the success result.

Rollback/repeated overwrites contribute only final reachable objects;
unchanged stored subtrees incur no staging loads. An empty successor has
no object writes. A namespace owner transfer may update the catalog but
must not rewrite its record/index/posting maps. Root preparation and
staging may revisit new paths a bounded number of times, but never old
state or history in proportion to its size.

## Publication, failures and recovery

Use one writer state machine: `Ready`, `Prepared`, `Uncertain`, or
`Unavailable`. Preparation failures are definitely before commit.
Persist new objects, manifest, index and head in one adapter transaction
against the precise expected head; initialization includes metadata there
as well. No public head change, success result or new snapshot lookup is
allowed before `Committed`. Existing old snapshots remain valid readers
while the store is healthy.

| Store outcome | Engine action |
| --- | --- |
| `Committed` | Swap the already prepared root-only head, release staging, return the new sequence/commit ID. Publication after this point has no storage I/O or fallible encoding. |
| `NotCommitted(error)` | Preserve the old public head. Report definite noncommit, retain/return the bounded prepared candidate for explicit retry. If the store is failed, recover it before another commit; never auto-reexecute operations. |
| `Indeterminate(error)` | Preserve the old public head, keep one bounded pending identity/batch, report uncertainty, stop further writes and suppress any success acknowledgement. |

If the process dies after storage commit and before RAM publication, the
next open loads the committed manifest/head; it does not reexecute or
duplicate the state transition. Disk-full at preparation/transaction
staging and disk-full inside commit must retain the adapter's different
definite/uncertain outcomes. Read or decode failures still use existing
snapshot error and branch-poisoning semantics; storage errors cannot
appear as query absence, invalid user input, receipt, or successful proof.

Recovery first gates readers/writers, clears the cache and reopens the
store; then revalidates profile/genesis and the relevant head/manifests.
Compare exact pending manifest/index bytes, not just a state root:

- Head equals the exact parent: the pending commit is absent. Once
  recovery establishes a healthy unchanged parent, the identical prepared
  batch may be retried; it is never reconstructed by executing the input.
- Head equals the candidate and its records match: its application is
  observed. This alone does **not** upgrade generic uncertainty to a
  durable acknowledgement after a possible failed sync.
- Any other head, missing/different record, or recovery error: remain
  unavailable; do not overwrite, rebase or acknowledge anything.

Successful parent reconciliation can reauthorize **only the facade-owned
prepared/pending operation** under the new writer generation. This covers
both definite noncommit with a failed handle and absent indeterminate
commit. It must recheck the exact original parent, profile, manifest/index
and batch identity before retry. Other caller-held branches/candidates
from before recovery remain stale. If no prepared/pending operation is
retained (for example a read failure, or a discarded definite failure),
`recover` requires the durable head to equal the already published head
and revalidates that manifest; it restores reads/new branches but grants no
retry exception. An unexpected head remains unavailable for explicit
close/open and inspection. Failed recovery leaves both paths fenced.

V1's generic engine policy is conservative: observed application after
`Indeterminate` is reported as `ObservedCommittedDurabilityUnknown`, with
writes stopped and no new successful durable publication on that handle.
Do not add an unproven generic `reconcile => Committed` shortcut. A caller
must resolve the underlying medium/restart condition and open a fresh
engine before continuing; the API documentation must distinguish this
operational recovery from an assertion that mere process reopen cures
failed-sync data loss. Ordinary startup trusts the adapter's recovery
contract, with the same platform limitations already recorded by N2.

The redb adapter's quick-repair/two-phase recovery evidence is useful but
limited: it does not establish every kernel/device's failed-sync behavior.
Keep quick repair enabled, rerun the existing lost-final-sync model with
real engine objects, and report its outcome. A future adapter-specific
durability assurance can allow a stronger recovery result without changing
canonical state; adding that assurance is not required to ship this
fail-closed engine v1.

## Memory accounting and isolation claims

The claim is bounded engine-owned committed-state retention, **not** a
constant bound on whole-process RSS under arbitrary caller workloads.
Publish a ledger with these separate categories and peaks:

| Category | Required bound/accounting |
| --- | --- |
| Current head/genesis/config | Constant number of small headers/records and stored roots |
| Historical manifests | Zero resident history catalog; transient one manifest per lookup |
| Encoded-object cache | Configured payload and entry bounds, plus measured fixed-per-entry/container overhead |
| Prepared/pending commit | One payload/object/work-bounded batch and explicit dedup/serialization/clone peaks |
| Caller-held snapshots | Small root/header/context handles, O(number of live handles), no retained loaded tree from read-only access |
| Branches/checkpoints/candidate clones | Caller-owned dirty paths, receipts and rollback history; counted/documented separately and released on drop |
| Active reads/proofs | Decode/traversal/output work, bounded by object and existing request/proof limits where those limits apply |
| redb | Separate page cache, allocator/table metadata, transaction dirty pages and backend recovery work |
| OS/runtime | Allocator retention, filesystem page cache and thread/process overhead; observed RSS, not attributed to the object cache |

Do not invent total-process boundedness from `block_budget`: zero-priced
operations, unbounded caller branch count, receipts, checkpoints and raw
values defeat that inference. V1 does not add a scheduler or silently
change `Branch` admission semantics. A host seeking a total memory budget
must bound concurrent handles/branches, operation input bytes/count,
checkpoints, receipts and read/proof concurrency. Tests use explicit
workload bounds and drop these external owners before asserting committed
tree release. Instrument live newly allocated trie nodes/bytes or use
drop/weak-reference assertions so allocator RSS reuse does not hide leaks.

Preserve physical object spaces: catalog, host and one namespace table per
namespace. Account target-namespace object access separately from catalog,
manifest/index, redb table directory, allocator and commit maintenance.
Foreign namespace objects must never be loaded for a target query/proof.
Shared metadata depth and backend page I/O may grow; do not call them a
constant disk-latency or constant physical-I/O guarantee. Cache churn is
measured behavior and never changes canonical charges. Existing
[namespace isolation](project-namespaces.md) and N2 limitations remain in
force; update evidence rather than enlarging claims.

## Proofs and compatibility

`Snapshot::prove_eq` already walks catalog, terms, postings, rows and keys
through `Nodes`. Route reopened snapshots through that same implementation;
retain the current proof version, limits, verifier and metering. Pin one
manifest per request. Its state root must match the proof request's
`SnapshotRef.state_root`; the caller still supplies/resolves protocol block
identity. An engine commit ID is not a signed block identity and the
engine must not fabricate a block header around it.

Demonstrate byte-identical supported proofs for equal requests/states in
memory and durable modes with cache disabled, cold and warm, including
absence and pagination. Verify with the independent existing verifier.
The broader historical block-to-snapshot mapping, signing and node serving
are N4. Existing proof errors may remain their established public
`StorageUnavailable` form; they must not become valid absence evidence.

## Implementation sequence and validation gates

Implement in reviewable slices; code and meaningful tests travel together.

1. **Format and facade foundations.** New durable config/genesis/manifest
   codec and golden vectors, explicit profile docs, shared snapshot/branch
   constructors, sequential lookup, initialization/open validation. No
   memory `Engine` behavior changes.
2. **Loader/cache and bounded reads.** Shared context health/generation
   gate, encoded cache, pre-copy size checks in redb and wrappers, counters.
   Test zero/tiny cache, eviction, DB/space isolation, missing/corrupt bytes,
   and cached reads after failed-store/recovery transitions.
3. **Bounded incremental preparation and publication.** Read-free preflight,
   fallible staging, nested-map dedup, atomic commit, root-only replacement,
   uncertainty state machine. Exercise all three outcomes and retained
   candidate ownership. Do not delay release until a later memory project.
4. **Engine differential/failure suite.** Run identical seeded histories
   through ordinary `Engine`, the durable facade over
   `MemoryVersionedStore`, and real redb. Include raw/typed data, every cell
   type, patch/delete/recreate, large/shared postings, rollback, empty
   commits, namespace transfer, schedule changes and host state. Compare
   receipts, state/namespace roots, results, ordering, pagination and proofs
   at old snapshots after close/reopen. Compare logical probes/charges;
   physical cache/store counters are measurements, not equality fixtures.
5. **Crash and measured acceptance.** Add a runnable example/measurement
   command and a checked-in report with workload/options, environment,
   counters, timings and observed limitations. Only then mark N3c engine
   storage v1 done; leave N4 and broad proof work open.

Required failure evidence includes:

- Wrong genesis/limits/profile/version, sequence overflow, truncated/trailing
  manifest bytes, wrong digest/root/index, absent old manifest, and malformed
  catalog/host/row/posting objects; no silent empty/unknown fallback.
- Preparation caps at their boundaries: oversize raw values before hashing,
  byte/object/visit cap exhaustion before commit, dedup across map kinds,
  nested inline/root cases, unreachable rollback paths and repeated updates.
- FaultyStore definite failures, applied/not-applied uncertainty, lost ack,
  failed recovery, cached reads under a failed health latch, unexpected
  concurrent head and stale pre-recovery branches/candidates.
- CrashDisk cuts/failures at actual engine transaction write/growth/sync
  points, plus process termination before commit and after commit/before
  publication. Reopen exposes exactly the complete parent or complete new
  engine snapshot. Every acknowledged commit survives the model's durable
  cut; uncertainty is allowed either result, never mixed roots/objects.
- Run the lost-final-sync/page-cache model with quick repair on using engine
  manifests and reachable namespace/host objects; do not replace it with
  only mock `CommitOutcome` tests. Keep the existing redb contract, disk-full,
  creation, latching and recovery suites.

Measured acceptance is deliberately separated from timing assertions:

| Experiment | Hard functional gate | Measurements to record |
| --- | --- | --- |
| Cold open at 100, 1,000 and 10,000 snapshots | Bounded metadata/manifest reads; zero trie object reads and no replay/namespace/index scan | Application open reads/time versus redb open/allocator/repair reads/time, file bytes |
| Old snapshot open/read/proof with 0, 64 KiB and 8 MiB object caches | Manifest lookup only until first walk; exact historical answers/proofs; cache limits never exceeded | Object/cache hits/misses, logical visits, backend reads, proof bytes/time |
| Fixed small update as live state/history grow by at least 10x | No untouched-space object writes; no stored-subtree staging reads; empty commit has zero object writes | New-node visits/unique bytes, record bytes, physical writes/syncs/amplification; compare with trie path-size growth rather than demand a false fixed byte count |
| 10,000 bounded publications with external candidates dropped | Only current stored-root head retained; no resident manifest/history vector; old dirty nodes drop | Live trie nodes/bytes, cache payload/entries, staging peak, redb metadata, process RSS at checkpoints |
| Retain many historical snapshot handles, then read and drop them | Read-only handles retain roots/header/context only; loaded objects reside solely in bounded cache or active call temporaries | Per-handle overhead, node drop evidence, peak live decode memory |
| Fixed target namespace while another grows by at least 10x | No foreign namespace object reads; same target answers, proofs and canonical charges | Catalog/directory/backend I/O separated from target-table I/O and cache effects |

V1 acceptance note (2026-09-24): the hard functional gates above passed.
The [reviewed measurement report](engine-storage-v1-report.md#interpretation-and-unavailable-attribution)
does not measure isolated peak active decode bytes/nodes, private per-handle
heap overhead, or resident redb metadata heap. Those allocation-attribution
targets remain deferred observability work, not passed measurements.
Stored-root ownership and graph-wide weak-reference tests establish the
committed-tree release contract; process RSS/HWM and disk allocation do not
substitute for the missing attribution or establish total-process bounds.

Use practical datasets (for example 1,000 versus 10,000 live rows plus the
10,000 small-commit history run); report exact workload sizes rather than
claiming production scale. Backend metadata and allocator RSS may grow;
if measurements show a meaningful unbounded application registry, history
vector, repeated full-state staging, or read handle retaining loaded trees,
fix it before acceptance. If redb itself dominates growth, report it
explicitly and narrow the claim; do not attribute all RSS to the cache or
silently omit the result. CI asserts structural bounds/counters, not tight
wall-clock or RSS thresholds tied to one machine.

Run affected crate tests during development, then the repository's existing
workspace tests, formatting, lint and minimum-toolchain checks as documented
in [testing](testing.md)/README. Existing memory roots/receipts/proof vectors
must remain unchanged. The final report must distinguish new measurements
from N2 adapter-only evidence and list any commands not run.

## Decisions fixed here; implementation details left open

Fixed: engine-only profile, separate durable facade, empty explicit genesis,
stable per-file snapshot sequences, existing state hashes/encodings,
encoded-byte cache, direct manifest lookup, one prepared/pending commit,
bounded preparation, root-only publication, no protocol block/body
persistence, fail-closed uncertain durability, no pruning/compaction, and
honest separation of caller/adapter/application memory.

The implementer may choose a simple cache eviction structure, precise Rust
error/ownership type names, placement of private constructors, instrumentation
mechanism, and a fallible traversal API that preserves these contracts.
Those are library details, not reasons to weaken the gates. Any proposed
change to identity, durable acknowledgement, logical bytes/charges,
namespace isolation or memory-retention claims requires revising this
specification and reviewing the change before implementation proceeds.
