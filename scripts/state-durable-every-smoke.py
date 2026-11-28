#!/usr/bin/env python3
"""Kills a producer and its follower that share durable state commits.

Both nodes share durable state commits (the default, every 64 blocks, or
STATE_DURABLE_EVERY=N for --state-durable-every N), so a kill leaves
node.redb several blocks behind history.redb. Each restart must replay those
blocks from history.redb: the producer comes back at the head it had served,
with the same blocks, and the follower never sees a second block at a height.
The first kill comes a few blocks after genesis, before any shared durable
commit.

Local isolated subprocesses; never touches a retained deployment.
"""
import json
import os
from pathlib import Path
import random
import signal
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
BINARY = Path(os.environ.get("ARKIV_PROPOSER_BIN", ROOT / "target/debug/arkiv-proposer"))
EVERY = os.environ.get("STATE_DURABLE_EVERY")
FLAG = ["--state-durable-every", EVERY] if EVERY else []
ROUNDS = int(os.environ.get("ROUNDS", "8"))


def port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def request(base, path, body=None):
    req = urllib.request.Request(base + "/signed/v1/" + path,
                                 data=None if body is None else json.dumps(body).encode(),
                                 headers={"Content-Type": "application/json"})
    for attempt in range(200):
        try:
            with urllib.request.urlopen(req, timeout=5) as response:
                return json.load(response)
        except urllib.error.HTTPError as error:
            if error.code != 429 or attempt == 199:
                raise
            time.sleep(0.025)
    raise AssertionError("unreachable")


def wait(callback, seconds=30):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        try:
            value = callback()
            if value:
                return value
        except (OSError, urllib.error.URLError):
            pass
        time.sleep(0.05)
    raise AssertionError("local node did not reach expected state")


def main():
    random.seed(int(os.environ.get("SEED", "1")))
    processes = {}
    with tempfile.TemporaryDirectory(prefix="arkiv-durable-every-") as temporary:
        directory = Path(temporary)
        endpoints = {name: f"http://127.0.0.1:{port()}" for name in ("producer", "full")}

        def cli(*args):
            return subprocess.check_output([str(BINARY), *map(str, args)], text=True).strip()

        def start(name, arguments):
            log = (directory / (name + ".log")).open("a")
            processes[name] = subprocess.Popen([str(BINARY), *arguments], stdout=log, stderr=log)
            log.close()
            wait(lambda: request(endpoints[name], "status"))

        def kill(name):
            process = processes.pop(name)
            process.send_signal(signal.SIGKILL)
            process.wait(timeout=10)

        def head(name):
            status = request(endpoints[name], "status")
            return int(status["head"]["height"]), status["head"]["hash"], status["health"]

        def replayed(name):
            blocks = 0
            for line in (directory / (name + ".log")).read_text().splitlines():
                if '"event":"history_replay"' in line:
                    blocks += json.loads(line)["blocks"]
            return blocks

        try:
            addresses = {name: cli("keygen", "--out", directory / (name + ".key")) for name in ("producer", "alice")}
            (directory / "allocations.json").write_text(json.dumps({a: str(1000 * 10**18) for a in addresses.values()}))
            producer = ["producer", "--run-dir", str(directory / "producer"), "--key-file", str(directory / "producer.key"),
                        "--listen", endpoints["producer"].removeprefix("http://"),
                        "--allocations-file", str(directory / "allocations.json"), "--block-period-ms", "50",
                        "--start", "running", *FLAG]
            start("producer", producer)
            status = request(endpoints["producer"], "status")
            identity = status["identity"]
            full = ["full", "--run-dir", str(directory / "full"), "--listen", endpoints["full"].removeprefix("http://"),
                    "--peer", endpoints["producer"], "--genesis-hash", identity["genesisHash"], "--proposer", status["proposer"],
                    "--chain-id", identity["chainId"], "--source-id", identity["sourceId"], "--run-id", identity["runId"],
                    *FLAG]
            start("full", full)
            for name in ("producer", "full"):
                log = (directory / (name + ".log")).read_text()
                assert '"durableEvery":%s' % (EVERY or 64) in log, log[:400]
            # A few blocks after genesis: nothing but the genesis is durable in node.redb.
            wait(lambda: head("full")[0] >= 3)
            served = head("producer")
            seen = request(endpoints["producer"], f"feed/blocks/{served[0]}")
            kill("producer")
            kill("full")
            start("producer", producer)
            assert head("producer")[0] >= served[0], (head("producer"), served)
            assert request(endpoints["producer"], f"feed/blocks/{served[0]}") == seen
            start("full", full)
            wait(lambda: head("full")[0] >= served[0])
            assert request(endpoints["full"], f"feed/blocks/{served[0]}") == seen
            early = served[0]
            (directory / "payload").write_bytes(b"durable-every-smoke-payload")
            nonce = 0
            checked = 0
            for round_number in range(ROUNDS):
                # Traffic, then let the follower see everything the producer served.
                for _ in range(3):
                    out = directory / f"alice-{nonce}.tx"
                    cli("create", "--key-file", directory / "alice.key", "--genesis-file", directory / "producer/genesis.bin",
                        "--nonce", nonce, "--request-id", "0x" + f"{nonce + 1:064x}", "--salt", nonce + 1,
                        "--data-file", directory / "payload", "--out", out)
                    body = {"transactionBytes": "0x" + out.read_bytes().hex()}
                    assert request(endpoints["producer"], "transactions", body)["status"] in ("pending", "included")
                    nonce += 1
                time.sleep(random.uniform(0.2, 1.2))
                served = head("producer")
                wait(lambda: head("full")[0] >= served[0])
                seen = request(endpoints["full"], f"feed/blocks/{served[0]}")
                victim = ("producer", "full", "both")[round_number % 3]
                for name in ("producer", "full"):
                    if victim in (name, "both"):
                        kill(name)
                if victim in ("producer", "both"):
                    start("producer", producer)
                    # Back at the head it served, or past it, with the same block there.
                    restarted = head("producer")
                    assert restarted[0] >= served[0], (restarted, served)
                    assert restarted[2] == "running", restarted
                    assert request(endpoints["producer"], f"feed/blocks/{served[0]}") == seen
                    checked += 1
                if victim in ("full", "both"):
                    start("full", full)
                    assert head("full")[2] == "following"
                target = head("producer")[0] + 5
                wait(lambda: head("producer")[0] >= target)
                wait(lambda: head("full")[0] >= target)
                assert head("full")[2] == "following", head("full")
                assert request(endpoints["full"], f"feed/blocks/{target}") == request(endpoints["producer"], f"feed/blocks/{target}")
            for name in ("producer", "full"):
                assert replayed(name) > 0, name + " never replayed from history"
            # A clean stop leaves nothing to replay.
            before = {name: replayed(name) for name in ("producer", "full")}
            stopped = head("producer")
            wait(lambda: head("full")[0] >= stopped[0])
            for name in ("full", "producer"):
                process = processes.pop(name)
                process.terminate()
                process.wait(timeout=20)
            start("producer", producer)
            start("full", full)
            assert head("producer")[0] >= stopped[0]
            assert {name: replayed(name) for name in ("producer", "full")} == before
            # Killed once more and started with a durable commit per block:
            # the state files remember that their commits shared durable
            # ones, so the nodes still replay, and then run as ordinary ones.
            time.sleep(0.7)
            served = head("producer")
            wait(lambda: head("full")[0] >= served[0])
            seen = request(endpoints["full"], f"feed/blocks/{served[0]}")
            kill("producer")
            kill("full")
            plain = lambda arguments: arguments[:len(arguments) - len(FLAG)] + ["--state-durable-every", "1"]
            start("producer", plain(producer))
            start("full", plain(full))
            assert head("producer")[0] >= served[0]
            assert request(endpoints["producer"], f"feed/blocks/{served[0]}") == seen
            after = {name: replayed(name) for name in ("producer", "full")}
            target = head("producer")[0] + 5
            wait(lambda: head("full")[0] >= target)
            assert request(endpoints["full"], f"feed/blocks/{target}") == request(endpoints["producer"], f"feed/blocks/{target}")
            # Every commit is durable again: a kill now loses nothing and replays nothing.
            served = head("producer")
            kill("producer")
            start("producer", plain(producer))
            assert head("producer")[0] >= served[0]
            assert replayed("producer") == after["producer"]
            print(f"PASS: a kill at block {early} and {ROUNDS} more with a durable state commit every {EVERY or '64 (default)'}: "
                  f"producer replayed {before['producer']} blocks "
                  f"and follower {before['full']} from history.redb, {checked} producer restarts kept the served blocks, "
                  f"no chain conflict, clean stop replays nothing, a start with --state-durable-every 1 still replays "
                  f"({after['producer'] - before['producer']} and {after['full'] - before['full']} blocks); head {head('producer')[0]}")
        except Exception:
            for name in ("producer", "full"):
                log = directory / (name + ".log")
                if log.exists():
                    lines = [l for l in log.read_text().splitlines() if '"apiVersion"' not in l]
                    print(name + ": " + "\n".join(lines)[-3000:])
            raise
        finally:
            for process in processes.values():
                process.kill()


if __name__ == "__main__":
    main()
