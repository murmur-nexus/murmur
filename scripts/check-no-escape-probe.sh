#!/usr/bin/env bash
# check-no-escape-probe.sh: a release build carries no escape-conformance probe.
#
# The test-only `escape-probe` Cargo feature (capsule-runtime, murmur-cli) gives a guest worker a
# host side that runs host syscalls a guest asks for (crates/capsule-runtime/src/guest_worker/
# escape_probe.rs). It exists for the escape-conformance gate's debug builds. This script checks
# both halves of "no release has it":
#
#   1. Each binary named on the command line (the released `mur` and `mur-roost`) holds none of
#      the probe's markers: the request key and the strings only its host side contains, searched
#      as bytes because a release binary is stripped, plus its module path in the symbol table
#      when `nm` can read one.
#   2. With --feature-refused, a release-profile build of capsule-runtime with the feature is
#      refused by the `compile_error!` in crates/capsule-runtime/src/lib.rs.
#
# Run from the repository root:
#   scripts/check-no-escape-probe.sh target/release/mur target/release/mur-roost
#   scripts/check-no-escape-probe.sh --feature-refused
# Exit codes: 0 every check passed; 1 a check failed, named on stderr; 2 usage error.
set -euo pipefail

# Strings only the probe's host side holds. Keep in step with escape_probe.rs.
markers=(
    "murmur-escape-probe"
    "the guest worker has no probe body for case"
    "the guest worker's host side ran a probe"
)

if [ "$#" -eq 0 ]; then
    echo "usage: $0 <binary>... | --feature-refused" >&2
    exit 2
fi

failed=0
for arg in "$@"; do
    if [ "$arg" = "--feature-refused" ]; then
        log=$(mktemp)
        if cargo check --release -p capsule-runtime --features escape-probe >"$log" 2>&1; then
            echo "FAIL: a release-profile build of capsule-runtime with escape-probe compiled" >&2
            failed=1
        elif grep -q "the \`escape-probe\` feature is test-only" "$log"; then
            echo "ok: a release-profile build with escape-probe is refused by compile_error!"
        else
            echo "FAIL: the release-profile build with escape-probe failed, but not with the" \
                "compile_error! this script expects:" >&2
            tail -n 20 "$log" >&2
            failed=1
        fi
        rm -f "$log"
        continue
    fi
    if [ ! -f "$arg" ]; then
        echo "FAIL: $arg is not a file" >&2
        failed=1
        continue
    fi
    found=0
    for marker in "${markers[@]}"; do
        if grep -aqF -- "$marker" "$arg"; then
            echo "FAIL: $arg contains the probe marker \"$marker\"" >&2
            found=1
        fi
    done
    if command -v nm >/dev/null 2>&1 && nm "$arg" 2>/dev/null | grep -q "escape_probe"; then
        echo "FAIL: $arg has an escape_probe symbol" >&2
        found=1
    fi
    if [ "$found" -eq 0 ]; then
        echo "ok: $arg carries no escape-probe marker"
    else
        failed=1
    fi
done
exit "$failed"
