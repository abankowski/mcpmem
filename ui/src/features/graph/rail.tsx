import { useState } from "react";
import { ChevronLeft, ChevronRight, Pin, PinOff } from "lucide-react";
import { Count } from "../../components/Count";
import type { TypeList } from "../../lib/schemas";
import type { GraphFilters } from "./session-state";

export interface RailProps {
  types: TypeList | null;
  filters: GraphFilters;
  pinned: readonly string[];
  onToggleHidden: (kind: "entity" | "relation", type: string) => void;
  onClearFilters: () => void;
  onUnpin: (name: string) => void;
  onSelectNode: (name: string) => void;
}

/** The collapsible left rail: entity and relation type checks with counts, and the pinned section. */
export function FilterRail({
  types, filters, pinned, onToggleHidden, onClearFilters, onUnpin, onSelectNode,
}: RailProps) {
  const [collapsed, setCollapsed] = useState(false);
  const entityHidden = new Set(filters.hiddenEntityTypes);
  const relationHidden = new Set(filters.hiddenRelationTypes);
  const anyHidden = entityHidden.size > 0 || relationHidden.size > 0;

  return (
    <aside className={`g-rail${collapsed ? " g-rail--collapsed" : ""}`} aria-label="Graph filters">
      <button
        type="button"
        className="g-rail__toggle"
        aria-label={collapsed ? "Expand filter rail" : "Collapse filter rail"}
        aria-expanded={!collapsed}
        onClick={() => setCollapsed((value) => !value)}
        title={collapsed ? "Expand rail" : "Collapse rail"}
      >
        {collapsed ? <ChevronRight size={16} aria-hidden="true" /> : <ChevronLeft size={16} aria-hidden="true" />}
      </button>
      {!collapsed && (
        <div className="g-rail__body">
          {anyHidden && (
            <button type="button" className="g-rail__clear" onClick={onClearFilters}>
              Show all types
            </button>
          )}
          <section className="g-rail__section" aria-label="Entity types">
            <h2 className="g-rail__heading">
              <span>Entity types</span>
              {entityHidden.size > 0 && <Count>{entityHidden.size} hidden</Count>}
            </h2>
            {types === null ? (
              <p className="g-empty">Loading types…</p>
            ) : types.entities.length === 0 ? (
              <p className="g-empty">No entity types yet.</p>
            ) : (
              <ul className="g-rail__list">
                {types.entities.map((entry) => {
                  const hidden = entityHidden.has(entry.type);
                  return (
                    <li key={entry.type}>
                      <label className="g-rail__item">
                        <input
                          type="checkbox"
                          checked={!hidden}
                          onChange={() => onToggleHidden("entity", entry.type)}
                        />
                        <span title={entry.type}>{entry.type}</span>
                        <Count>{entry.count}</Count>
                      </label>
                    </li>
                  );
                })}
              </ul>
            )}
          </section>
          <section className="g-rail__section" aria-label="Relation types">
            <h2 className="g-rail__heading">
              <span>Relation types</span>
              {relationHidden.size > 0 && <Count>{relationHidden.size} hidden</Count>}
            </h2>
            {types === null ? (
              <p className="g-empty">Loading types…</p>
            ) : types.relations.length === 0 ? (
              <p className="g-empty">No relation types yet.</p>
            ) : (
              <ul className="g-rail__list">
                {types.relations.map((entry) => {
                  const hidden = relationHidden.has(entry.type);
                  return (
                    <li key={entry.type}>
                      <label className="g-rail__item">
                        <input
                          type="checkbox"
                          checked={!hidden}
                          onChange={() => onToggleHidden("relation", entry.type)}
                        />
                        <span title={entry.type}>{entry.type}</span>
                        <Count>{entry.count}</Count>
                      </label>
                    </li>
                  );
                })}
              </ul>
            )}
          </section>
          <section className="g-rail__section" aria-label="Pinned nodes">
            <h2 className="g-rail__heading">
              <span>Pinned</span>
              {pinned.length > 0 && <Count>{pinned.length}</Count>}
            </h2>
            {pinned.length === 0 ? (
              <p className="g-empty">Drag a node or use its inspector pin.</p>
            ) : (
              <ul className="g-rail__list">
                {pinned.map((name) => (
                  <li className="g-rail__item" key={name} title={`Select ${name}`}>
                    <Pin size={12} aria-hidden="true" />
                    <button type="button" className="g-rail__item-link" onClick={() => onSelectNode(name)}>
                      <span>{name}</span>
                    </button>
                    <button
                      type="button"
                      className="g-rail__pin-off"
                      aria-label={`Unpin ${name}`}
                      onClick={() => onUnpin(name)}
                    >
                      <PinOff size={12} aria-hidden="true" />
                    </button>
                  </li>
                ))}
              </ul>
            )}
          </section>
        </div>
      )}
    </aside>
  );
}