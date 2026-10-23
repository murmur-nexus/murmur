#!/bin/sh
#
# check-dev-version.sh — is the version on this commit one a release already took?
#
#   scripts/check-dev-version.sh
#
# Reads [workspace.package] version from the top-level Cargo.toml of the git work
# tree containing the current directory, and looks for the release tag
# v<version>. A version equal to an existing tag is right on that tag's commit
# and wrong on every other one: a build from it would report a release it is not.
# After a release tag, main moves to the next minor with -dev (the "Reopen main"
# step of .github/RELEASE_GUIDE.md), so no tag carries its version until the next
# release's bump commit is tagged.
#
# Unlike wit-versions-changed-since.sh, this one runs on every pull request and
# every push to main (the dev-version job in .github/workflows/ci.yml), because
# the question has the same answer on every commit but the tag's. CI fetches the
# release tags first; a checkout without them passes for want of a tag.
#
# It reads and reports. It never creates, moves or fetches a tag.
#
# Exit codes:
#   0  no tag v<version> exists, or HEAD is that tag's commit
#   1  tag v<version> exists and points at a commit other than HEAD
#   2  not inside a git work tree, no Cargo.toml at its top level, or no
#      [workspace.package] version in it

set -eu

if ! top=$(git rev-parse --show-toplevel 2>/dev/null); then
    echo "error: $(pwd) is not inside a git work tree" >&2
    echo "  run this from a checkout of the murmur repository." >&2
    exit 2
fi

manifest="$top/Cargo.toml"
if [ ! -f "$manifest" ]; then
    echo "error: no Cargo.toml at the top of the work tree ($top)" >&2
    echo "  run this from a checkout of the murmur repository." >&2
    exit 2
fi

# The first `version = "..."` inside the [workspace.package] table, and nowhere else.
version=$(awk '
    /^[[:space:]]*\[/ {
        in_table = ($0 ~ /^[[:space:]]*\[workspace\.package\][[:space:]]*(#.*)?$/)
        next
    }
    in_table && /^[[:space:]]*version[[:space:]]*=/ {
        if (match($0, /"[^"]*"/)) {
            print substr($0, RSTART + 1, RLENGTH - 2)
            exit
        }
    }
' "$manifest")

if [ -z "$version" ]; then
    echo "error: $manifest has no [workspace.package] version" >&2
    echo "  the workspace version lives under [workspace.package] as version = \"X.Y.Z\"." >&2
    exit 2
fi

tag="v$version"
head=$(git -C "$top" rev-parse "HEAD^{commit}")

if ! tagged=$(git -C "$top" rev-parse --verify --quiet "refs/tags/$tag^{commit}"); then
    echo "ok: version $version has no release tag $tag"
    exit 0
fi

if [ "$tagged" = "$head" ]; then
    echo "ok: version $version is release $tag, and HEAD is that tag's commit"
    exit 0
fi

# A release version is X.Y.Z, so the next minor is X.(Y+1).0.
case $version in
    *[!0-9.]* | *.*.*.* | .* | *. | *..*)
        next="the next minor with -dev" ;;
    *.*.*)
        major=${version%%.*}
        rest=${version#*.}
        next="$major.$((${rest%%.*} + 1)).0-dev, the next minor with -dev" ;;
    *)
        next="the next minor with -dev" ;;
esac

echo "error: version $version is already release $tag, on another commit" >&2
printf '  %-10s %s\n' "$tag:" "$tagged" "HEAD:" "$head" >&2
echo "  move [workspace.package] version, the [workspace.dependencies] pins and Cargo.lock" >&2
echo "  to $next, in one commit," >&2
echo "  as the \"Reopen main\" step of .github/RELEASE_GUIDE.md says." >&2
exit 1
