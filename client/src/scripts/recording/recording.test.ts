import { describe, it, expect, vi, afterEach } from 'vitest';
import { COMP_W, COMP_H, GAP, computeLayout, containFit } from './layout';
import {
  MIME_CANDIDATES,
  formatElapsed,
  hueOf,
  initials,
  isRecordingSupported,
  pickMimeType,
  recordingFilename,
} from './utils';

describe('computeLayout', () => {
  it('1 participant fills the frame', () => {
    expect(computeLayout(1)).toEqual([{ x: 0, y: 0, w: 1280, h: 720 }]);
  });

  it('2 participants are side-by-side full-height columns', () => {
    expect(computeLayout(2)).toEqual([
      { x: 0, y: 0, w: 638, h: 720 },
      { x: 642, y: 0, w: 638, h: 720 },
    ]);
  });

  it('3 participants are two top + one centered bottom', () => {
    expect(computeLayout(3)).toEqual([
      { x: 0, y: 0, w: 638, h: 358 },
      { x: 642, y: 0, w: 638, h: 358 },
      { x: 321, y: 362, w: 638, h: 358 },
    ]);
  });

  it('4 participants are a 2×2 grid', () => {
    expect(computeLayout(4)).toEqual([
      { x: 0, y: 0, w: 638, h: 358 },
      { x: 642, y: 0, w: 638, h: 358 },
      { x: 0, y: 362, w: 638, h: 358 },
      { x: 642, y: 362, w: 638, h: 358 },
    ]);
  });

  it('keeps a 4px gap between tiles and stays inside the canvas', () => {
    for (const n of [2, 3, 4]) {
      for (const t of computeLayout(n)) {
        expect(t.x).toBeGreaterThanOrEqual(0);
        expect(t.y).toBeGreaterThanOrEqual(0);
        expect(t.x + t.w).toBeLessThanOrEqual(COMP_W);
        expect(t.y + t.h).toBeLessThanOrEqual(COMP_H);
      }
    }
    const [l, r] = computeLayout(2);
    expect(r!.x - (l!.x + l!.w)).toBe(GAP);
    const [top, , bottom] = computeLayout(4) as [
      { y: number; h: number },
      unknown,
      { y: number },
    ];
    expect(bottom.y - (top.y + top.h)).toBe(GAP);
  });

  it('clamps out-of-range counts to 1..4', () => {
    expect(computeLayout(0)).toEqual(computeLayout(1));
    expect(computeLayout(-2)).toEqual(computeLayout(1));
    expect(computeLayout(9)).toEqual(computeLayout(4));
  });
});

describe('containFit', () => {
  const tile = { x: 100, y: 50, w: 638, h: 358 };

  it('letterboxes a wide source in a taller tile (bars top/bottom)', () => {
    // 1920×800 (2.4:1) into 638×358: width-bound → scale 638/1920.
    const fit = containFit(1920, 800, tile);
    expect(fit.w).toBeCloseTo(638, 5);
    expect(fit.h).toBeCloseTo(800 * (638 / 1920), 5);
    expect(fit.x).toBeCloseTo(100, 5);
    expect(fit.y).toBeCloseTo(50 + (358 - fit.h) / 2, 5);
  });

  it('letterboxes a tall source in a wider tile (bars left/right)', () => {
    // 720×1280 (portrait) into 638×358: height-bound → scale 358/1280.
    const fit = containFit(720, 1280, tile);
    expect(fit.h).toBeCloseTo(358, 5);
    expect(fit.w).toBeCloseTo(720 * (358 / 1280), 5);
    expect(fit.y).toBeCloseTo(50, 5);
    expect(fit.x).toBeCloseTo(100 + (638 - fit.w) / 2, 5);
  });

  it('never overflows the tile and preserves aspect ratio', () => {
    const fit = containFit(640, 480, tile);
    expect(fit.x).toBeGreaterThanOrEqual(tile.x);
    expect(fit.y).toBeGreaterThanOrEqual(tile.y);
    expect(fit.x + fit.w).toBeLessThanOrEqual(tile.x + tile.w + 1e-6);
    expect(fit.y + fit.h).toBeLessThanOrEqual(tile.y + tile.h + 1e-6);
    expect(fit.w / fit.h).toBeCloseTo(640 / 480, 5);
  });

  it('falls back to the full tile for degenerate sources', () => {
    expect(containFit(0, 1080, tile)).toEqual(tile);
    expect(containFit(1920, 0, tile)).toEqual(tile);
  });
});

describe('pickMimeType', () => {
  it('prefers vp9, then vp8, then bare webm', () => {
    expect(pickMimeType(() => true)).toBe('video/webm;codecs=vp9,opus');
    expect(pickMimeType((t) => !t.includes('vp9'))).toBe('video/webm;codecs=vp8,opus');
    expect(pickMimeType((t) => t === 'video/webm')).toBe('video/webm');
  });

  it('returns "" when nothing is supported (recorder default)', () => {
    expect(pickMimeType(() => false)).toBe('');
  });

  it('candidate order is the documented fallback chain', () => {
    expect(MIME_CANDIDATES).toEqual([
      'video/webm;codecs=vp9,opus',
      'video/webm;codecs=vp8,opus',
      'video/webm',
    ]);
  });
});

describe('recordingFilename', () => {
  const date = new Date(2026, 5, 10, 9, 7); // 2026-06-10 09:07 local

  it('formats voxtranslate-{room}-{YYYY-MM-DD-HHmm}.webm', () => {
    expect(recordingFilename('daily-standup', date)).toBe(
      'voxtranslate-daily-standup-2026-06-10-0907.webm',
    );
  });

  it('slugs unsafe rooms and falls back to "call"', () => {
    expect(recordingFilename('sala café/♥', date)).toBe('voxtranslate-salacaf-2026-06-10-0907.webm');
    expect(recordingFilename('♥♥♥', date)).toBe('voxtranslate-call-2026-06-10-0907.webm');
    expect(recordingFilename('x'.repeat(60), date)).toBe(
      `voxtranslate-${'x'.repeat(40)}-2026-06-10-0907.webm`,
    );
  });
});

describe('formatElapsed', () => {
  it('formats MM:SS', () => {
    expect(formatElapsed(0)).toBe('00:00');
    expect(formatElapsed(5_000)).toBe('00:05');
    expect(formatElapsed(65_000)).toBe('01:05');
    expect(formatElapsed(600_000)).toBe('10:00');
  });

  it('clamps negatives and lets minutes grow past 99', () => {
    expect(formatElapsed(-1_000)).toBe('00:00');
    expect(formatElapsed(100 * 60_000)).toBe('100:00');
  });
});

describe('placeholder helpers', () => {
  it('initials takes up to two words', () => {
    expect(initials('Ada Lovelace')).toBe('AL');
    expect(initials('ada lovelace king')).toBe('AL');
    expect(initials('bob')).toBe('B');
    expect(initials('   ')).toBe('');
  });

  it('hueOf is stable and within 0..359', () => {
    expect(hueOf('Alice')).toBe(hueOf('Alice'));
    for (const n of ['Alice', 'Bob', '中文', '']) {
      const h = hueOf(n);
      expect(h).toBeGreaterThanOrEqual(0);
      expect(h).toBeLessThan(360);
    }
  });
});

describe('pickMimeType default detector', () => {
  afterEach(() => vi.unstubAllGlobals());

  it('returns "" when MediaRecorder is missing entirely (stripped env)', () => {
    vi.stubGlobal('MediaRecorder', undefined);
    expect(pickMimeType()).toBe('');
  });

  it('consults MediaRecorder.isTypeSupported when present', () => {
    vi.stubGlobal('MediaRecorder', { isTypeSupported: (t: string) => t === 'video/webm' });
    expect(pickMimeType()).toBe('video/webm');
  });
});

describe('isRecordingSupported', () => {
  afterEach(() => vi.unstubAllGlobals());

  it('requires MediaRecorder + AudioContext + canvas captureStream (Safari lacks parts)', () => {
    vi.stubGlobal('MediaRecorder', undefined);
    vi.stubGlobal('AudioContext', undefined);
    vi.stubGlobal('HTMLCanvasElement', undefined);
    expect(isRecordingSupported()).toBe(false); // nothing at all

    vi.stubGlobal('MediaRecorder', class {});
    expect(isRecordingSupported()).toBe(false); // no AudioContext

    vi.stubGlobal('AudioContext', class {});
    expect(isRecordingSupported()).toBe(false); // no HTMLCanvasElement

    class FakeCanvas {}
    vi.stubGlobal('HTMLCanvasElement', FakeCanvas);
    expect(isRecordingSupported()).toBe(false); // canvas without captureStream

    (FakeCanvas.prototype as unknown as { captureStream: () => void }).captureStream = () =>
      undefined;
    expect(isRecordingSupported()).toBe(true); // full composite-recording stack
  });
});
