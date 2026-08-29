#!/usr/bin/env bash
#
# One-time AWS setup for agent content negotiation.
#
# Runs the whole sequence: preflight checks, function create/publish, then the
# distribution association — with a confirmation prompt before the one step
# that mutates live infrastructure, and a saved rollback file.
#
# Usage:
#   docs/infra/setup-cloudfront-negotiation.sh            # interactive
#   docs/infra/setup-cloudfront-negotiation.sh --check    # read-only checks
#   docs/infra/setup-cloudfront-negotiation.sh --yes      # no prompt
#
# Safe to re-run: if the function is already associated, it re-uploads the code
# and skips the distribution change.
#
# NAME defaults to "murmur-index-rewrite": that function already owned
# viewer-request on this distribution (directory-index rewriting) before agent
# negotiation was added, and a behavior allows only one. The two rulesets were
# merged into cloudfront-agent-negotiation.js rather than adding a second
# function — see that file's header. This is why the "already associated"
# branch below is the expected, steady-state path here, not a fallback.

set -euo pipefail

DISTRIBUTION="${DOCS_DISTRIBUTION:-E3SVCJVONCNVPZ}"
NAME="${CF_FUNCTION_NAME:-murmur-index-rewrite}"
SITE_URL="${DOCS_SITE_URL:-https://docs.murmur.nexus}"
PROBE_PATH="${DOCS_PROBE_PATH:-/concepts/hooks}"

export AWS_PAGER=""
cd "$(dirname "$0")"

CHECK_ONLY=false
ASSUME_YES=false
for arg in "$@"; do
    case "$arg" in
        --check) CHECK_ONLY=true ;;
        --yes|-y) ASSUME_YES=true ;;
        *) echo "unknown flag: $arg" >&2; exit 2 ;;
    esac
done

for tool in aws jq node curl; do
    command -v "$tool" >/dev/null || { echo "error: $tool is required but not installed." >&2; exit 2; }
done

say() { printf '\n\033[1m%s\033[0m\n' "$*"; }

say "==> Who am I"
aws sts get-caller-identity --query '{account:Account,arn:Arn}' --output table

say "==> Reading distribution ${DISTRIBUTION}"
aws cloudfront get-distribution-config --id "$DISTRIBUTION" > dist.json
jq -r '.DistributionConfig | "aliases:   \(.Aliases.Items // [] | join(", "))\ncomment:   \(.Comment)\nbehaviors: \(.CacheBehaviors.Quantity) extra (beyond the default)"' dist.json

EXTRA_BEHAVIORS=$(jq -r '.DistributionConfig.CacheBehaviors.Quantity' dist.json)
if [[ "$EXTRA_BEHAVIORS" != "0" ]]; then
    cat >&2 <<EOF

WARNING: this distribution has ${EXTRA_BEHAVIORS} extra cache behavior(s).
This script only touches the DEFAULT behavior. If the docs pages are served by
one of the path-pattern behaviors instead, associate the function there by hand
and do not use this script. Inspect them with:

  aws cloudfront get-distribution-config --id ${DISTRIBUTION} \\
    --query 'DistributionConfig.CacheBehaviors.Items[].PathPattern'
EOF
fi

# --- The blocking check: only one viewer-request function per behavior. -------
say "==> Checking the viewer-request slot"
EXISTING_FN=$(jq -r '
  (.DistributionConfig.DefaultCacheBehavior.FunctionAssociations.Items // [])
  | map(select(.EventType == "viewer-request")) | .[0].FunctionARN // ""' dist.json)
EXISTING_LAMBDA=$(jq -r '
  (.DistributionConfig.DefaultCacheBehavior.LambdaFunctionAssociations.Items // [])
  | map(select(.EventType == "viewer-request")) | .[0].LambdaFunctionARN // ""' dist.json)

if [[ -n "$EXISTING_LAMBDA" ]]; then
    cat >&2 <<EOF
error: a Lambda@Edge function is already on viewer-request:
  ${EXISTING_LAMBDA}
A behavior cannot have both. Merge the two rewrite rules from
cloudfront-agent-negotiation.js into that Lambda instead. Nothing was changed.
EOF
    exit 1
fi

if [[ -n "$EXISTING_FN" && "$EXISTING_FN" != *":function/${NAME}" ]]; then
    cat >&2 <<EOF
error: another CloudFront Function already owns viewer-request:
  ${EXISTING_FN}
Only one is allowed per behavior. Merge the two rewrite rules from
cloudfront-agent-negotiation.js into that function instead. Nothing was changed.
EOF
    exit 1
fi

if [[ -n "$EXISTING_FN" ]]; then
    echo "    already associated with ${NAME} — will refresh the code only"
else
    echo "    free"
fi

if [[ "$CHECK_ONLY" == true ]]; then
    say "==> --check: read-only, nothing changed."
    exit 0
fi

# --- Upload + publish the function (never touches the distribution). ----------
say "==> Deploying the function"
./deploy-cloudfront-function.sh

ARN=$(aws cloudfront describe-function --name "$NAME" --stage LIVE \
    --query 'FunctionSummary.FunctionMetadata.FunctionARN' --output text)

if [[ -n "$EXISTING_FN" ]]; then
    say "==> Already associated — skipping the distribution update."
else
    say "==> Associating with the default cache behavior"
    # Replace only the viewer-request entry; assigning FunctionAssociations
    # wholesale would drop an existing viewer-response association.
    jq --arg arn "$ARN" '
      .DistributionConfig
      | .DefaultCacheBehavior.FunctionAssociations.Items =
          (((.DefaultCacheBehavior.FunctionAssociations.Items // [])
            | map(select(.EventType != "viewer-request")))
           + [{FunctionARN: $arn, EventType: "viewer-request"}])
      | .DefaultCacheBehavior.FunctionAssociations.Quantity =
          (.DefaultCacheBehavior.FunctionAssociations.Items | length)
    ' dist.json > config.json

    echo "    this will set FunctionAssociations to:"
    jq '.DefaultCacheBehavior.FunctionAssociations' config.json | sed 's/^/      /'
    echo "    rollback copy saved at $(pwd)/dist.json"

    if [[ "$ASSUME_YES" != true ]]; then
        read -r -p "    Update the live distribution? [y/N] " reply
        [[ "$reply" =~ ^[Yy]$ ]] || { echo "    aborted; nothing changed."; exit 1; }
    fi

    aws cloudfront update-distribution --id "$DISTRIBUTION" \
        --if-match "$(jq -r .ETag dist.json)" \
        --distribution-config file://config.json >/dev/null

    say "==> Waiting for the distribution to deploy (a few minutes)"
    aws cloudfront wait distribution-deployed --id "$DISTRIBUTION"
    echo "    deployed"
fi

# --- Verify all three discovery paths end to end. -----------------------------
say "==> Verifying"

fail=0
check() {
    local label="$1" expected="$2" actual="$3"
    if [[ "$actual" == *"$expected"* ]]; then
        printf '    ok   %s\n' "$label"
    else
        printf '    FAIL %s\n         expected to contain %q\n         got %q\n' "$label" "$expected" "$actual"
        fail=1
    fi
}

check "browser gets HTML" "text/html" \
    "$(curl -sI "${SITE_URL}${PROBE_PATH}" | tr -d '\r' | grep -i '^content-type:' || true)"
check "Accept: text/markdown gets markdown" "title:" \
    "$(curl -s -H 'Accept: text/markdown' "${SITE_URL}${PROBE_PATH}" | head -3 | tr '\n' ' ')"
check "?mode=agent gets markdown" "title:" \
    "$(curl -s "${SITE_URL}${PROBE_PATH}?mode=agent" | head -3 | tr '\n' ' ')"
check "alternate link present" 'type="text/markdown"' \
    "$(curl -s "${SITE_URL}${PROBE_PATH}" | grep -o '<link rel="alternate"[^>]*>' || true)"

if (( fail )); then
    cat >&2 <<EOF

Some checks failed. Most likely causes, in order:
  1. The markdown twins are not on S3 yet — run docs/deploy.sh.
  2. CloudFront is still serving a cached HTML response — invalidate, or wait.
  3. The docs are served by a non-default cache behavior (see the warning above).
EOF
    exit 1
fi

say "==> Done. All three discovery paths are live."
