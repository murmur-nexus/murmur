/**
 * Flatten Material-for-MkDocs markdown into portable markdown.
 *
 * Leadtype ships flatteners for MDX/JSX components, so nothing in it knows what
 * `!!! note` or `=== "Tab"` mean — left alone, those land verbatim in the agent
 * mirrors. This module normalises them the way leadtype normalises their MDX
 * equivalents: admonitions become blockquotes, content tabs become bold
 * headings.
 *
 * Everything here is fence-aware. A `!!!` inside a code block is sample text,
 * not an admonition.
 */

const INDENT_UNIT = 4;

// `!!! note`, `!!! warning "Title"`, `??? tip`, `???+ example "Title"`.
// Types may carry extra classes: `!!! note inline end`.
const ADMONITION_RE = /^(?<indent>[ \t]*)(?<marker>!!!|\?\?\?\+?)[ \t]+(?<types>[\w-]+(?:[ \t]+[\w-]+)*)(?:[ \t]+"(?<title>[^"]*)")?[ \t]*$/;

// `=== "Label"` / `===+ "Label"`. The required quote disambiguates a content tab
// from a setext H1 underline (`Title` over `===`).
const TAB_RE = /^(?<indent>[ \t]*)===\+?[ \t]+"(?<label>[^"]*)"[ \t]*$/;

const FENCE_RE = /^(?<indent>[ \t]*)(?<fence>```+|~~~+)(?<info>.*)$/;

// Trailing attr_list / md_in_html blocks: `{: .class }`, `{ #anchor }`,
// `{ data-foo="bar" }`. Only stripped when the braces close on the same line.
const ATTR_LIST_RE = /[ \t]*\{:?[ \t]*[#.][^}\n]*\}[ \t]*$/;

const indentWidth = (line) => {
  let width = 0;
  for (const char of line) {
    if (char === " ") width += 1;
    else if (char === "\t") width += INDENT_UNIT;
    else break;
  }
  return width;
};

const isBlank = (line) => line.trim() === "";

const titleCase = (slug) =>
  slug.replace(/[-_]/g, " ").replace(/\b\w/g, (c) => c.toUpperCase());

/**
 * Collect the indented body that belongs to a Material block opener at
 * `openerIndent`, consuming trailing blank lines only when more body follows.
 */
function takeBlock(lines, start, openerIndent) {
  const bodyIndent = openerIndent + INDENT_UNIT;
  const body = [];
  let i = start;
  let pendingBlanks = 0;

  while (i < lines.length) {
    const line = lines[i];
    if (isBlank(line)) {
      pendingBlanks += 1;
      i += 1;
      continue;
    }
    if (indentWidth(line) < bodyIndent) break;

    for (let b = 0; b < pendingBlanks; b += 1) body.push("");
    pendingBlanks = 0;
    body.push(line.slice(bodyIndent));
    i += 1;
  }

  return { body, next: i - pendingBlanks };
}

/**
 * Rewrite a block of lines, recursing into nested Material blocks.
 * Returns a new array of lines.
 */
function flattenLines(lines) {
  const out = [];
  let i = 0;
  let fence = null; // { marker, indent } while inside a code fence

  const pushBlank = () => {
    if (out.length > 0 && !isBlank(out[out.length - 1])) out.push("");
  };

  while (i < lines.length) {
    const line = lines[i];

    if (fence) {
      out.push(line);
      const close = FENCE_RE.exec(line);
      if (close && close.groups.fence[0] === fence.marker[0] && close.groups.fence.length >= fence.marker.length && close.groups.info.trim() === "") {
        fence = null;
      }
      i += 1;
      continue;
    }

    const openFence = FENCE_RE.exec(line);
    if (openFence) {
      fence = { marker: openFence.groups.fence, indent: openFence.groups.indent };
      out.push(line);
      i += 1;
      continue;
    }

    const admonition = ADMONITION_RE.exec(line);
    if (admonition) {
      const { indent, types, title } = admonition.groups;
      const { body, next } = takeBlock(lines, i + 1, indentWidth(indent));
      const heading = title !== undefined ? title : titleCase(types.split(/[ \t]+/)[0]);

      pushBlank();
      const inner = flattenLines(body);
      // The blank line after the opener is part of Material's syntax, not
      // content; keeping it would emit a doubled `>` under the title.
      while (inner.length && isBlank(inner[0])) inner.shift();

      const quoted = [];
      if (heading.trim() !== "") quoted.push(`**${heading.trim()}**`, "");
      quoted.push(...inner);
      while (quoted.length && isBlank(quoted[quoted.length - 1])) quoted.pop();

      for (const bodyLine of quoted) out.push(isBlank(bodyLine) ? ">" : `> ${bodyLine}`);
      out.push("");
      i = next;
      continue;
    }

    const tab = TAB_RE.exec(line);
    if (tab) {
      const { indent, label } = tab.groups;
      const { body, next } = takeBlock(lines, i + 1, indentWidth(indent));

      pushBlank();
      out.push(`**${label.trim()}**`, "");
      out.push(...flattenLines(body));
      pushBlank();
      i = next;
      continue;
    }

    out.push(line.replace(ATTR_LIST_RE, ""));
    i += 1;
  }

  return out;
}

/**
 * Derive a one-line description from the page body.
 *
 * `description` is the routing hint in llms.txt — it is how an agent decides
 * which page to fetch. These pages carry no `description:` frontmatter, and
 * leadtype's own fallback synthesises "Reference page for hooks.", which tells
 * an agent nothing. The first real paragraph is a far better signal.
 */
export function deriveDescription(markdown, { maxLength = 220 } = {}) {
  const lines = markdown.split("\n");
  const paragraph = [];
  let fence = null;
  let seenBody = false;

  for (const line of lines) {
    const fenceMatch = /^[ \t]*(```+|~~~+)/.exec(line);
    if (fenceMatch) {
      if (fence && fenceMatch[1][0] === fence[0]) fence = null;
      else if (!fence) fence = fenceMatch[1];
      if (paragraph.length) break;
      continue;
    }
    if (fence) continue;

    const trimmed = line.trim();

    if (trimmed === "") {
      if (paragraph.length) break;
      continue;
    }
    // Skip structural lines: headings, tables, lists, quotes, rules, images.
    if (/^(#|\||>|-{3,}|={3,}|\*{3,}|[-*+] |\d+\. |!\[)/.test(trimmed)) {
      if (paragraph.length) break;
      seenBody = true;
      continue;
    }

    paragraph.push(trimmed);
  }

  if (paragraph.length === 0) return undefined;

  // Strip inline markdown so the hint reads as prose.
  let text = paragraph
    .join(" ")
    .replace(/!\[[^\]]*\]\([^)]*\)/g, "")
    .replace(/\[([^\]]*)\]\([^)]*\)/g, "$1")
    .replace(/`([^`]+)`/g, "$1")
    .replace(/\*\*([^*]+)\*\*/g, "$1")
    .replace(/(?<!\w)[*_]([^*_]+)[*_](?!\w)/g, "$1")
    .replace(/\s+/g, " ")
    .trim();

  if (text.length <= maxLength) return text || undefined;

  // Prefer cutting at a sentence end, then a word boundary.
  const window = text.slice(0, maxLength);
  const sentenceEnd = Math.max(window.lastIndexOf(". "), window.lastIndexOf("? "));
  if (sentenceEnd > maxLength * 0.5) return window.slice(0, sentenceEnd + 1).trim();

  const wordEnd = window.lastIndexOf(" ");
  return `${window.slice(0, wordEnd > 0 ? wordEnd : maxLength).trim()}…`;
}

export function flattenMaterialMarkdown(markdown) {
  return flattenLines(markdown.split("\n")).join("\n").replace(/\n{3,}/g, "\n\n").trimEnd();
}

/**
 * Rewrite intra-docs `.md` links onto the canonical mirror paths.
 *
 * A relative `context.md` is meaningless to an agent that fetched the mirror
 * over HTTP. Pointing at `/concepts/context.md` lets it follow the link
 * straight to the next mirror instead of guessing at the HTML route.
 */
export function rewriteLinks(markdown, { srcUri, urlPathBySrcUri, redirects }) {
  const dir = srcUri.includes("/") ? srcUri.slice(0, srcUri.lastIndexOf("/")) : "";

  const resolve = (href) => {
    if (/^[a-z][a-z0-9+.-]*:/i.test(href) || href.startsWith("//") || href.startsWith("#")) {
      return null;
    }

    const hashAt = href.indexOf("#");
    const hash = hashAt === -1 ? "" : href.slice(hashAt);
    let path = hashAt === -1 ? href : href.slice(0, hashAt);
    if (!path.endsWith(".md")) return null;

    const segments = (path.startsWith("/") ? path.slice(1) : `${dir}/${path}`).split("/");
    const stack = [];
    for (const segment of segments) {
      if (segment === "." || segment === "") continue;
      if (segment === "..") stack.pop();
      else stack.push(segment);
    }

    let target = stack.join("/");
    target = redirects[target] ?? target;

    const urlPath = urlPathBySrcUri.get(target);
    if (!urlPath) return null;

    return `${urlPath === "/" ? "/index" : urlPath}.md${hash}`;
  };

  // Inline links `[text](href)` and reference definitions `[key]: href`.
  // Match on the `](` seam rather than the link text — link text legitimately
  // contains brackets, e.g. `[artifacts[].runtime](../reference/manifest.md)`.
  return markdown
    .replace(/\]\(([^)\s]+)(\)|\s)/g, (match, href, tail) => {
      const next = resolve(href);
      return next ? `](${next}${tail}` : match;
    })
    .replace(/^(\[[^\]]+\]:[ \t]+)(\S+)$/gm, (match, open, href) => {
      const next = resolve(href);
      return next ? `${open}${next}` : match;
    });
}
