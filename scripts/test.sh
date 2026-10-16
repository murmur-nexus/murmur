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
# Usage:  scripts/test.sh [extra cargo args...]
#         scripts/test.sh -p capsule-runtime          # narrow to one crate
set -uo pipefail

FEATURES="beta-all"
cd "$(dirname "$0")/.."

exec cargo test --workspace --no-fail-fast --features "$FEATURES" "$@"
