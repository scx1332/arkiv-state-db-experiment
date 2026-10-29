# Sourcify v4 prefill conversion

`scripts/sourcify-v4-prefill.py` turns a v4 (UDC) Sourcify bundle, the output
of the processor's `process --format udc`
([sourcify-arkiv-entity-processor `docs/udc-proposal.md`](https://github.com/Arkiv-Network/sourcify-arkiv-entity-processor/blob/udc/docs/udc-proposal.md),
"Prefill converter"), into a streaming prefill of row codec 3
([genesis bundle contract](genesis-bundle-v1.md#versions-and-identity)). A v4
bundle has no payload, so there is nothing to reinterpret: each line becomes
exactly one row, in the same order.

```sh
python3 -I scripts/sourcify-v4-prefill.py convert --bundle /data/bundle-v4 --out /data/converted --workers 8
arkiv-proposer genesis stream-build /data/base/genesis.bin --prefill /data/converted/prefill.bin \
  --out /data/bundle --sponsor 0xPROPOSER --state-out /data/genesis-state
```

The base genesis must be a permanent-prefill one (`[prefill] allow_permanent = true`,
see [unlimited expiry](sourcify-prefill-converter.md#unlimited-expiry---expiry-policy-unlimited)):
`stream-build` then commits a version-4 launch with `burnedWei` 0. `inventory`
runs every check and publishes only the report.

## Rows

```text
prefill.bin = "ARKPFL02" || u32(3) || u64(rows) || rows x (u32(len) || row)
row         = creator[20] || nonce u64 || salt u128 || expiry u64 || flags u8
              || u32(count) || count x (u32(len) || name || kind || tag || payload)
```

| Field | Value |
| --- | --- |
| Creator | The bundle owner (`seed.json` `owner`), or `--owner` |
| Creation nonce | The line's `nonce`: 0 to N−1 in bundle order |
| Salt | 0 |
| Expiry | `u64::MAX`, the permanent sentinel (rent sponsored) |
| Flags | 0 |
| Cells | One per attribute, names strictly ascending by bytes; kind `01` indexed, `02` stored |
| Tags | `bool` 01, `str` 02, `bytes` 03, `addr` 04, `bytes32` 0b, `u64` 0d, `i32` 10 (sign bit flipped), `key` 40 |

The tags and layout are those of the node's reader (`arkiv-db` `TypeTag`, the
`Encode for Value`/`Cell` impls, `genesis_bundle::PrefillEntity`), not only of
this document; `scripts/test-sourcify-v4-prefill.py` pins them against the
contract's example cell and decodes every row independently.

## Checks

Nothing is published unless every line passes:

- **Bundle.** `SHA256SUMS` lists exactly the parts, `report.json` and `seed.json`;
  `seed.json` is version 2 `sourcify-arkiv.udc-bundle.v1` with `firstNonce` 0;
  `report.json` is a completed `sourcify-udc-import.v1` report of the same chain,
  owner and prefix, Ethereum mainnet, permanent, processed with a
  source-complete marker (`--allow-eof-source` accepts one without, for
  fixtures); every part's SHA-256, size, entity count and first nonce; at the end
  the report's entity, kind, contract and distinct counts, bytes values and
  storage units (recomputed with the node's formula).
- **Node row rules.** At most 32 attributes, SDK-safe names strictly ascending,
  only the format's SDK types, `str` at most 128 bytes without controls, `bytes`
  unindexed and at most 128 KiB per value and per row, canonical base64.
- **Format.** The 120,000-byte budget; `arkiv.schema` = `<prefix>.<kind>`; each
  kind's attributes, types, indexed flags and required set (`udc::ATTRIBUTES`);
  each line's key the salt-0 key of (chain, owner, nonce); permanence; a known
  codec byte on every bytes value; `sourcerefs` inline (codec 0); one contract
  per address and per `matchid`, one entity per kind and content hash; contracts
  of source chain 1.
- **References.** Every key a row names (`compilationref`, `creationcoderef`,
  `runtimecoderef`, each `sourcerefs` key, each chunk-list key) is an earlier row
  of the kind it must be.
- **Content.** Chunk lists (chunk count, joined length and SHA-256); a source's
  SHA-256 and a code's Keccak-256 over the decoded value; a compilation's hash
  (SHA-256 of the manifest), its canonical manifest, its group digests, the
  hashes of the sources behind `sourcerefs` and of the codes behind its code
  slots; a contract's `compilationhash`, code hashes (also of literal on-chain
  bytecode) and `abihash` (against the compilation's ABI digest when no
  reference points into the ABI).

`--chain-id` and `--owner` re-key a bundle for another chain or owner: every
key is recomputed from (chain, owner, nonce, 0) in the key attributes, the
`sourcerefs` and the chunk lists; content hashes and digests do not change.

Part files are checked in `--workers` processes; one ordered pass checks
references, cross-row content and totals and writes the stream. Its memory is a
key index of the referenceable rows (sources, codes, compilations and chunks,
about 200 bytes each), the chunk data (`--max-chunk-mib`) and the parts in
flight. Output is `prefill.bin`, `conversion-report.json` and `source-lock.json`
in a new directory published with an atomic no-replace rename.

## Validation

```sh
python3 -I scripts/test-sourcify-v4-prefill.py -v
ARKIV_PROPOSER_BIN=target/release/arkiv-proposer SOURCIFY_V4_BUNDLE=/path/to/bundle-v4 \
  python3 -I scripts/test-sourcify-v4-prefill.py -v
python3 -I scripts/sourcify-v4-node-check.py keys|counts|reads --rpc URL --bundle BUNDLE ...
```

The tests write an independent synthetic v4 bundle (every kind, chunked and
inline values, both code slots, a literal code hash, proxy attributes), convert
it, decode every row and compare it with its line; re-key it; and refuse 45
tampered copies, each with checksums and report totals rewritten so that the
mutation reaches the check it targets. With `ARKIV_PROPOSER_BIN` they run
`genesis stream-build` and `stream-verify` on the stream; with
`SOURCIFY_V4_BUNDLE` they convert a real bundle and compare every row.
`sourcify-v4-node-check.py` reads a node started from the prefill: every key of
the owner in creation order against the predicted keys, counts per kind, and
`read-arkiv --format udc` of contracts against the bundle's `hydrate` output.

Results on 2026-10-29 (arkiv-state-db-experiment `2aff63b` node, processor `udc`
`dec4261`):

- The SafeProxy record (`docs/data/hydrated-contract.json`, owner `0x11…11`)
  gives the processor's `docs/udc-examples/safeproxy` entities (6 keys and every
  attribute equal) and converts to 6 rows, 19,325 storage units.
- The processor's PostgreSQL fixture (`SEED_FORMAT=udc pipeline/run.sh run`):
  16 contracts, 216 rows, 230 references, one chunk list.
- The first 5,027 contracts of the 2026-10-23 seed: 29,340 rows, 44,176
  references, 179,302,796 storage units, converted in 1.4 s; `stream-build`
  accepted the stream (version 4, same stream hash and units). A producer
  started from it returned all 29,340 keys in creation order equal to the
  bundle's, every count per kind, all 5,027 contracts through `read-arkiv
  --format udc` byte-identical to the bundle's `hydrate` (whose output equals
  the v3 hydration of the same records), 420 of 420 server responses
  (`crates/server --format udc`, 20 contracts × 21 queries) identical to the v3
  path, and the window paging walk in both directions in `matchid` order.
