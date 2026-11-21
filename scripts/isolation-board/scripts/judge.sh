#!/bin/bash
# Judge stage, apply. Runs on the host, in the card's clone, as the board's owner. No tokens.
# Judges the evidence Isolate wrote under the board (never the clone's copy) against the clone's
# commit and tree, with the trusted classification, requiring both sets. Exits with the judge's
# status: 0 pass, 1 fail, 2 unusable evidence. Transit attaches a script's deliverables only after
# exit 0, so a failing verdict is kept beside the evidence and in the card's log instead.
set -u
cd "${workspace:?}" || exit 2
T="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
P="$(cd "$T/../.." && pwd)"
EVIDENCE="$T/evidence/${card_id:?}.json"
VERDICT="$T/evidence/$card_id.verdict.txt"

python3 -I "$T/trusted/isolation-proofs.py" judge "$EVIDENCE" \
  --repo "$workspace" \
  --classification "$T/trusted/isolation-tests.toml" \
  --require-set landlock --require-set wasm-guest-workers > "$VERDICT" 2>&1
rc=$?
cat "$VERDICT"

mkdir -p .isolation-evidence
grep -qx '.isolation-evidence/' .git/info/exclude 2>/dev/null || echo '.isolation-evidence/' >> .git/info/exclude
cp "$VERDICT" ".isolation-evidence/$card_id.verdict.txt"
[ -n "${TRANSIT_DELIVERABLES:-}" ] && printf 'isolation-verdict %s\n' ".isolation-evidence/$card_id.verdict.txt" >> "$TRANSIT_DELIVERABLES"

if [ "$rc" -ne 0 ]; then
  # The first line of stderr is the stall reason `nexus transit why` shows: the verdict, then the
  # first failing line. The whole verdict goes to the card's log.
  first="$(grep -E '^(FAIL|NO-EVIDENCE|MISSING) ' "$VERDICT" | head -n 1)"
  judged="$(grep -E '^JUDGE: ' "$VERDICT" | tail -n 1)"
  echo "${judged:-JUDGE: exit $rc}${first:+; $first}" >&2
  nexus transit log add "Judge: $(tr '\n' ' ' < "$VERDICT" | cut -c1-1500) (verdict: $VERDICT)" \
    --card "$card_id" --project "$P" >/dev/null 2>&1 || true
fi
exit "$rc"
