# Public signed RPC demo

Replaced with a fresh funded GLM-account chain 2026-09-25 at https://rpc.experimental.arkiv-global.net.
This is the native signed REST API under `/signed/v1/`, not Ethereum JSON-RPC.
The existing unsigned explorer/full/light panels remain on their separate chain.

- Chain ID: 9005
- Run: `72632ab82db5e2a99746d18ea029eff2`
- Genesis: `0xdae22c6ac0f5a2b4ddfc0384d52ab20cf8de87e1171604053a98d56adb68a347`
- Proposer: `0x3ce9fd397a98418e6fa6717b1dd8c34814251349`
- Blocks: every five seconds, including empty blocks.
- Trust: pinned single-proposer secp256k1; no consensus/finality.
- Gas profile: `glm-v2`; 100M block gas, 90M user gas, fixed 1 gwei price.
- Token: GLM, 18 decimals. Actual fees are deducted and burned; unused gas reservations are refunded.
- Initial allocations: 1,000 test GLM to proposer, 10 to a separate funding wallet.
- No public faucet, mint API or bridge.
- Finite entity lifetimes are required.
- History and engine snapshots: retained without pruning.

The user requested discarding the previous test chain. The superseded chain-9003 and chain-9004 state,
full/light stores and old test artifacts were deleted after replacement
verification. Clients must fetch the new genesis and sign new transactions;
old chain identities and transaction envelopes no longer apply.

## Routing

HTTPS has a dedicated Let's Encrypt webroot certificate. Existing twice-daily
certbot cron renews certificates and reloads nginx. Root redirects to status.

| Public path | Backend |
| --- | --- |
| `/signed/v1/transactions`, account nonces, requests, status, genesis, headers | Producer, loopback19500 |
| `/signed/v1/query/eq`, `/signed/v1/query/resolve`, `/signed/v1/feed/` | Full node, loopback19502 |
| Other `/signed/v1/query/` routes, including text/inspect/verified/classify | Light node, loopback19503 |
| Control and replication | Public404; private loopback19501 with bearer authentication |

Submission accepts canonical signed native envelopes. See [signing guide](signed-proposer-v1.md).
The proxy limits request bodies to516KiB and requests to10/second/IP with burst20;
node limits also apply. No cross-origin browser CORS policy is configured.

## Example

The live acceptance run exercised public HTTPS submissions and verified:

| Case | Result | Gas used | Actual GLM fee |
| --- | --- | ---: | ---: |
| Entity create, block 2 | Committed | 56,713 | 0.000056713 |
| Transfer of 2 GLM, block 3 | Committed | 30,000 | 0.00003 |
| Create limited to 21,000 gas, block 4 | Failed; no entity, nonce consumed | 21,000 | 0.000021 |
| Unfunded sender | HTTP 422 `InsufficientBalance`, nonce unchanged | — | — |
| Pending transactions exceeding balance | HTTP 422 `InsufficientBalance` | — | — |

A separate wallet funded with 0.0015 GLM accepted one future-nonce transaction
reserving 0.001 GLM and rejected a second reservation of that size. Filling its
nonce gap allowed both affordable transactions to commit. Supply reconciled
exactly at block 6: initial 1,010 GLM, burned 0.000292339 GLM, remaining
1,009.999707661 GLM.

This historical account proof is reproducible through the hosted light verifier:

```sh
curl https://rpc.experimental.arkiv-global.net/signed/v1/query/account \
  -H 'Content-Type: application/json' \
  --data '{"actor":"0xc34071bc2ddcd3f12180610ed64ebc3e063a96d6","height":"6"}'
```

The response derives balance, nonce and supply from authenticated account and
burn-counter proofs and reports `verificationStatus.status = verified`. Entity
proofs also verified the successful create and absence of the failed create.

## Operations

Persistent services: `arkiv-signed-producer`, `arkiv-signed-full`,
`arkiv-signed-light`, enabled at boot with restart-on-failure. Private files and
binary are under `/home/ubuntu/.config/arkiv-simulator/signed-rpc` (mode0700);
keys and environment file must never be printed or committed. Active run
directories are `signed-rpc/glm-v2/{producer,full,light}`. The producer starts
with `--gas-profile glm-v2 --allocations-file .../glm-v2/allocations.json`;
followers pin the new chain identity. Allocations must match retained genesis
on every restart. Funding/recipient test keys remain private under `glm-v2/`. Units are in
`/etc/systemd/system/`; nginx site is
`/etc/nginx/sites-available/arkiv-signed-rpc.conf`. Both state/history files are
required for compatible backup/restore. Light retains headers only.

```sh
sudo systemctl status arkiv-signed-producer arkiv-signed-full arkiv-signed-light
```

Update the binary by installing a new file then atomically renaming it over
`bin/arkiv-proposer`; restart all three services. Never replace keys, genesis or
run directories as a restart procedure. Pending pool transactions are volatile;
clients may resubmit identical signed bytes. Included submissions are durable.

## Verification

Implementation commit: `6666c349a9018b54c040fc019e587987cf855529`. The locked
release binary SHA-256 is
`32bcf288b8c16f919d76214d3cb3226bffd3e7b047d5e94ccd506eaddbd1657f`.
Validation passed: 903 Arkiv-v2 workspace tests (three existing heavy tests
ignored), 521 node/database tests without the feature, eight new account tests,
and both funded-account and legacy subprocess smoke tests against the release
binary.

The live public test then passed fee deductions, transfer balances, insufficient
funds, cumulative pending reservation, failed-state rollback, supply reconciliation,
account/entity proofs and restart. Identical resubmissions returned original
receipts after all three services restarted. All roles subsequently agreed on
the same head. The old test data was removed after these checks.

See [public verification evidence](glm-accounts-live-verification.json) and
[account semantics/API](glm-accounts-v1.md). The detailed private artifact is
`glm-v2/public-verification.json`. Current status advertises `feesDebited: true`,
`feePolicy: burn` and the fixed `gasPriceWei`. Included receipts expose
`gasUsed`, `gasPriceWei`, and `feeChargedWei`.

The [proportional base-fee plan](base-fee-v1-plan.md) and
[genesis bundle/prefill proposal](genesis-bundle-design.md) are documented future
changes; this deployment uses fixed-price fees and no prefills.
