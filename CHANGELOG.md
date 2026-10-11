# Changelog

## 0.1.0-alpha.2 — 2026-10-11

Alpha pre-release. **Breaking:** state layout 2. Stores, genesis files and
proofs from 0.1.0-alpha.1 are rejected at open, and there is no migration;
start from fresh stores.

### Engine
- Unpadded, prefix-free keys ([#2](https://github.com/Arkiv-Network/arkiv-state-db-experiment/pull/2), issue #1).
  - The trie accepts variable-length keys with a maximum length. No stored key
    may be a strict prefix of another, and such inserts are rejected.
  - Term keys are `name || type tag || presence || value`:
    - names and raw keys are escaped and end in `00`;
    - each string value byte is incremented by 1, with a `00` terminator;
    - scalars keep their fixed-width encodings.
  - Raw-record and record-key-binding keys are unpadded the same way.
  - Example sizes: `color = "red"` goes from 164 bytes to 12, and a `u64`
    value from 164 to 18.
- Complete range proofs take inclusive, exclusive and unbounded endpoints
  directly; the successor and predecessor arithmetic on padded keys is gone.
- The indexed-string limit is separate from the key encoding. The default
  and Arkiv limit stays at 128 bytes, and a generic instance can configure
  up to 512. Verifiers must use the instance's committed genesis limits.
- Metering is unchanged (model version 3).

### Performance (NVMe, against alpha.1)
- 1.04–1.26x higher throughput wherever the difference is significant, with
  the largest gains on index-heavy workloads (1.26x with 16 indexed
  attributes). Concurrent reads are 12–18% faster.
- Reachable state is 6.5–33% smaller and equality proofs are 2–5% smaller.
- Against golem-db 57c48ca, throughput is 1.2–3.4x higher; 12 of 13 results
  are significant.
- Details are in `docs/benchmarks/2026-10-11-prekucki-v1.md`.

### Validation
- 1,161 tests pass, including:
  - BTreeMap differential tests with variable-length keys;
  - adversarial range-bound and proof tests;
  - tests at the 512-byte ceiling.
- An independent adversarial review of the proof paths found nothing above
  low severity.

### CI
- GitHub Actions run workspace checks (fmt, clippy, tests) and the golem-db
  conformance kit on pushes to `main` and `experimental-*` branches and on
  pull requests.

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
