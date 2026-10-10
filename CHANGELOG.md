# Changelog

## 0.1.0-alpha.1 — 2026-10-10

First tagged pre-release. It's an experimental database and node for Arkiv,
with no stability guarantees. The on-disk formats, wire formats and state
roots may change in any later release without migration, and there is no
consensus or finality.

### Engine
- An ordered, metered block engine on Merkle Patricia state, with typed
  records and attribute indexes inside each namespace.
- Queries with keyset pagination, pinned to one snapshot. Range queries work
  on every value type (numeric, bool, string, bytes).
- One profile: 32-byte cell names, 128-byte indexed values, all golem-db value
  types, and limits set at genesis. The only gas profile is `glm-v2`.
- A durable engine on redb (fsync on every commit), with a decoded-node cache
  and concurrent readers alongside the single block producer.
- State retention: `--state-retention archive|N`, archive by default.
  - Reads more than N snapshots below the head are refused with
    `HistoryPruned`: JSON-RPC `-32001`, REST `410 Gone`.
  - The floor is saved to disk and never moves back.
  - Nothing is physically deleted yet; see `docs/pruning-design.md`.

### Node
- `arkiv-proposer`: a signed single-proposer chain (producer, full and light
  followers) for the Arkiv v2 SDK entity model, with genesis bundles and
  replication.
- `arkiv-simulator`: an unsigned simulator node with its own stores.
- `arkiv-demo`: a small demo of a producer with full and light followers.
- Every binary accepts `--version`.

### golem-db conformance
- An adapter and kit for golem-db-conformance-testing, in `conformance/golem-db`.
  On golem's ported test suite, 16 tests pass and 5 fail; all 5 failures
  concern golem's internal system records.
- Benchmarks against golem-db 57c48ca on NVMe: arkiv is 1.4–2.7x faster in
  all 13 scenarios. See `docs/benchmarks/` and `docs/benchmarking.md`.

### Assets
The release includes the Linux x86_64 (glibc) binaries `arkiv-proposer`,
`arkiv-simulator` and `arkiv-demo`, with SHA-256 checksums.
