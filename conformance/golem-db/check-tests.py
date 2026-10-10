#!/usr/bin/env python3
"""Run the ported golem-db tests and compare the outcome with a known list.

Usage (from the harness checkout):

    python3 check-tests.py --expected-failures expected-failures.txt \
        --expected-total 21 -- \
        cargo test -p golemdb-harness golem_tests --no-fail-fast -- --test-threads=1

The command's output is passed through unchanged, then every libtest line
`test <name> ... ok|FAILED|ignored` is collected. The check fails when:

- a test outside the expected-failures list fails;
- a listed test passes (or no longer exists), so the list stays current;
- the number of tests run differs from --expected-total (in particular zero,
  which would mean the filter matched nothing);
- the command failed without any failing test (a build error, a crash).
"""

import argparse
import re
import subprocess
import sys

TEST_LINE = re.compile(r"^test (\S+) \.\.\. (ok|FAILED|ignored)\b")


def read_list(path):
    names = []
    with open(path, encoding="utf-8") as f:
        for line in f:
            line = line.split("#", 1)[0].strip()
            if line:
                names.append(line)
    if len(names) != len(set(names)):
        sys.exit(f"error: duplicate names in {path}")
    return set(names)


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--expected-failures", required=True)
    parser.add_argument("--expected-total", type=int, required=True)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if not command:
        parser.error("missing command after --")

    expected = read_list(args.expected_failures)

    proc = subprocess.Popen(
        command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, bufsize=1
    )
    results = {}
    for line in proc.stdout:
        sys.stdout.write(line)
        m = TEST_LINE.match(line.rstrip("\n"))
        if m:
            name, status = m.groups()
            if name in results:
                sys.exit(f"error: test {name} reported twice")
            results[name] = status
    code = proc.wait()

    passed = sorted(n for n, s in results.items() if s == "ok")
    failed = sorted(n for n, s in results.items() if s == "FAILED")
    ignored = sorted(n for n, s in results.items() if s == "ignored")

    unexpected_failures = [n for n in failed if n not in expected]
    now_passing = sorted(n for n in expected if results.get(n) == "ok")
    missing = sorted(n for n in expected if n not in results)
    total = len(passed) + len(failed)

    problems = []
    if total != args.expected_total:
        problems.append(f"ran {total} tests, expected {args.expected_total}")
    if unexpected_failures:
        problems.append("unexpected failures: " + ", ".join(unexpected_failures))
    if now_passing:
        problems.append(
            "listed as expected failures but passed (remove them from the list): "
            + ", ".join(now_passing)
        )
    if missing:
        problems.append("listed as expected failures but not run: " + ", ".join(missing))
    if ignored:
        problems.append("ignored tests: " + ", ".join(ignored))
    if code != 0 and not failed:
        problems.append(f"command exited with {code} without a failing test")
    if code == 0 and failed:
        problems.append("command exited with 0 despite failing tests")

    print()
    print("=== golem-db ported tests summary ===")
    print(f"run: {total}  passed: {len(passed)}  failed: {len(failed)}  "
          f"(expected failures: {len(expected)})")
    for name in failed:
        tag = "expected" if name in expected else "UNEXPECTED"
        print(f"  FAILED ({tag}): {name}")
    if problems:
        print("RESULT: FAIL")
        for p in problems:
            print(f"  - {p}")
        return 1
    print("RESULT: OK (only the listed tests fail)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
