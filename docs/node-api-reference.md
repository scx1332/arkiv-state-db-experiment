# Arkiv Node API reference

Every signed node serves the same Scalar browser at `/docs` and OpenAPI 3.1
specification at `/openapi.json`. The reference covers the public native REST
API, JSON-RPC, WebSocket subscriptions, private administration, and optional
Prometheus metrics. The files are embedded in the node binary. No DNS entry,
frontend build, or additional service is required.

## Choose the node and listener

Start with `GET /signed/v1/status` to identify the node's role and capabilities.
The default server is the public origin hosting the docs. Change the server in
Scalar to your light node when using verified queries. Cross-origin requests
need appropriate browser access; use Scalar's generated curl command when
mixed-content or CORS restrictions prevent browser execution.

| API | Node role | Listener |
| --- | --- | --- |
| Status, genesis, headers, query classification and request resolution | All | Public |
| Feed blocks, prefill, accounts, nonces, request status, raw proof generation, tree exploration | Producer/full | Public |
| Native signed transaction submission | Producer | Public |
| Verified account, equality, range, inspection and text-query execution | Light | Public |
| JSON-RPC at `/` | All; reads on light nodes are forwarded without proof verification | Public |
| WebSocket subscriptions at `/` | Producer/full; light nodes reject subscriptions | Public |
| Replication blocks and node control | Producer/full; production controls require producer | Separate `--private-listen` |
| Prometheus `/metrics` | When enabled | Separate `--metrics-listen` |

Private operations have their own loopback server settings and require the exact
bearer token from `--token-file`. Replace the illustrative port with the actual
listener or an authorized tunnel. Merely documenting these operations does not
add them to the public listener or public reverse proxy. Metrics has a separate
loopback server setting and no application bearer authentication. Both listeners
must remain restricted by the deployment's network controls.

## Requests and results

Native REST uses decimal strings for heights, balances, nonces and most IDs;
canonical binary payloads are hex strings starting with `0x`. They use the Arkiv
binary codec, not base64 or JSON. Resolve equality/range requests before passing
the resulting `request` bytes to a full node's proof-generation endpoint.
Most native REST requests have a 64 KiB cap and responses an 8 MiB cap; feed,
inspection and tree exploration have additional documented limits.

Verified endpoints require a light node that has already synchronized the
requested header. Equality continuations are opaque, bounded in-memory cursors
on that same light node. Range proofs are complete and unpaged and cover every
indexed type (`bool`, `str`, the fixed-width byte types and every numeric and
time type) in the engine's byte order; see `docs/range-proofs.md`. The text
query language and SDK JSON-RPC keep the reference Arkiv grammar, which allows
`<`, `<=`, `>`, `>=` only on numeric values. Text-query
failures may be HTTP 200: inspect `error` and `verificationStatus`.

JSON-RPC uses a JSON-RPC 2.0 envelope at `POST /`, with an example and parameter
reference for every implemented method. It supports batches and notifications.
The Ethereum compatibility profile does not provide an arbitrary EVM or Ethereum
consensus/MPT commitments. Check each SDK method's block selector encoding;
these are not uniformly native decimal strings or Ethereum hex quantities.

The `GET /` operation describes a WebSocket upgrade and includes the
`eth_subscribe` / `eth_unsubscribe` message shapes. Use a WebSocket client for
`newHeads` and `logs`; Scalar's HTTP request client cannot maintain subscription
sessions. The private control GET alias requires a JSON body and is not usable
with browser Fetch; use POST for control operations.

State history is served according to the node's `--state-retention`
(archive by default). `status` reports `stateRetention`,
`stateRetentionWindow`, `stateFloor` and `receiptFloor`. State reads below
`stateFloor`, and logs, receipts and block JSON (which carries `logsBloom`)
below `receiptFloor`, are refused: REST answers `410 Gone` with
`{"error":"HistoryPruned","requestedHeight":"N","stateFloor":"F"}`, JSON-RPC
answers code `-32006` (the SDK's `block` kind, on every method) with message
`history pruned: requested N, floor F` and hex `data.requested`/`data.floor`. Bodies, headers and transactions stay
available at every height. See the [pruning design](pruning-design.md).

Entity queries and counts (`arkiv_query`, `arkiv_getEntityCount`), raw proof
generation (`query/eq`, `query/range`) and tree exploration are detached reads:
they run against a snapshot pinned under the node lock, outside it. At most
`--read-concurrency` of them run at once; another answers `429` with
`{"error":"ServerBusy"}` (JSON-RPC: HTTP 429 for a single request, code
`-32010`, message `ServerBusy`). One running past `--read-deadline-secs` answers
`503` with `{"error":"ReadDeadlineExceeded"}` (JSON-RPC: code `-32000`, message
`ReadDeadlineExceeded`). Both are safe to retry; a deadline usually means the
read itself is too large for the node's limit.

`arkiv_query` and `arkiv_getEntityCount` use planned evaluation: each
AND-group is driven by its smallest index posting and checks the other
postings by id, so `scope AND address = X` costs what `address = X AND scope`
does; counts of one posting, a match-all, a `!=`, a prefix or a union of
distinct values are answered from stored posting sizes. Results, cursors and
counts are exactly those of the clause-order evaluation. Queries are admitted
with the genesis group and literal bounds (8 and 8) unless the node runs with
`--read-max-groups N` (1–64) or `--read-max-literals N` (1–256); a larger
query still takes the bounded scan fallback (`arkiv_query`) or is refused
(`arkiv_getEntityCount`, code `-32004`, kind `limits`). These bounds are
node-local: block execution never evaluates an SDK query.

Before the entity namespace exists (a chain without a prefill, at height 0,
before the proposer's bootstrap in block 1) `arkiv_query` answers an empty
page and `arkiv_getEntityCount` answers `0`. A page whose entities would
exceed the 8 MiB response cap is shortened instead: it returns the entities
that fit, in order, with a cursor that continues right after the last one
returned (the node re-reads the same pinned snapshot with the shorter limit,
so paging is unchanged).

## Entities: user-defined columns

The node serves the SDK's user-defined-columns (UDC) entity model (protocol
version 5, entity rules `arkiv-entity-v3`). An entity is its system fields
plus named, typed user attributes; there is no payload or content type.

- **Writes.** Each ABI attribute triple is `(bytes32 name, uint8 typeId,
  bytes value)`. The low seven bits of `typeId` are the type (`bool` 1, `i32`
  2, `u64` 3, `u256` 4, `dec` 5, `bytes32` 6, `bytes` 7, `str` 8, `addr` 9,
  `key` 10) and bit 7 (`0x80`) asks for an index: `0x88` is an indexed `str`,
  `0x08` an unindexed one. `bytes` is stored but never indexed, so `0x87` is
  refused. Type 0 is the patch-only tombstone, with an empty value and never
  the flag (`0x80` is refused). A patch that sets an attribute stores it with
  the flag of its new triple, so a patch can index or unindex an attribute.
- **Names and sizes.** User names match `^[A-Za-z][A-Za-z0-9._-]*$`, are at
  most 32 bytes, contain no `--` and are not a reserved word; `arkiv.schema`
  (the SDK's named-schema tag) is an ordinary user name. `$` names belong to
  the host. A create or patch carries at most 32 triples, sorted strictly
  ascending by name. A `str` is at most 128 bytes and a `bytes` value at most
  128 KiB; the `bytes` values of the resulting entity, after a create or
  patch, may hold at most 128 KiB together. A larger entity is rejected with
  the user fault `EntityTooLarge` (the batch fails; nothing panics). The
  removed `$payload` and `$contentType` are not writable, removable or
  queryable names.
- **System cells.** Every entity carries seven system cells (`$key`,
  `$creator`, `$owner`, `$createdAt`, `$updatedAt`, `$expiresAt`,
  `$creationFlags`). A signed genesis must admit 39 to 64 cells per record
  (`max_cells`), so the system cells never reduce the 32-attribute budget.
- **Reads.** `arkiv_query` returns the SDK's `RpcEntity`: `attributes` holds
  every selected user attribute, indexed or not, `bytes` values included,
  and `attributeSchema` entries are `{name, type, indexed}` with the type tag
  (`"str"`, `"bytes"`, …). `select` takes exactly the SDK's `RpcSelect` keys
  (`key`, `owner`, `creator`, `createdAt`, `updatedAt`, `expiresAt`,
  `creationFlags`, `attributeSchema`, `attributes`); any other key, including
  `payload` and `contentType`, is invalid params (`-32602`). `attributes` may
  also map names to booleans, which selects unindexed attributes by name too.
- **Predicates never match unindexed attributes.** Only indexed attributes
  enter the index, so `title = str('x')`, `EXISTS(title)` or `TYPEOF(title)
  = str` never match an entity whose `title` is unindexed, and a query never
  filters on `bytes`. The attribute is still returned. Both the planned index
  path and the bounded scan fallback apply this rule, and verified equality
  proofs of an unindexed attribute have no postings.
- **Gas and rent.** Gas prices execution only. Storage occupancy, which counts
  every cell's name, value and two tag bytes whatever its name, plus 128 units
  per indexed cell, is paid as **rent** in GLM per second of capped lifetime
  (20 gwei per unit per day), a separate debit from the sender that counts
  against neither the transaction nor the block gas limit
  ([charging rent](glm-gas-v1.md#charging-rent)):
  - `eth_estimateGas` returns execution gas only and ignores affordability, so
    the SDK's ordinary calls work unchanged; a 128 KiB permanent create
    executes in under a million gas.
  - **Rent is bounded only by the sender's balance.** A wallet's `gas × price
    + value` covers the fee and value, not the rent, which is charged on top
    from the balance left after the fee reservation. An operation risks about
    1 GLM at most (a full 128 KiB entity for the one-year cap), and the rent
    charged can exceed the quote: an extension across the cap costs one block
    of its entity more each block, and an extender pays the entity's size
    when it executes, so the owner growing a permissionless entity in the
    same block, first, raises it. A signed allowance (the transaction value)
    is planned for when the SDK can set it
    ([what bounds rent](glm-gas-v1.md#what-bounds-rent)).
  - `eth_sendRawTransaction` (and native submission) first checks the nonce,
    sizes, pool capacity and the fee and value against the sender's balance
    and pending reservations, then quotes the transaction's rent (its
    operations in order, each seeing the earlier ones of the same
    transaction, with a 64-block height margin) and refuses a sender who
    cannot cover that too: `-32000` / `insufficient funds for gas * price +
    value + rent`, with no transaction, nonce or fee.
  - Execution checks rent again. An operation whose rent the sender cannot pay
    after its gas reservation fails with `InsufficientRentBalance`; its
    transaction rolls back (status `0x0`, gas charged, no rent). A pending
    transaction its sender can no longer cover once an earlier one paid more
    rent than quoted is dropped: `eth_getTransactionByHash` and
    `eth_getTransactionReceipt` then answer `-32000` / `transaction dropped:
    insufficient funds for gas * price + value + rent`, and native request
    status reports `"status": "dropped"` with the reason.
  - `eth_getTransactionReceipt` adds `rentFee`, the wei the transaction burned
    beside `gasUsed × effectiveGasPrice` (`0x0` when it failed); native request
    status adds `rentChargedWei`. Logs are unchanged.
  - **Storage per block.** A block may add at most
    `maxNewStorageUnitsPerBlock` storage units (creates' entities, patches'
    growth; default 1,500,000, which no block the default input cap admits
    reaches). A transaction that would cross it waits for a later block; one
    that alone exceeds it is refused at admission with `-32000` /
    `transaction adds more storage units than a block may
    (max_new_storage_units_per_block)` ([storage per block](glm-gas-v1.md#storage-per-block)).
  - A signed transaction may hold 384 KiB, its native envelope 256 KiB, so an
    Ethereum transaction (whose original bytes travel beside the envelope)
    carries a full 128 KiB entity.
- **Lifetimes and permanence.** An entity expiry has no upper bound: any
  finite height, or `expiresAt = 2^64 − 1` (`u64::MAX`, the SDK's
  `ExpirationTime.permanent()`), is accepted in a live create or extension
  and never refused as too far. A `u64::MAX` entity is never scheduled for
  expiry and never swept; reads report `expiresAt` as `0xffffffffffffffff`
  and its `EntityCreated`/`ExpiryExtended` logs carry the same word. Rent is
  capped: an operation charges at most `maxRentLifetimeSeconds` ahead (status
  `capabilities.gas`, the genesis `max_rent_lifetime_seconds`, by default a
  year; `maxRentLifetimeBlocks` is that at the chain's block period), so a
  year, two years and permanence cost the same
  ([rent cap](glm-gas-v1.md#the-rent-cap)). Only the owner may extend an
  entity beyond one rent cap ahead: a permissionless-extension entity takes
  anyone's extension up to `height + maxRentLifetimeBlocks`, but another
  account asking further, permanence included, is refused with the revert
  `PermanenceOwnerOnly(bytes32 entityKey, uint64 requested, uint64 limit)`
  (the SDK's ABI does not name it yet, so it reports the selector). Extending
  a permanent entity to permanent is a no-op success that stamps
  `$updatedAt`; any finite expiry is `ExpiryShortened`. A finite entity may be
  extended to permanent, paying the growth of its capped horizon. Patches,
  transfers and deletes of permanent entities work as for any entity.

## Query errors

`arkiv_query` and `arkiv_getEntityCount` answer refusals with the SDK's code
table (`src/query/errors.ts`), with a message naming the kind:

| Code | Kind | Meaning |
| --- | --- | --- |
| `-32001` | `parse` | The query does not parse. `data` is `{position, expected, got}`: the UTF-8 byte offset, what the parser expected, and the source text there (`end of query` at the end). |
| `-32002` | `type` | A predicate is ill-typed: a range operator on an equality-only type, an unknown or unindexable type tag (`TYPEOF(x) = bytes`), a system attribute compared against the wrong type, `EXISTS($owner)`, or an unknown system attribute such as `$payload` or `$contentType`. `data` is `{position, got}`. |
| `-32003` | `literal` | A literal fails validation: an `i32` out of range, a bad checksum, more than 18 decimal places. `data` is `{position, got}`. |
| `-32004` | `limits` | A node budget is exceeded: more than 8192 bytes, 64 predicates or 32 levels of nesting, a Boolean expansion beyond 64 groups or 256 literals, a count beyond the read's group/literal bounds, an exhausted read budget, or a single entity whose JSON alone exceeds the 8 MiB response cap. |
| `-32005` | `cursor` | The cursor expired, is malformed, or belongs to another query, block or selection (cursors also need an explicit `atBlock`). |
| `-32006` | `block` | The requested height is above the head or below the state floor (`HistoryPruned`, with `data.requested`/`data.floor`). |

An engine refusal that is not the query's fault answers `-32000`
(`ReadRejected`). No other error uses these codes: invalid parameters are `-32602`,
`ServerBusy` is `-32010`, a WebSocket subscription past its per-connection
cap is `-32011`, `ReadDeadlineExceeded` and other runtime errors are
`-32000`. Cursor paging is unchanged by the UDC model.

The existing [tree exploration guide](state-tree-explorer-api.md) explains
navigating catalog, records, keys, rows, terms and postings. Tree responses are
diagnostic data, not light-node proofs.

## Maintain the reference

Edit `crates/arkiv-node/src/signed/state-openapi.json` with any wire-format or
route change. Its historical filename is retained to avoid needless churn.
`state-docs.html` loads the pinned Scalar bundle directly from jsDelivr and
provides a specification download link if the CDN is unavailable. Requests go
directly to the selected server; no Scalar proxy is used and credentials are
not persisted by the reference.

Coverage tests check native routes, JSON-RPC method inventory, private listener
boundaries, unique operation IDs and embedded schema references. The signed
HTTP tests also exercise the served documentation while the database is locked.
