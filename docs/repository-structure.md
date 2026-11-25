# Repository structure

*As of 2026-11-18 (`0ac2d54`).*

This is a guided tour: where things live and why the code is cut where it
is. The [architecture notes](architecture.md) describe each component in
detail; this page explains the shape.

## The layout at a glance

```text
crates/
  arkiv-db-storage   the storage contract: what a durable store must promise
  arkiv-db-memory    that contract in memory
  arkiv-db-redb      that contract on one redb file
  arkiv-db-trie      the Merkle Patricia map every piece of state is built from
  arkiv-db           the engine: metered blocks, records, indexes, queries, proofs
  arkiv-node         the node: block format, Arkiv entities, genesis, runtimes
vendor/alloy-trie    upstream trie source, unmodified
golem-api/           golem-db's public API on the engine; its own workspace, lock and toolchain
conformance/         golem-db conformance kit (runs golem-db's test suites on golem-api)
scripts/             smoke tests and benchmarks that drive the built binaries
docs/                design, formats, evidence; see "Documentation" below
deploy/              reverse-proxy example for the API docs
```

Dependencies point one way, from the node down to storage:

```text
arkiv-node ──> arkiv-db ──> arkiv-db-trie ──> alloy-trie
                   │
                   └──────> arkiv-db-storage <── arkiv-db-memory
                                             <── arkiv-db-redb ──> redb
```

Nothing below `arkiv-db` knows about blocks, entities or time, and nothing
below `arkiv-node` knows about Arkiv, gas or signatures.

`golem-api` stands beside the node, not in this chain: it depends on
`arkiv-db`, `arkiv-db-redb` and `arkiv-db-storage`, and nothing depends on
it. It is not a workspace member, because it also depends on golem-db (a git
dependency with MDBX and a newer Rust), which must stay out of the node's
`Cargo.lock`. See [its README](../golem-api/README.md).

## Why these crates

The rule is: **a new crate needs a concrete reason, not a topic.** Metering,
namespaces, records, indexes, queries and proofs are all modules of
`arkiv-db`, because they share one state and one invariant (every charge is
a function of the state and the operation) and splitting them would only
move that coupling into public APIs. A crate is split off when one of three
things is true.

**It isolates a dependency.** `arkiv-db-redb` is the only crate that
depends on redb and `libc`. The engine and the storage contract stay
standard-library-only (plus `alloy-primitives` for Keccak), so they can be
tested, reasoned about and ported without a database. Swapping redb for
another store means writing one more implementation of the contract, not
touching the engine.

**It is a contract with several implementations.** `arkiv-db-storage`
defines what a durable store promises: immutable content-addressed objects,
insert-only records, one head moved by compare-and-set, and three commit
outcomes (`Committed`, `NotCommitted`, `Indeterminate`). It is its own crate
so that the engine, the memory store and the redb store all depend on the
contract and none of them on each other. Its `testing` module holds the
behavioural suite every implementation runs, so the memory store and the
redb store are held to the same rules by the same tests.

**It must not depend on what sits above it.** `arkiv-db-trie` defines its
own loader and error types instead of using the engine's or the storage
contract's. That keeps the trie testable against `alloy-trie`'s builders as
an independent reference, and stops engine concerns from leaking into the
code that decides state roots.

`arkiv-node` is separate because it is the host. Block formats, admission,
expiry scheduling, gas, signatures and block assembly are consensus-facing
rules about Arkiv; the engine is a deterministic database that executes
what it is given. Keeping them apart is what lets the engine be checked on
its own terms (cost bounds, roots, isolation) and lets the node change its
protocol without touching storage.

## Inside the engine (`arkiv-db`)

```text
src/engine/     Engine, Snapshot, Branch; split by role:
                snapshot.rs, branch.rs (the handles), read.rs (one View
                for every metered read), write.rs, ops.rs, staging.rs,
                explore.rs
src/typed.rs    typed records and the term index; typed/planner.rs plans queries
src/proof.rs    equality proofs; proof/range.rs range proofs (generation and
                verification)
src/durable/    DurableEngine: manifests, the decoded-node cache, retention,
                streaming and bulk genesis, adoption of a staged store
src/codec.rs    the canonical binary codec every hash is taken over
src/layout.rs   how keys and leaves are laid out in the maps
src/meter.rs    operation classes, prices, budgets and receipts
```

Two decisions shape the public API.

**Every read and write is metered.** A receipt is a pure function of the
state and the operation. That is what lets a node quote a transaction
before it runs, and what makes a block's cost reproducible on every
follower.

**Storage errors are explicit.** Every method that can reach stored state
has a `try_` form returning a `StorageError`. The infallible forms exist
for tests and examples on the memory engine, where storage cannot fail.
`arkiv-node`'s `clippy.toml` bans them, so a failing disk aborts the
candidate block instead of turning into a panic, a rejection or an empty
read.

## Inside the node (`arkiv-node`)

The node library is three layers.

**The deterministic core** is everything a follower must agree on:
`block` (formats and hashes), `params` (genesis invariants, the expiry
reservation), `validate`, `select`, `identity`, `exec` (the one function
that both builds and replays a block) and `replay`. The Arkiv entity rules
sit beside it in `arkiv`, `gas`, `accounts` and `retention`.

**Genesis tooling** (`genesis_*`) turns an authored bundle into a verified
height-zero state: small bundles, streaming prefills of millions of rows,
bulk index construction, prebuilt stores and in-place adoption. These are
separate files because each is a different way of arriving at the same
state, with different memory and durability trade-offs.

**The runtimes**, each with its own binary:

| Module | Binary | What it is |
| --- | --- | --- |
| `signed` | `arkiv-proposer` | The real node: a signed single proposer with full and light followers, durable storage, JSON-RPC for the Arkiv SDK and a REST API |
| `simulator` | `arkiv-simulator` | An unsigned durable node used to study storage, proofs and the query language without signatures |
| `demo` | `arkiv-demo` | A volatile full/light demo with Ed25519 development authentication |

`signed` grew on top of `simulator`: it reuses the simulator's wire
helpers, proof inspection and query language, and the simulator does not
depend on `signed`. Code that both need and that belongs to neither lives
in small neutral modules: `error` (one closed set of error codes, rendered
on the wire by name), `text` (hex and quantities as the JSON APIs read
them), `http`, `entity_json`, `dev` (the development genesis) and `fsutil`.
No production module imports from `demo`.

Inside `signed`, the JSON-RPC surface is a module tree with one function
per method (`rpc/`), and the query path has its own tree (`rpc_reads/`:
syntax, errors, execution). `dispatch` is a flat match with one line per
method, so a method is found by its name.

## Tests

**One test binary per crate.** `arkiv-db`, `arkiv-db-trie` and
`arkiv-db-redb` each compile their integration tests as one binary
(`tests/it/main.rs` with a module per area) over a shared `tests/support`.
Separate binaries recompile the support code for each file, and that cost
is what used to push every file into re-typing its own helpers. `arkiv-node`
still has one binary per file: several of its tests spawn the built
binaries, bind ports and share process-wide caches, and they are not yet
safe to run in one process.

**Expected values never come from the code under test.** Costs are
hand-derived literals; query answers come from an independent model;
state roots come from `alloy-trie`'s builder; golden bytes are assembled
from the documented layout. A test that computes its expectation by calling
the engine can only confirm itself. The [testing notes](testing.md) state
these rules and what each test group pins.

**Fault injection is shared.** `FaultyStore` wraps any store and injects
I/O errors, lost acknowledgements, latched failures, read overrides and
hooks; `CrashDisk` simulates power loss and torn writes under redb. Tests
use these instead of writing their own store doubles, so every fault means
the same thing everywhere.

**Test support is not in the node.** The `testing` modules of the storage
crates sit behind a `testing` cargo feature that only `[dev-dependencies]`
enable, so the release binaries do not contain them.

## Vendored code

`vendor/alloy-trie` is the published `alloy-trie` 0.9.4 source, unmodified.
It is a path dependency so the trie that decides state roots is in the
repository and pinned, not resolved from a registry at build time. It keeps
its own MIT OR Apache-2.0 licence and is excluded from formatting and
Clippy so that it stays byte-comparable with upstream. See
[vendor/README.md](../vendor/README.md).

`crates/arkiv-node/src/simulator/query_language/` is adapted from
[Arkiv-Network/arkiv](https://github.com/Arkiv-Network/arkiv) and keeps its
upstream tests, so it can be diffed against the original. Because that
code is GPL-3.0-or-later, so is the whole repository.

## Documentation

`docs/` is organised by how long a document stays true:

| Where | What | Kept up to date |
| --- | --- | --- |
| `docs/*.md` | formats, contracts, design and how-to | yes: a statement that contradicts the code is a bug |
| `docs/proposals/` | changes not yet decided or implemented | until accepted or dropped |
| `docs/benchmarks/` | dated measurements | no: a record of what was measured, on what |
| `docs/runs/` | dated deployment and prefill runs | no: a record |
| `docs/reviews/` | dated code reviews | no: a record of the commit reviewed |
| `docs/examples/` | inputs to the genesis tooling | yes, and see below |

One file under `docs/examples/` is not documentation:
`docs/examples/genesis-bundle/profiles/fixed-glm-v2.toml` is compiled into
the node as the current genesis preset (`genesis_authoring.rs`). Editing it
changes genesis hashes.

Open work is tracked in two places: `docs/code-quality-todo.md` and
`docs/sdk-1.0-compatibility-todo.md` for lists that belong with the code,
and GitHub issues for individual items.

## Conventions

- **Finished work goes to `main`.** There are no long-lived branches; notes
  and to-dos live in the repository, not on branches.
- **CI** runs `cargo fmt --check`, `cargo clippy -D warnings` and the
  workspace tests on Rust 1.90, the minimum supported version.
- **Formats are versioned.** The block format, protocol, state layout and
  meter model each carry a version number; a change to any byte of them
  bumps it and is recorded in the [changelog](../CHANGELOG.md).
- **Less code over doubtful code.** A test that cannot fail, a helper typed
  twice or a comment that restates the line below it is removed rather
  than kept for completeness.
