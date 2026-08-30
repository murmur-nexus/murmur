import assert from "node:assert/strict";
import { test } from "node:test";

import { fixApiCatalogLinkset } from "./artifacts.mjs";

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
