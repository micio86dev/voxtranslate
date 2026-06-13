// Collaborative whiteboard (spec 0045). A <canvas> overlay whose strokes are
// relayed through the existing WebSocket: each local stroke streams as batches of
// NORMALISED (0..1) points so every client scales to its own canvas — no
// distortion across desktop/mobile. The server keeps the op-log and replays it to
// late-joiners (`applySnapshot`). Eraser uses destination-out so it reveals the
// board background (the canvas itself is transparent).

export type WbOp =
  | {
      op: 'draw';
      id: string;
      tool: 'pen' | 'eraser';
      color: string;
      width: number; // normalised: fraction of canvas width
      points: [number, number][]; // each [x, y] in 0..1
    }
  | { op: 'clear' };

const PEN_WIDTH = 0.005; // ~0.5% of canvas width
const ERASER_WIDTH = 0.04;
const FLUSH_MS = 55; // batch cadence while drawing (low-latency, low-chatter)

export class Whiteboard {
  tool: 'pen' | 'eraser' = 'pen';
  color = '#f1f5f9';

  private ctx: CanvasRenderingContext2D;
  private ops: WbOp[] = []; // local mirror → lets us redraw on resize
  private drawing = false;
  private strokeId = '';
  private strokeSeq = 0;
  private pending: [number, number][] = [];
  private last: [number, number] | null = null; // last flushed point (batch continuity)
  private flushTimer: number | null = null;

  constructor(
    private canvas: HTMLCanvasElement,
    private send: (op: WbOp) => void,
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

  /** Apply a remote (or local) op to the board. */
  applyOp(op: WbOp): void {
    if (op.op === 'clear') {
      this.ops = [];
      this.clearCanvas();
    } else {
      this.ops.push(op);
      this.render(op);
    }
  }

  /** Replace the board with the server's op-log (late-joiner snapshot). */
  applySnapshot(ops: WbOp[]): void {
    this.ops = Array.isArray(ops) ? ops.slice() : [];
    this.redraw();
  }

  /** Wipe the board everywhere (local + relayed). */
  clearBoard(): void {
    this.applyOp({ op: 'clear' });
    this.send({ op: 'clear' });
  }

  /** Drop everything (leaving the call) without relaying. */
  reset(): void {
    this.ops = [];
    this.drawing = false;
    this.pending = [];
    this.last = null;
    if (this.flushTimer) {
      clearTimeout(this.flushTimer);
      this.flushTimer = null;
    }
    this.clearCanvas();
  }

  // ---- rendering -------------------------------------------------------------

  private clearCanvas(): void {
    this.ctx.clearRect(0, 0, this.canvas.clientWidth, this.canvas.clientHeight);
  }

  private redraw(): void {
    this.clearCanvas();
    for (const op of this.ops) this.render(op);
  }

  private render(op: WbOp): void {
    if (op.op !== 'draw' || !op.points.length) return;
    const { ctx } = this;
    const w = this.canvas.clientWidth;
    const h = this.canvas.clientHeight;
    ctx.save();
    ctx.lineJoin = 'round';
    ctx.lineCap = 'round';
    ctx.lineWidth = Math.max(1, op.width * w);
    if (op.tool === 'eraser') {
      ctx.globalCompositeOperation = 'destination-out';
      ctx.strokeStyle = 'rgba(0,0,0,1)';
    } else {
      ctx.globalCompositeOperation = 'source-over';
      ctx.strokeStyle = op.color;
    }
    ctx.beginPath();
    const [x0, y0] = op.points[0];
    ctx.moveTo(x0 * w, y0 * h);
    if (op.points.length === 1) {
      // A tap: draw a dot.
      ctx.lineTo(x0 * w + 0.01, y0 * h);
    } else {
      for (let i = 1; i < op.points.length; i++) ctx.lineTo(op.points[i][0] * w, op.points[i][1] * h);
    }
    ctx.stroke();
    ctx.restore();
  }

  // ---- local drawing ---------------------------------------------------------

  private norm(e: PointerEvent): [number, number] {
    const r = this.canvas.getBoundingClientRect();
    return [(e.clientX - r.left) / r.width, (e.clientY - r.top) / r.height];
  }

  private onDown = (e: PointerEvent): void => {
    e.preventDefault();
    this.canvas.setPointerCapture(e.pointerId);
    this.drawing = true;
    this.strokeId = `${this.strokeSeq++}`;
    this.last = null;
    this.pending = [this.norm(e)];
    this.scheduleFlush();
  };

  private onMove = (e: PointerEvent): void => {
    if (!this.drawing) return;
    this.pending.push(this.norm(e));
    this.scheduleFlush();
  };

  private onUp = (e: PointerEvent): void => {
    if (!this.drawing) return;
    this.drawing = false;
    try {
      this.canvas.releasePointerCapture(e.pointerId);
    } catch {
      /* not captured */
    }
    this.flush();
  };

  private scheduleFlush(): void {
    if (this.flushTimer != null) return;
    this.flushTimer = window.setTimeout(() => {
      this.flushTimer = null;
      this.flush();
    }, FLUSH_MS);
  }

  /** Emit + locally apply the buffered points as one Draw op. Each batch carries
   *  the previous batch's last point so successive batches join seamlessly. */
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
      width: this.tool === 'eraser' ? ERASER_WIDTH : PEN_WIDTH,
      points,
    };
    this.applyOp(op);
    this.send(op);
  }
}
