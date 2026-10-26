#!/usr/bin/env python3
"""Run the user-defined-columns SDK against disposable producer/full/light nodes.

Requires Bun, an arkiv-sdk-js checkout of the udc/* branches with dependencies
installed and `bun run build` done, and a built arkiv-proposer. Only public
disposable fixture keys are used. Set ARKIV_SDK_DIR and ARKIV_PROPOSER_BIN to
override locations. Stages: exercise (writes, typed reads, queries, limits),
then reopen after a three-role restart, then follower reads via the full node.
"""

import importlib.util
import json
import os
from pathlib import Path
import secrets
import shutil
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
BINARY = Path(
    os.environ.get("ARKIV_PROPOSER_BIN", ROOT / "target/debug/arkiv-proposer")
).resolve()
SDK = Path(os.environ.get("ARKIV_SDK_DIR", ROOT.parent / "arkiv-sdk-js")).resolve()


def main():
    spec = importlib.util.spec_from_file_location(
        "genesis_fixture", ROOT / "scripts/genesis-bundle-smoke.py"
    )
    fixture = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(fixture)
    bun = shutil.which("bun")
    assert (
        bun and BINARY.is_file() and (SDK / "node_modules/viem").is_dir()
    ), "Build arkiv-proposer and install sibling SDK dependencies/Bun first"
    with tempfile.TemporaryDirectory(prefix="arkiv-sdk-compat-") as directory:
        work = Path(directory)
        processes = {}
        used = set()

        def port():
            while True:
                with socket.socket() as sock:
                    sock.bind(("127.0.0.1", 0))
                    number = sock.getsockname()[1]
                if number not in used:
                    used.add(number)
                    return number

        endpoints = {
            role: f"http://127.0.0.1:{port()}"
            for role in ("producer", "private", "full", "light")
        }
        env = dict(
            os.environ,
            ARKIV_PROPOSER_TOKEN=secrets.token_hex(32),
            SDK_RPC_URL=endpoints["producer"],
            SDK_REPORT_FILE=str(work / "sdk-report.json"),
        )
        # Keep the three prefills alive during the SDK run and speed up isolated blocks.
        source = fixture.FIXTURE.replace(
            'block_period_ms = "5000"', 'block_period_ms = "500"'
        )
        for old, new in ((4, 10000), (5, 10001), (6, 10002)):
            source = source.replace(
                f'expires_at_height = "{old}"', f'expires_at_height = "{new}"'
            )
        (work / "chain.toml").write_text(source)
        (work / "proposer.key").write_text("0x" + "01" * 32)
        (work / "proposer.key").chmod(0o600)
        modules = work / "node_modules"
        (modules / "@arkiv-network").mkdir(parents=True)
        (modules / "@arkiv-network/sdk").symlink_to(SDK, target_is_directory=True)
        (modules / "viem").symlink_to(
            SDK / "node_modules/viem", target_is_directory=True
        )
        shutil.copyfile(ROOT / "scripts/udc-sdk-smoke.ts", work / "smoke.ts")

        def command(*args):
            return subprocess.run(
                [str(BINARY), *map(str, args)],
                env=env,
                check=True,
                capture_output=True,
                text=True,
                timeout=120,
            )

        def request(role, path="status", body=None):
            headers = {"Content-Type": "application/json"}
            if role == "private":
                headers["Authorization"] = "Bearer " + env["ARKIV_PROPOSER_TOKEN"]
            req = urllib.request.Request(
                endpoints[role] + "/signed/v1/" + path,
                data=None if body is None else json.dumps(body).encode(),
                headers=headers,
            )
            for attempt in range(30):
                try:
                    with urllib.request.urlopen(req, timeout=5) as response:
                        return json.load(response)
                except urllib.error.HTTPError as error:
                    # Native REST rejects busy requests before execution. Never
                    # retry an ambiguous transport failure or another status.
                    if error.code != 429 or attempt == 29:
                        raise
                    error.close()
                    time.sleep(0.1)

        def wait(predicate):
            deadline = time.monotonic() + 30
            while time.monotonic() < deadline:
                try:
                    answer = predicate()
                    if answer:
                        return answer
                except (OSError, urllib.error.URLError):
                    pass
                time.sleep(0.1)
            raise AssertionError("Timed out waiting for isolated node")

        def start(role, args):
            with (work / (role + ".log")).open("a") as log:
                processes[role] = subprocess.Popen(
                    [str(BINARY), *map(str, args)], env=env, stdout=log, stderr=log
                )
            wait(lambda: request(role))

        def stop():
            for process in processes.values():
                process.terminate()
            for process in processes.values():
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
            processes.clear()

        def sync():
            head = request("producer")["head"]
            for role in ("full", "light"):
                wait(lambda role=role: request(role)["head"] == head)

        try:
            command("genesis", "lock", work / "chain.toml")
            command(
                "genesis",
                "build",
                work / "chain.toml",
                "--out",
                work / "bundle",
                "--locked",
            )
            producer = [
                "producer",
                "--run-dir",
                work / "producer",
                "--key-file",
                work / "proposer.key",
                "--bundle",
                work / "bundle",
                "--listen",
                endpoints["producer"][7:],
                "--private-listen",
                endpoints["private"][7:],
                "--start",
                "paused",
            ]
            start("producer", producer)
            status = request("producer")
            identity = status["identity"]
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
                work / "full",
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
                work / "light",
                "--listen",
                endpoints["light"][7:],
                "--peer",
                endpoints["full"],
                *pins,
            ]
            start("full", full)
            start("light", light)
            wait(lambda: request("private", "control", {"action": "resume"}))
            subprocess.run(
                [bun, str(work / "smoke.ts"), "exercise"],
                env=env,
                cwd=work,
                check=True,
                timeout=240,
            )
            wait(lambda: request("private", "control", {"action": "pause"}))
            sync()
            stop()
            # Followers must reopen before the producer is available.
            start("full", full)
            start("light", light)
            producer[-1] = "paused"
            start("producer", producer)
            sync()
            subprocess.run(
                [bun, str(work / "smoke.ts"), "reopen"],
                env=env,
                cwd=work,
                check=True,
                timeout=60,
            )
            follower_env = dict(env, SDK_RPC_URL=endpoints["full"])
            subprocess.run(
                [bun, str(work / "smoke.ts"), "follower"],
                env=follower_env,
                cwd=work,
                check=True,
                timeout=60,
            )
            print(
                "PASS independently replayed full/light heads and three-role restart",
                flush=True,
            )
        except Exception:
            for role in processes:
                print(f"{role}: {(work / (role + '.log')).read_text()[-4000:]}")
            raise
        finally:
            stop()


if __name__ == "__main__":
    main()
