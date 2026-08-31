/**
 * Tests for the Agent Skills discovery index (RFC v0.2.0).
 *
 * Two things are worth guarding here. The digest must be of the bytes
 * actually served — a stale or source-derived digest makes the index's
 * central integrity claim a lie. And a malformed skill must fail the build
 * loudly, because a skill silently absent from the index looks exactly like
 * one that was never written.
 */

import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdtemp, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { test } from "node:test";

import { writeSkillsIndex } from "./skills.mjs";

const BASE = "https://docs.murmur.nexus";

async function scratch() {
  return mkdtemp(path.join(os.tmpdir(), "murmur-skills-test-"));
}

async function writeSkill(skillsDir, dirName, frontmatter, body = "\n# Heading\n\nBody text.\n") {
  await mkdir(path.join(skillsDir, dirName), { recursive: true });
  await writeFile(path.join(skillsDir, dirName, "SKILL.md"), `---\n${frontmatter}\n---\n${body}`);
}

test("writes a valid RFC v0.2.0 index and copies each skill", async () => {
  const root = await scratch();
  try {
    const skillsDir = path.join(root, "skills");
    const outDir = path.join(root, "site");
    await writeSkill(skillsDir, "alpha-skill", "name: alpha-skill\ndescription: Does alpha things.");
    await writeSkill(skillsDir, "beta-skill", "name: beta-skill\ndescription: Does beta things.");

    const result = await writeSkillsIndex({ outDir, skillsDir, baseUrl: BASE });
    const index = JSON.parse(await readFile(result.indexFile, "utf8"));

    assert.equal(index.$schema, "https://schemas.agentskills.io/discovery/0.2.0/schema.json");
    assert.equal(index.skills.length, 2);

    const alpha = index.skills.find((s) => s.name === "alpha-skill");
    assert.equal(alpha.type, "skill-md");
    assert.equal(alpha.description, "Does alpha things.");
    assert.equal(alpha.url, `${BASE}/skills/alpha-skill.md`);
    assert.match(alpha.digest, /^sha256:[0-9a-f]{64}$/);

    // The file the url points at must actually exist in the build.
    const served = await readFile(path.join(outDir, "skills", "alpha-skill.md"), "utf8");
    assert.match(served, /name: alpha-skill/);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("digest matches the bytes actually served, not the source", async () => {
  const root = await scratch();
  try {
    const skillsDir = path.join(root, "skills");
    const outDir = path.join(root, "site");
    await writeSkill(skillsDir, "gamma-skill", "name: gamma-skill\ndescription: Does gamma things.");

    const result = await writeSkillsIndex({ outDir, skillsDir, baseUrl: BASE });
    const index = JSON.parse(await readFile(result.indexFile, "utf8"));

    const servedBytes = await readFile(path.join(outDir, "skills", "gamma-skill.md"));
    const expected = `sha256:${createHash("sha256").update(servedBytes).digest("hex")}`;
    assert.equal(index.skills[0].digest, expected);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("a trailing slash on baseUrl doesn't produce a double slash in urls", async () => {
  const root = await scratch();
  try {
    const skillsDir = path.join(root, "skills");
    const outDir = path.join(root, "site");
    await writeSkill(skillsDir, "delta-skill", "name: delta-skill\ndescription: Does delta things.");

    const result = await writeSkillsIndex({ outDir, skillsDir, baseUrl: `${BASE}/` });
    const index = JSON.parse(await readFile(result.indexFile, "utf8"));
    assert.equal(index.skills[0].url, `${BASE}/skills/delta-skill.md`);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("descriptions containing colons and backticks survive parsing", async () => {
  const root = await scratch();
  try {
    const skillsDir = path.join(root, "skills");
    const outDir = path.join(root, "site");
    const desc = "Author a manifest for `mur run`: identity, artifacts, capabilities — the lot.";
    await writeSkill(skillsDir, "epsilon-skill", `name: epsilon-skill\ndescription: ${desc}`);

    const result = await writeSkillsIndex({ outDir, skillsDir, baseUrl: BASE });
    const index = JSON.parse(await readFile(result.indexFile, "utf8"));
    assert.equal(index.skills[0].description, desc);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("a malformed skill fails the build loudly rather than being skipped", async () => {
  const cases = [
    ["no frontmatter at all", "zeta-skill", null, "# Just a heading\n"],
    ["missing name", "zeta-skill", "description: Only a description."],
    ["missing description", "zeta-skill", "name: zeta-skill"],
    ["name disagrees with directory", "zeta-skill", "name: something-else\ndescription: Mismatched."],
    ["name has illegal characters", "zeta-skill", "name: Zeta_Skill\ndescription: Bad name."],
  ];

  for (const [label, dirName, frontmatter, rawBody] of cases) {
    const root = await scratch();
    try {
      const skillsDir = path.join(root, "skills");
      const outDir = path.join(root, "site");
      if (frontmatter === null) {
        await mkdir(path.join(skillsDir, dirName), { recursive: true });
        await writeFile(path.join(skillsDir, dirName, "SKILL.md"), rawBody);
      } else {
        await writeSkill(skillsDir, dirName, frontmatter);
      }
      await assert.rejects(
        () => writeSkillsIndex({ outDir, skillsDir, baseUrl: BASE }),
        /SKILL\.md/,
        `expected a thrown error for: ${label}`
      );
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  }
});

test("a missing skills directory is not an error, just an empty result", async () => {
  const root = await scratch();
  try {
    const result = await writeSkillsIndex({
      outDir: path.join(root, "site"),
      skillsDir: path.join(root, "does-not-exist"),
      baseUrl: BASE,
    });
    assert.deepEqual(result.skills, []);
    assert.equal(result.indexFile, null);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});
