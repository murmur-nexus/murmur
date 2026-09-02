#!/bin/sh
#
# wit-versions-changed-since.sh — which murmur:* WIT contracts moved since a release.
#
#   scripts/wit-versions-changed-since.sh <revision>
#
# A release-time gate. It answers the one question check-wit-versions.sh cannot:
# that one is a within-commit check, so it can tell you the tree is
# self-consistent and its documentation true, but not that anything moved. This
# one diffs the declared (package, version) set at <revision> — normally the
# previous release tag — against the working tree, and names every murmur:*
# package that changed, appeared or disappeared.
#
# The answer decides work in another repo. The host accepts exactly one version
# of each interface and keeps no fallback for any earlier one (see
# crates/capsule-runtime/wit/VERSIONING.md), so a moved version means every
# artifact built against the old one must be rebuilt and republished in
# default-artifacts before the next tag is pushed — it will fail at
# instantiation otherwise, not degrade.
#
# Deliberately NOT wired into per-PR CI. The question is only meaningful
# relative to a release tag; a PR that legitimately bumps a WIT version would
# fail an always-on version of this check, and a gate that fails on correct work
# teaches everyone to ignore it. .github/workflows/ci.yml keeps
# check-wit-versions.sh as the per-PR check and gains nothing from this script.
#
# It reports and fails. It never rebuilds anything, never reads or writes
# default-artifacts, and never creates a tag, a checkout or a temporary file:
# the revision side is read straight out of the object store.
#
# Exit codes:
#   0  nothing moved
#   1  at least one package moved, was added, or was removed
#   2  bad usage, unresolvable revision, wrong working directory, empty read

set -eu

. "$(dirname "$0")/lib/wit-packages.sh"

if [ $# -ne 1 ]; then
    echo "error: expected exactly one argument, the revision to compare against" >&2
    echo "usage: scripts/wit-versions-changed-since.sh <revision>" >&2
    echo "  <revision>  a git tag, branch or commit — normally the previous release tag, e.g. v0.2.0" >&2
    exit 2
fi

rev="$1"

wit_require_repo_root

if ! git rev-parse --verify --quiet "${rev}^{commit}" >/dev/null 2>&1; then
    echo "error: git cannot resolve the revision '$rev'" >&2
    echo "  pass a tag, branch or commit that exists in this repository; \`git tag -l\` lists the release tags." >&2
    exit 2
fi

if ! was=$(wit_packages "$rev"); then
    echo "error: no murmur:* package declarations found under $WIT_DIR at $rev" >&2
    echo "  there is nothing to compare against; that revision predates the versioned WIT packages." >&2
    exit 2
fi

if ! now=$(wit_packages --worktree); then
    echo "error: no murmur:* package declarations found under $WIT_DIR" >&2
    exit 2
fi

names=$(printf '%s\n%s\n' "$was" "$now" | cut -d@ -f1 | sort -u)

width=0
for name in $names; do
    [ "${#name}" -le "$width" ] || width=${#name}
done

moved=""
added=""
removed=""
changes=0
compared=0

for name in $names; do
    # The trailing @ anchors the match, so murmur:tool cannot swallow
    # murmur:tool-registry.
    was_v=$(printf '%s\n' "$was" | sed -n "s/^${name}@//p")
    now_v=$(printf '%s\n' "$now" | sed -n "s/^${name}@//p")

    if [ -z "$was_v" ]; then
        added="${added}$(printf '    %-*s  %s' "$width" "$name" "$now_v")
"
        changes=$((changes + 1))
    elif [ -z "$now_v" ]; then
        removed="${removed}$(printf '    %-*s  %s' "$width" "$name" "$was_v")
"
        changes=$((changes + 1))
    elif [ "$was_v" != "$now_v" ]; then
        moved="${moved}$(printf '    %-*s  %s → %s' "$width" "$name" "$was_v" "$now_v")
"
        changes=$((changes + 1))
        compared=$((compared + 1))
    else
        compared=$((compared + 1))
    fi
done

if [ "$changes" -eq 0 ]; then
    echo "ok: no murmur:* WIT package moved since $rev — $compared packages compared, each at the same version"
    exit 0
fi

if [ "$changes" -eq 1 ]; then
    echo "error: 1 murmur:* WIT package changed since $rev." >&2
else
    echo "error: $changes murmur:* WIT packages changed since $rev." >&2
fi
echo >&2

if [ -n "$moved" ]; then
    echo "  version moved:" >&2
    printf '%s' "$moved" >&2
    echo >&2
fi
if [ -n "$added" ]; then
    echo "  added since $rev (not declared there):" >&2
    printf '%s' "$added" >&2
    echo >&2
fi
if [ -n "$removed" ]; then
    echo "  removed since $rev (declared there, gone now):" >&2
    printf '%s' "$removed" >&2
    echo >&2
fi

echo "The artifacts built against these contracts must be rebuilt and republished in" >&2
echo "default-artifacts before this release is tagged. The host keeps no fallback for a" >&2
echo "retired interface version: an artifact built against the old one fails at" >&2
echo "instantiation, it does not degrade. See $WIT_DIR/VERSIONING.md." >&2
exit 1
