#!/bin/sh
#
# check-glibc-floor.sh — refuse a Linux binary the distributions we claim to
# support cannot exec.
#
# One mode, one declared floor. Reads scripts/lib/glibc-floor.sh.
#
#   scripts/check-glibc-floor.sh [--floor <x.y>] <file> [<file>...]
#
#       For each ELF, reads the versioned-symbol requirements the dynamic loader
#       will check and fails when any required GLIBC_x.y is above the floor. This
#       is the only check that can answer whether the floor is true, rather than
#       whether we are claiming it consistently, so it runs on every pull request
#       over freshly built binaries and again at release time between the build
#       and the upload, where a binary that raises the floor fails the job instead
#       of being published. --floor overrides the declared floor for a one-off
#       question ("what does this binary actually need?"); it does not change what
#       a release enforces, and CI passes it nowhere.
#
# Exit codes:
#   0  every file is at or under the floor
#   1  a binary requires more than the floor
#   2  bad usage, or an input that could not be read: a missing file, a non-ELF,
#      or no readelf on the host. "Requires nothing above the floor" and "I could
#      not tell" are different answers and only the first one is silent.
#
# Deliberately not covered: macOS. The floor is a glibc promise and build-macos
# does not run this. Nor does it check the *runtime* libraries a binary needs
# (`mur` links libseccomp) — an operator missing libseccomp.so.2 is caught by the
# exec check in scripts/install.sh, which is cause-agnostic where this is not.

set -eu

. "$(dirname "$0")/lib/glibc-floor.sh"

usage() {
    if [ $# -gt 0 ]; then
        echo "error: $1" >&2
        echo >&2
    fi
    cat >&2 <<USAGE
usage: check-glibc-floor.sh [--floor <x.y>] <file> [<file>...]

  --floor <x.y>  compare against this version instead of the declared floor ($GLIBC_FLOOR)
USAGE
    exit 2
}

floor="$GLIBC_FLOOR"

while [ $# -gt 0 ]; do
    case "$1" in
        --floor)
            [ $# -ge 2 ] || usage "--floor needs a version"
            floor="$2"
            shift 2
            ;;
        --floor=*) floor="${1#--floor=}"; shift ;;
        -h | --help) usage ;;
        --) shift; break ;;
        -*) usage "unknown option: $1" ;;
        *) break ;;
    esac
done

case "$floor" in
    [0-9]*.[0-9]*) ;;
    *) usage "not a glibc version: ${floor}" ;;
esac

# ---------------------------------------------------------------- binary mode

check_binaries() {
    glibc_reader
    unreadable=0
    violation=0

    for file in "$@"; do
        if [ ! -e "$file" ]; then
            echo "error: ${file}: no such file" >&2
            unreadable=1
            continue
        fi
        if ! glibc_is_elf "$file"; then
            echo "error: ${file} is not an ELF executable, so its glibc requirements cannot be read." >&2
            echo "  This gate reads the .gnu.version_r section of a linked binary. Point it at the" >&2
            echo "  built artifact, not at a script, an archive or a Mach-O." >&2
            unreadable=1
            continue
        fi

        versions=$(glibc_required_versions "$file")
        if [ -z "$versions" ]; then
            echo "ok: ${file} requires no versioned glibc symbols (floor ${floor})"
            continue
        fi

        highest=$(printf '%s\n' "$versions" | tail -n 1)
        if ! glibc_version_gt "$highest" "$floor"; then
            echo "ok: ${file} requires up to GLIBC_${highest} (floor ${floor})"
            continue
        fi

        above=""
        for version in $versions; do
            if glibc_version_gt "$version" "$floor"; then
                above="${above} GLIBC_${version}"
            fi
        done
        symbols=$(glibc_symbols_requiring "$file" "$highest")

        echo "error: ${file} requires glibc ${highest}, above the declared floor of ${floor}." >&2
        echo "  highest required: GLIBC_${highest}" >&2
        echo "  declared floor:   GLIBC_${floor}" >&2
        echo "  above the floor: ${above# }" >&2
        if [ -n "$symbols" ]; then
            echo "  symbols requiring GLIBC_${highest}:" >&2
            printf '%s\n' "$symbols" | head -5 | sed 's/^/    /' >&2
        else
            echo "  no dynamic symbol names that requirement; it comes from the version needs section alone." >&2
        fi
        echo "  This binary does not start on ${GLIBC_FLOOR_DISTROS}: the loader refuses it with" >&2
        echo "  \"version \`GLIBC_${highest}' not found\" and nothing it links is even opened." >&2
        echo "  Build it with \`cargo zigbuild --target ${GLIBC_FLOOR_TARGET}\`, which pins the glibc" >&2
        echo "  it links against to the floor. To move the floor deliberately, change GLIBC_FLOOR in" >&2
        echo "  ${GLIBC_FLOOR_LIB} — see its header for what else must move with it." >&2
        violation=1
    done

    [ "$unreadable" -eq 0 ] || exit 2
    [ "$violation" -eq 0 ] || exit 1
}

[ $# -gt 0 ] || usage "no files to check"
check_binaries "$@"
