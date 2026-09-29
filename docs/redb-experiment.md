# redb storage experiment (node step N2)

Node step N2 of the [node and storage plan](node-storage-plan.md) evaluates
redb as the first durable backend behind a narrow storage contract. This
document fixes the pass criteria, describes what was built and how it was
measured, and records the results with a verdict per gate. The experiment
stores the objects, records and heads that callers commit; it does not
persist the engine yet (N3) or run a durable node (N4).

## Pass criteria

Gates 1–3 restate the criteria that the frozen N2 design brief (§5.2) fixed
before N2 began. The criteria of gates 4–9 were written by the N2
implementer during the experiment; this document was first committed
together with its results, so the history does not show them fixed before
the measurements. After review, gate 8's criterion was corrected: it said
that running out of space "applies nothing", which the evidence contradicts
(a `Full` failure of the final sync can be a commit that happened), and it
now states what the tests check. Gate 6's wording was made precise at the
same time: reconciliation reads state back through a fresh handle, which is
not necessarily durable state (see its results). A gate that fails gets a
named fallback; only passed gates are claimed.

1. **Atomicity.** A crash at any point of a commit that touches every table
   of a block (objects in the catalog, host and several namespace tables, a
   new namespace table, block, manifest, block-index, inclusion and metadata
   records, and the head) leaves either the parent state or the candidate
   state after recovery, never a mix: every candidate object and record is
   present with the candidate head, or every one of them is absent with the
   parent head, and all earlier blocks are intact. "Any point" means every
   mutating backend call (write, sync, length change) of the commit, under
   the power-loss model below, including torn writes.
2. **Isolation.** Namespace A's application-level object reads (calls, hits,
   bytes, spaces touched) are identical while namespace B grows 1×, 10× and
   100× A's size, while the number of namespaces grows, and while history
   grows. A's backend page reads are measured and stay within a bound derived
   from B-tree depth: with no page cache, each object read costs at most the
   height of redb's table directory plus the height of A's own table.
3. **Recovery.** The application's reads at open are constant: the store's
   metadata, the head and the manifest at the head, whatever the database
   size. Backend bytes and time at open, and backend bytes per commit, are
   measured against database size at three or more sizes in release builds,
   with quick repair on and off, and their growth rates reported. No claim of
   constant-time recovery is made from quick repair alone.

Further gates:

4. **Immediate durability.** A commit that returned `Committed` survives a
   power cut taken at that instant, and a process kill.
5. **Historical objects.** Every object of every committed block reads back
   unchanged after a reopen with a tiny cache, and while later blocks commit;
   no redb savepoint is created or restored to answer a historical read.
6. **Lost-acknowledgement reconciliation.** After an `Indeterminate` commit,
   the state read back after `recover()` decides it: the head is the
   candidate with identical records (done) or the parent (the identical
   batch then commits exactly once). An indeterminate commit that did commit
   must occur in the tests and reconcile as committed. `NotCommitted` must
   never turn out committed.
7. **Failed-handle latching.** After an I/O failure every call returns the
   failure (never "absent") until `recover()` succeeds; `recover()` waits for
   reads in flight, is idempotent and can be retried.
8. **Disk full.** Running out of space while committing gives a
   `Full`-kinded outcome. A `NotCommitted(Full)` batch is never applied; an
   `Indeterminate(Full)` batch is decided by reconciliation after
   `recover()`; no batch is applied twice, and the identical batch commits
   exactly once when space returns.
9. **Write amplification.** Backend bytes written per logical byte, syncs,
   file growth and commit latency are measured for few large against many
   small blocks at equal volume. This gate records numbers; it has no
   threshold, because no target was set.

Named fallbacks, if a gate fails: atomicity or durability — do not adopt
redb; evaluate another engine or a focused fork of redb's commit path.
Isolation — narrow the claim to "logical isolation; physical O(log n)" with
the measured bound, or change the layout (for example one file or one redb
database per namespace group). Recovery — keep quick repair and bound the
allocator state, or change engines; never claim constant recovery.

## What was built

| Component | Crate | Role |
| --- | --- | --- |
| `versioned` contract | `arkiv-db-storage` | `DatabaseId`, `SpaceId`, `ObjectId`, `Head`, `Table`, `CommitBatch`, `CommitOutcome`, `Health`, `ObjectReader`, `VersionedStore`, `StorageError`, `StoreStats`, `reconcile` |
| `testing::FaultyStore` | `arkiv-db-storage` | Fault injection over any store, with a latching mode that follows `RedbStore`'s latching rule |
| `testing::contract` | `arkiv-db-storage` | The behavioural suite every store runs (27 cases) |
| `MemoryVersionedStore` | `arkiv-db-memory` | Volatile reference implementation |
| `RedbStore` | `arkiv-db-redb` | The contract over one redb file, created under a temporary name and renamed into place |
| `CountingBackend` | `arkiv-db-redb` | redb backend wrapper counting I/O, forwarding every method including locks, with a kill switch |
| `testing::CrashDisk` | `arkiv-db-redb` | Simulated device with a power-loss and torn-write model, fault injection (including a failed sync that loses its writes), and length and allocation quotas |

### Contract semantics

A commit applies one `CommitBatch` atomically: a compare-and-set of the head
(`HeadMismatch` otherwise), objects put if absent per `(SpaceId, ObjectId)`
(identical bytes are idempotent, different bytes are `Conflict`), insert-only
records with a fixed key shape per table (`Incompatible` otherwise), and the
new head, whose height must exceed the expected head's. The outcome is
`Committed` (applied and durable), `NotCommitted` (nothing applied) or
`Indeterminate` (all or nothing applied; only durable state can tell). The
store never resolves an indeterminate commit; callers use `reconcile`, which
calls `recover()` and compares the head and the batch's records read through
the fresh handle. A redb store becomes `Failed` after an `Io` or `Full`
failure of any call, after any error of redb's `commit()` (an `Indeterminate`
outcome, whatever its kind: redb refuses further writes on that handle) and
after a failed `recover()`, whose error replaces the earlier one; every call,
reads included, then returns the latched error until `recover()` reopens the
file. Other failures — a damaged page or object found by a read, a conflict,
a head mismatch, a malformed batch — are reported on the call and do not
latch. `FaultyStore`'s latching mode follows the same rule (it can also
latch `Corrupt`, which the contract allows), and the contract suite pins the
failed-recovery rule for every store. A missing referenced object is
`Missing`; stored bytes that do not hash to their id, and a missing catalog,
host or record table, are `Corrupt`. The full text is in the rustdoc of
`arkiv_db_storage::versioned`.

### redb layout and settings

One file. Tables: `meta` (`&str` → bytes: `store/format`, `store/database_id`,
`store/head`, and caller metadata such as genesis identity and versions),
`blocks` and `manifests` (`u64` → bytes), `inclusions` (bytes → bytes),
`block_index` (`[u8; 32]` → `u64`), `objects_catalog`, `objects_host`, and
one `objects_ns_<16 hex digits>` table per namespace (`[u8; 32]` → bytes).
Every commit is one `WriteTransaction` with `Durability::Immediate` and
`set_quick_repair(true)`, including the store's initialization commit. A
new store file is built as `<file>.init` — redb's creation and the
initialization commit, both synced — then renamed to its name, and the
directory is synced; the temporary file is locked (`flock`) throughout, so
concurrent `create`s of one path cannot both initialize it. redb writes its
magic number last when it creates a file, and a non-empty file without it is
refused, so creating in place (as `RedbStore::with_backend` does on an empty
backend) is not crash-safe. Every read opens a fresh `ReadTransaction`. The `Database` lives in a
`RwLock<Option<Database>>` slot with an epoch; `recover()` takes the writer
lock and the slot exclusively (waiting for reads in flight), drops the
handle, reopens and checks the database identity. Object reads verify
`keccak256(bytes) == id` (`alloy-primitives`). The redb page cache is set
explicitly: 8 MiB by default (`RedbConfig::cache_size`; redb's default is
1 GiB). No savepoint is ever created.

### Dependency review

`redb = "=4.3.0"`, `default-features = false, features = ["std"]` (the
same code as redb's default feature set; `std` is the only default feature).
`cargo tree -p arkiv-db-redb -e normal` on 2026-09-23:

```text
arkiv-db-redb v0.1.0
├── alloy-primitives v1.7.3   (already a workspace dependency)
├── arkiv-db-storage v0.1.0
└── redb v4.3.0
    └── libc v0.2.189         (Linux and Apple targets only)
```

`Cargo.lock` pins redb 4.3.0 by checksum
`fb338a6c67830a61bed824b78c2bb034ab1ff401a20616bd7957524f4fdc2f22`. redb's
build script only declares `cfg`s (`redb_no_std` for the redb 5 preview
without `std`, `fuzzing`). redb itself contains `unsafe` code (its lock
wrappers, `fcntl` byte-range locks through `libc`, and a hash); the
workspace's `unsafe_code = "forbid"` covers our crates only. The disk adapter
is therefore not standard-library-only; the contract crate and the memory
store still are.

redb 4.3.0 declares `rust-version = "1.90"`. The workspace `rust-version` is
now 1.90 (was 1.88), and `cargo +1.90.0 check --workspace --all-targets
--offline` passes. The test profile optimizes `alloy-primitives`, `sha3`,
`keccak` and `redb` (`[profile.dev.package.*] opt-level = 3`): the crash
tests reopen and hash-verify thousands of images, and unoptimized Keccak
made them about 40× slower. redb's `debug_assertions` stay on in tests.

### Method

- **Power-loss model** (`CrashDisk`): the device keeps a durable image and an
  ordered log of unsynced writes and length changes; a successful sync makes
  the log durable. A crash first cuts the device off (every later call fails,
  a sync persists nothing), then drops the store, so redb's clean-close path
  cannot write. The crash image is the durable image plus none, all, or a
  seeded subset of the logged operations applied in log order, each write
  whole, dropped, or torn at 512-byte-sector or byte granularity; bytes
  outside persisted units keep their old value. It does not model firmware
  that acknowledges a sync without persisting, reordering across a completed
  sync, or bit rot.
- **Crash points**: a commit is cut off at its k-th mutating backend call,
  for k from 0 to the number of calls a complete commit makes (the last
  point lets it complete). Each crash image is checked with redb's
  `check_integrity` and reopened as a new store. Store creation is cut off
  the same way.
- **Space models** (`CrashDisk`): a *length quota* fails a write or length
  change ending beyond it. redb grows its file with `set_len` before its
  `commit()`, so under this model every out-of-space failure happens before
  the commit — unlike ext4, where extending a file with `ftruncate` makes it
  sparse and allocates nothing. An *allocation quota* models sparse files
  with allocation on write: length changes are free and a write fails if it
  would allocate 4 KiB blocks beyond the quota, so writes inside `commit()`
  can fail too.
- **Lost sync**: a sync can fail and lose the unsynced writes while reads
  keep returning them, as Linux can after an `fsync` error (it may mark the
  pages it could not write clean).
- **Process kill** on real files: `IoProbe::kill` fails every later backend
  call except closing and unlocking, then the store is dropped. Everything
  written before reached the operating system, as with `SIGKILL`.
- **Backend counters** (`CountingBackend`): reads, writes, bytes, syncs and
  length changes below redb's page cache. Application counters come from
  `StoreStats` per space.
- **Real disk**: measurement files live under `ARKIV_BENCH_DIR`, Cargo's
  target temporary directory for tests, or `<workspace>/target/bench` for the
  example — ext4 on this machine. `std::env::temp_dir()` is a RAM tmpfs here,
  where `fsync` does nothing, and is never used.
- **Profiles**: debug builds of redb walk every page on open (a
  `debug_assertions` check), so open I/O and cold-cache numbers come from
  release builds only. Times are wall-clock with the operating system's page
  cache warm (dropping it needs root); backend byte counts do not depend on
  it.

## Results

All numbers below come from one release run of the experiment example and
from the tests, on 2026-09-23:

- command: `cargo run --release -p arkiv-db-redb --example redb_experiment`
  (release profile, rustc 1.96.0; workspace MSRV 1.90 checked separately),
  run at 21:00 UTC, 10 min 50 s wall clock, 116 MB peak RSS. Its byte and
  page counts are identical to those of the first run (20:09 UTC) that this
  document reported before review; timings differ slightly;
- tests: `cargo test -p arkiv-db-redb --offline` (debug profile), heavy
  variants `cargo test --release -p arkiv-db-redb --test atomicity -- --ignored`
  (3 tests, passed in 785 s);
- machine: 13th Gen Intel Core i9-13900 (32 hardware threads), 62 GiB RAM,
  Linux 7.0.0-31-generic, files on ext4 over an NVMe RAID1 array
  (`<workspace>/target/bench`). `fsync` on this device completes in well
  under a millisecond, so absolute latencies here are not representative of
  slower disks.

### Gate 1: atomicity — pass (under the power-loss model)

| Run | Crash points | Calls per commit | Syncs | Length changes | Images | Parent | Candidate | Full repairs | Committed / Indeterminate / NotCommitted |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| Ten tables after a restart, quick repair, synced-only + all + 64 sector-torn + 64 byte-torn | 50 | 49 | 2 | 0 | 6,500 | 6,301 | 199 | 0 | 1 / 49 / 0 |
| Ten tables after a restart, no quick repair, 2 + 16 torn modes | 45 | 44 | 1 | 1 | 810 | 773 | 37 | 810 | 1 / 44 / 0 |
| Ten tables in steady state, quick repair, 2 + 128 torn modes | 55 | 54 | 2 | 0 | 7,150 | 6,951 | 199 | 0 | 1 / 54 / 0 |
| Commit larger than the cache that grows the file, after a restart, every crash point, synced-only + all + 1 torn | 2,323 | 2,322 | 3 | 1 | 6,969 | 6,965 | 4 | 0 | 1 / 284 / 2,038 |
| The same commit in steady state, every crash point, 2 + 1 torn | 2,349 | 2,348 | 4 | 2 | 7,047 | 7,043 | 4 | 0 | 1 / 289 / 2,059 |

"After a restart": three parent blocks were committed, the device crashed,
and the candidate is the first commit after reopening. "Steady state":
twelve parent blocks were committed through the same store just before the
candidate, so earlier commits had freed pages for reuse and redb's
allocator state was live rather than freshly loaded.

Every one of the 28,476 crash images passed redb's `check_integrity` (page
checksums, and an allocator state matching the pages reachable from the
roots) and reopened to exactly the parent or exactly the candidate, with all
earlier blocks intact; `reconcile` agreed each time, and an absent candidate
then committed exactly once. One more block then committed on top of each
image: every block read back, and after a power cut following that commit
the image passed the integrity check again and still held every block. The
ten-table commit makes all its backend writes inside redb's `commit()`, so a
crash there is `Indeterminate`; the larger commit spills pages before
`commit()`, and a crash there is `NotCommitted` and always left the parent.
With quick repair no crash ever needed redb's full repair; without it every
reopen after a crash did. Tests: `atomicity.rs` (all crash points of the
ten-table commit after a restart and in steady state with 18 images each,
sampled crash points of the large commit both ways, in the default run; its
three `#[ignore]`d heavy tests — 130 images per crash point of the
ten-table commit after a restart and in steady state, with and without
quick repair, and every crash point of the large commit both ways with four
images each — passed in 785 s in release), example Gate 1 (the table
above).

### Gate 2: isolation — pass, with the physical claim narrowed to O(log namespaces)

A holds 1,000 objects of 256 bytes and is frozen. Each phase reopens the
file with a zero-byte redb cache and reads all of A ("uncached"), then with a
256 KiB cache reads A twice ("cold", "warm").

| Phase | B objects | Namespaces | Blocks | File MB | Tables | Directory height | A height | A's reads (hits, bytes) | Other spaces read | Uncached page reads | Bound | Cold | Warm |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- | ---: | ---: | ---: | ---: | ---: |
| baseline | 1,000 | 2 | 1 | 0.9 | 9 | 1 | 3 | 1,000 (1,000, 256,000) | 0 | 4,000 | 4,000 | 492 | 434 |
| B ×10 | 10,000 | 2 | 4 | 14.3 | 9 | 1 | 3 | 1,000 (1,000, 256,000) | 0 | 4,000 | 4,000 | 500 | 443 |
| B ×100 | 100,000 | 2 | 27 | 57.3 | 9 | 1 | 3 | 1,000 (1,000, 256,000) | 0 | 4,000 | 4,000 | 500 | 443 |
| 1,000 namespaces | 100,000 | 1,000 | 28 | 57.3 | 1,007 | 2 | 3 | 1,000 (1,000, 256,000) | 0 | 5,000 | 5,000 | 511 | 455 |
| 10,000 namespaces | 100,000 | 10,000 | 33 | 114.6 | 10,007 | 3 | 3 | 1,000 (1,000, 256,000) | 0 | 6,000 | 6,000 | 524 | 470 |
| +1,000 blocks | 104,000 | 10,000 | 1,033 | 114.6 | 10,007 | 3 | 3 | 1,000 (1,000, 256,000) | 0 | 6,000 | 6,000 | 516 | 461 |

- A's application-level reads are identical in every phase and touch no
  other space.
- Uncached, every object read costs exactly `directory height + A's height`
  page reads (4 KiB each): the bound derived from B-tree depth holds with
  equality in every phase.
- B's growth to 100× and 1,000 more blocks of history change neither term:
  A's uncached page reads are identical.
- The namespace count is not free: redb finds a table through its table
  directory, a B-tree keyed by table name, which grew from one to three
  levels between 9 and 10,007 tables, adding one page read per object read
  per level. The physical claim is therefore **logical isolation; physical
  cost O(log number of namespaces) through the table directory**, not
  constant. It does not grow with other namespaces' sizes or with history.
- The small-cache passes show the cache's effect (A's 290 KB table does not
  fit 256 KiB, so the warm pass still reads most pages); they are reported,
  not asserted.

Tests: `isolation.rs` (200 objects, B to 100×, 400 namespaces, 100 blocks,
asserting the exact uncached bound), example Gate 2.

### Gate 3: recovery — pass on the criterion; backend cost grows with size

Blocks of 256 objects of 1 KiB were committed until the file reached each
size, then 16 small commits (16 objects of 256 bytes plus records) were
measured, then the handle was killed (no clean close; nothing more reaches
the file, so its size at the reopen is the size shown) and the file
reopened with a 1 MiB cache. "App backend" is the backend I/O of `head()`
and the manifest read right after the reopen.

| File MB | Blocks | Quick repair | Full repair at open | Open ms | Open backend reads | Open read MB | Open read / file | App reads at open | App backend reads (KB) | Write KB per small commit | Reads per commit | Syncs per commit | Commit p50 ms |
| ---: | ---: | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 16.8 | 32 | on | no | 0.093 | 8 | 0.025 | 0.001 | 4 | 2 (8.2) | 140.4 | 9 | 2 | 0.249 |
| 16.8 | 32 | off | yes | 3.547 | 3,634 | 14.877 | 0.88 | 4 | 2 (8.2) | 127.0 | 9 | 1 | 0.191 |
| 67.4 | 87 | on | no | 0.101 | 8 | 0.029 | 0.0004 | 4 | 2 (8.2) | 181.9 | 17 | 2 | 0.309 |
| 67.4 | 87 | off | yes | 15.322 | 15,868 | 64.987 | 0.96 | 4 | 2 (8.2) | 163.1 | 16 | 1 | 0.225 |
| 269.5 | 306 | on | no | 0.101 | 8 | 0.054 | 0.0002 | 4 | 2 (8.2) | 253.8 | 29 | 2 | 0.376 |
| 269.5 | 306 | off | yes | 65.217 | 64,934 | 265.962 | 0.99 | 4 | 2 (8.2) | 211.0 | 28 | 1 | 0.283 |
| 1,077.9 | 1,187 | on | no | 0.116 | 8 | 0.152 | 0.0001 | 4 | 2 (8.2) | 402.0 | 44 | 2 | 0.529 |
| 808.5 | 1,187 | off | yes | 271.929 | 262,026 | 1,073.251 | 1.33 | 4 | 2 (8.2) | 254.8 | 40 | 1 | 0.333 |

The two settings do not end at the same file size at the largest target:
both grew past 1 GiB, and during the 16 measured commits the quick-repair-off
file shrank to 808.5 MB (redb trims free space at the end of the file),
while the quick-repair-on file stayed at 1,077.9 MB. Growth rates below are
therefore stated per setting.

- **Application reads at open: four at every size, by construction.** The
  store reads two metadata entries while opening, and the application then
  reads `head()` and the manifest at the head; none of these reads depends
  on the database's contents, so the count is fixed by the code rather than
  measured. The tests assert it (`recovery.rs`) to catch any traversal. Their
  physical cost was measured: `head()` and the manifest cost 2 backend page
  reads (8.2 KB) at every size, through the cache the open had warmed.
- **Quick repair on (the store's setting).** The reopen after a kill loads
  redb's saved allocator state instead of walking the file: 8 backend reads
  at every size, but the bytes grow with the file — 25 KB at 16.8 MB, 29 KB
  at 67.4 MB, 54 KB at 269.5 MB, 152 KB at 1,077.9 MB (6× the bytes for 64×
  the size; about 2.8× for the last 4× step). Open time stayed near 0.1 ms
  with a warm operating-system page cache.
- **Quick repair off.** Every reopen after a kill runs the full repair,
  which read 0.88×, 0.96× and 0.99× the file length at 16.8, 67.4 and
  269.5 MB, and 1.33× at 808.5 MB (14.9 MB to 1,073 MB; 3.5 ms to 272 ms):
  at least linear in file size. Reading more bytes than the file holds means
  pages were read more than once: redb's repair walks the trees in several
  passes (its source describes three scans), and a 1 MiB cache cannot hold
  them. Why the re-reads appear only at the largest size was not
  investigated.
- **Per-commit cost grows with the database either way.** Bytes written per
  small commit rose from 140 to 402 KB with quick repair (16.8 → 1,077.9 MB)
  and from 127 to 255 KB without (16.8 → 808.5 MB); at equal sizes quick
  repair wrote 10–20% more (for example 253.8 against 211.0 KB at 269.5 MB)
  and synced twice instead of once. Each commit rewrites the whole allocator
  state (which grows with the file's regions) and copies deeper B-tree
  paths.
- Recovery is therefore not constant-time in backend bytes: with quick
  repair it is small and sublinear at these sizes, and it grows with file
  size.

Tests: `recovery.rs` (never a full repair with quick repair and exactly four
application reads at 1, 4 and 12 MiB; always a full repair without),
`durability.rs`, example Gate 3.

### Gate 4: immediate durability — pass (under the power-loss model)

After each of 50 commits on a `CrashDisk`, the image a power cut would leave
at that instant (synced data only) reopened with that commit and all earlier
ones: 50 of 50. Commits sync twice with quick repair (redb's two-phase
commit), once without; the store's initialization commit is durable too
once `create` has returned. On a real file, a process kill right after
`Committed` lost nothing and needed no full repair.

Creating a store is crash-safe on a path, not in place. Cut off at each of
its 23 mutating calls, an in-place creation (`with_backend` on an empty
`CrashDisk`) left 101 distinct crash images in the release run (104 in the
debug test run; the count varies because each creation writes a random
database id): reopened in place, 91 initialized a store, 2 opened a
complete one and 8 were refused as `Incompatible`, because redb writes its
magic number last. `RedbStore::create` builds the store in `<file>.init`
and renames it into place only after the initialization commit, then syncs
the directory; each of the 101 images, left behind as that temporary file
(with the target absent, or empty), was discarded by the next `create`,
which built a fresh store and left no temporary file. Two concurrent
`create`s of one path created exactly one store in each of 16 rounds; the
other was `Locked`. Tests: `durability.rs`, `initialization.rs`,
`atomicity.rs` (a `Committed` outcome survived the synced-only image at the
last crash point of every run), example.

### Gate 5: historical objects — pass

While 399 blocks committed (0.86 s), four readers verified 429,470 objects of
randomly chosen committed blocks; every one matched. After a reopen with a
zero-byte cache, all 23,990 objects of all 400 blocks read back identical.
The store never creates a redb savepoint (checked with redb's own savepoint
listing); history is ordinary immutable entries of the current tables.
Tests: `history.rs`, the contract's `history_stays_readable`,
`concurrent_reads_during_commits` and `reopen_preserves_state`, example.

### Gate 6: lost-acknowledgement reconciliation — pass

An I/O error injected at each of the 30 mutating calls of a commit (the
device keeps what it was given, as a page cache does when the process
survives) latched the store every time; after `recover()`, 29 reconciled as
absent (the identical batch then committed once, and a second retry was
refused by the head check) and 1 reconciled as committed: the final sync
failed after redb had written its commit record. The same failure followed by
a power cut instead reconciles as absent, which is why an indeterminate
commit can only be decided from durable state. A commit whose
acknowledgement was lost with the process reconciles as committed after
restart. `NotCommitted` never turned out committed in any test. After a
committed reconciliation of a failed final sync, a power cut kept the block
(the model's failed sync keeps its writes, and the reopen synced them).

Reading a commit back is not by itself proof that it is durable. When the
final sync fails and loses its writes while reads still return them (as
Linux can after an `fsync` error), reconciliation still reads the batch
back as committed. With quick repair (two-phase commit, the store's
setting) a power cut afterwards kept the block: the data pages had been
synced before the header was written, and `recover()`'s reopen rewrote and
synced the header. Without quick repair the lost sync also held the data
pages; redb's repair at reopen committed a header naming them, and after a
power cut redb refused the file ("Primary is corrupted despite 2-phase
commit"). Tests: `reconciliation.rs`, `atomicity.rs`, `disk_full.rs`, the
contract's `faulty_*` cases, example.

### Gate 7: failed-handle latching and `recover()` — pass

After one injected read error every call (object, record and head reads,
commits) returned the same `Io` error until `recover()`, which reopened the
device in 0.041 ms and returned the head. While the device keeps
failing, `recover()` fails and leaves the store `Failed` with the
recovery's own error, which every call then returns; it succeeds once the
device works, is idempotent, and waits for a read held inside the backend.
A write error inside a commit latches too. Object bytes damaged on the
medium read as `Corrupt` without latching, and a missing catalog, host or
record table is `Corrupt`, never "absent". `FaultyStore`'s latching mode
follows the same rule, checked by the contract cases
`faulty_latching_kinds_follow_the_redb_rule` and
`faulty_failed_recovery_is_retryable` over every store. Tests:
`latching.rs`, the contract's `faulty_*` cases (also over the memory store),
example.

### Gate 8: disk full — pass (under two quota models)

The candidate needs more space than the parent. Quotas were set at 24 even
steps between the space the parent uses and the space the committed
candidate needs, and at the last 8 blocks below the need.

| Space model | Quotas | Range (bytes) | `NotCommitted(Full)` | `Indeterminate(Full)` | Committed |
| --- | ---: | --- | ---: | ---: | ---: |
| Length quota (a write or length change ending beyond it fails) | 32 | 1,056,768 – 2,109,440 | 32 | 0 | 0 |
| Allocation quota (length free, writes allocate 4 KiB blocks) | 32 | 1,056,768 – 1,601,536 | 23 | 9 | 0 |

Under the length quota every failure came before redb's `commit()`, because
redb grows the file with `set_len` first and the model charges there; on
ext4, extending a file that way is sparse and never fails, so that model
alone cannot produce an indeterminate out-of-space commit. The allocation
quota charges writes, and failures of writes inside `commit()` gave
`Indeterminate(Full)`. Every failure under either model latched the store
`Failed(Full)`, reconciled as absent after `recover()` while still full,
failed again on retry with `Full`, left the parent after a power cut, and
committed exactly once after the quota was lifted. `StorageFull` injected at
each of the 34 mutating calls of a commit (as a file system can report it
from `fsync`) was always `Full`-kinded; 33 reconciled as absent, and 1 — the
final sync failing after the commit record was written — was an
`Indeterminate(Full)` that did commit and reconciled as committed. Tests:
`disk_full.rs`, the contract's `faulty_disk_full_then_retry_commits_once`
(both outcomes through `FaultyStore`), example.

### Gate 9: write amplification — measured

16,384 objects of 256 bytes over four namespaces; every block also carries a
block, manifest, index and inclusion record and the head.

| Objects per block | Quick repair | Blocks | Logical MB | Written MB | Written / logical | Writes | Syncs | File MB | Total s | p50 ms | p99 ms |
| ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | on | 16,384 | 12.812 | 1,444.91 | 112.8 | 382,970 | 32,773 | 19.01 | 3.41 | 0.194 | 0.356 |
| 16 | on | 1,024 | 5.224 | 193.81 | 37.1 | 49,206 | 2,052 | 9.83 | 0.38 | 0.380 | 0.589 |
| 256 | on | 64 | 4.750 | 66.63 | 14.0 | 16,384 | 131 | 8.43 | 0.10 | 1.550 | 1.865 |
| 4,096 | on | 4 | 4.721 | 26.16 | 5.5 | 6,393 | 12 | 16.83 | 0.04 | 11.738 | 13.999 |
| 16,384 | on | 1 | 4.719 | 13.46 | 2.9 | 3,289 | 5 | 8.43 | 0.03 | 29.610 | 29.610 |
| records only | on | 1,024 | 0.506 | 66.58 | 131.6 | 18,144 | 2,050 | 0.69 | 0.16 | 0.152 | 0.241 |
| 16 | off | 1,024 | 5.224 | 180.90 | 34.6 | 45,109 | 1,028 | 9.64 | 0.30 | 0.303 | 0.441 |
| 16,384 | off | 1 | 4.719 | 13.42 | 2.8 | 3,278 | 4 | 8.43 | 0.03 | 27.044 | 27.044 |

A commit has a fixed backend cost of about 65 KB written and two syncs even
with no objects (redb's header twice, the allocator state, the table
directory and each touched table's path), so many small blocks cost far more
than few large ones: 1,445 MB written for 16,384 one-object blocks against
13 MB for one block with the same objects. "Logical" counts object and record
bytes, so it also grows with the number of blocks. Quick repair adds about
7% bytes and one sync per commit at 16 objects per block. Tests:
`write_amplification.rs` (two syncs per commit and strictly falling
amplification at debug scale), example Gate 9.

## Verdict

| Gate | Verdict | Claim this evidence supports |
| --- | --- | --- |
| 1. Atomicity | Pass | Under the power-loss model (synced data durable, any subset of unsynced writes, torn at sector or byte granularity), every crash point of a block commit — the first after a restart or one in steady state — leaves the parent or the candidate, an image redb's integrity check accepts, and a store that keeps every block through the next commit |
| 2. Isolation | Pass, narrowed | Logical isolation; physical page reads per object read = table-directory height + the namespace's own table height, independent of other namespaces' sizes and of history, O(log namespaces) in the namespace count |
| 3. Recovery | Pass on the criterion | Application reads at open are four by construction (no traversal); backend bytes at open (quick repair) and per commit grow with file size, as measured; no constant-time claim |
| 4. Immediate durability | Pass | `Committed` survives a power cut at that instant (model) and a process kill (real file); creating a store on a path survives a crash at any point (in place it does not) |
| 5. Historical objects | Pass | Old objects stay readable after reopen with a tiny cache and during commits, without savepoints |
| 6. Reconciliation | Pass | Indeterminate commits are decided from state read through a fresh handle; nothing is applied twice. A commit read back after a failed sync is durable in the model only with quick repair on |
| 7. Latching | Pass | I/O failures, failed commits and failed recoveries are errors on every call until `recover()` succeeds; never "absent"; `FaultyStore` latches by the same rule |
| 8. Disk full | Pass | Under both quota models and injected `ENOSPC`: always a `Full` outcome; `NotCommitted(Full)` never applied; `Indeterminate(Full)` decided by reconciliation; never applied twice; the identical batch commits exactly once when space returns |
| 9. Write amplification | Measured | Fixed per-commit cost of about 65 KB and two syncs dominates small blocks |

redb 4.3.0 with this layout is adopted as the storage engine for N3 and N4.
No fallback is needed for gates 1 and 3–8; gate 2 uses its named narrower
claim.

## Remaining limitations

- **Table directory.** Every object read descends redb's table directory,
  shared by all namespaces; its height grows logarithmically with the number
  of namespace tables (1 → 3 levels from 9 to 10,007 tables). Tables also
  cost file space: adding 9,000 namespaces of two objects each grew the file
  from 57 MB to 115 MB (redb grows files in large steps, so this is an upper
  bound on their pages).
- **Global catalog object table.** `objects_catalog` holds the catalog
  (directory) nodes of every namespace and every block, so its B-tree height
  grows with total catalog history. N2 does not measure it because the
  engine does not write it yet; N3 must measure directory reads through it.
- **Allocator state.** With quick repair, every commit rewrites redb's whole
  allocator state and every open after a crash loads it; both grow with the
  file (152 KB read at open at 1.08 GB). Recovery is not constant in bytes.
- **Fixed per-commit cost.** About 65 KB written and two syncs per commit
  even for an empty block; mostly-empty blocks at a fixed interval write a
  steady stream (N4 must size block intervals and measure it).
- **Silent corruption.** redb verifies page checksums only during repair and
  integrity checks, not on normal reads (its B-tree lookup in
  `tree_store/btree.rs` descends pages without checking them). Object
  values are verified against their content hash, but a damaged B-tree page
  or record value can go undetected, a damaged page could make a present key
  read as absent, and a damaged page header can make redb panic (an
  `unreachable!` on an unknown page type) instead of returning an error.
  Only damage inside object values and missing store tables (read as
  `Corrupt`) were tested. Mitigations for later steps:
  redb's `check_integrity` as an explicit maintenance operation, and hash
  checks of blocks and manifests at the node level.
- **Model limits.** Crash results hold for the `CrashDisk` model; firmware
  that acknowledges a sync without persisting, reordering across a completed
  sync and bit rot are not modelled. Real-file kills are process kills, not
  power cuts. Disk full is two quota models plus injected `StorageFull`; a
  real file system running out of space was not exercised. The length quota
  charges at file growth, which ext4 does not; the allocation quota is closer
  to ext4 but ignores metadata blocks and delayed-allocation reservations.
- **Failed syncs on Linux.** After an `fsync` error Linux can mark the pages
  it could not write clean: they keep serving reads but never reach the
  medium, and later syncs succeed without writing them. Reading a commit
  back after its sync failed is then not proof that it is durable. In the
  model (a sync that fails and loses its writes), the store's two-phase
  commit makes the reconciled block durable, because only the header was
  lost and `recover()`'s reopen rewrites and syncs it; with quick repair off
  the same sequence leaves a file redb refuses after a power cut. This
  relies on redb 4.3.0 rewriting its header at reopen, and does not cover
  other kernels or page-cache eviction between the failure and the reopen.
  Quick repair must stay on; the node rules are in the
  [plan](node-storage-plan.md#atomic-durable-publication-and-restart).
- **Store creation.** `RedbStore::create` is crash-safe because the store
  appears under its name only by a rename after the initialization commit,
  followed by a directory sync; the rename's atomicity is the file system's,
  and on non-Unix platforms the directory is not synced. The test leaves
  every crash image of an in-place creation as the temporary file; it cannot
  cut power in the middle of a real rename. `RedbStore::with_backend`
  creates in place and is not crash-safe while it initializes an empty
  backend.
- **Measurements.** One machine with very fast `fsync`; wall-clock times with
  a warm operating-system page cache; open I/O only meaningful in release
  builds (debug redb walks the file on open).
- **Not covered here.** No engine persistence, roots, manifests or node
  integration (N3, N4); no compaction or pruning (the file only shrinks by
  trailing free space); redb's own `unsafe` code and `libc` dependency are
  outside the workspace's `unsafe_code = "forbid"`.

## Reproduce

```sh
cargo fetch
cargo test -p arkiv-db-redb --offline
cargo test --release -p arkiv-db-redb --offline --test atomicity -- --ignored
cargo run --release -p arkiv-db-redb --offline --example redb_experiment
cargo run --release -p arkiv-db-redb --offline --example redb_experiment -- --quick
cargo +1.90.0 check --workspace --all-targets --offline
```

`ARKIV_BENCH_DIR` moves the measurement files; it must be on a real disk.
