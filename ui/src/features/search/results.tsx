import type { ReactNode } from "react";
import { FileText } from "lucide-react";
import { Button } from "../../components/Button";
import { Tag } from "../../components/Tag";
import { canAuthorize } from "../../lib/auth";
import { ApiError } from "../../lib/api";
import type { SearchHit, SearchResults } from "../../lib/schemas";
import type { SearchParams } from "./state";

// --- result state -----------------------------------------------------------

export type ResultsStatus = "idle" | "loading" | "done" | "error" | "unavailable";

export interface ResultsState {
  status: ResultsStatus;
  /** The last completed search. Kept while a follow-up search is loading. */
  data: SearchResults | null;
  error: ApiError | null;
  /** The reason a mode cannot run: the vectors feature is off, a profile is missing, or a provider is absent. */
  unavailable: string | null;
}

export function freshResults(): ResultsState {
  return { status: "idle", data: null, error: null, unavailable: null };
}

// --- hit helpers ------------------------------------------------------------

/** One stable key per hit, used for multi-select state. */
export function hitKey(hit: SearchHit): string {
  switch (hit.kind) {
    case "entity": return `entity:${hit.name}`;
    case "relation": return `relation:${hit.from}\u001f${hit.to}\u001f${hit.relationType}`;
    case "attachment": return `attachment:${hit.attachmentId}`;
  }
}

/** The graph node names a hit stands for: the node, both endpoints, or the file's parent. */
export function hitNames(hit: SearchHit): readonly string[] {
  switch (hit.kind) {
    case "entity": return [hit.name];
    case "relation": return [hit.from, hit.to];
    case "attachment": return [hit.entityName];
  }
}

// --- view helpers -----------------------------------------------------------

// The graph palette by type rank. A stable hash of the type name picks one
// dot color, so one type keeps one color inside a results page.
const DOT_COLORS: readonly string[] = [
  "var(--node-1)", "var(--node-2)", "var(--node-3)", "var(--node-4)",
  "var(--node-5)", "var(--node-6)", "var(--node-7)", "var(--node-8)",
];

function typeColor(type: string): string {
  let hash = 0;
  for (const ch of type) hash = (hash * 31 + ch.charCodeAt(0)) % 997;
  return DOT_COLORS[hash % DOT_COLORS.length];
}

/** Mark the query tokens in a snippet. The snippet text is plain text; React escapes it. */
function highlighted(text: string, query: string): ReactNode {
  const tokens = query.toLowerCase().match(/[a-z0-9_]+/g) ?? [];
  if (tokens.length === 0) return text;
  const pattern = new RegExp(`(${tokens.map((token) => token.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")).join("|")})`, "gi");
  return text.split(pattern).map((part, index) => index % 2 === 1 ? <mark key={`${index}:${part}`}>{part}</mark> : part);
}

function scoreText(score: number): string {
  return score.toFixed(2);
}

// --- cards ------------------------------------------------------------------

interface ResultCardProps {
  hit: SearchHit;
  query: string;
  selected: boolean;
  onToggle: (key: string) => void;
  onOpen: (hit: SearchHit) => void;
  onInGraph: (hit: SearchHit) => void;
}

function ResultCard({ hit, query, selected, onToggle, onOpen, onInGraph }: ResultCardProps) {
  const key = hitKey(hit);
  switch (hit.kind) {
    case "entity": {
      const snippet = hit.snippet ?? "";
      return (
        <li className="s-result">
          <input
            type="checkbox" className="s-result__checkbox"
            checked={selected} aria-label={`Select ${hit.name}`}
            onChange={() => onToggle(key)}
          />
          <div className="s-result__main">
            <div className="s-result__title-row">
              <button type="button" className="s-result__title" onClick={() => onOpen(hit)}>
                {hit.name}
              </button>
              <Tag dotColor={typeColor(hit.entityType)} className="s-result__tag">{hit.entityType}</Tag>
            </div>
            {snippet && <p className="s-result__snippet">{highlighted(snippet, query)}</p>}
            <span className="s-result__meta">node</span>
          </div>
          <div className="s-result__side">
            {hit.score !== undefined && <span className="s-score">{scoreText(hit.score)}</span>}
            <div className="s-result__actions">
              <Button size="sm" onClick={() => onOpen(hit)}>Open</Button>
              <Button size="sm" variant="ghost" onClick={() => onInGraph(hit)}>In graph</Button>
            </div>
          </div>
        </li>
      );
    }
    case "relation": {
      const snippet = hit.snippet ?? "";
      return (
        <li className="s-result">
          <input
            type="checkbox" className="s-result__checkbox"
            checked={selected} aria-label={`Select ${hit.from} —${hit.relationType}→ ${hit.to}`}
            onChange={() => onToggle(key)}
          />
          <div className="s-result__main">
            <div className="s-result__title-row">
              <button type="button" className="s-result__title" onClick={() => onOpen(hit)}>
                {hit.from}
                <span className="s-result__arrow">—{hit.relationType}→</span>
                {hit.to}
              </button>
              <Tag className="s-result__tag">{hit.relationType}</Tag>
            </div>
            {snippet && <p className="s-result__snippet">{highlighted(snippet, query)}</p>}
            <span className="s-result__meta">relation</span>
          </div>
          <div className="s-result__side">
            {hit.score !== undefined && <span className="s-score">{scoreText(hit.score)}</span>}
            <div className="s-result__actions">
              <Button size="sm" onClick={() => onOpen(hit)}>Open</Button>
              <Button size="sm" variant="ghost" onClick={() => onInGraph(hit)}>In graph</Button>
            </div>
          </div>
        </li>
      );
    }
    case "attachment":
      return (
        <li className="s-result">
          <input
            type="checkbox" className="s-result__checkbox"
            checked={selected} aria-label={`Select ${hit.filename}`}
            onChange={() => onToggle(key)}
          />
          <div className="s-result__main">
            <div className="s-result__title-row">
              <button type="button" className="s-result__title" onClick={() => onOpen(hit)}>
                {hit.filename}
              </button>
              <Tag className="s-result__tag">{hit.entityName}</Tag>
              <Tag className="s-result__tag"><FileText size={12} aria-hidden="true" />p.{hit.page}</Tag>
            </div>
            <p className="s-result__snippet">{highlighted(hit.excerpt, query)}</p>
            <span className="s-result__meta">file</span>
          </div>
          <div className="s-result__side">
            <span className="s-score">{scoreText(hit.score)}</span>
            <div className="s-result__actions">
              <Button size="sm" onClick={() => onOpen(hit)}>Open file</Button>
              <Button size="sm" variant="ghost" onClick={() => onInGraph(hit)}>In graph</Button>
            </div>
          </div>
        </li>
      );
  }
}

// --- the results area --------------------------------------------------------

interface ResultListProps {
  params: SearchParams;
  state: ResultsState;
  selected: ReadonlySet<string>;
  onToggle: (key: string) => void;
  onToggleAll: () => void;
  onOpen: (hit: SearchHit) => void;
  onInGraph: (hit: SearchHit) => void;
  onShowInGraph: () => void;
  onRetry: () => void;
  onUseDirect: () => void;
  onRequestScope: () => void;
}

export function ResultList({
  params, state, selected, onToggle, onToggleAll, onOpen, onInGraph,
  onShowInGraph, onRetry, onUseDirect, onRequestScope,
}: ResultListProps) {
  const results = state.data?.results ?? [];
  const allKeys = results.map(hitKey);
  const allSelected = allKeys.length > 0 && allKeys.every((key) => selected.has(key));

  const meta = state.status === "loading"
    ? "Searching…"
    : state.data
      ? `${state.data.count} result${state.data.count === 1 ? "" : "s"} · ${params.mode} · ${state.data.elapsedMs} ms`
      : null;

  if (state.status === "idle") {
    return (
      <section className="s-results" aria-label="Search results">
        <div className="s-panel" role="status">
          <h2>Search the workspace</h2>
          <p>Enter a query to search node names, relation observations, and — with consent — attached files.</p>
        </div>
      </section>
    );
  }
  if (state.status === "unavailable") {
    return (
      <section className="s-results" aria-label="Search results">
        <div className="s-panel s-panel--unavailable" role="status">
          <h2>Vector search unavailable</h2>
          <p>{state.unavailable}</p>
          <div className="s-panel__actions">
            <Button size="sm" onClick={onUseDirect}>Use Direct search</Button>
          </div>
        </div>
      </section>
    );
  }
  if (state.status === "error") {
    const error = state.error;
    return (
      <section className="s-results" aria-label="Search results">
        <div className="s-panel s-panel--error" role="alert">
          <h2>Search failed</h2>
          <p className="s-panel__code">{error ? `${error.code}: ${error.message}` : "The search could not be completed."}</p>
          <div className="s-panel__actions">
            <Button size="sm" onClick={onRetry}>Retry</Button>
            {error?.code === "insufficient_scope" && canAuthorize("graph") && (
              <Button size="sm" variant="ghost" onClick={onRequestScope}>Request vectors access</Button>
            )}
          </div>
        </div>
      </section>
    );
  }

  return (
    <section className="s-results" aria-label="Search results">
      <div className="s-results__bar">
        <span className="s-results__meta" aria-live="polite">{meta}</span>
        <div className="s-results__bulk">
          <Button size="sm" variant="ghost" disabled={allKeys.length === 0} onClick={onToggleAll}>
            {allSelected ? "Clear selection" : "Select all"}
          </Button>
          <Button size="sm" disabled={selected.size === 0} onClick={onShowInGraph}>
            Show {selected.size} in graph
          </Button>
        </div>
      </div>
      {state.data === null ? (
        <div className="s-panel" role="status">
          <p>Searching…</p>
        </div>
      ) : results.length === 0 ? (
        // A data-less state is only "loading" (cold mount or retry); the
        // verdict is unknown until the request settles, so the definitive
        // empty panel must not render while it is in flight.
        <div className="s-panel" role="status">
          <h2>No results for “{params.q}”</h2>
          <p>Try a different query, scope, or type filter.</p>
        </div>
      ) : (
        <ul className="s-result-list">
          {results.map((hit) => {
            const key = hitKey(hit);
            return (
              <ResultCard
                key={key}
                hit={hit}
                query={params.q}
                selected={selected.has(key)}
                onToggle={onToggle}
                onOpen={onOpen}
                onInGraph={onInGraph}
              />
            );
          })}
        </ul>
      )}
    </section>
  );
}