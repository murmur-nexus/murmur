#!/usr/bin/env python3
"""Fail when a nextest JUnit report holds any `[SKIP-HOST]` line.

A test that cannot isolate a capsule on this host prints `[SKIP-HOST] <test>: <reason>`
and returns, which nextest counts as a pass. On a host prepared to install scoped and
sealed that is a hole in the evidence, so this gate turns it into a failure. nextest's
`ci` profile keeps passing tests' output (`store-success-output = true`), which is what
makes the line visible here.

    python3 -I .github/scripts/fail-on-host-skips.py target/nextest/ci/junit.xml

Exit codes:
  0  the report holds no `[SKIP-HOST]` line
  1  at least one test printed `[SKIP-HOST]`; each is listed with its first such line
  2  no report at that path, or one that cannot be read, parsed, or holds no test

Standard library only. When GITHUB_STEP_SUMMARY is set the count and the tests are
appended to it; when GITHUB_ACTIONS is "true" a failure is also an ::error:: annotation.
"""

from __future__ import annotations

import argparse
import os
import sys
import xml.etree.ElementTree as ElementTree

HOST_SKIP = "[SKIP-HOST]"
# Counted for the summary only: a missing local fixture or checkout, never a host gap.
OTHER_SKIP = "[SKIP]"
FIRST_LINE_CHARS = 300


def summary(lines: list[str]) -> None:
    path = os.environ.get("GITHUB_STEP_SUMMARY")
    if path:
        with open(path, "a", encoding="utf-8") as handle:
            handle.write("\n".join(lines) + "\n")


def annotate(message: str) -> None:
    if os.environ.get("GITHUB_ACTIONS") == "true":
        print(f"::error title=Host skip::{message}", flush=True)


def refuse(message: str) -> int:
    print(f"HOST-SKIP GATE FAILED: {message}", flush=True)
    annotate(message)
    summary(["### Tests skipped for the host", "", f"FAILED: {message}", ""])
    return 2


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Fail when a nextest JUnit report holds any [SKIP-HOST] line."
    )
    parser.add_argument("report", help="the JUnit report, e.g. target/nextest/ci/junit.xml")
    args = parser.parse_args()

    if not os.path.isfile(args.report):
        return refuse(
            f"no test report was written at {args.report}, so no test can be shown "
            "to have run rather than skipped"
        )
    try:
        root = ElementTree.parse(args.report).getroot()
    except (OSError, ElementTree.ParseError) as error:
        return refuse(f"the test report at {args.report} cannot be read: {error}")

    cases = list(root.iter("testcase"))
    if not cases:
        return refuse(f"the test report at {args.report} holds no test")

    host_skip_lines = 0
    other_skip_lines = 0
    skipped: list[tuple[str, str]] = []
    for case in cases:
        # Every output element of the test, including the retries' (flakyFailure/rerunFailure).
        text = "".join(case.itertext())
        host_skip_lines += text.count(HOST_SKIP)
        other_skip_lines += text.count(OTHER_SKIP)
        if HOST_SKIP in text:
            first = next(line for line in text.splitlines() if HOST_SKIP in line).strip()
            # The reason runs to a paragraph; the whole line is in the uploaded report.
            if len(first) > FIRST_LINE_CHARS:
                first = first[:FIRST_LINE_CHARS] + " …"
            name = f"{case.get('classname', '?')} {case.get('name', '?')}"
            skipped.append((name, first))

    print(
        f"{len(cases)} tests in {args.report}: {len(skipped)} printed {HOST_SKIP} "
        f"({host_skip_lines} lines), {other_skip_lines} {OTHER_SKIP} lines",
        flush=True,
    )
    lines = [
        "### Tests skipped for the host",
        "",
        f"- tests in the report: {len(cases)}",
        f"- tests that printed `{HOST_SKIP}`: {len(skipped)} ({host_skip_lines} lines)",
        f"- `{OTHER_SKIP}` lines (a local fixture or checkout this runner lacks): {other_skip_lines}",
    ]
    if not skipped:
        summary(lines + [""])
        print("PASS: no test skipped for the host", flush=True)
        return 0

    lines += ["", "| Test | First `[SKIP-HOST]` line |", "| --- | --- |"]
    for name, first in skipped:
        print(f"HOST SKIP: {name}: {first}", flush=True)
        cell = first.replace("|", "\\|")
        lines.append(f"| `{name}` | {cell} |")
    summary(lines + [""])
    # One annotation, not one per test: a step shows only its first ten.
    annotate(
        f"{len(skipped)} test{'s' if len(skipped) != 1 else ''} printed {HOST_SKIP}, "
        f"first {skipped[0][0]}; the step log lists them all"
    )
    print(
        f"HOST-SKIP GATE FAILED: {len(skipped)} test{'s' if len(skipped) != 1 else ''} "
        f"printed {HOST_SKIP}; on this host a host skip is a failure",
        flush=True,
    )
    return 1


if __name__ == "__main__":
    sys.exit(main())
