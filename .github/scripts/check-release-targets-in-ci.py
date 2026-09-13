#!/usr/bin/env python3
"""
Check that CI compiles every Apple target the release builds.

macOS shipped for a long time with nothing compiling it except `release.yml`'s
`build-macos` job, which only runs on a release tag. Code that did not build
for Apple sat on main undetected, and the release was the first build of the
platform. `ci.yml`'s `macos` job compiles those targets on every pull request.
This check keeps the two matrices in step: an Apple target added to the
release but not to CI fails here, so it cannot slip back to being compiled only
by a release.

It reads the `target:` values straight out of both workflow files rather than
restating them, for the same reason `check-workflow-invocations.py` does: a
copy would drift from what the workflows actually build.

Standard library only (`actions/setup-python` has no PyYAML), so this reads
the small subset of YAML the two matrices are written in, not YAML in general:
block mappings by indentation, `include:` entries carrying `target: <value>`,
and a `target:` given as a flow list `[a, b]` or a block list.

    python .github/scripts/check-release-targets-in-ci.py [--root <repo root>]

Exit codes:
    0  every release target is in CI's matrix
    1  at least one release target is missing from CI's matrix
    2  a workflow file, the job, its matrix, or any target in it cannot be found
"""

import argparse
import os
import re
import sys

RELEASE_WORKFLOW = os.path.join(".github", "workflows", "release.yml")
RELEASE_JOB = "build-macos"
CI_WORKFLOW = os.path.join(".github", "workflows", "ci.yml")
CI_JOB = "macos"

KEY_RE = re.compile(r"^(?P<indent> *)(?P<dash>- +)?(?P<key>[A-Za-z0-9_-]+):(?:\s+(?P<value>.*))?$")


class Unreadable(Exception):
    """The check cannot find what it checks. Never a pass."""


def significant_lines(text):
    """(indent, stripped line) for every line that is not blank or a whole-line comment."""
    for raw in text.splitlines():
        stripped = raw.strip()
        if not stripped or stripped.startswith("#"):
            continue
        yield len(raw) - len(raw.lstrip(" ")), raw.rstrip()


def scalar(value):
    """A YAML scalar with any trailing comment and surrounding quotes removed."""
    value = re.sub(r"\s+#.*$", "", value).strip()
    if len(value) >= 2 and value[0] == value[-1] and value[0] in "'\"":
        value = value[1:-1]
    return value


def child_block(lines, start, key):
    """
    The lines nested under the first `key:` found among `lines[start:]` at the
    shallowest indentation there, or None when there is no such key.
    """
    if start >= len(lines):
        return None
    level = lines[start][0]
    for i in range(start, len(lines)):
        indent, line = lines[i]
        if indent < level:
            break
        match = KEY_RE.match(line)
        if indent == level and match and not match.group("dash") and match.group("key") == key:
            block = []
            for nested in lines[i + 1:]:
                if nested[0] <= indent:
                    break
                block.append(nested)
            return block
    return None


def matrix_targets(path, job):
    """Every `target:` value in `job`'s `strategy.matrix`, in file order."""
    try:
        with open(path) as f:
            text = f.read()
    except OSError as error:
        raise Unreadable(f"cannot read {path}: {error.strerror}")

    lines = list(significant_lines(text))
    jobs = child_block(lines, 0, "jobs")
    if not jobs:
        raise Unreadable(f"{path} has no `jobs:` mapping")
    body = child_block(jobs, 0, job)
    if not body:
        raise Unreadable(f"{path} has no job `{job}`")
    strategy = child_block(body, 0, "strategy")
    matrix = child_block(strategy, 0, "matrix") if strategy else None
    if not matrix:
        raise Unreadable(f"{path}: job `{job}` has no `strategy.matrix`")

    targets = []
    for i, (indent, line) in enumerate(matrix):
        match = KEY_RE.match(line)
        if not match or match.group("key") != "target":
            continue
        value = scalar(match.group("value") or "")
        if value.startswith("[") and value.endswith("]"):
            targets.extend(scalar(item) for item in value[1:-1].split(",") if item.strip())
        elif value:
            targets.append(value)
        else:
            # A block list: `target:` followed by deeper `- value` lines.
            key_indent = indent + len(match.group("dash") or "")
            for nested_indent, nested in matrix[i + 1:]:
                if nested_indent <= key_indent or not nested.lstrip().startswith("- "):
                    break
                targets.append(scalar(nested.lstrip()[2:]))
    if not targets:
        raise Unreadable(f"{path}: job `{job}`'s matrix names no `target:`")
    return targets


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0].strip())
    parser.add_argument("--root", default=".", help="repository root (default: the current directory)")
    args = parser.parse_args()

    try:
        release = matrix_targets(os.path.join(args.root, RELEASE_WORKFLOW), RELEASE_JOB)
        ci = set(matrix_targets(os.path.join(args.root, CI_WORKFLOW), CI_JOB))
    except Unreadable as error:
        print(f"::error::{error}", file=sys.stderr)
        return 2

    missing = [target for target in release if target not in ci]
    for target in release:
        if target not in missing:
            print(f"ok   {target}: built by release.yml's {RELEASE_JOB}, compiled by ci.yml's {CI_JOB}")
    for target in missing:
        print(
            f"::error file={RELEASE_WORKFLOW}::{target} is built by release.yml's {RELEASE_JOB} "
            f"but not compiled by ci.yml's {CI_JOB}",
            file=sys.stderr,
        )
    return 1 if missing else 0


if __name__ == "__main__":
    sys.exit(main())
