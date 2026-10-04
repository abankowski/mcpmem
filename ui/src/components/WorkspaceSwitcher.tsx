import { PanelsTopLeft } from "lucide-react";
import type { Workspace } from "../lib/schemas";

interface WorkspaceSwitcherProps {
  workspaces: readonly Workspace[];
  current: Workspace | null;
  onChange: (workspaceId: string) => void;
  disabled?: boolean;
}

export function WorkspaceSwitcher({ workspaces, current, onChange, disabled }: WorkspaceSwitcherProps) {
  return (
    <div className="ui-workspace-switcher">
      <PanelsTopLeft size={16} aria-hidden="true" />
      <label className="ui-visually-hidden" htmlFor="workspace-switcher">Workspace</label>
      <select id="workspace-switcher" value={current?.workspaceId ?? ""}
        onChange={(event) => onChange(event.target.value)} disabled={disabled || workspaces.length === 0}>
        <option value="" disabled>{workspaces.length ? "Select a workspace" : "No workspaces"}</option>
        {workspaces.map((workspace) => (
          <option key={workspace.workspaceId} value={workspace.workspaceId}>
            {workspace.name} · {workspace.workspaceId}{workspace.isDefault ? " (saved default)" : ""}
          </option>
        ))}
      </select>
    </div>
  );
}
