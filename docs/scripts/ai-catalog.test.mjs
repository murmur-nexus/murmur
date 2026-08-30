/**
 * Validates the hand-curated ARD manifest (agenticresourcediscovery.org)
 * against its schema. This file is hand-edited, unlike everything else
 * agent-artifacts.mjs writes — nothing else catches a typo in it before it
 * ships.
 */

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

const catalog = JSON.parse(
  readFileSync(fileURLToPath(new URL("../ai-catalog.json", import.meta.url)), "utf8")
);

test("ai-catalog.json: top-level shape", () => {
  assert.equal(typeof catalog.specVersion, "string");
  assert.ok(catalog.specVersion.length > 0);
  assert.equal(typeof catalog.host.displayName, "string");
  assert.equal(typeof catalog.host.identifier, "string");
  assert.ok(Array.isArray(catalog.entries));
  assert.ok(catalog.entries.length > 0);
});

test("ai-catalog.json: every entry matches the ARD entry schema", () => {
  for (const entry of catalog.entries) {
    assert.match(entry.identifier, /^urn:air:[^:]+:[^:]+:[^:]+$/, `bad identifier: ${entry.identifier}`);
    assert.equal(typeof entry.displayName, "string");
    assert.equal(typeof entry.type, "string");

    // Exactly one of url/data, never both, never neither.
    const hasUrl = "url" in entry;
    const hasData = "data" in entry;
    assert.notEqual(hasUrl, hasData, `${entry.identifier}: needs exactly one of url/data`);
    if (hasUrl) assert.match(entry.url, /^https:\/\//, `${entry.identifier}: url must be absolute`);

    assert.ok(Array.isArray(entry.representativeQueries), `${entry.identifier}: representativeQueries missing`);
    assert.ok(
      entry.representativeQueries.length >= 2 && entry.representativeQueries.length <= 5,
      `${entry.identifier}: representativeQueries must have 2-5 items, has ${entry.representativeQueries.length}`
    );
  }
});

test("ai-catalog.json: no duplicate entry identifiers", () => {
  const ids = catalog.entries.map((entry) => entry.identifier);
  assert.equal(new Set(ids).size, ids.length);
});
