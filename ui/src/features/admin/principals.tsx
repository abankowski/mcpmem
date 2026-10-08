import { useEffect, useRef, useState, type FormEvent } from "react";
import { Ellipsis, Plus } from "lucide-react";
import { api, ApiError } from "../../lib/api";
import { Button } from "../../components/Button";
import { ConfirmDialog } from "../../components/ConfirmDialog";
import { Sheet } from "../../components/Sheet";
import { Tag } from "../../components/Tag";
import { useToast } from "../../components/Toast";
import type { Principal } from "../../lib/schemas";
import { formatError, type AdminPaneProps } from "./page";

const EMPTY_ROWS: readonly Principal[] = [];

const SCOPE_OPTIONS: readonly { slug: string; description: string }[] = [
  { slug: "graph-read", description: "read entities, relations, search" },
  { slug: "graph-write", description: "create, edit, merge, delete" },
  { slug: "vectors", description: "semantic and hybrid search" },
  { slug: "code", description: "code index tools" },
  { slug: "attachments", description: "file upload, read, and delete" },
  { slug: "admin", description: "this panel and server settings" },
];

interface RowMenuProps {
  row: Principal;
  onEdit: () => void;
  onDelete: () => void;
}

function RowMenu({ row, onEdit, onDelete }: RowMenuProps) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!open) return;
    const onPointerDown = (event: PointerEvent) => {
      if (ref.current && !ref.current.contains(event.target as Node)) setOpen(false);
    };
    document.addEventListener("pointerdown", onPointerDown);
    return () => document.removeEventListener("pointerdown", onPointerDown);
  }, [open]);
  return (
    <div className="ui-admin-menu" ref={ref}>
      <Button size="sm" variant="ghost" iconOnly aria-label={`Actions for ${row.name}`}
        onClick={() => setOpen((value) => !value)}>
        <Ellipsis size={16} aria-hidden="true" />
      </Button>
      {open && (
        <div className="ui-admin-menu__pop" role="menu">
          <button type="button" role="menuitem" className="ui-admin-menu__item"
            onClick={() => { setOpen(false); onEdit(); }}>
            Edit label and scopes
          </button>
          <button type="button" role="menuitem" className="ui-admin-menu__item ui-admin-menu__item--danger"
            onClick={() => { setOpen(false); onDelete(); }}>
            Delete
          </button>
        </div>
      )}
    </div>
  );
}

interface PrincipalFormProps {
  title: string;
  submitLabel: string;
  busy: boolean;
  defaults: { name: string; iss: string; sub: string; label: string; scopes: readonly string[] };
  showIdentity: boolean;
  error: ApiError | null;
  onClose: () => void;
  onSubmit: (input: { name: string; iss: string; sub: string; label: string; scopes: string[] }) => void | Promise<void>;
}

export function PrincipalForm({ title, submitLabel, busy, defaults, showIdentity, error, onClose, onSubmit }: PrincipalFormProps) {
  const [name, setName] = useState(defaults.name);
  const [iss, setIss] = useState(defaults.iss);
  const [sub, setSub] = useState(defaults.sub);
  const [label, setLabel] = useState(defaults.label);
  const [scopes, setScopes] = useState<string[]>(() => SCOPE_OPTIONS
    .filter((option) => defaults.scopes.includes(option.slug))
    .map((option) => option.slug));

  function toggleScope(slug: string): void {
    setScopes((previous) => previous.includes(slug) ? previous.filter((item) => item !== slug) : [...previous, slug]);
  }

  function submit(event: FormEvent<HTMLFormElement>): void {
    event.preventDefault();
    void onSubmit({ name, iss, sub, label, scopes });
  }

  return (
    <Sheet open title={title} onClose={onClose}
      footer={<>
        <Button onClick={onClose} disabled={busy}>Cancel</Button>
        <Button variant="primary" type="submit" form="principal-form" disabled={busy || !name.trim()}>
          {busy ? "Saving…" : submitLabel}
        </Button>
      </>}>
      <form id="principal-form" className="ui-admin-field" onSubmit={submit} aria-label={title}>
        <div className="ui-admin-field">
          <label htmlFor="principal-name">Name</label>
          <input id="principal-name" className="ui-admin-input" type="text" autoComplete="off"
            placeholder="e.g. claude-desktop"
            value={name} onChange={(event) => setName(event.target.value)} />
        </div>
        {showIdentity && (
          <>
            <div className="ui-admin-field">
              <label htmlFor="principal-iss">Issuer (iss)</label>
              <input id="principal-iss" className="ui-admin-input" type="text" autoComplete="off"
                placeholder="https://accounts.google.com"
                value={iss} onChange={(event) => setIss(event.target.value)} />
            </div>
            <div className="ui-admin-field">
              <label htmlFor="principal-sub">Subject (sub)</label>
              <input id="principal-sub" className="ui-admin-input" type="text" autoComplete="off"
                placeholder="identity id from the token"
                value={sub} onChange={(event) => setSub(event.target.value)} />
            </div>
          </>
        )}
        <div className="ui-admin-field">
          <label htmlFor="principal-label">Label</label>
          <input id="principal-label" className="ui-admin-input" type="text" autoComplete="off"
            placeholder="optional, shown in audit"
            value={label} onChange={(event) => setLabel(event.target.value)} />
        </div>
        <fieldset className="ui-admin-field">
          <legend style={{ color: "var(--ink-faint)", font: "500 11px/16px var(--font-mono)", letterSpacing: ".06em", textTransform: "uppercase" }}>
            Scopes
          </legend>
          <div className="ui-admin-row-actions" style={{ justifyContent: "flex-start" }}>
            <Button size="sm" variant="ghost" onClick={() => setScopes(SCOPE_OPTIONS.map((option) => option.slug))}>Select all</Button>
            <Button size="sm" variant="ghost" onClick={() => setScopes([])}>Clear</Button>
          </div>
          <div className="ui-admin-scopes">
            {SCOPE_OPTIONS.map((option) => (
              <label key={option.slug} className="ui-admin-scope">
                <input type="checkbox" checked={scopes.includes(option.slug)}
                  onChange={() => toggleScope(option.slug)} />
                <span>
                  <span className="ui-admin-scope__label">{option.slug}</span>
                  <br />
                  <span className="ui-admin-scope__desc">{option.description}</span>
                </span>
              </label>
            ))}
          </div>
          {scopes.length === 0 && <span className="ui-admin-hint">At least one scope is required.</span>}
        </fieldset>
        {error && <p className="ui-admin-error" role="alert">{formatError(error)}</p>}
      </form>
    </Sheet>
  );
}

/**
 * The principals table: built-ins from the principals file first, then
 * runtime rows, each with the scopes the server stored. A built-in-owned
 * identity is immutable; its row has no action menu. The count badge and
 * every scope tag come from the adapter response, never from a client list.
 */
export function PrincipalsPane({ adminSession, onCountChange }: AdminPaneProps) {
  const notify = useToast();
  const [defaultScopes, setDefaultScopes] = useState<readonly string[]>([]);
  const [rows, setRows] = useState<readonly Principal[]>(EMPTY_ROWS);
  const [loading, setLoading] = useState(true);
  const [loadError, setLoadError] = useState<ApiError | null>(null);
  const [reloadKey, setReloadKey] = useState(0);
  const [creating, setCreating] = useState(false);
  const [editing, setEditing] = useState<Principal | null>(null);
  const [formError, setFormError] = useState<ApiError | null>(null);
  const [busy, setBusy] = useState(false);
  const [deleteTarget, setDeleteTarget] = useState<Principal | null>(null);

  const countHandler = useRef(onCountChange);
  useEffect(() => { countHandler.current = onCountChange; });

  const canAdmin = adminSession?.scopes.includes("admin") ?? false;

  useEffect(() => {
    const controller = new AbortController();
    let active = true;
    setLoading(true);
    setLoadError(null);
    api.principals(controller.signal)
      .then((result) => {
        if (!active) return;
        setRows(result.principals);
        setDefaultScopes(result.defaultNewPrincipalScopes);
        countHandler.current?.(result.principals.length);
      })
      .catch((cause) => {
        if (active && !controller.signal.aborted) {
          setLoadError(cause instanceof ApiError ? cause : new ApiError(0, "network_error", "The principal list could not be loaded."));
        }
      })
      .finally(() => { if (active) setLoading(false); });
    return () => { active = false; controller.abort(); };
  }, [reloadKey]);

  if (!canAdmin) {
    return (
      <div className="ui-admin-unavailable" role="status">
        <Ellipsis size={28} aria-hidden="true" />
        <strong>Admin scope required</strong>
        <span>This pane renders only with a human token that holds the admin scope.</span>
      </div>
    );
  }

  async function saveCreating(input: { name: string; iss: string; sub: string; label: string; scopes: string[] }): Promise<void> {
    setBusy(true);
    setFormError(null);
    try {
      await api.createPrincipal({
        name: input.name.trim(),
        iss: input.iss.trim(),
        sub: input.sub.trim(),
        label: input.label.trim() || undefined,
        scopes: input.scopes,
      });
      setCreating(false);
      setReloadKey((key) => key + 1);
      notify("success", "Principal created.");
    } catch (cause) {
      // A failed mutation keeps the sheet open and the form input.
      setFormError(cause instanceof ApiError ? cause : new ApiError(0, "network_error", "The principal could not be created."));
    } finally {
      setBusy(false);
    }
  }

  async function saveEditing(input: { name: string; iss: string; sub: string; label: string; scopes: string[] }): Promise<void> {
    if (!editing) return;
    setBusy(true);
    setFormError(null);
    try {
      await api.updatePrincipal(editing.id, {
        name: input.name.trim(),
        // An empty label clears the stored one.
        label: input.label,
        scopes: input.scopes,
      });
      setEditing(null);
      setReloadKey((key) => key + 1);
      notify("success", "Principal updated.");
    } catch (cause) {
      setFormError(cause instanceof ApiError ? cause : new ApiError(0, "network_error", "The principal could not be updated."));
    } finally {
      setBusy(false);
    }
  }

  async function deletePrincipal(): Promise<void> {
    if (!deleteTarget) return;
    try {
      await api.deletePrincipal(deleteTarget.id);
      setDeleteTarget(null);
      setReloadKey((key) => key + 1);
      notify("success", "Principal deleted.");
    } catch (cause) {
      const error = cause instanceof ApiError ? cause : new ApiError(0, "network_error", "The principal could not be deleted.");
      throw new Error(formatError(error));
    }
  }

  const formDefaults = (row: Principal | null): { name: string; iss: string; sub: string; label: string; scopes: readonly string[] } => ({
    name: row?.name ?? "",
    iss: row?.iss ?? "",
    sub: row?.sub ?? "",
    label: row?.label ?? "",
    scopes: row?.scopes ?? defaultScopes,
  });

  return (
    <>
      <header className="ui-admin__head">
        <div className="ui-admin__head-copy">
          <h1>Principals</h1>
          <p>Identities that may authorize against this server, and the scopes they can be granted.</p>
        </div>
        <div className="ui-admin__head-actions">
          <Button variant="primary" onClick={() => { setFormError(null); setCreating(true); }}>
            <Plus size={16} aria-hidden="true" />Add principal
          </Button>
        </div>
      </header>

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
                <th>Name</th>
                <th>Issuer</th>
                <th>Subject</th>
                <th>Scopes</th>
                <th className="ui-admin-cell-actions">Actions</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((row) => {
                const immutable = row.builtin || row.maskedByBuiltin;
                return (
                  <tr key={row.id}>
                    <td>
                      <span style={{ fontWeight: 500 }}>{row.name}</span>
                      {row.builtin && <Tag className="ui-admin-tag-inline">built-in</Tag>}
                      {!row.builtin && row.maskedByBuiltin && <Tag tone="warn">masked by built-in</Tag>}
                      {row.label && <span className="ui-admin-secondary"> · {row.label}</span>}
                    </td>
                    <td className="ui-admin-mono ui-admin-cell">{row.iss}</td>
                    <td className="ui-admin-mono ui-admin-cell">{row.sub}</td>
                    <td>
                      <span className="ui-admin-tags">
                        {row.scopes.map((scope) => <Tag key={scope}>{scope}</Tag>)}
                      </span>
                    </td>
                    <td className="ui-admin-cell-actions">
                      {!immutable && (
                        <RowMenu row={row}
                          onEdit={() => { setFormError(null); setEditing(row); }}
                          onDelete={() => setDeleteTarget(row)} />
                      )}
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
          {loading && <p className="ui-admin-state">Loading principals…</p>}
          {!loading && rows.length === 0 && <p className="ui-admin-state">No principals.</p>}
        </div>
      )}

      {creating && (
        <PrincipalForm
          key="new"
          title="Add principal"
          submitLabel="Save principal"
          busy={busy}
          defaults={formDefaults(null)}
          showIdentity
          error={formError}
          onClose={() => { if (!busy) setCreating(false); }}
          onSubmit={saveCreating}
        />
      )}
      {editing && (
        <PrincipalForm
          key={editing.id}
          title={`Edit ${editing.name}`}
          submitLabel="Save changes"
          busy={busy}
          defaults={formDefaults(editing)}
          showIdentity={false}
          error={formError}
          onClose={() => { if (!busy) setEditing(null); }}
          onSubmit={saveEditing}
        />
      )}

      <ConfirmDialog
        open={deleteTarget != null}
        title="Delete principal"
        confirmLabel="Delete"
        onConfirm={deletePrincipal}
        onClose={() => setDeleteTarget(null)}
      >
        {deleteTarget ? <>Delete <strong>{deleteTarget.name}</strong>? Workspace access, pending grants, and live token families are removed too. This cannot be undone.</> : null}
      </ConfirmDialog>
    </>
  );
}
