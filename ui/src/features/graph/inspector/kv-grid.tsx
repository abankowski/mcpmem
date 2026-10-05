import { useState } from "react";
import { Pencil, Plus, Trash2, X } from "lucide-react";
import { Button } from "../../../components/Button";

export interface KVGridProps {
  attributes: Readonly<Record<string, string>>;
  canWrite: boolean;
  onSet: (key: string, value: string) => Promise<void>;
  onDelete: (key: string) => Promise<void>;
}

/** Key-value attributes with inline edit; one row per attribute. */
export function KVGrid({ attributes, canWrite, onSet, onDelete }: KVGridProps) {
  const [editingKey, setEditingKey] = useState<string | null>(null);
  const [draft, setDraft] = useState("");
  const [adding, setAdding] = useState(false);
  const [newKey, setNewKey] = useState("");
  const [newValue, setNewValue] = useState("");
  const [busyKey, setBusyKey] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const keys = Object.keys(attributes);

  async function saveEdit(key: string): Promise<void> {
    setBusyKey(key);
    setError(null);
    try {
      await onSet(key, draft);
      setEditingKey(null);
      setDraft("");
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : "The attribute could not be saved.");
    } finally {
      setBusyKey(null);
    }
  }

  async function addAttribute(): Promise<void> {
    const key = newKey.trim();
    if (!key) return;
    setBusyKey(key);
    setError(null);
    try {
      await onSet(key, newValue);
      setNewKey("");
      setNewValue("");
      setAdding(false);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : "The attribute could not be saved.");
    } finally {
      setBusyKey(null);
    }
  }

  async function remove(key: string): Promise<void> {
    setBusyKey(key);
    setError(null);
    try {
      await onDelete(key);
      if (editingKey === key) {
        setEditingKey(null);
        setDraft("");
      }
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : "The attribute could not be removed.");
    } finally {
      setBusyKey(null);
    }
  }

  return (
    <div>
      {keys.length === 0 ? (
        <p className="g-empty">No attributes.</p>
      ) : (
        <div className="g-kv">
          {keys.map((key) => {
            const editing = editingKey === key;
            return (
              <div className="g-kv__row" key={key}>
                <span className="g-kv__key" title={key}>{key}</span>
                {editing ? (
                  <span className="g-kv__value g-kv__value--editing">
                    <input
                      value={draft}
                      onChange={(event) => setDraft(event.target.value)}
                      onKeyDown={(event) => {
                        if (event.key === "Enter") void saveEdit(key);
                        if (event.key === "Escape") { setEditingKey(null); setDraft(""); }
                      }}
                      autoFocus
                      aria-label={`Value of ${key}`}
                    />
                  </span>
                ) : (
                  <span className="g-kv__value" title={attributes[key]}>{attributes[key]}</span>
                )}
                {canWrite && (
                  <span className="g-kv__actions">
                    <Button
                      size="sm" variant="ghost" iconOnly
                      aria-label={`Edit ${key}`} disabled={busyKey === key}
                      onClick={() => {
                        if (editing) {
                          setEditingKey(null);
                          setDraft("");
                        } else {
                          setEditingKey(key);
                          setDraft(attributes[key]);
                        }
                      }}
                    >
                      {editing ? <X size={13} aria-hidden="true" /> : <Pencil size={13} aria-hidden="true" />}
                    </Button>
                    <Button
                      size="sm" variant="ghost" iconOnly
                      aria-label={`Delete ${key}`} disabled={busyKey === key}
                      onClick={() => void remove(key)}
                    >
                      <Trash2 size={13} aria-hidden="true" />
                    </Button>
                  </span>
                )}
              </div>
            );
          })}
        </div>
      )}
      {canWrite && (
        <div className="g-kv__add">
          {adding ? (
            <>
              <input
                value={newKey} placeholder="Key"
                onChange={(event) => setNewKey(event.target.value)}
                onKeyDown={(event) => {
                  if (event.key === "Enter") void addAttribute();
                  if (event.key === "Escape") setAdding(false);
                }}
                autoFocus aria-label="Attribute key"
              />
              <input
                value={newValue} placeholder="Value"
                onChange={(event) => setNewValue(event.target.value)}
                onKeyDown={(event) => {
                  if (event.key === "Enter") void addAttribute();
                  if (event.key === "Escape") { setAdding(false); }
                }}
                aria-label="Attribute value"
              />
              <Button size="sm" disabled={!newKey.trim() || busyKey !== null} onClick={() => void addAttribute()}>
                <Plus size={14} aria-hidden="true" />Add
              </Button>
              <Button size="sm" variant="ghost" onClick={() => setAdding(false)}>Cancel</Button>
            </>
          ) : (
            <Button size="sm" variant="ghost" onClick={() => setAdding(true)}>
              <Plus size={14} aria-hidden="true" />Add attribute
            </Button>
          )}
        </div>
      )}
      {error && <p className="g-inline-form__error" role="alert">{error}</p>}
    </div>
  );
}