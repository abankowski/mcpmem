import { z } from "zod";
import { oauthUrl, pageUrl, PUBLIC_BASE } from "./urls";

export type Audience = "graph" | "admin";

const clients = {
  graph: {
    id: "mcpmem-graph-ui",
    tokenKey: "mcpmem_graph_access",
    verifierKey: "mcpmem_graph_verifier",
    stateKey: "mcpmem_graph_state",
    returnKey: "mcpmem_graph_return",
    redirect: "graph",
  },
  admin: {
    id: "mcpmem-admin-ui",
    tokenKey: "mcpmem_admin_access",
    verifierKey: "mcpmem_admin_verifier",
    stateKey: "mcpmem_admin_state",
    returnKey: "mcpmem_admin_return",
    redirect: "adminCallback",
  },
} as const;

const tokenResponse = z.object({ access_token: z.string().min(1) });
const oauthAvailable: Record<Audience, boolean> = { graph: false, admin: false };
let graphScopes = new Set<string>();

function base64url(bytes: Uint8Array): string {
  let raw = "";
  for (const byte of bytes) raw += String.fromCharCode(byte);
  return btoa(raw).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

function verifier(): string {
  return base64url(crypto.getRandomValues(new Uint8Array(32)));
}

export function captureHashToken(): boolean {
  const fragment = location.hash.slice(1);
  const match = /(?:^|&)token=([^&]*)/.exec(fragment);
  if (!match) return false;
  // URLSearchParams changes a literal '+' to a space. The old fragment
  // handoff accepted raw '+' in a token, so decode percent escapes only.
  let token: string;
  try { token = decodeURIComponent(match[1]); } catch { token = match[1]; }
  const rest = fragment.split("&").filter((part) => !part.startsWith("token=")).join("&");
  history.replaceState(history.state, "", location.pathname + location.search + (rest ? `#${rest}` : ""));
  if (!token) return false;
  setStaticToken(token);
  return true;
}

export function setStaticToken(token: string): void {
  sessionStorage.setItem("mcpmem_token", token.trim());
  sessionStorage.removeItem(clients.graph.tokenKey);
  graphScopes = new Set();
}

export function getToken(audience: Audience): string | null {
  return sessionStorage.getItem(clients[audience].tokenKey) ||
    (audience === "graph" ? sessionStorage.getItem("mcpmem_token") : null);
}

export function clearToken(audience: Audience): void {
  sessionStorage.removeItem(clients[audience].tokenKey);
  if (audience === "graph") {
    sessionStorage.removeItem("mcpmem_token");
    graphScopes = new Set();
  }
}

export function noteChallenge(audience: Audience, challenge: string | null): void {
  if (challenge?.includes("resource_metadata=")) oauthAvailable[audience] = true;
}

export function canAuthorize(audience: Audience): boolean {
  return oauthAvailable[audience];
}

export function setGraphScopes(scopes: readonly string[]): void {
  graphScopes = new Set(scopes);
}

export async function beginAuth(audience: Audience, scopes?: readonly string[]): Promise<boolean> {
  if (!canAuthorize(audience)) return false;
  const client = clients[audience];
  const secret = verifier();
  const state = verifier();
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(secret));
  sessionStorage.setItem(client.verifierKey, secret);
  sessionStorage.setItem(client.stateKey, state);
  sessionStorage.setItem(client.returnKey, location.pathname + location.search);
  const url = oauthUrl("authorize");
  url.search = new URLSearchParams({
    response_type: "code",
    client_id: client.id,
    redirect_uri: pageUrl(client.redirect).href,
    scope: audience === "admin" ? "admin" : [...new Set(["graph-read", ...(scopes ?? [])])].join(" "),
    state,
    code_challenge_method: "S256",
    code_challenge: base64url(new Uint8Array(digest)),
  }).toString();
  location.assign(url.href);
  return true;
}

// A static bearer cannot acquire OAuth consent. Return false without claiming
// the scope was granted when the authorization server is not advertised.
export async function requestConsent(scopes: readonly string[]): Promise<boolean> {
  if (getToken("graph") && scopes.every((scope) => graphScopes.has(scope))) return true;
  if (!canAuthorize("graph")) return false;
  await beginAuth("graph", [...graphScopes, ...scopes]);
  return false;
}

function restorePage(audience: Audience): void {
  const client = clients[audience];
  const stored = sessionStorage.getItem(client.returnKey);
  sessionStorage.removeItem(client.returnKey);
  const fallback = pageUrl(audience === "admin" ? "admin" : "graph");
  const basePath = new URL(PUBLIC_BASE).pathname;
  const target = stored ? new URL(stored, location.origin) : fallback;
  const safe = target.origin === location.origin &&
    (target.pathname === basePath.slice(0, -1) || target.pathname.startsWith(basePath)) &&
    !target.pathname.startsWith(basePath + "api/");
  history.replaceState(history.state, "", safe ? target.pathname + target.search : fallback.pathname);
}

export async function completeAuthCallback(): Promise<string | null> {
  const audience: Audience = location.pathname === pageUrl("adminCallback").pathname ? "admin" : "graph";
  const params = new URLSearchParams(location.search);
  if (!params.has("code") && !params.has("error")) {
    if (audience === "admin") restorePage(audience);
    return null;
  }
  const code = params.get("code");
  const state = params.get("state");
  const error = params.get("error");
  const client = clients[audience];
  const secret = sessionStorage.getItem(client.verifierKey);
  const expected = sessionStorage.getItem(client.stateKey);
  sessionStorage.removeItem(client.verifierKey);
  sessionStorage.removeItem(client.stateKey);
  // A code is single-use; remove it from the address bar before the network call.
  restorePage(audience);
  oauthAvailable[audience] = true;
  if (error) return "Authorization was denied or could not be completed.";
  if (!code || !secret || !state || state !== expected) return "The sign-in response does not match this request.";
  let response: Response;
  try {
    response = await fetch(oauthUrl("token"), {
      method: "POST",
      headers: { "Content-Type": "application/x-www-form-urlencoded" },
      body: new URLSearchParams({
        grant_type: "authorization_code",
        code,
        redirect_uri: pageUrl(client.redirect).href,
        client_id: client.id,
        code_verifier: secret,
      }),
    });
  } catch {
    return "The token exchange could not be completed. Sign in again.";
  }
  if (!response.ok) return "The token exchange failed. Sign in again.";
  const payload = tokenResponse.safeParse(await response.json().catch(() => null));
  if (!payload.success) return "The token response was invalid. Sign in again.";
  sessionStorage.setItem(client.tokenKey, payload.data.access_token);
  oauthAvailable[audience] = true;
  if (audience === "graph") sessionStorage.removeItem("mcpmem_token");
  return null;
}
