import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useSyncExternalStore } from "react";
import { Link2, ScanText, X } from "lucide-react";
import { api, ApiError } from "../../lib/api";
import { pageUrl } from "../../lib/urls";
import { useShell } from "../../lib/app-context";
import type { TypeList } from "../../lib/schemas";
import { recordRecentNode } from "../../components/CommandPalette";
import { Button } from "../../components/Button";
import { Sheet } from "../../components/Sheet";
import { useToast } from "../../components/Toast";
import { GraphCanvas, MAX_CANVAS_NODES, cssColor, type CanvasViewModel } from "./canvas";
import { FilterRail } from "./rail";
import { CanvasToolbar } from "./toolbar";
import { Legend } from "./legend";
import { NewMenu } from "./new-menu";
import { EdgeInspector } from "./inspector/edge-inspector";
import { Inspector } from "./inspector/inspector";
import {
  abortStale, clearFilters, edgeKey, filtersFor, initialSelectionFromUrl, pinnedFor,
  setPinned, snapshot, subscribe, toggleHiddenType, type EdgeRef, type GraphFilters,
  type InitialSelection, type Selection,
} from "./session-state";
import "./graph.css";

const PAGE_LIMIT = 1000;
const FALLBACK_PALETTE: readonly string[] = [
  "#f28c4a", "#6b9cf5", "#5fc385", "#b08cf0", "#4fc4c4", "#ee82b0", "#d9b84a", "#a39d91",
];
const EMPTY_FILTERS: GraphFilters = { hiddenEntityTypes: [], hiddenRelationTypes: [] };

interface EntityMeta {
  name: string;
  entityType: string;
  obsCount: number;
}

interface PageData {
  nodes: Map<string, EntityMeta>;
  edges: Map<string, EdgeRef>;
  order: string[];
}

interface BrowseState {
  loading: boolean;
  error: ApiError | null;
  offset: number;
  hasMore: boolean;
}

interface IsolatedSet {
  names: Set<string>;
  edgeKeys: Set<string>;
}

function emptyPageData(): PageData {
  return { nodes: new Map(), edges: new Map(), order: [] };
}

function toApiError(cause: unknown): ApiError {
  return cause instanceof ApiError ? cause : new ApiError(0, "network_error", "The graph could not be loaded.");
}

function mergePageData(target: PageData, page: GraphPageLike): PageData {
  const nodes = new Map(target.nodes);
  const edges = new Map(target.edges);
  const order = [...target.order];
  for (const entity of page.entities) {
    if (!nodes.has(entity.name)) order.push(entity.name);
    nodes.set(entity.name, { name: entity.name, entityType: entity.entityType, obsCount: entity.obsCount });
  }
  for (const relation of page.relations) edges.set(edgeKey(relation), relation);
  return { nodes, edges, order };
}

type GraphPageLike = {
  entities: readonly { name: string; entityType: string; obsCount: number }[];
  relations: readonly EdgeRef[];
};

function freshPageData(page: GraphPageLike): PageData {
  return mergePageData(emptyPageData(), page);
}

/** Fold one accumulated in-memory page into another (expand results). */
function mergeDataData(target: PageData, extra: PageData): PageData {
  const nodes = new Map(target.nodes);
  const edges = new Map(target.edges);
  const order = [...target.order];
  for (const [name, meta] of extra.nodes) {
    if (!nodes.has(name)) order.push(name);
    nodes.set(name, meta);
  }
  for (const [key, edge] of extra.edges) edges.set(key, edge);
  return { nodes, edges, order };
}

/**
 * Keep the selection neighbourhood, pinned nodes, and the isolated set when
 * a replace refresh drops everything that is not on the current page. Their
 * data came from recent expand / page fetches; dropping them would silently
 * shrink the canvas around what the user is looking at.
 */
function keepStable(base: PageData, previous: PageData, namesToKeep: readonly string[]): PageData {
  const kept = new Map<string, EntityMeta>();
  const keptEdges = new Map<string, EdgeRef>();
  for (const name of namesToKeep) {
    const meta = previous.nodes.get(name);
    if (meta) kept.set(name, meta);
  }
  for (const relation of previous.edges.values()) {
    if (namesToKeep.includes(relation.from) || namesToKeep.includes(relation.to)) {
      const fromMeta = previous.nodes.get(relation.from);
      const toMeta = previous.nodes.get(relation.to);
      if (fromMeta) kept.set(relation.from, fromMeta);
      if (toMeta) kept.set(relation.to, toMeta);
      keptEdges.set(edgeKey(relation), relation);
    }
  }
  const nodes = new Map(base.nodes);
  const edges = new Map(base.edges);
  const order = [...base.order];
  for (const [name, meta] of kept) {
    if (!nodes.has(name)) order.push(name);
    nodes.set(name, meta);
  }
  for (const [key, edge] of keptEdges) edges.set(key, edge);
  return { nodes, edges, order };
}

interface ExpandLike {
  entities: readonly { name: string; entityType: string; observations?: readonly unknown[] }[];
  relations: readonly EdgeRef[];
}

/** Fold one expand result into the accumulating page data and the isolated set. */
function absorbExpand(acc: PageData, names: Set<string>, edgeKeysOut: Set<string>, result: ExpandLike): void {
  for (const entity of result.entities) {
    names.add(entity.name);
    if (!acc.nodes.has(entity.name)) acc.order.push(entity.name);
    acc.nodes.set(entity.name, {
      name: entity.name,
      entityType: entity.entityType,
      obsCount: entity.observations?.length ?? 0,
    });
  }
  for (const relation of result.relations) {
    edgeKeysOut.add(edgeKey(relation));
    acc.edges.set(edgeKey(relation), relation);
  }
}

/** The Graph screen. The shell mounts it once per workspace. */
export function Page() {
  const { workspace, session } = useShell();
  const notify = useToast();
  const workspaceId = workspace?.workspaceId ?? null;
  const initialRef = useRef<InitialSelection | null>(initialSelectionFromUrl());
  const initial = initialRef.current;

  const canWrite = useMemo(() => {
    if (!session) return false;
    if (!session.scopes.includes("graph-write")) return false;
    const role = session.workspaceRole;
    return role === "owner" || role === "writer";
  }, [session]);

  useSyncExternalStore(subscribe, snapshot);
  const pinned = workspaceId ? pinnedFor(workspaceId) : [];
  const filters = workspaceId ? filtersFor(workspaceId) : EMPTY_FILTERS;

  const [pageData, setPageData] = useState<PageData>(emptyPageData);
  const [browse, setBrowse] = useState<BrowseState>({ loading: false, error: null, offset: 0, hasMore: false });
  const [types, setTypes] = useState<TypeList | null>(null);
  const [selection, setSelection] = useState<Selection | null>(() => (initial ? { kind: "node", name: initial.names[0] } : null));
  const [hoverEdge, setHoverEdge] = useState<EdgeRef | null>(null);
  const [depth, setDepth] = useState<1 | 2 | 3>(initial?.depth ?? 1);
  const [isolated, setIsolated] = useState<IsolatedSet | null>(null);
  const [isolateOn, setIsolateOn] = useState(initial?.isolate ?? false);
  const [connectMode, setConnectMode] = useState(false);
  const [connect, setConnect] = useState<{ source: string; target: string | null } | null>(null);
  const [connectTypeOpen, setConnectTypeOpen] = useState(false);
  const [connectType, setConnectType] = useState("");
  const [createNodeOpen, setCreateNodeOpen] = useState(false);
  const [newNodeName, setNewNodeName] = useState("");
  const [newNodeType, setNewNodeType] = useState("");
  const [newNodeObs, setNewNodeObs] = useState("");
  const [observeOpen, setObserveOpen] = useState(false);
  const [observeBody, setObserveBody] = useState("");
  const [mutationEpoch, setMutationEpoch] = useState(0);
  const [dataEpoch, setDataEpoch] = useState(0);
  const [busy, setBusy] = useState(false);

  const pageDataRef = useRef<PageData>(emptyPageData());
  pageDataRef.current = pageData;
  const isolatedRef = useRef<IsolatedSet | null>(null);
  isolatedRef.current = isolated;
  const pinnedRef = useRef<readonly string[]>([]);
  pinnedRef.current = pinned;
  const selectionRef = useRef<Selection | null>(null);
  selectionRef.current = selection;
  const replaceRef = useRef(true);
  const fitDoneRef = useRef(false);
  const canvasRef = useRef<GraphCanvas | null>(null);
  const containerRef = useRef<HTMLDivElement | null>(null);
  const lastNodeRef = useRef<string | null>(initial ? initial.names[0] : null);

  // Consume the URL selection once; a later workspace switch must not re-seed it.
  useEffect(() => {
    if (!initial) return;
    const params = new URLSearchParams(location.search);
    if (!params.has("node") && !params.has("nodes")) return;
    params.delete("node");
    params.delete("nodes");
    params.delete("depth");
    params.delete("isolate");
    const query = params.toString();
    history.replaceState(history.state, "", location.pathname + (query ? `?${query}` : ""));
  }, [initial]);

  // --- page-level fetches -----------------------------------------------------------
  // The browse and types loads use their own controllers (an effect re-run
  // cancels only its own previous load). The transient loads — double-click
  // expand, bring-in, isolate — share the session generation controller:
  // abortStale() on a selection or workspace change cancels exactly those.

  const transientRef = useRef(0);

  function transientSignal(): AbortSignal {
    transientRef.current += 1;
    return abortStale();
  }

  function transientEnd(): void {
    transientRef.current -= 1;
  }

  useEffect(() => {
    if (!workspaceId) return;
    const controller = new AbortController();
    let active = true;
    setBrowse((previous) => ({ ...previous, loading: true, error: null }));
    void api.graph({ workspaceId, offset: browse.offset, limit: PAGE_LIMIT }, controller.signal)
      .then((page) => {
        if (!active) return;
        let merged: PageData;
        if (replaceRef.current) {
          replaceRef.current = false;
          const namesToKeep: string[] = [...pinnedRef.current];
          const selected = selectionRef.current;
          if (selected?.kind === "node") namesToKeep.push(selected.name);
          namesToKeep.push(...(isolatedRef.current?.names ?? []));
          merged = keepStable(freshPageData(page), pageDataRef.current, namesToKeep);
        } else {
          merged = mergePageData(pageDataRef.current, page);
        }
        pageDataRef.current = merged;
        setPageData(merged);
        setBrowse({ loading: false, error: null, offset: page.page.offset, hasMore: page.page.hasMore });
        if (!fitDoneRef.current && merged.order.length > 0) {
          fitDoneRef.current = true;
          canvasRef.current?.fit();
        }
        if (initial && !initial.isolate) {
          const missing = initial.names.filter((name) => !merged.nodes.has(name));
          if (missing.length > 0) void bringIn(missing);
        }
      })
      .catch((cause: unknown) => {
        if (active && !controller.signal.aborted) {
          setBrowse({ loading: false, error: toApiError(cause), offset: browse.offset, hasMore: false });
        }
      });
    return () => { active = false; controller.abort(); };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [workspaceId, browse.offset, dataEpoch, initial]);

  useEffect(() => {
    if (!workspaceId) return;
    const controller = new AbortController();
    let active = true;
    void api.types(workspaceId, controller.signal)
      .then((result) => { if (active) setTypes(result); })
      .catch(() => { if (active && !controller.signal.aborted) setTypes(null); });
    return () => { active = false; controller.abort(); };
  }, [workspaceId, dataEpoch]);

  /** Fetch a selected node's one-hop neighbourhood when it is not on the loaded pages. */
  function bringIn(names: readonly string[]): Promise<void> {
    if (!workspaceId) return Promise.resolve();
    const signal = transientSignal();
    const acc = emptyPageData();
    return Promise.all(names.map((name) =>
      api.expand({ workspaceId, name, depth: 1, direction: "both" }, signal)
        .then((result) => absorbExpand(acc, new Set(), new Set(), result))
        .catch(() => {}),
    ))
      .then(() => {
        if (signal.aborted) return;
        setPageData((previous) => mergeDataData(previous, acc));
      })
      .finally(() => transientEnd());
  }

  // Initial multi-select from Search: an isolated depth-N set around each name.
  useEffect(() => {
    if (!workspaceId || !initial || !initial.isolate) return;
    const signal = transientSignal();
    let active = true;
    const acc = emptyPageData();
    const namesSet = new Set<string>();
    const edgeKeysOut = new Set<string>();
    void Promise.all(initial.names.map((name) =>
      api.expand({ workspaceId, name, depth: initial.depth, direction: "both" }, signal)
        .then((result) => { if (active) absorbExpand(acc, namesSet, edgeKeysOut, result); })
        .catch(() => {}),
    ))
      .then(() => {
        if (!active || signal.aborted) return;
        pageDataRef.current = mergeDataData(pageDataRef.current, acc);
        setPageData(pageDataRef.current);
        setIsolated({ names: namesSet, edgeKeys: edgeKeysOut });
        setIsolateOn(true);
        requestAnimationFrame(() => canvasRef.current?.fit());
      })
      .finally(() => transientEnd());
    return () => { active = false; };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [workspaceId, initial]);

  // --- canvas wiring ------------------------------------------------------------------

  const palette = useMemo(() => FALLBACK_PALETTE.map((hex, index) => cssColor(`--node-${index + 1}`, hex)), []);

  const typePalette = useMemo(() => {
    const colors = new Map<string, string>();
    const hollow = new Set<string>();
    const sorted = [...(types?.entities ?? [])].sort((a, b) => b.count - a.count);
    sorted.forEach((entry, index) => {
      colors.set(entry.type, palette[index % palette.length]);
      if (index >= palette.length) hollow.add(entry.type);
    });
    return { colors, hollow };
  }, [types, palette]);

  const vm: CanvasViewModel | null = useMemo(() => {
    if (!workspaceId) return null;
    let nodes = pageData.order.map((name) => pageData.nodes.get(name)!);
    let edges = [...pageData.edges.values()];
    if (isolateOn && isolated) {
      nodes = nodes.filter((node) => isolated.names.has(node.name));
      edges = edges.filter((edge) => isolated.edgeKeys.has(edgeKey(edge)));
    }
    const hiddenRelations = new Set(filters.hiddenRelationTypes);
    edges = edges.filter((edge) => !hiddenRelations.has(edge.relationType));
    const degree = new Map<string, number>();
    for (const edge of edges) {
      degree.set(edge.from, (degree.get(edge.from) ?? 0) + 1);
      degree.set(edge.to, (degree.get(edge.to) ?? 0) + 1);
    }
    const hiddenEntities = new Set(filters.hiddenEntityTypes);
    const dimmed = new Set<string>();
    for (const node of nodes) {
      if (hiddenEntities.has(node.entityType)) dimmed.add(node.name);
    }
    const selectedName = selection?.kind === "node" ? selection.name : null;
    if (nodes.length > MAX_CANVAS_NODES) {
      const preferred = new Set<string>();
      if (selectedName) {
        preferred.add(selectedName);
        for (const edge of edges) {
          if (edge.from === selectedName) preferred.add(edge.to);
          if (edge.to === selectedName) preferred.add(edge.from);
        }
      }
      nodes = nodes
        .filter((node) => preferred.has(node.name))
        .concat(nodes.filter((node) => !preferred.has(node.name)))
        .slice(0, MAX_CANVAS_NODES);
    }
    return {
      nodes: nodes.map((node) => ({ ...node, degree: degree.get(node.name) ?? 0 })),
      edges,
      typeColors: typePalette.colors,
      hollowTypes: typePalette.hollow,
      selectedEdge: selection?.kind === "relation" ? selection : null,
      hoverEdge,
      selectedName,
      dimmed,
      connectPick: connectMode,
      connectSource: connect?.source ?? null,
    };
  }, [pageData, isolateOn, isolated, filters, typePalette, selection, hoverEdge, connectMode, connect, workspaceId]);

  const vmRef = useRef<CanvasViewModel | null>(null);
  vmRef.current = vm;

  const callbacksRef = useRef<Callbacks>({
    selectNode: () => {}, selectEdge: () => {}, deselect: () => {}, hoverEdge: () => {},
    expandNode: () => {}, connectNode: () => {}, nodePinned: () => {},
  });

  useEffect(() => {
    const container = containerRef.current;
    if (!container) return;
    const canvas = new GraphCanvas(container, {
      onSelectNode: (name) => callbacksRef.current.selectNode(name),
      onSelectEdge: (edge) => callbacksRef.current.selectEdge(edge),
      onDeselect: () => callbacksRef.current.deselect(),
      onHoverEdge: (edge) => callbacksRef.current.hoverEdge(edge),
      onExpandNode: (name) => callbacksRef.current.expandNode(name),
      onConnectNode: (name) => callbacksRef.current.connectNode(name),
      onNodePinned: (name) => callbacksRef.current.nodePinned(name),
    });
    canvasRef.current = canvas;
    canvas.setViewModel(vmRef.current);
    return () => {
      canvas.destroy();
      canvasRef.current = null;
    };
  }, []);

  useEffect(() => {
    canvasRef.current?.setViewModel(vm);
  }, [vm]);

  useEffect(() => {
    canvasRef.current?.setPinned(pinned);
  }, [pinned]);

  // --- selection actions ----------------------------------------------------------------

  const ensureAbort = useCallback(() => {
    if (transientRef.current > 0) abortStale();
  }, []);

  const selectNode = useCallback((name: string) => {
    ensureAbort();
    setHoverEdge(null);
    setSelection({ kind: "node", name });
    lastNodeRef.current = name;
    if (workspaceId) recordRecentNode(workspaceId, name);
    canvasRef.current?.focus(name);
  }, [ensureAbort, workspaceId]);

  const selectRelation = useCallback((triple: EdgeRef) => {
    ensureAbort();
    setHoverEdge(null);
    setSelection({ kind: "relation", ...triple });
  }, [ensureAbort]);

  const clearSelection = useCallback(() => {
    setHoverEdge(null);
    setSelection(null);
  }, []);

  const handleViewChange = useCallback((next: Selection | null) => {
    if (!next) return;
    ensureAbort();
    setHoverEdge(null);
    setSelection(next);
    if (next.kind === "node") {
      lastNodeRef.current = next.name;
      canvasRef.current?.focus(next.name);
    }
  }, [ensureAbort]);

  const expandNodeAt = useCallback((name: string) => {
    if (!workspaceId) return;
    const signal = transientSignal();
    setHoverEdge(null);
    void api.expand({ workspaceId, name, depth, direction: "both" }, signal)
      .then((result) => {
        if (signal.aborted) return;
        const acc = emptyPageData();
        absorbExpand(acc, new Set(), new Set(), result);
        pageDataRef.current = mergeDataData(pageDataRef.current, acc);
        setPageData(pageDataRef.current);
        canvasRef.current?.focus(name);
      })
      .catch(() => {})
      .finally(() => transientEnd());
  }, [workspaceId, depth]);

  const isolateNode = useCallback((name: string) => {
    const existing = isolatedRef.current;
    if (existing && existing.names.has(name)) {
      setIsolateOn(true);
      return;
    }
    if (!workspaceId) return;
    const signal = transientSignal();
    void api.expand({ workspaceId, name, depth: 1, direction: "both" }, signal)
      .then((result) => {
        if (signal.aborted) return;
        const namesSet = new Set(existing?.names ?? []);
        const edgeKeysOut = new Set(existing?.edgeKeys ?? []);
        const acc = emptyPageData();
        absorbExpand(acc, namesSet, edgeKeysOut, result);
        pageDataRef.current = mergeDataData(pageDataRef.current, acc);
        setPageData(pageDataRef.current);
        setIsolated({ names: namesSet, edgeKeys: edgeKeysOut });
        setIsolateOn(true);
      })
      .catch(() => {})
      .finally(() => transientEnd());
  }, [workspaceId]);

  const onNodePinned = useCallback((name: string) => {
    if (!workspaceId) return;
    const next = pinned.includes(name) ? pinned : [...pinned, name];
    setPinned(workspaceId, next);
  }, [workspaceId, pinned]);

  callbacksRef.current = {
    selectNode,
    selectEdge: selectRelation,
    deselect: clearSelection,
    hoverEdge: setHoverEdge,
    expandNode: expandNodeAt,
    connectNode: onConnectNodeClick,
    nodePinned: onNodePinned,
  };

  // --- connect mode -----------------------------------------------------------------------

  function onConnectNodeClick(name: string): void {
    if (!connectMode) return;
    if (connect == null) {
      setConnect({ source: name, target: null });
      return;
    }
    if (connect.source === name) return;
    if (connect.target == null) {
      setConnect({ ...connect, target: name });
      setConnectType("");
      setConnectTypeOpen(true);
    }
  }

  function cancelConnect(): void {
    setConnectMode(false);
    setConnect(null);
    setConnectTypeOpen(false);
  }

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      const target = event.target instanceof HTMLElement ? event.target : null;
      const typing = target != null && (
        target.tagName === "INPUT" || target.tagName === "TEXTAREA" || target.tagName === "SELECT" ||
        target.isContentEditable
      );
      if (event.key === "Escape") {
        // A modal dialog owns its Escape (cancel closes it). The window
        // handler must not also clear the selection behind it.
        if (document.querySelector("dialog[open]") != null) return;
        const anyOpen = connectMode || connectTypeOpen || createNodeOpen || observeOpen;
        if (connectMode) cancelConnect();
        if (connectTypeOpen) setConnectTypeOpen(false);
        if (createNodeOpen) setCreateNodeOpen(false);
        if (observeOpen) setObserveOpen(false);
        if (!anyOpen && !typing) clearSelection();
        return;
      }
      // The handoff canvas shortcuts; never fire while the user types.
      if (typing || event.metaKey || event.ctrlKey || event.altKey) return;
      const canvas = canvasRef.current;
      if (!canvas) return;
      if (event.key === "f" || event.key === "F") {
        event.preventDefault();
        canvas.fit();
      } else if (event.key === "+" || event.key === "=") {
        event.preventDefault();
        canvas.zoomIn();
      } else if (event.key === "-" || event.key === "_") {
        event.preventDefault();
        canvas.zoomOut();
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [connectMode, connectTypeOpen, createNodeOpen, observeOpen]);

  async function createRelation(): Promise<void> {
    if (!workspaceId || !connect || connect.target == null) return;
    const relationType = connectType.trim();
    if (!relationType) return;
    setBusy(true);
    try {
      await api.mutate(workspaceId, {
        operation: "createRelation",
        payload: { from: connect.source, to: connect.target, relationType },
      });
      notify("success", `Relation ${connect.source} —${relationType}→ ${connect.target} created.`);
      cancelConnect();
      refreshAfterMutation();
    } catch (cause) {
      notify("error", cause instanceof Error ? cause.message : "The relation could not be created.");
    } finally {
      setBusy(false);
    }
  }

  // --- mutations -----------------------------------------------------------------------------

  const refreshAfterMutation = useCallback(() => {
    replaceRef.current = true;
    setDataEpoch((epoch) => epoch + 1);
    setMutationEpoch((epoch) => epoch + 1);
  }, []);

  async function createNode(): Promise<void> {
    const name = newNodeName.trim();
    const entityType = newNodeType.trim();
    if (!workspaceId || !name || !entityType) return;
    setBusy(true);
    try {
      await api.mutate(workspaceId, {
        operation: "createEntity",
        payload: {
          name,
          entityType,
          observations: newNodeObs.trim() ? [{ body: newNodeObs.trim() }] : [],
        },
      });
      notify("success", `Node ${name} created.`);
      setCreateNodeOpen(false);
      setNewNodeName("");
      setNewNodeObs("");
      refreshAfterMutation();
      requestAnimationFrame(() => selectNode(name));
    } catch (cause) {
      notify("error", cause instanceof Error ? cause.message : "The node could not be created.");
    } finally {
      setBusy(false);
    }
  }

  function openObservation(): void {
    setObserveBody("");
    setObserveOpen(true);
  }

  async function addObservation(): Promise<void> {
    const body = observeBody.trim();
    if (!workspaceId || selection?.kind !== "node" || !body) return;
    setBusy(true);
    try {
      await api.mutate(workspaceId, { operation: "addObservation", payload: { entityName: selection.name, body } });
      notify("success", "Observation added.");
      setObserveOpen(false);
      refreshAfterMutation();
    } catch (cause) {
      notify("error", cause instanceof Error ? cause.message : "The observation could not be added.");
    } finally {
      setBusy(false);
    }
  }

  const connectHint = connect == null
    ? "Connect mode: click the source node."
    : connect.target == null
      ? "Now click the target node."
      : null;

  const pageNumber = Math.floor(browse.offset / PAGE_LIMIT) + 1;
  const relationTypes = types?.relations.map((entry) => entry.type) ?? [];

  return (
    <div className="g-page">
      <div className="g-graph">
        {workspaceId && (
          <FilterRail
            types={types}
            filters={filters}
            pinned={pinned}
            onToggleHidden={(kind, type) => toggleHiddenType(workspaceId, kind, type)}
            onClearFilters={() => clearFilters(workspaceId)}
            onUnpin={(name) => setPinned(workspaceId, pinnedFor(workspaceId).filter((item) => item !== name))}
            onSelectNode={selectNode}
          />
        )}
        <div className="g-canvas-wrap" ref={containerRef}>
          <CanvasToolbar
            depth={depth}
            onDepthChange={setDepth}
            connectActive={connectMode}
            onConnectToggle={connectMode ? cancelConnect : () => { setConnect(null); setConnectMode(true); }}
            onFit={() => canvasRef.current?.fit()}
            onZoomIn={() => canvasRef.current?.zoomIn()}
            onZoomOut={() => canvasRef.current?.zoomOut()}
            onLayout={() => canvasRef.current?.relayout()}
            canConnect={canWrite}
          >
            {canWrite && (
              <NewMenu
                canConnect={!connectMode}
                observationDisabled={selection?.kind !== "node"}
                onNewNode={() => {
                  setNewNodeName("");
                  setNewNodeType(types?.entities[0]?.type ?? "");
                  setNewNodeObs("");
                  setCreateNodeOpen(true);
                }}
                onNewRelation={() => { setConnect(null); setConnectMode(true); }}
                onNewObservation={openObservation}
              />
            )}
          </CanvasToolbar>

          {connectMode && connectHint && (
            <div className="g-connect-hint" role="status">
              <Link2 size={14} aria-hidden="true" />
              {connectHint}
              <Button size="sm" variant="ghost" onClick={cancelConnect}><X size={13} aria-hidden="true" />Cancel</Button>
            </div>
          )}
          {isolateOn && (
            <div className="g-isolate-banner">
              Isolated view
              <Button size="sm" variant="ghost" onClick={() => setIsolateOn(false)}>Exit isolate</Button>
            </div>
          )}

          {browse.error && (
            <div className="g-overlay">
              <div className="g-overlay__card">
                <p>{browse.error.message}</p>
                <Button onClick={() => setDataEpoch((epoch) => epoch + 1)}>Retry</Button>
              </div>
            </div>
          )}

          <div className="g-pager">
            <Button
              size="sm" variant="ghost"
              disabled={browse.offset === 0}
              onClick={() => setBrowse((previous) => ({ ...previous, offset: Math.max(0, previous.offset - PAGE_LIMIT) }))}
            >
              Prev
            </Button>
            <span aria-live="polite">
              {browse.loading ? "Loading…" : `Page ${pageNumber} · ${pageData.order.length.toLocaleString()} nodes`}
            </span>
            <Button
              size="sm" variant="ghost"
              disabled={!browse.hasMore || browse.loading}
              onClick={() => setBrowse((previous) => ({ ...previous, offset: previous.offset + PAGE_LIMIT }))}
            >
              Next
            </Button>
          </div>

          <Legend
            types={types}
            typeColors={typePalette.colors}
            hollowTypes={typePalette.hollow}
            filters={filters}
            onToggle={(kind, type) => workspaceId && toggleHiddenType(workspaceId, kind, type)}
            onShowAll={() => workspaceId && clearFilters(workspaceId)}
          />
        </div>

        {selection?.kind === "node" && workspaceId && (
          <Inspector
            key={selection.name}
            workspaceId={workspaceId}
            name={selection.name}
            canWrite={canWrite}
            reloadKey={mutationEpoch}
            onViewChange={handleViewChange}
            onNodeSelected={selectNode}
            onNodeGone={() => { clearSelection(); refreshAfterMutation(); }}
            onChanged={refreshAfterMutation}
            onExpand={expandNodeAt}
            onIsolate={isolateNode}
            onEdgeHover={setHoverEdge}
          />
        )}
        {selection?.kind === "relation" && workspaceId && (
          <EdgeInspector
            key={`${selection.from}:${selection.to}:${selection.relationType}`}
            workspaceId={workspaceId}
            triple={selection}
            canWrite={canWrite}
            reloadKey={mutationEpoch}
            onBack={() => selectNode(lastNodeRef.current ?? selection.from)}
            onSelectNode={selectNode}
            onTripleChanged={(triple) => setSelection({ kind: "relation", ...triple })}
            onChanged={refreshAfterMutation}
          />
        )}
      </div>

      <Sheet open={createNodeOpen} title="New node" onClose={() => setCreateNodeOpen(false)}>
        <div className="g-field">
          <label htmlFor="g-new-node-name">Name</label>
          <input
            id="g-new-node-name" value={newNodeName} autoFocus
            onChange={(event) => setNewNodeName(event.target.value)}
            placeholder="Node name"
          />
        </div>
        <div className="g-field">
          <label htmlFor="g-new-node-type">Entity type</label>
          <input
            id="g-new-node-type" value={newNodeType}
            onChange={(event) => setNewNodeType(event.target.value)}
            placeholder="entity type"
            list="g-entity-type-options"
          />
          <datalist id="g-entity-type-options">
            {(types?.entities ?? []).map((entry) => <option value={entry.type} key={entry.type} />)}
          </datalist>
        </div>
        <div className="g-field">
          <label htmlFor="g-new-node-obs">First observation (optional)</label>
          <textarea
            id="g-new-node-obs" value={newNodeObs}
            onChange={(event) => setNewNodeObs(event.target.value)}
          />
        </div>
        <div className="g-inline-form__row">
          <Button disabled={!newNodeName.trim() || !newNodeType.trim() || busy} onClick={() => void createNode()}>
            Create node
          </Button>
          <Button variant="ghost" onClick={() => setCreateNodeOpen(false)}>Cancel</Button>
        </div>
      </Sheet>

      <Sheet
        open={observeOpen}
        title={selection?.kind === "node" ? `Add observation to ${selection.name}` : "Add observation"}
        onClose={() => setObserveOpen(false)}
      >
        {selection?.kind !== "node" ? (
          <p className="g-empty">Select a node first.</p>
        ) : (
          <div className="g-field">
            <label htmlFor="g-observe-body">Observation</label>
            <textarea
              id="g-observe-body" value={observeBody} autoFocus
              onChange={(event) => setObserveBody(event.target.value)}
            />
          </div>
        )}
        <div className="g-inline-form__row">
          <Button
            disabled={selection?.kind !== "node" || !observeBody.trim() || busy}
            onClick={() => void addObservation()}
          >
            <ScanText size={14} aria-hidden="true" />Add observation
          </Button>
          <Button variant="ghost" onClick={() => setObserveOpen(false)}>Cancel</Button>
        </div>
      </Sheet>

      <Sheet
        open={connectTypeOpen}
        title={connect ? `Connect ${connect.source} → ${connect.target ?? ""}` : "Connect"}
        onClose={() => cancelConnect()}
      >
        <div className="g-field">
          <label htmlFor="g-connect-type">Relation type</label>
          <input
            id="g-connect-type" value={connectType} autoFocus
            onChange={(event) => setConnectType(event.target.value)}
            placeholder="relation type"
            list="g-connect-type-options"
            onKeyDown={(event) => { if (event.key === "Enter") void createRelation(); }}
          />
          <datalist id="g-connect-type-options">
            {relationTypes.map((option) => <option value={option} key={option} />)}
          </datalist>
          <p className="g-hint">The exact triple is created; an existing one answers 409.</p>
        </div>
        <div className="g-inline-form__row">
          <Button disabled={!connectType.trim() || busy} onClick={() => void createRelation()}>
            Create relation
          </Button>
          <Button variant="ghost" onClick={() => cancelConnect()}>Cancel</Button>
        </div>
      </Sheet>
    </div>
  );
}

interface Callbacks {
  selectNode: (name: string) => void;
  selectEdge: (edge: EdgeRef) => void;
  deselect: () => void;
  hoverEdge: (edge: EdgeRef | null) => void;
  expandNode: (name: string) => void;
  connectNode: (name: string) => void;
  nodePinned: (name: string) => void;
}

/**
 * Open the Graph page with a selection. One name selects that node; several
 * names open an isolated depth-N set around each of them. Called by Search;
 * the current workspace binds the set.
 */
export function selectInGraph(names: readonly string[], depth = 1): void {
  // The current workspace binds the set via sessionStorage; a workspaceId
  // query string would lie on a new-tab deep link and break the selection.
  const url = pageUrl("graph");
  if (names.length === 1) {
    url.searchParams.set("node", names[0]);
  } else {
    url.searchParams.set("nodes", names.join(","));
    const clamped: 1 | 2 | 3 = depth === 2 || depth === 3 ? depth : 1;
    url.searchParams.set("depth", String(clamped));
    url.searchParams.set("isolate", "1");
  }
  location.assign(url.href);
}