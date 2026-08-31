/**
 * Agent Skills discovery index — /.well-known/agent-skills/index.json
 * per the Agent Skills Discovery RFC v0.2.0 (Cloudflare).
 *
 * Each skill in `docs/skills/<name>/SKILL.md` is copied to the site root as
 * `/skills/<name>.md` and listed in the index with a sha256 digest of the
 * exact bytes served. Publishing the digest is the point: a client can verify
 * it fetched the file we indexed, so the digest MUST be computed from the
 * same bytes that get written to the site — not from the source file, which
 * would silently diverge if the copy ever transformed anything.
 *
 * Every skill is `type: "skill-md"`. Murmur's other packaging format
 * (`.mur.zip`) maps onto the RFC's `"archive"` type, but a .mur.zip is
 * installed by the murmur runtime rather than fetched and read by an
 * arbitrary agent, so indexing one here would advertise something the
 * average client can't consume. Plain markdown is the portable form.
 */

import { createHash } from "node:crypto";
import { mkdir, readFile, readdir, writeFile } from "node:fs/promises";
import path from "node:path";

const SCHEMA_URL = "https://schemas.agentskills.io/discovery/0.2.0/schema.json";
// The RFC constrains skill names to lowercase letters, numbers and hyphens.
const NAME_PATTERN = /^[a-z0-9-]+$/;

/**
 * Pull `name` and `description` out of a SKILL.md's YAML frontmatter.
 *
 * Deliberately not a YAML parser: the frontmatter here is two scalar keys,
 * and taking everything after the first colon handles the colons, em-dashes
 * and backticks these descriptions actually contain. A real dependency would
 * buy nothing for two fields.
 */
function parseFrontmatter(source, label) {
  if (!source.startsWith("---\n")) {
    throw new Error(`${label}: missing YAML frontmatter — a SKILL.md must open with a --- block.`);
  }

  const end = source.indexOf("\n---", 3);
  if (end === -1) {
    throw new Error(`${label}: frontmatter block is never closed.`);
  }

  const fields = {};
  for (const line of source.slice(4, end).split("\n")) {
    const match = /^([A-Za-z_-]+):\s*(.*)$/.exec(line);
    if (!match) continue;
    fields[match[1]] = match[2].trim().replace(/^["'](.*)["']$/, "$1");
  }
  return fields;
}

/**
 * Copy every skill into `<outDir>/skills/` and write the discovery index.
 *
 * Throws rather than skipping on a malformed skill: a skill silently missing
 * from the index is indistinguishable from one that was never written, and
 * the index is a public claim about what exists.
 */
export async function writeSkillsIndex({ outDir, skillsDir, baseUrl }) {
  let dirents;
  try {
    dirents = await readdir(skillsDir, { withFileTypes: true });
  } catch (error) {
    if (error.code === "ENOENT") return { skills: [], indexFile: null };
    throw error;
  }

  const base = baseUrl.replace(/\/$/, "");
  const skillDirs = dirents.filter((entry) => entry.isDirectory()).map((entry) => entry.name).sort();
  const skillsOutDir = path.join(outDir, "skills");
  await mkdir(skillsOutDir, { recursive: true });

  const skills = [];
  for (const dirName of skillDirs) {
    const label = `docs/skills/${dirName}/SKILL.md`;
    const source = await readFile(path.join(skillsDir, dirName, "SKILL.md"), "utf8");
    const { name, description } = parseFrontmatter(source, label);

    if (!name) throw new Error(`${label}: frontmatter has no \`name\`.`);
    if (!description) throw new Error(`${label}: frontmatter has no \`description\`.`);
    if (!NAME_PATTERN.test(name)) {
      throw new Error(`${label}: name "${name}" must be lowercase letters, numbers and hyphens only.`);
    }
    // The directory is what a reader sees first; a name that disagrees with it
    // makes the index and the tree tell two different stories.
    if (name !== dirName) {
      throw new Error(`${label}: frontmatter name "${name}" does not match its directory "${dirName}".`);
    }

    const outFile = path.join(skillsOutDir, `${name}.md`);
    await writeFile(outFile, source);

    skills.push({
      name,
      type: "skill-md",
      description,
      url: `${base}/skills/${name}.md`,
      digest: `sha256:${createHash("sha256").update(source).digest("hex")}`,
    });
  }

  const indexDir = path.join(outDir, ".well-known", "agent-skills");
  await mkdir(indexDir, { recursive: true });
  const indexFile = path.join(indexDir, "index.json");
  await writeFile(indexFile, `${JSON.stringify({ $schema: SCHEMA_URL, skills }, null, 2)}\n`);

  return { skills, indexFile };
}
