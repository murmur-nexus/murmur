#!/bin/sh
#
# check-glibc-floor.sh — refuse a Linux binary the distributions we claim to
# support cannot exec, and refuse a build configuration that would produce one.
#
# Two modes, one declared floor. Both read scripts/lib/glibc-floor.sh.
#
#   scripts/check-glibc-floor.sh [--floor <x.y>] <file> [<file>...]
#
#       Binary mode. For each ELF, reads the versioned-symbol requirements the
#       dynamic loader will check and fails when any required GLIBC_x.y is above
#       the floor. This is the release gate: it runs between the build and the
#       upload, so a binary that raises the floor fails the job instead of being
#       published. --floor overrides the declared floor for a one-off question
#       ("what does this binary actually need?"); it does not change what a
#       release enforces.
#
#   scripts/check-glibc-floor.sh --config [--root <dir>]
#
#       Configuration mode. Asserts that the release workflow still builds in the
#       image the floor names, and that README.md and scripts/install.sh state
#       the floor the build enforces. Reads no binary and needs no toolchain, so
#       it runs on every pull request: a runner image moving away from the pinned
#       distro raises the floor silently, and this makes it arrive as a failing
#       diff instead. --root checks a copy of the tree somewhere else; every path
#       it reads is repo-relative to that root.
#
# Exit codes:
#   0  every file is at or under the floor / the configuration agrees
#   1  a binary requires more than the floor, or the configuration disagrees
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
       check-glibc-floor.sh --config [--root <dir>]

  --floor <x.y>  compare against this version instead of the declared floor ($GLIBC_FLOOR)
  --config       check the declared floor against the workflow, README and installer
  --root <dir>   in --config mode, read the tree rooted at <dir> instead of the current directory
USAGE
    exit 2
}

mode="binary"
floor="$GLIBC_FLOOR"
root="."

while [ $# -gt 0 ]; do
    case "$1" in
        --config) mode="config"; shift ;;
        --floor)
            [ $# -ge 2 ] || usage "--floor needs a version"
            floor="$2"
            shift 2
            ;;
        --floor=*) floor="${1#--floor=}"; shift ;;
        --root)
            [ $# -ge 2 ] || usage "--root needs a directory"
            root="$2"
            shift 2
            ;;
        --root=*) root="${1#--root=}"; shift ;;
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

# ---------------------------------------------------------------- config mode

# The floor as stated to a person, extracted from a file that states it in prose.
# Both surfaces write it as "glibc <x.y>"; install.sh additionally carries it as a
# shell variable, because its failure message quotes the number back to whoever
# just failed to install.
stated_floor() {
    {
        grep -oE 'glibc [0-9]+\.[0-9]+' "$1" | awk '{ print $2 }'
        sed -n 's/^SUPPORTED_GLIBC="\([0-9][0-9.]*\)".*/\1/p' "$1"
    } | sort -u
}

check_config() {
    workflow="${root}/${GLIBC_FLOOR_WORKFLOW}"
    readme="${root}/${GLIBC_FLOOR_README}"
    installer="${root}/${GLIBC_FLOOR_INSTALLER}"
    failed=0

    for required in "$workflow" "$readme"; do
        [ -f "$required" ] || {
            echo "error: ${required} not found — run this from the repository root, or pass --root <dir>" >&2
            exit 2
        }
    done

    # 1. The release workflow still builds where the floor says it does.
    image=$(sed -n 's/^[[:space:]]*image:[[:space:]]*\([^[:space:]#]*\).*/\1/p' "$workflow" |
        tr -d '"'"'" | sort -u)
    if [ -z "$image" ]; then
        echo "error: ${workflow} declares no build container image." >&2
        echo "  The linux-x86_64 build must run in ${GLIBC_FLOOR_IMAGE}: its glibc is the declared" >&2
        echo "  floor of ${GLIBC_FLOOR}, so a binary built in it cannot exceed the floor. A build on the" >&2
        echo "  runner's own image inherits that runner's glibc, which is how the floor moved before." >&2
        failed=1
    elif [ "$image" != "$GLIBC_FLOOR_IMAGE" ]; then
        echo "error: ${workflow} builds in an image the declared floor does not allow." >&2
        echo "  workflow builds in:    ${image}" >&2
        echo "  floor ${GLIBC_FLOOR} requires: ${GLIBC_FLOOR_IMAGE}" >&2
        echo "  A binary built in ${image} requires whatever versioned symbols that image's glibc" >&2
        echo "  offers, so this raises the floor silently — no build step fails, and the release" >&2
        echo "  ships a binary that will not start on ${GLIBC_FLOOR_DISTROS}." >&2
        echo "  To move the floor deliberately, change GLIBC_FLOOR and GLIBC_FLOOR_IMAGE in" >&2
        echo "  ${GLIBC_FLOOR_LIB}; see its header for what else must move with them." >&2
        failed=1
    fi

    # 2. The surfaces an operator reads state the floor the build enforces. A
    #    number nobody can install against is worse than no number.
    for surface in "$readme" "$installer"; do
        # install.sh is optional under --root so that a mutated copy of the tree
        # can carry only the file under test; both are always present in a repo.
        if [ ! -f "$surface" ]; then
            echo "note: ${surface} is not present under ${root}, so its statement of the floor was not checked"
            continue
        fi
        stated=$(stated_floor "$surface")
        if [ -z "$stated" ]; then
            echo "error: ${surface} does not state a glibc floor." >&2
            echo "  It is a surface an operator installing murmur reads, and the build enforces" >&2
            echo "  glibc ${GLIBC_FLOOR} (${GLIBC_FLOOR_DISTROS})." >&2
            failed=1
        elif [ "$stated" != "$GLIBC_FLOOR" ]; then
            echo "error: ${surface} states a floor the build does not enforce." >&2
            echo "  ${surface} states: $(printf '%s' "$stated" | tr '\n' ' ')" >&2
            echo "  the build enforces: ${GLIBC_FLOOR}" >&2
            echo "  Change ${GLIBC_FLOOR_LIB} and every surface in one commit, or an operator reads a" >&2
            echo "  promise the release does not keep." >&2
            failed=1
        fi
    done

    [ "$failed" -eq 0 ] || exit 1
    echo "ok: glibc floor ${GLIBC_FLOOR} — ${GLIBC_FLOOR_WORKFLOW} builds in ${GLIBC_FLOOR_IMAGE}, ${GLIBC_FLOOR_README} and ${GLIBC_FLOOR_INSTALLER} state it"
}

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
        echo "  Build it in ${GLIBC_FLOOR_IMAGE}, whose glibc is the floor. To move the floor" >&2
        echo "  deliberately, change GLIBC_FLOOR in ${GLIBC_FLOOR_LIB} — see its header for what" >&2
        echo "  else must move with it." >&2
        violation=1
    done

    [ "$unreadable" -eq 0 ] || exit 2
    [ "$violation" -eq 0 ] || exit 1
}

if [ "$mode" = "config" ]; then
    [ $# -eq 0 ] || usage "--config takes no file arguments, got: $1"
    check_config
else
    [ $# -gt 0 ] || usage "no files to check"
    check_binaries "$@"
fi
