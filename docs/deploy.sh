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

echo "==> Syncing to s3://${BUCKET}/${PREFIX}"
aws s3 sync site/ "s3://${BUCKET}/${PREFIX}" \
    --delete \
    --exclude ".DS_Store" \
    --exclude "*/.DS_Store"

echo "==> Invalidating CloudFront"
INVALIDATION=$(aws cloudfront create-invalidation \
    --distribution-id "$DISTRIBUTION" \
    --paths "/*" \
    --query 'Invalidation.Id' --output text)

echo "==> Done. Invalidation ${INVALIDATION} in flight."
echo "    https://docs.murmur.nexus"
