# Sourcify prefill conversion

`scripts/sourcify-prefill.py` converts a completed, checksummed Sourcify processor
or supported helper export into the experimental v2 canonical entity stream. It preserves the
source's compressed payload bytes, typed attributes, semantic content hashes
and staging identities. It writes only a new local output directory; it does not
start a node, fund accounts, modify the source, or create a signed genesis.

```sh
python3 scripts/sourcify-prefill.py inventory \
  --bundle /absolute/path/to/processor/bundle --out /absolute/path/to/inventory
python3 scripts/sourcify-prefill.py convert \
  --bundle /absolute/path/to/processor/bundle --out /absolute/path/to/converted
```

The default destination chain is `7706815`; a different `--chain-id` must match
the seed's chain ID. Both adapters accept seed version 1 and upload schema
`sourcify-arkiv.upload.v1`. The pinned metadata selects the adapter automatically;
the converter refuses ambiguous processor/helper manifests.

| Adapter | Accepted metadata and payloads |
| --- | --- |
| `processor-v1` | Completed `report.json` schema `sourcify-offline-import.v1`, required complete Ethereum-mainnet source, v3 compilation/verified-contract payloads |
| `helper-uploaded-v1` | Completed `provenance.json` schema `arkiv-prefill.derived-permanent-seed.v1`, wrapping `arkiv-prefill.postgres-uploaded-seed.v1`; contract selection from Sorbet, v2 compilation/verified-contract payloads |

The processor adapter accepts up to five documented source skips by default;
`--max-skipped 0` requires none. This option does not relax helper validation.
The helper adapter checks the nested inventory, part pins from the permanent
derivation, and unchanged `contracts.jsonl`/seed pins from original provenance.
It streams the contract audit into SQLite, requiring unique addresses and root
keys, matching Ethereum-mainnet contract entities, and matching totals, kinds,
publisher count, payload bytes and typed-reference count. Other provenance
versions, expiry policies and selection modes require an explicit extension.
Neither adapter treats a provenance statement as a fresh RPC verification.

Metadata and each part are checked against `SHA256SUMS` and declared lengths. JSON duplicate
members, duplicate entity identities/locators, missing dependencies, unsupported
schemas/types, unsafe paths and symlinks fail before publication.

The script uses Python's standard library, including SQLite and
`hashlib.new("KECCAK-256")`. The latter requires a Python/OpenSSL build exposing
Ethereum Keccak, such as this host's OpenSSL 3.5. It self-tests that algorithm and
fails clearly when unavailable; NIST `sha3_256` is never substituted.

## Output and reproducibility

Conversion publishes four files together with a synced, atomic Linux
`renameat2(RENAME_NOREPLACE)`. Existing output directories are refused.

| File | Meaning |
| --- | --- |
| `prefill.bin` | `ARKPFL02`, big-endian u32 version 2, u64 entity count, then u32-length-prefixed existing `PrefillEntity` encodings |
| `keys.jsonl` | Streamed source-key/staging-ID to new-key mapping, creator nonce, expiry, logical units and rent for every entity |
| `source-lock.json` | Source manifest/checksum identity and ordered paths, byte lengths and SHA-256 pins for every consumed file |
| `conversion-report.json` | Counts, limits, dependency count, chosen policies, canonical stream hash/length, logical storage units and exact rent |

Inventory performs the same identity, dependency and lifetime checks but only
publishes the source lock and report. The source-lock hash in the report hashes
its canonical compact JSON bytes, excluding the file's trailing newline.

`streamHash` is Keccak-256 of the one-byte domain length, ASCII domain
`arkiv/prefill-stream/v2`, and the complete `prefill.bin`. `prefillSha256` is a
separate file checksum. The Rust importer must independently validate the
canonical bytes and recompute state and rent before publishing genesis. The
conversion report is an audit artifact, not an authority for chain state.

Per-creator nonces are consecutive from zero in the pinned source manifest's
part/line order; salt is zero. The final stream is ordered by creator bytes and
nonce using SQLite's disk index. Changing source order can therefore change
derived keys and genesis identity. Cache size, local paths and execution speed
do not change canonical bytes. Attributes sort by exact name bytes.

Only typed `key` attributes are remapped. Payload hash references remain content
locators. The converter recomputes the source's staging identity after
resolving source-key references back to staging IDs, so keeping an old staging ID
while silently changing its semantic attributes is rejected. Processor source
keys must match its imported-staging derivation. Helper source keys are actual
uploaded keys and are instead checked for uniqueness and joined to the contract
audit; the processor key restriction is retained for processor input. Owner must equal
creator, source expiry must be `never`, and source flags must be empty; other
source policies need an explicit adapter extension.

The v2 profile permits 64 total cells / 55 user attributes. The preserved
39d5b75 smoke bundle actually uses 29 attributes on verified-contract entities,
including four Sourcify provenance IDs. These are preserved without schema
packing or omission.

## Build and verify the node bundle

The Rust node CLI consumes the converter's canonical stream:

```sh
arkiv-proposer genesis stream-build /absolute/path/to/base/genesis.bin \
  --prefill /absolute/path/to/converted/prefill.bin \
  --out /absolute/path/to/new-node-bundle \
  --sponsor 0x986c08292e6c327a310273a45965a3d2e2eac44c
arkiv-proposer genesis stream-verify /absolute/path/to/new-node-bundle
```

The base is an existing encoded signed genesis whose allocations fund the chosen
sponsor. The builder clears any prior prefill, selects the v2 64-cell profile and
recalculates the block expiry reservation. Chain ID, creator derivation and
funding must match the intended launch. `stream-verify` independently replays the
canonical stream and checks the resulting commitment.

Streaming replay uses 1 MiB batches by default; the builder accepts
`--batch-bytes` to override this. A batch is reduced when durable preparation
limits require it. Import-only decoded caches are bounded to 256 MiB and
262,144 entries, and each import redb cache is bounded to 256 MiB. Durable checkpoints bind the prefix
hash, stream cursor, state, logical units and accumulated burn; resumption checks
the source prefix. This is separate from the converter's non-resumable scratch.
Completed node-builder scratch is removed. The v2 protocol admission ceilings
are 100,915,200 rows, 1 TiB canonical bytes, 1 PiB logical units, 8 MiB per frame
and 55 user attributes; these are rejection bounds, not a measured capacity
claim for this host or evidence that the full dataset has been imported.

## Dependencies, expiry and funding

The disk graph includes typed entity-key references, source/code/compilation
references in logical payloads, and every spill-blob reference. The converter
validates blob dimensions/checksums and reconstructed source/code hashes.
Processor compilation hashes cover the v3 payload; helper v2 compilation hashes
cover the original ordered fingerprint array of compiler details, settings,
source path/hash pairs and four code hashes. Each helper compilation must have
a contract that verifies this fingerprint. Helper `sources`, recompiled code
locators and contract `codeRefs` become expiry dependencies. Local processor
compilation references do not add external dependencies. Cyclic graphs and
ambiguous/missing locators are rejected. Payloads are validated in their source
schema and never rewritten into another payload version.

A deterministic queue starts with entities that nobody depends on, breaking
ties by source ordinal. Dependency nodes are scheduled after all their parents.
Expiry slots increase with this order, at most 16 entities per height, ending
at `--max-expiry` (default 6,307,200). Thus each dependency expires at or after
every entity that needs it. With N rows the first occupied height is
`max_expiry - floor((N - 1) / 16)`. `--min-expiry` imposes a minimum acceptable
dataset lifetime; an import exceeding the available slots fails. Heights are
not seconds: the eventual genesis block cadence determines wall-clock lifetime.

Rent is computed per complete stored row with the current logical occupancy
formula and `ceil(units * expiry * 8 / 17280) * 1000000000` wei, rounded per row.
The report states the sponsor allocation required for genesis rent. It does not
debit a wallet. The node builder independently verifies this amount.

## Memory, disk and interruption

Dataset-sized row copies, key maps, uniqueness indexes, dependency edges and
ready queues live in a private SQLite scratch database. Its page cache defaults
to 64 MiB (`--cache-mib`), mmap is disabled and temporary storage is disk backed.
Entity JSON lines are limited to 1 MiB, metadata files to 16 MiB, wire payloads
to 120,000 bytes, and decoded per-entity payloads to 122,880 bytes. Individual
logical spill payloads are assembled one at a time with a configurable 64 MiB
default cap (`--max-logical-payload-mib`, maximum 256). Parsing one such payload
can use several times its byte size; this is a fixed per-payload bound rather
than an RSS guarantee. Inputs exceeding that cap require an explicit policy
choice, not automatic unbounded allocation.

The 80 GiB default disk reserve is checked before work and at bounded row
intervals. Scratch includes source row copies plus SQLite indexes and edges;
budget this alongside the canonical output and later node staging databases.
No process memory limit is imposed by the converter itself; run large authorized
work under the separate 32 GiB cgroup limit. Inventory and conversion do not
claim that the entire dataset or final node database fits this host until
measured at scale.

Progress goes to stderr at phase changes, completed parts and roughly ten-second
intervals between bounded row batches. The final JSON report remains on stdout.

This first converter has atomic publication, **not resumable conversion**.
Ordinary errors remove private scratch; an abrupt kill may leave an unreferenced
`.sourcify-prefill-*` directory, which is never accepted as a finished bundle.
Retry from the pinned source into a new destination. Durable node-import
checkpoints are a separate mechanism. The original source remains the recovery
input and is never deleted by this tool.

## Validation

```sh
python3 scripts/test-sourcify-prefill.py -v
SOURCIFY_SMOKE_BUNDLE=/absolute/path/to/the/263-entity/bundle \
  python3 scripts/test-sourcify-prefill.py -v
SOURCIFY_HELPER_BUNDLE=/absolute/path/to/sourcify-uploaded-100k-contracts-never \
  python3 scripts/test-sourcify-prefill.py -v
```

Tests cover Ethereum Keccak vectors, independent binary-frame decoding,
determinism across cache sizes and multiple creators, manifest tampering,
semantic identity tampering, unsupported schemas, duplicate identities/JSON
members, line/decompression bounds, symlinks, expiry capacity and graph cycles.
The optional processor test checks every payload and typed key reference.
Helper tests cover uploaded keys, v2 dependencies/fingerprints, contract audit
tampering, unsupported provenance and unchanged processor key restrictions.
The optional helper test selects one complete contract closure from at most
1,000 source rows; it does not launch the whole 445,583-entity conversion.

The preserved 16-contract fixture converts to 263 rows, 288 dependency edges,
795,646 canonical bytes, 1,544,460 logical storage units and
4,509,817,317,000,000,000 wei rent. Its expiries span 6,307,184–6,307,200 and its
stream hash is
`0x6f8634a91b73423f6b5ff462ccc08cbeb6b5b6ae2e799f67f057e5b25ec37e05`.
These are fixture results, not full-dataset performance claims.
