# Arkiv entity v2 compatibility profile

Implemented as an explicit `arkiv-v2` Cargo feature. The default build remains the
legacy generic profile. These binaries must use separate stores: there is no
in-place migration, no historical root rewrite, and no live height activation.

## Contract

| Area | Implemented behavior |
| --- | --- |
| Names | 32 ASCII bytes, protocol Ident32 grammar; SDK-safe authoring rejects reserved words and `--`; namespace names remain 20 bytes |
| Scalars | Distinct bool, i32, u64, u256, dec, bytes32, str, addr, key; payload-only bytes; generic i64 rejected at Arkiv boundary |
| Strings | 128 UTF-8 bytes, C0 and DEL rejected |
| Payload | 128 KiB per entity, separate block/proof/response limits |
| Encodings | Explicit SDK IDs 1–10; separate ABI, natural-width storage, canonical engine values and ordered index bytes |
| System fields | Committed `$key`, `$creator`, `$owner`, `$createdAt`, `$updatedAt`, `$expiresAt`, `$creationFlags`, `$contentType`, `$payload` |
| Identity | SDK-compatible Keccak derivation using chain ID, contract address, creator, committed creation nonce, salt |
| Lifecycle | Per-entity owner checks; immutable creator/key/createdAt/flags; readonly content; permissionless extension; transfer; delete; no revival; transaction rollback includes nonces and expiry schedules |
| Expiry | Checked minimum-lifetime resolution, no shortening, purge before publication; permanent MAX; equal-expiry successful extension stamps updatedAt |
| Queries | Bounded single typed equality with complete postings; `$key` equality gives point lookup; no full query language or RPC compatibility claim |
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
Current SDK builder exceptions for `$createdAt`, `$updatedAt` and `$contentType`
are reported in capabilities rather than claimed as supported SDK builder calls.
Payload and raw witnesses occur only on proof endpoints. Explorer feed rows keep
unindexed fields as length/digest descriptors; payloads and proofs are never feed
or database metadata.

## Version inventory

| Boundary | Default | Arkiv feature |
| --- | --- | --- |
| Engine layout, canonical codec, state format | 1 | 2 |
| Name/indexed-value/term-key bytes | 20 / 64 / 88 | 32 / 128 / 164 |
| Proof and inspection | eq-page-v1 / 1 | eq-page-v2 / 2 |
| Node block format / protocol / header | 1 / 2 / 2 | 2 / 3 / 3 |
| Durable engine magic | ARKENG01 | ARKENG02 |
| Simulator run / light store | 1 / ARKSIMLIGHT1 | 2 / ARKSIMLIGHT2 |
| Record-key encoding | 2 (33 bytes) | 2 (unchanged) |

The storage backend format itself is unchanged. Engine manifests and state
headers gate the logical layout, and node genesis/header domains gate the chain.
Legacy decoder rejects new scalar tags; legacy write admission rejects new types.
Retained legacy golden vectors are exercised by default builds. Shared database
tests run under both profiles; node v2 uses dedicated host, HTTP, full/light and
crash suites because the preserved generic/signed node fixtures describe another
protocol.

## Isolated demonstration

```
python3 scripts/arkiv-v2-demo.py --run-dir /tmp/my-arkiv-v2
```

The script builds into `target/arkiv-v2`, starts producer at 19400, private
replication at 19401, light at 19402 and full at 19403, all loopback-only. Use
`--base-port` to choose another four-port range, or `--binary` for an already built
v2 simulator. Ctrl-C stops all three processes. Repeating the command resumes the
same identity and durable history. It never resets data or points at existing
production stores. Private bearer material stays in a mode-0600 file.

Deterministic history creates five entities at block 2. At block 4 the first
entity transfers from address `a1` repeated 20 bytes to `b0` repeated 20 bytes;
block 5 patches it; block 6 performs a permissionless equal-expiry extension on
the fifth entity; block 8 deletes the fourth; block 14 expires the fifth. Empty
blocks then continue advancing. This finite trace intentionally rejects nonzero
`extraRowsPerBlock`; production workload growth is not implied.

Example POST to `http://127.0.0.1:19402/sim/v1/query/inspect`:

```json
{"height":"5","namespace":"1","attribute":"$owner","valueType":"addr","value":"0xb0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0","limit":64,"cursor":null}
```

Other fixtures: group U64 1 at block 3 has five rows; createdAt U64 2 has five;
price Dec `-12.34567890123456789`, quantity U256 MAX, counter I32 MIN,
checksum Bytes32 `42` repeated 32 bytes and reference Key `77` repeated 32 bytes
are populated. Content type is `application/json`. Expiry U64 14 has one row at
13 and zero at 14. Each proof response includes verified system projection,
canonical request/proof bytes and versioned actual witness paths.

## Verification

```
cargo test -p arkiv-db
cargo test -p arkiv-db --features arkiv-v2
cargo test -p arkiv-node
cargo test -p arkiv-node --features arkiv-v2
cargo clippy -p arkiv-db -p arkiv-node --all-targets -- -D warnings
cargo clippy -p arkiv-db -p arkiv-node --features arkiv-v2 --all-targets -- -D warnings
```

SDK-derived fixtures record the source revision in
`crates/arkiv-db/tests/fixtures/arkiv/sdk-host.json`. Tests cover natural widths
versus ABI words versus signed ordering, complete equality witnesses for every
new scalar, profile/version rejection, owner/flags/key forgery against a real
authenticated engine proof, rollback, nonce exhaustion, expiry, independent full
replay, header-only restart, HTTP inspection, 128 KiB payload, no feed payload,
and bounded proof failure. Redb simulator crash tests cover every mutating boundary
of a five-entity creation block, including creator counters and expiry schedules.

## Deployment

Build the simulator image with `--features arkiv-v2`, provision fresh producer,
full and light volumes, bootstrap their new source/run/genesis identity, and
point the debug panels at those peers. A compatible explorer feed validator must
understand the new metadata types and projection version before a fresh explorer
index is attached. Old volumes can remain offline for rollback with the old
binary. Never open them with a changed logical layout. The user has authorized
stopping the existing experimental chain and replacing it after validation;
operational deployment evidence belongs in the deployment report.
