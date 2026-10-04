// The built module lives under <public prefix>/ui/assets/. OAuth can be off;
// the module URL, not its configuration, identifies the public path.
export const PUBLIC_BASE = new URL(
  import.meta.env.DEV ? "/ui/" : "../",
  import.meta.url,
).href;

export type Page = "graph" | "search" | "admin" | "adminCallback";

const basePath = new URL(PUBLIC_BASE).pathname;
export const PAGE_PATHS: Record<Page, string> = {
  // The graph route has no trailing slash; OAuth matches it byte-for-byte.
  graph: basePath.slice(0, -1),
  search: basePath + "search",
  admin: basePath + "admin",
  adminCallback: basePath + "admin/callback",
};

export function pageUrl(page: Page): URL {
  return new URL(PAGE_PATHS[page], PUBLIC_BASE);
}

export function apiUrl(route: string, params?: Record<string, string | number | undefined>): URL {
  const url = new URL(`api/${route.replace(/^\/+/, "")}`, PUBLIC_BASE);
  if (url.searchParams.has("token") || url.searchParams.has("access_token")) {
    throw new Error("Pass bearer tokens in the Authorization header.");
  }
  for (const [key, value] of Object.entries(params ?? {})) {
    if (value !== undefined) {
      if (key === "token" || key === "access_token") throw new Error("Pass bearer tokens in the Authorization header.");
      url.searchParams.set(key, String(value));
    }
  }
  return url;
}

export function oauthUrl(route: "authorize" | "token"): URL {
  return new URL(`../oauth/${route}`, PUBLIC_BASE);
}
