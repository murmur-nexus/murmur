#!/usr/bin/env python3
"""
Check that every `release-notes.py` command line in the workflows parses.

The bug this exists to catch: `--repo` was declared on the main parser, which
in argparse means it has to precede the subcommand, while all three workflows
passed it after. Every function in release-notes.py was correct, so no test of
its parsing or grouping logic would have gone red — the fault lived purely in
the seam between the YAML and the Python. It failed on every PR for `validate`,
and sat unexploded in `aggregate`, which only runs on a release tag.

So this reads the invocations out of the workflow files rather than restating
them here. A copy of the command lines would drift from the real ones and
prove nothing; the point is to check what CI actually runs.

Parsing stops at `parse_args` — nothing is executed, so this needs no token
and no network.

    python .github/scripts/check-workflow-invocations.py
"""

import glob
import importlib.util
import os
import re
import shlex
import sys

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
SCRIPT = os.path.join(REPO_ROOT, ".github", "scripts", "release-notes.py")
WORKFLOWS = os.path.join(REPO_ROOT, ".github", "workflows", "*.yml")

# GitHub expands `${{ ... }}` before the shell ever sees it. Any placeholder
# value will do — this checks argument *shape*, not content — but it has to be
# something that survives `int()` for --pr, hence a bare number.
EXPRESSION_RE = re.compile(r"\$\{\{[^}]*\}\}")
PLACEHOLDER = "1"

# Shell variables the workflow sets earlier in the same `run:` block.
SHELL_VAR_RE = re.compile(r"\$\{?[A-Z_][A-Z0-9_]*(#v)?\}?")

# A command may continue across backslash-newline; capture through to the end
# of the continuation.
INVOCATION_RE = re.compile(
    r"^[ \t]*python3?[ \t]+\S*release-notes\.py(?P<args>(?:[^\n\\]|\\\n)*)", re.MULTILINE
)


def load_release_notes():
    """Import release-notes.py despite the hyphens in its name."""
    spec = importlib.util.spec_from_file_location("release_notes", SCRIPT)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def invocations():
    """Yield (workflow path, argument list) for each call in the workflows."""
    for path in sorted(glob.glob(WORKFLOWS)):
        with open(path) as f:
            text = f.read()
        for match in INVOCATION_RE.finditer(text):
            line = match.group("args").replace("\\\n", " ")
            line = EXPRESSION_RE.sub(PLACEHOLDER, line)
            line = SHELL_VAR_RE.sub(PLACEHOLDER, line)
            yield os.path.relpath(path, REPO_ROOT), shlex.split(line)


def main() -> int:
    parser = load_release_notes().build_parser()

    found = 0
    failures = []
    for workflow, args in invocations():
        found += 1
        rendered = " ".join(shlex.quote(a) for a in args)
        try:
            # argparse writes its own diagnosis to stderr and raises SystemExit;
            # let it through so the failure message names the offending flag.
            parser.parse_args(args)
        except SystemExit as exit_code:
            if exit_code.code:
                failures.append((workflow, rendered))
                continue
        print(f"ok   {workflow}: release-notes.py {rendered}")

    # No invocations means the regex stopped matching, not that everything
    # passes. A check that silently checks nothing is worse than no check.
    if not found:
        print(
            "::error::found no release-notes.py invocations in .github/workflows/. "
            "Either they moved, or INVOCATION_RE in this script needs updating.",
            file=sys.stderr,
        )
        return 1

    for workflow, rendered in failures:
        print(
            f"::error file={workflow}::release-notes.py rejects this workflow's "
            f"arguments: {rendered}",
            file=sys.stderr,
        )

    print(f"\n{found - len(failures)}/{found} invocations parse.")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
