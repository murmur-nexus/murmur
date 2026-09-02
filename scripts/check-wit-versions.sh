#!/bin/sh
#
# check-wit-versions.sh — keep the documented WIT versions equal to the real ones.
#
#   scripts/check-wit-versions.sh
#
# Scope: THIS repo only. It never reads default-artifacts and never writes
# anything, so it does not touch the direction of authority — murmur declares
# the versions, artifacts follow. Not to be confused with default-artifacts'
# `scripts/check-wit-sync.sh`, which is the cross-repo check: it byte-diffs that
# repo's vendored `wit/{guest,hook}` against a pinned murmur commit. That one
# answers "has the artifacts repo copied the current WIT?"; this one answers
# "is murmur's own WIT tree self-consistent, and is its documentation true?" —
# and it covers files the sync check deliberately excludes.
#
# Two facts about every `murmur:*` WIT package are asserted against the tree
# itself rather than against anyone's memory:
#
#   1. A package name is declared at exactly ONE version across the whole tree.
#      Several subtrees under wit/ are bound by different consumers (see
#      wit/README.md), and each is parsed independently, so nothing at build
#      time catches one copy being left a version behind another.
#
#   2. The "Current versions" table in wit/VERSIONING.md lists exactly the
#      (package, version) pairs the tree declares — no stale row, no missing row.
#
# Exits non-zero with a diff on any mismatch. Run it from the repo root.
#
# Within-commit only: it cannot tell you whether a version *moved* since the
# last release, because it never looks at history. That is the release-time
# question, and scripts/wit-versions-changed-since.sh answers it. Both read the
# declarations through lib/wit-packages.sh.

set -eu

# WIT_DIR and the extraction itself; see lib/wit-packages.sh for the contract.
. "$(dirname "$0")/lib/wit-packages.sh"

POLICY="$WIT_DIR/VERSIONING.md"

wit_require_repo_root

# Every declaration in the tree, deduplicated, via the shared extraction.
if ! declared=$(wit_packages --worktree); then
    echo "error: no murmur:* package declarations found under $WIT_DIR" >&2
    exit 2
fi

# 1. One version per package name.
dupes=$(printf '%s\n' "$declared" | cut -d@ -f1 | sort | uniq -d)
if [ -n "$dupes" ]; then
    echo "error: a package is declared at more than one version:" >&2
    for pkg in $dupes; do
        printf '%s\n' "$declared" | grep "^${pkg}@" | sed 's/^/  /' >&2
        wit_files_declaring "$pkg" | sed 's/^/    declared in: /' >&2
    done
    echo >&2
    echo "Every subtree under $WIT_DIR is parsed independently, so this will not" >&2
    echo "fail the build — but one copy is a version behind the other." >&2
    exit 1
fi

# 2. The documented table matches the tree. Rows look like:
#     | `murmur:hook`            | `0.5.0`  |
documented=$(sed -n 's/^| *`\(murmur:[a-z-]*\)` *| *`\([0-9]*\.[0-9]*\.[0-9]*\)` *|.*/\1@\2/p' \
    "$POLICY" | sort -u)

if [ "$declared" != "$documented" ]; then
    echo "error: the Current versions table in $POLICY does not match the tree." >&2
    echo >&2
    printf '%s\n' "$declared" > /tmp/wit-declared.$$
    printf '%s\n' "$documented" > /tmp/wit-documented.$$
    echo "  only in the tree (add a row):" >&2
    comm -23 /tmp/wit-declared.$$ /tmp/wit-documented.$$ | sed 's/^/    /' >&2
    echo "  only in the table (stale row):" >&2
    comm -13 /tmp/wit-declared.$$ /tmp/wit-documented.$$ | sed 's/^/    /' >&2
    rm -f /tmp/wit-declared.$$ /tmp/wit-documented.$$
    exit 1
fi

count=$(printf '%s\n' "$declared" | wc -l | tr -d ' ')
echo "ok: $count murmur:* packages, each declared at one version and documented in VERSIONING.md"
