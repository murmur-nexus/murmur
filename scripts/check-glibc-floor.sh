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
#       the floor. This is the honest check: it is the only one of the two that
#       can answer whether the promise is true, so it runs both on every pull
#       request over freshly built binaries and at release time between the build
#       and the upload, where a binary that raises the floor fails the job instead
#       of being published. --floor overrides the declared floor for a one-off
#       question ("what does this binary actually need?"); it does not change what
#       a release enforces.
#
#   scripts/check-glibc-floor.sh --config [--root <dir>]
#
#       Configuration mode. Asserts that the release workflow still builds at the
#       target the floor pins, that it decides the floor with that pin rather than
#       with a build container, and that README.md, scripts/install.sh and the
#       roost API reference state the floor the build enforces. Reads no binary and
#       needs no toolchain. It checks four declarations against each other and
#       cannot tell whether the promise they encode is *true*, so it is the cheap
#       companion to binary mode and not a substitute: what it catches is the pin
#       being removed or a surface drifting. --root checks a copy of the tree
#       somewhere else; every path it reads is repo-relative to that root.
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
  --config       check the declared floor against the workflow, README, installer and docs
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
# All three surfaces write it as "glibc <x.y>"; install.sh additionally carries it
# as a shell variable, because its failure message quotes the number back to
# whoever just failed to install.
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
    docs="${root}/${GLIBC_FLOOR_DOCS}"
    failed=0

    for required in "$workflow" "$readme"; do
        [ -f "$required" ] || {
            echo "error: ${required} not found — run this from the repository root, or pass --root <dir>" >&2
            exit 2
        }
    done

    # 1. The linux build still carries the glibc pin the floor names. Only literal
    #    x86_64-unknown-linux-gnu targets are read: the macOS jobs pass
    #    `--target ${{ matrix.target }}` and say nothing about glibc.
    target=$(grep -oE '\-\-target[[:space:]]+x86_64-unknown-linux-gnu[0-9.]*' "$workflow" |
        sed 's/^--target[[:space:]]*//' | sort -u)
    if [ -z "$target" ]; then
        echo "error: ${workflow} builds no x86_64-unknown-linux-gnu target." >&2
        echo "  The linux-x86_64 build must be pinned to ${GLIBC_FLOOR_TARGET}: the version" >&2
        echo "  suffix is what holds the declared floor of ${GLIBC_FLOOR}, and without it the build" >&2
        echo "  links against the runner's own glibc, which is how the floor moved before." >&2
        failed=1
    elif [ "$target" != "$GLIBC_FLOOR_TARGET" ]; then
        echo "error: ${workflow} builds a target the declared floor does not allow." >&2
        echo "  workflow builds:       $(printf '%s' "$target" | tr '\n' ' ')" >&2
        echo "  floor ${GLIBC_FLOOR} requires: ${GLIBC_FLOOR_TARGET}" >&2
        echo "  An unsuffixed target inherits the runner's glibc, so the binary requires whatever" >&2
        echo "  versioned symbols that runner offers. Nothing fails at build time, and the release" >&2
        echo "  ships a binary that will not start on ${GLIBC_FLOOR_DISTROS}." >&2
        echo "  To move the floor deliberately, change GLIBC_FLOOR and GLIBC_FLOOR_TARGET in" >&2
        echo "  ${GLIBC_FLOOR_LIB}; see its header for what else must move with them." >&2
        failed=1
    fi

    # 2. Nothing else decides the floor. A build container's glibc overrides the
    #    pin's intent silently — the pin would still read as satisfied here while
    #    an image nobody re-derived picked the real floor.
    # A `container:` key counts whether or not it carries a value on its own line:
    # the block form puts the image on a nested `image:` line, and a `container:`
    # holding only `options:` still runs the build somewhere other than the runner.
    container=$(sed -n \
        -e 's/^[[:space:]]*container:[[:space:]]*\([^[:space:]#]\{1,\}\).*/\1/p' \
        -e 's/^[[:space:]]*image:[[:space:]]*\([^[:space:]#]\{1,\}\).*/\1/p' \
        "$workflow" | tr -d '"'"'" | sort -u)
    if grep -qE '^[[:space:]]*container:' "$workflow" || [ -n "$container" ]; then
        echo "error: ${workflow} declares a build container." >&2
        echo "  declares: $(printf '%s' "${container:-container:}" | tr '\n' ' ')" >&2
        echo "  The floor comes from ${GLIBC_FLOOR_TARGET} and from nothing else. An image" >&2
        echo "  decides the glibc a binary links against too, so leaving one in place is a second," >&2
        echo "  unchecked answer to where the floor comes from — and an image's floor expires when" >&2
        echo "  its distribution goes out of support, which a pinned target's does not." >&2
        failed=1
    fi

    # 3. The surfaces an operator reads state the floor the build enforces. A
    #    number nobody can install against is worse than no number.
    for surface in "$readme" "$installer" "$docs"; do
        # install.sh and the docs page are optional under --root so that a mutated
        # copy of the tree can carry only the file under test; all three are always
        # present in a repo.
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
    echo "ok: glibc floor ${GLIBC_FLOOR} — ${GLIBC_FLOOR_WORKFLOW} builds ${GLIBC_FLOOR_TARGET} with no build container, ${GLIBC_FLOOR_README}, ${GLIBC_FLOOR_INSTALLER} and ${GLIBC_FLOOR_DOCS} state it"
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
        echo "  Build it with \`cargo zigbuild --target ${GLIBC_FLOOR_TARGET}\`, which pins the glibc" >&2
        echo "  it links against to the floor. To move the floor deliberately, change GLIBC_FLOOR in" >&2
        echo "  ${GLIBC_FLOOR_LIB} — see its header for what else must move with it." >&2
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
