/**
 * Tests for the CloudFront viewer-request function.
 *
 * The function runs at the edge on every request to docs.murmur.nexus, where a
 * wrong rewrite is invisible until someone reports that the docs "return raw
 * text in the browser". This function is merged with the distribution's
 * pre-existing directory-index rewrite (see the file header), so these cases
 * pin three things: an agent asking for markdown gets the twin, nobody else
 * ever does, and the original index.html rewrite still fires for plain
 * requests exactly as it did before the merge.
 *
 *   node --test infra/cloudfront-agent-negotiation.test.mjs
 */

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

// CloudFront Functions are plain scripts with a global `handler`, not modules.
// Evaluate the real deployed file so the tests can never drift from it.
const source = readFileSync(fileURLToPath(new URL("./cloudfront-agent-negotiation.js", import.meta.url)), "utf8");
const handler = new Function(`${source}; return handler;`)();

const request = (uri, { accept, mode } = {}) => ({
  request: {
    uri,
    querystring: mode ? { mode: { value: mode } } : {},
    headers: accept ? { accept: { value: accept } } : {},
  },
});

const uriFor = (...args) => handler(request(...args)).uri;

const BROWSER_ACCEPT =
  "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8";

test("Accept: text/markdown rewrites to the twin", () => {
  assert.equal(uriFor("/concepts/hooks", { accept: "text/markdown" }), "/concepts/hooks.md");
});

test("directory URLs rewrite to the same twin", () => {
  assert.equal(uriFor("/concepts/hooks/", { accept: "text/markdown" }), "/concepts/hooks.md");
});

test("?mode=agent rewrites without any Accept header", () => {
  assert.equal(uriFor("/reference/cli/", { mode: "agent" }), "/reference/cli.md");
});

test("the site root pairs with /index.md", () => {
  assert.equal(uriFor("/", { accept: "text/markdown" }), "/index.md");
  assert.equal(uriFor("/", { mode: "agent" }), "/index.md");
});

test("browsers never get markdown, but still get the pre-existing index.html rewrite", () => {
  // Merged function: a non-agent request falls through to the original
  // murmur-index-rewrite rule instead of passing through untouched.
  assert.equal(uriFor("/concepts/hooks/", { accept: BROWSER_ACCEPT }), "/concepts/hooks/index.html");
  assert.equal(uriFor("/"), "/index.html");
});

test("a bare */* does not win markdown", () => {
  // curl sends `*/*`. Treating that as a markdown request would hand raw
  // markdown to every unspecified client, including search crawlers.
  assert.equal(uriFor("/concepts/hooks/", { accept: "*/*" }), "/concepts/hooks/index.html");
});

test("requests that already name a file are left alone", () => {
  // Otherwise /concepts/hooks.md would become /concepts/hooks.md.md.
  assert.equal(uriFor("/concepts/hooks.md", { accept: "text/markdown" }), "/concepts/hooks.md");
  assert.equal(uriFor("/llms.txt", { accept: "text/markdown" }), "/llms.txt");
  assert.equal(uriFor("/llms-full.txt", { mode: "agent" }), "/llms-full.txt");
  assert.equal(uriFor("/search-index.json", { mode: "agent" }), "/search-index.json");
  assert.equal(uriFor("/assets/agent/webmcp.js", { mode: "agent" }), "/assets/agent/webmcp.js");
  assert.equal(uriFor("/sitemap.xml", { accept: "text/markdown" }), "/sitemap.xml");
});

test("mode values other than agent are ignored, but the index.html rewrite still applies", () => {
  assert.equal(uriFor("/concepts/hooks/", { mode: "human" }), "/concepts/hooks/index.html");
});

test("pre-existing rule: extensionless non-directory paths also get /index.html", () => {
  // murmur-index-rewrite's second branch: no trailing slash, no dot anywhere
  // in the path -> append "/index.html" rather than "index.html".
  assert.equal(uriFor("/concepts/hooks"), "/concepts/hooks/index.html");
});

test("pre-existing rule: a directory path with a dot elsewhere still gets index.html appended", () => {
  // The original function's trailing-slash branch is unconditional on
  // extension, so the merge must preserve that rather than newly gating it.
  assert.equal(uriFor("/release-1.0/"), "/release-1.0/index.html");
});

test("a quality-weighted markdown Accept still matches", () => {
  assert.equal(
    uriFor("/concepts/hooks/", { accept: "text/markdown;q=0.9,text/html;q=0.8" }),
    "/concepts/hooks.md"
  );
});
