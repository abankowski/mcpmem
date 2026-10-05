import { useEffect, useState } from "react";
import { Focus, Maximize, MoreVertical, Pin, PinOff, Pencil, Plus, Trash2 } from "lucide-react";
import { api, ApiError } from "../../../lib/api";
import { formatTimestamp } from "../../../lib/format";
import type { Mutation, NodeDetail } from "../../../lib/schemas";
import { Button } from "../../../components/Button";
import { ConfirmDialog } from "../../../components/ConfirmDialog";
import { Sheet } from "../../../components/Sheet";
import { KVGrid } from "./kv-grid";

export interface NodeTabProps {
  workspaceId: string;
  name: string;
  detail: NodeDetail | null;
  loading: boolean;
  error: ApiError | null;
  canWrite: boolean;
  pinned: boolean;
  onRetry: () => void;
  onRenamed: (newName: string) => void;
  onMerged: (targetName: string) => void;
  onDeleted: () => void;
  onChanged: () => void;
  onTogglePin?: () => void;
  onExpand?: () => void;
  onIsolate?: () => void;
}

function errorMessage(cause: unknown): string {
  return cause instanceof Error ? cause.message : "The action failed. Try again.";
}

export function NodeTab({
  workspaceId, name, detail, loading, error, canWrite, pinned, onRetry, onRenamed, onMerged,
  onDeleted, onChanged, onTogglePin, onExpand, onIsolate,
}: NodeTabProps) {
  const [menuOpen, setMenuOpen] = useState(false);
  const [renameOpen, setRenameOpen] = useState(false);
  const [renameBody, setRenameBody] = useState("");
  const [mergeOpen, setMergeOpen] = useState(false);
  const [mergeBody, setMergeBody] = useState("");
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [busy, setBusy] = useState(false);
  const [errorText, setErrorText] = useState<string | null>(null);

  const observations = detail?.observations ?? [];
  const [showAll, setShowAll] = useState(false);
  const [adding, setAdding] = useState(false);
  const [newBody, setNewBody] = useState("");
  const [editingId, setEditingId] = useState<number | null>(null);
  const [editBody, setEditBody] = useState("");

  useEffect(() => {
    setMenuOpen(false);
    setErrorText(null);
    setShowAll(false);
    setAdding(false);
    setEditingId(null);
  }, [name, workspaceId]);

  async function mutateSafe(change: Mutation, then: () => void): Promise<void> {
    setBusy(true);
    setErrorText(null);
    try {
      await api.mutate(workspaceId, change);
      onChanged();
      then();
    } catch (cause) {
      setErrorText(errorMessage(cause));
    } finally {
      setBusy(false);
    }
  }

  function addObservation(): void {
    const body = newBody.trim();
    if (!body) return;
    mutateSafe({ operation: "addObservation", payload: { entityName: name, body } }, () => {
      setNewBody("");
      setAdding(false);
    });
  }

  function saveEdit(observationId: number): void {
    const body = editBody.trim();
    if (!body) return;
    mutateSafe({ operation: "editObservation", payload: { entityName: name, observationId, body } }, () => {
      setEditingId(null);
      setEditBody("");
    });
  }

  function removeObservation(observationId: number): void {
    mutateSafe({ operation: "deleteObservation", payload: { entityName: name, observationId } }, () => {
      if (editingId === observationId) {
        setEditingId(null);
        setEditBody("");
      }
    });
  }

  function saveAttribute(key: string, value: string): Promise<void> {
    return api.mutate(workspaceId, { operation: "setEntityAttributes", payload: { entityName: name, attributes: { [key]: value } } })
      .then(() => onChanged());
  }

  function deleteAttribute(key: string): Promise<void> {
    return api.mutate(workspaceId, { operation: "deleteEntityAttributes", payload: { entityName: name, keys: [key] } })
      .then(() => onChanged());
  }

  function renameEntity(): void {
    const newName = renameBody.trim();
    if (!newName || newName === name) return;
    mutateSafe({ operation: "renameEntity", payload: { oldName: name, newName } }, () => {
      setRenameOpen(false);
      onRenamed(newName);
    });
  }

  function mergeEntity(): void {
    const target = mergeBody.trim();
    if (!target || target === name) return;
    mutateSafe({ operation: "mergeEntities", payload: { source: name, target } }, () => {
      setMergeOpen(false);
      onMerged(target);
    });
  }

  function deleteEntity(): void {
    mutateSafe({ operation: "deleteEntity", payload: { name } }, () => {
      setConfirmDelete(false);
      onDeleted();
    });
  }

  const headerActions = onTogglePin != null || onExpand != null || onIsolate != null;
  const shown = showAll ? observations : observations.slice(0, 3);

  return (
    <div>
      <header className="g-inspector__header">
        <div className="g-inspector__title">
          <h2>{name}</h2>
          {detail && <span className="ui-tag">{detail.entityType}</span>}
        </div>
        {headerActions && (
          <div className="g-inspector__actions">
            {onTogglePin != null && (
              <Button
                iconOnly variant="ghost"
                aria-label={pinned ? "Unpin node" : "Pin node"}
                title={pinned ? "Unpin node" : "Pin node"}
                onClick={onTogglePin}
              >
                {pinned ? <PinOff size={15} aria-hidden="true" /> : <Pin size={15} aria-hidden="true" />}
              </Button>
            )}
            {onExpand != null && (
              <Button iconOnly variant="ghost" aria-label="Expand node" title="Expand node" onClick={onExpand}>
                <Maximize size={15} aria-hidden="true" />
              </Button>
            )}
            {onIsolate != null && (
              <Button iconOnly variant="ghost" aria-label="Isolate node" title="Isolate node" onClick={onIsolate}>
                <Focus size={15} aria-hidden="true" />
              </Button>
            )}
            <div className="g-inspector__kebab">
              <Button iconOnly variant="ghost" aria-label="Node actions" aria-expanded={menuOpen} onClick={() => setMenuOpen((open) => !open)}>
                <MoreVertical size={15} aria-hidden="true" />
              </Button>
              {menuOpen && (
                <>
                  <div className="g-menu-backdrop" onClick={() => setMenuOpen(false)} />
                  <ul className="g-menu" role="menu">
                    <li role="none">
                      <button
                        type="button" role="menuitem"
                        onClick={() => { setMenuOpen(false); setRenameBody(name); setRenameOpen(true); }}
                      >
                        <Pencil size={13} aria-hidden="true" />Rename
                      </button>
                    </li>
                    <li role="none">
                      <button
                        type="button" role="menuitem"
                        onClick={() => { setMenuOpen(false); setMergeBody(""); setMergeOpen(true); }}
                      >
                        <Plus size={13} aria-hidden="true" />Merge into…
                      </button>
                    </li>
                    <li role="none">
                      <button
                        type="button" role="menuitem" className="g-menu__danger"
                        onClick={() => { setMenuOpen(false); setConfirmDelete(true); }}
                      >
                        <Trash2 size={13} aria-hidden="true" />Delete
                      </button>
                    </li>
                  </ul>
                </>
              )}
            </div>
          </div>
        )}
      </header>
      <div className="g-inspector__body">
        {loading && detail === null && <p className="g-empty">Loading node…</p>}
        {!loading && error != null && (
          <p className="g-inline-form__error" role="alert">
            {error.message} <Button size="sm" variant="ghost" onClick={onRetry}>Retry</Button>
          </p>
        )}
        {errorText != null && !loading && <p className="g-inline-form__error" role="alert">{errorText}</p>}
        {detail && (
          <>
            <dl className="g-meta">
              <dt>Type</dt><dd>{detail.entityType}</dd>
              <dt>In-degree</dt><dd>{detail.degree.in}</dd>
              <dt>Out-degree</dt><dd>{detail.degree.out}</dd>
              <dt>Observations</dt><dd>{observations.length}</dd>
              <dt>Attributes</dt><dd>{Object.keys(detail.attributes ?? {}).length}</dd>
              <dt>Relations</dt><dd>{detail.relations.length}</dd>
              <dt>Neighbors</dt><dd>{detail.neighbors.length}</dd>
            </dl>

            <section className="g-section" aria-label="Observations">
              <div className="g-section__heading">
                <span>Observations ({observations.length})</span>
                {canWrite && !adding && <Button size="sm" variant="ghost" onClick={() => setAdding(true)}>Add</Button>}
              </div>
              {adding && (
                <div className="g-inline-form">
                  <textarea
                    value={newBody} autoFocus
                    onChange={(event) => setNewBody(event.target.value)}
                    placeholder="Observation body" aria-label="Observation body"
                  />
                  <div className="g-inline-form__row">
                    <Button size="sm" disabled={!newBody.trim() || busy} onClick={addObservation}>Add</Button>
                    <Button size="sm" variant="ghost" onClick={() => { setAdding(false); setNewBody(""); }}>Cancel</Button>
                  </div>
                </div>
              )}
              {observations.length === 0 && !adding && <p className="g-empty">No observations.</p>}
              {shown.map((observation) => {
                const editing = editingId === observation.observationId;
                return (
                  <div className="g-observation" key={observation.observationId}>
                    <div className="g-observation__main">
                      {editing ? (
                        <div className="g-inline-form">
                          <textarea
                            value={editBody} autoFocus
                            onChange={(event) => setEditBody(event.target.value)}
                            aria-label="Edit observation body"
                          />
                          <div className="g-inline-form__row">
                            <Button size="sm" disabled={!editBody.trim() || busy} onClick={() => saveEdit(observation.observationId)}>Save</Button>
                            <Button size="sm" variant="ghost" onClick={() => { setEditingId(null); setEditBody(""); }}>Cancel</Button>
                          </div>
                        </div>
                      ) : (
                        <>
                          <p className="g-observation__body">{observation.body}</p>
                          <span className="g-observation__meta">
                            #{observation.observationId}
                            {observation.occurredAtUs != null ? ` · ${formatTimestamp(observation.occurredAtUs)}` : ""}
                            {observation.createdAtUs != null ? ` · created ${formatTimestamp(observation.createdAtUs)}` : ""}
                            {observation.originEntityName != null ? ` · origin ${observation.originEntityName}` : ""}
                          </span>
                        </>
                      )}
                    </div>
                    {canWrite && !editing && (
                      <div className="g-observation__actions">
                        <Button
                          size="sm" variant="ghost" iconOnly
                          aria-label={`Edit observation ${observation.observationId}`}
                          onClick={() => { setEditingId(observation.observationId); setEditBody(observation.body); }}
                        >
                          <Pencil size={13} aria-hidden="true" />
                        </Button>
                        <Button
                          size="sm" variant="ghost" iconOnly
                          aria-label={`Delete observation ${observation.observationId}`}
                          onClick={() => removeObservation(observation.observationId)}
                        >
                          <Trash2 size={13} aria-hidden="true" />
                        </Button>
                      </div>
                    )}
                  </div>
                );
              })}
              {observations.length > 3 && (
                <button type="button" className="g-more" onClick={() => setShowAll((value) => !value)}>
                  {showAll ? "Show fewer" : `Show ${observations.length - 3} more`}
                </button>
              )}
            </section>

            <section className="g-section" aria-label="Attributes">
              <div className="g-section__heading"><span>Attributes</span></div>
              <KVGrid
                attributes={detail.attributes ?? {}}
                canWrite={canWrite}
                onSet={saveAttribute}
                onDelete={deleteAttribute}
              />
            </section>
          </>
        )}
      </div>

      <Sheet open={renameOpen} title={`Rename ${name}`} onClose={() => setRenameOpen(false)}>
        <div className="g-field">
          <label htmlFor="g-rename-input">New name</label>
          <input
            id="g-rename-input" value={renameBody}
            onChange={(event) => setRenameBody(event.target.value)}
            onKeyDown={(event) => { if (event.key === "Enter") renameEntity(); }}
          />
        </div>
        <div className="g-inline-form__row">
          <Button disabled={!renameBody.trim() || renameBody.trim() === name || busy} onClick={renameEntity}>Rename</Button>
          <Button variant="ghost" onClick={() => setRenameOpen(false)}>Cancel</Button>
        </div>
      </Sheet>

      <Sheet open={mergeOpen} title={`Merge ${name} into…`} onClose={() => setMergeOpen(false)}>
        <div className="g-field">
          <label htmlFor="g-merge-input">Target node name</label>
          <input
            id="g-merge-input" value={mergeBody}
            onChange={(event) => setMergeBody(event.target.value)}
            onKeyDown={(event) => { if (event.key === "Enter") mergeEntity(); }}
            placeholder="Target node"
          />
          <p className="g-hint">The observations of {name} move into the target; {name} is removed.</p>
        </div>
        <div className="g-inline-form__row">
          <Button disabled={!mergeBody.trim() || mergeBody.trim() === name || busy} onClick={mergeEntity}>Merge</Button>
          <Button variant="ghost" onClick={() => setMergeOpen(false)}>Cancel</Button>
        </div>
      </Sheet>

      <ConfirmDialog
        open={confirmDelete} title={`Delete ${name}?`} confirmLabel="Delete"
        onClose={() => setConfirmDelete(false)} onConfirm={deleteEntity}
      >
        This removes the node and all its relations. This cannot be undone.
      </ConfirmDialog>
    </div>
  );
}