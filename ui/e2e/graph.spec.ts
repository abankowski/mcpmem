// Graph screen behaviors (U6): a create-and-persist round trip through the
// browser, inspector observation edits, relation type changes through the
// edge inspector, and connect mode with an exact relation triple driven by
// real canvas clicks.

import { expect, test, type Locator, type Page } from "@playwright/test";
import {
  canvasNodeBlobs,
  ensureServer,
  openWorkspace,
  seedWorkspace,
  waitForBlobSettle,
  type Blob,
  type WorkspaceShape,
} from "./helpers";

test.describe.configure({ mode: "serial" });

let graphWs: WorkspaceShape;
let connectWs: WorkspaceShape;

test.beforeAll(async () => {
  await ensureServer();
  graphWs = await seedWorkspace(`e2e-graph-${Date.now()}`, {
    entities: [
      { name: "Alpha", entityType: "note", observations: ["Alpha knows the graph protocol."] },
      { name: "Beta", entityType: "note", observations: ["Beta holds the other end."] },
    ],
    relations: [
      { from: "Alpha", to: "Beta", relationType: "knows", observations: ["Alpha and Beta stay connected."] },
    ],
  });
  connectWs = await seedWorkspace(`e2e-connect-${Date.now()}`, {
    entities: [{ name: "Solo-A", entityType: "note" }, { name: "Solo-B", entityType: "note" }],
  });
});

function inspector(page: Page) {
  return page.locator(".g-inspector");
}

function inspectorTitle(page: Page) {
  // The header h2 only; dialogs in the inspector carry their own headings.
  return page.locator(".g-inspector__header h2");
}

async function pickNode(page: Page, name: string): Promise<void> {
  await page.keyboard.press("Control+K");
  const palette = page.locator("dialog.ui-command-palette");
  await expect(palette).toBeVisible();
  await palette.locator("input").fill(name);
  const option = palette.locator(`li[role=option]:has-text("${name}")`).first();
  await expect(option).toBeVisible();
  await page.keyboard.press("Enter");
}

test.describe("graph round trip", () => {
  test("creates a node with an observation and it persists after reload", async ({ page }) => {
    await openWorkspace(page, graphWs.workspaceId, "/ui?node=Alpha");
    await expect(inspectorTitle(page)).toHaveText("Alpha");

    // New menu -> Node.
    await page.getByRole("button", { name: "New", exact: true }).click();
    await page.getByRole("menuitem", { name: "Node" }).click();
    const sheet = page.getByRole("dialog", { name: "New node" });
    await expect(sheet).toBeVisible();
    await sheet.locator("#g-new-node-name").fill("Browser Node");
    await sheet.locator("#g-new-node-type").fill("note");
    await sheet.locator("#g-new-node-obs").fill("made in the browser by the e2e suite");
    await sheet.getByRole("button", { name: "Create node" }).click();

    await expect(page.locator(".ui-toast", { hasText: "Node Browser Node created." })).toBeVisible();
    await expect(inspectorTitle(page)).toHaveText("Browser Node");
    await expect(inspector(page)).toContainText("made in the browser by the e2e suite");

    // Reload: the node must still exist, reachable by name through the
    // command palette.
    await page.reload();
    await expect(page.locator(".g-canvas")).toBeVisible();
    await pickNode(page, "Browser Node");
    await expect(inspectorTitle(page)).toHaveText("Browser Node");
    await expect(inspector(page)).toContainText("made in the browser by the e2e suite");
  });

  test("adds an observation in the inspector and it persists", async ({ page }) => {
    await openWorkspace(page, graphWs.workspaceId, "/ui?node=Beta");
    await expect(inspectorTitle(page)).toHaveText("Beta");

    const observations = inspector(page).locator("section[aria-label=Observations]");
    await observations.getByRole("button", { name: "Add" }).click();
    await observations.getByLabel("Observation body").fill("browser added note");
    await observations.getByRole("button", { name: "Add", exact: true }).click();
    await expect(observations).toContainText("browser added note");

    await page.reload();
    await pickNode(page, "Beta");
    const reloaded = inspector(page).locator("section[aria-label=Observations]");
    await expect(reloaded).toContainText("browser added note");
  });

  test("changes a relation type through the edge inspector and it persists", async ({ page }) => {
    await openWorkspace(page, graphWs.workspaceId, "/ui?node=Alpha");
    await expect(inspectorTitle(page)).toHaveText("Alpha");

    await inspector(page).getByRole("tab", { name: "Relations" }).click();
    const row = inspector(page).locator(".g-rel-row", { hasText: "knows" });
    await expect(row).toBeVisible();
    await row.click();

    // The edge inspector replaces the node inspector; its sections live in
    // its own container.
    const changeType = page.locator("section[aria-label='Change type']");
    await expect(changeType).toBeVisible();
    await page.getByLabel("New relation type").fill("mentors");
    await page.getByRole("button", { name: "Change type" }).click();

    await expect(page.locator(".ui-toast", { hasText: "Relation type changed." })).toBeVisible();
    // The edge inspector remounts with the new triple.
    await expect(page.getByLabel("New relation type")).toHaveValue("mentors");

    await page.reload();
    await pickNode(page, "Alpha");
    await inspector(page).getByRole("tab", { name: "Relations" }).click();
    await expect(inspector(page).locator(".g-rel-row", { hasText: "mentors" })).toBeVisible();
  });
});

test.describe("connect mode", () => {
  test("connects two nodes with an exact relation triple via canvas clicks", async ({ page }) => {
    await openWorkspace(page, connectWs.workspaceId, "/ui");
    await expect(page.locator("canvas.g-canvas")).toBeVisible();

    const settled = await waitForBlobSettle(page);
    expect(
      settled.blobs.length,
      `expected exactly two node blobs on the canvas, found ${settled.blobs.length}`,
    ).toBe(2);

    await page.getByRole("button", { name: "Connect", exact: true }).click();
    await expect(page.locator(".g-connect-hint")).toContainText("click the source node");

    // The nodes drift a little while the layout settles; re-scan before each
    // click and retry at the fresh centroid until the canvas registers.
    const [sourceSeed, targetSeed] = settled.blobs;
    const clickNode = async (seed: Blob, marker: Locator): Promise<void> => {
      for (let attempt = 0; attempt < 5; attempt++) {
        const fresh = await canvasNodeBlobs(page, "--node-1");
        const blobs = fresh.blobs.length >= 2 ? fresh.blobs : settled.blobs;
        const target = blobs.reduce((best, blob) =>
          Math.hypot(blob.x - seed.x, blob.y - seed.y) < Math.hypot(best.x - seed.x, best.y - seed.y) ? blob : best,
        );
        await page.mouse.click(target.x, target.y);
        try {
          await expect(marker).toBeVisible({ timeout: 1500 });
          return;
        } catch {
          // The layout moved the node or the hit missed; scan again.
        }
      }
      throw new Error("the canvas did not register a node click");
    };

    await clickNode(sourceSeed, page.locator(".g-connect-hint", { hasText: "Now click the target node" }));
    const sheet = page.getByRole("dialog", { name: /^Connect / });
    await clickNode(targetSeed, sheet);
    await expect(sheet).toBeVisible();
    await sheet.locator("#g-connect-type").fill("links");
    await sheet.getByRole("button", { name: "Create relation" }).click();

    await expect(page.locator(".ui-toast", { hasText: "Relation " })).toContainText("—links→");

    // The triple must be inspectable from the source node's relations tab.
    await pickNode(page, "Solo-A");
    await expect(inspectorTitle(page)).toHaveText("Solo-A");
    await inspector(page).getByRole("tab", { name: "Relations" }).click();
    const row = inspector(page).locator(".g-rel-row", { hasText: "links" });
    await expect(row).toBeVisible();
    expect(await row.textContent()).toContain("Solo-B");
  });
});