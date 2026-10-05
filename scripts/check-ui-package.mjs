#!/usr/bin/env node
// Verify that the packaged `mcpmem` crate carries the built UI.
//
//   node scripts/check-ui-package.mjs
//
// The check downloads nothing; it works on the local package draft only:
//
// 1. Membership. `cargo package -p mcpmem --list` must name
//    `ui/dist/ui-manifest.json` and every file the manifest lists. Without
//    them the packaged crate cannot compile: `src/ui/assets.rs` embeds the
//    manifest and the asset bytes with `include_str!` / `include_bytes!`.
// 2. Bytes. The file list cargo prints is the draft package, so this script
//    packs exactly those files into a temporary archive and runs the
//    Task-1 manifest contract against the packaged bytes: the archive holds
//    the manifest, the same file set the working tree names, every asset
//    with the sha256, content type and byte count the manifest claims, and
//    no legacy asset names.
//
// A full `cargo package -p mcpmem --locked` verification is possible only
// when the workspace crates are already published at this version, which
// is why `publish-crates.sh --dry-run` verifies `mcpmem-core` alone before
// the first release. The list and the drafted archive need no registry.
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, readFileSync, rmSync, mkdirSync, writeFileSync } from "node:fs";
import path from "node:path";

const repoRoot = path.resolve(import.meta.dirname, "..");
const manifestPath = path.join(repoRoot, "ui", "dist", "ui-manifest.json");
const draftDir = path.join(repoRoot, "target", "package", "ui-check-draft");

let failures = 0;
const check = (name, ok, detail) => {
  console.log(`[${ok ? "PASS" : "FAIL"}] ${name}`);
  if (!ok && detail) {
    console.log(`  ${detail}`);
    failures += 1;
  }
};

const run = (cmd, args, opts = {}) => {
  const p = spawnSync(cmd, args, { encoding: null, ...opts });
  return { status: p.status, out: p.stdout ?? Buffer.from(""), err: p.stderr ?? Buffer.from("") };
};

// --- Membership: the cargo package dry run -------------------------------

const list = run("cargo", ["package", "-p", "mcpmem", "--list", "--locked", "--allow-dirty"]);
check(
  "cargo package -p mcpmem --list runs",
  list.status === 0,
  list.err.toString("utf8", 0, 300),
);
const listed = new Set(list.out.toString("utf8").split("\n").map((l) => l.trim()).filter(Boolean));

check(
  "the package lists ui/dist/ui-manifest.json",
  listed.has("ui/dist/ui-manifest.json"),
  "the packaged crate must embed the manifest for include_str!",
);

let manifest = {};
if (existsSync(manifestPath)) {
  manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
}
check("the working-tree manifest exists", existsSync(manifestPath), manifestPath);

const missing = [];
for (const [assetPath, meta] of Object.entries(manifest.files || {})) {
  const rel = path.join("ui", "dist", assetPath.replace(/^\/ui\//, ""));
  if (!listed.has(rel)) {
    missing.push(rel);
  }
}
check(
  "the package lists every manifest asset",
  missing.length === 0,
  `not listed: ${missing.join(", ")}`,
);

// --- Bytes: pack the listed files and check the archive ------------------

rmSync(draftDir, { recursive: true, force: true });
mkdirSync(draftDir, { recursive: true });
// The dry-run list names two entries cargo synthesizes and that do not
// exist on disk (.cargo_vcs_info.json, and the Cargo.toml.orig backup the
// set-version flow leaves once). The archive packs the files that exist;
// the byte checks below read only manifest entries from it.
const packable = [...listed].filter((rel) => existsSync(path.join(repoRoot, rel)));
const fileListPath = path.join(draftDir, "files.txt");
writeFileSync(fileListPath, packable.join("\n") + "\n", "utf8");
const draft = path.join(draftDir, "mcpmem-draft.crate");
const packed = run("tar", ["-czf", draft, "-T", fileListPath], { cwd: repoRoot });

check("the draft archive packs the package file list", packed.status === 0, packed.err.toString("utf8", 0, 300));

const member = (name) => run("tar", ["-xzOf", draft, name]);
const packedManifestRaw = member("ui/dist/ui-manifest.json");
check(
  "the packaged bytes hold ui/dist/ui-manifest.json",
  packedManifestRaw.status === 0,
  packedManifestRaw.err.toString("utf8", 0, 200),
);

const packedKeys = new Set(Object.keys(manifest.files || {}));
let packedManifest = {};
if (packedManifestRaw.status === 0) {
  packedManifest = JSON.parse(packedManifestRaw.out.toString("utf8"));
  const packed = packedManifest.files || {};
  const inTreeNotPacked = [...packedKeys].filter((k) => !(k in packed));
  const inPackedNotTree = Object.keys(packed).filter((k) => !packedKeys.has(k));
  check(
    "the packaged manifest names the same files as the working tree",
    inTreeNotPacked.length === 0 && inPackedNotTree.length === 0,
    `draft-only: ${inTreeNotPacked.join(", ")}; tree-only: ${inPackedNotTree.join(", ")}`,
  );
}

const hash = (buf) => createHash("sha256").update(buf).digest("hex");
for (const [assetPath, meta] of Object.entries(packedManifest.files || {})) {
  check(
    `path is under /ui/assets/: ${assetPath}`,
    assetPath.startsWith("/ui/assets/") && !assetPath.includes(".."),
    assetPath,
  );
  const rel = assetPath.replace(/^\/ui\//, "");
  const raw = member(`ui/dist/${rel}`);
  check(`the packaged bytes hold: ui/dist/${rel}`, raw.status === 0, "archive member missing");
  check(
    `sha256 matches: ${assetPath}`,
    raw.status === 0 && hash(raw.out) === meta.sha256,
    meta.sha256 || "no sha256 in the packaged manifest",
  );
  check(
    `content type set: ${assetPath}`,
    typeof meta.contentType === "string",
    String(meta.contentType),
  );
}
check(
  "legacy asset names are gone",
  !Object.keys(packedManifest.files || {}).some((p) =>
    /graph\.css|graph\.js|nav\.css|admin\.js|admin\.css/.test(p),
  ),
  "old names must not be regenerated",
);

rmSync(draftDir, { recursive: true, force: true });
console.log(failures ? `\n${failures} check(s) failed` : "\nall checks passed");
process.exit(failures ? 1 : 0);