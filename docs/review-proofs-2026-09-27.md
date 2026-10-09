# Database and query-proof design review — 27 September 2026

> Historical review of 2026-09-27. At that time the node had an `arkiv-v2`
> cargo feature, a generic default build and several gas profiles; the commands
> and profile statements below describe that code. The node now has one build
> (no feature flags) and `glm-v2`-only signed chains.

Reviewed source baseline: `7f80db66bd68d3be135e1bd387ca4979e5387743`.
Scope: authenticated database layout, index maintenance, equality-page proofs,
complete numeric-range proofs, their verifier boundaries and resource policies.
This is a source and regression-test review, not a formal security audit or a
proof that no vulnerabilities remain. Signed-header acceptance, deployment and
frontend behavior are reviewed separately.

## Assessment

No confirmed correctness or proof-forgery defect was found in the reviewed
core paths. All **79 selected tests passed**. Existing adversarial tests cover
missing proof nodes, omitted terms/postings/rows, substituted rows and key
bindings, forged continuations, malformed trie nodes, wrong snapshots, trailing
bytes and every advertised resource-cap category.

The most important design boundary is deliberate: a verified result is complete
relative to an authenticated index. The proof does not establish that the
producer executed every transaction correctly or built that index correctly.
Returned rows are checked against their index terms, but an entirely omitted
index membership cannot be detected without an execution guarantee or checking
all rows. This is documented in `proof/range.rs::verify_range` and the proof
design documents. Presenting this prototype as trustless public-chain consensus
or execution verification would be incorrect.

## How the database is organized

Think of the state root as the fingerprint of a filing system at one completed
block. The fingerprint commits to a namespace directory and host state. Each
namespace has a row table, a key-to-row directory and a searchable index.
Each index entry is `(attribute name, exact type, value)` and points to a complete
ordered set of record IDs, called a posting set.

For example, an index entry for `price:u64=10` can identify records 4 and 9.
The row table holds their actual cells; the key directory associates their
external entity keys with those record IDs. Proofs authenticate all three
relationships, rather than accepting IDs or values supplied by a server.

Sources: `crates/arkiv-db/src/state.rs::StateHeader`,
`crates/arkiv-db/src/layout.rs` map-layout table and `TermKey`,
`crates/arkiv-db/src/typed.rs::terms_of`.

State roots depend on logical content, not memory addresses, cache state or disk
locations. Rows, keys and indexes use persistent Merkle Patricia tries. Updating
a branch builds changed paths; existing snapshots retain their prior roots.
Independent root-model tests confirm identical content gives the same root and
logical-field changes change the commitment.

## A write updates the whole relationship

Create inserts the row, its external key and its attribute memberships.
Patch calculates old and new memberships for changed cells, removing old values
and inserting new ones. Turning an attribute into a field removes its index
memberships; turning a field into an attribute adds them. Delete removes every
membership, the row and its key. Empty posting sets remove the index term.

These functions build replacement state and return it only after success.
Rollback tests verify rows, keys, indexes and record allocation move together.
The `is_consistent` test aid independently reconstructs expected indexes from
rows; it is not an automatic full-table scan performed by every light query.

Sources: `crates/arkiv-db/src/typed.rs::create`, `patch`, `delete`,
`remove_member`, `is_consistent`; `crates/arkiv-db/tests/records.rs`.

## What equality proves

For `price = 10`, the verifier checks the accepted snapshot and query identity,
opens the namespace and exact typed term, and rebuilds the entire posting trie
from the supplied sorted IDs. The rebuilt root must match the authenticated
term. This detects omissions that ordinary row-membership proofs would miss.
Only the selected page's rows are then opened, checked for the exact attribute
and value, and bound back to their external keys.

Later pages require a continuation previously issued by the verifier. A caller
cannot introduce a fabricated first cursor to skip early results. Changing page
size is permitted while retaining the same query and snapshot.

Sources: `crates/arkiv-db/src/proof.rs::verify_eq_inner`, `page_slice`,
`decode_row`; `crates/arkiv-db/tests/eq_proofs.rs`.

## What a complete range proves

For prices **9, 10, 15, 20, 21**, `10 ≤ price < 20` returns **10 and 15**.
That answer alone is insufficient evidence: a dishonest server could hide 15.
The implemented proof checks five connected things:

1. The expected snapshot's state root commits to this namespace and its index.
2. An interval witness opens every index subtree that intersects the typed
   numeric interval. A missing intersecting child is an error, not an empty gap.
3. Every matching term's entire posting set is rebuilt and compared with its
   authenticated root. Duplicate IDs across terms are rejected.
4. Every selected row is opened under the same snapshot and must contain the
   exact indexed attribute/value that selected it.
5. Every external key must resolve back to that record ID. Extra/truncated proof
   material and mismatched query/snapshot identities are rejected.

Out-of-range subtrees can remain opaque hashes. The generator uses the term
index, then selected postings and rows; it does not scan all rows to find
matches. Storage-isolation tests make unrelated term/row nodes unavailable and
still generate the same valid proof.

Sources: `crates/arkiv-db-trie/src/interval.rs::walk_interval`,
`IntervalProofIter`; `crates/arkiv-db/src/proof/range.rs::generate_range`,
`verify_range`, `terms`; `crates/arkiv-db/tests/range_proofs.rs`.

## Supported semantics and deliberate limits

| Topic | Actual contract |
| --- | --- |
| Snapshot | Exactly one selected immutable state; later updates do not rewrite the answer. |
| Matching | Indexed attribute, exact numeric type; fields, missing attributes and other types do not match. |
| Numeric types | Engine: i64/u64; arkiv-v2 adds i32/u256/dec. Signed HTTP profile: u64/i32/u256/dec. |
| Bounds | Inclusive, exclusive or one unbounded end; at least one bound required. Reversed/equal-excluded intervals are empty. |
| Range output | Full rows in record-ID order; no pagination, cursor, offset or silent truncation. |
| Initial range policy | At most 64 distinct matching terms and 64 rows; proof bytes, nodes, work and allocations are also bounded. |
| Empty answer | An existing namespace with no matches has a valid empty proof. Namespace absence is distinct. |
| General query language | Ordinary queries support more expressions; the dedicated proof APIs do not prove arbitrary AND/OR, negation, substring/string ranges, sorting or aggregates. |
| Failure behavior | Oversized/unavailable/malformed proofs fail the request; they are not successful partial or empty results. |

Equality pages also reconstruct the complete matching posting set on each page,
so paging bounds row materialization but does not eliminate whole-posting work.
This is a scalability tradeoff, not a completeness bug. Current default equality
posting admission is 4096 IDs. Large payloads can hit byte/work caps before row
count caps. Raising caps is a local policy change that requires capacity review.

## Findings and changes

**Low severity — documentation drift, fixed.** `docs/range-proofs.md` said the
frontend was unchanged after runnable examples and explanations had been added.
It now identifies `/queries` and `/range-explained`. The ordinary-query section
of `docs/single-block-query-proofs.md` now distinguishes absence of a general
`proof` option from the implemented dedicated equality/range APIs.

**Low severity — runtime description drift, fixed.** `docs/architecture.md`
described all node runtime durability as future work. Its scope and crate
description now distinguish the volatile original demo from implemented durable
simulator/signed runtimes, and label historical N4 planning references.

**Informational — accepted-root and index-consistency assumptions.** The engine
verifier intentionally accepts an expected snapshot supplied by its caller.
That caller must independently accept the root and chain identity. No engine
proof alone authenticates a producer or validates a chain. No runtime code was
changed: the review did not establish a localized core defect warranting one.

## Validation performed

Commands run from this repository, all passing:

```sh
cargo test -p arkiv-db --features arkiv-v2 --test range_proofs --test eq_proofs
# 7 range + 14 equality tests
cargo test -p arkiv-db-trie --test interval_security --test interval_model --test proofs
# 2 security + 2 interval oracle + 12 point-proof tests
cargo test -p arkiv-db --features arkiv-v2 --test records --test query_history --test roots
# 30 record/index + 9 historical query/model + 3 independent-root tests
```

The trie point tests include intentionally vulnerable behavior of the upstream
generic `alloy-trie` verifier as contrast fixtures; this project uses its own
strict walker. Their names are not reports of successful attacks on the project
verifier. Source review confirmed the dedicated proof paths call the strict
project walkers.

Remaining review limits: no new long-running fuzz campaign, formal proof,
production-scale resource benchmark or external consensus integration audit was
performed in this pass. Passing adversarial regressions is useful evidence,
not a guarantee against all future malformed inputs or execution bugs.
