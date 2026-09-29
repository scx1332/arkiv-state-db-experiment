# Block format and canonical codec

## Status and scope

This document specifies every byte string the node hashes, stores or
compares between runs. It has two parts:

1. **The canonical codec, version 1** (node step N0b-2, implemented in
   `arkiv_db::codec`): the encoding rules, the layout and tag table of every
   engine type the node will put into blocks, receipts and state, and the
   domain-separated digest. Golden vectors are in
   [`crates/arkiv-db/tests/codec.rs`](../crates/arkiv-db/tests/codec.rs).
2. **Block format version 1 and block execution** (node step N0b-3,
   implemented in the `arkiv-node` library): genesis, headers, block
   inputs, transaction envelopes and outcomes built from the codec in
   part 1, their hashes, the genesis invariants, the successor rule, and
   the execution, selection, request-identity and replay rules. Golden
   vectors are in
   [`crates/arkiv-node/tests/block_format.rs`](../crates/arkiv-node/tests/block_format.rs);
   see [part 2](#part-2-block-format-version-1).

The codec is not an interchange format for clients. JSON is transport only
([node plan](node-storage-plan.md#block-and-process-contract)); nothing
derived from Rust `Debug` output, JSON, physical storage ids, snapshot ids,
cache state or process tokens is ever encoded into a hashed byte string.

## Part 1: canonical codec, version 1

### Rules

| Item | Encoding |
| --- | --- |
| `u8` | one byte |
| `u32`, `u64` | 4 or 8 bytes, big-endian |
| `i64` | 8 bytes, big-endian two's complement (`i64::to_be_bytes`) |
| `bool` | `00` (false) or `01` (true); any other byte is rejected |
| bytes, text | `u32 length ‖ bytes`; text must be UTF-8 |
| `Option<T>` | `00` for `None`, `01 ‖ T` for `Some`; any other first byte is rejected |
| `Vec<T>` | `u32 count ‖ item₁ ‖ … ‖ itemₙ` |
| pair `(A, B)` | `A ‖ B` |
| fixed-width array (`[u8; N]`, `B256`) | the `N` raw bytes, no length |
| enum | `u8 tag ‖ fields in declaration order`; tags are listed below |

There is no padding, no alignment and no self-description: a reader must
know the type it decodes. A value is decoded with `decode_exact`, which
consumes the whole input and rejects trailing bytes. Every accepted input
re-encodes to exactly itself (`encode(decode(x)) == x`), so equal values
have equal bytes and each byte string has at most one meaning.

Hostile counts cannot make a decoder allocate much more than its input.
Every type declares a lower bound on its encoded length
(`Decode::MIN_ENCODED_LEN`, at least one byte), and a count whose items
cannot fit in the remaining input is rejected as `Truncated` before
anything is allocated: a count of four billion followed by a few bytes
fails at once. The same check precedes every byte-string copy. A count
that does fit can still name items that are larger in memory than their
encoding (a `WriteOp` can be 6 bytes encoded but is several times that in
memory), so a sequence decoder reserves at most as many bytes up front as
the remaining input holds and grows the vector only as items actually
decode. A malformed input therefore fails with a `DecodeError` after
reserving at most its own length per level of sequence nesting; beyond
that, memory grows only with the items actually decoded (which may take
several times their encoded size).

Lengths and counts are `u32`. Encoding a byte string or sequence longer
than `u32::MAX` panics; no value the engine admits comes near that, and the
node's request, transaction and block byte limits (N0b-3, N1) are far
smaller.

### What decoders enforce

Decoders enforce every invariant of the decoded **type**, so a decoded
value is always one the Rust constructors could have built:

- name grammar and length for `NamespaceName` and `CellName` (1 to 20
  bytes, ASCII, letter or `$`-plus-letter first, then letters, digits,
  `_`, `-`, `.`, `:`);
- record-key length 1 to 32;
- nonzero `NamespaceId` and `RecordId`;
- type ids 1 to 5, every enum tag, `bool` and `Option` bytes;
- UTF-8 text;
- a `PriceSchedule` class count equal to `OperationClass::ALL.len()` (31
  under counting-rule version 3);
- the version fields of the block format (part 2): a format, protocol or
  header version this build does not support is `UnsupportedVersion`.

Decoders do **not** apply the engine's admission rules: value and cell
limits (`Limits`), the 64-byte indexed-value cap, attribute kinds (a
`Bytes` attribute), duplicate or overlapping cell names, empty patches and
the 20-byte raw-key cap. Those depend on instance limits or produce
deterministic rejections with receipts; an operation that violates them
decodes and is then rejected by the engine at admission, or earlier by the
host's static validation (N0b-3). `codec.rs` tests this boundary.

### Versioning and compatibility

- `CODEC_VERSION = 1` covers the rules, every layout and every tag table in
  this part. Codec bytes do not carry the version themselves: the container
  that stores or hashes them does (the block format version of part 2, and
  the store metadata of node step N4), and a reader refuses a version it
  does not know rather than guessing.
- Unknown tags are rejected (`UnknownTag`), never skipped or mapped to a
  default.
- A new enum variant gets a new tag; a tag is never reused, even after its
  variant is removed. Adding a variant is a compatible extension for
  writers that never emit it; readers of an older build reject it.
- Any change to an existing layout, a field order, a width or a rule needs
  a new codec version. Old bytes are migrated explicitly, never
  reinterpreted.
- A `PriceSchedule` encodes one weight per operation class. Counting-rule
  version 3 has 31 classes. When a later counting-rule version appends
  classes, the count changes and schedules encoded under the old model are
  rejected (`ClassCountMismatch`), never padded with default weights; a
  migration must choose the new weights explicitly. A version-2 schedule
  (27 classes) is rejected today.
- `RecordKey` has two canonical forms for two purposes: the codec's
  variable-width `u8 len ‖ bytes` used inside encoded values, and the fixed
  33-byte key encoding version 2 (`len ‖ bytes ‖ zero padding`,
  `RecordKey::encoded`) used where keys must sort as fixed-width map keys
  (N3b). They differ in length for every key shorter than 32 bytes; for a
  32-byte key they are the same 33 bytes and read back as the same key, so
  neither form is ever misread as the other.
- A receipt's debug ledger is never encoded: receipts that differ only in
  their ledgers have equal bytes, and a decoded receipt has `ledger: None`.

### Type layouts

| Type | Layout | Bytes |
| --- | --- | --- |
| `NamespaceId` | ordinal `u64`; zero rejected | 8 |
| `OwnerId` | 20 raw bytes | 20 |
| `Revision` | `u64` | 8 |
| `Cost` | units `u64` | 8 |
| `RecordId` | ordinal `u64`; zero rejected | 8 |
| `NamespaceName` | text (validated name) | 5–24 |
| `CellName` | text (validated name) | 5–24 |
| `NamespaceDescriptor` | `id ‖ name ‖ owner ‖ revision` | 41–60 |
| `RecordKey` | `u8 len ‖ bytes`, len 1–32 | 2–33 |
| `TypeTag` | type id `u8` (table below) | 1 |
| `Value` | `type id ‖ payload`: `Bool` one `bool` byte; `I64` 8 bytes two's complement (not the sign-flipped term form); `U64` 8 bytes; `Str` text; `Bytes` bytes | 2+ |
| `CellKind` | `u8` (table below) | 1 |
| `Cell` | `kind ‖ value` | 3+ |
| `WriteOp` | tag ‖ fields (table below) | |
| `Effect` | tag ‖ fields (table below) | 2–9 |
| `ArgumentFault`, `QueryFault` | tag | 1 |
| `Rejection` | tag ‖ fields (table below) | 1–17 |
| `Outcome` | `01` applied, or `02 ‖ Rejection` | 1–18 |
| `ReceiptKind` | tag | 1 |
| `Receipt` | `kind ‖ model_version u32 ‖ schedule_id u32 ‖ spent u64 ‖ outcome` | 18–35 |
| `PriceSchedule` | `id u32 ‖ u32 class count (31) ‖ 31 × u64 weight` in `OperationClass::ALL` order | 256 |
| `Limits` | `namespace_cap ‖ max_cells ‖ max_value_len ‖ max_page ‖ max_groups ‖ max_literals`, each `u64` | 48 |

`Limits` fields that are `usize` in Rust are encoded as `u64`; a value
that does not fit the platform's `usize` is rejected (`OutOfRange`).
`B256` hashes and `[u8; N]` identifiers (such as the 32-byte request ids
of part 2) are their raw bytes. Cell lists (`Vec<(CellName, Cell)>`) and
name lists (`Vec<CellName>`) keep the order they were given in; the engine,
not the codec, rejects duplicates.

### Tag tables

Tags start at 1 so that a zeroed byte is never a valid tag. Every table is
also a list of constants in `crates/arkiv-db/src/codec.rs`; none is derived
from a Rust discriminant.

`TypeTag` and the `Value` tag (the value's type id, the same ids the term
encoding uses):

| Tag | Type | Payload |
| --- | --- | --- |
| 1 | `Bool` | `00`/`01` |
| 2 | `I64` | 8 bytes, two's complement |
| 3 | `U64` | 8 bytes |
| 4 | `Str` | `u32 len ‖ UTF-8` |
| 5 | `Bytes` | `u32 len ‖ bytes` |

`CellKind`: 1 `Attribute`, 2 `Field`.

`WriteOp`:

| Tag | Variant | Fields |
| --- | --- | --- |
| 1 | `CreateNamespace` | `name: NamespaceName` |
| 2 | `TransferNamespace` | `namespace ‖ expected_revision ‖ new_owner` |
| 3 | `Put` | `namespace ‖ key: bytes ‖ value: bytes` |
| 4 | `Delete` | `namespace ‖ key: bytes` |
| 5 | `CreateRecord` | `namespace ‖ key: RecordKey ‖ cells: Vec<(CellName, Cell)>` |
| 6 | `PatchRecord` | `namespace ‖ key ‖ set: Vec<(CellName, Cell)> ‖ remove: Vec<CellName>` |
| 7 | `DeleteRecord` | `namespace ‖ key` |

`Effect`:

| Tag | Variant | Fields |
| --- | --- | --- |
| 1 | `NamespaceCreated` | `NamespaceId` |
| 2 | `Transferred` | `revision` |
| 3 | `Put` | `replaced: bool` |
| 4 | `Deleted` | `existed: bool` |
| 5 | `RecordCreated` | `id: RecordId` |
| 6 | `RecordPatched` | `id` |
| 7 | `RecordDeleted` | `id` |

`ArgumentFault`: 1 `KeyTooLong`, 2 `TooManyCells`, 3 `DuplicateCell`,
4 `SetAndRemoveOverlap`, 5 `ValueTooLarge`, 6 `BytesNotIndexable`,
7 `EmptyPatch`, 8 `NotHostOperation`.

`QueryFault`: 1 `NoGroups`, 2 `TooManyGroups`, 3 `TooManyLiterals`,
4 `ZeroLimit`, 5 `LimitTooLarge`, 6 `NotIndexable`, 7 `ValueTooLarge`,
8 `UnboundedRange`, 9 `MixedBounds`, 10 `NotOrdered`,
11 `ProjectionTooLarge`.

`Rejection`:

| Tag | Variant | Fields |
| --- | --- | --- |
| 1 | `UnknownNamespace` | |
| 2 | `Unauthorized` | |
| 3 | `StaleRevision` | `expected: Revision ‖ current: Revision` |
| 4 | `ExceedsBlockBudget` | `remaining: Cost` |
| 5 | `OutOfBudget` | |
| 6 | `CostOverflow` | |
| 7 | `DirectoryFull` | |
| 8 | `CounterExhausted` | |
| 9 | `AlreadyExists` | |
| 10 | `NotFound` | |
| 11 | `RecordFull` | |
| 12 | `InvalidArgument` | `ArgumentFault` |
| 13 | `InvalidQuery` | `QueryFault` |
| 14 | `CursorMismatch` | |

`Outcome`: 1 `Applied`, 2 `Rejected` followed by the `Rejection`.
`ReceiptKind`: 1 `Write`, 2 `Read`.

`PriceSchedule` weight order (`OperationClass::ALL`, append-only per
counting-rule version): 0 `DirectoryLookup`, 1 `NamespaceCreate`,
2 `NamespaceTransfer`, 3 `PutRecord`, 4 `DeleteRecord`, 5 `WriteKeyByte`,
6 `WriteValueByte`, 7 `PointRead`, 8 `ScanSeek`, 9 `ScanEntry`,
10 `ReadByte`, 11 `RecordCreate`, 12 `RecordPatch`, 13 `RecordDelete`,
14 `CellWrite`, 15 `CellByte`, 16 `CellRemove`, 17 `IndexInsert`,
18 `IndexTermCreate`, 19 `IndexRemove`, 20 `QueryGroup`, 21 `TermSeek`,
22 `TermVisit`, 23 `PostingEntry`, 24 `RowVisit`, 25 `RecordProbe`,
26 `ResultRow`, 27 `RootUpdate`, 28 `RowByte`, 29 `ProofNode`,
30 `ProofByte`.

### Worked vectors

Hex, with `‖` between fields. Each is one of the golden vectors in
`codec.rs`, where it is assembled from these pieces.

- `NamespaceId` 258: `0000000000000102`.
- `Value::I64(-2)`: `02 ‖ fffffffffffffffe`; `Value::Str("hé")`:
  `04 ‖ 00000003 ‖ 68c3a9`.
- `RecordKey` `"a"`: `01 ‖ 61`.
- `WriteOp::CreateRecord { namespace 1, key "a", cells [("price",
  attribute U64 7), ("note", field Str "hi")] }`:
  `05 ‖ 0000000000000001 ‖ 01 61 ‖ 00000002 ‖ 00000005 7072696365 ‖ 01 03
  0000000000000007 ‖ 00000004 6e6f7465 ‖ 02 04 00000002 6869`.
- An applied write receipt of 379 units under schedule 1, model 3:
  `01 ‖ 00000003 ‖ 00000001 ‖ 000000000000017b ‖ 01`.
- `Rejection::StaleRevision { expected 1, current 2 }`:
  `03 ‖ 0000000000000001 ‖ 0000000000000002`.
- `PriceSchedule::FIXTURE`: `00000001 ‖ 0000001f ‖ 0000000000000001 ‖
  0000000000000032 ‖ … ‖ 0000000000000001` (256 bytes).
- `Limits::DEFAULT`: `0000000100000000 ‖ 0000000000000040 ‖
  0000000000001000 ‖ 00000000000000c8 ‖ 0000000000000008 ‖
  0000000000000008`.

### Digest

```text
digest(domain, bytes) = keccak256(u8 len(domain) ‖ domain ‖ bytes)
```

`keccak256` is the Ethereum Keccak-256 from `alloy-primitives`. The length
prefix separates domains that share a concatenation (`digest("a", "bc")`
≠ `digest("ab", "c")`). A domain is 1 to 255 bytes of printable ASCII
(`0x21..=0x7e`); `digest` panics on any other domain, because domains are
constants and a bad one is a programming error.

Domain naming convention: `arkiv/<object>/v<layout version>`, lowercase,
with the version of the hashed object's layout (not the codec version), so
a new layout of the same object gets a new domain. The domains of the
[node plan](node-storage-plan.md) (`arkiv/state/v1` is the engine's state
root since node step N3b; see `docs/architecture.md`):

| Domain | Hashes | Step |
| --- | --- | --- |
| `arkiv/header/v1` | encoded version-1 header: the block hash | N0b-3 (in use) |
| `arkiv/header/v2` | encoded version-2 header (with the state root) | layout in N0b-3, used from N3b |
| `arkiv/inputs/v1` | encoded block inputs | N0b-3 (in use) |
| `arkiv/outcomes/v1` | encoded block outcomes | N0b-3 (in use) |
| `arkiv/tx/v1` | encoded transaction envelope | N0b-3 (in use) |
| `arkiv/admin/v1` | encoded admin input | N0b-3 (in use) |
| `arkiv/genesis/v1` | encoded genesis | N0b-3 (in use) |
| `arkiv/state/v1` | encoded state header: the state root | N3b (reserved) |

Only canonical codec bytes are hashed.

### Evidence

`cargo test -p arkiv-db --test codec --offline` runs the golden vectors:
for every covered type the expected bytes are written in the test from the
pieces above (never produced by the codec), decode back to the value,
re-encode identically and are rejected with one trailing byte and at every
truncation; the rejection cases above; huge counts; every 1-byte input for
12 types, every 2-byte input for 9 types and 1,000 seeded mutations of each
of 17 seed vectors (whatever decodes re-encodes to itself, and the number of
accepted short inputs matches the tag tables); and digests against
`keccak256` over hand-assembled preimages. A unit test in `codec.rs` checks
that a sequence decoder reserves no more bytes than its remaining input.

## Part 2: block format version 1

### Status

Implemented in the `arkiv-node` library (node step N0b-3): `block` (types,
encodings, hashes), `params` (genesis invariants, reservation, sweep
bound, `ChainSpec`), `validate` (static validation, successor rule),
`select` (selection and its replay check), `identity` (request identities,
pending and inclusion indexes), `exec` (`execute_block`) and `replay`
(`MemoryChain`, `replay`). The [full/light demo](full-light-demo.md) adds
the volatile process/RPC, protocol-2 root-bearing genesis and terminal
headers, and pinned signed envelopes. Protocol-1 fixtures and replay remain
unchanged. Durable node storage (N4) remains open.

### Versions and compatibility

- **Block format version** `FORMAT_VERSION = 1` covers every layout and
  tag table of this part. The genesis records it; a genesis with another
  format version is refused (`UnsupportedVersion { kind: "block format" }`
  when decoding, `GenesisError::UnsupportedFormat` from `ChainSpec::new`).
- **Protocol version** selects the header version and the execution
  rules. This build executes legacy protocol 1 (header version 1, no
  state root) and protocol 2 (header version 2 with terminal state root).
  Other protocols are refused (`UnsupportedVersion { kind: "protocol" }`,
  `GenesisError::UnsupportedProtocol`). The demo creates only protocol-2
  genesis; protocol-1 fixtures and replay stay byte-identical.
- **Header version** is not a free field: a header is version 2 exactly
  when it carries a state root. The header decoder knows both layouts and
  refuses any other version (`UnsupportedVersion { kind: "header" }`);
  the successor rule requires the chain's protocol's header version, so a
  protocol-1 chain refuses a version-2 header and vice versa.
- Tags start at 1 (except the two request scopes, fixed at `00`/`01`), are
  never reused and never derived from Rust discriminants; unknown tags are
  refused. A new variant gets a new tag. Any change to an existing layout
  needs a new format version and, for hashed objects, a new digest domain;
  old bytes are migrated explicitly, never reinterpreted.
- `DecodeError::UnsupportedVersion` was added to the codec for these
  version fields. It is an error variant, not a layout change: codec
  version 1 is unchanged.

### Layouts

Every field is encoded with the rules of part 1, in the order listed. `B256`
is 32 raw bytes, `OwnerId` 20, `RequestId` (a request id) 32.

| Type | Layout | Bytes |
| --- | --- | --- |
| `ChainParams` | `block_budget u64 ‖ user_budget u64 ‖ expiry_per_block u32 ‖ max_admin_per_block u32 ‖ max_txs_per_block u32 ‖ max_ops_per_tx u32 ‖ max_ops_per_block u32 ‖ max_tx_bytes u32 ‖ max_block_input_bytes u32 ‖ max_lifetime u64` | 52 |
| `Genesis` | `format_version u32 ‖ protocol_version u32 ‖ chain_id u64 ‖ timestamp_ms u64 ‖ limits: Limits ‖ pricing: PriceSchedule ‖ params: ChainParams ‖ admins: Vec<OwnerId>` | 384 + 20 per admin |
| `Header` (version 1) | `version u32 (1) ‖ chain_id u64 ‖ height u64 ‖ parent_hash ‖ timestamp_ms u64 ‖ inputs_digest ‖ outcomes_digest` | 124 |
| `Header` (version 2) | the version-1 fields with `version` 2, then `‖ state_root` | 156 |
| `BlockInputs` | `admin: Vec<AdminInput> ‖ txs: Vec<TxEnvelope>` | 8 when empty |
| `AdminInput` | `actor ‖ request_id ‖ action: AdminAction` | 309 |
| `AdminAction` | tag ‖ fields (table below) | 257 |
| `TxEnvelope` | `chain_id u64 ‖ actor ‖ request_id ‖ tx_budget u64 ‖ ops: Vec<TxOp>` | 72 + operations |
| `TxOp` | tag ‖ fields (table below) | 6+ |
| `Expiry` | `expires_at u64 ‖ min_lifetime u64` | 16 |
| `BlockOutcomes` | `admin: Vec<AdminOutcome> ‖ expiry: Vec<ExpiryDeletion> ‖ txs: Vec<TxOutcome> ‖ spent u64` | 20 when empty |
| `AdminOutcome` | `01`, or `02 ‖ AdminRejection` | 1–6 |
| `AdminRejection` | tag, then `current u32` for `StaleSchedule` | 1–5 |
| `ExpiryDeletion` | `namespace ‖ record_id ‖ receipt` | 34–51 |
| `TxOutcome` | `status: TxStatus ‖ ops: Vec<OpOutcome> ‖ spent u64` | 13+ |
| `TxStatus` | `01`, or `02 ‖ op_index u32` | 1 or 5 |
| `OpOutcome` | tag ‖ fields (table below) | 1+ |
| `HostFault` | tag, then the `ExpiryFault` for `ExpiryInvalid` | 1–2 |
| `ExpiryFault` | tag | 1 |
| `Block` | `header ‖ inputs ‖ outcomes` | 152+ |
| `Scope` | `00` transaction, `01` admin | 1 |
| `RequestIdentity` | `scope ‖ chain_id u64 ‖ actor ‖ request_id` | 61 |
| `Inclusion` | `height u64 ‖ scope ‖ index u32 ‖ digest` | 45 |

Receipts inside outcomes are the part-1 `Receipt` encoding (no debug
ledger). `RequestIdentity` and `Inclusion` are not hashed; their encodings
exist for the durable inclusion records of step N4.

### Tag tables

`AdminAction`: 1 `SetPricing(PriceSchedule)`.

`TxOp` (the engine `WriteOp` tags and layouts, plus the expiry of record
creations and patches):

| Tag | Variant | Fields |
| --- | --- | --- |
| 1 | `CreateNamespace` | `name: NamespaceName` |
| 2 | `TransferNamespace` | `namespace ‖ expected_revision ‖ new_owner` |
| 3 | `Put` | `namespace ‖ key: bytes ‖ value: bytes` |
| 4 | `Delete` | `namespace ‖ key: bytes` |
| 5 | `CreateRecord` | `namespace ‖ key: RecordKey ‖ cells: Vec<(CellName, Cell)> ‖ expiry: Option<Expiry>` |
| 6 | `PatchRecord` | `namespace ‖ key ‖ set: Vec<(CellName, Cell)> ‖ remove: Vec<CellName> ‖ expiry: Option<Expiry>` |
| 7 | `DeleteRecord` | `namespace ‖ key` |

`AdminOutcome`: 1 `Applied`, 2 `Rejected` followed by the `AdminRejection`.
`AdminRejection`: 1 `ReservationViolated`, 2 `ReservationOverflow`,
3 `StaleSchedule { current: u32 }`, 4 `GasProfileLocked` (the opt-in GLM gas
profile cannot be entered or changed through admin repricing).

`TxStatus`: 1 `Committed`, 2 `Failed { op_index: u32 }`.

`OpOutcome`:

| Tag | Variant | Fields |
| --- | --- | --- |
| 1 | `Applied` | `effect: Effect ‖ receipt` |
| 2 | `RolledBack` | `effect ‖ receipt` |
| 3 | `Rejected` | `reason: Rejection ‖ receipt` |
| 4 | `AdmissionRejected` | `reason: Rejection` |
| 5 | `HostRejected` | `fault: HostFault` |
| 6 | `NotExecuted` | |

`HostFault`: 1 `ExpiryCapacityFull`, 2 `ExpiryInvalid` followed by the
`ExpiryFault`, 3 `ExpiryShortened`. `ExpiryFault`: 1 `Overflow`,
2 `DeadOnArrival`, 3 `TooFar`. `Scope`: `00` transaction, `01` admin.

### Hashes and the genesis block

| Hash | Preimage |
| --- | --- |
| block hash | `digest("arkiv/header/v<version>", encode(header))` |
| `inputs_digest` | `digest("arkiv/inputs/v1", encode(inputs))` |
| `outcomes_digest` | `digest("arkiv/outcomes/v1", encode(outcomes))` |
| transaction digest | `digest("arkiv/tx/v1", encode(envelope))` |
| admin input digest | `digest("arkiv/admin/v1", encode(admin_input))` |
| genesis digest | `digest("arkiv/genesis/v1", encode(genesis))` |

The genesis block is height 0 with parent hash `00…00`, the genesis
timestamp, the genesis digest as its `inputs_digest`, the digest of empty
outcomes (`00000000 ‖ 00000000 ‖ 00000000 ‖ 0000000000000000`) as its
`outcomes_digest`, and (protocol 1) no state root. The genesis hash, the
chain's identity, is that header's block hash; it commits to the whole
genesis. A block hash commits to the header, and through its digests to
the block's inputs and outcomes; the state root joins it in protocol 2.
Nothing else is hashed: no Debug output, JSON, snapshot ids or physical
state.

### Genesis invariants

`ChainSpec::new` accepts a genesis only if (all arithmetic checked; an
overflow fails the check):

- format version 1 and protocol version 1;
- `1 ≤ max_ops_per_tx ≤ max_ops_per_block`, `max_txs_per_block ≥ 1`,
  `expiry_per_block ≥ 1`, `user_budget ≥ 1`;
- `8 + max_admin_per_block × MAX_ADMIN_BYTES + max_tx_bytes ≤
  max_block_input_bytes`, with `MAX_ADMIN_BYTES = 20 (actor) + 32
  (request id) + 1 (tag) + 256 (schedule) = 309`, the encoded size of every
  admin input (there is one action, `SetPricing`, and a schedule's size is
  fixed). The 8 bytes are the two counts of `BlockInputs`;
- the expiry reservation under the initial schedule (below);
- `limits.max_cells ≥ 1`, `limits.max_page ≥ 1`, `limits.max_groups ≥ 1`
  and `limits.max_literals ≥ 1`. These four are this implementation's
  additions to the brief: with `max_cells = 0` no record can carry
  `$expiresAt` and the host's named-projection expiry read is refused;
  with `max_page = 0` the sweep's reads are refused; with `max_groups = 0`
  or `max_literals = 0` the sweep's `Eq` query (one group of one literal)
  is refused. Each would halt the chain at its first due height.

**Expiry reservation**: `block_budget ≥ user_budget + expiry_per_block ×
max_expiry_delete_cost`, where `max_expiry_delete_cost` is the model-3
`DeleteRecord` charge of a record whose `max_cells` cells are all
attributes: `DirectoryLookup + PointRead + RecordDelete + c·CellRemove +
3·RootUpdate + 2a·(IndexRemove + 2·RootUpdate)` with `c = a = max_cells`.
The reservation also requires two read costs to be representable: the
sweep read bound (below) and the host's expiry pre-read (execution step
4), `DirectoryLookup + PointRead + ResultRow + 18·ReadByte` (a
`get_record` projecting only `$expiresAt`: 10 name bytes and an 8-byte
`U64`; the key is not projected). The pre-read runs with an unlimited
budget before the engine sees an operation, whoever its actor is, so a
schedule under which it overflowed would let any user halt the producer.
Under `PriceSchedule::FIXTURE` it costs 1 + 3 + 3 + 18 = 25. The
reservation is checked at genesis and for every `SetPricing`.

**Sweep read bound** (the budget of each read of the expiry sweep, from
the schedule in force): the larger of a `terms` read of one `U64` height,
`DirectoryLookup + TermSeek + TermVisit + 8·ReadByte`, and a full
`KEY_ONLY` `Eq` page, `DirectoryLookup + QueryGroup + TermSeek +
(max_page + 1)·PostingEntry + max_page·(ResultRow + 32·ReadByte)` (the
page's rows, the look-ahead entry, keys of at most 32 bytes). Under
`PriceSchedule::FIXTURE`, 8 cells and pages of 4: one deletion reserves
1 + 3 + 20 + 32 + 6 + 160 = 222 units and the read bound is
max(16, 155) = 155.

Why users never meet `ExceedsBlockBudget`: the sweep runs first and
deletes at most `expiry_per_block` records (the capacity check below), each
costing at most `max_expiry_delete_cost` under the parent's schedule,
which the reservation covered when it was committed. Selection keeps the
declared transaction budgets within `user_budget`, and an operation's
budget is what is left of its transaction's. So when an operation starts,
the block's remaining budget is at least its own budget.

### Successor rule

A block at `height` with `timestamp_ms` follows `parent` when: the
parent's chain id is the genesis chain id; the parent uses the protocol's
header version; `height = parent.height + 1` (checked; the last height has
no successor); and `timestamp_ms ≥ parent.timestamp_ms` (equal is
allowed). A recorded header must also carry the genesis chain id, the
protocol's header version and `parent_hash = parent`'s block hash. The
timestamp is chosen by the producer and recorded; execution never reads
it.

### Execution

`execute_block(spec, branch, parent, height, timestamp_ms, inputs)` runs a
block on a `Branch` begun on the parent's state with `Some(block_budget)`
and returns the sealed candidate, the outcomes and the header. Production
and replay use the same function; replay passes the recorded timestamp
and inputs and must reproduce the recorded outcome and header bytes. The
executor calls only the engine's fallible `try_` API
(`crates/arkiv-node/clippy.toml` bans the infallible forms and
`expire_records`), plus the lifecycle calls without one (`checkpoint`,
`rollback_to`, `set_pricing`, `seal`).

1. **Checks.** The successor rule, a fresh branch (block limit equal to
   `block_budget`, the genesis limits, no receipt, nothing spent), and
   static block validity (below). A failure is `ExecError::InvalidBlock`
   (or `Invariant(Branch)`); nothing runs.
2. **Admin inputs**, in order. `SetPricing(s)`: the reservation under `s`
   (`ReservationViolated`, or `ReservationOverflow` when the arithmetic,
   the sweep bound or the expiry pre-read cost overflows), then the engine's rule that a schedule id
   must exceed the one it replaces (`StaleSchedule { current }`); otherwise
   `Applied`: `s` is committed into the candidate and is in force from the
   next block, while this block keeps the parent's schedule. Rejected
   admin inputs change nothing. The brief lists only reservation
   violations as rejections; the stale-id rejection is recorded because
   the engine refuses such a schedule and the outcome must say so.
3. **Expiry sweep.** The due schedule lives in the host-state map:
   `'D' ‖ height (u64 BE) ‖ namespace (u64 BE) ‖ 15 zero bytes` → the
   number of records of that namespace due at that height, and `'T' ‖
   height ‖ 23 zero bytes` → the height's total, both `u64` big-endian,
   never stored as zero (the entry is removed), changed with checked
   arithmetic. For every due entry at or below the block height, in
   (height, namespace) order, the sweep calls
   `try_expire_records(branch, namespace, "$expiresAt", height,
   Budget::limit(sweep_read_bound), Budget::unlimited())` (read receipts
   discarded; deletions are write receipts charged to the block), checks
   that it deleted exactly the counted records, records an
   `ExpiryDeletion` per record in (height, namespace, record id) order and
   removes the entry; then each swept height's total must equal the sum of
   its counts and is removed, and no total may remain at or below the
   height. Namespaces without due entries are never visited. A mismatch,
   a malformed entry or a missing count is
   `ExecError::ScheduleInconsistent`; a sweep stopped by a rejection is
   `ExecError::ExpiryHalted`; both discard the candidate, and under a
   validated genesis neither is reachable.
4. **User transactions**, in order. Each runs under a checkpoint; an
   operation runs with `Budget::limit(tx_budget − spent so far)`.
   - *Expiry pre-step* (operations that carry an expiry, and every patch
     and record deletion): the record's current lifetime, read through the
     host's own `try_get_record` with a projection of `$expiresAt`
     (receipt discarded). No live record, or no such namespace, means
     none (the engine then rejects the operation itself); a live record
     without `$expiresAt`, or with `U64(u64::MAX)`, is permanent;
     `U64(E)` is height `E`; anything else is
     `Invariant(MalformedExpiryCell)`.
   - *Resolution* of an `Expiry { expires_at, min_lifetime }` at height
     `h`: `E = max(expires_at, h + min_lifetime)`, where an overflowing
     `h + min_lifetime` is `HostRejected(ExpiryInvalid(Overflow))`;
     `E = u64::MAX` is permanent (in particular `expires_at = u64::MAX`),
     stored as `U64(u64::MAX)`, exempt from `max_lifetime` and capacity,
     with no due entry; otherwise `E ≤ h` is `DeadOnArrival` and `E > h +
     max_lifetime` is `TooFar`. A patch may only extend: `E` earlier than
     the current lifetime (permanent is latest) is `ExpiryShortened`.
     Finally the capacity of a finite `E`: `total(E) + 1 − [current ==
     E] > expiry_per_block` is `ExpiryCapacityFull`. Host faults are
     decided before the engine sees the operation: no receipt, no charge.
   - *Engine operation*: `try_execute(actor, op, budget)`, where a resolved
     expiry is injected as `$expiresAt = Attribute(U64(E))` into the
     creation's cells or the patch's set; its charges fall on the user's
     operation and appear in its receipt. On success the due schedule
     follows the change: a deletion decrements the record's height; a
     creation or patch with an expiry that changes the lifetime decrements
     the old height (if finite) and increments the new one (if finite);
     an unchanged height changes nothing.
   - *Failure*: the first operation that is not applied (an engine
     rejection with its receipt, or a host fault) fails the transaction:
     its units are added, the checkpoint is restored (records, allocators,
     descriptors and the due schedule), earlier applied operations become
     `RolledBack` with their receipts, later ones `NotExecuted`, and the
     next transaction runs. An engine rejection that recorded no receipt
     is an admission rejection; for statically valid operations it cannot
     happen, and it is `ExecError::Invariant(AdmissionRejected)`, never an
     outcome. `OpOutcome::AdmissionRejected` exists in the format but is
     never produced by this executor.
5. **Seal.** `outcomes.spent` is the branch's spend and must equal the sum
   of the deletions' and transactions' spends. The header is version 1
   (protocol 1), with the parent's block hash and the two digests.

`ExecError` means no block: `Storage` (the storage beneath the state
failed; an availability error, never a rejection or an empty read),
`ScheduleInconsistent`, `ExpiryHalted`, `Invariant` and `InvalidBlock`.
The producer must stop with an observable error rather than publish.

### Static validation

Applied before a request is accepted, and by the executor to every block.
A transaction envelope is valid when: its chain id is the genesis chain
id; `1 ≤ tx_budget ≤ user_budget`; `1 ≤ |ops| ≤ max_ops_per_tx`; its
encoding is at most `max_tx_bytes`; and every operation, in order, names
no cell starting with `$` (in cells, a patch's set or its remove list),
carries a statically valid expiry (not `(0, 0)`, and `min_lifetime ≤
max_lifetime`), and passes the engine's argument validation
(`WriteOp::validate`: raw keys and prefixes of at most 20 bytes, cell and
patch counts, duplicates, set/remove overlap, value and indexed-value
sizes, `Bytes` attributes, empty patches) **as it will execute**: with a
placeholder `$expiresAt = U64(0)` injected when it carries an expiry. So
a record with an expiry has at most `max_cells − 1` user cells, and an
expiry-only patch is valid. Names of at most 20 characters and record keys
of 1 to 32 bytes are guaranteed by their types. An admin input is valid
when its actor is a genesis admin. A block's inputs are valid when every
admin input and transaction is, the selection limits hold, and no request
identity repeats; the executor also checks that the block follows its
parent, and the chain (`MemoryChain`, later the node) that no identity was
included by an earlier block.

User requests reach neither `execute_host`, `set_pricing` nor the host
map, and cannot write or remove `$` cells.

### Selection

The producer takes admin inputs first, in FIFO order, up to
`max_admin_per_block`; then user transactions in strict FIFO order while,
with the next transaction added, the transaction count is at most
`max_txs_per_block`, the operation count at most `max_ops_per_block`,
`len(encode(BlockInputs))` at most `max_block_input_bytes` and the sum of
declared budgets at most `user_budget`; it stops at the first transaction
that does not fit (no transaction overtakes another). Replay checks exactly
these limits on the recorded inputs (the byte limit on their actual
encoding). The genesis invariants make a transaction of the largest valid
shape fit a block that holds only a full admin section.

### Request identity

An identity is `(scope, chain_id, actor, request_id)`: scope `00` with the
envelope's chain id for transactions, scope `01` with the genesis chain id
for admin inputs; the content is identified by the request's digest. The
node keeps a pending index (accepted, in memory only) and an inclusion
index (identity → `(height, scope, index, digest)`). A submission is
answered under one lock: included with the same digest → `Included`;
included with another → `Conflict`; pending with the same digest →
`Pending`; pending with another → `Conflict`; unknown → `Accepted` (now
pending). An identity stays pending while its block is produced or its
commit is undecided, and moves to the inclusion index in the critical
section that publishes the block; a dropped request is forgotten. Only an
inclusion survives a restart. A block in which an identity repeats, or
which contains an included identity, is invalid.

### Replay

`MemoryChain` holds one memory engine, every block's header and terminal
snapshot, and the inclusion index. `produce(timestamp, inputs)` checks the
identities, begins a branch on the head with the block budget, executes,
publishes the terminal snapshot and records the inclusions. `apply(block)`
checks the recorded header against the head (successor rule and parent
hash), re-executes the recorded inputs at the recorded timestamp and
publishes only if the outcome bytes and the header bytes are identical.
`replay(genesis, blocks)` applies a sequence to a fresh chain and returns
the first divergence with its height: an invalid block (successor,
identities, static validity, selection), an execution error, different
outcomes, or a different header.

### Decisions recorded here

Choices the brief left open, taken as the simplest that meets it:

- The header version is derived from the presence of the state root, so
  the two cannot disagree.
- `SetPricing` with a schedule id that does not exceed the current one is
  `AdminOutcome::Rejected(StaleSchedule { current })`; the reservation
  check also rejects a schedule whose sweep bound or expiry pre-read cost
  overflows.
- Permanence is `E = u64::MAX`, which includes `expires_at = u64::MAX`.
- The expiry pre-step of a creation with an expiry reads the key's current
  lifetime too, so the capacity check of a creation that will fail with
  `AlreadyExists` uses the same `[current == E]` term as a patch.
- Host expiry checks come before the engine's own checks: a creation with
  an expiry at a full height in a namespace the actor does not own is
  `HostRejected(ExpiryCapacityFull)`, not `Rejected(Unauthorized)`.
- A sweep halt is its own error, `ExecError::ExpiryHalted`.
- `max_cells ≥ 1`, `max_page ≥ 1`, `max_groups ≥ 1` and `max_literals ≥
  1` are genesis invariants.

### Replay fixtures

[`crates/arkiv-node/tests/replay.rs`](../crates/arkiv-node/tests/replay.rs),
under `PriceSchedule::FIXTURE`, 8 cells, 64-byte values, pages of 4 and
`expiry_per_block = 4` unless stated. Receipts are derived by hand in the
test's comments.

| Fixture | What it establishes |
| --- | --- |
| `multi_operation_transaction` | One transaction of five operations (namespace, raw put, two record creations, one with an expiry, a patch) commits in one block; receipts 53, 24, 132, 208 and 136 units; the injected `$expiresAt` cell and its due entries |
| `failing_transaction_rolls_back` | An `AlreadyExists` in the middle: `RolledBack`, `Rejected` (4 units) and `NotExecuted`, 138 units kept; the record id is released and minted again; an exact budget (74) commits, one unit less stops at 64 with `OutOfBudget` |
| `same_block_ownership_transfer` | After a transfer the old owner is `Unauthorized` and the new owner writes in the same block; a stale revision is refused; request ids are scoped by actor |
| `expiry_at_the_boundary` | Records due at 3 (one through `min_lifetime`) are live at 2 and deleted in block 3, 78 units each in id order, before its transactions: a patch of one is `NotFound` (4 units) and a creation under the other's key commits as a new record (64 units, id 4); a record due at 4 survives block 3; the schedule empties |
| `expiry_at_capacity` | Four 8-cell, all-attribute records with 32-byte keys in three namespaces expire at one height under an exact reservation (block budget = user budget + 4 × 222); a fifth is `ExpiryCapacityFull`, a patch keeping its height is admitted; one-row sweep pages; the sweep spends 888 and a transaction with the whole user budget still runs; a schedule one unit dearer per deletion is `ReservationViolated` |
| `set_pricing_raises_read_weights` | Every read weight ×100 applies from the next block; expiry completes under it (474 units per deletion); a stale schedule id is refused |
| `permanent_expiry` | Permanent records store `U64(u64::MAX)` and schedule nothing; extending a finite record to permanent removes its entry; a record without `$expiresAt` cannot get a finite expiry |
| `expiry_extends_only` | An earlier height is `ExpiryShortened` (and rolls back its transaction); the same height is admitted without moving the schedule (64 units); a later height moves it (114 units) and the record expires there |
| `unresolvable_expiry_is_host_rejected` | `DeadOnArrival`, `Overflow` (with `max_lifetime = u64::MAX`), and on a second chain `TooFar` at the `max_lifetime` edge (103 refused, 102 committed for 243 units) |
| `empty_blocks` | Empty blocks have headers and empty outcomes; a timestamp may repeat the parent's |
| `replay_rejects_duplicate_identities` | Replay and production refuse an identity included earlier or repeated in a block; a transaction and an admin input may share actor and request id |
| `replay_rejects_invalid_blocks` | Replay refuses a `$` cell, budgets of 0 and above the user budget, a non-admin admin input, budgets summing past the user budget, a wrong parent hash, an earlier timestamp, altered outcomes and an altered header digest |
| `admission_rejection_is_an_invariant_error` | Under an unchecked genesis whose reservation is broken, `ExceedsBlockBudget` at admission is `ExecError::Invariant`; nothing is published |
| `halted_expiry_aborts_the_candidate` | Under an unchecked genesis, a sweep that runs out of block budget is `ExecError::ExpiryHalted`; nothing is published and the records stay live |

### Evidence

Run with `cargo test -p arkiv-node --offline`. Expected values are
hand-derived or come from an independent oracle; see the
[test strategy](testing.md#node-library-tests-node-step-n0b-3).

- `block_format.rs`: golden vectors for every type of this part (every
  `TxOp` and `OpOutcome` variant, both header versions, genesis, inputs,
  outcomes, blocks, request identities and inclusions), assembled by hand
  from the layouts above; each decodes back, re-encodes identically and is
  refused with a trailing byte and at every truncation. Every digest and
  the genesis hash are `keccak256` over hand-built preimages. Refused:
  unsupported format, protocol and header versions, a 30-class schedule,
  every unknown tag, a bad option byte, hostile counts, a zero record id
  and an invalid name nested in a block.
- `replay.rs`: the fixtures below, each checked against hand-derived
  outcomes, replayed on a fresh engine to identical bytes, and pinned by
  golden block hashes (which pin determinism, not correctness).
- `block_model.rs`: 4 seeds × 60 generated blocks predicted in full by an
  independent oracle.
- `exec.rs`, `genesis.rs`, `validation.rs`, `selection.rs`, `identity.rs`:
  the failure paths, invariants and rules above.

Not established: the `ExecError::Storage` path. The memory engine's `try_`
forms never fail, so no test can inject a storage error yet; it becomes
testable with the persistent engine (N3b/N3c).
