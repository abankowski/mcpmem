// Responsive layout, keyboard, and focus behaviors (U11) at 1440, 900, and
// 390 pixels, following the handoff: Cmd/Ctrl+K palette, Escape, F fit, the
// plus/minus zoom keys, and the top-bar -> rail -> canvas -> inspector tab
// order.

import { expect, test, type Page } from "@playwright/test";
import {
  canvasNodeBlobs,
  ensureServer,
  openWorkspace,
  seedWorkspace,
  waitForBlobSettle,
  type WorkspaceShape,
} from "./helpers";

test.describe.configure({ mode: "serial" });

let ws: WorkspaceShape;
let teardownServer: (() => void) | undefined;

test.afterAll(async () => {
  teardownServer?.();
});

test.beforeAll(async () => {
  teardownServer = await ensureServer();
  ws = await seedWorkspace(`e2e-responsive-${Date.now()}`, {
    entities: [
      { name: "R-A", entityType: "note" },
      { name: "R-B", entityType: "note" },
    ],
  });
});

async function horizontalOverflow(page: Page): Promise<number> {
  return page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);
}

function blobArea(blobs: Array<{ count: number }>): number {
  return blobs.reduce((sum, blob) => sum + blob.count, 0);
}

test.describe("responsive layout", () => {
  test("1440px: expanded rail, floating toolbar, and a right-side inspector", async ({ page }) => {
    await openWorkspace(page, ws.workspaceId, "/ui?node=R-A");
    await expect(page.locator(".g-inspector__header h2")).toHaveText("R-A");

    await expect(page.locator("section[aria-label='Entity types']")).toBeVisible();
    await expect(page.locator("button[aria-label='Expand filter rail']")).toHaveCount(0);
    await expect(page.getByRole("button", { name: "Zoom in" })).toBeVisible();

    const inspectorBox = await page.locator(".g-inspector").boundingBox();
    expect(inspectorBox).not.toBeNull();
    expect(inspectorBox!.x).toBeGreaterThanOrEqual(900);
    expect(inspectorBox!.x + inspectorBox!.width).toBeLessThanOrEqual(1441);

    expect(await horizontalOverflow(page)).toBeLessThanOrEqual(1);
  });

  test("900px: the top bar wraps and the inspector becomes a bottom sheet", async ({ page }) => {
    await page.setViewportSize({ width: 900, height: 800 });
    await openWorkspace(page, ws.workspaceId, "/ui?node=R-A");
    await expect(page.locator(".g-inspector__header h2")).toHaveText("R-A");

    // The wrap rule hides the command trigger's label; the nav moves to its
    // own row and stays visible.
    await expect(page.locator(".ui-topbar__command span")).toBeHidden();
    await expect(page.locator(".ui-topbar__nav a", { hasText: "Graph" })).toBeVisible();

    const inspectorBox = await page.locator(".g-inspector").boundingBox();
    expect(inspectorBox).not.toBeNull();
    // The sheet hugs the viewport bottom: height min(58vh, 480px), 8px gap.
    expect(inspectorBox!.y).toBeGreaterThan(250);
    expect(inspectorBox!.y + inspectorBox!.height).toBeGreaterThanOrEqual(792);
    expect(inspectorBox!.y + inspectorBox!.height).toBeLessThanOrEqual(801);

    expect(await horizontalOverflow(page)).toBeLessThanOrEqual(1);
  });

  test("390px: the phone layout stays within the viewport and usable", async ({ page }) => {
    await page.setViewportSize({ width: 390, height: 844 });
    await openWorkspace(page, ws.workspaceId, "/ui");

    await expect(page.locator(".ui-topbar__avatar")).toBeHidden();
    await expect(page.locator(".ui-topbar__command kbd")).toBeHidden();
    await expect(page.getByRole("button", { name: "Zoom in" })).toBeVisible();
    await expect(page.locator("canvas.g-canvas")).toBeVisible();
    expect(await horizontalOverflow(page)).toBeLessThanOrEqual(1);

    await page.getByRole("button", { name: "Collapse filter rail" }).click();
    await expect(page.locator("section[aria-label='Entity types']")).toBeHidden();
    expect(await horizontalOverflow(page)).toBeLessThanOrEqual(1);

    // A selected node opens an inspector that fits the phone viewport.
    await openWorkspace(page, ws.workspaceId, "/ui?node=R-A");
    await expect(page.locator(".g-inspector__header h2")).toHaveText("R-A");
    const inspectorBox = await page.locator(".g-inspector").boundingBox();
    expect(inspectorBox).not.toBeNull();
    expect(inspectorBox!.x).toBeGreaterThanOrEqual(0);
    expect(inspectorBox!.x + inspectorBox!.width).toBeLessThanOrEqual(391);
  });
});

test.describe("keyboard", () => {
  test("Cmd/Ctrl+K opens the node palette and Escape closes it", async ({ page }) => {
    await openWorkspace(page, ws.workspaceId, "/ui");
    const palette = page.locator("dialog.ui-command-palette");

    await page.keyboard.press("Control+K");
    await expect(palette).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(palette).toBeHidden();

    await page.keyboard.press("Meta+K");
    await expect(palette).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(palette).toBeHidden();
  });

  test("Escape cancels connect mode", async ({ page }) => {
    await openWorkspace(page, ws.workspaceId, "/ui");
    await expect(page.locator("canvas.g-canvas")).toBeVisible();

    await page.getByRole("button", { name: "Connect", exact: true }).click();
    await expect(page.locator(".g-connect-hint")).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(page.locator(".g-connect-hint")).toHaveCount(0);
    await expect(page.getByRole("button", { name: "Connect", exact: true })).toBeVisible();
  });

  test("F fits the graph and the plus and minus keys zoom the canvas", async ({ page }) => {
    await openWorkspace(page, ws.workspaceId, "/ui");
    await waitForBlobSettle(page);

    const before = await canvasNodeBlobs(page, "--node-1");
    const beforeArea = blobArea(before.blobs);
    expect(before.blobs.length).toBeGreaterThanOrEqual(2);

    await page.keyboard.press("+");
    await page.waitForTimeout(400);
    const zoomedIn = await canvasNodeBlobs(page, "--node-1");
    expect(blobArea(zoomedIn.blobs)).toBeGreaterThan(beforeArea * 1.15);

    await page.keyboard.press("-");
    await page.waitForTimeout(400);
    const zoomedOut = await canvasNodeBlobs(page, "--node-1");
    expect(blobArea(zoomedOut.blobs)).toBeLessThan(blobArea(zoomedIn.blobs) * 0.85);

    // Fit after zooming in: every node pixel lands inside the canvas rect.
    await page.keyboard.press("+");
    await page.keyboard.press("+");
    await page.waitForTimeout(300);
    await page.keyboard.press("f");
    await page.waitForTimeout(400);
    const fitted = await canvasNodeBlobs(page, "--node-1");
    const rect = fitted.rect;
    for (const blob of fitted.blobs) {
      expect(blob.minX).toBeGreaterThanOrEqual(rect.left - 4);
      expect(blob.minY).toBeGreaterThanOrEqual(rect.top - 4);
      expect(blob.maxX).toBeLessThanOrEqual(rect.left + rect.width + 4);
      expect(blob.maxY).toBeLessThanOrEqual(rect.top + rect.height + 4);
    }
  });
});

test.describe("focus order", () => {
  test("tab moves through top bar, rail, and canvas toolbar before the inspector", async ({ page }) => {
    await openWorkspace(page, ws.workspaceId, "/ui?node=R-A");
    await expect(page.locator(".g-inspector__header h2")).toHaveText("R-A");

    interface FocusedElement {
      label: string | null;
      text: string;
      className: string;
      tagName: string;
    }
    const focused = (): Promise<FocusedElement | null> =>
      page.evaluate(() => {
        const element = document.activeElement;
        if (!element) return null;
        const label = element.getAttribute("aria-label");
        const text = element.textContent?.trim() ?? "";
        const className = typeof element.className === "string" ? element.className : "";
        return { label, text: text.slice(0, 40), className: className.slice(0, 120), tagName: element.tagName };
      });

    const pressTab = async (): Promise<FocusedElement | null> => {
      await page.keyboard.press("Tab");
      const current = await focused();
      expect(current, "a focused element after Tab").not.toBeNull();
      return current;
    };

    const skip = await pressTab();
    expect(skip!.className).toContain("ui-skip-link");

    const brand = await pressTab();
    expect(brand!.className).toContain("ui-topbar__brand");

    const switcher = await pressTab();
    expect(switcher!.tagName).toBe("SELECT");

    const graphNav = await pressTab();
    expect(graphNav!.text).toBe("Graph");
    const searchNav = await pressTab();
    expect(searchNav!.text).toBe("Search");
    const adminNav = await pressTab();
    expect(adminNav!.text).toBe("Admin");

    const command = await pressTab();
    expect(command!.className).toContain("ui-topbar__command");

    const railToggle = await pressTab();
    expect(railToggle!.className).toContain("g-rail__toggle");

    // The expanded rail's entity-type filter is a real control: it comes
    // next in the tab order (the workspace's single "note" type), then the
    // canvas toolbar.
    const entityFilter = await pressTab();
    expect(entityFilter!.tagName).toBe("INPUT");
    const filterLabel = await page.evaluate(
      () => document.activeElement?.closest("label")?.textContent?.trim() ?? "",
    );
    expect(filterLabel).toContain("note");

    const fit = await pressTab();
    expect(fit!.label).toBe("Fit graph");
    const zoomIn = await pressTab();
    expect(zoomIn!.label).toBe("Zoom in");
  });
});