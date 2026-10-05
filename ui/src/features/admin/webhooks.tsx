import { useEffect, useRef, useState, type FormEvent } from "react";
import { z } from "zod";
import { CircleAlert, ExternalLink, Plus } from "lucide-react";
import { api, ApiError } from "../../lib/api";
import { webhookSchema, type WebhookInput } from "../../lib/schemas";
import { Button } from "../../components/Button";
import { ConfirmDialog } from "../../components/ConfirmDialog";
import { Sheet } from "../../components/Sheet";
import { Tag } from "../../components/Tag";
import { useToast } from "../../components/Toast";
import { formatError, type AdminPaneProps } from "./page";

type Webhook = z.infer<typeof webhookSchema>;

const OPERATIONS = ["create", "update", "delete", "rename"] as const;
type WebhookOperation = typeof OPERATIONS[number];

const EMPTY_ROWS: readonly Webhook[] = [];

interface WebhookFormProps {
  title: string;
  submitLabel: string;
  busy: boolean;
  secrets: readonly string[];
  defaults: WebhookInput & { enabled: boolean };
  error: ApiError | null;
  onClose: () => void;
  onSubmit: (input: WebhookInput) => void | Promise<void>;
}

function WebhookForm({ title, submitLabel, busy, secrets, defaults, error, onClose, onSubmit }: WebhookFormProps) {
  const [endpoint, setEndpoint] = useState(defaults.endpoint);
  const [consumerOrigin, setConsumerOrigin] = useState(defaults.consumerOrigin);
  const [secretRef, setSecretRef] = useState(defaults.secretRef);
  const [operations, setOperations] = useState<WebhookOperation[]>(defaults.eventOperations);
  const [types, setTypes] = useState(defaults.entityTypes.join(", "));
  const [origins, setOrigins] = useState(defaults.ignoredOrigins.join(", "));
  const [enabled, setEnabled] = useState(defaults.enabled);

  function toggleOperation(operation: WebhookOperation): void {
    setOperations((previous) => previous.includes(operation)
      ? previous.filter((item) => item !== operation)
      : [...previous, operation]);
  }

  function submit(event: FormEvent<HTMLFormElement>): void {
    event.preventDefault();
    void onSubmit({
      endpoint: endpoint.trim(),
      consumerOrigin: consumerOrigin.trim(),
      secretRef: secretRef.trim(),
      eventOperations: operations,
      entityTypes: types.split(",").map((item) => item.trim()).filter((item) => item.length > 0),
      ignoredOrigins: origins.split(",").map((item) => item.trim()).filter((item) => item.length > 0),
      enabled,
    });
  }

  return (
    <Sheet open title={title} onClose={onClose}
      footer={<>
        <Button onClick={onClose} disabled={busy}>Cancel</Button>
        <Button variant="primary" type="submit" form="webhook-form" disabled={busy || !endpoint.trim() || !secretRef.trim()}>
          {busy ? "Saving…" : submitLabel}
        </Button>
      </>}>
      <form id="webhook-form" className="ui-admin-field" onSubmit={submit} aria-label={title}>
        <div className="ui-admin-field">
          <label htmlFor="webhook-endpoint">Endpoint URL</label>
          <input id="webhook-endpoint" className="ui-admin-input" type="url" autoComplete="off"
            placeholder="https://example.com/hooks"
            value={endpoint} onChange={(event) => setEndpoint(event.target.value)} />
        </div>
        <div className="ui-admin-field">
          <label htmlFor="webhook-origin">Consumer origin</label>
          <input id="webhook-origin" className="ui-admin-input" type="text" autoComplete="off"
            value={consumerOrigin} onChange={(event) => setConsumerOrigin(event.target.value)} />
        </div>
        <div className="ui-admin-field">
          <label htmlFor="webhook-secret">Secret reference</label>
          <input id="webhook-secret" className="ui-admin-input" type="text" autoComplete="off"
            list="webhook-secrets" placeholder="key of the configured signing secret"
            value={secretRef} onChange={(event) => setSecretRef(event.target.value)} />
          <datalist id="webhook-secrets">
            {secrets.map((secret) => <option key={secret} value={secret} />)}
          </datalist>
          {secrets.length === 0 && (
            <span className="ui-admin-hint">No signing secrets are configured; the reference is resolved at delivery time.</span>
          )}
        </div>
        <fieldset className="ui-admin-field">
          <legend style={{ color: "var(--ink-faint)", font: "500 11px/16px var(--font-mono)", letterSpacing: ".06em", textTransform: "uppercase" }}>
            Event operations
          </legend>
          <div className="ui-admin-scopes">
            {OPERATIONS.map((operation) => (
              <label key={operation} className="ui-admin-scope">
                <input type="checkbox" checked={operations.includes(operation)}
                  onChange={() => toggleOperation(operation)} />
                <span><span className="ui-admin-scope__label">{operation}</span></span>
              </label>
            ))}
          </div>
          {operations.length === 0 && <span className="ui-admin-hint">No selection delivers every operation.</span>}
        </fieldset>
        <div className="ui-admin-field">
          <label htmlFor="webhook-types">Entity types</label>
          <input id="webhook-types" className="ui-admin-input" type="text" autoComplete="off"
            placeholder="comma separated; empty delivers every type"
            value={types} onChange={(event) => setTypes(event.target.value)} />
        </div>
        <div className="ui-admin-field">
          <label htmlFor="webhook-ignored">Ignored origins</label>
          <input id="webhook-ignored" className="ui-admin-input" type="text" autoComplete="off"
            placeholder="comma separated; empty blocks no origin"
            value={origins} onChange={(event) => setOrigins(event.target.value)} />
        </div>
        <label className="ui-admin-scope" style={{ alignSelf: "flex-start" }}>
          <input type="checkbox" checked={enabled} onChange={(event) => setEnabled(event.target.checked)} />
          <span><span className="ui-admin-scope__label">Enabled</span></span>
        </label>
        {error && <p className="ui-admin-error" role="alert">{formatError(error)}</p>}
      </form>
    </Sheet>
  );
}

/**
 * Subscriptions of the selected workspace. The pane needs a human-admin
 * token and the workspace owner role, the same two gates the server runs.
 * Test delivers one signed event and reports the measured status and
 * latency; no outbox row changes.
 */
export function WebhooksPane({ workspace, adminSession, onCountChange }: AdminPaneProps) {
  const notify = useToast();
  const [rows, setRows] = useState<readonly Webhook[]>(EMPTY_ROWS);
  const [secrets, setSecrets] = useState<readonly string[]>([]);
  const [deliveryRole, setDeliveryRole] = useState(false);
  const [loading, setLoading] = useState(true);
  const [loadError, setLoadError] = useState<ApiError | null>(null);
  const [reloadKey, setReloadKey] = useState(0);
  const [creating, setCreating] = useState(false);
  const [editing, setEditing] = useState<Webhook | null>(null);
  const [formError, setFormError] = useState<ApiError | null>(null);
  const [busy, setBusy] = useState(false);
  const [testingId, setTestingId] = useState<string | null>(null);
  const [testResult, setTestResult] = useState<{ id: string; label: string; ok: boolean } | null>(null);
  const [deleteTarget, setDeleteTarget] = useState<Webhook | null>(null);

  const countHandler = useRef(onCountChange);
  useEffect(() => { countHandler.current = onCountChange; });

  const canAdmin = adminSession?.scopes.includes("admin") ?? false;

  const workspaceId = workspace?.workspaceId ?? null;
  useEffect(() => {
    const controller = new AbortController();
    let active = true;
    if (!workspaceId) return () => { active = false; controller.abort(); };
    setLoading(true);
    setLoadError(null);
    setRows(EMPTY_ROWS);
    api.webhooks(workspaceId, controller.signal)
      .then((result) => {
        if (!active) return;
        setRows(result.subscriptions);
        setSecrets(result.configuredSecrets);
        setDeliveryRole(result.deliveryRole);
        countHandler.current?.(result.subscriptions.length);
      })
      .catch((cause) => {
        if (active && !controller.signal.aborted) {
          setLoadError(cause instanceof ApiError ? cause : new ApiError(0, "network_error", "The webhooks could not be loaded."));
        }
      })
      .finally(() => { if (active) setLoading(false); });
    return () => { active = false; controller.abort(); };
  }, [workspaceId, reloadKey]);

  if (!workspaceId) {
    return (
      <div className="ui-admin-unavailable" role="status">
        <CircleAlert size={28} aria-hidden="true" />
        <strong>No workspace selected</strong>
        <span>Choose a workspace in the top bar to manage its webhooks.</span>
      </div>
    );
  }
  if (!canAdmin) {
    return (
      <div className="ui-admin-unavailable" role="status">
        <CircleAlert size={28} aria-hidden="true" />
        <strong>Admin scope required</strong>
        <span>Webhooks render only with a human token that holds the admin scope.</span>
      </div>
    );
  }
  if (workspace?.role !== "owner") {
    return (
      <div className="ui-admin-unavailable" role="status">
        <CircleAlert size={28} aria-hidden="true" />
        <strong>Workspace ownership required</strong>
        <span>Webhooks need the owner role on this workspace.</span>
      </div>
    );
  }

  // Function declarations hoist above the guards, so predicate narrowing
  // does not reach them. Bind the guarded id once for the mutation helpers.
  const targetWorkspaceId = workspaceId;

  async function saveCreating(input: WebhookInput): Promise<void> {
    setBusy(true);
    setFormError(null);
    try {
      await api.createWebhook(targetWorkspaceId, input);
      setCreating(false);
      setReloadKey((key) => key + 1);
      notify("success", "Webhook created.");
    } catch (cause) {
      // A failed mutation keeps the sheet open and the form input.
      setFormError(cause instanceof ApiError ? cause : new ApiError(0, "network_error", "The webhook could not be created."));
    } finally {
      setBusy(false);
    }
  }

  async function saveEditing(input: WebhookInput): Promise<void> {
    if (!editing) return;
    setBusy(true);
    setFormError(null);
    try {
      await api.updateWebhook(targetWorkspaceId, editing.subscriptionId, input);
      setEditing(null);
      setReloadKey((key) => key + 1);
      notify("success", "Webhook updated.");
    } catch (cause) {
      setFormError(cause instanceof ApiError ? cause : new ApiError(0, "network_error", "The webhook could not be updated."));
    } finally {
      setBusy(false);
    }
  }

  async function deleteWebhook(): Promise<void> {
    if (!deleteTarget) return;
    try {
      await api.deleteWebhook(targetWorkspaceId, deleteTarget.subscriptionId);
      setDeleteTarget(null);
      setReloadKey((key) => key + 1);
      notify("success", "Webhook deleted.");
    } catch (cause) {
      const error = cause instanceof ApiError ? cause : new ApiError(0, "network_error", "The webhook could not be deleted.");
      throw new Error(formatError(error));
    }
  }

  async function testWebhook(row: Webhook): Promise<void> {
    setTestingId(row.subscriptionId);
    setTestResult(null);
    try {
      const result = await api.testWebhook(targetWorkspaceId, row.subscriptionId);
      const latencyMs = Math.round(result.latencyUs / 1000);
      setTestResult({
        id: row.subscriptionId,
        ok: result.ok,
        label: `HTTP ${result.status} · ${result.ok ? "delivered" : "failed"} · ${latencyMs} ms`,
      });
    } catch (cause) {
      const error = cause instanceof ApiError ? cause : new ApiError(0, "network_error", "The test delivery could not be sent.");
      setTestResult({ id: row.subscriptionId, ok: false, label: formatError(error) });
    } finally {
      setTestingId(null);
    }
  }

  const emptyInput: WebhookInput & { enabled: boolean } = {
    endpoint: "", consumerOrigin: "", secretRef: "",
    eventOperations: [], entityTypes: [], ignoredOrigins: [], enabled: true,
  };

  return (
    <>
      <header className="ui-admin__head">
        <div className="ui-admin__head-copy">
          <h1>Webhooks</h1>
          <p>Push subscriptions of {workspace.name}. Delivery needs the worker configured on the server.</p>
        </div>
        <div className="ui-admin__head-actions">
          <Button variant="primary" onClick={() => { setFormError(null); setCreating(true); }}>
            <Plus size={16} aria-hidden="true" />Add webhook
          </Button>
        </div>
      </header>

      {!deliveryRole && (
        <p className="ui-admin-warn"><ExternalLink size={14} aria-hidden="true" />This process does not run the delivery worker. Stored subscriptions will not be delivered.</p>
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
                <th>Endpoint</th>
                <th>Operations</th>
                <th>Entity types</th>
                <th>Secret ref</th>
                <th>Status</th>
                <th className="ui-admin-cell-actions">Actions</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((row) => (
                <tr key={row.subscriptionId}>
                  <td className="ui-admin-mono ui-admin-cell" title={row.endpoint}>{row.endpoint}</td>
                  <td>
                    <span className="ui-admin-tags">
                      {row.eventOperations.length === 0
                        ? <Tag>all operations</Tag>
                        : row.eventOperations.map((operation) => <Tag key={operation}>{operation}</Tag>)}
                    </span>
                  </td>
                  <td>
                    <span className="ui-admin-tags">
                      {row.entityTypes.length === 0
                        ? <Tag>all types</Tag>
                        : row.entityTypes.slice(0, 3).map((type) => <Tag key={type}>{type}</Tag>)}
                      {row.entityTypes.length > 3 && <Tag>+{row.entityTypes.length - 3}</Tag>}
                    </span>
                  </td>
                  <td className="ui-admin-mono ui-admin-cell">{row.secretRef}</td>
                  <td><Tag tone={row.enabled ? "ok" : "neutral"}>{row.enabled ? "enabled" : "disabled"}</Tag></td>
                  <td className="ui-admin-cell-actions">
                    <span className="ui-admin-row-actions">
                      <Button size="sm" variant="ghost" disabled={testingId === row.subscriptionId}
                        onClick={() => void testWebhook(row)}>
                        {testingId === row.subscriptionId ? "Testing…" : "Test"}
                      </Button>
                      <Button size="sm" variant="ghost" onClick={() => { setFormError(null); setEditing(row); }}>
                        Edit
                      </Button>
                      <Button size="sm" variant="ghost" onClick={() => setDeleteTarget(row)}>
                        Delete
                      </Button>
                    </span>
                    {testResult?.id === row.subscriptionId && (
                      <p className={`ui-admin-test ${testResult.ok ? "ui-admin-test--ok" : "ui-admin-test--fail"}`} role="status">
                        {testResult.label}
                      </p>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
          {loading && <p className="ui-admin-state">Loading webhooks…</p>}
          {!loading && rows.length === 0 && <p className="ui-admin-state">No webhooks in this workspace.</p>}
        </div>
      )}

      {creating && (
        <WebhookForm
          key="new"
          title="Add webhook"
          submitLabel="Save webhook"
          busy={busy}
          secrets={secrets}
          defaults={emptyInput}
          error={formError}
          onClose={() => { if (!busy) setCreating(false); }}
          onSubmit={saveCreating}
        />
      )}
      {editing && (
        <WebhookForm
          key={editing.subscriptionId}
          title="Edit webhook"
          submitLabel="Save changes"
          busy={busy}
          secrets={secrets}
          defaults={editing}
          error={formError}
          onClose={() => { if (!busy) setEditing(null); }}
          onSubmit={saveEditing}
        />
      )}

      <ConfirmDialog
        open={deleteTarget != null}
        title="Delete webhook"
        confirmLabel="Delete"
        onConfirm={deleteWebhook}
        onClose={() => setDeleteTarget(null)}
      >
        {deleteTarget ? <>Delete the subscription to <strong>{deleteTarget.endpoint}</strong>? No further events are delivered to it.</> : null}
      </ConfirmDialog>
    </>
  );
}
