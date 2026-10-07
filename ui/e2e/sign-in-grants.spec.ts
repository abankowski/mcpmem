// The sign-in grant paths: what the UI asks the backend for, and what happens
// to the tokens it receives. Regression net for the grant geometry that left
// the admin sign-in holding only `admin` and every other feature needing a
// second consent round (2026-10-06): each audience must ask for every scope
// the server advertises (plus its own), the consent page then offers the
// intersection with what the human holds, and each audience's token must live
// in its own sessionStorage slot and be the one its API calls carry.
//
// The OAuth provider is stubbed with Playwright route interception: the
// authorize request is captured and answered with a redirect, the token
// endpoint answers with a fake access token. The server never sees the
// authorize request, so no client registration or OIDC provider is needed.
// The consent page itself (the tick boxes) is server-rendered from a login
// row only a real provider sign-in creates; its offerings are covered by the
// Rust unit tests in crates/mcpmem-oauth (consent::offered/approve).

import { expect, test, type Page } from "@playwright/test";
import { ensureServer, seedWorkspace, SERVER_ORIGIN, TEST_BEARER, type WorkspaceShape } from "./helpers";

test.describe.configure({ mode: "serial" });

let teardownServer: (() => void) | undefined;
let advertised: string[];
let ws: WorkspaceShape;

test.beforeAll(async () => {
  teardownServer = await ensureServer();
  ws = await seedWorkspace(`e2e-grants-${Date.now()}`, {
    entities: [{ name: "GrantTarget", entityType: "note" }],
  });
  // The resource advertises its scope list in the WWW-Authenticate challenge
  // of an unauthenticated /ui/api/session call. Read the actual list the test
  // server serves rather than hardcoding a feature-flag-dependent set.
  const response = await fetch("http://127.0.0.1:8080/ui/api/session");
  const challenge = response.headers.get("WWW-Authenticate") ?? "";
  const match = /scope="([^"]*)"/.exec(challenge);
  expect(match, `session 401 must advertise scopes in ${challenge}`).not.toBeNull();
  advertised = match![1].split(/\s+/).filter(Boolean);
  expect(advertised.length).toBeGreaterThan(0);
});

test.afterAll(async () => {
  teardownServer?.();
});

/**
 * Intercept the authorize navigation, assert the scope the UI sent, and answer
 * it with a code redirect. With `validState` the exchange completes and the
 * token endpoint is stubbed too; without it the redirect carries a wrong state
 * so the UI lands on the sign-in-error state and the flow stops early.
 */
async function interceptAuthorize(
  page: Page,
  expectScopes: Set<string>,
  opts: {
    validState?: boolean;
    tokenValue?: string;
  },
): Promise<void> {
  await page.route("**/oauth/authorize**", async (route) => {
    const url = new URL(route.request().url());
    const sent = new Set((url.searchParams.get("scope") ?? "").split(/\s+/).filter(Boolean));
    expect(sent).toEqual(expectScopes);
    const state = opts.validState ? url.searchParams.get("state") : "e2e-stale-state";
    // The UI names its own callback in redirect_uri; redirect there so the
    // real callback handler runs.
    const redirect = url.searchParams.get("redirect_uri");
    expect(redirect).not.toBeNull();
    const location = `${redirect}?code=e2e-fake-code&state=${encodeURIComponent(state ?? "")}`;
    await route.fulfill({ status: 302, headers: { location } });
  });
  if (opts.validState) {
    await page.route(
      (url) => url.pathname === "/oauth/token",
      async (route) => {
        await route.fulfill({
          status: 200,
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ access_token: opts.tokenValue ?? "e2e-token" }),
        });
      },
    );
  }
}

test("the graph sign-in asks the backend for every advertised scope", async ({ page }) => {
  await page.goto(`${SERVER_ORIGIN}/ui#token=invalid-e2e-bearer`);
  const signIn = page.getByRole("button", { name: "Sign in" });
  await expect(signIn).toBeVisible();
  await interceptAuthorize(page, new Set(["graph-read", ...advertised]), {});
  await signIn.click();
  await expect(page.getByText("The sign-in response does not match")).toBeVisible();
});

test("the admin sign-in asks the backend for every advertised scope plus admin", async ({ page }) => {
  await page.goto(`${SERVER_ORIGIN}/ui/admin`);
  const signIn = page.getByRole("button", { name: "Sign in as admin" });
  await expect(signIn).toBeVisible();
  const expected = new Set([...advertised, "admin"]);
  await interceptAuthorize(page, expected, {});
  await signIn.click();
  await expect(page.getByText("The sign-in response does not match")).toBeVisible();
});

test("a graph sign-in replaces the invalid static token with a valid token", async ({ page }) => {
  await page.goto(`${SERVER_ORIGIN}/ui#token=invalid-e2e-bearer`);
  const graphSignIn = page.getByRole("button", { name: "Sign in" });
  await expect(graphSignIn).toBeVisible();
  await interceptAuthorize(page, new Set(["graph-read", ...advertised]), {
    validState: true,
    tokenValue: TEST_BEARER,
  });
  await graphSignIn.click();
  await expect
    .poll(() => page.evaluate(() => sessionStorage.getItem("mcpmem_graph_access")))
    .toBe(TEST_BEARER);
  expect(await page.evaluate(() => sessionStorage.getItem("mcpmem_token"))).toBeNull();
});

test("admin API calls carry the admin token after sign-in", async ({ page }) => {
  await page.goto(`${SERVER_ORIGIN}/ui/admin`);
  const signIn = page.getByRole("button", { name: "Sign in as admin" });
  await expect(signIn).toBeVisible();
  await interceptAuthorize(
    page,
    new Set([...advertised, "admin"]),
    { validState: true, tokenValue: "e2e-admin-token" },
  );
  const seen: string[] = [];
  await page.route("**/ui/api/**", async (route) => {
    seen.push(route.request().headers()["authorization"] ?? "");
    await route.continue();
  });
  await signIn.click();
  // After the callback the app reloads and refetches session and workspaces;
  // those requests must carry the admin bearer.
  await expect
    .poll(() => seen.some((header) => header === "Bearer e2e-admin-token"))
    .toBe(true);
});