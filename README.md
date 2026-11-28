# Arkiv state DB experiment

> [!WARNING]
> **This is an R&D experiment, not the official Arkiv implementation.**
> It is a playground for exploring database and node designs. It is not
> supported, not audited and not meant for production, and it may depart
> from the Arkiv specifications or change without notice.
>
> For the official implementation, use the Arkiv Network repositories:
>
> - [Arkiv-Network/arkiv](https://github.com/Arkiv-Network/arkiv): the Arkiv
>   database chain
> - [Arkiv-Network/golemdb](https://github.com/Arkiv-Network/golemdb): Golem DB
> - [Arkiv-Network/arkiv-sdk-js](https://github.com/Arkiv-Network/arkiv-sdk-js):
>   the Arkiv SDK

A state database and a single-proposer node for the Arkiv entity model,
written in Rust. The target is Arkiv's blockchain workload: many ordered
operations per block, one committed state per block, and historical queries
that a light client can verify against a state root.

The repository has two layers:

- **The engine** (`arkiv-db`): metered, ordered block execution on immutable
  snapshots; project namespaces; typed records with attribute indexes; a
  declared query subset with keyset pagination pinned to one snapshot; a
  state root over Merkle Patricia maps; complete equality and bounded range
  proofs; and a durable facade over redb.
- **The node** (`arkiv-node`): a block format with deterministic execution
  and replay, the Arkiv entity host (entities, expiry, GLM gas and accounts),
  genesis bundles with prefills, and three runtimes. `arkiv-proposer` is the
  main one: a signed single-proposer chain with full and light followers that
  serves the Arkiv SDK over JSON-RPC.

There is no consensus or finality. A proposer signature authenticates who
produced a block; full followers re-execute every block, and light followers
verify headers and query proofs.

## Acknowledgements

This work builds on the Arkiv Network and Golem projects:

- The engine targets the
  [Golem-DB contract](https://github.com/Arkiv-Network/arkiv-source-of-truth/blob/main/golem-db.md)
  and does not yet implement all of it. [`golem-api`](golem-api/README.md)
  presents golem-db's public Rust API on the engine, as a crate outside the
  node's workspace, and the
  [conformance kit](conformance/golem-db/README.md) runs golem-db's
  conformance suites on it.
- The entity model, the SDK JSON-RPC surface and the entity query language
  follow [Arkiv-Network/arkiv](https://github.com/Arkiv-Network/arkiv). The
  query lexer and parser in `crates/arkiv-node/src/simulator/query_language/`
  are adapted from that repository (GPL-3.0-or-later; see the `README.md` and
  `LICENSE` in that directory).
- The trie layer uses the Alloy Patricia trie core, vendored under MIT OR
  Apache-2.0. See [source provenance and licenses](vendor/README.md).

## Quickstart

Rust 1.90 or newer is required (redb 4.3.0 needs it).

```sh
cargo build -p arkiv-node --bin arkiv-proposer

# Runs a producer, a full and a light follower in temporary directories,
# submits signed transactions, verifies proofs and restarts every role.
python3 scripts/signed-proposer-smoke.py
```

To run a producer by hand:

```sh
P=target/debug/arkiv-proposer
ADDRESS=$($P keygen --out proposer.key)        # prints the public address
echo "{\"$ADDRESS\": \"1000000000000000000000\"}" > allocations.json

export ARKIV_PROPOSER_TOKEN=$(head -c 32 /dev/urandom | base64)
$P producer --run-dir run --key-file proposer.key \
  --allocations-file allocations.json --start running \
  --listen 127.0.0.1:9500 --private-listen 127.0.0.1:9501

curl http://127.0.0.1:9500/signed/v1/status
```

`allocations.json` maps addresses to decimal base-unit balances and must fund
the proposer, whose bootstrap transaction creates the `entities` namespace in
block 1. A producer can instead start from a genesis `--bundle`, which may
carry a prefill. Without `--start running` the producer starts paused and is
stepped through the private listener. `arkiv-proposer --help` lists every
command and option.

The node serves the Arkiv SDK over JSON-RPC at `/`, its REST API under
`/signed/v1/`, and an API reference at `/docs` (OpenAPI at `/openapi.json`).
Followers, transaction signing, limits and recovery are described in the
[signed proposer guide](docs/signed-proposer-v1.md).

## Status

| Runtime | Binary | Storage | Blocks authenticated by | Query proofs |
| --- | --- | --- | --- | --- |
| Signed proposer | `arkiv-proposer` | durable (`node.redb`, `history.redb`), restarts from its files (after a crash it executes at most its last 63 blocks again) | secp256k1 proposer signature | equality pages, bounded ranges, accounts |
| Unsigned simulator | `arkiv-simulator` | durable, restarts without replay | nothing (explicitly pinned roots) | equality pages |
| Full/light demo | `arkiv-demo` | in memory; a restart needs a new genesis | Ed25519 development key | equality pages |

Not implemented: consensus and finality, physical pruning (`--state-retention`
only limits which heights are served; see the
[pruning design](docs/pruning-design.md)), and proofs for query shapes other
than equality and single-attribute ranges. Queries without a proof run on the
full node only. Prices in the engine's fixture schedule are not calibrated;
the node's gas profile is described in [GLM gas](docs/glm-gas-v1.md).

## Workspace

| Crate | Responsibility |
| --- | --- |
| `arkiv-db-storage` | The versioned storage contract (immutable objects, insert-only records, a durable head with explicit commit outcomes); `FaultyStore` and the shared contract test suite (`testing`, behind the `testing` feature) |
| `arkiv-db-memory` | `MemoryVersionedStore`: the volatile in-memory implementation of the versioned contract |
| `arkiv-db-redb` | `RedbStore`: the versioned contract over one redb 4.3.0 file; `CountingBackend` I/O instrumentation; `testing::CrashDisk` power-loss, fault and disk-full simulation (`testing` feature) |
| `arkiv-db-trie` | Persistent Merkle Patricia map (`MerkleMap`) with in-memory and lazily loaded nodes, staging for commits, point and interval proofs, a strict verifier, and order-preserving key encoders |
| `arkiv-db` | The block engine (`Engine`: metering, namespaces, typed records, indexes, queries, host state, state root); proofs (`proof`); the state tree explorer (`explore`); the durable engine (`durable::DurableEngine`); the canonical codec (`codec`) |
| `arkiv-node` | Block format, validation, selection, execution and replay (`block`, `validate`, `select`, `exec`, `replay`); the Arkiv entity host (`arkiv`, `gas`, `accounts`, `retention`); genesis authoring, bundles and prefills (`genesis_*`); and the runtimes `signed` (`arkiv-proposer`), `simulator` (`arkiv-simulator`) and `demo` (`arkiv-demo`) |
| `alloy-trie` (vendored) | The Alloy Merkle Patricia trie; `arkiv-db-trie` uses its node encoding, and the tests use its builders as independent oracles |

Dependencies point from the node toward storage. The storage contract
depends on neither the engine nor a concrete backend. The
[repository structure](docs/repository-structure.md) explains the split;
the [architecture notes](docs/architecture.md) give the detail.

## Development

Commands run from the repository root; CI runs the first four.

```sh
cargo fetch --locked
cargo test --workspace --offline
cargo clippy --offline --all-targets -p arkiv-db -p arkiv-db-storage -p arkiv-db-memory -p arkiv-db-redb -p arkiv-db-trie -p arkiv-node -- -D warnings
cargo fmt -p arkiv-db -p arkiv-db-storage -p arkiv-db-memory -p arkiv-db-redb -p arkiv-db-trie -p arkiv-node -- --check

cargo run -p arkiv-db --example blocks --offline     # namespaces and metered blocks
cargo run -p arkiv-db --example records --offline    # typed records, queries, expiry
cargo run --release -p arkiv-db-redb --example redb_experiment --offline -- --quick
cargo run --release -p arkiv-db-redb --example engine_storage_experiment --offline
python3 scripts/full-light-demo.py                   # producer, full and light demo processes
```

Fetching is needed once for a fresh checkout. The vendored `alloy-trie`
source is excluded from formatting and Clippy. The redb tests and experiments
write to a real disk (Cargo's target directory, or `ARKIV_BENCH_DIR`), not to
the system temporary directory; `redb_experiment` without `--quick` builds
files up to 1 GiB and takes several minutes.

The [testing notes](docs/testing.md) say what each test group pins. Packages
are unpublished, and the lockfile is checked in.

## Documentation

- [Repository structure](docs/repository-structure.md): what lives where,
  and why the code is cut that way.

Engine:

- [Architecture](docs/architecture.md): crate boundaries, state layout, state
  root, records and indexes, publication and durability.
- [Canonical codec and block format](docs/block-format.md).
- [State trie](docs/state-trie.md) and
  [key encoding](docs/experimental-prekucki-v1.md).
- [Query model](docs/query-model.md) and [metering](docs/metering.md): the
  query subset, its validation order, and the charge table.
- [Namespace contract](docs/project-namespaces.md) and
  [single-block query contract](docs/single-block-query-proofs.md).
- Proofs: [equality charges](docs/eq-proof-charges.md),
  [range proofs](docs/range-proofs.md).
- Durable storage: [usage and lifecycle](docs/engine-storage-run.md),
  [format](docs/engine-storage-v1.md), the
  [redb evaluation](docs/redb-experiment.md) and the
  [pruning design](docs/pruning-design.md).

Node:

- [Signed proposer](docs/signed-proposer-v1.md),
  [API reference](docs/node-api-reference.md),
  [metrics](docs/metrics.md) and
  [snapshots](docs/snapshots.md): starting a full node from a copy of
  another one's database.
- [Entity profile](docs/arkiv-v2-compatibility.md),
  [entity expiration](docs/entity-expiration.md),
  [GLM gas](docs/glm-gas-v1.md) and [GLM accounts](docs/glm-accounts-v1.md).
- [SDK compatibility](docs/sdk-compatibility.md) and its
  [open items](docs/sdk-1.0-compatibility-todo.md).
- Genesis: [bundles](docs/genesis-bundle-v1.md) with an
  [example](docs/examples/genesis-bundle/README.md),
  [streaming genesis](docs/streaming-genesis-v2.md),
  [bulk index construction](docs/bulk-prefill.md),
  [adoption](docs/genesis-adoption.md) and the
  [Sourcify converter](docs/sourcify-v4-prefill.md).
- [Simulator](docs/simulator-node.md) and the
  [full/light demo](docs/full-light-demo.md)
  ([run instructions](docs/full-light-demo-run.md)).

Evidence and proposals:

- [Testing](docs/testing.md), [benchmarking](docs/benchmarking.md) with dated
  results in [docs/benchmarks/](docs/benchmarks/), and the
  [golem-db API comparison](docs/golem-db-api-comparison.md).
- [Proposals](docs/proposals/), dated run records in [docs/runs/](docs/runs/),
  and the [code-quality review of 2026-11-17](docs/reviews/2026-11-17/README.md).
- [Changelog](CHANGELOG.md).

## License

GPL-3.0-or-later; see [LICENSE](LICENSE). `vendor/alloy-trie` keeps its
upstream MIT OR Apache-2.0 license.
