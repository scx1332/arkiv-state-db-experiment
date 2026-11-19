#!/usr/bin/env python3
"""Soak one arkiv-proposer producer for a few hours and record how it behaves.

    scripts/soak/run.py --work DIR [--quick] [--growth-min 60] [--churn-min 40]
        [--readers-min 15] [--block-ms 250] [--port 18700] [--drop-caches]
        [--bundle DIR --prebuilt-state DIR --sourcify-keys FILE --mutate-min 60]

By default the node starts from an empty genesis. With --bundle it starts from
a built genesis bundle instead (the Sourcify prefill, say), installing
--prebuilt-state on the first start, and --sourcify-keys names contract keys
of that dataset, owned by the soak account, for the mutate phases and probes.

Phases, each writing JSON lines under DIR (see docs/benchmarking.md):

  cohort   2,000 entities every read probe is aimed at, so probes stay comparable
  mutate   (with --sourcify-keys) patch dataset contracts: a third of the time
           only `verifiedat`, then as a re-verification does, rewriting
           attributes whose values millions of contracts share
  growth   creates at the block cap with a long lifetime; state only grows
  churn    creates, patches, deletes and extensions with a 10-minute lifetime,
           so the expiry sweep deletes as fast as the workload creates
  readers  churn at half rate beside 4 concurrent readers
  restart  SIGTERM, start again, check the chain is the same, probe again

A read probe runs every 5 minutes during growth and churn; a sampler records
the node's head, memory, CPU, files and metrics every 10 seconds throughout.

The run stops, keeping everything, when a transaction reverts, the node exits
or free disk drops below --min-free-gb. Nothing is retried. DIR must not exist.
Exit code: 0 completed, 1 stopped by a stop rule, 2 could not start.
"""

import argparse
import hashlib
import importlib.util
import json
import os
import platform
import re
import secrets
import shutil
import signal
import subprocess
import sys
import threading
import time
import urllib.request
from datetime import datetime, timedelta, timezone
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
# A development key: the chain is disposable and listens on loopback only.
PRIVATE_KEY = "0x" + "5a" * 32
FUNDING_WEI = 10**27
SAMPLE_SECS = 10
PROBE_SECS = 300
# Metric families kept from each scrape; histograms keep their sum and count.
METRICS = re.compile(
    r"^rogue_one_((?:head_height|pending_transactions|blocks_produced_total|node_tick_\w+"
    r"|db_(?:stored_bytes|file_allocated_bytes|entries|commits_total|commit_\w+_total"
    r"|backend_(?:read|write)_bytes_total|operation_errors_total"
    r"|operation_duration_seconds_(?:sum|count)))(?:\{[^}]*\})?) (\S+)$"
)


class Stop(Exception):
    """A stop rule fired; the message is the reason recorded in status.json."""


def get(url, timeout=5):
    with urllib.request.urlopen(url, timeout=timeout) as response:
        return response.read().decode()


def rpc(url, method, *params):
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": list(params)}).encode()
    request = urllib.request.Request(url, body, {"content-type": "application/json"})
    with urllib.request.urlopen(request, timeout=5) as response:
        return json.load(response)["result"]


class Soak:
    def __init__(self, args):
        self.args = args
        self.work = Path(args.work).resolve()
        self.url = f"http://127.0.0.1:{args.port}"
        self.metrics_url = f"http://127.0.0.1:{args.port + 2}/metrics"
        self.binary = Path(os.environ.get("ARKIV_PROPOSER_BIN", ROOT / "target/release/arkiv-proposer"))
        self.node = None
        self.phase = "setup"
        self.failure = None
        self.done = threading.Event()
        self.probing = threading.Event()
        self.restart = {}
        self.bundle = Path(args.bundle).resolve() if args.bundle else self.work / "bundle"
        self.prebuilt = Path(args.prebuilt_state).resolve() if args.prebuilt_state else None
        self.env = dict(
            os.environ,
            RPC_URL=self.url,
            PRIVATE_KEY=PRIVATE_KEY,
            ARKIV_PROPOSER_TOKEN=secrets.token_hex(32),
            COHORT_TAG="cohort",
            COHORT_KEYS=str(self.work / "cohort-keys.json"),
            GROW_TAG="growth",
        )
        if args.sourcify_keys:
            self.env["SOURCIFY_KEYS"] = str(Path(args.sourcify_keys).resolve())

    # ------------------------------------------------------------ records --

    def append(self, name, record):
        with (self.work / name).open("a") as out:
            out.write(json.dumps(record) + "\n")

    def status(self, state):
        record = {
            "state": state,
            "phase": self.phase,
            "failure": self.failure,
            "updated": datetime.now(timezone.utc).isoformat(timespec="seconds"),
            "restart": self.restart,
        }
        temporary = self.work / "status.json.tmp"
        temporary.write_text(json.dumps(record, indent=1) + "\n")
        temporary.replace(self.work / "status.json")

    def plan(self):
        git = lambda *a: subprocess.run(["git", "-C", str(ROOT), *a], capture_output=True, text=True).stdout.strip()
        cpu = next((l.split(":", 1)[1].strip() for l in open("/proc/cpuinfo") if l.startswith("model name")), "")
        memory = next(int(l.split()[1]) for l in open("/proc/meminfo") if l.startswith("MemTotal"))
        device = subprocess.run(["df", "--output=source,fstype", str(self.work)], capture_output=True, text=True)
        a = self.args
        record = {
            "started": datetime.now(timezone.utc).isoformat(timespec="seconds"),
            "commit": git("rev-parse", "HEAD"),
            "dirty": bool(git("status", "--porcelain", "--untracked-files=no")),
            "binarySha256": hashlib.sha256(self.binary.read_bytes()).hexdigest(),
            "host": {
                "cpu": cpu,
                "cores": os.cpu_count(),
                "memoryGiB": round(memory / 2**20, 1),
                "kernel": platform.release(),
                "filesystem": " ".join(device.stdout.split()[2:]),
                "load1": os.getloadavg()[0],
            },
            "blockMs": a.block_ms,
            "minutes": {"mutate": a.mutate_min, "growth": a.growth_min, "churn": a.churn_min, "readers": a.readers_min},
            "start": {"bundle": str(self.bundle), "prebuiltState": a.prebuilt_state, "sourcifyKeys": a.sourcify_keys}
            if a.bundle else "empty genesis",
            "workload": {
                "sdk": json.loads((HERE / "package.json").read_text())["dependencies"],
                "batch": 16,
                "wave": 8,
                "cohort": 2000,
                "probeReads": 100,
                "probeEverySeconds": 30 if a.quick else PROBE_SECS,
                "growthLifetimeBlocks": self.growth_lifetime,
                "churnLifetimeBlocks": self.churn_lifetime,
                "churnMix": "per 16 operations: 11 creates, 3 patches, 1 delete, 1 extension",
                "readersRateEps": self.readers_rate,
            },
            "node": {"flags": self.node_flags()[1:], "readConcurrency": "default (4)"},
            "dropCaches": a.drop_caches,
        }
        (self.work / "plan.json").write_text(json.dumps(record, indent=1) + "\n")

    # --------------------------------------------------------------- node --

    @property
    def growth_lifetime(self):
        return 2_000_000

    @property
    def churn_lifetime(self):
        return max(64, 600_000 // self.args.block_ms) if not self.args.quick else 30_000 // self.args.block_ms

    @property
    def readers_rate(self):
        # Half of the block cap: 128 operations per block.
        return round(64_000 / self.args.block_ms)

    def node_flags(self):
        port = self.args.port
        return [
            str(self.binary), "producer",
            "--run-dir", str(self.work / "producer"),
            "--key-file", str(self.work / "proposer.key"),
            "--bundle", str(self.bundle),
            "--listen", f"127.0.0.1:{port}",
            "--private-listen", f"127.0.0.1:{port + 1}",
            "--metrics-listen", f"127.0.0.1:{port + 2}",
            "--metrics-interval-secs", "60",
            "--start", "running",
        ] + (["--prebuilt-state", str(self.prebuilt), "--genesis-install", "adopt"] if self.prebuilt else [])  # fmt: skip

    def genesis(self):
        if self.args.bundle:
            # A built bundle: only the proposer key, the fixture key every bundle here is made for.
            key = self.work / "proposer.key"
            key.write_text("0x" + "01" * 32)
            key.chmod(0o600)
            return
        spec = importlib.util.spec_from_file_location("fixture", ROOT / "scripts/genesis-bundle-smoke.py")
        fixture = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(fixture)
        address = subprocess.run(
            ["bun", "load.ts", "address"], cwd=HERE, env=self.env, capture_output=True, text=True, check=True
        ).stdout.strip()
        start = (datetime.now(timezone.utc) - timedelta(seconds=5)).strftime("%Y-%m-%dT%H:%M:%SZ")

        def replace(text, old, new):
            if old not in text:
                raise SystemExit(f"genesis fixture changed: {old!r} not found")
            return text.replace(old, new, 1)

        chain = fixture.FIXTURE
        chain = replace(chain, 'block_period_ms = "5000"', f'block_period_ms = "{self.args.block_ms}"')
        chain = replace(chain, 'start_time = "2026-09-25T00:00:00Z"', f'start_time = "{start}"')
        chain = replace(
            chain,
            'expected_initial_supply_wei = "1010000000000000000000"',
            f'expected_initial_supply_wei = "{1010 * 10**18 + FUNDING_WEI}"',
        )
        chain = replace(
            chain,
            "[prefill]",
            f'[[allocations.entry]]\nlabel = "soak"\naddress = "{address}"\nbalance_wei = "{FUNDING_WEI}"\n\n[prefill]',
        )
        # The fixture's three prefilled entities would expire in the first seconds.
        for height in (4, 5, 6):
            chain = replace(chain, f'expires_at_height = "{height}"', f'expires_at_height = "{6_000_000 + height}"')
        (self.work / "chain.toml").write_text(chain)
        key = self.work / "proposer.key"
        key.write_text("0x" + "01" * 32)
        key.chmod(0o600)
        for command in (["genesis", "lock", "chain.toml"], ["genesis", "build", "chain.toml", "--out", "bundle", "--locked"]):
            subprocess.run([str(self.binary), *command], cwd=self.work, env=self.env, check=True, capture_output=True)

    def start_node(self):
        log = (self.work / "producer.log").open("a")
        self.node = subprocess.Popen(self.node_flags(), env=self.env, stdout=log, stderr=log)
        # The first start consumes the prebuilt state (it is moved into the run directory).
        self.prebuilt = None
        started = time.monotonic()
        while time.monotonic() - started < self.args.ready_min * 60:
            if self.node.poll() is not None:
                raise Stop(f"the node exited with {self.node.returncode} while starting")
            try:
                if int(json.loads(get(f"{self.url}/signed/v1/status", 2))["head"]["height"]) >= 2:
                    return time.monotonic() - started
            except (OSError, ValueError, KeyError):
                pass
            time.sleep(0.05)
        raise Stop(f"the node was not ready within {self.args.ready_min:g} minutes")

    def stop_node(self):
        if self.node and self.node.poll() is None:
            self.node.send_signal(signal.SIGTERM)
            started = time.monotonic()
            self.node.wait(timeout=600)
            return time.monotonic() - started
        return None

    # ----------------------------------------------------------- sampling --

    def sample(self):
        record = {"t": time.time(), "phase": self.phase, "load1": os.getloadavg()[0]}
        pid = self.node.pid if self.node and self.node.poll() is None else None
        if pid:
            try:
                fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
                record["cpuSeconds"] = (int(fields[11]) + int(fields[12])) / os.sysconf("SC_CLK_TCK")
                status = Path(f"/proc/{pid}/status").read_text()
                record["rssBytes"] = int(re.search(r"VmRSS:\s+(\d+)", status).group(1)) * 1024
            except (OSError, AttributeError, IndexError):
                pass
        files = {}
        for path in (self.work / "producer").glob("*.redb"):
            try:
                stat = path.stat()
                files[path.name] = {"bytes": stat.st_size, "allocated": stat.st_blocks * 512}
            except OSError:
                pass
        record["files"] = files
        try:
            # Asked over RPC: the node's own head metric is sampled without waiting for the node
            # lock and goes stale while the producer is saturated.
            record["head"] = int(rpc(self.url, "eth_blockNumber"), 16)
        except (OSError, ValueError, KeyError):
            pass
        record["freeBytes"] = shutil.disk_usage(self.work).free
        try:
            record["metrics"] = {
                m.group(1): float(m.group(2))
                for m in map(METRICS.match, get(self.metrics_url).splitlines())
                if m
            }
        except (OSError, ValueError):
            pass
        return record

    def sampler(self):
        while not self.done.wait(SAMPLE_SECS):
            record = self.sample()
            self.append("samples.jsonl", record)
            if record["freeBytes"] < self.args.min_free_gb * 2**30 and not self.failure:
                self.failure = f"free disk below {self.args.min_free_gb} GiB"

    def prober(self):
        """One probe every PROBE_SECS while a phase allows it; a failed probe is recorded, not fatal."""
        due = time.monotonic()
        while not self.done.wait(1):
            if not self.probing.is_set() or time.monotonic() < due:
                continue
            due = time.monotonic() + (30 if self.args.quick else PROBE_SECS)  # as plan.json records
            self.load("probe", "probes.jsonl", fatal=False)

    # -------------------------------------------------------------- loads --

    def load(self, mode, out, fatal=True, **env):
        """Runs load.ts to the end, appending its lines to `out`; a failure is a stop rule if `fatal`."""
        env = {key.upper(): str(value) for key, value in env.items()}
        with (self.work / out).open("a") as stdout, (self.work / "load.err").open("a") as stderr:
            process = subprocess.Popen(
                ["bun", "load.ts", mode], cwd=HERE, env=dict(self.env, PHASE=self.phase, **env), stdout=stdout, stderr=stderr
            )
            while process.poll() is None:
                if self.failure or (self.node and self.node.poll() is not None):
                    process.terminate()
                time.sleep(0.5)
        if self.node.poll() is not None:
            raise Stop(f"the node exited with {self.node.returncode} during {self.phase}")
        if self.failure:
            raise Stop(self.failure)
        if process.returncode != 0:
            if fatal:
                raise Stop(f"{mode} failed in {self.phase}; see load.err")
            self.append("probe-failures.jsonl", {"t": time.time(), "phase": self.phase, "exit": process.returncode})

    def enter(self, phase):
        self.phase = phase
        self.status("running")
        print(f"{datetime.now():%H:%M:%S} {phase}", flush=True)

    # ---------------------------------------------------------------- run --

    def run(self):
        a = self.args
        minutes = lambda value: int(value * 60)
        self.genesis()
        self.plan()
        self.start_node()
        threading.Thread(target=self.sampler, daemon=True).start()
        threading.Thread(target=self.prober, daemon=True).start()

        self.enter("cohort")
        self.load("cohort", "writes.jsonl", lifetime_blocks=self.growth_lifetime)

        if a.sourcify_keys and a.mutate_min > 0:
            self.probing.set()
            self.enter("mutate-verifiedat")
            self.load("mutate", "writes.jsonl", set="verifiedat", duration_s=minutes(a.mutate_min / 3))
            self.enter("mutate-reverify")
            self.load("mutate", "writes.jsonl", set="reverify", duration_s=minutes(2 * a.mutate_min / 3), offset=100_000)

        self.enter("growth")
        self.probing.set()
        self.load("write", "writes.jsonl", tag="growth", mix="create", duration_s=minutes(a.growth_min),
                  lifetime_blocks=self.growth_lifetime)  # fmt: skip

        self.enter("churn")
        self.load("write", "writes.jsonl", tag="churn", mix="churn", duration_s=minutes(a.churn_min),
                  lifetime_blocks=self.churn_lifetime)  # fmt: skip
        self.probing.clear()

        self.enter("readers")
        reading = threading.Thread(
            target=lambda: self.load("readers", "readers.jsonl", fatal=False, duration_s=minutes(a.readers_min))
        )
        reading.start()
        self.load("write", "writes.jsonl", tag="readers", mix="churn", duration_s=minutes(a.readers_min),
                  lifetime_blocks=self.churn_lifetime, rate_eps=self.readers_rate)  # fmt: skip
        reading.join()

        self.enter("restart")
        self.load("probe", "probes.jsonl", fatal=False)
        height = int(json.loads(get(f"{self.url}/signed/v1/status"))["head"]["height"])
        before = rpc(self.url, "eth_getBlockByNumber", hex(height), False)
        self.restart = {"height": height, "stopSeconds": round(self.stop_node(), 2)}
        if a.drop_caches:
            subprocess.run(["sudo", "-n", "sh", "-c", "sync; echo 3 > /proc/sys/vm/drop_caches"], check=True)
        self.restart["readySeconds"] = round(self.start_node(), 2)
        after = rpc(self.url, "eth_getBlockByNumber", hex(height), False)
        same = all(before[field] == after[field] for field in ("hash", "stateRoot"))
        self.restart.update(sameBlock=same, hash=before["hash"], stateRoot=before["stateRoot"])
        if not same:
            raise Stop(f"block {height} differs after the restart")
        self.phase = "after restart"
        self.load("probe", "probes.jsonl", fatal=False)
        self.load("probe", "probes.jsonl", fatal=False)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--work", required=True, help="new directory for the node and the results")
    parser.add_argument("--quick", action="store_true", help="every phase in miniature, about 3 minutes")
    parser.add_argument("--growth-min", type=float, default=60)
    parser.add_argument("--churn-min", type=float, default=40)
    parser.add_argument("--readers-min", type=float, default=15)
    parser.add_argument("--ready-min", type=float, default=60, help="how long a (re)start may take")
    parser.add_argument("--bundle", help="start from this built genesis bundle instead of an empty genesis")
    parser.add_argument("--prebuilt-state", help="the bundle's prebuilt state (stream-build --state-out), installed on the first start")
    parser.add_argument("--sourcify-keys", help="JSON list of dataset contract keys the soak account owns")
    parser.add_argument("--mutate-min", type=float, default=60, help="minutes of patching dataset contracts (with --sourcify-keys)")
    parser.add_argument("--block-ms", type=int, default=250)
    parser.add_argument("--port", type=int, default=18700, help="RPC port; the next two are also used")
    parser.add_argument("--min-free-gb", type=int, default=100)
    parser.add_argument("--drop-caches", action="store_true", help="drop the page cache before the restart (needs sudo)")
    args = parser.parse_args()
    if args.quick:
        args.growth_min, args.churn_min, args.readers_min, args.mutate_min = 1, 1, 0.5, 1.5

    soak = Soak(args)
    if not soak.binary.is_file() or not (HERE / "node_modules").is_dir() or not shutil.which("bun"):
        print("needs a release build of arkiv-proposer, Bun, and `bun install` in scripts/soak", file=sys.stderr)
        return 2
    try:
        soak.work.mkdir(parents=True, exist_ok=False)
    except FileExistsError:
        print(f"{soak.work} exists; a soak starts from an empty directory", file=sys.stderr)
        return 2
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(1))

    state = "failed"
    try:
        soak.run()
        state = "completed"
    except Stop as stop:
        soak.failure = str(stop)
        print(f"stopped: {stop}", file=sys.stderr)
    except BaseException as error:
        soak.failure = f"harness: {error!r}"
        raise
    finally:
        soak.done.set()
        try:
            soak.stop_node()
        finally:
            soak.status(state)
    subprocess.run([sys.executable, str(HERE / "report.py"), str(soak.work)])
    return 0 if state == "completed" else 1


if __name__ == "__main__":
    sys.exit(main())
