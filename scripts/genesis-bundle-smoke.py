#!/usr/bin/env python3
"""Isolated genesis-prefill CLI/producer/full/light/restart acceptance.

The fixed keys are disposable fixture identities, never deployment credentials.
Expected entity keys were independently calculated from the documented Keccak
preimage (chain ID, Arkiv address, creator, creation nonce, salt). Rent vectors
use independent integer arithmetic: storage units 1764/1762/1764, rent 4/5/5 gas.
"""

import json
import os
import secrets
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
B = Path(os.environ.get("ARKIV_PROPOSER_BIN", ROOT / "target/debug/arkiv-proposer"))

FIXTURE = """schema = "arkiv-genesis-bundle-v1"
protocol = "arkiv-launch-prefill-v1"
name = "Independent prefill end-to-end"
profile = { preset = "arkiv-entity-v3/glm-v2@1" }

[chain]
id = "9405"
run_id = "0x00000000000000000000000000009405"
start_time = "2026-09-25T00:00:00Z"
block_period_ms = "5000"
proposer = "@proposer"

[allocations]
expected_initial_supply_wei = "1010000000000000000000"

[[allocations.entry]]
label = "proposer"
address = "0x1a642f0e3c3af545e7acbd38b07251b3990914f1"
balance_wei = "1000000000000000000000"

[[allocations.entry]]
label = "alice"
address = "0x5050a4f4b3f9338c3472dcc01a87c76a144b3c9c"
balance_wei = "9000000000000000000"

[[allocations.entry]]
label = "bob"
address = "0x3325a78425f17a7e487eb5666b2bfd93abb06c70"
balance_wei = "1000000000000000000"

[prefill]
builder = "arkiv-prefill-v1"
namespace = "entities"
rent_policy = "sponsored-rent-v1"
sponsor = "@alice"

[prefill.limits]
max_entities = "16"
max_canonical_bytes = "65536"
max_storage_units = "65536"

[[prefill.entity]]
creator = "@alice"
create_nonce = "0"
salt = "10"
expires_at_height = "4"
flags = 0
attributes.body = { bytes_text = "prefilled alpha", indexed = false }
attributes.title = { str = "alpha", indexed = true }
attributes.group = { u64 = "42", indexed = true }

[[prefill.entity]]
creator = "@alice"
create_nonce = "1"
salt = "11"
expires_at_height = "5"
flags = 0
attributes.body = { bytes_text = "prefilled beta", indexed = false }
attributes.title = { str = "beta", indexed = true }
attributes.group = { u64 = "42", indexed = true }

[[prefill.entity]]
creator = "@bob"
create_nonce = "0"
salt = "12"
expires_at_height = "6"
flags = 0
attributes.body = { bytes_text = "prefilled gamma", indexed = false }
attributes.title = { str = "gamma", indexed = true }
attributes.group = { u64 = "42", indexed = true }
"""

WALLETS = {
    "proposer": "0x1a642f0e3c3af545e7acbd38b07251b3990914f1",
    "alice": "0x5050a4f4b3f9338c3472dcc01a87c76a144b3c9c",
    "bob": "0x3325a78425f17a7e487eb5666b2bfd93abb06c70",
}

EXPECTED = {
    "entities": [
        {
            "creator": "0x5050a4f4b3f9338c3472dcc01a87c76a144b3c9c",
            "nonce": 0,
            "salt": 10,
            "expiry": 4,
            "title": "alpha",
            "key": "0x223fd62733c4f6d95ecf8d6201f89d15197e6b29b86b2a7be7f985e2227aabfc",
            "storageUnits": 1764,
            "rentGas": 4,
        },
        {
            "creator": "0x5050a4f4b3f9338c3472dcc01a87c76a144b3c9c",
            "nonce": 1,
            "salt": 11,
            "expiry": 5,
            "title": "beta",
            "key": "0xa4c5ac5a9730d7f3d2598e88b53abdf3639a71475a43ffb3171631fbd225882c",
            "storageUnits": 1762,
            "rentGas": 5,
        },
        {
            "creator": "0x3325a78425f17a7e487eb5666b2bfd93abb06c70",
            "nonce": 0,
            "salt": 12,
            "expiry": 6,
            "title": "gamma",
            "key": "0xffe39c77fb98cdc8510526936c4ab7f3a25a24fe2fd946926312678ca7f2a6b0",
            "storageUnits": 1764,
            "rentGas": 5,
        },
    ],
    "burnedWei": "14000000000",
    "nextAliceKey": "0xb6efeb844b48b2758358c68bea181dae6c6e6f653349ba2edf53aa16a9fd555a",
}


def main():
    with tempfile.TemporaryDirectory(prefix="arkiv-genesis-prefill-") as temporary:
        D = Path(temporary)
        wallets = WALLETS
        expected = EXPECTED
        (D / "chain.toml").write_text(FIXTURE)
        for name, value in (("proposer", 1), ("alice", 2), ("bob", 3)):
            key = D / (name + ".key")
            key.write_text("0x" + f"{value:02x}" * 32)
            key.chmod(0o600)
        processes = {}
        reports = {}
        token = secrets.token_hex(32)
        env = dict(os.environ, ARKIV_PROPOSER_TOKEN=token)
        used = set()

        def port():
            while True:
                with socket.socket() as s:
                    s.bind(("127.0.0.1", 0))
                    p = s.getsockname()[1]
                if p not in used:
                    used.add(p)
                    return p

        endpoints = {
            name: f"http://127.0.0.1:{port()}"
            for name in ("producer", "private", "full", "light")
        }

        def request(role, path, body=None):
            headers = {"Content-Type": "application/json"}
            if role == "private":
                headers["Authorization"] = "Bearer " + token
            req = urllib.request.Request(
                endpoints[role] + "/signed/v1/" + path,
                data=None if body is None else json.dumps(body).encode(),
                headers=headers,
            )
            for attempt in range(30):
                try:
                    with urllib.request.urlopen(req, timeout=5) as f:
                        return json.load(f)
                except urllib.error.HTTPError as e:
                    raw = e.read()
                    if e.code == 429 and attempt < 29:
                        time.sleep(0.1)
                        continue
                    try:
                        detail = json.loads(raw)
                    except ValueError:
                        detail = raw.decode(errors="replace")[:500]
                    return {"httpStatus": e.code, "body": detail}

        def wait(fn):
            deadline = time.monotonic() + 30
            while time.monotonic() < deadline:
                try:
                    v = fn()
                    if v:
                        return v
                except OSError:
                    pass
                time.sleep(0.1)
            raise AssertionError("Timed out")

        def cli(*args, ok=True):
            r = subprocess.run(
                [str(B), *map(str, args)],
                env=env,
                capture_output=True,
                text=True,
                timeout=120,
            )
            if ok:
                assert r.returncode == 0, (args, r.stdout, r.stderr)
            else:
                assert r.returncode != 0, (args, r.stdout, r.stderr)
            return r

        def start(role, args):
            with (D / (role + ".log")).open("a") as log:
                processes[role] = subprocess.Popen(
                    [str(B), *map(str, args)], env=env, stdout=log, stderr=log
                )
            wait(lambda: request(role, "status"))

        def stop():
            for p in processes.values():
                p.terminate()
            for p in processes.values():
                try:
                    p.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    p.kill()
                    p.wait()
            processes.clear()

        def sync():
            head = request("producer", "status")["head"]
            for role in ("full", "light"):
                wait(lambda role=role: request(role, "status").get("head") == head)
            return head

        def step():
            v = request("private", "control", {"action": "step"})
            assert "httpStatus" not in v, v
            return sync()

        def query(q, height):
            for attempt in range(30):
                result = request(
                    "light",
                    "query/text",
                    {
                        "query": q,
                        "requireProof": True,
                        "height": str(height),
                        "namespace": "1",
                        "limit": 64,
                        "cursor": None,
                    },
                )
                # Query/text wraps transient lock contention in a 200 response.
                # Retry only that explicit error; proof failures must fail here.
                if result.get("error") != "ServerBusy" or attempt == 29:
                    break
                time.sleep(0.1)
            assert (
                result.get("verificationStatus", {}).get("status") == "verified"
            ), result
            return result

        def account(name, height):
            result = request(
                "light",
                "query/account",
                {"actor": wallets[name], "height": str(height)},
            )
            assert (
                result.get("verificationStatus", {}).get("status") == "verified"
            ), result
            return result

        def author(kind, name, nonce, **opts):
            rid = "0x" + secrets.token_hex(32)
            path = D / (rid[2:] + ".tx")
            args = [
                kind,
                "--key-file",
                D / (name + ".key"),
                "--genesis-file",
                D / "bundle/genesis.bin",
                "--nonce",
                nonce,
                "--request-id",
                rid,
                "--out",
                path,
                "--gas-limit",
                "1000000",
            ]
            if kind == "create":
                (D / "live-payload.txt").write_text("post-genesis entity")
                args += [
                    "--data-file",
                    D / "live-payload.txt",
                    "--salt",
                    "99",
                    "--lifetime-blocks",
                    "10",
                ]
            else:
                args += ["--to", opts["to"], "--amount-wei", opts["amount"]]
            cli(*args)
            body = {"transactionBytes": "0x" + path.read_bytes().hex()}
            admission = request("producer", "transactions", body)
            assert admission.get("status") == "pending", admission
            return rid, body

        def included(name, rid):
            receipt = request("producer", f"requests/{wallets[name]}/{rid}")
            assert receipt.get("executionStatus") == "committed", receipt
            return receipt

        try:
            cli("genesis", "lock", D / "chain.toml")
            cli("genesis", "build", D / "chain.toml", "--out", D / "bundle", "--locked")
            cli("genesis", "verify", D / "bundle")
            reports["build"] = json.loads((D / "bundle/report.json").read_text())
            cli(
                "genesis",
                "build",
                D / "chain.toml",
                "--out",
                D / "bundle-independent",
                "--locked",
            )
            assert (D / "bundle/genesis.bin").read_bytes() == (
                D / "bundle-independent/genesis.bin"
            ).read_bytes()
            assert (D / "bundle/prefill.bin").read_bytes() == (
                D / "bundle-independent/prefill.bin"
            ).read_bytes()
            assert reports["build"]["burnedWei"] == expected["burnedWei"], reports[
                "build"
            ]
            by_key = {row["key"]: row for row in reports["build"]["entities"]}
            for row in expected["entities"]:
                built = by_key[row["key"]]
                assert int(built["storageUnits"]) == row["storageUnits"]
                assert int(built["rentGas"]) == row["rentGas"]
                assert int(built["createNonce"]) == row["nonce"]
            print(
                "PASS deterministic independent builds and exact sponsored rent",
                flush=True,
            )
            producer = [
                "producer",
                "--run-dir",
                D / "producer",
                "--key-file",
                D / "proposer.key",
                "--bundle",
                D / "bundle",
                "--listen",
                endpoints["producer"][7:],
                "--private-listen",
                endpoints["private"][7:],
                "--start",
                "paused",
            ]
            start("producer", producer)
            status = request("producer", "status")
            identity = status["identity"]
            assert status["head"]["height"] == "0", status
            pins = [
                "--genesis-hash",
                identity["genesisHash"],
                "--proposer",
                status["proposer"],
                "--chain-id",
                identity["chainId"],
                "--source-id",
                identity["sourceId"],
                "--run-id",
                identity["runId"],
            ]
            full = [
                "full",
                "--run-dir",
                D / "full",
                "--listen",
                endpoints["full"][7:],
                "--peer",
                endpoints["producer"],
                "--replication-peer",
                endpoints["private"],
                *pins,
            ]
            light = [
                "light",
                "--run-dir",
                D / "light",
                "--listen",
                endpoints["light"][7:],
                "--peer",
                endpoints["full"],
                *pins,
            ]
            start("full", full)
            start("light", light)
            sync()
            reports["identity"] = identity
            genesis = query("group = u64(42)", 0)
            reports["genesisQuery"] = genesis
            entities = {e["key"]: e for e in genesis["entities"]}
            assert set(entities) == {r["key"] for r in expected["entities"]}, entities
            for row in expected["entities"]:
                entity = entities[row["key"]]
                assert entity["createdAt"] == "0" and entity["updatedAt"] == "0"
                assert (
                    entity["creator"] == row["creator"]
                    and entity["owner"] == row["creator"]
                )
                assert entity["expiresAt"] == str(row["expiry"])
                body = next(a for a in entity["attributes"] if a["name"] == "body")
                assert body["type"] == "bytes" and body["indexed"] is False
                assert bytes.fromhex(body["value"][2:]) == ("prefilled " + row["title"]).encode()
            reports["genesisAccounts"] = {name: account(name, 0) for name in wallets}
            for name, value in reports["genesisAccounts"].items():
                assert value["nextNonce"] == "0"
            assert int(
                reports["genesisAccounts"]["alice"]["balanceWei"]
            ) == 9 * 10**18 - int(expected["burnedWei"])
            assert (
                reports["genesisAccounts"]["alice"]["burnedWei"]
                == expected["burnedWei"]
            )
            print(
                "PASS height0 entity/account proofs, keys, metadata and supply",
                flush=True,
            )
            rid, create_body = author("create", "alice", 0)
            assert step()["height"] == "1"
            created = included("alice", rid)
            reports["firstLiveCreate"] = created
            alice = query(f"$owner = addr({wallets['alice']})", 1)
            assert len(alice["entities"]) == 3
            live = [e for e in alice["entities"] if e["createdAt"] == "1"]
            assert len(live) == 1 and live[0]["key"] == expected["nextAliceKey"], live
            assert account("proposer", 1)["nextNonce"] == "0"
            assert account("alice", 1)["nextNonce"] == "1"
            rid2, transfer_body = author(
                "transfer", "bob", 0, to=wallets["alice"], amount=10**17
            )
            assert step()["height"] == "2"
            reports["transfer"] = included("bob", rid2)
            assert step()["height"] == "3"
            before = query("group = u64(42)", 0)
            before_accounts = {name: account(name, 3) for name in wallets}
            stop()
            start("full", full)
            start("light", light)
            start("producer", producer)
            sync()
            assert query("group = u64(42)", 0) == before
            assert {name: account(name, 3) for name in wallets} == before_accounts
            assert request("producer", "transactions", create_body) == created
            assert (
                request("producer", "transactions", transfer_body)
                == reports["transfer"]
            )
            print(
                "PASS live nonce0/creationnonce2, ordinary block1, transfers and restart",
                flush=True,
            )
            reports["expiry"] = {}
            for height, count in [(4, 2), (5, 1), (6, 0)]:
                assert step()["height"] == str(height)
                response = query("group = u64(42)", height)
                assert len(response["entities"]) == count
                reports["expiry"][str(height)] = {"verified": True, "count": count}
            assert len(query("group = u64(42)", 0)["entities"]) == 3
            assert len(query(f"$owner = addr({wallets['alice']})", 6)["entities"]) == 1
            reports["finalAccounts"] = {name: account(name, 6) for name in wallets}
            supply = reports["finalAccounts"]["alice"]
            assert sum(
                int(a["balanceWei"]) for a in reports["finalAccounts"].values()
            ) == int(supply["totalSupplyWei"])
            assert int(supply["initialSupplyWei"]) - int(supply["burnedWei"]) == int(
                supply["totalSupplyWei"]
            )
            reports["passed"] = True
            (D / "report.json").write_text(json.dumps(reports, indent=2) + "\n")
            print(
                "PASS scheduled expiry, retained historical proofs and exact supply reconciliation",
                flush=True,
            )
        except Exception:
            for role in ("producer", "full", "light"):
                log = D / (role + ".log")
                if log.exists():
                    print(role + ": " + log.read_text()[-4000:])
            raise
        finally:
            stop()


if __name__ == "__main__":
    main()
