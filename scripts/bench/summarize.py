#!/usr/bin/env python3
"""Build results.jsonl and summary.md for a bench-compare results directory.

Usage: summarize.py OUT_DIR

Re-parses every run directory under OUT_DIR/runs (so it is idempotent and
works on partial or resumed results), rewrites OUT_DIR/results.jsonl and
writes OUT_DIR/summary.md. A difference is marked significant only when the
min..max ranges of the two engines do not overlap.
"""

import json
import os
import statistics
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import parse_run  # noqa: E402

ENGINES = ["golem", "arkiv-old", "arkiv-new", "arkiv-mem"]


def med(values):
    values = [v for v in values if v is not None]
    return statistics.median(values) if values else None


def lo(values):
    values = [v for v in values if v is not None]
    return min(values) if values else None


def hi(values):
    values = [v for v in values if v is not None]
    return max(values) if values else None


def fmt(value, digits=0):
    if value is None:
        return "–"
    if digits == 0:
        return f"{value:,.0f}"
    return f"{value:,.{digits}f}"


def mib(value):
    return None if value is None else value / (1 << 20)


def compare(a_vals, b_vals):
    """Ratio of medians a/b and whether the min..max ranges are disjoint."""
    a = [v for v in a_vals if v is not None]
    b = [v for v in b_vals if v is not None]
    if not a or not b or statistics.median(b) == 0:
        return "–"
    ratio = statistics.median(a) / statistics.median(b)
    disjoint = max(a) < min(b) or max(b) < min(a)
    if len(a) < 2 or len(b) < 2:
        tag = " (n<2)"
    elif disjoint:
        tag = " **sig**"
    else:
        tag = " (overlap)"
    return f"{ratio:.2f}x{tag}"


def load_records(out):
    runs = os.path.join(out, "runs")
    records = []
    if os.path.isdir(runs):
        for name in sorted(os.listdir(runs)):
            path = os.path.join(runs, name)
            if os.path.isdir(path) and os.path.exists(os.path.join(path, "job.json")):
                records.append(parse_run.parse(path))
    return records


def read(path):
    try:
        with open(path, encoding="utf-8", errors="replace") as f:
            return f.read()
    except OSError:
        return ""


SCENARIO_ORDER = [
    "index-layout/1-no-index",
    "index-layout/2-same-term",
    "index-layout/3-16-terms",
    "index-layout/4-unique-terms",
    "index-layout/5-16-attributes",
    "block-size/1-blocks-of-10",
    "block-size/2-blocks-of-100",
    "block-size/3-blocks-of-1000",
    "block-size/4-blocks-of-5000",
    "commit-growth",
    "concurrent-writers",
    "scaled/large-2m-small",
    "scaled/large-realistic-small",
    "large/large-2m",
    "large/large-realistic",
]


def scenario_rank(scn):
    return (SCENARIO_ORDER.index(scn) if scn in SCENARIO_ORDER else len(SCENARIO_ORDER), scn)


def scenario_order(records):
    return sorted({r.get("scenario") for r in records}, key=scenario_rank)


def main_section(records, lines):
    main = [r for r in records if r.get("variant") == "main"]
    if not main:
        return
    scenarios = scenario_order(main)
    ok = [r for r in main if r["status"] == "ok"]

    def vals(scn, eng, field):
        return [r.get(field) for r in ok if r["scenario"] == scn and r["engine"] == eng]

    lines.append("## Overview (median ops/s)\n")
    lines.append("| scenario | golem | arkiv-old | arkiv-new | arkiv-mem | new/old | new/golem |")
    lines.append("|---|---:|---:|---:|---:|---|---|")
    for scn in scenarios:
        cells = [fmt(med(vals(scn, e, "ops_per_s"))) for e in ENGINES]
        lines.append(
            f"| {scn} | " + " | ".join(cells) + " | "
            + compare(vals(scn, "arkiv-new", "ops_per_s"), vals(scn, "arkiv-old", "ops_per_s"))
            + " | "
            + compare(vals(scn, "arkiv-new", "ops_per_s"), vals(scn, "golem", "ops_per_s"))
            + " |"
        )
    lines.append("")
    lines.append(
        "Ratios are of medians; **sig** = the min..max ranges do not overlap; "
        "(overlap) = not significant.\n"
    )

    lines.append("## Per scenario\n")
    for scn in scenarios:
        lines.append(f"### {scn}\n")
        lines.append(
            "| engine | ok/runs | ops/s median | min | max | seal p50 ms | commit p50 ms "
            "| commit p99 ms | disk MiB | peak RSS MiB |"
        )
        lines.append("|---|---|---:|---:|---:|---:|---:|---:|---:|---:|")
        for eng in ENGINES:
            all_runs = [r for r in main if r["scenario"] == scn and r["engine"] == eng]
            if not all_runs:
                continue
            ops = vals(scn, eng, "ops_per_s")
            rss = [None if v is None else v / 1024 for v in vals(scn, eng, "peak_rss_kib")]
            lines.append(
                f"| {eng} | {len(ops)}/{len(all_runs)} | {fmt(med(ops))} | {fmt(lo(ops))} | "
                f"{fmt(hi(ops))} | {fmt(med(vals(scn, eng, 'seal_p50_ms')), 2)} | "
                f"{fmt(med(vals(scn, eng, 'commit_p50_ms')), 2)} | "
                f"{fmt(med(vals(scn, eng, 'commit_p99_ms')), 2)} | "
                f"{fmt(mib(med(vals(scn, eng, 'disk_bytes'))), 1)} | {fmt(med(rss))} |"
            )
        lines.append("")
        new = vals(scn, "arkiv-new", "ops_per_s")
        lines.append(
            f"- new/old ops/s: {compare(new, vals(scn, 'arkiv-old', 'ops_per_s'))}; "
            f"new/golem: {compare(new, vals(scn, 'golem', 'ops_per_s'))}; "
            f"commit p50 new/old: {compare(vals(scn, 'arkiv-new', 'commit_p50_ms'), vals(scn, 'arkiv-old', 'commit_p50_ms'))}; "
            f"disk new/golem: {compare(vals(scn, 'arkiv-new', 'disk_bytes'), vals(scn, 'golem', 'disk_bytes'))}\n"
        )


def cache_section(records, lines):
    sweep = [r for r in records if str(r.get("variant", "")).startswith("cache-")]
    if not sweep:
        return
    main_new = [
        r for r in records
        if r.get("variant") == "main" and r.get("engine") == "arkiv-new" and r["status"] == "ok"
    ]
    lines.append("## Object cache sweep (arkiv-new)\n")
    lines.append(
        "`ARKIV_OBJECT_CACHE_MIB` with `ARKIV_OBJECT_CACHE_ENTRIES` = 16384 per 8 MiB. "
        "The `default` row is the main matrix's arkiv-new runs.\n"
    )
    lines.append(
        "| scenario | cache | ok/runs | ops/s median | min | max | commit p50 ms | commit p99 ms "
        "| peak RSS MiB | vs default |"
    )
    lines.append("|---|---|---|---:|---:|---:|---:|---:|---:|---|")
    scenarios = sorted({r["scenario"] for r in sweep}, key=scenario_rank)
    variants = sorted({r["variant"] for r in sweep}, key=lambda v: int(v.split("-")[1]))
    for scn in scenarios:
        base = [r for r in main_new if r["scenario"] == scn]
        base_ops = [r.get("ops_per_s") for r in base]
        rows = [("default", base, len(base))]
        for v in variants:
            runs = [r for r in sweep if r["scenario"] == scn and r["variant"] == v]
            rows.append((v.split("-")[1] + " MiB", [r for r in runs if r["status"] == "ok"], len(runs)))
        for label, ok, total in rows:
            if total == 0:
                continue
            ops = [r.get("ops_per_s") for r in ok]
            rss = [None if r.get("peak_rss_kib") is None else r["peak_rss_kib"] / 1024 for r in ok]
            vs = "–" if label == "default" else compare(ops, base_ops)
            lines.append(
                f"| {scn} | {label} | {len(ok)}/{total} | {fmt(med(ops))} | {fmt(lo(ops))} | "
                f"{fmt(hi(ops))} | {fmt(med([r.get('commit_p50_ms') for r in ok]), 2)} | "
                f"{fmt(med([r.get('commit_p99_ms') for r in ok]), 2)} | {fmt(med(rss))} | {vs} |"
            )
    lines.append("")


def readers_section(records, lines, revs):
    rd = [r for r in records if r.get("variant") == "readers"]
    if not rd:
        return
    lines.append("## Reader workload (bench_readers)\n")
    lines.append(
        "One producer commits blocks (3 creates : 1 patch) on a durable redb engine while N "
        "reader threads do point reads of committed records on the latest head. "
        "Producer ops/s is over the measured writes; reads/s is total across readers.\n"
    )
    if revs.get("readers_old") == "0":
        lines.append("- bench_readers did not build at the old revision: old runs are missing.\n")
    if revs.get("old_bench_readers_injected") == "yes":
        lines.append("- The old revision did not carry bench_readers; the runner injected the current copy.\n")
    lines.append(
        "| block ops | readers | producer ops/s old | producer ops/s new | new/old | reads/s old "
        "| reads/s new | new/old | read p99 µs old | read p99 µs new | block p50 ms new |"
    )
    lines.append("|---:|---:|---:|---:|---|---:|---:|---|---:|---:|---:|")
    keys = []
    for r in rd:
        k = r["scenario"]
        if k not in keys:
            keys.append(k)

    def parse_key(k):
        spec = k.split("/", 1)[1]
        b, t = spec.split("-r")
        return int(b.lstrip("b")), int(t)

    keys.sort(key=parse_key)
    for k in keys:
        b, t = parse_key(k)
        ok = {e: [r for r in rd if r["scenario"] == k and r["engine"] == e and r["status"] == "ok"]
              for e in ("arkiv-old", "arkiv-new")}

        def col(e, field):
            return [r.get(field) for r in ok[e]]

        def rng(e, field):
            v = col(e, field)
            if not v:
                return "–"
            return f"{fmt(med(v))} ({fmt(lo(v))}–{fmt(hi(v))})"

        lines.append(
            f"| {b} | {t} | {rng('arkiv-old', 'producer_ops_per_s')} | "
            f"{rng('arkiv-new', 'producer_ops_per_s')} | "
            f"{compare(col('arkiv-new', 'producer_ops_per_s'), col('arkiv-old', 'producer_ops_per_s'))} | "
            f"{rng('arkiv-old', 'reader_reads_per_s') if t else '–'} | "
            f"{rng('arkiv-new', 'reader_reads_per_s') if t else '–'} | "
            f"{compare(col('arkiv-new', 'reader_reads_per_s'), col('arkiv-old', 'reader_reads_per_s')) if t else '–'} | "
            f"{fmt(med(col('arkiv-old', 'read_p99_us')))} | {fmt(med(col('arkiv-new', 'read_p99_us')))} | "
            f"{fmt(med(col('arkiv-new', 'block_p50_ms')), 2)} |"
        )
    lines.append("")


def kv(text):
    out = {}
    for line in text.splitlines():
        if "=" in line:
            k, v = line.split("=", 1)
            out[k.strip()] = v.strip()
    return out


def section(text, name):
    marker = f"== {name}\n"
    if marker not in text:
        return ""
    body = text.split(marker, 1)[1]
    return body.split("\n== ", 1)[0].strip()


def meta_section(out, records, lines, revs):
    lines.append("## Run conditions\n")
    gate = read(os.path.join(out, "meta", "load-gate.txt")).strip() or "(no load gate record)"
    lines.append(f"- Load gate: {gate}")
    for k in ("old_rev", "old_sha", "new_rev", "new_sha", "conformance_rev", "harness_rustc",
              "arkiv_rustc", "redb_version"):
        if k in revs:
            lines.append(f"- {k}: `{revs[k]}`")
    cfg = kv(read(os.path.join(out, "config.env")))
    if cfg:
        lines.append("- config: " + ", ".join(f"{k}={v}" for k, v in sorted(cfg.items())))
    loads = [r.get("load5_start") for r in records if r.get("load5_start") is not None]
    if loads:
        lines.append(
            f"- 5-min load at run starts: min {min(loads):.2f}, median "
            f"{statistics.median(loads):.2f}, max {max(loads):.2f} over {len(loads)} runs"
        )
    status = kv(read(os.path.join(out, "STATUS")))
    if status:
        lines.append(f"- runner status at summary time: phase={status.get('phase')} "
                     f"started={status.get('started')}")
    lines.append("")
    start = read(os.path.join(out, "meta", "machine-start.txt"))
    if start:
        lines.append("### Machine at start\n")
        lines.append("```")
        for name in ("date", "uptime", "uname", "free -m", "df", "top10 by cpu"):
            body = section(start, name)
            if body:
                lines.append(f"== {name}")
                lines.append(body)
        lines.append("```\n")
    failed = [r for r in records if r["status"] != "ok"]
    lines.append(f"## Failures ({len(failed)})\n")
    for r in failed:
        tail = " / ".join(r.get("log_tail", [])[-2:])
        lines.append(
            f"- `{r.get('key')}`: {r.get('failure', r['status'])} (exit {r.get('exit_code')}) {tail}"
        )
    lines.append("")


def main():
    if len(sys.argv) != 2:
        print(__doc__, file=sys.stderr)
        return 2
    out = sys.argv[1]
    records = load_records(out)
    with open(os.path.join(out, "results.jsonl.tmp"), "w", encoding="utf-8") as f:
        for r in records:
            f.write(json.dumps(r, sort_keys=True) + "\n")
    os.replace(os.path.join(out, "results.jsonl.tmp"), os.path.join(out, "results.jsonl"))

    revs = kv(read(os.path.join(out, "meta", "revs.txt")))
    ok = sum(1 for r in records if r["status"] == "ok")
    lines = [
        "# bench-compare summary\n",
        f"Results: `{out}`. {ok} of {len(records)} runs succeeded. "
        f"old = `{revs.get('old_rev', '?')}` ({revs.get('old_sha', '?')[:12]}), "
        f"new = `{revs.get('new_rev', '?')}` ({revs.get('new_sha', '?')[:12]}). "
        "Engines: golem = golem-db on MDBX; arkiv-old / arkiv-new = DurableEngine on redb "
        "(GOLEMDB_BACKEND=arkiv) built from each revision with its own conformance kit; "
        "arkiv-mem = in-memory Engine at the new revision. Disk = `du -sb` of the database "
        "directory after the run.\n",
    ]
    main_section(records, lines)
    cache_section(records, lines)
    readers_section(records, lines, revs)
    meta_section(out, records, lines, revs)
    with open(os.path.join(out, "summary.md.tmp"), "w", encoding="utf-8") as f:
        f.write("\n".join(lines))
    os.replace(os.path.join(out, "summary.md.tmp"), os.path.join(out, "summary.md"))
    print(f"wrote {out}/summary.md ({len(records)} runs)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
