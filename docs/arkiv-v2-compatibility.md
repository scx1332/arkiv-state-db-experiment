# Arkiv entity compatibility profile

The node (`arkiv-node`) implements the Arkiv SDK's user-defined-columns (UDC)
entity model as its host protocol (`arkiv-entity-v3` in status capabilities,
protocol version 2). There is one build and one protocol: every chain reserves
namespace 1 for Arkiv entities, and the database engine (`arkiv-db`) has a
single layout. The earlier payload model (`arkiv-entity-v2`, with `$payload`
and `$contentType`) is retired; see the
[node API reference](node-api-reference.md#entities-user-defined-columns).

## Contract

| Area | Implemented behavior |
| --- | --- |
| Names | 32 ASCII bytes, protocol Ident32 grammar; SDK-safe authoring rejects reserved words and `--`; namespace names remain 20 bytes |
| Scalars | Distinct bool, i32, u64, u256, dec, bytes32, bytes, str, addr, key; each attribute indexed or not by the writer (ABI `typeId` bit 7), `bytes` never indexed; other engine types (i64, the golem-db grid types) rejected at the Arkiv boundary |
| Strings | 128 UTF-8 bytes, C0 and DEL rejected |
| Bytes | 128 KiB per value and 128 KiB for all `bytes` values of one entity together, separate block/proof/response limits |
| Attributes | At most 32 per create or patch; indexed ones are queryable, unindexed ones stored and returned only |
| Encodings | Explicit SDK IDs 1–10 with the `0x80` indexed flag; separate ABI, natural-width storage, canonical engine values and ordered index bytes |
| System fields | Committed `$key`, `$creator`, `$owner`, `$createdAt`, `$updatedAt`, `$expiresAt`, `$creationFlags`; no payload or content type |
| Identity | SDK-compatible Keccak derivation using chain ID, contract address, creator, committed creation nonce, salt |
| Lifecycle | Per-entity owner checks; immutable creator/key/createdAt/flags; readonly content; permissionless extension; transfer; delete; no revival; transaction rollback includes nonces and expiry schedules |
| Expiry | Checked minimum-lifetime resolution, no shortening, purge before publication; permanent MAX; equal-expiry successful extension stamps updatedAt |
| Queries | Proven: bounded single typed equality with complete postings (`eq-page-v1`) and complete ranges (`range-complete-v1`); `$key` equality gives point lookup. The signed node additionally serves unverified SDK JSON-RPC queries |
| Proofs | All indexable scalars, profile-bound requests and continuation tokens, real Patricia paths and full posting reconstruction, verified entity projection |
| Trust | Unsigned internal producer; light verifies against configured headers; full independently reexecutes; proof alone does not attest historical authorization |

The entity namespace is ordinal 1. Generic writes and namespace transfer are
rejected there. Other generic namespaces remain available. Public authoring cannot
set host-owned cells. Typed patch removal is separate from stored values;
protocol-triple decoding requires sorted unique canonical Ident32 names and empty
ID-zero tombstones, and rejects create tombstones. That adapter admits protocol
names, while the simulator's typed authoring API intentionally remains SDK-safe.

JSON integer query values are exact decimal strings, including i32. u256 values
are not JavaScript numbers. Decimal values are exact decimal strings, and bytes32,
addr and key use fixed-width `0x` hex. This experimental wire contract is not the
production SDK JSON-RPC representation. SDK ABI values and key/decimal fixtures
are tested independently from the experimental JSON transport.

`$updatedAt` equality is an advertised extension to the pinned production parser.
Current SDK builder exceptions for `$createdAt` and `$updatedAt` are reported in
capabilities rather than claimed as supported SDK builder calls. Unindexed
attribute values and raw witnesses occur only on proof and query endpoints.
Explorer feed rows keep unindexed fields as length/digest descriptors; their
values and proofs are never feed or database metadata.

## Version inventory

| Boundary | Value |
| --- | --- |
| Engine layout, canonical codec, state format | 1 |
| Name / indexed-value / term-key bytes (hard caps) | 32 / 128 / 164 |
| Proof profiles | `eq-page-v1`, `range-complete-v1` |
| Node block format / protocol | 1 / 1 (every header carries the state root) |
| Durable engine magic | ARKENG01 |
| Simulator run / light store | 1 / ARKSIMLIGHT1 |
| Record-key encoding | 33 bytes |

The cell limits within the hard caps are chain parameters (`Limits`, set in
the genesis): the entity profile sets 32-byte names, 128-byte indexed
strings and 128 KiB values. Engine manifests and state headers gate the
logical layout, and node genesis/header domains gate the chain. Scalar type
ids are golem-db's cell type ids (see `docs/block-format.md`); generic
namespaces admit all of them, and the Arkiv entity namespace admits only the
SDK scalars above. Status reports both lists: `capabilities.valueTypes`
(every engine type) and `capabilities.entityNamespace.valueTypes` (the SDK
ABI types).

## Isolated demonstration

```
python3 scripts/arkiv-v2-demo.py --run-dir /tmp/my-arkiv-v2
```

The script builds `arkiv-simulator` in the normal target directory, starts producer at 19400, private
replication at 19401, light at 19402 and full at 19403, all loopback-only. Use
`--base-port` to choose another four-port range, or `--binary` for an already built
simulator. Ctrl-C stops all three processes. Repeating the command resumes the
same identity and durable history. It never resets data or points at existing
production stores. Private bearer material stays in a mode-0600 file.

Deterministic history creates five entities at block 2. At block 4 the first
entity transfers from address `a1` repeated 20 bytes to `b0` repeated 20 bytes;
block 5 patches it; block 6 performs a permissionless equal-expiry extension on
the fifth entity; block 8 deletes the fourth; block 14 expires the fifth. Empty
blocks then continue advancing. The trace is finite; it does not model
production workload growth.

Example POST to `http://127.0.0.1:19402/sim/v1/query/inspect`:

```json
{"height":"5","namespace":"1","attribute":"$owner","valueType":"addr","value":"0xb0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0","limit":64,"cursor":null}
```

Other fixtures: group U64 1 at block 3 has five rows; createdAt U64 2 has five;
price Dec `-12.34567890123456789`, quantity U256 MAX, counter I32 MIN,
checksum Bytes32 `42` repeated 32 bytes and reference Key `77` repeated 32 bytes
are populated, all indexed; the unindexed `format` str (`application/json`) and
`data` bytes are stored and returned but have no postings. Expiry U64 14 has one
row at 13 and zero at 14. Each proof response includes the verified system
projection and every attribute with its `indexed` flag, canonical request/proof
bytes and versioned actual witness paths.

## Verification

```
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

SDK-derived fixtures record the source revision in
`crates/arkiv-db/tests/fixtures/arkiv/sdk-host.json`. Tests cover natural widths
versus ABI words versus signed ordering, complete equality witnesses for every
new scalar, profile/version rejection, owner/flags/key forgery against a real
authenticated engine proof, rollback, nonce exhaustion, expiry, independent full
replay, header-only restart, HTTP inspection, a 128 KiB unindexed bytes
attribute that is proven but kept out of the feed,
and bounded proof failure. Redb simulator crash tests cover every mutating boundary
of a five-entity creation block, including creator counters and expiry schedules.

## Deployment

Deploy the simulator image on fresh producer, full and light volumes,
bootstrap their source/run/genesis identity, and point the debug panels at
those peers. An explorer feed validator must understand the metadata types
and projection version before an explorer index is attached. Stores are not
migrated: a changed logical layout requires fresh volumes.
