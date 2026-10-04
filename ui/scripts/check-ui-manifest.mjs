// Check the UI build manifest. Exit with code 1 when any check fails.
import { readFileSync, existsSync } from "node:fs";
import { createHash } from "node:crypto";
import path from "node:path";

// Resolve from this file so the check works from any working directory.
const distDir = path.join(import.meta.dirname, "..", "dist");
const manifestPath = path.join(distDir, "ui-manifest.json");

let mf = "";
let manifest = {};
if (existsSync(manifestPath)) {
  mf = readFileSync(manifestPath, "utf8");
  manifest = JSON.parse(mf);
}

let failures = 0;
const check = (name, ok, detail) => {
  console.log(`[${ok ? "PASS" : "FAIL"}] ${name}`);
  if (!ok) {
    console.log("  " + detail);
    failures += 1;
  }
};

check(
  "manifest exists",
  manifest.files && typeof manifest.files === "object",
  mf.slice(0, 200),
);
for (const [assetPath, meta] of Object.entries(manifest.files || {})) {
  check(
    `path is under /ui/assets/: ${assetPath}`,
    assetPath.startsWith("/ui/assets/") && !assetPath.includes(".."),
    assetPath,
  );
  const disk = path.join(distDir, assetPath.replace(/^\/ui\//, ""));
  check(`file exists: ${disk}`, existsSync(disk), "missing built file");
  const buf = existsSync(disk) ? readFileSync(disk) : null;
  check(
    `sha256 matches: ${assetPath}`,
    buf !== null && hash(buf) === meta.sha256,
    meta.sha256,
  );
  check(
    `content type set: ${assetPath}`,
    typeof meta.contentType === "string",
    String(meta.contentType),
  );
}
check(
  "legacy asset names are gone",
  !Object.keys(manifest.files || {}).some((p) =>
    /graph\.css|graph\.js|nav\.css|admin\.js|admin\.css/.test(p),
  ),
  "old names must not be regenerated",
);

function hash(buf) {
  return createHash("sha256").update(buf).digest("hex");
}

process.exit(failures ? 1 : 0);