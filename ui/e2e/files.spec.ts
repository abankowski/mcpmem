// Files tab behaviors (U9): upload a real text file through the dropzone,
// watch the row poll from uploaded/extracting to ready, preview the escaped
// text in the lightbox, and download the exact bytes.

import { expect, test, type Page } from "@playwright/test";
import { readFileSync } from "node:fs";
import path from "node:path";
import { SERVER_ORIGIN, TEMP_DIR, TEXT_FIXTURE, ensureServer, openWorkspace, seedWorkspace, type WorkspaceShape } from "./helpers";

test.describe.configure({ mode: "serial" });

let ws: WorkspaceShape;
let teardownServer: (() => void) | undefined;

test.afterAll(async () => {
  teardownServer?.();
});

test.beforeAll(async () => {
  teardownServer = await ensureServer();
  ws = await seedWorkspace(`e2e-files-${Date.now()}`, {
    entities: [{ name: "files-node", entityType: "note" }],
  });
});

async function openFilesTab(page: Page): Promise<void> {
  await openWorkspace(page, ws.workspaceId, "/ui?node=files-node");
  await expect(page.locator(".g-inspector__header h2")).toHaveText("files-node");
  await page.locator(".g-inspector").getByRole("tab", { name: "Files" }).click();
  await expect(page.locator(".ui-files")).toBeVisible();
  await page.locator("input[type=file]").waitFor({ state: "visible" });
}

test("upload, poll to ready, preview text, and download the exact bytes", async ({ page }) => {
  await openFilesTab(page);
  await expect(page.locator(".ui-files-state", { hasText: "No files attached yet." })).toBeVisible();

  await page.locator("input[type=file]").setInputFiles({
    name: "e2e-notes.txt",
    mimeType: "text/plain",
    buffer: Buffer.from(TEXT_FIXTURE),
  });

  // The row names the selected file from the start (uploaded/extracting),
  // then the panel's own polling moves it to ready without a reload.
  const row = page.locator(".ui-files-row", { hasText: "e2e-notes.txt" });
  await expect(row).toBeVisible();
  await expect(row).toContainText("1 page", { timeout: 60_000 });
  await expect(row.locator(".ui-files-row__meta")).toContainText("ready", { timeout: 60_000 });

  // Read: the lightbox shows the extracted page with the raw text escaped.
  await row.getByRole("button", { name: "Preview" }).click();
  const lightbox = page.locator("dialog.ui-files-lightbox");
  await expect(lightbox).toBeVisible();
  await expect(lightbox).toContainText("Page 1 of 1");
  const text = await lightbox.locator("pre.ui-files-lightbox__text").textContent();
  expect(text).toContain("E2E files text line one.");
  expect(text).toContain("E2E files text line two.");
  await page.keyboard.press("Escape");
  await expect(lightbox).toBeHidden();

  // Download: the anchor hands back the original bytes byte for byte.
  const downloadPromise = page.waitForEvent("download");
  await row.getByRole("button", { name: "Download" }).click();
  const download = await downloadPromise;
  expect(download.suggestedFilename()).toBe("e2e-notes.txt");
  const target = path.join(TEMP_DIR, "downloaded-notes.txt");
  await download.saveAs(target);
  expect(readFileSync(target, "utf8")).toBe(TEXT_FIXTURE);
});

test("files offer attachment consent after an attachment scope refusal", async ({ page }) => {
  await page.route("**/ui/api/session**", async (route) => {
    const response = await route.fetch();
    const body: { scopes: string[] } = await response.json();
    await route.fulfill({
      response,
      json: { ...body, scopes: body.scopes.filter((scope) => scope !== "attachments") },
    });
  });
  await page.route("**/ui/api/attachments*", async (route) => {
    await route.fulfill({
      status: 403,
      contentType: "application/json",
      headers: {
        "WWW-Authenticate": `Bearer resource_metadata="${SERVER_ORIGIN}/.well-known/oauth-protected-resource", scope="graph-read graph-write vectors code attachments"`,
      },
      body: JSON.stringify({ code: "insufficient_scope", message: "insufficient scope" }),
    });
  });
  await openWorkspace(page, ws.workspaceId, "/ui?node=files-node");
  await expect(page.locator(".g-inspector__header h2")).toHaveText("files-node");
  await page.locator(".g-inspector").getByRole("tab", { name: "Files" }).click();
  await expect(page.locator(".ui-files [role=alert]")).toContainText("insufficient_scope: insufficient scope");

  const grant = page.getByRole("button", { name: "Grant attachments" });
  await expect(grant).toBeVisible();
  let requestedScopes = new Set<string>();
  await page.route("**/oauth/authorize**", async (route) => {
    const url = new URL(route.request().url());
    requestedScopes = new Set((url.searchParams.get("scope") ?? "").split(/\s+/).filter(Boolean));
    await route.abort();
  });

  await grant.click();
  await expect.poll(() => requestedScopes.has("attachments")).toBe(true);
});

test("the dropzone refuses a file type outside text and PDF", async ({ page }) => {
  await openFilesTab(page);
  await page.locator("input[type=file]").setInputFiles({
    name: "malicious.zip",
    mimeType: "application/zip",
    buffer: Buffer.from("not a zip"),
  });
  await expect(page.locator(".ui-files [role=alert]")).toContainText("this file type is not supported");
  await expect(page.locator(".ui-files-row", { hasText: "malicious.zip" })).toHaveCount(0);
});