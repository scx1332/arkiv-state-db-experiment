# Bricks namespace sample

On GLM chains, native `TxOp::Arkiv` operations accept any existing namespace.
The signed admission and execution paths still prohibit generic record writes
and namespace transfers under this pricing profile. Arkiv keeps per-entity
ownership, system cells, finite lifetimes, storage rent and atomic rollback.
Chains without GLM pricing (the unsigned simulator and demo) restrict Arkiv
operations to namespace one because their other namespaces admit generic writes. Ethereum/SDK APIs keep namespace
one as their default; explicit namespace selection uses native transactions.

Records, indexes, equality proofs, due counts and expiry sweeps already include
the namespace. Creation counters remain shared per actor, preserving distinct
entity keys across namespaces.

`scripts/seed-bricks.py` adds one atomic batch of 16 toy bricks to an existing
`bricks` namespace. Indexed attributes include `sample`, `sku`, `color`, `size`,
`width`, `length`, `height`, `shape` and `material`. Width/length are studs and
height is plates; the JSON payload names these units. Duplicate colors and
sizes create posting lists with multiple records. Eight colors and three shapes
make the term tree useful to inspect.

Use an existing funded signer and the active genesis file:

```sh
python3 scripts/seed-bricks.py --binary target/release/arkiv-proposer \
  --peer http://127.0.0.1:19500 --full http://127.0.0.1:19502 \
  --key-file /private/tester.key --genesis-file /private/genesis.bin \
  --actor 0xPUBLIC_SIGNER_ADDRESS
```

The default lifetime is 1,296,000 blocks (30 days at two-second blocks). Pass
`--lifetime-blocks` for a different block interval. Rent is paid on-chain;
entities expire after that lifetime. Stable per-chain/namespace/signer request
IDs make retries safe: the helper inspects existing inclusion and does not
create duplicate samples. A previously failed or expired installation requires
explicit operator review rather than an automatic duplicate transaction.

Open the separate state tree explorer, choose Latest, select `bricks · ID 2`,
and inspect Record contents (rows), Attribute index (terms), or Postings. The
Raw records map is separate from typed entity rows. For example, `color = red` has three
records and `size = 2 x 4` has seven.
