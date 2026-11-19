# Arkiv litepaper v2 against this node (2026-11-19)

The [Arkiv litepaper](../references/ARKIV-Litepaper.pdf) (November 2026,
20 pages, from `download.arkiv.network`, SHA-256 `4de98d821c53a0ca…`)
compared with this repository at `7c98173`. Docs and code were read; no
tests were run for this review. Where it mattered, upstream `arkiv`
(`c1987ad`) and SDK 1.0.1 were checked as a third reference.

The litepaper describes a target design. On p7 it says the network is "not
yet deployed as the decentralized network described here". The table
therefore compares what the litepaper says with what we do, and says when
upstream differs from both.

## Area by area

| Area | Litepaper | This node | Verdict |
| --- | --- | --- | --- |
| Layers | Coordination chain bridged to Ethereum, DBChains, engine; a sidechain, not a rollup (p6–7) | One standalone chain. No registry, bridge or Arkiv chain | Not covered |
| Execution client | reth fork with the EVM replaced by Golem DB (p7) | Own Rust node and engine; native block and state commitments, not Ethereum RLP/MPT | Differs, deliberately |
| Consensus | GLM staking, BFT with single-slot finality, more than 2/3 honest (p13) | One signed proposer, no finality; `safe` and `finalized` alias the head (`signed/rpc/params.rs:60`) | Not covered |
| Engine boundary | No blocks, transactions, gas, addresses or owner field in the engine (p12) | `$owner` is an ordinary cell, as described, but the engine checks namespace ownership (`engine/write.rs:92`) and the expiry sweep uses `BlockNumber` (`arkiv-db/src/host.rs:22,106`) | Differs, defensibly |
| Determinism, branches, budgets, receipts | p12 | Branch per block, per-transaction checkpoints, estimates on a discarded branch, metered receipts and budgets | Matches |
| Record model | Key plus typed cells, indexed or stored, no schema, indexed from the first write (p5, p12) | Same. 10 SDK scalar types, 32 attributes, 128-byte indexed strings, 128 KiB per entity (`arkiv/schema.rs:100-113`) | Matches |
| Immutable records | Cannot be changed or deleted by anyone, owner included; no change, transfer or delete (p5, p8) | The readonly flag blocks patches only (`arkiv/mod.rs:626-630`); transfer (`:650`) and delete (`:658`) do not check it. Upstream and the SDK behave the same | Claim not met, by us or upstream |
| Ownership | Only the owner changes, transfers or deletes; delegation later (p8) | Owner checks at `arkiv/mod.rs:478-482`; no delegation | Matches |
| Extension | Anyone may pay to extend a flagged record; lifetime never shortens (p5, p9) | Same; a non-owner can extend at most one rent cap ahead (`arkiv/mod.rs:507-518`) | Matches, with one extra rule |
| Lifetime cap | Each chain caps how far ahead a lifetime can be bought; data lives on by renewal (p9, p14) | No cap: `u64::MAX` is permanent, and any lifetime of a year or more costs one year of rent (`docs/glm-gas-v1.md`). Upstream caps at 7,884,000 blocks and has no permanence | Differs, deliberately |
| Expiry | A record leaves the query surface at expiry (p8–9) | Exact sweep at the expiry block, up to 128 per block, spilling at most 64 blocks (`exec.rs:296`) | Matches; stricter than upstream |
| Events | Created, updated, deleted, extended, owner changed; subscriptions (p9) | The same five logs (`signed/rpc/logs.rs:236-261`), `eth_subscribe` for `logs` and `newHeads`; no expiry event | Matches |
| Queries | Key fetch, filters, sorting, cursor paging, projection, count, reads at a past block within retention (p9) | All of them, plus `!=`, `NOT` and `$createdAt` predicates; sorting is fetch-and-sort under a 25M-unit read budget | Matches |
| Result order | One fully specified order (p12) | Record-id order. Upstream orders by creating transaction, then key, so the two differ within a transaction that creates several entities | Differs, deliberately |
| Retention window | Recent history every node keeps, set per chain (p9) | A per-node flag (`--state-retention`, default archive) that only sets a serving floor; nothing is deleted | Gap |
| Proofs | A record's cells, an index entry or an absence, checked against the commitment alone (p5, p12) | State root in every header; equality pages, ranges and account proofs; the light follower verifies them. Native REST only; Boolean, `NOT` and sorted queries are not proven | Partly matches; ahead of upstream |
| Write fee | Data, lifetime, indexing, computation and current storage load, plus a base fee that rises with full blocks (p14) | Execution gas at a fixed 1 gwei, plus rent charged outside gas. No base fee (`docs/proposals/base-fee.md` is not implemented) and no load term | Differs; partly deliberate, partly a gap |
| Estimate before signing | A cost estimate is available before signing (p14) | `eth_estimateGas` covers execution only; rent has no public quote and nothing signed bounds it (internal quote at `signed/host.rs:777`) | Gap |
| Read fees | None at protocol level (p5, p14) | None; a local read budget and `--read-concurrency 4`, excess refused with `ServerBusy` | Matches |
| GLM | ERC-20 bridged through the Arkiv chain; secures the chain at mainnet (p14) | Native genesis allocations; fees and rent burned; no bridge or staking | Not covered |
| Ethereum alignment | Addresses, wallets, Ethereum-style JSON-RPC (p13) | Legacy and EIP-1559 transactions to `0x44…44`, `eth_*` reads and logs, WebSocket; no `eth_getProof`, no contract calls | Matches what the SDK needs |
| SDK | `defineSchema`, `createEntity`, `flags.readonly`, `extendEntity`, `select().from().where().ownedBy()`, patch, delete (p10–11) | SDK 1.0.1 suites pass unmodified (component 33/33, health 31/31) | Matches |
| Block time | DBChain-1: 2 s (p7) | A genesis parameter; CLI default 1,000 ms; benchmarks ran at 250 ms | Configurable |
| Data availability | Validators; retained history and archives have their own limits (p9) | Bodies never pruned; followers replicate from one producer | Not covered |
| MCP, hub, faucet, API keys, AIPs, several clients | p5, p13, p18–19 | None | Not covered |

## Findings, most important first

1. **"Immutable" is weaker in code than in the litepaper.** The readonly
   flag blocks patches, but the owner can still delete or transfer the
   record, here and upstream (`arkiv-reth-executor/src/arkiv.rs:287`). The
   SDK's own error text says a readonly entity "can be transferred or
   deleted" (`arkiv-sdk-js/src/utils/revert.ts:77`). The litepaper's
   public-commitment use cases (p17) depend on the stronger meaning.
   Either the litepaper or every implementation is wrong, and that needs
   settling upstream. Matching the litepaper would mean a readonly check at
   `arkiv/mod.rs:650` and `:658`, which is a consensus change.
2. **Permanent lifetimes contradict the litepaper's economics.** The
   litepaper has a per-chain cap with data kept alive by renewal. Upstream
   caps lifetime at about 182 days with no permanence. We sell permanent
   lifetimes for one year of rent. This is deliberate, but it is the
   largest economic departure. A genesis maximum lifetime would let each
   chain choose.
3. **There is no complete estimate before signing.** Rent is usually the
   larger cost (1 KiB for 30 days: 0.00185 GLM of rent against 0.000066 GLM
   of execution), but it is not in `eth_estimateGas` and nothing signed
   bounds it. Upstream puts storage inside gas, so its estimate is
   complete. Exposing `Host::quote` over RPC and adding the planned signed
   allowance would close this.
4. **No base fee and no demand pricing.** Our gas price is fixed at 1 gwei
   and the base-fee proposal is not implemented. Upstream has an EIP-1559
   base fee. Neither has the litepaper's storage-load term.
5. **The SDK assumes 2-second blocks.** It hardcodes `BLOCK_TIME = 2`
   (`arkiv-sdk-js/src/consts.ts:3`) when converting days to blocks. On our
   1 s default, `ExpirationTime.fromDays(30)` buys 15 days; at 250 ms it
   buys 3.75. Rent is charged for the real time, so billing is right, but
   the lifetime is not what the caller asked for. Either run SDK-facing
   chains at 2 s or have the SDK read `arkiv_getBlockTiming`.
6. **At 2-second blocks our caps allow 64 operations per second.** The
   block format caps a block at 16 transactions, 16 operations each and
   128 operations in all (`signed/protocol.rs:38,198-200`). Our
   512 creates/s needed 250 ms blocks. These caps are format bounds, not
   node limits: the producer used about 15% of a core at that rate.
   Upstream reports about 250 small entities per 2-second block.
7. **Proofs are where we are ahead.** Upstream lists record proofs and
   index-completeness proofs as open (`arkiv/docs/api-spec.md:476`); we
   serve and verify equality, range and account proofs. They are only on
   native REST, and they prove state under one trusted proposer, not
   execution or consensus.
8. **The retention window is not a chain parameter, and nothing is
   pruned.** The litepaper's window implies physical pruning and a restart
   time independent of chain length. We have neither: see
   [the 2026-11-19 soak](../benchmarks/2026-11-19-node-soak.md) (a 146 GiB
   `node.redb` holding 41.5 GiB, and a 614 s restart at height 108,056).
9. **This node cannot be a second client of DBChain-1.** State root, block
   format, result order within a transaction, purge timing and fee rules
   all differ from upstream. That is fine for a playground, but the
   litepaper's client diversity (p12–13) would need one engine
   specification that both follow.
10. **The engine boundary is wider than the litepaper's.** Namespace
    ownership and the block-number-driven expiry sweep live in `arkiv-db`.
    Moving the sweep helper into the node would match the stated boundary.

## Problems in the litepaper itself

- **Immutable:** pp. 5 and 8 contradict the SDK and the reference executor
  (finding 1).
- **Cursor paging and past-block reads:** promised on pp. 9 and 16, but SDK
  1.0.1 removed them ("The network only answers at the head",
  `arkiv-sdk-js/CHANGELOG.md:70-72`). Upstream's API spec still documents
  `cursor` and `atBlock`, and we serve both.
- **Verifiability:** "each guarantee can be checked against what the
  network committed" (p5), but the SDK teaser has no proof API and
  upstream marks proof serving as open. Commitments posted to Ethereum are
  recorded "without verifying" (p13).
- **Who may change a record:** "only its owner may change it" (p5) sits
  beside extension by anyone, and p9 says "anyone the record authorizes"
  where the mechanism is a single flag.
- **Engine vocabulary:** "no address semantics" (p12) next to "a cell
  holding an Ethereum address".
- **Fees:** "current storage load" and the base fee are two undefined
  demand mechanisms (p14), and it doesn't say where fees go. Upstream burns
  compute fees and credits storage fees to a pool that nothing pays out
  of.
- **Consensus:** a reth-fork execution client with BFT consensus, without
  saying how the two connect. p13 also has an unclosed parenthesis.
- **Expiry events:** there is no expiry event (p9), yet the queue and
  heartbeat patterns on p16 depend on noticing expiry.
- **Extension:** `extendEntity(..., fromDays(120))` doesn't say whether
  120 days counts from now or from the current expiry. On chain it is
  `max(expiresAt, now + lifetime)`.
- **Missing numbers:** the only figures are 2-second blocks, 2027 and the
  2/3 threshold. Throughput, latency, cost, record limits, retention window
  and lifetime cap are all unstated, so none of them can be checked.
- **Cross-reference:** p7 cites "Section 6" in a document without numbered
  sections.

## Numbers side by side

| Quantity | Litepaper | Upstream (its docs, not re-measured) | This node |
| --- | --- | --- | --- |
| Block period | 2 s | 2 s (dev image: 250 ms) | Default 1 s; measured at 250 ms |
| Finality | Single slot, more than 2/3 of stake | — | None |
| Write throughput | — | About 250 small entities per 2 s block at 30M gas | 512 creates/s at the cap with 250 ms blocks; about 256/s once inclusion took two blocks; at most 64 operations/s with 2 s blocks |
| Transaction latency p50 | — | 256 ms (dev image, 250 ms blocks) | 242 ms at the cap; 489 ms in growth; p95 up to 5.3 s once the state file outgrew the page cache |
| Read latency p50 | — | 0.4–19 ms | 0.3–6 ms; 4 concurrent, the rest refused |
| Gas per small create | — | 28,648 | About 41,800 |
| Gas price | Base fee, demand-driven | EIP-1559 | Fixed 1 gwei |
| Storage price | — | About $1.80 per GB-month (1 gwei, GLM at $0.10) | About 650 GLM per GiB-month, about $65 at the same GLM price |
| Lifetime cap | Set per chain | 7,884,000 blocks (about 182 days), no permanence | None; rent capped at one year |
| Expiry removal | At expiry | At most 10 per block, lazily | Up to 128 per block, exact, spilling at most 64 blocks |
| Per-entity limits | — | 128 KiB per `bytes` value; per-entity cap open | 32 attributes, 128 KiB, 16 operations per transaction |

Both price schedules are declared uncalibrated in their own documents, so
the roughly 35-fold storage price gap is between two placeholders.
