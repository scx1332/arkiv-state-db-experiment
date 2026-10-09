#!/usr/bin/env python3
"""Parse one bench-compare run directory into a JSON object (one line on stdout).

A run directory holds job.json, out.log (harness or bench_readers output),
time.txt (/usr/bin/time -v), exit, wall_secs, disk_bytes and meta.txt.
Usable as a module: parse(rundir) -> dict.
"""

import json
import os
import re
import sys

OPS_RE = re.compile(
    r"^\s+(\d+) ops/s\s+\((\d+) ops in (\d+) commits, ([\d.]+)s[^)]*\)\s+disk (\d+) MiB"
)
LAT_HEADER_RE = re.compile(r"^\s+call\s+count\s+p50 ms\s+p99 ms\s+max ms")
LAT_ROW_RE = re.compile(r"^\s{2}(\S.*?)\s+(\d+)\s+([\d.]+|NaN)\s+([\d.]+|NaN)\s+([\d.]+|NaN)\s*$")
LOST_RE = re.compile(r"lost races: (\d+) of (\d+) branch attempts")
PRELOAD_RE = re.compile(r"preloaded (\d+) records in ([\d.]+)s")
BACKEND_RE = re.compile(r"^\s+backend: (.*)$")


def read(path, default=""):
    try:
        with open(path, encoding="utf-8", errors="replace") as f:
            return f.read()
    except OSError:
        return default


def num(text):
    try:
        value = float(text)
    except (TypeError, ValueError):
        return None
    if value != value:  # NaN
        return None
    return value


def parse_time(text):
    out = {}
    for line in text.splitlines():
        line = line.strip()
        if line.startswith("Maximum resident set size (kbytes):"):
            out["peak_rss_kib"] = int(line.rsplit(":", 1)[1])
        elif line.startswith("User time (seconds):"):
            out["user_secs"] = num(line.rsplit(":", 1)[1])
        elif line.startswith("System time (seconds):"):
            out["sys_secs"] = num(line.rsplit(":", 1)[1])
        elif line.startswith("Percent of CPU this job got:"):
            out["cpu_percent"] = num(line.rsplit(":", 1)[1].strip().rstrip("%"))
        elif line.startswith("Command terminated by signal"):
            out["signal"] = line
    return out


def parse_harness(text):
    out = {}
    lat = {}
    in_lat = False
    for line in text.splitlines():
        m = OPS_RE.match(line)
        if m and "ops_per_s" not in out:
            out.update(
                ops_per_s=int(m.group(1)),
                ops=int(m.group(2)),
                commits=int(m.group(3)),
                measured_secs=float(m.group(4)),
                harness_disk_mib=int(m.group(5)),
            )
            continue
        m = BACKEND_RE.match(line)
        if m:
            out["backend"] = m.group(1)
        m = PRELOAD_RE.search(line)
        if m:
            out["preload_secs"] = float(m.group(2))
        m = LOST_RE.search(line)
        if m:
            out["lost_races"] = int(m.group(1))
            out["branch_attempts"] = int(m.group(2))
        if LAT_HEADER_RE.match(line):
            in_lat = True
            continue
        if in_lat:
            m = LAT_ROW_RE.match(line)
            if m:
                lat[m.group(1)] = {
                    "count": int(m.group(2)),
                    "p50_ms": num(m.group(3)),
                    "p99_ms": num(m.group(4)),
                    "max_ms": num(m.group(5)),
                }
            else:
                in_lat = False
    if lat:
        out["latency"] = lat
        for op in ("seal", "commit"):
            if op in lat:
                out[f"{op}_p50_ms"] = lat[op]["p50_ms"]
                out[f"{op}_p99_ms"] = lat[op]["p99_ms"]
    return out


def parse_readers(text):
    out = {}
    for line in text.splitlines():
        if line.startswith("RESULT "):
            for item in line[len("RESULT "):].split():
                if "=" in item:
                    k, v = item.split("=", 1)
                    n = num(v)
                    out[k] = n if n is not None else (None if v == "NaN" else v)
    return out


def parse(rundir):
    job = {}
    try:
        job = json.loads(read(os.path.join(rundir, "job.json"), "{}") or "{}")
    except json.JSONDecodeError:
        pass
    rec = dict(job)
    rec["rundir"] = rundir
    exit_text = read(os.path.join(rundir, "exit")).strip()
    rec["exit_code"] = int(exit_text) if exit_text.lstrip("-").isdigit() else None
    rec["status"] = "ok" if os.path.exists(os.path.join(rundir, "done")) else "failed"
    failed = read(os.path.join(rundir, "failed")).strip()
    if failed:
        rec["failure"] = failed
    rec["wall_secs"] = num(read(os.path.join(rundir, "wall_secs")).strip())
    disk = read(os.path.join(rundir, "disk_bytes")).strip()
    rec["disk_bytes"] = int(disk) if disk.isdigit() else None
    rec.update(parse_time(read(os.path.join(rundir, "time.txt"))))
    meta = read(os.path.join(rundir, "meta.txt"))
    m = re.search(r"== loadavg\n(\S+) (\S+) (\S+)", meta)
    if m:
        rec["load1_start"], rec["load5_start"] = num(m.group(1)), num(m.group(2))
    env = read(os.path.join(rundir, "env.txt")).split()
    rec["env"] = [e for e in env if e]
    text = read(os.path.join(rundir, "out.log"))
    if rec.get("variant") == "readers":
        # bench_readers removes its directory itself and reports disk_bytes.
        rec.update(parse_readers(text))
    else:
        rec.update(parse_harness(text))
    if rec["status"] == "ok" and rec.get("ops_per_s") is None and rec.get("producer_ops_per_s") is None:
        rec["status"] = "unparsed"
    if rec["status"] != "ok":
        tail = [line for line in text.splitlines() if line.strip()][-5:]
        rec["log_tail"] = tail
    return rec


def main():
    if len(sys.argv) != 2:
        print("usage: parse_run.py RUNDIR", file=sys.stderr)
        return 2
    print(json.dumps(parse(sys.argv[1]), sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(main())
