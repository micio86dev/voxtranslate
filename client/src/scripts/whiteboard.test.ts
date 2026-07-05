import { describe, it, expect, vi, beforeAll, afterEach } from 'vitest';
import { Whiteboard, pageOf, drawOp, contentRect, type WbOp, type WbTool } from './whiteboard';

// The board needs a <canvas> + 2D context; in the node test env we stub both. Every
// ctx method is a no-op (the proxy returns a function for any property), so the page-
// model + op logic can be exercised without real rendering. Listeners are recorded so
// tests can synthesise pointer events (issue #225 coordinate-stability checks).
interface CanvasStub {
  canvas: HTMLCanvasElement;
  ctx: CanvasRenderingContext2D;
  listeners: Map<string, (e: PointerEvent) => void>;
  setSize: (w: number, h: number) => void;
  /** Style writes made to the sibling .wb-frame overlay (letterbox marker, #96). */
  frameStyle: Record<string, string>;
  /** Records canvas.captureStream(fps) calls (#230). */
  capture: ReturnType<typeof vi.fn>;
}
function stub(w = 800, h = 600): CanvasStub {
  const ctx = new Proxy({}, { get: () => () => {} }) as unknown as CanvasRenderingContext2D;
  const listeners = new Map<string, (e: PointerEvent) => void>();
  const frameStyle: Record<string, string> = {};
  const capture = vi.fn(() => ({}) as MediaStream);
  let cw = w;
  let ch = h;
  const canvas = {
    get clientWidth() {
      return cw;
    },
    get clientHeight() {
      return ch;
    },
    width: 0,
    height: 0,
    getContext: () => ctx,
    addEventListener: (type: string, fn: (e: PointerEvent) => void) => listeners.set(type, fn),
    setPointerCapture: () => {},
    releasePointerCapture: () => {},
    getBoundingClientRect: () => ({ left: 0, top: 0, width: cw, height: ch }),
    captureStream: capture,
    parentElement: {
      querySelector: (sel: string) => (sel === '.wb-frame' ? { style: frameStyle } : null),
    },
  } as unknown as HTMLCanvasElement;
  return {
    canvas,
    ctx,
    listeners,
    frameStyle,
    capture,
    setSize: (nw: number, nh: number) => {
      cw = nw;
      ch = nh;
    },
  };
}

// Deterministic rAF fakes: callbacks queue here and run only when a test flushes them
// (shape previews and the ResizeObserver re-fit both coalesce through rAF).
const rafCbs = new Map<number, FrameRequestCallback>();
let nextRafId = 1;
function flushRaf(): void {
  const pending = [...rafCbs.values()];
  rafCbs.clear();
  for (const cb of pending) cb(0);
}

/** Records the observe callback so tests can simulate the canvas element resizing
 *  without a window 'resize' (panel open/close — #225 follow-up). */
class FakeResizeObserver {
  static instances: FakeResizeObserver[] = [];
  observed: unknown[] = [];
  constructor(public cb: () => void) {
    FakeResizeObserver.instances.push(this);
  }
  observe(el: unknown): void {
    this.observed.push(el);
  }
  disconnect(): void {}
}

// The board reads window.devicePixelRatio in resize() and window.setTimeout while
// free-hand drawing; the node test env has no window, so provide a minimal one.
beforeAll(() => {
  if (typeof (globalThis as { window?: unknown }).window === 'undefined') {
    (globalThis as { window?: unknown }).window = {
      devicePixelRatio: 1,
      setTimeout: (fn: () => void, ms?: number) => setTimeout(fn, ms),
      clearTimeout: (id: ReturnType<typeof setTimeout>) => clearTimeout(id),
    };
  }
  (globalThis as { requestAnimationFrame?: unknown }).requestAnimationFrame = (
    cb: FrameRequestCallback,
  ): number => {
    const id = nextRafId++;
    rafCbs.set(id, cb);
    return id;
  };
  (globalThis as { cancelAnimationFrame?: unknown }).cancelAnimationFrame = (id: number): void => {
    rafCbs.delete(id);
  };
  (globalThis as { ResizeObserver?: unknown }).ResizeObserver = FakeResizeObserver;
});

afterEach(() => {
  vi.useRealTimers();
  rafCbs.clear();
  FakeResizeObserver.instances = [];
});

/** Synthesise a pointer gesture (down → moves → up) at CSS-pixel coordinates. */
function gesture(s: CanvasStub, pts: [number, number][]): void {
  const ev = (x: number, y: number): PointerEvent =>
    ({ clientX: x, clientY: y, pointerId: 1, preventDefault: () => {} }) as unknown as PointerEvent;
  s.listeners.get('pointerdown')?.(ev(pts[0][0], pts[0][1]));
  for (let i = 1; i < pts.length; i++) s.listeners.get('pointermove')?.(ev(pts[i][0], pts[i][1]));
  s.listeners.get('pointerup')?.(ev(pts[pts.length - 1][0], pts[pts.length - 1][1]));
}

const draw = (id: string, tool: WbTool = 'pen'): WbOp => ({
  op: 'draw',
  id,
  tool,
  color: '#fff',
  width: 0.005,
  points: [
    [0.1, 0.1],
    [0.5, 0.5],
  ],
});

describe('pageOf (spec 0062)', () => {
  it('parses the page prefix and falls back to p1 for legacy ids', () => {
    expect(pageOf('p-abc:3')).toBe('p-abc');
    expect(pageOf('p1:0')).toBe('p1');
    expect(pageOf('7')).toBe('p1'); // legacy spec-0045 id (no ":")
  });
});

describe('drawOp (spec 0062)', () => {
  it('renders every tool without throwing and ignores marker ops', () => {
    const { ctx } = stub();
    const tools: WbTool[] = ['pen', 'highlighter', 'eraser', 'line', 'arrow', 'rect', 'ellipse'];
    for (const t of tools) expect(() => drawOp(ctx, draw('p1:0', t), 800, 600)).not.toThrow();
    expect(() =>
      drawOp(ctx, { op: 'draw', id: 'p1:add', tool: 'page-add', color: '', width: 0, points: [] } as WbOp, 800, 600),
    ).not.toThrow();
  });
});

describe('contentRect — stable logical box (issue #225)', () => {
  const ASPECT = 1600 / 900;
  const near = (a: number, b: number) => Math.abs(a - b) < 1e-6;

  it('is empty for a zero/degenerate CSS box', () => {
    expect(contentRect(0, 600)).toEqual({ x: 0, y: 0, w: 0, h: 0 });
    expect(contentRect(800, 0)).toEqual({ x: 0, y: 0, w: 0, h: 0 });
  });

  it('always preserves the fixed 16:9 aspect, whatever the CSS box shape', () => {
    for (const [w, h] of [
      [800, 600], // 4:3 → pillarboxed (height-bound)
      [1920, 1080], // exact 16:9 → fills
      [1000, 1000], // square → pillarboxed
      [2000, 500], // ultra-wide → letterboxed (width-bound)
    ]) {
      const r = contentRect(w, h);
      expect(near(r.w / r.h, ASPECT)).toBe(true); // never stretched
      expect(r.w).toBeLessThanOrEqual(w + 1e-9);
      expect(r.h).toBeLessThanOrEqual(h + 1e-9);
      expect(near(r.x, (w - r.w) / 2)).toBe(true); // centred
      expect(near(r.y, (h - r.h) / 2)).toBe(true);
    }
  });

  it('fills exactly when the CSS box is already 16:9', () => {
    const r = contentRect(1600, 900);
    expect(r).toEqual({ x: 0, y: 0, w: 1600, h: 900 });
  });
});

describe('Whiteboard resize / coordinate stability (issue #225)', () => {
  it('redraws from the op-log on resize WITHOUT mutating stored normalised ops', () => {
    const s = stub();
    const wb = new Whiteboard(s.canvas, vi.fn());
    const op = draw('p1:0') as WbOp & { points: [number, number][] };
    const before = JSON.stringify(op.points);
    wb.applyOp(op);
    s.setSize(400, 1200); // a wildly different aspect
    wb.resize();
    s.setSize(1920, 1080);
    wb.resize();
    // Stored coords are the source of truth and must never be scaled in place.
    expect(JSON.stringify(op.points)).toBe(before);
  });

  it('maps a pointer to the SAME logical coord across repeated resizes (no drift)', () => {
    // Drawing at the centre of the content box must yield ~[0.5,0.5] at any window size.
    const sizes: [number, number][] = [
      [800, 600],
      [1600, 900],
      [400, 1200],
      [1280, 720],
    ];
    const captured: [number, number][] = [];
    for (const [w, h] of sizes) {
      const s = stub(w, h);
      const send = vi.fn();
      const wb = new Whiteboard(s.canvas, send);
      wb.resize();
      // Tap the centre of the WHOLE CSS box, which is also the centre of the centred
      // content box → logical (0.5, 0.5) regardless of letterboxing.
      gesture(s, [[w / 2, h / 2]]);
      const op = send.mock.calls.at(-1)?.[0] as WbOp & { points: [number, number][] };
      captured.push(op.points[op.points.length - 1]);
    }
    for (const [x, y] of captured) {
      expect(Math.abs(x - 0.5)).toBeLessThan(1e-6);
      expect(Math.abs(y - 0.5)).toBeLessThan(1e-6);
    }
  });

  it('clamps pointers outside the letterbox to the [0,1] logical range', () => {
    const s = stub(2000, 500); // ultra-wide → wide pillars left/right
    const send = vi.fn();
    const wb = new Whiteboard(s.canvas, send);
    wb.resize();
    gesture(s, [[0, 250]]); // far-left margin, vertically centred
    const op = send.mock.calls.at(-1)?.[0] as WbOp & { points: [number, number][] };
    const [x, y] = op.points[0];
    expect(x).toBeGreaterThanOrEqual(0);
    expect(x).toBeLessThanOrEqual(1);
    expect(Math.abs(y - 0.5)).toBeLessThan(1e-6);
  });
});

describe('Whiteboard export uses the logical size (issue #225)', () => {
  it('renders export pages at the fixed 16:9 logical resolution, matching the UI', () => {
    // Capture the offscreen canvases renderPage() creates, regardless of the live board's
    // CSS aspect — export must always be the logical 16:9 buffer (no UI-size leakage).
    const canvases: Array<{ width: number; height: number }> = [];
    const realDoc = (globalThis as { document?: unknown }).document;
    const realUrl = (globalThis as { URL?: unknown }).URL;
    (globalThis as { document?: unknown }).document = {
      createElement: (tag: string) => {
        if (tag === 'canvas') {
          const c = {
            width: 0,
            height: 0,
            getContext: () => new Proxy({}, { get: () => () => {} }),
            toDataURL: () => 'data:image/jpeg;base64,QUJD',
          };
          canvases.push(c as unknown as { width: number; height: number });
          return c;
        }
        return { href: '', download: '', click: () => {}, remove: () => {} }; // <a> for download
      },
      body: { appendChild: () => {} },
    };
    (globalThis as { URL?: unknown }).URL = { createObjectURL: () => 'blob:x', revokeObjectURL: () => {} };
    try {
      const s = stub(400, 1200); // tall, non-16:9 live board
      const send = vi.fn();
      const wb = new Whiteboard(s.canvas, send);
      wb.resize();
      wb.applyOp(draw('p1:0', 'ellipse'));
      wb.exportPdf(); // renders one offscreen canvas per page at the logical size
      expect(canvases.length).toBeGreaterThan(0);
      for (const c of canvases) {
        expect(c.width).toBe(1600);
        expect(c.height).toBe(900);
        expect(c.width / c.height).toBeCloseTo(1600 / 900, 6);
      }
    } finally {
      (globalThis as { document?: unknown }).document = realDoc;
      (globalThis as { URL?: unknown }).URL = realUrl;
    }
  });
});

describe('Whiteboard multi-page model (spec 0062)', () => {
  it('starts on a single default page', () => {
    const { canvas } = stub();
    const wb = new Whiteboard(canvas, vi.fn());
    expect(wb.pageCount()).toBe(1);
    expect(wb.pageIndex()).toBe(0);
  });

  it('a remote draw on an unseen page registers that page', () => {
    const { canvas } = stub();
    const wb = new Whiteboard(canvas, vi.fn());
    wb.applyOp(draw('p1:0'));
    expect(wb.pageCount()).toBe(1);
    wb.applyOp(draw('pX:0', 'rect'));
    expect(wb.pageCount()).toBe(2);
  });

  it('addPage relays a page-add marker, switches to it, and notifies', () => {
    const { canvas } = stub();
    const send = vi.fn();
    const onPages = vi.fn();
    const wb = new Whiteboard(canvas, send, onPages);
    onPages.mockClear();
    wb.addPage();
    expect(wb.pageCount()).toBe(2);
    expect(wb.pageIndex()).toBe(1); // viewing the new page
    expect(send).toHaveBeenCalledWith(expect.objectContaining({ op: 'draw', tool: 'page-add' }));
    expect(onPages).toHaveBeenCalledWith(2, 1);
  });

  it('duplicatePage copies the current page drawables onto a new page', () => {
    const { canvas } = stub();
    const send = vi.fn();
    const wb = new Whiteboard(canvas, send);
    wb.applyOp(draw('p1:0'));
    wb.applyOp(draw('p1:1', 'rect'));
    send.mockClear();
    wb.duplicatePage();
    expect(wb.pageCount()).toBe(2);
    // one page-add + two drawable copies relayed
    const tools = send.mock.calls.map((c) => (c[0] as WbOp & { tool?: string }).tool);
    expect(tools.filter((t) => t === 'page-add').length).toBe(1);
    expect(tools.filter((t) => t === 'pen' || t === 'rect').length).toBe(2);
  });

  it('deleteCurrentPage relays page-del and never deletes the last page', () => {
    const { canvas } = stub();
    const send = vi.fn();
    const wb = new Whiteboard(canvas, send);
    wb.addPage(); // now 2 pages, on page 2
    send.mockClear();
    wb.deleteCurrentPage();
    expect(wb.pageCount()).toBe(1);
    expect(send).toHaveBeenCalledWith(expect.objectContaining({ tool: 'page-del' }));
    // last page can't be deleted
    send.mockClear();
    wb.deleteCurrentPage();
    expect(wb.pageCount()).toBe(1);
    expect(send).not.toHaveBeenCalled();
  });

  it('clearPage relays a page-clear marker for the current page', () => {
    const { canvas } = stub();
    const send = vi.fn();
    const wb = new Whiteboard(canvas, send);
    wb.applyOp(draw('p1:0'));
    send.mockClear();
    wb.clearPage();
    expect(send).toHaveBeenCalledWith(expect.objectContaining({ tool: 'page-clear' }));
  });

  it('applySnapshot reconstructs pages, honouring add/del markers', () => {
    const { canvas } = stub();
    const wb = new Whiteboard(canvas, vi.fn());
    const ops: WbOp[] = [
      draw('p1:0'),
      { op: 'draw', id: 'pA:add', tool: 'page-add', color: '', width: 0, points: [] },
      draw('pA:1', 'rect'),
      { op: 'draw', id: 'pB:add', tool: 'page-add', color: '', width: 0, points: [] },
      { op: 'draw', id: 'pB:del', tool: 'page-del', color: '', width: 0, points: [] },
    ];
    wb.applySnapshot(ops);
    expect(wb.pageCount()).toBe(2); // p1 + pA (pB was added then deleted)
    expect(wb.pageIndex()).toBe(0);
  });

  it('a legacy global clear resets to a single page', () => {
    const { canvas } = stub();
    const wb = new Whiteboard(canvas, vi.fn());
    wb.addPage();
    expect(wb.pageCount()).toBe(2);
    wb.applyOp({ op: 'clear' });
    expect(wb.pageCount()).toBe(1);
    expect(wb.pageIndex()).toBe(0);
  });
});

/** A structural marker op (page-add / page-del / page-clear) as relayed by a peer. */
const marker = (tool: 'page-add' | 'page-del' | 'page-clear', pid: string): WbOp => ({
  op: 'draw',
  id: `${pid}:${tool}`,
  tool,
  color: '',
  width: 0,
  points: [],
});

const ev = (x: number, y: number): PointerEvent =>
  ({ clientX: x, clientY: y, pointerId: 1, preventDefault: () => {} }) as unknown as PointerEvent;

describe('Whiteboard shape tools (spec 0062)', () => {
  it('commits a rect on pointerup with exactly [start, end] normalised points', () => {
    const s = stub(1600, 900); // 16:9 → the content box fills the CSS box
    const send = vi.fn();
    const wb = new Whiteboard(s.canvas, send);
    wb.resize();
    wb.tool = 'rect';
    gesture(s, [
      [160, 90],
      [800, 450],
    ]);
    expect(send).toHaveBeenCalledTimes(1);
    const op = send.mock.calls[0][0] as WbOp & { points: [number, number][] };
    expect(op.op).toBe('draw');
    if (op.op !== 'draw') return;
    expect(op.tool).toBe('rect');
    expect(op.points.length).toBe(2);
    expect(op.points[0][0]).toBeCloseTo(0.1, 6);
    expect(op.points[0][1]).toBeCloseTo(0.1, 6);
    expect(op.points[1][0]).toBeCloseTo(0.5, 6);
    expect(op.points[1][1]).toBeCloseTo(0.5, 6);
  });

  it('previews shapes through rAF, coalescing bursts to one frame', () => {
    const s = stub(1600, 900);
    const send = vi.fn();
    const wb = new Whiteboard(s.canvas, send);
    wb.resize();
    wb.tool = 'ellipse';
    rafCbs.clear();
    s.listeners.get('pointerdown')?.(ev(160, 90));
    s.listeners.get('pointermove')?.(ev(400, 300)); // schedules one preview frame
    s.listeners.get('pointermove')?.(ev(500, 300)); // coalesced — no second rAF
    expect(rafCbs.size).toBe(1);
    flushRaf(); // the rubber-band renders; nothing is relayed yet
    expect(send).not.toHaveBeenCalled();
    s.listeners.get('pointerup')?.(ev(800, 450));
    expect(send).toHaveBeenCalledTimes(1);
    const op = send.mock.calls[0][0] as WbOp;
    expect(op.op === 'draw' && op.tool).toBe('ellipse');
  });

  it('cancels a pending preview on pointerup and ignores zero-size shapes', () => {
    const s = stub(1600, 900);
    const send = vi.fn();
    const wb = new Whiteboard(s.canvas, send);
    wb.resize();
    wb.tool = 'line';
    rafCbs.clear();
    s.listeners.get('pointerdown')?.(ev(300, 300));
    s.listeners.get('pointermove')?.(ev(600, 400)); // preview armed but never flushed
    s.listeners.get('pointerup')?.(ev(301, 300)); // back at the start → a stray click
    expect(send).not.toHaveBeenCalled(); // zero-size shape dropped
    expect(rafCbs.size).toBe(0); // and its preview frame cancelled
  });
});

describe('Whiteboard free-hand batching (spec 0045)', () => {
  it('flushes point batches on the 55ms cadence, chaining batches via the last point', () => {
    vi.useFakeTimers();
    const s = stub(1600, 900);
    const send = vi.fn();
    const wb = new Whiteboard(s.canvas, send);
    wb.resize();
    s.listeners.get('pointerdown')?.(ev(160, 90));
    s.listeners.get('pointermove')?.(ev(320, 180));
    expect(send).not.toHaveBeenCalled(); // still batching
    vi.advanceTimersByTime(55);
    expect(send).toHaveBeenCalledTimes(1);
    const first = send.mock.calls[0][0] as WbOp & { points: [number, number][]; id: string };
    expect(first.points.length).toBe(2);
    s.listeners.get('pointermove')?.(ev(480, 270));
    vi.advanceTimersByTime(55);
    expect(send).toHaveBeenCalledTimes(2);
    const second = send.mock.calls[1][0] as WbOp & { points: [number, number][]; id: string };
    // seamless joins: each batch re-carries the previous batch's last point
    expect(second.points[0]).toEqual(first.points[first.points.length - 1]);
    expect(second.id).toBe(first.id); // same stroke
    s.listeners.get('pointerup')?.(ev(480, 270)); // nothing pending → no extra op
    expect(send).toHaveBeenCalledTimes(2);
  });

  it('scales the op width for the eraser and highlighter', () => {
    const s = stub(1600, 900);
    const send = vi.fn();
    const wb = new Whiteboard(s.canvas, send);
    wb.resize();
    const widthOf = (tool: WbTool, key: 's' | 'm' | 'l'): number => {
      wb.tool = tool;
      wb.widthKey = key;
      send.mockClear();
      gesture(s, [[800, 450]]);
      return (send.mock.calls[0][0] as WbOp & { width: number }).width;
    };
    expect(widthOf('eraser', 'm')).toBeCloseTo(0.006 * 6, 9); // chunky eraser
    expect(widthOf('highlighter', 's')).toBeCloseTo(0.0035 * 4, 9); // broad highlighter
    expect(widthOf('pen', 'l')).toBeCloseTo(0.011, 9); // pen uses the base width
  });

  it('maps pointers through the bounding rect before the first resize()', () => {
    const s = stub(1600, 900);
    const send = vi.fn();
    const wb = new Whiteboard(s.canvas, send);
    expect(wb.pageCount()).toBe(1); // constructed, never resized
    gesture(s, [[800, 450]]); // centre tap with no content box computed yet
    const op = send.mock.calls[0][0] as WbOp & { points: [number, number][] };
    expect(op.points[0][0]).toBeCloseTo(0.5, 6);
    expect(op.points[0][1]).toBeCloseTo(0.5, 6);
  });
});

describe('Whiteboard remote marker ops (spec 0062)', () => {
  it('applies a remote page-del, falling back to the first page, and never relays', () => {
    const send = vi.fn();
    const onPages = vi.fn();
    const { canvas } = stub();
    const wb = new Whiteboard(canvas, send, onPages);
    wb.applyOp(marker('page-add', 'pA'));
    wb.setPageIndex(1); // viewing pA
    wb.applyOp(draw('pA:1'));
    wb.applyOp(marker('page-del', 'pA'));
    expect(wb.pageCount()).toBe(1);
    expect(wb.pageIndex()).toBe(0); // dropped back to p1
    expect(onPages).toHaveBeenLastCalledWith(1, 0);
    expect(send).not.toHaveBeenCalled(); // remote ops must never echo
    wb.applyOp(marker('page-del', 'p1')); // the last page is indestructible
    expect(wb.pageCount()).toBe(1);
  });

  it('applies a remote page-clear only to the target page', () => {
    const send = vi.fn();
    const { canvas } = stub();
    const wb = new Whiteboard(canvas, send);
    wb.applyOp(draw('p1:0'));
    wb.applyOp(draw('pA:0')); // a drawable on an unseen page registers it
    expect(wb.pageCount()).toBe(2);
    wb.applyOp(marker('page-clear', 'p1')); // current page → redrawn
    wb.applyOp(marker('page-clear', 'pZ')); // some other page → no redraw
    expect(wb.pageCount()).toBe(2); // clearing never removes pages
    // Behavioural proof p1 is now empty: duplicating it relays ONLY the page-add.
    send.mockClear();
    wb.duplicatePage();
    expect(send).toHaveBeenCalledTimes(1);
    expect((send.mock.calls[0][0] as { tool: string }).tool).toBe('page-add');
  });
});

describe('Whiteboard page navigation (spec 0062)', () => {
  it('nextPage/prevPage/setPageIndex clamp to the valid range and skip no-ops', () => {
    const onPages = vi.fn();
    const { canvas } = stub();
    const wb = new Whiteboard(canvas, vi.fn(), onPages);
    wb.applyOp(marker('page-add', 'pA'));
    wb.applyOp(marker('page-add', 'pB')); // three pages, viewing p1
    onPages.mockClear();
    wb.nextPage();
    expect(wb.pageIndex()).toBe(1);
    wb.nextPage();
    expect(wb.pageIndex()).toBe(2);
    wb.nextPage(); // already at the end
    expect(wb.pageIndex()).toBe(2);
    wb.prevPage();
    expect(wb.pageIndex()).toBe(1);
    wb.setPageIndex(0);
    expect(wb.pageIndex()).toBe(0);
    wb.setPageIndex(0); // same page → no notify
    wb.setPageIndex(99); // out of range → no notify
    expect(onPages).toHaveBeenCalledTimes(4); // only the real switches
  });
});

describe('Whiteboard lifecycle', () => {
  it('reset drops pages, ops and pending batches without relaying', () => {
    vi.useFakeTimers();
    const s = stub(1600, 900);
    const send = vi.fn();
    const wb = new Whiteboard(s.canvas, send);
    wb.resize();
    wb.addPage();
    s.listeners.get('pointerdown')?.(ev(160, 90));
    s.listeners.get('pointermove')?.(ev(320, 180)); // a batch is now pending
    send.mockClear();
    wb.reset();
    expect(wb.pageCount()).toBe(1);
    expect(wb.pageIndex()).toBe(0);
    vi.advanceTimersByTime(200); // the armed flush timer must be dead
    expect(send).not.toHaveBeenCalled();
  });

  it('captureStream hands out the canvas stream at the requested fps (#230)', () => {
    const s = stub();
    const wb = new Whiteboard(s.canvas, vi.fn());
    wb.captureStream();
    expect(s.capture).toHaveBeenCalledWith(10); // default recorder cadence
    wb.captureStream(30);
    expect(s.capture).toHaveBeenCalledWith(30);
  });

  it('re-fits when the canvas element itself resizes (ResizeObserver → one rAF)', () => {
    const s = stub(640, 360);
    new Whiteboard(s.canvas, vi.fn());
    const ro = FakeResizeObserver.instances.at(-1);
    expect(ro?.observed).toContain(s.canvas);
    rafCbs.clear();
    s.setSize(1280, 720); // e.g. the chat panel closed — no window resize fires
    ro?.cb();
    ro?.cb(); // burst: coalesced into a single scheduled re-fit
    expect(rafCbs.size).toBe(1);
    flushRaf();
    expect((s.canvas as unknown as { width: number }).width).toBe(1280); // dpr 1
    expect((s.canvas as unknown as { height: number }).height).toBe(720);
  });

  it('positions the .wb-frame overlay exactly over the letterboxed content box', () => {
    const s = stub(2000, 500); // ultra-wide → pillarboxed
    const wb = new Whiteboard(s.canvas, vi.fn());
    wb.resize();
    const r = contentRect(2000, 500);
    expect(s.frameStyle.left).toBe(`${r.x}px`);
    expect(s.frameStyle.top).toBe(`${r.y}px`);
    expect(s.frameStyle.width).toBe(`${r.w}px`);
    expect(s.frameStyle.height).toBe(`${r.h}px`);
  });
});

describe('Whiteboard exportPng (spec 0062)', () => {
  it('downloads the current page as a PNG named by page number', async () => {
    const anchors: Array<{ href: string; download: string; clicked: boolean }> = [];
    const revoke = vi.fn();
    const realDoc = (globalThis as { document?: unknown }).document;
    const realUrl = (globalThis as { URL?: unknown }).URL;
    (globalThis as { document?: unknown }).document = {
      createElement: (tag: string) => {
        if (tag === 'canvas') {
          return {
            width: 0,
            height: 0,
            getContext: () => new Proxy({}, { get: () => () => {} }),
            toBlob: (cb: (b: Blob | null) => void) => cb(new Blob(['png'], { type: 'image/png' })),
          };
        }
        const a = {
          href: '',
          download: '',
          clicked: false,
          click(): void {
            this.clicked = true;
          },
          remove(): void {},
        };
        anchors.push(a);
        return a;
      },
      body: { appendChild: () => {} },
    };
    (globalThis as { URL?: unknown }).URL = { createObjectURL: () => 'blob:png', revokeObjectURL: revoke };
    vi.useFakeTimers();
    try {
      const s = stub();
      const wb = new Whiteboard(s.canvas, vi.fn());
      wb.applyOp(draw('p1:0'));
      await wb.exportPng();
      expect(anchors.length).toBe(1);
      expect(anchors[0].download).toBe('whiteboard-page-1.png');
      expect(anchors[0].clicked).toBe(true);
      vi.runAllTimers(); // the deferred object-URL release
      expect(revoke).toHaveBeenCalledWith('blob:png');
    } finally {
      vi.useRealTimers();
      (globalThis as { document?: unknown }).document = realDoc;
      (globalThis as { URL?: unknown }).URL = realUrl;
    }
  });
});

describe('Whiteboard snapshot edge cases (spec 0062)', () => {
  it('tolerates junk, honours page-clear, and survives every page being deleted', () => {
    const { canvas } = stub();
    const wb = new Whiteboard(canvas, vi.fn());
    wb.applySnapshot(null as unknown as WbOp[]); // non-array payload → empty board
    expect(wb.pageCount()).toBe(1);
    wb.applySnapshot([
      { op: 'clear' }, // non-draw entries are skipped
      draw('p1:0'),
      marker('page-clear', 'p1'), // wipes p1's drawables
      marker('page-del', 'p1'), // snapshots may even delete the default page
    ]);
    expect(wb.pageCount()).toBe(1); // restored to a single default page
    expect(wb.pageIndex()).toBe(0);
  });

  it('moves the view back to the first page when the current page vanishes', () => {
    const { canvas } = stub();
    const wb = new Whiteboard(canvas, vi.fn());
    wb.applyOp(marker('page-add', 'pA'));
    wb.setPageIndex(1); // viewing pA
    wb.applySnapshot([draw('p1:0')]); // the server log has no pA
    expect(wb.pageCount()).toBe(1);
    expect(wb.pageIndex()).toBe(0);
  });
});
