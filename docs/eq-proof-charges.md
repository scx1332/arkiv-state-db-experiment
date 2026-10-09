# Complete equality proof resource and charge rules

`arkiv_db::proof` implements binary proof profile `eq-page-v1` from
[the demo specification](full-light-demo.md). `Snapshot::prove_eq` produces
canonical proof bytes; `proof::verify_eq` requires the caller's exact
accepted block identity, original query, local genesis limits, local proof
caps and prior `VerifiedContinuation`. It never accesses an engine, loader,
RPC, wall clock or head. The host authenticates the selected header and
resolves its height/hash/root to one snapshot before invoking either API.

`VerifiedEq::NamespaceAbsent` is a proven namespace error, distinct from a
successful empty page. A successful page returns its decoded full rows and
an opaque `VerifiedContinuation` only when more rows remain. The token has
no public constructor or decoder; `cursor()` exposes its wire form for the
next request. The verifier accepts a wire continuation only alongside the
identical prior verified token. Page size may change.

## Counting rules and literal vectors

The new read API issues advisory receipts with `model_version = 4`.
Existing reads and every model-3 write receipt are unchanged. No operation
class or pricing schedule layout changes. The selected snapshot's committed
pricing prices all counts below.

| Operation class | Count per successful proof |
| --- | --- |
| DirectoryLookup | 1 |
| QueryGroup | 1 if the namespace exists, otherwise 0 |
| TermSeek | 1 if the namespace exists, otherwise 0 |
| PostingEntry | complete authenticated posting count, including ids before the cursor and after the page |
| RecordProbe | returned page rows |
| ResultRow | returned page rows |
| ReadByte | key bytes + each returned cell's name bytes and logical value bytes |
| ProofNode | visited catalog, term, row and key proof nodes, including inline nodes |
| ProofByte | exact complete binary response size, including the state header, identities, counts, posting ids, paths and optional next cursor |

All other classes have count zero. Ordinary positive Eq counts remain
`DirectoryLookup 1, QueryGroup 1, TermSeek 1`, with `RecordProbe 1` and
`ResultRow 1` per materialized row. This proof API replaces the ordinary
lazy posting scan with one charged full posting traversal; it does not add
a `TermVisit` charge to a point term lookup. Witness traversals have their
own `ProofNode` charges in addition to ordinary index/row work.

Literal fixture-price vectors, exercised in `tests/eq_proofs.rs`:

- Empty catalog / namespace absent: `ProofByte 497, DirectoryLookup 1`,
  total **498** units. There are no proof nodes. The 497 bytes consist of
  envelope 145, state header 348 and empty path count 4.
- Nine fixture rows with ids 4 and 8 holding U64(7), the other seven holding
  Str("book"), limit one: `DirectoryLookup 1, QueryGroup 1, TermSeek 1,
  PostingEntry 7, RecordProbe 1, ResultRow 1, ReadByte 40`. The selected row
  contains an eight-byte key, `kind`/`book` (4 + 4 bytes), and
  `secret`/`all cells included` (6 + 18 bytes). Before witness charges the
  cost is **62** units. Add `2 * visited proof nodes + binary response
  bytes` under the fixture schedule.

## Conservative capacity reservations

Before every proof iterator advance, the server checks remaining budget
for `ProofNode (2 * key_bytes + 1)` and `ProofByte (max_node_bytes + 4)`, and
checks the remaining response-byte and supplied-node capacity. Only actual
visited nodes and emitted bytes are charged. Fixed metadata maps reserve
1,024 bytes per possible node, which bounds both metadata leaves and the
532-byte largest branch. Row nodes reserve the larger of 1,024 and
`min(genesis.max_cells, 64) * (max(max_str_len, max_bytes_len, 32) + 44) + 128`.
All reservation arithmetic is checked.

Before a full row lookup the server reserves the same row-size capacity
for `ReadByte`, plus `RecordProbe 1` and `ResultRow 1`; it charges actual
logical row bytes after the bounded lookup. Every posting iterator advance
is preceded by `PostingEntry 1` and capacity for its eight-byte id. The
advertised posting count is capped before iterating or allocating the set.
Fixed framing is charged before emitting it. Empty tries need no node
reservation. Cancellation/errors discard all evidence accumulated so far.

Reservations are conservative operational admission, not additional
receipt charges. A caller can need a budget larger than the final receipt,
and a byte cap larger than the eventual response, to cover the possible
next node or row. The last iterator advance can reserve capacity even when
it produces no additional node. The API can therefore reject before the
final exact byte or cost cap would be exhausted. Failure returns a typed
error and no partial proof; rejected service attempts do not return an
advisory receipt in this API.

## Verifier bounds

Defaults are 4,096 posting ids, 64 rows (also bounded by genesis max_page),
8 MiB **binary** proof bytes, 8,192 supplied nodes and 128 MiB each of
conservative verification-work and cumulative allocation capacity. The
transport's separate 8 MiB HTTP/JSON cap includes hex expansion: a host
using hex must lower the binary cap, for example to
`(8 * 1024 * 1024 - 4096) / 2`, and still enforce actual transport bytes.

The decoder checks response bytes before parsing. It checks declared
sequence counts against both semantic caps and remaining payload before
reserving vectors. Paths borrow supplied node bytes, have catalog/term/row/
key bounds of 17/177/17/67, and strict trie traversal enforces these same
structural bounds including inline descendants. Node lengths are bounded
before walking, hashing or copying their values. The entire posting set
must be sorted, unique, nonzero, of the exact committed count and rebuild
to the committed posting root. A page's row sequence and next cursor are
derived from that complete set before row evidence is accepted.

The verifier charges one work unit per input byte initially, 1,024 per
possible visited path node, three per supplied node byte, and one per
row-leaf byte before row decoding. Each posting insertion reserves 17 *
1,024 work and allocation units before building its path. Vector storage,
possible terminal-value copies and row/cell materialization reserve
allocation capacity before allocation. These are conservative cumulative
bounds, not elapsed time or live heap measurements. A separate service
budget is never interpreted as proof of client verification cost.

Rows are decoded incrementally with at most min(genesis.max_cells, 64)
cells; variable value lengths are checked before allocation against the
caller's genesis limits. Sorted unique names, typed indexed attributes,
exact equality, and the row's record-key-v2 binding are required. No row is
returned until the entire proof, next cursor and end of input are valid.

These are complete equality pages relative to the authenticated index and
its execution invariant. A producer can sign an invalid index omitting an
otherwise matching row; this profile is not a proof of correct execution.
There is no persistence, pruning, broader expression proof or standalone
point-read API implied by this D2 subset.
