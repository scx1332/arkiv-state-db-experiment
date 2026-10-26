/** Runs the user-defined-columns SDK (arkiv-sdk-js udc/*) against an isolated native chain. */
import assert from "node:assert/strict"
import { readFileSync, writeFileSync } from "node:fs"
import { createPublicClient, createWalletClient, ExpirationTime } from "@arkiv-network/sdk"
import { and, eq, gt } from "@arkiv-network/sdk/query"
import { addr, bytes, dec, defineSchema, str, u64 } from "@arkiv-network/sdk/schema"
import { bytesToHex, http } from "viem"
import { privateKeyToAccount } from "viem/accounts"

const url = process.env.SDK_RPC_URL!
const chain = {
  id: 9405,
  name: "UDC smoke",
  nativeCurrency: { name: "GLM", symbol: "GLM", decimals: 18 },
  rpcUrls: { default: { http: [url] } },
}
const transport = http(url, { retryCount: 0, timeout: 20_000 })
const alice = privateKeyToAccount(`0x${"02".repeat(32)}`)
const read = createPublicClient({ chain, transport, pollingInterval: 100 })
const write = createWalletClient({ chain, transport, account: alice, pollingInterval: 100 })
const expiration = ExpirationTime.fromBlocks(5000)

const Posts = defineSchema("posts", {
  author: addr().indexed(),
  category: str().indexed(),
  score: dec().indexed(),
  views: u64().indexed(),
  title: str(),
  body: bytes(),
  thumbnail: bytes().optional(),
  mimeType: str().default("text/markdown"),
})

// The SDK network-health probe: a named schema with an optional empty body.
const Probe = defineSchema("probe", {
  purpose: str().indexed(),
  body: bytes().optional(),
})
const PERMANENT = 2n ** 64n - 1n

const rpc = (q: string, opts: Record<string, unknown>) =>
  read.request({ method: "arkiv_query", params: [q, opts] } as any) as Promise<any>
const step = (name: string) => console.log("ok:", name)
const report = process.env.SDK_REPORT_FILE!
const stage = process.argv[2] ?? "exercise"

if (stage !== "exercise") {
  // After a three-role restart (reopen) and through the replicated full node (follower).
  const { entityKey, bigKey, permanentKey, extendedKey } = JSON.parse(readFileSync(report, "utf8"))
  for (const key of [permanentKey, extendedKey]) {
    assert.equal((await read.getEntity(key)).$expiresAt, PERMANENT)
  }
  const row = await read.getEntity(entityKey, Posts)
  assert.equal(row.title.value, "Patched")
  assert.equal(row.score.value, "4.9")
  assert.equal(row.thumbnail, undefined)
  const big = await read.getEntity(bigKey, Posts)
  assert.equal(big.body.value.length, 2 + 2 * 100 * 1024)
  const page = await read.select({ $key: true }).from(Posts).where(gt("score", dec("4.8"))).fetch()
  assert.deepEqual(
    page.entities.map((e) => e.$key),
    [entityKey],
  )
  console.log(`UDC SMOKE ${stage.toUpperCase()} PASSED`)
  process.exit(0)
}

const body = new Uint8Array(2048).map((_, i) => i % 251)
const { entityKey } = await write.createEntity(
  Posts,
  {
    author: addr(alice.address),
    category: "docs",
    score: dec("4.5"),
    views: u64(7n),
    title: "Hello UDC",
    body,
  },
  { expiration },
)
step(`create ${entityKey}`)

const row = await read.getEntity(entityKey, Posts)
assert.equal(row.title.value, "Hello UDC")
assert.equal(row.category.value, "docs")
assert.equal(row.mimeType.value, "text/markdown")
assert.equal(row.views.value, 7n)
assert.equal(row.body.value.toLowerCase(), bytesToHex(body))
assert.equal(row.author.value.toLowerCase(), alice.address.toLowerCase())
step("typed getEntity round-trips every column, 2 KiB bytes included")

const page = await read
  .select({ $key: true, title: true, $attributeSchema: true })
  .from(Posts)
  .where(and(eq("category", "docs"), gt("score", dec("4.0"))))
  .limit(10)
  .fetch()
assert.equal(page.entities.length, 1)
assert.equal(page.entities[0].$key, entityKey)
const schema = page.entities[0].$attributeSchema as Record<string, { type: string; indexed?: boolean }>
assert.deepEqual(
  Object.fromEntries(Object.entries(schema).map(([n, s]) => [n, [s.type, s.indexed]])),
  {
    author: ["addr", true],
    category: ["str", true],
    score: ["dec", true],
    views: ["u64", true],
    title: ["str", false],
    body: ["bytes", false],
    mimeType: ["str", false],
  },
)
step("typed query on indexed columns; attributeSchema reports indexed flags")

const raw = await rpc(`arkiv.schema = str('posts')`, {
  select: { key: true, attributeSchema: true },
})
const tag = raw.data[0].attributeSchema.find((a: any) => a.name === "arkiv.schema")
assert.deepEqual(tag, { name: "arkiv.schema", type: "str", indexed: true })
step("arkiv.schema tag stored as an indexed str")

const unindexed = await rpc(`title = str('Hello UDC')`, { select: { key: true } })
assert.equal(unindexed.data.length, 0)
step("unindexed column is not matched by a predicate")

for (const bad of [{ payload: true }, { contentType: true }]) {
  await assert.rejects(rpc(`$key = key(${entityKey})`, { select: bad }))
}
step("select rejects payload and contentType")

await write.patchEntity(Posts, entityKey, {
  set: { score: dec("4.9"), title: "Patched", thumbnail: new Uint8Array([1, 2, 3]) },
})
let after = await read.getEntity(entityKey, Posts)
assert.equal(after.score.value, "4.9")
assert.equal(after.title.value, "Patched")
assert.equal(after.thumbnail?.value, "0x010203")
const byScore = await read.select({ $key: true }).from(Posts).where(gt("score", dec("4.8"))).fetch()
assert.equal(byScore.entities.length, 1)
step("patch updates indexed and unindexed columns; index follows the patch")

await write.patchEntity(Posts, entityKey, { unset: ["thumbnail"] })
after = await read.getEntity(entityKey, Posts)
assert.equal(after.thumbnail, undefined)
step("unset removes an optional bytes column")

// Node-side per-entity cap: 100 KiB body, then a 100 KiB thumbnail patch (each op is
// within the SDK's per-operation budget; the resulting entity is not).
const big = await write.createEntity(
  Posts,
  {
    author: addr(alice.address),
    category: "big",
    score: dec("1"),
    views: u64(0n),
    title: "big",
    body: new Uint8Array(100 * 1024).fill(7),
  },
  { expiration },
)
step("create with a 100 KiB bytes column")
await assert.rejects(
  write.patchEntity(Posts, big.entityKey, {
    set: { thumbnail: new Uint8Array(100 * 1024).fill(9) },
  }),
)
const bigRow = await read.getEntity(big.entityKey, Posts)
assert.equal(bigRow.thumbnail, undefined)
step("node rejects a patch that takes the entity past 128 KiB of bytes; state unchanged")

const untyped = await read.getEntity(entityKey)
assert.equal((untyped.$attributes as any)["arkiv.schema"]?.value, "posts")
step("untyped getEntity returns every attribute")

// Permanence: ExpirationTime.permanent() is expiresAt = 2^64 - 1, as in the
// SDK's network-health test. The node charges it the capped rent.
const permanent = await write.createEntity(
  Probe,
  { purpose: "permanent_test", body: new Uint8Array() },
  { expiration: ExpirationTime.permanent() },
)
assert.equal(permanent.expiresAt, PERMANENT)
assert.equal((await read.getEntity(permanent.entityKey)).$expiresAt, PERMANENT)
step("permanent create: receipt and getEntity report expiresAt 2^64 - 1")

const again = await write.extendEntity({
  entityKey: permanent.entityKey,
  expiration: ExpirationTime.permanent(),
})
assert.equal(again.expiresAt, PERMANENT)
await assert.rejects(
  write.extendEntity({ entityKey: permanent.entityKey, expiration: ExpirationTime.fromBlocks(10) }),
)
step("extending a permanent entity to permanent succeeds; a finite expiry is refused")

const finite = await write.createEntity(Probe, { purpose: "extend_to_permanent" }, { expiration })
const extended = await write.extendEntity({
  entityKey: finite.entityKey,
  expiration: ExpirationTime.permanent(),
})
assert.equal(extended.expiresAt, PERMANENT)
assert.equal((await read.getEntity(finite.entityKey)).$expiresAt, PERMANENT)
step("a finite entity extends to permanent")

const status = (await (await fetch(`${url}/signed/v1/status`)).json()) as any
assert.equal(status.capabilities.gas.permanentEntitiesSupported, true)
assert.ok(BigInt(status.capabilities.gas.maxRentLifetimeBlocks) > 0n)
step(`status: permanent entities supported, rent capped at ${status.capabilities.gas.maxRentLifetimeBlocks} blocks`)

writeFileSync(
  report,
  JSON.stringify({
    entityKey,
    bigKey: big.entityKey,
    permanentKey: permanent.entityKey,
    extendedKey: finite.entityKey,
  }),
)
console.log("UDC SMOKE PASSED")
