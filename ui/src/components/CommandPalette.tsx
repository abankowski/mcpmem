import { useEffect, useId, useRef, useState } from "react";
import { Search, X } from "lucide-react";
import { z } from "zod";
import { api, ApiError } from "../lib/api";
import { Button } from "./Button";

type NodeChoice = { name: string; entityType?: string };
const recentSchema = z.array(z.string());

function recentNodes(workspaceId: string): string[] {
  try {
    const raw = sessionStorage.getItem(`mcpmem_recent_nodes_${workspaceId}`);
    const result = raw && recentSchema.safeParse(JSON.parse(raw));
    return result && result.success ? result.data : [];
  } catch {
    return [];
  }
}

export function recordRecentNode(workspaceId: string, name: string): void {
  const recent = recentNodes(workspaceId).filter((item) => item !== name);
  sessionStorage.setItem(`mcpmem_recent_nodes_${workspaceId}`, JSON.stringify([name, ...recent].slice(0, 10)));
}

interface CommandPaletteProps {
  open: boolean;
  workspaceId: string | null;
  onClose: () => void;
  onPick: (name: string) => void;
}

export function CommandPalette({ open, workspaceId, onClose, onPick }: CommandPaletteProps) {
  const dialog = useRef<HTMLDialogElement>(null);
  const input = useRef<HTMLInputElement>(null);
  const listId = useId();
  const [query, setQuery] = useState("");
  const [choices, setChoices] = useState<NodeChoice[]>([]);
  const [active, setActive] = useState(0);
  const [status, setStatus] = useState("");
  useEffect(() => {
    const element = dialog.current;
    if (open && !element?.open) {
      element?.showModal();
      input.current?.focus();
    } else if (!open && element?.open) {
      element.close();
    }
    return () => { if (element?.open) element.close(); };
  }, [open]);
  useEffect(() => {
    if (!open || !workspaceId) {
      setChoices([]);
      setStatus(workspaceId ? "" : "Select a workspace to find a node.");
      return;
    }
    if (!query.trim()) {
      setChoices(recentNodes(workspaceId).map((name) => ({ name })));
      setStatus("");
      setActive(0);
      return;
    }
    const abort = new AbortController();
    setChoices([]);
    setStatus("Searching nodes");
    const timer = window.setTimeout(() => {
      api.search({ workspaceId, q: query.trim(), scope: "nodes", mode: "direct", k: 10 }, abort.signal)
        .then((page) => {
          setChoices(page.results.filter((hit) => hit.kind === "entity").map((hit) => ({ name: hit.name, entityType: hit.entityType })));
          setActive(0);
          setStatus("");
        })
        .catch((error: unknown) => {
          if (abort.signal.aborted) return;
          setChoices([]);
          setStatus(error instanceof ApiError ? error.message : "Node search failed. Try again.");
        });
    }, 150);
    return () => { window.clearTimeout(timer); abort.abort(); };
  }, [open, workspaceId, query]);

  function pick(choice: NodeChoice): void {
    if (!workspaceId) return;
    recordRecentNode(workspaceId, choice.name);
    onClose();
    onPick(choice.name);
  }

  return (
    <dialog ref={dialog} className="ui-command-palette" aria-label="Find a node"
      onCancel={(event) => { event.preventDefault(); onClose(); }}>
      <div className="ui-command-palette__header">
        <Search size={18} aria-hidden="true" />
        <label className="ui-visually-hidden" htmlFor={listId + "-query"}>Search node names</label>
        <input ref={input} id={listId + "-query"} type="search" placeholder="Search node names"
          value={query} onChange={(event) => setQuery(event.target.value)} disabled={!workspaceId}
          role="combobox" aria-expanded="true" aria-controls={listId}
          aria-activedescendant={choices[active] ? `${listId}-${active}` : undefined}
          onKeyDown={(event) => {
            if (event.key === "ArrowDown" && choices.length) { event.preventDefault(); setActive((index) => (index + 1) % choices.length); }
            if (event.key === "ArrowUp" && choices.length) { event.preventDefault(); setActive((index) => (index + choices.length - 1) % choices.length); }
            if (event.key === "Enter" && choices[active]) { event.preventDefault(); pick(choices[active]); }
          }} />
        <Button iconOnly aria-label="Close search" variant="ghost" onClick={onClose}><X size={16} aria-hidden="true" /></Button>
      </div>
      <div role="status" className="ui-command-palette__status">{status || (!choices.length ? (query ? "No nodes found." : "No recent nodes.") : (query ? "Results" : "Recent nodes"))}</div>
      <ul id={listId} role="listbox" aria-label="Node matches" className="ui-command-palette__results">
        {choices.map((choice, index) => (
          <li id={`${listId}-${index}`} key={choice.name} role="option" aria-selected={active === index}>
            <button type="button" onMouseEnter={() => setActive(index)} onClick={() => pick(choice)}>
              <span>{choice.name}</span>{choice.entityType && <small>{choice.entityType}</small>}
            </button>
          </li>
        ))}
      </ul>
    </dialog>
  );
}
