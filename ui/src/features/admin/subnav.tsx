import { Count } from "../../components/Count";

export type AdminPaneId =
  | "workspaces"
  | "members"
  | "webhooks"
  | "vectors"
  | "principals"
  | "approvals"
  | "repos";

export interface AdminNavItem {
  id: AdminPaneId;
  label: string;
  /** A measured count from the pane's own list; absent means no badge. */
  count?: number;
}

export interface AdminNavGroup {
  label: string;
  items: AdminNavItem[];
}

interface AdminSubNavProps {
  groups: readonly AdminNavGroup[];
  active: AdminPaneId;
  onSelect: (id: AdminPaneId) => void;
}

/** Two nav groups: workspace-scoped panes, then the human-admin server panes. */
export function AdminSubNav({ groups, active, onSelect }: AdminSubNavProps) {
  return (
    <aside className="ui-admin__nav" aria-label="Admin sections">
      {groups.map((group) => (
        <div key={group.label} className="ui-admin__group">
          <span className="ui-admin__cap" aria-hidden="true">{group.label}</span>
          {group.items.map((item) => {
            const selected = item.id === active;
            return (
              <button
                key={item.id}
                type="button"
                className="ui-admin__item"
                aria-current={selected ? "page" : undefined}
                onClick={() => onSelect(item.id)}
              >
                <span>{item.label}</span>
                {item.count !== undefined && (
                  <Count aria-label={`${item.count} total`}>{item.count}</Count>
                )}
              </button>
            );
          })}
        </div>
      ))}
    </aside>
  );
}
