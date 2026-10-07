// Admin screen behaviors (U5, U8): the workspace group for an owner without
// a human-admin token, the boundary where the server group disappears, the
// named unavailable states, and the nested admin callback route.

import { expect, test, type Page } from "@playwright/test";
import { ensureServer, openWorkspace, seedWorkspace, SERVER_ORIGIN, TEST_BEARER, type WorkspaceShape } from "./helpers";

test.describe.configure({ mode: "serial" });

let ws: WorkspaceShape;
let teardownServer: (() => void) | undefined;

test.beforeAll(async () => {
  teardownServer = await ensureServer();
  ws = await seedWorkspace(`e2e-admin-${Date.now()}`, {
    entities: [{ name: "AdminTarget", entityType: "note" }],
  });
});

test.afterAll(async () => {
  teardownServer?.();
});

function nav(page: Page) {
  return page.locator(".ui-admin__nav");
}

test("an owner without admin sees the workspace group and no server group", async ({ page }) => {
  await openWorkspace(page, ws.workspaceId, "/ui/admin", { adminToken: TEST_BEARER });

  await expect(page.getByRole("heading", { name: "Workspaces" })).toBeVisible();
  await expect(nav(page).getByRole("button", { name: "Workspaces" })).toBeVisible();
  await expect(nav(page).getByRole("button", { name: "Members and grants" })).toBeVisible();
  await expect(nav(page).getByRole("button", { name: "Vector index" })).toBeVisible();

  // The server group requires a human-admin token; a static bearer sees no
  // Principals or approvals entry at all.
  await expect(nav(page).getByRole("button", { name: "Principals" })).toHaveCount(0);
  await expect(nav(page).getByRole("button", { name: "Pending approvals" })).toHaveCount(0);
  await expect(nav(page).getByRole("button", { name: "Webhooks" })).toHaveCount(0);

  const row = page.locator("table.ui-admin-table tbody tr", { hasText: ws.name });
  await expect(row).toBeVisible();
  await expect(row).toContainText("owner");
  await expect(row.getByRole("button", { name: "Make public" })).toBeVisible();
  await expect(page.getByRole("button", { name: "Create workspace" })).toBeVisible();
});

test("members and grants stay owner-only and the identity picker names the human-admin gate", async ({ page }) => {
  await openWorkspace(page, ws.workspaceId, "/ui/admin", { adminToken: TEST_BEARER });
  await nav(page).getByRole("button", { name: "Members and grants" }).click();

  await expect(page.getByRole("heading", { name: "Members and grants" })).toBeVisible();
  // The identity picker lists principals, which the server gates behind a
  // human-admin credential: the pane names the gate and renders no picker.
  await expect(page.locator(".ui-admin-error")).toContainText("The identity picker is unavailable");
  await expect(page.locator("select#grant-principal")).toHaveCount(0);
});

test("the vector pane shows the measured unavailable state and no refresh control", async ({ page }) => {
  await openWorkspace(page, ws.workspaceId, "/ui/admin", { adminToken: TEST_BEARER });
  await nav(page).getByRole("button", { name: "Vector index" }).click();

  // No serving profile is configured for the suite, so the adapter answers
  // 503 and the pane renders the named unavailable state.
  await expect(page.getByRole("heading", { name: "Vector index" })).toBeVisible();
  await expect(page.locator(".ui-admin-unavailable")).toContainText("Vector index unavailable");
  await expect(
    page.locator(".ui-admin__content").getByRole("button", { name: /refresh/i }),
  ).toHaveCount(0);
});

test("an owner renames their workspace and the row follows", async ({ page }) => {
  await openWorkspace(page, ws.workspaceId, "/ui/admin", { adminToken: TEST_BEARER });

  const row = page.locator("table.ui-admin-table tbody tr", { hasText: ws.name });
  await expect(row).toBeVisible();
  await row.getByRole("button", { name: "Rename" }).click();
  const input = page.locator("input#workspace-rename");
  await expect(input).toBeVisible();
  await input.fill("renamed-workspace");
  await page.getByRole("button", { name: "Rename workspace" }).click();

  const renamed = page.locator("table.ui-admin-table tbody tr", { hasText: "renamed-workspace" });
  await expect(renamed).toBeVisible();
  await expect(renamed).toContainText("owner");
  await expect(row).toHaveCount(0);
  await expect(page.locator("#workspace-switcher option:checked")).toContainText("renamed-workspace");
});

test("a mixed workspace patch cannot silently ignore visibility", async ({ request }) => {
  const url = `${SERVER_ORIGIN}/ui/api/workspaces/${ws.workspaceId}`;
  const headers = { Authorization: `Bearer ${TEST_BEARER}` };
  const response = await request.patch(url, {
    headers,
    data: { name: "partial-rename", visibility: "public" },
  });
  expect(response.status()).toBe(400);

  const view = await request.get(url, { headers });
  expect(view.status()).toBe(200);
  const body: { workspace: { name: string; visibility: string } } = await view.json();
  expect(body.workspace.name).toBe("renamed-workspace");
  expect(body.workspace.visibility).toBe("private");
});

test("a pending rename cannot submit twice through Enter", async ({ page }) => {
  let patches = 0;
  await page.route(`**/ui/api/workspaces/${ws.workspaceId}`, async (route) => {
    if (route.request().method() === "PATCH") {
      patches += 1;
      const delay = Promise.withResolvers<void>();
      setTimeout(delay.resolve, 1_000);
      await delay.promise;
    }
    await route.continue();
  });
  await openWorkspace(page, ws.workspaceId, "/ui/admin", { adminToken: TEST_BEARER });
  const row = page.locator("table.ui-admin-table tbody tr", { hasText: ws.workspaceId });
  await row.getByRole("button", { name: "Rename" }).click();
  const input = page.locator("#workspace-rename");
  await input.fill("rename-once");
  await page.getByRole("button", { name: "Rename workspace" }).click();
  await expect(input).toBeDisabled();
  await page.keyboard.press("Enter");
  await expect(row).toContainText("rename-once");
  expect(patches).toBe(1);
});

test("the nested admin callback route loads the admin page", async ({ page }) => {
  await openWorkspace(page, ws.workspaceId, "/ui/admin/callback", { adminToken: TEST_BEARER });

  await expect(page.getByRole("heading", { name: "Workspaces" })).toBeVisible();
  await expect(page.locator(".ui-topbar")).toBeVisible();
});