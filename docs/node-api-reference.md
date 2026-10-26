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
version 2, entity rules `arkiv-entity-v3`). An entity is its system fields
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
- **Gas.** Storage occupancy counts every cell's name, value and two tag bytes,
  whatever its name; only indexed cells add the 128-unit posting reservation
  (see [GLM gas](glm-gas-v1.md)).

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
