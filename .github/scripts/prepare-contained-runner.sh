#!/usr/bin/env bash
# prepare-contained-runner.sh: turn a GitHub-hosted ubuntu-24.04 runner into a host that installs
# the `scoped` and `sealed` containment classes, for ci.yml's `contained` job.
#
# Both classes need Landlock, a delegated cgroup v2 scope, and an unprivileged user namespace
# (scoped for its network namespace, sealed for its mount namespace too). A stock runner has
# Landlock, refuses the user namespace under AppArmor's unprivileged-userns restriction, and runs
# no systemd user manager for `runner`. This script:
#
#   1. prints the kernel, the active LSMs and the restriction, and fails without Landlock;
#   2. checks for passwordless sudo;
#   3. grants user namespaces to this checkout's target/{debug,release}/mur with
#      `sudo scripts/install-dev-apparmor.sh`, falling back to
#      `sysctl kernel.apparmor_restrict_unprivileged_userns=0`, and says which it used;
#   4. installs a user@.service drop-in with `Delegate=memory pids cpu io`;
#   5. enables lingering and waits for the user manager's bus;
#   6. exports XDG_RUNTIME_DIR and DBUS_SESSION_BUS_ADDRESS (to $GITHUB_ENV when set) and
#      CONTAINED_USERNS_GRANT, the mechanism step 3 used;
#   7. runs check-cgroup-delegation.sh.
#
# Run from the repository root:  .github/scripts/prepare-contained-runner.sh
# It calls sudo itself and needs it passwordless. It changes host-wide settings; it is meant for
# a disposable CI runner. Exit codes: 0 prepared; 1 a step failed, named on stderr.
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$repo_root"

restrict_file=/proc/sys/kernel/apparmor_restrict_unprivileged_userns
step=""
fail() {
    local message="prepare-contained-runner: step '${step}' failed: $*"
    echo "$message" >&2
    if [ "${GITHUB_ACTIONS:-}" = "true" ]; then
        echo "::error title=Prepare the contained runner::$message"
    fi
    if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
        printf '### Host preparation: FAILED\n\n- step: %s\n- reason: %s\n\n' "$step" "$*" \
            >> "$GITHUB_STEP_SUMMARY"
    fi
    exit 1
}
# Any command that fails under `set -e` without its own message still names the step.
trap 'fail "command exited $?: ${BASH_COMMAND}"' ERR

begin() {
    step="$1"
    echo
    echo "== ${step}"
}

restriction() {
    if [ -r "$restrict_file" ]; then cat "$restrict_file"; else echo "absent"; fi
}

begin "read the kernel and its LSMs"
kernel="$(uname -r)"
lsms="$(cat /sys/kernel/security/lsm 2>/dev/null || echo unreadable)"
restriction_before="$(restriction)"
echo "kernel: ${kernel}"
echo "active LSMs: ${lsms}"
echo "kernel.apparmor_restrict_unprivileged_userns: ${restriction_before}"
echo "AppArmor enabled: $(cat /sys/module/apparmor/parameters/enabled 2>/dev/null || echo absent)"
echo "user: $(id -un) ($(id -u)), systemd $(systemctl --version | head -n 1)"
case ",${lsms}," in
    *,landlock,*) ;;
    *) fail "landlock is not an active LSM (${lsms}); neither scoped nor sealed can be installed" ;;
esac

begin "check for passwordless sudo"
sudo -n true || fail "sudo needs a password; this runner cannot be prepared"
echo "sudo: passwordless"

begin "grant user namespaces to mur"
echo "apparmor_parser: $(command -v apparmor_parser || sudo sh -c 'command -v apparmor_parser' || echo 'not on PATH')"
grant=""
if sudo scripts/install-dev-apparmor.sh; then
    # One command, not `sudo cat … | grep -q`: under pipefail, grep -q's early exit gives cat
    # SIGPIPE and the pipeline reads as no match.
    if sudo grep -q '^mur-sealed-dev ' /sys/kernel/security/apparmor/profiles 2>/dev/null; then
        grant="AppArmor profile mur-sealed-dev (scripts/install-dev-apparmor.sh)"
    else
        echo "install-dev-apparmor.sh exited 0 but no mur-sealed-dev profile is loaded"
    fi
else
    echo "install-dev-apparmor.sh failed (exit $?)"
fi
if [ -z "$grant" ]; then
    if [ "$(restriction)" = "absent" ]; then
        grant="none needed: this kernel has no unprivileged-userns restriction"
    else
        echo "falling back to the host-wide sysctl"
        sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0 \
            || fail "neither the dev AppArmor profile nor the sysctl could grant user namespaces"
        grant="sysctl kernel.apparmor_restrict_unprivileged_userns=0 (host-wide fallback)"
    fi
fi
echo "user namespace grant: ${grant}"
echo "kernel.apparmor_restrict_unprivileged_userns now: $(restriction)"

begin "delegate cgroup controllers to the user manager"
dropin_dir=/etc/systemd/system/user@.service.d
sudo mkdir -p "$dropin_dir"
echo "installing $dropin_dir/10-murmur-delegate.conf:"
printf '[Service]\nDelegate=memory pids cpu io\n' | sudo tee "$dropin_dir/10-murmur-delegate.conf"
sudo systemctl daemon-reload

begin "start a lingering user manager"
uid="$(id -u)"
# A user manager already running was started without the drop-in; restart it so it delegates.
if systemctl is-active --quiet "user@${uid}.service"; then
    echo "user@${uid}.service was already active; restarting it to apply the drop-in"
    sudo systemctl restart "user@${uid}.service"
fi
sudo loginctl enable-linger "$(id -un)"
export XDG_RUNTIME_DIR="/run/user/${uid}"
for _ in $(seq 1 300); do
    [ -S "${XDG_RUNTIME_DIR}/bus" ] && break
    sleep 0.1
done
[ -S "${XDG_RUNTIME_DIR}/bus" ] \
    || fail "no user manager bus at ${XDG_RUNTIME_DIR}/bus 30 s after enable-linger"
export DBUS_SESSION_BUS_ADDRESS="unix:path=${XDG_RUNTIME_DIR}/bus"
loginctl show-user "$(id -un)" -p Linger -p State || true
systemctl show "user@${uid}.service" -p ActiveState -p Delegate -p DelegateControllers
systemctl --user show -p Version -p DefaultTasksMax \
    || fail "the user manager's bus exists but systemctl --user cannot reach it"

begin "export the user bus to later steps"
if [ -n "${GITHUB_ENV:-}" ]; then
    {
        echo "XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR}"
        echo "DBUS_SESSION_BUS_ADDRESS=${DBUS_SESSION_BUS_ADDRESS}"
        echo "CONTAINED_USERNS_GRANT=${grant}"
    } >> "$GITHUB_ENV"
    echo "wrote XDG_RUNTIME_DIR, DBUS_SESSION_BUS_ADDRESS and CONTAINED_USERNS_GRANT to \$GITHUB_ENV"
else
    echo "GITHUB_ENV is not set; export these in your shell:"
    echo "  export XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR}"
    echo "  export DBUS_SESSION_BUS_ADDRESS=${DBUS_SESSION_BUS_ADDRESS}"
fi

begin "check cgroup delegation"
bash "${repo_root}/.github/scripts/check-cgroup-delegation.sh" \
    || fail "check-cgroup-delegation.sh could not create a child cgroup in a delegated scope"

trap - ERR
if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
    {
        echo "### Host preparation: done"
        echo
        echo "- kernel: ${kernel}"
        echo "- active LSMs: ${lsms}"
        echo "- kernel.apparmor_restrict_unprivileged_userns before: ${restriction_before}, after: $(restriction)"
        echo "- user namespace grant for mur: ${grant}"
        echo "- cgroup: user@.service delegates memory pids cpu io; a delegated scope created and removed a child cgroup"
        echo
    } >> "$GITHUB_STEP_SUMMARY"
fi
echo
echo "host prepared: ${grant}; user manager at ${XDG_RUNTIME_DIR}/bus"
