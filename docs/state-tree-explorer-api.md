# State tree exploration API

`POST /signed/v1/state/explore` is an additive, read-only diagnostic API on a
signed full node. It opens one actual node in a retained snapshot. It does not
scan or materialize a whole tree, accept arbitrary object IDs, expose host-map
contents, mutate state, or claim that a light node has verified the response.

```json
{"height":"2","map":"terms","namespace":"1","path":[]}
```

`height` is required and pins every request. The default map is `catalog`.
Namespace maps `records`, `keys`, `rows`, `terms`, and `postings` require a decimal
namespace ordinal. `path` defaults to `[]` (root). Each subsequent integer
selects branch slot 0–15 or extension child 16. Returned child paths can be
submitted directly. Catalog leaf metadata discovers namespace IDs and the four
namespace map roots. A term leaf exposes its posting root/count; open that tree
with `map:"postings"`, `termPath` equal to the term leaf's path, and `path:[]`.
The posting root is derived from that reachable term; callers cannot supply it.

Responses contain chain `identity`, the pinned `snapshot`, `certificateBytes`,
`verification:{status:"exploratory",source:"full-node"}`, `stateComposition`,
selected `map`, and `node`. Composition includes the canonical encoded state
header, domain, state root, catalog root/count, and host root/count. The engine
recomputes the state root and checks it against the retained signed certificate.
Storage loads verify content hashes. These checks protect server-side integrity;
they are **not independent verification in the browser**. For that, use the
existing light-node `/signed/v1/query/range/inspect` proof-verification route.

A node contains actual `rlp`, `hash`, `kind`, `prefix`, `compressedPath`, complete
leaf `key`, `valueBytes`, and child references. Prefix/path strings are hexadecimal
nibbles without `0x`; byte strings have `0x`. Children distinguish `empty`, `hash`,
and `inline`; inline RLP is taken from the embedded reference. No unloaded leaf
values are invented. Empty maps return `node:null`; invalid routes fail, including
walking past a leaf. All integers potentially exceeding JS precision are decimal
strings. Node paths and slots are small JSON integers.

Decoded catalog leaves expose namespace name/owner and map roots/counts. Terms
expose attribute/type/value, existence-term flag, and posting root/count. Posting
and key leaves expose record IDs. Rows expose record ID/key and cells with name,
attribute/field kind, type and readable value. Strings show at most 256 characters;
bytes show at most 32 bytes, with `valueTruncated` and original `byteLength`.
Complete bytes remain available in the raw node. Raw record values stay opaque.

Bounds are structural: maximum path steps are twice the term key width (328 for
Arkiv v2; 176 for legacy), with each selected map enforcing its smaller key width.
A request traverses at most that root-relative path, the bounded catalog lookup,
and, for postings, one bounded term lookup. Empty/invalid paths never trigger a
scan. Node RLP plus leaf value are limited to 384 KiB before hex amplification;
serialized response is capped at 1 MiB. Existing request-size limits, four job
slots, and full-node storage locking apply. Oversized nodes fail explicitly.
Full state exploration may contend with other requests; it is a helper API,
not a bulk export or proof replacement.

Tests walk durable trees and validate every expanded hash/edge, inline children,
full leaf keys, catalog-to-map roots, term-to-posting roots, snapshot/state-header
consistency, historic empty roots, unknown height/namespace, bad routes, path
bounds, and rejection on light nodes.
