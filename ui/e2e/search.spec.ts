// Search screen behaviors (U7): URL-held state, direct node and relation
// hits, relation endpoint filters, the named unavailable state for vector
// modes without a profile, the k control, and "In graph".

import { expect, test } from "@playwright/test";
import { ensureServer, openWorkspace, seedWorkspace, type WorkspaceShape } from "./helpers";

test.describe.configure({ mode: "serial" });

let ws: WorkspaceShape;
let teardownServer: (() => void) | undefined;

test.afterAll(async () => {
  teardownServer?.();
});

test.beforeAll(async () => {
  teardownServer = await ensureServer();
  ws = await seedWorkspace(`e2e-search-${Date.now()}`, {
    entities: [
      { name: "Alpha", entityType: "note", observations: ["zirconium alpha flavour"] },
      { name: "Beta", entityType: "note", observations: ["zirconium beta flavour"] },
      { name: "Gamma", entityType: "note", observations: ["unrelated gamma only"] },
    ],
    relations: [
      { from: "Alpha", to: "Beta", relationType: "knows", observations: ["zirconium alpha-beta link"] },
    ],
  });
});

const QUERY = "zirconium";

test("keeps the query and controls in the URL and returns direct node hits", async ({ page }) => {
  await openWorkspace(page, ws.workspaceId, "/ui/search");
  await page.getByLabel("Search query").fill(QUERY);
  await page.keyboard.press("Enter");

  await expect(page).toHaveURL(/q=zirconium/);
  await expect(page).toHaveURL(/mode=direct/);
  await expect(page).toHaveURL(/scope=nodes/);

  const alphaCard = page.locator(".s-result", { hasText: "Alpha" });
  await expect(alphaCard).toBeVisible();
  await expect(page.locator(".s-result__meta", { hasText: "node" })).toHaveCount(2);
  await expect(page.locator(".s-results__meta")).toContainText("direct");
});

test("relation scope returns a relation hit and honors the endpoint filters", async ({ page }) => {
  await openWorkspace(page, ws.workspaceId, "/ui/search");
  await page.getByLabel("Search query").fill(QUERY);
  await page.keyboard.press("Enter");
  await expect(page.locator(".s-result")).toHaveCount(2);

  await page.getByRole("radio", { name: "Relations" }).check();
  await expect(page).toHaveURL(/scope=relations/);

  const relationCard = page.locator(".s-result", { hasText: "knows" });
  await expect(relationCard).toBeVisible();
  expect(await relationCard.locator(".s-result__title").textContent()).toContain("Alpha");
  expect(await relationCard.locator(".s-result__title").textContent()).toContain("Beta");
  await expect(relationCard.locator(".s-result__meta", { hasText: "relation" })).toBeVisible();

  // A matching source filter keeps the hit.
  await page.getByLabel("Relation source filter").fill("Alpha");
  await page.keyboard.press("Enter");
  await expect(page).toHaveURL(/from=Alpha/);
  await expect(page.locator(".s-result", { hasText: "knows" })).toBeVisible();

  // A non-matching source filter empties the result list.
  await page.getByLabel("Relation source filter").fill("Nobody-zirconium");
  await page.keyboard.press("Enter");
  await expect(page.locator(".s-panel")).toContainText("No results for");
});

test("semantic and hybrid modes show the named unavailable state and fall back to direct", async ({ page }) => {
  await openWorkspace(page, ws.workspaceId, "/ui/search");
  await page.getByLabel("Search query").fill(QUERY);
  await page.keyboard.press("Enter");
  await expect(page.locator(".s-result")).toHaveCount(2);

  for (const mode of ["Semantic", "Hybrid"] as const) {
    await page.getByRole("radio", { name: mode }).check();
    await expect(page).toHaveURL(new RegExp(`mode=${mode.toLowerCase()}`));
    await expect(page.getByRole("heading", { name: "Vector search unavailable" })).toBeVisible();
    await expect(page.getByRole("button", { name: "Use Direct search" })).toBeVisible();

    await page.getByRole("button", { name: "Use Direct search" }).click();
    await expect(page).toHaveURL(/mode=direct/);
    await expect(page.locator(".s-result")).toHaveCount(2);
  }
});

test("the result limit control writes k to the URL", async ({ page }) => {
  await openWorkspace(page, ws.workspaceId, "/ui/search");
  await page.getByLabel("Search query").fill(QUERY);
  await page.keyboard.press("Enter");
  await expect(page).toHaveURL(/k=10/);

  await page.getByLabel("Result limit").selectOption("50");
  await expect(page).toHaveURL(/k=50/);
  await expect(page.locator(".s-result")).toHaveCount(2);
});

test("In graph opens the same node in the Graph screen", async ({ page }) => {
  await openWorkspace(page, ws.workspaceId, "/ui/search");
  await page.getByLabel("Search query").fill("Alpha");
  await page.keyboard.press("Enter");

  const alphaCard = page.locator(".s-result", { hasText: "Alpha" }).first();
  await expect(alphaCard).toBeVisible();
  await alphaCard.getByRole("button", { name: "In graph" }).click();

  // The graph page consumes the ?node param and selects the node by name.
  await expect(page.locator(".g-inspector__header h2")).toHaveText("Alpha");
});