#!/usr/bin/env python3
"""Write report.md for a soak directory (scripts/soak/run.py); safe to rerun, also on a partial run.

    scripts/soak/report.py DIR [--rows 24]
"""

import json
import statistics
import sys
from pathlib import Path

COMMIT = 'db_operation_duration_seconds_{}{{database="state",layer="store",operation="commit"}}'


def lines(path):
    if not path.exists():
        return []
    records = []
    for line in path.read_text().splitlines():
        try:
            records.append(json.loads(line))
        except ValueError:
            pass  # a line cut short when the run stopped
    return records


def table(header, rows):
    out = ["| " + " | ".join(header) + " |", "| " + " | ".join("---" for _ in header) + " |"]
    out += ["| " + " | ".join("" if cell is None else str(cell) for cell in row) + " |" for row in rows]
    return "\n".join(out) + "\n"


def fmt(value, digits=1):
    return None if value is None else f"{value:,.{digits}f}"


def gib(value):
    return None if value is None else f"{value / 2**30:.2f}"


def rate(later, earlier, key, per):
    """Change of a counter between two samples, per unit of `per`."""
    try:
        span = later[per] - earlier[per] if isinstance(per, str) else per(later) - per(earlier)
        return (key(later) - key(earlier)) / span if span > 0 else None
    except (KeyError, TypeError):
        return None


def main():
    work = Path(sys.argv[1])
    rows_wanted = int(sys.argv[sys.argv.index("--rows") + 1]) if "--rows" in sys.argv else 24
    plan = json.loads((work / "plan.json").read_text())
    status = json.loads((work / "status.json").read_text()) if (work / "status.json").exists() else {}
    writes, samples = lines(work / "writes.jsonl"), lines(work / "samples.jsonl")
    probes, readers = lines(work / "probes.jsonl"), lines(work / "readers.jsonl")
    origin = min([r["t"] for r in writes + samples] or [0])
    minute = lambda record: (record["t"] - origin) / 60
    host = plan["host"]

    out = [f"# Node soak, started {plan['started']}\n"]
    out.append(
        f"- Commit `{plan['commit'][:7]}`{' (with uncommitted changes)' if plan['dirty'] else ''}, "
        f"binary SHA-256 `{plan['binarySha256'][:16]}…`\n"
        f"- Host: {host['cpu']}, {host['cores']} cores, {host['memoryGiB']} GiB, kernel {host['kernel']}, "
        f"{host['filesystem']}; load {host['load1']:.2f} at the start\n"
        f"- {plan['blockMs']} ms blocks; batches of {plan['workload']['batch']} operations, "
        f"{plan['workload']['wave']} transactions in flight; SDK {plan['workload']['sdk']['@arkiv-network/sdk']}\n"
        f"- Result: **{status.get('state', 'unknown')}**"
        + (f", stopped in `{status.get('phase')}`: {status['failure']}" if status.get("failure") else "")
        + "\n"
    )

    # ------------------------------------------------------------ phases --
    out.append("## Phases\n")
    rows = []
    for phase in dict.fromkeys(r["phase"] for r in writes):
        windows = [r for r in writes if r["phase"] == phase]
        seconds = sum(r["seconds"] for r in windows)
        ops = sum(r["creates"] + r["patches"] + r["deletes"] + r["extensions"] for r in windows)
        creates, blocks = sum(r["creates"] for r in windows), sum(r["blocks"] for r in windows)
        p95 = [r["txMsP95"] for r in windows if r["txMsP95"] is not None]
        rows.append([
            phase, fmt(seconds / 60), f"{creates:,}", f"{ops:,}", fmt(creates / seconds), fmt(ops / seconds),
            fmt(ops / blocks) if blocks else None,
            fmt(statistics.median(r["txMsP50"] for r in windows if r["txMsP50"] is not None), 0),
            fmt(statistics.median(p95), 0), fmt(max(p95), 0),
            fmt(sum(int(r["gas"]) for r in windows) / ops, 0) if ops else None,
        ])  # fmt: skip
    out.append(table(
        ["Phase", "Minutes", "Created", "Operations", "Created/s", "Operations/s", "Operations/block",
         "Tx p50 ms (median window)", "Tx p95 ms (median window)", "Tx p95 ms (worst window)", "Gas/operation"],
        rows,
    ))  # fmt: skip

    # ---------------------------------------------------------- mutation --
    mutating = [r for r in writes if r["phase"].startswith("mutate")]
    if mutating:
        out.append("## Patching dataset contracts\n")
        out.append(
            "Each patch first reads the contract. `verifiedat` sets one attribute with a unique value; "
            "`reverify` also flips `match` and rewrites `chainid`, `language`, `evmversion`, "
            "`compilerversion` and `optimizer`, whose values millions of contracts share.\n"
        )
        rows = []
        for phase in dict.fromkeys(r["phase"] for r in mutating):
            windows = [r for r in mutating if r["phase"] == phase]
            seconds = sum(r["seconds"] for r in windows)
            patches = sum(r["patches"] for r in windows)
            median = lambda key: statistics.median([r[key] for r in windows if r.get(key) is not None] or [0])
            rows.append([
                phase, fmt(seconds / 60), f"{patches:,}", fmt(patches / seconds) if seconds else None,
                fmt(median("txMsP50"), 0), fmt(median("txMsP95"), 0),
                fmt(max(r["txMsP95"] for r in windows if r["txMsP95"] is not None), 0),
                fmt(median("readMsP50")), fmt(median("readMsP95")),
                f"{sum(r.get('readsRefused', 0) for r in windows):,}",
                fmt(sum(int(r["gas"]) for r in windows) / patches, 0) if patches else None,
            ])  # fmt: skip
        out.append(table(
            ["Phase", "Minutes", "Patches", "Patches/s", "Tx p50 ms", "Tx p95 ms", "Tx p95 ms (worst)",
             "Read p50 ms", "Read p95 ms", "Reads refused", "Gas/patch"],
            rows,
        ))  # fmt: skip

    # --------------------------------------------------------- over time --
    out.append("## Over time\n")
    out.append(
        "Each row covers the interval since the row above and names the phase running at its end. "
        "Created is cumulative; files are allocated bytes; commit is the mean wall time of a "
        "state-store commit, from the node's metrics; CPU is of one core.\n"
    )
    total = minute(samples[-1]) if samples else 0
    step = max(1, round(total / rows_wanted))
    rows, previous, created = [], samples[0] if samples else None, 0
    edge = step
    for sample in samples[1:]:
        if minute(sample) < edge and sample is not samples[-1]:
            continue
        window = [r for r in writes if previous["t"] < r["t"] <= sample["t"]]
        created += sum(r["creates"] for r in window)
        seconds = sum(r["seconds"] for r in window)
        files = sample.get("files", {})
        metric = lambda name: (lambda s: s["metrics"][name])
        commit = rate(sample, previous, metric(COMMIT.format("sum")), metric(COMMIT.format("count")))
        cpu = rate(sample, previous, lambda s: s["cpuSeconds"], "t")
        p95 = [r["txMsP95"] for r in window if r["txMsP95"] is not None]
        rows.append([
            fmt(minute(sample), 0), sample["phase"], f"{sample['head']:,}" if "head" in sample else None,
            f"{created:,}", fmt(sum(r["creates"] for r in window) / seconds) if seconds else None,
            fmt(max(p95), 0) if p95 else None,
            fmt(sample.get("rssBytes", 0) / 2**20, 0), fmt(cpu * 100, 0) if cpu is not None and cpu >= 0 else None,
            gib(files.get("node.redb", {}).get("allocated")), gib(files.get("history.redb", {}).get("allocated")),
            fmt(commit * 1000, 2) if commit is not None else None,
        ])  # fmt: skip
        previous = sample
        edge += step
    out.append(table(
        ["Minute", "Phase then", "Head", "Created", "Created/s", "Tx p95 ms (worst)", "RSS MiB", "CPU %",
         "node.redb GiB", "history.redb GiB", "Commit ms"],
        rows,
    ))  # fmt: skip

    growth = [s for s in samples if s["phase"] == "growth" and "node.redb" in s.get("files", {})]
    grown = sum(r["creates"] for r in writes if r["phase"] == "growth")
    if len(growth) > 1 and grown:
        delta = lambda name: growth[-1]["files"][name]["allocated"] - growth[0]["files"][name]["allocated"]
        out.append(
            f"During growth the state file grew by {delta('node.redb') / grown:,.0f} allocated bytes per entity "
            f"created and the history file by {delta('history.redb') / grown:,.0f}.\n"
        )

    # ------------------------------------------------------------- reads --
    if probes:
        names = list(probes[0]["reads"])
        out.append("## Read probes\n")
        out.append(
            f"p50 / p95 in milliseconds of {plan['workload']['probeReads']} requests one at a time, while the "
            "phase's writes continue. All but the last two read the fixed 2,000-entity cohort.\n"
        )
        cell = lambda probe, name: (
            f"{probe['reads'][name]['p50Ms']} / {probe['reads'][name]['p95Ms']}" if name in probe["reads"] else None
        )
        out.append(table(
            ["Minute", "Phase", "Head"] + names,
            [[fmt(minute(p), 0), p["phase"], f"{p['head']:,}"] + [cell(p, n) for n in names] for p in probes],
        ))  # fmt: skip
        out.append(f"Requests per second from {probes[0]['conc']} concurrent callers (refused with ServerBusy):\n")
        burst = lambda probe, name: (
            f"{probe['reads'][name]['reqPerSec']:,} ({probe['reads'][name]['refused']})" if name in probe["reads"] else None
        )
        out.append(table(
            ["Minute", "Phase"] + names,
            [[fmt(minute(p), 0), p["phase"]] + [burst(p, n) for n in names] for p in probes],
        ))  # fmt: skip
    failed = len(lines(work / "probe-failures.jsonl"))
    if failed:
        out.append(f"{failed} probe or reader runs failed; see `load.err`.\n")

    if readers:
        out.append("## Readers beside writes\n")
        served, refused = sum(r["served"] for r in readers), sum(r["refused"] for r in readers)
        seconds = readers[-1]["t"] - readers[0]["t"] + 10
        out.append(
            f"{readers[0]['conc']} callers: {served:,} reads served ({served / seconds:,.0f} per second), "
            f"{refused:,} refused with ServerBusy ({100 * refused / max(1, served + refused):.2f}%).\n"
        )
        names = sorted({name for r in readers for name in r["reads"]})
        each = lambda name, key: [r["reads"][name][key] for r in readers if r["reads"].get(name, {}).get(key) is not None]
        out.append(table(
            ["Read", "p50 ms (median window)", "p95 ms (median window)", "p95 ms (worst window)"],
            [[n, fmt(statistics.median(each(n, "p50Ms"))), fmt(statistics.median(each(n, "p95Ms"))),
              fmt(max(each(n, "p95Ms")))] for n in names if each(n, "p50Ms")],
        ))  # fmt: skip

    restart = status.get("restart") or {}
    if restart:
        out.append("## Restart\n")
        out.append(
            f"Stopped at height {restart.get('height'):,} in {restart.get('stopSeconds')} s"
            + (" and dropped the page cache" if plan.get("dropCaches") else "")
            + f"; ready again in {restart.get('readySeconds')} s; block {restart.get('height'):,} "
            + ("has the same hash and state root" if restart.get("sameBlock") else "**differs**")
            + ". The probes before and after are the last rows of the tables above.\n"
        )

    (work / "report.md").write_text("\n".join(out))
    print(work / "report.md")


if __name__ == "__main__":
    main()
