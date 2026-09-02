#!/bin/sh
#
# wit-packages.sh — the one place the `package murmur:x@y.z` line is parsed.
#
# Sourced, never executed:
#
#     . "$(dirname "$0")/lib/wit-packages.sh"
#
# Two scripts need the same (package, version) set from two different inputs:
# scripts/check-wit-versions.sh reads the checked-out tree, and
# scripts/wit-versions-changed-since.sh reads a git revision as well. Those are
# two input branches of one match-and-normalize pipeline here, not two regexes
# in two files that drift apart the first time the declaration syntax moves.
#
# Contract for a sourcer:
#
#   * Have `set -eu` already in force. Every function here is written to be
#     safe under it, and `wit_require_repo_root` relies on `exit` reaching the
#     sourcing script.
#   * Run from the repository root. WIT_DIR is repo-relative and doubles as a
#     `git` pathspec; `wit_require_repo_root` is the shared guard that says so.
#   * Treat a non-zero return from `wit_packages` as "nothing matched" and
#     write its own diagnostic — the helper stays quiet so each caller can name
#     the side of the comparison that came up empty.

# The murmur WIT tree. Several subtrees under it are bound independently by
# different consumers, which is why a package can end up declared twice.
WIT_DIR="crates/capsule-runtime/wit"

# The declaration this repo derives every version fact from, anchored at the
# start of a line. Every murmur:* package carries an explicit @x.y.z — see
# $WIT_DIR/VERSIONING.md for why, and for what a bump costs.
WIT_PACKAGE_PATTERN='^package murmur:[a-z-]+@[0-9]+\.[0-9]+\.[0-9]+'

# Exit 2 with the shared message unless the caller is at the repository root.
# Exits the sourcing script; it is a guard, not a predicate.
wit_require_repo_root() {
    if [ ! -d "$WIT_DIR" ]; then
        echo "error: $WIT_DIR not found — run this from the repository root" >&2
        exit 2
    fi
}

# wit_packages <source>
#
#   <source>  `--worktree` for the checked-out tree, or any git revision that
#             `git ls-tree` accepts (tag, branch, commit).
#
# Writes the declared packages to stdout as `murmur:name@x.y.z`, one per line,
# deduplicated and sorted by name. Writes nothing to stderr of its own; git's
# own stderr passes through. Returns 1 and writes nothing at all when the
# source declares no murmur:* package, so that an empty set can never be
# mistaken for a successful read of an empty tree.
wit_packages() {
    _wit_source="$1"
    _wit_found=$(
        case "$_wit_source" in
            --worktree)
                grep -rhoE "$WIT_PACKAGE_PATTERN" "$WIT_DIR" --include='*.wit' || true
                ;;
            *)
                # Read the revision out of the object store: no checkout, no
                # stash, nothing touched in the working tree.
                git ls-tree -r --name-only "$_wit_source" -- "$WIT_DIR" |
                    grep '\.wit$' |
                    while IFS= read -r _wit_file; do
                        git show "$_wit_source:$_wit_file"
                    done |
                    grep -oE "$WIT_PACKAGE_PATTERN" || true
                ;;
        esac | sed 's/^package //' | sort -u
    )
    [ -n "$_wit_found" ] || return 1
    printf '%s\n' "$_wit_found"
}

# wit_files_declaring <package>
#
#   <package>  a bare package name such as `murmur:hook`.
#
# Writes the paths of the working-tree .wit files declaring it, one per line,
# for naming the offenders in a duplicate-version report.
wit_files_declaring() {
    grep -rlE "^package ${1}@" "$WIT_DIR" --include='*.wit'
}
