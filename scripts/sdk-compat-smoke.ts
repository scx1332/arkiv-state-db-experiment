/**
 * Runs unchanged sibling SDK source against an isolated native chain.
 *
 * PRE-UDC: written for the payload-era SDK (0.8.x: `stringToPayload`,
 * payload/contentType writes and selections). Current nodes implement the
 * user-defined-columns model and refuse those, so this smoke is expected to
 * fail against them; a separate UDC smoke replaces it.
 */
import assert from "node:assert/strict";
import { readFileSync, writeFileSync } from "node:fs";
import {
	createPublicClient,
	createWalletClient,
	ExpirationTime,
	stringToPayload,
} from "@arkiv-network/sdk";
import {
	addr,
	bool,
	bytes32,
	dec,
	i32,
	key,
	str,
	u64,
	u256,
} from "@arkiv-network/sdk/attr";
import {
	and,
	eq,
	exists,
	gte,
	hasType,
	not,
	or,
	startsWith,
} from "@arkiv-network/sdk/query";
import { http, keccak256, parseEther, webSocket } from "viem";
import { privateKeyToAccount } from "viem/accounts";

const url = process.env.SDK_RPC_URL!;
const chain = {
	id: 9405,
	name: "Native SDK acceptance",
	nativeCurrency: { name: "GLM", symbol: "GLM", decimals: 18 },
	rpcUrls: { default: { http: [url] } },
};
const transport = http(url, { retryCount: 0, timeout: 10_000 });
const alice = privateKeyToAccount(`0x${"02".repeat(32)}`);
const bob = privateKeyToAccount(`0x${"03".repeat(32)}`);
const read = createPublicClient({ chain, transport, pollingInterval: 100 });
const write = createWalletClient({
	chain,
	transport,
	account: alice,
	pollingInterval: 100,
});
const other = createWalletClient({
	chain,
	transport,
	account: bob,
	pollingInterval: 100,
});
const file = process.env.SDK_REPORT_FILE!;
const lifetime = ExpirationTime.fromBlocks(10000);
const receipt = (hash: `0x${string}`) =>
	read.waitForTransactionReceipt({
		hash,
		timeout: 30_000,
		pollingInterval: 100,
	});
const stage = process.argv[2] ?? "exercise";

if (stage === "websocket") {
	const ws = webSocket(url.replace(/^http/, "ws"), {
		retryCount: 0,
		timeout: 10_000,
		reconnect: false,
	});
	const reader = createPublicClient({
		chain,
		transport: ws,
		pollingInterval: 100,
	});
	const writer = createWalletClient({
		chain,
		transport: ws,
		account: alice,
		pollingInterval: 100,
	});
	const events: any[] = [],
		blocks: any[] = [],
		errors: Error[] = [];
	const unwatch = reader.watchEntityEvents({
		onEvent: (e) => events.push(e),
		onError: (e) => errors.push(e),
		pollingInterval: 100,
	});
	const stopBlocks = reader.watchBlocks({
		onBlock: (b) => blocks.push(b),
		onError: (e) => errors.push(e),
		poll: false,
	});
	try {
		assert.equal(await reader.getChainId(), chain.id);
		await Bun.sleep(300);
		const created = await writer.createEntity({
			payload: stringToPayload("WebSocket SDK"),
			contentType: "text/plain",
			expires: lifetime,
		});
		assert.equal(
			(await reader.getEntity(created.entityKey)).toText(),
			"WebSocket SDK",
		);
		const deadline = Date.now() + 10_000;
		while (
			Date.now() < deadline &&
			(!events.some((e) => e.entityKey === created.entityKey) || !blocks.length)
		)
			await Bun.sleep(100);
		assert.deepEqual(errors, []);
		assert(
			events.some(
				(e) => e.type === "EntityCreated" && e.entityKey === created.entityKey,
			),
		);
		assert(blocks.length > 0);
		console.log(
			"PASS unchanged SDK WebSocket signed create/read, entity log subscription and newHeads subscription",
		);
	} finally {
		unwatch();
		stopBlocks();
		(await reader.transport.getRpcClient()).close();
	}
	process.exit(0);
}

if (stage === "reopen" || stage === "follower") {
	const saved = JSON.parse(readFileSync(file, "utf8"));
	const tx = await read.getTransactionReceipt({ hash: saved.txHash });
	assert.equal(tx.blockHash, saved.blockHash);
	assert.equal(tx.status, "success");
	assert.equal(
		(await read.getEntity(saved.survivingKey)).toText(),
		"survives restart",
	);
	const historical = await read
		.select()
		.where(eq("$key", key(saved.deletedKey)))
		.atBlock(BigInt(saved.createdAt))
		.fetch();
	assert.equal(historical.entities.length, 1);
	assert.equal(historical.entities[0].toText(), "SDK payload");
	if (stage === "reopen")
		assert.equal(
			await read.sendRawTransaction({ serializedTransaction: saved.raw }),
			saved.txHash,
		);
	console.log(
		"PASS SDK receipts, retained historical query, live entity and duplicate raw transaction after restart",
	);
	process.exit(0);
}

assert.equal(await read.getChainId(), chain.id);
assert.equal(await read.getGasPrice(), 1_000_000_000n);
assert.equal(await read.getEntityNonce(alice.address), 2n);
assert.equal(await read.getTransactionCount({ address: alice.address }), 0);
assert.equal(await read.getEntityCount(), 3);

// An unfunded caller must reach signed submission after gas estimation. The
// SDK currently wraps non-revert contract errors with a generic top-level
// message; viem and the original RPC cause must still identify insufficient funds.
const unfunded = privateKeyToAccount(`0x${"04".repeat(32)}`);
const unfundedWriter = createWalletClient({ chain, transport, account: unfunded });
assert.equal(await read.getBalance({ address: unfunded.address }), 0n);
await assert.rejects(
	unfundedWriter.createEntity({
		payload: new Uint8Array(5000),
		contentType: "application/octet-stream",
		expires: lifetime,
	}),
	(error: any) => {
		const causes: any[] = [];
		for (let cause = error; cause; cause = cause.cause) causes.push(cause);
		assert(causes.some((cause) => cause.name === "InsufficientFundsError"));
		assert(causes.some((cause) => /eth_sendRawTransaction/.test(cause.message)));
		return true;
	},
);
assert.equal(await read.getTransactionCount({ address: unfunded.address }), 0);
assert.equal(await read.getEntityNonce(unfunded.address), 0n);
assert.equal(await read.getBalance({ address: unfunded.address }), 0n);
console.log("PASS unfunded SDK create estimates gas, then signed submission fails with insufficient funds without changing state");
const timing = await read.getBlockTiming();
assert(
	timing.currentBlock >= 0n &&
		timing.currentBlockTime > 0 &&
		timing.blockDuration >= 0,
);
const genesis = await read
	.select()
	.where(eq("group", u64(42n)))
	.atBlock(0n)
	.fetch();
assert.equal(genesis.entities.length, 3);
assert(genesis.entities.every((e) => e.createdAt === 0n));
console.log(
	"PASS unchanged SDK discovery, gas, separate nonces, timing and genesis reads",
);

const [predicted] = await read.predictEntityKeys({
	owner: alice.address,
	salts: [99n],
});
const attributes = {
	suite: str("sdk-e2e"),
	label: str("alpha"),
	rank: i32(-3),
	amount: u64(42n),
	large: u256(2n ** 200n),
	ratio: dec("-12.5"),
	enabled: bool(true),
	ownerRef: addr(bob.address),
	fixed: bytes32(`0x${"ab".repeat(32)}`),
	reference: key(predicted.key),
};
const created = await write.createEntity({
	payload: stringToPayload("SDK payload"),
	contentType: "text/plain",
	attributes,
	expires: lifetime,
	salt: 99n,
});
assert.equal(created.entityKey, predicted.key);
const creation = await receipt(created.txHash);
assert.equal(creation.status, "success");
assert.equal(creation.effectiveGasPrice, 1_000_000_000n);
const entity = await read.getEntity(created.entityKey);
assert.equal(entity.toText(), "SDK payload");
assert.deepEqual(entity.attributes, attributes);
assert.equal(entity.creator.toLowerCase(), alice.address.toLowerCase());
assert.equal(entity.createdAt, creation.blockNumber);
assert.equal(await read.getEntityNonce(alice.address), 3n);
assert.equal(await read.getTransactionCount({ address: alice.address }), 1);
console.log(
	"PASS SDK create with default fee/gas estimation, original signing, all scalar types and receipt event decoding",
);

const page = await read
	.select({ key: true, attributes: { rank: true } })
	.where(
		and(
			eq("suite", str("sdk-e2e")),
			or(gte("rank", i32(-3)), startsWith("label", "zzz")),
			not(eq("enabled", bool(false))),
		),
	)
	.fetch();
assert.deepEqual(
	page.entities.map((e) => e.key),
	[created.entityKey],
);
assert.equal(page.entities[0].payload, undefined);
assert.deepEqual(page.entities[0].attributes, { rank: i32(-3) });
const existence = await read
	.select({ key: true })
	.where(
		and(eq("suite", str("sdk-e2e")), exists("large"), hasType("rank", "i32")),
	)
	.fetch();
assert.equal(existence.entities.length, 1);
const prefillFirst = await read
	.select({ key: true })
	.where(eq("group", u64(42n)))
	.atBlock(0n)
	.limit(1)
	.fetch();
const walked = [...prefillFirst.entities];
let next = prefillFirst;
while (next.hasNextPage()) {
	next = await next.next();
	walked.push(...next.entities);
}
assert.equal(new Set(walked.map((e) => e.key)).size, 3);
console.log(
	"PASS compound/range/prefix/NOT/EXISTS/TYPEOF queries, exact projection and historical pagination",
);

await write.patchEntity({
	entityKey: created.entityKey,
	set: { label: str("patched") },
	unset: ["rank"],
	payload: stringToPayload("changed"),
});
assert.equal(
	(await read.getEntity(created.entityKey)).attributes.rank,
	undefined,
);
const extended = await write.extendEntity({
	entityKey: created.entityKey,
	expires: ExpirationTime.fromBlocks(20000),
});
assert(extended.expiresAt > created.expiresAt);
await write.changeOwnership({
	entityKey: created.entityKey,
	newOwner: bob.address,
});
assert.equal(
	(await read.getEntity(created.entityKey)).owner.toLowerCase(),
	bob.address.toLowerCase(),
);
await other.deleteEntity({ entityKey: created.entityKey });
await assert.rejects(read.getEntity(created.entityKey));
const old = await read
	.select()
	.where(eq("$key", key(created.entityKey)))
	.atBlock(creation.blockNumber)
	.fetch();
assert.equal(old.entities[0].toText(), "SDK payload");
assert.deepEqual(old.entities[0].attributes, attributes);
const logs = await read.getLogs({
	fromBlock: creation.blockNumber,
	toBlock: "latest",
	address: "0x4400000000000000000000000000000000000044",
});
assert.equal(logs.length, 5);
console.log(
	"PASS SDK patch/extend/transfer/delete, five lifecycle logs and immutable historical state",
);

const accountNonce = await read.getTransactionCount({ address: alice.address });
const entityNonce = await read.getEntityNonce(alice.address);
await assert.rejects(
	write.executeBatch(
		{
			creates: [
				{
					payload: stringToPayload("rollback"),
					contentType: "text/plain",
					attributes: { rollback: str("sdk") },
					expires: lifetime,
				},
			],
			deletes: [{ entityKey: `0x${"ff".repeat(32)}` }],
		},
		{ gas: 2_000_000n, gasPrice: 1_000_000_000n },
	),
);
assert.equal(await read.getEntityNonce(alice.address), entityNonce);
assert.equal(
	await read.getTransactionCount({ address: alice.address }),
	accountNonce + 1,
);
assert.equal(
	(
		await read
			.select({ key: true })
			.where(eq("rollback", str("sdk")))
			.fetch()
	).entities.length,
	0,
);
console.log(
	"PASS failed signed batch consumes transaction nonce and rolls back entity state/creation nonce",
);

const survive = await write.createEntity({
	payload: stringToPayload("survives restart"),
	contentType: "text/plain",
	expires: lifetime,
});
const beforeAlice = await read.getBalance({ address: alice.address });
const beforeBob = await read.getBalance({ address: bob.address });
const nonce = await read.getTransactionCount({ address: alice.address });
const raw = await write.signTransaction({
	chain,
	account: alice,
	to: bob.address,
	value: parseEther("0.1"),
	nonce,
	gas: 21_000n,
	gasPrice: 1_000_000_000n,
	type: "legacy",
});
const txHash = await read.sendRawTransaction({ serializedTransaction: raw });
assert.equal(txHash, keccak256(raw));
const transfer = await receipt(txHash);
assert.equal(transfer.status, "success");
assert.equal(
	await read.getBalance({ address: bob.address }),
	beforeBob + parseEther("0.1"),
);
assert.equal(
	await read.getBalance({ address: alice.address }),
	beforeAlice -
		parseEther("0.1") -
		transfer.gasUsed * transfer.effectiveGasPrice,
);
const block = await read.getBlock({
	blockNumber: transfer.blockNumber,
	includeTransactions: true,
});
assert(block.transactions.some((t) => t.hash === txHash));
assert.equal(
	await read.sendRawTransaction({ serializedTransaction: raw }),
	txHash,
);
console.log(
	"PASS signed native-token transfer, exact fee accounting, Ethereum hashes, block views and duplicate submission",
);

const { EthereumRpcClient } = await import(process.env.SDK_INDEXER_FILE!);
const indexer = new EthereumRpcClient(url, "");
assert.equal(await indexer.getChainId(), chain.id);
assert((await indexer.getLatestBlockNumber()) >= transfer.blockNumber);
const indexed = await indexer.getBlockWithTransactions(transfer.blockNumber);
assert(
	indexed.transactions.some(
		(tx: any) =>
			tx.hash === txHash &&
			tx.from.toLowerCase() === alice.address.toLowerCase(),
	),
);
assert.equal((await indexer.getTransactionReceipt(txHash)).status, "0x1");
const balances = await indexer.getBalances(
	[alice.address, bob.address],
	transfer.blockNumber,
);
assert.equal(
	BigInt(balances.get(bob.address.toLowerCase())),
	beforeBob + parseEther("0.1"),
);
console.log(
	"PASS unchanged indexer RPC client: chain/head, full block transactions, receipt and batched historical balances",
);

writeFileSync(
	file,
	JSON.stringify(
		{
			txHash,
			raw,
			blockHash: transfer.blockHash,
			survivingKey: survive.entityKey,
			deletedKey: created.entityKey,
			createdAt: creation.blockNumber.toString(),
			transferHeight: transfer.blockNumber.toString(),
		},
		null,
		2,
	),
);
