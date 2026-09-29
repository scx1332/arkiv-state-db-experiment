# Complete index-backed numeric ranges

`range-complete-v1` proves one typed numeric attribute range at one selected
snapshot. It supports inclusive/exclusive lower and upper bounds, or one
unbounded end. At least one bound is required and both ends must have the same
numeric type. The engine supports `i64` and `u64`, plus `i32`, `u256` and `dec`
under `arkiv-v2`; the signed Arkiv HTTP adapter accepts its existing scalar
profile (`u64`, `i32`, `u256`, `dec`). Reversed bounds and equal excluded bounds
produce a complete empty result. Different types, fields, and missing attributes
do not match. All cells and keys of matching rows are returned in RecordId order.

There is **no pagination**: no query limit, offset, cursor or continuation.
An oversized result returns `ProofLimitExceeded` or `BudgetExceeded`; it never
returns a successful truncated result. Default node policy is at most **1,000
distinct matching terms and 1,000 matching rows**, 4096 total posting memberships,
32,768 supplied trie nodes, and `(8 MiB - 4096) / 2` binary proof bytes (to leave
space for the hex-encoded HTTP envelope). Verification work is capped at 512 Mi units and cumulative allocation
reservations at 128 MiB. Full nodes
also apply a 10,000,000-unit proof-serving budget. These conservative caps can
reject large payloads even when their row count is small. Local engine callers
can supply `RangeProofLimits`; caps are admission policy, not query semantics.

### Exploring limits

These defaults are an experimental starting point, not a protocol maximum.
Operators can set `ARKIV_RANGE_MAX_ROWS`, `ARKIV_RANGE_MAX_TERMS`,
`ARKIV_RANGE_MAX_POSTING_IDS`, `ARKIV_RANGE_MAX_NODES`,
`ARKIV_RANGE_MAX_VERIFICATION_WORK`, and `ARKIV_RANGE_MAX_ALLOCATED_BYTES`
on both full and light processes. Values must be positive integers; invalid
values fail requests with `InvalidRangeLimits`. Each process enforces its own
policy. Increasing only the row count may expose another resource limit.
The browser accepts the server's complete result without a second hard-coded
row-count limit. The hosted 8 MiB HTTP envelope, proof-serving budget, and
inspector's separate 1 MiB raw-proof / 8 MiB JSON limits still apply.

Use the opt-in engine experiment to measure larger proofs independently of
those transport and inspector limits:

```sh
RANGE_BENCH_ROWS=10000 cargo test --release -p arkiv-db --features arkiv-v2 \
  --test range_proofs range_scaling_experiment -- --ignored --nocapture
```

Measured locally on 2026-09-27, one run per case, optimized build, in-memory
records with one numeric attribute and an 8-byte key (no entity payload):

| Rows | Distinct values | Binary proof bytes | Generate, including internal verification | Verify |
| ---: | ---: | ---: | ---: | ---: |
| 1,000 | 1 | 2,623,658 | 42 ms | 34 ms |
| 1,000 | 1,000 | 2,834,296 | 55 ms | 38 ms |
| 10,000 | 1 | 35,558,529 | 493 ms | 399 ms |
| 10,000 | 10,000 | 37,665,806 | 609 ms | 456 ms |

These are engine measurements, not hosted end-to-end latency or a production
capacity guarantee. Fixtures exclude disk reads, networking, concurrent load,
large payloads and expanded inspection JSON. No maximum engine capacity was
established. At 10,000 rows the current HTTP transport cannot carry these
proofs; repeated per-row authentication paths are a clear size cost to measure
before choosing compression, shared-node multiproofs or larger envelopes.

The full node walks only term-index prefixes intersecting the requested range,
then matching posting sets, then the selected rows and key bindings. It never
scans the row table to select matches. An interval witness opens every hash
child whose encoded-key prefix intersects the bounds, in depth-first key order.
Out-of-range subtrees remain opaque. Empty gaps are authenticated by the same
walk. The verifier rejects missing boundary/interior nodes, extra/reordered
nodes, malformed paths and truncated proofs. It reconstructs each term's entire
posting trie, rejects duplicate IDs across terms, authenticates every selected
row and key binding, and checks the row's attribute against its exact term.

The query digest binds namespace, attribute, typed bounds, inclusivity, ordering
and projection. The response also binds the caller-selected genesis, height,
block hash and state root. The signed light client anchors this state root in an
accepted producer certificate and applies existing Arkiv row/lifetime checks.
As with equality proofs, completeness relies on the authenticated execution
invariant that indexes describe live rows; this is not a consensus or execution
proof. State layout, equality proof bytes, write rules and query planning are
unchanged. No migration or chain reset is required. The text-query grammar is
unchanged; ranges use the dedicated API. The separate light-node frontend now
exposes runnable numeric-range examples at `/queries` and a walkthrough at
`/range-explained`.

## Signed HTTP API

Send this request to `POST /signed/v1/query/range/verified` on a light node:

```json
{
  "height": "42",
  "namespace": "1",
  "attribute": "score",
  "valueType": "u64",
  "lower": {"value": "10", "inclusive": true},
  "upper": {"value": "20", "inclusive": false}
}
```

Omit `lower` or `upper` for a one-sided range. Numeric values and height/namespace
use the same strict string encodings as equality requests. Unknown fields,
including `limit` and `cursor`, are rejected. A successful response includes
`rows`, `termCount`, `postingCount`, `complete: true`,
`proofProfile: "range-complete-v1"`, the snapshot and certificate. Namespace
absence is `NamespaceNotFound`, while an existing namespace with no matches
returns an empty `rows` array.

For independent verification, send the same JSON to
`POST /signed/v1/query/range/resolve` on a full or light node to obtain
`{"request":"0x…"}`. Send that canonical request to
`POST /signed/v1/query/range` on a full node to receive `{"proof":"0x…"}`.
Decode the canonical request as `arkiv_db::proof::RangeRequest` and call
`arkiv_db::proof::verify_range(&request, &proof_bytes, caps, limits)`, after
independently accepting its snapshot identity and state root. The engine's
producer entrypoint is `Snapshot::prove_range`.

Validation covers a separate sorted-map trie oracle (including inline nodes,
long prefixes and empty gaps), independent row-model numeric boundary/extrema
checks, byte-identical proofs from reopened snapshots, unavailable unrelated
term/row nodes, malicious omissions/substitutions/truncations, and every resource
cap. `scripts/range-proof-smoke.py` starts isolated producer/full/light processes
to exercise the real HTTP flow and restart persistence.

## Inspect the exact branching witness

`POST /signed/v1/query/range/inspect` on a light node accepts the same selection
as `/query/range/verified`. After the signed header, lifetime, completeness, row
and key checks pass, it adds `canonicalRequest`, `canonicalProof`, and
`inspection`. Normal range proofs and their canonical bytes are unchanged.

The inspection preserves the equality inspector's `stateComposition` and
`pointPaths` formats for the catalog, rows and key bindings. `interval.nodes`
contains the actual depth-first terms-trie traversal. Each node identifies its
parent trace index and branch slot, exact RLP, hash, compressed path and prefix.
`proofIndex` identifies a supplied witness entry; an inline node shares the
index of the supplied ancestor containing its bytes. Child references distinguish
`open`, `outside`, and authenticated `empty` decisions. An outside hash reveals a
commitment and key prefix, not its descendant values. A visited leaf can be
outside the final bounds: for example, the exclusive boundary 20 may need to be
opened for `10 <= price < 20`, but is not included in the result.

`postingSets` links each included term node to its entire record-ID list and
matching authenticated/reconstructed posting roots. These posting tries are
rebuilt from the IDs; their nodes are not supplied separately. Empty gaps use
parent-authenticated empty slots, divergent paths and excluded boundary leaves.
Missing intersecting hash children always fail verification.

Inspection admits at most 1 MiB of binary proof and 16,384 visited interval nodes
including inline children, with the usual 8 MiB final JSON response limit.
Trace copies and decoding consume the engine's existing allocation/work budgets.
Exceeding any limit rejects the whole request; a successful trace is never
truncated. This display data does not add a browser verifier or change the trust
model: the hosted light node verifies against its authenticated proposer header.

The light-panel `/proof-inspector` defaults to this range trace and reuses the
existing Patricia inspector for catalog, row and key paths. Its supplied-node
list selects actual interval nodes; the initial tree focuses on the first
multi-child fork while earlier shared-prefix nodes remain reachable. The page
shows excluded boundary leaves separately from matching results and preserves
an explicit equality mode. Live read-only desktop/mobile checks passed on chain
9009 after upgrading only the light process and its panel, with chain identity
and retained headers preserved.
