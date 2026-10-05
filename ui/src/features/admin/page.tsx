import { useEffect, useState, type ComponentType } from "react";
import { ApiError } from "../../lib/api";
import { useShell } from "../../lib/app-context";
import type { Session, Workspace } from "../../lib/schemas";
import { AdminSubNav, type AdminNavItem, type AdminPaneId } from "./subnav";
import { WorkspacesPane } from "./workspaces";
import { MembersPane } from "./members";
import { PrincipalsPane } from "./principals";
import { ApprovalsPane } from "./approvals";
import { WebhooksPane } from "./webhooks";
import { ReposPane } from "./repos";
import { VectorsPane } from "./vectors";
import "./admin.css";

/** The props every admin pane receives from the page shell. */
export interface AdminPaneProps {
  workspace: Workspace | null;
  session: Session | null;
  adminSession: Session | null;
  /** Report the pane's row count for a nav badge. Only measured counts. */
  onCountChange?: (count: number) => void;
}

export function formatError(error: ApiError): string {
  return `${error.code}: ${error.message}`;
}

const PANES: Record<AdminPaneId, ComponentType<AdminPaneProps>> = {
  workspaces: WorkspacesPane,
  members: MembersPane,
  webhooks: WebhooksPane,
  vectors: VectorsPane,
  principals: PrincipalsPane,
  approvals: ApprovalsPane,
  repos: ReposPane,
};

export function Page() {
  const { workspace, session, adminSession } = useShell();
  const [pane, setPane] = useState<AdminPaneId>("workspaces");
  const [counts, setCounts] = useState<Partial<Record<AdminPaneId, number>>>({});

  // The server reports its compile surface in the session features. A
  // compiled-out group is hidden; a present group without its role renders
  // a named unavailable state inside the pane.
  const features = session?.features ?? adminSession?.features;
  const canAdmin = adminSession?.scopes.includes("admin") ?? false;
  const owner = workspace?.role === "owner";

  const groups = (() => {
    const workspaceItems: AdminNavItem[] = [
      { id: "workspaces", label: "Workspaces", count: counts.workspaces },
    ];
    if (owner) workspaceItems.push({ id: "members", label: "Members and grants", count: counts.members });
    if (canAdmin && features?.webhooks) workspaceItems.push({ id: "webhooks", label: "Webhooks", count: counts.webhooks });
    if (features?.vectors) workspaceItems.push({ id: "vectors", label: "Vector index" });
    const serverItems: AdminNavItem[] = [];
    if (canAdmin) {
      serverItems.push({ id: "principals", label: "Principals", count: counts.principals });
      serverItems.push({ id: "approvals", label: "Pending approvals", count: counts.approvals });
    }
    if (canAdmin && features?.code) serverItems.push({ id: "repos", label: "Code repositories", count: counts.repos });
    return [
      { label: "Workspace", items: workspaceItems },
      { label: "Server", items: serverItems },
    ];
  })();

  // A pane whose gate closed (workspace switch, token change) must not keep
  // rendering. Fall back to the Workspaces landing.
  const visible: Record<string, boolean> = {};
  for (const group of groups) {
    for (const item of group.items) visible[item.id] = true;
  }
  const activePane = visible[pane] ? pane : "workspaces";

  // Counts belong to the selected workspace; drop them when it changes.
  const workspaceId = workspace?.workspaceId ?? "none";
  useEffect(() => {
    setCounts({});
  }, [workspaceId]);

  const Pane = PANES[activePane];
  return (
    <div className="ui-admin">
      <AdminSubNav groups={groups} active={activePane} onSelect={(id) => setPane(id)} />
      <section className="ui-admin__content" aria-label={`Admin - ${activePane}`}>
        <Pane
          key={`${activePane}:${workspaceId}`}
          workspace={workspace}
          session={session}
          adminSession={adminSession}
          onCountChange={(count) => setCounts((previous) => ({ ...previous, [activePane]: count }))}
        />
      </section>
    </div>
  );
}
