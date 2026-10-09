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
