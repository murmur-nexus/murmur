#!/bin/bash
# Install the isolation board into a Transit project:
#
#   bash scripts/isolation-board/install.sh <project-dir> [<trusted-tree>]
#
# Copies the preset and the stage scripts into <project>/.nexus/transit/, and the runner, the
# classification, the nextest config and the preflight from <trusted-tree> (default: the repository
# holding this script) into <project>/.nexus/transit/trusted/. The board runs those copies, never the
# card clone's files, so a card cannot change the code that judges it. Re-run it to update the
# trusted copies after a reviewed change to any of them lands.
set -eu
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
[ $# -ge 1 ] && [ $# -le 2 ] || { echo "usage: install.sh <project-dir> [<trusted-tree>]" >&2; exit 2; }
PROJECT="$(cd "$1" && pwd)" || { echo "no project directory at $1" >&2; exit 2; }
TRUSTED="$(cd "${2:-$HERE/../..}" && git rev-parse --show-toplevel)" || { echo "${2:-$HERE/../..} is not a git tree" >&2; exit 2; }
BOARD="$PROJECT/.nexus/transit"

copy() { # copy <from> <to>
  [ -f "$1" ] || { echo "missing in the trusted tree: $1" >&2; exit 2; }
  mkdir -p "$(dirname "$2")"
  cp "$1" "$2"
  printf '  %s  <- %s\n' "${2#"$PROJECT"/}" "$1"
}

commit="$(git -C "$TRUSTED" rev-parse HEAD)"
state="clean"; [ -z "$(git -C "$TRUSTED" status --porcelain --untracked-files=no)" ] || state="with uncommitted changes"
echo "installing the isolation board into $BOARD"
echo "trusted tree: $TRUSTED at $commit ($state)"
copy "$TRUSTED/scripts/isolation-board/preset.yaml" "$BOARD/preset.yaml"
copy "$TRUSTED/scripts/isolation-board/scripts/isolate.sh" "$BOARD/scripts/isolate.sh"
copy "$TRUSTED/scripts/isolation-board/scripts/judge.sh" "$BOARD/scripts/judge.sh"
copy "$TRUSTED/scripts/isolation-proofs.py" "$BOARD/trusted/isolation-proofs.py"
copy "$TRUSTED/.config/isolation-tests.toml" "$BOARD/trusted/isolation-tests.toml"
copy "$TRUSTED/.config/nextest.toml" "$BOARD/trusted/nextest.toml"
copy "$TRUSTED/.github/scripts/assert-containment-tier.py" "$BOARD/trusted/assert-containment-tier.py"
mkdir -p "$BOARD/evidence"
printf '%s %s\n' "$commit" "$state" > "$BOARD/trusted/FROM"
echo "done: nexus transit doctor --project $PROJECT"
