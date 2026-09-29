# Source and adaptations

The lexer, parser, literal validation, limits, errors and conformance tests are
adapted from `Arkiv-Network/arkiv`, revision
`0fd58ce5bd6a0a196b0079f60d4679539346adc6`, `crates/arkiv-query/src`.
The minimal local AST is adapted from `crates/arkiv-interfaces/src/query.rs`
and `entity.rs`. The source workspace declares **GPL-3.0-or-later**;
its license terms are retained here (see LICENSE).

Local adaptations replace crate imports with a minimal local AST and `std`,
remove SDK RPC-code mapping, and expose classification via the sibling
`query_language.rs`. Grammar, literal rules, limits and upstream tests are
preserved. A test that compared internal index bytes now compares distinct
values, since this module does not implement the source index encoding.

Source grammar limits: 8192 UTF-8 bytes, 64 predicates, depth 32, attribute
names 32 bytes, indexed strings 128 UTF-8 bytes. Diagnostic offsets are UTF-8
byte offsets; equal start/end marks the source parser's error caret.

The six queryable system fields are `$key`, `$owner`, `$creator`, `$createdAt`,
`$expiresAt` and `$contentType`. The source rejects `$updatedAt` and
`$creationFlags` as projection-only and `$payload` as unindexed. The engine's
older explicit equality endpoint may expose additional committed fields;
this source-language endpoint deliberately preserves source grammar semantics.

Source system heights normalize internally to U256, but this engine stores
them as U64: the selection adapter restores U64. User U256 values remain U256.
No Boolean/DNF rewriting is performed. Only one typed equality (possibly
parenthesized) is eligible for the existing node equality endpoint and proof.
Valid range, prefix, Boolean and all-entity queries are explicitly classified
as unsupported execution/proof shapes. Reserved `!=`, `exists`, `typeof`
produce unsupported-language status with their original TypeError diagnostics.
Other parse failures are invalid-language status. Classification never promises
that a requested snapshot or runtime proof budget will succeed.
