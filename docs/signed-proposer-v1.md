# Signed proposer v1

`arkiv-proposer` runs the Arkiv entity protocol with signed native
transactions, a bounded volatile pool, independently executing full followers,
and signed-header light followers. It shares the engine formats, native header
encoding, query parser/classifier and Eq proof encoding with the unsigned
simulator. Every signed chain runs the `glm-v2` gas profile (GLM pricing with
funded accounts; see [GLM accounts](glm-accounts-v1.md)). The implementation also accepts a limited Ethereum
transaction/RPC compatibility surface: replay-protected legacy and type-2
transactions, native GLM transfers, and Arkiv ABI operations. It is not an EVM;
its roots and block hashes remain native commitments. This profile does not
provide consensus, finality, fair ordering or availability.

## Design decisions

- **Signing uses `k256` through `alloy-primitives`**, not `alloy-signer`.
  `alloy_primitives::{Address, B256, Signature}` plus
  `k256::ecdsa::SigningKey` are enough to sign and recover a prehash, and
  `alloy-signer` would raise the minimum Rust version above 1.90 and pull in
  consensus and network crates.
- **No Reth transaction pool.** Its pool traits require Ethereum transaction
  types (EIP-2718 decoding, fee fields, blob sidecars) and bring EVM,
  chainspec and storage dependencies. Native operations would need fake
  Ethereum fields, and the budget, payload and state checks would still be
  ours. The pool here is small and has explicit limits (see below).
- **Transaction bodies live in a separate `history.redb`.** `node.redb` keeps
  engine state, headers, sanitized metadata and body references, so engine
  state and retained history are stored, backed up and bounded separately.
- **The transaction nonce has its own host key**, `arkiv/txnon/ || actor`,
  distinct from the create nonce `arkiv/nonce/ || actor`. An included
  transaction consumes a transaction nonce even when it rolls back; the create
  nonce counts successful entity creates only and feeds key derivation.

## Run locally

Build and run the isolated subprocess acceptance scenario:

```sh
cargo build -p arkiv-node --bin arkiv-proposer
python3 scripts/signed-proposer-smoke.py
```

The script uses fresh temporary directories, random local keys and loopback
ports, and cleans up its child processes. It bootstraps the namespace, signs
external transactions with two independent user keys, verifies full replay and
light query proofs, restarts every role, checks duplicate inclusion and reads
an old retained body. No running deployment is contacted.

For manual use:

```sh
target/debug/arkiv-proposer keygen --out /tmp/proposer.key
target/debug/arkiv-proposer producer \
  --run-dir /tmp/signed-producer --key-file /tmp/proposer.key \
  --allocations-file allocations.json \
  --listen 127.0.0.1:9500 --private-listen 127.0.0.1:9501
```

`allocations.json` maps addresses to decimal base-unit balances and must fund
the proposer for its block-1 bootstrap transaction (see
[GLM accounts](glm-accounts-v1.md#cli)); `--bundle BUNDLE` creates the producer
from a [genesis bundle](genesis-bundle-v1.md) instead.

Set `ARKIV_PROPOSER_TOKEN` to a private bearer of at least 32 bytes before
enabling the private listener, or running a full follower with
`--replication-peer`. Key generation writes
a new file with mode 0600 on Unix and prints only the public address. The
producer starts paused; private `POST /signed/v1/control` accepts `action`
`step`, `pause`, `resume` or `retry`. `--start running` enables the periodic
producer. These development controls do not have persistent command-ID
idempotency; a step request whose response is lost must be checked against
status before another step. Signed v1 has no pricing/admin control section.

Periodic production uses fixed monotonic deadlines at the signed genesis block
period. Block execution and storage time do not extend the next interval;
transient lock contention retries the same slot, and slow blocks catch up missed
slots after recovery. Pausing discards elapsed scheduler slots. Restarting starts
a fresh schedule: it does not automatically produce blocks for downtime.

Block timestamps follow the wall clock. The producer stamps a block with
`max(now, parent timestamp + block period, genesis start + height × block
period)`, so after downtime the chain clock jumps to the present instead of
staying behind for good. Every node and light client checks:

- a header alone: its timestamp is not before `genesis start + height × block
  period` (the schedule floor);
- a block or header after its parent: its timestamp is at least the parent's
  plus one block period.

A chain that never stopped is valid under both rules, so older chains whose
timestamps followed the schedule exactly still verify. Execution never reads
the timestamp; rent and expiry count blocks, so time a chain spends stopped
does not age its entities. No upper bound is checked against followers'
clocks: the single proposer is trusted for time as for everything else.
(Until 2026-11-22 the timestamp was exactly the schedule floor, and a chain
that stopped stayed behind the wall clock by its downtime.)

Without a genesis prefill, the first block contains exactly one proposer-signed
nonce-zero transaction creating the `entities` namespace. External admission returns
`ChainInitializing` until that block commits. No implicit demo keys or synthetic
unsigned transactions are inserted. Normal transactions, including the signed
bootstrap, pass through the pool and the native executor.

`GET /signed/v1/status` reports identity, proposer, acknowledged head, role,
authentication and body retention (`bodyRetention`, `bodyFloor` and the
snapshot pin `bodyPin`). `GET /signed/v1/genesis` returns public
canonical genesis and its certificate. The producer exports `genesis.bin` and
`certificate.bin`. A fresh follower requires these explicit pins:

```sh
target/debug/arkiv-proposer full \
  --run-dir /tmp/signed-full --listen 127.0.0.1:9510 \
  --peer http://127.0.0.1:9500 \
  --source-id SOURCE_UUID --run-id RUN_HEX32 --chain-id 9001 \
  --genesis-hash 0xGENESIS_HASH --proposer 0xPROPOSER_ADDRESS
```

A full follower started this way needs no token. Every 50 ms it asks its
peer's public `GET /signed/v1/blocks` for up to 64 blocks after its head,
authenticates each header against the pinned proposer and re-executes each
block. The peer must run a build that serves `GET blocks` (later than
0.1.0-alpha.7). With `--replication-peer http://127.0.0.1:9501` and
`ARKIV_PROPOSER_TOKEN` the follower instead reads one header per poll from
`GET headers` and its body from the private listener, as every release up
to 0.1.0-alpha.7 does; this still works against older and newer peers.

A full node does not have to start from genesis. `arkiv-proposer snapshot
install` turns a copy of another full node's databases into a run directory,
checked against the same pins, and the node then syncs only the blocks after
the snapshot: see [snapshots](snapshots.md). Three rules of `full` follow
from it:

- A `full` node whose `node.redb` already has a head and whose run directory
  has no `prefill.bin` neither downloads nor audits the prefill
  (`{"event":"prefill_skipped"}`), and serves none: `GET prefill` and `GET
  prefill/chunk` answer `PrefillBundleRequired`. With `prefill.bin` present,
  or `--bundle` given, the prefill is audited at every start and served, as
  before. A node without a head starts from the prefill as before.
- The file `RUN/snapshot-pin` (written by `snapshot create` and `snapshot
  pin`) holds a height. A producer or full node started with it keeps every
  block body from that height on, whatever `--body-retention` says, and
  reports it as `bodyPin`. The file is read at start only.
- A full node whose peer answers `HistoryPruned` for the block after its
  head (or for that block's header) cannot recover by retrying. It logs one
  `{"event":"sync_gap",…}` line that names its head and the peer and exits
  with status 3. It has to be installed again from a newer snapshot, or
  pointed at a peer that still has the bodies.

A peer URL is `https://HOST[:PORT][/PREFIX]`, or `http://` to loopback
(`127.0.0.1`, `localhost`, `::1`). The prefix is where a reverse proxy
mounts the node, for example `https://node.example/rpc`. `--allow-http-peers
true` on `full` and `light` accepts `http://` to any host, for a private
network or TLS that ends elsewhere. What is synced is verified the same over
either transport, but a replication token sent over plain HTTP crosses the
network in clear text.

Use `light` instead of `full` and another run directory/listener for a light
node; it takes no replication peer. Pins are validated and retained in
`trust.json` before initialization. Followers can reopen with those same pins
while the peer is offline; network availability is needed only to advance or
fetch proofs. A changed signer or pinned identity is rejected.

The `create` helper reads a public canonical genesis, local signer, explicit
nonce/request ID and a data file (stored as an unindexed `bytes` attribute
`data` beside an indexed `u64` attribute `group = 1`), and writes a complete
signed transaction:

```sh
target/debug/arkiv-proposer create \
  --key-file /tmp/user.key --genesis-file /tmp/signed-producer/genesis.bin \
  --nonce 0 --request-id 0xREQUEST_ID_64_HEX \
  --data-file /tmp/payload --out /tmp/create.tx
target/debug/arkiv-proposer submit \
  --peer http://127.0.0.1:9500 --transaction-file /tmp/create.tx
```

Read the committed and advisory pending nonce first at
`GET /signed/v1/accounts/0xADDRESS/nonce`. For arbitrary supported native
operations, `sign --key-file ... --genesis-file ... --tx-file NATIVE_TX_BYTES
--nonce N --out SIGNED_TX_BYTES` signs a canonical native envelope. The native
actor must match the local key. `submit` prints admission and polls request
status briefly; pending is volatile, and a timeout is not a rejection.

## HTTP and trust

All public paths use `/signed/v1/`. The unsigned simulator serves a separate
`/sim/v1/` API.

| Method/path | Result |
| --- | --- |
| `POST transactions` | Closed `{transactionBytes:"0x..."}`; signed hash, actor, request ID, nonce and pending/included status |
| `GET requests/:actor/:requestId` | Durable inclusion and execution status, volatile pending, or unknown |
| `GET accounts/:actor/nonce` | Committed next nonce, advisory contiguous pending nonce and sampled head |
| `GET headers` | Bounded certificate batch and sampled signed head; `afterHeight`, `afterHash`, `limit` |
| `GET blocks` | Producer/full: up to 64 consecutive canonical signed blocks (each with its certificate) and the sampled signed head; `afterHeight`, `afterHash`, `limit`; a page is cut at about 3 MiB of block bytes; `HistoryPruned` once a body expired (bodies from the snapshot pin on do not) |
| `GET feed/blocks` | Full-node sanitized metadata; signed transaction hashes, no calldata |
| `POST query/classify` | Existing independent language, execution and proof classification |
| `POST query/resolve`, `query/eq` | Native canonical Eq request/proof transport |
| `POST query/verified`, `query/inspect`, `query/text` | Light verification against an accepted signed header, including pagination |
| Private `GET replication/blocks/:height` | Canonical signed body read from retained history at an acknowledged height |

Full/light nodes refuse transaction submissions. The private listener checks
the configured bearer; public query requests cannot select arbitrary peers or
forward client credentials. Peer URLs must be HTTPS or loopback HTTP, with an
optional path prefix; `--allow-http-peers true` also accepts plain HTTP to
other hosts.

Signatures use domain-separated native prehashes and exactly 65 bytes
`r || s || yParity`, with parity 0/1. Received high-S, zero/out-of-range
scalars, alternate parity encodings, trailing bytes and failed recovery are
rejected. Actor, chain/run genesis, account nonce, request ID, budget and every
operation are bound by the transaction signature. Signed inputs are committed
by the unchanged native header's input digest. Header certificates bind the
pinned signed genesis and complete native header hash.

Full followers repeat signatures, static checks, resource limits, request-ID
and nonce checks, native execution, receipts/outcomes and terminal roots. Light
followers authenticate headers and verify requested native proofs; they do not
execute bodies. Two authentic conflicting native headers freeze a follower
durably. A forged signature does not freeze it. There is no fork choice or
operator unfreeze command.
Authenticated sampled and advertised headers are retained as observations even
when their height is ahead of the accepted chain and the body fetch fails.
Later polls and reopening compare those observations before acceptance;
catalog storage sequences are separate from accepted chain height.

## Nonces, bounds and persistence

Account transaction nonces occupy a distinct state-root-committed host key
`arkiv/txnon/ || actor`. An otherwise valid included transaction consumes one
nonce even when its operations fail and roll back. The existing
`arkiv/nonce/ || actor` counts successful entity creates and retains its
existing rollback/key-derivation meaning. Invalid signatures/nonces are invalid
blocks and publish no state. Included request IDs and transaction nonces survive
restart without scanning history; pool entries do not.

The pool allows 1,024 entries, 16 MiB canonical bytes, 256 senders and 16 entries
per sender, within the next 16 account nonces. It accepts bounded gaps but
executes contiguous sender heads, ordered by local arrival among ready heads.
No-fit heads and their descendants defer while other senders may fit.
Replacement is disabled. Exact signed bytes are idempotent; changed content at
an existing request ID or account nonce conflicts. These limits bound retained
work, not total process RSS or Sybil-resistant admission.

Complete signed transactions are at most 384 KiB (their native envelopes
256 KiB; an Ethereum transaction also carries its original bytes, so a full
128 KiB entity fits), signed input lists 768 KiB, and retained signed bodies
1 MiB. Blocks have at most 16 transactions and 128 operations. `bytes` values
retain the native 128 KiB per-entity limit. There are four
concurrent bounded request/verification slots, including blocking jobs after
HTTP cancellation, and five-second request-body/fetch deadlines. Feed and
response caps remain 4 MiB and 8 MiB. Resource-limit proof failures remain
explicit; text queries never silently downgrade to unverified results.

Reads whose cost grows with the state are detached: full-node
`POST state/explore`, `query/eq` and `query/range`, and JSON-RPC `arkiv_query`,
`arkiv_getEntity` and `arkiv_getEntityCount`. Such a read holds the node lock only to check
heights and pin a snapshot (plus identity and limits), then evaluates on a
blocking thread after releasing it. A REST read gives back its admission slot
once admitted. At most `--read-concurrency` (1–64, default 4) detached reads
run at once; another is refused at once with `ServerBusy` (HTTP 429, JSON-RPC
code `-32010`). A detached read running past `--read-deadline-secs` (1–3600,
default 30) answers `ReadDeadlineExceeded` (HTTP 503, JSON-RPC code `-32000`),
and one whose client disconnects is abandoned; its metered walk stops at the
next charge and releases its permit. Status, headers, accounts, `eth_*` reads,
transaction admission and block production therefore do not wait for a
long count. Those other requests, and every write, are serialized under the
node lock.

Block production and follower sync are the node lock's writers, and the only
callers that queue on it. Every other caller polls it, does not try while a
writer waits, and takes turns: reads one at a time, JSON-RPC requests one at a
time, and a read and a request that both wait alternate. A read that should
not fail just because the lock is held at that moment (a block being
produced, imported or appended, another request) waits out the holder for
about a second and answers `ServerBusy` only if it stays held: every REST
request (status, headers, feed, accounts, transaction admission, control, a
full follower's body fetch, prefill chunks), detached reads pinning their
snapshot, and a light node's verified reads (`query/verified`,
`query/inspect`, `query/text`, `query/range/*`, `query/account`,
`query/resolve`). A JSON-RPC request waits without a limit. No read load
therefore holds up production or sync by more than the request holding the
lock, as long as their tasks can start: both run on Tokio's blocking pool (at
most 512 threads), where each waiting JSON-RPC request occupies a thread, so
more than about 500 concurrent JSON-RPC requests delay them. A light node
verifies the proof outside its lock and only then confirms, under it, that it
still accepts the header. A full node keeps the certificates it has
authenticated for up to 256 of the highest heights read, so header reads and
snapshot pins do not repeat signature recovery under the lock.

`arkiv_query` and `arkiv_getEntityCount` evaluate with planned read budgets
(`arkiv_db::Budget::planned`, see [metering](metering.md)):
each AND-group is driven by its smallest index posting and counts use stored
posting sizes, with the same results as the written evaluation. Their group
and literal bounds are the genesis limits unless `--read-max-groups` (1–64)
or `--read-max-literals` (1–256) raise them for these reads; consensus never
sees either option, since block execution (the expiry sweep included) only
runs unplanned single-literal reads under the genesis limits. Their offset
and sort walks read engine pages of 16,384 rows (a local read's own page
bound), so a group is planned once per such page, and a walk the read budget
cannot finish is refused before it starts when stored posting sizes show it
(see the [API reference](node-api-reference.md)). An `arkiv_query` `offset`
above `--read-max-offset` (default 100,000; `0` for no limit) is refused
before the read is planned.

A pinned snapshot is consistent while blocks are produced: it fixes one
published state header, every trie node below it is a committed,
content-addressed and hash-verified object that later commits never change or
delete, each object read is its own short redb read transaction, and the
decoded-node cache is keyed by object id. The snapshot pins its height against
retention while it is read. See `crates/arkiv-node/src/signed/reads.rs`.

`history.redb` stores framed content-addressed bodies under raw Keccak IDs,
with its own profile, identity and storage sequence. `node.redb` stores engine
snapshots plus a bounded signed header/feed/body-reference envelope and typed
inclusion metadata, without transaction operations or transaction signatures.
Engine entity values and historical engine snapshots still retain payloads as
materialized state. Light nodes store certificates in `headers.redb` only.

Publication prepares the engine candidate, durably acknowledges the exact body
in history, then commits the engine reference/state/inclusions atomically.
Only acknowledged engine commit advances the visible head and removes pool
entries. Definite and indeterminate failures retain the exact candidate and
fence new production. Private `retry` attempts only that candidate; merely
observing records after a failed sync does not establish durability. Reopen
uses each store's recovery contract and validates the referenced head body.

A crash before engine publication can leave an orphan history object. It is
never treated as inclusion and cannot block another body at the same height.
Missing or corrupt committed body references fail closed. Backups must include
compatible state and history files. All bodies and snapshots remain stored.
`--state-retention <N>` (producer and full) only limits which state heights
are served (older ones are `HistoryPruned`, see the
[pruning design](pruning-design.md)); there is no physical pruning,
compaction or key rotation. A full node can start from a database snapshot
of another one instead of from genesis ([snapshots](snapshots.md)).

## Validation

Dedicated suites cover canonical vectors, original signature encoding and
mutations; pool count/byte/sender/nonce limits; multi-operation rollback and
separate nonces; independent full replay; metadata/payload separation; native
proof binding; real-socket pagination and query classifications; durable
equivocation; and both stores' definite/indeterminate failure paths. History
tests include real redb reopen and missing/corrupt restore cases. Existing redb
crash suites test the underlying immediate-commit storage contract.

```sh
cargo test -p arkiv-node \
  --test signed_protocol --test signed_pool --test signed_history \
  --test signed_host --test signed_http
cargo test --workspace
```

Fault injection exercises cuts before/during history acknowledgment and engine
publication using the storage contract; it is not evidence that every possible
two-file hardware failure was exhaustively enumerated. The subprocess smoke
tests actual stopped-process reopening and external HTTP flows. No production
deployment, PostgreSQL simulation, consensus or pruning is included.

### Dedicated node debug panels

The signed full node exposes `GET /signed/v1/feed/blocks/H` for a single retained
block's metadata (the same feed record as the paged endpoint). Light-node
`query/inspect` responses include the original typed `query` and verified entity
`creationFlags` alongside the nested identity, authenticated certificate and
canonical proof. These additive fields support the explorer repository's node
panels without changing proof bytes, chain identity or persisted history.

Configure the panels with `DEBUG_NODE_PROTOCOL=signed` and fixed full/light
upstreams. The browser must bind the response to the signed status identity and
must retain the authenticated single-proposer verification label; the unsigned
simulator label is not interchangeable. Verification runs in the hosted Rust
light process, not in the browser.

## Named sample namespaces

Signed chains admit native `CreateNamespace` transactions. Creation pays the
normal transaction intrinsic gas and metered namespace-directory/create/root
charges, debited from the signer's balance and burned like other transactions.
The namespace belongs to the signer. Generic record writes are disabled under
GLM pricing; Arkiv entity operations, with their storage-rent and ownership
rules, may address namespace `1` or any other namespace.

To add an **empty** `bricks` namespace to a running sample chain, use a funded
local signer:

```sh
python3 scripts/create-sample-namespace.py \
  --peer http://127.0.0.1:19500 --full http://127.0.0.1:19502 \
  --key-file /private/tester.key --actor 0xPUBLIC_SIGNER_ADDRESS \
  --genesis-file /private/producer/genesis.bin --name bricks
```

The helper discovers the catalog at a pinned full-node height, returns an
existing namespace with that name on reruns, signs through the native CLI, and
waits for full-node visibility. It prints public receipts and the assigned ID;
it never assumes namespace ID `2`. Use a dedicated signer without concurrent
submissions. A timed-out submission must be checked by request ID before retry.
In State Tree Explorer, refresh the snapshot and open **Catalog**, then follow
the `bricks` leaf. Its records, keys, rows and terms maps are initially empty.
Creation requires no explorer frontend change.
