#!/usr/bin/env python3
"""Drop-in upgrade check between two arkiv-proposer builds; local and isolated.

OLD_BIN (required) and NEW_BIN (default target/debug/arkiv-proposer) name the
two binaries. The script never touches a retained deployment:

1. OLD produces a chain with a state window past the record floor, with a NEW
   full follower replicating it (NEW accepts OLD's blocks).
2. NEW opens OLD's run directory in place, with the same flags and no
   migration step, and continues the chain; OLD's blocks, headers and answers
   are unchanged.
3. An OLD full follower replicates NEW's chain from genesis and an OLD light
   client follows its headers (OLD accepts NEW's blocks); the NEW follower from
   step 1 restarts and continues.
4. OLD reopens the run directory NEW wrote and continues the chain (rollback).
"""
import json
import os
from pathlib import Path
import secrets
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
OLD = Path(os.environ["OLD_BIN"])
NEW = Path(os.environ.get("NEW_BIN", ROOT / "target/debug/arkiv-proposer"))
PERIOD_MS = 100
# Records expire once a height is 256 blocks old (the request identity window).
PAST_FLOOR = 300


def port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def rest(base, path, body=None, token=None):
    headers = {"Content-Type": "application/json"}
    if token:
        headers["Authorization"] = "Bearer " + token
    req = urllib.request.Request(base + "/signed/v1/" + path,
                                 data=None if body is None else json.dumps(body).encode(), headers=headers)
    with urllib.request.urlopen(req, timeout=10) as response:
        return json.load(response)


def rpc(base, method, params):
    body = {"jsonrpc": "2.0", "id": 1, "method": method, "params": params}
    req = urllib.request.Request(base + "/", data=json.dumps(body).encode(),
                                 headers={"Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=30) as response:
            return json.load(response)
    except urllib.error.HTTPError as error:
        return json.load(error)


def wait(what, callback, limit=120):
    deadline = time.monotonic() + limit
    while time.monotonic() < deadline:
        try:
            value = callback()
            if value:
                return value
        except (OSError, urllib.error.URLError, KeyError, ValueError):
            pass
        time.sleep(0.1)
    raise AssertionError("timed out waiting for " + what)


def height(base):
    return int(rest(base, "status")["head"]["height"])


def main():
    processes = {}
    with tempfile.TemporaryDirectory(prefix="arkiv-upgrade-smoke-", dir=os.environ.get("TMPDIR")) as temporary:
        directory = Path(temporary)
        token = secrets.token_hex(32)
        environment = dict(os.environ, ARKIV_PROPOSER_TOKEN=token)
        url = {name: f"http://127.0.0.1:{port()}" for name in ("producer", "private", "new-full", "old-full", "old-light")}

        def cli(binary, *args):
            return subprocess.check_output([str(binary), *map(str, args)], env=environment, text=True).strip()

        def start(name, binary, arguments, ready=True):
            log = (directory / (name + ".log")).open("a")
            log.write(f"=== {binary} {' '.join(arguments)}\n")
            log.flush()
            processes[name] = subprocess.Popen([str(binary), *arguments], env=environment, stdout=log, stderr=log)
            log.close()
            if ready:
                wait(name + " status", lambda: rest(url[name], "status"))

        def stop(name):
            process = processes.pop(name)
            process.terminate()
            assert process.wait(timeout=30) == 0, name + " did not stop cleanly"

        def transaction(binary, key, nonce, request):
            (directory / "payload").write_bytes(b"upgrade-smoke-payload-%d" % nonce)
            out = directory / f"{key}-{nonce}.tx"
            cli(binary, "create", "--key-file", directory / (key + ".key"), "--genesis-file",
                directory / "producer/genesis.bin", "--nonce", nonce, "--request-id", "0x" + request * 32,
                "--data-file", directory / "payload", "--out", out, "--salt", nonce)
            body = {"transactionBytes": "0x" + out.read_bytes().hex()}
            rest(url["producer"], "transactions", body)
            tx = wait("inclusion", lambda: (s := rest(url["producer"], "transactions", body))["status"] == "included" and s)
            return int(tx["height"])

        try:
            addresses = {name: cli(NEW, "keygen", "--out", directory / (name + ".key")) for name in ("producer", "alice")}
            (directory / "allocations.json").write_text(json.dumps({a: str(1000 * 10**18) for a in addresses.values()}))
            producer = ["producer", "--run-dir", directory / "producer", "--key-file", directory / "producer.key",
                        "--listen", url["producer"][7:], "--private-listen", url["private"][7:],
                        "--allocations-file", directory / "allocations.json", "--block-period-ms", PERIOD_MS,
                        "--start", "running", "--state-retention", "8"]
            producer = list(map(str, producer))

            # 1. OLD produces; a NEW follower replicates it.
            start("producer", OLD, producer)
            status = rest(url["producer"], "status")
            identity = status["identity"]
            pins = ["--peer", url["producer"], "--genesis-hash", identity["genesisHash"], "--proposer", status["proposer"],
                    "--chain-id", identity["chainId"], "--source-id", identity["sourceId"], "--run-id", identity["runId"]]
            new_full = ["full", "--run-dir", str(directory / "new-full"), "--listen", url["new-full"][7:],
                        "--replication-peer", url["private"], "--state-retention", "8", *pins]
            start("new-full", NEW, new_full)
            early = [transaction(OLD, "alice", nonce, "%02x" % (0x10 + nonce)) for nonce in range(3)]
            wait("the record floor to pass the early blocks", lambda: height(url["producer"]) > early[-1] + PAST_FLOOR, 180)
            old_status = rest(url["producer"], "status")
            assert int(old_status["recordFloor"]) > early[-1], old_status
            old_head = int(old_status["head"]["height"])
            wait("the NEW follower", lambda: height(url["new-full"]) >= old_head)
            old_headers = rest(url["producer"], "headers?limit=64")["certificates"]
            old_body = rest(url["private"], f"replication/blocks/{early[0]}", token=token)
            old_refusal = rpc(url["producer"], "eth_getTransactionByBlockNumberAndIndex", [hex(early[0]), "0x0"])
            assert old_refusal["error"]["code"] == -32006, old_refusal
            stop("producer")
            stopped_head = height(url["new-full"])

            # 2. NEW opens OLD's run directory in place and continues.
            start("producer", NEW, producer)
            reopened = rest(url["producer"], "status")
            assert reopened["identity"] == identity and reopened["proposer"] == status["proposer"], reopened
            assert int(reopened["head"]["height"]) >= old_head
            assert rest(url["producer"], "headers?limit=64")["certificates"] == old_headers
            assert rest(url["private"], f"replication/blocks/{early[0]}", token=token) == old_body
            late = transaction(NEW, "alice", 3, "20")
            # Below the record floor, transactions are served from the kept bodies.
            for index, block in enumerate(early):
                served = rpc(url["producer"], "eth_getTransactionByBlockNumberAndIndex", [hex(block), "0x0"])
                assert served["result"]["blockNumber"] == hex(block), served
                count = rpc(url["producer"], "eth_getBlockTransactionCountByNumber", [hex(block)])
                assert count["result"] == "0x1", count
            assert rpc(url["producer"], "eth_getBlockByNumber", [hex(early[0]), False])["error"]["code"] == -32006
            wait("the NEW follower to resume", lambda: height(url["new-full"]) >= late)
            assert height(url["new-full"]) > stopped_head

            # 3. OLD followers accept NEW's chain from genesis; NEW's restarts.
            old_full = ["full", "--run-dir", str(directory / "old-full"), "--listen", url["old-full"][7:],
                        "--replication-peer", url["private"], "--state-retention", "8", *pins]
            old_light = ["light", "--run-dir", str(directory / "old-light"), "--listen", url["old-light"][7:], *pins]
            start("old-full", OLD, old_full)
            start("old-light", OLD, old_light)
            stop("new-full")
            start("new-full", NEW, new_full)
            # Paused, the head and its state window hold still for the comparison.
            rest(url["private"], "control", {"action": "pause"}, token)
            target = height(url["producer"])
            for name in ("old-full", "old-light", "new-full"):
                wait(name + " to catch up", lambda name=name: height(url[name]) >= target, 300)
            pinned = hex(target)
            for name in ("old-full", "new-full"):
                head = rpc(url[name], "eth_getBlockByNumber", [pinned, False])["result"]
                assert head == rpc(url["producer"], "eth_getBlockByNumber", [pinned, False])["result"], name
            # The same query answers on every full node at one block.
            query = ["group = u64(1)", {"atBlock": pinned, "select": {"key": True, "count": True}}]
            answers = {name: rpc(url[name], "arkiv_query", query)["result"] for name in ("producer", "old-full", "new-full")}
            assert answers["producer"]["count"] == "0x4", answers
            for name in ("producer", "old-full", "new-full"):
                answers[name].pop("cursor", None)
            assert answers["old-full"] == answers["producer"] == answers["new-full"], answers
            sorted_page = rpc(url["producer"], "arkiv_query", ["group = u64(1) ORDER BY $createdAt DESC",
                                                                {"atBlock": pinned, "limit": 2, "offset": 1}])
            assert len(sorted_page["result"]["data"]) == 2, sorted_page
            past = rpc(url["producer"], "arkiv_query", ["group = u64(1)", {"atBlock": pinned, "offset": 10**12}])
            assert past["result"]["data"] == [] and "cursor" not in past["result"], past
            rest(url["private"], "control", {"action": "resume"}, token)
            for name in ("old-full", "old-light", "new-full"):
                stop(name)

            # 4. OLD reopens the directory NEW wrote and continues.
            stop("producer")
            start("producer", OLD, producer)
            last = height(url["producer"])
            wait("OLD to continue NEW's chain", lambda: height(url["producer"]) > last + 5)
            transaction(OLD, "alice", 4, "30")
            stop("producer")
            versions = [cli(binary, "--version").split()[-1] for binary in (OLD, NEW)]
            print(f"PASS: {versions[0]} -> {versions[1]} in place, followers both ways, rollback "
                  f"(record floor {old_status['recordFloor']}, heads {old_head} -> {target})")
        except Exception:
            for log in sorted(directory.glob("*.log")):
                print(f"--- {log.name}\n" + log.read_text()[-4000:])
            raise
        finally:
            for process in processes.values():
                process.kill()


if __name__ == "__main__":
    main()
