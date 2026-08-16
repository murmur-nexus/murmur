#!/usr/bin/env bash
#
# Run the workspace test suite on a Linux host with systemd user delegation.
#
# Why this exists rather than a bare `cargo test --workspace`:
#
#   * The runtime refuses to launch a subprocess-capable capsule without a cgroup v2 scope
#     carrying delegated memory/pids/cpu (fail-closed; see crates/capsule-runtime/src/cgroup.rs).
#     Around 50 tests need one.
#   * A cgroup can only enable controllers for its children while no task sits directly in it, so
#     the runtime has to be the *only* process in its cgroup. Wrapping the whole `cargo test`
#     invocation in a delegated scope does not work — `cargo` stays resident there and blocks the
#     controller write. Each test binary therefore gets its own transient scope.
#   * `topology_cmd` and `deploy_cmd` compile out entirely without their beta features, so a bare
#     run silently skips both files rather than reporting them.
#
# On a host without delegation the cgroup-dependent tests fail with E-RUN-012; see
# docs/content/reference/resource-limits-manual-verification.md for the systemd configuration.
#
# Usage:  scripts/test.sh [extra cargo args...]
#         scripts/test.sh -p capsule-runtime          # narrow to one crate
set -uo pipefail

FEATURES="beta-mur-topology beta-mur-deploy"
cd "$(dirname "$0")/.."

if ! command -v systemd-run >/dev/null 2>&1; then
    echo "systemd-run not found — falling back to a plain cargo test." >&2
    echo "Tests needing a cgroup scope will fail; that is the host, not the code." >&2
    exec cargo test --workspace --no-fail-fast --features "$FEATURES" "$@"
fi

echo "Building test binaries..."
# Filter on `profile.test`, not merely on `executable` being present: `cargo test --no-run` also
# emits an artifact for the `mur` binary itself, which has an executable path but no libtest
# harness — running it just prints help, and it would be reported as a suite that produced no
# result line.
mapfile -t BINS < <(
    cargo test --workspace --no-run --features "$FEATURES" "$@" --message-format=json 2>/dev/null \
        | python3 -c '
import json, sys
for line in sys.stdin:
    try:
        msg = json.loads(line)
    except ValueError:
        continue
    if msg.get("profile", {}).get("test") and msg.get("executable"):
        print(msg["executable"])
' | sort -u
)

if [ "${#BINS[@]}" -eq 0 ]; then
    echo "No test binaries were built — check the cargo output above." >&2
    exit 1
fi

echo "Running ${#BINS[@]} test binaries, each in its own delegated scope."
echo

total_pass=0 total_fail=0 total_ignored=0 failed_suites=()

for bin in "${BINS[@]}"; do
    name=$(basename "$bin" | sed 's/-[0-9a-f]*$//')
    out=$(timeout 600 systemd-run --user --scope -q -p Delegate=yes -- "$bin" 2>&1)
    line=$(echo "$out" | grep -E '^test result' | tail -1)

    pass=$(echo "$line" | sed -n 's/.* \([0-9]*\) passed.*/\1/p'); pass=${pass:-0}
    fail=$(echo "$line" | sed -n 's/.* \([0-9]*\) failed.*/\1/p'); fail=${fail:-0}
    ign=$(echo "$line"  | sed -n 's/.* \([0-9]*\) ignored.*/\1/p'); ign=${ign:-0}

    total_pass=$((total_pass + pass))
    total_fail=$((total_fail + fail))
    total_ignored=$((total_ignored + ign))

    if [ "$fail" -gt 0 ] || [ -z "$line" ]; then
        failed_suites+=("$name")
        printf '  %-28s FAIL  %s\n' "$name" "${line:-<no result line — crashed or timed out>}"
        echo "$out" | grep -A6 '^---- ' | sed 's/^/      /'
    else
        printf '  %-28s ok    %s passed, %s ignored\n' "$name" "$pass" "$ign"
    fi
done

echo
echo "─────────────────────────────────────────────"
echo "  ${#BINS[@]} suites   $total_pass passed   $total_fail failed   $total_ignored ignored"

if [ "${#failed_suites[@]}" -gt 0 ]; then
    echo "  failing: ${failed_suites[*]}"
    exit 1
fi
exit 0
