#!/usr/bin/env python3
"""Isolated HTTP range-proof acceptance. Never touches a retained deployment."""
import importlib.util
import json
import os
from pathlib import Path
import secrets
import subprocess
import tempfile
import urllib.error

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("signed_smoke", ROOT / "scripts/signed-proposer-smoke.py")
helpers = importlib.util.module_from_spec(spec)
spec.loader.exec_module(helpers)
request, wait, port = helpers.request, helpers.wait, helpers.port
BINARY = Path(os.environ.get("ARKIV_PROPOSER_BIN", ROOT / "target/debug/arkiv-proposer"))


def main():
    processes = {}
    with tempfile.TemporaryDirectory(prefix="arkiv-range-proof-smoke-") as tmp:
        directory = Path(tmp)
        token = secrets.token_hex(32)
        env = dict(os.environ, ARKIV_PROPOSER_TOKEN=token)
        endpoints = {name: f"http://127.0.0.1:{port()}" for name in ("producer", "private", "full", "light")}

        def cli(*args):
            return subprocess.check_output([str(BINARY), *map(str, args)], env=env, text=True).strip()

        def start(name, args):
            with (directory / f"{name}.log").open("ab") as log:
                processes[name] = subprocess.Popen([str(BINARY), *map(str, args)], env=env, stdout=log, stderr=log)
            wait(lambda: request(endpoints[name], "status"))

        def step():
            request(endpoints["private"], "control", {"action": "step"}, token)
            head = request(endpoints["producer"], "status")["head"]
            for name in ("full", "light"):
                wait(lambda name=name: request(endpoints[name], "status")["head"] == head)
            return head

        def rejected(base, path, body):
            try:
                request(base, path, body)
            except urllib.error.HTTPError as error:
                assert 400 <= error.code < 500, error.code
                return
            raise AssertionError("invalid range request accepted")

        try:
            owner = cli("keygen", "--out", directory / "owner.key")
            (directory / "allocations.json").write_text(json.dumps({owner: str(1000 * 10**18)}))
            start("producer", ["producer", "--run-dir", directory / "producer", "--key-file", directory / "owner.key",
                               "--listen", endpoints["producer"].removeprefix("http://"), "--private-listen", endpoints["private"].removeprefix("http://"),
                               "--gas-profile", "glm-v2", "--allocations-file", directory / "allocations.json", "--chain-id", "9317"])
            status = request(endpoints["producer"], "status")
            identity = status["identity"]
            pins = ["--peer", endpoints["producer"], "--genesis-hash", identity["genesisHash"], "--proposer", status["proposer"],
                    "--chain-id", identity["chainId"], "--source-id", identity["sourceId"], "--run-id", identity["runId"]]
            full_args = ["full", "--run-dir", directory / "full", "--listen", endpoints["full"].removeprefix("http://"), "--replication-peer", endpoints["private"], *pins]
            light_pins = pins.copy()
            light_pins[light_pins.index("--peer") + 1] = endpoints["full"]
            light_args = ["light", "--run-dir", directory / "light", "--listen", endpoints["light"].removeprefix("http://"), *light_pins]
            start("full", full_args)
            start("light", light_args)
            step()  # namespace bootstrap consumes producer nonce 0
            for nonce in range(1, 4):
                payload = directory / f"payload-{nonce}"
                payload.write_text(f"range-proof-row-{nonce}")
                output = directory / f"transaction-{nonce}"
                cli("create", "--key-file", directory / "owner.key", "--genesis-file", directory / "producer/genesis.bin",
                    "--nonce", nonce, "--request-id", "0x" + f"{nonce:02x}" * 32, "--payload-file", payload,
                    "--salt", nonce, "--out", output)
                request(endpoints["producer"], "transactions", {"transactionBytes": "0x" + output.read_bytes().hex()})
            head = step()
            query = {"height": str(head["height"]), "namespace": "1", "attribute": "group", "valueType": "u64",
                     "lower": {"value": "1", "inclusive": True}, "upper": {"value": "2", "inclusive": False}}
            full_request = request(endpoints["full"], "query/range/resolve", query)
            light_request = request(endpoints["light"], "query/range/resolve", query)
            assert full_request["request"] == light_request["request"]
            proof = request(endpoints["full"], "query/range", {"request": full_request["request"]})
            assert proof["proof"].startswith("0x") and len(proof["proof"]) > 2

            def verify(q, count):
                result = request(endpoints["light"], "query/range/verified", q)
                assert result["complete"] is True
                assert result["proofProfile"] == "range-complete-v1"
                assert len(result["rows"]) == count
                assert not result.get("continuation") and not result.get("cursor")
                return result

            verified = verify(query, 3)
            verify({**query, "lower": {"value": "1", "inclusive": False}}, 0)
            verify({**query, "upper": {"value": "1", "inclusive": True}}, 3)
            verify({k: v for k, v in query.items() if k != "upper"}, 3)
            verify({**query, "lower": {"value": "2", "inclusive": True}, "upper": {"value": "3", "inclusive": True}}, 0)
            rejected(endpoints["light"], "query/range/verified", {k: v for k, v in query.items() if k not in ("lower", "upper")})
            for change in ({"limit": 1}, {"cursor": "0x00"}, {"valueType": "str"}, {"lower": {"value": "-1", "inclusive": True}}):
                rejected(endpoints["light"], "query/range/verified", {**query, **change})
            for name in ("producer", "full", "light"):
                assert request(endpoints[name], "status")["head"] == head, "queries changed state"
            for name in ("light", "full"):
                process = processes.pop(name)
                process.terminate()
                process.wait(timeout=10)
            start("full", full_args)
            start("light", light_args)
            wait(lambda: request(endpoints["light"], "status")["head"] == head)
            assert verify(query, 3)["rows"] == verified["rows"]
            print("PASS: signed HTTP unpaged range proofs, canonical full/light requests, inclusive/exclusive and one-sided/empty results, rejected pagination/type errors, unchanged state, durable follower restart")
        except Exception:
            for path in directory.glob("*.log"):
                print(path.name, path.read_text()[-4000:])
            raise
        finally:
            for process in processes.values():
                process.terminate()
            for process in processes.values():
                process.wait(timeout=10)


if __name__ == "__main__":
    main()
