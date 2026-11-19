#!/usr/bin/env python3
"""Render the Node Parity Board's rows as one static HTML completion report.

    scripts/board-report.py EXPORT_DIR OUT.html

EXPORT_DIR holds the board's database as the ArtifactData tool exports it
(`out_dir`): `board/meta.json` and `features/*.json`, one feature per file (see
docs/board/README.md). The page needs no script and loads nothing from
elsewhere, so it can be served as a plain file.
"""

import html
import json
import sys
from datetime import datetime, timezone
from pathlib import Path

STATES = [
    ("done", "Done"),
    ("partial", "Partial"),
    ("in-progress", "In progress"),
    ("planned", "Planned"),
    ("missing", "Missing"),
    ("na", "Not applicable"),
    ("unknown", "Unknown"),
]
LABEL = dict(STATES)
WEIGHT = {"done": 1, "partial": 0.5, "in-progress": 0.5}
DEFAULT_SUBJECTS = [
    {"id": "ours", "name": "Our node", "detail": "arkiv-state-db-experiment"},
    {"id": "arkiv", "name": "Arkiv node", "detail": "Arkiv-Network/arkiv"},
]

CSS = """
:root{--bg:#f5f6f8;--surface:#fff;--ink:#161b26;--muted:#5b6475;--rule:#dde1e8;--accent:#2d4fc4;--accent-soft:#e6ebfa;
--done:#1f7a4d;--done-bg:#e2f3ea;--partial:#9a6300;--partial-bg:#fbefd7;--in-progress:#2d4fc4;--in-progress-bg:#e6ebfa;
--planned:#6a4fb3;--planned-bg:#eee9fa;--missing:#b3261e;--missing-bg:#fbe4e2;--na:#6b7280;--na-bg:#eceef1;--unknown:#8a8f99;--unknown-bg:#f0f1f3;
--display:"Archivo","Helvetica Neue",Arial,sans-serif;--body:"Instrument Sans","Segoe UI",system-ui,sans-serif;--mono:"JetBrains Mono",ui-monospace,Menlo,monospace}
@media (prefers-color-scheme:dark){:root{--bg:#0f131b;--surface:#171c27;--ink:#e7eaf0;--muted:#9aa3b5;--rule:#2a3142;--accent:#8ea7ff;--accent-soft:#1e2a4d;
--done:#6fd29c;--done-bg:#143426;--partial:#f0b85a;--partial-bg:#3a2c12;--in-progress:#8ea7ff;--in-progress-bg:#1e2a4d;--planned:#b9a3f5;--planned-bg:#2b2346;
--missing:#ff8f86;--missing-bg:#3e1a18;--na:#9aa3b5;--na-bg:#232a38;--unknown:#8a93a5;--unknown-bg:#1f2533;color-scheme:dark}}
*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--ink);font:15px/1.5 var(--body)}
.wrap{max-width:1180px;margin:0 auto;padding:28px max(16px,env(safe-area-inset-right,0px)) 64px max(16px,env(safe-area-inset-left,0px));display:grid;gap:24px}
h1,h2,h3{font-family:var(--display);margin:0;text-wrap:balance}h1{font-size:clamp(1.6rem,3vw,2.1rem)}h2{font-size:1.2rem}
.eyebrow{font:500 .72rem var(--mono);letter-spacing:.08em;text-transform:uppercase;color:var(--muted)}
.lede{color:var(--muted);max-width:68ch;margin:6px 0 0}.meta{font:.78rem var(--mono);color:var(--muted)}
.summary{display:grid;grid-template-columns:repeat(auto-fit,minmax(260px,1fr));gap:16px}
.card{background:var(--surface);border:1px solid var(--rule);border-radius:10px;padding:18px;display:grid;gap:10px;min-width:0}
.pct{font:700 2rem var(--display);font-variant-numeric:tabular-nums;line-height:1}.pct small{font-size:.9rem;color:var(--muted);font-weight:500;margin-left:4px}
.bar{display:flex;height:10px;border-radius:5px;overflow:hidden;background:var(--unknown-bg)}.bar span{display:block;height:100%}
.legend{display:flex;flex-wrap:wrap;gap:4px 14px;font-size:.8rem;color:var(--muted);font-variant-numeric:tabular-nums}
.legend i{display:inline-block;width:9px;height:9px;border-radius:2px;margin-right:5px}
.diff{background:var(--accent-soft)}.diff .pct{color:var(--accent)}
.scroll{overflow-x:auto;background:var(--surface);border:1px solid var(--rule);border-radius:10px}
table{border-collapse:collapse;width:100%;font-variant-numeric:tabular-nums}
th,td{text-align:left;padding:10px 14px;border-top:1px solid var(--rule);vertical-align:top}
thead th{border-top:0;background:var(--bg);font:500 .72rem var(--mono);letter-spacing:.08em;text-transform:uppercase;color:var(--muted)}
tr.area th{background:var(--bg);font:600 .95rem var(--display)}
.name{font-weight:600}.sum{color:var(--muted);font-size:.88rem}.ref,.ev,.chk{font:.72rem var(--mono);color:var(--muted);overflow-wrap:anywhere}
td p{margin:6px 0 0;font-size:.88rem}tr.differs td:first-child{box-shadow:inset 3px 0 0 var(--accent)}
.pill{display:inline-block;font-size:.78rem;font-weight:600;border-radius:999px;padding:2px 10px;white-space:nowrap}
""" + "".join(f".s-{k}{{color:var(--{k});background:var(--{k}-bg)}}" for k, _ in STATES)


def esc(value):
    return html.escape("" if value is None else str(value))


def state_of(feature, subject):
    state = str(((feature.get("status") or {}).get(subject) or {}).get("state", "")).lower()
    return state if state in LABEL else "unknown"


def completion(features, subject):
    scored = [f for f in features if state_of(f, subject) != "na"]
    if not scored:
        return None
    return round(100 * sum(WEIGHT.get(state_of(f, subject), 0) for f in scored) / len(scored))


def bar(features, subject):
    counts = {k: 0 for k, _ in STATES}
    for f in features:
        counts[state_of(f, subject)] += 1
    total = max(1, len(features))
    spans = "".join(
        f'<span style="width:{100 * n / total:.2f}%;background:var(--{k})"></span>' for k, n in counts.items() if n
    )
    legend = "".join(
        f'<span><i style="background:var(--{k})"></i>{LABEL[k]} {n}</span>' for k, n in counts.items() if n
    )
    label = ", ".join(f"{LABEL[k]}: {n}" for k, n in counts.items())
    return f'<div class="bar" role="img" aria-label="{esc(label)}">{spans}</div><div class="legend">{legend}</div>'


def main():
    source, out = Path(sys.argv[1]), Path(sys.argv[2])
    meta_file = source / "board" / "meta.json"
    meta = json.loads(meta_file.read_text()) if meta_file.exists() else {}
    features = []
    for path in sorted((source / "features").glob("*.json")):
        feature = json.loads(path.read_text())
        feature.setdefault("id", path.stem)
        features.append(feature)
    subjects = [s for s in (meta.get("subjects") or DEFAULT_SUBJECTS) if isinstance(s, dict) and s.get("id")][:2]
    order = [str(a) for a in meta.get("areas") or []]
    area = lambda f: str(f.get("area") or "Other")
    rank = lambda a: order.index(a) if a in order else len(order)
    features.sort(key=lambda f: (rank(area(f)), area(f), float(f.get("order") or 0), str(f.get("name") or "")))
    differs = lambda f: len({state_of(f, s["id"]) for s in subjects}) > 1
    areas = list(dict.fromkeys(area(f) for f in features))
    latest = max((str(f.get("updatedAt")) for f in features if f.get("updatedAt")), default="")
    generated = datetime.now(timezone.utc).strftime("%Y-%m-%d %H:%M UTC")
    title = meta.get("title") or "Node Parity Board"

    cards = []
    for s in subjects:
        pct = completion(features, s["id"])
        cards.append(
            f'<article class="card"><div><h2>{esc(s.get("name") or s["id"])}</h2><div class="ref">{esc(s.get("detail"))}</div></div>'
            f'<div class="pct">{"–" if pct is None else f"{pct}%"}<small>complete</small></div>{bar(features, s["id"])}</article>'
        )
    if len(subjects) == 2:
        cards.append(
            f'<article class="card diff"><div class="eyebrow">Where they differ</div>'
            f'<div class="pct">{sum(map(differs, features))}<small>of {len(features)} features</small></div>'
            f'<div class="legend">Marked at the left edge of the table below.</div></article>'
        )

    by_area = ['<div class="scroll"><table><thead><tr><th>Area</th><th>Features</th>'
               + "".join(f"<th>{esc(s.get('name') or s['id'])}</th>" for s in subjects)
               + "<th>Differ</th></tr></thead><tbody>"]  # fmt: skip
    for a in areas:
        rows = [f for f in features if area(f) == a]
        cells = "".join(
            f"<td>{'–' if completion(rows, s['id']) is None else str(completion(rows, s['id'])) + '%'}</td>" for s in subjects
        )
        by_area.append(f"<tr><td>{esc(a)}</td><td>{len(rows)}</td>{cells}<td>{sum(map(differs, rows))}</td></tr>")
    by_area.append("</tbody></table></div>")

    detail = ['<div class="scroll"><table><thead><tr><th>Feature</th>'
              + "".join(f"<th>{esc(s.get('name') or s['id'])}</th>" for s in subjects) + "</tr></thead><tbody>"]  # fmt: skip
    for a in areas:
        detail.append(f'<tr class="area"><th colspan="{1 + len(subjects)}">{esc(a)}</th></tr>')
        for f in (f for f in features if area(f) == a):
            head = f'<div class="name">{esc(f.get("name") or f["id"])}</div>'
            if f.get("summary"):
                head += f'<div class="sum">{esc(f["summary"])}</div>'
            if f.get("reference"):
                head += f'<div class="ref">{esc(f["reference"])}</div>'
            cells = ""
            for s in subjects:
                st = (f.get("status") or {}).get(s["id"]) or {}
                k = state_of(f, s["id"])
                cell = f'<span class="pill s-{k}">{LABEL[k]}</span>'
                if st.get("note"):
                    cell += f"<p>{esc(st['note'])}</p>"
                if st.get("evidence"):
                    cell += f'<div class="ev">{esc(st["evidence"])}</div>'
                checked = " · ".join(str(x) for x in (st.get("checkedBy"), st.get("checkedAt")) if x)
                if checked:
                    cell += f'<div class="chk">Checked {esc(checked)}</div>'
                cells += f"<td>{cell}</td>"
            detail.append(f'<tr class="{"differs" if differs(f) else ""}"><td>{head}</td>{cells}</tr>')
    detail.append("</tbody></table></div>")

    page = f"""<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1,viewport-fit=cover">
<meta name="robots" content="noindex,nofollow,noarchive"><meta name="referrer" content="no-referrer">
<title>{esc(title)}</title>
<link rel="stylesheet" href="https://fonts.googleapis.com/css2?family=Archivo:wght@600;700&family=Instrument+Sans:wght@400;600&family=JetBrains+Mono:wght@400;500&display=swap">
<style>{CSS}</style></head><body><div class="wrap">
<header><div class="eyebrow">Feature completion report</div><h1>{esc(title)}</h1>
<p class="lede">{esc(meta.get("description") or "What each node implements, feature by feature.")}</p>
<div class="meta">{len(features)} features · generated {generated}{" · last change " + esc(latest) if latest else ""}</div></header>
<section class="summary">{"".join(cards)}</section>
<section><h2>By area</h2><p class="lede">Completion counts done as 1 and partial or in progress as half; not applicable is left out.</p>{"".join(by_area)}</section>
<section><h2>Features</h2>{"".join(detail)}</section>
</div></body></html>
"""
    out.write_text(page)
    print(f"{out}: {len(features)} features")


if __name__ == "__main__":
    main()
