# Genesis bundle authoring example

These inputs build the bounded height-zero prefill profile described in the
[genesis bundle design](../../genesis-bundle-design.md). The addresses are
synthetic, with no signing keys or deployment configuration. Building and
verifying these fixtures does not start a node.

Start with [chain.inline.toml](chain.inline.toml) for a small chain. It keeps one
allocation and entity together, resolves `@catalog` against the allocation
label, and expands the immutable `arkiv-entity-v3/glm-v2@1` preset. The generated
lock records all source hashes and lengths, the full profile and resolved
identities/economics.

The expanded example is [chain.toml](chain.toml). It pins the
[profile](profiles/fixed-glm-v2.toml), [allocations](allocations.toml), and two
entities in [catalog.toml](datasets/catalog.toml). `bytes_file` references
resolve relative to this example directory, including references inside
datasets.
Supplied hashes and byte lengths are assertions; refresh does not waive them.

Build the CLI from the repository root:

```sh
cargo build -p arkiv-node --bin arkiv-proposer
```

Copy this source directory to a scratch location before generating its lock.
From that copy, with `arkiv-proposer` on `PATH`, run:

```sh
arkiv-proposer genesis lock chain.toml
arkiv-proposer genesis build chain.toml --out ./built --locked
arkiv-proposer genesis verify ./built
```

For a chain authored with your own public proposer address, start a fresh local
producer with its matching private key:

```sh
arkiv-proposer producer --run-dir NEW_DIR --key-file PROPOSER_KEY --bundle ./built
```

The producer starts paused. The namespace and prefills already exist at height 0;
block 1 is ordinary. The synthetic addresses in these examples have no supplied
private keys. For a complete isolated test with generated temporary inputs, run
`python3 scripts/genesis-bundle-smoke.py` from the repository root.

The lock command refuses an existing lock. After editing source inputs, use
`arkiv-proposer genesis lock chain.toml --refresh`; its JSON output reports the
changed resolved fields with before/after values. A locked build requires an
exact complete match and never changes the source lock. Omitting `--locked`
builds from current validated inputs and writes a generated lock inside the
bundle only. Builds refuse an existing destination directory.

The output is:

```text
built/
  genesis.bin          canonical signed-genesis launch record
  prefill.bin          canonical typed entity stream
  genesis.lock.toml    exact source pins, expanded profile and resolved recipe
  report.json          independently reproduced keys, roots, counts and rent
```

The builder creates a private directory and publishes it atomically. Verification
needs no source tree: it reconstructs the state from canonical rows, checks the
launch commitment and exact artifact pins, and compares the report and expanded
profile. Source pins are reviewable provenance; checking their bytes again
requires the original source tree and a locked build.

Both representations produce the same semantic identity when their expanded
values are identical. Comments, source paths, attribute order and source row
order change provenance only. Decimal attributes normalize exact units, so
`12.50` and `12.500000000000000000` agree. The
[JSONL alternative](datasets/catalog.jsonl) has the same records as the TOML
dataset. Replace the existing `[[prefill.dataset]]` reference with:

```toml
[[prefill.dataset]]
id = "catalog"
format = "arkiv-entities-jsonl-v2"
path = "datasets/catalog.jsonl"
bytes = 1106
sha256 = "246459edff95095c022d3606c3e86a05778d8c729fb96a2701b979636bb99cc4"
```

Do not append it as a second dataset: the repeated creation nonces are invalid.
JSONL requires one strict typed entity per line and rejects empty lines,
duplicate keys, floats, unknown fields and untyped attributes. Allocation and
entity integer values use canonical decimal strings; fixed bytes and addresses
use exact-width lowercase `0x` hex.

Entities follow the user-defined-columns model: there is no payload or content
type, only attributes. Each attribute is a table with exactly one typed value
and an explicit `indexed` boolean, for example
`attributes.title = { str = "Welcome", indexed = true }`. The value tags are
`bool`, `i32`, `u64`, `u256`, `dec`, `str`, `bytes32`, `addr`, `key` and three
spellings of `bytes`: `bytes` (padded standard Base64), `bytes_text` (exact
UTF-8 text) and `bytes_file = { path = ..., bytes = ..., sha256 = ... }`
(a pinned source file). An indexed attribute is queryable; an unindexed one is
stored and returned but never matched; `bytes` is never indexed. An entity has
at most 32 attributes, and its `bytes` values together at most 128 KiB. The
examples store the old payload as an unindexed `bytes` attribute `body`, the
content type as an unindexed `str`, and tag each entity with an indexed
`arkiv.schema`, as an SDK named schema would.

The expanded example has 10,000 GLM in gross allocations. Its sponsor pays the
two finite rents from its 9,000 GLM allocation; `report.json` contains the exact
burn and remaining sponsor balance. Both account transaction nonces remain 0.
The creator's next entity-creation nonce is 2. The `entities` namespace and
both rows exist at height 0, with creation/update heights 0 and explicit expiry
heights 86,400 and 86,401. A prefilled chain treats block 1 as an ordinary block.

The supported envelope is 256 rows, 1 MiB canonical bytes, 8 MiB logical storage
units, and 256 allocations. Every input document and `bytes_file` is at most 1 MiB;
all source bytes together are at most 8 MiB, across at most 512 files. Paths
must be relative ASCII paths of at most 512 bytes. The frontend rejects
traversal, symlinks, duplicate paths, case collisions and multiline TOML string
forms. The source and canonical rows fit explicit memory bounds; this is a small
reference builder, without bulk sorting, object packs or resume checkpoints.

The independent fixture source audit remains available:

```sh
python3 docs/examples/genesis-bundle/verify-source.py
```

It checks example pins, TOML/JSONL equivalence, allocations, nonce sequences,
cell counts and expiry capacity. The Rust verifier additionally reconstructs
canonical state and rent. Replace the synthetic public identity and economics,
then rebuild and review commitments before using any separately authorized
runtime deployment configuration.
