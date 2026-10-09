#!/usr/bin/env python3
"""Real-process demo acceptance tests. No third-party Python dependencies."""
from __future__ import annotations

import argparse
import copy
import http.client
import importlib.util
import json
from pathlib import Path
import struct
import subprocess
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import urllib.request

sys.dont_write_bytecode = True

spec = importlib.util.spec_from_file_location("full_light_demo", Path(__file__).with_name("full-light-demo.py"))
demo = importlib.util.module_from_spec(spec)
spec.loader.exec_module(demo)


def check(condition, message):
    if not condition:
        raise AssertionError(message)


def expect_error(code, call):
    try:
        call()
    except demo.RpcError as error:
        check(error.code == code, f"expected {code}, received {error.code}")
        return
    raise AssertionError(f"expected {code}, call succeeded")


class Proxy:
    """Deterministic mutation/dropped-response peer; verification stays untouched."""
    def __init__(self, target):
        self.target = target
        self.mode = "pass"
        self.hits = 0
        self.batches = {}
        self.stale_hash = None
        self.lock = threading.Lock()
        outer = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def handle(self):
                try:
                    super().handle()
                except (ConnectionResetError, BrokenPipeError):
                    pass  # restarted followers and capped readers disconnect

            def log_message(self, *_):
                pass

            def do_POST(self):
                length = int(self.headers.get("Content-Length", "0"))
                if length > demo.REQUEST_CAP:
                    self.send_error(413)
                    return
                raw = self.rfile.read(length)
                rpc_request = json.loads(raw)["request"]
                method = rpc_request["method"]
                with outer.lock:
                    mode = outer.mode
                sync_mutation = method in ("headers", "blocks") and mode in (
                    f"gap_{method}", f"prefix_gap_{method}", f"stale_{method}")
                active = sync_mutation or (mode == "drop_blocks" and method == "blocks") or (mode == "bad_header" and method == "headers") or (mode in ("bad_proof", "missing_id", "truncate", "oversize_response") and method == "queryEq")
                if active:
                    with outer.lock:
                        outer.hits += 1
                if active and mode == "drop_blocks":
                    self.send_error(503, "deterministic unavailable body")
                    return
                if active and mode == "oversize_response":
                    self.send_response(200)
                    self.send_header("Content-Type", "application/json")
                    self.send_header("Transfer-Encoding", "chunked")
                    self.end_headers()
                    try:
                        remaining = demo.RESPONSE_CAP + 1
                        while remaining:
                            chunk = b" " * min(65536, remaining)
                            self.wfile.write(f"{len(chunk):x}\r\n".encode() + chunk + b"\r\n")
                            remaining -= len(chunk)
                        self.wfile.write(b"0\r\n\r\n")
                    except OSError:
                        pass  # bounded client intentionally stops reading
                    return
                try:
                    request = urllib.request.Request(outer.target + "/rpc", raw, {"Content-Type": "application/json"})
                    with urllib.request.urlopen(request, timeout=6) as response:
                        payload = response.read(demo.RESPONSE_CAP + 1)
                    if len(payload) > demo.RESPONSE_CAP:
                        self.send_error(502)
                        return
                    reply = json.loads(payload)
                    result = reply.get("result")
                    if result and method in ("headers", "blocks") and result[method]:
                        with outer.lock:
                            outer.batches.setdefault(method, copy.deepcopy(result))
                    if active and result:
                        if sync_mutation:
                            if mode == f"stale_{method}":
                                with outer.lock:
                                    saved = copy.deepcopy(outer.batches[method])
                                    sampled_hash = outer.stale_hash
                                # The first cached batch starts at height one;
                                # replay its first genuine signed entry and its
                                # genuine historical sampled anchor repeatedly.
                                result[method] = saved[method][:1]
                                result["sampledHeight"] = "1"
                                result["sampledHash"] = sampled_hash
                            elif len(result[method]) >= 2:
                                at_genesis = rpc_request.get("afterHeight", "0") == "0" if method == "headers" else rpc_request.get("fromHeight") == "1"
                                # First reject a missing first height. The
                                # prefix variant also tests atomic per-block
                                # acceptance: retain height one before a gap,
                                # then keep omitting the next missing height.
                                index = 1 if mode == f"prefix_gap_{method}" and at_genesis and len(result[method]) >= 3 else 0
                                result[method].pop(index)
                        elif mode == "bad_header" and result["headers"]:
                            data = bytearray.fromhex(result["headers"][0][2:])
                            data[-1] ^= 1
                            result["headers"][0] = "0x" + data.hex()
                        elif mode in ("bad_proof", "missing_id", "truncate"):
                            data = bytearray.fromhex(result["proof"][2:])
                            if mode == "bad_proof":
                                data[80] ^= 1  # response snapshot state root
                            elif mode == "truncate":
                                del data[-1]
                            else:
                                offset = 145 + 348  # envelope + StateHeader
                                for _ in range(2):  # catalog path, term path
                                    count = struct.unpack_from(">I", data, offset)[0]
                                    offset += 4
                                    for _ in range(count):
                                        size = struct.unpack_from(">I", data, offset)[0]
                                        offset += 4 + size
                                count = struct.unpack_from(">I", data, offset)[0]
                                check(count > 0, "mutation fixture needs a posting")
                                struct.pack_into(">I", data, offset, count - 1)
                                del data[offset + 4:offset + 12]
                            result["proof"] = "0x" + data.hex()
                    payload = json.dumps(reply, separators=(",", ":")).encode()
                    self.send_response(200)
                    self.send_header("Content-Type", "application/json")
                    self.send_header("Content-Length", str(len(payload)))
                    self.end_headers()
                    self.wfile.write(payload)
                except (OSError, ValueError, KeyError, AssertionError):
                    self.send_error(502)

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.url = f"http://127.0.0.1:{self.server.server_port}"
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def set(self, mode):
        with self.lock:
            self.mode, self.hits = mode, 0

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=2)


def chunked_request(url, size):
    body = json.dumps({"rpcVersion": 1, "request": {"method": "status"}}).encode()
    body += b" " * (size - len(body))
    connection = http.client.HTTPConnection(url.removeprefix("http://"), timeout=6)
    try:
        connection.request("POST", "/rpc", (body[i:i+16384] for i in range(0, len(body), 16384)),
                           headers={"Content-Type": "application/json"}, encode_chunked=True)
        return json.loads(connection.getresponse().read(65536))
    finally:
        connection.close()


ENTITIES, ALPHA, BETA = 1, 2, 3


def oracle_history(blocks, seed=7):
    """Plain logical row model, no codec, trie, engine or response-derived data.

    Transaction rollback releases its tentative id. Expiry runs first.
    Namespace 1 is the reserved entity namespace and stays empty; generic rows
    live in alpha (2) and beta (3). Beta transfers affect authorization but
    leave its rows intact.
    """
    rows = {ENTITIES: {}, ALPHA: {}, BETA: {}}
    next_id = {ENTITIES: 1, ALPHA: 1, BETA: 1}
    history = {0: None}
    permanent = str(2**64 - 1)

    def create(namespace, key, value_type="u64", value="1", expiry=permanent):
        record = {
            "id": str(next_id[namespace]), "key": "0x" + key.encode().hex(),
            "cells": [
                {"name": "$expiresAt", "kind": "attribute", "type": "u64", "value": str(expiry)},
                {"name": "group", "kind": "attribute", "type": value_type, "value": value},
                {"name": "note", "kind": "field", "type": "str", "value": "retained in memory"},
            ],
        }
        if expiry == permanent:
            record["cells"].pop(0)
        next_id[namespace] += 1
        rows[namespace][key] = record

    for height in range(1, blocks + 1):
        for namespace in rows:
            rows[namespace] = {k: row for k, row in rows[namespace].items() if int(next((c["value"] for c in row["cells"] if c["name"] == "$expiresAt"), permanent)) > height}
        round_, slot = divmod(height, 20)
        key = lambda kind: f"{seed}-{round_}-{kind}"
        if height != 1:
            if slot == 2:
                create(ALPHA, key("expires"), expiry=height + 12)
            elif slot == 3:
                create(ALPHA, key("recreated"))
            elif slot == 4:
                create(BETA, key("other"))
            elif slot in (5, 6):
                next(c for c in rows[ALPHA][key("expires")]["cells"] if c["name"] == "group")["value"] = "2" if slot == 5 else "1"
            elif slot == 7:
                pass  # create then missing delete rolls back the entire transaction
            elif slot == 8:
                del rows[ALPHA][key("recreated")]
            elif slot == 9:
                create(ALPHA, key("recreated"))
            elif slot in (10, 11):
                next(c for c in rows[ALPHA][key("recreated")]["cells"] if c["name"] == "group")["type"] = "str" if slot == 10 else "u64"
            elif 15 <= slot <= 18:
                create(ALPHA, key(f"stable-{slot}"))
        history[height] = copy.deepcopy(rows)
    return history


def oracle_eq(history, height, namespace=ALPHA, value_type="u64", value="1"):
    state = history[height]
    if state is None or namespace not in state:
        return None
    return sorted([row for row in state[namespace].values()
                   if any(c["name"] == "group" and c["kind"] == "attribute" and c["type"] == value_type and c["value"] == value for c in row["cells"])],
                  key=lambda row: int(row["id"]))


def produce_to(cluster, target):
    while int(cluster.call("producer", "status")["height"]) < target:
        cluster.call("producer", "produceBlock")


def query_walk(cluster, height, size, expected, *, light="light", namespace=ALPHA, value_type="u64", value="1", advance=False):
    request = demo.resolve(cluster, light, height, namespace, value_type, value, size)["request"]
    rows = []
    page_number = 0
    while True:
        # Switch proof endpoints on every page; block identity stays pinned.
        full = "full" if page_number % 2 == 0 else "full2"
        page = demo.compare_page(cluster, request, light, full,
                                 cluster.url("producer") if page_number % 3 == 1 else cluster.url(full))
        check(page["height"] == str(height), "pagination silently changed block")
        check(page["outcome"] == ("NamespaceNotFound" if expected is None else "Page"), "namespace absence conflated with empty term")
        rows.extend(page["rows"])
        if advance and page_number == 0:
            cluster.call("producer", "produceBlock")
        request = page["nextRequest"]
        if request is None:
            break
        request = demo.page_size(request, size)
        page_number += 1
        check(page_number < 4096, "unbounded page walk")
    check(rows == (expected or []), f"oracle mismatch at B={height}, size={size}, light={light}: {rows} != {expected}")


def sync_batch_recovery(cluster, role):
    """Authentic gaps and old prefixes are availability faults, not forks."""
    method = "headers" if role == "light" else "blocks"
    target = int(cluster.call("producer", "status")["height"])
    proxy = Proxy(cluster.url("producer"))
    name = f"{role}_recovery"
    try:
        # Derive the genuine old anchor from the producer's exact B=1
        # snapshot identity. This is test fixture data, not follower trust.
        snapshot = bytes.fromhex(demo.resolve(cluster, "producer", 1)["snapshot"][2:])
        proxy.stale_hash = "0x" + snapshot[44:76].hex()
        proxy.set(f"gap_{method}")
        cluster.start(name, role, proxy.url)
        demo.wait_until(lambda: proxy.hits >= 2, description=f"{role} missing first height refetch")
        status = cluster.call(name, "status")
        check(status["height"] == "0", f"{role} skipped missing first height")
        check(status["health"] not in ("InvalidHeader", "ChainConflict"), f"{role} permanently halted on authentic gap")

        proxy.set(f"prefix_gap_{method}")
        demo.wait_until(lambda: proxy.hits >= 2 and cluster.call(name, "status")["height"] == "1",
                        description=f"{role} accepted valid prefix before gap")
        status = cluster.call(name, "status")
        check(status["hash"] == proxy.stale_hash, f"{role} lost or skipped accepted prefix")
        check(status["health"] not in ("InvalidHeader", "ChainConflict"), f"{role} permanently halted after valid prefix")
        proxy.set("pass")
        before = cluster.wait_height(name, target)

        proxy.set(f"stale_{method}")
        demo.wait_until(lambda: proxy.hits >= 2 and cluster.call(name, "status")["health"] == "stalled",
                        description=f"{role} repeated historical batch remains stalled")
        after = cluster.call(name, "status")
        for field in ("height", "hash", "root"):
            check(after[field] == before[field], f"{role} replayed old batch changed {field}")
        check(after["observedPeerHeight"] == before["observedPeerHeight"], f"{role} old advisory prefix rewound observed target")
        proxy.set("pass")
        cluster.call("producer", "produceBlock")
        cluster.wait_height(name, target + 1)
        check(cluster.call(name, "status")["hash"] == cluster.call("producer", "status")["hash"],
              f"{role} failed to resume after old duplicate batch")
    finally:
        if name in cluster.children:
            cluster.stop(name)
        proxy.close()


def test_run(keep=False):
    history = oracle_history(210)
    with demo.Cluster(interval_ms=0, capacity=210, keep=keep) as cluster:
        producer = cluster.start("producer")
        cluster.start("full")
        cluster.start("light", peer=producer)
        proxy = Proxy(producer)
        try:
            check(chunked_request(producer, demo.REQUEST_CAP)["error"] is None, "exact-cap chunked request rejected")
            check(chunked_request(producer, demo.REQUEST_CAP + 1)["error"] == "ProofLimitExceeded", "oversized chunked request accepted")
            check(cluster.call("producer", "status")["height"] == "0", "oversized request changed state")
            produce_to(cluster, 40)
            for name in ("full", "light"):
                cluster.wait_height(name, 40)
            # Two independently initialized late followers.
            cluster.start("full2", "full", proxy.url)
            cluster.start("light2", "light", cluster.url("full"))
            produce_to(cluster, 60)
            for name in ("full", "full2", "light", "light2"):
                cluster.wait_height(name, 60)
            for name in ("full2", "light2"):
                cluster.call(name, "pause", paused=True)
            produce_to(cluster, 85)
            for name in ("full2", "light2"):
                check(cluster.call(name, "status")["height"] == "60", f"paused {name} advanced")
                cluster.call(name, "pause", paused=False)
            produce_to(cluster, 110)
            for name in ("full", "full2", "light", "light2"):
                cluster.wait_height(name, 110)
            # A missing response/body cannot advance the executed head.
            proxy.set("drop_blocks")
            demo.wait_until(lambda: proxy.hits >= 2, description="dropped block responses")
            produce_to(cluster, 135)
            check(cluster.call("full2", "status")["height"] == "110", "unavailable bodies advanced full follower")
            proxy.set("pass")
            produce_to(cluster, 170)
            cluster.wait_height("full2", 170)
            cluster.restart("full2")  # volatile replay starts again from local genesis
            produce_to(cluster, 200)
            for name in ("full", "full2", "light", "light2"):
                cluster.wait_height(name, 200)
            statuses = [cluster.call(name, "status") for name in ("producer", "full", "full2", "light", "light2")]
            for key in ("genesisHash", "height", "hash", "root"):
                check(len({s[key] for s in statuses}) == 1, f"heads disagree on {key}")
            print("PASS: 200 blocks; two full and two light processes; late join, 25-block pause, dropped bodies, restart replay", flush=True)

            for role in ("full", "light"):
                sync_batch_recovery(cluster, role)
            print("PASS: authentic missing-height batches refetch without skipping; valid prefix retained; old duplicate batches idempotent and stalled; both roles recover", flush=True)

            samples = [0, 1, 2, 3, 5, 6, 7, 8, 9, 10, 11, 13, 14, 18, 40, 85, 110, 170, 200]
            for height in samples:
                for light in ("light", "light2"):
                    for size in (1, 3, 64):
                        query_walk(cluster, height, size, oracle_eq(history, height), light=light)
            for height in (1, 4, 10, 14, 200):
                for namespace, tag, value in ((BETA, "u64", "1"), (ALPHA, "str", "1"), (ALPHA, "u64", "999"), (ENTITIES, "u64", "1"), (999, "u64", "1")):
                    query_walk(cluster, height, 3, oracle_eq(history, height, namespace, tag, value), namespace=namespace, value_type=tag, value=value)
            query_walk(cluster, 200, 3, oracle_eq(history, 200), advance=True)
            print("PASS: independent Python row oracle, historical deletion/expiry/type/rollback/recreation, page sizes 1/3/64, endpoint switches under new writes", flush=True)

            req = demo.resolve(cluster, "light", 200, limit=3)["request"]
            for mode in ("bad_proof", "missing_id", "truncate"):
                proxy.set(mode)
                expect_error("InvalidProof", lambda: cluster.call("light", "queryVerified", request=req, peer=proxy.url))
                check(proxy.hits >= 1, "proof mutation did not run")
            proxy.set("oversize_response")
            expect_error("ProofLimitExceeded", lambda: cluster.call("light", "queryVerified", request=req, peer=proxy.url))
            proxy.set("pass")
            page = demo.compare_page(cluster, req)
            cursor_request = page["nextRequest"]
            check(cursor_request is not None, "cursor test needs a nonterminal page")
            # Another light process lacks the first process's verified continuation.
            cluster.restart("light2")
            cluster.wait_height("light2", 200)
            expect_error("CursorMismatch", lambda: cluster.call("light2", "queryVerified", request=cursor_request, peer=producer))
            # Changed stable snapshot identity is rejected before proof display.
            raw = bytearray.fromhex(req[2:]); raw[76] ^= 1
            expect_error("InvalidRequest", lambda: cluster.call("light", "queryVerified", request="0x" + raw.hex(), peer=producer))
            print("PASS: fixed-root corrupted/truncated/incomplete-posting proofs, invented continuation and changed snapshot rejected; actual chunked request/response byte caps", flush=True)

            # Bad signatures stop a new header-only follower before its first height.
            proxy.set("bad_header")
            cluster.start("light_bad", "light", proxy.url)
            demo.wait_until(lambda: cluster.call("light_bad", "status")["health"] == "InvalidHeader", description="bad signature rejection")
            check(cluster.call("light_bad", "status")["height"] == "0", "bad header advanced light")
            cluster.stop("light_bad")
            proxy.set("pass")
            print("PASS: proxy-mutated signed header rejected by a separate light process", flush=True)

            produce_to(cluster, 209)
            expect_error("CapacityReached", lambda: cluster.call("producer", "produceBlock"))
            check(cluster.call("producer", "status")["height"] == "209", "capacity failure changed head")
            query_walk(cluster, 3, 1, oracle_eq(history, 3))
            old_identity = cluster.call("producer", "status")["genesisHash"]
            cluster.stop("producer")
            refused = subprocess.run([str(demo.BINARY), "producer", "--run-dir", str(cluster.run_dir), "--interval-ms", "0"],
                                     stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=10)
            check(refused.returncode != 0, "producer reused an existing signing identity")
            with demo.Cluster(interval_ms=0, capacity=2, keep=keep) as fresh:
                fresh.start("producer")
                check(fresh.call("producer", "status")["genesisHash"] != old_identity, "producer restart reused genesis identity")
            print("PASS: capacity stops without eviction; old proof remains readable; producer restart requires a new run identity", flush=True)
            print("All real-process full/light acceptance checks passed.", flush=True)
        finally:
            proxy.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--no-build", action="store_true")
    parser.add_argument("--keep", action="store_true", help="keep process logs and local run files for inspection")
    args = parser.parse_args()
    if not args.no_build:
        demo.build()
    test_run(args.keep)


if __name__ == "__main__":
    main()
