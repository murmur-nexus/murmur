import assert from "node:assert/strict";
import { test } from "node:test";

import { canonicalPageUrl, fixApiCatalogLinkset, fixSitemapMarkdown, fixSitemapXml } from "./artifacts.mjs";

test("fixApiCatalogLinkset: strips leadtype's hardcoded /docs/ prefix", () => {
  const input = {
    linkset: [
      {
        anchor: "https://docs.murmur.nexus/",
        "api-catalog": [
          { href: "https://docs.murmur.nexus/.well-known/api-catalog", type: "application/linkset+json" },
        ],
        "service-doc": [{ href: "https://docs.murmur.nexus/docs/llms.txt", type: "text/plain" }],
        "service-desc": [
          { href: "https://docs.murmur.nexus/docs/agent-readability.json", type: "application/json" },
        ],
        describedby: [{ href: "https://docs.murmur.nexus/sitemap.xml", type: "application/xml" }],
      },
    ],
  };

  const fixed = fixApiCatalogLinkset(input);
  const entry = fixed.linkset[0];

  assert.equal(entry["service-doc"][0].href, "https://docs.murmur.nexus/llms.txt");
  assert.equal(entry["service-desc"][0].href, "https://docs.murmur.nexus/agent-readability.json");
  // Untouched entries must survive exactly as they came in.
  assert.equal(entry["api-catalog"][0].href, "https://docs.murmur.nexus/.well-known/api-catalog");
  assert.equal(entry.describedby[0].href, "https://docs.murmur.nexus/sitemap.xml");
});

test("fixApiCatalogLinkset: a future leadtype version without the bug is a no-op", () => {
  const input = {
    linkset: [
      {
        "service-doc": [{ href: "https://docs.murmur.nexus/llms.txt", type: "text/plain" }],
      },
    ],
  };

  const fixed = fixApiCatalogLinkset(input);
  assert.equal(fixed.linkset[0]["service-doc"][0].href, "https://docs.murmur.nexus/llms.txt");
});

test("fixApiCatalogLinkset: missing linkset/service-doc/service-desc doesn't throw", () => {
  assert.doesNotThrow(() => fixApiCatalogLinkset({}));
  assert.doesNotThrow(() => fixApiCatalogLinkset({ linkset: [{}] }));
});

test("canonicalPageUrl: adds the trailing slash to a page path or URL", () => {
  assert.equal(canonicalPageUrl("/concepts/hooks"), "/concepts/hooks/");
  assert.equal(canonicalPageUrl("https://docs.murmur.nexus/concepts/hooks"), "https://docs.murmur.nexus/concepts/hooks/");
});

test("canonicalPageUrl: leaves the root, slashed paths and files alone", () => {
  assert.equal(canonicalPageUrl("/"), "/");
  assert.equal(canonicalPageUrl("https://docs.murmur.nexus/"), "https://docs.murmur.nexus/");
  assert.equal(canonicalPageUrl("/concepts/hooks/"), "/concepts/hooks/");
  assert.equal(canonicalPageUrl("/llms.txt"), "/llms.txt");
  assert.equal(canonicalPageUrl("/concepts/hooks.md"), "/concepts/hooks.md");
  // The host has dots in it; only the path decides whether this names a file.
  assert.equal(canonicalPageUrl("https://docs.murmur.nexus/reference/cli"), "https://docs.murmur.nexus/reference/cli/");
});

test("fixSitemapXml: every <loc> names the page, not the redirect", () => {
  const input = `<?xml version="1.0" encoding="UTF-8"?>
<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
  <url>
    <loc>https://docs.murmur.nexus/</loc>
    <lastmod>2026-09-01</lastmod>
  </url>
  <url>
    <loc>https://docs.murmur.nexus/concepts/hooks</loc>
    <lastmod>2026-09-02</lastmod>
  </url>
</urlset>
`;
  const fixed = fixSitemapXml(input);
  assert.match(fixed, /<loc>https:\/\/docs\.murmur\.nexus\/<\/loc>/);
  assert.match(fixed, /<loc>https:\/\/docs\.murmur\.nexus\/concepts\/hooks\/<\/loc>/);
  assert.match(fixed, /<lastmod>2026-09-02<\/lastmod>/);
  assert.doesNotMatch(fixed, /<loc>[^<]*[^/]<\/loc>/);
});

test("fixSitemapXml: a future leadtype version that writes slashes is a no-op", () => {
  const input = "<urlset><url><loc>https://docs.murmur.nexus/concepts/hooks/</loc></url></urlset>";
  assert.equal(fixSitemapXml(input), input);
});

test("fixSitemapMarkdown: page links get the slash, descriptions and files do not change", () => {
  const input = [
    "# Sitemap",
    "",
    "- [Hooks](/concepts/hooks): A hook observes a lifecycle point (see /concepts/tools).",
    "- [Home](/): The overview.",
    "- [Full corpus](/llms-full.txt): Everything.",
    "- [Upstream](https://github.com/murmur-nexus/murmur): Source.",
    "",
  ].join("\n");
  const fixed = fixSitemapMarkdown(input);
  assert.match(fixed, /\[Hooks\]\(\/concepts\/hooks\/\): A hook observes a lifecycle point \(see \/concepts\/tools\)\./);
  assert.match(fixed, /\[Home\]\(\/\)/);
  assert.match(fixed, /\[Full corpus\]\(\/llms-full\.txt\)/);
  assert.match(fixed, /\[Upstream\]\(https:\/\/github\.com\/murmur-nexus\/murmur\)/);
});
