import { describe, it, expect, vi, beforeAll } from 'vitest';
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
}
function stub(w = 800, h = 600): CanvasStub {
  const ctx = new Proxy({}, { get: () => () => {} }) as unknown as CanvasRenderingContext2D;
  const listeners = new Map<string, (e: PointerEvent) => void>();
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
  } as unknown as HTMLCanvasElement;
  return {
    canvas,
    ctx,
    listeners,
    setSize: (nw: number, nh: number) => {
      cw = nw;
      ch = nh;
    },
  };
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
    const op = draw('p1:0');
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
