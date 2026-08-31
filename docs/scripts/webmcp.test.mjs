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
// ask-docs posts to an absolute external URL (the shared murmur-ask-docs
// Lambda), not a same-origin path — intercepted separately so it never
// pollutes `fetched`/`missing`, which are about this site's own artifacts.
const askDocsCalls = [];
// Telemetry goes to the same host; kept in its own list so the ask-docs
// assertions stay about ask-docs.
const telemetryCalls = [];
// When true, the telemetry endpoint never answers — used to prove a tool does
// not wait on its own telemetry.
let telemetryHangs = false;

before(async () => {
  assert.ok(
    existsSync(BUNDLE),
    `${BUNDLE} is missing. Run \`mkdocs build && node scripts/agent-artifacts.mjs\` first.`
  );

  // Serve from the built site the way CloudFront serves these same paths, so a
  // URL the site does not have shows up here as a 404 rather than as a pass.
  globalThis.fetch = async (url, init) => {
    const urlStr = String(url);
    if (urlStr === "https://api.murmur.nexus/telemetry") {
      telemetryCalls.push(JSON.parse(init.body));
      if (telemetryHangs) return new Promise(() => {});
      return { ok: true, status: 204, statusText: "No Content", text: async () => "", json: async () => ({}) };
    }
    if (urlStr.startsWith("https://api.murmur.nexus/")) {
      askDocsCalls.push({ url: urlStr, body: init?.body });
      return { ok: true, status: 200, statusText: "OK", text: async () => "canned answer", json: async () => ({}) };
    }

    const urlPath = urlStr.replace(/^https?:\/\/[^/]+/, "");
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

test("registers exactly the expected tools", () => {
  assert.deepEqual(registered.map((t) => t.name).sort(), ["ask-docs", "get-page", "search-docs"]);
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

test("ask-docs posts to this site's own path on the shared endpoint", async () => {
  const result = await tool("ask-docs").execute({ question: "what is a capsule?" }, {});
  assert.deepEqual(askDocsCalls, [
    { url: "https://api.murmur.nexus/docs/ask", body: JSON.stringify({ question: "what is a capsule?" }) },
  ]);
  assert.equal(result, "canned answer");
});

test("client-side tools report what was asked and what came back", () => {
  // search-docs and get-page never touch the backend, so this report is the
  // only record that they ran at all.
  const search = telemetryCalls.find((c) => c.tool === "search-docs");
  assert.ok(search, `no search-docs telemetry (got: ${telemetryCalls.map((c) => c.tool).join(", ")})`);
  assert.equal(search.site, "docs");
  assert.equal(search.ok, true);
  // The query and the paths it returned — enough to tell later whether a
  // search found the right pages, which is the whole point of collecting it.
  assert.equal(search.args.query, "hook commit policy");
  assert.ok(search.result.count > 0);
  assert.ok(search.result.paths.every((p) => typeof p === "string" && p.startsWith("/")));

  const getPage = telemetryCalls.find((c) => c.tool === "get-page");
  assert.ok(getPage, "no get-page telemetry");
  assert.equal(getPage.args.urlPath, "/concepts/hooks");
  // Page bodies are static files we already have; only the size is reported.
  assert.ok(getPage.result.chars > 0);
  assert.equal(getPage.result.body, undefined);
});

test("ask-docs is not double-reported — the backend already logs it", () => {
  assert.equal(telemetryCalls.filter((c) => c.tool === "ask-docs").length, 0);
});

test("a tool never waits on its own telemetry", async () => {
  // The whole reason telemetry is acceptable on a client-side tool is that it
  // costs the caller nothing: the report is fired, not awaited. If that ever
  // regressed to an `await`, every search would inherit a round trip to
  // api.murmur.nexus — and the failure would be invisible in production,
  // showing up only as tools that feel slow.
  //
  // Here the endpoint never answers at all. The tool must still resolve.
  telemetryHangs = true;
  try {
    const results = await tool("search-docs").execute({ query: "capsule", limit: 3 }, {});
    assert.ok(Array.isArray(results), "tool resolved while its telemetry is still in flight");
    const page = await tool("get-page").execute({ urlPath: "/" }, {});
    assert.ok(page.length > 0);
  } finally {
    telemetryHangs = false;
  }
});

test("a failing tool still reports, and reports the failure", async () => {
  const before = telemetryCalls.length;
  await assert.rejects(() => tool("get-page").execute({ urlPath: "/does-not-exist" }, {}));
  const reported = telemetryCalls.slice(before).find((c) => c.tool === "get-page");
  assert.ok(reported, "a failed tool call must still be reported");
  assert.equal(reported.ok, false);
});

test("ask-docs is read-only and takes only a question", () => {
  const t = tool("ask-docs");
  assert.equal(t.annotations?.readOnlyHint, true);
  assert.deepEqual(t.inputSchema.required, ["question"]);
});

test("every URL the tools requested exists in the build", () => {
  assert.deepEqual(missing, [], `these fetches 404'd against the built site: ${missing.join(", ")}`);
});
