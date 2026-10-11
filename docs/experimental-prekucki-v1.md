# Experimental: variable-length prefix-free trie keys (prekucki-v1)

Branch `experimental-prekucki-v1`. An experimental implementation of GitHub
issue #1 ("Support variable-length trie keys and remove term-value padding")
with a deliberately different trie design. `main` is unchanged.

## Summary

- The trie (`arkiv-db-trie`) accepts bounded variable-length keys. Instead
  of giving branches terminal values (the issue's section 2), every key
  encoding is **prefix-free**: no stored key is a strict prefix of another
  key of its map. Branch nodes, the 17-item RLP encoding, hashing, inline
  rules and proof shapes are unchanged; only the key-length assumptions
  went away.
- Term keys drop the 32-byte padded name, the 128-byte padded value and
  both length bytes. Raw record keys and record-key bindings are unpadded
  too. This is state layout 2; layout 1 stores, genesis files and proofs are
  rejected at open/decode, never misread.
- Indexed-string admission is separated from the representation: the
  default and Arkiv profile stays at 128 bytes, a generic instance may
  configure up to a 512-byte ceiling.

## Trie key contract (step 1)

`TrieKey::LEN` became `TrieKey::MAX_LEN`, a safety bound. Encodings must be
injective and canonical (decoders reject bad escapes, missing terminators,
trailing bytes, wrong fixed widths), order-preserving under unsigned
bytewise lexicographic order (a proper prefix first), and prefix-free.

- `MerkleMap::insert` rejects a key that is prefix-related to a stored key
  with the caller error `TrieError::KeyNotPrefixFree`, before changing
  anything. `get`, `remove`, `range_from`, point proofs and `walk` accept any
  byte string: a key ending inside a path or at a branch, or extending a
  leaf's key, is genuinely absent (branches hold no values). The verifier
  reports a key ending at a branch as the new `PathTerminal::KeyEndsAtBranch`.
- Corruption checks no longer compare a leaf with the requested key's
  length. A stored leaf must end on a byte boundary and within
  `K::MAX_LEN`; an inner node must leave room below the bound. Iteration
  decodes every stored key with `K::decode`.
- Complete interval proofs take a `KeyRange` of `Bound<Vec<u8>>` endpoints
  over raw encoded bytes (`Included`, `Excluded`, `Unbounded`) and
  `IntervalLimits { max_entries, max_key_len }`. Producer and verifier share
  one pruning rule, `KeyRange::may_hold(nibble prefix)`: a subtree is skipped
  only if its prefix differs from the lower endpoint at a smaller nibble, or
  its smallest completion (the prefix, plus a zero nibble if it ends inside
  a byte) is above the upper endpoint (or equal to an excluded one). A
  prefix run is `KeyRange::prefix(p) = [p, prefix_successor(p))`. Limit
  failures are errors, never truncated complete proofs. For the old
  fixed-width closed intervals the rule opens exactly the same children, so
  step 1 left every range witness byte-identical.
- `order` gained `encode_bytes`/`escape_bytes`/`decode_bytes` (generic
  escaped strings), `encode_shifted`/`shift_bytes`/`decode_shifted` (strings
  without `ff`, such as UTF-8), `prefix_successor` and `ByteKey<MAX>` (a
  bounded byte-string key, the empty string included). The padded
  fixed-width encoders were removed with their last user.

Step 1 kept every engine key encoding, so every existing root, golden vector
and proof fixture was unchanged; the full suite passed without touching an
expected root.

## Encodings (step 2)

| Map | Layout 1 | Layout 2 |
| --- | --- | --- |
| terms | `name pad 32 ‖ name_len ‖ tag ‖ presence ‖ value pad 128 ‖ value_len` (164) | `bytes⁰(name) ‖ tag ‖ 00` or `bytes⁰(name) ‖ tag ‖ 01 ‖ value` |
| records (raw) | `key pad 20 ‖ len` (21) | `bytes⁰(key)` (1–41) |
| keys | `len ‖ key pad 32` (33) | `len ‖ key` (2–33) |
| catalog, rows, postings, host | fixed width | unchanged |

- `bytes⁰` (`order::encode_bytes`): `00 → 01 01`, `01 → 01 02`, other bytes
  as is, then a `00` terminator. Names never contain `00`/`01` (the name
  grammar), so `bytes⁰(name)` is the name plus `00`.
- `text⁺` (`order::encode_shifted`) for `Str` term values: every UTF-8 byte
  plus one, then `00`. UTF-8 never holds `f5`–`ff`, so the shift cannot
  overflow; the result is exactly `len + 1` bytes.
- Scalar term values keep their fixed-width order-preserving term bytes
  (`Value::term_bytes`). Within one `(name, tag)` they have one width, so
  they are prefix-free; `bool`, the integers, decimals, floats (IEEE total
  order form), time types, `addr`, `bytes4/8/16/32` and `key` are all fixed
  width. `Bytes` is never indexed.
- Existence and value keys differ at the presence byte, `bytes⁰` is
  prefix-free, and each value part is prefix-free within its tag, so the
  whole term key set is prefix-free. Every part preserves order, so each
  `(name, tag)` is still one contiguous run, existence first, values in
  their previous order.

Examples: `c = "red"` is `63 00 02 01 73 66 65 00` (8 bytes, was 164);
`n = 1i64` is `6e 00 11 01 80 00 00 00 00 00 00 01` (12 bytes, was 164); the
existence term of `n: i64` is `6e 00 11 00` (4 bytes). Under the default
limits the longest term key is 33 + 2 + 129 = 164 bytes, the old fixed
width; at the 512-byte ceiling it is 548 bytes (`TermKey::MAX_LEN`).

### Queries and proofs

- Equality: the term key of the value, unchanged API.
- Range (`range-complete-v1`): the interval is now expressed with the
  query's own inclusivity instead of fixed-width successor/predecessor
  arithmetic. Within the value run `P = bytes⁰(name) ‖ tag ‖ 01`: lower is
  `Included(P)` or the bound's term key with its inclusivity; upper is
  `Excluded(prefix_successor(P))` or the bound's term key with its
  inclusivity. Order preservation and prefix-freeness make an exclusive
  endpoint exclude exactly that value. The response envelope version is
  `RANGE_PROOF_VERSION = LAYOUT_VERSION` (2); the request encoding
  (`RANGE_VERSION = 1`) is unchanged.
- `starts_with(s)`: the run of `Str` values starting with `s` is
  `[P ‖ shift(s), prefix_successor(P ‖ shift(s)))` (`TermKey::str_prefix_run`,
  checked against every tricky string in the `layout.rs` tests).
  `shift(s)` without the terminator is a prefix of `text⁺(t)` exactly when
  `t` starts with `s`, because the shift is a byte-wise bijection. The
  engine's run iteration still seeks to `text⁺(s)` (the smallest key of the
  run, since `00` is the smallest byte) and stops at the first value that
  does not start with `s`.
- Walk bounds use the instance's limits: `TermKey::max_len_for(limits)`
  (164 for the defaults) bounds term-path visits (`2L + 1` = 329), interval
  walks and verifier reservations; `TrieKey::MAX_LEN` (548) is the static
  bound for corrupt-node checks.

## Admission limits (issue section 6)

- `Value::MAX_INDEXED_LEN` (128, "fixed by the key width") was removed.
  `Limits::DEFAULT_INDEXED_LEN = 128` is the explicit default and
  Arkiv-facing policy; `Value::INDEXED_LEN_CEILING = 512` is the hard
  representation/resource ceiling (`Limits::validate`).
- `Limits::indexed_value_len() = min(max_indexed_len, max_str_len, ceiling)`
  applies to writes, equality values, range bounds, prefixes and term-seek
  bounds alike, counting UTF-8 bytes. Limits are persisted in the durable
  genesis, so a different admitted maximum is part of the instance identity.
- Why 512: a worst-case term leaf (548-byte key path, 40-byte posting
  handle, RLP framing) stays below the 1 KiB proof-node limit
  (`META_NODE_BYTES`) of equality and range proofs, so a long-string
  generic instance can still serve proofs.
- Metering and gas: the meter charges logical units (term visits, value
  bytes, nodes visited), not key bytes, so no schedule changed and
  `MODEL_VERSION` stays 3. GLM gas charges logical occupancy
  (`INDEX_OVERHEAD_UNITS = 128` per attribute is a policy constant, not a
  key width). Node-visit counts (`PhysicalTrace`, `ProofNode` charges of
  proof serving) follow trie shape, which changes with the encoding; no
  receipt literal in the suite changed. The `trace_regression.rs` fixture
  value `rose` became `rise`, because shifted `o` (0x70) would have moved it
  under `ruby`'s branch; `rise` keeps the hand-derived shape. Proof-serving
  capacity pre-checks, the term-path supply bound (`2L + 1` = 329, was 177
  derived for layout 1) and verifier reservations derive from
  `TermKey::max_len_for(limits)`, which is 164 for the default limits, the
  old fixed width.
- Explorer: `MAX_TREE_PATH` is `2 · TermKey::MAX_LEN` = 1,096 route steps
  (was 328); the range inspection JSON reports term-key endpoints with
  `lowerInclusive`/`upperInclusive`.

## Versions and compatibility

No backward compatibility (repository rule): fresh stores only.

- `LAYOUT_VERSION` 1 → 2, and with it `STATE_FORMAT` and `PROOF_VERSION`.
  The durable genesis records these versions, so opening a layout-1 store
  fails with `UnsupportedVersion` before any state is read.
- `CODEC_VERSION` stays 1: no codec layout changed (it was an alias of the
  layout version before and is now independent).
- The range proof response envelope carries `LAYOUT_VERSION` (2).

## Why not the issue's design

- **Branch terminal values (issue section 2).** Not needed: making every
  map's key set prefix-free removes the only case a terminal would serve. It
  keeps node encoding, canonical normalization, nested-value staging,
  reachability and proof shapes exactly as they were, so the change is
  confined to length assumptions. The cost is one terminator byte per
  variable-length field.
- **Unterminated values after a fixed header (issue section 4).** The issue
  keeps the 34-byte padded name header and appends raw value bytes, which
  needs terminals because `"a"` is a prefix of `"aa"`. Here values are
  terminated instead, and the name is compacted in the same step.
- **FoundationDB tuple escaping.** The suggested `00 → 00 ff`, `00`
  terminator form is self-delimiting only with lookahead: `61 00` (`"a"`)
  is a strict prefix of `61 00 ff 00` (`"a\0"`), so as the last field of a
  key it is not prefix-free. `bytes⁰` uses `01` as the escape byte so the
  terminator `00` never appears inside a field; `text⁺` avoids escapes for
  UTF-8 entirely, which keeps the worst-case term key at the old 164 bytes
  for the default profile instead of doubling it for `\0`-heavy strings.
- **Hashed group header (issue section 7).** Not done. It would save two
  bytes over a padded-name header and nothing over the compact
  `bytes⁰(name)` used here (a typical name is shorter than 32 bytes); it is
  irreversible, so iteration, typed validation, proofs and the explorer
  would need committed name/type metadata (in the entry or an authenticated
  dictionary) with its own storage and proof cost; and it would change group
  order. Prefix-free name compaction already removes more bytes than the
  hash would.

## Not converted

Every padded key was converted (terms, raw record keys, record-key
bindings). Fixed-width keys (`NamespaceId`,
`RecordId`, the 32-byte host keys) were never padded and are unchanged.

## Evidence

See `docs/state-trie.md` (contract, verifier, intervals),
`crates/arkiv-db-trie/tests/variable_keys.rs` (BTreeMap differential tests
with variable keys), the `layout.rs` unit tests (order, prefix-freeness,
golden vectors, malformed keys) and `crates/arkiv-db/tests/prefix_free_terms.rs`
(engine-level queries and proofs). The independent root models in
`crates/arkiv-db/tests/roots.rs` and `crates/arkiv-node/tests/support/block_roots.rs`
(200-block continuous demo) were rewritten for layout 2 from the byte
layout alone and match the engine. Benchmarks:
`docs/benchmarks/2026-10-11-prekucki-v1.md`.
