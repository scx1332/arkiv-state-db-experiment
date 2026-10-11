# State trie: the persistent Merkle Patricia map

## Status and scope

Node step N3a of the [node and storage plan](node-storage-plan.md) is
implemented as the standalone crate `crates/arkiv-db-trie`: a persistent
Merkle Patricia map (`MerkleMap`) with in-memory and stored nodes, lazy
fallible loading, a staging walk for atomic commits, point-proof
generation and a strict proof verifier (`walk`), plus the order-preserving
key encoders the engine's maps will use. It depends only on the vendored
`alloy-trie`, `alloy-primitives` and `alloy-rlp`; it has no dependency on
`arkiv-db` or on the N2 storage contract, and it defines its own loader and
error types for the engine to map onto its own.

Node step N3b built the engine on it: every engine map is a `MerkleMap`
with the key encodings below, and every engine state has a state root (see
[Engine integration](#engine-integration-node-step-n3b)). N3c now persists
the engine with bounded preflight/staging and stored-root publication; the
demo composes the point proofs needed for complete Eq pages. Broader
point-read APIs remain N3d (see
[remaining work](#durable-integration-and-remaining-work)). Every statement about the crate is
backed by the tests listed in [Evidence](#evidence); the engine integration
section cites its own tests.

## Node encoding

A node encodes exactly as `alloy_trie::nodes` encodes it (the Ethereum MPT
encoding):

| Node | Encoding |
| --- | --- |
| Leaf | RLP list `[hex_prefix(path, leaf), value]` |
| Extension | RLP list `[hex_prefix(path, extension), child reference]` |
| Branch | RLP list of 16 child references (`0x80` for an empty slot) and an empty value `0x80` |

A child reference is the child's own encoding when that is shorter than 32
bytes (an *inline* child) and `0xa0 ‖ keccak256(encoding)` otherwise. The
hex-prefix path encoding is the yellow paper's (appendix C): flag nibble
`0`/`1` for an extension, `2`/`3` for a leaf, the odd flag carrying the
first nibble. A node's object id is `keccak256(encoding)`, the same 32
bytes its parent embeds, so the store is content-addressed.

The crate uses its own node types rather than `alloy_trie`'s `LeafNode` and
`ExtensionNode`, because those hold paths in `nybbles::Nibbles`, which is
capped at 64 nibbles (32-byte keys), while engine maps need 33- and 164-byte
keys. Its paths are unpacked nibbles of any length; the byte layout is the
same format continued to longer paths. Evidence: `node.rs` unit tests
compare every encoding with alloy's encoders byte for byte wherever alloy
can represent the node, and `alloy_node_decoder_panics_on_paths_beyond_32_bytes`
shows `alloy_trie::nodes::TrieNode::decode` panicking on a 35-byte path,
which is also why the crate decodes nodes itself.

Decoding is strict and total: it never panics, accepts only canonical RLP
and hex-prefix encodings (so `encode(decode(bytes)) == bytes` for every
accepted input, checked on 20,000 mutated inputs), and rejects shapes a
canonical map never contains: branch values, branches with fewer than two
children, empty extension paths, extensions without a branch child, empty
leaf values, and inline children of 32 bytes or more.

## Canonical shape and the root

Every map has exactly one shape for its key/value set: branches have at
least two children and no value; extensions precede only branches and have
nonempty paths; each leaf holds the rest of its key. Inserts split a leaf or
an extension at the first differing nibble (keeping the old extension's
child link as it was, unloaded); removals collapse a branch left with one
child into its parent's path, merging leaf and extension paths, which may
require loading that remaining sibling from the store. The root hash is
`keccak256(root encoding)` (the root is always referenced by hash, even
when its encoding is shorter than 32 bytes) and `EMPTY_ROOT_HASH` for the
empty map.

The root therefore depends only on the key/value set. Evidence: after
random insert/replace/remove histories (key lengths 1, 2, 8, 21, 32, 33 and
88 bytes, dense and sparse key alphabets) the root equals
`alloy_trie::HashBuilder` over the sorted leaves (keys ≤ 32 bytes) or the
independent reference construction in `tests/support` (any length, itself
checked against `HashBuilder` up to 32 bytes); permuted insertion orders with
decoy inserts, removals and overwritten values give one root.

Root hashing never reads the store: a stored link's hash is its id, and only
nodes built in memory are encoded and hashed (once; the reference is cached
in the node). `MerkleMap::root_hash` is therefore infallible.

## Keys and their encodings

`TrieKey` bounds a key type's encoded length by `MAX_LEN` (at least one
byte, checked at compile time) and fixes its encoding; the map orders entries
by the unsigned bytewise lexicographic order of the encodings (a proper
prefix first), so iteration is in encoded order. Keys may have any length up
to `MAX_LEN`. The contract (see `codec.rs`): encodings are injective and
canonical (`decode` rejects every byte string `encode` does not produce),
order-preserving and **prefix-free**: no encoding is a strict prefix of
another. Branches carry no value (the node encoding, proof shapes and
hashing are unchanged from the fixed-length trie), so a key that is a strict
prefix of a stored key cannot be represented; `insert` rejects one with the
caller error `TrieError::KeyNotPrefixFree` before changing anything, while
`get`, `remove` and proofs simply answer absence for it. `MAX_LEN` is a
safety bound, used to reject stored leaves whose key exceeds it or ends
inside a byte (`TrieError::Inconsistent`), never to preallocate. Fixed-width
keys (`u64` big-endian, `[u8; N]`, the engine's ids) are trivially
prefix-free and keep their exact widths in their own decoders;
`order::ByteKey<MAX>` is a bounded variable-length byte-string key (the
empty string included). `is_prefix_free` checks a key set in tests.

`arkiv_db_trie::order` provides the order-preserving building blocks for
composite keys:

- `encode_bytes(bytes)` writes a self-delimiting string: `00 → 01 01`,
  `01 → 01 02`, every other byte as is, then a `00` terminator. The
  terminator occurs nowhere else, so the encoding is prefix-free; the
  escapes keep the order of `00 < 01 < 02…` and the terminator is the
  smallest byte, so encodings order exactly as the strings (`"" < "a" <
  "a\0" < "aa"`). `escape_bytes` is the same without the terminator; it is a
  prefix of the encoding of exactly the strings that start with its input,
  so the run of those strings is `[escape(p), prefix_successor(escape(p)))`.
  `decode_bytes` rejects a missing terminator, a bad escape and an
  over-long value. (FoundationDB's tuple form, `00 → 00 ff` with a `00`
  terminator, is self-delimiting only with one byte of lookahead: `61 00`
  (`"a"`) is a strict prefix of `61 00 ff 00` (`"a\0"`), so it is not
  prefix-free as a whole key and is not used.)
- `prefix_successor(p)` increments the last byte of `p` below `ff` and drops
  the rest (none for an empty or all-`ff` prefix: unbounded). It is the
  exclusive end of a prefix run, not the successor of one key.

- `encode_padded(bytes, max_len)` writes `bytes ‖ 0x00 × (max_len − len) ‖
  len`, `max_len + 1` bytes. Order is preserved: the first differing padded
  byte decides exactly as the bytewise comparison of the strings does (a
  string that ends first shows `0x00` where the longer one continues, and a
  proper prefix is smaller); if all padded bytes agree, one string is the
  other plus trailing zero bytes and the length byte orders them. Decoding
  rejects a length above `max_len` and nonzero padding, so the encoding is
  unique.
- Fixed-width fields (bytes, big-endian integers) compare as their values,
  and concatenated fixed-width fields compare as tuples, because each field
  starts at the same offset in every key.
- `KeyWriter` and `KeyReader` compose them.

Evidence (`tests/encoding.rs`): random strings biased toward empty strings,
trailing zeros, prefixes of other strings and maximum lengths (for maximum
lengths 0, 1, 2, 3, 20 and 64) order exactly as Rust's slice ordering, and the
88-byte composite `(name ≤ 20, tag, presence, value ≤ 64)` orders exactly as
the Rust tuple ordering, with existence (`presence = 0`) before an empty
value, and a map keyed by it iterates in tuple order.

The engine key encodings (implemented in node step N3b, in
`crates/arkiv-db/src/layout.rs`) and the resulting bounds (see
[Depth bound](#depth-bound) and
[Nodes an update creates](#nodes-an-update-creates)):

| Map | Key encoding | Bytes | Nodes on a path (≤ 2·L + 1) | New nodes per insert (≤ 2·L + 2) |
| --- | --- | --- | --- | --- |
| catalog | `NamespaceId` big-endian | 8 | 17 | 18 |
| host | raw `[u8; 32]` | 32 | 65 | 66 |
| records (raw) | `key ‖ 0x00 pad to 20 ‖ len` | 21 | 43 | 44 |
| keys | RecordKey `len ‖ bytes ‖ pad` | 33 | 67 | 68 |
| rows, postings | `RecordId` big-endian | 8 | 17 | 18 |
| terms | `name ‖ pad to 32 ‖ name_len ‖ tag ‖ presence ‖ value ‖ pad to 128 ‖ value_len` | 164 | 329 | 330 |

## Depth bound

A key of `L` bytes has `2L` nibbles. Every branch on its path consumes one
nibble, every extension at least one and is followed by a branch, and the
path ends at one leaf (possibly with an empty path) or earlier, so a path
has at most `2L + 1` nodes. A lookup, an insert or a proof visits at most
that many; a removal visits at most one more (the collapse sibling). The
bound is reached: the crafted set of `2L + 1` keys that are zero except for
one nibble each (`deep_keys` in `tests/map.rs`) forces a branch at every
nibble of the zero key, whose lookup visits exactly `2L + 1` nodes and
whose deepest neighbor's removal visits exactly `2L + 2`, for
L = 1, 8, 21, 32, 33 and 88. Random histories assert the bound on every
operation.

### Nodes an update creates

The nodes an update creates are the ones the next root computation
encodes (and, at 32 bytes or more, hashes) and the next staging walk may
emit, so they, not the visits, bound the rehashing work of one entry
write. They can exceed the path bound by one:

- An insert of a new key creates the new key's whole path: the rebuilt
  nodes above the point where it leaves the existing trie, a new branch
  there (with an extension above it when the keys share more nibbles) and
  its leaf, at most `2L + 1` nodes. When it leaves the trie inside a leaf,
  or inside an extension more than one nibble before its end, it also
  re-creates that node below the new branch with a shorter path. At most
  `2L + 2`.
- A replacement of an existing key's value rebuilds its path: at most
  `2L + 1`.
- A removal rebuilds the at most `2L` nodes above the removed leaf. A
  branch left with one child becomes one node (the child with the nibble
  prepended, or an extension over it), which an extension above absorbs;
  both replace nodes on the path rather than add to them. At most `2L`.

Evidence (`update_creation_bounds_are_reached` in `tests/store.rs`, for
L = 1, 8, 21, 33 and 88, with 40-byte values so every node is an object of
its own): with the deep-path key set committed except the key that parts
at the last nibble, inserting that key stages exactly `2L + 2` objects,
which are exactly the objects of the new trie that the old one lacks
according to the reference construction; removing it again stages exactly
`2L`. Random histories on reopened maps stage no more than `2L + 2`
objects per update in total. A per-entry-write bound for the engine's
metering (`RootUpdate`) has to use `2L + 2`, not the path bound `2L + 1`,
when it counts rehashed nodes; node step N3b measures the engine against it
(see [Engine integration](#engine-integration-node-step-n3b)).

## Links, handles, loading and errors

```text
Link<V>    = Empty | Mem(Arc<NodeBox<V>>) | Stored(ObjectId)
NodeBox<V> = { node, rlp: OnceLock<RlpNode>, from_store: bool }
MerkleMap<K, V> = { root: Link<V>, len: u64 }         // O(1) clone
```

A handle is a root link plus the entry count; the count is kept in the
handle and must be committed by whoever stores the root
(`MerkleMap::from_root(root, len)` accepts `(EMPTY_ROOT_HASH, 0)` as the
empty map and rejects every other pairing of the empty root or a zero
length). Updates copy one key path and share everything else, so older
handles stay valid. The object space is not part of the handle: the host
chooses the loader (and so the space) for each operation.

Handles and nodes hold no store, loader, context or cache reference. Every
operation that can reach a stored link takes a loader by reference:

```rust
pub trait NodeLoader<V: TrieValue> {
    type Error: From<TrieError>;
    fn read(&self, id: &ObjectId) -> Result<Option<Vec<u8>>, Self::Error>;
    fn load(&self, id: &ObjectId) -> Result<Arc<NodeBox<V>>, Self::Error>; // default: read + decode_stored
}
```

`NodeBox::decode_stored(id, bytes)` checks `keccak256(bytes) == id`,
decodes strictly, turns inline children into `Mem` nodes and marks every
node it produces `from_store`. Shapes that hash correctly but are not
canonical are errors, not answers: inside one object they are
`TrieError::Malformed`; across objects (a stored non-root node shorter than
32 bytes, an extension whose stored child is not a branch, a leaf whose key
ends inside a byte or exceeds the key type's `MAX_LEN`) they are
`TrieError::Inconsistent`. The bound comes from the key type, never from a
requested key, so an unrelated leaf is not judged by a query's length.
A host with its own storage error type implements `read` with that type and
gets it back unchanged from every operation (trie failures arrive through
`From<TrieError>`: `Missing` maps to "object missing", `EmptyValue` to a
caller error, everything else to "corrupt"). A host with a bounded decoded-node cache keyed by (database id,
space, map kind, object id) overrides `load`; decoded nodes own nothing that
points back at the cache, so the cache never creates reference cycles and
can be dropped at any time. The map kind belongs in the key because the
decoded node type depends on the value type.

Reading a missing, corrupt or unreadable object is an error from `get`,
`insert`, `remove`, iteration (the error is yielded, then the iterator
ends) and proof generation; it is never reported as an absent key.

### Visit counting

Every operation reports a `Probe { nodes, loads }`. `nodes` follows the
trie visit semantics of the engine's `PhysicalTrace` (node step N3b): every
node an operation examines counts once, whether it was in memory, served by
a cache or loaded, inline children included; a lookup and an insert count
the key's path, a removal also the collapse sibling, an iterator its seek
path and every node it steps into. Nodes an update creates are not visits.
`loads` counts stored links followed through the loader and is physical
instrumentation. Evidence: a map reopened from the store reports the same
`nodes` as the in-memory original for each of 300 lookups per key length
and for a full iteration, while `loads` differ, and a caching loader leaves
both unchanged on a warm read while making no store read.

## Values and nested maps

`TrieValue` values encode canonically and never to the empty string:
`MerkleMap::insert` rejects a value whose encoding is empty with
`TrieError::EmptyValue` before it visits or reads anything (it asks
`TrieValue::encodes_empty`, which by default encodes the value once; a
type whose encoding can never be empty answers `false` without encoding,
which keeps inserting a value that holds nested maps from hashing them),
and the decoder rejects stored empty values. The
crate's own `Vec<u8>` value can be empty, so this is a caller error, not
a panic; the map still panics if an encoding it hashes is empty, which
only a nondeterministic `encode` can cause. A value may hold nested maps (a
posting map in a term entry): it encodes the nested root and length, which
is the hashing hook, and implements `stage_nested` to stage the nested map's
new nodes, which is the staging hook.

## Staging and commits

`MerkleMap::stage(sink)` walks from the root and records `(id, encoding)`
for every node built in memory that must be an object: the root, whatever
its size, and every other node of 32 bytes or more. Inline nodes are never
recorded (they are part of their parent's bytes). The walk stops at stored
links and at `from_store` nodes. Its invariant: a `from_store` node's bytes
are committed in the map's space, as an object or inside an ancestor's
object, and updates never place a loaded node into a new version except as
the unchanged inline child of a rebuilt node or through its original stored
link. For each leaf of a new node the walk calls `TrieValue::stage_nested`
(same space); `stage_with` hands the values to a closure instead, for nested
maps in another space. The host gives each space its own sink, and a sink
keeps each id once. Staging hashes whatever is not hashed yet and reads
nothing.

The walk does **not** stop at a new node whose id the sink already holds.
Maps with different value types share a space (the planned raw, keys,
rows, postings and terms maps of a namespace all do), and a leaf of one
can be byte-identical to a leaf of another whose value names a nested map:
the same bytes are a plain value in one map and a posting root and length
in the other. An equal id therefore does not mean that the nested maps
below were staged, and skipping the subtree could commit a leaf whose
nested map is missing. So the walk visits every new node reachable from
the root and every new leaf value, whatever the sink has seen; the sink
deduplicates only what it keeps. Its work is the number of new nodes
reachable from the root, which the updates since the last commit created.

After the staged objects are committed, `to_stored()` gives a handle whose
root is a `Stored` link, so the in-memory nodes can be dropped and later
staging starts empty.

Evidence (`tests/store.rs`): the staged ids of a map equal the set of
objects the independent reference construction derives (root plus nodes of
at least 32 bytes), each once; after updates on a map reopened from its root,
the staged ids are a subset of the new trie's objects and include every one
the store lacks, at most `2L + 2` per update; a removal whose remaining
sibling is a branch reuses its stored link (one object staged); nested
posting maps are staged through the hook, identical posting maps once, and
through `stage_with` into a second sink; a plain map and a terms map whose
single leaves are byte-identical, staged into one sink in either order,
stage the posting map and read back in full
(`a_shared_sink_stages_nested_maps_whatever_was_staged_first`, which fails
if the walk skips known ids); everything reopens and reads lazily; the old
root stays readable.

## Proofs

A proof for key `k` in one map is the root node followed by every
hash-referenced node on `k`'s path, in path order. Inline nodes travel inside
their parent. The same form proves inclusion (the path ends at `k`'s leaf)
and absence (it ends at a leaf or extension diverging from `k`, or at a
branch whose slot for `k` is empty). The empty map's proof is empty.
`MerkleMap::proof` yields the nodes lazily, one per `next`, so a host can
charge each node before it is produced and stop without further work
(node step N3d plans to charge `{ProofNode 1, ProofByte len}` per node).

### The strict verifier

`walk(root, key, nodes) -> Result<Option<Vec<u8>>, ProofError>` returns the
value the proof establishes; it never takes a claimed value. It enforces:

1. empty map ⇔ `root == EMPTY_ROOT_HASH` and no nodes (nodes with the empty
   root are rejected, although `keccak256(0x80)` is the empty root);
2. `keccak256(nodes[0]) == root`, whatever the node's length;
3. every later node is at least 32 bytes and hashes to the 33-byte reference
   the walk reached along the key's nibbles; inline children are decoded in
   place;
4. the terminal is a leaf whose path equals the remaining key (inclusion), a
   leaf or extension diverging from it (including a path the key ends
   inside, or a leaf path the key extends), an empty branch slot, or a
   branch the key ends at (`KeyEndsAtBranch`) (absence); nothing may follow
   it (`TrailingNodes`);
5. a hash reference reached with key nibbles left and no next node is
   `Incomplete`, never absence;
6. every node decodes strictly; an extension's child must be a branch; a
   leaf's key must end on a byte boundary.

Missing, extra, duplicated, reordered and separately supplied inline nodes
therefore all fail. The walk is iterative, allocates in proportion to its
input and never panics.

Any byte string can be queried. Branches hold no values, so a key that is
prefix-related to a stored key (it ends inside a path or at a branch, or
extends a leaf's key) is proven absent, which is the truth; it can never be
proven included. A verifier composing proofs encodes the key itself, with
the map's `TrieKey`, before calling `walk`. Evidence:
`prefix_related_keys_are_proven_absent_never_included` in `tests/proofs.rs`
and `tests/variable_keys.rs`.

### Complete intervals

`MerkleMap::interval_proof(&KeyRange)` emits a complete witness for an
interval of raw encoded byte strings, each end `Included`, `Excluded` or
`Unbounded` (`KeyRange::prefix(p)` is `[p, prefix_successor(p))`).
`walk_interval(root, &range, nodes, IntervalLimits { max_entries,
max_key_len })` verifies it and returns every entry in the interval. Both
open exactly the children for which `KeyRange::may_hold(prefix)` holds: a
subtree whose keys all extend the nibble path `prefix` is skipped only when
`prefix` differs from the lower endpoint at a nibble where it is smaller, or
its smallest completion (`prefix`, plus a zero nibble when it ends inside a
byte) is above the upper endpoint (or equal to an excluded one). No
fixed-width zero/`ff` completion is used. An empty interval (`lower >
upper`, or equal endpoints with either excluded) has an empty witness. A
missing intersecting child, an extra node, more than `max_entries` entries,
or a key/path beyond `max_key_len` fails the whole walk; a limit never
yields a truncated successful result. For the fixed-width closed intervals
the engine used before, this rule opens exactly the children the old
prefix comparison opened, so those witnesses are byte-identical.

### Why `alloy_trie::proof::verify_proof` is not the verifier

`verify_proof(root, key, expected, proof)` compares a caller-claimed value
with what it read at the end of the proof, and treats "the proof ended
before the key's path did" as a value of `None`. The tests run alloy's own
verifier to show three forgeries it accepts, each rejected by `walk`:

| Forgery | alloy `verify_proof` | `walk` |
| --- | --- | --- |
| An inclusion proof truncated after any node, claimed as absence | `Ok(())` for every cut of 40 proofs | `Incomplete` |
| A node `F` appended after the leaf whose stored value is `0xa0 ‖ keccak256(F)` (or `F` itself, if under 32 bytes), with `F` a leaf carrying a forged value | `Ok(())` for the forged value | `TrailingNodes` |
| A proof cut after the last branch, claiming the leaf's hash reference as the value | `Ok(())` | `Incomplete` |

It also rejects honest proofs whose root node is shorter than 32 bytes
(single-entry maps), and cannot take keys longer than 32 bytes. It is used
only as a cross-check on honest proofs whose root node is at least 32 bytes
and whose keys are at most 32 bytes, where it agrees with `walk`.

## Evidence

Run `cargo test -p arkiv-db-trie --offline` (all tests below also run in
`cargo test --workspace --offline`). Oracles are independent of the crate:
`BTreeMap` models, `alloy_trie::HashBuilder`/`ProofRetainer` (keys ≤ 32
bytes; retained nodes under 32 bytes other than the root are dropped
because they are inline), and the reference construction in
`tests/support/mod.rs`, written from the yellow paper with its own RLP and
hex-prefix code and cross-checked against `HashBuilder`. Visit counts are
hand-derived literals from the crafted key sets.

| File | Establishes |
| --- | --- |
| `src/node.rs` (unit) | Byte equality with alloy's leaf, extension and branch encoders; long paths; strict decoding round trips; hash check on load; `from_store` inline children; rejection of noncanonical forms; 20,000 mutated inputs never panic and only canonical bytes decode; alloy's decoder panics on a 35-byte path |
| `src/nibbles.rs`, `src/order.rs` (unit) | Hex-prefix vectors and rejections; padded-field vectors; writer/reader round trip |
| `tests/map.rs` | Random histories against `BTreeMap` and the root oracles for key lengths 1–88; history independence; retained versions unchanged; depth bound reached exactly (`2L + 1`, removal `2L + 2`); empty and single-entry maps; handle validation; empty values rejected by `insert` before any visit or read |
| `tests/proofs.rs` | Proofs equal `ProofRetainer` (≤ 32 bytes) and the reference (33, 88 bytes); `walk` gives the model's answer; alloy cross-check where valid; the three alloy forgeries; randomized mutations (bit flips, truncations, reordering, duplication, extra and inline nodes, wrong roots, other keys and versions) are rejected or give the honest answer; self-consistent adversarial tries and garbage never panic; old proofs verify against old roots; lazy, counted generation; prefix-related keys proven absent, never included |
| `tests/variable_keys.rs` | Variable-length prefix-free `ByteKey`s (`00`/`01`/`ff`-heavy, the empty key, maximum length) against a byte-ordered `BTreeMap`: insert/overwrite/remove histories, lookups, typed and raw seeks, retained handles, roots equal to `HashBuilder` and the reference, insertion-order independence, collapse back to the original root, point proofs equal to the reference, complete interval proofs over random inclusive/exclusive/unbounded raw endpoints and prefix runs (omissions, extras, entry and key-length limits rejected), stored reopen/walk/prove; a non-prefix-free codec rejected by `insert` without change |
| `tests/store.rs` | Staging equals the reference object set; reopened maps read lazily with residency-independent visit counts; lazy updates load only touched paths; restaging is complete, minimal and bounded; the per-update creation bounds (`2L + 2` for an insert, `2L` for a removal) reached exactly; history readable; missing and corrupt objects and host loader errors surface as errors; caching loader; nested maps through the hook and `stage_with`, also when another value type staged a byte-identical leaf into the same sink first; hash-valid noncanonical tries rejected by the map and by `walk` |
| `tests/encoding.rs` | Order preservation and uniqueness of the padded and composite encodings |

## Engine integration (node step N3b)

The engine (`crates/arkiv-db`) keeps every map in a `MerkleMap`: the
catalog, the host map, and per namespace the raw records, key bindings,
rows and terms, with a posting map nested in every term leaf. The key
encodings are the table above; leaf values are canonical-codec bytes (raw
and host values `u32 len ‖ bytes`, key bindings a `u64` record id, rows
`RecordKey ‖ cells sorted by name`, postings `01`, term leaves `posting root
‖ posting len`, catalog leaves `descriptor ‖ next_record ‖` four `root ‖
len` handles). The state root is `digest("arkiv/state/v1",
encode(StateHeader { format 1, catalog root and length, next_namespace,
pricing, host root and length }))`. The layouts are specified in
`docs/architecture.md`; `layout.rs` unit tests show the raw and term key
orders equal the orders the engine used before (bytewise, and
`(name, type, existence first, value bytes)`).

Every map operation takes a loader naming the map's object space and kind:
the crate's `MemoryOnly` loader for a memory engine, or an
`arkiv_db_storage::versioned::ObjectReader` for a state reopened from its
committed objects (the hidden `Engine::from_state` test aid, until node
step N3c supplies the store context and cache). Storage errors reach the
engine's `try_` forms unchanged and poison a branch.

Evidence in `crates/arkiv-db/tests`:

- `roots.rs`: over a seeded 40-block history, every map root and the state
  root equal an independent construction over leaves built by hand from a
  model: the reference construction of this crate's `tests/support`
  (included by path), cross-checked with `HashBuilder` for every map whose
  keys have at most 32 bytes, and a hand-encoded `StateHeader` digested
  with keccak256. Identical content reached by different histories has one
  root; old snapshots keep their roots; every logical field (owner,
  pricing, record allocator, host entry, cell value, raw value) changes the
  root.
- `storage_faults.rs`: a state staged into an object store and reopened
  reads nothing at open, loads nodes on demand and answers every read with
  the memory engine's values, receipts and node visits (visits do not
  depend on what was loaded or cached); failed, missing and corrupt nodes
  are storage errors, never absent records or rejections.
- `rehash.rs`: per map, the nodes a block builds that carry a hash (every
  new map root and every new node of 32 bytes or more; computing the state
  root hashes each once, plus one state-header digest), against the
  `RootUpdate`-charged entry writes times `2L + 2`, and the distinct objects
  the block stages, which a commit writes. The two differ when writes of
  one block build identical nodes, such as the one-entry posting maps of a
  new record's terms: each is hashed, but stored once. Measured on
  2026-09-24 (`cargo test -p arkiv-db --offline --test rehash --
  --nocapture`):

  | Block | Hashed nodes | Staged objects | Allowed by the entry writes |
  | --- | --- | --- | --- |
  | 200 record creations in two namespaces | 710 | 650 | 438,436 |
  | 20 mixed blocks of 2–30 operations | 28–206 | 27–199 | 866–22,006 |
  | One key inserted on a crafted 67-node key path | 66 key-map (71 in all) | 66 key-map (71 in all) | 68 key-map (104 in all) |
  | One key removed there, collapsing the deepest branch | 65 key-map | 65 key-map | 68 |
  | One record added to a crafted 175-node term path | 177 term (189 in all) | 177 term (189 in all) | 660 term (800 in all) |

  The crafted fixtures reach the bounds as closely as valid data allows: 66
  record keys branch at every nibble a 33-byte key encoding can branch on
  (the length's low nibble cannot: 0x21–0x2f exceed 32), so the spine's
  path is 67 = `2·33 + 1` nodes and one more key rehashes 66 of them (its
  new leaf is inline). 214 terms branch at 172 nibbles of the term
  encoding's name and value fields; only the name length (equal padded names are
  equal names, since names never contain 0x00) and the tag's and presence
  byte's high nibbles cannot differ while the bytes before them agree. The
  value length can: a `Str` value may end in zero bytes, so values that
  agree on all padded value bytes can still differ in length. The spine
  term's path has 175 nodes (172 branches, two extensions and a leaf).
  Adding a record to the spine term rewrites its leaf (175 path nodes) and
  its existence term's leaf, with one node on the existence term's side
  of the trie (two more: 177). These are
  logical counts; hashing time was not measured.

## Durable integration and remaining work

- **N3c is implemented.** `DurableEngine` supplies a gated store context,
  bounded decoded-node cache, manifests, durable head and lazy historical
  snapshots over redb. `preflight_with` checks new node/value bounds before
  encoding/hashing; `try_stage_with` stops on work/sink failures and visits
  nested maps synchronously. Physical batch deduplication does not skip
  nested maps of another type. Weak-reference tests establish dirty-node
  release and that stored handles retain no decoded trees after reads.
  `Engine::from_state` and unbounded staging helpers remain test aids.
- **Broader N3d APIs remain open.** The implemented demo composes catalog,
  term, row and key proofs with StateHeader binding for bounded complete Eq
  pages; every block header carries the state root. General point-read/proof APIs and
  other complete query profiles are separate work.
- [Engine storage measurements](engine-storage-v1-report.md) now cover
  10,000 publications, lazy opens, cache sizes, isolation and incremental
  writes. Paths are stored unpacked (one byte per nibble) for simplicity;
  nodes are not size-optimized. The engine's tests take about 2.5 times as
  long on the trie as they did on the AVL tree in debug builds and about 1.9
  times in release builds (see `docs/testing.md`).
- The reference oracle for keys over 32 bytes lives in the crate's tests;
  the engine's tests include it by path.
