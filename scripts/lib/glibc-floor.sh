#!/bin/sh
#
# glibc-floor.sh — the one place the linux-x86_64 glibc floor is declared.
#
# Sourced, never executed:
#
#     . "$(dirname "$0")/lib/glibc-floor.sh"
#
# The floor is a release promise: a published `linux-x86_64` binary must start on
# every host whose glibc is at least GLIBC_FLOOR. Nothing in the toolchain holds
# that promise on its own. A dynamically linked binary requires whatever versioned
# symbols the glibc that built it offered, so without a pinned build image the
# floor is silently whatever the release runner ships, and a runner image moving
# raises it with no build failure anywhere.
#
# Two declarations hold the promise, and they only mean anything together:
#
#   GLIBC_FLOOR        the highest GLIBC_x.y a published binary may require
#   GLIBC_FLOOR_IMAGE  the build image whose glibc *is* that floor
#
# Moving the floor is a deliberate act with four edits: both variables here, the
# `### Install` section of README.md, and the header of scripts/install.sh — the
# two surfaces that state the floor to the person installing. Those two are
# machine-checked against this file by `scripts/check-glibc-floor.sh --config`,
# so a floor that moves in one place and not the others fails CI rather than
# reaching an operator as a wrong promise.
#
# Contract for a sourcer:
#
#   * Have `set -eu` already in force.
#   * The path variables below are repo-relative; a caller that reads them out of
#     somewhere other than the repository root prefixes them itself.
#   * `glibc_reader` must be called before any function that reads an ELF.

# The floor itself. Debian 11 (bullseye) and Ubuntu 20.04 both ship glibc 2.31,
# and RHEL 9 ships 2.34 — so this one number covers every distribution named in
# GLIBC_FLOOR_DISTROS. Ubuntu 18.04 (2.27) and Debian 10 (2.28) are below it and
# are deliberately not covered: `cargo install murmur-cli` is the path there.
GLIBC_FLOOR="2.31"

# The image that produces exactly that floor. Bullseye's glibc *is* 2.31, so a
# binary built in it cannot require anything above the floor by construction —
# the gate is then a check on the build staying where it is, not a check the
# build could otherwise fail. Its libseccomp is 2.5.1, which clears the
# `libseccomp` crate's documented 2.5.0 minimum.
GLIBC_FLOOR_IMAGE="debian:bullseye"

# The distributions the floor covers, in the words the README and the installer
# use. Stated here so the gate's failure message and the operator-facing surfaces
# describe the same promise.
GLIBC_FLOOR_DISTROS="Debian 11+, Ubuntu 20.04+, RHEL 9+"

# The three files whose statement of the floor must agree with the two
# declarations above; see `scripts/check-glibc-floor.sh --config`. Repo-relative.
GLIBC_FLOOR_WORKFLOW=".github/workflows/release.yml"
GLIBC_FLOOR_README="README.md"
GLIBC_FLOOR_INSTALLER="scripts/install.sh"

# This file, for a message that has to say where the floor is changed.
GLIBC_FLOOR_LIB="scripts/lib/glibc-floor.sh"

# The reader for every ELF question below, resolved once into GLIBC_READELF.
# Exits 2 with the shared message when the host has none; it is a guard, not a
# predicate. binutils is not installed by default in a minimal build container,
# so the release job installs it alongside the compiler.
glibc_reader() {
    for _candidate in readelf llvm-readelf eu-readelf; do
        if command -v "$_candidate" >/dev/null 2>&1; then
            GLIBC_READELF="$_candidate"
            return 0
        fi
    done
    echo "error: need readelf (binutils), llvm-readelf or eu-readelf to read symbol versions, found none" >&2
    exit 2
}

# glibc_is_elf <file>
#
# Whether the file's first four bytes are the ELF magic. Non-ELF is not a pass:
# every caller reports it rather than treating "no versions found" as "requires
# nothing above the floor" — a Mach-O, a shell script and a truncated download
# all read as zero requirements otherwise.
glibc_is_elf() {
    [ -f "$1" ] || return 1
    [ "$(dd if="$1" bs=4 count=1 2>/dev/null | od -An -tx1 | tr -d ' \n')" = "7f454c46" ]
}

# glibc_required_versions <file>
#
# The GLIBC_x.y versions the file *requires*, as bare `x.y`, one per line, sorted
# ascending by version so the last line is the highest.
#
# Read from `.gnu.version_r` — the version needs section, which is exactly what
# the dynamic loader checks against the glibc it finds at exec time, and exactly
# what it refuses the binary over. Version *definitions* and the `.gnu.version`
# symbol table are deliberately not read: a shared library defines versions it
# provides, and reading those would report a libc as requiring itself.
#
# Writes nothing and returns 0 for a file that requires no versioned glibc
# symbol at all. Callers distinguish that from an unreadable file themselves,
# which is why this stays quiet.
glibc_required_versions() {
    "$GLIBC_READELF" -V -W "$1" 2>/dev/null | awk '
        /^Version needs section/ { needs = 1; next }
        /^Version (definition|symbols) section/ { needs = 0 }
        needs && match($0, /Name: GLIBC_[0-9][0-9.]*/) {
            print substr($0, RSTART + 12, RLENGTH - 12)
        }
    ' | sort -uV
}

# glibc_symbols_requiring <file> <version>
#
# The dynamic symbols bound to exactly `GLIBC_<version>`, one name per line, for
# naming what is responsible for a requirement. Empty output is possible and not
# an error — a version need can outlive the symbol that introduced it.
glibc_symbols_requiring() {
    _glibc_escaped=$(printf '%s' "$2" | sed 's/\./\\./g')
    "$GLIBC_READELF" --dyn-syms -W "$1" 2>/dev/null |
        grep -oE '[A-Za-z_][A-Za-z0-9_.]*@+GLIBC_[0-9][0-9.]*' |
        grep -E "@+GLIBC_${_glibc_escaped}\$" |
        sed 's/@.*//' |
        sort -u || true
}

# glibc_version_gt <a> <b>
#
# True when version `a` is strictly above version `b`. `sort -V` is the
# comparator because these are dotted versions, not numbers and not strings: it
# is the only ordering that puts 2.4 below 2.10 and 2.10 below 2.31.
glibc_version_gt() {
    [ "$1" != "$2" ] || return 1
    [ "$(printf '%s\n%s\n' "$1" "$2" | sort -V | tail -n 1)" = "$1" ]
}
