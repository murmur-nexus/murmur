#!/usr/bin/env bash
#
# Run the workspace test suite.
#
# Why this exists rather than a bare `cargo test --workspace`:
#
#   * `topology_cmd` and `deploy_cmd` compile out entirely without their beta features, so a bare
#     run silently skips both files rather than reporting them.
#
# The runtime still refuses to launch a subprocess-capable capsule without a delegated cgroup v2
# scope (fail-closed; see crates/capsule-runtime/src/cgroup.rs), and around 50 tests need one —
# but each test binary asks systemd for its own transient scope when it first needs a cgroup
# base, so `cargo`'s own residency in the invoking shell's cgroup doesn't matter.
#
# On a host with no systemd user session at all the cgroup-dependent tests still fail with
# E-RUN-012; see docs/content/reference/resource-limits-manual-verification.md.
#
# Usage:  scripts/test.sh [extra cargo args...]
#         scripts/test.sh -p capsule-runtime          # narrow to one crate
set -uo pipefail

FEATURES="beta-mur-topology beta-mur-deploy"
cd "$(dirname "$0")/.."

exec cargo test --workspace --no-fail-fast --features "$FEATURES" "$@"
