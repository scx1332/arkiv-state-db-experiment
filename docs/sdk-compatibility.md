# Native engine SDK compatibility

> **Pre-UDC document.** This page was written for the payload-era SDK (0.8.x:
> an entity payload and content type, `payload`/`contentType` selections). The
> node implements the SDK's user-defined-columns model instead; for current
> write rules, read shapes and error codes see the
> [node API reference](node-api-reference.md#entities-user-defined-columns).
> The transport, gas and account behaviour below is unchanged.
>
> The SDK smoke is `scripts/udc-sdk-smoke.{py,ts}`: it runs
> `@arkiv-network/sdk` 1.0.x (a checkout of its `main` or `develop` with
> dependencies installed; point `ARKIV_SDK_DIR` at it) against disposable
> producer/full/light nodes, through a three-role restart and follower reads.
> It does not support the pre-1.0 `udc/*` branches: 1.0 removed
> `ExpirationTime.permanent()`, cursors and `atBlock`.

The signed node exposes Ethereum/Arkiv JSON-RPC at `/` on its existing public
listener, over HTTP POST and WebSocket. Native status exposes the structured
`sdkRpcProfile` capability; the blanket `sdkRpcCompatible` flag remains false
because this profile deliberately differs from complete Ethereum/Arkiv parity. Native `/signed/v1/...` endpoints remain
available. The adapter is implemented inside the signed node; no signing proxy
or private account keys are needed.

## Authenticated writes and execution

`eth_sendRawTransaction` accepts EIP-155-protected legacy and EIP-1559
transactions for the configured chain. Original signed bytes are retained in
signed transaction wire version 2 and authenticated again by full replay. The
transaction hash is the Keccak hash of those original bytes. Native wire version
1 is unchanged. Old binaries cannot replay new Ethereum transactions; upgrade
all roles before sending them.

Calls to `0x4400000000000000000000000000000000000044` decode the SDK's
`execute((uint8,bytes)[])` ABI into create, patch, extend, transfer-ownership and
delete operations. Attribute triples follow the user-defined-columns model of
the SDK's `udc/docs` branch: bit 7 of `typeId` marks an attribute indexed, user
`bytes` attributes are stored unindexed, and there is no payload or content
type (see the [node API reference](node-api-reference.md#entities-user-defined-columns)). Empty-data transfers to other addresses move native GLM.
Execution remains atomic across the operations in one transaction. Failed
transactions consume their transaction nonce and charge work, but roll back
entity mutations, creation nonces and emitted events.

The actual gas price remains fixed at **1 gwei**. Signed fee ceilings below
that price are rejected. Higher ceilings do not increase the fixed charged
price; priority fees are not paid. `eth_estimateGas` executes a disposable
branch, including failure checks, and adds a small gas allowance. It returns
execution gas only: entity rent is GLM debited beside the fee, never gas
([charging rent](glm-gas-v1.md#charging-rent)), so a full 128 KiB permanent
create estimates under a million gas. Estimation ignores the caller's balance
for gas fees, transfer value and rent, including when pending transactions
reserve that balance. It uses the same gas metering, operation permissions,
expiry rules and supplied gas limit as execution. Fee reservation/settlement is skipped; transfers receive only a
temporary caller balance override. The resulting candidate is discarded, with
no account, entity, nonce, burn-counter, history or transaction-pool changes.
The estimate is a gas quote, not an admission or affordability guarantee.

Actual submission and block execution still enforce funds, rent included.
JSON-RPC insufficient balance errors use code `-32000` and message
`insufficient funds for gas * price + value + rent`, including pending
reservations and the quoted rent (viem reports it as `InsufficientFundsError`).
Receipts add `rentFee`, the wei burned as rent (viem passes the field through).
They are admission failures, not contract reverts, and have no ABI revert data.
Native REST keeps its existing `InsufficientBalance` identifier.
`eth_call` supports the entity creation nonce and `execute` simulation, including
its ABI-encoded result keys, and retains its account checks. Simulation does not
commit state. Entity validation failures in estimation still return their
ABI-encoded revert data.

Contract creation, arbitrary EVM calls, unprotected legacy transactions,
nonempty access lists and other typed transaction formats are unsupported.
Expiry arguments retain the SDK's exact block-height/minimum-lifetime
semantics; `ExpirationTime.permanent()` (`u64::MAX`) is a live permanent
lifetime that pays a year of rent. Another account may extend a
permissionless-extension entity up to one rent cap ahead only: further,
permanence included, is the owner's (revert `PermanenceOwnerOnly`, which the
SDK's ABI does not name yet). A transaction that alone adds more storage units
than a block may is refused at submission, and one dropped from the pool
answers `eth_getTransactionReceipt` with `transaction dropped: ...`, so
`waitForTransactionReceipt` fails instead of waiting. Existing native limits still apply,
including operation, attribute and namespace limits. At most `expiry_per_block`
entities (128 by default) expire at one block; a write aiming at a full block
gets the next block with room, up to 64 later, so the `expiresAt` an event or
a read reports can be later than the one asked for (see
[entity expiration](entity-expiration.md)).

## Reads and subscriptions

The adapter supplies chain ID, block number, balances, account nonces, gas
quotes, fee history, blocks, transactions, receipts, logs and polling filters.
Receipts and logs are reconstructed deterministically from retained committed
blocks and prior state, including operation-order ownership changes. Failed
transactions emit no entity logs. Block gas usage includes native expiry work;
receipt cumulative gas includes user transactions only. `eth_subscribe` supports `newHeads` and `logs`.

`arkiv_query` supports the SDK's typed scalar JSON, selected fields and
attributes, equality, ranges on every indexable type (numeric order for
`i32`, `u64`, `u256` and `dec`, `false < true`, unsigned bytes for `str`,
`bytes32`, `addr` and `key`), inclusive `BETWEEN`, string prefixes, Boolean
expressions, `EXISTS` and `TYPEOF`, and SDK 1.0's paging and sorting:
`limit`, `offset`, `select.count` (the hex size of the whole match set, on
every page; alone, the whole answer) and `ORDER BY name:type [ASC|DESC]`
(`select(...).orderBy(asc(...))`/`desc(...)`, indexed and unindexed
attributes alike). Without `ORDER BY`, matches come in creation order: by
block, transaction and operation (record id). `arkiv_getEntity` (`[key]` or `[key, block]`) returns
one entity with every field except `attributeSchema`, or `null`; it is the
one-row `$key` query. `arkiv_getEntityCount` and `arkiv_getBlockTiming` use
retained native state. These JSON-RPC query responses are ordinary
**unverified reads**. The native proof API continues to prove its documented
equality subset separately.

### Paging: `atBlock`, cursors and `offset`

SDK 1.0 pages with `limit` and `offset` and sends no `atBlock`, so each page
is read at the head: an entity created or deleted between two requests shifts
the matches after it, and a row repeats or is missed. That is SDK 1.0
compatibility, not consistent paging. The consistent modes need a pinned
block: follow the `cursor` with page one's `blockNumber` as `atBlock` (the
primary mode; cursors are snapshot-bound, process-local and expire after
restart or eviction), or send that `blockNumber` as `atBlock` with every
`offset`. `offset` composes with `atBlock` and `select.count`, so offset pages
pinned to one block partition its matches and repeat one count however the
head moves, as long as the height stays retained (`--state-retention`). A
cursor carries its own position, so it continues past an offset page and an
`offset` beside it is ignored. An offset at or past the end is an empty page
without a cursor. `ORDER BY` pages are offset windows only: they carry no
cursor and refuse one (`-32005`), so pin `atBlock` to page a sorted result
consistently.

An offset is a walk: the node reads the skipped matches' ids (engine pages of
16,384) and then reads on after the last of them, all charged to the
request's read budget; `select.count` is the planned count (stored posting
sizes where they decide it), charged to the same budget. A deep offset is
refused as `-32004` (`limits`) rather than scanned for free: at once when it
is above the node's `--read-max-offset` (default 100,000, about 7 s of walk
on the Sourcify store; `0` turns it off), and otherwise before the walk when
posting sizes show that it passes more matches than the budget pays for
(1,250,000 at 20 units a match). Deeper pages follow the cursor. A query beyond
the group and literal bounds (an untyped `EXISTS(x)` is nine groups) takes
the scan fallback for its page, its offset and its `select.count` alike;
`arkiv_getEntityCount` still refuses it. `ORDER BY` is upstream's
fetch-and-sort: every page reads the key and sort cell of every match, keeps
the first `offset + limit` in order and reads only that window's rows, so its
cost grows with the match set, not the page, and a match set too large to
sort within the budget is refused as `limits`: at once when posting sizes
show more than 480,769 matches (52 units a match).

Upstream orders matches, and `ORDER BY` ties, by creating transaction, then
by entity key among the entities one transaction creates; this node orders
by record id, which agrees except inside a transaction that creates several
entities (operation order here, key order upstream). Record ids are committed
state, so this is left as it is. Open questions, divergences and follow-ups
are listed in [SDK 1.0 compatibility: open items](sdk-1.0-compatibility-todo.md).

### Supersets of the network

SDK 1.0 removed `ne()`/`!=`, `not()`/`NOT`, `atBlock`, cursors and
`getEntityCount()`, because the Arkiv network it targets answers at the head
only and has no negation. This node keeps them: typed `!=`, `NOT` (the
complement over live entities), `atBlock` on every read, cursors,
`arkiv_getEntityCount`, and predicates on `$createdAt`. Clients that rely on
them will not work against the real network.

A full follower reads its own state and forwards raw submissions to its peer.
A light node forwards JSON-RPC requests to its peer; this does not turn those
responses into locally verified proofs. WebSocket subscriptions use the same read path and follow the local
accepted head; a light node obtains the corresponding block/log views from its
peer. The chain continues to have one trusted proposer.
The `safe` and `finalized` block tags are compatibility aliases for the local
head, not consensus-finality claims. `eth_syncing` currently returns `false`;
use the signed status endpoint to inspect follower progress and health.

## Boundaries and resource limits

This is an application compatibility layer, not an EVM execution engine.
Block hashes, transaction/state/receipt commitments and authenticated state
roots remain native Arkiv commitments, not Ethereum RLP/MPT commitments.
Existing native REST transactions also retain their native authentication.
An Ethereum consensus client cannot validate these blocks as Ethereum blocks.

HTTP requests are limited to 1 MiB with a five-second body timeout. JSON-RPC
calls over HTTP and WebSocket do not use the native REST API's four admission
slots, and HTTP has no fixed concurrent-request gate. Blocking engine work runs
on Tokio's blocking pool and waits for the node mutex without a limit: requests
poll it one at a time, behind block production or a follower's sync when
either is waiting, alternating with native reads; concurrent requests do not
imply parallel state execution. Excess demand can increase queueing latency
and memory use rather than receiving an immediate `ServerBusy` response.
Native REST admission and WebSocket connection limits remain separate.

`arkiv_query`, `arkiv_getEntity` and `arkiv_getEntityCount` are the exception: they are detached
reads. They hold the node mutex only to check the height and pin its snapshot,
then evaluate on that snapshot after releasing it, so a long count or scan no
longer delays other requests or block production. At most
`--read-concurrency` (default 4) detached reads run at once; another is refused
immediately with code `-32010`, message `ServerBusy`, and a single HTTP request
also gets status 429 (a batch keeps per-entry errors). A detached read waits
for the node mutex to pin its snapshot for about a second, like a native read,
and then answers the same way; it used to wait up to `--read-deadline-secs`.
A detached read running
longer than `--read-deadline-secs` (default 30) answers code `-32000`, message
`ReadDeadlineExceeded`; one whose HTTP client disconnects is abandoned. Either
way its walk stops at the next metering charge. Results, budgets and cursors
are otherwise unchanged.

Both are evaluated by a cost-based plan: each AND-group is driven by its
smallest index posting whatever the clause order, other postings are checked
by id instead of loading rows, and counts use the stored posting sizes where
those decide them. The answers are exactly the clause-order evaluation's. The
genesis bounds of 8 groups and 8 literals per group apply unless the node sets
`--read-max-groups` (1–64) or `--read-max-literals` (1–256), and an `offset`
may skip at most `--read-max-offset` matches (default 100,000; `0`: no limit).
HTTP batches contain at most 64 requests and total
JSON responses are capped at 8 MiB. WebSocket sessions and subscriptions have
additional bounded limits and disconnect rather than silently skip blocks if
they fall too far behind.

Queries have a shared 25-million-unit read budget: a request's count, offset
walk and rows all draw on it. Larger Boolean expressions
can use a bounded scan of at most 4,096 rows when the native index query limit
cannot represent them. Pagination caches at most 1,024 native cursors; cursors
expire after restart or eviction and bind to the chain, snapshot, query and
projection. Callers can restart pagination at the original block height.

Log ranges contain at most 256 blocks; clients should split wider requests.
Transaction/block hash lookup uses a derived in-memory index rebuilt from
retained blocks before listening after startup. It is capped at one million
block entries and one million transaction entries. Hitting the cap disables
hash lookup with an explicit RPC error, without stopping native chain startup or
production. Rebuilding this derived index adds work proportional to retained
history at startup. It does not replace the
retained canonical data. Bodies are never pruned, so the index covers every
block; with `--state-retention` logs and receipts below `receiptFloor` are
refused with JSON-RPC error -32006 (see the [pruning design](pruning-design.md)).
There is no snapshot bootstrap.

## Validation

The repository includes independent viem-signed authentication fixtures,
producer/full replay tests, real prefilled-state query tests, RPC simulation and
rollback/event tests, and an SDK end-to-end smoke:

```sh
cargo test --workspace
cargo build -p arkiv-node --bin arkiv-proposer
ARKIV_SDK_DIR=../arkiv-sdk-js python3 scripts/udc-sdk-smoke.py
```

The smoke script uses isolated temporary producer, full and light nodes. It does
not reset or deploy a public chain.

## Full released indexer acceptance

The regular Arkiv Chain Indexer (`Arkiv-Network/arkiv-chain-indexer`, the UDC
indexer, v0.6.x) is exercised by `scripts/full-indexer-smoke.py` through that
checkout's own, unmodified `docker-compose.yml`. Run it with Docker Compose, Bun,
the release `arkiv-proposer` and an SDK 1.0.x checkout (what
`scripts/udc-sdk-smoke.py` supports):

```sh
cargo build --release -p arkiv-node --bin arkiv-proposer
ARKIV_INDEXER_DIR=../../arkiv-network/arkiv-chain-indexer \
ARKIV_SDK_DIR=../arkiv-sdk-js \
python3 scripts/full-indexer-smoke.py
```

`ARKIV_INDEXER_DIR` defaults to `../../arkiv-network/arkiv-chain-indexer`
(relative to this repository), `ARKIV_SDK_DIR` to `../arkiv-sdk-js` and
`ARKIV_PROPOSER_BIN` to `target/release/arkiv-proposer`. Configuration goes
through the environment and an empty `--env-file` in a temporary directory, so
the indexer checkout's own `.env`, `rpc-keys/` and `baseload-config/` are never
read or written. The containers reach the node as `host.docker.internal`, so its
public listener binds to the Docker default bridge gateway (`docker0`, normally
`172.17.0.1`; never a public interface) and the control listener stays on
loopback. The user running it needs Docker access (the `docker` group).

This builds the indexer images from the checkout, launches an isolated
three-entity prefill chain (the `udc-sdk-smoke.py` chain: 500 ms blocks and a
200,000-unit per-block storage cap) and runs the scanner, backfill scanner, gap
filler, the released UDC decoder (`arkiv-transaction-decoder:v0.3.0`),
PostgreSQL, Redis, aggregators, statistics collector, backend and frontend. The
UDC SDK smoke's `exercise` stage drives the chain; then the script checks
decoded SDK operations, receipt logs, balance readings, every block from 0
(forward scan plus backfill), genesis import over RPC, projected entity metadata
on `/shadow-rpc/experimental` against the node's `arkiv_query` at the same
block, aggregates, statistics, the frontend proxy and persistence across an
indexer restart. The index withholds `bytes` attribute values by design (it
keeps their size and keccak256), so those values are nulled on the node side
before the comparison; everything else must match exactly. Disposable fixture
keys and a randomly named compose project are used, and its containers,
volumes and built images are removed afterwards; deployed chains are not
modified.

Validated on 2026-11-17 against arkiv-chain-indexer `9f424c6` (v0.6.2) with
`@arkiv-network/sdk` 1.0.1 (the `develop` checkout `d464c8d`, the same code as
`main` `e9fa8dd` and the npm release): every check passed in 50 to 70 seconds
with cached images (about 75 blocks, 22 transactions, 20 decoded operations,
a delete among them, 3 genesis entities imported), before and after `ORDER BY`
joined the exercise. `scripts/udc-sdk-smoke.py` passed against the same
checkout through its exercise, three-role restart and follower stages.

The indexer's genesis importer uncovered two additional RPC requirements:
`arkiv_getEntityCount([{block: 0}])` reads the requested snapshot (with optional
`query` filtering), and `arkiv_query` accepts numeric as well as hex page limits.
These are implemented in the engine, without a custom indexer adapter.

Existing chains retain their original genesis clock and transaction history.
Old native-wire transactions do not have Ethereum execute calldata and are not
reconstructed as Arkiv operations by the standard decoder. New SDK transactions
and genesis-bundle prefills pass the full pipeline. Current-time explorer charts
require a genesis start time appropriate to the actual deployment.
