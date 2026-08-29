#!/usr/bin/env bash
#
# Create/update and publish the agent content-negotiation CloudFront Function.
#
# The function source lives in this repo, but CloudFront runs its own copy —
# editing the .js file changes nothing in production until this script uploads
# it. Run it after any change to cloudfront-agent-negotiation.js.
#
# Usage:
#   docs/infra/deploy-cloudfront-function.sh            # upload, test, publish
#   docs/infra/deploy-cloudfront-function.sh --dry-run  # test only, no publish
#
# Requires AWS credentials with cloudfront:CreateFunction, UpdateFunction,
# PublishFunction, DescribeFunction and TestFunction.
#
# This script never modifies the distribution. Associating the published
# function with a cache behavior is a one-time manual step — see README.md in
# this directory, and mind the "only one viewer-request function per behavior"
# constraint before you do it.
#
# The default NAME is "murmur-index-rewrite", not a name of our own: the docs
# distribution's default behavior already had a viewer-request function doing
# directory-index rewriting before agent negotiation was added, and a behavior
# allows only one. cloudfront-agent-negotiation.js was merged into that
# function's logic rather than standing up a second one — see its file header.
# Do not point this at a different NAME without re-associating the behavior.

set -euo pipefail

NAME="${CF_FUNCTION_NAME:-murmur-index-rewrite}"
RUNTIME="${CF_FUNCTION_RUNTIME:-cloudfront-js-2.0}"
DISTRIBUTION="${DOCS_DISTRIBUTION:-E3SVCJVONCNVPZ}"

cd "$(dirname "$0")"
CODE_FILE="cloudfront-agent-negotiation.js"

DRY_RUN=false
[[ "${1:-}" == "--dry-run" ]] && DRY_RUN=true

# CloudFront rejects function code over 10 KB; catch it here rather than in a
# confusing API error.
SIZE=$(wc -c <"$CODE_FILE")
if (( SIZE > 10240 )); then
    echo "error: $CODE_FILE is ${SIZE} bytes; CloudFront Functions cap out at 10240." >&2
    exit 1
fi

echo "==> Running local tests first"
node --test cloudfront-agent-negotiation.test.mjs >/dev/null
echo "    passed"

etag_of() {
    aws cloudfront describe-function --name "$NAME" --stage DEVELOPMENT \
        --query 'ETag' --output text 2>/dev/null || true
}

ETAG=$(etag_of)

if [[ -z "$ETAG" || "$ETAG" == "None" ]]; then
    echo "==> Creating function ${NAME}"
    ETAG=$(aws cloudfront create-function \
        --name "$NAME" \
        --function-config "Comment=Rewrite directory requests to index.html; route agents to markdown twins,Runtime=${RUNTIME}" \
        --function-code "fileb://${CODE_FILE}" \
        --query 'ETag' --output text)
else
    echo "==> Updating function ${NAME}"
    ETAG=$(aws cloudfront update-function \
        --name "$NAME" \
        --if-match "$ETAG" \
        --function-config "Comment=Rewrite directory requests to index.html; route agents to markdown twins,Runtime=${RUNTIME}" \
        --function-code "fileb://${CODE_FILE}" \
        --query 'ETag' --output text)
fi

# Exercise the uploaded copy at the edge runtime, not just locally: a syntax or
# runtime feature the local Node accepts may still be rejected by CloudFront.
echo "==> Testing the uploaded function against a markdown request"
cat >/tmp/cf-event.json <<'JSON'
{
  "version": "1.0",
  "context": { "eventType": "viewer-request" },
  "viewer": { "ip": "1.2.3.4" },
  "request": {
    "method": "GET",
    "uri": "/concepts/hooks/",
    "querystring": {},
    "headers": { "accept": { "value": "text/markdown" } },
    "cookies": {}
  }
}
JSON

RESULT=$(aws cloudfront test-function \
    --name "$NAME" \
    --if-match "$ETAG" \
    --stage DEVELOPMENT \
    --event-object "fileb:///tmp/cf-event.json" \
    --query 'TestResult.FunctionOutput' --output text)

echo "$RESULT" | grep -q '"uri":"/concepts/hooks.md"' || {
    echo "error: uploaded function did not rewrite to the markdown twin." >&2
    echo "$RESULT" >&2
    exit 1
}
echo "    rewrote /concepts/hooks/ -> /concepts/hooks.md"

if [[ "$DRY_RUN" == true ]]; then
    echo "==> --dry-run: stopping before publish. DEVELOPMENT stage is updated."
    exit 0
fi

echo "==> Publishing to LIVE"
aws cloudfront publish-function --name "$NAME" --if-match "$ETAG" >/dev/null

ARN=$(aws cloudfront describe-function --name "$NAME" --stage LIVE \
    --query 'FunctionSummary.FunctionMetadata.FunctionARN' --output text)

echo "==> Published"
echo "    ${ARN}"

ASSOCIATED=$(aws cloudfront get-distribution-config --id "$DISTRIBUTION" \
    --query "DistributionConfig.DefaultCacheBehavior.FunctionAssociations.Items[?FunctionARN=='${ARN}'] | length(@)" \
    --output text 2>/dev/null || echo "unknown")

if [[ "$ASSOCIATED" == "0" ]]; then
    echo
    echo "    NOT yet associated with distribution ${DISTRIBUTION}."
    echo "    Publishing alone changes nothing for viewers — see infra/README.md"
    echo "    for the one-time association step."
elif [[ "$ASSOCIATED" == "unknown" ]]; then
    echo "    (could not read the distribution config to check association)"
else
    echo "    Associated with ${DISTRIBUTION}; live on the next propagation."
fi
