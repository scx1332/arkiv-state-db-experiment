# Simulator host storage profile v1

The explicitly unsigned simulator uses `ARKSIM01 || u32be(1)` at
`Meta[engine/profile]`. Ordinary engine-only opens reject this profile;
simulator opens reject `ARKENG01`. Canonical engine state, manifest bytes,
logical roots and pricing remain unchanged. This is a host-composition
profile, not a signed block format or consensus mechanism.

The coordinator exclusively owns one `DurableEngine` and its versioned
store. It uses `initialize_simulator`, `open_simulator` and
`prepare_simulator`; ordinary `prepare` is refused on this profile. A host
packet names exactly one height/root, one opaque versioned node envelope,
and a bounded list of typed inclusion records. The engine admits no caller
selected tables, metadata keys, head updates, or arbitrary extra writes.
The node remains responsible for validating canonical bodies, feed bytes,
genesis limits, request identities and wire servability before preparation.

All integers below are big-endian. Every trailing digest uses the existing
engine domain-separated digest helper. `bytes` means `u32 length || bytes`.

| Record | Encoding before its trailing 32-byte digest | Digest domain |
| --- | --- | --- |
| `Meta[simulator/identity]` | `ARKSIMI1 || sourceId[16] || runId[16] || nodeGenesisHash[32] || bytes(hostGenesisMetadata)` | `arkiv/simulator-profile/v1` |
| `Blocks[u64 height]` | `ARKSIMB1 || height[u64] || manifestDigest[32] || stateRoot[32] || bytes(nodeEnvelope)` | `arkiv/simulator-node-record/v1` |
| `Inclusions[identityKey]` | `ARKSIMR1 || height[u64] || manifestDigest[32] || keyLength[u8] || identityKey || bytes(inclusionValue)` | `arkiv/simulator-inclusion/v1` |

The engine deployment is exactly
`digest("arkiv/simulator-engine/v1", nodeGenesisHash || runId).0`.
The immutable host genesis metadata is nonempty and at most 64 KiB. It
contains the node's canonical genesis/configuration and is checked against
the expected identity on open/recovery. `read_simulator_identity` performs
bounded profile/identity reads so a host can decode its local persisted
configuration before opening; it does not authorize accepting a different
remotely supplied identity.

The complete stored block record is at most 8 MiB, including 116 bytes of
engine framing. An inclusion key is the existing 61-byte tx/admin request
identity beginning with tag 0/1, or tag 2 followed by a 32-byte control id.
Values are nonempty and at most 1 KiB; a packet has at most 4,096 unique
keys. Genesis has no inclusions. The host can impose smaller admission caps.
All host record bytes count toward the same engine preparation payload
budget before trie staging. The existing consuming storage commit still
temporarily clones one bounded batch, including these records.

The atomic batch contains engine trie objects, manifest/digest index,
simulator envelope/inclusions and the engine head CAS. The head remains
`(sequence, manifest digest)` and `BlockIndex` remains the manifest index.
Height equals engine sequence. An uncertain commit retains the old public
head and the exact prepared batch. Recovery compares every prepared host
record as well as engine records; observed application cannot certify
durability. Public block/inclusion reads never expose future uncertain
records above the last acknowledged head.

Opening reads fixed metadata, engine genesis/head manifests and the head
node record; no history or trie scan occurs. Historical block reads use
one manifest association and one bounded block record. Missing promised
records, bad framing/digests and mismatched manifest/root/height are
corruption. The facade retains no historical block, header or inclusion
vector. All history remains on disk without pruning.

`crates/arkiv-db/tests/simulator_composition.rs` contains hand-built framing
vectors, wrong-profile/identity/cap tests, atomic visibility and uncertainty
checks, corruption cases, and a 1,000-publication lazy-open/retention test.
The simulator host's node tests exercise canonical bodies and real redb.
