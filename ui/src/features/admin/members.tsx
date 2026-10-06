import { useEffect, useRef, useState, type FormEvent } from "react";
import { CircleAlert } from "lucide-react";
import { api, ApiError } from "../../lib/api";
import { canAuthorize, requestConsent } from "../../lib/auth";
import { Button } from "../../components/Button";
import { ConfirmDialog } from "../../components/ConfirmDialog";
import { Tag } from "../../components/Tag";
import { useToast } from "../../components/Toast";
import type { Principal } from "../../lib/schemas";
import { formatError, type AdminPaneProps } from "./page";

interface GrantRow {
  principalId: string;
  role: "reader" | "writer";
}

const EMPTY_GRANTS: readonly GrantRow[] = [];
const EMPTY_PRINCIPALS: readonly Principal[] = [];

/**
 * Members and grants of the selected workspace. Grants and revokes run the
 * workspace-owner adapters; the identity picker lists principals, which the
 * server gates behind a human-admin credential. When that list is denied,
 * the form shows the server error and keeps its input.
 */
export function MembersPane({ workspace, session, adminSession, onCountChange }: AdminPaneProps) {
  const notify = useToast();
  const [grants, setGrants] = useState<readonly GrantRow[]>(EMPTY_GRANTS);
  const [grantsLoading, setGrantsLoading] = useState(true);
  const [grantsError, setGrantsError] = useState<ApiError | null>(null);
  const [grantsKey, setGrantsKey] = useState(0);
  const [principals, setPrincipals] = useState<readonly Principal[]>(EMPTY_PRINCIPALS);
  const [principalsError, setPrincipalsError] = useState<ApiError | null>(null);
  const [principalsKey, setPrincipalsKey] = useState(0);
  const [picker, setPicker] = useState("");
  const [role, setRole] = useState<"reader" | "writer">("reader");
  const [grantError, setGrantError] = useState<ApiError | null>(null);
  const [granting, setGranting] = useState(false);
  const [revoking, setRevoking] = useState<GrantRow | null>(null);

  const countHandler = useRef(onCountChange);
  useEffect(() => { countHandler.current = onCountChange; });

  const canAdmin = adminSession?.scopes.includes("admin") ?? false;
  const canWrite = session?.scopes.includes("graph-write") ?? false;

  useEffect(() => {
    const controller = new AbortController();
    let active = true;
    if (!workspace) return () => { active = false; controller.abort(); };
    setGrantsLoading(true);
    setGrantsError(null);
    setGrants(EMPTY_GRANTS);
    api.grants(workspace.workspaceId, controller.signal)
      .then((result) => {
        if (!active) return;
        setGrants(result.grants);
        countHandler.current?.(result.grants.length);
      })
      .catch((cause) => {
        if (active && !controller.signal.aborted) {
          setGrantsError(cause instanceof ApiError ? cause : new ApiError(0, "network_error", "The grants could not be loaded."));
        }
      })
      .finally(() => { if (active) setGrantsLoading(false); });
    return () => { active = false; controller.abort(); };
  }, [workspace, grantsKey]);

  useEffect(() => {
    const controller = new AbortController();
    let active = true;
    if (!workspace) return () => { active = false; controller.abort(); };
    setPrincipalsError(null);
    setPrincipals(EMPTY_PRINCIPALS);
    api.principals(controller.signal)
      .then((result) => { if (active) setPrincipals(result.principals); })
      .catch((cause) => {
        if (active && !controller.signal.aborted) {
          setPrincipalsError(cause instanceof ApiError ? cause : new ApiError(0, "network_error", "The principal list could not be loaded."));
        }
      });
    return () => { active = false; controller.abort(); };
  }, [workspace, principalsKey]);

  if (!workspace) {
    return (
      <div className="ui-admin-unavailable" role="status">
        <CircleAlert size={28} aria-hidden="true" />
        <strong>No workspace selected</strong>
        <span>Choose a workspace in the top bar to see its members and grants.</span>
      </div>
    );
  }
  if (workspace.role !== "owner") {
    return (
      <div className="ui-admin-unavailable" role="status">
        <CircleAlert size={28} aria-hidden="true" />
        <strong>Workspace ownership required</strong>
        <span>Members and grants need the owner role on this workspace.</span>
      </div>
    );
  }

  async function submitGrant(): Promise<void> {
    if (!picker || !workspace || !canAdmin) return;
    if (!(await requestConsent(["graph-write"]))) return;
    setGranting(true);
    setGrantError(null);
    try {
      // The registry stores a human grant under its stable id; the admin
      // principal id is the stable id without the "human:" prefix.
      await api.grant(workspace.workspaceId, `human:${picker}`, role);
      setPicker("");
      setRole("reader");
      setGrantsKey((key) => key + 1);
      notify("success", "Access granted.");
    } catch (cause) {
      // A failed mutation keeps the form input.
      setGrantError(cause instanceof ApiError ? cause : new ApiError(0, "network_error", "The grant could not be saved."));
    } finally {
      setGranting(false);
    }
  }

  function submitGrantForm(event: FormEvent<HTMLFormElement>): void {
    event.preventDefault();
    void submitGrant();
  }

  async function revokeGrant(): Promise<void> {
    if (!revoking || !workspace) return;
    if (!(await requestConsent(["graph-write"]))) return;
    try {
      await api.revokeGrant(workspace.workspaceId, revoking.principalId);
      setRevoking(null);
      setGrantsKey((key) => key + 1);
      notify("success", "Access revoked.");
    } catch (cause) {
      const error = cause instanceof ApiError ? cause : new ApiError(0, "network_error", "The access could not be revoked.");
      throw new Error(formatError(error));
    }
  }

  const nameById: Record<string, string> = {};
  for (const principal of principals) nameById[`human:${principal.id}`] = principal.name;

  return (
    <>
      <header className="ui-admin__head">
        <div className="ui-admin__head-copy">
          <h1>Members and grants</h1>
          <p>Who can read or write {workspace.name}. Only the owner sees this pane.</p>
        </div>
      </header>

      {!canWrite && (
        <p className="ui-admin-warn ui-admin-warn--action">
          <span>Changing members needs the graph-write scope.</span>
          {canAuthorize("graph") && <Button variant="ghost" size="sm" onClick={() => { void requestConsent(["graph-write"]); }}>Grant graph-write</Button>}
        </p>
      )}

      <div className="ui-admin-table-wrap">
        <table className="ui-admin-table">
          <thead>
            <tr>
              <th>Principal ID</th>
              <th>Role</th>
              <th className="ui-admin-cell-actions">Actions</th>
            </tr>
          </thead>
          <tbody>
            {grants.map((grant) => (
              <tr key={grant.principalId}>
                <td className="ui-admin-mono ui-admin-cell" title={grant.principalId}>
                  {nameById[grant.principalId] ?? grant.principalId}
                  {nameById[grant.principalId] && (
                    <span className="ui-admin-secondary"> · {grant.principalId}</span>
                  )}
                </td>
                <td><Tag>{grant.role}</Tag></td>
                <td className="ui-admin-cell-actions">
                  {canWrite && (
                    <Button size="sm" variant="ghost" onClick={() => setRevoking(grant)}>Revoke</Button>
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
        {grantsLoading && <p className="ui-admin-state">Loading members…</p>}
        {!grantsLoading && grants.length === 0 && <p className="ui-admin-state">No members granted yet.</p>}
      </div>
      {grantsError && (
        <div className="ui-admin-error" role="alert">
          <span>{formatError(grantsError)}</span>
          <Button size="sm" variant="ghost" onClick={() => setGrantsKey((key) => key + 1)}>Retry</Button>
        </div>
      )}

      <form className="ui-admin-field" onSubmit={submitGrantForm}>
        <label htmlFor="grant-principal">Grant access</label>
        {principalsError ? (
          <div className="ui-admin-error" role="alert">
            <span>The identity picker is unavailable: {formatError(principalsError)}</span>
            <Button size="sm" variant="ghost" onClick={() => setPrincipalsKey((key) => key + 1)}>Retry</Button>
          </div>
        ) : (
          <>
            <div className="ui-admin-row-actions" style={{ justifyContent: "flex-start" }}>
              <select id="grant-principal" className="ui-admin-input"
                style={{ minWidth: 260 }}
                value={picker}
                disabled={!canAdmin || !canWrite || granting}
                onChange={(event) => setPicker(event.target.value)}>
                <option value="">Choose an identity…</option>
                {principals.map((principal) => (
                  <option key={principal.id} value={principal.id}>
                    {principal.name} · {principal.id}
                  </option>
                ))}
              </select>
              <select
                aria-label="Role to grant"
                className="ui-admin-input"
                value={role}
                disabled={!canAdmin || !canWrite || granting}
                onChange={(event) => setRole(event.target.value === "writer" ? "writer" : "reader")}>
                <option value="reader">reader</option>
                <option value="writer">writer</option>
              </select>
              <Button variant="primary" type="submit" disabled={!canAdmin || !canWrite || granting || !picker}>
                {granting ? "Granting…" : "Grant access"}
              </Button>
            </div>
            {!canAdmin && <span className="ui-admin-hint">The identity picker needs a human-admin sign-in.</span>}
          </>
        )}
        {grantError && <p className="ui-admin-error" role="alert">{formatError(grantError)}</p>}
      </form>

      <ConfirmDialog
        open={revoking != null}
        title="Revoke access"
        confirmLabel="Revoke"
        onConfirm={revokeGrant}
        onClose={() => setRevoking(null)}
      >
        {revoking ? <>Remove <strong>{nameById[revoking.principalId] ?? revoking.principalId}</strong> from {workspace.name}? The member loses access immediately.</> : null}
      </ConfirmDialog>
    </>
  );
}
