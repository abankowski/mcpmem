import { useEffect, useMemo, useRef, useState } from "react";
import { ApiError, api } from "../../lib/api";
import { canAuthorize, requestConsent } from "../../lib/auth";
import { useShell } from "../../lib/app-context";
import { recordRecentNode } from "../../components/CommandPalette";
import { Sheet } from "../../components/Sheet";
import { useToast } from "../../components/Toast";
import type { Attachment, SearchHit, TypeList } from "../../lib/schemas";
import { selectInGraph } from "../graph/page";
import { Inspector } from "../graph/inspector/inspector";
import { EdgeInspector } from "../graph/inspector/edge-inspector";
import type { EdgeRef } from "../graph/session-state";
import { Lightbox } from "../files/lightbox";
import { SearchControls } from "./controls";
import { ResultList, freshResults, hitKey, hitNames, type ResultsState } from "./results";
import { applyParams, readParams, serializeParams, type SearchParams } from "./state";
import "./search.css";

const FEATURE_OFF_REASON =
  "The server does not enable the vectors category, so it cannot embed a query. " +
  "Enable it with --enable-vectors or --enable-all, or use Direct search.";

/** The right-sheet panel: a node inspector or one relation inspector. */
type OpenPanel =
  | { kind: "node"; name: string }
  | { kind: "relation"; triple: EdgeRef; fromName: string };

/** The Search screen: URL-held query and controls, results, and the open actions. */
export function Page() {
  const { workspace, session } = useShell();
  const notify = useToast();
  const workspaceId = workspace?.workspaceId ?? null;
  const vectorsOn = session?.features.vectors ?? false;
  const canWrite = useMemo(() => {
    if (!session) return false;
    if (!session.scopes.includes("graph-write")) return false;
    const role = session.workspaceRole;
    return role === "owner" || role === "writer";
  }, [session]);

  const [params, setParams] = useState<SearchParams>(() => readParams(location.search));
  const [types, setTypes] = useState<TypeList | null>(null);
  const [results, setResults] = useState<ResultsState>(freshResults);
  const [selected, setSelected] = useState<ReadonlySet<string>>(new Set());
  const [refreshEpoch, setRefreshEpoch] = useState(0);
  const [panel, setPanel] = useState<OpenPanel | null>(null);
  const [filePreview, setFilePreview] = useState<{ attachment: Attachment; page: number } | null>(null);
  const fileBusy = useRef(false);

  // The workspace type catalogue feeds the type dropdown. A failure keeps
  // the "Any type" option; the catalogue is a convenience, not a gate.
  useEffect(() => {
    if (!workspaceId) return;
    const controller = new AbortController();
    let active = true;
    setTypes(null);
    void api.types(workspaceId, controller.signal)
      .then((result) => { if (active) setTypes(result); })
      .catch(() => { if (active && !controller.signal.aborted) setTypes(null); });
    return () => { active = false; controller.abort(); };
  }, [workspaceId]);

  useEffect(() => {
    if (!workspaceId) return;
    if (!params.q) {
      setResults(freshResults());
      return;
    }
    if (params.mode !== "direct" && !vectorsOn) {
      // The feature gate is known without a request: a missing profile or
      // provider surfaces as a 503 later, through the same unavailable state.
      setResults({ status: "unavailable", data: null, error: null, unavailable: FEATURE_OFF_REASON });
      return;
    }
    const controller = new AbortController();
    let active = true;
    setResults((previous) => ({ ...previous, status: "loading", error: null, unavailable: null }));
    void api.search(
      {
        workspaceId,
        q: params.q,
        mode: params.mode,
        scope: params.scope,
        type: params.scope === "nodes" ? params.type || undefined : undefined,
        from: params.from || undefined,
        to: params.to || undefined,
        relationType: params.scope === "relations" ? params.relationType || undefined : undefined,
        k: params.k,
      },
      controller.signal,
    )
      .then((data) => {
        if (!active) return;
        setResults({ status: "done", data, error: null, unavailable: null });
        // Drop selection entries that the fresh results no longer contain.
        const keys = new Set(data.results.map(hitKey));
        setSelected((previous) => {
          const kept = new Set([...previous].filter((key) => keys.has(key)));
          return kept.size === previous.size ? previous : kept;
        });
      })
      .catch((cause: unknown) => {
        if (!active || controller.signal.aborted) return;
        const error = cause instanceof ApiError ? cause : new ApiError(0, "network_error", "The search could not be completed.");
        if (error.status === 503) {
          setResults({ status: "unavailable", data: null, error: null, unavailable: error.message });
        } else {
          setResults({ status: "error", data: null, error, unavailable: null });
        }
      });
    return () => { active = false; controller.abort(); };
  }, [workspaceId, params, vectorsOn, refreshEpoch]);

  // Back and forward navigate the URL state; re-read it and refetch.
  useEffect(() => {
    const onPopState = () => {
      const next = readParams(location.search);
      if (serializeParams(next) !== serializeParams(params)) setParams(next);
    };
    window.addEventListener("popstate", onPopState);
    return () => window.removeEventListener("popstate", onPopState);
  }, [params]);

  function commit(patch: Partial<SearchParams>, replace: boolean): void {
    const next = { ...params, ...patch };
    if (serializeParams(next) === serializeParams(params)) return;
    applyParams(next, replace);
    setParams(next);
  }

  const refresh = () => setRefreshEpoch((epoch) => epoch + 1);

  function toggle(key: string): void {
    setSelected((previous) => {
      const next = new Set(previous);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      return next;
    });
  }

  function toggleAll(): void {
    const keys = results.data?.results.map(hitKey) ?? [];
    setSelected((previous) => {
      const all = keys.length > 0 && keys.every((key) => previous.has(key));
      return all ? new Set() : new Set(keys);
    });
  }

  const resultsByKey = useMemo(() => {
    const map = new Map<string, SearchHit>();
    for (const hit of results.data?.results ?? []) map.set(hitKey(hit), hit);
    return map;
  }, [results]);

  function showSelectedInGraph(): void {
    const names = new Set<string>();
    for (const key of selected) {
      const hit = resultsByKey.get(key);
      if (hit) for (const name of hitNames(hit)) names.add(name);
    }
    if (names.size === 0) return;
    selectInGraph([...names], 1);
  }

  async function ensureFileConsent(): Promise<boolean> {
    const scopes = session?.scopes ?? [];
    if (scopes.includes("attachments")) return true;
    if (!session?.features.attachments) {
      notify("error", "File actions need the attachments category, which this server does not enable.");
      return false;
    }
    if (!canAuthorize("graph")) {
      notify("error", "File actions need the attachments scope, which a static bearer cannot grant.");
      return false;
    }
    // The flow navigates the browser; the fresh session returns with the scope.
    await requestConsent(["attachments"]);
    return false;
  }

  async function openFile(hit: SearchHit & { kind: "attachment" }): Promise<void> {
    if (!workspaceId || fileBusy.current) return;
    fileBusy.current = true;
    try {
      if (!(await ensureFileConsent())) return;
      const detail = await api.attachment(workspaceId, hit.attachmentId);
      setFilePreview({ attachment: detail, page: hit.page });
    } catch (cause) {
      notify("error", cause instanceof Error ? cause.message : "The file could not be opened.");
    } finally {
      fileBusy.current = false;
    }
  }

  function openHit(hit: SearchHit): void {
    switch (hit.kind) {
      case "entity":
        if (workspaceId) recordRecentNode(workspaceId, hit.name);
        setPanel({ kind: "node", name: hit.name });
        break;
      case "relation":
        if (workspaceId) recordRecentNode(workspaceId, hit.from);
        setPanel({ kind: "relation", triple: { from: hit.from, to: hit.to, relationType: hit.relationType }, fromName: hit.from });
        break;
      case "attachment":
        void openFile(hit);
        break;
    }
  }

  function inGraph(hit: SearchHit): void {
    if (hit.kind === "attachment") {
      void ensureFileConsent().then((ok) => { if (ok) selectInGraph(hitNames(hit)); });
      return;
    }
    selectInGraph(hitNames(hit));
  }

  return (
    <div className="s-page">
      <h1 className="s-heading">Search</h1>
      <SearchControls
        params={params}
        types={types}
        vectorsOn={vectorsOn}
        onChange={(patch) => commit(patch, true)}
        onSubmitQuery={(q) => commit({ q }, false)}
      />
      <ResultList
        params={params}
        state={results}
        selected={selected}
        onToggle={toggle}
        onToggleAll={toggleAll}
        onOpen={openHit}
        onInGraph={inGraph}
        onShowInGraph={showSelectedInGraph}
        onRetry={refresh}
        onUseDirect={() => commit({ mode: "direct" }, true)}
        onRequestScope={() => { void requestConsent(["vectors"]); }}
      />

      {panel?.kind === "node" && workspaceId && (
        <Sheet open title={`Node · ${panel.name}`} onClose={() => setPanel(null)}>
          <div className="s-panel-host">
            <Inspector
              key={panel.name}
              workspaceId={workspaceId}
              name={panel.name}
              canWrite={canWrite}
              reloadKey={refreshEpoch}
              onViewChange={(selection) => {
                if (!selection) return;
                if (selection.kind === "node") setPanel({ kind: "node", name: selection.name });
                else setPanel({
                  kind: "relation",
                  triple: { from: selection.from, to: selection.to, relationType: selection.relationType },
                  fromName: panel.name,
                });
              }}
              onNodeSelected={(name) => setPanel({ kind: "node", name })}
              onNodeGone={() => setPanel(null)}
              onChanged={refresh}
            />
          </div>
        </Sheet>
      )}
      {panel?.kind === "relation" && workspaceId && (
        <Sheet
          open
          title={`${panel.triple.from} —${panel.triple.relationType}→ ${panel.triple.to}`}
          onClose={() => setPanel(null)}
        >
          <div className="s-panel-host">
            <EdgeInspector
              key={`${panel.triple.from}\u001f${panel.triple.to}\u001f${panel.triple.relationType}`}
              workspaceId={workspaceId}
              triple={panel.triple}
              canWrite={canWrite}
              reloadKey={refreshEpoch}
              onBack={() => setPanel({ kind: "node", name: panel.fromName })}
              onSelectNode={(name) => setPanel({ kind: "node", name })}
              onTripleChanged={(triple) => setPanel({ kind: "relation", triple, fromName: panel.fromName })}
              onChanged={refresh}
            />
          </div>
        </Sheet>
      )}
      {filePreview && workspaceId && (
        <Lightbox
          workspaceId={workspaceId}
          attachment={filePreview.attachment}
          initialPage={filePreview.page}
          onClose={() => setFilePreview(null)}
        />
      )}
    </div>
  );
}