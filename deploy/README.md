# API documentation hosting

The signed node serves Scalar at `/docs` (also `/docs/`) and its node API
OpenAPI specification at `/openapi.json`. Serve both paths through the existing
node hostname; no additional DNS record is needed. Public requests use the same
origin by default. Select a node with the role required by the operation.

The reference also describes WebSocket subscriptions, the separate metrics
listener and bearer-authenticated private control/replication operations.
Documentation does not publish those listeners. Keep the existing public proxy
restrictions for `/signed/v1/control` and `/signed/v1/replication/` in place;
private operations require the operator's separate listener and credentials.

For an existing Arkiv v2 deployment, the two static assets can also be published
without replacing or restarting a node:

1. Copy `crates/arkiv-node/src/signed/state-docs.html` and
   `crates/arkiv-node/src/signed/state-openapi.json` to a release directory under
   `/var/www/arkiv-state-db-docs/`.
2. Point `/var/www/arkiv-state-db-docs/current` to that release directory.
3. Include `nginx-api-docs.conf` inside the existing full-node TLS server block.
   Keep the existing `/signed/v1/` upstream route.
4. Run `nginx -t`, then reload Nginx. Verify `/docs`, `/openapi.json`, and a
   read-only request from Scalar's client.

The specification describes Arkiv v2 path limits. The signed-node API and its
documentation routes require the `arkiv-v2` build feature.

Scalar's JavaScript is loaded from the version-pinned jsDelivr URL in the HTML.
The schema and API calls stay on the node origin. Publishing these assets does
not require changing or redeploying the block explorer or state tree explorer.

## Deployment verification (2026-09-29)

Published at <https://full-node.rogue-one.eu/docs>, with the specification at
<https://full-node.rogue-one.eu/openapi.json>. `/docs/` redirects to `/docs`.

- The two focused Rust HTTP tests passed with
  `cargo test -p arkiv-node --features arkiv-v2 --test signed_http state_`.
- OpenAPI 3.1 structural validation passed. Real full-node responses for all six
  map kinds matched the response schema at one pinned height, including decoded
  catalog, key, row, term and posting leaves and an empty records map.
- Headless Chromium loaded the deployed Scalar page without JavaScript errors
  and sent its catalog example directly to the same-origin API, receiving 200.
- Nginx configuration validation passed. Deployment only installed static
  documentation assets and added exact routes to the full-node virtual host.
  Node and explorer containers were not restarted.

### Expanded node reference

The same URLs now cover 30 paths / 33 HTTP operations, all 31 HTTP JSON-RPC
methods, and WebSocket subscription messages.

- Three documentation coverage tests and all ten signed HTTP integration tests
  passed with `--features arkiv-v2`.
- OpenAPI structure and all 31 RPC request examples validated. Live checks
  matched 25 native REST responses and 23 read-only JSON-RPC method results to
  their schemas, including populated feeds, included transactions, account
  proofs, nonempty equality/range results, and proof inspections.
- Chromium exercised status, tree exploration, and JSON-RPC through Scalar on
  the deployed origin. Desktop and mobile rendering passed without JavaScript
  errors or mobile horizontal overflow.
- Published HTML/specification bytes match the repository assets. Public
  requests to control and replication routes still return 404. Deployment only
  switched the static-asset release; no node or explorer restart was needed.
