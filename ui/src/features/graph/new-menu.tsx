import { useState } from "react";
import { Link2, Plus, ScanText } from "lucide-react";
import { Button } from "../../components/Button";

export interface NewMenuProps {
  canConnect: boolean;
  observationDisabled: boolean;
  onNewNode: () => void;
  onNewRelation: () => void;
  onNewObservation: () => void;
}

/** The New menu: node, relation, and observation entries (write only). */
export function NewMenu({
  canConnect, observationDisabled, onNewNode, onNewRelation, onNewObservation,
}: NewMenuProps) {
  const [open, setOpen] = useState(false);
  return (
    <div className="g-inspector__kebab">
      <Button variant="primary" onClick={() => setOpen((value) => !value)} aria-expanded={open} aria-haspopup="menu">
        <Plus size={15} aria-hidden="true" />New
      </Button>
      {open && (
        <>
          <div className="g-menu-backdrop" onClick={() => setOpen(false)} />
          <ul className="g-menu" role="menu">
            <li role="none">
              <button
                type="button" role="menuitem"
                onClick={() => { setOpen(false); onNewNode(); }}
              >
                <Plus size={13} aria-hidden="true" />Node
              </button>
            </li>
            <li role="none">
              <button
                type="button" role="menuitem"
                disabled={!canConnect}
                title={canConnect ? "Connect two nodes" : "Connect mode is already active"}
                onClick={() => { setOpen(false); onNewRelation(); }}
              >
                <Link2 size={13} aria-hidden="true" />Relation
              </button>
            </li>
            <li role="none">
              <button
                type="button" role="menuitem"
                disabled={observationDisabled}
                title={observationDisabled ? "Select a node first" : "Add an observation to the selected node"}
                onClick={() => { setOpen(false); onNewObservation(); }}
              >
                <ScanText size={13} aria-hidden="true" />Observation
              </button>
            </li>
          </ul>
        </>
      )}
    </div>
  );
}