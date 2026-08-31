#!/usr/bin/env bash
# One-time setup: attach a shared CloudFront Response Headers Policy, applied
# to both docs.murmur.nexus and murmur.nexus, that adds:
#
#   Link: </llms.txt>; rel="service-doc",          (RFC 8288 / RFC 9727 sec 3)
#         </.well-known/api-catalog>; rel="api-catalog"
#   Access-Control-Allow-Origin: *                 (required by the ARD manifest
#                                                    at /.well-known/ai-catalog.json)
#
# One policy for both distributions — both Link targets are site-relative
# paths that exist on each domain, so the same header value is correct on
# both. The CORS header is safe to apply site-wide: both sites are fully
# public static content with no auth/cookies, so allowing cross-origin JS to
# *read* responses changes nothing about who can already reach them over
# plain HTTP.
#
# Idempotent: creates the policy if missing, updates it in place if the
# header set has changed (e.g. a header was added since it was first
# created), and only updates a distribution if it isn't already attached.
# Safe to re-run.
#
# Usage: docs/infra/setup-response-headers.sh
set -euo pipefail

POLICY_NAME="murmur-agent-link-header"
DOCS_DIST_ID="E3SVCJVONCNVPZ"
LANDING_DIST_ID="E8I1RI0YU23W1"

DESIRED_CONFIG=$(jq -n --arg name "$POLICY_NAME" '{
  Name: $name,
  Comment: "Agent-discovery response headers: Link (RFC 8288/9727) + CORS for the ARD manifest",
  CustomHeadersConfig: {
    Quantity: 1,
    Items: [
      { Header: "Link", Value: "</llms.txt>; rel=\"service-doc\", </.well-known/api-catalog>; rel=\"api-catalog\"", Override: true }
    ]
  },
  CorsConfig: {
    AccessControlAllowCredentials: false,
    AccessControlAllowHeaders: { Quantity: 1, Items: ["*"] },
    AccessControlAllowMethods: { Quantity: 2, Items: ["GET", "HEAD"] },
    AccessControlAllowOrigins: { Quantity: 1, Items: ["*"] },
    OriginOverride: true
  }
}')

echo "==> Looking for existing policy '$POLICY_NAME'"
POLICY_ID=$(aws cloudfront list-response-headers-policies --type custom \
  --query "ResponseHeadersPolicyList.Items[?ResponseHeadersPolicy.ResponseHeadersPolicyConfig.Name=='$POLICY_NAME'].ResponseHeadersPolicy.Id" \
  --output text)

if [ -z "$POLICY_ID" ] || [ "$POLICY_ID" = "None" ]; then
  echo "==> Creating policy '$POLICY_NAME'"
  POLICY_ID=$(aws cloudfront create-response-headers-policy \
    --response-headers-policy-config "$DESIRED_CONFIG" \
    --query 'ResponseHeadersPolicy.Id' --output text)
  echo "    created: $POLICY_ID"
else
  echo "    found: $POLICY_ID"
  CURRENT=$(aws cloudfront get-response-headers-policy --id "$POLICY_ID")
  CURRENT_ITEMS=$(echo "$CURRENT" | jq -S '{c: .ResponseHeadersPolicy.ResponseHeadersPolicyConfig.CustomHeadersConfig.Items, cors: .ResponseHeadersPolicy.ResponseHeadersPolicyConfig.CorsConfig}')
  DESIRED_ITEMS=$(echo "$DESIRED_CONFIG" | jq -S '{c: .CustomHeadersConfig.Items, cors: .CorsConfig}')
  if [ "$CURRENT_ITEMS" != "$DESIRED_ITEMS" ]; then
    echo "    header set changed, updating"
    ETAG=$(echo "$CURRENT" | jq -r '.ETag')
    aws cloudfront update-response-headers-policy --id "$POLICY_ID" --if-match "$ETAG" \
      --response-headers-policy-config "$DESIRED_CONFIG" > /dev/null
  else
    echo "    already up to date"
  fi
fi

attach() {
  local dist_id="$1"
  local label="$2"
  local config_file="/tmp/murmur-response-headers-${dist_id}.json"

  aws cloudfront get-distribution-config --id "$dist_id" > "$config_file"
  local current
  current=$(jq -r '.DistributionConfig.DefaultCacheBehavior.ResponseHeadersPolicyId // empty' "$config_file")

  if [ "$current" = "$POLICY_ID" ]; then
    echo "==> $label ($dist_id): already attached, skipping"
    rm -f "$config_file"
    return
  fi

  echo "==> $label ($dist_id): attaching policy"
  local etag
  etag=$(jq -r '.ETag' "$config_file")
  jq --arg pid "$POLICY_ID" '.DistributionConfig.DefaultCacheBehavior.ResponseHeadersPolicyId = $pid | .DistributionConfig' \
    "$config_file" > "${config_file}.patched"

  aws cloudfront update-distribution --id "$dist_id" \
    --if-match "$etag" \
    --distribution-config "file://${config_file}.patched" > /dev/null
  echo "    updated (propagation takes a few minutes)"
  rm -f "$config_file" "${config_file}.patched"
}

attach "$DOCS_DIST_ID" "docs.murmur.nexus"
attach "$LANDING_DIST_ID" "murmur.nexus"

echo "==> Done. Verify with:"
echo "    curl -sI https://docs.murmur.nexus/ | grep -iE '^(link|access-control)'"
echo "    curl -sI https://murmur.nexus/       | grep -iE '^(link|access-control)'"
