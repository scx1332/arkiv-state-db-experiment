# GLM gas schedule

Every signed native chain runs the `glm-v2` gas profile: this gas schedule plus
[GLM accounts](glm-accounts-v1.md), which debit the fees from funded balances
and burn them. This document describes the schedule: how transaction execution
and entity occupancy are priced in gas, under the capped rent rule
`glm-rent-v2`.

Gas is priced at a fixed **1 gwei**, meaning 1,000,000,000 GLM base units per
gas, with 18 decimal places per GLM. There is no fee market, EIP-1559 base fee
or miner tip.

The default block limit is **100,000,000 gas**, with **90,000,000 gas** available
to declared user transaction budgets; the rest protects expiry processing.
The existing transaction-count, operation-count and encoded-byte limits also
apply. The gas limit is not a throughput promise, and the existing 16-transaction
cap can bind before gas does. 50–200 million gas is the intended experimental
block range, subject to validating the expiry reservation.

## Schedule identity

Schedule ID `0x474c4d01` (`GLM` + version 1) commits the exact 31 engine prices in
genesis. The node rejects different weights under this ID. Host intrinsic and
rent rules are part of this named schedule, in addition to engine metering model
3. Admin pricing changes cannot enter, replace, or alter it
(`AdminRejection::GasProfileLocked`); a signed genesis without it is invalid.
`tx_budget` is the transaction gas limit. Receipts, outcomes and signed headers
commit the charged units through the outcome commitment.

The status response advertises `capabilities.gas` (profile `glm-v2`, exact
limits, gas price, `rentRule: "glm-rent-v2"`, `maxRentLifetimeBlocks` and its
nominal `maxRentLifetimeSeconds`, `permanentEntitiesSupported: true`, the
`permanentEntities` rules and `feesDebited: true`).

## Storage units and rent

For the complete committed entity, including its system attributes:

```
units = 512
      + sum(cell name UTF-8 bytes + value encoded bytes + 2 tags)
      + 128 × indexed cell count

horizon(E, h) = min(E, h + cap)          # cap = max_rent_lifetime blocks
create rent   = ceil(units × (horizon(E, h) − h) × 8 / 17280)
```

Every cell's data is storage, whatever its name or type: a 1 KiB `bytes`
attribute costs its name, 1,024 value bytes and two tags like any other cell.
Only indexed cells (`CellKind::Attribute`: the system cells except
`$creationFlags`, and user attributes written with the `0x80` indexed flag)
add the 128-unit posting reservation; an unindexed attribute, and every
`bytes` value, pays storage only. Under the user-defined-columns model no name
is special-cased (the former `$payload` exemption from name and tag bytes is
gone with the payload itself).

The 512-unit base reserves entity/key and fixed bookkeeping overhead. Each
128-unit indexed-cell allowance covers its value and typed-presence posting
contribution together. These are **logical capacity coefficients**, not claims
about heap allocations, database disk bytes, or a measured storage-provider
price. They avoid charging differently according to shared prefixes, term
sharing, cache state or insertion order. Creation of a new shared term has
zero additional price; its ordinary index/root work is still priced.

17,280 blocks is one day **only at a five-second block period**. The consensus
formula uses block heights, not elapsed wall time. Changing the block cadence
changes the real duration bought by the same charge.

### The rent cap

Rent is never charged past `h + cap` blocks from the current height `h`. The
cap is the consensus parameter `ChainParams::max_rent_lifetime` (blocks) in the
genesis; authoring profiles give it as `max_rent_lifetime_seconds`, converted
to whole blocks (rounded up) at the chain's block period. The default is one
year: `31,536,000` seconds, which is **15,768,000 blocks at 2-second blocks**
(6,307,200 at 5-second blocks). Every lifetime of at least `cap` blocks, a
finite one far in the future or the permanent `u64::MAX`, costs the same;
shorter lifetimes are priced exactly as before. Lifetimes have no upper bound:
`max_lifetime` no longer applies to Arkiv entities (see
[block format](block-format.md)), so no expiry is ever `TooFar`.

With `E_old` the expiry before and `E_new` after an operation at height `h`:

- **Create** prepays `units × (horizon(E_new, h) − h)`: at most the cap.
- **Growth** (a patch that enlarges the entity) prepays the positive size
  increase for the capped remaining lifetime, `growth × (horizon(E_old, h) − h)`;
  for a permanent entity that is always a full cap. Same-sized changes pay
  execution but no additional occupancy rent.
- **Extension** prepays the current size for the growth of the capped horizon
  only, `units × (horizon(E_new, h) − horizon(E_old, h))`. Extending an entity
  whose expiry already reaches `h + cap` costs no rent.
- **Ownership transfer** changes neither size nor expiry: execution gas only.
- Shrinking or deleting gives no refund. Regrowing pays for the renewed increase.

**Time passing.** The horizon is recomputed from the current height on every
operation; nothing like a "paid through" height is stored. An entity created
permanent at `h0` pays `cap` blocks; once `h0 + cap` passes it simply stays,
with no further payment (the user-chosen rule: permanent costs the same as one
cap). Extending it again is a no-op success (`u64::MAX` is the only expiry
that does not shorten it, and its horizon cannot grow), and a finite expiry is
refused with `ExpiryShortened`. A finite entity may be extended to permanent:
it pays the capped horizon increase.

**No discount on permanence.** Whatever the sequence of creates and
extensions, an entity whose expiry reaches `E` has paid at least
`min(E − h0, cap)` blocks of its size: while every expiry stays below
`h + cap` the payments telescope exactly to `E − h0`, and the step that first
reaches `h + cap` brings the total to at least `cap`. Permanence therefore never
costs less than one cap at creation, and splitting it into short extensions
only costs more (`gas.rs` tests this exhaustively on small values). Growth is
always paid for a full capped lifetime, so a small entity made permanent and
then enlarged pays for the enlargement too.

**Overflow.** `h + cap` saturates at `u64::MAX`; unit-block products use a
128-bit intermediate; a quote that does not fit `u64` gas is rejected
(`GasOverflow`). `u64::MAX` expiries are never scheduled for expiry, never
counted against the per-height expiry capacity and never swept.

**Budget.** A transaction is limited to the 90,000,000-gas user budget. At the
default cap a permanent (or year-long) entity of `units` costs about
`units × 7,300` gas of rent at 2-second blocks, so roughly 12,000 units (about
10 KiB of data) is the largest entity that can be made permanent in one
transaction; larger ones need a shorter lifetime or a smaller cap.

Genesis prefill rows follow the same capped rent: a finite row at or beyond the
cap pays the cap. A `u64::MAX` row is admitted only by a permanent-prefill
launch (versions 3 and 4), which sponsors it without rent.

All arithmetic is checked and integer-only, using a wide intermediate for
unit-block multiplication and one upward division per operation. An operation
reserves rent before running the engine; a rejected engine operation does not
charge that rent. If a later operation fails an atomic transaction, state is
rolled back but already consumed execution and rent gas remains in its receipt.
The fee for the gas used is debited and burned (see
[GLM accounts](glm-accounts-v1.md)); there are no storage refunds.

Expiry removes live occupancy. Historical snapshots and signed transaction
bodies remain stored under the current no-pruning policy. Rent therefore prices
**live-state occupancy only**; archive retention needs a separate future policy.

## Execution prices and representative calibration

Every transaction starts at 21,000 intrinsic gas. Important engine weights are:
20,000 for record creation, 12,000 for patch, 10,000 for deletion, 250 per cell
write, 4 per submitted cell byte, 2 per rewritten row byte, 500 per inserted
posting, 300 per removed posting and 100 per logical authenticated-map update.
An empty patch (golem-db's no-op) pays the directory and key lookups and the
patch base weight only; it writes nothing, so it has no cell, row or map-update
charges. Every scalar type is charged by its encoded width alone
(`Value::encoded_len`: 1 to 32 bytes for the fixed-width types); no type has a
type-specific price. Reads are priced the same way: a range over any ordered
type (every indexable type, including `bool`, `str` and the fixed-width byte
types) costs one `TermSeek`, one `TermVisit` per distinct term in the run and
one `PostingEntry` per match, exactly as a numeric range or a `str` prefix
does; a term visit compares at most 128 bytes (the indexed-string cap).
Configured cell limits change which inputs are admitted, not any price.
The full price array lives in `crates/arkiv-node/src/gas.rs`. Invalid operations
still consume their preceding work; gas is a capacity budget, not only a count
of successfully written entities.

The executable calibration creates real user-defined-columns entities: the
generated system attributes, an unindexed `bytes` attribute `body` of the
indicated size, an unindexed `str` attribute `format` (`application/json`)
and the indicated number of indexed `u64` attributes. It runs the actual
engine/host meter with the default one-year cap (15,768,000 blocks) and
reports actual gas.

| `body` bytes | Indexed `u64` attributes | Lifetime (17,280-block days) | Execution gas | Rent gas | Total gas | GLM at 1 gwei |
| --- | ---: | --- | ---: | ---: | ---: | ---: |
| 256 B | 2 | 1 day | 58,358 | 16,256 | 74,614 | 0.000074614 |
| 1 KiB | 4 | 7 days | 66,422 | 172,816 | 239,238 | 0.000239238 |
| 1 KiB | 4 | 30 days | 66,422 | 740,640 | 807,062 | 0.000807062 |
| 1 KiB | 4 | permanent (charged as the cap) | 66,422 | 22,527,800 | 22,594,222 | 0.022594222 |
| 16 KiB | 4 | 7 days | 158,582 | 1,032,976 | 1,191,558 | 0.001191558 |
| 128 KiB | 4 | 30 days | 846,710 | 31,952,160 | 32,798,870 | 0.03279887 |

Thus small ordinary transactions land in the requested approximately 50,000–
1,000,000 gas range. Large or long-lived entities deliberately cost more; there
is no arbitrary cap forcing them into the standard range. A standard 1 KiB
entity's canonical key-and-cells encoding is 1,391 bytes and its logical
occupancy is 3,086 units; these measurements are not a physical trie/disk/RAM
benchmark. Hardware-based calibration remains future work.

Reproduce the table:

```sh
cargo run -p arkiv-node --example gas_calibration
```

Reference fee calculations use exact integers and decimal strings:
`gas × 1,000,000,000` base units, divided by `10^18` for GLM display. For example,
1,000,000 gas costs 0.001 GLM; a completely consumed 100,000,000-gas block costs
0.1 GLM.

## Validation

`cargo test -p arkiv-node --test gas_execution` covers exact gas-limit success
and failure, independent producer/full replay, a live permanent create paying
exactly the cap, batch rollback and nonce behavior,
growth/shrink/extension/delete accounting, restart of durable full and light
stores, proof verification after restart, and expiry processing alongside the
maximum declared user gas budget. Unit tests in `gas.rs` cover storage-unit
arithmetic, the default cap at several block periods, equal rent at the cap,
twice the cap and permanent, extension pricing below, at and beyond the cap,
an exhaustive no-discount check for permanence, saturation near `u64::MAX`,
rounding, overflow and exact GLM formatting; `arkiv` unit tests measure the
rent charged by live creates, extensions (including after time has passed),
growth and transfers of permanent entities.
