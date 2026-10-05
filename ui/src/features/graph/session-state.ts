// The Graph screen's shared session: the browse cursor, the selection,
// filters and pinned nodes (localStorage per workspace), and the abort
// controller that cancels stale page-level fetches when the workspace or
// the selection changes. React reads the persisted pieces through
// useSyncExternalStore; the canvas code never re-renders with React.

const listeners = new Set<() => void>();
let revision = 0;
let fetchController: AbortController | null = null;

/** One relation triple as the canvas, the rows, and the inspector address it. */
export interface EdgeRef {
  from: string;
  to: string;
  relationType: string;
}

/** The current inspector selection: a node or one exact relation triple. */
export type Selection =
  | { kind: "node"; name: string }
  | { kind: "relation"; from: string; to: string; relationType: string };

export function edgeKey(edge: EdgeRef): string {
  return `${edge.from}\u001f${edge.to}\u001f${edge.relationType}`;
}

export function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

export function snapshot(): number {
  return revision;
}

function emit(): void {
  revision += 1;
  for (const listener of listeners) listener();
}

/**
 * Start a fresh fetch generation: abort every in-flight page-level fetch
 * (browse, expand, types) and return the signal the next fetches use. Call
 * this when the workspace or the selection changes so a stale response can
 * never overwrite newer state.
 */
export function abortStale(): AbortSignal {
  fetchController?.abort();
  fetchController = new AbortController();
  return fetchController.signal;
}

/** The signal of the current fetch generation, if one exists. */
export function currentSignal(): AbortSignal | null {
  return fetchController?.signal ?? null;
}

// --- per-workspace persistence -------------------------------------------------

const PINNED_PREFIX = "mcpmem_graph_pinned_";
const FILTERS_PREFIX = "mcpmem_graph_filters_";

export interface GraphFilters {
  hiddenEntityTypes: readonly string[];
  hiddenRelationTypes: readonly string[];
}

const EMPTY_FILTERS: GraphFilters = { hiddenEntityTypes: [], hiddenRelationTypes: [] };

function readJson<T>(key: string): T | null {
  try {
    const raw = localStorage.getItem(key);
    if (raw == null) return null;
    return JSON.parse(raw) as T;
  } catch {
    return null;
  }
}

/** The pinned node names of one workspace, in pin order. */
export function pinnedFor(workspaceId: string): readonly string[] {
  const parsed = readJson<unknown[]>(PINNED_PREFIX + workspaceId);
  return Array.isArray(parsed) ? parsed.filter((item): item is string => typeof item === "string") : [];
}

export function isPinned(workspaceId: string, name: string): boolean {
  return pinnedFor(workspaceId).includes(name);
}

export function togglePinned(workspaceId: string, name: string): void {
  const current = pinnedFor(workspaceId);
  const next = current.includes(name) ? current.filter((item) => item !== name) : [...current, name];
  localStorage.setItem(PINNED_PREFIX + workspaceId, JSON.stringify(next));
  emit();
}

/** Replace the pinned list (used when the canvas reports a drag-pin). */
export function setPinned(workspaceId: string, names: readonly string[]): void {
  localStorage.setItem(PINNED_PREFIX + workspaceId, JSON.stringify([...new Set(names)]));
  emit();
}

/** The hidden type sets of one workspace. */
export function filtersFor(workspaceId: string): GraphFilters {
  const parsed = readJson<{ hiddenEntityTypes?: unknown; hiddenRelationTypes?: unknown }>(FILTERS_PREFIX + workspaceId);
  if (!parsed) return EMPTY_FILTERS;
  const entities = Array.isArray(parsed.hiddenEntityTypes)
    ? parsed.hiddenEntityTypes.filter((item): item is string => typeof item === "string")
    : [];
  const relations = Array.isArray(parsed.hiddenRelationTypes)
    ? parsed.hiddenRelationTypes.filter((item): item is string => typeof item === "string")
    : [];
  return { hiddenEntityTypes: entities, hiddenRelationTypes: relations };
}

export function setFilters(workspaceId: string, next: GraphFilters): void {
  localStorage.setItem(
    FILTERS_PREFIX + workspaceId,
    JSON.stringify({
      hiddenEntityTypes: [...next.hiddenEntityTypes],
      hiddenRelationTypes: [...next.hiddenRelationTypes],
    }),
  );
  emit();
}

/** Flip one type between shown and hidden. */
export function toggleHiddenType(workspaceId: string, kind: "entity" | "relation", type: string): void {
  const filters = filtersFor(workspaceId);
  const hidden = kind === "entity" ? filters.hiddenEntityTypes : filters.hiddenRelationTypes;
  const next = hidden.includes(type) ? hidden.filter((item) => item !== type) : [...hidden, type];
  setFilters(
    workspaceId,
    kind === "entity"
      ? { hiddenEntityTypes: next, hiddenRelationTypes: filters.hiddenRelationTypes }
      : { hiddenEntityTypes: filters.hiddenEntityTypes, hiddenRelationTypes: next },
  );
}

export function clearFilters(workspaceId: string): void {
  setFilters(workspaceId, EMPTY_FILTERS);
}

// --- initial selection from the URL -------------------------------------------

export interface InitialSelection {
  names: readonly string[];
  depth: 1 | 2 | 3;
  isolate: boolean;
}

/**
 * Read the `?node=` / `?nodes=&depth=&isolate=` state Search writes with
 * `selectInGraph`. Read it once per page mount; the page clears the
 * parameters afterwards so a workspace switch never re-seeds it.
 */
export function initialSelectionFromUrl(): InitialSelection | null {
  const params = new URLSearchParams(location.search);
  const node = params.get("node");
  if (node) return { names: [node], depth: 1, isolate: false };
  const nodesParam = params.get("nodes");
  if (!nodesParam) return null;
  const names = nodesParam
    .split(",")
    .map((item) => item.trim())
    .filter(Boolean);
  if (names.length === 0) return null;
  const rawDepth = Number(params.get("depth"));
  const depth: 1 | 2 | 3 = rawDepth === 2 || rawDepth === 3 ? rawDepth : 1;
  return { names, depth, isolate: params.get("isolate") === "1" };
}