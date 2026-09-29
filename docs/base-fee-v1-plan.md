# Proportional base fee plan

Selected by the user on 2026-09-25 while GLM account fee collection was being
implemented. This is a design decision for a subsequent versioned fee profile;
it does not change the fixed-price account profile by itself.

## Parameters and recurrence

- Minimum and initial base fee: 1,000,000,000 GLM base units per gas (1 gwei).
- Target: 25,000,000 gas consumed per block.
- Maximum block gas: 100,000,000.
- Adjustment denominator: 20.
- Input: the immediately preceding block's actual gas consumption, not declared
  transaction gas limits, mempool size, wall-clock time or a moving average.

For parent fee `B`, parent gas consumed `G`, target `T = 25,000,000`, denominator
`D = 20`, and floor `F = 1,000,000,000`:

```text
G == T: next = B
G > T:  next = B + max(1, floor(B * (G - T) / (T * D)))
G < T:  next = B - floor(B * (T - G) / (T * D))
next = max(F, next)
```

Use checked wide integer arithmetic. Never use floating-point arithmetic in
execution, header validation or fee settlement. Define and validate a maximum
representable fee before deployment so overflow cannot silently wrap or stall
otherwise valid production.

| Parent gas used | Next-fee change, before rounding and the minimum floor |
| ---: | ---: |
| 0 | -5% |
| 12,500,000 | -2.5% |
| 25,000,000 | unchanged |
| 50,000,000 | +5% |
| 75,000,000 | +10% |
| 100,000,000 | +15% |

The user explicitly selected this proportional response, rather than a capped
+5% response or a fixed upward/downward step. A 100M block is four times the
25M target, so its increase is 15%, not 5%. The target is a feedback setpoint;
it does not guarantee average usage when demand is low or the fee hits its floor.

## Repository reference

The local `arkiv/crates/arkiv-reth-chainspec/src/lib.rs` implements
`max(inner.next_block_base_fee(parent), genesis floor)` in
`EthChainSpec::next_block_base_fee`. The local
`arkiv-op-geth/consensus/misc/eip1559/eip1559.go` implements the proportional
parent-gas recurrence with a minimum one-unit upward adjustment. These are
read-only reference repositories. Their configured defaults vary; this plan
selects its own floor, target and denominator explicitly.

## Integration work before activation

1. Commit all fee parameters in a new genesis/profile version and authenticate
   each block's effective fee and actual gas used, allowing independent full and
   light validation of the next fee.
2. Add a signed transaction maximum fee per gas. Reserve
   `gas_limit * max_fee_per_gas`, charge `gas_used * block_base_fee`, and refund
   the remainder. Continue burning the collected base fee; tips are out of scope.
   Never let a fee increase authorize spending above the sender's signed cap.
3. Keep underpriced pending transactions out of block selection; bounded pool
   capacity and deterministic affordability rules must prevent an underpriced
   sender from stalling other senders. Recheck affordability as the chain moves.
4. Expose current and next base fee in status, the paid price and total burned
   fee in receipts, and the signed fee cap in transaction tooling.
5. Explicitly settle whether the feedback meter includes automatic expiry gas.
   Recommended: all gas counted against the 100M block budget, including expiry,
   with user fees charged only for the user's own execution and storage rent.
   Existing 90M user-budget reservation remains separate from the 25M target.
6. Test floor behavior, target equality, one gas above/below target, empty/full
   blocks, integer boundaries, fee-cap deferral, balance reservation/refunds,
   failed execution, independent replay, proof verification and durable restart.

Activation follows implementation and validation, using a fresh disposable demo
chain. The current account implementation and deployment remain separate work.
