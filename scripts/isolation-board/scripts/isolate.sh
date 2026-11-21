#!/bin/bash
# Isolate stage, apply. Runs on the host, in the card's clone, as the board's owner. No tokens.
# Runs the isolation sets of the clone's tree with the board's trusted runner and inputs, writes the
# evidence under the board (outside the clone), and attaches a copy as isolation-evidence.
# Exit 0 whenever evidence was written (pass, fail or no evidence), so Judge sees a failure;
# exit 2 when the runner refused and no evidence exists.
set -u
cd "${workspace:?}" || exit 2
T="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
EVIDENCE_DIR="$T/evidence"
EVIDENCE="$EVIDENCE_DIR/${card_id:?}.json"
mkdir -p "$EVIDENCE_DIR"

python3 -I "$T/trusted/isolation-proofs.py" run \
  --repo "$workspace" \
  --set landlock --set wasm-guest-workers \
  --evidence "$EVIDENCE" \
  --classification "$T/trusted/isolation-tests.toml" \
  --nextest-config "$T/trusted/nextest.toml" \
  --preflight-script "$T/trusted/assert-containment-tier.py" \
  --target-dir "${ISOLATION_TARGET_DIR:-$HOME/.cache/murmur-targets/isolation-board}"
rc=$?

case "$rc" in
  0|1|3)
    [ -f "$EVIDENCE" ] || { echo "the runner exited $rc but wrote no evidence at $EVIDENCE" >&2; exit 2; }
    # Transit attaches only paths inside the workspace. The copy lives in a folder git ignores for
    # this clone, so it changes neither `git status --untracked-files=no` nor `git diff HEAD`.
    mkdir -p .isolation-evidence
    grep -qx '.isolation-evidence/' .git/info/exclude 2>/dev/null || echo '.isolation-evidence/' >> .git/info/exclude
    cp "$EVIDENCE" ".isolation-evidence/$card_id.json"
    [ -n "${TRANSIT_DELIVERABLES:-}" ] && printf 'isolation-evidence %s\n' ".isolation-evidence/$card_id.json" >> "$TRANSIT_DELIVERABLES"
    echo "evidence: $EVIDENCE (runner exit $rc)"
    exit 0 ;;
  *)
    echo "the isolation runner refused (exit $rc): no evidence; the reason is above" >&2
    exit 2 ;;
esac
