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
// One bytes column at the 128 KiB per-entity cap.
const Blob = defineSchema("blob", { body: bytes() })
const maxBody = new Uint8Array(128 * 1024).map((_, i) => (i * 7) % 256)
const PERMANENT = 2n ** 64n - 1n
const GLM = 10n ** 18n

const rpc = (q: string, opts: Record<string, unknown>) =>
  read.request({ method: "arkiv_query", params: [q, opts] } as any) as Promise<any>
const step = (name: string) => console.log("ok:", name)
const report = process.env.SDK_REPORT_FILE!
const stage = process.argv[2] ?? "exercise"

if (stage !== "exercise") {
  // After a three-role restart (reopen) and through the replicated full node (follower).
  const { entityKey, bigKey, permanentKey, extendedKey, maxKey } = JSON.parse(
    readFileSync(report, "utf8"),
  )
  for (const key of [permanentKey, extendedKey, maxKey]) {
    assert.equal((await read.getEntity(key)).$expiresAt, PERMANENT)
  }
  assert.equal((await read.getEntity(maxKey, Blob)).body.value, bytesToHex(maxBody))
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
// SDK's network-health test. The node charges it the capped rent, in GLM.
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

// Rent is GLM outside the gas budget: a 128 KiB permanent create is one SDK
// call whose gas is execution only; the receipt's rentFee is the year of rent.
const call = (method: string, params: unknown[]) =>
  read.request({ method, params } as any) as Promise<any>
const balanceOf = async (who: string) => BigInt(await call("eth_getBalance", [who, "latest"]))
const nonceOf = async (who: string) => BigInt(await call("eth_getTransactionCount", [who, "latest"]))
const aliceBefore = await balanceOf(alice.address)
const max = await write.createEntity(Blob, { body: maxBody }, { expiration: ExpirationTime.permanent() })
assert.equal(max.expiresAt, PERMANENT)
const maxReceipt = (await read.getTransactionReceipt({ hash: max.txHash })) as any
const rentFee = BigInt(maxReceipt.rentFee)
assert.ok(rentFee > (9n * GLM) / 10n && rentFee < GLM, `rent ${rentFee}`)
// A year of whole storage units at 20 gwei a day.
assert.equal(rentFee % 7_300_000_000_000n, 0n)
assert.ok(maxReceipt.gasUsed < 1_000_000n, `gas ${maxReceipt.gasUsed}`)
assert.equal(
  aliceBefore - (await balanceOf(alice.address)),
  maxReceipt.gasUsed * maxReceipt.effectiveGasPrice + rentFee,
)
assert.equal((await read.getEntity(max.entityKey, Blob)).body.value, bytesToHex(maxBody))
step(
  `128 KiB permanent create in one call: ${maxReceipt.gasUsed} gas, rentFee ${rentFee} wei, both debited`,
)

// A sender holding the gas but not the rent is refused at admission, cleanly:
// no transaction, no nonce, no fee.
const describe = (e: any): string =>
  e ? [e.message, e.details, e.shortMessage, describe(e.cause)].filter(Boolean).join(" | ") : ""
const carol = privateKeyToAccount(`0x${"0c".repeat(32)}`)
const fund = async (value: bigint) =>
  read.waitForTransactionReceipt({
    hash: await (write as any).sendTransaction({ to: carol.address, value }),
  })
await fund(GLM / 100n)
const carolWrite = createWalletClient({ chain, transport, account: carol, pollingInterval: 100 })
let refusal = ""
await assert.rejects(
  carolWrite.createEntity(Blob, { body: maxBody }, { expiration: ExpirationTime.permanent() }),
  (e: any) => {
    refusal = describe(e)
    return /insufficient funds for gas \* price \+ value \+ rent/.test(refusal) || assert.fail(refusal)
  },
)
assert.equal(await nonceOf(carol.address), 0n)
assert.equal(await balanceOf(carol.address), GLM / 100n)
const [summary] = refusal.split(" | ")
const cause = refusal.split(" | ").find((part) => part.includes("+ rent"))
step(`a sender with gas but not rent is refused at admission, no nonce or fee: ${summary} (${cause})`)
await fund(GLM)
const carols = await carolWrite.createEntity(
  Blob,
  { body: maxBody },
  { expiration: ExpirationTime.permanent() },
)
assert.equal(carols.expiresAt, PERMANENT)
assert.equal(await nonceOf(carol.address), 1n)
step("topped up with gas plus rent, the same call commits")

// Only the owner makes an entity permanent: a permissionless-extension entity
// takes anyone's finite extension, never another account's permanence.
const open = await write.createEntity(
  Probe,
  { purpose: "permissionless" },
  { expiration, flags: { permissionlessExtension: true } },
)
const farther = await carolWrite.extendEntity({
  entityKey: open.entityKey,
  expiration: ExpirationTime.fromBlocks(20_000),
})
assert.ok(farther.expiresAt > open.expiresAt)
await assert.rejects(
  carolWrite.extendEntity({ entityKey: open.entityKey, expiration: ExpirationTime.permanent() }),
  (e: any) => /owned by|NotOwner/i.test(describe(e)) || assert.fail(describe(e)),
)
assert.equal((await read.getEntity(open.entityKey)).$expiresAt, farther.expiresAt)
const owned = await write.extendEntity({
  entityKey: open.entityKey,
  expiration: ExpirationTime.permanent(),
})
assert.equal(owned.expiresAt, PERMANENT)
step("anyone extends a permissionless entity finitely; only its owner makes it permanent")

const status = (await (await fetch(`${url}/signed/v1/status`)).json()) as any
const gas = status.capabilities.gas
assert.equal(gas.permanentEntitiesSupported, true)
assert.equal(gas.rentRule, "glm-rent-v3")
assert.equal(gas.rentCharging, "sender-balance-outside-gas")
assert.equal(gas.rentReceiptField, "rentFee")
// A year in seconds; 63,072,000 blocks at this chain's 500 ms.
assert.equal(gas.maxRentLifetimeSeconds, "31536000")
assert.equal(gas.maxRentLifetimeBlocks, "63072000")
step(
  `status: rent ${gas.rentWeiPerStorageUnitDay} wei/unit/day outside gas, capped at ${gas.maxRentLifetimeSeconds} s`,
)

writeFileSync(
  report,
  JSON.stringify({
    entityKey,
    bigKey: big.entityKey,
    permanentKey: permanent.entityKey,
    extendedKey: finite.entityKey,
    maxKey: max.entityKey,
  }),
)
console.log("UDC SMOKE PASSED")
