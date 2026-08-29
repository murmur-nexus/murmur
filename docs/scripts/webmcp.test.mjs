/**
 * End-to-end test for the WebMCP bundle against the built site.
 *
 * The tools resolve three URLs at runtime — the search index, the content file,
 * and each page's markdown twin. Nothing at build time checks that those paths
 * match what the site actually serves, so a wrong one fails only in a browser
 * agent, silently, in production. (This test is how the `/docs/...` prefix bug
 * in the staged search index was found.)
 *
 * Run after a build:
 *   mkdocs build && node scripts/agent-artifacts.mjs && node --test scripts/webmcp.test.mjs
 */

import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { existsSync } from "node:fs";
import path from "node:path";
import { before, test } from "node:test";
import { fileURLToPath } from "node:url";

const SITE = fileURLToPath(new URL("../site", import.meta.url));
const BUNDLE = path.join(SITE, "assets", "agent", "webmcp.js");

const registered = [];
const fetched = [];
const missing = [];

before(async () => {
  assert.ok(
    existsSync(BUNDLE),
    `${BUNDLE} is missing. Run \`mkdocs build && node scripts/agent-artifacts.mjs\` first.`
  );

  // Serve from the built site the way CloudFront serves these same paths, so a
  // URL the site does not have shows up here as a 404 rather than as a pass.
  globalThis.fetch = async (url) => {
    const urlPath = String(url).replace(/^https?:\/\/[^/]+/, "");
    fetched.push(urlPath);
    const file = path.join(SITE, urlPath);
    try {
      const body = await readFile(file, "utf8");
      return { ok: true, status: 200, statusText: "OK", text: async () => body, json: async () => JSON.parse(body) };
    } catch {
      missing.push(urlPath);
      return { ok: false, status: 404, statusText: "Not Found", text: async () => "", json: async () => ({}) };
    }
  };
  globalThis.window = { addEventListener() {} };
  globalThis.document = { modelContext: { registerTool: (tool) => registered.push(tool) } };

  await import(BUNDLE);
});

const tool = (name) => {
  const found = registered.find((t) => t.name === name);
  assert.ok(found, `tool ${name} was not registered (got: ${registered.map((t) => t.name).join(", ")})`);
  return found;
};

test("registers exactly the client-side tools", () => {
  assert.deepEqual(
    registered.map((t) => t.name).sort(),
    ["get-page", "search-docs"],
    "ask-docs needs a server endpoint and must not be registered until one exists"
  );
});

test("registration performs no network I/O", () => {
  // The bundle loads on every page view; fetching a ~1 MB index eagerly would
  // be a real cost paid by every human reader for a tool no one called.
  assert.deepEqual(fetched, []);
});

test("search-docs returns paths the site actually serves", async () => {
  const results = await tool("search-docs").execute({ query: "hook commit policy", limit: 5 }, {});

  assert.ok(results.length > 0, "expected hits for a term that appears in the docs");
  for (const hit of results) {
    assert.match(hit.urlPath, /^\//, `urlPath should be site-absolute: ${hit.urlPath}`);
    assert.doesNotMatch(
      hit.urlPath,
      /^\/docs\//,
      `urlPath leaked the search-index staging prefix: ${hit.urlPath}`
    );
    const twin = hit.urlPath === "/" ? "/index.md" : `${hit.urlPath}.md`;
    assert.ok(existsSync(path.join(SITE, twin)), `no markdown twin built for ${hit.urlPath}`);
  }
});

test("get-page returns the markdown twin", async () => {
  const page = await tool("get-page").execute({ urlPath: "/concepts/hooks" }, {});
  assert.match(page, /^---\ntitle: "Hooks"/, "expected the mirror's frontmatter");
  assert.ok(page.includes("commit_policy"), "expected the page body");
});

test("the site root resolves", async () => {
  const page = await tool("get-page").execute({ urlPath: "/" }, {});
  assert.ok(page.length > 0);
});

test("every URL the tools requested exists in the build", () => {
  assert.deepEqual(missing, [], `these fetches 404'd against the built site: ${missing.join(", ")}`);
});
