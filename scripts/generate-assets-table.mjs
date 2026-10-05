#!/usr/bin/env node
// Regenerate the embedded asset table in `src/ui/assets.rs` from the build
// manifest. Run it after every frontend rebuild:
//
//   node scripts/generate-assets-table.mjs
//
// The UI module embeds the bundle at compile time: `src/ui/assets.rs` holds
// a constant table of `include_bytes!` rows, one per manifest entry, and
// the manifest itself arrives through `include_str!`. The table is checked
// in, so it must change with the manifest. This script keeps the two in
// step without a hand edit; the frontend CI job runs it and then asserts
// `git diff --exit-code -- src/ui/assets.rs` is clean.
//
// The rest of the file is preserved verbatim: the header, the import lines,
// the doc comments and the route handler are not part of the table. Rows
// are sorted by asset name so the output is byte-identical across machines
// and the diff check never sees a reordering.
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import path from "node:path";

const repoRoot = path.resolve(import.meta.dirname, "..");
const manifestPath = path.join(repoRoot, "ui", "dist", "ui-manifest.json");
const tablePath = path.join(repoRoot, "src", "ui", "assets.rs");

if (!existsSync(manifestPath)) {
  console.error(`[FAIL] no build manifest at ${manifestPath}; run the frontend build first`);
  process.exit(1);
}

const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
const assetPaths = Object.keys(manifest.files || {});
for (const assetPath of assetPaths) {
  if (!assetPath.startsWith("/ui/assets/") || assetPath.includes("..")) {
    console.error(`[FAIL] manifest entry escapes /ui/assets/: ${assetPath}`);
    process.exit(1);
  }
}
assetPaths.sort();

const open = "const UI_ASSET_BYTES: &[(&str, &[u8])] = &[\n";
const close = "];\n";

const src = readFileSync(tablePath, "utf8");
const openAt = src.indexOf(open);
if (openAt === -1) {
  console.error(`[FAIL] cannot find the table opener in ${tablePath}`);
  process.exit(1);
}
const closeAt = src.indexOf(close, openAt + open.length);
if (closeAt === -1) {
  console.error(`[FAIL] cannot find the table closer in ${tablePath}`);
  process.exit(1);
}

const header = src.slice(0, openAt + open.length);
const footer = src.slice(closeAt + close.length);

const rows = assetPaths.map((assetPath) => {
  const name = assetPath.slice("/ui/assets/".length);
  if (name.includes('"') || name.includes("\\")) {
    console.error(`[FAIL] asset name cannot appear in a Rust string: ${name}`);
    process.exit(1);
  }
  return `    (\n        "${name}",\n        include_bytes!("../../ui/dist/assets/${name}"),\n    ),`;
});

const out = header + rows.join("\n") + "\n" + close + footer;
writeFileSync(tablePath, out);
console.log(`wrote ${rows.length} asset row(s) to src/ui/assets.rs`);