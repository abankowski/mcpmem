// Scope gating and session recovery in the admin UI. Regression net for the
// graph-write consent affordance: a session that already holds graph-write
// must not see the "needs the graph-write scope" warning, and no
// "Grant graph-write" affordance may appear when the OAuth consent flow
// cannot run (a static bearer cannot acquire consent, so the button would be
// a dead end). An invalid graph token must land on the authentication-recovery
// state instead of a dead Retry-only screen.

import { expect, test } from "@playwright/test";
import { ensureServer, openWorkspace, seedWorkspace, TEST_BEARER, type WorkspaceShape } from "./helpers";

test.describe.configure({ mode: "serial" });

let ws: WorkspaceShape;
let teardownServer: (() => void) | undefined;

test.beforeAll(async () => {
  teardownServer = await ensureServer();
  ws = await seedWorkspace(`e2e-grant-${Date.now()}`, {
    entities: [{ name: "GrantTarget", entityType: "note" }],
  });
});

test.afterAll(async () => {
  teardownServer?.();
});

test("a graph-write session shows no scope warning and no consent affordance", async ({ page }) => {
  await openWorkspace(page, ws.workspaceId, "/ui/admin", { adminToken: TEST_BEARER });

  await expect(page.getByRole("heading", { name: "Workspaces" })).toBeVisible();
  // The e2e static bearer holds graph-write, so the gated actions are
  // available and the "needs the graph-write" warning must not appear, any
  // more than the consent affordance that only exists for missing scopes.
  await expect(page.locator(".ui-admin-warn")).toHaveCount(0);
  await expect(page.getByRole("button", { name: "Grant graph-write" })).toHaveCount(0);
});

test("the members pane shows no scope warning and no consent affordance", async ({ page }) => {
  await openWorkspace(page, ws.workspaceId, "/ui/admin", { adminToken: TEST_BEARER });
  await page.locator(".ui-admin__nav").getByRole("button", { name: "Members and grants" }).click();

  await expect(page.getByRole("heading", { name: "Members and grants" })).toBeVisible();
  await expect(page.locator(".ui-admin-warn")).toHaveCount(0);
  await expect(page.getByRole("button", { name: "Grant graph-write" })).toHaveCount(0);
});

test("an invalid graph token lands on the authentication-recovery state, not a dead retry", async ({ page }) => {
  await page.goto("/ui#token=invalid-e2e-bearer");

  await expect(page.getByRole("heading", { name: "Authentication required" })).toBeVisible();
  // The static-bearer recovery affordance is the token form; the OAuth
  // sign-in button appears only when the resource advertises the challenge.
  await expect(page.locator("input#static-token")).toBeVisible();
});