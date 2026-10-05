// The four consumer-visible contracts ported from the deleted
// tests/ui_graph_defects.test.mjs, now as real browser assertions:
// 1. the attachments list request sends limit >= 1000;
// 2. the upload confirmation names the selected file;
// 3. the attachment next-page control follows pageCount, not /pages eof;
// 4. the OAuth return restores a node by name even when it is off-page.

import { expect, test, type Page } from "@playwright/test";
import {
  ensureServer,
  openWorkspace,
  seedBulkEntities,
  seedWorkspace,
  twoPagePdf,
  type WorkspaceShape,
} from "./helpers";

test.describe.configure({ mode: "serial" });

let ws: WorkspaceShape;
let bulkWs: WorkspaceShape;

const OFFPAGE_NODE = "e2e-offpage-target";

test.beforeAll(async () => {
  await ensureServer();
  ws = await seedWorkspace(`e2e-regressions-${Date.now()}`, {
    entities: [{ name: "regression-node", entityType: "note" }],
  });

  // A graph larger than one page: 1000 page-1 entries plus one target that
  // can only be restored by name.
  bulkWs = await seedWorkspace(`e2e-bulk-${Date.now()}`);
  const pageOne = Array.from({ length: 1000 }, (_, index) => `e2e-bulk-${String(index).padStart(4, "0")}`);
  await seedBulkEntities(bulkWs.workspaceId, pageOne);
  await seedBulkEntities(bulkWs.workspaceId, [OFFPAGE_NODE]);
});

async function openFilesTab(page: Page): Promise<void> {
  await openWorkspace(page, ws.workspaceId, "/ui?node=regression-node");
  await expect(page.locator(".g-inspector__header h2")).toHaveText("regression-node");
  await page.locator(".g-inspector").getByRole("tab", { name: "Files" }).click();
  await expect(page.locator(".ui-files")).toBeVisible();
  await page.locator("input[type=file]").waitFor({ state: "visible" });
}

test("the attachments list request sends limit >= 1000", async ({ page }) => {
  const listRequest = page.waitForRequest((request) => {
    const url = new URL(request.url());
    return (
      request.method() === "GET" &&
      url.pathname.endsWith("/ui/api/attachments") &&
      url.searchParams.get("entityName") === "regression-node"
    );
  });
  await openFilesTab(page);
  const request = await listRequest;
  const limit = Number(new URL(request.url()).searchParams.get("limit"));
  expect(limit, `the list request must carry limit >= 1000, sent ${limit}`).toBeGreaterThanOrEqual(1000);
});

test("the upload confirmation names the selected file", async ({ page }) => {
  // Hold the upload open so the in-flight confirmation is observable.
  await page.route("**/ui/api/attachments*", async (route) => {
    if (route.request().method() === "POST") {
      const { promise, resolve } = Promise.withResolvers<void>();
      setTimeout(resolve, 800);
      await promise;
    }
    await route.continue();
  });
  await openFilesTab(page);

  await page.locator("input[type=file]").setInputFiles({
    name: "confirmation.txt",
    mimeType: "text/plain",
    buffer: Buffer.from("confirmation fixture"),
  });

  const pending = page.locator(".ui-files-row", { hasText: "confirmation.txt" });
  await expect(pending).toBeVisible();
  await expect(pending).toContainText("Upload in progress");

  // The upload lands and the row moves through the polling states.
  await expect(pending.locator(".ui-files-row__meta")).toContainText("ready", { timeout: 60_000 });
  await expect(pending).toContainText("1 page");
});

test("the attachment next-page control follows pageCount", async ({ page }) => {
  await openFilesTab(page);

  // A one-page PDF still proves the extraction pipeline end to end: Poppler
  // renders both pages and the fixture vision endpoint transcribes them.
  await page.locator("input[type=file]").setInputFiles({
    name: "e2e-two-pages.pdf",
    mimeType: "application/pdf",
    buffer: twoPagePdf(),
  });
  const pdfRow = page.locator(".ui-files-row", { hasText: "e2e-two-pages.pdf" });
  await expect(pdfRow.locator(".ui-files-row__meta")).toContainText("ready", { timeout: 60_000 });
  await expect(pdfRow).toContainText("2 pages");

  // The pager lives on text previews. The contract: Next follows pageCount,
  // never the /pages eof flag. A body longer than one 4096-char span ends
  // the first span with eof=false, which must not arm Next for page 1 of 1.
  const longText = `E2E page-one span. ${"z".repeat(9000)} The end of page one.`;
  await page.locator("input[type=file]").setInputFiles({
    name: "e2e-long.txt",
    mimeType: "text/plain",
    buffer: Buffer.from(longText),
  });
  const row = page.locator(".ui-files-row", { hasText: "e2e-long.txt" });
  await expect(row.locator(".ui-files-row__meta")).toContainText("ready", { timeout: 60_000 });
  await expect(row).toContainText("1 page");

  await row.getByRole("button", { name: "Preview" }).click();
  const lightbox = page.locator("dialog.ui-files-lightbox");
  await expect(lightbox).toBeVisible();
  await expect(lightbox).toContainText("Page 1 of 1");
  const nextButton = lightbox.getByRole("button", { name: "Next" });
  await expect(nextButton).toBeDisabled();

  // The first span is not at eof, so "Show more text" is offered; Next stays
  // disabled the whole walk because pageCount is 1.
  const more = lightbox.getByRole("button", { name: "Show more text" });
  for (let index = 0; index < 4; index++) {
    if (!(await more.isEnabled().catch(() => false))) break;
    await more.click();
    await expect(nextButton).toBeDisabled();
  }
  await expect(lightbox).toContainText("Page 1 of 1");
  await expect(nextButton).toBeDisabled();
});

test("the OAuth return restores a node by name even when it is off-page", async ({ page }) => {
  // First page of the bulk workspace: the target is not among the 1000 rows.
  await openWorkspace(page, bulkWs.workspaceId, "/ui");
  await expect(page.locator(".g-pager")).toContainText("1,000 nodes", { timeout: 60_000 });
  await expect(page.locator(".g-pager")).toContainText("Page 1");
  await expect(page.locator(".g-inspector__header h2")).toHaveCount(0);

  // Paged loading: Next follows the server page metadata.
  await page.locator(".g-pager").getByRole("button", { name: "Next" }).click();
  await expect(page.locator(".g-pager")).toContainText("Page 2", { timeout: 30_000 });
  await expect(page.locator(".g-pager")).toContainText("1,001 nodes");

  // The OAuth return state: the saved URL carries the node name, the page
  // reloads, and the inspector restores it by name from beyond page one.
  await openWorkspace(page, bulkWs.workspaceId, `/ui?node=${OFFPAGE_NODE}`);
  await expect(page.locator(".g-inspector__header h2")).toHaveText(OFFPAGE_NODE, { timeout: 60_000 });
  await expect(page).toHaveURL(/\/ui$/);
});