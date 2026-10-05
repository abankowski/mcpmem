// Regenerate the design-handoff renders from the fixed HTML screens.
// One full-page shot per screen at the width the originals used.
// Run from the repository root: node scripts/regenerate-renders.mjs
import path from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, "..");
// Resolve Playwright from the ui/ toolchain, never from an absolute machine path.
const require = createRequire(path.join(root, "ui", "package.json"));
const { chromium } = require("playwright");

const screens = path.join(root, "designs/mcpmem-ui-handoff/screens");
const landing = path.join(root, "designs/mcpmem-ui-handoff/landing");
const out = path.join(root, "designs/mcpmem-ui-handoff/renders");

const jobs = [
  { file: "00-tokens-dark.html", width: 1440, out: "00-tokens-dark.png" },
  { file: "01-graph-explorer.html", width: 1440, out: "01-graph-explorer.png" },
  { file: "02-inspector-states.html", width: 1240, out: "02-inspector-states.png" },
  { file: "03-search.html", width: 1440, out: "03-search.png" },
  { file: "04-admin.html", width: 1440, out: "04-admin.png" },
  { file: "05-oauth-consent.html", width: 1440, out: "05-oauth-consent.png" },
  { file: null, width: 1440, out: "06-landing.png" },
];

const browser = await chromium.launch();
for (const job of jobs) {
  const page = await browser.newPage({ viewport: { width: job.width, height: 900 } });
  const file = job.file ? path.join(screens, job.file) : path.join(landing, "index.html");
  await page.goto(`file://${file}`, { waitUntil: "networkidle" });
  await page.screenshot({ path: path.join(out, job.out), fullPage: true });
  await page.close();
  console.log("rendered", job.out);
}
await browser.close();