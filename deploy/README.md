# API documentation hosting

The signed node serves Scalar at `/docs` (also `/docs/`) and its tree exploration
OpenAPI specification at `/openapi.json`. Serve both paths through the existing
node hostname; no additional DNS record is needed. The browser sends requests
directly to `/signed/v1/state/explore` on the same origin.

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
