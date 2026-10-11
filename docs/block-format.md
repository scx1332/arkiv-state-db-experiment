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

- name grammar and length for `NamespaceName` (1 to 20 bytes) and
  `CellName` (1 to 32 bytes, the term-key name width), ASCII, letter or `$`-plus-letter first, then letters, digits,
  `_`, `-`, `.`, `:`);
- record-key length 1 to 32;
- nonzero `NamespaceId` and `RecordId`;
- the 25 type ids (table below), every enum tag, `bool` and `Option`
  bytes; fixed-width payloads that are not a canonical term form (a bool
  byte above 1, the forms of NaN and negative zero);
- UTF-8 text;
- a `PriceSchedule` class count equal to `OperationClass::ALL.len()` (31
  under counting-rule version 3);
- the version fields of the block format (part 2): a format, protocol or
  header version other than 1 is `UnsupportedVersion`.

Decoders do **not** apply the engine's admission rules: value and cell
limits (`Limits`: name, `Str`, `Bytes` and indexed-string lengths),
attribute kinds (a `Bytes` attribute), duplicate or overlapping cell names
and the 20-byte raw-key cap. (An empty patch is not an admission fault: it
is golem-db's no-op.) Those depend on instance limits or produce
deterministic rejections with receipts; an operation that violates them
decodes and is then rejected by the engine at admission, or earlier by the
host's static validation (N0b-3). `codec.rs` tests this boundary.

### Versioning

- `CODEC_VERSION = 1` covers the rules, every layout and every tag table in
  this part. Codec bytes do not carry the version themselves: the container
  that stores or hashes them does (the block format version of part 2, and
  the store metadata of node step N4), and a reader refuses a version it
  does not know rather than guessing.
- Unknown tags are rejected (`UnknownTag`), never skipped or mapped to a
  default.
- A new enum variant gets a new tag; a tag is never reused.
- Any change to an existing layout, a field order, a width or a rule needs
  a new codec version; a reader never reinterprets bytes of another
  version.
- A `PriceSchedule` encodes one weight per operation class. Counting-rule
  version 3 has 31 classes; a schedule with another count is rejected
  (`ClassCountMismatch`), never padded with default weights.
- `RecordKey` has one canonical form, `u8 len ‖ bytes`, used inside encoded
  values and (since state layout 2) as the key-binding map's trie key; the
  length byte makes it prefix-free.
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
| `CellName` | text (validated name) | 5–36 |
| `NamespaceDescriptor` | `id ‖ name ‖ owner ‖ revision` | 41–60 |
| `RecordKey` | `u8 len ‖ bytes`, len 1–32 | 2–33 |
| `TypeTag` | type id `u8` (table below) | 1 |
| `Value` | `type id ‖ payload`: `Str` text; `Bytes` bytes; every other type its fixed-width term bytes (golem-db's stored form, table below) | 2+ |
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
| `Limits` | `namespace_cap ‖ max_cells ‖ max_cell_name_len ‖ max_str_len ‖ max_bytes_len ‖ max_indexed_len ‖ max_page ‖ max_groups ‖ max_literals`, each `u64` | 72 |

`Limits` fields that are `usize` in Rust are encoded as `u64`; a value
that does not fit the platform's `usize` is rejected (`OutOfRange`).

The four cell limits follow golem-db's genesis `cell_limits`:

| Field | Bounds | Default | Hard cap |
| --- | --- | --- | --- |
| `max_cell_name_len` | every cell name a write sets or removes (`$` counted) | 32 | 32 (name grammar) |
| `max_str_len` | every `Str` value, attribute or field | 4096 | none (codec `u32`) |
| `max_bytes_len` | every `Bytes` value (fields only) | 4096 | none (codec `u32`) |
| `max_indexed_len` | `Str` attributes and `Str` query values, range bounds, prefixes and `terms` bounds; effective bound `min(max_indexed_len, max_str_len)`, UTF-8 bytes | 128 (`Limits::DEFAULT_INDEXED_LEN`, the Arkiv profile) | 512 (`Value::INDEXED_LEN_CEILING`, representation/resource ceiling) |

Term keys store names and `Str` values unpadded (state layout 2,
`docs/experimental-prekucki-v1.md`), so the indexed-string limit is
admission policy, not a key width: the default and Arkiv-facing profile
stays at 128 bytes, and a generic instance may configure up to the
512-byte ceiling, which keeps a worst-case term leaf below the 1 KiB proof
node limit. `Limits::validate` rejects a configuration above a cap (and a
zero name limit),
the engine constructors and a durable genesis require it, and the node
checks it with the chain parameters. Fixed-width types have no
configurable limit. A durable database persists its limits in its genesis,
and opening it with any other limits is `Incompatible`.
`B256` hashes and `[u8; N]` identifiers (such as the 32-byte request ids
of part 2) are their raw bytes. Cell lists (`Vec<(CellName, Cell)>`) and
name lists (`Vec<CellName>`) keep the order they were given in; the engine,
not the codec, rejects duplicates.

### Tag tables

Tags start at 1 so that a zeroed byte is never a valid tag. Every table is
also a list of constants in `crates/arkiv-db/src/codec.rs`; none is derived
from a Rust discriminant.

`TypeTag` and the `Value` tag: the type id, which is golem-db's cell type
id (golem `CellType::id`). One id table serves both systems. `Key`, the
engine's only type without a golem counterpart, takes 64, outside golem's
7-bit table. Ids 5–7, 26, 27 and 30–63 are golem's reserved ids and are
rejected, as is every other unlisted id.

A fixed-width payload is the value's **term bytes**, the order-preserving
form that is also golem-db's stored value. So one byte form serves storage,
hashing and index order, and `Value::from_term_bytes` decodes it.

| Id | Type | golem type | Payload |
| --- | --- | --- | --- |
| 1 | `Bool` | `bool` | `00` / `01` |
| 2 | `Str` | `str` | `u32 len ‖ UTF-8` |
| 3 | `Bytes` | `bytes` | `u32 len ‖ bytes` |
| 4 | `Addr` | `bytes20` | 20 bytes |
| 8 | `Bytes4` | `bytes4` | 4 bytes |
| 9 | `Bytes8` | `bytes8` | 8 bytes |
| 10 | `Bytes16` | `bytes16` | 16 bytes |
| 11 | `Bytes32` | `bytes32` | 32 bytes |
| 12 | `U32` | `u32` | 4 bytes, big-endian |
| 13 | `U64` | `u64` | 8 bytes, big-endian |
| 14 | `U128` | `u128` | 16 bytes, big-endian |
| 15 | `U256` | `u256` | 32 bytes, big-endian |
| 16 | `I32` | `i32` | 4 bytes, two's complement, sign bit flipped |
| 17 | `I64` | `i64` | 8 bytes, likewise |
| 18 | `I128` | `i128` | 16 bytes, likewise |
| 19 | `I256` | `i256` | 32 bytes, likewise |
| 20 | `Dec32` | `dec32` | 4 bytes, units of 10^-4, sign bit flipped |
| 21 | `Dec64` | `dec64` | 8 bytes, units of 10^-6, likewise |
| 22 | `Dec128` | `dec128` | 16 bytes, units of 10^-18, likewise |
| 23 | `Dec` | `dec256` | 32 bytes, units of 10^-18, likewise |
| 24 | `F32` | `f32` | 4 bytes, IEEE total-order form |
| 25 | `F64` | `f64` | 8 bytes, IEEE total-order form |
| 28 | `Date32` | `date32` | 4 bytes, days since the Unix epoch, sign bit flipped |
| 29 | `Timestamp64` | `timestamp64` | 8 bytes, microseconds since the Unix epoch, sign bit flipped |
| 64 | `Key` | (none; reads as `bytes32`) | 32 bytes |

The IEEE total-order form flips the sign bit of a non-negative float and
every bit of a negative one. Floats have one valid form per value: NaN is
rejected at construction and decode, `-0.0` normalizes to `+0.0`, and
infinities are admitted. Decimal types carry golem's fixed scale per type;
values are unscaled integers and are never rescaled. Every payload is
big-endian.

#### Value types and order

Every type except `Bytes` can be an attribute, and every attribute type is
ordered (`TypeTag::is_ordered`): range predicates, keyset-paginated range
queries and range proofs apply to all of them. The order is the bytewise
order of the term bytes, which is golem-db's attribute order:

| Types | Order |
| --- | --- |
| integers, decimals, `Date32`, `Timestamp64` | numeric |
| `F32`, `F64` | IEEE total order (`-inf` < … < `-0` = `+0` < … < `+inf`) |
| `Bool` | `false` < `true` |
| `Addr`, `Bytes4/8/16/32`, `Key` | unsigned bytewise (big-endian) |
| `Str` | bytewise over UTF-8, which is code point order; a proper prefix sorts first, so `"" < "a" < "a\0" < "ab"` |
| `Bytes` | none: field-only |

golem stores a `str` with no length prefix and lets it run to the end of
its index key, so its order is plain bytewise order over the UTF-8 bytes.
The engine's term key stores a `str` as its UTF-8 bytes each plus one and a
`00` terminator (`text⁺`, state layout 2); adding one keeps byte order and
the terminator is smaller than every shifted byte, so a proper prefix
sorts first and the order is exactly bytewise order on the unshifted
bytes, including strings that contain `\0`. `Str` also supports prefix predicates. Values
of different types never compare: a range is typed by its bounds and
matches only attributes of that type.

Golden vectors: `crates/arkiv-db/tests/codec.rs` (`type_tags_and_values`)
for the encoding, `crates/arkiv-db/tests/golem_types.rs`
(`term_bytes_are_golem_stored_forms`) against golem's own vectors, and
`crates/arkiv-db/tests/ordered_ranges.rs` for the order of the byte-ordered
types. The Arkiv entity namespace (`arkiv/schema.rs`, signed JSON-RPC)
admits only the ten Arkiv SDK types (`bool`, `i32`, `u64`, `u256`, `dec`,
`bytes32`, `bytes` as payload, `str`, `addr`, `key`).

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
| 8 | `BalanceTransferred` | `to: OwnerId ‖ amount: 32 bytes, big-endian` |

`ArgumentFault`: 1 `KeyTooLong`, 2 `TooManyCells`, 3 `DuplicateCell`,
4 `SetAndRemoveOverlap`, 5 `ValueTooLarge`, 6 `BytesNotIndexable`,
7 `NotHostOperation`, 8 `NameTooLong`.

`QueryFault`: 1 `NoGroups`, 2 `TooManyGroups`, 3 `TooManyLiterals`,
4 `ZeroLimit`, 5 `LimitTooLarge`, 6 `NotIndexable`, 7 `ValueTooLarge`,
8 `UnboundedRange`, 9 `MixedBounds`, 10 `NotOrdered` (a `Bytes` bound),
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
- `Value::I64(-2)`: `11 ‖ 7ffffffffffffffe`; `Value::Str("hé")`:
  `02 ‖ 00000003 ‖ 68c3a9`.
- `RecordKey` `"a"`: `01 ‖ 61`.
- `WriteOp::CreateRecord { namespace 1, key "a", cells [("price",
  attribute U64 7), ("note", field Str "hi")] }`:
  `05 ‖ 0000000000000001 ‖ 01 61 ‖ 00000002 ‖ 00000005 7072696365 ‖ 01 0d
  0000000000000007 ‖ 00000004 6e6f7465 ‖ 02 02 00000002 6869`.
- An applied write receipt of 379 units under schedule 1, model 3:
  `01 ‖ 00000003 ‖ 00000001 ‖ 000000000000017b ‖ 01`.
- `Rejection::StaleRevision { expected 1, current 2 }`:
  `03 ‖ 0000000000000001 ‖ 0000000000000002`.
- `PriceSchedule::FIXTURE`: `00000001 ‖ 0000001f ‖ 0000000000000001 ‖
  0000000000000032 ‖ … ‖ 0000000000000001` (256 bytes).
- `Limits::DEFAULT`: `0000000100000000 ‖ 0000000000000040 ‖
  0000000000000020 ‖ 0000000000001000 ‖ 0000000000001000 ‖
  0000000000000080 ‖ 00000000000000c8 ‖ 0000000000000008 ‖
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
| `arkiv/header/v1` | encoded header (with the state root): the block hash | in use |
| `arkiv/inputs/v1` | encoded block inputs | in use |
| `arkiv/outcomes/v1` | encoded block outcomes | in use |
| `arkiv/tx/v1` | encoded transaction envelope | in use |
| `arkiv/admin/v1` | encoded admin input | in use |
| `arkiv/genesis/v1` | encoded genesis | in use |
| `arkiv/state/v1` | encoded state header: the state root | in use |

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
the volatile process/RPC and pinned signed envelopes; the simulator and the
signed proposer add durable node storage.

### Versions

- **Block format version** `FORMAT_VERSION = 1` covers every layout and
  tag table of this part. The genesis records it and every header starts
  with it; another format version is refused (`UnsupportedVersion { kind:
  "block format" }` or `{ kind: "header" }` when decoding,
  `GenesisError::UnsupportedFormat` from `ChainSpec::new`).
- **Protocol version** `PROTOCOL_VERSION = 1` names the execution rules of
  this document. The genesis records it; another protocol is refused
  (`UnsupportedVersion { kind: "protocol" }`,
  `GenesisError::UnsupportedProtocol`).
- Every header carries its block's terminal state root; there is one
  header layout.
- Tags start at 1 (except the two request scopes, fixed at `00`/`01`), are
  never reused and never derived from Rust discriminants; unknown tags are
  refused. A new variant gets a new tag. Any change to an existing layout
  needs a new format version and, for hashed objects, a new digest domain;
  bytes of another version are never reinterpreted.

### Layouts

Every field is encoded with the rules of part 1, in the order listed. `B256`
is 32 raw bytes, `OwnerId` 20, `RequestId` (a request id) 32.

| Type | Layout | Bytes |
| --- | --- | --- |
| `ChainParams` | `block_budget u64 ‖ user_budget u64 ‖ expiry_per_block u32 ‖ max_admin_per_block u32 ‖ max_txs_per_block u32 ‖ max_ops_per_tx u32 ‖ max_ops_per_block u32 ‖ max_tx_bytes u32 ‖ max_block_input_bytes u32 ‖ max_lifetime u64` | 52 |
| `Genesis` | `format_version u32 ‖ protocol_version u32 ‖ chain_id u64 ‖ timestamp_ms u64 ‖ limits: Limits ‖ pricing: PriceSchedule ‖ params: ChainParams ‖ admins: Vec<OwnerId>` | 408 + 20 per admin |
| `Header` | `format_version u32 (1) ‖ chain_id u64 ‖ height u64 ‖ parent_hash ‖ timestamp_ms u64 ‖ inputs_digest ‖ outcomes_digest ‖ state_root` | 156 |
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
| `HostFault` | tag, then the `ExpiryFault`, `ArkivFault` or `AccountFault` | 1–2 |
| `ExpiryFault` | tag | 1 |
| `Block` | `header ‖ inputs ‖ outcomes` | 184+ |
| `Scope` | `00` transaction, `01` admin | 1 |
| `RequestIdentity` | `scope ‖ chain_id u64 ‖ actor ‖ request_id` | 61 |
| `Inclusion` | `height u64 ‖ scope ‖ index u32 ‖ digest` | 45 |

Receipts inside outcomes are the part-1 `Receipt` encoding (no debug
ledger). `RequestIdentity` and `Inclusion` are not hashed; their encodings
exist for the durable inclusion records of step N4.

### Tag tables

`AdminAction`: 1 `SetPricing(PriceSchedule)`.

`TxOp` (the engine `WriteOp` tags and layouts, plus the expiry of record
creations and patches, an Arkiv entity operation and a native transfer):

| Tag | Variant | Fields |
| --- | --- | --- |
| 1 | `CreateNamespace` | `name: NamespaceName` |
| 2 | `TransferNamespace` | `namespace ‖ expected_revision ‖ new_owner` |
| 3 | `Put` | `namespace ‖ key: bytes ‖ value: bytes` |
| 4 | `Delete` | `namespace ‖ key: bytes` |
| 5 | `CreateRecord` | `namespace ‖ key: RecordKey ‖ cells: Vec<(CellName, Cell)> ‖ expiry: Option<Expiry>` |
| 6 | `PatchRecord` | `namespace ‖ key ‖ set: Vec<(CellName, Cell)> ‖ remove: Vec<CellName> ‖ expiry: Option<Expiry>` |
| 7 | `DeleteRecord` | `namespace ‖ key` |
| 8 | `Arkiv` | `namespace ‖ operation: ArkivOperation` |
| 9 | `Transfer` | `to: OwnerId ‖ amount: 32 bytes, big-endian` |

`ArkivOperation` (`crates/arkiv-node/src/arkiv`): 1 `Create { salt: 16
bytes ‖ flags u8 ‖ expiry: Expiry ‖ attributes: Vec<(CellName, Value)> }`,
2 `Patch { key ‖ set: Vec<(CellName, Value)> ‖ remove: Vec<CellName> }`,
3 `ExtendExpiry { key ‖ expiry }`, 4 `TransferOwnership { key ‖ owner: 20
bytes }`, 5 `Delete { key }`.

`AdminOutcome`: 1 `Applied`, 2 `Rejected` followed by the `AdminRejection`.
`AdminRejection`: 1 `ReservationViolated`, 2 `ReservationOverflow`,
3 `StaleSchedule { current: u32 }`, 4 `GasProfileLocked` (a chain's GLM
gas schedule cannot be entered or changed through admin repricing).

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
| 7 | `AccountRejected` | `fault: AccountFault ‖ receipt` |

`HostFault`: 1 `ExpiryCapacityFull`, 2 `ExpiryInvalid` followed by the
`ExpiryFault`, 3 `ExpiryShortened`, 4 `Arkiv` followed by the `ArkivFault`
(one byte, 1 `Name` … 15 `UnsupportedGasOperation`), 5 `Account` followed by
the `AccountFault` (1 `InsufficientBalance`, 2 `Overflow`, 3 `Unsupported`). `ExpiryFault`: 1 `Overflow`,
2 `DeadOnArrival`, 3 `TooFar`. `Scope`: `00` transaction, `01` admin.

### Hashes and the genesis block

| Hash | Preimage |
| --- | --- |
| block hash | `digest("arkiv/header/v1", encode(header))` |
| `inputs_digest` | `digest("arkiv/inputs/v1", encode(inputs))` |
| `outcomes_digest` | `digest("arkiv/outcomes/v1", encode(outcomes))` |
| transaction digest | `digest("arkiv/tx/v1", encode(envelope))` |
| admin input digest | `digest("arkiv/admin/v1", encode(admin_input))` |
| genesis digest | `digest("arkiv/genesis/v1", encode(genesis))` |

The genesis block is height 0 with parent hash `00…00`, the genesis
timestamp, the genesis digest as its `inputs_digest`, the digest of empty
outcomes (`00000000 ‖ 00000000 ‖ 00000000 ‖ 0000000000000000`) as its
`outcomes_digest`, and the root of the empty initial state (no namespaces,
empty host map, the genesis pricing) as its `state_root`, computed without
an engine. The genesis hash, the chain's identity, is that header's block
hash; it commits to the whole genesis. A block hash commits to the header,
and through its digests and state root to the block's inputs, outcomes and
terminal state. A signed chain with a height-zero prefill replaces the
genesis state root by the prefilled state's root (see
`docs/genesis-bundle-v1.md`).
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
  is refused. Each would halt the chain at its first due height;
- `Limits::validate`: a cell-name limit of 1 to 32 and an indexed-string
  limit of at most 128 (`GenesisError::CellLimits`).

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
parent's chain id is the genesis chain id; `height = parent.height + 1` (checked; the last height has
no successor); and `timestamp_ms ≥ parent.timestamp_ms` (equal is
allowed). A recorded header must also carry the genesis chain id and
`parent_hash = parent`'s block hash. The
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
   of the deletions' and transactions' spends. The header carries the
   parent's block hash, the two digests and the candidate's state root.

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
sizes, `Bytes` attributes) **as it will execute**: with a
placeholder `$expiresAt = U64(0)` injected when it carries an expiry. So
a record with an expiry has at most `max_cells − 1` user cells, and an
expiry-only patch is valid. Names of at most 20 characters and record keys
of 1 to 32 bytes are guaranteed by their types. An empty patch (no set,
no remove, no expiry) is valid too: golem-db's no-op. An admin input is valid
when its actor is a genesis admin. A block's inputs are valid when every
admin input and transaction is, the selection limits hold, and no request
identity repeats; the executor also checks that the block follows its
parent, and the chain (`MemoryChain`, later the node) that no identity was
included by an earlier block.

User requests reach neither `execute_host`, `set_pricing` nor the host
map, and cannot write or remove `$` cells.

Namespace 1 is the Arkiv entity namespace on every chain and is
host-controlled: a generic operation (`Put`, `Delete`, record or namespace
operations) addressed to it is refused (`OpFault::HostOwnedCell`). An
`Arkiv` operation must pass the entity rules' static checks
(`ArkivOperation::validate`) and its expiry the expiry checks above; on a
chain without GLM pricing it may only address namespace 1, and under GLM
pricing it may address any namespace, while generic record operations are
disabled there (only `CreateNamespace` executes; the rest are
`HostRejected(Arkiv(UnsupportedGasOperation))`). `Transfer` passes static
validation and executes only on chains with accounts (signed chains).

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

- There is one header layout; it always carries the state root.
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
  `TxOp` and `OpOutcome` variant, the header, genesis, inputs,
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
