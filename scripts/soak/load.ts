// Workload for the node soak (scripts/soak/run.py), through @arkiv-network/sdk.
//
//   bun load.ts address   print the address of PRIVATE_KEY
//   bun load.ts cohort    write COHORT entities tagged COHORT_TAG and stop
//   bun load.ts write     write for DURATION_S seconds, one JSON line per WINDOW_S
//   bun load.ts probe     time each read once over, print one JSON line
//   bun load.ts readers   CONC callers reading for DURATION_S seconds, one line per WINDOW_S
//   bun load.ts mutate    patch Sourcify contracts for DURATION_S seconds, one JSON line per WINDOW_S
//
// Env: RPC_URL, PRIVATE_KEY, PHASE (label copied into every line), and per mode:
//   write    TAG, MIX (create|churn), BATCH (16), WAVE (8), LIFETIME_BLOCKS, RATE_EPS (0 = unthrottled)
//   cohort   COHORT_TAG, COHORT (2000), COHORT_KEYS (file the keys are written to)
//   probe    COHORT_TAG, COHORT, COHORT_KEYS, GROW_TAG (optional), READS (100), CONC (4)
//   readers  COHORT_TAG, COHORT, COHORT_KEYS, CONC (4)
//   mutate   SOURCIFY_KEYS (file of contract keys this account owns), SET (reverify|verifiedat|cohort), BATCH, WAVE
//            SET=cohort patches the cohort's own items (keys from SOURCIFY_KEYS, which run.py
//            points at COHORT_KEYS): a fixed small state rewritten for the whole phase
// With SOURCIFY_KEYS set, probe and readers also time reads of the Sourcify dataset.
// A reverted or lost transaction ends `write` with exit code 1 and the reason on stderr.
import { createPublicClient, createWalletClient, ExpirationTime } from "@arkiv-network/sdk"
import { localhost } from "@arkiv-network/sdk/chains"
import { and, desc, eq, gte, lt } from "@arkiv-network/sdk/query"
import { bool, bytes, defineSchema, str, u64 } from "@arkiv-network/sdk/schema"
import { defineChain, type Hex, http } from "viem"
import { privateKeyToAccount } from "viem/accounts"

const env = (name: string, fallback?: string) => {
  const value = process.env[name] ?? fallback
  if (value === undefined) throw new Error(`${name} is not set`)
  return value
}
const num = (name: string, fallback: number) => Number(process.env[name] ?? fallback)

const mode = process.argv[2]
const account = privateKeyToAccount(env("PRIVATE_KEY") as Hex)
if (mode === "address") {
  console.log(account.address.toLowerCase())
  process.exit(0)
}

const url = env("RPC_URL")
const phase = process.env.PHASE ?? mode
const id = await createPublicClient({ chain: localhost, transport: http(url) }).getChainId()
const chain = defineChain({
  id,
  name: "soak",
  nativeCurrency: { decimals: 18, name: "GLM", symbol: "GLM" },
  rpcUrls: { default: { http: [url] } },
})
// viem polls for receipts every 4 s by default, which would hide the node's own latency.
const POLL_MS = 50
const pub = createPublicClient({ chain, transport: http(), pollingInterval: POLL_MS })
const wallet = createWalletClient({ chain, transport: http(), pollingInterval: POLL_MS, account })

const Item = defineSchema({
  run: str().indexed(),
  category: str().indexed(),
  score: u64().indexed(),
  title: str().indexed(),
  body: bytes().optional(),
})
// The Sourcify contract attributes a re-verification rewrites that SDK 1.0.1 can type (it has no
// bytes32): the node merges a patch's `set` into the entity, so the rest stay as they are.
const Contract = defineSchema("sourcify.v4.contract", {
  verifiedat: u64().indexed(),
  match: str().indexed(),
  matchid: u64().indexed(),
  chainid: u64().indexed(),
  language: str().indexed(),
  evmversion: str().indexed(),
  compilerversion: str().indexed(),
  optimizer: bool().indexed(),
})
const body = new Uint8Array(256).fill(7)
const item = (run: string, n: number) => ({
  run,
  category: `cat-${n % 10}`,
  score: u64(BigInt(n)),
  title: `item ${String(n).padStart(8, "0")}`,
  body,
})

const pct = (xs: number[], p: number) => {
  if (xs.length === 0) return null
  const s = [...xs].sort((a, b) => a - b)
  return +s[Math.min(s.length - 1, Math.floor((p / 100) * s.length))]!.toFixed(1)
}
const emit = (line: Record<string, unknown>) => console.log(JSON.stringify({ t: Date.now() / 1000, phase, ...line }))
const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms))

// ---------------------------------------------------------------- writes --

type Ops = { creates: number; patches: number; deletes: number; extensions: number }

/** Sends one batch and waits for its receipt; returns the created keys and the gas used. */
async function send(batch: Parameters<typeof wallet.executeBatch>[0], nonce: number) {
  const res = await wallet.executeBatch(batch, { nonce })
  const receipt = await pub.waitForTransactionReceipt({ hash: res.txHash, pollingInterval: POLL_MS })
  if (receipt.status !== "success") throw new Error(`transaction ${res.txHash} (nonce ${nonce}) reverted`)
  return { keys: res.createdEntities, gas: receipt.gasUsed }
}

async function write(tag: string, opts: { seconds?: number; entities?: number; created?: Hex[] }) {
  const BATCH = num("BATCH", 16)
  const WAVE = num("WAVE", 8)
  const LIFETIME = num("LIFETIME_BLOCKS", 2_000_000)
  const RATE = num("RATE_EPS", 0)
  const WINDOW_MS = num("WINDOW_S", 10) * 1000
  const churn = process.env.MIX === "churn"
  // Of a churn batch's 16 operations: 11 creates, 3 patches, 1 delete, 1 extension.
  const share = (n: number) => Math.max(1, Math.round((BATCH * n) / 16))

  // Keys this run created that a later batch may patch, delete or extend. Only the younger half
  // of a lifetime is offered, so a target cannot expire between being chosen and being included.
  const live: { key: Hex; block: bigint }[] = []
  let oldest = 0
  let created = 0
  const take = (head: bigint) => {
    while (oldest < live.length && head - live[oldest]!.block > BigInt(LIFETIME) / 2n) oldest++
    return oldest < live.length ? live[oldest++] : undefined
  }

  let nonce = await pub.getTransactionCount({ address: account.address, blockTag: "pending" })
  let head = await pub.getBlockNumber()
  const started = performance.now()
  const deadline = opts.seconds === undefined ? Infinity : started + opts.seconds * 1000
  let windowStart = started
  let windowBlock = head
  let lat: number[] = []
  let gas = 0n
  let ops: Ops = { creates: 0, patches: 0, deletes: 0, extensions: 0 }
  let txs = 0
  const flush = async () => {
    const now = performance.now()
    head = await pub.getBlockNumber()
    const seconds = (now - windowStart) / 1000
    emit({
      seconds: +seconds.toFixed(2),
      txs,
      ...ops,
      entitiesPerSecond: +(ops.creates / seconds).toFixed(1),
      opsPerSecond: +((ops.creates + ops.patches + ops.deletes + ops.extensions) / seconds).toFixed(1),
      blocks: Number(head - windowBlock),
      head: Number(head),
      txMsP50: pct(lat, 50),
      txMsP95: pct(lat, 95),
      txMsMax: pct(lat, 100),
      gas: gas.toString(),
      createdTotal: created,
    })
    windowStart = now
    windowBlock = head
    lat = []
    gas = 0n
    ops = { creates: 0, patches: 0, deletes: 0, extensions: 0 }
    txs = 0
  }

  while (performance.now() < deadline && (opts.entities === undefined || created < opts.entities)) {
    const waveStart = performance.now()
    const wave: Promise<void>[] = []
    let waveOps = 0
    for (let w = 0; w < WAVE; w++) {
      const targets = churn ? Array.from({ length: share(5) }, () => take(head)).filter((k) => k !== undefined) : []
      const patches = targets.slice(0, share(3))
      const deletes = targets.slice(share(3), share(3) + share(1))
      const extensions = targets.slice(share(3) + share(1))
      let size = BATCH - targets.length
      if (opts.entities !== undefined) size = Math.min(size, opts.entities - created)
      if (size <= 0 && targets.length === 0) break
      const first = created
      created += size
      waveOps += size + targets.length
      const sentAt = performance.now()
      const sentNonce = nonce++
      wave.push(
        send(
          {
            creates: Array.from({ length: size }, (_, i) => ({
              schema: Item,
              values: item(tag, first + i),
              expiration: ExpirationTime.fromBlocks(LIFETIME),
            })),
            patches: patches.map(({ key }, i) => ({ schema: Item, entityKey: key, set: { score: u64(BigInt(first + i)) } })),
            deletes: deletes.map(({ key }) => ({ entityKey: key })),
            extensions: extensions.map(({ key }) => ({ entityKey: key, expiration: ExpirationTime.fromBlocks(LIFETIME) })),
          },
          sentNonce,
        ).then((res) => {
          lat.push(performance.now() - sentAt)
          gas += res.gas
          txs++
          ops.creates += size
          ops.patches += patches.length
          ops.deletes += deletes.length
          ops.extensions += extensions.length
          // A patch does not move the expiry, so a patched key is offered again at its original
          // age; deleted and extended keys are not offered again.
          if (churn) for (const key of res.keys) live.push({ key, block: head })
          live.push(...patches)
          opts.created?.push(...res.keys)
        }),
      )
    }
    await Promise.all(wave)
    if (churn && oldest > 100_000) {
      live.splice(0, oldest)
      oldest = 0
    }
    if (RATE > 0) await sleep(Math.max(0, (waveOps / RATE) * 1000 - (performance.now() - waveStart)))
    if (performance.now() - windowStart >= WINDOW_MS) await flush()
    else head = await pub.getBlockNumber()
  }
  if (txs > 0) await flush()
  return created
}

// ---------------------------------------------------------------- mutate --

/**
 * Patches Sourcify contracts the way a re-verification does. Each one is read first (as the sync
 * reads before it patches), then `SET=reverify` rewrites `chainid`, `language`, `evmversion`,
 * `compilerversion` and `optimizer` with their current values, flips `match` between `match` and
 * `exact_match` and sets `verifiedat` to now. Nearly every contract shares its `chainid`,
 * `language` and `match` values, so these touch posting lists of millions of entries.
 * `SET=verifiedat` only sets `verifiedat`, the baseline. Read and transaction time are timed apart.
 */
async function mutate() {
  const BATCH = num("BATCH", 16)
  const WAVE = num("WAVE", 8)
  const WINDOW_MS = num("WINDOW_S", 10) * 1000
  const SET = process.env.SET ?? "reverify"
  if (SET !== "reverify" && SET !== "verifiedat" && SET !== "cohort") throw new Error(`unknown SET ${SET}`)
  const keys: Hex[] = JSON.parse(await Bun.file(env("SOURCIFY_KEYS")).text())
  let next = num("OFFSET", 0) % keys.length
  let nonce = await pub.getTransactionCount({ address: account.address, blockTag: "pending" })
  let head = await pub.getBlockNumber()
  const deadline = performance.now() + num("DURATION_S", 60) * 1000
  let windowStart = performance.now()
  let windowBlock = head
  let lat: number[] = []
  let readLat: number[] = []
  let gas = 0n
  let patches = 0
  let txs = 0
  let refused = 0
  let moved = 0
  const read = async (key: Hex) => {
    for (;;) {
      const t = performance.now()
      try {
        const e = await pub.getEntity(key)
        readLat.push(performance.now() - t)
        if (!e) throw new Error(`contract ${key} not found`)
        return (e as unknown as { $attributes: Record<string, { value: unknown }> }).$attributes
      } catch (error) {
        if (!busy(error)) throw error
        refused++
        await sleep(5)
      }
    }
  }
  const flush = async () => {
    const now = performance.now()
    head = await pub.getBlockNumber()
    const seconds = (now - windowStart) / 1000
    emit({
      seconds: +seconds.toFixed(2),
      set: SET,
      txs,
      creates: 0,
      patches,
      deletes: 0,
      extensions: 0,
      entitiesPerSecond: 0,
      opsPerSecond: +(patches / seconds).toFixed(1),
      matchMoved: moved,
      blocks: Number(head - windowBlock),
      head: Number(head),
      txMsP50: pct(lat, 50),
      txMsP95: pct(lat, 95),
      txMsMax: pct(lat, 100),
      readMsP50: pct(readLat, 50),
      readMsP95: pct(readLat, 95),
      readsRefused: refused,
      gas: gas.toString(),
      createdTotal: 0,
    })
    windowStart = now
    windowBlock = head
    lat = []
    readLat = []
    gas = 0n
    patches = txs = refused = moved = 0
  }
  while (performance.now() < deadline) {
    const batches: Hex[][] = []
    for (let w = 0; w < WAVE; w++) {
      batches.push(Array.from({ length: BATCH }, () => keys[next++ % keys.length]!))
    }
    // Reads stay under the node's read concurrency (4): three at a time.
    const current: Record<string, Record<string, { value: unknown }>> = {}
    const flat = SET === "cohort" ? [] : batches.flat()
    let i = 0
    await Promise.all(
      Array.from({ length: 3 }, async () => {
        while (i < flat.length) {
          const key = flat[i++]!
          current[key] = await read(key)
        }
      }),
    )
    const now = BigInt(Math.floor(Date.now() / 1000))
    await Promise.all(
      batches.map(async (batch) => {
        const sentNonce = nonce++
        const set = (key: Hex) => {
          if (SET === "verifiedat") return { verifiedat: u64(now) }
          const a = current[key]!
          const was = String(a.match?.value)
          moved++
          return {
            verifiedat: u64(now),
            match: str(was === "match" ? "exact_match" : "match"),
            chainid: u64(BigInt(a.chainid!.value as bigint | string)),
            language: str(String(a.language!.value)),
            evmversion: str(String(a.evmversion!.value)),
            compilerversion: str(String(a.compilerversion!.value)),
            optimizer: bool(Boolean(a.optimizer!.value)),
          }
        }
        const t = performance.now()
        const res = await send(
          SET === "cohort"
            ? {
                // A new score and category per patch: both are indexed, so every patch moves
                // the item between posting lists.
                patches: batch.map((key) => ({
                  schema: Item,
                  entityKey: key,
                  set: { score: u64(now * 1000n + BigInt(patches++ % 1000)), category: `cat-${Number(now) % 10}` },
                })),
              }
            : { patches: batch.map((key) => ({ schema: Contract, entityKey: key, set: set(key) })) },
          sentNonce,
        )
        lat.push(performance.now() - t)
        gas += res.gas
        txs++
        if (SET !== "cohort") patches += batch.length
      }),
    )
    if (performance.now() - windowStart >= WINDOW_MS) await flush()
  }
  if (txs > 0) await flush()
}

// ----------------------------------------------------------------- reads --

const COHORT = num("COHORT", 2000)
const cohortTag = process.env.COHORT_TAG ?? "cohort"
const inCohort = eq("run", str(cohortTag))
let cohortKeys: Hex[] = []

/** Each read returns its row count, which is checked against `rows` so a wrong answer is not timed as a fast one. */
const reads: Record<string, { rows: number | null; run: (i: number) => Promise<number> }> = {
  getEntity: {
    rows: 1,
    run: async (i) => ((await pub.getEntity(cohortKeys[(i * 7919) % cohortKeys.length]!, Item)) ? 1 : 0),
  },
  "eq, limit 100": {
    rows: 100,
    run: async (i) =>
      (await pub.select("*").from(Item).where(inCohort, eq("category", str(`cat-${i % 10}`))).limit(100).fetch())
        .entities.length,
  },
  "range, limit 100": {
    rows: 100,
    run: async (i) => {
      const lo = BigInt((i * 37) % (COHORT - 100))
      const window = and(inCohort, gte("score", u64(lo)), lt("score", u64(lo + 100n)))
      return (await pub.select("*").from(Item).where(window).limit(100).fetch()).entities.length
    },
  },
  "count cohort": { rows: COHORT, run: async () => Number(await pub.count().from(Item).where(inCohort).fetch()) },
  "order by score desc, limit 20": {
    rows: 20,
    run: async () =>
      (await pub.select("*").from(Item).where(inCohort).orderBy(desc("score")).limit(20).fetch()).entities.length,
  },
  "offset N-50, limit 50": {
    rows: 50,
    run: async () =>
      (await pub.select("*").from(Item).where(inCohort).limit(50).offset(COHORT - 50).fetch()).entities.length,
  },
}
// Two reads over the set the writer is still growing: their cost may depend on its size.
if (process.env.GROW_TAG) {
  const growing = eq("run", str(process.env.GROW_TAG))
  reads["count growing set"] = { rows: null, run: async () => Number(await pub.count().from(Item).where(growing).fetch()) }
  reads["eq on growing set, limit 100"] = {
    rows: null,
    run: async (i) =>
      (await pub.select("*").from(Item).where(growing, eq("category", str(`cat-${i % 10}`))).limit(100).fetch())
        .entities.length,
  }
}

// Reads of the Sourcify dataset, when SOURCIFY_KEYS names contract keys it holds.
let sourcifyKeys: Hex[] = []
if (process.env.SOURCIFY_KEYS) {
  reads["sourcify getEntity"] = {
    rows: 1,
    run: async (i) => ((await pub.getEntity(sourcifyKeys[(i * 7919) % sourcifyKeys.length]!)) ? 1 : 0),
  }
  reads["sourcify Vyper, limit 100"] = {
    rows: null,
    run: async () =>
      (await pub.select("*").from(Contract).where(eq("language", str("Vyper"))).limit(100).fetch()).entities.length,
  }
  reads["sourcify matchid window, limit 100"] = {
    rows: null,
    run: async (i) => {
      const lo = BigInt(1_000_000 + ((i * 7_919_993) % 58_000_000))
      return (await pub.select("*").from(Contract).where(gte("matchid", u64(lo)), lt("matchid", u64(lo + 50_000n))).limit(100).fetch())
        .entities.length
    },
  }
  reads["sourcify count Vyper"] = {
    rows: null,
    run: async () => Number(await pub.count().from(Contract).where(eq("language", str("Vyper"))).fetch()),
  }
}

/** Whether the node refused a read because `--read-concurrency` reads were already running. */
const busy = (error: unknown): boolean => {
  const e = error as { message?: string; details?: string; code?: number; status?: number; cause?: unknown }
  if (!e) return false
  if (e.code === -32010 || e.status === 429 || /ServerBusy|-32010/.test(`${e.message} ${e.details}`)) return true
  return e.cause !== undefined && e.cause !== error && busy(e.cause)
}

async function loadCohort() {
  if (process.env.SOURCIFY_KEYS) sourcifyKeys = JSON.parse(await Bun.file(process.env.SOURCIFY_KEYS).text())
  cohortKeys = JSON.parse(await Bun.file(env("COHORT_KEYS")).text())
  if (cohortKeys.length !== COHORT) throw new Error(`${env("COHORT_KEYS")} holds ${cohortKeys.length} keys, expected ${COHORT}`)
}

async function probe() {
  const READS = num("READS", 100)
  const CONC = num("CONC", 4)
  await loadCohort()
  const out: Record<string, unknown> = {}
  for (const [name, read] of Object.entries(reads)) {
    let rows: number
    try {
      rows = await read.run(0)
    } catch (error) {
      // One read the node refuses (a read budget, say) is a result, not the end of the probe.
      out[name] = { error: String((error as Error)?.message ?? error).split("\n")[0]!.slice(0, 200) }
      continue
    }
    if (read.rows !== null && rows !== read.rows) throw new Error(`${name}: ${rows} rows, expected ${read.rows}`)
    const lat: number[] = []
    for (let i = 0; i < READS; i++) {
      const t = performance.now()
      await read.run(i)
      lat.push(performance.now() - t)
    }
    let next = 0
    let refused = 0
    const t = performance.now()
    await Promise.all(
      Array.from({ length: CONC }, async () => {
        while (next < READS) {
          try {
            await read.run(next++)
          } catch (error) {
            if (!busy(error)) throw error
            refused++
          }
        }
      }),
    )
    const seconds = (performance.now() - t) / 1000
    out[name] = { rows, p50Ms: pct(lat, 50), p95Ms: pct(lat, 95), reqPerSec: +((READS - refused) / seconds).toFixed(0), refused }
  }
  emit({ head: Number(await pub.getBlockNumber()), conc: CONC, reads: out })
}

async function readers() {
  const CONC = num("CONC", 4)
  const WINDOW_MS = num("WINDOW_S", 10) * 1000
  const deadline = performance.now() + num("DURATION_S", 60) * 1000
  await loadCohort()
  const names = Object.keys(reads)
  let lat: Record<string, number[]> = {}
  let refused = 0
  let served = 0
  let turn = 0
  const callers = Array.from({ length: CONC }, async () => {
    while (performance.now() < deadline) {
      const i = turn++
      const name = names[i % names.length]!
      const t = performance.now()
      try {
        await reads[name]!.run(i)
        ;(lat[name] ??= []).push(performance.now() - t)
        served++
      } catch (error) {
        if (!busy(error)) throw error
        refused++
      }
    }
  })
  const windows = (async () => {
    while (performance.now() < deadline) {
      await sleep(Math.min(WINDOW_MS, Math.max(0, deadline - performance.now())))
      const window = lat
      const line = { served, refused, reqPerSec: +(served / (WINDOW_MS / 1000)).toFixed(0), conc: CONC }
      lat = {}
      served = 0
      refused = 0
      emit({
        ...line,
        reads: Object.fromEntries(Object.entries(window).map(([name, xs]) => [name, { p50Ms: pct(xs, 50), p95Ms: pct(xs, 95) }])),
      })
    }
  })()
  await Promise.all([...callers, windows])
}

// ------------------------------------------------------------------ main --

try {
  if (mode === "cohort") {
    process.env.MIX = "create"
    const created: Hex[] = []
    await write(cohortTag, { entities: COHORT, created })
    await Bun.write(env("COHORT_KEYS"), JSON.stringify(created))
  } else if (mode === "write") await write(env("TAG"), { seconds: num("DURATION_S", 60) })
  else if (mode === "probe") await probe()
  else if (mode === "readers") await readers()
  else if (mode === "mutate") await mutate()
  else throw new Error(`unknown mode ${mode}`)
} catch (error) {
  console.error(`${mode} failed: ${(error as Error)?.stack ?? error}`)
  process.exit(1)
}
