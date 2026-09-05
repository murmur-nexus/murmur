#!/bin/sh
#
# mur installer — https://github.com/murmur-nexus/murmur
#
#   curl -fsSL https://install.murmur.rs | sh
#
# Detects the platform, resolves the latest release, verifies every download against
# the release's checksums.txt, runs each binary once, and installs `mur` and `mur-roost`
# onto PATH.
#
# The published linux-x86_64 binaries require glibc 2.31 or newer: Debian 11+, Ubuntu
# 20.04+, RHEL 9+. On an older host, build from source with `cargo install murmur-cli`.
# Nothing is installed unless it runs here — a binary this host's loader refuses leaves
# no `mur` on PATH and no previous install disturbed.
#
# Environment overrides:
#   MUR_VERSION      version to install, with or without the leading "v" (default: latest)
#   MUR_INSTALL_DIR  directory to install into (default: /usr/local/bin, else ~/.local/bin)
#   MUR_REPO         owner/repo to install from (default: murmur-nexus/murmur)

set -eu

DEFAULT_REPO="murmur-nexus/murmur"
REPO="${MUR_REPO:-$DEFAULT_REPO}"
CHECKSUMS_FILE="checksums.txt"

# The oldest glibc a published linux-x86_64 binary is built to run on, quoted back to
# whoever a binary refused to start for. Declared in scripts/lib/glibc-floor.sh and
# enforced over every asset at release time by scripts/check-glibc-floor.sh; this copy
# is checked against that declaration by `scripts/check-glibc-floor.sh --config`.
SUPPORTED_GLIBC="2.31"

# AppArmor profile that lets `mur` create an unprivileged user namespace, which is
# what `capabilities.containment: sealed` and every capsule's own network namespace
# both need on Ubuntu 23.10+ and any other host with
# kernel.apparmor_restrict_unprivileged_userns=1. See packaging/apparmor/mur-sealed.
APPARMOR_PROFILE_NAME="mur-sealed"
APPARMOR_PROFILE_DIR="/etc/apparmor.d"

TMPDIR_INSTALL=""

# ---------------------------------------------------------------- output helpers

info() { printf 'info: %s\n' "$1"; }
warn() { printf 'warning: %s\n' "$1" >&2; }

# Everything that can fail exits through here, so a failure never leaves a
# half-written binary behind — the real binary is only moved into place once
# the download has been verified.
die() {
    printf 'error: %s\n' "$1" >&2
    exit 1
}

cleanup() {
    if [ -n "$TMPDIR_INSTALL" ] && [ -d "$TMPDIR_INSTALL" ]; then
        rm -rf "$TMPDIR_INSTALL"
    fi
}

need_cmd() {
    command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"
}

# ---------------------------------------------------------------- http

# Prefer curl, fall back to wget: the one-liner implies curl exists, but the
# script is also run standalone.
detect_downloader() {
    if command -v curl >/dev/null 2>&1; then
        DOWNLOADER=curl
    elif command -v wget >/dev/null 2>&1; then
        DOWNLOADER=wget
    else
        die "need curl or wget to download files, found neither"
    fi
}

# download <url> <dest>
download() {
    case "$DOWNLOADER" in
        curl) curl -fsSL --proto '=https' --tlsv1.2 -o "$2" "$1" ;;
        wget) wget -q --https-only -O "$2" "$1" ;;
    esac
}

# ---------------------------------------------------------------- platform

# Maps uname output onto the platform tags used by the release assets. Kept in
# sync with murmur-artifact/src/platform.rs — the canonical list lives there.
detect_platform() {
    os="$(uname -s)"
    arch="$(uname -m)"

    case "$os" in
        Darwin) os_tag="darwin" ;;
        Linux) os_tag="linux" ;;
        MINGW* | MSYS* | CYGWIN* | Windows_NT)
            die "Windows is not supported yet. Build from source with \`cargo install murmur-cli\`, or check https://github.com/${REPO}/releases for a Windows build."
            ;;
        *)
            die "unsupported operating system: ${os}. See https://github.com/${REPO}/releases for the platforms we publish."
            ;;
    esac

    case "$arch" in
        arm64 | aarch64) arch_tag="aarch64" ;;
        x86_64 | amd64) arch_tag="x86_64" ;;
        *)
            die "unsupported CPU architecture: ${arch}. See https://github.com/${REPO}/releases for the platforms we publish."
            ;;
    esac

    PLATFORM="${os_tag}-${arch_tag}"

    # Linux arm64 has a platform tag but no published binary yet: name it
    # explicitly rather than 404ing on a plausible-looking asset name.
    if [ "$PLATFORM" = "linux-aarch64" ]; then
        die "linux-aarch64 binaries are not published yet. Build from source with \`cargo install murmur-cli\`, or watch https://github.com/${REPO}/releases for a linux-aarch64 build."
    fi
}

# ---------------------------------------------------------------- version

# Resolves the latest tag by following the /releases/latest redirect rather than
# hitting api.github.com, which rate-limits anonymous callers to 60 req/hour and
# would break this script for exactly the high-volume case it exists to serve.
resolve_latest_version() {
    latest_url="https://github.com/${REPO}/releases/latest"

    case "$DOWNLOADER" in
        curl)
            effective="$(curl -fsSL --proto '=https' --tlsv1.2 -o /dev/null -w '%{url_effective}' "$latest_url" 2>/dev/null)" ||
                die "could not reach GitHub to resolve the latest release. Check your network, or pin a version with MUR_VERSION=x.y.z."
            ;;
        wget)
            # No --write-out equivalent: read the final Location header instead.
            effective="$(wget -q --https-only --spider -S "$latest_url" 2>&1 |
                awk '/[Ll]ocation: /{ url = $2 } END { print url }')"
            [ -n "$effective" ] ||
                die "could not reach GitHub to resolve the latest release. Check your network, or pin a version with MUR_VERSION=x.y.z."
            ;;
    esac

    # GitHub 301s renamed repos to their new name, so a stale or mistyped REPO can
    # land us on a *different* project's release. Refuse rather than resolve a
    # version that belongs to someone else's tags.
    case "$effective" in
        "https://github.com/${REPO}/releases/tag/"*) ;;
        *)
            die "https://github.com/${REPO}/releases/latest redirected to another repository:
  ${effective}
Refusing to install from it. Check that ${REPO} is correct, or pin a known version with MUR_VERSION=x.y.z."
            ;;
    esac

    # .../releases/tag/v1.2.3 -> v1.2.3
    tag="${effective##*/}"

    case "$tag" in
        v[0-9]*) ;;
        *) die "could not parse a release tag out of '${effective}'. Pin a version with MUR_VERSION=x.y.z." ;;
    esac

    VERSION="${tag#v}"
}

# ---------------------------------------------------------------- install dir

# Picks a system-wide directory when it is writable, otherwise a per-user one.
# Never uses sudo: this script is designed to be piped into sh, where prompting
# for a password is both hostile and unreliable.
detect_install_dir() {
    if [ -n "${MUR_INSTALL_DIR:-}" ]; then
        INSTALL_DIR="$MUR_INSTALL_DIR"
        mkdir -p "$INSTALL_DIR" 2>/dev/null ||
            die "cannot create install directory ${INSTALL_DIR}"
        [ -w "$INSTALL_DIR" ] || die "install directory is not writable: ${INSTALL_DIR}"
        return
    fi

    if [ -d /usr/local/bin ] && [ -w /usr/local/bin ]; then
        INSTALL_DIR="/usr/local/bin"
        return
    fi

    INSTALL_DIR="${HOME}/.local/bin"
    mkdir -p "$INSTALL_DIR" 2>/dev/null ||
        die "cannot create install directory ${INSTALL_DIR}. Set MUR_INSTALL_DIR to a writable directory."
    [ -w "$INSTALL_DIR" ] ||
        die "install directory is not writable: ${INSTALL_DIR}. Set MUR_INSTALL_DIR to a writable directory."
}

on_path() {
    case ":${PATH}:" in
        *":$1:"*) return 0 ;;
        *) return 1 ;;
    esac
}

# ---------------------------------------------------------------- checksum

# asset_in_checksums <asset_name> <checksums_file>
#
# Whether the release published this asset at all, asked before downloading it. An asset
# missing from checksums.txt is one this release does not carry — a different state from an
# asset that is carried and fails to verify.
asset_in_checksums() {
    awk -v name="$1" '$2 == name || $2 == "*" name { found = 1 } END { exit !found }' "$2"
}

# verify_checksum <file> <asset_name> <checksums_file> [soft]
#
# Dies on any failure by default — the main `mur` binary must never be installed
# unverified. Pass "soft" for an optional asset (the AppArmor profile) whose own
# doc comment promises it never aborts the install: with "soft", a bad checksum
# warns and returns 1 instead of calling `die`, so a corrupt or tampered profile
# download costs one containment class, not the whole run.
#
# `sh` has no local variables, so every name here is underscore-prefixed: an
# unprefixed `asset` would overwrite main's, and main still needs its own after
# the call to name the asset in later messages.
verify_checksum() {
    _sum_file="$1"
    _sum_asset="$2"
    _sums="$3"
    _sum_soft="${4:-}"

    _expected="$(awk -v name="$_sum_asset" '$2 == name || $2 == "*" name { print $1; exit }' "$_sums")"
    if [ -z "$_expected" ]; then
        if [ "$_sum_soft" = "soft" ]; then
            warn "no checksum for ${_sum_asset} in ${CHECKSUMS_FILE} — not installing an unverified file."
            return 1
        fi
        die "no checksum for ${_sum_asset} in ${CHECKSUMS_FILE} — refusing to install an unverified binary."
    fi

    if command -v sha256sum >/dev/null 2>&1; then
        _actual="$(sha256sum "$_sum_file" | awk '{print $1}')"
    elif command -v shasum >/dev/null 2>&1; then
        _actual="$(shasum -a 256 "$_sum_file" | awk '{print $1}')"
    else
        if [ "$_sum_soft" = "soft" ]; then
            warn "need sha256sum or shasum to verify ${_sum_asset}, found neither — not installing an unverified file."
            return 1
        fi
        die "need sha256sum or shasum to verify the download, found neither."
    fi

    if [ "$_actual" != "$_expected" ]; then
        if [ "$_sum_soft" = "soft" ]; then
            warn "checksum mismatch for ${_sum_asset}
  expected: ${_expected}
  actual:   ${_actual}
The download may be corrupt or tampered with. Not installed."
            return 1
        fi
        die "checksum mismatch for ${_sum_asset}
  expected: ${_expected}
  actual:   ${_actual}
The download may be corrupt or tampered with. Nothing was installed."
    fi
}

# ---------------------------------------------------------------- exec check

# verify_runs <file> <asset_name>
#
#   <file>        the staged binary inside INSTALL_DIR, already chmod 755
#   <asset_name>  the release asset it came from, for the failure message
#
# Runs the staged binary once and refuses the whole install if this host cannot
# exec it. Every other check in this script asks whether the bytes are the right
# bytes — the release resolves, the asset is listed, the sha256 matches — and none
# of them asks whether the result starts. Without this one, a release built against
# a newer glibc than it promises reports a successful install and leaves behind a
# `mur` that can only ever print a loader error.
#
# Cause-agnostic by construction: it classifies nothing, so it catches a glibc
# requirement above the floor, a missing libseccomp.so.2, the wrong architecture,
# and a download that checksummed and still is not a program. The loader's own
# message is printed verbatim rather than summarised — it names the exact library
# and version that is missing, which no message written here could.
#
# Runs against the staged file, before the rename. The staged path is inside
# INSTALL_DIR, so it has the same filesystem, the same mode and the same exec
# semantics as the target: the check is exactly as strong as one run after the
# rename, and nothing that cannot run ever reaches PATH. On failure it removes both
# staging files and dies, so refusing a bad install is never worse than not having
# run the installer at all.
verify_runs() {
    _file="$1"
    _asset="$2"
    _stderr_file="${TMPDIR_INSTALL}/${_asset}.exec-check"

    if "$_file" --version >/dev/null 2>"$_stderr_file"; then
        info "${_asset} runs on this host"
        return 0
    else
        _status=$?
    fi

    rm -f "$staged" "$roost_staged"
    die "${_asset} was downloaded and verified, but it does not run on this host (exit ${_status}):

$(cat "$_stderr_file" 2>/dev/null)

The published linux-x86_64 binaries require glibc ${SUPPORTED_GLIBC} or newer (Debian 11+, Ubuntu 20.04+, RHEL 9+), the shared libraries mur links (libseccomp.so.2), and a matching CPU architecture. Nothing was installed and no existing install was changed. On a host older than that, build from source with \`cargo install murmur-cli\`."
}

# ---------------------------------------------------------------- apparmor

# Installs and loads the `mur-sealed` AppArmor profile: this runtime's whole
# capability-grant mechanism, and the only one it has. There is no `setcap` step and
# no setuid binary — every namespace `mur` creates is an *unprivileged* one, made
# inside a user namespace the calling process owns, so the only thing a host has to
# grant is permission to call `unshare(CLONE_NEWUSER)` at all. On the AppArmor hosts
# that restrict that (Ubuntu 23.10+), this profile is that permission.
#
# It gates two mechanisms, not one. It has always been what makes
# `capabilities.containment: sealed` achievable; since the network namespace replaced
# the seccomp connect/sendto interception it is also what lets the runtime build the
# egress boundary that enforces `capabilities.network.allow` for native subprocesses
# — at every containment class, `advisory` included.
#
# Warns and continues on every failure, never `die`s. `mur` must still install on a
# host with no AppArmor (Fedora, Arch, macOS), on a host where this script is not
# root, and on a host whose operator does not want any of this. The install itself
# never depends on the grant; only what a capsule can then do does.
#
# Like the rest of this script it never invokes sudo — it prints the two commands to
# run instead. Prompting for a password inside `curl | sh` is both hostile and
# unreliable.
install_apparmor_profile() {
    # Not Linux, or no AppArmor: nothing to install and nothing missing.
    [ "$os_tag" = "linux" ] || return 0
    if [ ! -d /sys/module/apparmor ]; then
        return 0
    fi

    manual="  sudo install -m 644 <murmur checkout>/packaging/apparmor/${APPARMOR_PROFILE_NAME} ${APPARMOR_PROFILE_DIR}/${APPARMOR_PROFILE_NAME}
  sudo apparmor_parser -r ${APPARMOR_PROFILE_DIR}/${APPARMOR_PROFILE_NAME}"

    if ! command -v apparmor_parser >/dev/null 2>&1; then
        warn "AppArmor is enabled on this host but apparmor_parser was not found, so the ${APPARMOR_PROFILE_NAME} profile was not installed.
mur is installed and works normally, but on a host with kernel.apparmor_restrict_unprivileged_userns=1 the runtime cannot create the network namespace that enforces capabilities.network.allow for native subprocesses: every capsule with a capabilities.shell.allow list refuses to launch with error[E-CAP-005], and capsules declaring \`capabilities.containment: sealed\` refuse with error[E-CAP-003]. Capsules that spawn no subprocess are unaffected. Install the AppArmor userspace tools (Debian/Ubuntu: apparmor-utils) and then run:
${manual}"
        return 0
    fi

    # Prefer a copy sitting next to this script in a checkout; fall back to the
    # release asset for the version being installed. An asset that is present in
    # checksums.txt is verified against it — an unverified file is never written
    # into /etc/apparmor.d, matching how the binary itself is handled above.
    script_dir="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd 2>/dev/null)" || script_dir=""
    profile_src=""
    if [ -n "$script_dir" ] && [ -f "${script_dir}/../packaging/apparmor/${APPARMOR_PROFILE_NAME}" ]; then
        profile_src="${script_dir}/../packaging/apparmor/${APPARMOR_PROFILE_NAME}"
    elif [ -n "${TMPDIR_INSTALL:-}" ]; then
        asset_name="${APPARMOR_PROFILE_NAME}.apparmor"
        if download "${base_url}/${asset_name}" "${TMPDIR_INSTALL}/${asset_name}" 2>/dev/null; then
            if asset_in_checksums "$asset_name" "${TMPDIR_INSTALL}/${CHECKSUMS_FILE}"; then
                if verify_checksum "${TMPDIR_INSTALL}/${asset_name}" "$asset_name" \
                    "${TMPDIR_INSTALL}/${CHECKSUMS_FILE}" soft; then
                    profile_src="${TMPDIR_INSTALL}/${asset_name}"
                else
                    warn "mur is installed and works normally, but on a host with kernel.apparmor_restrict_unprivileged_userns=1 the runtime cannot create the network namespace that enforces capabilities.network.allow for native subprocesses: every capsule with a capabilities.shell.allow list refuses to launch with error[E-CAP-005], and capsules declaring \`capabilities.containment: sealed\` refuse with error[E-CAP-003]. Capsules that spawn no subprocess are unaffected. From a murmur checkout, run:
${manual}"
                    return 0
                fi
            else
                warn "the ${APPARMOR_PROFILE_NAME} AppArmor profile is not listed in ${CHECKSUMS_FILE} for this release, so it was not installed — this script does not write unverified files into ${APPARMOR_PROFILE_DIR}.
mur is installed and works normally, but on a host with kernel.apparmor_restrict_unprivileged_userns=1 the runtime cannot create the network namespace that enforces capabilities.network.allow for native subprocesses: every capsule with a capabilities.shell.allow list refuses to launch with error[E-CAP-005], and capsules declaring \`capabilities.containment: sealed\` refuse with error[E-CAP-003]. Capsules that spawn no subprocess are unaffected. From a murmur checkout, run:
${manual}"
                return 0
            fi
        fi
    fi

    if [ -z "$profile_src" ]; then
        warn "could not obtain the ${APPARMOR_PROFILE_NAME} AppArmor profile, so it was not installed.
mur is installed and works normally, but on a host with kernel.apparmor_restrict_unprivileged_userns=1 the runtime cannot create the network namespace that enforces capabilities.network.allow for native subprocesses: every capsule with a capabilities.shell.allow list refuses to launch with error[E-CAP-005], and capsules declaring \`capabilities.containment: sealed\` refuse with error[E-CAP-003]. Capsules that spawn no subprocess are unaffected. From a murmur checkout, run:
${manual}"
        return 0
    fi

    if [ "$(id -u)" != "0" ]; then
        warn "installing the ${APPARMOR_PROFILE_NAME} AppArmor profile needs root, and this script never invokes sudo.
mur is installed and works normally, but on a host with kernel.apparmor_restrict_unprivileged_userns=1 the runtime cannot create the network namespace that enforces capabilities.network.allow for native subprocesses: every capsule with a capabilities.shell.allow list refuses to launch with error[E-CAP-005], and capsules declaring \`capabilities.containment: sealed\` refuse with error[E-CAP-003]. Capsules that spawn no subprocess are unaffected. Run:
  sudo install -m 644 ${profile_src} ${APPARMOR_PROFILE_DIR}/${APPARMOR_PROFILE_NAME}
  sudo apparmor_parser -r ${APPARMOR_PROFILE_DIR}/${APPARMOR_PROFILE_NAME}"
        return 0
    fi

    # Parse before writing, so a profile this host's AppArmor cannot understand never
    # reaches /etc/apparmor.d — the same "verify, then move into place" ordering the
    # binary install uses.
    if ! apparmor_parser -Q "$profile_src" >/dev/null 2>&1; then
        warn "the ${APPARMOR_PROFILE_NAME} AppArmor profile did not parse on this host's AppArmor version, so it was not installed. Nothing was written to ${APPARMOR_PROFILE_DIR}.
mur is installed and works normally, but on a host with kernel.apparmor_restrict_unprivileged_userns=1 the runtime cannot create the network namespace that enforces capabilities.network.allow for native subprocesses: every capsule with a capabilities.shell.allow list refuses to launch with error[E-CAP-005], and capsules declaring \`capabilities.containment: sealed\` refuse with error[E-CAP-003]. Capsules that spawn no subprocess are unaffected."
        return 0
    fi

    if ! install -m 644 "$profile_src" "${APPARMOR_PROFILE_DIR}/${APPARMOR_PROFILE_NAME}" 2>/dev/null; then
        warn "could not write ${APPARMOR_PROFILE_DIR}/${APPARMOR_PROFILE_NAME}.
mur is installed and works normally, but on a host with kernel.apparmor_restrict_unprivileged_userns=1 the runtime cannot create the network namespace that enforces capabilities.network.allow for native subprocesses: every capsule with a capabilities.shell.allow list refuses to launch with error[E-CAP-005], and capsules declaring \`capabilities.containment: sealed\` refuse with error[E-CAP-003]. Capsules that spawn no subprocess are unaffected. Install it as root with:
${manual}"
        return 0
    fi

    if ! apparmor_parser -r "${APPARMOR_PROFILE_DIR}/${APPARMOR_PROFILE_NAME}" >/dev/null 2>&1; then
        warn "wrote ${APPARMOR_PROFILE_DIR}/${APPARMOR_PROFILE_NAME} but could not load it.
mur is installed and works normally, but on a host with kernel.apparmor_restrict_unprivileged_userns=1 the runtime cannot create the network namespace that enforces capabilities.network.allow for native subprocesses: every capsule with a capabilities.shell.allow list refuses to launch with error[E-CAP-005], and capsules declaring \`capabilities.containment: sealed\` refuse with error[E-CAP-003]. Capsules that spawn no subprocess are unaffected. Retry with:
  sudo apparmor_parser -r ${APPARMOR_PROFILE_DIR}/${APPARMOR_PROFILE_NAME}"
        return 0
    fi

    info "loaded AppArmor profile ${APPARMOR_PROFILE_NAME} (native-subprocess network enforcement and capabilities.containment: sealed are now available)"
}

# ---------------------------------------------------------------- main

main() {
    detect_downloader
    need_cmd uname
    need_cmd awk
    need_cmd mktemp

    detect_platform

    if [ -n "${MUR_VERSION:-}" ]; then
        VERSION="${MUR_VERSION#v}"
    else
        resolve_latest_version
    fi

    detect_install_dir

    asset="mur-${VERSION}-${PLATFORM}"
    roost_asset="mur-roost-${VERSION}-${PLATFORM}"
    base_url="https://github.com/${REPO}/releases/download/v${VERSION}"
    target="${INSTALL_DIR}/mur"
    roost_target="${INSTALL_DIR}/mur-roost"

    info "installing mur ${VERSION} (${PLATFORM}) to ${INSTALL_DIR}"

    TMPDIR_INSTALL="$(mktemp -d)" || die "could not create a temporary directory"
    trap cleanup EXIT INT TERM HUP

    download "${base_url}/${asset}" "${TMPDIR_INSTALL}/mur" ||
        die "could not download ${asset} from ${base_url}
The release may not include a binary for ${PLATFORM}. See https://github.com/${REPO}/releases."

    download "${base_url}/${CHECKSUMS_FILE}" "${TMPDIR_INSTALL}/${CHECKSUMS_FILE}" ||
        die "could not download ${CHECKSUMS_FILE} from ${base_url} — refusing to install an unverified binary."

    verify_checksum "${TMPDIR_INSTALL}/mur" "$asset" "${TMPDIR_INSTALL}/${CHECKSUMS_FILE}"
    info "checksum verified"

    # mur-roost is the daemon a capsule declaring capabilities.spawn.allow registers with at
    # launch. It comes from the same release, at the same version, and both binaries are
    # downloaded and verified before either is moved into place — a release that lists the asset
    # and cannot deliver it installs nothing.
    #
    # A release that does not list it in checksums.txt does not carry it: warn and install `mur`
    # alone, so installing an older release stays possible.
    install_roost=""
    if asset_in_checksums "$roost_asset" "${TMPDIR_INSTALL}/${CHECKSUMS_FILE}"; then
        download "${base_url}/${roost_asset}" "${TMPDIR_INSTALL}/mur-roost" ||
            die "could not download ${roost_asset} from ${base_url}, and this release lists it in ${CHECKSUMS_FILE}. Nothing was installed."
        verify_checksum "${TMPDIR_INSTALL}/mur-roost" "$roost_asset" "${TMPDIR_INSTALL}/${CHECKSUMS_FILE}"
        install_roost=1
        info "checksum verified for ${roost_asset}"
    else
        warn "release v${VERSION} carries no ${roost_asset}, so mur-roost was not installed.
mur is installed and works normally, but a capsule declaring capabilities.spawn.allow has no daemon to register with: \`mur run\` refuses it with error[E-RUN-019]. Install a release that ships mur-roost to get the daemon."
    fi

    # Report what is being replaced before overwriting it.
    if [ -e "$target" ]; then
        previous="$("$target" --version 2>/dev/null | head -1)" || previous=""
        if [ -n "$previous" ]; then
            info "replacing existing install: ${previous}"
        else
            info "replacing existing install at ${target}"
        fi
    fi

    chmod 755 "${TMPDIR_INSTALL}/mur"
    [ -z "$install_roost" ] || chmod 755 "${TMPDIR_INSTALL}/mur-roost"

    # Stage inside the install directory so the final step is a same-filesystem
    # rename: atomic, and it replaces a running binary without a "text file busy"
    # failure. Both paths are inside INSTALL_DIR, so nothing outside it is touched.
    staged="${INSTALL_DIR}/.mur.install.$$"
    roost_staged="${INSTALL_DIR}/.mur-roost.install.$$"
    if ! cp "${TMPDIR_INSTALL}/mur" "$staged"; then
        rm -f "$staged"
        die "could not write to ${INSTALL_DIR}"
    fi
    if [ -n "$install_roost" ] && ! cp "${TMPDIR_INSTALL}/mur-roost" "$roost_staged"; then
        rm -f "$staged" "$roost_staged"
        die "could not write to ${INSTALL_DIR}"
    fi
    # Both binaries are exec-verified before either is moved into place, the same
    # all-or-nothing rule the checksum leg holds to: a mur-roost that cannot start
    # fails a delegation launch with something far less obvious than a loader error.
    verify_runs "$staged" "$asset"
    [ -z "$install_roost" ] || verify_runs "$roost_staged" "$roost_asset"

    if ! mv -f "$staged" "$target"; then
        rm -f "$staged" "$roost_staged"
        die "could not install to ${target}"
    fi

    info "installed mur ${VERSION} to ${target}"

    if [ -n "$install_roost" ]; then
        if ! mv -f "$roost_staged" "$roost_target"; then
            rm -f "$roost_staged"
            die "could not install to ${roost_target}"
        fi
        info "installed mur-roost ${VERSION} to ${roost_target}"
    fi

    install_apparmor_profile

    if ! on_path "$INSTALL_DIR"; then
        warn "${INSTALL_DIR} is not on your PATH. Add it to your shell profile:

    export PATH=\"${INSTALL_DIR}:\$PATH\"
"
    fi

    # shellcheck disable=SC2016  # backticks are literal text in the hint, not a subshell
    printf '\nRun `mur doctor` to verify your setup.\n'
}

main "$@"
