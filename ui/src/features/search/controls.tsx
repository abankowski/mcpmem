import { useEffect, useState, type FormEvent } from "react";
import { Search as SearchIcon } from "lucide-react";
import { SegmentedControl } from "../../components/SegmentedControl";
import type { TypeList } from "../../lib/schemas";
import { K_OPTIONS, type SearchParams } from "./state";

interface SearchControlsProps {
  params: SearchParams;
  /** The workspace type catalogue; null until it loads or after a failure. */
  types: TypeList | null;
  /**
   * Whether the server enables the vectors category. Modes stay selectable;
   * the results area renders a named unavailable state when the active mode
   * cannot run, never an empty result.
   */
  vectorsOn: boolean;
  /** Commit one control change. */
  onChange: (patch: Partial<SearchParams>) => void;
  /** Commit the query on Enter. */
  onSubmitQuery: (q: string) => void;
}

export function SearchControls({ params, types, vectorsOn, onChange, onSubmitQuery }: SearchControlsProps) {
  const [draft, setDraft] = useState(params.q);
  const [fromDraft, setFromDraft] = useState(params.from);
  const [toDraft, setToDraft] = useState(params.to);
  useEffect(() => { setDraft(params.q); }, [params.q]);
  useEffect(() => { setFromDraft(params.from); }, [params.from]);
  useEffect(() => { setToDraft(params.to); }, [params.to]);

  const queryOptions = params.scope === "nodes" ? types?.entities ?? [] : types?.relations ?? [];
  const queryTypes = [...queryOptions].sort((a, b) => b.count - a.count);
  const typeValue = params.scope === "nodes" ? params.type : params.relationType;

  function submitQuery(event: FormEvent<HTMLFormElement>): void {
    event.preventDefault();
    onSubmitQuery(draft.trim());
  }

  function submitFrom(event: FormEvent<HTMLFormElement>): void {
    event.preventDefault();
    onChange({ from: fromDraft.trim() });
  }

  function submitTo(event: FormEvent<HTMLFormElement>): void {
    event.preventDefault();
    onChange({ to: toDraft.trim() });
  }

  return (
    <section className="s-controls" aria-label="Search controls">
      <form onSubmit={submitQuery} className="s-query">
        <SearchIcon size={18} aria-hidden="true" />
        <label className="ui-visually-hidden" htmlFor="s-query">Search query</label>
        <input
          id="s-query"
          className="s-query__input"
          type="text"
          value={draft}
          aria-label="Search query"
          placeholder="Search nodes, relations, and attached files"
          onChange={(event) => setDraft(event.target.value)}
        />
      </form>
      <div className="s-control-row">
        <SegmentedControl
          label="Search mode"
          name="s-mode"
          value={params.mode}
          onChange={(mode) => onChange({ mode })}
          options={[
            { value: "direct", label: "Direct" },
            { value: "semantic", label: "Semantic" },
            { value: "hybrid", label: "Hybrid" },
          ]}
        />
        <SegmentedControl
          label="Search scope"
          name="s-scope"
          value={params.scope}
          onChange={(scope) => onChange({ scope })}
          options={[
            { value: "nodes", label: "Nodes" },
            { value: "relations", label: "Relations" },
          ]}
        />
        <div className="s-control">
          <span className="s-cap">Type</span>
          <select
            className="s-select"
            aria-label={params.scope === "nodes" ? "Entity type filter" : "Relation type filter"}
            value={typeValue}
            onChange={(event) => {
              if (params.scope === "nodes") onChange({ type: event.target.value });
              else onChange({ relationType: event.target.value });
            }}
          >
            <option value="">Any type</option>
            {queryTypes.map((entry) => (
              <option key={entry.type} value={entry.type}>{entry.type} · {entry.count}</option>
            ))}
          </select>
        </div>
        {params.scope === "relations" && (
          <form onSubmit={submitFrom} className="s-control">
            <span className="s-cap">From</span>
            <input
              className="s-endpoint"
              type="text"
              value={fromDraft}
              aria-label="Relation source filter"
              placeholder="any node"
              onChange={(event) => setFromDraft(event.target.value)}
            />
          </form>
        )}
        {params.scope === "relations" && (
          <form onSubmit={submitTo} className="s-control">
            <span className="s-cap">To</span>
            <input
              className="s-endpoint"
              type="text"
              value={toDraft}
              aria-label="Relation target filter"
              placeholder="any node"
              onChange={(event) => setToDraft(event.target.value)}
            />
          </form>
        )}
        <div className="s-control s-control--end">
          <span className="s-cap">Top k</span>
          <select
            className="s-select"
            aria-label="Result limit"
            value={String(params.k)}
            onChange={(event) =>
              onChange({ k: K_OPTIONS.find((option) => String(option) === event.target.value) ?? 10 })}
          >
            {K_OPTIONS.map((option) => <option key={option} value={String(option)}>{option}</option>)}
          </select>
        </div>
      </div>
      <p className="s-feature-note">
        {params.mode === "direct"
          ? "Direct search matches node names or relation observations with full-text search."
          : vectorsOn
            ? `${params.mode === "semantic" ? "Semantic" : "Hybrid"} search embeds the query with the server's index profile.`
            : "Vector search is off on this server; semantic and hybrid modes cannot run."}
      </p>
    </section>
  );
}