# Implemented genesis bundle v1 contract

The authoring commands and examples are described in
[the design](genesis-bundle-design.md#implemented-envelope). This document freezes
the machine fields and runtime behavior of the bounded implementation.

## Versions and identity

`arkiv-launch-prefill-v1` is a signed genesis (version 1) whose optional
`PrefillLaunch` is present. It uses the one native block format and protocol
(version 1), state layout 1 and codec 1, and the `glm-v2` gas profile. A chain
without a prefill uses the same signed genesis layout with the prefill absent
and creates its entity namespace in block 1 instead. Prefills require a new
chain/run directory. The builtin `arkiv-entity-v2/glm-v2@1` expands immutable
constants; no dynamic base fees are enabled.

Integers are checked fixed-width big-endian. Strings and bytes have u32 byte
lengths. Fixed arrays carry no length; counted sequences have the stated count.
Digests use `Keccak256(u8(domain.len) || ASCII(domain) || bytes)`.

`prefill.bin` is `"ARKPFL01" || u32(1) || u64(row_count)`, followed by repeated
`u32(frame_length) || EntityV1`. Entity fields are:

| Field | Encoding |
| --- | --- |
| Creator | 20 bytes |
| Creation nonce | u64 |
| Salt | u128 |
| Expiry height | u64 |
| Flags | u8 |
| Content type | string |
| Payload | bytes |
| Attributes | u32 count, repeated `(CellName, Value)` using engine codec 2 |

Rows sort by creator bytes then numeric creation nonce. Nonces for each creator
are exactly `0..N-1`. Attribute names sort by their exact bytes. No aliases, source
paths, caller-provided keys or system attributes enter the canonical rows.
`stream_hash = digest("arkiv/prefill-stream/v1", prefill.bin)`.

Recipe fields, in order, are `u32(1), native Genesis, run_id[16], proposer[20],
block_period_ms:u64, SignedLimits, AccountConfig, u32(builder_policy=1),
PrefillLimits, sponsor[20], stream_hash[32], row_count:u64, canonical_bytes:u64`.
Builder policy 1 fixes namespace `entities`,
ordinal 1, and `sponsored-rent-v1`. `PrefillLimits` is three u64 values: max
entities, max canonical bytes, max logical storage units.
`recipe_hash = digest("arkiv/genesis-recipe/v1", recipe_bytes)`.

The signed genesis is `u32(1)` followed by the native genesis, run ID, proposer,
cadence, signed limits and account configuration, then the prefill option
(`01 ‖ PrefillLaunch` here, `00` without a prefill):

| Field | Encoding |
| --- | --- |
| Builder/policy version | u32 = 1 |
| Prefill limits | three u64 values |
| Sponsor | 20 bytes |
| Stream hash | 32 bytes |
| Row count, canonical bytes | two u64 values |
| Recipe hash | 32 bytes |
| Initial state | full existing StateHeader |
| Initial burned wei | u256, 32 bytes |
| Initial logical storage units | u64 |

Height 0 has zero parent and the existing empty-outcome commitment, with
`inputs_digest = digest("arkiv/signed-genesis/v1", encode(SignedGenesis))` (the same domain as a genesis without prefill) and
`state_root = initial_state.root()`. Its ordinary header hash is the genesis
hash clients pin. Recipe identity excludes derived state/burn, source metadata,
compiler identity, local paths and the proposer certificate.

## State, rent and first live block

The builder creates namespace 1 through the normal engine, owned by the proposer,
even with no rows. It then uses normal entity creation at height 0 in canonical
order, including keys, record allocator/revisions, system cells, indexes, expiry
due/total counters and entity creation nonces. Generated creator and owner are
the declared creator; created/updated heights are zero. The next namespace is 2
and the next record is `row_count + 1`.

At the builtin limits, finite expiry is `1..=6307200`, with at most 16 deletions
reserved at one height. For each complete generated row:

```text
rent_gas = ceil(storage_units * expiry_height * 8 / 17280)
burn_wei = sum(each row's rent_gas * 1000000000)
```

Rounding is per row. The explicit sponsor pays from its immutable allocation;
no genesis intrinsic/write fees, transfers, credits or signed transactions are
created. Positive rent materializes the sponsor's balance with transaction nonce
zero and the authenticated cumulative burn counter. Other allocations remain
lazy defaults. Initial supply is the checked allocation sum; remaining supply is
initial supply minus burn. A zero-row import has zero burn.

Block 1 is ordinary, including its expiry sweep. The proposer needs no bootstrap
funding and cannot submit the old namespace-bootstrap operation. An account with
two imported entities signs its first live transaction at account nonce **0**;
a create within it uses entity creation nonce **2**. Live gas remains fixed at
**1,000,000,000 wei per gas**.

## Runtime and publication

```sh
arkiv-proposer genesis lock chain.toml
arkiv-proposer genesis build chain.toml --out bundle --locked
arkiv-proposer genesis verify bundle
arkiv-proposer producer --run-dir NEW_DIR --key-file PROPOSER_KEY --bundle bundle
```

A bundle contains `genesis.bin`, `prefill.bin`, `genesis.lock.toml` and
`report.json`. Build/verify independently reproduce the state/rent/report.
Existing output directories are refused. Private temporary sibling directories
are synced and published with atomic no-replace rename; interruption can leave
an unreferenced temporary directory, never a partially published destination.

The producer verifies before certifying and installing genesis. A full follower
can use `--bundle BUNDLE` or fetch `/signed/v1/prefill` from its pinned peer; the
response is `{ "prefillBytes": "0x..." }`. It independently rebuilds before
following blocks. The existing full/light identity and proposer pin flags remain
required. Full nodes retain `prefill.bin` and verify it on restart. Light nodes
only download the bounded genesis/certificate and verify account/entity proofs
against selected roots; this does not prove dataset replay or availability.

The durable initializer validates a complete memory-built candidate, stages its
reachable objects and commits them with the height-0 manifest and host metadata
in one store transaction. Bounds or validation errors expose no state head. Lost
acknowledgements use existing initialization recovery/reopen. A later head still
checks its configured original genesis root. New nonempty engine genesis uses a
distinct envelope magic; old empty engine genesis bytes are unchanged. The
separate history store may contain an unreferenced body after failed state
installation, but no partial state is published or served. Restart does not
debit genesis rent again.

Ceilings are 256 rows, 1 MiB canonical stream, 8 MiB logical storage units,
128 KiB per value, 23 user attributes (32 total cells), 256 positive allocations,
1 MiB per source, 8 MiB aggregate sources and 512 source files. Durable defaults
add 32 MiB prepared bytes, 100,000 objects and 1,000,000 visits. Any exceeded bound
fails explicitly. This implementation uses bounded memory sorting and one atomic
publication: external sorting, object packs, chunk transport, cross-process bulk
checkpoints and million-row support remain future work. These are admission
bounds, not a demonstrated maximum-volume performance claim.

## Validation

`cargo test -p arkiv-node --test genesis_prefill` covers strict
streams, order independence, rent/capacity/sponsor bounds, empty genesis golden
roots, system metadata, nonce separation, full/light replay, expiry and restart.
Authoring unit tests cover typed TOML/JSONL equivalence, source mutation/pins,
unsafe paths and atomic publication. Durable simulator tests inject failed or
uncertain initialization and verify complete object closure, limits and root
corruption.

`python3 scripts/genesis-bundle-smoke.py` runs isolated real producer/full/light
processes with temporary stores. It checks independently calculated keys/rent,
height-0 account/entity proofs, a live create and transfer, restart, expiry and
historical genesis proofs. It does not modify deployments.
