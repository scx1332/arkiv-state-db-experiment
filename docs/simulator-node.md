# Unsigned durable simulator node

`cargo run -p arkiv-node --bin arkiv-simulator -- producer --unsigned-simulator
--run-dir /tmp/arkiv-simulator --listen 127.0.0.1:9400` creates a new run, or opens
the existing run without replay. Production starts paused. `--resume` explicitly
starts the timer; `--start running` is the same startup policy in a form a static
deployment command line can select (`--start paused` is the default and followers
reject the flag); `--interval-ms` changes wall-clock cadence only. The default
chain is 9001, seed is 7, genesis time is 1700000000000 ms and logical block period
is 1000 ms. Every timestamp is checked `genesis + height * period`.

This executable does not sign headers. The `arkiv-demo` executable and the
signed `arkiv-proposer` are separate. The source UUID, random 128-bit run ID and
canonical genesis-header hash identify a retained run. Restart never replaces
those identities. A new directory creates a separate run; there is no reset,
pruning or truncation API. `--state-retention <N>` (producer, full, inspect)
limits the state heights served to the head and the `N` before it; older
heights answer `410 HistoryPruned`, and nothing is deleted (see the
[pruning design](pruning-design.md)).

The `producer` and `full` roles keep one `node.redb` file. The `light` role keeps
one independent `headers.redb` file containing pinned identity, canonical headers,
their hash index and its acknowledged head. Light has no engine, transaction
bodies or unbounded header vector. Opening a published follower file requires no
remote genesis fetch. A fresh follower validates its explicitly pinned genesis
before creating a file; transient bootstrap failures retry under a 10-second
deadline. An empty adapter file left before host initialization can be initialized
on a later attempt, while a published wrong-profile or wrong-run file is refused.

Followers require `--peer`, `--source-id`, `--run-id`, `--genesis-hash` and the
matching `--chain-id`. `full` also requires a distinct `--replication-peer` and
`SIMULATOR_REPLICATION_TOKEN`. Its canonical bodies are executed through the same
executor, and header, outcomes, terminal state and metadata must match before
publication. It is execution checking against an unsigned trusted source.

HTTPS is required for configured non-loopback peers by default. The explicit
`SIMULATOR_ALLOW_PRIVATE_HTTP=true` deployment setting permits configured cleartext
peers on a protected private network, such as the local Docker network. Only exact
`true` and `false` are accepted. Public requests cannot choose peer URLs. Redirects
are disabled and public metadata/proof requests never carry the private bearer.

`--admin-listen` enables a separate private listener. Both its control endpoint and
canonical body endpoint require `Authorization: Bearer …` matching
`SIMULATOR_ADMIN_TOKEN` (at least 32 bytes). Do not publish that private port in the
local compose deployment. Public listeners cannot execute controls or fetch bodies.

| Endpoint | Purpose |
| --- | --- |
| Public `GET /sim/v1/status` | Pinned identity, role, head, health, immutable profile, workload/configuration revision, engine cache telemetry and, when the process knows its retained file, `storage.fileBytes` (absent rather than zero when unknown) |
| Public `GET /sim/v1/genesis` | Canonical genesis/header and immutable run metadata |
| Public `GET /sim/v1/headers` or `/feed/blocks` | At most 64 consecutive header descriptors and one sampled head; `fromHeight=0` bootstraps, subsequent calls pin `afterHeight` and `afterHash` |
| Public `GET /sim/v1/feed/blocks/H` | Immutable payload-free metadata for exactly one retained block |
| Public `POST /sim/v1/query/eq` | Canonical Eq request and complete proof bytes, separate from the feed |
| Light `POST /sim/v1/query/inspect` | Same verified page plus the exact canonical request/proof and verified Patricia path inspection; [contract and reproducible samples](simulator-proof-inspection.md) |
| Light `POST /sim/v1/query/verified` | Local stateless verifier against an accepted local header; returns verified rows as metadata descriptors plus `diagnostics` (`verifier`, complete `proofBytes`, `fetchMs`, `verifyMs`) describing that light process's own work |
| Private `POST /sim/v1/control` | Pause/resume, one step, or pending workload configuration |
| Private `GET /sim/v1/replication/blocks/H` | Complete canonical body and immutable metadata envelope for a full follower |

Control requests bind a 32-byte command ID, run ID, expected configuration revision
and optional expected head. Pause/resume acknowledge volatile desired runtime state.
Step commits its command digest/result height atomically with one normal block.
Configuration is one bounded pending request, adopted by the next normal block;
its revision and command inclusion survive restart. Only `payloadBytes`
(0–131072, the size of the workload entities' unindexed `bytes` attribute
`data`) may change. Version, seed and logical period remain
immutable. Duplicate committed commands return the original height, including
after a lost acknowledgement and restart; changed content conflicts.

The workload is the finite Arkiv entity fixture (five entities created at block
2, then transfer, patch, extension, deletion and expiry; empty blocks after
that) in namespace 1, under the selected workload genesis limits and execution
parameters; the simulator does not advertise arbitrary smaller parameter sets as
compatible with the trace.

The host envelope is `u32(1) || bytes(canonical Block) || bytes(feed JSON) ||
Workload || u64(configRevision) || Vec<ControlReceipt>`. Workload is
`u32(version) || str(seedDecimal) || str(periodDecimal) || u32(payloadBytes)`. A control receipt is `commandId[32] || digest[32]`.
All binary lengths and integers use the existing canonical big-endian codec.
Genesis uses an empty Block body and the existing special genesis-header digests.
The engine adds the manifest/root framing specified in
[simulator-storage-profile.md](simulator-storage-profile.md).

The feed uses a closed DTO, never serialization of canonical transactions or
arbitrary decoder results. Indexed attributes are intentional metadata. Every
unindexed field, including strings and primitive scalars, becomes its type,
underlying byte length and `digest("arkiv/simulator-value/v1", encode(Value))`;
raw values use the same rule with `Value::Bytes`. Canonical bodies require the
private replication listener. The public proof endpoint intentionally returns
complete field witnesses to the local verifier. Neither enters PG. Metadata
suppression is an ingestion boundary, not a confidentiality promise for values
available through a complete proof.

`feedDigest` covers the entire parsed closed feed DTO except `feedDigest` itself,
under `arkiv/simulator-feed-block/v1`. Its deterministic binary value codec is:
null=0, false=1, true=2, u32=3 plus four bytes, string=4 plus u32 UTF-8 byte length,
array=5 plus u32 count and values, object=6 plus u32 count and key/value pairs.
Object keys are tagged strings ordered by UTF-8 bytes. The codec caps depth at 64,
values at 1,048,576 and encoded bytes at 8 MiB. JSON is only transport. The shared
Rust/TypeScript fixture digest is
`f0db2b45a68f5c2f427351f89e2aa35072cff97bea42f76262038225be3341b6`.
This digest detects metadata transfer mismatch; it does not prove redacted
metadata is committed by the canonical header.

The whole-block metadata profile replaces section paging for this bounded genesis.
At most 128 user operations plus 16 expiry deletions touch terminal records, with
at most 32 cells per row. Even 512 bytes per maximally escaped attribute, 640 per operation
and an additional 128 × 256 bytes for namespace/key metadata remain below 3 MiB.
A bounded serializer independently rejects feeds above 4 MiB before publication.
Canonical bodies are capped at 1 MiB; stored node envelopes have an 8 MiB engine
framing cap and must also fit the 8 MiB hex replication response. Engine trie and
host packet bytes share the 32 MiB preparation cap; commit can temporarily retain
one additional batch clone. These caps do not claim to bound the whole process.

HTTP requests cap actual streamed bytes at 64 KiB, reads at four active calls,
request/peer deadlines at five seconds, and response bytes at 8 MiB. The complete
Eq proof cap reserves its hex expansion, `(8 MiB - 4096) / 2`; existing posting,
row, work and allocation limits still apply. Light retains at most 64 successful
verified continuations, bound to exact snapshot, query and page size. An evicted
or post-restart continuation fails explicitly. Headers, bodies and snapshots
remain on disk; a query limit never silently removes retained history.

Only `Committed` advances the host head. Definite and indeterminate failures keep
the exact prepared packet and fence production. `retry_pending` can publish only
after unchanged-parent reconciliation authorizes that exact batch. Observed
application after an indeterminate result remains durability-unknown on the same
handle. Fresh adapter open follows the existing redb operational recovery boundary.
Conflicting known light headers freeze that catalog, including on subsequent opens.
Capacity rejection pauses automatic selection and reports stalled health; an
explicit operator action is required before selection resumes.

`arkiv-simulator inspect --unsigned-simulator --run-dir DIR` opens a stopped
producer/full file and reports bounded host-open reads, cache counters, disk bytes
and process RSS/HWM without executing blocks. `inspect-light` takes the same
explicit source/run/genesis/chain identity flags as a light follower and opens a
stopped `headers.redb` catalog, reporting its fixed read count and retained header
and continuation counts. Neither command equates RSS with isolated decode
allocation. Node tests cover 200-block memory/durable root and receipt
parity, historical pages at sizes 1/3/64, all commit outcomes, controls, field
redaction, shared metadata codecs, header gaps/stale batches, local proof failures,
real sockets/bootstrap and actual redb write/sync crash cuts. Cross-repository PG
and deployment measurements are recorded by the simulator integration runner.
