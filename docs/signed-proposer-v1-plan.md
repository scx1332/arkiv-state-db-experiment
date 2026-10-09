# Signed proposer and transaction admission v1

Status: approved and implemented; see [the implementation guide](signed-proposer-v1.md)
for commands, operating limits and validation evidence. This plan is based
on engine commit `e12a364`; preserve its query parser/classification changes.
It concerns a new, explicitly selected signed chain profile. It does not reset,
migrate or deploy over the retained unsigned simulator.

This plan is the design record of that step. The node has since been unified:
there is no `arkiv-v2` cargo feature or separate generic protocol (the "legacy"
codecs and the feature gate it mentions no longer exist), every signed chain is
`glm-v2` with funded accounts, and the signed genesis has a single version.
The [implementation guide](signed-proposer-v1.md) describes the current code.

## Decision

Use Alloy's primitive hash/address/signature types and its k256 implementation
for transaction and header signatures. Add a small bounded, nonce-aware pool
and construct blocks through the existing Arkiv executor and durable engine.
Keep our canonical engine, entity operations, metering, rollback, expiry,
state-root layout and Eq proof format. Reuse Reth's separation of admission,
pool ordering and payload construction as a design reference; do not add the
Reth transaction pool dependency in this first implementation.

Transactions and blocks receive native Arkiv signing envelopes. They are not
Ethereum transactions, Ethereum JSON-RPC or Ethereum consensus. A full follower
authenticates the proposer and independently executes every signed transaction.
A light follower authenticates the proposer and verifies requested proofs
against accepted state roots; it does not execute blocks. A proposer signature
does not establish consensus, finality, availability, fair ordering or absence
of equivocation. Two conflicting authenticated headers must stop a follower.

Store retained canonical transaction/block bodies in a separate physical
`history.redb` file owned by the node. `node.redb` retains engine state and a
bounded header/feed/body-reference/inclusion envelope, without serialized
transaction operations. PostgreSQL remains a metadata/query projection. Live
entity payload fields and retained engine snapshots still contain materialized
state; removing the duplicate transaction log does not prune those snapshots.
No history or snapshot pruning is implemented in v1.

## Existing seams and constraints

* `block.rs::TxEnvelope` currently records chain ID, a caller-supplied 20-byte
  actor, request ID, transaction budget and operations. There is no signature
  or transaction nonce. `validate.rs` checks structure/budgets/operations but
  cannot establish the actor's authority. `identity.rs` tracks request IDs;
  simulator inclusion records make included request-ID rejection durable.
* `arkiv::nonce_key` uses the 32-byte key `arkiv/nonce/ || caller[20]`. It counts
  successful entity **Create operations**, feeds `derive_entity_key`, and is
  rolled back with a failing transaction. It is not an account transaction
  nonce. Changing this counter to mean transactions would change entity keys
  and existing Arkiv behavior.
* `exec.rs::execute_block_observed` checks native inputs, runs admin/expiry/txs,
  rolls back failed txs, calls a read-only terminal observer, then seals. Its
  `Executor::tx` checkpoint and rollback provide a precise insertion point for
  account nonce advancement after the transaction outcome has been decided.
* Root-bearing native headers are 156 bytes and already bind parent, height,
  timestamp, input digest, outcome digest and terminal state. Arkiv-v2 uses the
  existing header domain `arkiv/header/v3`. Preserve this header/proof ABI.
* `simulator::Host::execute_publish` independently executes imports and compares
  the complete expected record, then publishes only after `Committed`.
  `StoredBlock::bytes`, however, currently puts the full native `Block` inside
  the engine's opaque host envelope. That format cannot be reused unchanged
  for the new storage requirement.
* The durable engine's simulator composition API accepts bounded opaque host
  bytes and typed request/control inclusions. Its metadata can distinguish a
  new host profile without changes to trie state, manifests or preparation.
  Signed and unsigned opens must reject each other's metadata magic/version.
* The existing ed25519 `demo::auth` is a separate, memory-oriented demo with a
  different trust profile. Do not label that signature as secp256k1 or combine
  its trust keys with the new profile.
* Current simulator workloads hardcode ALICE/BOB addresses and derive entity
  keys from those addresses. Those addresses are not controlled by the new
  signers. Implement a separate signed workload using real signer addresses.

## Dependencies and the Reth decision

Preferred minimal dependency declaration:

```toml
# Existing workspace dependency, pinned to the already locked primitive version.
alloy-primitives = { version = "=1.7.3", default-features = false,
                     features = ["std", "map-foldhash"] }

# Only arkiv-node enables ECDSA support.
alloy-primitives = { workspace = true, features = ["k256"] }
k256 = { version = "=0.13.4", default-features = false,
         features = ["ecdsa", "std"] }
```

`alloy_primitives::{Address, B256, Signature}` plus
`k256::ecdsa::SigningKey` are enough. Alloy primitives does not publicly
re-export k256; the direct dependency is necessary for signing. Both versions
above are already present in this repository's lockfile. Do not add an
umbrella Alloy provider, consensus or network dependency just to sign a hash.
Keep the current Rust 1.90 declaration; primitive 1.7.3 declares Rust 1.85.
Run the existing workspace vectors after any dependency resolution change.

The alternative `alloy-signer = "=2.3.0"` plus
`alloy-signer-local = { version = "=2.3.0", default-features = false }` gives
`PrivateKeySigner`/`SignerSync::sign_hash_sync`, but the upstream 2.3.0 workspace
requires Rust 1.94.1 and signer-local imports consensus/network. Local rustc
1.96 can compile it, but using it would deliberately raise the repository's
MSRV. The neighboring `/arkiv` manifest says `2.3` (a compatible version range,
not an exact pin), and the local cache contains signer-local 2.4.1. These are
not evidence that a 1.90 build is compatible.
[Alloy 2.3.0 manifest](https://github.com/alloy-rs/alloy/blob/v2.3.0/Cargo.toml),
[signer-local dependencies](https://github.com/alloy-rs/alloy/blob/v2.3.0/crates/signer-local/Cargo.toml).

Reth v2.5.0's `TransactionPool::Transaction` requires `EthPoolTransaction`;
`PoolTransaction` extends Alloy's Ethereum `Transaction` trait and associates
`SignedTransaction` types. The API includes EIP-2718 decoding, Ethereum fee/cost
fields and blob sidecars. Its Cargo dependencies include EVM, Ethereum EVM,
chainspec, chain state and storage APIs. Adapting our native operations would
require fake Ethereum fields or a substantial extraction/fork, plus extra
validation we still need for our budgets, payloads and state. For a fee-free
single proposer and the explicit limits below, that integration exceeds the
small pool it replaces. This is a source-based integration assessment, not a
claim that Reth is unusable for custom chains.
[Reth pool traits](https://github.com/paradigmxyz/reth/blob/v2.5.0/crates/transaction-pool/src/traits.rs),
[Reth pool dependencies](https://github.com/paradigmxyz/reth/blob/v2.5.0/crates/transaction-pool/Cargo.toml).

## Canonical signing and chain identity

Use the existing strict big-endian `Encode`/`Decode` codec, exact decoding and
`arkiv_db::digest(domain, bytes)` framing throughout. JSON and hex are transport
only. Give all new host types their own fixed magic/version and golden vectors;
do not reinterpret legacy `TxEnvelope`, `BlockInputs`, `Genesis` or `Block`
bytes as signed data. Compile the initial signed profile only with `arkiv-v2`.

`SignedGenesisV1` canonically contains:

1. `u32(1)`, inner native `Genesis` with the Arkiv-v2 protocol/layout;
2. a cryptographically random `run_id[16]`, generated once and retained;
3. authorized proposer `Address[20]`;
4. `u64(block_period_ms)` and signed-profile limits below.

The inner genesis has no admins and `max_admin_per_block = 0`. Signed v1 admits
no admin section, so a proposer cannot smuggle an unsigned `SetPricing` past
transaction authentication. A later signed admin protocol is a separate
version. Workload configuration remains bounded host metadata and has no
authority to change canonical pricing or signature/nonce rules.

Reserve namespace ordinal one using a protocol-checked first block: exactly
one signed transaction from the pinned proposer, nonce zero, with the single
operation `CreateNamespace("entities")`. It uses ordinary static validation,
budget execution, pool admission and account nonce consumption. The producer
constructs/signs/enqueues it locally; external admission returns
`ChainInitializing` until that block commits. Full replay verifies this first
block rule independently. This avoids a public submitter allocating namespace
one under an unrelated name before entity initialization. After initialization,
the existing generic-operation restrictions and Arkiv namespace protection
remain in force; additional legitimate namespaces need no new special rule.

Construct the signed genesis native header from the inner genesis header, but
replace `inputs_digest` with
`digest("arkiv/signed-genesis/v1", encode(SignedGenesisV1))`. Its native
`Header::hash()` is the signed chain's genesis hash. It is **not**
`inner_genesis.hash()`. This commits to the proposer, run identity, execution
parameters and new host protocol without a signer/genesis circular hash. The
genesis state remains the existing empty engine state. Everywhere that accepts
proof snapshot identity or pins a peer uses this signed genesis hash.

`SignedTxV1` canonically contains:

```text
u32(1) || genesis_hash[32] || nonce[u64] || tx:TxEnvelope || signature[65]
```

Its signing preimage is all fields before the signature, under
`arkiv/signed-tx-intent/v1`. `TxEnvelope` already commits chain ID, claimed actor,
request ID, budget and every operation in order, including namespace, entity
key/salt, payload and attributes. Retain the actor field for compatibility, but
require it to equal the address recovered from the signature. Never take actor
authority from a request URL, JSON header, server signer or pool entry.

The transaction hash is
`digest("arkiv/signed-tx/v1", encode(SignedTxV1))`. The intent digest and signed
transaction hash are different values. Inclusion records and signed feed
transaction digests use the signed transaction hash. Request-ID idempotency is
still scoped by chain/actor and compares this hash; changing nonce or signature
under an existing request ID is a conflict.

`SignedInputsV1 = u32(1) || Vec<SignedTxV1>`. Its digest uses
`arkiv/signed-inputs/v1`. `SignedBlockV1` retains a native `Header`, these signed
inputs and native `BlockOutcomes`. The native header's `inputs_digest` is the
signed-inputs digest, so the block hash commits complete signed transactions,
their order and nonces. No separate unauthenticated list of signatures exists.
The native executor's temporary unsigned input digest is replaced only by the
signed execution entry after it has validated the one-to-one conversion; full
replay uses that same signed entry. Never use generic native `replay` to claim
signed-block validation.

`SignedHeaderV1 = u32(1) || genesis_hash[32] || Header || signature[65]`.
The proposer signs
`digest("arkiv/signed-header/v1", u32(1) || genesis_hash || Header::hash())`.
The signature is detached from the native header hash; all fields it
authenticates are transitively committed. The genesis also receives this
certificate. A different valid signature for the same header is not a
conflicting block; compare native header hashes/bytes for equivocation.

Signature encoding is exactly `r[32] || s[32] || y_parity[1]`, with parity only
0 or 1. Reject 27/28, EIP-155 parity, compact signatures, DER, trailing bytes,
zero/out-of-range r or s, high-S and failed recovery. Sign the already computed
32-byte digest, not a personal-sign string and not a second arbitrary hash.
Use deterministic k256 prehash signing and verify its produced encoding with
the same validator. Never silently normalize received signatures.

This last check is essential: Alloy 1.7.3 recovery internally normalizes high-S
and its raw parser accepts more parity forms than this protocol. Reject
`signature.normalize_s().is_some()` and invalid original parity **before**
recovery; use k256 scalar parsing or recovery to reject invalid scalar ranges.
Do not assume `Signature::as_bytes()` has our parity convention: explicitly
encode the two scalars plus `u8::from(signature.v())`, or use the inspected
`Signature::as_rsy()` method.
[Alloy signature implementation](https://github.com/alloy-rs/core/blob/v1.7.3/crates/primitives/src/signature/sig.rs).

## Account nonce and execution

Reserve a distinct 32-byte host-state key, for example
`b"arkiv/txnon/" || actor[20]` (12 prefix bytes), with exactly eight big-endian
bytes of value. Missing means next nonce zero; malformed stored values are
corruption. Do not reuse `arkiv/nonce/`. Nonce changes are part of the existing
host trie and therefore the state root and durable publication, not an
uncommitted runtime map or a PostgreSQL counter.

For each signed transaction in block order:

1. Authenticate the full envelope and run the existing static argument checks.
2. Require its nonce to equal the next nonce in the candidate state. Disallow
   nonce `u64::MAX`; checked increment may produce `u64::MAX`, permanently
   exhausting that actor after its last usable nonce. Never wrap.
3. Execute its operations with the existing transaction checkpoint/rollback.
4. After that rollback decision, increment the transaction nonce once, whether
   the outcome is committed or failed. Invalid authentication/nonce/structure
   is an invalid block, not a charged failed operation. An otherwise valid
   included transaction that fails ownership, expiry, existence or budget
   execution consumes its nonce and request ID.

Add a signed-specific execution entry sharing the existing implementation.
An internal post-transaction hook or explicit signed nonce context is sufficient
to write the new nonce after `Executor::tx` returns and before terminal
metadata observation/sealing. Legacy entry points use no-op behavior. Do not
expose a general caller-controlled mutation escape hatch on the HTTP surface.
Nonce host operations are unmetered like existing host bookkeeping; receipt
sums and operation budgets retain their present meaning. CPU authentication
work is bounded separately by admission and block limits.

Full validation repeats authentication, nonce ordering, request-ID duplication
checks, resource limits, proposer certificate, timestamp/parent continuity,
engine execution, receipts/outcomes, state root and sanitized feed generation.
It cannot trust recovered actors or successful validation labels supplied by
the proposer. Invalid input late in a block discards the whole candidate.
Producer and full follower must arrive at the same header and all canonical
body bytes. Signature validity plus a body hash cannot recreate missing
operations; independent replay requires the retained canonical body.

## Mempool and bounded admission

Initial local pool policy: at most 1,024 entries, 16 MiB retained canonical
bytes, 256 senders and 16 entries per sender. A nonce may be in
`committed_next .. committed_next+15`, with checked/saturating bounds.
Gaps are accepted as queued, within those limits. Account state reads come
from the currently acknowledged engine snapshot. Track total decoded/auxiliary
memory as bounded by both entry and byte caps; do not claim the byte cap is
the process RSS cap.

Admission order is bounded HTTP read, strict decode/cap check, chain/genesis
check, canonical signature/recovery, static validation, durable duplicate
lookup, nonce window, then pool insertion under the writer/admission lock.
Use at most four concurrent signature-validation jobs and a bounded request
deadline. Recheck head/nonce and capacity inside the lock before insertion.
There is no fee market, balance or payment promise in v1.

Exact duplicate signed bytes return their existing pending/included result.
A different hash at the same actor+nonce is `NonceConflict`; a different hash
at the same actor+request ID is `RequestConflict`. Replacement is disabled.
This is an intentional first policy: no invented fee bump or budget-based
priority. Reject capacity exhaustion explicitly and retain existing entries.
Pending entries are volatile; a restart clears them and clients resubmit the
same signed bytes. Included records and committed nonce survive restart.

Assign monotonic local arrival numbers with checked increment. At block
construction, consider each sender's contiguous ready head and choose ready
transactions by arrival order. Selecting nonce N exposes that sender's N+1.
If a transaction cannot fit the remaining block budget/bytes/operations, defer
it and its descendants and continue with other ready senders. Selection has
bounded work proportional to pool limits. Replay validates the recorded order
and limits; arrival order/fairness is not consensus-verifiable. A valid pool
entry must fit an otherwise empty block.

Hold selected entries until definitive engine publication. Definite or
indeterminate engine failure fences production and retains the exact candidate;
do not release sender nonces for replacement or build a second candidate while
the first can still become canonical. Remove included entries and refresh
ready/queued state only on acknowledged commit. A client timeout is not a
transaction rejection or evidence of non-inclusion.

## Signed profile size limits

Keep the existing engine namespace/cell/value/expiry limits, including the
128 KiB payload limit, and the existing 16 txs/16 ops per tx/128 ops per block
and 100,000,000 user-budget example. For this profile choose:

| Limit | Initial value |
| --- | ---: |
| Complete canonical signed transaction | 256 KiB |
| Complete canonical signed inputs | 768 KiB |
| Complete retained signed block/certificate body | 1 MiB |
| Sanitized feed | Existing 4 MiB |
| Engine host envelope | Existing 8 MiB framing cap |
| Replication response | Existing 8 MiB actual streamed bytes |
| Transaction submission JSON | `2 * 256 KiB + 4096` bytes |

Set the inner genesis native tx/input limits consistently and enforce the
stricter signed limits on actual encodings. Include signature/wrapper overhead
in selection. A five-create transaction with five maximum payloads does not
fit the signed tx limit; split demo work into separate signed transactions.
Candidate publication must independently check actual body, feed, envelope
and replication response servability before committing anything. Submission
gets its own route limit; keep the smaller existing query/control request
limits. Chain parameters may be tightened during implementation if vectors
demonstrate a necessary bound, with the exact values committed by genesis.

## Separate durable history and publication ordering

Implement a narrow node-owned `HistoryStore` around a second `RedbStore`, or
an equally narrow dedicated redb adapter. Reusing `VersionedStore`'s tested
commit outcomes is preferred. With that adapter, store framed bodies as
content-addressed objects in this second file (`SpaceId::Host` is a storage
table name here, not engine host state). Its object ID must follow the
adapter's contract: raw `keccak256(framed_body_bytes)`. The frame contains a
fixed signed-history magic/version and the canonical signed block/certificate;
verify both hash and frame on every read. Record length and object ID in the
engine metadata reference. The history store has its own profile, chain/run
identity and monotonically increasing storage commit sequence, independent
of chain height.

Do not key history bodies solely by height: an orphaned candidate must not
prevent a later valid candidate at that same height. Content addressing makes
an exact retry idempotent and lets unreferenced candidates coexist. No orphan
deletion or compaction is needed now. A body being present in history is never
by itself evidence of canonical inclusion.

Replace the signed engine host envelope with a versioned native header and
proposer certificate, body reference, sanitized feed, workload/config revision
and bounded control receipts. It must not contain `BlockInputs`, `SignedTxV1`,
raw operations, payload bytes or arbitrary decoded transaction JSON. Ordinary
native engine state values remain unchanged. Existing typed request-inclusion
keys can retain `(height,index,signed_tx_hash)` without changing the durable
engine's narrow write contract.

Publication sequence:

1. Validate/sign/execute the candidate; compute body bytes/reference and full
   metadata; check every size/servability limit.
2. Prepare the engine candidate with the small reference-bearing host packet.
   Preparation does not publish it. Retain exactly these candidate bytes.
3. Persist the referenced history body and receive a definite durable success
   from the history store. On not-committed or uncertain history outcome,
   fence this publication; do not call engine commit. Recovery can retry the
   identical body only under the history adapter's reconciliation contract.
4. Commit the prepared engine candidate. Only `Committed` advances visible
   head, visible inclusion, runtime account nonce and the pool.
5. On engine uncertainty, retain the exact prepared packet and existing
   durability-unknown behavior. Never infer success from a future stored body
   or from observed-but-unacknowledged engine records.

A crash before engine publication can leave an orphan body. A correctly
ordered committed engine reference cannot point to a body that was never
durably acknowledged. Persist fresh history-file initialization before
referencing it; do not weaken the existing adapter's file-creation durability
contract. Backup/restore must keep the referenced history file and engine
file from a compatible retained set.

On reopen, validate both file profiles and exact chain/run identity, open the
engine at its acknowledged head, then validate the referenced head body.
This is bounded and does not replay or scan historical transactions. Old
body references are verified lazily on access. An absent/wrong/corrupt body
for a committed reference is `HistoryUnavailable`/corruption and cannot be
silently treated as an empty block. V1 refuses to serve full-node replay
without the promised body. Engine nonce state, inclusion indexes and headers
remain authoritative; pending pool contents are not reconstructed from orphans.

All history is retained in v1. The future pruning boundary must distinguish
header retention, transaction-body retention, receipt metadata and engine
snapshot retention. After body pruning, old headers/proofs may still verify,
but full replay from before the retention boundary requires another body
source or a separately trusted snapshot. A hash/signature cannot recover
pruned calldata. Do not expose a pruning flag until those semantics exist.

## Trust, HTTP and workload

Provide a separate signed mode/binary, for example `arkiv-proposer`, with
producer/full/light roles. Existing unsigned simulator commands remain valid
only for their retained unsigned runs. Producers load a secp256k1 secret from
a local key file or explicit process environment; never log it, return it from
status or accept it in public requests. An explicit dev-key option may exist
for local fixtures, visibly labeled and never used as an implicit default.
Reopening with a different signer fails; key rotation needs a new version.

Fresh followers explicitly pin chain ID, signed genesis hash and proposer
address (and existing source/run identity for feed association). Retrieve
genesis/config over the configured peer only, recompute the hash and verify
its certificate before initializing persistent state. Persist pins. Restart
uses them, rejecting changed command-line identity rather than trusting the
peer's advertised replacement. Proposer identity is never accepted solely
from the same untrusted response it authenticates.

Persist signed certificates in the light catalog. Authenticate before accepting
or recording conflicts, then enforce height, parent and logical timestamp
`genesis_time + height * period`. Check arithmetic. Store both authenticated
conflicting certificates and a durable frozen marker; invalid signatures do
not freeze the chain. Refuse continued acceptance after reopen until an
explicit future recovery policy exists. There is no fork choice in v1.

Keep existing proof requests/header bytes and query parser/classification.
Feed metadata can retain its closed shape: `transactions[].digest` becomes
the signed tx hash in the signed profile, and provenance/status explicitly
advertises `signed-proposer-v1`, authenticated header trust and retained body
availability. Carry header certificates in a signed response wrapper, not
inside encoded native `Header`. The indexer must pin the new profile/identity
and must not call a signature authenticated feed consensus-final. No calldata
or proof witness ingestion into PostgreSQL is added. Separate feed digests
remain transfer consistency checks, not proofs of redacted metadata.

Minimum submission surface, using a versioned signed prefix:

| Route | Contract |
| --- | --- |
| `POST /signed/v1/transactions` | Closed JSON `{transactionBytes:"0x..."}` carrying exact canonical signed bytes; producer validates/enqueues; returns signed hash, actor, nonce, request ID and pending/included status. |
| `GET /signed/v1/accounts/:actor/nonce` | Committed next nonce, optional contiguous pending next nonce, and sampled acknowledged header identity; pending is advisory. |
| `GET /signed/v1/requests/:actor/:requestId` | Pending/unknown or durable inclusion with signed hash, height, index and execution outcome. |
| Existing public header/feed/proof routes in signed mode | Signed profile identity/certificate wrapper, bounded headers, sanitized metadata and complete intentional proof witnesses. |
| Private canonical-body replication route | Reads referenced `history.redb` body only at acknowledged heights, with existing separate bearer-protected listener. |

An actor/request-ID status lookup reuses the existing durable inclusion key.
A global hash-only transaction index is optional future work; do not add an
unbounded RAM index or a history scan for it. Full/light nodes either reject
submission with an explicit producer-only result or relay to a configured
producer with correct response semantics; v1 should choose the simpler reject.
Signing authorizes operations, not endpoint availability; bounded public
admission does not claim Sybil-resistant spam protection.

Provide a Rust signing/submission helper or CLI command that fetches/pins
genesis, reads nonce, constructs canonical operations, signs locally, submits
and polls the explicit request status. Demonstrate at least two independent
keys: signed create, transfer, new-owner update, wrong-owner failed transaction,
duplicate submission and restart resubmission. A dedicated workload signs
with these keys and passes through the same admission/pool path. It cannot
insert forged actor envelopes directly into block construction. Resolve entity
keys using the preserved successful-create counter or returned receipts; do
not assume account transaction nonce equals entity creation nonce. Include
empty blocks and multiple pending transactions from a sender.

For deterministic fixtures, use known test keys, fixed logical timestamps and
explicit signed genesis bytes. Keep default live/demo workload stopped or
opt-in as the current simulator does. No live deployment/reset is part of
this implementation handoff.

## Implementation sequence and acceptance evidence

1. Add an Arkiv-v2 signed module with exact codecs/genesis/signature validation
   and golden vectors. Keep legacy codecs and root vectors passing.
2. Add the signed executor entry and durable transaction nonce semantics;
   verify independently replayed native outcomes/root plus signed input hash.
3. Add the bounded pool and tests using real signed envelopes and state nonce.
4. Add separate content-addressed history storage and small engine host
   envelopes; exercise both store commit state machines before wiring HTTP.
5. Add producer/full/light signed runtime/CLI and submission helper/workload;
   expose accurate capability/trust/retention status. Adapt indexer profile
   handling only where necessary to observe the new feed; SQL query work is
   outside this plan.
6. Run targeted signed tests, both legacy and Arkiv-v2 workspace checks, and a
   fresh-directory local producer/full/light smoke with restart. Commit/push
   only the authorized repositories according to their working agreements;
   do not start or replace deployed services.

Required tests are behavior/security evidence, not merely implementation
mirrors:

* Hand-built canonical preimages and known secp256k1 vectors: exact signing
  digest, recovered actor, signed tx hash, signed genesis, input digest and
  header certificate. Roundtrip, every truncation, trailing bytes, unknown
  versions and hostile counts fail correctly. Confirm parity and high-S
  rejection independently of the recovering API.
* Mutating genesis/chain, actor, nonce, request ID, budget, operation ordering,
  namespace/key/salt, attributes or payload invalidates the original signature.
  Cross-run replay with the same chain number fails. A proposer cannot replace
  the full body or attach signatures from unrelated transactions.
* Exact duplicates are idempotent; changed request IDs with used nonce fail;
  changed content at a pending nonce conflicts; future gaps do not execute;
  same-block contiguous nonces do execute. Nonce overflow fails closed.
* A failed multi-operation transaction rolls back entity changes and entity
  creation nonce, consumes one transaction nonce, and leaves the expected
  receipt charges. A storage failure consumes no published nonce. Multiple
  creates in one tx distinguish the two counters.
* Restart restores committed nonce/inclusion from state without a replay scan,
  drops only volatile pending entries, and permits identical resubmission to
  return included status. Full replay independently reproduces these values.
* Pool count/byte/sender/window limits, HTTP streamed-byte cap, signature
  concurrency, signed block overhead accounting, no-fit deferral and bounded
  selection all hold at boundary values. Invalid or full admission does not
  evict an existing pending transaction.
* Wrong signer, signed genesis changes, bad parent/time, missing certificate,
  invalid tx inside a valid proposer-signed block, wrong nonce, altered input
  digest, outcome or state root are rejected by full validation. Light rejects
  invalid certificates and freezes durably on two authentic conflicting
  headers without claiming execution validation.
* Fault injection/process cuts before body commit, during body commit, between
  acknowledged body and engine commit, during engine commit and after commit
  before response establish old-or-new visibility. History uncertainty never
  publishes an engine reference. Orphans cannot appear as included, block a
  replacement height or be silently promoted on restart. Missing/corrupt
  referenced body fails closed; all retained old bodies remain fetchable.
* Decode engine host records to show they contain only metadata/references;
  submit a distinctive payload sentinel and establish that it appears in
  history and intended engine entity state, never in serialized engine host
  transaction envelopes or the PostgreSQL feed. A raw file grep is insufficient
  because engine snapshots intentionally contain entity payload state.
* Fresh real-socket producer/full/light run: external signed submission,
  execution match, verified query proof against authenticated header, stop/
  reopen all roles, resubmit duplicate, continue nonce, fetch old body. Existing
  query classification fixtures and proof bytes continue to pass.

Document measured limits and completed tests separately from planned
properties. The resulting demo is an authenticated single-proposer execution
chain with retained bodies, not a production consensus network.
