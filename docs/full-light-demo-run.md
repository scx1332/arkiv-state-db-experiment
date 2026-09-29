# Run the volatile full/light demo

From this repository:

```bash
python3 scripts/full-light-demo.py
```

The launcher builds `arkiv-demo`, then starts three separate processes on
loopback: producer `8781`, full follower `8782`, light follower `8783`.
The producer appends a deterministic workload block once per second, using
seed 7. The full node independently executes retained block bodies. The
light node follows signed headers and verifies complete Eq pages against
its accepted header roots. It never downloads the database or executes
blocks. The launcher uses only Python's standard library.

The terminal shows heights, observed lag, retained coverage, root/hash prefixes,
health and each process's resident memory on Linux. It labels the run
`single-producer development chain` and `retained in memory`; nothing is
pruned. Full-node results say `execution checked`, light headers say
`producer authenticated`, and light query results say `query proof
verified`. Signature and query verification do not establish public
consensus, execution validity or global freshness. A stale valid peer can
withhold newer headers; lag is relative to the last observed peer head.

## Terminal commands

Wait for height 18 or later to exercise the first complete workload cycle.
Each query resolves its block once against the light process's accepted
headers, then compares the full process's ordinary query with rows derived
from the light process's verified proof. Successful comparisons include
posting count, proof bytes, verification time and response latency. Rows
are printed only after successful verification and exact comparison.

| Command | Behavior |
| --- | --- |
| `query` | Namespace 1, `Eq(group, U64(1))`, latest accepted light height, three rows |
| `query 3` | Pin block 3; both initial records are present |
| `query 8` | Observe the recreated-key record after deletion |
| `query 13` then `query 14` | Compare the expiring row immediately before and after expiry |
| `query 10 1 str 1 1` | Typed string equality after a U64-to-Str patch |
| `query 18 2 u64 1 3` | Query the second namespace |
| `query 18 999 u64 1 3` | A proven `NamespaceNotFound`, distinct from an empty term |
| `query latest 1 u64 999 3` | A successful proven empty Eq result |
| `next 1 producer` | Continue the prior verified page at the same block, changing page size and proof endpoint |
| `next 64 full` | Continue against the full server at that same pinned block |
| `pause full` / `resume full` | Pause/resume independent block execution |
| `pause light` / `resume light` | Pause/resume signed-header following |
| `late full` / `late light` | Start another process from local genesis trust, then catch up |
| `pause full2` / `resume full2` | Control a late follower |
| `next 3 full2` | Switch a retained proof walk to a caught-up late full follower |
| `produce 5` | Manually append five blocks through the same producer writer path |
| `status` | Refresh the terminal view |
| `quit` | Stop every process launched by this invocation |

Queries support exactly one typed Eq predicate on `group` in this terminal
view. Types are `bool`, `i64`, `u64` and `str`. The verified engine profile
supports any valid attribute name through RPC. Projection is always id,
key and all cells. Page sizes are 1 through 64, with a maximum complete
posting set of 4,096 ids; larger sets fail explicitly. Every page carries
its entire posting set, so small pages do not imply small proof work.

A query at a height the light node has not accepted fails without changing
the selected height. If the full process is behind that block, resume it
and retry the pinned query. `next` requires a prior verified nonterminal
page; a server-supplied cursor cannot establish the first boundary. A
follower restart discards continuation context and starts a fresh walk.

## Options and lifecycle

```bash
# Manual production only; append with the terminal's produce command.
python3 scripts/full-light-demo.py --interval-ms 0

# Independent ports, smaller explicit run capacity, fixed seed.
python3 scripts/full-light-demo.py --base-port 9881 --capacity 250 --seed 7

# Noninteractive smoke run after a prior build.
python3 scripts/full-light-demo.py --no-build --duration 8
```

Capacity defaults to 10,000 retained heights including genesis. Reaching
capacity stops production before publication and leaves old snapshots,
blocks and proofs available. Memory grows with history until that explicit
capacity; this is not a bounded-RAM archive or durable recovery design.

Each invocation creates a private temporary run directory. The producer
exclusively creates its run subdirectory, writes a local `producer.key`
(mode 0600), and writes `trust.bin`. The launcher supplies that exact local
trust file to followers; RPC endpoints never select the trusted signer or
genesis. Default exit removes run files after terminating the processes.
`--keep` preserves logs and local run files for inspection, and
`--run-parent /existing/directory` chooses their parent. Keep the private
key local to the producer.

A producer cannot reopen an existing run directory, even if its old blocks
are available. Every new producer run generates a fresh genesis identity.
A volatile follower can restart and replay from the still-running peer;
there is no database checkpoint download or durable engine recovery claim.

## Process acceptance tests

```bash
python3 scripts/test-full-light-demo.py
# Existing build:
python3 scripts/test-full-light-demo.py --no-build
# Preserve logs and local run files for a failed run:
python3 scripts/test-full-light-demo.py --keep
```

The test uses separate real child processes, ephemeral loopback ports and
a deterministic HTTP proxy. Its independent Python model implements the
workload's row lifecycle without reading query answers, engine codecs or
trie code to derive expectations. It verifies:

- 200 deterministic blocks, with two independent full followers and two
  light followers agreeing on genesis, height, hash and root.
- Late starts, a 25-block pause, dropped block responses, reconnect and a
  full follower restarting from genesis. An unavailable body never
  advances its executed height.
- Historical queries at 19 sampled heights, on both light processes, with
  page sizes 1, 3 and 64. Full local reads and verified rows exactly equal
  the model, including all cells and recreated record ids.
- Type changes, rollback, deletion and both sides of expiry; separate
  absent-term and absent-namespace outcomes; namespace 2 after transfers.
- Proof endpoint switching on page walks, and unchanged pages pinned to
  block 200 while a newer block is produced.
- Corrupted roots, truncated proofs, a removed posting id with otherwise
  valid row membership paths, changed snapshot identity and continuation
  without prior verified context. No forged response yields rows.
- Authentic batches with missing heights reject/refetch without skipping;
  a valid prefix remains accepted. Old duplicate signed batches leave the
  accepted head and observed maximum unchanged, report stalled, and recover
  when the peer resumes serving current data, for both follower roles.
- A proxy-mutated signature rejected by another header-only process.
- Actual chunked request bodies: exactly 1 MiB accepted and one byte over
  rejected; a chunked proof response one byte over 8 MiB is also rejected
  before a verified result is returned.
- Capacity stopping without eviction, an old proof still readable,
  existing producer identity reuse refused and a new run getting a new
genesis identity.

The launcher retries explicit `ServerBusy` admission responses for at most
five seconds. A lost response to a mutation is never automatically retried,
because it could already have executed.

The binary's Rust tests additionally cover canonical format vectors,
strict codecs, header/body authentication and linkage attacks, execution
root mismatches, publication boundaries, resource bounds, and independent
state-root/outcome oracles. Run them with `cargo test -p arkiv-node` and
`cargo test -p arkiv-db`. See [the specification](full-light-demo.md) for the
trust model and [proof accounting](eq-proof-charges.md) for binary/transport
caps, read charges and conservative work reservations.
