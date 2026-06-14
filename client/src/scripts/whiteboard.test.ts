import { describe, it, expect, vi } from 'vitest';
import { Whiteboard, pageOf, drawOp, type WbOp, type WbTool } from './whiteboard';

// The board needs a <canvas> + 2D context; in the node test env we stub both. Every
// ctx method is a no-op (the proxy returns a function for any property), so the page-
// model + op logic can be exercised without real rendering.
function stub(): { canvas: HTMLCanvasElement; ctx: CanvasRenderingContext2D } {
  const ctx = new Proxy({}, { get: () => () => {} }) as unknown as CanvasRenderingContext2D;
  const canvas = {
    clientWidth: 800,
    clientHeight: 600,
    width: 0,
    height: 0,
    getContext: () => ctx,
    addEventListener: () => {},
    setPointerCapture: () => {},
    releasePointerCapture: () => {},
    getBoundingClientRect: () => ({ left: 0, top: 0, width: 800, height: 600 }),
  } as unknown as HTMLCanvasElement;
  return { canvas, ctx };
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
