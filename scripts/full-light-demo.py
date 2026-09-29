#!/usr/bin/env python3
"""Launch the volatile full/light demo using only the Python standard library."""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import select
import shlex
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
BINARY = ROOT / "target" / "debug" / "arkiv-demo"
RESPONSE_CAP = 8 * 1024 * 1024


class RpcError(RuntimeError):
    def __init__(self, code):
        super().__init__(str(code))
        self.code = code


def rpc(url, method, **fields):
    # An explicit admission rejection did no work and is safe to retry,
    # including manual production. Never retry a lost mutation response.
    deadline = time.monotonic() + 5
    while True:
        try:
            return _rpc_once(url, method, **fields)
        except RpcError as error:
            if error.code != "ServerBusy" or time.monotonic() >= deadline:
                raise
            time.sleep(0.025)


def _rpc_once(url, method, **fields):
    body = json.dumps({"rpcVersion": 1, "request": {"method": method, **fields}}, separators=(",", ":")).encode()
    request = urllib.request.Request(url.rstrip("/") + "/rpc", body, {"Content-Type": "application/json"})
    with urllib.request.urlopen(request, timeout=6) as response:
        raw = response.read(RESPONSE_CAP + 1)
    if len(raw) > RESPONSE_CAP:
        raise RpcError("ProofLimitExceeded")
    reply = json.loads(raw)
    if set(reply) != {"rpcVersion", "result", "error"} or reply["rpcVersion"] != 1:
        raise RpcError("InvalidResponse")
    if reply["error"] is not None:
        raise RpcError(reply["error"])
    return reply["result"]


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def build():
    subprocess.run(["cargo", "build", "-p", "arkiv-node", "--bin", "arkiv-demo"], cwd=ROOT, check=True)


def page_size(request, limit):
    """Change only the uncommitted page size in a canonical EqRequest.

    The query/snapshot and verifier-issued cursor remain byte-identical.
    This helper is deliberately not a cursor constructor.
    """
    if not 1 <= limit <= 64:
        raise ValueError("page size must be 1..64")
    data = bytearray.fromhex(request.removeprefix("0x"))
    offset = 108 + 4 + 8  # SnapshotRef, query version and namespace
    length = struct.unpack_from(">I", data, offset)[0]
    offset += 4 + length
    tag = data[offset]
    offset += 1
    if tag == 1:
        offset += 1
    elif tag in (2, 3):
        offset += 8
    elif tag == 4:
        length = struct.unpack_from(">I", data, offset)[0]
        offset += 4 + length
    else:
        raise ValueError("unsupported Eq value")
    offset += 2  # ordering and full-row projection
    struct.pack_into(">I", data, offset, limit)
    return "0x" + data.hex()


def wait_until(predicate, timeout=20, description="condition"):
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        try:
            value = predicate()
            if value:
                return value
        except (OSError, RpcError, ValueError) as error:
            last = error
        time.sleep(0.05)
    raise TimeoutError(f"Timed out waiting for {description}; last error: {last}")


def rss_mib(pid):
    try:
        for line in Path(f"/proc/{pid}/status").read_text().splitlines():
            if line.startswith("VmRSS:"):
                return int(line.split()[1]) / 1024
    except OSError:
        pass
    return 0.0


class Cluster:
    def __init__(self, *, seed=7, capacity=10000, interval_ms=1000, base_port=None, parent=None, keep=False):
        self.directory = Path(tempfile.mkdtemp(prefix="arkiv-demo-", dir=parent))
        os.chmod(self.directory, 0o700)
        self.run_dir = self.directory / "run"  # producer exclusively creates this
        self.seed, self.capacity, self.interval_ms = seed, capacity, interval_ms
        self.base_port, self.keep = base_port, keep
        self.children = {}
        self.serial = 0

    def start(self, name, role=None, peer=None, capacity=None):
        role = role or name.rstrip("0123456789")
        if name in self.children:
            raise ValueError(f"{name} already exists")
        port = self.base_port + self.serial if self.base_port is not None else free_port()
        self.serial += 1
        url = f"http://127.0.0.1:{port}"
        args = [str(BINARY), role, "--listen", f"127.0.0.1:{port}", "--capacity", str(capacity or self.capacity)]
        if role == "producer":
            args += ["--run-dir", str(self.run_dir), "--interval-ms", str(self.interval_ms), "--seed", str(self.seed)]
        else:
            args += ["--trust", str(self.run_dir / "trust.bin"), "--peer", peer or self.url("producer"), "--interval-ms", "50"]
        log_path = self.directory / f"{name}-{self.serial}.log"
        log = log_path.open("w")
        process = subprocess.Popen(args, cwd=ROOT, stdout=log, stderr=subprocess.STDOUT)
        self.children[name] = {"process": process, "url": url, "role": role, "log": log, "log_path": log_path, "peer": peer}

        def ready():
            if process.poll() is not None:
                raise RuntimeError(f"{name} exited {process.returncode}: {log_path.read_text()}")
            return rpc(url, "status")
        wait_until(ready, description=f"{name} RPC")
        return url

    def url(self, name):
        return self.children[name]["url"]

    def call(self, name, method, **fields):
        return rpc(self.url(name), method, **fields)

    def stop(self, name):
        child = self.children.pop(name)
        child["process"].terminate()
        try:
            child["process"].wait(timeout=5)
        except subprocess.TimeoutExpired:
            child["process"].kill()
            child["process"].wait(timeout=5)
        child["log"].close()
        return child

    def restart(self, name):
        child = self.stop(name)
        return self.start(name, child["role"], child["peer"])

    def wait_height(self, name, height, timeout=30):
        return wait_until(lambda: (s if int(s["height"]) >= height else None) if (s := self.call(name, "status")) else None,
                          timeout, f"{name} height {height}")

    def close(self):
        for name in list(self.children)[::-1]:
            self.stop(name)
        if self.keep:
            print(f"Run files retained at {self.directory}")
        else:
            shutil.rmtree(self.directory)

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()


def resolve(cluster, light="light", height="latest", namespace="1", value_type="u64", value="1", limit=3):
    return cluster.call(light, "resolveQuery", height=str(height), namespace=str(namespace), attribute="group",
                        valueType=value_type, value=str(value), limit=limit)


def compare_page(cluster, request, light="light", full="full", peer=None):
    verified = cluster.call(light, "queryVerified", request=request, peer=peer or cluster.url(full))
    local = cluster.call(full, "queryLocal", request=request)
    if verified["verification"] != "query proof verified":
        raise AssertionError("light response did not report successful verification")
    if local["outcome"] != verified["outcome"] or local.get("rows", []) != verified["rows"]:
        raise AssertionError("independently executed and verified results differ")
    return verified


def render(cluster, note=""):
    lines = ["single-producer development chain | retained in memory | no pruning", f"capacity: {cluster.capacity} retained heights including genesis | seed: {cluster.seed}",
             "ROLE         HEIGHT  OBSERVED  LAG   RETAINED     RSS MiB  ROOT        HASH        HEALTH"]
    for name, child in cluster.children.items():
        try:
            s = cluster.call(name, "status")
            lines.append(f"{name:12} {s['height']:>6} {s['observedPeerHeight']:>9} {s['lag']:>4}   "
                         f"{s['retainedFrom']}..{s['retainedThrough']:<8} {rss_mib(child['process'].pid):7.1f}  {s['root'][2:12]}  {s['hash'][2:12]}  {s['health']}")
        except (OSError, RpcError) as error:
            lines.append(f"{name:12} unavailable: {error}")
    lines += ["Full: execution checked. Light: producer authenticated. Proof results: query proof verified.",
              "Lag is relative to the last observed peer head; a stale peer cannot prove global freshness.",
              "query [height|latest] [namespace] [u64|i64|bool|str] [value] [limit]",
              "next [limit] [producer|full|full2] | pause/resume NAME | late full/light | produce [N] | quit"]
    if note:
        lines += ["", note]
    if sys.stdout.isatty():
        print("\033[2J\033[H", end="")
    print("\n".join(lines), flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--seed", type=int, default=7)
    parser.add_argument("--capacity", type=int, default=10000)
    parser.add_argument("--interval-ms", type=int, default=1000)
    parser.add_argument("--base-port", type=int, default=8781)
    parser.add_argument("--duration", type=float, help="stop after this many seconds (smoke runs)")
    parser.add_argument("--run-parent", type=Path)
    parser.add_argument("--keep", action="store_true", help="keep local logs, trust and private key on exit")
    parser.add_argument("--no-build", action="store_true")
    args = parser.parse_args()
    if not args.no_build:
        build()
    with Cluster(seed=args.seed, capacity=args.capacity, interval_ms=args.interval_ms, base_port=args.base_port,
                 parent=args.run_parent, keep=args.keep) as cluster:
        cluster.start("producer")
        cluster.start("full")
        cluster.start("light", peer=cluster.url("full"))
        started = time.monotonic()
        next_request = None
        note = "Historical query example: query 3; compare before deletion at 8 and expiry at 14."
        last_render = 0
        input_open = True
        while args.duration is None or time.monotonic() - started < args.duration:
            if time.monotonic() - last_render >= 1:
                render(cluster, note)
                last_render = time.monotonic()
            readable, _, _ = select.select([sys.stdin] if input_open else [], [], [], 0.2)
            if not readable:
                continue
            line = sys.stdin.readline()
            if not line:
                input_open = False
                if args.duration is None:
                    break
                continue
            try:
                parts = shlex.split(line)
            except ValueError as error:
                note = f"Invalid command: {error}"
                last_render = 0
                continue
            if not parts:
                continue
            try:
                command, *tail = parts
                if command in ("quit", "exit", "q"):
                    break
                if command in ("pause", "resume"):
                    note = str(cluster.call(tail[0], "pause", paused=command == "pause"))
                elif command == "produce":
                    for _ in range(int(tail[0]) if tail else 1):
                        cluster.call("producer", "produceBlock")
                    note = "Manual block production completed."
                elif command == "late":
                    role = tail[0]
                    if role not in ("full", "light"):
                        raise ValueError("late requires full or light")
                    number = 2
                    while f"{role}{number}" in cluster.children:
                        number += 1
                    name = f"{role}{number}"
                    cluster.start(name, role, cluster.url("full") if role == "light" else cluster.url("producer"))
                    note = f"{name} started from local trust configuration and genesis."
                elif command in ("query", "next"):
                    if command == "query":
                        defaults = ["latest", "1", "u64", "1", "3"]
                        defaults[:len(tail)] = tail
                        selected = resolve(cluster, height=defaults[0], namespace=defaults[1], value_type=defaults[2], value=defaults[3], limit=int(defaults[4]))
                        request = selected["request"]
                        peer = cluster.url("full")
                    else:
                        if next_request is None:
                            raise ValueError("no verified nonterminal page; run query first")
                        request = page_size(next_request, int(tail[0])) if tail else next_request
                        peer = cluster.url(tail[1]) if len(tail) > 1 else cluster.url("full")
                    page = compare_page(cluster, request, peer=peer)
                    next_request = page["nextRequest"]
                    note = (f"B={page['height']} {page['outcome']} | full and verified rows match | "
                            f"posting={page['postingCount']} proof={page['proofBytes']} bytes verify={page['verificationMicros']} µs "
                            f"response={page['responseMicros']} µs | next={'yes' if next_request else 'no'}\n" + json.dumps(page["rows"], ensure_ascii=False))
                elif command != "status":
                    raise ValueError("unknown command")
            except (OSError, RpcError, ValueError, IndexError, AssertionError) as error:
                note = f"Request failed: {error}"
            last_render = 0


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        pass
