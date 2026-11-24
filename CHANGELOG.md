# Changelog

## Unreleased

### Full followers sync from a public block endpoint, without a token

Steps 4 and 5 of the
[snapshot sync proposal](docs/proposals/2026-11-24-snapshot-sync.md).
Nothing stored and nothing in block validity changes: consensus, blocks,
headers, transactions, signatures, genesis, state roots and the `node.redb`
and `history.redb` layouts are as in 0.1.0-alpha.7, and this build opens an
alpha.7 run directory in place with no rebuild or migration.
`scripts/upgrade-smoke.py` passed 0.1.0-alpha.7 -> this build, followers
and light clients both ways and rollback included.

- Public `GET /signed/v1/blocks?afterHeight=H&afterHash=X&limit=K` on
  producer and full nodes answers consecutive canonical blocks after `H`,
  each with its certificate, and the sampled signed head. A page holds at
  most 64 blocks and about 3 MiB of block bytes (the first block is always
  returned). Without `afterHeight` it starts at genesis; a wrong or missing
  `afterHash` is `ChainConflict`; an expired body is `HistoryPruned` (410).
  The blocks are read through the height index of `history.redb` and
  returned as stored. They hold nothing JSON-RPC does not already serve.
- A `full` node without `--replication-peer` syncs from that endpoint, up
  to 64 blocks per 50 ms poll, and needs no `ARKIV_PROPOSER_TOKEN`. It
  authenticates every header against the pinned proposer and re-executes
  every block, as before. With `--replication-peer` and the token it syncs
  as alpha.7 does, one block per poll from the private listener.
- Peer URLs (`--peer`, `--replication-peer`) may carry a path prefix where
  a reverse proxy mounts the node: `https://host/rpc`. Credentials, a query
  and a fragment are refused, as before.
- `--allow-http-peers true|false` (default `false`) on `full` and `light`
  accepts `http://` peers beyond loopback, for a private network or TLS
  that ends elsewhere. What is synced is verified the same over either
  transport; a replication token sent over plain HTTP crosses the network
  in clear text.
- **Compatibility:** a peer must run this build or later to serve
  `GET blocks`. A follower of this build started with `--replication-peer`
  and the token still syncs from older peers, and older followers sync
  from this build unchanged.
- Local check on 2026-11-24 (250 ms blocks, debug build): a follower
  started on a copy of the producer's `node.redb` and `history.redb`, with
  no token, caught up 412 blocks in under 11 s and ended on the producer's
  head (height, hash, state root).

## 0.1.0-alpha.7 — 2026-11-23

Alpha pre-release. **A drop-in upgrade of 0.1.0-alpha.6** (and of
0.1.0-alpha.5): consensus, blocks, headers, transactions, signatures,
genesis, state roots and the `node.redb` and `history.redb` layouts are
unchanged, and an alpha.7 node opens an alpha.6 run directory in place
with no rebuild or migration. `scripts/upgrade-smoke.py` passed
0.1.0-alpha.6 -> 0.1.0-alpha.7, followers and light clients both ways and
rollback included. **One intended behaviour change:** an `arkiv_query`
`offset` above 100,000 is refused (below).

### A node-local offset limit: `--read-max-offset` (default 100,000)

- Live testing of 0.1.0-alpha.6 on the Sourcify mirror: an offset walk
  passes about 15,000 matches a second, so an offset of 300,000 took 19.8 s,
  400,000 reached the 30-second deadline, and every offset from about
  450,000 up to the budget's cut-off of 1,250,000 held a read slot for the
  whole 30 s and then failed with `ReadDeadlineExceeded`.
- `arkiv_query` with an `offset` above `--read-max-offset` (all node roles
  that serve reads; default 100,000, about 7 s of walk there) is refused at
  once with `-32004` (`limits`), when the read is planned and before any
  state is read: `offset N exceeds this node's limit of M
  (--read-max-offset): page with the cursor of the previous page and its
  blockNumber as atBlock, or narrow the query`. An offset at the limit runs;
  an offset beside a `cursor` is still ignored, as before.
- `--read-max-offset 0` turns the limit off; the alpha.6 budget check stays
  the backstop either way. `ReadLimits::max_offset` (`None` for no limit)
  carries it.
- **The one intended behaviour change:** offsets between 100,000 and the
  budget's cut-off that used to succeed slowly (about 6 to 30 s on the
  mirror) are now refused. Clients page deeper with the cursor, or the
  operator raises or disables the limit.

### Upgrading

- Replace the binary or image (`ghcr.io/scx1332/arkiv-node:0.1.0-alpha.7`)
  and restart. Without new flags the node refuses offsets above 100,000;
  add `--read-max-offset N` to choose another limit, or
  `--read-max-offset 0` to keep alpha.6's behaviour.

## 0.1.0-alpha.6 — 2026-11-23

Alpha pre-release. **A drop-in upgrade of 0.1.0-alpha.5:** consensus,
blocks, headers, transactions, signatures, genesis, state roots and the
`node.redb` and `history.redb` layouts are unchanged. An alpha.6 node opens
an alpha.5 run directory in place and continues its chain, with no rebuild
or migration; alpha.5 and alpha.6 producers, full followers and light
clients accept each other's blocks; alpha.5 reopens a directory alpha.6
wrote. `scripts/upgrade-smoke.py` checks all of this and passed against
0.1.0-alpha.5. Queries that answered before answer the same; the new
refusals replace reads that ended in the deadline or an exhausted budget,
and two reads that were refused below the record floor now answer.

### Expensive reads fail fast instead of running into the read deadline

- On the 16.8-million-entity Sourcify node, `arkiv_query` with `"*"` and
  `offset` 10,000,000, and `arkiv.schema = str('sourcify.v4.contract') ORDER
  BY matchid:u64 DESC` (9.26 million matches), each held a read slot for the
  whole 30 s deadline and answered `ReadDeadlineExceeded`. `matchid >
  u64(59505252) AND arkiv.schema = str('sourcify.v4.contract') ORDER BY
  matchid:u64 DESC` (8.4 thousand matches) took 26.8 s.
- Causes: the 25-million-unit read budget allows far more than 30 s of work
  on a store that size, so the deadline ended these reads, not the budget.
  And offset and sort walks read engine pages of the genesis `max_page`
  (64), planning every group again for each page: the range above was walked
  and its 8.4 thousand postings positioned 133 times.
- **New refusal, `-32004` (`limits`):** an offset or `ORDER BY` walk the read
  budget cannot finish is refused before it starts, when a 20,000-unit count
  probe sizes the match set (stored posting sizes usually do). An offset that
  passes more than 1,250,000 matches (at least 20 units each on the GLM
  schedule) or a sort of more than 480,769 matches (52 units each) is
  refused. The message gives the numbers and suggests a cursor or a narrower
  query. Only reads that exhausted the budget or hit the deadline before are
  refused; an offset past the end of a small match set is still an empty page.
- Offset and sort walks, and the pages of a request, read engine pages of up
  to 16,384 rows (`ReadLimits::WALK_PAGE`, through the new read-side page
  bound `arkiv_db::QueryLimits::max_page`), so a group is planned once per
  16,384 matches. Rows, order, cursors and counts are unchanged; fewer units
  are charged. The 8.4-thousand-match sort plans its run once instead of 133
  times.
- A planned page that projects ids only (an offset's skip) no longer loads
  each row; its charges are unchanged.
- The deadline now also stops a planned count while it positions a range's
  postings, which no charge covers: before, a count over a run of millions of
  terms could keep its read slot for a minute or more after the deadline.
  Every phase of a walk now stops within a few object reads of its interrupt
  (tested at every third read of four walks).
- Not changed: a read the budget can finish runs as before. A walk of the row
  map (a match-all or negated query) costs about 67 µs a row on the Sourcify
  store, more than the budget assumes, so `"*"` with an offset between about
  450,000 and 1,250,000 still ends at the deadline
  (`docs/sdk-1.0-compatibility-todo.md`).

### Transactions below the record floor are served from the kept bodies

- With `bodyRetention: all` (the default `--body-retention archive`) every
  block body stays in `history.redb`, but since 0.1.0-alpha.3
  `eth_getTransactionByBlockNumberAndIndex` and
  `eth_getBlockTransactionCountByNumber` answered `-32006 history pruned`
  below the record floor. The docs disagreed: the API reference and the
  pruning design promised transactions at every height, the OpenAPI text and
  the record-expiry proposal expired them with the records. They now read
  the kept body (authenticated against its indexed certificate), as
  replication and `/headers` already did. With `--body-retention window`
  they are refused as before.
- Still refused below their floors, now documented as such: block JSON
  (`eth_getBlockByNumber`/`ByHash`, with or without whole transactions),
  whose `logsBloom` needs the state before the block, receipts and logs; the
  REST feed (`/feed/blocks/{h}`, 410), whose entries carry each block's state
  changes; request status. Hash lookups (`eth_getTransactionByHash`,
  `eth_getBlockByHash`, `eth_getTransactionReceipt`) cover the record window
  and answer `null` below it; `eth_feeHistory` starts at the record floor.

### Upgrading

- Replace the binary or image (`ghcr.io/scx1332/arkiv-node:0.1.0-alpha.6`)
  and restart with the same flags; there are no new flags and no migration.
  Producers and followers may be upgraded in any order.
- Clients that page with an `offset` past 1,250,000 matches, or sort more
  than 480,769, get `-32004` at once instead of `ReadDeadlineExceeded` after
  30 s: page with the cursor or narrow the query.

## 0.1.0-alpha.5 — 2026-11-22

Alpha pre-release.

### Clean stop on SIGTERM and SIGINT

- The node stops on SIGTERM or SIGINT once no block is in flight: it waits
  for the node lock, under which every block commits durably, and exits 0
  (logged as `{"event":"shutdown",...}`). Before, it had no signal handlers:
  as PID 1 in a container it ignored SIGTERM, so `docker stop` and Kubernetes
  waited out the whole grace period and then killed it.
- New dependency: `signal-hook-registry` (tokio's `signal` feature).

## 0.1.0-alpha.4 — 2026-11-22

Alpha pre-release. Block timestamps follow the wall clock, and the node
ships as an attested, scanned container image. **Breaking:** older nodes and
light clients require the timestamp to equal the schedule
(`start + height × period`) and refuse blocks from a 0.1.0-alpha.4 producer
once its chain has stopped for longer than a block period; upgrade followers
first. Stores and chains from 0.1.0-alpha.3 open unchanged.

### Block timestamps follow the wall clock

- The producer stamps each block with the wall clock, at least one block
  period after its parent and never before `genesis start + height × block
  period`. Before, the timestamp was exactly that floor, so a chain that had
  stopped stayed behind real time by all its downtime (a node restarted
  after 9 hours of rebuilds reported blocks 9 hours old).
- Nodes and light clients accept a header whose timestamp is at or after the
  floor and, where the parent is known, at least one period after it. Chains
  stamped the old way still verify. Gas estimates simulate the next block
  with the same rule. Details: `docs/signed-proposer-v1.md`.
- `SignedGenesis::timestamp` is now `earliest_timestamp`; `next_timestamp`
  and `check_successor_timestamp` are new. `Host::set_clock` replaces the
  producer's clock in tests.

### Metrics

- The node sample (head height and timestamp, pool, retention) no longer
  freezes on a busy producer: the sampler retried the node lock once a
  second and, with 250 ms blocks, almost never got it. It now retries every
  20 ms.

### Security checks

- `security.yml` (on push to main, on PRs, weekly), ported from
  golemfactory/golem-p2p, runs pinned, checksum-verified tools:
  - `cargo deny` and `cargo audit` on the Rust dependencies;
  - `zizmor` and `actionlint` on the workflows;
  - `gitleaks` over the whole git history;
  - Trivy on the published image, weekly.
- The Image workflow scans each image with Trivy before publishing. A fixable
  HIGH or CRITICAL stops the publish, and the full report goes to code
  scanning. See `docs/security-checks.md`.
- Dependency updates for RustSec advisories:
  - rustls 0.23.46, rustls-webpki 0.103.17 and webpki-roots 1.0.12;
  - quinn-proto 0.11.19 and quinn-udp 0.5.16;
  - url 2.5.9, ruint 1.20.0, tokio 1.54.1, rustix 1.1.6 and yoke-derive 0.8.4.
- Remaining exceptions: crypto-bigint 0.5, via k256 through alloy, which has
  no fixed release, and paste, which is unmaintained.
- Every workflow action is pinned to a commit. Checkouts no longer keep the
  token, and write permissions are given only to the job that publishes.

### Container image

- `Dockerfile` builds the node (`arkiv-proposer`) into a `debian:bookworm-slim`
  image that runs as uid 10001 with its state under `/data`, for Docker and
  Kubernetes. Ports: 9500 (API and JSON-RPC), 9501 (private), 9600
  (metrics); the binary still binds 127.0.0.1 unless told otherwise.
- The `Image` workflow builds it beside CI, starts a producer in it as a
  smoke test and publishes `ghcr.io/scx1332/arkiv-node`: `main` and
  `sha-<short>` from main, `X.Y.Z[-pre]` and `latest` from release tags.
- Each published image carries a build provenance attestation (Sigstore,
  stored on GitHub and pushed to the registry): `gh attestation verify
  oci://ghcr.io/scx1332/arkiv-node:TAG --repo scx1332/arkiv-state-db-experiment`.

## 0.1.0-alpha.3 — 2026-11-22

Alpha pre-release. Disk use is bounded by a 256-block window: old state,
indices and per-height records are deleted, not only refused. **Breaking:**

- A store that collects garbage is store format 2; older binaries refuse it.
- A request identity is unique for 256 blocks, not forever (below). Nodes
  on older binaries would refuse a block that reuses an older identity.
- `--metrics-interval-secs` is replaced by `--metrics-table-stats-secs`.

### State is pruned to a 256-block window

- `--state-retention` defaults to 256 for producer and full roles on new or
  adopted state. A file that has always been an archive stays one unless it
  is migrated (`state migrate --run-dir DIR --state-retention N`).
- Trie objects (entity data and indices) are reference-counted from the
  head and deleted inside the block commit once no retained snapshot or
  open reader can reach them. Per-block budgets are floors, so the backlog
  cannot keep growing.
- Nothing that feeds roots, receipts or charges reads garbage-collection
  rows.
- Status shows `stateGc`; `state_gc_*` metrics need no walk. Offline:
  `state gc-enable | gc-status | gc-verify | table-stats`.
- On the 16.8-million-entity Sourcify state, `node.redb` grew 3 GiB over
  12,000 blocks, against 544 GiB in 76 minutes before
  (`docs/benchmarks/2026-11-20-sourcify-pruned-soak.md`).

### Metrics no longer walk the database

- The exporter's periodic table walk held one redb read transaction for
  tens of minutes on a large state, so no freed page could be reused; it
  pinned 536 GiB of a 650 GiB file. File sizes now come from `stat`; the
  walk is opt-in with `--metrics-table-stats-secs N`.

### Per-height records expire with the state window

- **Protocol: a request identity is unique for 256 blocks**
  (`REQUEST_IDENTITY_WINDOW`), not forever. A block may not include an
  identity one of the previous 256 blocks included; an older identity may be
  used again. Nonces still make every transaction single-use. Older binaries
  would refuse such a block.
- A window node expires block records, feeds, receipts, inclusions, the hash
  index and floor records below `recordFloor` (status, metrics), inside
  block commits. They answer `HistoryPruned`; hash lookups answer `null`.
  Restart reads only the kept blocks.
- `--body-retention archive|window` (default `archive`): bodies in
  `history.redb` are kept for followers and light clients that start from
  genesis, or deleted with the records.
- Files from before are indexed once at start (`state migrate-records`).
- `CommitBatch` carries record and object removals.

### Stricter requests and typed errors

- **A y-parity above 1 is `InvalidEthereumTransaction`**; it was reported as
  `GasPriceBelowMinimum`.
- **Quantities are minimal `0x` hex.** `"0x+1"` was accepted; a sign, an
  empty `"0x"` and leading zeros are refused. `"0x0"` is valid.
- **`arkiv-proposer` subcommands take only their own flags.** `keygen --out k
  --peer x` wrote a key; it is refused with `InvalidRequest` before any side
  effect.
- **A submitted transaction is validated once**, outside the node lock
  (`protocol::ValidTx`); it was validated again under the lock.
- The demo runtime answers 500 when a response cannot be serialized, not an
  empty 200. `Branch::checkpoint` and `Branch::seal` are banned in
  `arkiv-node`; the fallible forms are used.
- Error codes are a closed `ErrorCode` enum rendering the same strings;
  `HistoryPruned` details travel in the error. Durable-engine messages name
  the admission fault ("genesis row fails admission: ..") where they said
  "row exceeds admission limits".

### Structure

- Test support (`arkiv_db_storage::testing`, `arkiv_db_redb::testing`) is
  behind a `testing` feature and is not compiled into the node. The durable
  import entry points nothing called (`prepare_import`, `import_checkpoint`,
  `initialize_from_staged`) are removed.
- `arkiv-node`: `signed/rpc` and `signed/rpc_reads` are module trees with one
  function per JSON-RPC method; the due schedule, expiry resolution and
  account debit exist once; `exec` takes a `BlockEnv` and a `Mode`. A failing
  CLI prints the underlying cause on stderr before its final code line;
  response bodies are unchanged.
- `arkiv-db`: `engine.rs` is the `engine/` module tree; proofs, durable
  publication and the codec share helpers; integration tests are one binary
  (`tests/it`) over `tests/support`.
- `arkiv-node`: `signed` no longer imports from `demo` (`text`, `dev`,
  `http`, `entity_json`, `fsutil`, `error` modules); `Host::create` takes a
  `Launch` and a `Prefill`; the proposer has one function per subcommand.

### Cleanup

- **Licence: GPL-3.0-or-later** (root `LICENSE`, every crate manifest).
  `vendor/alloy-trie` keeps MIT OR Apache-2.0.
- Removed code that only its own tests used: the raw key-value facade
  (`Database`, `KvStore`, `MemoryStore`, `WriteBatch`), `KeyWriter` and
  `KeyReader`, `arkiv::execute_batch`, `SignedTx::from_bytes`,
  `SignedBlock::from_bytes`, and the unused `vendor/reth-trie`.
- Removed `arkiv_db_memory::persistent` (unused), about thirty unused public
  items, duplicate runs of the storage contract suite, tests that compared a
  method with its own wrapper, and charge vectors pinned in two files. The
  workspace runs 1,167 tests, was 1,319; no pinned vector changed.
- `FaultyStore` forwards `stage_objects` and `relabel`, as documented. A
  panicking adoption worker no longer leaves the others waiting.
- Documentation states the current formats and limits; finished plans, run
  logs and nine obsolete scripts are gone or under `docs/runs/`. Open items:
  [docs/code-quality-todo.md](docs/code-quality-todo.md).

### Receipt polling no longer starves block production

- **Stored blocks are authenticated once.** `eth_getTransactionReceipt`,
  `eth_getBlockByNumber`, `eth_getLogs` and the other block reads re-read and
  re-authenticated the whole stored block on every call (a signature recovery
  per transaction, every digest), under the node lock. A client polling for
  receipts kept the lock busy, so the producer and `eth_sendRawTransaction`
  waited: a wave of 8 transactions took two to three blocks. The host now
  keeps the 32 highest blocks it has read (`Host::shared_block`).
- **Logs hash each transaction once.** `block_logs` hashed the transaction
  body and the block header once per log, 128 times for a full block.
- Measured through the SDK (2,000 entities, 250 ms blocks, 8 transactions of
  16 creates in flight): **485 entities per second, was 162**; 118 entities
  per block of the 128 a block holds; p50 transaction latency 242 ms, was
  663 ms (issue #12). No protocol or response change.

### Expiry: 128 per block, and a full block spills to the next

- **A height holds up to 128 expiring entities** (was 16). A signed genesis
  accepts `expiry_per_block` from 1 to 128 (`MAX_EXPIRY_PER_BLOCK`, the most
  operations a block holds); a producer started without a bundle and the new
  preset `arkiv-entity-v3/glm-v2@5` use 128. Creates sharing one lifetime in
  one block (8 transactions of 16) therefore all commit; before, 7 of 8
  reverted with `ExpiryCapacityFull` (issue #9). Purge stays exact: block `E`
  deletes every entity recorded at `E`.
- **This is a genesis parameter.** An existing chain keeps the value in its
  genesis (16) and opens unchanged; to get 128 an operator creates a new
  chain from a new genesis (bundle profile `[chain_params] expiry_per_block`,
  or the `@5` preset). The value must fit the expiry reservation: 128
  deletions reserve 6,835,200 of the default 10,000,000 units at 39 cells per
  record and fit up to 61 cells; a 64-cell profile needs 123 or fewer.
- **Preset versions.** `arkiv-entity-v3/glm-v2@4` is still accepted and
  expands to the same bytes (now kept in
  `crates/arkiv-node/src/genesis_presets/`), so chains authored with it lock,
  build and verify the same genesis. Changed on purpose, with new hashes:
  `docs/examples/genesis-bundle/profiles/fixed-glm-v2.toml` (the `@5`
  expansion) and `docs/examples/far-future-prefill/far-future-glm-v2.toml`
  (both `expiry_per_block = 128`, so the `sha256`/`bytes` pins in the two
  `chain.toml` files), the examples and smoke scripts that named `@4` (now
  `@5`), and the golden hashes of tests built on the default native genesis.
  A bundle rebuilt from these examples is a different chain than one built
  from the old files.
- **Spill (consensus).** An operation that sets an expiry (create,
  `extendExpiry`, a generic record's create or patch) and resolves to a full
  height is recorded at the first of the next 64 heights with room
  (`EXPIRY_SPILL_WINDOW`), in execution order, and fails with
  `ExpiryCapacityFull` only when all 65 are full. Permanent entities never
  spill; generic records never spill past `max_lifetime`; genesis prefill
  rows stay strict. Rent is charged for the height asked for, so the extra
  blocks are free. Events (`EntityCreated`, `ExpiryExtended`) and reads report
  the recorded height. Upgrade every node of a chain together; a chain whose
  history contains an `ExpiryCapacityFull` rejection cannot be re-executed
  from genesis by this version (the rejected write would now succeed).
- When the whole window is full, `eth_call`/`eth_estimateGas` answer with an
  `Error(string)` revert reason naming the blocks and the capacity instead of
  empty revert data.
- Measured sweep of a full height on durable redb (commit included, empty
  block 5 to 8 ms): 128 typical entities 19 ms; 128 worst-case entities (31
  indexed strings and a 128 KiB value) 260 ms, 64 take 134 ms, 32 take 50 ms.
  128 suits block periods of 1 s and more; a 250 ms chain that must bound the
  worst case should set 32. See `docs/entity-expiration.md`, the new page on
  the whole expiry lifecycle.
- The simulator/demo genesis (`demo::workload::genesis`) keeps 16: simulator
  stores check their parameters against it.

### `str` values take any UTF-8

- A `str` value may hold control characters (C0 and DEL were refused): only
  valid UTF-8 and the 128-byte cap bound it, as on the Arkiv network. SDK
  1.0's health suite writes `"line one\n\tline two"`, which
  `eth_estimateGas` answered with `InvalidArkivAttribute`. The rule is shared
  by transactions, genesis prefill rows and query literals, so such values
  are stored, returned and queryable (`=`, ranges, `STARTSWITH`, `ORDER BY`)
  with the characters raw inside `str('...')`.
- **Consensus:** this widens which transactions and prefill rows are valid.
  A producer on this version can include a transaction that a full follower
  on an older binary refuses, so upgrade every node of a chain together.
  Existing state stays valid.
- `scripts/sourcify-v4-prefill.py` no longer refuses such strings either.

### `arkiv_query`: `ORDER BY`

- A query may end with `ORDER BY name:type [ASC|DESC]`, as SDK 1.0's
  `orderBy(asc(...))`/`desc(...)` sends it: one attribute read as one type,
  indexed or not (`$key`, `$owner`, `$creator`, `$expiresAt` and
  `$createdAt` carry their own). A match without a value of that type sorts
  below every value; ties come in record-id order in either direction.
- It is upstream's fetch-and-sort: every page reads the key and sort cell of
  every match at the pinned snapshot, keeps the first `offset + limit` in a
  bounded heap and reads that window's rows by key, all charged to the read
  budget (`-32004` when the match set is too large to sort). Sorted pages
  page by `offset` (with `atBlock` and `select.count`) and carry no cursor; a
  `cursor` beside `ORDER BY` answers `-32005`, and a sorted window past the
  8 MiB cap answers `-32004` instead of being shortened. `select.count` and
  `arkiv_getEntityCount` ignore the clause.
- The UDC SDK smoke sorts through the schema (`desc("views")`,
  `asc("$key")`, and `asc("title")` on an unindexed column), paged by offset.
- Open items are listed in `docs/sdk-1.0-compatibility-todo.md`.

### `arkiv_query`: SDK 1.0 paging, counts and ranges

- `offset` (hex or number) skips that many matches before the page, in
  record-id (creation) order. The node walks the skipped matches by id, one
  engine page at a time, then reads on after the last of them
  (`Snapshot::cursor_after`); the walk is charged to the request's read
  budget, so a deep offset answers `-32004` (`limits`) instead of scanning
  for free. An offset at or past the end is an empty page without a cursor. A
  cursor carries its own position: it continues past an offset page, and an
  `offset` beside it is ignored.
- `select.count` adds `count`, the hex size of the whole match set at the
  block, independent of `limit`, `offset` and the cursor and repeated on every
  page; selected with no entity field (SDK `client.count()` sends
  `{count: true}`), it is the whole answer, with `data: []` and no cursor. It
  is the planned count, charged to the same budget; a query beyond the group
  and literal bounds counts by the bounded scan, as its page reads.
- Both compose with `atBlock`: offset pages pinned to page one's
  `blockNumber` stay on that snapshot while the head moves. SDK 1.0 sends no
  `atBlock`, so its offset pages read the head and shift when entities are
  written between them (see `docs/sdk-compatibility.md`).
- Comparisons (`<`, `<=`, `>`, `>=`) take every indexable type, in index
  order, and `a BETWEEN lo AND hi` is an inclusive range whose bounds share a
  type (`lo > hi` matches nothing), as the reference grammar and SDK 1.0 have
  them. `n > str('x')` used to be a type error; `between` is now a reserved
  word in queries.
- `Snapshot::cursor_after` builds a cursor of a snapshot that resumes a query
  after any record id, as if a page had ended there; a page cut to fit the
  8 MiB response cap now continues from one instead of re-reading the
  snapshot with the shorter limit.
- `scripts/udc-sdk-smoke.{py,ts}` target SDK 1.0.x: `getEntity` over
  `arkiv_getEntity`, `limit`/`offset` windows with `withCount()`,
  `client.count()`, `between` and a `str` range through a schema, offset pages
  pinned with `atBlock`, and `ExpirationTime.max()` where it used the removed
  `permanent()`.

### `arkiv_getEntity` for SDK 1.x

- JSON-RPC `arkiv_getEntity` (`[key]` at the head or `[key, block]` at a
  retained block) returns one entity with every field except
  `attributeSchema`, or `null` when no live entity has the key, the shape
  `@arkiv-network/sdk` 1.0's `getEntity` reads. It is planned as the one-row
  `$key` query and is a detached read like `arkiv_query`.

### Node lock: reads wait out holders, writers go first

- A light node's verified reads no longer fail with `ServerBusy` because a
  node lock is held at that moment. `query/text` had answered such a refusal
  as a 200 with `"error":"ServerBusy"` and verification `failed`, whenever the
  forwarded `query/eq` found the producer's lock taken by a follower's header
  poll; `scripts/signed-proposer-smoke.py` failed about one run in four.
- Block production and follower sync are the lock's writers and the only
  callers that queue on it; everything else polls it, yields to a waiting
  writer and takes turns (reads one at a time, JSON-RPC requests one at a
  time, the two alternating when both wait). REST requests, transaction
  admission and control included, detached reads pinning their snapshot
  (`query/eq`, `query/range`, `state/explore`, `arkiv_query`,
  `arkiv_getEntityCount`) and a light node's verified reads wait out a holder
  for about a second, then answer `ServerBusy`; other JSON-RPC requests wait
  without a limit. JSON-RPC `arkiv_query` and `arkiv_getEntityCount` used to
  wait for the lock up to `--read-deadline-secs` and now answer `-32010`
  (`ServerBusy`) once it has been held for about a second. In a release-build
  stress run with clients hammering `query/eq`, light verified reads or
  `arkiv_getEntityCount`, the producer made 115 to 118 of 118 blocks in 6 s
  (before: 1 to 21 under `query/eq` or `arkiv_getEntityCount`) and the light
  stayed within one block (before: 103 behind under `query/text`).
- A light node verifies a proof outside its lock and then confirms, under
  it, that it still accepts the header (`light::verify_eq_at`,
  `light::verify_range_at`).
- A full node keeps the certificates it authenticated for up to 256 of the
  highest heights read, so header reads and snapshot pins no longer repeat
  signature recovery under the lock.

### Live permanent entities and rent outside gas

**Breaking:** block format version 5 (the genesis gains the rent clock,
`block_period_ms` and `max_rent_lifetime_seconds`, and the storage cap
`max_new_storage_units_per_block`, under digest domain `arkiv/genesis/v4`;
each transaction outcome gains `rent_wei`, under `arkiv/outcomes/v2`) and
protocol version 5. Earlier genesis files and stores are refused; formats and
protocols 3 and 4 were never released.

- Live creates and extensions accept `expiresAt = 2^64 − 1` (the SDK's
  `ExpirationTime.permanent()`). Such entities are never scheduled or swept;
  reads and `EntityCreated`/`ExpiryExtended` logs report `0xffffffffffffffff`.
  Status reports `permanentEntitiesSupported: true`, the `permanentEntities`
  rules, and no longer lists `permanent-entities` as unsupported.
- Rent rule `glm-rent-v3`: rent is GLM, not gas. It is 20 gwei per storage unit
  per day (the former rate at 2-second blocks), priced per second at the
  genesis block period, so a year costs the same at every period. It is a
  separate debit from the sender, burned like the fee, that counts against
  neither the transaction nor the block gas limit: a full 128 KiB entity made
  permanent executes in about 850,000 gas and pays about 0.97 GLM of rent in
  one transaction.
- Rent is never charged past `height + cap`. Create, growth and extension
  charge through `min(expiry, height + cap)`, recomputed from the current
  height, so every lifetime of at least the cap, permanent included, costs the
  same, and shorter ones are priced exactly. An extension pays only the growth
  of the capped horizon. Permanence never costs less than one cap per unit.
- The cap is a genesis consensus value in seconds (default 31,536,000, one year;
  at most 100 years), applied in blocks rounded up at the genesis block period
  (which a signed genesis requires to equal its timestamp period). Status
  advertises `rentRule`, `rentCharging`, `rentWeiPerStorageUnitDay`,
  `rentBlockPeriodMs`, `maxRentLifetimeSeconds` and `maxRentLifetimeBlocks`.
  Profiles set `max_rent_lifetime_seconds`. A producer without a bundle uses
  a year.
- Rent is bounded only by the sender's balance: a wallet's `gas × price +
  value` does not cover it. An operation risks about 1 GLM at most; a signed
  allowance is planned for when the SDK can set one.
- An operation whose rent the sender cannot pay after its gas reservation
  fails with `ArkivFault::InsufficientRentBalance` (tag 17); its transaction
  rolls back, gas charged, no rent. Admission checks nonce, sizes, capacity,
  fee and value first, then quotes the transaction's rent (its operations in
  order, each seeing the earlier ones, with a 64-block height margin) and
  refuses a sender who cannot cover it with `insufficient funds for gas *
  price + value + rent`. `eth_estimateGas` returns execution gas only.
- The producer builds each block in one execution: a transaction whose gas
  reservation an earlier rent consumed is left out with its sender's later
  ones, and after each commit pending transactions their senders can no
  longer cover are dropped. A dropped transaction answers
  `eth_getTransactionByHash`/`eth_getTransactionReceipt` with `transaction
  dropped: ...` and native status `dropped` with the reason.
- `max_new_storage_units_per_block` caps the storage units a block's
  committed transactions add (default 1,500,000, the most the default input
  cap admits, so nothing changes by default). The producer defers what would
  cross it; a transaction that alone exceeds it is refused at admission;
  followers refuse a block over it. Status advertises
  `maxNewStorageUnitsPerBlock`.
- Receipts add `rentFee` (wei); native request status adds `rentChargedWei`,
  and the block feed's transactions `rentWei`.
- Only the owner extends an entity beyond one rent cap ahead: a
  permissionless-extension entity takes anyone's extension up to `height +
  cap`, and another account asking further, permanence included, is refused
  with `ArkivFault::PermanenceOwnerOnly` (tag 18). Extending a permanent
  entity to permanent is a no-op success; a finite expiry is
  `ExpiryShortened`. `PermanentStorageUnsupported` (fault 14) is no longer
  produced.
- Entity expiries are no longer bounded by `max_lifetime` (no `TooFar`);
  `max_lifetime` now bounds generic records only. Finite prefill rows burn the
  capped rent in wei; sponsored permanent rows are waived before any quote.
- The signed transaction cap is 384 KiB (the native envelope stays at
  256 KiB), so an Ethereum transaction, which carries its original bytes
  beside the envelope, fits a full 128 KiB entity.

### User-defined columns

**Breaking:** user-defined columns (UDC), the entity model of the JS SDK's
`udc/docs` branch. Block format version 2 and protocol version 2. Genesis
files, prefill streams, stores and SDK clients from earlier builds are
refused; start a fresh chain.

#### Entity model
- An entity is its seven system cells plus named, typed user attributes. The
  payload and content type are gone: `$payload` and `$contentType` are no
  longer created, writable, removable, queryable or returned.
- Bit 7 (`0x80`) of an ABI attribute `typeId` marks the attribute indexed;
  the low seven bits keep the type IDs 1–10. Indexed attributes are stored
  as engine attributes (queryable), unindexed ones as fields (stored and
  returned only). `bytes` is a user type, never indexed (`0x87` is refused).
  Type 0 stays the patch-only tombstone and may not carry the flag.
- A patch stores each attribute with the flag of its new triple.
- Entity operations carry each attribute as an engine `Cell` (kind ‖ value),
  so the flag is signed, encoded in blocks and replayed. This layout change
  is block format version 2: transaction and block-input digests use the
  `arkiv/tx/v2` and `arkiv/inputs/v2` domains.
- Each `bytes` value is at most 128 KiB, and the `bytes` values of the
  resulting entity, after a create or patch, at most 128 KiB together
  (`ArkivFault::EntityTooLarge`, tag 16).
- At most 32 triples per create or patch. Signed genesis now requires
  `max_cells` between 39 and 64, so the system cells always leave room for 32
  user attributes; the default and preset profiles use 39.

#### JSON-RPC
- `arkiv_query` entities match the SDK's `RpcEntity`: `attributeSchema`
  entries are `{name, type, indexed}`, and `attributes` include unindexed
  attributes, `bytes` values too. `select` takes the SDK's `RpcSelect` keys;
  `payload` and `contentType` are refused as unknown keys.
- Query refusals use the SDK's code table: -32001 parse (with `data`
  `{position, expected, got}`), -32002 type, -32003 literal, -32004 limits,
  -32005 cursor, -32006 block. `HistoryPruned` moves from -32001 to -32006 on
  every method, `ServerBusy` from -32005 to -32010, and the WebSocket
  subscription limit from -32005 to -32011.
- Predicates never match unindexed attributes.
- Verified equality pages list every attribute of each proven entity with its
  `indexed` flag; the entity profile is `arkiv-entity-v3`.

#### Gas
- Storage units count every cell's name, value and two tag bytes, whatever its
  name; only indexed cells add the 128-unit posting reservation. The
  calibration in `docs/glm-gas-v1.md` is regenerated.

#### Genesis
- Prefill rows use row codec 3 in both framings (`ARKPFL01 ‖ u32(3)` and
  `ARKPFL02 ‖ u32(3)`): each attribute is `CellName ‖ Cell`, with kind `01`
  indexed and `02` unindexed; no payload or content type; at most 32
  attributes and 128 KiB of `bytes` per row. See `docs/genesis-bundle-v1.md`.
- Authored entities give each attribute exactly one typed value and an
  explicit `indexed`; `bytes`, `bytes_text` and `bytes_file` spell bytes
  values. Dataset formats are `arkiv-entities-toml-v2` and
  `arkiv-entities-jsonl-v2`.
- `arkiv-proposer create` takes `--data-file` (an unindexed `bytes`
  attribute `data`) instead of `--payload-file`.

## 0.1.0-alpha.2 — 2026-10-11

Alpha pre-release. **Breaking:** state layout 2. Stores, genesis files and
proofs from 0.1.0-alpha.1 are rejected at open, and there is no migration;
start from fresh stores.

### Engine
- Unpadded, prefix-free keys ([#2](https://github.com/Arkiv-Network/arkiv-state-db-experiment/pull/2), issue #1).
  - The trie accepts variable-length keys with a maximum length. No stored key
    may be a strict prefix of another, and such inserts are rejected.
  - Term keys are `name || type tag || presence || value`:
    - names and raw keys are escaped and end in `00`;
    - each string value byte is incremented by 1, with a `00` terminator;
    - scalars keep their fixed-width encodings.
  - Raw-record and record-key-binding keys are unpadded the same way.
  - Example sizes: `color = "red"` goes from 164 bytes to 12, and a `u64`
    value from 164 to 18.
- Complete range proofs take inclusive, exclusive and unbounded endpoints
  directly; the successor and predecessor arithmetic on padded keys is gone.
- The indexed-string limit is separate from the key encoding. The default
  and Arkiv limit stays at 128 bytes, and a generic instance can configure
  up to 512. Verifiers must use the instance's committed genesis limits.
- Metering is unchanged (model version 3).

### Performance (NVMe, against alpha.1)
- 1.04–1.26x higher throughput wherever the difference is significant, with
  the largest gains on index-heavy workloads (1.26x with 16 indexed
  attributes). Concurrent reads are 12–18% faster.
- Reachable state is 6.5–33% smaller and equality proofs are 2–5% smaller.
- Against golem-db 57c48ca, throughput is 1.2–3.4x higher; 12 of 13 results
  are significant.
- Details are in `docs/benchmarks/2026-10-11-prekucki-v1.md`.

### Tests
- Added BTreeMap differential tests with variable-length keys, adversarial
  range-bound and proof tests, and tests at the 512-byte ceiling.

### CI
- GitHub Actions run workspace checks (fmt, clippy, tests) and the golem-db
  conformance kit on pushes to `main` and `experimental-*` branches and on
  pull requests.

## 0.1.0-alpha.1 — 2026-10-10

First tagged pre-release. It's an experimental database and node for Arkiv,
with no stability guarantees. The on-disk formats, wire formats and state
roots may change in any later release without migration, and there is no
consensus or finality.

### Engine
- An ordered, metered block engine on Merkle Patricia state, with typed
  records and attribute indexes inside each namespace.
- Queries with keyset pagination, pinned to one snapshot. Range queries work
  on every value type (numeric, bool, string, bytes).
- One profile: 32-byte cell names, 128-byte indexed values, all golem-db value
  types, and limits set at genesis. The only gas profile is `glm-v2`.
- A durable engine on redb (fsync on every commit), with a decoded-node cache
  and concurrent readers alongside the single block producer.
- State retention: `--state-retention archive|N`, archive by default.
  - Reads more than N snapshots below the head are refused with
    `HistoryPruned`: JSON-RPC `-32001`, REST `410 Gone`.
  - The floor is saved to disk and never moves back.
  - Nothing is physically deleted yet; see `docs/pruning-design.md`.

### Node
- `arkiv-proposer`: a signed single-proposer chain (producer, full and light
  followers) for the Arkiv v2 SDK entity model, with genesis bundles and
  replication.
- `arkiv-simulator`: an unsigned simulator node with its own stores.
- `arkiv-demo`: a small demo of a producer with full and light followers.
- Every binary accepts `--version`.

### golem-db conformance
- An adapter and kit for golem-db-conformance-testing, in `conformance/golem-db`.
  On golem's ported test suite, 16 tests pass and 5 fail; all 5 failures
  concern golem's internal system records.
- Benchmarks against golem-db 57c48ca on NVMe: arkiv is 1.4–2.7x faster in
  all 13 scenarios. See `docs/benchmarks/` and `docs/benchmarking.md`.

### Assets
The release includes the Linux x86_64 (glibc) binaries `arkiv-proposer`,
`arkiv-simulator` and `arkiv-demo`, with SHA-256 checksums.
