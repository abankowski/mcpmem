import { useEffect, useState } from "react";
import { useSyncExternalStore } from "react";
import { Folder, List, ScanText } from "lucide-react";
import { api, ApiError } from "../../../lib/api";
import type { NodeDetail } from "../../../lib/schemas";
import { FilesPanel } from "../../files/page";
import { isPinned, snapshot, subscribe, togglePinned, type EdgeRef, type Selection } from "../session-state";
import { EdgeInspector } from "./edge-inspector";
import { NodeTab } from "./node-tab";
import { RelationsTab } from "./relations-tab";
import "../graph.css";

export interface InspectorProps {
  workspaceId: string;
  name: string;
  canWrite: boolean;
  /** Bump to refresh the node detail after a mutation anywhere on the page. */
  reloadKey?: number;
  onViewChange?: (selection: Selection | null) => void;
  onNodeSelected?: (name: string) => void;
  onNodeGone?: () => void;
  onChanged?: () => void;
  onExpand?: (name: string) => void;
  onIsolate?: (name: string) => void;
  onEdgeHover?: (edge: EdgeRef | null) => void;
}

type Tab = "node" | "relations" | "files";

function toApiError(cause: unknown): ApiError {
  return cause instanceof ApiError ? cause : new ApiError(0, "network_error", "The node could not be loaded.");
}

/**
 * The node inspector: observations, attributes, metadata, and the kebab
 * actions on the Node tab; the All/Out/In relation list with row-to-edge
 * hover on the Relations tab; the Files panel on the Files tab. Search
 * mounts it inside a sheet; the Graph page renders it in the right rail and
 * mirrors view changes to the canvas through `onViewChange`.
 */
export function Inspector({
  workspaceId, name, canWrite, reloadKey = 0, onViewChange, onNodeSelected, onNodeGone,
  onChanged, onExpand, onIsolate, onEdgeHover,
}: InspectorProps) {
  const [tab, setTab] = useState<Tab>("node");
  const [detail, setDetail] = useState<NodeDetail | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<ApiError | null>(null);
  const [reloadTick, setReloadTick] = useState(0);
  const [relationView, setRelationView] = useState<EdgeRef | null>(null);
  const [fileCount, setFileCount] = useState(0);
  useSyncExternalStore(subscribe, snapshot);
  const pinned = isPinned(workspaceId, name);

  useEffect(() => {
    const controller = new AbortController();
    let active = true;
    setDetail(null);
    setLoading(true);
    setError(null);
    setTab("node");
    setRelationView(null);
    void api.node(workspaceId, name, controller.signal)
      .then((result) => {
        if (active) setDetail(result);
      })
      .catch((cause: unknown) => {
        if (active && !controller.signal.aborted) setError(toApiError(cause));
      })
      .finally(() => {
        if (active) setLoading(false);
      });
    return () => { active = false; controller.abort(); };
  }, [workspaceId, name, reloadKey, reloadTick]);

  function retry(): void {
    setReloadTick((tick) => tick + 1);
  }

  function openRelation(triple: EdgeRef): void {
    setRelationView(triple);
    onViewChange?.({ kind: "relation", ...triple });
  }

  function backToNode(): void {
    setRelationView(null);
    onViewChange?.({ kind: "node", name });
  }

  function openNode(nextName: string): void {
    setRelationView(null);
    onViewChange?.({ kind: "node", name: nextName });
    onNodeSelected?.(nextName);
  }

  if (relationView) {
    return (
      <div className="g-inspector">
        <EdgeInspector
          workspaceId={workspaceId}
          triple={relationView}
          canWrite={canWrite}
          reloadKey={reloadKey}
          onBack={backToNode}
          onSelectNode={openNode}
          onTripleChanged={(triple) => { setRelationView(triple); onViewChange?.({ kind: "relation", ...triple }); }}
          onChanged={() => { onChanged?.(); }}
        />
      </div>
    );
  }

  return (
    <div className="g-inspector">
      <div className="g-inspector__tabs" role="tablist" aria-label="Inspector sections">
        <button
          type="button" role="tab" aria-selected={tab === "node"} className="g-inspector__tab"
          onClick={() => setTab("node")}
        >
          <ScanText size={14} aria-hidden="true" />Node
        </button>
        <button
          type="button" role="tab" aria-selected={tab === "relations"} className="g-inspector__tab"
          onClick={() => setTab("relations")}
        >
          <List size={14} aria-hidden="true" />
          Relations {detail && detail.relations.length > 0 ? `(${detail.relations.length})` : ""}
        </button>
        <button
          type="button" role="tab" aria-selected={tab === "files"} className="g-inspector__tab"
          onClick={() => setTab("files")}
        >
          <Folder size={14} aria-hidden="true" />
          Files {fileCount > 0 ? `(${fileCount})` : ""}
        </button>
      </div>
      {tab === "node" && (
        <NodeTab
          workspaceId={workspaceId}
          name={name}
          detail={detail}
          loading={loading}
          error={error}
          canWrite={canWrite}
          pinned={pinned}
          onRetry={retry}
          onRenamed={(newName) => { onNodeSelected?.(newName); onChanged?.(); }}
          onMerged={(target) => { onNodeSelected?.(target); onChanged?.(); }}
          onDeleted={() => { onNodeGone?.(); onChanged?.(); }}
          onChanged={onChanged ?? (() => {})}
          onTogglePin={() => togglePinned(workspaceId, name)}
          onExpand={onExpand != null ? () => onExpand(name) : undefined}
          onIsolate={onIsolate != null ? () => onIsolate(name) : undefined}
        />
      )}
      {tab === "relations" && (
        <div className="g-inspector__body">
          {detail ? (
            <RelationsTab
              nodeName={name}
              relations={detail.relations}
              selectedEdge={relationView}
              onSelectRelation={openRelation}
              onEdgeHover={onEdgeHover ?? (() => {})}
            />
          ) : (
            <p className="g-empty">{loading ? "Loading node…" : "The node could not be loaded."}</p>
          )}
        </div>
      )}
      {tab === "files" && (
        <div className="g-inspector__body">
          <FilesPanel
            workspaceId={workspaceId}
            entityName={name}
            canWrite={canWrite}
            onCountChange={setFileCount}
          />
        </div>
      )}
    </div>
  );
}