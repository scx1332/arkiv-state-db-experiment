# SDK 1.0 compatibility: open items

The node serves what `@arkiv-network/sdk` 1.0.x sends (see
[SDK compatibility](sdk-compatibility.md)): `arkiv_getEntity`, `limit`,
`offset`, `select.count`, `ORDER BY`, `BETWEEN` and ranges on every
indexable type. This page records what is still open: divergences from the
upstream node ([`arkiv` `docs/api-spec.md`](https://github.com/Arkiv-Network/arkiv)),
costs, and follow-ups. Nothing here blocks the SDK smoke.

## Ordering

- **No cursor with `ORDER BY`.** Sorted pages are offset windows, as
  upstream's are (its cursor encodes `offset + limit`). They carry no
  `cursor`, a `cursor` beside `ORDER BY` is refused (`-32005`), and a sorted
  window past the 8 MiB response cap is refused (`-32004`) rather than
  shortened, since nothing could carry the cut rows. Consistent sorted paging
  needs `atBlock`, which the SDK does not send. A keyset variant was built
  and set aside: the cursor was the last row's sort value and record id, and
  a page kept `limit + 1` matches after it. It can come back if sorted
  cursors are wanted.
- **Fetch-and-sort cost.** Every sorted page reads the key and sort cell of
  every match (a named projection) and keeps the first `offset + limit` in a
  heap, so its cost grows with the match set, not the page; it is bounded only
  by the 25-million-unit read budget (`-32004` past it, at once when posting
  sizes show more than 480,769 matches). For an indexed sort
  attribute held by every match (system attributes always are), an index walk
  over the term map in value order would read only the terms up to the window;
  golemdb's `docs/sort-implementation-plan.md` (phase 2, `IndexWalk`) has the
  design. `DESC` would need a reverse `terms` read in the engine.
- **Creation order inside one transaction.** Without `ORDER BY`, and for
  `ORDER BY` ties, matches come in record-id order: block, transaction,
  operation. Upstream orders by creating transaction, then entity key, so the
  two differ only among entities one transaction creates (an `executeBatch`
  with several creates). Record ids are committed state; matching upstream
  would mean allocating them in key order within a transaction, a consensus
  change.
- **`ORDER BY $createdAt`** is accepted here (as `$createdAt` predicates are);
  upstream answers a type error.

## Offset and count

- **Offset is a walk.** An offset reads the skipped matches' ids, in engine
  pages of 16,384 (not the genesis `max_page`), all charged to the read
  budget; one the budget cannot pay for is refused before the walk when
  posting sizes show it. Skipping by posting sizes or an order-statistics trie
  would make deep offsets cheap; nothing does that yet.
- **The read budget and the deadline measure different things.** The budget
  counts logical units; the 30-second deadline is wall time. On the
  16.8-million-entity Sourcify store a walk of the row map (a match-all or a
  negated query) costs about 67 µs a row, so the budget allows about 80 s of
  such a walk and the deadline ends it first (`ReadDeadlineExceeded`, the
  permit returned at the next charge). Since 0.1.0-alpha.7 a node-local
  offset cap, `--read-max-offset` (default 100,000, about 7 s at the 15,000
  matches a second measured on the Sourcify mirror), refuses deeper offsets
  at once; `0` turns it off and leaves the budget as the only bound.
- **A shortened unsorted page and SDK offsets.** An unsorted page whose
  entities pass the 8 MiB cap is shortened and continues by cursor. The SDK
  ignores cursors and computes the next offset as `n * size`, so it would skip
  the rows cut from such a page. Refusing instead of shortening when the
  request carries an `offset` would make the SDK see the problem.
- **Counts beyond the read bounds.** `select.count` of a query beyond the group
  and literal bounds counts by the bounded scan (at most 4,096 rows), as its
  page reads; `arkiv_getEntityCount` refuses the same query. An untyped
  `EXISTS(x)` (the SDK's `exists()`) expands to nine groups, past the genesis
  bound of eight, so it always takes the scan; an engine term for "an
  attribute of any type" would keep it on the index.
- **`select` defaults.** A `select` without `key` still returns the key unless
  it selects `count` alone; upstream turns every field off once `select` is
  given. The SDK always sends every key, so only hand-written clients see it.
- **`offset` beside a cursor** is ignored (the cursor carries its position);
  upstream leaves the combination unspecified and its implementation does the
  same.

## The SDK's own test suites

`arkiv-sdk-js` v1.0.1 (`e9fa8dd`, unmodified) runs its own suites against this
node through environment variables only: the CI "component" suite
(`test/src/arkiv.test.ts`: `ARKIV_SDK_TEST_RPC_URL`, `ARKIV_SDK_TEST_WS_URL`,
`ARKIV_SDK_TEST_CHAIN_ID`, `PRIVATE_KEY`) and the scheduled health suite
(`test/src/network-health.test.ts`: `RPC_URL`, `PRIVATE_KEY`), against one
loopback producer whose genesis funds the SDK CI's public test key.

Run of 2026-11-17, 1-second blocks, after `str` values took control
characters: unit 468/468, component 33/33, health 31/31. The reference node
(`ghcr.io/arkiv-network/arkiv-reth-dev:latest`) scored component 33/33 and
health 30/31 the same day.

- With 500 ms blocks two health tests are timing-fragile, and neither is a
  node fault. "an entity expires on its own" gives the entity five blocks,
  which pass before the SDK's receipt poll returns; it also failed on the
  reference node at 1-second blocks. "the chain reports its block timing"
  wants `duration > 0`, but `arkiv_getBlockTiming` reports whole seconds
  between the last two blocks, as upstream computes it, so it alternates
  0 and 1 on a sub-second chain.
- The suites are not part of this repository's CI. The helper scripts that
  start the node and run them live outside the repository on the development
  machine; moving them into `scripts/` is open.

## Supersets of the network

SDK 1.0 removed `ne()`/`!=`, `not()`/`NOT`, `atBlock`, cursors and
`getEntityCount()` because the network it targets answers at the head only
and has no negation. The node keeps all of them, plus `arkiv_getEntity`'s
optional block, `$createdAt` predicates and `limit` up to 200 (the SDK and
upstream stop at 100). Clients relying on them will not work against the
network; whether to keep, flag or drop each is open. SDK 1.0.1 maps only codes
`-32001` to `-32004` to `QueryError` kinds, so `-32005` (cursor) and `-32006`
(block) reach it as plain RPC errors.

## Language and parser

- **Two parsers.** `signed/rpc_reads/syntax.rs` has its own Boolean parser (to DNF,
  with `ORDER BY`), which hands each predicate to the adapted GPL copy of the
  upstream grammar in `simulator/query_language` (revision `0fd58ce5`). That
  copy gained `BETWEEN` and ranges on every type, modelled on upstream's
  `parse_between`; the `ORDER BY` clause is written by hand in `rpc_reads.rs`
  after upstream's `parse/order.rs` semantics, not copied. Upstream's
  `arkiv-query` crate is now MIT and has all of these; vendoring it would drop
  the GPL copy and the duplication.
- **Reserved words.** `between` is now a keyword in queries, but writes still
  accept it as an attribute name (a write rule is consensus), so such an
  attribute cannot be filtered on by name. The SDK also reserves `order`,
  `by`, `asc` and `desc`; the node reads them as the clause only where it
  stands.

## Expiry and permanence

SDK 1.0 removed `ExpirationTime.permanent()` and refuses lifetimes past its
`MAX_LIFETIME` (7,884,000 blocks) client-side. This node still admits
permanent entities and caps rent at a year (63,072,000 blocks at 500 ms), so
the SDK smoke no longer covers permanence or `PermanenceOwnerOnly`; the Rust
tests do. Whether chains should advertise the SDK's lifetime cap is open.

Creates sharing a lifetime no longer fail (issue #9): a height holds 128
expiring entities by default, and a write aiming at a full height is recorded
at the next height with room, up to 64 later
([entity expiration](entity-expiration.md), [proposal](proposals/expiry-spill.md)).
SDK 1.0.1 takes `expiresAt` from the `EntityCreated`/`ExpiryExtended` event
and its health suite compares it with the stored `$expiresAt`; it never
compares either with the lifetime it asked for, so a spilled expiry passes.
Open:

- An application that computes the expiry instead of reading it back can be
  up to 64 blocks early. No "fail instead of spill" flag exists.
- When every height of the window is full the transaction reverts; a replay
  through `eth_call` then carries an `Error(string)` reason, but if the state
  has moved on and the replay succeeds the SDK still reports "No reason
  provided by backend": a receipt has no reason field.
- The spill's reads are bounded (65) but not charged.
