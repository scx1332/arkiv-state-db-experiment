# Changelog

## Unreleased

### Live permanent entities and rent outside gas

**Breaking:** block format version 5 (the genesis gains the rent clock,
`block_period_ms` and `max_rent_lifetime_seconds`, and the storage cap
`max_new_storage_units_per_block`, under digest domain `arkiv/genesis/v4`;
each transaction outcome gains `rent_wei`, under `arkiv/outcomes/v2`) and
protocol version 5. Earlier genesis files and stores are refused; formats and
protocols 3 and 4 were never released.

- Live creates and extensions accept `expiresAt = 2^64 − 1` (the SDK's
  `ExpirationTime.permanent()`). Such entities are never scheduled or swept;
  reads and `EntityCreated`/`ExpiryExtended` logs report `0xffffffffffffffff`.
  Status reports `permanentEntitiesSupported: true`, the `permanentEntities`
  rules, and no longer lists `permanent-entities` as unsupported.
- Rent rule `glm-rent-v3`: rent is GLM, not gas. It is 20 gwei per storage unit
  per day (the former rate at 2-second blocks), priced per second at the
  genesis block period, so a year costs the same at every period. It is a
  separate debit from the sender, burned like the fee, that counts against
  neither the transaction nor the block gas limit: a full 128 KiB entity made
  permanent executes in about 850,000 gas and pays about 0.97 GLM of rent in
  one transaction.
- Rent is never charged past `height + cap`. Create, growth and extension
  charge through `min(expiry, height + cap)`, recomputed from the current
  height, so every lifetime of at least the cap, permanent included, costs the
  same, and shorter ones are priced exactly. An extension pays only the growth
  of the capped horizon. Permanence never costs less than one cap per unit.
- The cap is a genesis consensus value in seconds (default 31,536,000, one year;
  at most 100 years), applied in blocks rounded up at the genesis block period
  (which a signed genesis requires to equal its timestamp period). Status
  advertises `rentRule`, `rentCharging`, `rentWeiPerStorageUnitDay`,
  `rentBlockPeriodMs`, `maxRentLifetimeSeconds` and `maxRentLifetimeBlocks`.
  Profiles set `max_rent_lifetime_seconds`; the preset is
  `arkiv-entity-v3/glm-v2@4`. A producer without a bundle uses a year.
- Rent is bounded only by the sender's balance: a wallet's `gas × price +
  value` does not cover it. An operation risks about 1 GLM at most; a signed
  allowance is planned for when the SDK can set one.
- An operation whose rent the sender cannot pay after its gas reservation
  fails with `ArkivFault::InsufficientRentBalance` (tag 17); its transaction
  rolls back, gas charged, no rent. Admission checks nonce, sizes, capacity,
  fee and value first, then quotes the transaction's rent (its operations in
  order, each seeing the earlier ones, with a 64-block height margin) and
  refuses a sender who cannot cover it with `insufficient funds for gas *
  price + value + rent`. `eth_estimateGas` returns execution gas only.
- The producer builds each block in one execution: a transaction whose gas
  reservation an earlier rent consumed is left out with its sender's later
  ones, and after each commit pending transactions their senders can no
  longer cover are dropped. A dropped transaction answers
  `eth_getTransactionByHash`/`eth_getTransactionReceipt` with `transaction
  dropped: ...` and native status `dropped` with the reason.
- `max_new_storage_units_per_block` caps the storage units a block's
  committed transactions add (default 1,500,000, the most the default input
  cap admits, so nothing changes by default). The producer defers what would
  cross it; a transaction that alone exceeds it is refused at admission;
  followers refuse a block over it. Status advertises
  `maxNewStorageUnitsPerBlock`.
- Receipts add `rentFee` (wei); native request status adds `rentChargedWei`,
  and the block feed's transactions `rentWei`.
- Only the owner extends an entity beyond one rent cap ahead: a
  permissionless-extension entity takes anyone's extension up to `height +
  cap`, and another account asking further, permanence included, is refused
  with `ArkivFault::PermanenceOwnerOnly` (tag 18). Extending a permanent
  entity to permanent is a no-op success; a finite expiry is
  `ExpiryShortened`. `PermanentStorageUnsupported` (fault 14) is no longer
  produced.
- Entity expiries are no longer bounded by `max_lifetime` (no `TooFar`);
  `max_lifetime` now bounds generic records only. Finite prefill rows burn the
  capped rent in wei; sponsored permanent rows are waived before any quote.
- The signed transaction cap is 384 KiB (the native envelope stays at
  256 KiB), so an Ethereum transaction, which carries its original bytes
  beside the envelope, fits a full 128 KiB entity.

### User-defined columns

**Breaking:** user-defined columns (UDC), the entity model of the JS SDK's
`udc/docs` branch. Block format version 2 and protocol version 2. Genesis
files, prefill streams, stores and SDK clients from earlier builds are
refused; start a fresh chain.

### Entity model
- An entity is its seven system cells plus named, typed user attributes. The
  payload and content type are gone: `$payload` and `$contentType` are no
  longer created, writable, removable, queryable or returned.
- Bit 7 (`0x80`) of an ABI attribute `typeId` marks the attribute indexed;
  the low seven bits keep the type IDs 1–10. Indexed attributes are stored
  as engine attributes (queryable), unindexed ones as fields (stored and
  returned only). `bytes` is a user type, never indexed (`0x87` is refused).
  Type 0 stays the patch-only tombstone and may not carry the flag.
- A patch stores each attribute with the flag of its new triple.
- Entity operations carry each attribute as an engine `Cell` (kind ‖ value),
  so the flag is signed, encoded in blocks and replayed. This layout change
  is block format version 2: transaction and block-input digests use the
  `arkiv/tx/v2` and `arkiv/inputs/v2` domains.
- Each `bytes` value is at most 128 KiB, and the `bytes` values of the
  resulting entity, after a create or patch, at most 128 KiB together
  (`ArkivFault::EntityTooLarge`, tag 16).
- At most 32 triples per create or patch. Signed genesis now requires
  `max_cells` between 39 and 64, so the system cells always leave room for 32
  user attributes; the default and preset profiles use 39.

### JSON-RPC
- `arkiv_query` entities match the SDK's `RpcEntity`: `attributeSchema`
  entries are `{name, type, indexed}`, and `attributes` include unindexed
  attributes, `bytes` values too. `select` takes the SDK's `RpcSelect` keys;
  `payload` and `contentType` are refused as unknown keys.
- Query refusals use the SDK's code table: -32001 parse (with `data`
  `{position, expected, got}`), -32002 type, -32003 literal, -32004 limits,
  -32005 cursor, -32006 block. `HistoryPruned` moves from -32001 to -32006 on
  every method, `ServerBusy` from -32005 to -32010, and the WebSocket
  subscription limit from -32005 to -32011.
- Predicates never match unindexed attributes.
- Verified equality pages list every attribute of each proven entity with its
  `indexed` flag; the entity profile is `arkiv-entity-v3`.

### Gas
- Storage units count every cell's name, value and two tag bytes, whatever its
  name; only indexed cells add the 128-unit posting reservation. The
  calibration in `docs/glm-gas-v1.md` is regenerated.

### Genesis
- Prefill rows use row codec 3 in both framings (`ARKPFL01 ‖ u32(3)` and
  `ARKPFL02 ‖ u32(3)`): each attribute is `CellName ‖ Cell`, with kind `01`
  indexed and `02` unindexed; no payload or content type; at most 32
  attributes and 128 KiB of `bytes` per row. See `docs/genesis-bundle-v1.md`.
- Authored entities give each attribute exactly one typed value and an
  explicit `indexed`; `bytes`, `bytes_text` and `bytes_file` spell bytes
  values. Dataset formats are `arkiv-entities-toml-v2` and
  `arkiv-entities-jsonl-v2`; the preset is `arkiv-entity-v3/glm-v2@2`.
- `arkiv-proposer create` takes `--data-file` (an unindexed `bytes`
  attribute `data`) instead of `--payload-file`.

## 0.1.0-alpha.2 — 2026-10-11

Alpha pre-release. **Breaking:** state layout 2. Stores, genesis files and
proofs from 0.1.0-alpha.1 are rejected at open, and there is no migration;
start from fresh stores.

### Engine
- Unpadded, prefix-free keys ([#2](https://github.com/Arkiv-Network/arkiv-state-db-experiment/pull/2), issue #1).
  - The trie accepts variable-length keys with a maximum length. No stored key
    may be a strict prefix of another, and such inserts are rejected.
  - Term keys are `name || type tag || presence || value`:
    - names and raw keys are escaped and end in `00`;
    - each string value byte is incremented by 1, with a `00` terminator;
    - scalars keep their fixed-width encodings.
  - Raw-record and record-key-binding keys are unpadded the same way.
  - Example sizes: `color = "red"` goes from 164 bytes to 12, and a `u64`
    value from 164 to 18.
- Complete range proofs take inclusive, exclusive and unbounded endpoints
  directly; the successor and predecessor arithmetic on padded keys is gone.
- The indexed-string limit is separate from the key encoding. The default
  and Arkiv limit stays at 128 bytes, and a generic instance can configure
  up to 512. Verifiers must use the instance's committed genesis limits.
- Metering is unchanged (model version 3).

### Performance (NVMe, against alpha.1)
- 1.04–1.26x higher throughput wherever the difference is significant, with
  the largest gains on index-heavy workloads (1.26x with 16 indexed
  attributes). Concurrent reads are 12–18% faster.
- Reachable state is 6.5–33% smaller and equality proofs are 2–5% smaller.
- Against golem-db 57c48ca, throughput is 1.2–3.4x higher; 12 of 13 results
  are significant.
- Details are in `docs/benchmarks/2026-10-11-prekucki-v1.md`.

### Validation
- 1,161 tests pass, including:
  - BTreeMap differential tests with variable-length keys;
  - adversarial range-bound and proof tests;
  - tests at the 512-byte ceiling.
- An independent adversarial review of the proof paths found nothing above
  low severity.

### CI
- GitHub Actions run workspace checks (fmt, clippy, tests) and the golem-db
  conformance kit on pushes to `main` and `experimental-*` branches and on
  pull requests.

## 0.1.0-alpha.1 — 2026-10-10

First tagged pre-release. It's an experimental database and node for Arkiv,
with no stability guarantees. The on-disk formats, wire formats and state
roots may change in any later release without migration, and there is no
consensus or finality.

### Engine
- An ordered, metered block engine on Merkle Patricia state, with typed
  records and attribute indexes inside each namespace.
- Queries with keyset pagination, pinned to one snapshot. Range queries work
  on every value type (numeric, bool, string, bytes).
- One profile: 32-byte cell names, 128-byte indexed values, all golem-db value
  types, and limits set at genesis. The only gas profile is `glm-v2`.
- A durable engine on redb (fsync on every commit), with a decoded-node cache
  and concurrent readers alongside the single block producer.
- State retention: `--state-retention archive|N`, archive by default.
  - Reads more than N snapshots below the head are refused with
    `HistoryPruned`: JSON-RPC `-32001`, REST `410 Gone`.
  - The floor is saved to disk and never moves back.
  - Nothing is physically deleted yet; see `docs/pruning-design.md`.

### Node
- `arkiv-proposer`: a signed single-proposer chain (producer, full and light
  followers) for the Arkiv v2 SDK entity model, with genesis bundles and
  replication.
- `arkiv-simulator`: an unsigned simulator node with its own stores.
- `arkiv-demo`: a small demo of a producer with full and light followers.
- Every binary accepts `--version`.

### golem-db conformance
- An adapter and kit for golem-db-conformance-testing, in `conformance/golem-db`.
  On golem's ported test suite, 16 tests pass and 5 fail; all 5 failures
  concern golem's internal system records.
- Benchmarks against golem-db 57c48ca on NVMe: arkiv is 1.4–2.7x faster in
  all 13 scenarios. See `docs/benchmarks/` and `docs/benchmarking.md`.

### Assets
The release includes the Linux x86_64 (glibc) binaries `arkiv-proposer`,
`arkiv-simulator` and `arkiv-demo`, with SHA-256 checksums.
