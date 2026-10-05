import { useEffect, useRef, useState, type FormEvent } from "react";
import { z } from "zod";
import { CircleAlert, Plus } from "lucide-react";
import { api, ApiError } from "../../lib/api";
import { repoSchema, type RepoInput } from "../../lib/schemas";
import { formatTimestamp } from "../../lib/format";
import { Button } from "../../components/Button";
import { ConfirmDialog } from "../../components/ConfirmDialog";
import { Sheet } from "../../components/Sheet";
import { Tag } from "../../components/Tag";
import { useToast } from "../../components/Toast";
import { formatError, type AdminPaneProps } from "./page";

type Repo = z.infer<typeof repoSchema>;

const EMPTY_ROWS: readonly Repo[] = [];
const POLL_INTERVAL_MS = 2000;
const ACTIVE_STATES = ["pending", "cloning", "indexing", "removing"] as const;

const STATE_TONE: Record<Repo["state"], "neutral" | "ok" | "warn" | "error"> = {
  pending: "warn",
  cloning: "warn",
  indexing: "warn",
  indexed: "ok",
  error: "error",
  removing: "neutral",
};

interface RepoFormProps {
  busy: boolean;
  error: ApiError | null;
  onClose: () => void;
  onSubmit: (input: RepoInput) => void | Promise<void>;
}

function RepoForm({ busy, error, onClose, onSubmit }: RepoFormProps) {
  const [key, setKey] = useState("");
  const [url, setUrl] = useState("");
  const [authKind, setAuthKind] = useState<RepoInput["authKind"]>("none");
  const [authSecret, setAuthSecret] = useState("");
  const [snippets, setSnippets] = useState(false);

  function submit(event: FormEvent<HTMLFormElement>): void {
    event.preventDefault();
    void onSubmit({
      key: key.trim(),
      url: url.trim(),
      authKind,
      authSecret: authSecret.trim() || undefined,
      snippets,
    });
  }

  return (
    <Sheet open title="Add repository" onClose={onClose}
      footer={<>
        <Button onClick={onClose} disabled={busy}>Cancel</Button>
        <Button variant="primary" type="submit" form="repo-form" disabled={busy || !key.trim() || !url.trim()}>
          {busy ? "Saving…" : "Add repository"}
        </Button>
      </>}>
      <form id="repo-form" className="ui-admin-field" onSubmit={submit} aria-label="Add repository">
        <div className="ui-admin-field">
          <label htmlFor="repo-key">Key</label>
          <input id="repo-key" className="ui-admin-input" type="text" autoComplete="off"
            placeholder="unique name, e.g. mcp-memory"
            value={key} onChange={(event) => setKey(event.target.value)} />
        </div>
        <div className="ui-admin-field">
          <label htmlFor="repo-url">Clone URL</label>
          <input id="repo-url" className="ui-admin-input" type="text" autoComplete="off"
            value={url} onChange={(event) => setUrl(event.target.value)} />
        </div>
        <div className="ui-admin-field">
          <label htmlFor="repo-auth">Authentication</label>
          <select id="repo-auth" className="ui-admin-input" value={authKind}
            onChange={(event) => setAuthKind(event.target.value as RepoInput["authKind"])}>
            <option value="none">none</option>
            <option value="token">token</option>
            <option value="ssh">ssh</option>
          </select>
        </div>
        {authKind !== "none" && (
          <div className="ui-admin-field">
            <label htmlFor="repo-secret">{authKind === "token" ? "Token" : "SSH key path"}</label>
            <input id="repo-secret" className="ui-admin-input" type="password" autoComplete="off"
              value={authSecret} onChange={(event) => setAuthSecret(event.target.value)} />
          </div>
        )}
        <label className="ui-admin-scope" style={{ alignSelf: "flex-start" }}>
          <input type="checkbox" checked={snippets} onChange={(event) => setSnippets(event.target.checked)} />
          <span>
            <span className="ui-admin-scope__label">Index snippets</span>
            <br />
            <span className="ui-admin-scope__desc">store code snippets with the symbols</span>
          </span>
        </label>
        {error && <p className="ui-admin-error" role="alert">{formatError(error)}</p>}
      </form>
    </Sheet>
  );
}

/**
 * Managed repositories with their live job states. The list is the same
 * async-state machine the server reports: pending, cloning, indexing,
 * indexed, error, removing. While a job runs, the pane polls the list; it
 * never invents a transition.
 */
export function ReposPane({ adminSession, onCountChange }: AdminPaneProps) {
  const notify = useToast();
  const [rows, setRows] = useState<readonly Repo[]>(EMPTY_ROWS);
  const [loading, setLoading] = useState(true);
  const [loadError, setLoadError] = useState<ApiError | null>(null);
  const [pollError, setPollError] = useState<ApiError | null>(null);
  const [reloadKey, setReloadKey] = useState(0);
  const [creating, setCreating] = useState(false);
  const [formError, setFormError] = useState<ApiError | null>(null);
  const [busy, setBusy] = useState(false);
  const [reindexingKey, setReindexingKey] = useState<string | null>(null);
  const [removeTarget, setRemoveTarget] = useState<Repo | null>(null);

  const countHandler = useRef(onCountChange);
  useEffect(() => { countHandler.current = onCountChange; });

  const canAdmin = adminSession?.scopes.includes("admin") ?? false;

  async function loadRows(signal: AbortSignal, quiet: boolean): Promise<void> {
    if (!quiet) {
      setLoading(true);
      setLoadError(null);
    }
    try {
      const result = await api.repos(signal);
      if (signal.aborted) return;
      setRows(result.repos);
      setPollError(null);
      countHandler.current?.(result.repos.length);
    } catch (cause) {
      if (signal.aborted) return;
      const error = cause instanceof ApiError ? cause : new ApiError(0, "network_error", "The repository list could not be loaded.");
      if (quiet) setPollError(error);
      else setLoadError(error);
    } finally {
      if (!quiet) setLoading(false);
    }
  }

  useEffect(() => {
    const controller = new AbortController();
    void loadRows(controller.signal, false);
    return () => controller.abort();
  }, [reloadKey]);

  // Poll while any job is in flight, like the files tab polls extraction.
  const hasActiveJob = rows.some((row) => (ACTIVE_STATES as readonly string[]).includes(row.state));
  useEffect(() => {
    if (!hasActiveJob) return;
    const controller = new AbortController();
    const timer = window.setInterval(() => void loadRows(controller.signal, true), POLL_INTERVAL_MS);
    return () => { window.clearInterval(timer); controller.abort(); };
  }, [hasActiveJob, reloadKey]);

  if (!canAdmin) {
    return (
      <div className="ui-admin-unavailable" role="status">
        <CircleAlert size={28} aria-hidden="true" />
        <strong>Admin scope required</strong>
        <span>Code repositories render only with a human token that holds the admin scope.</span>
      </div>
    );
  }

  async function addRepo(input: RepoInput): Promise<void> {
    setBusy(true);
    setFormError(null);
    try {
      await api.createRepo(input);
      setCreating(false);
      setReloadKey((key) => key + 1);
      notify("success", "Repository accepted for indexing.");
    } catch (cause) {
      // A failed mutation keeps the sheet open and the form input.
      setFormError(cause instanceof ApiError ? cause : new ApiError(0, "network_error", "The repository could not be added."));
    } finally {
      setBusy(false);
    }
  }

  async function reindex(row: Repo): Promise<void> {
    setReindexingKey(row.key);
    try {
      await api.reindexRepo(row.key);
      setReloadKey((key) => key + 1);
      notify("success", "Reindex accepted.");
    } catch (cause) {
      const error = cause instanceof ApiError ? cause : new ApiError(0, "network_error", "The reindex could not be scheduled.");
      notify("error", formatError(error));
    } finally {
      setReindexingKey(null);
    }
  }

  async function removeRepo(): Promise<void> {
    if (!removeTarget) return;
    try {
      await api.removeRepo(removeTarget.key);
      setRemoveTarget(null);
      setReloadKey((key) => key + 1);
      notify("success", "Repository removal scheduled.");
    } catch (cause) {
      const error = cause instanceof ApiError ? cause : new ApiError(0, "network_error", "The repository could not be removed.");
      throw new Error(formatError(error));
    }
  }

  return (
    <>
      <header className="ui-admin__head">
        <div className="ui-admin__head-copy">
          <h1>Code repositories</h1>
          <p>Managed repositories, cloned and indexed by this server.</p>
        </div>
        <div className="ui-admin__head-actions">
          <Button variant="primary" onClick={() => { setFormError(null); setCreating(true); }}>
            <Plus size={16} aria-hidden="true" />Add repository
          </Button>
        </div>
      </header>

      {loadError && (
        <div className="ui-admin-error" role="alert">
          <span>{formatError(loadError)}</span>
          <Button size="sm" variant="ghost" onClick={() => setReloadKey((key) => key + 1)}>Retry</Button>
        </div>
      )}
      {pollError && <p className="ui-admin-error" role="alert">{formatError(pollError)}</p>}
      {!loadError && (
        <div className="ui-admin-table-wrap">
          <table className="ui-admin-table">
            <thead>
              <tr>
                <th>Key</th>
                <th>URL</th>
                <th>Auth</th>
                <th>Snippets</th>
                <th>State</th>
                <th>Last indexed</th>
                <th className="ui-admin-cell-actions">Actions</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((row) => {
                const reindexable = row.state === "indexed" || row.state === "error";
                return (
                  <tr key={row.key}>
                    <td className="ui-admin-mono ui-admin-cell">{row.key}</td>
                    <td className="ui-admin-mono ui-admin-cell" title={row.url}>{row.url}</td>
                    <td><Tag>{row.authKind}</Tag></td>
                    <td>{row.snippets ? "yes" : "no"}</td>
                    <td><Tag tone={STATE_TONE[row.state]}>{row.state}</Tag></td>
                    <td className="ui-admin-mono ui-admin-mono--muted">
                      {row.lastIndexedUs != null ? formatTimestamp(row.lastIndexedUs) : "-"}
                    </td>
                    <td className="ui-admin-cell-actions">
                      <span className="ui-admin-row-actions">
                        <Button size="sm" variant="ghost" disabled={!reindexable || reindexingKey === row.key}
                          onClick={() => void reindex(row)}>
                          {reindexingKey === row.key ? "Scheduling…" : "Reindex"}
                        </Button>
                        <Button size="sm" variant="ghost" disabled={reindexingKey != null} onClick={() => setRemoveTarget(row)}>
                          Remove
                        </Button>
                      </span>
                      {row.state === "error" && row.lastError && (
                        <p className="ui-admin-test ui-admin-test--fail">{row.lastError}</p>
                      )}
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
          {loading && <p className="ui-admin-state">Loading repositories…</p>}
          {!loading && rows.length === 0 && <p className="ui-admin-state">No repositories yet.</p>}
        </div>
      )}

      {creating && (
        <RepoForm
          busy={busy}
          error={formError}
          onClose={() => { if (!busy) setCreating(false); }}
          onSubmit={addRepo}
        />
      )}

      <ConfirmDialog
        open={removeTarget != null}
        title="Remove repository"
        confirmLabel="Remove"
        onConfirm={removeRepo}
        onClose={() => setRemoveTarget(null)}
      >
        {removeTarget ? <>Remove <strong>{removeTarget.key}</strong>? Its index, worktree, and row are deleted. This cannot be undone.</> : null}
      </ConfirmDialog>
    </>
  );
}
