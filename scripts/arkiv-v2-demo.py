#!/usr/bin/env python3
"""Run an unsigned Arkiv entity simulator producer/full/light trio. Never resets stores."""
import argparse
import json
import os
from pathlib import Path
import secrets
import signal
import subprocess
import time
import urllib.request

ROOT = Path(__file__).resolve().parents[1]


def status(port):
    with urllib.request.urlopen(f"http://127.0.0.1:{port}/sim/v1/status", timeout=2) as response:
        return json.load(response)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--run-dir", type=Path, required=True)
    parser.add_argument("--base-port", type=int, default=19400)
    parser.add_argument("--binary", type=Path)
    args = parser.parse_args()
    if not 1024 <= args.base_port <= 65532:
        parser.error("base port must leave four nonprivileged ports")
    directory = args.run_dir.resolve()
    directory.mkdir(parents=True, exist_ok=True, mode=0o700)
    if args.binary:
        binary = args.binary.resolve()
    else:
        subprocess.run(["cargo", "build", "-p", "arkiv-node", "--bin", "arkiv-simulator"], cwd=ROOT, check=True)
        binary = ROOT / "target/debug/arkiv-simulator"
    token_file = directory / "private-token"
    if not token_file.exists():
        with os.fdopen(os.open(token_file, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "w") as output:
            output.write(secrets.token_hex(32))
    token = token_file.read_text().strip()
    environment = dict(os.environ, SIMULATOR_ADMIN_TOKEN=token, SIMULATOR_REPLICATION_TOKEN=token)
    processes = []
    logs = []
    def launch(role, flags):
        log = (directory / f"{role}.log").open("ab")
        logs.append(log)
        process = subprocess.Popen([str(binary), role, "--unsigned-simulator", "--run-dir", str(directory / role), *flags], env=environment, stdout=log, stderr=log)
        processes.append(process)
        return process
    def ready(port, process):
        for _ in range(150):
            if process.poll() is not None:
                raise RuntimeError("Node exited; inspect role log in run directory")
            try:
                result = status(port)
                capabilities = result["capabilities"]
                if capabilities["profile"] != "arkiv-entity-v2" or "entityNamespace" not in capabilities:
                    raise RuntimeError("Unexpected node or occupied port")
                return result
            except (OSError, ValueError):
                time.sleep(0.1)
        raise RuntimeError("Node startup timeout")
    def stop(_signum=None, _frame=None):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    try:
        base = args.base_port
        producer = launch("producer", ["--listen", f"127.0.0.1:{base}", "--admin-listen", f"127.0.0.1:{base+1}", "--chain-id", "9002", "--start", "running", "--interval-ms", "1000"])
        identity = ready(base, producer)
        shared = ["--peer", f"http://127.0.0.1:{base}", "--source-id", identity["sourceId"], "--run-id", identity["runId"], "--genesis-hash", identity["genesisHash"], "--chain-id", identity["chainId"], "--interval-ms", "200"]
        light = launch("light", [*shared, "--listen", f"127.0.0.1:{base+2}"])
        full = launch("full", [*shared, "--listen", f"127.0.0.1:{base+3}", "--replication-peer", f"http://127.0.0.1:{base+1}"])
        ready(base+2, light)
        ready(base+3, full)
        (directory / "identity.json").write_text(json.dumps({key: identity[key] for key in ("sourceId", "runId", "genesisHash", "chainId")}, indent=2) + "\n")
        print(json.dumps({"profile": "arkiv-entity-v2", "producer": base, "light": base+2, "full": base+3, "runDir": str(directory)}), flush=True)
        while all(process.poll() is None for process in processes):
            time.sleep(1)
        raise RuntimeError("A node exited; inspect logs")
    except KeyboardInterrupt:
        pass
    finally:
        for process in processes:
            if process.poll() is None:
                process.terminate()
        for process in processes:
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
        for log in logs:
            log.close()

if __name__ == "__main__":
    main()
