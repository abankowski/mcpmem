import { existsSync } from "node:fs";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import path from "node:path";
import { defineConfig } from "@playwright/test";
import { TEMP_DIR, serverArgs } from "./helpers";

// The browser behavior suite (Tasks 16 / U1-U12). One server binary serves
// the whole run; every spec seeds its own workspaces through the API. The
// webServer command starts the binary on a fresh memory file: it removes the
// previous run's database first, writes the fixture vision key, and passes
// ui/e2e/server.toml ([ocr]) so PDF extraction works against the fixture
// vision endpoint.

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const binary = path.join(repoRoot, "target", "debug", "mcpmem");
if (!existsSync(binary)) {
  throw new Error(
    `The e2e binary is missing at ${binary}. Build it with: cargo build --features indexer,extractor,webhooks`,
  );
}

// The fixture vision endpoint must outlive the whole run. The runner process
// owns it and stops it on exit.
const servicesPath = path.join(path.dirname(fileURLToPath(import.meta.url)), "e2e-services.mjs");
const services = spawn(process.execPath, [servicesPath, "8092"], { stdio: "ignore" });
process.on("exit", () => services.kill("SIGTERM"));

const serverCommand = [
  `rm -rf ${TEMP_DIR} && mkdir -p ${TEMP_DIR} &&`,
  `printf 'e2e-vision-key\\n' > ${TEMP_DIR}/vision-key &&`,
  `printf 'e2e-oidc-secret\\n' > ${TEMP_DIR}/oidc-secret &&`,
  binary,
  ...serverArgs(),
].join(" ");

export default defineConfig({
  testDir: ".",
  timeout: 120_000,
  expect: { timeout: 15_000 },
  fullyParallel: false,
  workers: 1,
  retries: 0,
  reporter: [["list"]],
  use: {
    baseURL: "http://127.0.0.1:8080",
    headless: true,
    viewport: { width: 1440, height: 900 },
    actionTimeout: 15_000,
    navigationTimeout: 30_000,
    trace: "retain-on-failure",
  },
  webServer: {
    command: serverCommand,
    cwd: repoRoot,
    url: "http://127.0.0.1:8080/ui",
    reuseExistingServer: false,
    timeout: 60_000,
  },
});