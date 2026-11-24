#!/usr/bin/env python3
"""Snapshot sync check with the arkiv-proposer binary; local and isolated.

BIN (default target/debug/arkiv-proposer) names the binary. The script never
touches a retained deployment:

1. A producer expires block bodies with its records (`--body-retention
   window`), and so does a full follower F.
2. F is stopped, `snapshot create` copies its databases and pins it, and F is
   restarted inside the producer's body window.
3. More than a record window (256 blocks) later the producer has dropped the
   bodies after the snapshot; F, pinned, still has them.
4. `snapshot install --peer PRODUCER` is refused. `snapshot install --peer F
   --verify-state` installs node G, which starts without a prefill or a
   token and catches up from F to the same head (height, hash, state root)
   and the same query answer.
5. A node installed from the same snapshot and pointed at the producer logs
   that it must be reinstalled and exits with status 3.
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
BIN = Path(os.environ.get("BIN", ROOT / "target/debug/arkiv-proposer"))
PERIOD_MS = 100
# Records, and with `--body-retention window` bodies, expire once a height is
# 256 blocks old (the request identity window).
WINDOW = 256


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
    started = time.monotonic()
    with tempfile.TemporaryDirectory(prefix="arkiv-snapshot-smoke-", dir=os.environ.get("TMPDIR")) as temporary:
        directory = Path(temporary)
        token = secrets.token_hex(32)
        environment = dict(os.environ, ARKIV_PROPOSER_TOKEN=token)
        url = {name: f"http://127.0.0.1:{port()}" for name in ("producer", "private", "f", "g", "stranded")}

        def cli(*args, check=True):
            done = subprocess.run([str(BIN), *map(str, args)], env=environment, text=True, capture_output=True)
            if check:
                assert done.returncode == 0, (args, done.stderr)
                return done.stdout.strip()
            return done

        def start(name, arguments, ready=True):
            log = (directory / (name + ".log")).open("a")
            log.write(f"=== {BIN} {' '.join(arguments)}\n")
            log.flush()
            # Followers get no token: they sync from the public endpoint.
            env = environment if name == "producer" else {k: v for k, v in environment.items() if k != "ARKIV_PROPOSER_TOKEN"}
            processes[name] = subprocess.Popen([str(BIN), *arguments], env=env, stdout=log, stderr=log)
            log.close()
            if ready:
                wait(name + " status", lambda: rest(url[name], "status"))

        def stop(name):
            process = processes.pop(name)
            process.terminate()
            assert process.wait(timeout=30) == 0, name + " did not stop cleanly"

        def control(action):
            # A busy node (429) is asked again.
            wait(action, lambda: rest(url["private"], "control", {"action": action}, token)["paused"] == (action == "pause"))

        def transaction(nonce):
            (directory / "payload").write_bytes(b"snapshot-smoke-payload-%d" % nonce)
            out = directory / f"alice-{nonce}.tx"
            cli("create", "--key-file", directory / "alice.key", "--genesis-file", directory / "producer/genesis.bin",
                "--nonce", nonce, "--request-id", "0x" + ("%02x" % (0x10 + nonce)) * 32,
                "--data-file", directory / "payload", "--out", out, "--salt", nonce)
            body = {"transactionBytes": "0x" + out.read_bytes().hex()}
            rest(url["producer"], "transactions", body)
            tx = wait("inclusion", lambda: (s := rest(url["producer"], "transactions", body))["status"] == "included" and s)
            return int(tx["height"])

        try:
            addresses = {name: cli("keygen", "--out", directory / (name + ".key")) for name in ("producer", "alice")}
            (directory / "allocations.json").write_text(json.dumps({a: str(1000 * 10**18) for a in addresses.values()}))
            retention = ["--state-retention", "8", "--body-retention", "window"]

            # 1. A producer and a follower that both expire bodies.
            start("producer", list(map(str, [
                "producer", "--run-dir", directory / "producer", "--key-file", directory / "producer.key",
                "--listen", url["producer"][7:], "--private-listen", url["private"][7:],
                "--allocations-file", directory / "allocations.json", "--block-period-ms", PERIOD_MS,
                "--start", "running", *retention])))
            status = rest(url["producer"], "status")
            identity = status["identity"]
            pins = ["--genesis-hash", identity["genesisHash"], "--proposer", status["proposer"],
                    "--chain-id", identity["chainId"], "--source-id", identity["sourceId"], "--run-id", identity["runId"]]

            def full(name, peer):
                return ["full", "--run-dir", str(directory / name), "--listen", url[name][7:],
                        "--peer", url[peer], *retention, *pins]

            start("f", full("f", "producer"))
            before = [transaction(nonce) for nonce in range(3)]
            wait("F to follow", lambda: height(url["f"]) >= before[-1])

            # 2. F is stopped, snapshotted and pinned, and restarted while the
            #    producer still has the blocks it missed.
            stop("f")
            down = time.monotonic()
            created = json.loads(cli("snapshot", "create", "--run-dir", directory / "f", "--out", directory / "snapshot"))
            snapshot = created["snapshot"]
            at = int(snapshot["height"])
            assert at >= before[-1] and created["pin"] == str(at), created
            assert sorted(p.name for p in (directory / "snapshot").iterdir()) == [
                "certificate.bin", "genesis.bin", "history.redb", "node.redb", "snapshot.json"]
            assert (directory / "f/snapshot-pin").read_text() == f"{at}\n"
            start("f", full("f", "producer"))
            downtime = time.monotonic() - down
            assert rest(url["f"], "status")["bodyPin"] == str(at)

            # 3. More than a record window passes.
            after = [transaction(nonce) for nonce in range(3, 6)]
            later = at + WINDOW + 20
            wait("the producer's bodies to pass the snapshot",
                 lambda: (s := rest(url["producer"], "status")) and int(s["head"]["height"]) >= later
                 and int(s["bodyFloor"]) > at + 1, 180)
            wait("F to follow", lambda: height(url["f"]) >= later)
            pinned = rest(url["f"], "status")
            assert pinned["bodyFloor"] == str(at) and int(pinned["recordFloor"]) > at + 1, pinned
            assert pinned["bodyRetention"] == "window", pinned

            # 4. The producer cannot continue the snapshot; F can.
            install = ["snapshot", "install", "--from", directory / "snapshot", *pins]
            refused = cli(*install, "--run-dir", directory / "refused", "--peer", url["producer"], check=False)
            assert refused.returncode != 0 and "cannot continue a snapshot" in refused.stderr, refused.stderr
            assert not (directory / "refused").exists()
            installed = json.loads(cli(*install, "--run-dir", directory / "g", "--peer", url["f"], "--verify-state"))
            assert installed["head"] == {"height": str(at), "hash": snapshot["headerHash"],
                                         "stateRoot": snapshot["stateRoot"]}, installed
            assert installed["peer"]["role"] == "full" and installed["verifiedState"] is True, installed
            assert sorted(p.name for p in (directory / "g").iterdir()) == [
                "certificate.bin", "genesis.bin", "history.redb", "node.redb", "trust.json"]
            behind = height(url["f"]) - at
            assert behind > WINDOW, behind
            catching = time.monotonic()
            start("g", full("g", "f"))
            # Paused, the head holds still for the comparison.
            control("pause")
            target = height(url["producer"])
            for name in ("f", "g"):
                wait(name + " to catch up", lambda name=name: height(url[name]) >= target, 300)
            catch_up = time.monotonic() - catching
            head = rest(url["producer"], "status")["head"]
            for name in ("f", "g"):
                assert rest(url[name], "status")["head"] == head, name
            assert rest(url["g"], "status")["bodyPin"] is None
            query = ["group = u64(1)", {"atBlock": hex(target), "select": {"key": True, "count": True}}]
            answers = {name: rpc(url[name], "arkiv_query", query)["result"] for name in ("producer", "f", "g")}
            assert answers["producer"]["count"] == hex(len(before + after)), answers
            for answer in answers.values():
                answer.pop("cursor", None)
            assert answers["f"] == answers["producer"] == answers["g"], answers
            control("resume")
            # G keeps following.
            wait("G to follow new blocks", lambda: height(url["g"]) > target + 3)

            # 5. Pointed at the producer, a node from the same snapshot stops.
            cli(*install, "--run-dir", directory / "stranded")
            start("stranded", full("stranded", "producer"), ready=False)
            code = processes.pop("stranded").wait(timeout=60)
            log = (directory / "stranded.log").read_text()
            assert code == 3, (code, log[-2000:])
            assert log.count("sync_gap") == 1 and "reinstalled from a newer snapshot" in log, log[-2000:]
            assert f'"head":"{at}"' in log and url["producer"] in log, log[-2000:]

            for name in ("g", "f", "producer"):
                stop(name)
            size = sum(int(f["bytes"]) for f in snapshot["files"])
            print(f"PASS: snapshot at {at} ({size} bytes, create {created['seconds']:.2f} s, F down {downtime:.2f} s), "
                  f"install with --verify-state {installed['seconds']:.2f} s, G caught up {target - at} blocks "
                  f"from the pinned follower in {catch_up:.2f} s (head {target}), stranded node exited 3; "
                  f"{cli('--version').split()[-1]}, total {time.monotonic() - started:.0f} s")
        except Exception:
            for log in sorted(directory.glob("*.log")):
                print(f"--- {log.name}\n" + log.read_text()[-4000:])
            raise
        finally:
            for process in processes.values():
                process.kill()


if __name__ == "__main__":
    main()
