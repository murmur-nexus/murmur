#!/usr/bin/env node
/**
 * Emit the leadtype agent-artifact set into the built MkDocs site.
 *
 * Run after `mkdocs build`:
 *
 *     mkdocs build && node scripts/agent-artifacts.mjs
 *
 * Reads `.agent-export/pages.json` (written by hooks/agent_export.py during the
 * MkDocs build, so macros are expanded and snippets inlined), flattens
 * Material-specific markdown, and hands the result to leadtype's
 * `generateAgentArtifacts()`.
 *
 * Writes into `site/`: llms.txt, .well-known/llms.txt, .well-known/api-catalog,
 * .well-known/ai-catalog.json, .well-known/agent-skills/index.json plus
 * skills/*.md, one `.md` mirror per page, sitemap.xml, sitemap.md, robots.txt,
 * and agent-readability.json. MkDocs owns the HTML; this owns the agent surface.
 */

import { copyFile, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { existsSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { generateAgentArtifacts } from "leadtype/llm";

import {
  deriveDescription,
  flattenMaterialMarkdown,
  rewriteLinks,
} from "./lib/material-markdown.mjs";
import {
  applyCuratedLlmsTxt,
  fixApiCatalogLinkset,
  seedCuratedLlmsTxt,
  writeLlmsFullTxt,
  writeSearchIndex,
} from "./lib/artifacts.mjs";
import { bundleWebMcp } from "./lib/webmcp-bundle.mjs";
import { writeSkillsIndex } from "./lib/skills.mjs";

const DOCS_DIR = path.resolve(fileURLToPath(new URL("..", import.meta.url)));
const EXPORT_FILE = path.join(DOCS_DIR, ".agent-export", "pages.json");
const OUT_DIR = path.join(DOCS_DIR, "site");
// Hand-curated; the build copies it over leadtype's generated llms.txt.
const CURATED_LLMS_TXT = path.join(DOCS_DIR, "llms.txt");
// Hand-curated ARD manifest (agenticresourcediscovery.org) — leadtype doesn't
// generate this, so it's a plain copy, same pattern as CURATED_LLMS_TXT.
const CURATED_AI_CATALOG = path.join(DOCS_DIR, "ai-catalog.json");
// Agent Skills, one <name>/SKILL.md per directory. Kept out of content/ so
// MkDocs doesn't render them as doc pages — they're artifacts for agents to
// fetch, not pages for humans to browse.
const SKILLS_DIR = path.join(DOCS_DIR, "skills");

// Identity that isn't derivable from mkdocs.yml. Everything else — site name,
// description, base URL, repo — comes from the MkDocs config.
const ORGANIZATION = {
  name: "Murmur Nexus",
  url: "https://murmur.nexus",
};

// Crawler stance written into robots.txt as Content-Signals.
// "open" suits public docs for an open-source project: it invites agents to
// fetch, cite, and train. Switch to "balanced" or "block-training" to tighten.
const ROBOTS_POLICY = "open";

const slugify = (value) =>
  value
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "");

/**
 * Turn the MkDocs nav into leadtype groups, and record which group each page
 * belongs to. Nav sections nest, so groups nest the same way; only leaf groups
 * hold pages, which is what leadtype expects.
 */
function buildGroups(nav) {
  const groupBySrcUri = new Map();
  const orderBySrcUri = new Map();
  const navTitleBySrcUri = new Map();
  const usedSlugs = new Set();
  let counter = 0;

  const walk = (items, trail) => {
    const groups = [];

    for (const item of items) {
      if (item.type === "page") {
        // A page directly under a section belongs to that section's group.
        const owner = trail[trail.length - 1];
        if (owner) groupBySrcUri.set(item.srcUri, owner);
        orderBySrcUri.set(item.srcUri, counter++);
        // The nav title is what a reader sees in the sidebar ("Home"), and is
        // more deliberate than the H1 MkDocs falls back to ("Index").
        if (item.title) navTitleBySrcUri.set(item.srcUri, item.title);
        continue;
      }

      let slug = slugify(item.title);
      while (usedSlugs.has(slug)) slug = `${slug}-x`;
      usedSlugs.add(slug);

      const children = walk(item.children ?? [], [...trail, slug]);
      const group = { slug, title: item.title };
      if (children.length > 0) group.children = children;
      groups.push(group);
    }

    return groups;
  };

  const groups = walk(nav, []);

  // A page sitting under a section that also has child sections would land in a
  // non-leaf group, which leadtype rejects. Detect it rather than fail opaquely.
  const nonLeaf = new Set();
  const markNonLeaf = (list) => {
    for (const group of list) {
      if (group.children?.length) {
        nonLeaf.add(group.slug);
        markNonLeaf(group.children);
      }
    }
  };
  markNonLeaf(groups);

  for (const [srcUri, slug] of groupBySrcUri) {
    if (nonLeaf.has(slug)) {
      throw new Error(
        `${srcUri} sits in nav section "${slug}", which also contains subsections. ` +
          `Leadtype only lets leaf groups hold pages — move the page into a subsection.`
      );
    }
  }

  return { groups, groupBySrcUri, orderBySrcUri, navTitleBySrcUri };
}

async function main() {
  if (!existsSync(EXPORT_FILE)) {
    throw new Error(
      `Missing ${path.relative(process.cwd(), EXPORT_FILE)}. Run \`mkdocs build\` first — ` +
        `hooks/agent_export.py writes it during the build.`
    );
  }

  const exported = JSON.parse(await readFile(EXPORT_FILE, "utf8"));

  if (!exported.siteUrl) {
    throw new Error("mkdocs.yml needs a site_url — sitemap and canonical links depend on it.");
  }
  if (exported.pages.length === 0) {
    throw new Error("No pages in the export. Did the MkDocs build actually render anything?");
  }

  const urlPathBySrcUri = new Map(exported.pages.map((page) => [page.srcUri, page.urlPath]));
  const { groups, groupBySrcUri, orderBySrcUri, navTitleBySrcUri } = buildGroups(exported.nav);

  const undescribed = [];

  const pages = exported.pages.map((page) => {
    const linked = rewriteLinks(page.content, {
      srcUri: page.srcUri,
      urlPathBySrcUri,
      redirects: exported.redirects ?? {},
    });
    const content = flattenMaterialMarkdown(linked);

    // Authored frontmatter wins; otherwise derive from the body. Either beats
    // leadtype's "Reference page for hooks." fallback, which is what an agent
    // would otherwise read when choosing between pages.
    const description = page.description ?? deriveDescription(content);
    if (!page.description) undescribed.push(page.srcUri);

    const group = groupBySrcUri.get(page.srcUri);

    return {
      urlPath: page.urlPath,
      title: navTitleBySrcUri.get(page.srcUri) ?? page.title,
      ...(description ? { description } : {}),
      ...(page.lastModified ? { lastModified: page.lastModified } : {}),
      ...(group ? { groups: [group] } : {}),
      order: orderBySrcUri.get(page.srcUri) ?? 0,
      content,
    };
  });

  const product = {
    name: exported.siteName,
    tagline: exported.siteDescription ?? "",
    homepage: ORGANIZATION.url,
    docs: exported.siteUrl,
    repository: exported.repoUrl,
    kind: "library",
    category: "DeveloperApplication",
  };

  const result = await generateAgentArtifacts({
    outDir: OUT_DIR,
    baseUrl: exported.siteUrl,
    product,
    organization: ORGANIZATION,
    groups,
    pages,
    agents: { robots: { policy: ROBOTS_POLICY } },
  });

  // MkDocs writes sitemap.xml + sitemap.xml.gz; leadtype has just replaced the
  // former with a lastmod-bearing version. Drop the gzip copy rather than leave
  // a stale duplicate of the sitemap next to the fresh one.
  if (result.files.sitemapXml) {
    await rm(path.join(OUT_DIR, "sitemap.xml.gz"), { force: true });
  }

  // llms.txt is curated, not generated. Seed the source file on first run, then
  // copy it over whatever leadtype just wrote.
  const seed = await seedCuratedLlmsTxt({
    sourceFile: CURATED_LLMS_TXT,
    product,
    groups,
    pages,
    baseUrl: exported.siteUrl,
  });
  const curated = await applyCuratedLlmsTxt({
    outDir: OUT_DIR,
    sourceFile: CURATED_LLMS_TXT,
  });

  const llmsFull = await writeLlmsFullTxt({
    outDir: OUT_DIR,
    baseUrl: exported.siteUrl,
    product,
    pages,
  });

  const search = await writeSearchIndex({
    outDir: OUT_DIR,
    baseUrl: exported.siteUrl,
    markdownFiles: result.files.markdown,
  });

  const webmcp = await bundleWebMcp({ outDir: OUT_DIR });

  const wellKnownDir = path.join(OUT_DIR, ".well-known");
  await mkdir(wellKnownDir, { recursive: true });
  await copyFile(CURATED_AI_CATALOG, path.join(wellKnownDir, "ai-catalog.json"));

  const skills = await writeSkillsIndex({
    outDir: OUT_DIR,
    skillsDir: SKILLS_DIR,
    baseUrl: exported.siteUrl,
  });

  // See fixApiCatalogLinkset's own comment for why this rewrite is needed.
  if (result.files.apiCatalog) {
    const apiCatalog = fixApiCatalogLinkset(JSON.parse(await readFile(result.files.apiCatalog, "utf8")));
    await writeFile(result.files.apiCatalog, `${JSON.stringify(apiCatalog, null, 2)}\n`);
  }

  const rel = (file) => path.relative(DOCS_DIR, file);
  console.log(`agent-artifacts: ${pages.length} pages -> ${path.relative(DOCS_DIR, OUT_DIR)}/`);
  console.log(`  ${result.files.markdown.length} markdown mirrors`);
  console.log(`  ${rel(llmsFull)}`);
  console.log(`  ${rel(result.files.manifest)}`);
  console.log(
    `  ${rel(search.indexFile)} (${search.docs} docs, ${search.chunks} chunks)`
  );
  console.log(`  ${rel(webmcp.outfile)} (${Math.round(webmcp.bytes / 1024)} kB)`);
  console.log(`  ${rel(path.join(wellKnownDir, "ai-catalog.json"))}`);
  if (skills.indexFile) {
    console.log(`  ${rel(skills.indexFile)} (${skills.skills.length} skills)`);
  }
  if (result.files.sitemapXml) console.log(`  ${rel(result.files.sitemapXml)}`);
  if (result.files.robotsTxt) console.log(`  ${rel(result.files.robotsTxt)}`);

  if (seed.seeded) {
    console.log(
      `\n  seeded ${rel(CURATED_LLMS_TXT)} from the nav — it is curated from here on.\n` +
        `  Edit it and commit; the build copies it verbatim and never rewrites it.`
    );
  } else if (curated.applied) {
    console.log(`  llms.txt: curated (${rel(CURATED_LLMS_TXT)})`);
  } else {
    console.log(`  llms.txt: GENERATED — ${rel(CURATED_LLMS_TXT)} is missing.`);
  }

  if (undescribed.length > 0) {
    console.log(
      `  note: ${undescribed.length}/${pages.length} pages have no \`description:\` frontmatter; ` +
        `descriptions were derived from each page's first paragraph.`
    );
  }
}

main().catch((error) => {
  console.error(`agent-artifacts: ${error.message}`);
  process.exitCode = 1;
});
