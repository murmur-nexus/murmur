# Docs infrastructure

Everything the docs site needs beyond "sync `site/` to S3".

There is exactly one piece of AWS state here that the repo does not manage:
the **CloudFront Function** that routes agents to markdown twins. It is
deployed under the name `murmur-index-rewrite`, not a name of our own — see
the next section for why.

## What is already automated

`docs/deploy.sh` and `.github/workflows/docs.yml` build the site, generate the
agent artifacts, run the tests, sync to S3, and invalidate CloudFront. Nothing
below changes on a normal docs deploy.

## The CloudFront Function (one-time setup)

`cloudfront-agent-negotiation.js` implements two of the three markdown
discovery paths:

| Request | Serves |
| --- | --- |
| `curl -H "Accept: text/markdown" .../concepts/hooks` | `/concepts/hooks.md` |
| `curl ".../concepts/hooks?mode=agent"` | `/concepts/hooks.md` |
| a browser | the HTML page, untouched |

The third path — `<link rel="alternate" type="text/markdown">` in every page
head — is built into the HTML and needs nothing here.

The function source lives in this repo, but CloudFront runs its own uploaded
copy. **Editing the `.js` file changes nothing in production.**

### Why this function is named `murmur-index-rewrite`

A cache behavior allows **only one** viewer-request function association (and
a Lambda@Edge viewer-request association is mutually exclusive with it).
E3SVCJVONCNVPZ's default behavior already had one before agent negotiation was
added — `murmur-index-rewrite`, doing plain directory-index rewriting
(`/foo/` → `/foo/index.html`). Rather than a second function, which CloudFront
does not allow, both rulesets were merged into
`cloudfront-agent-negotiation.js` and deployed under the existing function's
name. The file's header comment explains the merge and, in particular, why
markdown negotiation has to run *before* the index-rewrite rule.

If you're setting this up on a distribution where the slot is genuinely free,
override the name (`CF_FUNCTION_NAME=murmur-docs-agent-negotiation` or
whatever fits) and follow the same steps below — a fresh function still needs
step 2 (associate).

Check what currently owns the slot before changing anything:

```bash
aws cloudfront get-distribution-config --id E3SVCJVONCNVPZ \
  --query 'DistributionConfig.DefaultCacheBehavior.{fn:FunctionAssociations,lambda:LambdaFunctionAssociations}'
```

### 1. Upload and publish

```bash
docs/infra/deploy-cloudfront-function.sh --dry-run   # upload + test, no publish
docs/infra/deploy-cloudfront-function.sh             # upload, test, publish
```

The script runs the local tests, uploads, exercises the uploaded copy through
`aws cloudfront test-function`, then publishes to LIVE. It never touches the
distribution. Because the function already exists and is already associated,
publishing to LIVE is enough to take effect — no distribution update needed.
`setup-cloudfront-negotiation.sh` automates exactly this "already associated"
path.

### 2. Associate it with the distribution (only if the slot was free)

Skip this if you deployed under `murmur-index-rewrite` as above — it's already
associated. This step is only for a genuinely fresh function on a distribution
with a free viewer-request slot.

Console: CloudFront → distribution → Behaviors → edit the default behavior →
Function associations → Viewer request → CloudFront Functions → your function
name → save.

CLI equivalent, if you prefer. Note the jq keeps any existing
`viewer-response` association and replaces only the `viewer-request` slot —
assigning `FunctionAssociations` wholesale would silently drop the others.

```bash
ARN="<the ARN printed by deploy-cloudfront-function.sh>"

aws cloudfront get-distribution-config --id E3SVCJVONCNVPZ > dist.json

jq --arg arn "$ARN" '
  .DistributionConfig
  | .DefaultCacheBehavior.FunctionAssociations.Items =
      (((.DefaultCacheBehavior.FunctionAssociations.Items // [])
        | map(select(.EventType != "viewer-request")))
       + [{FunctionARN: $arn, EventType: "viewer-request"}])
  | .DefaultCacheBehavior.FunctionAssociations.Quantity =
      (.DefaultCacheBehavior.FunctionAssociations.Items | length)
' dist.json > config.json

aws cloudfront update-distribution --id E3SVCJVONCNVPZ \
  --if-match "$(jq -r .ETag dist.json)" \
  --distribution-config file://config.json
```

`update-distribution` redeploys the distribution; propagation takes a few
minutes. To roll back, re-run with the `viewer-request` entry removed.

### 3. Verify

```bash
curl -sI https://docs.murmur.nexus/concepts/hooks | grep -i content-type      # text/html
curl -s  https://docs.murmur.nexus/concepts/hooks?mode=agent | head -3        # frontmatter
curl -s -H "Accept: text/markdown" https://docs.murmur.nexus/concepts/hooks | head -3
```

## Link response header (one-time setup)

`setup-link-header.sh` creates one CloudFront Response Headers Policy,
`murmur-agent-link-header`, and attaches it to both `docs.murmur.nexus`
(`E3SVCJVONCNVPZ`) and `murmur.nexus` (`E8I1RI0YU23W1`). It adds:

```
Link: </llms.txt>; rel="service-doc"
```

on every response — RFC 8288 header syntax, `service-doc` per RFC 9727 §3
(a link to documentation intended for a human/agent audience). One policy
works for both distributions because `/llms.txt` is a site-relative path, so
the same header value resolves correctly against either domain — mirroring
`murmur-index-rewrite`, a single shared piece of infra rather than one per
site.

```bash
docs/infra/setup-link-header.sh
```

Idempotent — reuses the policy if it already exists, and skips a
distribution that's already attached. Verify:

```bash
curl -sI https://docs.murmur.nexus/ | grep -i '^link:'
curl -sI https://murmur.nexus/       | grep -i '^link:'
```

To roll back, detach the policy (`ResponseHeadersPolicyId` back to null) via
`update-distribution` on either distribution, or delete the policy once
detached from both.

## What you do NOT need

**No cache policy changes.** A viewer-request function runs before the cache
lookup, and the default cache key is domain + URL path. Because the function
rewrites the path, the HTML and markdown variants land on different cache keys
on their own.

Specifically, do not add `Accept` to the cache key. Accept strings vary widely
across browsers and bots, so including one fragments the cache into
near-duplicate objects and lowers the hit ratio without buying any correctness.

**No Lambda, no origin changes, no new bucket.** The markdown twins are plain
files already synced to S3 by `deploy.sh`.

## Files

| File | Purpose |
| --- | --- |
| `cloudfront-agent-negotiation.js` | The function CloudFront runs. ES5.1, under the 10 KB cap. |
| `cloudfront-agent-negotiation.test.mjs` | Tests the rewrite rules. Runs in CI via `npm test`. |
| `deploy-cloudfront-function.sh` | Uploads, tests and publishes the function. |
