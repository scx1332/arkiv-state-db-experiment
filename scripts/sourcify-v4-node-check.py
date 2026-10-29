#!/usr/bin/env python3
"""Check a node started from a v4 Sourcify prefill against its bundle (read-only).

  keys    walk every entity of the dataset owner at one block in creation order
          (cursor pages, keys only) and require the i-th key to be the salt-0
          key of (chain, owner, nonce i), and the count to be the bundle's;
          --bundle-keys also compares each key with the bundle line's own key
  counts  arkiv_getEntityCount per kind (and in total) against report.json
  reads   `arkiv-entity-processor read-arkiv --format udc`, pinned at the block,
          for every contract of an expected NDJSON file (the bundle's
          `hydrate` output, or a sample of it), byte for byte

The node is only read: arkiv_query, arkiv_getEntityCount and eth_getBlockByNumber.
"""

import argparse
import concurrent.futures
import hashlib
import json
from pathlib import Path
import random
import subprocess
import sys
import time
import urllib.request

KINDS = ("contract", "compilation", "source", "code", "chunk")
ARKIV_ADDRESS = bytes.fromhex("44" + "00" * 18 + "44")


def keccak(data):
    return hashlib.new("KECCAK-256", data).digest()


def entity_key(chain_id, owner, nonce):
    return keccak(chain_id.to_bytes(32, "big") + ARKIV_ADDRESS + owner + nonce.to_bytes(8, "big") + bytes(16))


class Rpc:
    def __init__(self, url, timeout=120):
        self.url, self.timeout, self.calls = url, timeout, 0

    def call(self, method, params):
        body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
        for attempt in range(8):
            request = urllib.request.Request(self.url, body, {"content-type": "application/json"})
            self.calls += 1
            try:
                with urllib.request.urlopen(request, timeout=self.timeout) as response:
                    reply = json.loads(response.read())
            except urllib.error.HTTPError as error:
                if error.code == 429 and attempt < 7:
                    time.sleep(0.1 * 2 ** attempt)
                    continue
                raise
            if "error" in reply:
                if reply["error"].get("code") == -32010 and attempt < 7:
                    time.sleep(0.1 * 2 ** attempt)
                    continue
                raise RuntimeError(f"{method}: {reply['error']}")
            return reply["result"]
        raise RuntimeError(f"{method}: still busy")


def bundle(dir):
    seed = json.loads((dir / "seed.json").read_text())
    report = json.loads((dir / "report.json").read_text())
    return seed, report


def keys(args, rpc, seed, report):
    chain, owner = args.chain_id, bytes.fromhex(args.owner[2:])
    total = report["entities"]
    lines = None
    if args.bundle_keys:
        def bundle_lines():
            for part in seed["entities"]:
                with open(args.bundle / part["path"], "rb") as f:
                    for line in f:
                        yield json.loads(line)["key"]
        lines = bundle_lines()
    query = f"$owner = addr({args.owner})"
    cursor, nonce, started, last = None, 0, time.monotonic(), time.monotonic()
    while True:
        options = {"limit": hex(args.page), "atBlock": hex(args.block), "select": {"key": True, "attributes": False}}
        if cursor:
            options["cursor"] = cursor
        result = rpc.call("arkiv_query", [query, options])
        assert int(result["blockNumber"], 16) == args.block, "answered at another block"
        for row in result["data"]:
            key = row["key"].lower()
            expected = "0x" + entity_key(chain, owner, nonce).hex()
            if key != expected:
                raise SystemExit(f"keys: entity {nonce} has key {key}, predicted {expected}")
            if lines is not None:
                line_key = next(lines).lower()
                if key != line_key:
                    raise SystemExit(f"keys: entity {nonce} has key {key}, the bundle line {line_key}")
            nonce += 1
        cursor = result.get("cursor")
        if time.monotonic() - last >= 30:
            print(json.dumps({"keys": nonce, "of": total, "seconds": round(time.monotonic() - started)}), file=sys.stderr, flush=True)
            last = time.monotonic()
        if not cursor:
            break
    if nonce != total:
        raise SystemExit(f"keys: the node holds {nonce} entities of the owner, the bundle {total}")
    if lines is not None and next(lines, None) is not None:
        raise SystemExit("keys: the bundle has more lines than the node")
    return {"keys": nonce, "inCreationOrder": True, "bundleLineKeysCompared": lines is not None,
            "queries": rpc.calls, "seconds": round(time.monotonic() - started, 1)}


def counts(args, rpc, seed, report):
    out, prefix = {}, seed["schemaPrefix"]
    owner_clause = f"$owner = addr({args.owner})"
    for kind in KINDS:
        n = rpc.call("arkiv_getEntityCount", [{"query": f"{owner_clause} AND arkiv.schema = str('{prefix}.{kind}')",
                                                "block": args.block}])
        out[kind] = n
    out["total"] = rpc.call("arkiv_getEntityCount", [{"query": owner_clause, "block": args.block}])
    expected = {k: report["kinds"].get(k, 0) for k in KINDS} | {"total": report["entities"]}
    if out != expected:
        raise SystemExit(f"counts: node {out}, bundle {expected}")
    return {"counts": out, "matchesReport": True}


def reads(args, rpc, seed, report):
    with open(args.expected, "rb") as f:
        expected = [line for line in f if line.strip()]
    chosen = list(range(len(expected)))
    if args.sample and args.sample < len(expected):
        chosen = sorted(random.Random(args.seed).sample(chosen, args.sample))
    started = time.monotonic()

    def one(i):
        line = expected[i]
        address = json.loads(line)["address"]
        command = [str(args.processor), "read-arkiv", "--rpc-url", args.rpc, "--owner", args.owner,
                   "--arkiv-chain-id", str(args.chain_id), "--format", "udc", "--schema-prefix", seed["schemaPrefix"],
                   "--read-view", "pinned", "--block", str(args.block), address]
        for attempt in range(3):
            done = subprocess.run(command, capture_output=True)
            if done.returncode == 0:
                break
        stats = {}
        if done.returncode == 0:
            try:
                stats = json.loads(done.stderr.decode().strip().splitlines()[-1]).get("readArkiv", {})
            except (ValueError, IndexError):
                pass
        return i, address, done.returncode, done.stdout == line, done.stderr.decode()[-500:], stats

    failures, queries, entities = [], 0, 0
    with concurrent.futures.ThreadPoolExecutor(args.concurrency) as pool:
        for n, (i, address, code, same, err, stats) in enumerate(pool.map(one, chosen), 1):
            queries += stats.get("queries", 0)
            entities += stats.get("entities", 0)
            if code != 0 or not same:
                failures.append({"line": i + 1, "address": address, "exit": code, "identical": same, "stderr": err})
            if n % 500 == 0:
                print(json.dumps({"reads": n, "of": len(chosen), "failures": len(failures),
                                  "seconds": round(time.monotonic() - started)}), file=sys.stderr, flush=True)
    result = {"contracts": len(chosen), "of": len(expected), "identical": len(chosen) - len(failures),
              "failures": failures[:20], "queries": queries, "entities": entities,
              "seconds": round(time.monotonic() - started, 1),
              "sampleSha256": hashlib.sha256(b"".join(expected[i] for i in chosen)).hexdigest()}
    if failures:
        print(json.dumps(result, indent=2), file=sys.stderr)
        raise SystemExit(f"reads: {len(failures)} of {len(chosen)} contracts differ or failed")
    return result


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("check", choices=("keys", "counts", "reads"))
    p.add_argument("--rpc", required=True)
    p.add_argument("--bundle", type=Path, required=True, help="the v4 bundle the prefill came from")
    p.add_argument("--block", type=int, default=0)
    p.add_argument("--chain-id", type=int, help="chain the prefill was converted for (default: the bundle's)")
    p.add_argument("--owner", help="creator of the rows (default: the bundle's owner); with --chain-id, a re-keyed prefill")
    p.add_argument("--page", type=int, default=200)
    p.add_argument("--bundle-keys", action="store_true", help="keys: also read every bundle line's key")
    p.add_argument("--expected", type=Path, help="reads: NDJSON of hydrated contracts (bundle hydrate output)")
    p.add_argument("--processor", type=Path, help="reads: arkiv-entity-processor binary")
    p.add_argument("--sample", type=int, default=0, help="reads: a random sample of this many (0: all)")
    p.add_argument("--seed", type=int, default=20261029)
    p.add_argument("--concurrency", type=int, default=4)
    args = p.parse_args()
    seed, report = bundle(args.bundle)
    args.chain_id = seed["chainId"] if args.chain_id is None else args.chain_id
    args.owner = (seed["owner"] if args.owner is None else args.owner).lower()
    if args.bundle_keys and (args.chain_id, args.owner) != (seed["chainId"], seed["owner"].lower()):
        raise SystemExit("--bundle-keys compares with the bundle's own keys: not for a re-keyed prefill")
    rpc = Rpc(args.rpc)
    head = rpc.call("eth_getBlockByNumber", [hex(args.block), False])
    result = {"keys": keys, "counts": counts, "reads": reads}[args.check](args, rpc, seed, report)
    result.update({"check": args.check, "block": args.block, "blockHash": head["hash"], "owner": args.owner,
                   "chainId": args.chain_id, "schemaPrefix": seed["schemaPrefix"]})
    print(json.dumps(result, sort_keys=True))


if __name__ == "__main__":
    main()
