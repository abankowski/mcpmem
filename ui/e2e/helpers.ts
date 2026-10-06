// Shared harness for the browser behavior suite.
//
// The Playwright webServer starts the server binary once per run; the specs
// seed their own workspaces through the API and open the app with the static
// bearer in the URL fragment. `ensureServer` also supports a standalone run
// (no webServer): it spawns the binary itself and tears it down when the
// returned function is called.

import { spawn, type ChildProcess } from "node:child_process";
import { existsSync, mkdirSync, rmSync, writeFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import type { Page } from "@playwright/test";

export const SERVER_ORIGIN = "http://127.0.0.1:8080";
export const TEST_BEARER = "e2e-static-bearer";
export const TEMP_DIR = "/tmp/mcpmem-e2e";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
export const BINARY = path.join(repoRoot, "target", "debug", "mcpmem");
export const VISION_PORT = 8092;
export const CONFIG_FILE = path.join(repoRoot, "ui", "e2e", "server.toml");

/** The server flags shared by the webServer command and the standalone spawn. */
export function serverArgs(): string[] {
  return [
    "--config",
    CONFIG_FILE,
    "--memory-file",
    `${TEMP_DIR}/graph.mem`,
    "--transport",
    "http",
    "--bind",
    "127.0.0.1:8080",
    "--enable-all",
    "--ui",
    "true",
    "--auth-token",
    TEST_BEARER,
    "--legacy-owner-id",
    "machine:local",
    "--role",
    "mcp,extractor",
    // A fake OIDC issuer: nothing in the suite completes a provider exchange
    // (authorize requests are intercepted or never signed), but the resource
    // must advertise the OAuth challenge so the UI's sign-in grant paths run.
    // The issuer and public-url must be https (config.rs refuses anything else);
    // traffic still goes to 127.0.0.1, the https values are canonical only.
    "--oidc-issuer",
    "https://idp.e2e.invalid",
    "--public-url",
    "https://ui.e2e.invalid",
    "--oidc-client-id",
    "e2e-oidc-client",
    "--oidc-client-secret-file",
    `${TEMP_DIR}/oidc-secret`,
    "--principals-file",
    path.join(path.dirname(fileURLToPath(import.meta.url)), "principals.json"),
    "--oauth-trust-forwarded-proto",
    "--log-level",
    "info",
    "--log-file",
    `${TEMP_DIR}/server.log`,
  ];
}

/**
 * Start the fixture vision endpoint (PDF transcription) if it is not already
 * running. Returns a teardown that stops it. The Playwright config also
 * starts it, so the common path here is a no-op health check.
 */
export async function startE2eServices(): Promise<() => void> {
  try {
    const response = await fetch(`http://127.0.0.1:${VISION_PORT}/health`);
    if (response.ok) return () => {};
  } catch {
    // Not running; spawn below.
  }
  const servicesPath = path.join(path.dirname(fileURLToPath(import.meta.url)), "e2e-services.mjs");
  const child = spawn(process.execPath, [servicesPath, String(VISION_PORT)], { stdio: "ignore" });
  const deadline = Date.now() + 30_000;
  let ready = false;
  while (Date.now() < deadline && !ready) {
    try {
      const response = await fetch(`http://127.0.0.1:${VISION_PORT}/health`);
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
    child.kill("SIGTERM");
    throw new Error(`The e2e vision endpoint did not become ready on port ${VISION_PORT}`);
  }
  return () => {
    child.kill("SIGTERM");
  };
}

export interface WorkspaceShape {
  workspaceId: string;
  name: string;
  visibility: "private" | "public";
  role: "owner" | "writer" | "reader" | "public";
  isDefault: boolean;
}

export async function apiJson<T>(route: string, init: RequestInit = {}): Promise<T> {
  const response = await fetch(`${SERVER_ORIGIN}${route}`, {
    ...init,
    headers: { Authorization: `Bearer ${TEST_BEARER}`, ...(init.headers ?? {}) },
  });
  const body = await response.text();
  if (!response.ok) {
    throw new Error(`API ${init.method ?? "GET"} ${route} -> ${response.status}: ${body.slice(0, 300)}`);
  }
  return JSON.parse(body) as T;
}

/** Wait until GET /ui answers 200. Used by the standalone spawn path. */
export async function waitForServer(timeoutMs = 60_000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  let lastError = "server not reachable";
  while (Date.now() < deadline) {
    try {
      const response = await fetch(`${SERVER_ORIGIN}/ui`);
      if (response.ok) return;
      lastError = `GET /ui -> ${response.status}`;
    } catch (error) {
      lastError = error instanceof Error ? error.message : String(error);
    }
    const { promise, resolve } = Promise.withResolvers<void>();
    setTimeout(resolve, 250);
    await promise;
  }
  throw new Error(`The e2e server did not become ready: ${lastError}`);
}

let spawned: ChildProcess | null = null;

/**
 * Make sure the server is up. With the Playwright webServer this is a no-op;
 * without one, spawn the binary and return a teardown that stops it.
 */
export async function ensureServer(): Promise<() => void> {
  try {
    const response = await fetch(`${SERVER_ORIGIN}/ui`);
    if (response.ok) return () => {};
  } catch {
    // Not running; spawn below.
  }
  if (spawned) return () => {};
  if (!existsSync(BINARY)) {
    throw new Error(
      `The e2e binary is missing at ${BINARY}. Build it with: cargo build --features indexer,extractor,webhooks`,
    );
  }
  rmSync(TEMP_DIR, { recursive: true, force: true });
  mkdirSync(TEMP_DIR, { recursive: true });
  writeFileSync(path.join(TEMP_DIR, "vision-key"), "e2e-vision-key\n");
  writeFileSync(path.join(TEMP_DIR, "oidc-secret"), "e2e-oidc-secret\n");
  const stopServices = await startE2eServices();
  spawned = spawn(BINARY, serverArgs(), { stdio: "ignore" });
  await waitForServer();
  return () => {
    spawned?.kill("SIGTERM");
    spawned = null;
    stopServices();
  };
}

export interface EntitySeed {
  name: string;
  entityType?: string;
  observations?: readonly string[];
}

export interface RelationSeed {
  from: string;
  to: string;
  relationType: string;
  observations?: readonly string[];
}

export async function mutate(workspaceId: string, operation: string, payload: Record<string, unknown>): Promise<void> {
  await apiJson("/ui/api/mutations", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ workspaceId, operation, payload }),
  });
}

export async function createWorkspace(name: string): Promise<WorkspaceShape> {
  const result = await apiJson<{ workspace: WorkspaceShape }>("/ui/api/workspaces", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ name }),
  });
  return result.workspace;
}

export async function seedWorkspace(
  name: string,
  opts: { entities?: readonly EntitySeed[]; relations?: readonly RelationSeed[] } = {},
): Promise<WorkspaceShape> {
  const workspace = await createWorkspace(name);
  for (const entity of opts.entities ?? []) {
    await mutate(workspace.workspaceId, "createEntity", {
      name: entity.name,
      entityType: entity.entityType ?? "note",
      observations: (entity.observations ?? []).map((body) => ({ body })),
    });
  }
  for (const relation of opts.relations ?? []) {
    await mutate(workspace.workspaceId, "createRelation", {
      from: relation.from,
      to: relation.to,
      relationType: relation.relationType,
      observations: (relation.observations ?? []).map((body) => ({ body })),
    });
  }
  return workspace;
}

/**
 * Create many entities quickly, in small concurrent batches. A duplicate
 * name answers 409 and is tolerated (idempotent re-seed).
 */
export async function seedBulkEntities(workspaceId: string, names: readonly string[]): Promise<void> {
  const batchSize = 12;
  for (let start = 0; start < names.length; start += batchSize) {
    const batch = names.slice(start, start + batchSize);
    await Promise.all(
      batch.map(async (name) => {
        try {
          await mutate(workspaceId, "createEntity", { name, entityType: "bulk", observations: [] });
        } catch (error) {
          const message = error instanceof Error ? error.message : String(error);
          if (!message.includes("409")) throw error;
        }
      }),
    );
  }
}

/** Open the app as the static bearer. The token lives in the fragment only. */
export async function openApp(page: Page, path = "/ui"): Promise<void> {
  await page.goto(`${path}#token=${TEST_BEARER}`);
}

/** Open the app with a pinned workspace selection for every navigation. */
export async function openWorkspace(
  page: Page,
  workspaceId: string,
  path = "/ui",
  opts: { adminToken?: string } = {},
): Promise<void> {
  await page.addInitScript(({ selected, adminToken, bearer }) => {
    sessionStorage.setItem("mcpmem_workspace_selection", selected);
    // The browser keeps a separate admin token. The fixture places the same
    // static bearer there: the server still answers 200 with the bearer's
    // real scopes (which hold no admin scope), so the page renders the
    // owner-without-admin state this suite asserts.
    if (adminToken) sessionStorage.setItem("mcpmem_admin_access", adminToken);
    // The static fragment handoff stores the graph token under its own key.
    if (!adminToken && bearer) sessionStorage.setItem("mcpmem_token", bearer);
  }, { selected: workspaceId, adminToken: opts.adminToken ?? null, bearer: TEST_BEARER });
  await openApp(page, path);
}

export interface Blob {
  x: number;
  y: number;
  count: number;
  minX: number;
  minY: number;
  maxX: number;
  maxY: number;
}

/**
 * Find the screen positions of the graph nodes on the canvas, by scanning
 * the rendered pixels for the type-palette color the page itself resolves
 * (`--node-1`: the color of the workspace's most numerous entity type).
 * Returns one blob per connected painted region, in CSS coordinates, plus
 * the canvas client rect so a spec can bound the fit check.
 */
export async function canvasNodeBlobs(
  page: Page,
  colorVar = "--node-1",
): Promise<{ blobs: Blob[]; rect: { left: number; top: number; width: number; height: number } }> {
  const result = await page.evaluate(({ colorVar: varName }) => {
    const canvas = document.querySelector<HTMLCanvasElement>("canvas.g-canvas");
    if (!canvas) throw new Error("Graph canvas not found");
    const ctx = canvas.getContext("2d");
    if (!ctx) throw new Error("Graph canvas context unavailable");

    let color = getComputedStyle(document.documentElement).getPropertyValue(varName).trim();
    const match = /^#([0-9a-f]{6})$/i.exec(color);
    if (match) {
      const value = parseInt(match[1], 16);
      color = `${(value >> 16) & 0xff},${(value >> 8) & 0xff},${value & 0xff}`;
    }
    const parts = color.split(",").map((part) => Number(part.trim()));
    if (parts.length !== 3 || parts.some((part) => Number.isNaN(part))) {
      throw new Error(`Unparsable node color for ${varName}: ${color}`);
    }
    const [red, green, blue] = parts;

    const width = canvas.width;
    const height = canvas.height;
    const data = ctx.getImageData(0, 0, width, height).data;

    // A 4x downsampled occupancy grid of near-color pixels.
    const step = 4;
    const gw = Math.ceil(width / step);
    const gh = Math.ceil(height / step);
    const grid = new Uint8Array(gw * gh);
    for (let y = 0; y < height; y += step) {
      for (let x = 0; x < width; x += step) {
        const offset = (y * width + x) * 4;
        const dr = data[offset] - red;
        const dg = data[offset + 1] - green;
        const db = data[offset + 2] - blue;
        if (data[offset + 3] > 100 && dr * dr + dg * dg + db * db <= 900) {
          grid[Math.floor(y / step) * gw + Math.floor(x / step)] = 1;
        }
      }
    }

    // Connected components on the grid (4-neighbour).
    const seen = new Uint8Array(gw * gh);
    const found: { x: number; y: number; count: number; minX: number; minY: number; maxX: number; maxY: number }[] = [];
    const stack: number[] = [];
    for (let index = 0; index < gw * gh; index++) {
      if (!grid[index] || seen[index]) continue;
      seen[index] = 1;
      stack.push(index);
      let cells = 0;
      let sumX = 0;
      let sumY = 0;
      let minX = gw;
      let minY = gh;
      let maxX = 0;
      let maxY = 0;
      while (stack.length > 0) {
        const current = stack.pop() as number;
        cells += 1;
        const cx = current % gw;
        const cy = Math.floor(current / gw);
        sumX += cx;
        sumY += cy;
        if (cx < minX) minX = cx;
        if (cx > maxX) maxX = cx;
        if (cy < minY) minY = cy;
        if (cy > maxY) maxY = cy;
        for (const neighbor of [current - 1, current + 1, current - gw, current + gw]) {
          if (neighbor >= 0 && neighbor < gw * gh && grid[neighbor] && !seen[neighbor]) {
            seen[neighbor] = 1;
            stack.push(neighbor);
          }
        }
      }
      found.push({
        x: (sumX / cells + 0.5) * step,
        y: (sumY / cells + 0.5) * step,
        count: cells * step * step,
        minX: minX * step,
        minY: minY * step,
        maxX: (maxX + 1) * step,
        maxY: (maxY + 1) * step,
      });
    }

    const rect = canvas.getBoundingClientRect();
    const dpr = rect.width > 0 ? width / rect.width : 1;
    return { found, rect: { left: rect.left, top: rect.top, width: rect.width, height: rect.height }, dpr };
  }, { colorVar });

  // Each occupancy cell covers 4x4 backing pixels; keep blobs of 3+ cells.
  const blobs = result.found
    .filter((blob) => blob.count >= 48)
    .map((blob) => ({
      x: result.rect.left + blob.x / result.dpr,
      y: result.rect.top + blob.y / result.dpr,
      count: blob.count,
      minX: result.rect.left + blob.minX / result.dpr,
      minY: result.rect.top + blob.minY / result.dpr,
      maxX: result.rect.left + blob.maxX / result.dpr,
      maxY: result.rect.top + blob.maxY / result.dpr,
    }));
  return { blobs, rect: result.rect };
}

/**
 * Wait until the force layout settles: two consecutive scans, 600 ms apart,
 * report the same blob count and centers within 2 px. Returns the settled
 * blobs.
 */
export async function waitForBlobSettle(
  page: Page,
  colorVar = "--node-1",
): Promise<{ blobs: Blob[]; rect: { left: number; top: number; width: number; height: number } }> {
  const canvas = page.locator("canvas.g-canvas");
  await canvas.waitFor({ state: "visible" });
  let previous = await canvasNodeBlobs(page, colorVar);
  let settled = false;
  let attempts = 0;
  while (!settled && attempts < 20) {
    await page.waitForTimeout(600);
    const current = await canvasNodeBlobs(page, colorVar);
    const sameCount = current.blobs.length === previous.blobs.length;
    const samePlaces = current.blobs.every((blob, index) => {
      const before = previous.blobs[index];
      return before != null && Math.abs(blob.x - before.x) < 3 && Math.abs(blob.y - before.y) < 3;
    });
    if (sameCount && samePlaces) {
      settled = true;
      return current;
    }
    previous = current;
    attempts += 1;
  }
  return previous;
}

/**
 * A minimal, real two-page PDF. Poppler (pdfinfo, pdftotext, pdftoppm) must
 * parse it, so the xref offsets are computed from the assembled bytes.
 */
export function twoPagePdf(): Buffer {
  const streamOne = "BT /F1 24 Tf 72 720 Td (E2E PDF page one) Tj ET\n";
  const streamTwo = "BT /F1 24 Tf 72 720 Td (E2E PDF page two) Tj ET\n";
  const objectBodies = [
    "<< /Type /Catalog /Pages 2 0 R >>",
    "<< /Type /Pages /Kids [3 0 R 5 0 R] /Count 2 >>",
    "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 7 0 R >> >> /Contents 4 0 R >>",
    `<< /Length ${Buffer.byteLength(streamOne)} >>\nstream\n${streamOne}endstream`,
    "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 7 0 R >> >> /Contents 6 0 R >>",
    `<< /Length ${Buffer.byteLength(streamTwo)} >>\nstream\n${streamTwo}endstream`,
    "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
  ];
  let output = "%PDF-1.4\n";
  const offsets: number[] = [];
  objectBodies.forEach((body, index) => {
    offsets.push(Buffer.byteLength(output));
    output += `${index + 1} 0 obj\n${body}\nendobj\n`;
  });
  const xrefStart = Buffer.byteLength(output);
  output += `xref\n0 ${objectBodies.length + 1}\n0000000000 65535 f \n`;
  for (const offset of offsets) {
    output += `${String(offset).padStart(10, "0")} 00000 n \n`;
  }
  output += `trailer\n<< /Size ${objectBodies.length + 1} /Root 1 0 R >>\nstartxref\n${xrefStart}\n%%EOF\n`;
  return Buffer.from(output, "utf8");
}

export const TEXT_FIXTURE = "E2E files text line one.\nE2E files text line two.\n";