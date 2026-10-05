import { useMemo, useState } from "react";
import { SegmentedControl } from "../../../components/SegmentedControl";
import { edgeKey, type EdgeRef } from "../session-state";

export interface RelationsTabProps {
  nodeName: string;
  relations: readonly EdgeRef[];
  selectedEdge: EdgeRef | null;
  onSelectRelation: (edge: EdgeRef) => void;
  onEdgeHover: (edge: EdgeRef | null) => void;
}

type Direction = "all" | "out" | "in";

function relationMatches(edge: EdgeRef, nodeName: string): "out" | "in" {
  return edge.from === nodeName ? "out" : "in";
}

export function RelationsTab({ nodeName, relations, selectedEdge, onSelectRelation, onEdgeHover }: RelationsTabProps) {
  const [direction, setDirection] = useState<Direction>("all");
  const visible = useMemo(() => {
    if (direction === "all") return relations;
    return relations.filter((edge) => relationMatches(edge, nodeName) === direction);
  }, [relations, direction, nodeName]);

  return (
    <div>
      <SegmentedControl
        label="Relation direction" name="edge-direction"
        value={direction}
        options={[
          { value: "all", label: "All" },
          { value: "out", label: "Out" },
          { value: "in", label: "In" },
        ]}
        onChange={setDirection}
      />
      <p className="g-empty">Hover a row to highlight its edge on the canvas.</p>
      {visible.length === 0 ? (
        <p className="g-empty">{direction === "all" ? "No relations." : `No ${direction === "out" ? "outgoing" : "incoming"} relations.`}</p>
      ) : (
        <ul className="g-rel-list">
          {visible.map((edge) => {
            const selected = selectedEdge != null && edgeKey(selectedEdge) === edgeKey(edge);
            return (
              <li key={edgeKey(edge)}>
                <button
                  type="button"
                  className={`g-rel-row${selected ? " g-rel-row--dim" : ""}`}
                  onMouseEnter={() => onEdgeHover(edge)}
                  onMouseLeave={() => onEdgeHover(null)}
                  onClick={() => onSelectRelation(edge)}
                >
                  <span className="g-rel__from" title={edge.from}>{edge.from}</span>
                  <span className="g-rel__type" title={edge.relationType}>{edge.relationType}</span>
                  <span className="g-rel__arrow">→</span>
                  <span className="g-rel__to" title={edge.to}>{edge.to}</span>
                  {direction !== "all" && <span className="g-rel__arrow">{relationMatches(edge, nodeName) === "out" ? "out" : "in"}</span>}
                </button>
              </li>
            );
          })}
        </ul>
      )}
    </div>
  );
}