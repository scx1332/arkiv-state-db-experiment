# GLM gas schedule

Every signed native chain runs the `glm-v2` gas profile: this gas schedule plus
[GLM accounts](glm-accounts-v1.md), which debit the fees from funded balances
and burn them. This document describes how transaction execution is priced in
gas, and how entity occupancy is priced under the rent rule `glm-rent-v3`:
in GLM per second of capped lifetime, **outside gas**.

Gas is priced at a fixed **1 gwei**, meaning 1,000,000,000 GLM base units per
gas, with 18 decimal places per GLM. There is no fee market, EIP-1559 base fee
or miner tip.

The default block limit is **100,000,000 gas**, with **90,000,000 gas** available
to declared user transaction budgets; the rest protects expiry processing.
Both limits count execution only; rent never uses them. The existing
transaction-count, operation-count and encoded-byte limits also apply. The gas
limit is not a throughput promise, and the existing 16-transaction cap can bind
before gas does. 50–200 million gas is the intended experimental block range,
subject to validating the expiry reservation.

## Schedule identity

Schedule ID `0x474c4d01` (`GLM` + version 1) commits the exact 31 engine prices in
genesis. The node rejects different weights under this ID. Host intrinsic and
rent rules are part of this named schedule, in addition to engine metering model
3. Admin pricing changes cannot enter, replace, or alter it
(`AdminRejection::GasProfileLocked`); a signed genesis without it is invalid.
`tx_budget` is the transaction gas limit. Receipts, outcomes and signed headers
commit the charged units, and each transaction outcome commits the rent it
burned (`TxOutcome::rent_wei`), through the outcome commitment.

The status response advertises `capabilities.gas`: profile `glm-v2`, exact
limits, gas price, `rentRule: "glm-rent-v3"`,
`rentCharging: "sender-balance-outside-gas"`, `rentWeiPerStorageUnitDay`,
`rentBlockPeriodMs`, `maxRentLifetimeSeconds` and the `maxRentLifetimeBlocks`
it applies as, `rentReceiptField: "rentFee"`, `rentBound: "sender-balance"`,
`rentQuoteMarginBlocks`, `maxNewStorageUnitsPerBlock`,
`permanentEntitiesSupported: true`, the `permanentEntities` rules and
`feesDebited: true`.

## Storage units and rent

For the complete committed entity, including its system attributes:

```
units        = 512
             + sum(cell name UTF-8 bytes + value encoded bytes + 2 tags)
             + 128 × indexed cell count

cap_blocks   = ceil(max_rent_lifetime_seconds × 1000 / block_period_ms)
horizon(E,h) = min(E, h + cap_blocks)
create       : unit_blocks = units × (horizon(E, h) − h)
rent_wei     = ceil(unit_blocks × block_period_ms × 20,000,000,000 / 86,400,000)
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

**The rate is per second.** Rent is **20 gwei per storage unit per day**
(`RENT_WEI_PER_UNIT_DAY`), 7,300 gwei per unit per year, whatever the block
period. That is the `glm-rent-v2` rate at the SDK's nominal 2-second blocks (8
gas at 1 gwei per 17,280 blocks of 2 s), so a 2-second chain pays what it paid
before, and a year now costs the same at every block period. Lifetimes are
block heights, so the charged blocks are converted to time at the chain's
consensus block period `ChainParams::block_period_ms` (a signed chain's equals
the period its timestamps advance by). The rate reduces to `6250 / 27` wei per
unit-millisecond; the result is rounded up once per operation, to the wei.

### The rent cap

Rent is never charged past `h + cap_blocks` from the current height `h`.
Consensus stores the cap in **seconds**, `ChainParams::max_rent_lifetime_seconds`
(1 s to 100 years, `MAX_RENT_CAP_SECONDS` = 3,153,600,000), beside the block
period (1 ms to one day); every node derives `cap_blocks` from the two with the
same integer ceiling, so the cap is never shorter than asked. The default is
one year, `31,536,000` seconds: 15,768,000 blocks at 2 s, 31,536,000 at 1 s,
6,307,200 at 5 s. A period that does not divide a year rounds the cap up by
less than one block (4,505,143 blocks of 7 s are one second over a year). At
the bounds `h + cap_blocks` cannot saturate before heights near `u64::MAX`, and
no quote of any `u64` size overflows `u128`.

Every lifetime of at least the cap, a finite one far in the future or the
permanent `u64::MAX`, costs the same; shorter lifetimes are priced exactly.
Lifetimes have no upper bound: `max_lifetime` no longer applies to Arkiv
entities (see [block format](block-format.md)), so no expiry is ever `TooFar`.

With `E_old` the expiry before and `E_new` after an operation at height `h`:

- **Create** prepays `units × (horizon(E_new, h) − h)`: at most the cap.
- **Growth** (a patch that enlarges the entity) prepays the positive size
  increase for the capped remaining lifetime, `growth × (horizon(E_old, h) − h)`;
  for a permanent entity that is always a full cap. Same-sized changes pay
  execution but no rent.
- **Extension** prepays the current size for the growth of the capped horizon
  only, `units × (horizon(E_new, h) − horizon(E_old, h))`. Extending an entity
  whose expiry already reaches `h + cap_blocks` costs no rent.
- **Ownership transfer** changes neither size nor expiry: no rent.
- Shrinking or deleting gives no refund. Regrowing pays for the renewed increase.

**Time passing.** The horizon is recomputed from the current height on every
operation; nothing like a "paid through" height is stored. An entity created
permanent at `h0` pays one cap; once `h0 + cap_blocks` passes it simply stays,
with no further payment (the user-chosen rule: permanent costs the same as one
cap). Extending it again is a no-op success (`u64::MAX` is the only expiry
that does not shorten it, and its horizon cannot grow), and a finite expiry is
refused with `ExpiryShortened`. A finite entity may be extended to permanent:
it pays the capped horizon increase.

**Only the owner extends beyond one cap.** An entity created with the
permissionless-extension flag (creation flag 2) takes anyone's extension up to
one rent cap ahead, `h + cap_blocks`; anything further, a far-future finite
expiry, `u64::MAX`, or a minimum lifetime that resolves to either, is the
owner's alone, and anyone else is refused with
`ArkivFault::PermanenceOwnerOnly` (revert `PermanenceOwnerOnly(bytes32
entityKey, uint64 requested, uint64 limit)`). Rent stops at the cap, so
without the limit an extender could buy a near-permanent lifetime for the
price of one cap. Since a permanent entity cannot be shortened, another
account cannot extend it at all. An entity without the flag takes extensions
from its owner only (`NotOwner`).

**No discount on permanence.** Whatever the sequence of creates, extensions and
size changes, every storage unit an entity holds has paid at least
`min(E − g, cap_blocks)` blocks, where `E` is the entity's current expiry and
`g` the height that unit last appeared (its create, or the growth that added
it): while every expiry stays below `h + cap_blocks` the payments telescope
exactly, and the step that first reaches it brings the total to at least one
cap. Permanence therefore never costs less than one cap per unit, splitting it
into short extensions only costs more, and units added by growth pay for
themselves. `gas.rs` checks this, and an independent per-unit model of every
quote, in wei: exhaustively over every create and two later operations of a
small domain (heights that never go backwards, sizes that grow, shrink and
stay, expiries that stay, grow, cross an 8-block cap and become permanent),
and over 16,000 seeded random schedules of up to eight operations under four
caps.

**Expiry capacity.** At most `expiry_per_block` (16 on signed chains) records
may expire at one height. A finite expiry, however far away, counts against its
height's capacity (`ExpiryCapacityFull`), and lifetimes are unbounded while
rent is capped at a year, so filling a far-future height costs 16 entities'
worth of one year of rent each. That squatting only denies that exact height:
another expiry one block away, or a permanent one, which never counts against
any height and is never swept, is still free. It is cheap but low impact.

**Overflow.** `h + cap_blocks` saturates at `u64::MAX`; unit-block and wei
products use 128-bit intermediates, which the genesis bounds keep from
overflowing; only a storage-unit count that does not fit `u64` is rejected
(`GasOverflow`). `u64::MAX` expiries are never scheduled for expiry, never
counted against the per-height expiry capacity and never swept.

## Charging rent

Rent is **not gas**. It counts against neither the transaction's gas limit nor
the block gas limit; only the sender's balance limits it.

- **Execution.** Each Arkiv operation quotes its rent before the engine write.
  The sender must hold it after its transaction's gas reservation (the fee at
  the declared gas, `gas limit × 1 gwei`, reserved up front; transferred value
  is debited by its transfer operation); otherwise the operation fails with
  `ArkivFault::InsufficientRentBalance` before anything is written. After a
  successful write the rent is debited and burned, like the fee. A
  transaction is atomic: when any operation fails, the rollback restores every
  rent debit and burn of the transaction, its gas stays charged as for any
  failure, and its outcome records `rent_wei = 0`.
- **Admission.** The cheap checks come first, before any state is read:
  identity, the nonce window, sizes, pool capacity, and funds for every
  pending reservation of the sender plus the fee at the declared gas and the
  value. Only then is the transaction's rent quoted: its Arkiv operations in
  order, each seeing what the earlier ones of the same transaction created or
  changed (so `[create E, patch E]` quotes the patch on the new entity), with
  each operation reserved at the larger of its rent at the next height and
  `RENT_QUOTE_MARGIN_BLOCKS` (64) later. A create reads nothing and a keyed
  operation its one entity (in full: the engine has no size-only read). A
  sender who cannot cover the quote too is refused with `insufficient funds
  for gas * price + value + rent`: no transaction, no nonce, no fee. An
  operation building on another *pending* transaction (a patch of an entity
  created by an earlier, uncommitted one) quotes what the head shows, nothing
  for an entity not created yet; execution then checks the real rent.
- **Production.** Rent can exceed its quote (the cases below), and a later
  transaction of the same sender may then be unable to cover its gas
  reservation, which would invalidate a block. The producer builds each
  block in one execution: such a transaction, and its sender's later ones,
  are left out where they stand instead of the block being rebuilt, and
  after every commit each sender's pending transactions are checked in nonce
  order against its new balance; the first that no longer fits leaves the
  pool with its successors. Every candidate executes at most once per block.
- **Dropped transactions.** A transaction evicted this way (or one that
  alone exceeds the storage cap, below) is remembered in a bounded volatile
  registry: `eth_getTransactionByHash` and `eth_getTransactionReceipt` answer
  `-32000` / `transaction dropped: insufficient funds for gas * price + value
  + rent` (or `... adds more storage units than a block may`), so viem's
  `waitForTransactionReceipt` fails instead of waiting, and native request
  status reports `"status": "dropped"` with the reason.
- **Estimation.** `eth_estimateGas` returns execution gas only, without
  checking affordability, so the SDK's ordinary create, extend and patch calls
  work unchanged: a full 128 KiB entity made permanent executes in about
  850,000 gas and pays about 0.97 GLM of rent in the same transaction.
- **Visibility.** `eth_getTransactionReceipt` adds `rentFee`, the wei the
  transaction burned beside `gasUsed × effectiveGasPrice` (like an L2
  receipt's separate `l1Fee`; viem passes unknown receipt fields through).
  Native request status reports `rentChargedWei` beside `feeChargedWei`, read
  from the block's feed metadata, and account responses' `burnedWei` and
  `totalSupplyWei` include burned rent. Logs are unchanged.

Chains without accounts (unsigned fixtures) quote and record rent without
debiting it.

### What bounds rent

Rent is bounded **only by the sender's balance** in this version: nothing the
transaction signs limits it. A wallet's `gas × price + value`, the total it
shows and checks, covers the fee and transferred value only; the rent is
charged on top, from whatever balance is left after the fee reservation.

- **Exposure per operation.** An operation pays at most its entity's size for
  one cap: at the default one-year cap a full 128 KiB entity costs about
  0.97 GLM, and a patch growing an entity by the 128 KiB `bytes` limit about
  the same, so an operation risks about **1 GLM**, and a transaction of at
  most 16 operations (extensions of 16 full entities, say) about 16 GLM.
- **Quotes drift with height.** Extending an entity across the cap pays
  `h + cap − old`, which grows by one block of the entity's size per block,
  about 62 gwei a block for a full entity at 2-second blocks. The pool's
  64-block quote margin covers that drift for transactions included within
  64 blocks of admission; beyond it the real rent is charged.
- **Concurrent changes raise rent.** An extension pays for the entity's size
  when it executes. If the owner grows a permissionless-extension entity in
  the same block, before an extender's transaction, the extender pays the
  larger size for its extension, many times its quote. The extender never
  pays more than its balance, and never more than about 1 GLM per extension.
- **Planned bound.** A signed allowance, such as the transaction's value
  field, which a transaction to the Arkiv address must now leave at zero,
  will cap the rent once the SDK can set it.

### Storage per block

`ChainParams::max_new_storage_units_per_block` caps the storage units the
committed transactions of one block add: a create adds its entity's units, a
patch its positive growth; extensions, transfers, shrinks, deletes and expiry
add nothing and free nothing against it. A block over the cap is invalid, and
followers refuse it (`ExecError::StorageCapExceeded`). Building a block, the
producer estimates each candidate's addition on the state it will see (exact
when it commits; a transaction that fails adds nothing) and leaves a candidate
that would cross the cap, with its sender's later ones, in the pool for a
later block; one that alone exceeds the cap can never fit and is dropped. At
admission a transaction that alone exceeds it is refused with `transaction
adds more storage units than a block may`.

The default, **1,500,000 units**, binds no block the default input cap admits.
Every user cell costs at most its encoded input bytes (its name, value and two
tags against a length prefix, a kind and a value tag) plus 128 if indexed, and
each operation adds at most an empty entity (1,460 units: 512 overhead, 180
system-cell bytes, 6 × 128 indexed system cells) and 32 index reservations.
So a block adds at most `max_block_input_bytes + max_ops_per_block × (1,460 +
32 × 128)` = 786,432 + 128 × 5,556 = **1,497,600** units, which the default
rounds up. A block of five full 128 KiB native creates adds about 666,000
units, 128 one-kilobyte entities about 395,000. A genesis lowers the cap to
throttle storage growth, for example 300,000 for at most two full entities a
block.

**Size, not gas, bounds an entity.** A permanent (or year-long) entity of
`units` costs `units × 7,300` gwei of rent. A full entity, 128 KiB of `bytes`
(about 133,000 units), costs about 0.97 GLM; the limits are the per-entity
128 KiB `bytes` cap and the transaction size: 256 KiB for a native envelope and
384 KiB signed, which fits an Ethereum transaction (whose original bytes travel
beside the decoded envelope) carrying a full entity.

Genesis prefill rows follow the same capped rent, in wei, burned from the
sponsor: a finite row at or beyond the cap pays the cap. A `u64::MAX` row is
admitted only by a permanent-prefill launch (versions 3 and 4), which sponsors
it without rent; that waiver is decided before any quote.

All arithmetic is checked and integer-only. If a later operation fails an
atomic transaction, state is rolled back but already consumed execution gas
remains in its receipt. The fee for the gas used is debited and burned (see
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
engine/host meter at 2-second blocks with the default one-year cap and reports
actual execution gas and rent.

| `body` bytes | Indexed `u64` attributes | Lifetime | Execution gas | Execution GLM | Rent GLM | Total GLM |
| --- | ---: | --- | ---: | ---: | ---: | ---: |
| 256 B | 2 | 1 day | 58,358 | 0.000058358 | 0.00004064 | 0.000098998 |
| 1 KiB | 4 | 7 days | 66,422 | 0.000066422 | 0.00043204 | 0.000498462 |
| 1 KiB | 4 | 30 days | 66,422 | 0.000066422 | 0.0018516 | 0.001918022 |
| 1 KiB | 4 | permanent (charged as the year cap) | 66,422 | 0.000066422 | 0.0225278 | 0.022594222 |
| 16 KiB | 4 | 7 days | 158,582 | 0.000158582 | 0.00258244 | 0.002741022 |
| 128 KiB | 4 | 30 days | 846,710 | 0.00084671 | 0.0798804 | 0.08072711 |
| 128 KiB | 4 | permanent (charged as the year cap) | 846,710 | 0.00084671 | 0.9718782 | 0.97272491 |

Execution of ordinary transactions lands in the requested approximately
50,000–1,000,000 gas range, a full 128 KiB entity included. Long-lived entities
deliberately cost more, in rent rather than gas. Lifetimes are real days at
2-second blocks (43,200 blocks a day); the same days cost the same rent at any
block period. A standard 1 KiB entity's canonical key-and-cells encoding is
1,391 bytes and its logical occupancy is 3,086 units; these measurements are
not a physical trie/disk/RAM benchmark. Hardware-based calibration remains
future work.

Reproduce the table:

```sh
cargo run -p arkiv-node --example gas_calibration
```

Reference fee calculations use exact integers and decimal strings:
`gas × 1,000,000,000` base units, divided by `10^18` for GLM display. For example,
1,000,000 gas costs 0.001 GLM; a completely consumed 100,000,000-gas block costs
0.1 GLM. Rent adds `rent_wei`, already in base units.

## Validation

`cargo test -p arkiv-node --test gas_execution` covers exact gas-limit success
and failure, independent producer/full replay with rent recorded per
transaction, a live permanent create paying exactly a year of rent within an
ordinary gas budget, a 128 KiB permanent create in one transaction, admission
refusing a sender short of gas plus rent by one wei, an execution-time
`InsufficientRentBalance` rolling its transaction back (gas charged, rent not),
poisoned senders costing one block execution per block (left out, then
dropped with a status), a `[create E, patch E]` transaction quoting its own
patch so 32 senders behind a heavy prefix never poison, the per-block storage
cap (a deferred transaction, a follower refusing a forged block over the cap,
admission refusing and production dropping transactions that alone exceed
it), batch rollback and nonce behavior, growth/shrink/extension/delete
accounting, restart of durable full and light stores, proof verification after
restart, and expiry processing alongside the maximum declared user gas budget.
Unit tests in `gas.rs` cover storage-unit arithmetic, the clock bounds, a year
costing the same at five block periods, the default storage cap against the
input-cap bound, equal rent at the cap, twice the cap and permanent, extension
pricing below, at and beyond the cap, the exhaustive and random no-discount
checks above, saturation near `u64::MAX`, rounding and exact GLM formatting;
`arkiv` unit tests check the rent of live creates, extensions (including after
time has passed), growth and transfers of permanent entities, debits and burns
outside the gas budget, refusal without the balance before any write,
rolled-back batches, the one-cap limit of non-owner extensions, the prefill
waiver, the transaction quote against execution (rent and storage units, its
own earlier operations included) and the quote margin across the cap.
