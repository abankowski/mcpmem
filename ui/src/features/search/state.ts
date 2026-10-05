// The Search screen's URL state. Every query and control value lives in the
// URL (`q`, `mode`, `scope`, `type`, `from`, `to`, `relationType`, `k`), so a
// search survives page reloads and workspace switches: the shell remounts
// this page per workspace and readParams re-reads the URL on each mount.
// The known keys are rewritten in place; unrelated parameters such as
// `workspaceId` stay untouched.

export type SearchMode = "direct" | "semantic" | "hybrid";
export type SearchScope = "nodes" | "relations";
export const K_OPTIONS = [10, 20, 50] as const;
export type SearchK = (typeof K_OPTIONS)[number];

export interface SearchParams {
  q: string;
  mode: SearchMode;
  scope: SearchScope;
  /** Entity type filter in nodes scope. */
  type: string;
  /** Relation endpoint filters; the server honors them in relations scope. */
  from: string;
  to: string;
  /** Relation type filter in relations scope. */
  relationType: string;
  k: SearchK;
}

const PARAM_KEYS: readonly string[] = ["q", "mode", "scope", "type", "from", "to", "relationType", "k"];
const MODES: readonly string[] = ["direct", "semantic", "hybrid"];
const SCOPES: readonly string[] = ["nodes", "relations"];

/** Parse the search state out of a query string, with the server defaults. */
export function readParams(search: string): SearchParams {
  const params = new URLSearchParams(search);
  const mode = params.get("mode");
  const scope = params.get("scope");
  const k = params.get("k");
  return {
    q: params.get("q") ?? "",
    mode: MODES.includes(mode ?? "") ? (mode as SearchMode) : "direct",
    scope: SCOPES.includes(scope ?? "") ? (scope as SearchScope) : "nodes",
    type: params.get("type") ?? "",
    from: params.get("from") ?? "",
    to: params.get("to") ?? "",
    relationType: params.get("relationType") ?? "",
    k: k === "20" ? 20 : k === "50" ? 50 : 10,
  };
}

/**
 * The canonical query string for one state, shared by the URL writer and the
 * change-detection comparisons in the page.
 */
export function serializeParams(params: SearchParams): string {
  const query = new URLSearchParams();
  if (params.q) query.set("q", params.q);
  query.set("mode", params.mode);
  query.set("scope", params.scope);
  if (params.type) query.set("type", params.type);
  if (params.from) query.set("from", params.from);
  if (params.to) query.set("to", params.to);
  if (params.relationType) query.set("relationType", params.relationType);
  query.set("k", String(params.k));
  return query.toString();
}

/**
 * Write the state into the current URL's query string, keeping every
 * unrelated parameter. `replace` uses replaceState (control changes), false
 * uses pushState (a committed query, so Back walks the searches). A no-op
 * when the URL already carries the same state.
 */
export function applyParams(params: SearchParams, replace: boolean): void {
  const url = new URL(location.href);
  const kept = new URLSearchParams();
  for (const [key, value] of url.searchParams) {
    if (!PARAM_KEYS.includes(key)) kept.append(key, value);
  }
  const next = new URLSearchParams(serializeParams(params));
  for (const [key, value] of kept) next.append(key, value);
  url.search = next.toString();
  if (url.href !== location.href) {
    if (replace) history.replaceState(history.state, "", url.href);
    else history.pushState(history.state, "", url.href);
  }
}