# Testing

This page is a map: which group of tests pins which behaviour, and the rules
the expected values follow. Each test file states its own scope in its
header comment; that header is the detailed reference.

## Running

```sh
cargo test --workspace --offline
```

The suite needs no network and no running service. Tests that start a node
spawn the workspace binaries on ephemeral local ports.

- Heavy and opt-in tests are `#[ignore]`d and named in their attribute:
  the exhaustive torn-write sweeps
  (`cargo test --release -p arkiv-db-redb --test it atomicity:: -- --ignored`),
  and an expiry-sweep timing experiment.
- Tests that measure storage create their files on a real disk: in
  `ARKIV_BENCH_DIR` if set, otherwise in Cargo's per-target temporary
  directory. They never use `std::env::temp_dir()`, which may be a RAM
  file system where `fsync` does nothing.
- Debug builds of redb walk every page on open, so tests assert
  application-level read counts and structural bounds, not cold-cache page
  counts. The release measurements come from the examples described in the
  [redb experiment](redb-experiment.md) and [benchmarking](benchmarking.md).

## Continuous integration

`.github/workflows/ci.yml` runs on pushes to `main` and `experimental-*`
branches and on pull requests, with the workspace's minimum Rust version:
`cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` on the
maintained crates, then `cargo test --workspace`. Ignored tests and the
scripts under `scripts/` are not run.

`.github/workflows/golem-conformance.yml` builds the
[golem-db conformance kit](../conformance/golem-db/README.md#ci) inside the
upstream harness at the revisions pinned in the workflow, compares the
ported golem-db tests with `conformance/golem-db/expected-failures.txt` and
smoke-runs two small scenarios on each backend. A weekly non-gating job
repeats this at the latest upstream revisions. Results of a full run are in
the [conformance report](golem-db-conformance-report.md).

## Oracle rules

These rules apply to every test that asserts a cost, a result or a root.

- **Costs are literals.** Expected receipts and ledgers are numbers derived
  by hand from the fixture weights and the charge-point table in
  [metering](metering.md), or from a copy of that table inside the test.
  A test never computes an expectation by calling the engine's pricing API
  or by running the engine first, so a wrong charge point cannot confirm
  itself. When tests are merged or moved, the literals move unchanged.
- **Results come from an independent model.** Expected query answers come
  from a `BTreeMap` model of the live records and a full-scan evaluator of
  the query semantics, or from a mutation-log oracle that replays intended
  writes; never from the engine's index or a second engine call.
- **Roots and proofs come from an independent construction.** Expected
  state roots are built with `alloy-trie`'s `HashBuilder` or the reference
  trie construction in the test support code; expected proofs with
  `ProofRetainer`. Golden bytes for the codec and the block format are
  assembled in the test from the documented layout, never produced by the
  encoder under test.
- **Isolation is measured on discarded branches.** The isolation tests run
  namespace A's fixed workload on a branch that is dropped, never
  published, so A's state is identical in every phase. Between phases
  another namespace and the namespace directory grow. A's receipts, effects
  and namespace-local node visits must not change; directory visits may
  grow only as the directory trie's shape dictates. A budget-aborted scan
  shows that work stops early instead of being materialised and then
  charged.

The engine tests establish work and cost bounds, not latency. Cost vectors
use the declared fixture weights, not a calibrated production schedule.

## arkiv-db: the engine

Integration tests are in `crates/arkiv-db/tests/`.

| Group | What it pins | Anchor files |
| --- | --- | --- |
| Blocks, namespaces, rollback | `begin`/`execute`/`seal`/`publish`, stale and foreign candidates, namespace ordinals, ownership and authorisation order, checkpoints and transaction rollback, admission bounds on names and raw keys | `blocks.rs`, `namespaces.rs`, `rollback.rs`, `bounds.rs` |
| Metering | Hand-counted receipts for every operation under every budget from zero to the total, ledger order, atomic charge groups, block budgets, overflow, estimation parity and pinned pricing (model version 3) | `metering.rs`, `query_metering.rs` |
| Generated histories | Seeded multi-block histories with rollbacks, budget failures, stale losers and pricing changes; the oracle predicts every verdict, receipt and read before looking at the engine | `history.rs`, `query_history.rs` |
| Typed records and queries | Frozen encodings (`RecordKey` is `len ‖ bytes`, unpadded; cell names; type tags; term bytes), create/patch/delete, every query operator against the full-scan reference, the validation order of query faults, keyset pagination and cursor binding, the `terms` read, expiry sweeps, configurable cell limits, the opt-in planned evaluation | `records.rs`, `queries.rs`, `pagination.rs`, `terms.rs`, `cell_limits.rs`, `query_planner.rs` |
| Key layout | Unpadded, prefix-free term keys with strings chosen to break variable-length encodings; range order for every indexable type; golem-db's type grid | `prefix_free_terms.rs`, `ordered_ranges.rs`, `golem_types.rs` |
| Isolation | The discarded-branch rule above for raw records and for typed records and indexes | `isolation.rs`, `query_isolation.rs` |
| Physical traces | A read rejected part way still reports every node it visited (regression tests for a real bug) | `trace_regression.rs` |
| Codec, host state, interrupts | Golden vectors of the canonical codec; the host-state map; host interrupts of metered reads | `codec.rs`, `host_state.rs`, `interrupt.rs` |
| State roots | Roots over seeded histories against the independent construction; nodes rehashed per block against the prepaid `RootUpdate` bound; storage failures beneath engine state surfacing as errors, never as absent data | `roots.rs`, `rehash.rs`, `storage_faults.rs` |
| Proofs | Complete equality and range proofs against independent row and trie models, tamper cases and resource caps; charges per [eq proof charges](eq-proof-charges.md) and [range proofs](range-proofs.md) | `eq_proofs.rs`, `range_proofs.rs`, `scalar_profile.rs` |
| Durable engine | One seeded history run through the memory engine and the durable engine with identical roots, receipts, pages and proof bytes; format vectors; every commit and initialisation outcome; cache bounds; health latching and recovery; the retention floor; streaming import and in-place adoption of a bulk-built store | `durable.rs`, `durable_failures.rs`, `durable_retention.rs`, `adopt.rs` |

Unit tests inside the crate pin the record encodings, trie key orders and
leaf layouts, the meter's atomic groups, the decoded-node cache and the
restoration of a branch after a storage error.

## arkiv-db-trie

`crates/arkiv-db-trie/tests/` checks the Merkle map described in
[state trie](state-trie.md):

- contents against a `BTreeMap` and roots against `HashBuilder` or the
  reference construction, after every step of random histories (`map.rs`);
- order-preserving, prefix-free key encoders and variable-length keys,
  including adversarial prefix-related keys (`encoding.rs`,
  `variable_keys.rs`, `adversarial_bounds.rs`);
- point proofs equal to independent proofs, and the strict verifier
  rejecting every tampered proof (`proofs.rs`);
- interval reads and their proofs against a sorted-map model, including
  gaps (`interval_*.rs`);
- staging, committing and reopening from a root through a fallible loader,
  bounded preparation that rejects before encoding, and streaming
  construction that yields the same root and object set (`store.rs`,
  `bounded_stage.rs`, `sorted.rs`).

## Storage contract and redb

The versioned storage contract has one behavioural suite,
`arkiv_db_storage::testing::contract`, whose expected answers are written
into the cases. It is instantiated for the memory store
(`crates/arkiv-db-memory/tests/`) and, in `crates/arkiv-db-redb/tests/`,
for redb on a real file, redb on the simulated `CrashDisk`, and both behind
the fault-injecting wrapper.

The other redb tests are the debug-scale checks behind the
[redb experiment](redb-experiment.md):

| Group | What it pins | Anchor files |
| --- | --- | --- |
| Crash atomicity | A multi-table commit cut at every mutating backend call, with synced-only, unsynced and torn images, reopens to exactly the parent or exactly the candidate; store creation survives a crash at any point | `atomicity.rs`, `initialization.rs`, `relabel.rs` |
| Durability and recovery | `Committed` survives a power cut taken at that instant; reopening after a kill needs a fixed number of application reads | `durability.rs`, `recovery.rs` |
| Failures | An I/O error latches the store until `recover`; lost acknowledgements are reconciled from durable state, never by executing a block again; disk full fails the commit and the identical batch later commits once; damaged bytes are errors, never "absent" | `latching.rs`, `reconciliation.rs`, `disk_full.rs`, `bounded_reads.rs` |
| History, isolation, amplification | Historical objects stay readable with a tiny cache and under concurrent commits; reads of a frozen namespace cost the same as others grow; bytes written per logical byte fall as blocks grow | `history.rs`, `isolation.rs`, `write_amplification.rs` |
| Durable engine on redb | The seeded engine history on a real file, closed and reopened; crash cuts through real engine commits; child processes killed at the two publication boundaries | `engine_history.rs`, `engine_crashes.rs`, `engine_process.rs` |

Unit tests cover the `CrashDisk` model itself and the instrumented
backends.

## Node library tests

`crates/arkiv-node/tests/`. The formats are specified in
[block format](block-format.md#part-2-block-format).

| Group | What it pins | Anchor files |
| --- | --- | --- |
| Block format | Golden bytes and digests for every block structure, and rejection of every malformed or non-canonical encoding | `block_format.rs` |
| Validation and selection | Static validation of transactions and admin inputs; admin-first, strict FIFO selection under the count, operation, byte and budget limits; the request-identity state machine | `validation.rs`, `selection.rs`, `identity.rs` |
| Execution and replay | Fixtures produced on a memory chain, checked against hand-derived receipts, replayed on a fresh engine to identical bytes and pinned by golden block hashes; executor failure paths; seeded block histories against an oracle with its own charge table, expiry resolution and state root (it shares only `params::check_reservation` with the node) | `replay.rs`, `exec.rs`, `block_model.rs` |
| Entities, accounts, gas | The Arkiv entity host across producer, full and light roles; native accounts; rent and gas numbers; externally signed Ethereum transaction fixtures | `arkiv_profile.rs`, `accounts.rs`, `gas_execution.rs`, `ethereum_transactions.rs` |
| Genesis | Genesis invariants and the expiry reservation; bundle authoring, prefill, streaming build, and prebuilt state, through the library and the `arkiv-proposer` binary; in-place adoption through the binary only | `genesis.rs`, `genesis_prefill.rs`, `genesis_adopt.rs`, `prebuilt_genesis_cli.rs` |
| Signed proposer | Signature and preimage checks with independent keys; pool nonce ordering and capacity; execution, independent replay and publication faults; retained bodies and light headers; submission, body replication and light proof verification over real sockets; detached reads bounded in concurrency and time; the state-retention floor on every state consumer | `signed_protocol.rs`, `signed_pool.rs`, `signed_host.rs`, `signed_http.rs`, `signed_reads.rs`, `signed_retention.rs` |
| Snapshots | The body pin holds bodies past the record window and a follower far behind syncs from them over both sync paths; the gap a pruned peer leaves is reported for the follower's next block only; `snapshot create` copies closed files whose hashes it records, and `install` refuses other pins, a changed byte, an occupied run directory and a peer that cannot continue; through the binary on a chain with a streaming prefill: a node installed from a snapshot starts without the prefill, catches up from the pinned follower, and one pointed at a pruned peer exits with status 3 | `signed_snapshot.rs`, `snapshot_cli.rs` |
| Simulator | The durable simulator model, restart and uncertainty handling; redb write and sync cuts leaving a complete parent or candidate packet; HTTP controls, caps and replay over real sockets | `simulator.rs`, `simulator_crash.rs`, `simulator_http.rs` |
| Demo | Authentication and wire rules of the bounded producer/full/light demo | `demo_auth.rs` |

## Script-level checks

These run real binaries as separate processes. They are run by hand, not by
CI; each takes the binary path as an option.

- `scripts/signed-proposer-smoke.py`, `glm-accounts-smoke.py`,
  `range-proof-smoke.py`, `genesis-bundle-smoke.py`,
  `streaming-prefill-smoke.py`: isolated producer, full and light nodes
  exercising submission, sync, proofs, genesis bundles and restart.
- `scripts/udc-sdk-smoke.py` and `full-indexer-smoke.py`: the released SDK
  and indexer against a local chain (see
  [SDK compatibility](sdk-compatibility.md)).
- `scripts/test-full-light-demo.py`: the demo behind a tampering proxy (see
  the [demo run guide](full-light-demo-run.md)).
- `scripts/test-sourcify-v4-prefill.py`: the prefill converter against an
  independent reader of its output.
- `scripts/upgrade-smoke.py` (`OLD_BIN`, `NEW_BIN`): a drop-in upgrade. The
  new binary opens the old one's run directory in place and continues its
  chain, old and new followers and light clients accept each other's blocks,
  and the old binary reopens what the new one wrote. Run it against the
  previous release before tagging one that claims compatibility.
- `scripts/snapshot-smoke.py` (`BIN`): [snapshot sync](snapshots.md) end to
  end. A producer and a follower with `--body-retention window`; the
  follower is stopped, snapshotted, pinned and restarted; more than 256
  blocks later a node installed from the snapshot with `--peer` and
  `--verify-state` catches up from the follower to the producer's head and
  answers the same query, an install against the producer is refused, and a
  node pointed at the producer exits with the reinstall message.

## Not covered

- Consensus: there is one proposer. No fork choice, no proposer rotation.
- Pruning of anything but trie objects: blocks, bodies and manifests are
  never deleted. Garbage collection of trie objects is covered by
  `arkiv-db-storage` (`gc::tests`, a toy graph over tiny domains),
  `arkiv-db` (`it/durable_gc.rs`, archive against pruned) and
  `arkiv-db-redb` (`it/gc.rs`, reopen, crash cuts, format gate, suspension).
- Query proofs other than complete equality and range results.
- Latency or throughput guarantees on shared hardware; benchmarks are
  reported separately under `docs/benchmarks/`.
- Physical media faults beyond the `CrashDisk` model (synced, unsynced and
  torn images, I/O errors, quotas) and real process kills.
- Cost calibration: weights are test fixtures.
