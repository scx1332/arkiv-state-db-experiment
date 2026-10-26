# Native engine SDK compatibility

> **Pre-UDC document.** This page and `scripts/sdk-compat-smoke.{py,ts}`
> describe and test the payload-era SDK (0.8.x: an entity payload and content
> type, `payload`/`contentType` selections). The node
> now implements the SDK's user-defined-columns model instead; for current
> write rules, read shapes and error codes see the
> [node API reference](node-api-reference.md#entities-user-defined-columns).
> The transport, gas and account behaviour below is unchanged.
>
> The current-SDK smoke is `scripts/udc-sdk-smoke.{py,ts}`: it runs the
> `udc/*` SDK branches (built with `bun run build`; point `ARKIV_SDK_DIR` at
> that checkout) against disposable producer/full/light nodes, through a
> three-role restart and follower reads.

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
semantics; `ExpirationTime.permanent()` (`u64::MAX`) is a live, owner-only
permanent lifetime that pays a year of rent. Existing native limits still apply,
including operation, attribute, namespace, lifetime and expiry bucket limits.

## Reads and subscriptions

The adapter supplies chain ID, block number, balances, account nonces, gas
quotes, fee history, blocks, transactions, receipts, logs and polling filters.
Receipts and logs are reconstructed deterministically from retained committed
blocks and prior state, including operation-order ownership changes. Failed
transactions emit no entity logs. Block gas usage includes native expiry work;
receipt cumulative gas includes user transactions only. `eth_subscribe` supports `newHeads` and `logs`.

`arkiv_query` supports the SDK's typed scalar JSON, selected fields and
attributes, historical block selection, pagination, equality and inequality,
ranges, string prefixes, Boolean expressions, `EXISTS` and `TYPEOF`.
`arkiv_getEntityCount` and `arkiv_getBlockTiming` use retained native state.
These JSON-RPC query responses are ordinary **unverified reads**. The native
proof API continues to prove its documented equality subset separately.

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
on Tokio's blocking pool and waits for the node mutex; concurrent requests do
not imply parallel state execution. Excess demand can increase queueing latency
and memory use rather than receiving an immediate `ServerBusy` response.
Native REST admission and WebSocket connection limits remain separate.

`arkiv_query` and `arkiv_getEntityCount` are the exception: they are detached
reads. They hold the node mutex only to check the height and pin its snapshot,
then evaluate on that snapshot after releasing it, so a long count or scan no
longer delays other requests or block production. At most
`--read-concurrency` (default 4) detached reads run at once; another is refused
immediately with code `-32010`, message `ServerBusy`, and a single HTTP request
also gets status 429 (a batch keeps per-entry errors). A detached read running
longer than `--read-deadline-secs` (default 30) answers code `-32000`, message
`ReadDeadlineExceeded`; one whose HTTP client disconnects is abandoned. Either
way its walk stops at the next metering charge. Results, budgets and cursors
are otherwise unchanged.

Both are evaluated by a cost-based plan: each AND-group is driven by its
smallest index posting whatever the clause order, other postings are checked
by id instead of loading rows, and counts use the stored posting sizes where
those decide them. The answers are exactly the clause-order evaluation's. The
genesis bounds of 8 groups and 8 literals per group apply unless the node sets
`--read-max-groups` (1–64) or `--read-max-literals` (1–256).
HTTP batches contain at most 64 requests and total
JSON responses are capped at 8 MiB. WebSocket sessions and subscriptions have
additional bounded limits and disconnect rather than silently skip blocks if
they fall too far behind.

Queries have a shared 25-million-unit read budget. Larger Boolean expressions
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
rollback/event tests, and an unchanged-SDK end-to-end harness:

```sh
cargo test --workspace
cargo build -p arkiv-node --bin arkiv-proposer
python3 scripts/sdk-compat-smoke.py
```

The smoke script uses isolated temporary producer, full and light nodes. It does
not reset or deploy the public chain. See the script for environment variables
selecting the local SDK checkout and optional upstream integration suite.

### Rogue One balance-independent estimation rollout (2026-09-26)

Node change `bb688b2` is deployed to the producer, full follower and light
follower on chain 9008. Existing genesis, state/history, two-second production
and public metrics endpoints were retained. Followers were upgraded and caught
up before the producer; explicit downtime catch-up added 32 real blocks.

The exact previously failing wallet-6 create (5,000-byte payload) now estimates
`0x1b53c` (111,932 gas), identical to a funded caller, on all three public
endpoints over HTTP and WebSocket. Wallet-6 remains at zero balance and nonce.
Each endpoint rejected a genuinely signed unfunded EIP-1559 transaction with
`-32000` / `insufficient funds for gas * price + value`.

A funded signed copy submitted through the full follower committed in block
31794 with 100,848 gas used and 100,848,000,000,000 wei charged. All three nodes
returned the same successful receipt for transaction
`0x66ffd5f8e178ebb7d075040b0b322edc22670d054b2ef8b2b15e55ca4059a2c1`.
The pre-upgrade block 31611 retained its hash and state root on every role.
Thirteen parsed Prometheus scrapes per role over 60.5 seconds passed type and
unique-series checks with no new error counters. The producer advanced 31
blocks; all roles agreed on block 31823, its hash and state root afterward.

Validation passed 960 workspace tests (three explicitly ignored heavy crash
tests), the unchanged-SDK lifecycle/indexer/WebSocket/restart/replay harness,
and the new 5 KB unfunded SDK-create regression. Unit coverage checks identical
funded/unfunded metering and actual execution gas, transfer value including
`U256::MAX`, pending reservations, out-of-gas and missing-entity failures,
`eth_call` account enforcement, and unchanged state/nonces/burn counter/pool.

The deployed worker now fails at `eth_sendRawTransaction`, with the correct
insufficient-funds cause; it no longer fails during estimation. The SDK still
wraps non-revert contract errors in the generic outer message
`EntityMutationError: Transaction failed: Execution error without revert data`.
That presentation issue is separate from this RPC fix; company SDK/indexer
repositories and wallet funding were not changed.

The integration references are SDK commit
`db27f4c360e1b1e081bb674224fb6858b4356091` and Arkiv commit
`0fd58ce5bd6a0a196b0079f60d4679539346adc6`. The optional upstream run copies the
SDK's integration tests unchanged to an isolated temporary directory and runs
both HTTP and WebSocket cases. Its explicit permanent-entity test is excluded
because this chain requires finite rent; the harness does not alter that policy
or rewrite the upstream test to manufacture a pass:

```sh
SDK_RUN_UPSTREAM=1 \
SDK_UPSTREAM_PATTERN='^(?!.*permanent entity).*' \
ARKIV_PROPOSER_BIN="$PWD/target/release/arkiv-proposer" \
python3 scripts/sdk-compat-smoke.py
```

The focused smoke additionally imports the existing indexer's actual
`EthereumRpcClient` for block, receipt and batched-balance checks. Neither
reference repository is modified.

Validated on 2026-09-25: 31 applicable unchanged upstream SDK integration tests
passed over HTTP/WebSocket; the single permanent-entity case was explicitly
filtered out. The complete unfiltered run identified that deliberate policy
difference. Workspace regression tests passed (947 tests in the build that then
carried the Arkiv entity protocol, 907 in the generic build; the node now has a
single build), including dependency/doctest suites, and
workspace/all-target Clippy passed with warnings denied. Existing genesis-bundle
and GLM-account real-process smoke tests also passed against the release binary.

## Full released indexer acceptance

The standard Arkiv Chain Indexer v0.5.20 image stack is also exercised by
`scripts/full-indexer-smoke.py`, using the sibling experimental checkout's
`compose.signed-chain.yml`. Run it with Docker Compose, Bun and the installed SDK:

```sh
python3 scripts/full-indexer-smoke.py
```

This launches an isolated three-entity prefill chain and the unchanged released
scanner, decoder, PostgreSQL, Redis, aggregators, statistics collector, backend
and frontend. It verifies decoded SDK lifecycle operations, receipt logs,
historical balance readings, every block, genesis import, exact projected entity
metadata and persistence across an indexer restart. Disposable fixture keys and
randomly named Docker volumes are used; deployed chains are not modified.

The indexer's genesis importer uncovered two additional RPC requirements:
`arkiv_getEntityCount([{block: 0}])` reads the requested snapshot (with optional
`query` filtering), and `arkiv_query` accepts numeric as well as hex page limits.
These are implemented in the engine, without a custom indexer adapter.

Existing chains retain their original genesis clock and transaction history.
Old native-wire transactions do not have Ethereum execute calldata and are not
reconstructed as Arkiv operations by the standard decoder. New SDK transactions
and genesis-bundle prefills pass the full pipeline. Current-time explorer charts
require a genesis start time appropriate to the actual deployment.
