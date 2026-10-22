#!/usr/bin/env python3
"""Clause-order and count benchmark of arkiv_query / arkiv_getEntityCount.

Runs the shapes of the 2026-10-22 external node benchmark against one node:
lookups written in the Block Explorer's order (scope first, selective clause
last) and in the selective order, scope AND (name = A OR ...), counts of
single filters, intersections and unions, and AND/OR ladders past the
genesis bounds. Values are discovered from the node's own data (a Sourcify
prefill) and saved, so a second run against another binary on the same
chain uses the same queries; every answer is hashed so runs can be compared
answer by answer.

Usage:
  query-order-bench.py --url http://127.0.0.1:19631 --out RESULT.json
      [--values VALUES.json] [--runs 3] [--timeout 40] [--repeat-below 5]
      [--only REGEX]

Read-only: it sends JSON-RPC reads only.
"""

import argparse
import hashlib
import json
import re
import statistics
import sys
import time
import urllib.error
import urllib.request


def rpc(url, method, params, timeout):
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
    request = urllib.request.Request(url, data=body, headers={"Content-Type": "application/json"})
    started = time.monotonic()
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            answer = json.loads(response.read())
    except urllib.error.HTTPError as error:
        answer = json.loads(error.read() or b"{}")
    except Exception as error:  # timeouts and resets are results here
        answer = {"error": {"message": type(error).__name__}}
    elapsed = time.monotonic() - started
    return answer, elapsed


def discover(url, timeout):
    """Common values of the Sourcify attributes, read from the first pages."""
    counts = {}
    cursor = None
    for _ in range(10):
        options = {"limit": 200, "select": {"key": True, "owner": True, "attributes": True}}
        if cursor:
            options.update(cursor=cursor, atBlock=at_block)
        answer, _ = rpc(url, "arkiv_query", ["*", options], timeout)
        result = answer["result"]
        at_block = result["blockNumber"]
        for entity in result["data"]:
            counts.setdefault("$owner", {}).setdefault(("addr", entity["owner"]), 0)
            counts["$owner"][("addr", entity["owner"])] += 1
            for attribute in entity["attributes"]:
                slot = counts.setdefault(attribute["name"], {})
                value = (attribute["type"], json.dumps(attribute["value"]))
                slot[value] = slot.get(value, 0) + 1
        cursor = result.get("cursor")
        if not cursor:
            break
    def common(name):
        (kind, value), _ = max(counts[name].items(), key=lambda item: item[1])
        return literal(kind, json.loads(value) if name != "$owner" else value)
    values = {
        "owner": common("$owner"),
        "projectid": common("projectid"),
        "uploadschema": common("uploadschema"),
        "chainid": common("chainid"),
    }
    return values


def literal(kind, value):
    if kind == "str":
        return f"str('{value}')"
    if kind == "addr":
        return f"addr({value})"
    if kind == "u64":
        return f"u64({int(value, 16) if isinstance(value, str) else value})"
    if kind == "bool":
        return "true" if value else "false"
    raise ValueError(kind)


def first_value(url, query, name, timeout):
    """The value of attribute `name` on the first entity matching `query`."""
    answer, _ = rpc(url, "arkiv_query", [query, {"limit": 1, "select": {"attributes": True}}], timeout)
    for attribute in answer["result"]["data"][0]["attributes"]:
        if attribute["name"] == name:
            return literal(attribute["type"], attribute["value"])
    raise KeyError(name)


def cases(values):
    v = values
    scope = [
        f"projectid = {v['projectid']}",
        f"$owner = {v['owner']}",
        f"uploadschema = {v['uploadschema']}",
        "kind = str('verified_contract')",
        f"chainid = {v['chainid']}",
    ]
    filters = {
        "owner": f"$owner = {v['owner']}",
        "lang": "language = str('Solidity')",
        "kind": "kind = str('verified_contract')",
        "opt": "optimizer = true",
        "paris": "evmversion = str('paris')",
        "viair": "viair = true",
        "safe": "name STARTSWITH str('Safe')",
    }
    address = f"address = {v['address']}"
    name = f"name = {v['name']}"
    out = []

    def query(case, clauses, joiner=" AND "):
        out.append({"case": case, "method": "arkiv_query", "source": joiner.join(clauses)})

    def count(case, clauses, joiner=" AND "):
        out.append({"case": case, "method": "arkiv_getEntityCount", "source": joiner.join(clauses)})

    query("S1 address lookup", [address])
    query("S2 address + scope, explorer order", scope + [address])
    query("S3 address + scope, address first", [address] + scope)
    query("S4 name + scope, explorer order", scope + [name])
    query("S4 name + scope, name first", [name] + scope)
    query("S5 scope only", scope)
    query("S6 viair + scope, explorer order", scope + ["viair = true"])
    query("S6 viair + scope, viair first", ["viair = true"] + scope)
    query("S7 Safe prefix + scope, explorer order", scope + [filters["safe"]])
    query("S7 Safe prefix + scope, prefix first", [filters["safe"]] + scope)
    query("S8 paris + optimizer + Solidity", [filters["paris"], filters["opt"], filters["lang"]])
    seven = [filters[k] for k in ("safe", "viair", "paris", "opt", "kind", "lang", "owner")]
    query("S9 all seven filters, selective first", seven)
    query("S9 all seven filters, broad first", list(reversed(seven)))
    window = [f"verifiedat >= u64({v['verified_from']})", f"verifiedat < u64({v['verified_to']})"]
    query("S10 verifiedat window + scope, range first", window + scope)
    query("S10 verifiedat window + scope, explorer order", scope + window)
    for key, clause in filters.items():
        count(f"count {key}", [clause])
    count("count match-all", [])
    count("count scope", scope)
    for pair in (("owner", "kind"), ("lang", "kind"), ("kind", "opt"), ("kind", "paris"),
                 ("opt", "viair"), ("paris", "viair"), ("lang", "safe"), ("viair", "safe")):
        count(f"count {'+'.join(pair)}", [filters[k] for k in pair])
        count(f"count {'+'.join(reversed(pair))}", [filters[k] for k in reversed(pair)])
    names = v["names"]
    for n in (1, 2, 3, 5, 8, 10):
        ors = "(" + " OR ".join(f"name = {x}" for x in names[:n]) + ")"
        query(f"OR {n} names, scope first", scope + [ors])
        query(f"OR {n} names, OR first", [ors] + scope)
        count(f"count OR {n} names + scope", scope + [ors])
        count(f"count OR {n} names", [ors])
    extra = [f"language = str('Solidity')", "optimizer = true", f"evmversion = {v['evm']}",
             f"compiler = {v['compiler']}"]
    for n in (8, 9, 10):
        clauses = ([address] + scope + extra)[:n]
        query(f"ladder {n} AND clauses, address first", clauses)
        query(f"ladder {n} AND clauses, address last", clauses[1:] + clauses[:1])
        count(f"count ladder {n} AND clauses", clauses)
    return out


def summarize(answer, url, params, timeout):
    """The answer's size and a digest of what it returned. A page with a
    cursor is digested with the page its cursor continues to (cursor tokens
    themselves are process-local and left out)."""
    if "error" in answer:
        return {"error": answer["error"].get("message")}
    result = answer["result"]
    if not isinstance(result, dict):
        digest = hashlib.sha256(json.dumps(result, sort_keys=True).encode()).hexdigest()[:16]
        return {"count": result, "sha": digest}
    content = {key: value for key, value in result.items() if key != "cursor"}
    if "cursor" in result:
        options = dict(params[1], cursor=result["cursor"], atBlock=result["blockNumber"])
        following, _ = rpc(url, "arkiv_query", [params[0], options], timeout)
        following = following.get("result", following)
        content["next"] = {key: value for key, value in following.items() if key != "cursor"}
        content["next"]["more"] = "cursor" in following
    digest = hashlib.sha256(json.dumps(content, sort_keys=True).encode()).hexdigest()[:16]
    return {"rows": len(result.get("data", [])), "more": "cursor" in result, "sha": digest}


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--url", required=True)
    parser.add_argument("--out", required=True)
    parser.add_argument("--values", help="reuse (or save) the discovered values")
    parser.add_argument("--runs", type=int, default=3)
    parser.add_argument("--timeout", type=float, default=40)
    parser.add_argument("--only", help="run only the cases whose name matches this regex")
    parser.add_argument("--repeat-below", type=float, default=5,
                        help="repeat a case only while it answers faster than this (s)")
    args = parser.parse_args()
    values = None
    if args.values:
        try:
            with open(args.values) as stream:
                values = json.load(stream)
        except FileNotFoundError:
            pass
    if values is None:
        values = discover(args.url, args.timeout)
        values["address"] = first_value(args.url, "kind = str('verified_contract')", "address", args.timeout)
        values["name"] = first_value(args.url, "kind = str('verified_contract')", "name", args.timeout)
        values["evm"] = first_value(args.url, "kind = str('verified_contract')", "evmversion", args.timeout)
        values["compiler"] = first_value(args.url, "kind = str('verified_contract')", "compiler", args.timeout)
        stamp = first_value(args.url, "kind = str('verified_contract')", "verifiedat", args.timeout)
        start = int(stamp[4:-1])
        values["verified_from"], values["verified_to"] = start, start + 4 * 86400
        names = []
        for prefix in ("Safe", "ERC20", "Token", "Proxy", "Ownable", "Uniswap", "WETH", "Gnosis",
                       "Lib", "Math", "Strings", "Pool"):
            answer, _ = rpc(args.url, "arkiv_query",
                            [f"name STARTSWITH str('{prefix}')",
                             {"limit": 50, "select": {"attributes": True}}], args.timeout)
            for entity in answer.get("result", {}).get("data", []):
                for attribute in entity["attributes"]:
                    if attribute["name"] == "name":
                        value = literal(attribute["type"], attribute["value"])
                        if value not in names:
                            names.append(value)
                        break
                if len(names) >= 12:
                    break
        values["names"] = names[:12]
        if args.values:
            with open(args.values, "w") as stream:
                json.dump(values, stream, indent=1)
    results = []
    for case in cases(values):
        if args.only and not re.search(args.only, case["case"]):
            continue
        if case["method"] == "arkiv_query":
            params = [case["source"] or "*", {"limit": 20, "select": {"key": True, "attributes": True}}]
        else:
            params = [{"query": case["source"]}] if case["source"] else []
        timings, summary = [], None
        for _ in range(args.runs):
            answer, elapsed = rpc(args.url, case["method"], params, args.timeout)
            timings.append(elapsed)
            summary = summarize(answer, args.url, params, args.timeout)
            if elapsed > args.repeat_below:
                break  # a slow answer is not repeated
        case.update(summary, ms=round(statistics.median(timings) * 1000, 1),
                    ms_all=[round(t * 1000, 1) for t in timings])
        results.append(case)
        print(f"{case['ms']:>9.1f} ms  {json.dumps(summary)}  {case['case']}", flush=True)
    with open(args.out, "w") as stream:
        json.dump({"url": args.url, "values": values, "results": results}, stream, indent=1)


if __name__ == "__main__":
    sys.exit(main())
