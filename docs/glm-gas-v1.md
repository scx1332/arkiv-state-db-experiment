# GLM gas v1

For native balances, transfers and actual fee payment, see [GLM accounts (`glm-v2`)](glm-accounts-v1.md). This document describes the unchanged gas-only `glm-v1` profile.

This opt-in native-chain profile prices transaction execution and finite entity
occupancy in gas. It uses a **1 gwei reference price**, meaning 1,000,000,000
GLM base units per gas, with 18 decimal places per GLM. Fees are **quoted, not
debited**: there is no balance ledger, fee market, EIP-1559 base fee, miner tip,
or Ethereum JSON-RPC compatibility implied by this profile.

The default block limit is **100,000,000 gas**, with **90,000,000 gas** available
to declared user transaction budgets; the rest protects expiry processing.
The existing transaction-count, operation-count and encoded-byte limits also
apply. The gas limit is not a throughput promise, and the existing 16-transaction
cap can bind before gas does. 50–200 million gas is the intended experimental
block range, subject to validating the expiry reservation.

## Version and activation

Schedule ID `0x474c4d01` (`GLM` + version 1) commits the exact 31 engine prices in
genesis. The node rejects different weights under this ID. Host intrinsic and
rent rules are part of this named profile, in addition to engine metering model
3. Admin pricing changes cannot activate, replace, or alter this profile: use a
fresh genesis. The signed-genesis envelope uses version 2 for GLM gas v1;
legacy chains retain version 1, and mismatched version/profile pairs are
rejected. Old binaries reject the new genesis before trusting its rules.
Existing fixture-priced chains keep their old behavior. The signed transaction
and header envelope formats are unchanged; `tx_budget` is the transaction gas limit.
Receipts, outcomes and signed headers commit the charged units through the
existing outcome commitment. No public deployment is performed by this change.

Create a new producer with `--gas-profile glm-v1` (see `arkiv-proposer --help`
for the other required identity, path and key arguments). The status response
advertises `capabilities.gas`, including whether the profile is active, exact
limits, quote price, finite-lifetime restriction and `feesDebited: false`.

## Storage units and rent

For the complete committed entity, including its system attributes:

```
units = 512
      + payload byte length
      + sum(non-payload cell name UTF-8 bytes + value encoded bytes + 2 tags)
      + 128 × indexed cell count

create rent = ceil(units × lifetime blocks × 8 / 17280)
```

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

- Create prepays the entire finite lifetime.
- Growing an entity prepays only its positive size increase over the remaining
  lifetime. Same-sized changes pay execution but no additional occupancy rent.
- Extending expiry prepays the entity's current size for the added blocks.
- Shrinking or deleting gives no refund. Regrowing pays for the renewed increase.
- Ownership transfer does not change occupancy, but pays execution gas.
- Permanent entities (`u64::MAX` expiry) are rejected in this profile. V1 does not
  pretend that a finite fee purchases unbounded retention.

All arithmetic is checked and integer-only, using a wide intermediate for
unit-block multiplication and one upward division per operation. An operation
reserves rent before running the engine; a rejected engine operation does not
charge that rent. If a later operation fails an atomic transaction, state is
rolled back but already consumed execution and rent gas remains in its receipt.
There are no monetary debits or refunds in this implementation.

Expiry removes live occupancy. Historical snapshots and signed transaction
bodies remain stored under the current no-pruning policy. Rent therefore prices
**live-state occupancy only**; archive retention needs a separate future policy.

## Execution prices and representative calibration

Every transaction starts at 21,000 intrinsic gas. Important engine weights are:
20,000 for record creation, 12,000 for patch, 10,000 for deletion, 250 per cell
write, 4 per submitted cell byte, 2 per rewritten row byte, 500 per inserted
posting, 300 per removed posting and 100 per logical authenticated-map update.
The full price array lives in `crates/arkiv-node/src/gas.rs`. Invalid operations
still consume their preceding work; gas is a capacity budget, not only a count
of successfully written entities.

The executable calibration creates real entities, with generated system
attributes and `application/json` content type, runs the actual engine/host
meter, and reports actual gas. Its sizes below include only the indicated user
payload; its attribute count excludes system attributes and content type.

| Payload | User attributes | Lifetime at 5 s/block | Execution gas | Rent gas | Total gas | GLM at 1 gwei |
| --- | ---: | --- | ---: | ---: | ---: | ---: |
| 256 B | 2 | 1 day | 59,818 | 17,280 | 77,098 | 0.000077098 |
| 1 KiB | 4 | 7 days | 67,882 | 179,984 | 247,866 | 0.000247866 |
| 1 KiB | 4 | 30 days | 67,882 | 771,360 | 839,242 | 0.000839242 |
| 16 KiB | 4 | 7 days | 160,042 | 1,040,144 | 1,200,186 | 0.001200186 |
| 128 KiB | 4 | 30 days | 848,170 | 31,982,880 | 32,831,050 | 0.03283105 |

Thus small ordinary transactions land in the requested approximately 50,000–
1,000,000 gas range. Large or long-lived entities deliberately cost more; there
is no arbitrary cap forcing them into the standard range. A standard 1 KiB
entity's canonical key-and-cells encoding is 1,401 bytes and its logical
occupancy is 3,214 units; these measurements are not a physical trie/disk/RAM
benchmark. Hardware-based calibration remains future work.

Reproduce the table:

```sh
cargo run -p arkiv-node --features arkiv-v2 --example gas_calibration
```

Reference fee calculations use exact integers and decimal strings:
`gas × 1,000,000,000` base units, divided by `10^18` for GLM display. For example,
1,000,000 gas quotes 0.001 GLM; a completely consumed 100,000,000-gas block quotes
0.1 GLM. Neither number means that tokens were transferred.

## Validation

The profile's six integration tests cover exact gas-limit success and failure,
independent producer/full replay, finite lifetime enforcement, batch rollback and
nonce behavior, growth/shrink/extension/delete accounting, signed-genesis version
pinning, restart of durable full and light stores, proof verification after
restart, and expiry processing alongside the maximum declared user gas budget.
Four unit tests cover storage-unit arithmetic, rounding, overflow and exact GLM
formatting. Existing engine and node tests run under both feature profiles.
