#!/usr/bin/env bash
# Runs one check of .github/workflows/security.yml (ported from
# golemfactory/golem-p2p harness/security-check.sh). The output goes to the
# log, and, when $GITHUB_STEP_SUMMARY is set, into a section of the run
# summary with the result. Exits with the check's status, or 0 for an
# informational check.
#
# Usage: scripts/security/check.sh [--informational] TITLE -- COMMAND...
set -uo pipefail

informational=false
if [ "${1:-}" = --informational ]; then
  informational=true
  shift
fi
if [ $# -lt 3 ] || [ "$2" != -- ]; then
  echo "usage: $0 [--informational] TITLE -- COMMAND..." >&2
  exit 2
fi
title=$1
shift 2

log=$(mktemp)
trap 'rm -f "$log"' EXIT
"$@" 2>&1 | tee "$log"
status=${PIPESTATUS[0]}

if [ "$status" -eq 0 ]; then
  result="passed"
elif $informational; then
  result="findings (informational, does not fail the job)"
else
  result="FAILED (exit status $status)"
fi

if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
  lines=$(wc -l < "$log")
  {
    printf '### %s: %s\n\n' "$title" "$result"
    last=$(grep -v '^[[:space:]]*$' "$log" | tail -n 1 | cut -c 1-200)
    [ -n "$last" ] && printf '> %s\n\n' "${last//</&lt;}"
    printf '<details><summary><code>%s</code> (%s lines of output' "$*" "$lines"
    [ "$lines" -gt 300 ] && printf ', last 300 shown'
    printf ')</summary>\n\n```text\n'
    # Keep the output inside the code block.
    # shellcheck disable=SC2016 # literal backticks, nothing to expand
    tail -n 300 "$log" | sed 's/```/` ` `/g'
    printf '```\n\n</details>\n\n'
  } >> "$GITHUB_STEP_SUMMARY"
fi

# One JSON object per check (kept for a later model-based scan report).
if [ -n "${SECURITY_CHECK_RESULTS:-}" ]; then
  python3 - "$SECURITY_CHECK_RESULTS" "$title" "$*" "$status" "$informational" "$log" <<'PY'
import json, sys
path, title, command, status, informational, log = sys.argv[1:]
with open(log, encoding="utf-8", errors="replace") as f:
    lines = f.read().splitlines()
nonblank = [line for line in lines if line.strip()]
record = {
    "title": title,
    "command": command,
    "exit_code": int(status),
    "informational": informational == "true",
    "last_line": nonblank[-1][:200] if nonblank else "",
    "output_tail": "\n".join(lines[-60:]),
}
with open(path, "a", encoding="utf-8") as f:
    f.write(json.dumps(record) + "\n")
PY
fi

if $informational; then
  exit 0
fi
exit "$status"
