import { useState } from "react";
import { LogOut, Search } from "lucide-react";
import { Button } from "./Button";
import { WorkspaceSwitcher } from "./WorkspaceSwitcher";
import { PAGE_PATHS, type Page } from "../lib/urls";
import type { Workspace } from "../lib/schemas";

interface TopBarProps {
  page: Page;
  current: Workspace | null;
  workspaces: readonly Workspace[];
  principalName: string | null;
  onWorkspaceChange: (workspaceId: string) => void;
  onOpenCommand: () => void;
  onSignOut: () => void;
}

export function TopBar({ page, current, workspaces, principalName, onWorkspaceChange, onOpenCommand, onSignOut }: TopBarProps) {
  const [menuOpen, setMenuOpen] = useState(false);
  const initials = principalName?.trim().split(/\s+/).slice(0, 2).map((part) => part[0]?.toUpperCase()).join("") || "?";
  const signedIn = principalName != null;
  return (
    <header className="ui-topbar">
      <a className="ui-topbar__brand" href={PAGE_PATHS.graph} aria-label="mcpmem - Graph">mcpmem</a>
      <WorkspaceSwitcher workspaces={workspaces} current={current} onChange={onWorkspaceChange} />
      <nav className="ui-topbar__nav" aria-label="Main navigation">
        {(["graph", "search", "admin"] as const).map((item) => (
          <a key={item} href={PAGE_PATHS[item]} aria-current={page === item ? "page" : undefined}>
            {item[0].toUpperCase() + item.slice(1)}
          </a>
        ))}
      </nav>
      <div className="ui-topbar__spacer" />
      <Button variant="ghost" className="ui-topbar__command" onClick={onOpenCommand} aria-label="Find a node">
        <Search size={16} aria-hidden="true" /><span>Jump to node</span><kbd>⌘K</kbd>
      </Button>
      <div className="ui-topbar__avatar-wrap">
        <button
          type="button"
          className="ui-topbar__avatar"
          disabled={!signedIn}
          aria-label={signedIn ? `Signed in as ${principalName}` : "No account signed in"}
          aria-haspopup="menu"
          aria-expanded={menuOpen}
          title={signedIn ? `Signed in as ${principalName}` : "No account signed in"}
          onClick={() => setMenuOpen((value) => !value)}
        >
          {initials}
        </button>
        {signedIn && menuOpen && (
          <>
            <div className="ui-topbar__menu-backdrop" onClick={() => setMenuOpen(false)} />
            <div className="ui-topbar__menu" role="menu" aria-label="Account">
              <div className="ui-topbar__menu-label">{principalName}</div>
              <button
                type="button" role="menuitem"
                onClick={() => { setMenuOpen(false); onSignOut(); }}
              >
                <LogOut size={14} aria-hidden="true" />Sign out
              </button>
            </div>
          </>
        )}
      </div>
    </header>
  );
}
