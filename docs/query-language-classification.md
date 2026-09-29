# Query language and proof classification

The simulator accepts the Arkiv node's text language without changing its storage
or proof design. Parsing an expression does not promise execution or a proof.
The PostgreSQL explorer query implementation is independent and unchanged.

`POST /sim/v1/query/classify` accepts `{ "query": "group = u64(1)" }` on full
and light nodes. Its result separates:

- `language`: valid, invalid, or a reserved unsupported language feature, with
  diagnostics and UTF-8 byte offsets.
- `execution`: whether this simulator's text query endpoint can execute this
  expression, not whether some lower-level engine primitive exists.
- `proof`: whether the expression fits the existing proof shape. Supported means
  eligible, subject to runtime limits; it never means verified.
- `selection`: the exact typed Eq request when eligible, plus shape and notes.

The grammar includes typed literals, `AND`, `OR`, `NOT`, numeric comparisons,
`STARTSWITH`, parentheses, comments and standalone `*`. Untagged numbers mean
`i32`, not `u64`. Therefore the demo's group query is `group = u64(1)`.
Boolean keywords are case-insensitive; attribute names are case-sensitive.
`NOT` is the complement of the entire predicate, including missing attributes
and different types. No DNF conversion is performed.

For this release, only one equality predicate (with optional grouping) executes
through the text API and fits the existing Eq page proof. Valid range, prefix,
Boolean and all-entity expressions receive explicit unsupported execution and
proof classifications. We do not execute a partial expression, rewrite a typed
literal to another type, or add general-purpose proof machinery.

`POST /sim/v1/query/text` is a light-node endpoint. Send `query`, explicit
`requireProof`, `height`, `namespace`, `limit` and optional `cursor`. Snapshot,
namespace and pagination remain outside the expression and retain the existing
Eq API constraints. A successful response has the existing inspected verified
page plus `classification`, `queryText` and `verificationStatus.status = "verified"`.
Verification happens in the hosted Rust light process, not browser JavaScript.

When `requireProof` is true, an unsupported proof shape is rejected as
`UnsupportedProof`. When false, unsupported execution is rejected as
`UnsupportedQueryExecution`. False permits a proof; it does not request an
unverified fallback. Both modes currently return verified Eq pages only.
The full node classifies queries but rejects text execution as
`UnsupportedNodeRole`; raw Eq proof production remains its existing API.

Errors include classification and an actual verification outcome:
`not_executed`, `resource_limit` for `ProofLimitExceeded`/`BudgetExceeded`, or
`failed` for a failed request/verification. None returns a successful proof
badge or unverified replacement rows. Invalid language returns
`QueryLanguageError`. The original typed Eq endpoints remain available.

The parser is pinned to the current reference node grammar, not every SDK
helper. In particular `$updatedAt` and `$creationFlags` are projection-only in
that grammar and `$payload` is not queryable, even though the playground has
additional internal indexed fields exposed by the older typed debug controls.
This deliberate distinction is reported by the classifier, not silently
expanded into an incompatible text language.
