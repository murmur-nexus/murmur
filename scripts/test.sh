#!/usr/bin/env bash
#
# Run the workspace test suite.
#
# Why this exists rather than a bare `cargo test --workspace`:
#
#   * `topology_cmd`, `deploy_cmd` and the `mur new <task>` tests compile out entirely without their
#     beta features, so a bare run silently skips them rather than reporting them. `beta-all` turns
#     on every beta feature.
#
# The runtime refuses to launch a subprocess-capable capsule without a delegated cgroup v2 scope
# (fail-closed; see crates/capsule-runtime/src/cgroup.rs). Each test binary asks systemd for its
# own transient scope the first time it needs a cgroup base, so the tests that need one run under
# a plain `cargo test` regardless of which cgroup the invoking shell sits in.
#
# On a host with no systemd user session the cgroup-dependent tests fail with E-RUN-012; see
# docs/content/reference/resource-limits-manual-verification.md.
#
# The full output is also written to target/test-logs/test-<timestamp>.log, because a full run
# overflows the terminal's scrollback. The log path and the names of any failed tests are printed
# at the end.
#
# Usage:  scripts/test.sh [extra cargo args...]
#         scripts/test.sh -p capsule-runtime          # narrow to one crate
set -uo pipefail

FEATURES="beta-all"
cd "$(dirname "$0")/.."

LOG_DIR="target/test-logs"
mkdir -p "$LOG_DIR"
LOG="$PWD/$LOG_DIR/test-$(date +%Y%m%d-%H%M%S).log"

cargo test --workspace --no-fail-fast --features "$FEATURES" "$@" 2>&1 | tee "$LOG"
status=${PIPESTATUS[0]}

echo
if [ "$status" -ne 0 ]; then
    # Each failed test's captured output: its `---- <name> stdout ----` block, up to the
    # `failures:` list that follows it, capped per test so one noisy test cannot flood the summary.
    echo "Errors:"
    awk -v max=40 '
        /^---- .* ----$/ { inblock = 1; n = 0; print; next }
        inblock && /^failures:$/ { inblock = 0; print ""; next }
        inblock {
            n++
            if (n <= max) print "  " $0
            else if (n == max + 1) print "  ... (truncated, see the log)"
            next
        }
        /^error(\[[A-Z0-9]+\])?: / { print }
    ' "$LOG"
    echo "Failed tests:"
    grep -E '^test .* \.\.\. FAILED$' "$LOG" | sed -E 's/^test (.*) \.\.\. FAILED$/  \1/' || true
    echo "Failed targets:"
    grep -E '^ +`.*`$' "$LOG" || true
    echo
fi
echo "Full log: $LOG"
echo "  search: grep -nE 'panicked|^---- |FAILED' $LOG"
exit "$status"
