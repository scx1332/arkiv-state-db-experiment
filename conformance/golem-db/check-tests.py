#!/usr/bin/env python3
"""Run a libtest command and compare its outcome with a list of expected failures.

Usage:

    python3 check-tests.py --name "in-repo on arkiv" \
        --expected-failures expected-failures/in-repo.txt \
        --expected-run 100 --expected-ignored 8 -- \
        cargo test -p golemdb-conformance --no-fail-fast

The command's output is passed through unchanged, then every libtest line
`test <name> ... ok|FAILED|ignored` is collected. The check fails when:

- a test outside the expected-failures list fails;
- a listed test passes, or is not run, so the list stays current;
- an entry of the list has no reason (`<test name> <reason>` per line);
- the number of tests run, or ignored, differs from the expected one
  (`any` skips that comparison, but zero tests run always fails);
- the command failed without any failing test (a build error, a crash),
  or succeeded despite failing tests.
"""

import argparse
import collections
import re
import subprocess
import sys

TEST_LINE = re.compile(r"^test (\S+) \.\.\. (ok|FAILED|ignored)\b")


def read_list(path):
    """`{test name: reason}` from `<test name> <reason>` lines; `#` comments."""
    expected = {}
    with open(path, encoding="utf-8") as f:
        for number, line in enumerate(f, 1):
            line = line.split("#", 1)[0].strip()
            if not line:
                continue
            fields = line.split()
            if len(fields) != 2:
                sys.exit(f"error: {path}:{number}: expected `<test name> <reason>`")
            name, reason = fields
            if name in expected:
                sys.exit(f"error: {path}:{number}: {name} is listed twice")
            expected[name] = reason
    return expected


def count(text):
    return None if text == "any" else int(text)


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--name", default="tests")
    parser.add_argument("--expected-failures", required=True)
    parser.add_argument("--expected-run", type=count, required=True,
                        help="tests run (passed + failed), or `any`")
    parser.add_argument("--expected-ignored", type=count, default=0,
                        help="tests ignored, or `any`")
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
    not_run = sorted(n for n in expected if results.get(n) in (None, "ignored"))
    run = len(passed) + len(failed)

    problems = []
    if run == 0:
        problems.append("no test ran")
    elif args.expected_run is not None and run != args.expected_run:
        problems.append(f"ran {run} tests, expected {args.expected_run}")
    if args.expected_ignored is not None and len(ignored) != args.expected_ignored:
        problems.append(f"{len(ignored)} tests ignored, expected {args.expected_ignored}")
    if unexpected_failures:
        problems.append("unexpected failures: " + ", ".join(unexpected_failures))
    if now_passing:
        problems.append(
            "listed as expected failures but passed (remove them from the list): "
            + ", ".join(now_passing)
        )
    if not_run:
        problems.append("listed as expected failures but not run: " + ", ".join(not_run))
    if code != 0 and not failed:
        problems.append(f"command exited with {code} without a failing test")
    if code == 0 and failed:
        problems.append("command exited with 0 despite failing tests")

    by_reason = collections.Counter(expected[n] for n in failed if n in expected)
    reasons = ", ".join(f"{reason} {n}" for reason, n in sorted(by_reason.items()))
    print()
    print(f"=== {args.name} ===")
    print(f"run: {run}  passed: {len(passed)}  failed: {len(failed)}  "
          f"ignored: {len(ignored)}  (expected failures: {len(expected)})")
    if reasons:
        print(f"expected failures by reason: {reasons}")
    for name in failed:
        tag = f"expected, {expected[name]}" if name in expected else "UNEXPECTED"
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
