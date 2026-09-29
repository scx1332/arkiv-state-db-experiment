#!/usr/bin/env python3
"""Isolated real-process GLM v2 acceptance, including redb reopen and light proofs."""
import json
import os
from pathlib import Path
import runpy
import secrets
import subprocess
import tempfile
import urllib.error

helpers = runpy.run_path(str(Path(__file__).with_name("signed-proposer-smoke.py")))
BINARY = helpers["BINARY"]
request, wait, port = (helpers[name] for name in ("request", "wait", "port"))


def main():
    processes = {}
    with tempfile.TemporaryDirectory(prefix="arkiv-glm-smoke-") as temporary:
        directory = Path(temporary)
        token = secrets.token_hex(32)
        env = dict(os.environ, ARKIV_PROPOSER_TOKEN=token)
        endpoints = {name: f"http://127.0.0.1:{port()}" for name in ("producer", "private", "full", "light")}

        def cli(*args):
            return subprocess.check_output([str(BINARY), *map(str, args)], env=env, text=True).strip()

        def start(name, arguments):
            with (directory / f"{name}.log").open("a") as log:
                processes[name] = subprocess.Popen([str(BINARY), *arguments], env=env, stdout=log, stderr=log)
            wait(lambda: request(endpoints[name], "status"))

        def stop():
            for process in processes.values():
                process.terminate()
            for process in processes.values():
                process.wait(timeout=10)
            processes.clear()

        def step():
            request(endpoints["private"], "control", {"action": "step"}, token)
            head = request(endpoints["producer"], "status")["head"]
            for name in ("full", "light"):
                wait(lambda name=name: request(endpoints[name], "status")["head"] == head)
            return head["height"]

        def account(name, height=None):
            path = "accounts/" + addresses[name]
            if height is not None:
                path += "?height=" + height
            return request(endpoints["producer"], path)

        def transaction(kind, name, nonce, request_id, gas, extra):
            path = directory / f"{request_id}.tx"
            cli(kind, "--key-file", directory / f"{name}.key", "--genesis-file", directory / "producer/genesis.bin",
                "--nonce", nonce, "--request-id", "0x" + request_id * 32, "--gas-limit", gas, "--out", path, *extra)
            return {"transactionBytes": "0x" + path.read_bytes().hex()}

        try:
            addresses = {name: cli("keygen", "--out", directory / f"{name}.key") for name in ("producer", "alice", "bob")}
            (directory / "allocations.json").write_text(json.dumps({addresses["producer"]: str(1000 * 10**18), addresses["alice"]: str(10 * 10**18)}))
            producer = ["producer", "--run-dir", str(directory / "producer"), "--key-file", str(directory / "producer.key"),
                        "--listen", endpoints["producer"].removeprefix("http://"), "--private-listen", endpoints["private"].removeprefix("http://"),
                        "--gas-profile", "glm-v2", "--allocations-file", str(directory / "allocations.json"), "--chain-id", "9301"]
            start("producer", producer)
            status = request(endpoints["producer"], "status")
            assert status["capabilities"]["gas"]["feesDebited"] is True
            assert status["capabilities"]["gas"]["profile"] == "glm-v2"
            identity = status["identity"]
            pins = ["--peer", endpoints["producer"], "--genesis-hash", identity["genesisHash"], "--proposer", status["proposer"],
                    "--chain-id", identity["chainId"], "--source-id", identity["sourceId"], "--run-id", identity["runId"]]
            full = ["full", "--run-dir", str(directory / "full"), "--listen", endpoints["full"].removeprefix("http://"), "--replication-peer", endpoints["private"], *pins]
            light = ["light", "--run-dir", str(directory / "light"), "--listen", endpoints["light"].removeprefix("http://"), *pins]
            start("full", full)
            start("light", light)
            initial_proof = request(endpoints["light"], "query/account", {"actor": addresses["alice"], "height": "0"})
            assert initial_proof["balanceWei"] == str(10 * 10**18)
            assert initial_proof["verificationStatus"]["status"] == "verified"
            step()
            (directory / "payload").write_bytes(b"GLM fee settlement and replay")
            unfunded = transaction("create", "bob", "0", "01", "1000000", ["--payload-file", directory / "payload"])
            try:
                request(endpoints["producer"], "transactions", unfunded)
                raise AssertionError("unfunded transaction accepted")
            except urllib.error.HTTPError as error:
                assert json.load(error)["error"] == "InsufficientBalance"
            assert account("bob")["nextNonce"] == "0"
            send = transaction("transfer", "alice", "0", "02", "100000", ["--to", addresses["bob"], "--amount-wei", str(10**18)])
            request(endpoints["producer"], "transactions", send)
            height = step()
            receipt = request(endpoints["producer"], "transactions", send)
            assert receipt["status"] == "included" and receipt["gasUsed"] == "21000"
            assert receipt["feeChargedWei"] == str(21000 * 10**9)
            assert account("alice")["balanceWei"] == str(9 * 10**18 - 21000 * 10**9)
            assert account("bob")["balanceWei"] == str(10**18)
            # The exact previously rejected bytes become affordable after funding.
            request(endpoints["producer"], "transactions", unfunded)
            height = step()
            created = request(endpoints["producer"], "transactions", unfunded)
            assert created["executionStatus"] == "committed"
            assert account("bob")["balanceWei"] == str(10**18 - int(created["feeChargedWei"]))
            poor_gas = transaction("create", "bob", "1", "03", "21000", ["--payload-file", directory / "payload", "--salt", "1"])
            request(endpoints["producer"], "transactions", poor_gas)
            height = step()
            failed = request(endpoints["producer"], "transactions", poor_gas)
            assert failed["executionStatus"] == "failed" and failed["gasUsed"] == "21000"
            assert account("bob")["nextNonce"] == "2"
            query = {"query": "group = u64(1)", "requireProof": True, "height": height, "namespace": "1", "limit": 64, "cursor": None}
            assert len(request(endpoints["light"], "query/text", query)["entities"]) == 1
            proof = request(endpoints["light"], "query/account", {"actor": addresses["bob"], "height": height})
            assert proof["verificationStatus"]["status"] == "verified"
            assert proof["balanceWei"] == account("bob")["balanceWei"]
            assert sum(int(account(name)["balanceWei"]) for name in addresses) == int(proof["totalSupplyWei"])
            assert int(proof["initialSupplyWei"]) - int(proof["burnedWei"]) == int(proof["totalSupplyWei"])
            snapshots = {name: account(name, height) for name in addresses}
            stop()
            start("full", full)
            start("light", light)
            start("producer", producer)
            assert request(endpoints["producer"], "transactions", send) == receipt
            assert request(endpoints["light"], "query/account", {"actor": addresses["bob"], "height": height}) == proof
            assert {name: account(name, height) for name in addresses} == snapshots
            # Existing chain identity prevents accidental allocation edits on restart.
            stop()
            changed = directory / "changed.json"
            changed.write_text(json.dumps({addresses["producer"]: str(2000 * 10**18)}))
            modified = list(producer)
            modified[modified.index("--allocations-file") + 1] = str(changed)
            result = subprocess.run([str(BINARY), *modified], env=env, text=True, capture_output=True, timeout=15)
            assert result.returncode != 0 and "IdentityMismatch" in result.stderr
            print("PASS: GLM v2 allocation commitment, transfer/create fees, insufficient funds, failed gas, supply, full replay, light account/entity proofs, redb restart and duplicate receipts")
        except Exception:
            for name in ("producer", "full", "light"):
                log = directory / f"{name}.log"
                if log.exists():
                    print(name + ": " + log.read_text()[-6000:])
            raise
        finally:
            stop()


if __name__ == "__main__":
    main()
