import { z } from "zod";
import { api } from "./api";
import { getToken } from "./auth";
import { workspaceSchema, type Workspace } from "./schemas";

const cacheKey = "mcpmem_workspace_list";
const selectionKey = "mcpmem_workspace_selection";
const cachedPage = z.object({ fingerprint: z.string(), workspaces: z.array(workspaceSchema) });
const listeners = new Set<() => void>();
let activeToken: string | null = null;
let list: Workspace[] = [];
let selected: Workspace | null = null;

function syncSelection(): void {
  const wanted = sessionStorage.getItem(selectionKey);
  selected = wanted === null
    ? list.find((item) => item.isDefault) ?? null
    : list.find((item) => item.workspaceId === wanted) ?? null;
  if (selected) sessionStorage.setItem(selectionKey, selected.workspaceId);
  else if (wanted !== "") sessionStorage.removeItem(selectionKey);
  for (const listener of listeners) listener();
}

export function currentWorkspace(): Workspace | null {
  return selected;
}

export function onWorkspaceChange(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

export function selectWorkspace(workspaceId: string | null): void {
  if (workspaceId !== null && !list.some((item) => item.workspaceId === workspaceId)) {
    throw new Error("Select an accessible workspace.");
  }
  if (workspaceId === null) sessionStorage.setItem(selectionKey, "");
  else sessionStorage.setItem(selectionKey, workspaceId);
  selected = workspaceId ? list.find((item) => item.workspaceId === workspaceId) ?? null : null;
  for (const listener of listeners) listener();
}

export function invalidateWorkspaces(): void {
  sessionStorage.removeItem(cacheKey);
  activeToken = null;
  list = [];
  selected = null;
  for (const listener of listeners) listener();
}
async function tokenFingerprint(token: string): Promise<string | null> {
  // WebCrypto is unavailable on insecure non-local HTTP. Skip the persistent
  // cache there; a live workspace read still works with a static bearer.
  if (!crypto.subtle) return null;
  const bytes = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(token));
  return Array.from(new Uint8Array(bytes), (byte) => byte.toString(16).padStart(2, "0")).join("");
}


export async function loadWorkspaces(force = false, signal?: AbortSignal): Promise<readonly Workspace[]> {
  const token = getToken("graph");
  if (token && !force && token === activeToken) return list;
  let fingerprint: string | null = null;
  if (token) {
    try { fingerprint = await tokenFingerprint(token); } catch { fingerprint = null; }
  }
  if (signal?.aborted || token !== getToken("graph")) throw new DOMException("Workspace request changed.", "AbortError");
  if (fingerprint && !force) {
    try {
      const raw = sessionStorage.getItem(cacheKey);
      if (raw) {
        const parsed = cachedPage.safeParse(JSON.parse(raw));
        if (parsed.success && parsed.data.fingerprint === fingerprint) {
          activeToken = token;
          list = parsed.data.workspaces;
          syncSelection();
          return list;
        }
      }
    } catch {
      // Invalid session cache is not server data. Read the current list instead.
    }
  }
  const workspaces: Workspace[] = [];
  let cursor: string | null = null;
  do {
    const page = await api.workspaces(cursor ?? undefined, undefined, signal);
    workspaces.push(...page.workspaces);
    cursor = page.nextCursor;
  } while (cursor !== null);
  if (signal?.aborted || token !== getToken("graph")) throw new DOMException("Workspace request changed.", "AbortError");
  list = workspaces;
  activeToken = token;
  syncSelection();
  if (fingerprint) {
    try { sessionStorage.setItem(cacheKey, JSON.stringify({ fingerprint, workspaces })); } catch {
      // The cache is optional; keep the validated live list in memory.
    }
  }
  return list;
}
