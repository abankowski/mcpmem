import { useEffect, useState } from "react";
import { ArrowLeft, RefreshCw, Trash2 } from "lucide-react";
import { api, ApiError } from "../../../lib/api";
import { formatTimestamp } from "../../../lib/format";
import type { RelationDetail, TypeList } from "../../../lib/schemas";
import { useToast } from "../../../components/Toast";
import { Button } from "../../../components/Button";
import { ConfirmDialog } from "../../../components/ConfirmDialog";
import type { EdgeRef } from "../session-state";
import { KVGrid } from "./kv-grid";

export interface EdgeInspectorProps {
  workspaceId: string;
  triple: EdgeRef;
  canWrite: boolean;
  reloadKey?: number;
  onBack: () => void;
  onSelectNode: (name: string) => void;
  onTripleChanged?: (triple: EdgeRef) => void;
  onChanged?: () => void;
}

function toApiError(cause: unknown): ApiError {
  return cause instanceof ApiError ? cause : new ApiError(0, "network_error", "The relation could not be loaded.");
}

export function EdgeInspector({
  workspaceId, triple, canWrite, reloadKey = 0, onBack, onSelectNode, onTripleChanged, onChanged,
}: EdgeInspectorProps) {
  const notify = useToast();
  const [current, setCurrent] = useState<EdgeRef>(triple);
  const [detail, setDetail] = useState<RelationDetail | null>(null);
  const [types, setTypes] = useState<TypeList | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<ApiError | null>(null);
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [confirmReverse, setConfirmReverse] = useState(false);
  const [newType, setNewType] = useState(triple.relationType);
  const [addingObservation, setAddingObservation] = useState(false);
  const [body, setBody] = useState("");
  const [busy, setBusy] = useState(false);
  const [showAll, setShowAll] = useState(false);
  const [reloadTick, setReloadTick] = useState(0);

  // The page mounts one inspector per triple (key), so `current` starts from
  // the prop and then owns the triple across Reverse and Change type.
  useEffect(() => {
    const controller = new AbortController();
    let active = true;
    setDetail(null);
    setLoading(true);
    setError(null);
    setShowAll(false);
    void api.relation({
      workspaceId, from: current.from, to: current.to, relationType: current.relationType,
    }, controller.signal)
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
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [workspaceId, current.from, current.to, current.relationType, reloadKey, reloadTick]);

  useEffect(() => {
    const controller = new AbortController();
    let active = true;
    void api.types(workspaceId, controller.signal)
      .then((result) => { if (active) setTypes(result); })
      .catch(() => { if (active) setTypes(null); });
    return () => { active = false; controller.abort(); };
  }, [workspaceId]);

  function refreshCurrent(): void {
    setReloadTick((tick) => tick + 1);
  }

  const observations = detail?.observations ?? [];

  async function run(fn: () => Promise<void>): Promise<void> {
    setBusy(true);
    try {
      await fn();
    } finally {
      setBusy(false);
    }
  }

  async function changeType(): Promise<void> {
    const relationType = newType.trim();
    if (!relationType || relationType === current.relationType) return;
    await run(async () => {
      try {
        await api.mutate(workspaceId, {
          operation: "changeRelationType",
          payload: { from: current.from, to: current.to, relationType: current.relationType, newRelationType: relationType },
        });
        notify("success", "Relation type changed.");
        const next = { from: current.from, to: current.to, relationType };
        setCurrent(next);
        setNewType(next.relationType);
        onTripleChanged?.(next);
        onChanged?.();
        refreshCurrent();
      } catch (cause) {
        notify("error", cause instanceof Error ? cause.message : "The relation type could not be changed.");
      }
    });
  }

  async function reverse(): Promise<void> {
    await run(async () => {
      try {
        await api.mutate(workspaceId, {
          operation: "reverseRelation",
          payload: { from: current.from, to: current.to, relationType: current.relationType },
        });
        notify("success", "Relation reversed.");
        const next = { from: current.to, to: current.from, relationType: current.relationType };
        setConfirmReverse(false);
        setCurrent(next);
        setNewType(next.relationType);
        onTripleChanged?.(next);
        onChanged?.();
        refreshCurrent();
      } catch (cause) {
        setConfirmReverse(false);
        notify("error", cause instanceof Error ? cause.message : "The relation could not be reversed.");
      }
    });
  }

  async function remove(): Promise<void> {
    await run(async () => {
      try {
        await api.mutate(workspaceId, {
          operation: "deleteRelation",
          payload: { from: current.from, to: current.to, relationType: current.relationType },
        });
        setConfirmDelete(false);
        notify("success", "Relation deleted.");
        onChanged?.();
        onBack();
      } catch (cause) {
        setConfirmDelete(false);
        notify("error", cause instanceof Error ? cause.message : "The relation could not be deleted.");
      }
    });
  }

  async function addObservation(): Promise<void> {
    const trimmed = body.trim();
    if (!trimmed) return;
    await run(async () => {
      try {
        await api.mutate(workspaceId, {
          operation: "addRelationObservation",
          payload: { from: current.from, to: current.to, relationType: current.relationType, body: trimmed },
        });
        setBody("");
        setAddingObservation(false);
        notify("success", "Observation added.");
        onChanged?.();
        refreshCurrent();
      } catch (cause) {
        notify("error", cause instanceof Error ? cause.message : "The observation could not be added.");
      }
    });
  }

  async function deleteObservation(observationId: number): Promise<void> {
    await run(async () => {
      try {
        await api.mutate(workspaceId, {
          operation: "deleteRelationObservation",
          payload: { from: current.from, to: current.to, relationType: current.relationType, observationId },
        });
        notify("success", "Observation deleted.");
        onChanged?.();
        refreshCurrent();
      } catch (cause) {
        notify("error", cause instanceof Error ? cause.message : "The observation could not be deleted.");
      }
    });
  }

  async function setAttributes(attributes: Record<string, string>): Promise<void> {
    await api.mutate(workspaceId, {
      operation: "setRelationAttributes",
      payload: { from: current.from, to: current.to, relationType: current.relationType, attributes },
    });
    onChanged?.();
    refreshCurrent();
  }

  async function deleteAttributes(keys: string[]): Promise<void> {
    await api.mutate(workspaceId, {
      operation: "deleteRelationAttributes",
      payload: { from: current.from, to: current.to, relationType: current.relationType, keys },
    });
    onChanged?.();
    refreshCurrent();
  }

  const shown = showAll ? observations : observations.slice(0, 3);
  const relationTypeOptions = types?.relations.map((entry) => entry.type) ?? [];

  return (
    <div>
      <header className="g-edge-header">
        <Button size="sm" variant="ghost" className="g-edge-back" onClick={onBack}>
          <ArrowLeft size={14} aria-hidden="true" />Back
        </Button>
        <div className="g-edge-triple">
          <button type="button" onClick={() => onSelectNode(current.from)} title={`Open ${current.from}`}>{current.from}</button>
          <span className="g-rel__type">{current.relationType}</span>
          <span className="g-rel__arrow">→</span>
          <button type="button" onClick={() => onSelectNode(current.to)} title={`Open ${current.to}`}>{current.to}</button>
        </div>
        {canWrite && (
          <div className="g-inspector__actions">
            <Button size="sm" variant="ghost" disabled={busy} onClick={() => setConfirmReverse(true)}>Reverse</Button>
            <Button size="sm" variant="destructive" disabled={busy} onClick={() => setConfirmDelete(true)}>Delete</Button>
          </div>
        )}
      </header>
      <div className="g-inspector__body">
        {loading && detail === null && <p className="g-empty">Loading relation…</p>}
        {!loading && error != null && (
          <p className="g-inline-form__error" role="alert">
            {error.message}{" "}
            <Button size="sm" variant="ghost" onClick={refreshCurrent}><RefreshCw size={13} aria-hidden="true" />Retry</Button>
          </p>
        )}
        {detail && (
          <>
            <section className="g-section" aria-label="Observations">
              <div className="g-section__heading">
                <span>Observations ({observations.length})</span>
                {canWrite && !addingObservation && (
                  <Button size="sm" variant="ghost" onClick={() => setAddingObservation(true)}>Add</Button>
                )}
              </div>
              {addingObservation && (
                <div className="g-inline-form">
                  <textarea
                    value={body}
                    onChange={(event) => setBody(event.target.value)}
                    placeholder="Observation body"
                    aria-label="Observation body"
                    autoFocus
                  />
                  <div className="g-inline-form__row">
                    <Button size="sm" disabled={!body.trim() || busy} onClick={() => void addObservation()}>Add</Button>
                    <Button size="sm" variant="ghost" onClick={() => { setAddingObservation(false); setBody(""); }}>Cancel</Button>
                  </div>
                </div>
              )}
              {observations.length === 0 && !addingObservation ? (
                <p className="g-empty">No observations.</p>
              ) : (
                shown.map((observation) => (
                  <div className="g-observation" key={observation.observationId}>
                    <div>
                      <p className="g-observation__body">{observation.body}</p>
                      <span className="g-observation__meta">
                        #{observation.observationId}
                        {observation.occurredAtUs != null ? ` · ${formatTimestamp(observation.occurredAtUs)}` : ""}
                        {observation.createdAtUs != null ? ` · created ${formatTimestamp(observation.createdAtUs)}` : ""}
                        {observation.originEntityName != null ? ` · origin ${observation.originEntityName}` : ""}
                      </span>
                    </div>
                    {canWrite && (
                      <div className="g-observation__actions">
                        <Button
                          size="sm" variant="ghost" iconOnly
                          aria-label={`Delete observation ${observation.observationId}`}
                          disabled={busy}
                          onClick={() => void deleteObservation(observation.observationId)}
                        >
                          <Trash2 size={13} aria-hidden="true" />
                        </Button>
                      </div>
                    )}
                  </div>
                ))
              )}
              {observations.length > 3 && (
                <button type="button" className="g-more" onClick={() => setShowAll((value) => !value)}>
                  {showAll ? "Show fewer" : `Show ${observations.length - 3} more`}
                </button>
              )}
            </section>
            <section className="g-section" aria-label="Attributes">
              <div className="g-section__heading"><span>Attributes</span></div>
              <KVGrid
                attributes={detail.attributes}
                canWrite={canWrite}
                onSet={(key, value) => setAttributes({ [key]: value })}
                onDelete={(key) => deleteAttributes([key])}
              />
            </section>
            {canWrite && (
              <section className="g-section" aria-label="Change type">
                <div className="g-section__heading"><span>Relation type</span></div>
                <div className="g-inline-form__row">
                  <input
                    list="g-relation-type-options"
                    value={newType}
                    onChange={(event) => setNewType(event.target.value)}
                    aria-label="New relation type"
                    placeholder={current.relationType}
                  />
                  <datalist id="g-relation-type-options">
                    {relationTypeOptions.map((option) => <option value={option} key={option} />)}
                  </datalist>
                  <Button size="sm" disabled={busy || newType.trim() === current.relationType} onClick={() => void changeType()}>
                    Change type
                  </Button>
                </div>
              </section>
            )}
          </>
        )}
      </div>
      <ConfirmDialog
        open={confirmDelete} title="Delete relation?"
        confirmLabel="Delete"
        onClose={() => setConfirmDelete(false)}
        onConfirm={() => void remove()}
      >
        This removes the relation <strong>{current.from} —{current.relationType}→ {current.to}</strong>. This cannot be undone.
      </ConfirmDialog>
      <ConfirmDialog
        open={confirmReverse} title="Reverse relation?"
        confirmLabel="Reverse"
        onClose={() => setConfirmReverse(false)}
        onConfirm={() => void reverse()}
      >
        The direction changes from <strong>{current.from} —{current.relationType}→ {current.to}</strong> to{" "}
        <strong>{current.to} —{current.relationType}→ {current.from}</strong>. Existing observations and attributes stay on the relation.
      </ConfirmDialog>
    </div>
  );
}