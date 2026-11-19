# Arkiv metering work against this node (2026-11-19)

Read-only review of new metering work in the company repositories, compared
with this repository's metering. Fresh clones, nothing modified. The golemdb
branch's tests were run; Arkiv was not built.

## golemdb `kf/feat/historical-data-metering`

Merge base `9d805cb` with `main` (`b141dc0`). The branch has 14 commits. Six
are the history work:

- `93a6c65`: the `crates/history` crate.
- `302f375`: cell and index history adapters.
- `9f3a915`: history written at seal.
- `4f7ea41` and `d6fe751`: merged live and history scans.
- `38d7a36` (2026-11-19): "Introduce metering for historical reads".

The other 8 are the unmerged flat-index rework, so `main...branch` mixes two
features.

**History model.** Each item has a `CellHistory[(record,name)]` row holding a
bitmap (`CommitSet`) of the commits that changed it, and a
`CellChangeSet[(commit‖record‖name)]` holding the value just before each such
commit. A read at T uses the pre-image of the first commit after T, or the
live value if there is none (`crates/history/src/history.rs:120-143`). The
bitmap is re-encoded on every change (`history.rs:193-229`) and has no size
ceiling. If it exceeds a backend limit, the transaction fails
(`lib.rs:19-22`).

**What is metered.** Only historical cell reads, specified in
`docs/golemdb-metering.md` §D6.2. The charges:

- a `CellHistory` lookup costs `read.cell`;
- each cursor step costs `read.cell_scan`;
- each bitmap costs `read.commit_set + encoded_len × read.byte`, admitted
  all or nothing (`metering/src/read.rs:18-20,40-42`);
- returned bytes cost `read.byte`.

Storage emits `ReadOperation` callbacks before each step. A full record read
prepays its bytes from the stored `#meta`, and the remaining counters must
reach zero or the read fails with `CorruptState`
(`record/src/historical.rs:58-106`).

**Enforcement.** Every call needs a budget, and each step is checked before
it runs. A refused step is not performed, and earlier charges stay. Historical
reads are priced at current prices, not those at T.

**Determinism.** The doc says (lines 718-722) that different retention
settings give different step counts, byte counts and budget stopping points
for the same logical result. A cost therefore depends on the node's retained
history as well as on state and operation. Pruning is not implemented
(`crates/pruning/src/history.rs` only creates a `history_floor` of 0).

**Unfinished.**

- The public API does not serve reads at a past commit
  (`crates/api/src/lib.rs:124`).
- Historical index (query) scans have the callbacks but no meter, and query
  metering (D6.3) is still a proposal.
- `WRITE_CODE_VERSION` went from 1 to 2 and version 1 is now rejected
  (`metering/src/schedule.rs:8`), so a version-1 genesis no longer opens.

**Tests.** 17 binaries, 197 tests, all passing, including `history_pricing`
on the memory and MDBX backends.

## Arkiv tokenomics branches, #305, estimategas

- `matthiaszimmermann/feat/tokenomics-init` contains all of
  `kfonal/feat/tokenomics-init` (same patch ids) plus 10 commits up to
  2026-11-01. Among them:
  - `a05d342`: `OpCost::FromStore`, which makes GolemDB receipts the whole
    cost;
  - `7fdb0fb`: extend, transfer and delete no longer read the entity.

  Most of this is already on `main` through #255, #258/#273, #259 and #278.
  On `main`, `crates/arkiv-tokenomics/src/meter.rs` charges
  `compute_gas = max(tx_gas + Σop_cost + Σreceipt.cost, floor_gas)`, with one
  GolemDB unit equal to one gas.
- The branch's spec:
  - compute gas follows EIP-1559 with a `min_base_fee`;
  - storage and purge are prepaid through the transaction `value` into a pool
    at `ARKIV_ADDRESS`, which pays nothing out;
  - the purge runs outside the block gas limit, with a 6M budget.

  Open items: read metering for the owner check behind patch, extend and
  delete, and for the purge query (O16), and unfunded storage between expiry
  and purge (O17).
- #305 (`efbccf1`) removes `get_operation_cost` and `get_query_cost` from the
  `StateManager` trait. Pricing is no longer a state-layer concern.
- `sewerynkras/bug/estimategas` (`8490663`) only adds a script reproducing a
  bug. On a chain without Osaka, `eth_estimateGas` for a delete returns the
  whole block gas limit. Every block starts with the purge transaction, so
  that delete is never mined.

## Side by side

| | golemdb / Arkiv | This node |
| --- | --- | --- |
| Metered | Writes, core reads, historical cell reads; queries proposed | Every read and write, 31 classes including queries and proofs (`crates/arkiv-db/src/meter.rs:95-170`) |
| Units | Count × weight, abstract units; 1 unit = 1 gas on Arkiv | `OperationClass` × `PriceSchedule`, model version 3, schedule fixed at genesis |
| Determinism | State, retained history and schedule | Snapshot and operation only; retention refuses reads but never changes a charge |
| Budgets | Required on every call | Counted always; optional for local reads; `ExceedsBlockBudget` at admission |
| Historical reads | Extra charges per bitmap and pre-image, at current prices | The same classes as a head read, at the schedule in that snapshot's header |
| Receipt | Ledger, `priced_at`, `evaluated_at` | Kind, model version, schedule id, spent, outcome, optional ledger; no snapshot sequence |
| Reads inside writes | Open (O16) | Charged in the write receipt |
| Fees | EIP-1559 compute gas; storage prepaid into a pool | Gas at a fixed 1 gwei, burned; rent outside gas, burned |

## Worth adopting

1. **Put the snapshot (sequence or root) a read was evaluated at in read
   receipts.** Without it, a receipt can't be checked as a function of state
   and operation unless the client supplies that context.
2. **Check full-row reads against the row's stored length**, as their
   prepayment counters do, so that corrupt metadata cannot authorise
   unpaid copies.
3. **Decide which prices historical reads use, and document it.** We use the
   old snapshot's schedule; they use current prices, which match the work
   the node does now.
4. **Test that `eth_estimateGas` never returns more than the user's share of
   the block gas**, 90M of 100M, with 10M reserved for expiry. Their bug is an
   estimate that ignored the purge.
5. **Keep pricing independent of retention.** It is the stronger design for
   reproducible quotes.

## Risks in their design

- **Quotes depend on retention.** An archive node and a pruned node quote
  different costs for the same historical read. That breaks reproducibility
  once historical reads reach consensus or pruning exists.
- **A flat `read.commit_set` underprices large bitmaps.** A hot cell's
  bitmap grows with every commit and is re-encoded on each one, while the
  write charge is flat.
- **An oversized bitmap fails the transaction.** If that happened on one node
  and not another, the nodes would split.
- **A version-1 genesis no longer opens**, and the branch is stacked on
  unmerged flat-index work.

## Not verified

- Whether the estimategas bug is fixed on Arkiv `main`.
- golemdb's full workspace, clippy and benchmarks.
- Real bitmap sizes for hot cells, and MDBX's value limits.
- Whether this node charges `ReadByte` before copying on every read path.
