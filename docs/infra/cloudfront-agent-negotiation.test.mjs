/**
 * Tests for the CloudFront viewer-request function.
 *
 * The function runs at the edge on every request to docs.murmur.nexus, where a
 * wrong rewrite is invisible until someone reports that the docs "return raw
 * text in the browser". This function is merged with the distribution's
 * pre-existing directory-index rewrite (see the file header), so these cases
 * pin four things: an agent asking for markdown gets the twin, nobody else
 * ever does, a page has exactly one URL that serves it, and the original
 * index.html rewrite still fires for plain directory requests.
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

const request = (uri, { accept, mode, querystring, host = "docs.murmur.nexus" } = {}) => ({
  request: {
    uri,
    querystring: querystring ?? (mode ? { mode: { value: mode } } : {}),
    headers: {
      ...(accept ? { accept: { value: accept } } : {}),
      ...(host ? { host: { value: host } } : {}),
    },
  },
});

const uriFor = (...args) => {
  const result = handler(request(...args));
  assert.equal(result.statusCode, undefined, `expected a rewrite, got ${result.statusCode}`);
  return result.uri;
};

const locationFor = (...args) => {
  const result = handler(request(...args));
  assert.equal(result.statusCode, 301);
  assert.equal(result.statusDescription, "Moved Permanently");
  return result.headers.location.value;
};

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

test("markdown negotiation wins over the trailing-slash redirect", () => {
  // Both agent entry points are documented against the slashless form, so a
  // redirect firing first would cost every agent an extra round trip — and
  // clients that do not follow redirects would get the 301 instead of content.
  assert.equal(uriFor("/concepts/hooks", { mode: "agent" }), "/concepts/hooks.md");
  assert.equal(uriFor("/reference/cli", { accept: "text/markdown" }), "/reference/cli.md");
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

test("the slashless form of a page redirects to the trailing-slash form", () => {
  // Serving it instead would give the page a second URL whose relative nav
  // links resolve one segment too high: href="../artifacts/" reached from
  // /concepts/access-control becomes /artifacts/, which is a 404.
  assert.equal(
    locationFor("/concepts/access-control", { accept: BROWSER_ACCEPT }),
    "https://docs.murmur.nexus/concepts/access-control/"
  );
  assert.equal(locationFor("/reference/cli"), "https://docs.murmur.nexus/reference/cli/");
});

test("the redirect is cacheable by the client", () => {
  // A viewer-request response is generated before the cache lookup, so it is
  // never stored at the edge — the client's own cache is the only one.
  const result = handler(request("/reference/cli"));
  assert.equal(result.headers["cache-control"].value, "public, max-age=86400");
});

test("the redirect keeps the query string", () => {
  assert.equal(
    locationFor("/reference/cli", { querystring: { h: { value: "mur+run" } } }),
    "https://docs.murmur.nexus/reference/cli/?h=mur+run"
  );
  assert.equal(
    locationFor("/reference/cli", { querystring: { mode: { value: "human" }, h: { value: "x" } } }),
    "https://docs.murmur.nexus/reference/cli/?mode=human&h=x"
  );
});

test("the redirect keeps repeated and valueless parameters", () => {
  assert.equal(
    locationFor("/reference/cli", {
      querystring: { tag: { value: "a", multiValue: [{ value: "a" }, { value: "b" }] } },
    }),
    "https://docs.murmur.nexus/reference/cli/?tag=a&tag=b"
  );
  assert.equal(
    locationFor("/reference/cli", { querystring: { print: { value: "" } } }),
    "https://docs.murmur.nexus/reference/cli/?print"
  );
});

test("a missing Host header falls back to a relative Location", () => {
  // Host is always present in practice; a relative Location is still valid per
  // RFC 7231 §7.1.2, so the redirect degrades rather than pointing at
  // "https:///reference/cli/".
  assert.equal(locationFor("/reference/cli", { host: null }), "/reference/cli/");
});

test("paths with a dot anywhere are not redirected", () => {
  // Same guard the index.html rewrite used for this branch, so the set of URIs
  // claimed here is unchanged. The api-catalog linkset is extensionless but
  // lives under a dotted directory, and it is a file, not a page.
  assert.equal(uriFor("/.well-known/api-catalog"), "/.well-known/api-catalog");
  assert.equal(uriFor("/release-1.0"), "/release-1.0");
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
