// Write ui-manifest.json after the Vite build.
import { readFileSync, writeFileSync, globSync } from "node:fs";
import { createHash } from "node:crypto";
import path from "node:path";

// Resolve from this file so the script works from any working directory.
const distDir = path.join(import.meta.dirname, "..", "dist");

const files = globSync(path.join(distDir, "assets", "*")).map((p) => {
  const buf = readFileSync(p);
  const name = path.basename(p);
  const contentType = name.endsWith(".js")
    ? "text/javascript; charset=utf-8"
    : name.endsWith(".css")
      ? "text/css; charset=utf-8"
      : "application/octet-stream";
  return [
    `/ui/assets/${name}`,
    {
      contentType,
      bytes: buf.length,
      sha256: createHash("sha256").update(buf).digest("hex"),
    },
  ];
});

writeFileSync(
  path.join(distDir, "ui-manifest.json"),
  JSON.stringify(
    {
      files: Object.fromEntries(files),
      pages: { graph: "/ui", search: "/ui/search", admin: "/ui/admin" },
    },
    null,
    2,
  ),
);
console.log(`wrote ui-manifest.json with ${files.length} file(s)`);