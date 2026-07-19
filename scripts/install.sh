#!/bin/sh
#
# mur installer — https://github.com/murmur-nexus/murmur
#
#   curl -fsSL https://install.murmur.rs | sh
#
# Detects the platform, resolves the latest release, verifies the download against
# the release's checksums.txt, and installs `mur` onto PATH.
#
# Environment overrides:
#   MUR_VERSION      version to install, with or without the leading "v" (default: latest)
#   MUR_INSTALL_DIR  directory to install into (default: /usr/local/bin, else ~/.local/bin)
#   MUR_REPO         owner/repo to install from (default: murmur-nexus/murmur)

set -eu

DEFAULT_REPO="murmur-nexus/murmur"
REPO="${MUR_REPO:-$DEFAULT_REPO}"
CHECKSUMS_FILE="checksums.txt"

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

# verify_checksum <file> <asset_name> <checksums_file>
verify_checksum() {
    file="$1"
    asset="$2"
    sums="$3"

    expected="$(awk -v name="$asset" '$2 == name || $2 == "*" name { print $1; exit }' "$sums")"
    [ -n "$expected" ] ||
        die "no checksum for ${asset} in ${CHECKSUMS_FILE} — refusing to install an unverified binary."

    if command -v sha256sum >/dev/null 2>&1; then
        actual="$(sha256sum "$file" | awk '{print $1}')"
    elif command -v shasum >/dev/null 2>&1; then
        actual="$(shasum -a 256 "$file" | awk '{print $1}')"
    else
        die "need sha256sum or shasum to verify the download, found neither."
    fi

    if [ "$actual" != "$expected" ]; then
        die "checksum mismatch for ${asset}
  expected: ${expected}
  actual:   ${actual}
The download may be corrupt or tampered with. Nothing was installed."
    fi
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
    base_url="https://github.com/${REPO}/releases/download/v${VERSION}"
    target="${INSTALL_DIR}/mur"

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

    # Stage inside the install directory so the final step is a same-filesystem
    # rename: atomic, and it replaces a running binary without a "text file busy"
    # failure. Both paths are inside INSTALL_DIR, so nothing outside it is touched.
    staged="${INSTALL_DIR}/.mur.install.$$"
    cp "${TMPDIR_INSTALL}/mur" "$staged" || die "could not write to ${INSTALL_DIR}"
    if ! mv -f "$staged" "$target"; then
        rm -f "$staged"
        die "could not install to ${target}"
    fi

    info "installed mur ${VERSION} to ${target}"

    if ! on_path "$INSTALL_DIR"; then
        warn "${INSTALL_DIR} is not on your PATH. Add it to your shell profile:

    export PATH=\"${INSTALL_DIR}:\$PATH\"
"
    fi

    # shellcheck disable=SC2016  # backticks are literal text in the hint, not a subshell
    printf '\nRun `mur doctor` to verify your setup.\n'
}

main "$@"
