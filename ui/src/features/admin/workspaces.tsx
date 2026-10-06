import { useEffect, useRef, useState, type FormEvent } from "react";
import { Check, Plus } from "lucide-react";
import { api, ApiError } from "../../lib/api";
import { requestConsent } from "../../lib/auth";
import { loadWorkspaces } from "../../lib/workspaces";
import { Button } from "../../components/Button";
import { Sheet } from "../../components/Sheet";
import { Tag } from "../../components/Tag";
import { useToast } from "../../components/Toast";
import type { Workspace } from "../../lib/schemas";
import { formatError, type AdminPaneProps } from "./page";

const EMPTY_ROWS: readonly Workspace[] = [];

/**
 * Workspaces the caller can access. Creation needs the graph-write scope;
 * the visibility toggle and the grants pane need the owner role. Only the
 * rows the server returns appear: no invented dates, sizes, or clocks.
 */
export function WorkspacesPane({ session, onCountChange }: AdminPaneProps) {
  const notify = useToast();
  const [rows, setRows] = useState<readonly Workspace[]>(EMPTY_ROWS);
  const [loading, setLoading] = useState(true);
  const [loadError, setLoadError] = useState<ApiError | null>(null);
  const [reloadKey, setReloadKey] = useState(0);
  const [creating, setCreating] = useState(false);
  const [name, setName] = useState("");
  const [createError, setCreateError] = useState<ApiError | null>(null);
  const [busy, setBusy] = useState(false);
  const [togglingId, setTogglingId] = useState<string | null>(null);

  const canWrite = session?.scopes.includes("graph-write") ?? false;
  const countHandler = useRef(onCountChange);
  useEffect(() => { countHandler.current = onCountChange; });

  useEffect(() => {
    const controller = new AbortController();
    let active = true;
    setLoading(true);
    setLoadError(null);
    setRows(EMPTY_ROWS);
    (async () => {
      const all: Workspace[] = [];
      let cursor: string | null = null;
      try {
        do {
          const page = await api.workspaces(cursor ?? undefined, undefined, controller.signal);
          if (!active) return;
          all.push(...page.workspaces);
          cursor = page.nextCursor;
        } while (cursor !== null);
        setRows(all);
        countHandler.current?.(all.length);
      } catch (cause) {
        if (!active || controller.signal.aborted) return;
        setLoadError(cause instanceof ApiError ? cause : new ApiError(0, "network_error", "The workspace list could not be loaded."));
      } finally {
        if (active) setLoading(false);
      }
    })();
    return () => { active = false; controller.abort(); };
  }, [reloadKey]);

  async function createWorkspace(): Promise<void> {
    if (!name.trim()) return;
    // The graph session may hold graph-read only; the consent flow asks for
    // graph-write and returns here on the fresh session.
    if (!(await requestConsent(["graph-write"]))) return;
    setBusy(true);
    setCreateError(null);
    try {
      await api.createWorkspace(name.trim());
      setCreating(false);
      setName("");
      setReloadKey((key) => key + 1);
      // The top-bar switcher keeps its own validated cache. Refresh it so a
      // new workspace appears there without a full reload.
      void loadWorkspaces(true).catch(() => undefined);
      notify("success", "Workspace created.");
    } catch (cause) {
      // A failed mutation keeps the sheet open and the input intact.
      setCreateError(cause instanceof ApiError ? cause : new ApiError(0, "network_error", "The workspace could not be created."));
    } finally {
      setBusy(false);
    }
  }

  function submitCreate(event: FormEvent<HTMLFormElement>): void {
    event.preventDefault();
    void createWorkspace();
  }

  async function toggleVisibility(row: Workspace): Promise<void> {
    if (!(await requestConsent(["graph-write"]))) return;
    const target = row.visibility === "private" ? "public" : "private";
    setTogglingId(row.workspaceId);
    try {
      const result = await api.setWorkspaceVisibility(row.workspaceId, target);
      setRows((previous) => previous.map((item) => item.workspaceId === result.workspace.workspaceId ? result.workspace : item));
      notify("success", `Workspace is now ${target}.`);
    } catch (cause) {
      const error = cause instanceof ApiError ? cause : new ApiError(0, "network_error", "The visibility could not be changed.");
      notify("error", formatError(error));
    } finally {
      setTogglingId(null);
    }
  }

  return (
    <>
      <header className="ui-admin__head">
        <div className="ui-admin__head-copy">
          <h1>Workspaces</h1>
          <p>The workspaces this identity can access. Ownership controls visibility and grants.</p>
        </div>
        <div className="ui-admin__head-actions">
          {canWrite && (
            <Button variant="primary" onClick={() => setCreating(true)}>
              <Plus size={16} aria-hidden="true" />Create workspace
            </Button>
          )}
        </div>
      </header>

      {!canWrite && (
        <p className="ui-admin-warn">
          Creating workspaces needs the graph-write scope.{" "}
          <Button variant="ghost" size="sm" onClick={() => { void requestConsent(["graph-write"]); }}>Grant graph-write</Button>
        </p>
      )}
      {loadError && (
        <div className="ui-admin-error" role="alert">
          <span>{formatError(loadError)}</span>
          <Button size="sm" variant="ghost" onClick={() => setReloadKey((key) => key + 1)}>Retry</Button>
        </div>
      )}
      {!loadError && (
        <div className="ui-admin-table-wrap">
          <table className="ui-admin-table">
            <thead>
              <tr>
                <th>Workspace ID</th>
                <th>Name</th>
                <th>Visibility</th>
                <th>Role</th>
                <th>Default</th>
                <th className="ui-admin-cell-actions">Actions</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((row) => (
                <tr key={row.workspaceId}>
                  <td className="ui-admin-mono ui-admin-cell">{row.workspaceId}</td>
                  <td className="ui-admin-cell">{row.name}</td>
                  <td>
                    <Tag tone={row.visibility === "public" ? "warn" : "neutral"}>{row.visibility}</Tag>
                  </td>
                  <td><Tag>{row.role}</Tag></td>
                  <td>
                    {row.isDefault
                      ? <Check size={16} aria-label="default" style={{ color: "var(--green-deep)" }} />
                      : <span aria-label="not default" className="ui-admin-mono--muted ui-admin-mono">-</span>}
                  </td>
                  <td className="ui-admin-cell-actions">
                    {row.role === "owner" && (
                      <Button size="sm" variant="ghost"
                        disabled={!canWrite || togglingId === row.workspaceId}
                        title={canWrite ? undefined : "Visibility changes need the graph-write scope."}
                        onClick={() => void toggleVisibility(row)}>
                        {row.visibility === "private" ? "Make public" : "Make private"}
                      </Button>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
          {!loading && rows.length === 0 && (
            <p className="ui-admin-state">No accessible workspaces.</p>
          )}
          {loading && <p className="ui-admin-state">Loading workspaces…</p>}
        </div>
      )}

      <Sheet open={creating} title="Create workspace" onClose={() => setCreating(false)}
        footer={<>
          <Button onClick={() => setCreating(false)} disabled={busy}>Cancel</Button>
          <Button variant="primary" onClick={() => void createWorkspace()} disabled={busy || !name.trim()}>
            {busy ? "Creating…" : "Create workspace"}
          </Button>
        </>}>
        <form onSubmit={submitCreate}>
          <div className="ui-admin-field">
            <label htmlFor="workspace-name">Name</label>
            <input id="workspace-name" className="ui-admin-input" type="text"
              value={name} autoComplete="off" placeholder="e.g. second-brain"
              onChange={(event) => setName(event.target.value)} />
            <span className="ui-admin-hint">The new workspace is private and owned by you.</span>
          </div>
          {createError && <p className="ui-admin-error" role="alert">{formatError(createError)}</p>}
        </form>
      </Sheet>
    </>
  );
}
