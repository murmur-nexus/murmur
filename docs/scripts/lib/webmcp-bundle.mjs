/**
 * Bundle the WebMCP registration for the browser.
 *
 * `leadtype/webmcp` ships as ESM with bare imports, which a browser cannot
 * resolve, so it needs bundling before every page can load it as a module.
 */

import path from "node:path";
import { fileURLToPath } from "node:url";

import * as esbuild from "esbuild";

const ENTRY = fileURLToPath(new URL("../webmcp-entry.mjs", import.meta.url));

export async function bundleWebMcp({ outDir }) {
  const outfile = path.join(outDir, "assets", "agent", "webmcp.js");

  const result = await esbuild.build({
    entryPoints: [ENTRY],
    outfile,
    bundle: true,
    format: "esm",
    target: ["es2022"],
    minify: true,
    // The bundle is loaded from every page; a sourcemap keeps it debuggable in
    // production without costing anything until devtools opens.
    sourcemap: true,
    logLevel: "warning",
    metafile: true,
  });

  const bytes = Object.values(result.metafile.outputs).reduce(
    (total, output) => total + (output.bytes ?? 0),
    0
  );

  return { outfile, bytes };
}
