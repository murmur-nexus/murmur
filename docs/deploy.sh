#!/usr/bin/env bash
#
# Build and publish the docs site to docs.murmur.nexus.
#
# Usage:
#   docs/deploy.sh
#
# Requires AWS credentials with write access to the murmur-storage bucket
# and CloudFront invalidation rights on the docs distribution. In CI these
# come from the github-actions-docs-deploy role via OIDC; locally they come
# from your ambient AWS profile.
#
# Target overrides (all optional, useful for staging):
#   DOCS_BUCKET, DOCS_PREFIX, DOCS_DISTRIBUTION

set -euo pipefail

BUCKET="${DOCS_BUCKET:-murmur-storage}"
PREFIX="${DOCS_PREFIX:-docs}"
DISTRIBUTION="${DOCS_DISTRIBUTION:-E3SVCJVONCNVPZ}"

cd "$(dirname "$0")"

echo "==> Building site"
mkdocs build --clean --strict

echo "==> Generating agent artifacts"
# Markdown twins, llms-full.txt, search index, WebMCP bundle, agent-readability
# manifest, sitemap and robots. Writes into site/, so the sync below picks them
# up with everything else. Not --omit=dev: esbuild bundles the WebMCP client.
npm ci --silent
node scripts/agent-artifacts.mjs

echo "==> Syncing to s3://${BUCKET}/${PREFIX}"
aws s3 sync site/ "s3://${BUCKET}/${PREFIX}" \
    --delete \
    --exclude ".DS_Store" \
    --exclude "*/.DS_Store"

# The api-catalog linkset has no file extension, so the sync above uploads it as
# application/octet-stream. Agents that discover it via the homepage
# `Link: rel="api-catalog"` header expect JSON.
aws s3 cp "site/.well-known/api-catalog" "s3://${BUCKET}/${PREFIX}/.well-known/api-catalog" \
    --content-type "application/linkset+json" \
    --metadata-directive REPLACE

echo "==> Invalidating CloudFront"
INVALIDATION=$(aws cloudfront create-invalidation \
    --distribution-id "$DISTRIBUTION" \
    --paths "/*" \
    --query 'Invalidation.Id' --output text)

echo "==> Done. Invalidation ${INVALIDATION} in flight."
echo "    https://docs.murmur.nexus"
