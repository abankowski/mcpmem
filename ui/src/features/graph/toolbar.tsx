import { Link2, Maximize, Minus, Plus, RefreshCw } from "lucide-react";
import { Button } from "../../components/Button";
import type { ReactNode } from "react";

export interface ToolbarProps {
  depth: 1 | 2 | 3;
  onDepthChange: (depth: 1 | 2 | 3) => void;
  connectActive: boolean;
  onConnectToggle: () => void;
  onFit: () => void;
  onZoomIn: () => void;
  onZoomOut: () => void;
  onLayout: () => void;
  canConnect: boolean;
  children?: ReactNode;
}

/** Floating canvas toolbar: fit, zoom, layout, expand depth, and connect mode. */
export function CanvasToolbar({
  depth, onDepthChange, connectActive, onConnectToggle, onFit, onZoomIn, onZoomOut, onLayout,
  canConnect, children,
}: ToolbarProps) {
  return (
    <div className="g-toolbar" role="toolbar" aria-label="Canvas tools">
      <div className="g-toolbar__group">
        <Button iconOnly variant="ghost" aria-label="Fit graph" title="Fit graph" onClick={onFit}>
          <Maximize size={15} aria-hidden="true" />
        </Button>
        <Button iconOnly variant="ghost" aria-label="Zoom in" title="Zoom in" onClick={onZoomIn}>
          <Plus size={15} aria-hidden="true" />
        </Button>
        <Button iconOnly variant="ghost" aria-label="Zoom out" title="Zoom out" onClick={onZoomOut}>
          <Minus size={15} aria-hidden="true" />
        </Button>
        <Button iconOnly variant="ghost" aria-label="Re-run layout" title="Re-run layout" onClick={onLayout}>
          <RefreshCw size={15} aria-hidden="true" />
        </Button>
      </div>
      <div className="g-toolbar__divider" />
      <label className="g-toolbar__depth">
        Depth
        <select
          value={depth}
          onChange={(event) => onDepthChange(Number(event.target.value) as 1 | 2 | 3)}
          aria-label="Expand depth"
        >
          <option value={1}>1</option>
          <option value={2}>2</option>
          <option value={3}>3</option>
        </select>
      </label>
      {children}
      {canConnect && (
        <>
          <div className="g-toolbar__divider" />
          <Button
            variant={connectActive ? "default" : "ghost"}
            className={connectActive ? "g-toolbar__toggle-on" : undefined}
            onClick={onConnectToggle}
            aria-pressed={connectActive}
            title={connectActive ? "Cancel connect mode (Esc)" : "Connect two nodes"}
          >
            <Link2 size={15} aria-hidden="true" />
            {connectActive ? "Connecting…" : "Connect"}
          </Button>
        </>
      )}
    </div>
  );
}