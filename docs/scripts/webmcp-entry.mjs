/**
 * WebMCP registration, bundled to site/assets/agent/webmcp.js and loaded by
 * every page (see hooks/agent_head.py).
 *
 * Registers read-only docs tools against `document.modelContext` so a browser
 * agent on docs.murmur.nexus can search and read pages through tool calls
 * instead of scraping the DOM. Both tools run entirely client-side against the
 * generated search artifacts — no backend.
 *
 *   search-docs  BM25 query over the generated chunk index
 *   get-page     fetch one page's markdown twin
 *
 * On browsers without WebMCP support this is a no-op: `registerDocsWebMcpTools`
 * returns `{ supported: false }` rather than throwing.
 */

import { registerDocsWebMcpTools } from "leadtype/webmcp";

const registration = registerDocsWebMcpTools({
  // Artifacts sit at the site root, not under /docs — docs.murmur.nexus serves
  // the docs tree directly, so leadtype's default `/docs/...` paths don't apply.
  indexUrl: "/search-index.json",
  contentUrl: "/search-content.json",
  // Mirror layout: `/concepts/hooks` -> `/concepts/hooks.md`, root -> `/index.md`.
  markdownUrl: (urlPath) => (urlPath === "/" ? "/index.md" : `${urlPath}.md`),
});

/*
 * ask-docs seam.
 *
 * A third tool — natural-language answers grounded in these docs — needs a
 * server: leadtype's `streamDocsAnswer` runs on the Vercel AI SDK and holds a
 * model API key, and docs.murmur.nexus is static S3 behind CloudFront with
 * nothing to run it. When an endpoint exists, add it here alongside the two
 * above:
 *
 *   import { createDocsWebMcpTools, registerWebMcpTools } from "leadtype/webmcp";
 *
 *   const askDocs = {
 *     name: "ask-docs",
 *     description: "Answer a question using the murmur documentation.",
 *     inputSchema: {
 *       type: "object",
 *       properties: { question: { type: "string" } },
 *       required: ["question"],
 *     },
 *     annotations: { readOnlyHint: true },
 *     execute: async ({ question }) => {
 *       const response = await fetch("/api/ask", {
 *         method: "POST",
 *         headers: { "content-type": "application/json" },
 *         body: JSON.stringify({ question }),
 *       });
 *       if (!response.ok) throw new Error(`ask-docs failed: ${response.status}`);
 *       return response.text();
 *     },
 *   };
 *
 *   registerWebMcpTools([...createDocsWebMcpTools(options), askDocs]);
 *
 * Registering it against a missing endpoint is worse than not registering it:
 * the agent sees a tool it can call and gets a 403 from CloudFront.
 */

if (typeof window !== "undefined") {
  window.addEventListener("pagehide", () => registration.unregister(), { once: true });
}
