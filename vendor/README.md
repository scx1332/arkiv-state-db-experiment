# Vendored trie sources

The user explicitly authorized copying the Reth trie implementation and adding
its dependencies. This supersedes the initial dependency-free restriction for
the trie integration. The KV storage contract and memory backend remain
standard-library-only.

## Reth extraction

Reference revision:
[`paradigmxyz/reth` at `81a13259d0bfa62138d47fa1316893e22a2dffe8`](https://github.com/paradigmxyz/reth/tree/81a13259d0bfa62138d47fa1316893e22a2dffe8).

The local `reth-trie` package is a focused extraction, **not a drop-in copy of
the entire upstream crate**. It contains these unmodified source files:

- [`crates/trie/common/src/key.rs`](https://github.com/paradigmxyz/reth/blob/81a13259d0bfa62138d47fa1316893e22a2dffe8/crates/trie/common/src/key.rs)
  → `reth-trie/src/key.rs`.
- [`crates/trie/trie/src/forward_cursor.rs`](https://github.com/paradigmxyz/reth/blob/81a13259d0bfa62138d47fa1316893e22a2dffe8/crates/trie/trie/src/forward_cursor.rs)
  → `reth-trie/src/forward_cursor.rs`.

The manifest and small `lib.rs` are local integration code. As in Reth's
[`trie-common` exports](https://github.com/paradigmxyz/reth/blob/81a13259d0bfa62138d47fa1316893e22a2dffe8/crates/trie/common/src/lib.rs),
the core nodes, nibble paths, and hash builder come from Alloy. Reth's
account/storage orchestration, execution dependencies, database codecs, and
path-addressed persistence are outside this extraction.

## Actual MPT implementation

`alloy-trie/` contains the complete published `alloy-trie` **0.9.4** Rust source,
manifest, benchmarks, README, and upstream licenses. This is the dependency
version named in the pinned Reth workspace. Its published VCS metadata pins
[`alloy-rs/trie` at `04208a0e64ac347a2e463dd4c5935b20aa9ed4b4`](https://github.com/alloy-rs/trie/tree/04208a0e64ac347a2e463dd4c5935b20aa9ed4b4).
Upstream Rust source is unmodified, including its node encoding, Keccak hashing,
tests, and proof modules. The manifest's package-local profiling section is
removed because Cargo profiles belong at the workspace root; the original
manifest is retained as `Cargo.toml.orig`. `arkiv-db-trie` uses alloy's `RlpNode`
and `EMPTY_ROOT_HASH`, and its tests use alloy's node encoders,
`HashBuilder`, `ProofRetainer` and `verify_proof` as independent oracles; it
does not use `verify_proof` as its verifier (see `docs/state-trie.md`).

Both upstream projects license these files under **MIT OR Apache-2.0**. Their
original license files and copyright notices are retained in each directory.
Other dependencies are resolved through Cargo and pinned in the root lockfile.

## Updating

Update deliberately against a pinned upstream revision. Compare the copied
files and dependency versions, preserve the licenses, and run upstream unit
tests plus our database tests. Do not silently replace the core with a registry
dependency: the path dependency is intentional so the implementation is local.
Exclude `alloy-trie` from formatting and Clippy changes to preserve its source;
the two copied Reth modules have `rustfmt::skip` annotations in the local wrapper.
