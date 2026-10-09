#!/usr/bin/env python3
"""Local isolated subprocess acceptance; never touches a retained deployment."""
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
BINARY = Path(os.environ.get("ARKIV_PROPOSER_BIN", ROOT / "target/debug/arkiv-proposer"))


def port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def request(base, path, body=None, token=None):
    headers = {"Content-Type": "application/json"}
    if token:
        headers["Authorization"] = "Bearer " + token
    req = urllib.request.Request(base + "/signed/v1/" + path,
                                 data=None if body is None else json.dumps(body).encode(),
                                 headers=headers)
    for attempt in range(20):
        try:
            with urllib.request.urlopen(req, timeout=5) as response:
                return json.load(response)
        except urllib.error.HTTPError as error:
            # A rejected try-lock/slot request performed no operation. Retry
            # bounded backpressure, never an ambiguous transport timeout.
            if error.code != 429 or attempt == 19:
                raise
            detail = json.load(error)
            if detail.get("error") != "ServerBusy":
                raise
            time.sleep(0.025)
    raise AssertionError("unreachable")


def wait(callback):
    deadline = time.monotonic() + 20
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
    processes = {}
    with tempfile.TemporaryDirectory(prefix="arkiv-signed-smoke-") as temporary:
        directory = Path(temporary)
        token = secrets.token_hex(32)
        environment = dict(os.environ, ARKIV_PROPOSER_TOKEN=token)
        endpoints = {name: f"http://127.0.0.1:{port()}" for name in ("producer", "private", "full", "light")}

        def cli(*args):
            return subprocess.check_output([str(BINARY), *map(str, args)], env=environment, text=True).strip()

        def start(name, arguments):
            log = (directory / (name + ".log")).open("a")
            process = subprocess.Popen([str(BINARY), *arguments], env=environment, stdout=log, stderr=log)
            log.close()
            processes[name] = process
            wait(lambda: request(endpoints[name], "status"))

        def stop():
            for process in processes.values():
                process.terminate()
            for process in processes.values():
                process.wait(timeout=10)
            processes.clear()

        try:
            addresses = {name: cli("keygen", "--out", directory / (name + ".key")) for name in ("producer", "alice", "bob")}
            (directory / "allocations.json").write_text(json.dumps({address: str(1000 * 10**18) for address in addresses.values()}))
            producer = ["producer", "--run-dir", str(directory / "producer"), "--key-file", str(directory / "producer.key"),
                        "--listen", endpoints["producer"].removeprefix("http://"), "--private-listen", endpoints["private"].removeprefix("http://"),
                        "--allocations-file", str(directory / "allocations.json"), "--block-period-ms", "100"]
            start("producer", producer)
            status = request(endpoints["producer"], "status")
            assert status["authentication"] == "signed-proposer-v1"
            assert status["capabilities"]["signatures"] is True
            identity = status["identity"]
            pins = ["--peer", endpoints["producer"], "--genesis-hash", identity["genesisHash"], "--proposer", status["proposer"],
                    "--chain-id", identity["chainId"], "--source-id", identity["sourceId"], "--run-id", identity["runId"]]
            full = ["full", "--run-dir", str(directory / "full"), "--listen", endpoints["full"].removeprefix("http://"),
                    "--replication-peer", endpoints["private"], *pins]
            light = ["light", "--run-dir", str(directory / "light"), "--listen", endpoints["light"].removeprefix("http://"), *pins]
            start("full", full)
            start("light", light)
            request(endpoints["private"], "control", {"action": "step"}, token)

            def transaction(name, request_id):
                (directory / "payload").write_bytes(b"signed-history-smoke-payload")
                out = directory / (name + ".tx")
                cli("create", "--key-file", directory / (name + ".key"), "--genesis-file", directory / "producer/genesis.bin",
                    "--nonce", "0", "--request-id", "0x" + request_id * 32, "--payload-file", directory / "payload", "--out", out)
                body = {"transactionBytes": "0x" + out.read_bytes().hex()}
                assert request(endpoints["producer"], "transactions", body)["status"] == "pending"
                request(endpoints["private"], "control", {"action": "step"}, token)
                return body

            alice = transaction("alice", "11")
            for name in ("full", "light"):
                wait(lambda name=name: request(endpoints[name], "status")["head"]["height"] == "2")
                assert request(endpoints[name], "status")["head"] == request(endpoints["producer"], "status")["head"]
            query = {"query": "group = u64(1)", "requireProof": True, "height": "2", "namespace": "1", "limit": 64, "cursor": None}
            proof = request(endpoints["light"], "query/text", query)
            assert proof["verificationStatus"]["status"] == "verified", proof
            assert len(proof["entities"]) == 1
            assert proof["canonicalProof"]
            old_body = request(endpoints["private"], "replication/blocks/2", token=token)
            stop()
            # Pins are retained locally: followers open even while their peer is offline.
            start("full", full)
            start("light", light)
            start("producer", producer)
            assert request(endpoints["producer"], "transactions", alice)["status"] == "included"
            transaction("bob", "22")
            for name in ("full", "light"):
                wait(lambda name=name: request(endpoints[name], "status")["head"]["height"] == "3")
            assert request(endpoints["private"], "replication/blocks/2", token=token) == old_body
            query["height"] = "3"
            assert len(request(endpoints["light"], "query/text", query)["entities"]) == 2
            query["query"] = "group >= u64(1)"
            unsupported = request(endpoints["light"], "query/text", query)
            assert unsupported["error"] == "UnsupportedProof"
            assert unsupported["verificationStatus"]["status"] == "not_executed"
            assert (directory / "producer/history.redb").exists()
            assert (directory / "full/history.redb").exists()
            assert not (directory / "light/history.redb").exists()
            print("PASS: signed CLI submissions, producer/full/light replay, authenticated query proof, offline reopen, duplicate inclusion, old body retention")
        except Exception:
            for name in ("producer", "full", "light"):
                log = directory / (name + ".log")
                if log.exists():
                    print(name + ": " + log.read_text()[-6000:])
            raise
        finally:
            stop()


if __name__ == "__main__":
    main()
