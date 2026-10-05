import { useEffect, useRef, useState } from "react";
import { z } from "zod";
import { api, ApiError } from "../../lib/api";
import { waitlistSchema } from "../../lib/schemas";
import { formatDate, formatTimestamp } from "../../lib/format";
import { Button } from "../../components/Button";
import { ConfirmDialog } from "../../components/ConfirmDialog";
import { useToast } from "../../components/Toast";
import { formatError, type AdminPaneProps } from "./page";

type WaitlistEntry = z.infer<typeof waitlistSchema>["entries"][number];

const EMPTY_ROWS: readonly WaitlistEntry[] = [];

/**
 * Identities awaiting approval. The stored first-seen and last-seen values
 * are measured server fields; no other clock is invented. Approve promotes
 * the entry to a runtime principal, Deny discards it.
 */
export function ApprovalsPane({ onCountChange }: AdminPaneProps) {
  const notify = useToast();
  const [rows, setRows] = useState<readonly WaitlistEntry[]>(EMPTY_ROWS);
  const [loading, setLoading] = useState(true);
  const [loadError, setLoadError] = useState<ApiError | null>(null);
  const [reloadKey, setReloadKey] = useState(0);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [denyTarget, setDenyTarget] = useState<WaitlistEntry | null>(null);

  const countHandler = useRef(onCountChange);
  useEffect(() => { countHandler.current = onCountChange; });

  useEffect(() => {
    const controller = new AbortController();
    let active = true;
    setLoading(true);
    setLoadError(null);
    api.waitlist(controller.signal)
      .then((result) => {
        if (!active) return;
        setRows(result.entries);
        countHandler.current?.(result.entries.length);
      })
      .catch((cause) => {
        if (active && !controller.signal.aborted) {
          setLoadError(cause instanceof ApiError ? cause : new ApiError(0, "network_error", "The approval list could not be loaded."));
        }
      })
      .finally(() => { if (active) setLoading(false); });
    return () => { active = false; controller.abort(); };
  }, [reloadKey]);

  async function approve(row: WaitlistEntry): Promise<void> {
    setBusyId(row.id);
    try {
      await api.approveWaitlist(row.id);
      setReloadKey((key) => key + 1);
      notify("success", `${row.name} is now a principal.`);
    } catch (cause) {
      const error = cause instanceof ApiError ? cause : new ApiError(0, "network_error", "The entry could not be approved.");
      notify("error", formatError(error));
    } finally {
      setBusyId(null);
    }
  }

  async function deny(): Promise<void> {
    if (!denyTarget) return;
    try {
      await api.dismissWaitlist(denyTarget.id);
      setDenyTarget(null);
      setReloadKey((key) => key + 1);
      notify("success", "Entry discarded.");
    } catch (cause) {
      const error = cause instanceof ApiError ? cause : new ApiError(0, "network_error", "The entry could not be discarded.");
      throw new Error(formatError(error));
    }
  }

  return (
    <>
      <header className="ui-admin__head">
        <div className="ui-admin__head-copy">
          <h1>Pending approvals</h1>
          <p>Identities that asked for access and wait for an admin decision.</p>
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
                <th>First seen</th>
                <th>Last seen</th>
                <th className="ui-admin-cell-actions">Actions</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((row) => (
                <tr key={row.id}>
                  <td style={{ fontWeight: 500 }}>{row.name}</td>
                  <td className="ui-admin-mono ui-admin-cell">{row.iss}</td>
                  <td className="ui-admin-mono ui-admin-cell">{row.sub}</td>
                  <td className="ui-admin-mono ui-admin-mono--muted">{formatDate(row.firstSeenUs)}</td>
                  <td className="ui-admin-mono ui-admin-mono--muted">{formatTimestamp(row.lastSeenUs)}</td>
                  <td className="ui-admin-cell-actions">
                    <span className="ui-admin-row-actions">
                      <Button size="sm" variant="primary" disabled={busyId === row.id} onClick={() => void approve(row)}>
                        {busyId === row.id ? "Approving…" : "Approve"}
                      </Button>
                      <Button size="sm" variant="ghost" disabled={busyId === row.id} onClick={() => setDenyTarget(row)}>
                        Deny
                      </Button>
                    </span>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
          {loading && <p className="ui-admin-state">Loading approvals…</p>}
          {!loading && rows.length === 0 && <p className="ui-admin-state">No pending approvals.</p>}
        </div>
      )}

      <ConfirmDialog
        open={denyTarget != null}
        title="Deny approval"
        confirmLabel="Deny"
        onConfirm={deny}
        onClose={() => setDenyTarget(null)}
      >
        {denyTarget ? <>Discard the approval for <strong>{denyTarget.name}</strong>? The entry is removed without creating a principal.</> : null}
      </ConfirmDialog>
    </>
  );
}
