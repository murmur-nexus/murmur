/**
 * The artifact steps that sit outside `generateAgentArtifacts()`.
 *
 * Leadtype's no-docs-tree escape hatch emits llms.txt, markdown mirrors,
 * sitemap, robots and the readability manifest — but not llms-full.txt and not
 * the search index, both of which live on the docs-tree code path and assume
 * artifacts under `<outDir>/docs/`. These helpers supply them for a site served
 * from its own root.
 */

import { cp, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { existsSync } from "node:fs";
import os from "node:os";
import path from "node:path";

import { generateDocsSearchFiles } from "leadtype/search/node";

/**
 * Concatenate every page into one file at the site root.
 *
 * llms-full.txt is the whole-corpus fallback: an agent that can't resolve a
 * question from llms.txt's page map fetches this instead of walking every
 * mirror. Pages appear in nav order, each under its canonical URL.
 */
export async function writeLlmsFullTxt({ outDir, baseUrl, product, pages }) {
  const ordered = [...pages].sort((a, b) => (a.order ?? 0) - (b.order ?? 0));

  const sections = ordered.map((page) => {
    const canonical = new URL(page.urlPath, `${baseUrl}/`).toString();
    const heading = [`# ${page.title}`, "", `Source: ${canonical}`];
    if (page.description) heading.push("", `> ${page.description}`);

    // Demote body headings one level so the per-page H1 above stays the only
    // top-level heading in the concatenated file.
    const body = page.content
      .replace(/^---\n[\s\S]*?\n---\n/, "")
      .replace(/^(#{1,5}) /gm, "#$1 ")
      .trim();

    return [...heading, "", body].join("\n");
  });

  const header = [
    `# ${product.name}`,
    "",
    `> ${product.tagline}`,
    "",
    `Full documentation corpus, ${ordered.length} pages. ` +
      `Individual pages are available as markdown at their canonical URL plus \`.md\`.`,
    "",
  ].join("\n");

  const file = path.join(outDir, "llms-full.txt");
  await writeFile(file, `${header}\n${sections.join("\n\n---\n\n")}\n`, "utf8");
  return file;
}

/**
 * Build the BM25 search index the WebMCP tools query.
 *
 * `generateDocsSearchFiles` insists on reading markdown from
 * `<outDir>/docs/` — a docs-tree assumption that doesn't hold here, where the
 * mirrors sit at the site root. Staging a throwaway tree with the expected
 * shape is cheaper and less brittle than reimplementing the indexer.
 */
export async function writeSearchIndex({ outDir, baseUrl, markdownFiles }) {
  const stage = await mkdtempAgent();

  try {
    const stageDocs = path.join(stage, "docs");
    await mkdir(stageDocs, { recursive: true });

    for (const file of markdownFiles) {
      const relative = path.relative(outDir, file);
      const target = path.join(stageDocs, relative);
      await mkdir(path.dirname(target), { recursive: true });
      await cp(file, target);
    }

    const result = await generateDocsSearchFiles({
      outDir: stage,
      baseUrl,
      // The staging directory is named `docs/` because the indexer demands it,
      // and without this mount every indexed urlPath would inherit that as a
      // `/docs/...` prefix — URLs that 404 on a site served from its own root,
      // and which would make `get-page` reject every real path.
      mounts: [{ pathPrefix: "", urlPrefix: "/" }],
      // Keep chunk text in a separate file rather than embedded in the index.
      // Every page load fetches the index to answer `search-docs`; only a hit
      // that needs a snippet pulls the heavier content file.
      embedContent: false,
    });

    if (!result.contentOutputPath) {
      throw new Error(
        "generateDocsSearchFiles produced no search-content.json — the WebMCP client " +
          "fetches it at /search-content.json and would 404."
      );
    }

    const indexFile = path.join(outDir, "search-index.json");
    const contentFile = path.join(outDir, "search-content.json");

    await cp(result.outputPath, indexFile);
    if (result.contentOutputPath) await cp(result.contentOutputPath, contentFile);

    return {
      indexFile,
      contentFile: result.contentOutputPath ? contentFile : undefined,
      docs: result.docs,
      chunks: result.chunks,
    };
  } finally {
    await rm(stage, { recursive: true, force: true });
  }
}

async function mkdtempAgent() {
  const base = path.join(os.tmpdir(), "murmur-docs-search-");
  const { mkdtemp } = await import("node:fs/promises");
  return mkdtemp(base);
}

/**
 * Replace the generated llms.txt with the curated source file.
 *
 * llms.txt is the front door: it is the one artifact worth writing by hand,
 * because it says which pages matter and why. Leadtype writes a mechanical
 * version from the nav; this overwrites it with `docs/llms.txt`, keeping the
 * `.well-known` discovery copy in sync.
 */
export async function applyCuratedLlmsTxt({ outDir, sourceFile }) {
  if (!existsSync(sourceFile)) return { applied: false };

  const curated = await readFile(sourceFile, "utf8");
  const targets = [
    path.join(outDir, "llms.txt"),
    path.join(outDir, ".well-known", "llms.txt"),
  ];

  for (const target of targets) {
    await mkdir(path.dirname(target), { recursive: true });
    await writeFile(target, curated, "utf8");
  }

  return { applied: true, targets };
}

/**
 * Seed a first-draft llms.txt from the nav so the curated file starts from
 * something editable. Only ever writes when the file is absent — a curated
 * file is never overwritten by the build.
 */
export async function seedCuratedLlmsTxt({ sourceFile, product, groups, pages, baseUrl }) {
  if (existsSync(sourceFile)) return { seeded: false };

  const byGroup = new Map();
  for (const page of pages) {
    const key = page.groups?.[0] ?? "";
    if (!byGroup.has(key)) byGroup.set(key, []);
    byGroup.get(key).push(page);
  }

  // Walk the group tree depth-first so sections come out in nav order rather
  // than in whatever order the pages happened to be sorted.
  const flatGroups = [];
  const walk = (list) => {
    for (const group of list) {
      flatGroups.push(group);
      if (group.children) walk(group.children);
    }
  };
  walk(groups);

  const lines = [
    `# ${product.name}`,
    "",
    `> ${product.tagline}`,
    "",
    "<!--",
    "  This file is CURATED, not generated. The build copies it verbatim over",
    "  leadtype's generated llms.txt (scripts/agent-artifacts.mjs).",
    "",
    "  It was seeded once from the MkDocs nav. Edit freely: cut pages that do not",
    "  help an agent orient, reorder by importance rather than nav order, and",
    "  rewrite descriptions to say when a page is the right one to read.",
    "-->",
    "",
  ];

  const ungrouped = byGroup.get("") ?? [];
  if (ungrouped.length > 0) {
    lines.push("## Start here", "");
    for (const page of ungrouped) {
      lines.push(`- [${page.title}](${mirrorPath(page.urlPath)}): ${page.description ?? ""}`.trimEnd());
    }
    lines.push("");
  }

  for (const group of flatGroups) {
    const groupPages = byGroup.get(group.slug);
    if (!groupPages || groupPages.length === 0) continue;

    lines.push(`## ${group.title}`, "");
    for (const page of groupPages.sort((a, b) => (a.order ?? 0) - (b.order ?? 0))) {
      lines.push(`- [${page.title}](${mirrorPath(page.urlPath)}): ${page.description ?? ""}`.trimEnd());
    }
    lines.push("");
  }

  lines.push(
    "## Optional",
    "",
    `- [Full documentation corpus](${baseUrl}/llms-full.txt): Every page concatenated, for when the page map above is not enough.`,
    ""
  );

  await writeFile(sourceFile, lines.join("\n"), "utf8");
  return { seeded: true };
}

const mirrorPath = (urlPath) => (urlPath === "/" ? "/index.md" : `${urlPath}.md`);
