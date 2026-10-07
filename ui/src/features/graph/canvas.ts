// The graph canvas: a dependency-free force layout drawn on a 2D context.
// React owns the data (the view model); this module owns positions,
// geometry, pointer interaction, and the render loop. Pinned nodes keep
// their world position across view-model updates; new nodes scatter around
// the current viewport centre.

export const MAX_CANVAS_NODES = 3000;

const IDEAL_EDGE_LENGTH = 92;
const GRAVITY = 0.012;
const DAMPING = 0.85;
const MAX_SPEED = 8;
const SETTLE_SPEED = 0.05;
const MAX_TICKS = 900;
const MIN_ZOOM = 0.15;
const MAX_ZOOM = 4;

export interface CanvasEdge {
  from: string;
  to: string;
  relationType: string;
}

export interface CanvasNode {
  name: string;
  entityType: string;
  degree: number;
}

export interface CanvasViewModel {
  nodes: readonly CanvasNode[];
  edges: readonly CanvasEdge[];
  typeColors: ReadonlyMap<string, string>;
  hollowTypes: ReadonlySet<string>;
  selectedEdge: CanvasEdge | null;
  hoverEdge: CanvasEdge | null;
  selectedName: string | null;
  dimmed: ReadonlySet<string>;
  connectPick: boolean;
  connectSource: string | null;
}

export interface CanvasCallbacks {
  onSelectNode: (name: string) => void;
  onSelectEdge: (edge: CanvasEdge) => void;
  onDeselect: () => void;
  onHoverEdge: (edge: CanvasEdge | null) => void;
  onExpandNode: (name: string) => void;
  onConnectNode: (name: string) => void;
  onNodePinned: (name: string) => void;
}

interface Placed {
  x: number;
  y: number;
  vx: number;
  vy: number;
}

interface Palette {
  surface: string;
  edge: string;
  edgeActive: string;
  edgeHot: string;
  orange: string;
  ink: string;
  inkMuted: string;
  inkFaint: string;
}

/** Read one theme token; the literals keep the canvas usable without CSS. */
export function cssColor(name: string, fallback: string): string {
  try {
    const value = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
    return value || fallback;
  } catch {
    return fallback;
  }
}

/** Deterministic name hash: scatter positions without set ordering churn. */
function nameHash(value: string): number {
  let hash = 0;
  for (let index = 0; index < value.length; index += 1) {
    hash = (hash * 31 + value.charCodeAt(index)) | 0;
  }
  return Math.abs(hash);
}

function radiusFor(degree: number): number {
  return 5 + Math.min(11, Math.sqrt(degree) * 2.6);
}

interface DragState {
  name: string;
  startX: number;
  startY: number;
  moved: boolean;
}

interface PanState {
  startX: number;
  startY: number;
  viewX: number;
  viewY: number;
}

export class GraphCanvas {
  private readonly container: HTMLElement;
  private readonly canvas: HTMLCanvasElement;
  private readonly ctx: CanvasRenderingContext2D;
  private readonly callbacks: CanvasCallbacks;
  private readonly palette: Palette;
  private vm: CanvasViewModel | null = null;
  private readonly positions = new Map<string, Placed>();
  private readonly pinned = new Set<string>();
  private view = { x: 0, y: 0, scale: 1 };
  private width = 0;
  private height = 0;
  private raf = 0;
  private tick = 0;
  private settled = false;
  /** True until the human picks their own view; cleared by pan, zoom or drag. */
  private fitOnSettle = true;
  private pulseFrames = 0;
  private drag: DragState | null = null;
  private pan: PanState | null = null;
  private hoverName: string | null = null;
  private roi: string[] = [];
  private lastClickAt = 0;
  private readonly ro: ResizeObserver;
  private readonly onPointerDown: (event: PointerEvent) => void;
  private readonly onPointerMove: (event: PointerEvent) => void;
  private readonly onPointerUp: (event: PointerEvent) => void;
  private readonly onPointerLeave: () => void;
  private readonly onDoubleClick: (event: MouseEvent) => void;
  private readonly onWheel: (event: WheelEvent) => void;

  constructor(container: HTMLElement, callbacks: CanvasCallbacks) {
    this.container = container;
    this.callbacks = callbacks;
    this.palette = {
      surface: cssColor("--surface-0", "#141310"),
      edge: cssColor("--edge", "#3d3831"),
      edgeActive: cssColor("--edge-active", "#7a7367"),
      edgeHot: cssColor("--edge-hot", "#ff9152"),
      orange: cssColor("--orange", "#f26b1d"),
      ink: cssColor("--ink", "#ece8df"),
      inkMuted: cssColor("--ink-muted", "#b1aa9d"),
      inkFaint: cssColor("--ink-faint", "#8f887b"),
    };
    this.canvas = document.createElement("canvas");
    this.canvas.className = "g-canvas";
    this.canvas.setAttribute("aria-label", "Graph canvas");
    container.appendChild(this.canvas);
    const context = this.canvas.getContext("2d");
    if (!context) throw new Error("The 2D canvas context is unavailable.");
    this.ctx = context;

    this.onPointerDown = (event) => this.pointerDown(event);
    this.onPointerMove = (event) => this.pointerMove(event);
    this.onPointerUp = (event) => this.pointerUp(event);
    this.onPointerLeave = () => {
      this.hoverName = null;
      if (this.roi.length > 0) {
        this.roi = [];
        this.callbacks.onHoverEdge(null);
      }
      this.canvas.style.cursor = "";
    };
    this.onDoubleClick = (event) => this.doubleClick(event);
    this.onWheel = (event) => this.wheel(event);

    this.canvas.addEventListener("pointerdown", this.onPointerDown);
    this.canvas.addEventListener("pointermove", this.onPointerMove);
    this.canvas.addEventListener("pointerup", this.onPointerUp);
    this.canvas.addEventListener("pointerleave", this.onPointerLeave);
    this.canvas.addEventListener("dblclick", this.onDoubleClick);
    this.canvas.addEventListener("wheel", this.onWheel, { passive: false });

    this.ro = new ResizeObserver(() => this.resize());
    this.ro.observe(container);
    this.resize();
  }

  destroy(): void {
    cancelAnimationFrame(this.raf);
    this.ro.disconnect();
    this.canvas.removeEventListener("pointerdown", this.onPointerDown);
    this.canvas.removeEventListener("pointermove", this.onPointerMove);
    this.canvas.removeEventListener("pointerup", this.onPointerUp);
    this.canvas.removeEventListener("pointerleave", this.onPointerLeave);
    this.canvas.removeEventListener("dblclick", this.onDoubleClick);
    this.canvas.removeEventListener("wheel", this.onWheel);
    this.canvas.remove();
  }

  // --- public controls ---------------------------------------------------------

  setViewModel(vm: CanvasViewModel | null): void {
    const previousCount = this.vm?.nodes.length ?? 0;
    this.vm = vm;
    for (const node of vm?.nodes ?? []) {
      if (!this.positions.has(node.name)) this.place(node.name);
    }
    this.prunePositions(vm?.nodes.length ?? 0);
    if (vm && vm.nodes.length !== previousCount) this.unsettle();
    if (vm?.connectPick && this.pulseFrames === 0 && !this.raf) this.pulseFrames = 240;
    this.render();
  }

  /** The persisted pinned set; dragged nodes pin through `onNodePinned`. */
  setPinned(names: readonly string[]): void {
    this.pinned.clear();
    for (const name of names) this.pinned.add(name);
    this.render();
  }

  zoomIn(): void {
    this.zoomBy(1.25);
  }

  zoomOut(): void {
    this.zoomBy(0.8);
  }

  /** Restart the simulation with a small jitter so the layout re-settles. */
  relayout(): void {
    for (const placed of this.positions.values()) {
      placed.vx += (Math.random() - 0.5) * 24;
      placed.vy += (Math.random() - 0.5) * 24;
    }
    this.unsettle();
  }

  fit(): void {
    const nodes = this.vm?.nodes ?? [];
    if (nodes.length === 0) {
      this.view = { x: this.width / 2, y: this.height / 2, scale: 1 };
      this.render();
      return;
    }
    let minX = Infinity;
    let minY = Infinity;
    let maxX = -Infinity;
    let maxY = -Infinity;
    for (const node of nodes) {
      const placed = this.positions.get(node.name);
      if (!placed) continue;
      minX = Math.min(minX, placed.x);
      minY = Math.min(minY, placed.y);
      maxX = Math.max(maxX, placed.x);
      maxY = Math.max(maxY, placed.y);
    }
    if (!Number.isFinite(minX)) { this.render(); return; }
    const pad = 64;
    const spanX = Math.max(1, maxX - minX + pad * 2);
    const spanY = Math.max(1, maxY - minY + pad * 2);
    const scale = Math.min(this.width / spanX, this.height / spanY, 1.4);
    this.view = {
      x: this.width / 2 - ((minX + maxX) / 2) * scale,
      y: this.height / 2 - ((minY + maxY) / 2) * scale,
      scale: Math.max(MIN_ZOOM, scale),
    };
    this.render();
  }

  focus(name: string): void {
    const placed = this.positions.get(name);
    if (!placed) return;
    this.fitOnSettle = false;
    // Keep the current zoom; move the node into the viewport centre.
    this.view = {
      x: this.width / 2 - placed.x * this.view.scale,
      y: this.height / 2 - placed.y * this.view.scale,
      scale: this.view.scale,
    };
    this.render();
  }

  // --- layout and rendering -----------------------------------------------------

  private place(name: string): void {
    const angle = nameHash(name) * 0.01745;
    const radius = 60 + (nameHash(name + ":r") % Math.floor(Math.min(this.width, this.height) / 2.4));
    this.positions.set(name, {
      x: this.view.x / this.view.scale + Math.cos(angle) * radius + this.width / (2 * this.view.scale),
      y: this.view.y / this.view.scale + Math.sin(angle) * radius + this.height / (2 * this.view.scale),
      vx: 0,
      vy: 0,
    });
  }

  private prunePositions(nodeCount: number): void {
    if (this.positions.size <= nodeCount * 3 + 200) return;
    const keep = new Set(this.vm?.nodes.map((node) => node.name) ?? []);
    for (const name of this.positions.keys()) {
      if (!keep.has(name)) this.positions.delete(name);
    }
  }

  private unsettle(): void {
    this.settled = false;
    this.tick = 0;
    if (!this.raf) this.frame();
  }

  private frame(): void {
    this.raf = requestAnimationFrame(() => {
      this.raf = 0;
      if (this.vm && this.vm.nodes.length > 0 && !this.settled) {
        this.settled = this.step();
        if (!this.settled) {
          this.frame();
        } else if (this.fitOnSettle) {
          // The nodes start stacked on the view centre and the layout only
          // spreads them wide over the ticks; the initial fit therefore fits
          // a tiny box and the spread graph ends up off-centre. Re-fit once
          // the layout settles, unless the human already took the view.
          this.fit();
        }
      }
      this.render();
    });
  }

  /** One simulation tick. Returns true when the layout has settled. */
  private step(): boolean {
    const vm = this.vm;
    if (!vm) return true;
    this.tick += 1;
    const nodes = vm.nodes;
    const positions = this.positions;
    const cell = 140;
    const grid = new Map<string, number[]>();
    for (let index = 0; index < nodes.length; index += 1) {
      const placed = positions.get(nodes[index].name);
      if (!placed) continue;
      const key = `${Math.floor(placed.x / cell)},${Math.floor(placed.y / cell)}`;
      const bucket = grid.get(key);
      if (bucket) bucket.push(index);
      else grid.set(key, [index]);
    }
    const forces: Float64Array = new Float64Array(nodes.length * 2);
    const indexByName = new Map<string, number>();
    for (let index = 0; index < nodes.length; index += 1) indexByName.set(nodes[index].name, index);
    for (let index = 0; index < nodes.length; index += 1) {
      const placed = positions.get(nodes[index].name);
      if (!placed || this.pinned.has(nodes[index].name)) continue;
      const key = `${Math.floor(placed.x / cell)},${Math.floor(placed.y / cell)}`;
      const [cx, cy] = key.split(",").map(Number);
      for (let dx = -1; dx <= 1; dx += 1) {
        for (let dy = -1; dy <= 1; dy += 1) {
          const bucket = grid.get(`${cx + dx},${cy + dy}`);
          if (!bucket) continue;
          for (const other of bucket) {
            if (other === index) continue;
            const otherPlaced = positions.get(nodes[other].name);
            if (!otherPlaced) continue;
            let dxv = placed.x - otherPlaced.x;
            let dyv = placed.y - otherPlaced.y;
            const distSq = dxv * dxv + dyv * dyv;
            if (distSq > cell * cell * 2.4) continue;
            const dist = Math.sqrt(distSq) || 0.01;
            dxv /= dist;
            dyv /= dist;
            const force = 3400 / (distSq + 40);
            forces[index * 2] += dxv * force;
            forces[index * 2 + 1] += dyv * force;
          }
        }
      }
    }
    for (const edge of vm.edges) {
      const from = positions.get(edge.from);
      const to = positions.get(edge.to);
      if (!from || !to) continue;
      if (this.pinned.has(edge.from) && this.pinned.has(edge.to)) continue;
      let dxv = to.x - from.x;
      let dyv = to.y - from.y;
      const dist = Math.sqrt(dxv * dxv + dyv * dyv) || 0.01;
      dxv /= dist;
      dyv /= dist;
      const force = 0.028 * (dist - IDEAL_EDGE_LENGTH);
      const fromIndex = indexByName.get(edge.from);
      const toIndex = indexByName.get(edge.to);
      if (fromIndex !== undefined && !this.pinned.has(edge.from)) {
        forces[fromIndex * 2] += dxv * force;
        forces[fromIndex * 2 + 1] += dyv * force;
      }
      if (toIndex !== undefined && !this.pinned.has(edge.to)) {
        forces[toIndex * 2] -= dxv * force;
        forces[toIndex * 2 + 1] -= dyv * force;
      }
    }
    let centreX = 0;
    let centreY = 0;
    let free = 0;
    for (let index = 0; index < nodes.length; index += 1) {
      const placed = positions.get(nodes[index].name);
      if (!placed || this.pinned.has(nodes[index].name)) continue;
      centreX += placed.x;
      centreY += placed.y;
      free += 1;
    }
    if (free > 0) { centreX /= free; centreY /= free; }
    let energy = 0;
    for (let index = 0; index < nodes.length; index += 1) {
      const name = nodes[index].name;
      const placed = positions.get(name);
      if (!placed || this.pinned.has(name)) continue;
      if (free > 0) {
        forces[index * 2] += (centreX - placed.x) * GRAVITY;
        forces[index * 2 + 1] += (centreY - placed.y) * GRAVITY;
      }
      placed.vx = (placed.vx + forces[index * 2]) * DAMPING;
      placed.vy = (placed.vy + forces[index * 2 + 1]) * DAMPING;
      const speed = Math.hypot(placed.vx, placed.vy);
      if (speed > MAX_SPEED) {
        placed.vx = (placed.vx / speed) * MAX_SPEED;
        placed.vy = (placed.vy / speed) * MAX_SPEED;
      }
      placed.x += placed.vx;
      placed.y += placed.vy;
      energy += speed;
    }
    return (energy < SETTLE_SPEED * Math.max(1, free)) || this.tick > MAX_TICKS;
  }

  private resize(): void {
    const rect = this.container.getBoundingClientRect();
    const dpr = window.devicePixelRatio || 1;
    this.width = Math.max(1, rect.width);
    this.height = Math.max(1, rect.height);
    this.canvas.width = Math.round(this.width * dpr);
    this.canvas.height = Math.round(this.height * dpr);
    this.ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    if (this.view.scale === 1 && this.vm && this.vm.nodes.length > 0) this.fit();
    else this.render();
  }

  private render(): void {
    const ctx = this.ctx;
    ctx.clearRect(0, 0, this.width, this.height);
    ctx.fillStyle = this.palette.surface;
    ctx.fillRect(0, 0, this.width, this.height);
    if (this.vm === null) return;
    ctx.save();
    ctx.translate(this.view.x, this.view.y);
    ctx.scale(this.view.scale, this.view.scale);
    const vm = this.vm;
    const neighborSet = new Set<string>();
    if (vm.selectedName) {
      for (const edge of vm.edges) {
        if (edge.from === vm.selectedName) neighborSet.add(edge.to);
        if (edge.to === vm.selectedName) neighborSet.add(edge.from);
      }
    }
    for (const edge of vm.edges) this.renderEdge(ctx, edge);
    for (const node of vm.nodes) this.renderNode(ctx, node);
    for (const node of vm.nodes) {
      if (node.degree >= 3 || node.name === vm.selectedName || neighborSet.has(node.name)) {
        this.renderLabel(ctx, node, vm);
      }
    }
    ctx.restore();
    if (this.vm.nodes.length === 0) {
      ctx.fillStyle = this.palette.inkFaint;
      ctx.font = "500 13px 'IBM Plex Sans', sans-serif";
      ctx.textAlign = "center";
      ctx.fillText("No nodes on this page yet.", this.width / 2, this.height / 2);
    }
    if (this.pulseFrames > 0) {
      if (!this.vm.connectPick) {
        this.pulseFrames = 0;
      } else if (this.raf === 0) {
        this.pulseFrames -= 1;
        this.raf = requestAnimationFrame(() => {
          this.raf = 0;
          this.render();
        });
      }
    }
  }

  private renderEdge(ctx: CanvasRenderingContext2D, edge: CanvasEdge): void {
    const vm = this.vm;
    if (!vm) return;
    const from = this.positions.get(edge.from);
    const to = this.positions.get(edge.to);
    if (!from || !to) return;
    const hover = vm.hoverEdge != null
      && vm.hoverEdge.from === edge.from
      && vm.hoverEdge.to === edge.to
      && vm.hoverEdge.relationType === edge.relationType;
    const selected = vm.selectedEdge != null
      && vm.selectedEdge.from === edge.from
      && vm.selectedEdge.to === edge.to
      && vm.selectedEdge.relationType === edge.relationType;
    ctx.beginPath();
    ctx.moveTo(from.x, from.y);
    ctx.lineTo(to.x, to.y);
    ctx.lineWidth = hover || selected ? 1.5 : 1;
    ctx.strokeStyle = hover ? this.palette.edgeHot : selected ? this.palette.edgeActive : this.palette.edge;
    ctx.globalAlpha = hover || selected ? 1 : 0.85;
    ctx.stroke();
    ctx.globalAlpha = 1;
  }

  private renderNode(ctx: CanvasRenderingContext2D, node: CanvasNode): void {
    const vm = this.vm;
    if (!vm) return;
    const placed = this.positions.get(node.name);
    if (!placed) return;
    const radius = radiusFor(node.degree);
    const color = vm.typeColors.get(node.entityType) ?? this.palette.inkFaint;
    const hollow = vm.hollowTypes.has(node.entityType);
    ctx.globalAlpha = vm.dimmed.has(node.name) ? 0.3 : 1;
    ctx.beginPath();
    ctx.arc(placed.x, placed.y, radius, 0, Math.PI * 2);
    if (hollow) {
      ctx.fillStyle = this.palette.surface;
      ctx.fill();
      ctx.lineWidth = 1.5;
      ctx.strokeStyle = color;
      ctx.stroke();
    } else {
      ctx.fillStyle = color;
      ctx.fill();
    }
    const selected = vm.selectedName === node.name;
    if (selected) {
      ctx.beginPath();
      ctx.arc(placed.x, placed.y, radius + 3, 0, Math.PI * 2);
      ctx.lineWidth = 2;
      ctx.strokeStyle = this.palette.orange;
      ctx.stroke();
    } else if (this.pinned.has(node.name)) {
      ctx.beginPath();
      ctx.arc(placed.x, placed.y, radius + 3, 0, Math.PI * 2);
      ctx.lineWidth = 1.5;
      ctx.strokeStyle = this.palette.inkMuted;
      ctx.stroke();
    }
    if (vm.connectPick && vm.connectSource === node.name) {
      const pulse = 0.55 + 0.45 * Math.sin(performance.now() / 220);
      ctx.beginPath();
      ctx.arc(placed.x, placed.y, radius + 6, 0, Math.PI * 2);
      ctx.lineWidth = 2;
      ctx.strokeStyle = this.palette.orange;
      ctx.globalAlpha = pulse;
      ctx.stroke();
      ctx.globalAlpha = 1;
    }
    ctx.globalAlpha = 1;
  }

  private renderLabel(ctx: CanvasRenderingContext2D, node: CanvasNode, vm: CanvasViewModel): void {
    if (this.view.scale < 0.5) return;
    const placed = this.positions.get(node.name);
    if (!placed) return;
    const radius = radiusFor(node.degree);
    const dimmed = vm.dimmed.has(node.name);
    const selected = vm.selectedName === node.name;
    ctx.globalAlpha = dimmed && !selected ? 0.4 : 1;
    ctx.font = "500 11px 'JetBrains Mono', ui-monospace, monospace";
    ctx.textAlign = "center";
    ctx.textBaseline = "bottom";
    ctx.fillStyle = selected ? this.palette.ink : this.palette.inkMuted;
    ctx.fillText(node.name, placed.x, placed.y - radius - 4);
    ctx.globalAlpha = 1;
  }

  // --- interaction ----------------------------------------------------------------

  private screenToWorld(clientX: number, clientY: number): { x: number; y: number } {
    const rect = this.canvas.getBoundingClientRect();
    return {
      x: (clientX - rect.left - this.view.x) / this.view.scale,
      y: (clientY - rect.top - this.view.y) / this.view.scale,
    };
  }

  private nodeAt(worldX: number, worldY: number): CanvasNode | null {
    const vm = this.vm;
    if (!vm) return null;
    let best: CanvasNode | null = null;
    let bestDist = Infinity;
    for (const node of vm.nodes) {
      const placed = this.positions.get(node.name);
      if (!placed) continue;
      const dxv = placed.x - worldX;
      const dyv = placed.y - worldY;
      const distSq = dxv * dxv + dyv * dyv;
      const reach = radiusFor(node.degree) + 6 / this.view.scale;
      if (distSq < reach * reach && distSq < bestDist) {
        best = node;
        bestDist = distSq;
      }
    }
    return best;
  }

  private edgeAt(worldX: number, worldY: number): CanvasEdge | null {
    const vm = this.vm;
    if (!vm) return null;
    if (vm.connectPick) return null;
    const threshold = 5 / this.view.scale;
    let best: CanvasEdge | null = null;
    let bestDist = Infinity;
    for (const edge of vm.edges) {
      const from = this.positions.get(edge.from);
      const to = this.positions.get(edge.to);
      if (!from || !to) continue;
      const dxv = to.x - from.x;
      const dyv = to.y - from.y;
      const lengthSq = dxv * dxv + dyv * dyv;
      if (lengthSq === 0) continue;
      const t = Math.max(0, Math.min(1, ((worldX - from.x) * dxv + (worldY - from.y) * dyv) / lengthSq));
      const projX = from.x + t * dxv;
      const projY = from.y + t * dyv;
      const dist = Math.hypot(worldX - projX, worldY - projY);
      if (dist < threshold && dist < bestDist) {
        best = edge;
        bestDist = dist;
      }
    }
    return best;
  }

  private pointerDown(event: PointerEvent): void {
    if (event.button !== 0) return;
    this.canvas.setPointerCapture(event.pointerId);
    const point = this.screenToWorld(event.clientX, event.clientY);
    const node = this.nodeAt(point.x, point.y);
    const vm = this.vm;
    if (node) {
      if (vm?.connectPick) {
        this.callbacks.onConnectNode(node.name);
        return;
      }
      this.drag = { name: node.name, startX: event.clientX, startY: event.clientY, moved: false };
      this.canvas.style.cursor = "grabbing";
      return;
    }
    if (!vm?.connectPick) {
      const edge = this.edgeAt(point.x, point.y);
      if (edge) {
        this.callbacks.onSelectEdge(edge);
        return;
      }
    }
    this.pan = { startX: event.clientX, startY: event.clientY, viewX: this.view.x, viewY: this.view.y };
    this.canvas.style.cursor = "grabbing";
  }

  private pointerMove(event: PointerEvent): void {
    const point = this.screenToWorld(event.clientX, event.clientY);
    if (this.drag) {
      const drag = this.drag;
      const placed = this.positions.get(drag.name);
      if (!placed) { this.drag = null; return; }
      const dxv = event.clientX - drag.startX;
      const dyv = event.clientY - drag.startY;
      if (Math.hypot(dxv, dyv) > 5) {
        drag.moved = true;
        this.fitOnSettle = false;
      }
      if (drag.moved) {
        placed.x = point.x;
        placed.y = point.y;
        placed.vx = 0;
        placed.vy = 0;
        this.render();
      }
      return;
    }
    if (this.pan) {
      const dx = event.clientX - this.pan.startX;
      const dy = event.clientY - this.pan.startY;
      if (dx === 0 && dy === 0) return;
      this.fitOnSettle = false;
      this.view = {
        x: this.pan.viewX + dx,
        y: this.pan.viewY + dy,
        scale: this.view.scale,
      };
      this.render();
      return;
    }
    const node = this.nodeAt(point.x, point.y);
    const vm = this.vm;
    if (vm?.connectPick && node) {
      this.canvas.style.cursor = "crosshair";
      this.render();
      return;
    }
    if (node) {
      this.canvas.style.cursor = vm?.connectPick ? "crosshair" : "grab";
      if (this.roi[0] !== `n:${node.name}`) {
        this.roi = [`n:${node.name}`];
        this.callbacks.onHoverEdge(null);
      }
      if (this.hoverName !== node.name) this.hoverName = node.name;
      return;
    }
    if (this.hoverName !== null) {
      this.hoverName = null;
      this.callbacks.onHoverEdge(null);
    }
    const edge = this.edgeAt(point.x, point.y);
    if (edge) {
      this.canvas.style.cursor = "pointer";
      if (this.roi[0] !== `e:${edge.from}:${edge.to}:${edge.relationType}`) {
        this.roi = [`e:${edge.from}:${edge.to}:${edge.relationType}`];
        this.callbacks.onHoverEdge(edge);
      }
    } else {
      this.canvas.style.cursor = vm?.connectPick ? "crosshair" : "grab";
      if (this.roi.length > 0 && this.roi[0].startsWith("e:")) {
        this.roi = [];
        this.callbacks.onHoverEdge(null);
      }
    }
  }

  private pointerUp(event: PointerEvent): void {
    if (this.drag) {
      const drag = this.drag;
      this.drag = null;
      this.canvas.style.cursor = "grab";
      if (drag.moved) {
        this.callbacks.onNodePinned(drag.name);
      } else {
        const now = performance.now();
        // Let a double-click expand, not select twice.
        if (now - this.lastClickAt > 450) {
          this.lastClickAt = now;
          this.callbacks.onSelectNode(drag.name);
        }
      }
      return;
    }
    if (this.pan) {
      const pan = this.pan;
      this.pan = null;
      this.canvas.style.cursor = "grab";
      const moved = Math.hypot(event.clientX - pan.startX, event.clientY - pan.startY) > 5;
      if (!moved) this.callbacks.onDeselect();
    }
  }

  private doubleClick(event: MouseEvent): void {
    const point = this.screenToWorld(event.clientX, event.clientY);
    const node = this.nodeAt(point.x, point.y);
    if (node) this.callbacks.onExpandNode(node.name);
  }

  private wheel(event: WheelEvent): void {
    event.preventDefault();
    const rect = this.canvas.getBoundingClientRect();
    const factor = Math.exp(-event.deltaY * 0.0012);
    this.zoomBy(factor, event.clientX - rect.left, event.clientY - rect.top);
  }

  private zoomBy(factor: number, pivotX?: number, pivotY?: number): void {
    this.fitOnSettle = false;
    const cx = pivotX ?? this.width / 2;
    const cy = pivotY ?? this.height / 2;
    const next = Math.min(MAX_ZOOM, Math.max(MIN_ZOOM, this.view.scale * factor));
    const worldX = (cx - this.view.x) / this.view.scale;
    const worldY = (cy - this.view.y) / this.view.scale;
    this.view = {
      x: cx - worldX * next,
      y: cy - worldY * next,
      scale: next,
    };
    this.render();
  }
}