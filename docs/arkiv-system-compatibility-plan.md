# Arkiv system and scalar compatibility plan

Status: implementation authorized by the user on 2026-09-24; delivered as the node's Arkiv entity protocol (originally behind an `arkiv-v2` cargo feature, now the only build). See [implementation contract and validation](arkiv-v2-compatibility.md). The original plan below is retained as design context. Later user authorization permits stopping the existing experiment and deploying fresh compatible stores; old stores must never be reinterpreted in place.

## Recommendation

Keep the generic Merkle database and add an explicit, versioned Arkiv entity execution profile above it. Extend the database with the missing *distinct* scalar types and larger names/strings. Implement Arkiv system fields as host-maintained committed cells, with per-entity lifecycle rules. Translate SDK type IDs at the API boundary; do not reinterpret existing engine IDs. Deliver correct system-field storage, projections and bounded equality proofs before adding a full query-language/RPC surface.

This provides semantic compatibility with Arkiv entities and scalar values, not byte-identical production state roots, transaction processing, index layout or consensus. The simulator stays driven by an internal unsigned producer. Record this distinction in machine-readable capabilities and the demo.

## Evidence and source precedence

Inspected local revisions:

- Database/node: `7a2321504e148c4d4d90b18dfa9c3983a46e9a61`.
- Indexer experimental: `e7e42acf544145a0c111dc1b80cd910c8db5d6c1`.
- Arkiv production node: `0fd58ce5bd6a0a196b0079f60d4679539346adc6`.
- JS SDK: `db27f4c360e1b1e081bb674224fb6858b4356091`.
- Architecture: `e63c2c80450955d71cfb9c9dc4f827eb39ccedd0`.

Source paths are relative to `/home/ubuntu/arkiv-network`:

- `arkiv-architecture/planning/260713-followup/arkiv-engine.md`: sections 2–6, entity operations, system authority, expiry, type/index encodings.
- `arkiv-architecture/planning/260713-followup/arkiv-node-api.md`: sections 2–5, ABI/type IDs, public names, query operators and projection shape.
- `arkiv/crates/arkiv-interfaces/src/entity.rs`: `AttributeValue`, `AttributeType`, canonical natural-width storage versus ordered index bytes, flags.
- `arkiv/crates/arkiv-bindings/src/types/ident32.rs`: protocol name grammar/padding.
- `arkiv/crates/arkiv-reth-executor/src/decode.rs`: sorted triples, tombstones, expiry resolution, key derivation.
- `arkiv/crates/arkiv-reth-executor/src/arkiv.rs`: authorization, active checks, last-modified behavior.
- `arkiv/crates/arkiv-query/src/parse.rs`: actual recognized public system query names.
- `arkiv-sdk-js/src/attr/{names,types,values,codec,decimal}.ts` and `src/query/expression.ts`: current client vocabulary, constraints and query-builder restrictions.

Use the architecture contract as the intended public target, pin actual node/SDK behavior as interoperability fixtures, and list deliberate extensions/deviations instead of silently mixing them. These sources disagree in several places:

1. API specification permits comparisons on `$updatedAt`; current node parser rejects it. SDK query builder also rejects `$createdAt` and `$contentType`, although current node parser accepts those. Recommendation: index all intended scalar system query fields, enable their equality through our typed simulator interface, advertise `$updatedAt` querying as an extension to current node behavior, and keep a strict current-SDK capability profile. Do not claim SDK builder support until SDK changes are separately authorized.
2. SDK rejects reserved query words and `--` in names in addition to Ident32 grammar. Protocol Ident32 itself has a broader admission grammar. Recommendation: distinguish protocol-valid names from SDK-safe authoring names. Demo/API authoring uses SDK-safe validation; a future strict ABI decoder follows the pinned protocol and does not retroactively reject protocol-valid historical names.
3. Public `$expiresAt` and `$createdAt` differ from old committed-store `$expiration`/`$createdAtBlock`. Use public names only; never expose those legacy storage aliases as canonical API names.
4. Equal-expiry extension is documented as a satisfied no-op; current executor still stamps `last_modified_at_block` through `mutate`. Recommendation: initially match pinned executor behavior (successful extend stamps `$updatedAt`, even if expiry value is unchanged), explicitly document it and pin a regression vector. A later upstream decision can change it through a protocol version.
5. Architecture logical expiry allows delayed physical purge; this simulator currently completes expiry before publishing a block and refuses to publish if expiry fails. Preserve that stricter implementation for this milestone: it gives the same visible live set and simpler complete equality proofs. Do not add delayed purge or pretend the current expiry schedule is production's index-only purge implementation.

## Current gaps that cannot be fixed by renaming fields

`crates/arkiv-db/src/record.rs` currently has names up to 20 bytes, indexed values up to 64 bytes, and local tags bool=1/i64=2/u64=3/str=4/bytes=5. Its generic grammar allows `:` and `$`; cells have no built-in semantic meaning. `layout.rs` uses fixed-width term-key fields derived from these limits. Raising those limits changes trie keys and roots, including roots of existing short-name data.

`arkiv-node/src/block.rs`, `validate.rs`, and `exec.rs` currently provide generic namespace operations, caller-assigned record keys and injected `$expiresAt`; generic namespace ownership is not Arkiv per-entity ownership. Recreating a deleted key is allowed by the generic engine, unlike normal Arkiv key derivation/non-reuse semantics. Keep that generic behavior outside the Arkiv profile.

The simulator exposes only bool/i64/u64/str equality and metadata projections. `proof.rs` limits witness decoding according to the existing type/name/length profile. Storage, block envelopes, request fingerprints, light verification and the browser's Patricia decoder all depend on those encodings.

## Target scalar model

| Public type | SDK ID | Value and supported predicates |
|---|---:|---|
| bool | 1 | Canonical 0/1; equality |
| i32 | 2 | Signed 32-bit exact bounds; equality/range |
| u64 | 3 | Unsigned 64-bit; equality/range |
| u256 | 4 | 32-byte unsigned big-endian; equality/range |
| dec | 5 | Signed 256-bit two's complement scaled by 10^18; equality/range |
| bytes32 | 6 | Exactly 32 opaque bytes; equality |
| bytes | 7 | Unindexed variable bytes, system payload only at Arkiv boundary |
| str | 8 | UTF-8 up to 128 bytes, pinned allowed-character rules; equality/prefix |
| addr | 9 | Exactly 20 address bytes; equality |
| key | 10 | Exactly 32 entity-reference bytes; equality; dangling references allowed |

Add native variants to generic `Value`/`TypeTag`; never represent `addr`, `key`, `bytes32`, `u256`, and `dec` as a shared untyped bytes/string value. Preserve existing local IDs 1–5. Allocate new internal IDs in an explicit engine table (proposed i32=6, u256=7, dec=8, bytes32=9, addr=10, key=11), and introduce a separate `ArkivTypeId` 1–10 table in the adapter. `i64` remains generic-only; Arkiv boundary rejects it rather than silently narrowing it to i32.

Keep three encodings separate: SDK ABI words/raw variable values; generic canonical row serialization; typed ordered term bytes. Signed index bytes flip the most significant sign bit; unsigned are big-endian. Do not bias signed storage bytes a second time or treat natural-width storage as ABI. Decimal parsing is exact, rejects excess precision/overflow, and never uses floats. JSON u64/u256/decimal units remain strings; addr presentation may checksum but comparisons use bytes. Pin canonical decimal rendering and negative-zero normalization against SDK fixtures.

Increase generic cell capacity to 32 name bytes and 128 indexed value bytes in a new layout version. Keep the generic permissive grammar where harmless; the Arkiv host validates names strictly and allows only recognized system names. Namespace-name limits are independent: audit `namespace.rs`, which also has a 20-byte limit, and do not mechanically widen unrelated namespace encodings. Indexed string admission counts UTF-8 bytes, not characters. System payload support targets the specification's 128 KiB cap with independent aggregate request/block/proof caps; a large valid entity may exceed a multirow proof budget and must get an explicit bounded error rather than a partial result.

## Committed system-field model

Use typed cells in the existing row trie for the initial implementation; a compact custom entity header can be considered later if measured memory requires it. No metadata sidecar may be the authority for proof-visible fields.

| Public field | Internal value / indexing | Authority and projection |
|---|---|---|
| `$key` | Key; indexed equality | Host-derived 32-byte key, immutable; must equal structural RecordKey bytes |
| `$creator` | Addr; indexed equality | Caller of create, immutable |
| `$owner` | Addr; indexed equality | Initially creator; changes only via transfer |
| `$createdAt` | U64; indexed | Creation block, immutable |
| `$updatedAt` | U64; indexed | Host-set block of successful mutation; advertised query extension where needed |
| `$expiresAt` | U64; indexed | Resolved create/extend expiry; MAX is permanent |
| `$creationFlags` | Unindexed U64 field constrained to valid u8 bits | Immutable create argument; project `{readonly, permissionlessExtension, raw}`; not queryable |
| `$contentType` | Str; indexed | User-managed only through typed create/patch fields or allowed protocol triples |
| `$payload` | Bytes; unindexed | User-managed opaque bytes; never queryable |

The raw flags field is an internal storage choice, not a new public scalar type. Reject reserved flag bits. Preserve distinction between zero-length payload/content type and absent user attribute; pin create omission and tombstone projection behavior to executor/SDK fixtures before coding that path.

Materializing `$key` lets ordinary complete-Eq proofs work without a special query engine. It duplicates the structural key deliberately; host writes and verified row decoding must check equality, and golden tests must reject disagreement. A future virtual-key optimization needs a separately specified proof profile. Structural `#key` is not a user attribute and is never accepted as an alias.

All unknown `$` names and all attempts to set/remove host-owned system fields fail. `$payload`/`$contentType` are user-managed exceptions, but type IDs and allowed operations remain fixed. Tombstone ID 0 is a patch operation with an empty wire value, never a stored `Value` or an Eq literal. Reject create tombstones, nonempty tombstones, duplicate/unsorted protocol triples, and type substitution on system fields.

## Arkiv entity execution profile

Introduce a small `arkiv` module in `arkiv-node` containing the schema/adapter and deterministic execution rules. Recommended API concepts: `ArkivTypeId`, `SystemField`, `ArkivValue`, `ArkivEntityProjection`, `ArkivOperation::{Create,Patch,ExtendExpiry,TransferOwnership,Delete}`, and `ArkivExecutionContext { caller, block_height, chain_id }`. Exact naming can follow repo conventions.

Use one host-controlled entity namespace initially, retaining generic namespace operations only in the existing generic profile. Authenticate each entity mutation against its committed `$owner`; do not treat the namespace owner as every entity's owner. The internal producer supplies caller identity explicitly in canonical block inputs. That is deterministic authorization simulation, not signature verification. The host's privileged write path must be inaccessible to public arbitrary cell writes.

Create mints the key exactly as the SDK/node do: keccak(chain ID as 32-byte BE || ARKIV_ADDRESS || creator address || creator nonce as u64 BE || salt as u128 BE). Persist per-creator creation counters in the committed host-state map with a dedicated domain; account for rollback/exhaustion and never derive identity from record ordinal. Transfer does not change creator or creation counter. Never accept arbitrary-key recreation in this profile, including after delete/expiry. Pin the deployed Arkiv contract address/domain value from bindings in golden vectors rather than inventing one.

Preserve atomic batches: later operations observe earlier effects; any rejected operation rolls back entity cells, indexes, host nonce state and expiry bookkeeping for the whole batch. Implement owner-only patch/delete/transfer; transfer rejects zero/same owner. Readonly freezes content patches, not lifecycle operations. Permissionless extension permits nonowner extension only when flag set. No operation revives an expired entity.

Resolve expiry with checked `max(expiresAt, current+minLifetime)`, require target > current, allow MAX permanence, never shorten. Keep current genesis-pinned capacity/maximum-lifetime limits as explicitly advertised simulator resource constraints. Every published block must have completed required expiry cleanup; otherwise stop publication. Old pinned snapshots continue to show entities before expiry. If delayed purge is introduced later, every proof/query must prove active-set filtering and pagination completeness; it is not a drop-in garbage-collection optimization.

## Proofs, versions and durable compatibility

Do not change constants in place and reopen old stores. Define a new engine layout/codec profile, node protocol profile, proof profile and inspection schema. Keep the existing record-key encoding version distinct: 32-byte raw keys already fit its current 33-byte canonical form.

- `record.rs`, `codec.rs`, `layout.rs`, `typed.rs`: new values, canonical decoders, strict widths, order bytes, term-key limits and validation. Update charge accounting for larger names/values; field bytes must still count toward budgets.
- `proof.rs`: new request/proof/continuation version, bounded decoders and generic equality validation for every indexable type. Reconstruct complete postings before pagination, bind exact typed request and root, and validate returned system-field shapes/key agreement in the Arkiv verifier adapter.
- `durable/{mod,format,prepare,simulator}.rs`, storage versioned-store metadata, node genesis/block codecs: identify and gate layout/profile in durable manifests/genesis and canonical hash inputs. Audit actual durable-format ownership before choosing numeric version bumps; storage backend format and logical trie layout are different concerns.
- `identity.rs`, `select.rs`, `replay.rs`, simulator `light.rs`: reject profile mismatches before syncing; scope cursors to profile, identity, snapshot, namespace, exact type/value/query and page limit. Never accept an old request with a newly interpreted type byte.
- `simulator/{wire,runtime,inspection,metadata}.rs`: versioned JSON tagged values, Arkiv projection, capability discovery and actual proof paths. Keep raw canonical proof bytes available; inspection remains derivative of verified witness.
- Frontend `patriciaProof.ts` and `PatriciaProofVisualizer.tsx`: dispatch explicit profile/encoding version; label system typed cells, updated term-key widths and signed biases. Unsupported versions show a clear error, not a guessed graph.

An equality proof attests rows and completeness against a trusted root. It does not itself prove the entire historical execution/owner authorization. Full-node replay enforces those transitions; the unsigned light client still trusts the configured header source. Do not imply the new system fields add consensus security.

Rollout recommendation: retain old store/binary/proof vectors for read-only archival verification, provision a separate v2 run with new genesis and separate full/light stores, test it, then propose a controlled cutover. No automatic reset, no silent in-place conversion, and no rewriting historical headers. If migration is requested, export/import or replay creates a new identity/root history; missing old creator/flags/nonce facts must be reported, not fabricated. A live height-activated upgrade with dual decoders is a separate, considerably larger project.

## Delivery phases and file ownership

1. **Compatibility contract and fixtures (1–2 engineering days).** Freeze the above public matrix, source revisions, local-vs-SDK ID table and version inventory. Add canonical SDK-produced vectors to `crates/arkiv-db/tests/fixtures/arkiv/`; include current-node behavior conflicts as named fixtures. New Rust host module owns protocol semantics. No company SDK/spec changes.
2. **Generic scalar/layout support (3–5 days).** Implement `record.rs`, `codec.rs`, `layout.rs`, `typed.rs`, `query.rs` and durable/version gates; retain old fixtures. Add 32-byte names,128-byte indexed strings and distinct native types, ordered ranges internally. Complete codec/property/golden tests before wiring network endpoints.
3. **Entity system fields/lifecycle (4–7 days).** Implement Arkiv module and integrate `block.rs`, `validate.rs`, `exec.rs`, `params.rs`, committed nonce domains and `simulator/workload.rs`. Preserve existing generic profile behavior. Deliver host authority and atomic rollback tests; per-entity ownership is the highest-risk part.
4. **Proof/light interface (3–5 days, partly parallel with phase 3).** Extend `proof.rs`, node select/identity/replay and simulator wire/light/inspection. Support point reads and bounded Eq for all allowed indexed system/scalar types, exact projections and version/cursor rejection. Retain existing proof size/concurrency defenses; recompute caps for larger field values.
5. **Demonstration and integration (2–4 days).** Update indexer `frontend/src/{nodeDebugApi,NodeDebugView,patriciaProof,PatriciaProofVisualizer}` and associated tests. Update `src/simulator/` wire validators/projector only where new metadata contract requires it; PostgreSQL remains rebuildable explorer state, never proof authority. Never persist payload/raw proof bytes there. Add fixtures for owner, creator, expiry, created/updated blocks, content type, references, decimals and max-width integer values. Keep legacy explorer running during staging.
6. **Review and staged rollout (1–2 days).** Independent proof/version/authorization review, full targeted CI, crash/restart and live browser acceptance on isolated ports. Produce a cutover plan without modifying existing live data. A fresh deployment/reset is a later operational decision, not part of this planning request.

Estimate: roughly 14–25 engineering days of implementation/review effort; parallel execution can reduce elapsed time but not remove dependency and review gates. Full SDK RPC/ABI parity, generic query-language proofs, delayed purge, live historical upgrades, signatures and consensus are outside this estimate. Basic system types alone are smaller; credible lifecycle and proof compatibility is the larger task.

## Acceptance and adversarial verification

- **Name/value tests:** lengths0/20/21/32/33, ASCII case, colon/leading dollar/reserved words/`--`, Ident32 null padding; string127/128/129 UTF-8 bytes, multibyte and control characters. bool invalid bytes; each fixed type wrong width; i32 extremes; u64/u256 extremes; decimal signed extremes, ±1 unit,18/19 fractional digits, overflow and negative zero.
- **Property tests:** canonical encode/decode bijection and reject noncanonical input; signed and unsigned index ordering equals mathematical ordering; values with identical bytes but different types never collide; random create/patch/transfer/delete/expiry sequences preserve derived term/posting consistency.
- **Cross-SDK vectors:** ABI encode/decode, natural-width values, canonical decimal projection, address/key bytes and key derivation; known public system query renderings. Record unsupported builder fields as expected capability mismatches, not passing end-to-end SDK query tests.
- **Lifecycle tests:** immutable fields; reserved and unknown system writes; readonly/permissionless flags; owner transfer and old-owner rejection; no-op extension updatedAt policy; duplicate/noncanonical tombstones; references to missing/deleted entities; nonce rollback/exhaustion; multiple creates/patches in a batch; failure after successful earlier ops rolls everything back; no key recreation; exact expiry at E-1/E, permanent records and overflow.
- **Proof attacks:** mutate type ID/name/value/order bytes/root/namespace/key/system-cell kind; substitute addr for bytes32/key, i64 for i32; fake structural `$key` agreement; remove/duplicate posting members/rows, forged empty result, truncated proof, overlong allocations, unsupported version, mixed old/new payload, replay cursor against another profile/block/query/limit. No verified badge or projected entity before all checks succeed.
- **History and persistence:** producer/full independently agree on block outcomes and roots for the same fixture; restart each role with durable stores, inject failures around publication, verify header-only light state and historical queries; old stores fail closed under the new profile and remain readable by old tooling. Invalid expiry maintenance must prevent block publication.
- **UI/E2E:** sample `$owner` Eq changes after transfer while `$creator` stays constant; `$key` point and Eq agree; `$expiresAt`/`$createdAt`/`$updatedAt` show exact integers; `$contentType` Eq, absent matches, paging while head advances, max values and typed reference examples. Download witness and independently verify offline; byte tampering fails. Patricia tree shows actual authenticated paths and distinguishes posting completeness from one-row inclusion.
- **Resource checks:** charge and response-size bounds for the 32-byte name/128-byte indexed string profile, payload128KiB, worst-case escaped JSON,256-bit numbers, maximal posting/page caps; explicit bounded error for a valid request whose full proof exceeds the profile. Ensure payload and full proofs never enter explorer DB/logs. Keep tests deterministic/offline by default; browser/DB tests are explicit integration runs.

## Decisions that need user input

There is no blocking question for implementing and testing an isolated profile with these recommendations. Before live cutover, choose parallel fresh v2 deployment versus an explicitly funded versioned migration; current data must be preserved until that choice. Product approval is useful for whether the UI should offer `$updatedAt` equality as an advertised extension immediately, but the engine can safely implement the committed/indexed field and gate the public capability until decided.
