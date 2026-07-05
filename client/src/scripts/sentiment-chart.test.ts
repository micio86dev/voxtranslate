// @vitest-environment jsdom
//
// Sentiment timeline renderer (spec 0015): drawn against a fake 2D context —
// jsdom has no real canvas, so every ctx method is a spy and the assertions
// check the computed geometry (padding math, clamping, DPR sizing) and the
// stroke/fill colors captured at paint time.
import { afterEach, describe, expect, it, vi } from 'vitest';
import { drawSentimentTimeline } from './sentiment-chart';

// Geometry for the default 320×120 fallback box (PAD = 10/10/18/30):
// w = 280, h = 92 → x∈[30,310], y: +1→10, 0→56, −1→102.
const X_LEFT = 30;
const X_RIGHT = 310;
const Y_TOP = 10;
const Y_MID = 56;
const Y_BOT = 102;

function makeCanvas(clientW = 0, clientH = 0) {
  const canvas = document.createElement('canvas');
  if (clientW) Object.defineProperty(canvas, 'clientWidth', { value: clientW });
  if (clientH) Object.defineProperty(canvas, 'clientHeight', { value: clientH });
  const strokes: string[] = [];
  const fills: string[] = [];
  const ctx = {
    font: '',
    textAlign: '',
    textBaseline: '',
    fillStyle: '',
    strokeStyle: '',
    globalAlpha: 1,
    lineWidth: 0,
    lineJoin: '',
    scale: vi.fn(),
    clearRect: vi.fn(),
    beginPath: vi.fn(),
    moveTo: vi.fn(),
    lineTo: vi.fn(),
    setLineDash: vi.fn(),
    fillText: vi.fn(),
    arc: vi.fn(),
    stroke: vi.fn(),
    fill: vi.fn(),
  };
  // Capture the color in effect when each stroke()/fill() lands.
  ctx.stroke.mockImplementation(() => {
    strokes.push(String(ctx.strokeStyle));
  });
  ctx.fill.mockImplementation(() => {
    fills.push(String(ctx.fillStyle));
  });
  canvas.getContext = vi.fn(() => ctx) as unknown as HTMLCanvasElement['getContext'];
  return { canvas, ctx, strokes, fills };
}

function setDpr(value: number) {
  Object.defineProperty(window, 'devicePixelRatio', { value, configurable: true });
}

afterEach(() => {
  vi.unstubAllGlobals();
  setDpr(1);
});

describe('drawSentimentTimeline', () => {
  it('sizes the bitmap from the CSS box at the device pixel ratio', () => {
    setDpr(2);
    const { canvas, ctx } = makeCanvas(400, 150);
    drawSentimentTimeline(canvas, { points: [], durationSeconds: 60 });
    expect(canvas.width).toBe(800);
    expect(canvas.height).toBe(300);
    expect(ctx.scale).toHaveBeenCalledWith(2, 2);
    expect(ctx.clearRect).toHaveBeenCalledWith(0, 0, 400, 150);
  });

  it('falls back to 320×120 (and DPR 1) when the canvas has no layout', () => {
    setDpr(0); // covers the `|| 1` fallback
    const { canvas, ctx } = makeCanvas();
    drawSentimentTimeline(canvas, { points: [], durationSeconds: 60 });
    expect(canvas.width).toBe(320);
    expect(canvas.height).toBe(120);
    expect(ctx.scale).toHaveBeenCalledWith(1, 1);
    expect(ctx.clearRect).toHaveBeenCalledWith(0, 0, 320, 120);
  });

  it('bails out without touching the context when 2D is unavailable', () => {
    const { canvas, ctx } = makeCanvas();
    canvas.getContext = vi.fn(() => null) as unknown as HTMLCanvasElement['getContext'];
    expect(() =>
      drawSentimentTimeline(canvas, { points: [{ t: 0, score: 0 }], durationSeconds: 60 }),
    ).not.toThrow();
    expect(ctx.scale).not.toHaveBeenCalled();
    // Sizing happens before the context lookup.
    expect(canvas.width).toBe(320);
  });

  it('draws the frame guides with score labels and the axis end labels', () => {
    const { canvas, ctx } = makeCanvas();
    drawSentimentTimeline(canvas, { points: [], durationSeconds: 3600 });
    expect(ctx.fillText).toHaveBeenCalledWith('+1', X_LEFT - 6, Y_TOP);
    expect(ctx.fillText).toHaveBeenCalledWith('0', X_LEFT - 6, Y_MID);
    expect(ctx.fillText).toHaveBeenCalledWith('-1', X_LEFT - 6, Y_BOT);
    // fmtTime: 3600s → "1:00h"; origin label "0m".
    expect(ctx.fillText).toHaveBeenCalledWith('1:00h', X_RIGHT, Y_BOT + 4);
    expect(ctx.fillText).toHaveBeenCalledWith('0m', X_LEFT, Y_BOT + 4);
    // Guides: solid zero line + dashed ±1 lines.
    expect(ctx.setLineDash).toHaveBeenCalledWith([3, 3]);
    expect(ctx.setLineDash).toHaveBeenCalledWith([]);
  });

  it('formats sub-hour sessions in minutes and floors the duration at 1s', () => {
    const short = makeCanvas();
    drawSentimentTimeline(short.canvas, { points: [], durationSeconds: 300 });
    expect(short.ctx.fillText).toHaveBeenCalledWith('5m', X_RIGHT, Y_BOT + 4);

    const zero = makeCanvas();
    drawSentimentTimeline(zero.canvas, { points: [], durationSeconds: 0 });
    // Math.max(1, 0) → fmtTime(1) → "0m" at both ends, no division by zero.
    expect(zero.ctx.fillText).toHaveBeenCalledWith('0m', X_RIGHT, Y_BOT + 4);
  });

  it('sorts points by time and clamps both axes to the plot area', () => {
    const { canvas, ctx, strokes } = makeCanvas();
    drawSentimentTimeline(canvas, {
      // Unsorted on purpose; t=250 > duration and |score| > 1 must clamp.
      points: [
        { t: 250, score: 5 },
        { t: 0, score: -9 },
      ],
      durationSeconds: 100,
    });
    expect(ctx.moveTo).toHaveBeenCalledWith(X_LEFT, Y_BOT); // first (sorted) point
    expect(ctx.lineTo).toHaveBeenCalledWith(X_RIGHT, Y_TOP); // clamped t + score
    // Line drawn in the accent fallback color; one dot per point.
    expect(strokes).toContain('#3b82f6');
    expect(ctx.arc).toHaveBeenCalledWith(X_LEFT, Y_BOT, 2.5, 0, Math.PI * 2);
    expect(ctx.arc).toHaveBeenCalledWith(X_RIGHT, Y_TOP, 2.5, 0, Math.PI * 2);
    expect(ctx.fill).toHaveBeenCalledTimes(2);
  });

  it('draws bookmarks as dashed vertical lines even when there are no points', () => {
    const { canvas, ctx, strokes } = makeCanvas();
    drawSentimentTimeline(canvas, {
      points: [],
      durationSeconds: 100,
      bookmarks: [50],
    });
    // Vertical line at x(50) = 170 spanning the plot height.
    expect(ctx.moveTo).toHaveBeenCalledWith(170, Y_TOP);
    expect(ctx.lineTo).toHaveBeenCalledWith(170, Y_BOT);
    expect(ctx.setLineDash).toHaveBeenCalledWith([4, 3]);
    expect(strokes).toContain('#f59e0b'); // warning fallback
    // No mood line, no dots: 3 frame strokes + 1 bookmark stroke only.
    expect(ctx.stroke).toHaveBeenCalledTimes(4);
    expect(ctx.arc).not.toHaveBeenCalled();
    expect(ctx.fill).not.toHaveBeenCalled();
  });

  it('rings key moments in the danger color when negative, accent when positive', () => {
    const { canvas, ctx, strokes } = makeCanvas();
    drawSentimentTimeline(canvas, {
      points: [
        { t: 0, score: 0 },
        { t: 100, score: 0.5 },
      ],
      durationSeconds: 100,
      keyMoments: [
        { t: 50, score: -0.5 },
        { t: 100, score: 0.5 },
      ],
    });
    // 2 window dots (r 2.5) + 2 key-moment rings (r 5).
    const radii = ctx.arc.mock.calls.map((c) => c[2] as number);
    expect(radii.filter((r) => r === 2.5)).toHaveLength(2);
    expect(radii.filter((r) => r === 5)).toHaveLength(2);
    expect(strokes).toContain('#ef4444'); // negative ring → danger fallback
    expect(strokes.filter((s) => s === '#3b82f6').length).toBeGreaterThanOrEqual(2); // line + positive ring
  });

  it('follows the app theme via CSS custom properties when they resolve', () => {
    vi.stubGlobal(
      'getComputedStyle',
      () =>
        ({
          getPropertyValue: (name: string) => (name === '--accent' ? ' #ff0000 ' : ''),
        }) as unknown as CSSStyleDeclaration,
    );
    const { canvas, strokes } = makeCanvas();
    drawSentimentTimeline(canvas, {
      points: [
        { t: 0, score: 0 },
        { t: 10, score: 1 },
      ],
      durationSeconds: 60,
    });
    expect(strokes).toContain('#ff0000'); // trimmed themed accent, not the fallback
  });
});
