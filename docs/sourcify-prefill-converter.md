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
the seed's chain ID. All adapters accept seed version 1 and upload schema
`sourcify-arkiv.upload.v1`. The pinned metadata selects the adapter automatically;
the converter refuses ambiguous processor/helper manifests.

| Adapter | Accepted metadata and payloads |
| --- | --- |
| `processor-v1` | Completed `report.json` schema `sourcify-offline-import.v1`, required complete Ethereum-mainnet source, v3 compilation/verified-contract payloads |
| `helper-uploaded-v1` | Completed `provenance.json` schema `arkiv-prefill.derived-permanent-seed.v1`, wrapping `arkiv-prefill.postgres-uploaded-seed.v1`; contract selection from Sorbet, v2 compilation/verified-contract payloads |
| `helper-imported-v1` | Completed native `arkiv-prefill.postgres-imported-seed.v1` provenance with `expiryPolicy: never`, or its permanent derivation wrapper; imported Ethereum-mainnet contracts with mixed uploaded/synthetic identities, v2 compilation/verified-contract payloads |

The processor adapter accepts up to five documented source skips by default;
`--max-skipped 0` requires none. This option does not relax helper validation.
The helper adapters check the inventory and original `contracts.jsonl`/seed pins.
For permanent derivations, entity-part pins come from the derivation while the
contract audit and seed pins come from the embedded original provenance. Native
imported provenance pins all these files directly.
It streams the contract audit into SQLite, requiring unique addresses and root
keys, matching Ethereum-mainnet contract entities, and matching totals, kinds,
publisher count for uploaded exports, payload bytes and typed-reference count.
Imported exports require source chain ID 1, `imported-contracts` selection,
matching uploaded/synthetic totals, the supported imported-key derivation domain
and a nonzero synthetic owner. Other provenance
versions, expiry policies and selection modes require an explicit extension.
No adapter treats a provenance statement as a fresh RPC verification.

Metadata and each part are checked against `SHA256SUMS` and declared lengths. JSON duplicate
members, duplicate entity identities/locators, missing dependencies, unsupported
schemas/types, unsafe paths and symlinks fail before publication.

The script uses Python's standard library, including SQLite and
`hashlib.new("KECCAK-256")`. The latter requires a Python/OpenSSL build exposing
Ethereum Keccak, such as this host's OpenSSL 3.5. It self-tests that algorithm and
fails clearly when unavailable; NIST `sha3_256` is never substituted.

## Deterministic imported-contract subsets

For a native or derived imported-helper bundle, select the first N contract
ordinals from its pinned `contracts.jsonl` audit:

```sh
python3 scripts/sourcify-prefill.py convert \
  --bundle /absolute/path/to/imported-source \
  --out /absolute/path/to/converted-200k --contract-limit 200000
```

The ordinal order records the exporter's oldest revision per distinct Ethereum
address, ordered by insertion time and stable contract ID. The converter requires
contiguous audit ordinals, distinct addresses and root keys, and matching source
contract entities. It retains those roots and their complete transitive typed-key,
payload-locator and spill-blob dependency closure. Shared dependencies occur once;
unselected contract roots are excluded. The resulting entity count is not the
contract limit. Missing dependencies or additional contract roots in the closure
fail conversion. The limit is supported only for imported-helper input.

Every source part is still streamed into bounded SQLite scratch and checked
against its checksum, byte length, wire inventory and identity counts. Payload
semantic validation applies to the selected closure: its staging identities,
payload checksums, source/code hashes, compilation fingerprints and spill contents
are all checked. Unselected payloads are not claimed to have passed semantic
validation. The report explicitly records this `validationScope`.

`source-lock.json` retains the original source pins and adds the selection policy
and requested contract count; it does not invent a smaller source export.
`sourceEntities`, `sourceContracts` and `sourceIdentityCounts` describe the full
source. `entities`, `inputContracts`, `entitiesByKind` and
`selectedIdentityCounts` describe the selected closure. Finite-expiry capacity
is checked against that closure. Each invocation creates its own scratch and
rechecks the source; it does not reuse another conversion's validation.

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
part/line order; salt is zero. Subset conversion assigns these nonces after
selection, preserving original source ordinals in `keys.jsonl`. First-100k and
first-200k subsets are nested in source keys/staging IDs, but shared entities can
have different destination keys when the larger closure adds earlier entities. The final stream is ordered by creator bytes and
nonce using SQLite's disk index. Changing source order can therefore change
derived keys and genesis identity. Cache size, local paths and execution speed
do not change canonical bytes. Attributes sort by exact name bytes.

Only typed `key` attributes are remapped. Payload hash references remain content
locators. The converter recomputes the source's staging identity after
resolving source-key references back to staging IDs, so keeping an old staging ID
while silently changing its semantic attributes is rejected. Processor source
keys must match its imported-staging derivation. Uploaded-helper keys are preserved source upload keys, checked for uniqueness
and joined to the contract audit. Imported-helper inputs combine these with
synthetic keys computed as SHA-256 of the ASCII domain
`arkiv-prefill.imported-staging.v1:` followed by the lowercase staging-ID hex
without `0x`. Synthetic identities must use the provenance owner; the complete
source identity totals must match provenance. These checks do not independently
prove an on-chain upload. The stricter processor key rule remains unchanged. Owner must equal
creator, source expiry must be `never`, and source flags must be empty; other
source policies need an explicit adapter extension.

The v2 frame profile permits 64 total cells / 55 user attributes. Current node
entity-operation validation permits at most 32 supplied attributes, including
payload and content type, so its effective limit is **30 user attributes**. The
converter's `maxUserAttributesSupported: 55` describes its frame bound, not node
admission; check the actual `maxUserAttributes` against 30 before bundle building. The preserved
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
funding must match the intended launch. `stream-verify` independently constructs
the canonical stream's state and checks the resulting commitment.

Version-two import uses bounded external sorting and bulk authenticated-index
construction, without an incremental replay fallback. It validates canonical
frames, spills sorted runs, merges them and builds complete posting lists and
tries before publishing state. The deprecated `--batch-bytes` option controls
progress sampling only; it does not set replay transaction sizes or sort buffers.
See [the bulk construction design and measured resource behavior](bulk-prefill.md)
and [streaming genesis resource bounds](streaming-genesis-v2.md).

Incomplete `bulk-v1` scratch is rebuilt from input after interruption. Completed
scratch may be reused only after validating its input/genesis binding, durable
receipt and engine association. This is not committed-prefix resumption; old
incremental checkpoints are not interpreted as bulk state. Final installation
still copies reachable objects in bounded durable batches into the headless node
store, then publishes height-zero metadata. A failed installation restarts its
traversal; successful installation removes importer scratch. This recovery is
separate from the converter's non-resumable SQLite scratch.

The v2 protocol admission ceilings are 100,915,200 rows, 1 TiB canonical bytes,
1 PiB logical units, 8 MiB per frame and 55 user attributes, with the effective
node limit of 30 user attributes described above. These are rejection bounds,
not a measured capacity claim for this host or evidence of a full-dataset import.

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

### Far-future expiry (`--expiry-policy far-future`)

The source rows say `$expiresAt: never`, and the original Arkiv prefill stores
them as `u64::MAX` with no rent and no ordering. This protocol cannot: the stream
format rejects `u64::MAX` (`genesis_streaming::validate_row`), GLM rent rejects
it as `PermanentUnsupported` (`gas::rent`), and every signed genesis must use GLM
pricing. A finite height within `max_lifetime` is the only admissible expiry.

`--expiry-policy far-future` therefore keeps finite heights but puts them beyond
any realistic chain horizon. The last expiry defaults to `--max-expiry
1000000000000` (about 63,000 years at 2-second blocks) and may be at most
`10^15`, where any admissible row's rent gas still fits u64. Rows fill heights in
emission (creator, nonce) order, 16 per height, ending at `--max-expiry`. With N
rows the first height is `max_expiry - floor((N - 1) / 16)`. Since nothing can
expire within the horizon, dependency order is moot. The converter builds no
dependency graph and skips dependency-safe ordering. The report records
`expiryPolicy: far-future-uniform-v1` and `dependencyEdges: null`, meaning no
graph was built, not zero dependencies.

Without `--contract-limit`, semantic payload validation is skipped too. That
covers staging identities, code/source/compilation hashes, payload schemas,
payload hash references, spill reassembly and helper fingerprints. The report's
`validationScope` says so. Still enforced:

- every SHA256SUMS, report and part checksum and byte length;
- per-row wire validation: system fields, `never` expiry, empty flags,
  owner/creator, attribute names and types, base64, gzip bounds and payload
  SHA-256;
- the entity count and duplicate keys;
- in `convert` only, every typed `key:` reference, which must resolve when
  encoded. `inventory` emits nothing and reports a scope without key references.

With `--contract-limit`, the dependency phase still runs over the selected closure,
because the closure is found through its edges. Only the expiry assignment changes.

The node-side requirements are explicit. The base genesis profile's
`chain_params.max_lifetime` must be at least `--max-expiry`; the preset's
6,307,200 rejects the stream with `ExpiryInvalid(TooFar)`. Rent grows linearly
with expiry, and the sponsor allocation must cover it. The genesis
`expiry_per_block` must stay at 16, the per-height packing the converter
assumes.
[`examples/far-future-prefill`](examples/far-future-prefill) pins a profile with
`max_lifetime = 1000000000000` and a 10^30 wei sponsor. A larger `max_lifetime` also lets live
transactions buy longer lifetimes, still limited by their gas budget.

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
Retry from the pinned source into a new destination. Completed bulk node-import
scratch has a separate validated reuse mechanism. The original source remains the recovery
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
Imported-adapter tests cover native and derived manifests, deterministic subset
selection, shared dependencies, transitive spill closure, synthetic owner/count
validation, missing or corrupt selected dependencies, nonce reassignment and
expiry capacity based on the selected closure. The optional helper test accepts
an uploaded or imported source directory and selects one complete contract
closure from at most 1,000 source rows; it does not run a full-source conversion.

The preserved 16-contract fixture converts to 263 rows, 288 dependency edges,
795,646 canonical bytes, 1,544,460 logical storage units and
4,509,817,317,000,000,000 wei rent. Its expiries span 6,307,184–6,307,200 and its
stream hash is
`0x6f8634a91b73423f6b5ff462ccc08cbeb6b5b6ae2e799f67f057e5b25ec37e05`.
These are fixture results, not full-dataset performance claims.
With `--expiry-policy far-future` the same fixture keeps all keys, payloads and
attributes. It reports no dependency graph, expiries 999,999,999,984 to
1,000,000,000,000, and 715,027,777,772,040,000,000,000 wei rent. Its stream hash is
`0xd6d893668d463a3f944792caa69af23db779e4d5ba0e7ce66dfcf6891eb08042`.

On October 20, 2026 the 445,583-entity 100k helper bundle was converted on the
same host, while the full prefill was running alongside. The default
`dependency-safe` conversion took 513.9 s and reproduced the recorded stream hash
`0x58691b67…1eb6`. Ingest took 113 s, dependencies 288 s, expiry 33 s and
emit 73 s. `far-future` took 203.6 s: ingest 122 s and emit 73 s, with the same
canonical bytes.
