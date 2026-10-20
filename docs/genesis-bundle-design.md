# Genesis bundles and deterministic prefills

Status: bounded reference implementation, 2026-09-25. The signed genesis's
optional `PrefillLaunch` implements the small sponsored-rent prefill profile;
a signed genesis without it creates the entity namespace in block 1. This
document also keeps the larger-scale design, which goes beyond the shipped
envelope. The "baseline" and "stages" sections below are the design record of
2026-09-25; the current wire format is in
[genesis-bundle-v1.md](genesis-bundle-v1.md).
The tooling does not authorize or perform a deployment.

The exact implemented wire fields and runtime behavior are frozen in
[genesis-bundle-v1.md](genesis-bundle-v1.md).

## Implemented envelope

`arkiv-proposer genesis lock CHAIN [--refresh]`,
`arkiv-proposer genesis build CHAIN --out BUNDLE --locked`, and
`arkiv-proposer genesis verify BUNDLE` support the examples below. The current
implementation admits at most 256 prefill entities, a 1 MiB canonical stream,
8 MiB logical storage units and 256 positive allocations. Source documents and
payload files are each at most 1 MiB, their total exact input bytes at most 8 MiB,
and the inventory at most 512 files. Relative source paths are ASCII, at most
512 bytes; traversal, symlinks, duplicate paths and case collisions fail closed.
The compiler freezes each opened file into bounded memory, hashes those same
bytes, and accounts canonical row bytes before retaining normalized rows.
It uses in-memory sorting and a single bounded state construction; external
sorting, resumable bulk checkpoints, object packs and multi-chunk transport
remain future work. No million-row or multi-GiB support is claimed.

Inline or file allocations, inline entities, typed TOML/JSONL datasets, payload
files/text/padded Base64, allocation aliases and the immutable
`arkiv-entity-v2/glm-v2@1` preset all feed the same canonical builder. The lock
contains every source SHA-256/length, the expanded profile and its digest,
resolved identities/economics and the exact canonical artifact hashes. Refresh
reports changed resolved fields with before/after values; supplied pins remain
assertions during refresh. Builds do not refresh the source lock.

The portable directory contains `genesis.bin`, `prefill.bin`,
`genesis.lock.toml` and `report.json`. It is built privately and published with
an atomic no-replace rename. Independent verification reconstructs the full
initial state and compares canonical bytes, roots, sponsor debit and report;
source inventory remains provenance and requires the original source tree for
rechecking its contents. See the [runnable examples](examples/genesis-bundle/README.md).

## Recommendation

Author a chain in **`chain.toml`**, with small, named files for allocations and
datasets when they are useful; small chains can keep both inline. Run
**`genesis lock`** to generate content pins and expand the selected versioned
profile. Users should not calculate hashes or copy pricing arrays by hand.
Compile the locked inputs into an **Arkiv genesis bundle**: a compact canonical launch
record, a canonical entity stream, source pins, and an independently reproducible
initial state root. Install and verify the bundle before serving or producing
block 1. Keep `genesis.bin` as the machine interface and export JSON only for
inspection or compatibility.

The substantive change is a deterministic state-construction protocol, including
indexes, counters, expiry reservations and supply accounting. Changing the file
extension without that protocol would leave the difficult part unsolved.

Use the current account model: at most 256 immutable allocations, lazy account
records, fixed 1 gwei gas price. Recommend finite prefill lifetimes with rent paid
from an explicit genesis sponsor. Dataset installation consumes Arkiv creation
nonces, but does not invent signed transactions or consume account transaction
nonces. Create the `entities` namespace at height 0; block 1 is an ordinary block
in the new profile. The separate [base-fee plan](base-fee-v1-plan.md) is future
work and is not enabled by these examples.

The complete illustrative authoring set is in
[examples/genesis-bundle](examples/genesis-bundle/README.md). Its hashes pin real
files. It deliberately contains no invented state root or launch certificate.

## Baseline mechanisms and integration points

The following observations record the baseline that motivated this design.
The prefill launch adds the bounded integration described above. These are not
claims about another Arkiv implementation.

| Existing mechanism | Consequence for this design |
| --- | --- |
| [`Genesis`](../crates/arkiv-node/src/block.rs) and [`SignedGenesis`](../crates/arkiv-node/src/signed/protocol.rs) already have strict canonical binary codecs | Human syntax is an authoring layer. Do not replace the codec with TOML or JSON hashing. |
| `Genesis::header()` derives an empty catalog and host root | A prefilled root needs a new initializer and versioned header construction; copying rows into a running database is invalid. |
| The signed genesis carries an `AccountConfig` | Prefill sponsorship debits a declared allocation. |
| [`accounts.rs`](../crates/arkiv-node/src/accounts.rs) uses sorted positive `(OwnerId20, U256)` allocations, at most 256, and checked supply | Defaults authenticate through genesis. A missing account record means nonce 0 and its allocation, or zero balance. It does not imply that a funded account lacks state. |
| Signed block 1 is a proposer-only namespace bootstrap | The new prefilled profile must remove that special case in validation, producer, follower, pool and light verification together. |
| [`arkiv/mod.rs`](../crates/arkiv-node/src/arkiv/mod.rs) derives entity keys from chain, creator, creation nonce and salt | Creation nonces differ from account transaction nonces. Both must be specified at launch. |
| `$createdAt`, `$updatedAt`, `$expiresAt` are block heights | They are not Unix timestamps. The genesis header's `timestamp_ms` is separate. |
| Expiry uses authenticated per-height/per-namespace counters and a reservation of at most 16 deletions per block in the signed profile | Initial rows must populate those counters and respect the existing reservation. |
| [`DurableEngine`](../crates/arkiv-db/src/durable/mod.rs) initializes only the expected empty state and bounds prepared batches | There is no completed bulk-prefill API. A large genesis cannot be one enormous `Branch` or commit. |
| Stored trie handles and staging exist; indexes are derived from rows | They are useful building blocks, not evidence of an already scalable importer. |

## Authoring format

Prefer a deliberately restricted **TOML 1.0** schema for the small manifest,
profiles, allocation list and hand-authored entity fixtures. It provides comments,
named tables and duplicate-key rejection. Keep numeric protocol values that can
exceed signed 64-bit range as decimal strings. Large integers in JSON sources are
also strings, avoiding consumer precision differences. See the primary
[TOML specification](https://toml.io/en/v1.0.0) and
[JSON specification](https://www.rfc-editor.org/rfc/rfc8259).

| Candidate | Decision |
| --- | --- |
| TOML | Primary human format. Cap each document and reject unsupported value types. Small repeated entities use `[[entity]]`. |
| YAML | Readable, but tags, aliases and scalar-resolution choices add an unnecessary accepted-language surface. A safe constrained YAML frontend could be added later to the same compiler. See the [YAML specification](https://yaml.org/spec/1.2.2/). |
| Custom DSL / executable configuration | Do not introduce an evaluator, imports, loops, environment expansion, or arbitrary code into launch validation. Dataset generation is an explicit external authoring step whose output is pinned. |
| Typed JSONL | Streaming input for large exported datasets. One strict entity object per line; no whole-document array. It is an interchange format, not the human chain manifest or consensus encoding. |
| CSV / SQL dump / database snapshot | Optional external converters only. They must produce the typed format and report conversions; inferred types, locale parsing and hidden query order are unsuitable launch inputs. |

All schema versions reject unknown keys and duplicate object/attribute keys.
Decimals are exact signed 256-bit values scaled by 10^18; no binary floats,
rounding, NaN, or infinity. Integer strings are canonical decimal (`0` or a
nonzero digit followed by digits; negative only for signed types). Addresses and
fixed bytes use lowercase `0x` hex of the exact width. Attribute types are explicit
one-key tables such as `{ str = "Berlin" }`, `{ u64 = "42" }`, or
`{ dec = "12.50" }`; decimal spelling normalizes through exact integer units.
Boolean values remain booleans. There is no implicit string-to-number coercion.

Use UTF-8 without BOM. Reject multiline TOML string forms for semantic fields
because TOML permits platform-dependent newline normalization; external payload
files preserve their exact bytes. Ordinary escaped strings have their decoded
UTF-8 bytes preserved, with no Unicode normalization. Schema names/IDs are ASCII;
user values follow the current Arkiv validation rules. A quoted UTC start string
must be exactly `YYYY-MM-DDTHH:MM:SSZ`, with no leap second, and converts to a
checked nonnegative integer millisecond timestamp. Never derive a consensus field
from the clock, filesystem metadata, environment, locale, or random generator.

### Make the easy path genuinely small

The [inline example](examples/genesis-bundle/chain.inline.toml) has no content
hashes, file lengths or pricing array to maintain. Its essential form is:

```toml
schema = "arkiv-genesis-bundle-v1"
protocol = "arkiv-launch-prefill-v1"
profile = { preset = "arkiv-entity-v2/glm-v2@1" }
# Explicit chain identity/time and prefill policy omitted from this excerpt.

[[allocations.entry]]
label = "catalog"
address = "0x2222222222222222222222222222222222222222"
balance_wei = "10000000000000000000000"

[[prefill.entity]]
creator = "@catalog"
create_nonce = "0"
salt = "10"
expires_at_height = "86400"
flags = 0
content_type = "text/plain"
payload_text = "Welcome to Arkiv."
attributes.title = { str = "Welcome" }
```

Authoring permits exactly one inline allocation list or allocation-file
reference. Inline entities and dataset sources can coexist; the same global
sorting/duplicate rules apply to their union. Account aliases resolve only
against unique labels in the current allocations. `payload_text` encodes exact
decoded UTF-8 bytes. A binary exporter can use `payload_base64` with strict
standard padded Base64 and no whitespace, keeping large JSONL datasets independent
of a one-file-per-entity layout. Exactly one of text, Base64 or a payload-file
reference is allowed; all decode to bytes under the same payload/frame limits.
The canonical stream has neither aliases nor inline/file distinctions.

`genesis lock chain.toml` writes a generated `genesis.lock.toml`: it pins the
authoring source and every transitive file by hash/length, expands the selected
versioned builtin preset into all constants, and records the resolved semantic
recipe inputs. Missing pins are allowed during authoring; supplied pins are
assertions and must match. The lock command does not fetch network resources,
choose a timestamp, alter expiry, invent a run ID or execute a generator.
Its output includes a human diff of resolved economics, identities and datasets.
Refreshing an existing lock is an explicit command. Build/verify with `--locked`
requires a matching complete lock and never updates it automatically.

A builtin preset is identified by a version, not `latest`; its expansion and
digest are recorded in the lock. Changing a compiler binary must not change that
expansion silently. The fully pinned
[chain.toml](examples/genesis-bundle/chain.toml) and profile files in this proposal
illustrate the expanded inputs for review, not mandatory hand-written boilerplate.

Paths resolve relative to the bundle root, including paths inside dataset files.
Only regular files under that root are accepted: reject absolute paths, `..`,
empty/dot segments, symlinks, device files, backslashes, duplicate normalized
paths and case-folding collisions. Freeze opened inputs into an immutable staging
area while hashing them; do not check a path and reopen mutable bytes later.
Every locked reference includes `sha256` and exact byte length. No globbing, implicit
includes, URL fetches, or shell substitutions occur during build or verification.
A separate optional fetch command can retrieve hash-pinned bytes into this root,
with bounded downloads and no credentials in the resulting bundle.

## Four artifacts, two kinds of identity

1. **Source tree and generated lock:** `chain.toml`, inline values or separate
   profiles/allocations/datasets/payloads, and `genesis.lock.toml`. Labels and
   comments help review. The lock command resolves and pins them completely.
2. **Canonical recipe:** expanded launch parameters and the commitment to one
   typed, ordered entity stream. This is the deterministic input to the builder.
3. **Launch record:** the recipe commitment, initial `StateHeader`, initial burn
   and counts, plus all immutable chain parameters. Its height-0 header hash is
   the genesis hash clients pin. The proposer can certify that exact header.
4. **Portable bundle:** canonical files, source inventory, build report and
   optional content-addressed state objects. A redb file is a local build cache,
   not the portable or authoritative genesis format.

Keep semantic identity separate from transport/provenance identity:

```text
source file pin   = SHA-256(exact file bytes)
stream_hash       = digest("arkiv/prefill-stream/v1", canonical_stream)
recipe_hash       = digest("arkiv/genesis-recipe/v1", encode(RecipeV1))
state_root        = initial_state_header.root()          # existing state codec
genesis_digest    = digest(new_signed_genesis_domain, encode(LaunchGenesisNext))
genesis_hash      = hash(height_0_header(genesis_digest, state_root))
```

Here `digest(domain, bytes)` is the existing
`Keccak256(u8(domain_length) || ASCII(domain) || bytes)`, not SHA3-256 and not raw
Keccak of the payload. Use an incremental hasher for streams. The codec keeps
checked big-endian fixed-width integers, explicit tags, and bounded length/count
fields; accepted machine bytes must re-encode identically. Freeze exact field
tables and golden vectors before implementing a reader in another language.

`RecipeV1` contains expanded native genesis parameters, run ID, proposer, cadence,
signed limits, `AccountConfig`, the builder/rent policy IDs, namespace policy,
prefill limits, sponsor, `stream_hash`, row count and canonical byte count. It
does **not** contain the final root, certificate, local build ID, compiler
binary hash, paths, file chunks, source formatting, or a hash of itself. The
launch record adds `recipe_hash`, the full initial `StateHeader`, and derived
prefill burn/counts to those parameters. Recompute duplicated/derived fields
and reject disagreement; they are not independent overrides.

Changing TOML layout, file paths, record input order, or transport chunk size
leaves semantic identity unchanged when the resolved values are identical.
Source pins and the provenance inventory change appropriately. Changing a
semantic value, including a creation nonce or run ID, changes the recipe and
genesis identity. Different settings may legitimately give the same state root;
the genesis hash still commits to them. Entity keys use chain ID rather than
genesis hash, so root construction has no circular dependency.

### Canonical prefill stream

The proposed stream layout is `ASCII("ARKPFL01") || u32(codec_version) ||
u64(row_count) || repeated(u32(frame_length) || encode(EntityV1))`.
`EntityV1` fields, in order, are creator `[u8;20]`, creation nonce `u64`, salt
`u128`, expiry height `u64`, flags `u8`, content type `str`, payload `bytes`, then
`u32(attribute_count)` pairs of `(CellName, Value)`. Use the pinned engine value
codec, not public SDK type IDs as internal tags. Payload references and account
aliases are resolved before encoding. No system attributes appear in this
user-attribute list. Attributes sort by exact name bytes, with duplicates rejected.

Sort the entire logical stream by `(creator address bytes, creation nonce as
u64)`, regardless of source-file order. Require per-creator nonces to be exactly
`0..N-1`, including across datasets; reject duplicates and gaps. Thus chunking,
parallel parsing and sorting never change record IDs. Use bounded external sort
and bounded fan-in merge for unsorted inputs, not a map holding all rows. A
converter may emit already ordered streams to avoid that sort. Incremental
framing/count verification rejects truncation, extra frames and trailing bytes.

Source IDs and labels are authoring metadata, not entity identity. No caller-
supplied entity keys, arbitrary host keys, namespace IDs, internal record IDs,
account nonce overrides, arbitrary system cells, updates or deletes are accepted
by builder version 1. References between entities are explicit typed key values;
an optional authoring helper can calculate them from the declared tuples. It
must never resolve a label differently based on file discovery order. Dangling
key values retain the current Arkiv semantics; labels do not create extra state.

## State-construction rules

Use a dedicated versioned genesis builder sharing the normal entity validation,
system-field construction, row/index mutation, key derivation, expiry counter
and rent functions. Do not use unaudited raw trie writes as the reference path.
It has no transaction signatures, mempool, block receipts or execution scheduler.
Each row is validated as a whole; any invalid input rejects the entire launch.

| State component | Builder version 1 rule |
| --- | --- |
| Namespace | Create only `entities`, namespace ordinal 1, owned by the configured proposer, with the descriptor/revision produced by the pinned engine create operation. It exists even for an empty dataset. `next_namespace = 2`. |
| Record allocation | Create in canonical stream order. Record ordinal is global zero-based stream position plus 1; `next_record = row_count + 1`, checked for overflow. Normal engine creation determines all revisions and index handles. |
| Entity key | Reuse current `derive_entity_key`: `Keccak256(24 zero bytes || chain_id:u64be || ARKIV_ADDRESS:20 || creator:20 || creation_nonce:u64be || salt:u128be)`. Reject any derived key collision. |
| Entity ownership | `$creator = $owner = creator`. No genesis transfer shortcut in version 1. A third party installing an entity does not prove the creator signed it: launch authority explicitly assigns these initial entities. |
| System heights | `$createdAt = $updatedAt = 0`; `$expiresAt` is the explicit finite expiry height. Header timestamp is the configured millisecond timestamp. |
| Remaining cells | Generate `$key`, `$creationFlags`, `$payload`, `$contentType` with the current types/indexing. Flags permit only bits 0/1. Require explicit flags/content type/payload in canonical rows; empty values are valid. Preserve current total-cell and value limits, including generated cells. |
| Entity nonce | The current `arkiv/nonce/` host counter is `N` for each creator with `N` imported entities; omit zero counters. The next live create uses nonce `N`. Salt need not be unique if its creator/nonce tuple differs. |
| Account nonce | Every allocation starts at transaction nonce 0; the namespace and entity import consume no account transaction nonce. Omit all default account records except a sponsor with a positive debit. |
| Expiry state | Materialize exactly the normal due/total counters and indexed `$expiresAt` values. Counts match rows, zero entries are omitted, and every finite height fits the reservation. |
| Supply | Preserve allocation defaults, apply the explicit sponsor debit, and initialize the authenticated burn record when positive. No credit, transfer, mint, hidden beneficiary or negative balance occurs. |
| Initial header | Commit catalog/host roots and lengths, namespace allocator, pricing and all nested namespace/row/key/term/posting state through the existing `StateHeader` format. |

There are two distinct allocators, an entity-creation nonce and an account nonce;
the builder must not reconstruct one from another. Importing two entities for
Alice leaves her next creation nonce at 2 and her first signed transaction nonce
at 0. That transaction may create an entity, extend expiry, or perform another
allowed operation. The new block-1 rule does not insert a hidden nonce-0 proposer
transaction. Existing version-3 chains still do exactly that bootstrap.

### Prefill rent and supply

Recommended explicit policy ID: `sponsored-rent-v1`. Let `U_i` be the existing
`gas::storage_units` of the complete generated entity, and `E_i` its finite
expiry. Calculate each row's rent with the existing function at height 0:

```text
rent_gas_i       = ceil(U_i * E_i * 8 / 17280)
genesis_burn_wei = sum_i(rent_gas_i * 1_000_000_000)
initial_supply  = checked sum of immutable allocations
sponsor_balance = sponsor_allocation - genesis_burn_wei
total_supply_0  = initial_supply - genesis_burn_wei
burnt_0         = genesis_burn_wei
```

Match the current checked intermediate widths and per-row upward rounding;
sum monetary amounts with checked U256 arithmetic. Reject an overflowing row
or sum, insufficient sponsor balance, or a sponsor absent from the allocations.
The manifest's expected initial supply is an assertion, never a mint instruction.
Report gross allocation supply, genesis burn, remaining supply and net sponsor
balance separately, in exact base units and human GLM display.

Genesis import is explicitly exempt from transaction intrinsic and namespace/
entity execution fees under this proposed policy. Those operations are bounded
offline construction work; there is no fabricated transaction gas receipt or
block-0 gas usage. Rent still prepays finite live occupancy. The build reports
execution work/cost for capacity planning, but that report does not debit money.
This exemption is a policy recommendation requiring acceptance before protocol
implementation, not current GLM-v2 behavior or an implicit “unlimited gas” path.

Once launched, normal GLM-v2 fees, rent extension, no-refund rules and supply
accounting apply. Genesis rent is not charged again. Default allocation plus a
host nonmembership proof remains sufficient for untouched accounts. A sponsor
proof includes its materialized account; supply proofs include the initial burn.
Zero rent means no sponsor/burn record is materialized. This does not create a
second spendable copy of the sponsor's original allocation: stored accounts
override, rather than add to, defaults.

### Expiry is a real launch constraint

Require `1 <= expiry_height <= max_lifetime`, rejecting `u64::MAX` in this
profile. Validate both per-namespace and total counters before committing any
launch state. In the current signed limits, one million entities at the same
expiry height must fail: the reservation permits **16**, not one million.

One million expirations need at least 62,500 distinct block slots at 16 per
block, about 3.62 days of blocks at five-second cadence. This is only the minimum
spread, not a recommended lifetime. That spread must itself fit `max_lifetime`.
These reservations also consume capacity that later live transactions might
want at those heights. The report must show occupied expiry slots and peak
counts, including the first sweep at height 1.

The builder never silently staggers, shortens, extends or truncates a dataset.
A separately invoked authoring helper may propose explicit values such as
`start_height + floor(global_ordinal / 16)` and write new source files with a
diff and new pins. The reviewed result is the input. Permanent genesis datasets
or delayed sweeping require their own retention, pricing and proof contract;
they cannot be enabled by a magic value in this format.

That separate contract now exists as prefill launch versions 3 (small) and 4
(streaming), selected only by an explicit `[prefill] allow_permanent = true` and
committed by the launch version and recipe domain. It admits `u64::MAX` rows
with no expiry scheduling and no time-based rent (storage units still count);
finite rows are unchanged. Live execution gains nothing: permanent creation,
extension to permanent and updates of permanent entities stay rejected, and only
deletion applies to them. Versions 1 and 2 keep rejecting the sentinel.

## Future larger-scale construction and portability

This section preserves the larger importer proposal. It is not the v4 admission
contract: the implemented ceilings remain 256 rows / 1 MiB canonical stream as
specified above. Chunking, external sorting and checkpoints here are unimplemented.

Keep the launch descriptor within the current 64 KiB genesis transport/identity
budget. Do not embed rows, source inventories or growing chunk lists in it.
With the existing 256-allocation ceiling the allocation vector alone takes
13,312 bytes plus its 12-byte configuration framing. A million allocations do
not fit this design just because datasets can stream.

Suggested future importer admission ceilings, separately versioned from engine
limits: 64 KiB chain/profile documents, 1 MiB small TOML datasets, 512 KiB JSONL
lines, 256 KiB canonical entity frames, and 1,024 input files. A future measured
envelope could target at most one million rows / 2 GiB canonical stream,
whichever is reached first, plus an explicit logical storage-unit ceiling.
These are proposed admission ceilings, not demonstrated safe operating sizes.
Each launch commits stricter or equal declared row/byte/unit limits; checks
cover expanded payloads and repeated references, not just compressed or unique
file sizes. Exceeding a limit fails, never produces a smaller chain.

Transport the canonical stream in chunks, conventionally at most 16 MiB and
ending between frames, listed with byte lengths and SHA-256 hashes. Concatenating
chunks gives exactly the canonical stream; chunk boundaries do not enter its
semantic hash. Inventory files have explicit bounded counts/bytes. Build version
1 can use an uncompressed directory; a later archive transport must reject path
escapes, links, duplicate entries and decompression bombs and enforce expanded
size caps. Every required file must be included for an offline portable bundle.

The existing durable defaults are an 8 MiB decoded-node cache, 16,384 cache entries,
32 MiB prepared payload budget, 100,000 staged objects and 1,000,000 staging
visits. These are runtime policies and do not establish a bound for an entire
import. Local sort/cache/staging budgets may change batching or cause a resource
error, never state semantics. Check size before allocating/encoding; parse one
bounded row at a time and spill data and duplicate checks to disk. Reopen each
checkpoint as stored handles so the builder does not retain the whole trie or
all prior branches, receipts, objects or sort runs in memory.

Use this construction lifecycle:

1. **Resolve/freeze:** require the generated lock, parse and validate bounded sources, verify pins into a
   private staging directory, expand profile constants and aliases. Check
   allocation supply and all independent type/range/admission constraints.
2. **Normalize:** external-sort rows into the canonical stream. Check contiguous
   nonces, duplicate keys, expiry counts, byte/storage-unit totals and rent.
   Compute the recipe before installing anything. Pinned canonical streams can
   be verified directly without the authoring parser.
3. **Build privately:** insert rows and their indexes with bounded mutation
   batches. Persist new content-addressed objects and an import checkpoint
   atomically, including recipe hash, builder version, canonical byte/row cursor,
   current `StateHeader`, allocator/nonces, rent accumulator and checkpoint
   generation. Import generations are not chain heights or published blocks.
4. **Resume exactly:** verify recipe, stream pins and checkpoint identity, load
   the latest durable generation, and continue after its committed row. The
   journal cursor, counters and objects must commit together; a lost commit
   acknowledgement must be reconciled by reading that exact generation. Never
   charge rent twice, skip a row, or reuse a checkpoint for altered input. A
   torn, untrusted or inconsistent checkpoint can be discarded and rebuilt.
5. **Seal/verify:** apply sponsor/burn state, finalize all derived counts and
   `StateHeader`, then independently rebuild or verify the logical state before
   emitting a launch record. Require a second independent build to reproduce
   the recipe/root/genesis hash before a release is certified.
6. **Install atomically:** in a fresh target store, stage verified reachable
   objects before publishing a single height-0 manifest/header/identity packet.
   The final namespace and host state must replace the empty-state expectation
   through a new explicit initialization API. Create the final deployment
   identity only once the genesis hash exists; temporary import identities are
   separate. Sync required history/body files before publishing references to
   them, and use existing commit-uncertainty handling. Nodes expose no head or
   service while incomplete. Reopen and check the exact genesis/root before
   enabling reads or signing block 1.

Steps 3/6 require new durable-storage support. The present single prepared batch
and empty `EngineGenesis::initial_header` cannot be repurposed merely by changing
a filename. A possible implementation stages objects plus private import records
without a normal chain head; the final atomic metadata commit creates height 0
with sequence 0 and no parent. Staging checkpoints must not leak as genesis
history. Filesystem publication, if used, needs same-filesystem atomic rename,
directory durability and exclusive ownership; never replace an existing chain.
An interrupted install is retried against an absent or exactly matching target.

Content-addressed object packs are optional acceleration. Hash every object,
respect its physical object space (catalog, host, namespace), check bounded
encoding and complete root reachability, and reject malformed typed leaves.
Object hashes alone do not prove that rows, keys, postings, counters and rent
are consistent with the recipe. Initially full verification rebuilds from the
canonical rows; an optimized verifier must independently derive all those maps.
Missing object bytes cause failure even when the advertised root is correct.

## Verification and trust

| Participant / mode | What it checks and what it trusts |
| --- | --- |
| Producer | Full recipe validation and deterministic build/root match; exact launch identity; proposer certificate/key address; atomic height-0 installation; ordinary block-1 execution and account nonces. No signing before verification. |
| Full follower | Independently rebuilds from the pinned canonical stream, compares roots and allocation/rent/counter invariants, installs locally, then replays blocks. A producer-supplied database is not evidence of execution. |
| Light client | Starts with an externally chosen genesis hash; verifies the bounded canonical launch record and height-0 header, certificate and requested proofs against its committed root. Lazy balance proofs also consult committed allocations. It need not download datasets. |
| Snapshot/fast bootstrap | May verify reachable state objects against a trusted genesis root without rebuilding the recipe, but must advertise that trust mode. It is not independent prefill replay or proof of data availability. |

A light client authenticates a selected genesis and proof answers; it does not
establish that the entire prefill was valid or remains downloadable. A proposer
signature authenticates authorship, not public agreement on a unique chain or
data availability. Distribution must retain canonical stream chunks and enough
state/proof service for new followers; publish their byte counts and hashes in
the portable inventory. Full rebuild checks are offline launch requirements,
not secretly delegated to every light client.

Keep immutable launch parameters in the signed record: version/profile tables,
chain ID/run ID, genesis timestamp, cadence, proposer public address, admins
(empty here), limits, all pricing weights and host rent rules, allocations,
initial state commitment, builder/namespace/prefill policy and input commitment.
Changing any of these starts a new chain or requires an explicitly designed
protocol upgrade. Do not let a mutable named profile override compiled values.

Keep private signing keys, keystore passwords, RPC credentials, peer URLs,
listen addresses, filesystem paths, TLS, service names, metrics and local cache/
sort/disk budgets in deployment/runtime configuration. Public proposer identity
belongs in genesis; possession of its key does not. Generated runtime config
pins `genesis_hash` and never overrides its economics or initial state.

## Failure and validation matrix

| Case | Required behavior / validation gate |
| --- | --- |
| Comments/reordering/renamed source file/equivalent TOML and JSONL | Same expanded recipe, canonical stream, root and genesis hash; changed provenance pins are expected. |
| Duplicate TOML/JSON key, repeated attribute, wrong type, overflow, unknown field/version | Reject with source path/row and a bounded diagnostic before launch publication. |
| Payload mutation, swapped file, truncated chunk, stale pin, path traversal/symlink | Reject hash/length/path mismatch; never silently fetch substitutes or follow outside paths. |
| Empty prefill | Still create namespace 1 in the new profile; zero burn, untouched allocation defaults, ordinary block 1. Compare an explicit golden root. |
| Creator nonce gap/duplicate/overflow, salt or chain change, derived key collision | Reject invalid nonce/key stream; valid changes produce recomputed keys and identity. Exercise first live create after import. |
| Signed nonce confused with creation nonce | Fixture with two imported creates and first signed nonce 0; its new entity uses creation nonce 2. |
| False system height/owner/key, unknown system attribute, too many total cells | Reject author overrides; independently check generated row projection and all indexes. |
| Expiry 0/permanent/too far, 17 rows at one height, inconsistent due count | Reject. Cover exactly 16, first expiry at block 1, later user scheduling and complete sweep/proofs. |
| Excess allocations, duplicate/zero balance, U256 supply overflow, unfunded sponsor | Reject; exercise 256/257 boundary, exact funding and one-wei shortfall. |
| Per-row vs aggregate rent rounding, sponsor default added twice, double rent on resume | Pin monetary vectors and proofs; final balance plus burnt supply must reconcile to allocations. |
| Changed object root or valid hashes with wrong postings/nonces/rent | Full rebuild rejects inconsistency; proof readers fail on malformed or missing objects. |
| Different chunk/batch/sort/cache sizes, serial vs parallel parse, independent clean builds | Identical canonical frames, record IDs, roots and genesis hash; physical DB bytes may differ. |
| Memory/disk/line/frame cap reached, one oversized row, decompression amplification | Bounded explicit failure with no published chain; no unbounded allocation or partial successful import. |
| Crash before/after object checkpoint, commit acknowledgement lost, crash at final install | Fault injection proves exact continuation or safe restart and no exposed partial height 0. |
| Existing destination or stale checkpoint for another recipe | Refuse identity mismatch; no overwrite or in-place re-genesis. |
| Light proofs at genesis and after first mutation/expiry | Verify rows, empty/nonempty account paths, sponsor debit and burn; reject wrong-chain/wrong-root and omitted matches. |
| Unsupported old binary opens new genesis | Fail closed before initialization; all old golden genesis bytes/hashes remain unchanged. |

## Honest scale limits

Streaming bounds input memory; it does not make trie construction free. Cost
scales with rows, payload bytes, indexed attributes, distinct terms/postings,
sort spill volume and durable writes. Common attributes still create postings
for every record. Report peak RSS, scratch/final disk, objects/bytes by map,
read/write amplification, time, restart time, proof sizes, expiry histogram and
rent. Test increasing volume and skewed indexes, not only distinct tiny rows.

First ship a small validated builder, then demonstrate larger envelopes under
fixed RAM/disk budgets before advertising them. A million-row / 2 GiB limit is
an admission ceiling, not a benchmark result or a promise that current redb
builds can import it economically. Bound sort fan-in and open files. Account
creation-nonce state may grow with distinct dataset creators even though account
allocations remain small; include it in disk/host-map measurements.

Expiry removes live rows under the current complete-sweep rule. It does not
reclaim retained old snapshots or signed history. Genesis data may remain
reachable from height 0 indefinitely, and the canonical distribution stream is
another copy. Deduplication and eventual pruning need separate measurement and
policy. Rent units are logical capacity accounting, not claimed disk bytes or
an archive-storage payment.

Millions of genesis-funded accounts need a separate design: either a committed
allocation trie with membership/nonmembership proofs plus dynamic overrides,
or fully materialized initial accounts with allocation data outside the bounded
launch descriptor. Both require new account-proof semantics, streamable checked
supply accounting, authenticated completeness and cross-language vectors.
Keep the <=256 vector in the first bundle profile; do not lift its count limit
and assume the 64 KiB RPC/storage/light-client bounds will accommodate it.

## Compatibility and implementation stages

1. **Authoring frontend without consensus changes.** Define schemas and a CLI
   `genesis check/lock/compile/inspect`. Implement inline values, optional source
   pins, immutable preset expansion and a generated lock before adding streaming
   datasets. A no-prefill legacy target emits exactly the
   existing v1/v2/v3 `genesis.bin` bytes for explicit inputs, preserving the
   empty state and old block-1 bootstrap. An import/export adapter can accept a
   known legacy JSON schema or binary; unknown fields/layouts fail. Prefill
   targets remain explicitly unsupported at this stage.
2. **Freeze the next launch contract.** Assign new signed-genesis/native protocol
   identifiers and digest domains; preserve state layout if byte semantics are
   unchanged. Specify exact `RecipeV1`/launch field order and bounded decoding,
   genesis header construction, initial sponsor burn, block-1 dispatch and light
   proof semantics. Accept the prefill rent policy before implementation.
   Never append optional fields to v3 or reinterpret a feature flag.
3. **Small reference builder.** Implement canonical input stream and reusable
   height-0 entity construction; compare every derived map/root against a
   separately checked small model. Golden fixtures cover empty, multi-creator,
   sponsor, key/nonces, boundary rent and staggered expiry. The CLI builds and
   verifies locally, without service changes.
4. **Durable import and atomic initialization.** Add explicit private import
   checkpoints and initial-root publication to durable storage, host metadata,
   history and reopening. Exercise the crash matrix and resource limits. Avoid
   all-state maps in validation paths; source normalization and consistency
   checks must also spill or stream.
5. **Portable verification and measured scale.** Add chunks/inventories and
   optional object packs; reproduce roots on independent builds and full
   followers, then test light genesis proofs and ordinary block 1. Benchmark
   progressively larger and skewed datasets, documenting the largest supported
   envelope and physical limits. Only then propose a concrete launch separately.

Current entry points that will need coordinated work include
`block.rs::Genesis::header`, signed `protocol.rs` genesis/block1 validation,
`accounts.rs`, `arkiv/mod.rs`, `signed/host.rs`, `signed/light.rs`, `signed/pool.rs`,
`signed/runtime.rs`, CLI genesis loading, and durable `format.rs`/initialization.
The proposer constructor's bootstrap-funding rule must become version-specific:
the old rule remains for v3; the new profile has no bootstrap transaction and
only enforces the chosen sponsor debit. An optional operational funding check
must not masquerade as an old consensus bootstrap requirement.

Existing stores are never edited in place. An existing chain continues from its
original genesis. A new chain from exported application data uses a reviewed
conversion and new genesis identity; builder v1 regenerates keys/heights and
does not preserve historical creators' transaction signatures or arbitrary
system metadata. A state-preserving fork/migration tool is a different protocol
and is outside this first builder. Source pins, expected roots and certificates
must be reviewed again after any semantic change.

## Material choices before implementation

The syntax and construction details above can proceed as design defaults. These
choices change launch economics or the useful scope and should be settled before
implementing the next protocol:

- Accept sponsored finite rent with no genesis intrinsic/write fees, or choose
  another explicitly priced prefill policy. Select the actual sponsor/allocation
  amounts only for a concrete launch.
- Choose the dataset's required lifetimes and an explicit expiry distribution
  that fits 16 deletions per block. Permanent reference data would expand the
  scope to a new retention/sweep/pricing contract.
- Confirm the first launch fits <=256 funded accounts and a measured dataset
  envelope. A larger account distribution or state-preserving migration changes
  the protocol scope and should not be hidden in an import flag.

Public chain ID/run ID, proposer and start time are launch-specific inputs, not
blockers for building the tooling. The example uses synthetic public addresses
and a fixed future date solely to make all authoring inputs explicit.
