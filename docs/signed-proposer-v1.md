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

The [design plan](signed-proposer-v1-plan.md) records the protocol and dependency
decision. Implementation uses the existing locked Alloy primitives 1.7.3 and
k256 0.13.4. Reth's Ethereum transaction pool is not added. `cargo +1.90.0 check
-p arkiv-node --locked` succeeds with the existing MSRV.

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
enabling the private listener or running a full follower. Key generation writes
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
Timestamps remain deterministic (`genesis start + height × block period`), so an
operator aligning an existing chain after downtime must explicitly pause, step
the missing blocks, and resume. This retains the signed genesis and history.

Without a genesis prefill, the first block contains exactly one proposer-signed
nonce-zero transaction creating the `entities` namespace. External admission returns
`ChainInitializing` until that block commits. No implicit demo keys or synthetic
unsigned transactions are inserted. Normal transactions, including the signed
bootstrap, pass through the pool and the native executor.

`GET /signed/v1/status` reports identity, proposer, acknowledged head, role,
authentication and body retention. `GET /signed/v1/genesis` returns public
canonical genesis and its certificate. The producer exports `genesis.bin` and
`certificate.bin`. A fresh follower requires these explicit pins:

```sh
target/debug/arkiv-proposer full \
  --run-dir /tmp/signed-full --listen 127.0.0.1:9510 \
  --peer http://127.0.0.1:9500 --replication-peer http://127.0.0.1:9501 \
  --source-id SOURCE_UUID --run-id RUN_HEX32 --chain-id 9001 \
  --genesis-hash 0xGENESIS_HASH --proposer 0xPROPOSER_ADDRESS
```

Use `light` instead of `full`, another run directory/listener, and omit the
replication peer for a light node. Pins are validated and retained in
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

All public paths use `/signed/v1/`. The unsigned `/sim/v1/` API and PostgreSQL
indexer configuration are unchanged; this change does not register a new
indexer source or deploy frontend assets.

| Method/path | Result |
| --- | --- |
| `POST transactions` | Closed `{transactionBytes:"0x..."}`; signed hash, actor, request ID, nonce and pending/included status |
| `GET requests/:actor/:requestId` | Durable inclusion and execution status, volatile pending, or unknown |
| `GET accounts/:actor/nonce` | Committed next nonce, advisory contiguous pending nonce and sampled head |
| `GET headers` | Bounded certificate batch and sampled signed head; `afterHeight`, `afterHash`, `limit` |
| `GET feed/blocks` | Full-node sanitized metadata; signed transaction hashes, no calldata |
| `POST query/classify` | Existing independent language, execution and proof classification |
| `POST query/resolve`, `query/eq` | Native canonical Eq request/proof transport |
| `POST query/verified`, `query/inspect`, `query/text` | Light verification against an accepted signed header, including pagination |
| Private `GET replication/blocks/:height` | Canonical signed body read from retained history at an acknowledged height |

Full/light nodes refuse transaction submissions. The private listener checks
the configured bearer; public query requests cannot select arbitrary peers or
forward client credentials. Peer URLs must be HTTPS or loopback HTTP.

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

Complete signed transactions are at most 256 KiB, signed input lists 768 KiB,
and retained signed bodies 1 MiB. Blocks have at most 16 transactions and 128
operations. Payload fields retain the native 128 KiB limit. There are four
concurrent bounded request/verification slots, including blocking jobs after
HTTP cancellation, and five-second request-body/fetch deadlines. Feed and
response caps remain 4 MiB and 8 MiB. Resource-limit proof failures remain
explicit; text queries never silently downgrade to unverified results.

Reads whose cost grows with the state are detached: full-node
`POST state/explore`, `query/eq` and `query/range`, and JSON-RPC `arkiv_query`
and `arkiv_getEntityCount`. Such a read holds the node lock only to check
heights and pin a snapshot (plus identity and limits), then evaluates on a
blocking thread after releasing it. A REST read gives back its admission slot
once admitted. At most `--read-concurrency` (1–64, default 4) detached reads
run at once; another is refused at once with `ServerBusy` (HTTP 429, JSON-RPC
code `-32010`). A detached read running past `--read-deadline-secs` (1–3600,
default 30) answers `ReadDeadlineExceeded` (HTTP 503, JSON-RPC code `-32000`),
and one whose client disconnects is abandoned; its metered walk stops at the
next charge and releases its permit. Status, headers, accounts, `eth_*` reads,
transaction admission and block production therefore no longer wait for a
long count. Those other requests, and every write, remain serialized under the
node lock as before; writers still answer `ServerBusy` when the lock is
momentarily held.

`arkiv_query` and `arkiv_getEntityCount` evaluate with planned read budgets
(`arkiv_db::Budget::planned`, see the [query plan](snapshot-query-plan.md)):
each AND-group is driven by its smallest index posting and counts use stored
posting sizes, with the same results as the written evaluation. Their group
and literal bounds are the genesis limits unless `--read-max-groups` (1–64)
or `--read-max-literals` (1–256) raise them for these reads; consensus never
sees either option, since block execution (the expiry sweep included) only
runs unplanned single-literal reads under the genesis limits.

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
compaction, key rotation or snapshot-based bootstrap.

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
cargo test --workspace --exclude reth-trie --exclude alloy-trie
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
