#!/bin/sh
#
# rebuild-components.sh — rebuild the WASM components committed to this repo,
# or check which ones a WIT change has left stale.
#
#   scripts/rebuild-components.sh [<name>]
#   scripts/rebuild-components.sh --check [<name>]
#
# crates/capsule-runtime/wit/components.list names every committed .wasm, the
# WIT subtree and source crate it builds from, and whether it is frozen.
#
# With no flag it rebuilds every component the list does not freeze, or just
# <name>, with `cargo build --release --locked --target wasm32-wasip2` into
# target/committed-components/, and copies each result over its committed
# path. It refuses before building anything when the wasm32-wasip2 target is
# not installed. Commit the files it rewrites.
#
# With --check it writes nothing and needs no wasm target: it compares each
# non-frozen component's murmur:* interface versions with the ones its WIT
# subtree declares, and names every stale one with the command that rebuilds
# it. `cargo test --workspace` runs the same check, and so does the
# wit-components CI job when a change touches crates/capsule-runtime/wit/.
#
# Exit status: 0 done and current; 1 a component is stale, missing, unlisted,
# not a component or failed to build; 2 could not run as asked.
#
# The work is done by the committed-components crate; this script is its
# entry point. Run it from the repository root.

set -eu

# For wit_require_repo_root; see lib/wit-packages.sh.
. "$(dirname "$0")/lib/wit-packages.sh"

wit_require_repo_root

exec cargo run --quiet --locked -p committed-components -- "$@"
