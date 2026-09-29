# Node execution and signed replication review — 27 September 2026

Scope: source review of `crates/arkiv-node` signed protocol, execution, accounts,
gas, mempool, HTTP replication and Ethereum compatibility; focused local tests.
This is a code/design review, not a consensus audit or proof of production safety.
No deployment, chain reset, keys, or live balances were changed by this review.

## Finding fixed: an advertised header could be forgotten after a failed fetch

**Medium severity; fixed in `crates/arkiv-node/src/signed/runtime.rs` (`sync_once`).**
A full follower durably remembered the sampled upstream head, but only compared
an advertised intermediate certificate against that sample. It did not retain
that intermediate observation until its block body arrived. If the body fetch
failed, a later authentic conflicting header at that height was not detected.
This weakened the promised signer-equivocation detection; it did not bypass
signature checks or independent block execution.

The full follower now calls `Host::observe_certificate` for every authenticated
advertised certificate before requesting bodies. The existing history catalog
makes each observation durable. A conflict freezes the follower before another
body fetch and remains a conflict when history reopens. No block format, genesis,
root calculation, fee schedule, or execution semantics change.

Regression: `advertised_header_survives_failed_body_fetch_and_detects_later_conflict`
in `crates/arkiv-node/tests/signed_http.rs`. A real local HTTP peer advertises a
height-1 certificate and a height-2 sample; the body endpoint returns 404. The
next poll substitutes a different signed height-1 certificate with the same
height-2 sample. Before the fix the test failed with `StorageUnavailable`
instead of `ChainConflict`. After the fix it verifies no second body fetch,
unchanged accepted head, frozen health and a persistent conflict on reopening.

## Plain-language design for the presentation

### Three jobs, with different levels of trust

- **Producer:** accepts signed requests, orders them, executes the database rules,
  commits the result, and signs the resulting block header. One configured key
  has this role. Its authority is pinned in genesis.
- **Full follower:** downloads the signed bodies through the private replication
  endpoint, checks signatures and request nonces, repeats execution, and compares
  outcomes and the final state root. It independently checks the state transition.
- **Light follower:** retains authenticated headers and checks requested database
  proofs against their roots. It does not replay transactions or check that the
  producer executed every block correctly.

A signature answers “which configured producer endorsed this header?” A database
proof answers “does this result match this committed state?” Full replay answers
“do these transactions produce this state under the execution rules?” Those are
three distinct checks. None supplies multi-party consensus or finality.

Source: `signed/protocol.rs` (`SignedHeader::authenticate`, `SignedBlock::authenticate`),
`signed/host.rs` (`import`, `execute_publish`), `signed/light.rs` (`append`, `header`).

### One transaction, from wallet to committed state

1. A wallet signs either the canonical native request or a supported Ethereum
   envelope. Native signatures bind genesis, chain, nonce, request ID, budget,
   actor and all operations. Ethereum transactions use their standard chain-ID
   signature domain and are mapped deterministically to native operations.
2. Admission authenticates the signature, validates sizes and operations,
   checks committed nonce/request identity, and reserves enough balance for
   pending maximum fees plus outgoing non-self transfers.
3. The volatile pool accepts limited nonce gaps. Production selects contiguous
   sender nonces, ordered by local arrival among ready sender heads. It can skip
   a head that does not fit while continuing with other senders.
4. Execution reserves the fee budget, charges work, applies operations atomically,
   settles actual fees and increments the transaction nonce. A failed operation
   rolls back its transaction's state changes; included failures still pay for
   work and consume the transaction nonce. Entity-create nonces have separate
   rollback semantics.
5. The candidate body is acknowledged in history before state and its body
   reference are published. Only acknowledged publication advances the head and
   removes included pool entries. Exact duplicate retries are idempotent.
6. The full follower repeats validation/execution. The light follower advances
   its signed-header chain, and can then verify proofs for retained snapshots.

Source: `signed/protocol.rs`, `signed/pool.rs`, `signed/host.rs`, `exec.rs`,
`accounts.rs`; durability details are covered by the separate storage review.

### Money, gas and expiry

GLM balances use 18-decimal U256 base units. Genesis allocations supply absent
account defaults; state-root-committed account records override those defaults.
The signed genesis authenticates the allocation table. Fees are fixed at one
Gwei per gas in this profile, and actual fees are burned. Unspent reservations
are returned. There is no automatic refill or unlimited supply: a billion-GLM
faucet balance is very large but finite.

A standalone native transfer costs **21,000 gas**, including when submitted as
a supported legacy or type-2 Ethereum transaction. Multi-operation transactions
retain per-transfer additional charges. Entity writes also pay logical work and
rent for storage amount and lifetime. The active gas profile requires finite
entity lifetimes. Shrinking/deleting an entity does not refund prepaid rent.

Source: `accounts.rs` (`reserve`, `settle`, `transfer`, `reservation`), `gas.rs`,
`exec.rs` (`tx`), `signed/rpc.rs` (transfer estimate tests), `tests/accounts.rs`.

### Bounds are explicit

The default pool retains at most 1,024 entries / 16 MiB / 256 senders / 16 entries
per sender. The nonce window is 16. A signed block allows at most 16 transactions
and 128 operations; envelopes/bodies and HTTP responses have separate size caps.
These are bounded-work policies, not Sybil resistance or guarantees on total RSS.
Pending entries disappear on restart; committed nonce/request records survive.

Source: `signed/pool.rs`, `signed/protocol.rs`, `signed/runtime.rs`.

## Intentional limits and follow-up considerations

- **Single signer, no consensus:** authenticated equivocation is detectable only
  when conflicting signed statements are observed. No fork choice, signer-key
  rotation, or operator unfreeze is implemented. A malicious signer can withhold
  data; a light node cannot independently detect every invalid execution root.
- **Partial Ethereum compatibility:** replay-protected legacy and type-2 envelopes
  are supported; access lists, contract creation, arbitrary EVM execution, and
  many Ethereum methods are not. RPC block/state hashes are native commitments,
  not Ethereum MPT roots. Fixed fees differ from Ethereum fee-market behavior.
  `safe` and `finalized` currently resolve to the local head as compatibility
  aliases, not consensus finality. `eth_syncing` returns false; use signed status
  to reason about actual follower progress. These should be explicit in product
  documentation before general Ethereum-client use.
- **Reset isolation:** Ethereum signatures bind chain ID, not the signed native
  genesis hash. A fresh deployment should continue using a fresh chain ID rather
  than assuming native genesis pinning prevents replay of old Ethereum envelopes.
- **Retention and indexing:** history/snapshots are not pruned. RPC hash indexes
  are built in memory and have an explicit one-million-entry cap. No claim of
  indefinitely bounded disk growth or mature archival indexing is warranted.
- **Scheduler:** deterministic timestamps advance by genesis period; a process
  restart starts a fresh wall-clock schedule, not automatic downtime catch-up.
- **Documentation corrected:** `docs/signed-proposer-v1.md` incorrectly implied
  no Ethereum transaction compatibility after the compatibility layer was added.
  Its overview now describes the limited supported surface and updated header
  observation behavior. Historical deployment documents must be read with their
  dates and chain IDs; they are not evidence of the current deployment.

These limitations were not expanded into protocol redesigns during this review.

## Validation evidence

Initial focused run passed **197 tests**, zero failures:

```sh
cargo test -p arkiv-node --features arkiv-v2 \
  --test signed_protocol --test signed_pool --test signed_history \
  --test signed_host --test signed_http --test accounts \
  --test ethereum_transactions --test gas_execution --lib
```

Coverage includes signature mutation/canonicality, replay of altered roots and
outcomes, both nonce types, rollback/fee burn, full U256 balances, exact transfer
gas, pool reservation/caps, publication retries, retained observations, native
proof binding, and socket-level RPC/replication.

After the fix, signed history (22) and signed host (16) passed. The socket suite
passed **7/7** with `--test-threads=1`, including the new regression. An initial
parallel rerun encountered the existing timing-sensitive
`streamed_submission_caps_private_auth_and_bounded_jobs` admission-saturation
assertion; this unrelated test passed on the initial run and on the serialized
rerun. No production code was weakened to accommodate its scheduling sensitivity.

```sh
cargo test -p arkiv-node --features arkiv-v2 \
  --test signed_http -- --test-threads=1
git diff --check
```

`git diff --check` passed. No live network verification or deployment was performed
by this review agent. The root review coordinates broader checks and release.
