# Native GLM accounts (`glm-v2`)

Every signed chain runs the `glm-v2` profile: balances and actual fee burning. It
uses the [GLM gas schedule](glm-gas-v1.md), a 100M block gas
limit (90M user budget), and a fixed **1,000,000,000 base units per gas**. GLM has
18 decimals. These are native experimental-chain balances; no bridge, mint API,
faucet, rewards or backing relationship is introduced.

A chain created from a [genesis bundle](genesis-bundle-v1.md) adds a sponsored
finite-rent prefill to the same account rules. It creates the namespace at height
0, leaves transaction nonces at zero, and makes block 1 ordinary. A chain created
from an allocations file follows the block-1 bootstrap described below.

## Chain identity and account state

The signed genesis (version 1) carries an account configuration after the signed
limits: `gas_price_wei: u64`, allocation count `u32`, then `(address: 20 bytes,
balance: 32-byte big-endian u256)` pairs, followed by the optional prefill. At most 256 positive allocations are
allowed, sorted by address without duplicates. Their sum must fit u256. The
price must equal 1 gwei. The proposer must cover the bootstrap transaction's
maximum fee (`user_budget * price`) unless the genesis carries a prefill. Invalid
configurations fail before startup.

Both the price and allocations are committed by the signed genesis hash, which
every signed transaction and certificate binds. Stores cannot be converted by
changing CLI flags: use a new run directory/genesis. A supplied allocation file
must also match the retained configuration on restart.

An account consists of a u64 next transaction nonce and a u256 balance. Host key
`arkiv/glm/a/ || address` stores `version:u32=1 || nonce:u64 || balance:32 bytes`.
Until an account is touched, an absent key resolves to nonce zero and its immutable
genesis allocation (or zero for an unallocated address). Thus allocation defaults
are authenticated by genesis and subsequent overrides by the host trie. This
avoids changing the engine's empty-genesis initialization protocol. Accounts are
never removed, including zero-balance accounts: their nonces and spent allocations
must not be reset by falling back to genesis. The independent Arkiv entity creation
counter is unchanged.

Without a prefill, block 1 creates the `entities` namespace using the proposer's signed nonce-0
transaction. It pays its actual gas fee and advances the proposer to nonce 1.
Other allocations remain at nonce 0 until their first included transaction.

## Execution and supply

1. Authenticate the envelope, chain identity, nonce, size and gas limit. The
   minimum gas limit is 21,000. Check that the sender can cover its maximum fee.
2. Deduct `gas_limit * gas_price` before the transaction checkpoint.
3. Execute operations against the remaining balance. Native
   `Transfer { to, amount }` uses operation tag 9. A standalone transfer costs
   exactly 21,000 gas, covered by transaction intrinsic gas. Each transfer in a
   multi-operation transaction adds 9,000 gas to the transaction intrinsic gas.
   Amounts are fixed 32-byte big-endian u256 values in signed canonical bytes. Transfer amounts are not fees.
   An entity operation's rent ([GLM gas](glm-gas-v1.md#charging-rent)) is
   debited from the remaining balance and added to the burned counter; an
   operation whose rent the sender cannot pay fails with
   `InsufficientRentBalance` before writing anything.
4. On operation failure, restore all entity/account operation changes, rent
   debits and rent burns included. Keep metered work, refund unused gas
   reservation and consume the transaction nonce.
5. Add `gas_used * gas_price` to the authenticated cumulative burned counter.
   Fees and rent are destroyed; total supply is initial allocation sum minus
   everything burned.

All additions/subtractions of balances and supply are checked. A self-transfer
checks spendable balance and leaves the value unchanged. A zero transfer does not
create a recipient account. Recipient overflow or insufficient transfer value
fails the operation atomically while retaining metered gas (21,000 total for
a standalone transfer; an additional 9,000 per transfer in a batch). A gas
limit failure charges the work actually metered, including intrinsic gas; it does
not automatically charge the full gas limit. There are no storage-deletion gas
refunds. Unused reserved transaction gas is refunded.

Native transfer effects use engine effect tag 8 (`recipient:20 || amount:32`);
charged account rejections use operation-outcome tag 7 (`fault || receipt`),
and unsupported account operations use host-fault tag 5. Account fault subtags
are 1 insufficient balance, 2 overflow, and 3 unsupported. The status field
`refunds:false` means no storage refunds; explicit `storageRefunds` and
`unusedGasRefunds` fields distinguish the two.

The volatile pool reserves the sum of maximum fees **plus gross non-self outgoing
transfer values plus quoted rent** for every pending transaction from a sender,
including nonce gaps. The rent quote prices the transaction's entity
operations in order on the head state, each seeing the earlier ones of the
same transaction, with a 64-block height margin; execution checks the real
rent. A transaction whose gas reservation an earlier transaction's rent
consumed is left out of the block being built, and after every commit each
sender's pending transactions that its new balance no longer covers are
dropped, with a status and receipt error naming the reason
([charging rent](glm-gas-v1.md#charging-rent)). It does not count uncommitted incoming transfers or prospective fee refunds
as funds. This conservative rule can reject a sequence that would eventually be
affordable, but prevents accepted transactions from overcommitting a balance.
Duplicate submissions do not reserve twice. Entries are removed only after an
acknowledged commit or such a drop. Full nodes independently perform the same execution and fee
settlement; transaction admission or a producer signature cannot authorize an
invalid debit. State, burn counters, nonces and receipts share the existing atomic
publication/recovery boundary, including lost-ack recovery.

## CLI

Use a map of addresses to **decimal strings in base units**:

```json
{
  "0x1111111111111111111111111111111111111111": "1000000000000000000000",
  "0x2222222222222222222222222222222222222222": "10000000000000000000"
}
```

Generate your own keys and substitute the actual proposer/account addresses.
Duplicate JSON/address entries, non-string amounts, zero allocations, nondecimal
amounts, and out-of-range amounts are rejected.

```sh
arkiv-proposer producer --run-dir NEW_DIR --key-file PROPOSER_KEY \
  --allocations-file allocations.json --chain-id 9005

arkiv-proposer transfer --key-file SENDER_KEY --genesis-file NEW_DIR/genesis.bin \
  --nonce 0 --request-id 0xREQUEST_ID_32_BYTES --to 0xRECIPIENT_ADDRESS \
  --amount-wei 1000000000000000000 --gas-limit 21000 --out transfer.bin

arkiv-proposer submit --peer http://127.0.0.1:9500 --transaction-file transfer.bin
```

The producer starts paused unless `--start running` is provided. The private
control endpoints advance the bootstrap and later blocks. The `create` and
`sign` commands also use the funded account's transaction nonce, which
`accounts/ADDRESS/nonce` reports.

## Account reads, proofs and receipts

- `GET /signed/v1/accounts/ADDRESS?height=N` returns `balanceWei`, `nextNonce`,
  `gasPriceWei`, `initialSupplyWei`, `burnedWei`, `totalSupplyWei`, `height` and
  `blockHash`. Omitted height selects the current head. Current-head responses add
  advisory `pendingReservedWei`; that reservation is volatile, not a proof claim.
- `GET /signed/v1/accounts/ADDRESS/proof?height=N` adds `accountProof` and
  `supplyProof`. Each proof carries its state header and bounded host-trie path.
  The proof verifier checks the selected root, exact key, canonical Blob framing,
  proof/node sizes, exact consumption and all trie links. Membership and
  nonmembership both work, including genesis allocations and zero-balance records.
- `POST /signed/v1/query/account` on the light node with
  `{"actor":"0xADDRESS","height":"N"}` fetches both paths, verifies them against
  an accepted proposer-authenticated header, derives every balance/supply field
  locally and returns `verificationStatus.status: "verified"`. The provider's
  declared balances are ignored. The light node checks its accepted catalog again
  after the network await. This has the same pinned-single-proposer trust model as
  entity proofs; it is not an execution or consensus proof.
- Included submission/request receipts expose decimal-string `gasUsed`,
  `gasPriceWei` and `feeChargedWei`. Their values are derived from retained execution
  gas and the immutable account profile. Duplicate submissions/restarts return the
  original receipt.

## Validation

`cargo test -p arkiv-node --test accounts` covers fee/refund
arithmetic, maximum u256 supply, transfers, self/zero transfers, insufficient
funds, pending reservations, failure rollback, replay, membership/nonmembership,
malformed proofs, identity/version pinning, restart and uncertain-publication
recovery. `python scripts/glm-accounts-smoke.py` runs isolated producer/full/light
subprocesses and redb stores, including real HTTP, CLI authoring, entity/account
proofs, fee/supply reconciliation and offline reopen. It never touches a retained
deployment.
