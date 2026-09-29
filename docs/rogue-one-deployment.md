# Rogue One: fresh 21,000-gas transfer chain

The 2026-09-26 reset replaces chain 9008 with **chain 9009**. Plain native
transfers now cost exactly 21,000 gas, including transactions submitted in
legacy or EIP-1559 Ethereum encoding. `eth_estimateGas` returns 21,000 for
plain transfers. Transfers inside multi-operation native batches still charge
9,000 gas each in addition to the transaction intrinsic charge.

This is a fresh-genesis execution change, not a migration of old receipts or
state. Keep the old executable with old chain data; replaying chain 9008 with
this executable is unsupported. Wallets must use chain ID 9009 and new nonces.
The chain remains a signed single-proposer test network, with two-second blocks,
18-decimal native test GLM, and fixed 1 gwei gas fees that are burned.

## Endpoints and allocations

| Endpoint | Purpose |
| --- | --- |
| `https://rpc.rogue-one.eu/` | Producer HTTP/WebSocket JSON-RPC |
| `https://full-node.rogue-one.eu/` | Full replay follower and node panel |
| `https://light-node.rogue-one.eu/` | Light proof verifier and node panel |
| `https://explorer.rogue-one.eu/` | Explorer with fresh indexing storage |
| `https://rogue-one.eu/faucet/` | Password-gated internal faucet |

The dedicated faucet account is
`0x18721D6E737EEA730f02c99507A471FB1e43Df97`, allocated
**1,000,000,000 GLM** at genesis. Existing proposer, treasury, tester and baseload
wallet addresses retain their previous genesis allocations. Total initial
supply is **1,002,020,000 GLM**; this reserve is finite, not a mint facility.

The faucet uses unmodified `Arkiv-Network/internal-faucet` commit
`d8308936b0c37a3a81881f36ce1aa62ff6039d28`. Each authenticated drip sends 100 GLM
through the producer, using its existing hardcoded 21,000-gas EIP-1559 sender.
One service instance serializes sends. The UI displays a submitted transaction
hash; check its receipt to confirm inclusion. There are no cooldowns or
per-address limits. The reserve covers just under ten million 100-GLM drips
including transaction fees.

## Deployment and credentials

Host-private deployment directory: `/home/ubuntu/.config/rogue-one-9009`.
The `rogue-one-current` symlink selects the active deployment. `compose.json`
starts node, indexer, explorer, panels, PostgreSQL, Redis and faucet services;
`identity.json`, `bundle/report.json` and `release.json` record public trust pins,
genesis allocation totals and image/binary identities.

The faucet binds host loopback `127.0.0.1:19504` and submits to the producer on
`127.0.0.1:19500`. Nginx provides HTTPS at `/faucet/`, strips that prefix upstream,
rewrites the two form actions and login redirect, and scopes its secure session
cookie to `/faucet/`. Other public routes and metrics remain at their existing
origins. The upstream `/healthz` checks HTTP liveness only.

Secrets are local mode-0600 files, not repository contents. `faucet.env` supplies
its funding key and generated login password; `faucet-password.txt` contains
only the login password for operators. `keys/faucet.key` is the funding key.
Never print, commit or put these credentials in URLs. Existing Google explorer
login settings and baseload mnemonic are retained in private environment files.

Operate with `sudo /home/ubuntu/.config/rogue-one-current/compose ...`.
Do not recreate genesis or delete data/volumes during an ordinary restart.
The stopped chain 9008 deployment and volumes remain available for recovery;
its services share host ports and must not run beside chain 9009.

## Verification

Deployment evidence is recorded privately in `live-verification.json` and
`faucet-verification.json`, containing public transaction hashes and checks,
not credentials. These cover exact 21,000-gas signed legacy/EIP-1559 transfers,
fee/balance accounting, full replay, light account proofs, WebSocket RPC, SDK
entity creation, fresh explorer indexing, and an authenticated 100-GLM faucet
drip. Source tests also cover multi-operation transfer metering and rollback.

Verified deployment identity:

- Chain ID: `9009`
- Genesis: `0x6dc9df7aad9f879f56b85387b2afe6c7c1822d13f6ecb97f35f8d20d180f91e3`
- Run ID: `d7a8aca9d0ffc2ccfd819d91bbc6d468`
- Faucet verification transaction: `0x889aec6d784a2b345f1e2b6e7294a8fad060f12a0b4bf73bff9765141cbb853e`
- Faucet receipt: block 93, 21,000 gas, 0.000021 GLM fee.

Validation passed 962 workspace tests (three heavy tests explicitly ignored),
the isolated native-account process smoke test, and upstream faucet tests under
the Go race detector. Live verification passed both Ethereum transaction types,
SDK creation, follower replay/proofs, indexing, authentication, and exact faucet
fees. Restart verification preserved genesis, all tested transfer receipts and
faucet balance across all three node roles. Public metrics remained available.
