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

## Shared agent-discovery response headers (one-time setup)

`setup-response-headers.sh` creates one CloudFront Response Headers Policy,
`murmur-agent-link-header`, and attaches it to both `docs.murmur.nexus`
(`E3SVCJVONCNVPZ`) and `murmur.nexus` (`E8I1RI0YU23W1`). It adds, on every
response from either distribution:

```
Link: </llms.txt>; rel="service-doc"
Access-Control-Allow-Origin: *          (only echoed back when the request sends an Origin header — standard CORS behavior)
```

The `Link` header is RFC 8288 syntax, `service-doc` per RFC 9727 §3 (a link
to documentation intended for a human/agent audience). The CORS header is
what [ARD](https://agenticresourcediscovery.org/)'s manifest at
`/.well-known/ai-catalog.json` requires — see below — set via the policy's
`CorsConfig` (CloudFront rejects a bare CORS header name inside
`CustomHeadersConfig`). Both are safe to apply site-wide: these are fully
public static sites with no auth/cookies, so allowing cross-origin JS to
*read* a response changes nothing about who can already reach it over plain
HTTP. One policy works for both distributions because `/llms.txt` is a
site-relative path, so the same `Link` value resolves correctly against
either domain — mirroring `murmur-index-rewrite`, a single shared piece of
infra rather than one per site.

```bash
docs/infra/setup-response-headers.sh
```

Idempotent — creates the policy if missing, updates it in place if the
header set has changed since it was created, and skips a distribution
that's already attached. Verify (the CORS header only shows with an `Origin`
request header, matching real cross-origin fetch behavior):

```bash
curl -sI https://docs.murmur.nexus/ | grep -i '^link:'
curl -sI https://murmur.nexus/       | grep -i '^link:'
curl -sI -H 'Origin: https://example.com' https://docs.murmur.nexus/.well-known/ai-catalog.json | grep -i '^access-control'
```

To roll back, detach the policy (`ResponseHeadersPolicyId` back to null) via
`update-distribution` on either distribution, or delete the policy once
detached from both.

## ARD manifest (`/.well-known/ai-catalog.json`)

Each site publishes its own [Agentic Resource Discovery](https://agenticresourcediscovery.org/)
manifest — a JSON document listing the site's real, callable
agent-facing capabilities, so a registry can index them without crawling.

Hand-curated at `docs/ai-catalog.json` (repo root, same pattern as
`docs/llms.txt`), copied verbatim into `site/.well-known/ai-catalog.json` by
`scripts/agent-artifacts.mjs` — leadtype doesn't generate this format, so
there's no generated version to seed from or overwrite. Two entries: the
site's own `llms.txt`, and the shared `ask-docs` API (its OpenAPI document is
served by the `murmur-ask-docs` repo at `https://api.murmur.nexus/openapi.yaml`).

Deliberately does **not** claim an MCP server or an A2A agent card — this
site doesn't run either (WebMCP is browser-side JS, not a network protocol
server), and an ARD entry is a claim that a real callable resource exists at
that URL with that type.

## Agent Skills index (`/.well-known/agent-skills/index.json`)

Publishes murmur's skills for discovery per the [Agent Skills Discovery
RFC](https://github.com/cloudflare/agent-skills-discovery-rfc) v0.2.0. Each
skill's markdown is served at `/skills/<name>.md` and listed in the index
with a sha256 digest, so a client can verify it fetched what was advertised.

**`docs/skills/` in this repo is the source of truth.** Skills authored
elsewhere (e.g. a local agent workspace) must be copied here — the docs
deploy runs in GitHub Actions, which can only see the repo.

To add a skill, create `docs/skills/<name>/SKILL.md` with frontmatter:

```markdown
---
name: <name>
description: What it does. When to use it; when not to.
---
```

`scripts/lib/skills.mjs` does the rest on the next build. Two things it
guarantees, both covered by `scripts/lib/skills.test.mjs`:

- **The digest is of the bytes actually served**, not the source file — a
  digest derived from the source would silently stop matching if the copy
  ever transformed anything, quietly falsifying the index's one integrity
  claim.
- **A malformed skill fails the build**, rather than being skipped. Missing
  frontmatter, a missing `name`/`description`, a `name` that disagrees with
  its directory, or a name outside `[a-z0-9-]` all throw. A skill silently
  absent from the index is indistinguishable from one that was never
  written.

Skills live outside `content/` on purpose: they're artifacts for agents to
fetch, not pages for humans to browse, so MkDocs never renders them and they
stay out of the nav, sitemap, search index, `llms.txt` and
`agent-readability.json`. The only place they appear is the index that must
reference them.

Only `type: "skill-md"` is published. Murmur's `.mur.zip` packaging maps onto
the RFC's `"archive"` type, but a `.mur.zip` is installed by the murmur
runtime rather than fetched and read by an arbitrary agent — indexing one
would advertise something most clients can't consume.

Verify:

```bash
curl -s https://docs.murmur.nexus/.well-known/agent-skills/index.json
# then confirm a published digest matches what is actually served:
curl -s https://docs.murmur.nexus/skills/secure-murmur-capsule.md | shasum -a 256
```

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
