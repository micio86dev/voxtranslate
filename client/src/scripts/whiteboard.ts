// Collaborative whiteboard (spec 0045 → advanced in spec 0062 / #96). A <canvas>
// overlay whose ops relay through the existing WebSocket as batches of NORMALISED
// (0..1) points, so every client scales to its own canvas (no distortion across
// desktop/mobile). The server keeps the op-log and replays it to late-joiners.
//
// Spec 0062 adds multi-page, highlighter + shapes, and PNG/PDF export WITHOUT any
// server change: the new data rides the existing `Draw` op (serde drops unknown
// fields server-side, so nothing new may live outside `id`/`tool`/`color`/`width`/
// `points`):
//   • page  → encoded in `id` as "<pageId>:<seq>" (legacy ids with no ":" → page p1)
//   • tool  → the free-string `tool`: pen|highlighter|eraser|line|arrow|rect|ellipse
//   • shapes carry exactly two points [start, end]
//   • page structure → marker ops (empty points) the server persists like any draw:
//       page-add / page-del / page-clear — replayed in order, so late-joiners
//       reconstruct the same page set + content.

import { jpegPagesToPdf, pngBlob, triggerDownload } from './wb-export';

export type WbTool = 'pen' | 'highlighter' | 'eraser' | 'line' | 'arrow' | 'rect' | 'ellipse';
type MarkerTool = 'page-add' | 'page-del' | 'page-clear';

export type WbOp =
  | {
      op: 'draw';
      id: string; // "<pageId>:<seq>" — the page is encoded in the id
      tool: WbTool | MarkerTool;
      color: string;
      width: number; // normalised: fraction of canvas width
      points: [number, number][]; // each [x, y] in 0..1
    }
  | { op: 'clear' }; // legacy global clear (spec 0045) — kept for back-compat

export type WbWidth = 's' | 'm' | 'l';

export const DEFAULT_PAGE = 'p1';
const BOARD_BG = '#0f1320'; // the board surface (matches .whiteboard CSS) — used for export
const WIDTHS: Record<WbWidth, number> = { s: 0.0035, m: 0.006, l: 0.011 };
const ERASER_SCALE = 6; // eraser is much chunkier than the pen
const HL_SCALE = 4; // highlighter is broad
const HL_ALPHA = 0.4; // highlighter is semi-transparent
const FLUSH_MS = 55; // batch cadence while free-hand drawing (low-latency, low-chatter)
const FREEHAND = new Set<WbTool>(['pen', 'highlighter', 'eraser']);
const EXPORT_W = 1600;
const EXPORT_H = 900;

/** The page a draw op belongs to. Legacy ops (id without ":") live on the default page. */
export function pageOf(id: string): string {
  const i = id.indexOf(':');
  return i >= 0 ? id.slice(0, i) : DEFAULT_PAGE;
}

const isMarker = (t: string): t is MarkerTool =>
  t === 'page-add' || t === 'page-del' || t === 'page-clear';

/** Render ONE op onto any 2D context sized w×h (shared by the live board + export). */
export function drawOp(ctx: CanvasRenderingContext2D, op: WbOp, w: number, h: number): void {
  if (op.op !== 'draw' || !op.points.length || isMarker(op.tool)) return;
  const t = op.tool as WbTool;
  ctx.save();
  ctx.lineJoin = 'round';
  ctx.lineCap = 'round';
  ctx.lineWidth = Math.max(1, op.width * w);
  if (t === 'eraser') {
    ctx.globalCompositeOperation = 'destination-out';
    ctx.strokeStyle = 'rgba(0,0,0,1)';
  } else {
    ctx.globalCompositeOperation = 'source-over';
    ctx.strokeStyle = op.color;
    if (t === 'highlighter') ctx.globalAlpha = HL_ALPHA;
  }
  const P = op.points.map(([x, y]) => [x * w, y * h] as [number, number]);
  if (t === 'line' || t === 'arrow') {
    const a = P[0];
    const b = P[P.length - 1];
    ctx.beginPath();
    ctx.moveTo(a[0], a[1]);
    ctx.lineTo(b[0], b[1]);
    ctx.stroke();
    if (t === 'arrow') {
      const ang = Math.atan2(b[1] - a[1], b[0] - a[0]);
      const head = Math.max(8, op.width * w * 3);
      ctx.beginPath();
      ctx.moveTo(b[0], b[1]);
      ctx.lineTo(b[0] - head * Math.cos(ang - Math.PI / 6), b[1] - head * Math.sin(ang - Math.PI / 6));
      ctx.moveTo(b[0], b[1]);
      ctx.lineTo(b[0] - head * Math.cos(ang + Math.PI / 6), b[1] - head * Math.sin(ang + Math.PI / 6));
      ctx.stroke();
    }
  } else if (t === 'rect') {
    const [a, b] = P;
    ctx.strokeRect(Math.min(a[0], b[0]), Math.min(a[1], b[1]), Math.abs(b[0] - a[0]), Math.abs(b[1] - a[1]));
  } else if (t === 'ellipse') {
    const [a, b] = P;
    ctx.beginPath();
    ctx.ellipse((a[0] + b[0]) / 2, (a[1] + b[1]) / 2, Math.abs(b[0] - a[0]) / 2, Math.abs(b[1] - a[1]) / 2, 0, 0, 2 * Math.PI);
    ctx.stroke();
  } else {
    // free-hand: pen / highlighter / eraser
    ctx.beginPath();
    ctx.moveTo(P[0][0], P[0][1]);
    if (P.length === 1) ctx.lineTo(P[0][0] + 0.01, P[0][1]); // a tap → a dot
    else for (let i = 1; i < P.length; i++) ctx.lineTo(P[i][0], P[i][1]);
    ctx.stroke();
  }
  ctx.restore();
}

export class Whiteboard {
  tool: WbTool = 'pen';
  color = '#f1f5f9';
  widthKey: WbWidth = 'm';

  private ctx: CanvasRenderingContext2D;
  private ops: WbOp[] = []; // local mirror of EVERY page → lets us redraw + export
  private pages: string[] = [DEFAULT_PAGE]; // ordered by first appearance in the op-log
  private current = DEFAULT_PAGE;
  private drawing = false;
  private strokeId = '';
  private strokeSeq = 0;
  private pending: [number, number][] = []; // free-hand batch
  private last: [number, number] | null = null; // last flushed point (batch continuity)
  private flushTimer: number | null = null;
  private shapeStart: [number, number] | null = null; // shape rubber-band anchor
  private previewRaf = 0;

  constructor(
    private canvas: HTMLCanvasElement,
    private send: (op: WbOp) => void,
    private onPagesChanged?: (count: number, index: number) => void,
  ) {
    this.ctx = canvas.getContext('2d')!;
    canvas.addEventListener('pointerdown', this.onDown);
    canvas.addEventListener('pointermove', this.onMove);
    canvas.addEventListener('pointerup', this.onUp);
    canvas.addEventListener('pointercancel', this.onUp);
    canvas.addEventListener('pointerleave', this.onUp);
  }

  /** Match the backing store to the displayed size (DPR-aware) + redraw. Call
   *  whenever the overlay opens or the window resizes. */
  resize(): void {
    const dpr = window.devicePixelRatio || 1;
    const w = this.canvas.clientWidth;
    const h = this.canvas.clientHeight;
    if (!w || !h) return;
    this.canvas.width = Math.round(w * dpr);
    this.canvas.height = Math.round(h * dpr);
    this.ctx.setTransform(dpr, 0, 0, dpr, 0, 0); // draw in CSS pixels
    this.redraw();
  }

  // ---- op application (local + remote) ---------------------------------------

  /** Apply a remote (or local) op to the board. */
  applyOp(op: WbOp): void {
    if (op.op === 'clear') {
      this.ops = [];
      this.pages = [DEFAULT_PAGE];
      this.current = DEFAULT_PAGE;
      this.clearCanvas();
      this.notify();
      return;
    }
    const pid = pageOf(op.id);
    if (op.tool === 'page-add') {
      this.ensurePage(pid);
      this.notify();
      return;
    }
    if (op.tool === 'page-del') {
      this.removePage(pid);
      return;
    }
    if (op.tool === 'page-clear') {
      this.ops = this.ops.filter((o) => o.op !== 'draw' || pageOf(o.id) !== pid);
      if (pid === this.current) this.redraw();
      return;
    }
    // a drawable op — registering its page if we've not seen it
    this.ensurePage(pid);
    this.ops.push(op);
    if (pid === this.current) this.render(op);
  }

  /** Replace the board with the server's op-log (late-joiner snapshot). */
  applySnapshot(ops: WbOp[]): void {
    this.ops = [];
    this.pages = [DEFAULT_PAGE];
    this.current = DEFAULT_PAGE;
    for (const op of Array.isArray(ops) ? ops : []) {
      if (op.op !== 'draw') continue;
      const pid = pageOf(op.id);
      if (op.tool === 'page-add') this.ensurePage(pid);
      else if (op.tool === 'page-del') {
        this.pages = this.pages.filter((p) => p !== pid);
        this.ops = this.ops.filter((o) => o.op !== 'draw' || pageOf(o.id) !== pid);
      } else if (op.tool === 'page-clear') {
        this.ops = this.ops.filter((o) => o.op !== 'draw' || pageOf(o.id) !== pid);
      } else {
        this.ensurePage(pid);
        this.ops.push(op);
      }
    }
    if (!this.pages.includes(this.current)) this.current = this.pages[0] ?? DEFAULT_PAGE;
    if (!this.pages.length) this.pages = [DEFAULT_PAGE];
    this.redraw();
    this.notify();
  }

  /** Drop everything (leaving the call) without relaying. */
  reset(): void {
    this.ops = [];
    this.pages = [DEFAULT_PAGE];
    this.current = DEFAULT_PAGE;
    this.drawing = false;
    this.shapeStart = null;
    this.pending = [];
    this.last = null;
    if (this.flushTimer) {
      clearTimeout(this.flushTimer);
      this.flushTimer = null;
    }
    this.clearCanvas();
    this.notify();
  }

  // ---- pages -----------------------------------------------------------------

  pageCount(): number {
    return this.pages.length;
  }
  pageIndex(): number {
    return Math.max(0, this.pages.indexOf(this.current));
  }

  private ensurePage(pid: string): void {
    if (!this.pages.includes(pid)) this.pages.push(pid);
  }

  private removePage(pid: string): void {
    if (this.pages.length <= 1) return; // never delete the last page
    this.pages = this.pages.filter((p) => p !== pid);
    this.ops = this.ops.filter((o) => o.op !== 'draw' || pageOf(o.id) !== pid);
    if (this.current === pid) this.current = this.pages[0];
    this.redraw();
    this.notify();
  }

  private newPageId(): string {
    return `p-${Math.random().toString(36).slice(2, 8)}-${this.strokeSeq++}`;
  }

  /** Switch the locally-viewed page (navigation is NOT relayed). */
  setPageIndex(index: number): void {
    const pid = this.pages[index];
    if (!pid || pid === this.current) return;
    this.current = pid;
    this.shapeStart = null;
    this.redraw();
    this.notify();
  }
  prevPage(): void {
    this.setPageIndex(this.pageIndex() - 1);
  }
  nextPage(): void {
    this.setPageIndex(this.pageIndex() + 1);
  }

  /** Add a fresh blank page (relayed) and switch to it. */
  addPage(): void {
    const pid = this.newPageId();
    this.relayMarker('page-add', pid);
    this.ensurePage(pid);
    this.current = pid;
    this.redraw();
    this.notify();
  }

  /** Duplicate the current page's drawables onto a new page (relayed) + switch. */
  duplicatePage(): void {
    const pid = this.newPageId();
    this.relayMarker('page-add', pid);
    this.ensurePage(pid);
    for (const o of this.ops) {
      if (o.op !== 'draw' || pageOf(o.id) !== this.current || isMarker(o.tool)) continue;
      const copy: WbOp = { ...o, id: `${pid}:${this.strokeSeq++}`, points: o.points.map((p) => [...p] as [number, number]) };
      this.ops.push(copy);
      this.send(copy);
    }
    this.current = pid;
    this.redraw();
    this.notify();
  }

  /** Delete the current page (relayed). No-op when it's the last page. */
  deleteCurrentPage(): void {
    if (this.pages.length <= 1) return;
    const pid = this.current;
    this.relayMarker('page-del', pid);
    this.removePage(pid);
  }

  /** Wipe ONLY the current page, for everyone (spec 0062 — was a global clear). */
  clearPage(): void {
    const pid = this.current;
    this.relayMarker('page-clear', pid);
    this.ops = this.ops.filter((o) => o.op !== 'draw' || pageOf(o.id) !== pid);
    this.redraw();
  }

  private relayMarker(tool: MarkerTool, pid: string): void {
    this.send({ op: 'draw', id: `${pid}:${tool}`, tool, color: '', width: 0, points: [] });
  }

  private notify(): void {
    this.onPagesChanged?.(this.pages.length, this.pageIndex());
  }

  // ---- export ----------------------------------------------------------------

  /** Render one page to an offscreen canvas on the board surface colour. */
  private renderPage(pid: string): HTMLCanvasElement {
    const c = document.createElement('canvas');
    c.width = EXPORT_W;
    c.height = EXPORT_H;
    const cx = c.getContext('2d')!;
    cx.fillStyle = BOARD_BG;
    cx.fillRect(0, 0, EXPORT_W, EXPORT_H);
    for (const op of this.ops) if (op.op === 'draw' && pageOf(op.id) === pid) drawOp(cx, op, EXPORT_W, EXPORT_H);
    return c;
  }

  /** Download the current page as a PNG. */
  async exportPng(): Promise<void> {
    const blob = await pngBlob(this.renderPage(this.current));
    if (blob) triggerDownload(blob, `whiteboard-page-${this.pageIndex() + 1}.png`);
  }

  /** Download every page, in order, as one multi-page PDF. */
  exportPdf(): void {
    const jpegs = this.pages.map((pid) => ({
      dataUrl: this.renderPage(pid).toDataURL('image/jpeg', 0.92),
      w: EXPORT_W,
      h: EXPORT_H,
    }));
    triggerDownload(jpegPagesToPdf(jpegs), 'whiteboard.pdf');
  }

  // ---- rendering -------------------------------------------------------------

  private clearCanvas(): void {
    this.ctx.clearRect(0, 0, this.canvas.clientWidth, this.canvas.clientHeight);
  }

  private redraw(): void {
    this.clearCanvas();
    for (const op of this.ops) if (op.op === 'draw' && pageOf(op.id) === this.current) this.render(op);
  }

  private render(op: WbOp): void {
    drawOp(this.ctx, op, this.canvas.clientWidth, this.canvas.clientHeight);
  }

  // ---- local input -----------------------------------------------------------

  private norm(e: PointerEvent): [number, number] {
    const r = this.canvas.getBoundingClientRect();
    return [(e.clientX - r.left) / r.width, (e.clientY - r.top) / r.height];
  }

  private opWidth(): number {
    const base = WIDTHS[this.widthKey];
    if (this.tool === 'eraser') return base * ERASER_SCALE;
    if (this.tool === 'highlighter') return base * HL_SCALE;
    return base;
  }

  private onDown = (e: PointerEvent): void => {
    e.preventDefault();
    this.canvas.setPointerCapture(e.pointerId);
    const pt = this.norm(e);
    if (FREEHAND.has(this.tool)) {
      this.drawing = true;
      this.strokeId = `${this.current}:${this.strokeSeq++}`;
      this.last = null;
      this.pending = [pt];
      this.scheduleFlush();
    } else {
      this.drawing = true;
      this.shapeStart = pt;
    }
  };

  private onMove = (e: PointerEvent): void => {
    if (!this.drawing) return;
    if (FREEHAND.has(this.tool)) {
      this.pending.push(this.norm(e));
      this.scheduleFlush();
    } else if (this.shapeStart) {
      const cur = this.norm(e);
      if (this.previewRaf) return; // coalesce previews to one per frame
      this.previewRaf = requestAnimationFrame(() => {
        this.previewRaf = 0;
        this.redraw();
        drawOp(
          this.ctx,
          { op: 'draw', id: `${this.current}:preview`, tool: this.tool, color: this.color, width: this.opWidth(), points: [this.shapeStart!, cur] },
          this.canvas.clientWidth,
          this.canvas.clientHeight,
        );
      });
    }
  };

  private onUp = (e: PointerEvent): void => {
    if (!this.drawing) return;
    this.drawing = false;
    try {
      this.canvas.releasePointerCapture(e.pointerId);
    } catch {
      /* not captured */
    }
    if (FREEHAND.has(this.tool)) {
      this.flush();
    } else if (this.shapeStart) {
      const end = this.norm(e);
      const start = this.shapeStart;
      this.shapeStart = null;
      if (this.previewRaf) {
        cancelAnimationFrame(this.previewRaf);
        this.previewRaf = 0;
      }
      // Ignore a zero-size shape (a stray click).
      if (Math.abs(end[0] - start[0]) < 0.002 && Math.abs(end[1] - start[1]) < 0.002) {
        this.redraw();
        return;
      }
      const op: WbOp = {
        op: 'draw',
        id: `${this.current}:${this.strokeSeq++}`,
        tool: this.tool,
        color: this.color,
        width: this.opWidth(),
        points: [start, end],
      };
      this.ops.push(op);
      this.redraw();
      this.send(op);
    }
  };

  private scheduleFlush(): void {
    if (this.flushTimer != null) return;
    this.flushTimer = window.setTimeout(() => {
      this.flushTimer = null;
      this.flush();
    }, FLUSH_MS);
  }

  /** Emit + locally apply the buffered free-hand points as one Draw op. Each batch
   *  carries the previous batch's last point so successive batches join seamlessly. */
  private flush(): void {
    if (this.flushTimer != null) {
      clearTimeout(this.flushTimer);
      this.flushTimer = null;
    }
    if (!this.pending.length) return;
    const points = this.last ? [this.last, ...this.pending] : [...this.pending];
    this.last = this.pending[this.pending.length - 1];
    this.pending = [];
    const op: WbOp = {
      op: 'draw',
      id: this.strokeId,
      tool: this.tool,
      color: this.color,
      width: this.opWidth(),
      points,
    };
    this.ops.push(op);
    this.render(op);
    this.send(op);
  }
}
