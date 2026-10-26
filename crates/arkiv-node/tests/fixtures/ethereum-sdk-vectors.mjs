// Generates ethereum-sdk-vectors.json with viem 2.x, independently of the node.
// Usage: node ethereum-sdk-vectors.mjs VIEM_DIR udc > ethereum-sdk-vectors.json
// (VIEM_DIR holds node_modules/viem; "old" reproduces the payload-era vectors).
import { createRequire } from "node:module";
const require = createRequire(process.argv[2] + "/package.json");
const viem = require("viem");
const accounts = require("viem/accounts");
const mode = process.argv[3];
const account = accounts.privateKeyToAccount(`0x${"02".repeat(32)}`);
const ARKIV = "0x4400000000000000000000000000000000000044";
const abi = viem.parseAbi([
  "struct Operation { uint8 operation; bytes operationData; }",
  "function execute(Operation[] ops) returns (bytes32[] keys)",
]);
const attributeT = "(bytes32 name, uint8 typeId, bytes value)[]";
const createT = viem.parseAbiParameters(`(uint128 salt, uint64 expiresAt, uint64 minLifetime, uint8 creationFlags, ${attributeT} attributes)`);
const patchT = viem.parseAbiParameters(`(bytes32 entityKey, ${attributeT} mutations)`);
const name = (s) => viem.toHex(s, { size: 32 });
const word = (n) => viem.toHex(n, { size: 32 });
const attributes = mode === "old"
  ? [
      { name: name("$payload"), typeId: 7, value: "0x010203" },
      { name: name("title"), typeId: 8, value: viem.toHex("hello") },
    ]
  : [
      // The SDK's encodeCreate for a named schema "posts": strictly ascending by bytes32 name.
      { name: name("arkiv.schema"), typeId: 0x88, value: viem.toHex("posts") },
      { name: name("body"), typeId: 0x07, value: "0x010203" },
      { name: name("score"), typeId: 0x83, value: word(42n) },
      { name: name("title"), typeId: 0x08, value: viem.toHex("hello") },
    ];
const create = viem.encodeAbiParameters(createT, [{ salt: 9n, expiresAt: 0n, minLifetime: 20n, creationFlags: 0, attributes }]);
const data = viem.encodeFunctionData({ abi, functionName: "execute", args: [[{ operation: 1, operationData: create }]] });
const key = `0x${"ab".repeat(32)}`;
const batch = viem.encodeFunctionData({
  abi,
  functionName: "execute",
  args: [[
    { operation: 2, operationData: viem.encodeAbiParameters(patchT, [{ entityKey: key, mutations: [{ name: name("title"), typeId: 0, value: "0x" }] }]) },
    { operation: 3, operationData: viem.encodeAbiParameters(viem.parseAbiParameters("bytes32, uint64, uint64"), [key, 123n, 30n]) },
    { operation: 4, operationData: viem.encodeAbiParameters(viem.parseAbiParameters("bytes32, address"), [key, account.address]) },
    { operation: 5, operationData: viem.encodeAbiParameters(viem.parseAbiParameters("bytes32"), [key]) },
  ]],
});
const legacy = (fields) => account.signTransaction({ type: "legacy", chainId: 9300, nonce: 0, gas: 100000n, gasPrice: 1000000000n, to: ARKIV, data, ...fields });
const out = {
  address: account.address,
  data,
  legacy: await legacy({}),
  eip1559: await account.signTransaction({ type: "eip1559", chainId: 9300, nonce: 0, gas: 100000n, maxFeePerGas: 2000000000n, maxPriorityFeePerGas: 0n, to: ARKIV, data }),
  batch,
  lowFee: await legacy({ gasPrice: 999999999n }),
  transfer: await account.signTransaction({ type: "legacy", chainId: 9300, nonce: 0, gas: 100000n, gasPrice: 1000000000n, to: account.address, value: 123n }),
  unprotected: await legacy({ chainId: undefined }),
  contractCreation: await legacy({ to: undefined }),
  wrongDestination: await legacy({ to: account.address }),
  arkivValue: await legacy({ value: 1n }),
};
if (mode !== "old") {
  // The SDK's `ExpirationTime.permanent()`: expiresAt = 2^64 - 1, no floor,
  // for a Probe-like named schema with an empty body (network-health test).
  const permanent = viem.encodeAbiParameters(createT, [{
    salt: 10n, expiresAt: 2n ** 64n - 1n, minLifetime: 0n, creationFlags: 0,
    attributes: [
      { name: name("arkiv.schema"), typeId: 0x88, value: viem.toHex("probe") },
      { name: name("body"), typeId: 0x07, value: "0x" },
      { name: name("purpose"), typeId: 0x88, value: viem.toHex("permanent_test") },
    ],
  }]);
  out.permanentData = viem.encodeFunctionData({ abi, functionName: "execute", args: [[{ operation: 1, operationData: permanent }]] });
  out.permanent = await account.signTransaction({ type: "eip1559", chainId: 9300, nonce: 1, gas: 20000000n, maxFeePerGas: 2000000000n, maxPriorityFeePerGas: 0n, to: ARKIV, data: out.permanentData });
}
console.log(JSON.stringify(out, null, 2));
