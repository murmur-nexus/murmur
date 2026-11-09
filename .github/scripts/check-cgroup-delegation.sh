#!/usr/bin/env bash
# check-cgroup-delegation.sh: prove that `systemd-run --user --scope -p Delegate=yes` gives a
# cgroup a capsule launch can write, by creating and removing a child cgroup inside one.
#
# A capsule that spawns native subprocesses needs a delegated cgroup v2 scope (E-RUN-012
# otherwise). This is the smallest check that a user manager is running, answers on its bus, and
# hands a scope a subtree its own processes can write. It needs no root.
#
# Usage:  bash .github/scripts/check-cgroup-delegation.sh
# Reads XDG_RUNTIME_DIR (defaults to /run/user/<uid>) to reach the user manager. Prints the
# scope's cgroup, the parent's cgroup.subtree_control and the scope's cgroup.controllers.
# Exit codes: 0 a child cgroup was created and removed; 1 a step failed, named on stderr.
set -euo pipefail

step=""
fail() {
    local message="check-cgroup-delegation: step '${step}' failed: $*"
    echo "$message" >&2
    if [ "${GITHUB_ACTIONS:-}" = "true" ]; then
        echo "::error title=cgroup delegation::$message"
    fi
    exit 1
}

step="find the user manager"
echo "== ${step}"
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
echo "XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR}"
[ -S "${XDG_RUNTIME_DIR}/bus" ] \
    || fail "no user manager bus at ${XDG_RUNTIME_DIR}/bus; is lingering enabled for $(id -un)?"
systemctl --user show -p Version -p DefaultTasksMax 2>&1 \
    || fail "systemctl --user cannot reach the user manager on ${XDG_RUNTIME_DIR}/bus"

step="create a child cgroup in a delegated scope"
echo "== ${step}"
# The inner script reports its own step and exits 10 or more, so a failure of systemd-run itself
# (no bus, no scope) is told apart from a failure inside the scope.
set +e
# shellcheck disable=SC2016  # the single-quoted script expands its variables in the inner bash
systemd-run --user --scope --quiet --collect -p Delegate=yes -- bash -c '
    set -u
    in_step() { echo "check-cgroup-delegation: step '"'"'$1'"'"' failed: $2" >&2; exit "$3"; }
    rel=$(grep "^0::" /proc/self/cgroup | cut -d: -f3-) \
        || in_step "read the scope cgroup" "/proc/self/cgroup has no cgroup v2 line: $(cat /proc/self/cgroup)" 11
    cg="/sys/fs/cgroup${rel}"
    echo "scope cgroup: ${cg}"
    [ -d "$cg" ] || in_step "read the scope cgroup" "${cg} is not a directory" 12
    echo "parent cgroup.subtree_control: $(cat "$(dirname "$cg")/cgroup.subtree_control" 2>&1)"
    echo "scope cgroup.controllers: $(cat "$cg/cgroup.controllers" 2>&1)"
    probe="$cg/tier-probe"
    if ! err=$(mkdir "$probe" 2>&1); then
        in_step "create a child cgroup" "mkdir ${probe}: ${err}" 13
    fi
    if ! err=$(rmdir "$probe" 2>&1); then
        in_step "remove the child cgroup" "rmdir ${probe}: ${err} (left behind)" 14
    fi
    echo "child cgroup: created and removed (${probe})"
'
status=$?
set -e
case "$status" in
    0) ;;
    1[1-4]) fail "inside the scope, exit ${status} (the line above names the step)" ;;
    *) fail "systemd-run --user --scope -p Delegate=yes exited ${status}; no scope was created" ;;
esac
echo "cgroup delegation: OK"
