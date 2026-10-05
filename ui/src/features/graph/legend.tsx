import type { TypeList } from "../../lib/schemas";
import type { GraphFilters } from "./session-state";

export interface LegendProps {
  types: TypeList | null;
  typeColors: ReadonlyMap<string, string>;
  hollowTypes: ReadonlySet<string>;
  filters: GraphFilters;
  onToggle: (kind: "entity" | "relation", type: string) => void;
  onShowAll: () => void;
}

/** Bottom-left legend; each entry toggles its type filter. */
export function Legend({ types, typeColors, hollowTypes, filters, onToggle, onShowAll }: LegendProps) {
  if (types === null) return null;
  const entityHidden = new Set(filters.hiddenEntityTypes);
  const relationHidden = new Set(filters.hiddenRelationTypes);
  return (
    <div className="g-legend" aria-label="Graph legend">
      <span className="g-legend__title">Legend</span>
      {types.entities.length > 0 && (
        <div className="g-legend__types">
          {types.entities.map((entry) => {
            const hidden = entityHidden.has(entry.type);
            const color = typeColors.get(entry.type) ?? "var(--ink-faint)";
            const hollow = hollowTypes.has(entry.type);
            return (
              <button
                key={entry.type}
                type="button"
                className={`g-legend__type${hidden ? " g-legend__type--off" : ""}`}
                aria-pressed={!hidden}
                title={hidden ? `Show ${entry.type}` : `Hide ${entry.type}`}
                onClick={() => onToggle("entity", entry.type)}
              >
                <span
                  className={`g-legend__dot${hollow ? " g-legend__dot--hollow" : ""}`}
                  style={{ backgroundColor: hollow ? "transparent" : color, borderColor: color }}
                  aria-hidden="true"
                />
                {entry.type}
              </button>
            );
          })}
        </div>
      )}
      {types.relations.length > 0 && (
        <div className="g-legend__rel">
          {types.relations.map((entry) => (
            <button
              key={entry.type}
              type="button"
              className={relationHidden.has(entry.type) ? "g-legend__type--off" : undefined}
              aria-pressed={!relationHidden.has(entry.type)}
              title={relationHidden.has(entry.type) ? `Show ${entry.type}` : `Hide ${entry.type}`}
              onClick={() => onToggle("relation", entry.type)}
            >
              {entry.type}
            </button>
          ))}
        </div>
      )}
      {(entityHidden.size > 0 || relationHidden.size > 0) && (
        <button type="button" className="g-rail__clear" onClick={onShowAll}>
          Show all types
        </button>
      )}
    </div>
  );
}