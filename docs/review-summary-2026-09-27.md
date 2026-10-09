# Node design review — 27 September 2026

> Historical review of 2026-09-27. At that time the node had an `arkiv-v2`
> cargo feature, a generic default build and several gas profiles; the commands
> and profile statements below describe that code. The node now has one build
> (no feature flags) and `glm-v2`-only signed chains.

A coordinated review by three agents and the main reviewer covered the experimental
node in `arkiv-db-pure-astra`, plus its dedicated panels in `arkiv-chain-indexer`.
Reviewed baselines: node `7f80db66bd68d3be135e1bd387ca4979e5387743`; frontend
`88fb07b`. The source changes and reports in this review are intentionally small;
there is no new consensus mechanism, state format, fee rule or genesis.

Presentation: [self-contained HTML](node-design-review.html). It explains the
producer/full/light roles, transaction execution, typed indexing, state roots,
equality and range proofs, durable publication, recovery, GLM fees, SDK
compatibility, operations and the findings below. It has contents navigation,
an interactive transaction walkthrough, a responsive layout and print styling.
A matching public copy is maintained in the frontend at `public/node-design.html`.

## Findings fixed

### N1 — Medium: full follower could forget an advertised signed header

If an intermediate advertised certificate's block body was unavailable, the full
follower did not durably remember the certificate. A subsequent authentic
conflicting certificate at the same height could therefore escape detection.
The sampled peer head was already remembered, but that did not cover every
intermediate observation. The follower now records each authenticated certificate
before fetching the body. Signature checks and execution replay were never bypassed.

The new real-socket regression first failed on the original code, then passed with
the fix. It checks that the accepted head remains unchanged, conflict prevents a
second body fetch, and reopening history preserves the frozen state.

Evidence: `crates/arkiv-node/src/signed/runtime.rs::sync_once` and
`crates/arkiv-node/tests/signed_http.rs::advertised_header_survives_failed_body_fetch_and_detects_later_conflict`.
Full explanation: [node review](review-node-2026-09-27.md).

### U1 — Low: malformed node status could crash the panel

The status decoder accepted an object where the new `proofProfiles` array or
`proposer` display string was expected. Subsequent `.includes()` or React rendering
could throw. The client now validates both fields before passing status to the UI,
so malformed data produces the existing status-error behavior. Tests cover an
object, string and null list entry for proof profiles, plus an object proposer.

Evidence in the frontend repository: `frontend/src/nodeDebugApi.ts::fetchNodeStatus`
and `frontend/nodeDebugApi.test.ts`.

### V1 — Low: deployment checker compared missing identity objects

The signed-panel acceptance script compared `full.identity` with `light.identity`,
but status exposes identity fields at the top level. Both values were undefined,
so the assertion passed without checking identity. It now requires string-valued
source, run, genesis, chain and authentication fields and compares each one.
The emitted report also records those actual identity fields.

### D1 — Documentation drift

Updated stale claims about durable node integration, dedicated proof APIs, range
frontend availability and limited Ethereum transaction compatibility. The SDK guide
now explicitly states that `safe`/`finalized` alias local head and `eth_syncing`
returns false; these are not consensus-finality or follower-progress guarantees.

## Remaining design limits

- **Single proposer:** signed headers authenticate the configured authority, not
  multi-party consensus, finality or correct execution. Full replay independently
  checks execution; the light node does not.
- **Index consistency assumption:** completeness is relative to authenticated
  indexes that correctly describe live rows. A query proof does not detect an
  intentionally omitted index membership in a maliciously constructed state.
- **Retention:** all snapshots, bodies and publication orphans remain retained.
  No pruning, safe garbage collection or snapshot bootstrap exists. Cache limits
  are not disk limits or whole-process RSS limits.
- **Resource admission:** native REST is bounded; JSON-RPC HTTP has no equivalent
  fixed concurrent-request gate. Requests can queue behind the node mutex. This
  warrants measured admission/backpressure before higher public load.
- **Read coverage:** ordinary SDK queries are unverified. Complete equality and
  bounded numeric ranges are separate APIs; different-attribute Boolean proofs
  and paged ranges are outside the current proof profiles.
- **Startup and integrity:** engine open is bounded, while derived SDK hash lookup
  rebuilds from retained history. Old-object corruption is discovered lazily on
  access, not by an exhaustive startup scrub.
- **Cache performance:** encoded-object lookup is linear under a shared mutex.
  Its impact was not measured in this review; changing it is not an obvious
  correctness fix.

No confirmed proof-forgery or storage-atomicity defect was found in the reviewed
paths. This is not a formal audit or a guarantee that other defects do not exist.

## Evidence and validation

- [Proof and index review](review-proofs-2026-09-27.md): 79 focused tests passed.
- [Storage/recovery review](review-storage-2026-09-27.md): 58 tests passed;
  three explicitly heavy crash matrices were ignored.
- [Execution/signed-node review](review-node-2026-09-27.md): initial 197 tests
  passed; after the fix, history 22, host 16 and serialized HTTP 7 passed. An
  initial parallel HTTP rerun encountered an existing admission-timing flake;
  the serialized regression suite passed without weakening production code.
- Full frontend suite: `bun --no-env-file test frontend`: **301 passed, 16 skipped,
  zero failures**, 1,196 assertions. Network/browser opt-in suites remain skipped.
- Presentation browser acceptance: 15 sections, local anchors, contents/next
  navigation, interactive stages, mobile width and print/PDF all passed locally.
- Changed Rust files pass direct `rustfmt --edition 2024 --check`; `git diff
  --check` passes. Repository-wide `cargo fmt -p arkiv-node -- --check` reports a
  pre-existing `genesis_authoring`/`genesis_bundle` module-order difference in
  `lib.rs`, which this review leaves untouched.

Focused counts overlap; they must not be added into a unique-test total.
Final full workspace command:

```sh
cargo test --workspace --offline --features arkiv-node/arkiv-v2 -- --test-threads=1
```

**974 passed, zero failures, 3 ignored**, including documentation tests.
The ignored tests are the explicitly heavy crash/torn-write matrices.
`cargo build --release --locked --offline -p arkiv-node --features arkiv-v2
--bin arkiv-proposer` also passed. The frontend test typecheck passed.

## Live observation

Read-only status sampling before the update observed both full and light roles
following chain 9009 at block 32,092 with identical block hash and state root.
The producer, full, light and dedicated panel containers reported healthy.
No live transaction was submitted as part of the review. Private deployment
credentials were neither included in the report nor printed.

## Release verification

The fixed full-follower binary and updated full/light panels were deployed on
27 September 2026. The full follower reopened the existing database, preserved
source/run/genesis/chain identity and advanced beyond its pre-update height.
The producer and light-node processes were not restarted; genesis was unchanged.
The full follower spent roughly a minute rebuilding its derived RPC lookup index
before listening, consistent with the documented history-dependent startup work.

Public browser checks passed for the standalone presentation (desktop/mobile,
contents navigation, interactive stages and print/PDF) and both signed panels
(full block inspector, equality membership/pagination/absence, witness display,
HTTP RPC, WebSocket chain identity and restricted-route rejection). The signed
panel report was anchored at block 32,487 on chain 9009 and checked the actual
top-level identity fields. The presentation is available at
<https://light-node.rogue-one.eu/node-design.html> and linked from the light-page
footer. Local standalone artifact: `docs/node-design-review.html`.

Build output contains the existing Vite large-chunk advisory. It does not prevent
the production build; bundle splitting remains a separate optimization.
