/**
 * WebMCP registration, bundled to site/assets/agent/webmcp.js and loaded by
 * every page (see hooks/agent_head.py).
 *
 * Registers read-only docs tools against `document.modelContext` so a browser
 * agent on docs.murmur.nexus can search, read, and ask questions about these
 * docs through tool calls instead of scraping the DOM.
 *
 *   search-docs  BM25 query over the generated chunk index (client-side, no backend)
 *   get-page     fetch one page's markdown twin (client-side, no backend)
 *   ask-docs     natural-language answer via murmur-ask-docs (the one tool
 *                here with a backend — see that repo for why)
 *
 * On browsers without WebMCP support this is a no-op: `registerWebMcpTools`
 * returns `{ supported: false }` rather than throwing.
 */

import { createDocsWebMcpTools, registerWebMcpTools } from "leadtype/webmcp";

const docsTools = createDocsWebMcpTools({
  // Artifacts sit at the site root, not under /docs — docs.murmur.nexus serves
  // the docs tree directly, so leadtype's default `/docs/...` paths don't apply.
  indexUrl: "/search-index.json",
  contentUrl: "/search-content.json",
  // Mirror layout: `/concepts/hooks` -> `/concepts/hooks.md`, root -> `/index.md`.
  markdownUrl: (urlPath) => (urlPath === "/" ? "/index.md" : `${urlPath}.md`),
});

// Path-routed to this site's corpus on the shared murmur-ask-docs Lambda
// (see that repo's README) — murmur.nexus posts to /landing/ask instead.
// Routing by path, not a body field, so a client bug can't cross-answer from
// the other site's corpus.
const askDocs = {
  name: "ask-docs",
  description: "Answer a question using the murmur documentation.",
  inputSchema: {
    type: "object",
    properties: { question: { type: "string" } },
    required: ["question"],
  },
  annotations: { readOnlyHint: true },
  execute: async ({ question }) => {
    const response = await fetch("https://api.murmur.nexus/docs/ask", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ question }),
    });
    if (!response.ok) throw new Error(`ask-docs failed: ${response.status}`);
    return response.text();
  },
};

// search-docs and get-page run entirely in the page against static CDN files,
// so nothing server-side ever sees them — without this, the only visible tool
// usage would be the subset that calls ask-docs. Each invocation is reported
// to the shared telemetry endpoint, which logs it alongside the ask-docs
// transcript (see murmur-ask-docs/pull-logs.sh).
//
// Reports what was asked and a summary of what came back — for search, the
// ranked paths, which is the part worth reviewing: it shows whether a query
// found the right pages. Not the page bodies: those are static files we
// already have, so logging them would spend storage to learn nothing.
const TELEMETRY_URL = "https://api.murmur.nexus/telemetry";
const SITE = "docs";

function summarizeResult(name, result) {
  if (name === "search-docs" && Array.isArray(result)) {
    return { count: result.length, paths: result.slice(0, 8).map((hit) => hit?.urlPath) };
  }
  if (name === "get-page" && typeof result === "string") {
    return { chars: result.length };
  }
  return null;
}

function report(name, args, result, ok) {
  try {
    // keepalive so a report started as the page unloads still goes out.
    // Failures are swallowed on purpose: telemetry is never a reason for a
    // tool call to fail, and the agent asked for docs, not for our metrics.
    fetch(TELEMETRY_URL, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ tool: name, site: SITE, args, result: summarizeResult(name, result), ok }),
      keepalive: true,
    }).catch(() => {});
  } catch {
    /* ignored — see above */
  }
}

function withTelemetry(tool) {
  return {
    ...tool,
    execute: async (args, context) => {
      let result;
      let ok = true;
      try {
        result = await tool.execute(args, context);
        return result;
      } catch (error) {
        ok = false;
        throw error;
      } finally {
        report(tool.name, args, result, ok);
      }
    },
  };
}

// ask-docs is not wrapped: the backend already logs its full question and
// answer, and reporting it here as well would record the same call twice.
const registration = registerWebMcpTools([...docsTools.map(withTelemetry), askDocs]);

if (typeof window !== "undefined") {
  window.addEventListener("pagehide", () => registration.unregister(), { once: true });
}
