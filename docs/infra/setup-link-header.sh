#!/usr/bin/env bash
# One-time setup: attach a shared CloudFront Response Headers Policy that adds
# a `Link` response header (RFC 8288) advertising each site's own /llms.txt
# as its machine-readable service documentation (RFC 9727 §3 `service-doc`
# relation). One policy, attached to both distributions — /llms.txt is a
# site-relative path, so the same header value is correct on both domains.
#
# Idempotent: creates the policy only if a policy with this name doesn't
# already exist, and only updates a distribution if it isn't already
# attached. Safe to re-run.
#
# Usage: docs/infra/setup-link-header.sh
set -euo pipefail

POLICY_NAME="murmur-agent-link-header"
LINK_VALUE='</llms.txt>; rel="service-doc"'
DOCS_DIST_ID="E3SVCJVONCNVPZ"
LANDING_DIST_ID="E8I1RI0YU23W1"

echo "==> Looking for existing policy '$POLICY_NAME'"
POLICY_ID=$(aws cloudfront list-response-headers-policies --type custom \
  --query "ResponseHeadersPolicyList.Items[?ResponseHeadersPolicy.ResponseHeadersPolicyConfig.Name=='$POLICY_NAME'].ResponseHeadersPolicy.Id" \
  --output text)

if [ -z "$POLICY_ID" ] || [ "$POLICY_ID" = "None" ]; then
  echo "==> Creating policy '$POLICY_NAME'"
  CONFIG=$(jq -n --arg name "$POLICY_NAME" --arg value "$LINK_VALUE" '{
    Name: $name,
    Comment: "Adds a Link header (RFC 8288) pointing agents at /llms.txt as service-doc (RFC 9727 sec 3)",
    CustomHeadersConfig: {
      Quantity: 1,
      Items: [ { Header: "Link", Value: $value, Override: true } ]
    }
  }')
  POLICY_ID=$(aws cloudfront create-response-headers-policy \
    --response-headers-policy-config "$CONFIG" \
    --query 'ResponseHeadersPolicy.Id' --output text)
  echo "    created: $POLICY_ID"
else
  echo "    found: $POLICY_ID"
fi

attach() {
  local dist_id="$1"
  local label="$2"
  local config_file="/tmp/murmur-link-header-${dist_id}.json"

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
echo "    curl -sI https://docs.murmur.nexus/ | grep -i '^link:'"
echo "    curl -sI https://murmur.nexus/       | grep -i '^link:'"
