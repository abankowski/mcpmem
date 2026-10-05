// The path-prefix contract: an app served under /mem (through a
// prefix-stripping proxy) must boot at every page, must not request a root
// asset or root OAuth URL, and must never bypass the proxy to the server.
// The same pages must also still load at the unprefixed root.

import { spawn, type ChildProcess } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { expect, test, type Page } from "@playwright/test";
import { ensureServer, openWorkspace, seedWorkspace, SERVER_ORIGIN, TEST_BEARER, type WorkspaceShape } from "./helpers";

const PROXY_ORIGIN = "http://127.0.0.1:8082";

test.describe.configure({ mode: "serial" });

let ws: WorkspaceShape;
let proxy: ChildProcess | null = null;
let teardownServer: (() => void) | undefined;

test.afterAll(async () => {
  teardownServer?.();
});

test.beforeAll(async () => {
  teardownServer = await ensureServer();
  ws = await seedWorkspace(`e2e-prefix-${Date.now()}`, {
    entities: [{ name: "prefixed-node", entityType: "note" }],
  });

  const proxyPath = path.join(path.dirname(fileURLToPath(import.meta.url)), "prefix-proxy.mjs");
  proxy = spawn(process.execPath, [proxyPath, "8082", "8080"], { stdio: "ignore" });
  const deadline = Date.now() + 30_000;
  let ready = false;
  while (Date.now() < deadline && !ready) {
    try {
      const response = await fetch(`${PROXY_ORIGIN}/mem/ui`);
      ready = response.ok;
    } catch {
      // Not up yet.
    }
    if (!ready) {
      const { promise, resolve } = Promise.withResolvers<void>();
      setTimeout(resolve, 200);
      await promise;
    }
  }
  if (!ready) {
    throw new Error("The /mem prefix proxy did not become ready on port 8082");
  }
});

test.afterAll(() => {
  proxy?.kill("SIGTERM");
  proxy = null;
});

interface RequestLog {
  /** Requests that left the prefix or hit the server directly. */
  violations: string[];
}

function trackRequests(page: Page): RequestLog {
  const violations: string[] = [];
  page.on("request", (request) => {
    const url = new URL(request.url());
    if (url.origin === PROXY_ORIGIN) {
      if (!(url.pathname === "/mem" || url.pathname.startsWith("/mem/"))) {
        violations.push(request.url());
      }
    } else if (url.origin === SERVER_ORIGIN) {
      violations.push(request.url());
    }
  });
  return { violations };
}

async function expectPrefixedOnly(log: RequestLog): Promise<void> {
  // Give late assets and API calls a moment to land.
  const { promise, resolve } = Promise.withResolvers<void>();
  setTimeout(resolve, 1000);
  await promise;
  expect(log.violations, `a prefixed page requested a root URL: ${log.violations.join(", ")}`).toEqual([]);
}

test("root pages load at /ui and the nested admin callback", async ({ page }) => {
  await openWorkspace(page, ws.workspaceId, "/ui");
  await expect(page.locator(".ui-topbar")).toBeVisible();
  await expect(page.locator("canvas.g-canvas")).toBeVisible();

  await openWorkspace(page, ws.workspaceId, "/ui/admin/callback");
  await expect(page.locator(".ui-topbar")).toBeVisible();
});

test("the prefixed graph page boots and requests only prefixed URLs", async ({ page }) => {
  const log = trackRequests(page);
  await openWorkspace(page, ws.workspaceId, `${PROXY_ORIGIN}/mem/ui`);

  await expect(page.locator(".ui-topbar")).toBeVisible();
  await expect(page.locator("canvas.g-canvas")).toBeVisible();
  // The proxied API answers: the workspace list reaches the switcher.
  await expect(page.locator(".ui-workspace-switcher select")).toContainText(ws.name);
  await expectPrefixedOnly(log);
});

test("prefixed search, admin, and the nested admin callback load without root requests", async ({ page }) => {
  const log = trackRequests(page);

  await openWorkspace(page, ws.workspaceId, `${PROXY_ORIGIN}/mem/ui/search`);
  await expect(page.locator(".s-controls")).toBeVisible();

  await openWorkspace(page, ws.workspaceId, `${PROXY_ORIGIN}/mem/ui/admin`, { adminToken: TEST_BEARER });
  await expect(page.locator(".ui-admin__nav")).toBeVisible();

  await openWorkspace(page, ws.workspaceId, `${PROXY_ORIGIN}/mem/ui/admin/callback`, { adminToken: TEST_BEARER });
  await expect(page.locator(".ui-admin__nav")).toBeVisible();

  await expectPrefixedOnly(log);
});