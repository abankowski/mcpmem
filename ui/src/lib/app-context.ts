import { createContext, useContext } from "react";
import type { Session, Workspace } from "./schemas";

export interface ShellContext {
  workspace: Workspace | null;
  session: Session | null;
  adminSession: Session | null;
  reload: () => void;
  selectWorkspace: (workspaceId: string) => void;
}

export const shellContext = createContext<ShellContext | null>(null);

export function useShell(): ShellContext {
  const value = useContext(shellContext);
  if (!value) throw new Error("The UI shell is missing.");
  return value;
}
