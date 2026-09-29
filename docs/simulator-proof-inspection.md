# Verified simulator proof inspection

`POST /sim/v1/query/inspect` is a bounded, read-only light-node endpoint.
It accepts precisely the selection used by `/sim/v1/query/verified`:

```json
{"height":"20","namespace":"1","attribute":"group","valueType":"u64","value":"1","limit":2,"cursor":null}
```

The light node resolves the selected historical header from its own durable
header catalog, fetches the proof from its configured peer's public `/query/eq`,
and uses the **complete Eq verifier**. The endpoint returns nothing but an
error if the witness, query binding, posting completeness, row/key binding,
continuation, resource limits, or local header recheck fails. Namespace absence
keeps the existing `NamespaceNotFound` error contract. An absent term returns
an authenticated empty page, including its actual absence path.

Success preserves all verified-page fields: `verification`, source/run/genesis/
chain identity, `snapshot`, `query`, metadata `rows`, `postingCount`,
`continuation`, and transport `diagnostics`. It adds `canonicalRequest` and
`canonicalProof` as lowercase `0x`-prefixed canonical bytes, plus `inspection`.
The ordinary `/query/verified` response remains unchanged.

The source is still an **unsigned development simulator**. Verification binds
the result to the selected locally accepted simulator header, not an external
consensus or signature. The diagram describes the server's successful verifier
walk. Rendering a diagram does not independently verify it in the browser.

## Inspection version 1

Every hash, root, byte key, `rlp`, and `*Bytes` encoding is lowercase
`0x`-prefixed hex **except** `valueBytes`, which is a decimal byte count.
`compressedPath`, `keyNibbles`, `selectedNibble`, and branch `slot` contain
lowercase nibble text without a prefix. Identifiers and potentially large
counts are decimal strings. Small indices, `count`, and node counts are JSON
numbers.

```typescript
type Handle = { root: string; entries: string };
type Child =
  | { slot: string | null; kind: "empty" }
  | { slot: string | null; kind: "hash"; hash: string }
  | { slot: string | null; kind: "inline"; rlp: string };
type Inspection = {
  version: 1;
  proofProfile: "eq-page-v1";
  queryDigest: string;
  stateComposition: {
    stateRoot: string;
    domain: "arkiv/state/v1";
    headerBytes: string;
    catalog: Handle;
    host: Handle;
    nextNamespace: string;
    pricingBytes: string;
    namespace: null | {
      id: string;
      nextRecord: string;
      records: Handle;
      keys: Handle;
      rows: Handle;
      terms: Handle;
    };
  };
  pointPaths: Array<{
    id: string; // catalog, terms, rows:<id>, keys:<id>
    map: "catalog" | "terms" | "rows" | "keys";
    namespaceId: string | null; // catalog uses null
    recordId: string | null;
    root: string;
    key: string;
    keyNibbles: string;
    suppliedNodeCount: number;
    terminal: {
      kind: "inclusion" | "absence";
      reason: "leaf-match" | "empty-root" | "divergent-leaf"
        | "divergent-extension" | "empty-branch-slot";
    };
    nodes: Array<{
      index: number;
      proofIndex: number; // index in this point path's supplied wire nodes
      depth: number; // consumed key nibbles before this node
      source: "root" | "hash" | "inline";
      hash: string; // keccak256(rlp), even for inline nodes
      rlp: string; // exact RLP from this witness
      kind: "branch" | "extension" | "leaf";
      compressedPath: string;
      selectedNibble: string | null;
      children: Child[];
      valueBytes: string;
    }>;
  }>;
  postingSet: null | {
    method: "complete-set-reconstruction";
    authenticatedRoot: string | null;
    reconstructedRoot: string;
    count: number;
    recordIds: string[];
    selectedRecordIds: string[];
    termPresent: boolean;
  };
};
```

`nodes` contains only nodes actually visited by the strict trie verifier,
including inline children. A root is always checked by hash, including a root
shorter than 32 bytes. Inline nodes retain their containing wire-node index;
they are not extra supplied proof nodes. `children` has exactly sixteen slots
for a branch, one child with `slot: null` for an extension, and no entries for a
leaf. Branch siblings are authenticated commitments; they are **not traversed
subtrees**. An empty root has no nodes. No graph nodes are inferred or invented
for an unvisited subtree.

The state root is `digest("arkiv/state/v1", encode(StateHeader))`, not the root
of one giant trie. The state header commits to the catalog and host handles,
namespace allocator, and pricing. The proven catalog leaf commits to the
namespace's raw records, key bindings, rows, and terms. No point paths are
claimed for the host or raw-record maps by this proof profile.

The term leaf commits to a posting root and count. Posting completeness uses
**all sorted posting IDs**, rebuilding the canonical Patricia map and comparing
its root with that commitment. This is separate from the catalog/term/row/key
point paths. When the term is absent, `authenticatedRoot` is null, the rebuilt
set is empty, and `termPresent` is false. Selected IDs identify the current
page, not the entire posting set. Actual row and key-binding paths are included
for every returned row.

## Bounds and exposure

Inspection shares the normal 64 KiB request cap, four-request semaphore,
nonblocking node lock, configured-peer URL policy, five-second fetch timeout,
header recheck, cursor cache, and error envelope. It admits at most 1 MiB of
canonical witness, uses the normal 8 MiB hard serialized-response cap, and
charges inspection allocation and work to the verifier's existing resource
meter. Posting IDs remain capped at 4096 and page rows at 64. It never forwards
incoming cookies or Authorization headers. Full nodes return `UnsupportedProof`
for this light-only endpoint.

The canonical proof and graph RLP intentionally contain complete proven row
values, as `/query/eq` already does. These are separate from the payload-free
explorer feed and PostgreSQL projection. The endpoint exposes no canonical
transaction bodies, private controls, arbitrary peer selection, or unbounded
whole-tree dump.

## Reproducible examples

The default workload (seed 7, no extra rows) has these exact historical samples,
all for namespace `1`, attribute `group`:

| Height | Type | Value | Posting count | Demonstration |
| --- | --- | --- | --- | --- |
| 20 | u64 | `1` | 5 | Inclusion and pagination; use limit 1 or 2 |
| 20 | u64 | `999999` | 0 | Authenticated term absence |
| 10 | str | `1` | 1 | String equality is distinct from integer equality |
| 11 | str | `1` | 0 | Previous string term removed by a patch |
| 13 | u64 | `1` | 2 | Includes a record before expiry |
| 14 | u64 | `1` | 1 | Expired record removed from the selected state |

Produce actual, fully verified sample JSON without touching a stored run:

```bash
cargo run -q -p arkiv-node --example inspect_eq -- 20 u64 1 2
cargo run -q -p arkiv-node --example inspect_eq -- 20 u64 999999 2
cargo run -q -p arkiv-node --example inspect_eq -- 10 str 1 2
```

The example uses fixed in-memory simulator identity and genesis. Its output is
the core inspection response; HTTP adds actual fetch/verification timings in
`diagnostics`. Sample bytes from this fixture are not a deployed run's bytes:
source/run/genesis/header identity must always be taken from that live run.

## Offline independent verification

Save four JSON files: the light's inspection response, `/sim/v1/genesis` from
the independently selected full node, the selected full node's
`/sim/v1/feed/blocks/<height>` (or just its `header` object), and the original
selection posted to the light. Then run:

```bash
cargo run -q -p arkiv-node --example verify_inspection -- \
  response.json trusted-genesis.json trusted-header-or-feed.json selection.json
```

This bounded tool derives the request from the separately supplied identity,
header and original query. It verifies the exact canonical witness with the
ordinary stateless verifier and also checks the displayed rows/count/snapshot.
It ignores the endpoint's claimed verification label and inspection diagram.
It prints a compact success summary, or exits unsuccessfully on any mismatch.
Editing one byte of `canonicalProof` in `response.json` must make it fail.
Only first pages are supported: later pages require a local previously verified
continuation token. Supplying a header here chooses a trust anchor; this tool
does not turn an unsigned simulator header into a consensus-authenticated one.
