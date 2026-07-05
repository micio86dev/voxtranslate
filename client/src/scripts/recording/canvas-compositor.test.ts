import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { CanvasCompositor } from './canvas-compositor';
import { COMP_W, COMP_H } from './layout';
import type { ParticipantSource } from './types';

// Node-env harness: the compositor only touches document.createElement, rAF and
// window.setInterval — all stubbed here so every frame is driven by hand.

function makeCtx(withRoundRect = true) {
  const ctx: Record<string, any> = {
    fillStyle: '',
    font: '',
    textAlign: '',
    textBaseline: '',
    fillRect: vi.fn(),
    beginPath: vi.fn(),
    arc: vi.fn(),
    fill: vi.fn(),
    fillText: vi.fn(),
    measureText: vi.fn(() => ({ width: 40 })),
    save: vi.fn(),
    restore: vi.fn(),
    rect: vi.fn(),
    clip: vi.fn(),
    drawImage: vi.fn(),
  };
  if (withRoundRect) ctx.roundRect = vi.fn();
  return ctx;
}

let playRejects = false;

class FakeVideo {
  muted = false;
  playsInline = false;
  srcObject: any = null;
  videoWidth = 0;
  videoHeight = 0;
  play = vi.fn(() =>
    playRejects ? Promise.reject(new Error('autoplay blocked')) : Promise.resolve(),
  );
  pause = vi.fn();
}

let ctx2d: Record<string, any>;
let canvasEl: any;
let videos: FakeVideo[];
let rafCbs: ((now: number) => void)[];
let tickFns: (() => void)[];
let cancelRaf: ReturnType<typeof vi.fn>;

beforeEach(() => {
  ctx2d = makeCtx();
  videos = [];
  rafCbs = [];
  tickFns = [];
  playRejects = false;
  cancelRaf = vi.fn();
  canvasEl = {
    width: 0,
    height: 0,
    getContext: vi.fn(() => ctx2d),
    captureStream: vi.fn((fps?: number) => ({ fps })),
  };
  vi.stubGlobal('document', {
    createElement: (tag: string) => {
      if (tag === 'canvas') return canvasEl;
      const v = new FakeVideo();
      videos.push(v);
      return v;
    },
  });
  vi.stubGlobal('requestAnimationFrame', (cb: (now: number) => void) => {
    rafCbs.push(cb);
    return rafCbs.length;
  });
  vi.stubGlobal('cancelAnimationFrame', cancelRaf);
  vi.stubGlobal('window', {
    setInterval: (fn: () => void) => {
      tickFns.push(fn);
      return 7777;
    },
  });
});

afterEach(() => {
  vi.unstubAllGlobals();
});

const track = (id: string) => ({ id, kind: 'video' });
const streamOf = (t: unknown) => ({ getVideoTracks: () => (t ? [t] : []) }) as any;
const src = (peerId: string, stream: any, videoOff = false, name = peerId): ParticipantSource => ({
  peerId,
  name,
  stream,
  videoOff,
});

describe('CanvasCompositor', () => {
  it('throws a clear error when the 2d context is unavailable', () => {
    canvasEl.getContext = vi.fn(() => null);
    expect(() => new CanvasCompositor()).toThrow('2d canvas unavailable');
  });

  it('sizes the detached canvas to 1280×720 and captures at the requested fps', () => {
    const c = new CanvasCompositor();
    expect(canvasEl.width).toBe(COMP_W);
    expect(canvasEl.height).toBe(COMP_H);
    expect(c.captureStream()).toEqual({ fps: 30 }); // default = composite FPS
    expect(canvasEl.captureStream).toHaveBeenCalledWith(30);
    c.captureStream(15);
    expect(canvasEl.captureStream).toHaveBeenCalledWith(15);
  });

  it('throttles the rAF loop to ~30fps and keeps the 1s background-tab tick', () => {
    const c = new CanvasCompositor();
    c.start([src('me', null)]);
    expect(rafCbs.length).toBe(1);
    expect(tickFns.length).toBe(1);

    rafCbs[0]!(10); // 10ms since last frame < 33.3ms → skipped, but rescheduled
    expect(ctx2d.fillRect).not.toHaveBeenCalled();
    expect(rafCbs.length).toBe(2);

    rafCbs[1]!(40); // ≥ one frame period → draws the black base then tiles
    expect(ctx2d.fillRect).toHaveBeenCalledWith(0, 0, COMP_W, COMP_H);

    const draws = ctx2d.fillRect.mock.calls.length;
    tickFns[0]!(); // safety tick draws even while rAF is paused (background tab)
    expect(ctx2d.fillRect.mock.calls.length).toBeGreaterThan(draws);

    c.stop();
    const scheduled = rafCbs.length;
    const drawsBefore = ctx2d.fillRect.mock.calls.length;
    rafCbs[scheduled - 1]!(1000); // stopped → loop exits: no draw, no reschedule
    expect(rafCbs.length).toBe(scheduled);
    expect(ctx2d.fillRect.mock.calls.length).toBe(drawsBefore);
    expect(cancelRaf).toHaveBeenCalled();
  });

  it('draws live video via containFit and placeholders for camera-off/frame-less tiles', () => {
    const c = new CanvasCompositor();
    const sources = [
      src('alice', streamOf(track('a')), false, 'Alice Ng'),
      src('bob', streamOf(track('b')), true, 'Bob'), // camera off → placeholder
      src('carol', null, false, ''), // no media yet → placeholder '?'
    ];
    c.start(sources);
    expect(videos.length).toBe(3);
    videos[0]!.videoWidth = 1920; // only Alice has decodable frames
    videos[0]!.videoHeight = 800;

    tickFns[0]!(); // one composite frame

    // Alice's tile letterboxes her 1920×800 into the 638×358 top-left tile.
    expect(ctx2d.drawImage).toHaveBeenCalledTimes(1);
    const [img, , y, w, h] = ctx2d.drawImage.mock.calls[0]!;
    expect(img).toBe(videos[0]);
    expect(w).toBeCloseTo(638, 5);
    expect(h).toBeCloseTo(800 * (638 / 1920), 5);
    expect(y).toBeCloseTo((358 - 800 * (638 / 1920)) / 2, 5);

    // Placeholders: initials disc for Bob, '?' fallback for the unnamed peer.
    const texts = ctx2d.fillText.mock.calls.map((call: any[]) => call[0]);
    expect(texts).toContain('B');
    expect(texts).toContain('?');
    expect(ctx2d.arc).toHaveBeenCalledTimes(2);

    // Every tile gets a clipped name pill (rounded when roundRect exists).
    expect(ctx2d.clip).toHaveBeenCalledTimes(3);
    expect(ctx2d.roundRect).toHaveBeenCalledTimes(3);
    expect(texts).toContain('Alice Ng');
    c.stop();
  });

  it('falls back to fillRect for the name pill when roundRect is missing', () => {
    ctx2d = makeCtx(false); // Safari-ish 2d context without roundRect
    canvasEl.getContext = vi.fn(() => ctx2d);
    const c = new CanvasCompositor();
    c.start([src('p', null, false, 'Pat')]);
    tickFns[0]!();
    // The pill is the only 22px-high rect in the frame.
    expect(ctx2d.fillRect.mock.calls.some((call: any[]) => call[3] === 22)).toBe(true);
    c.stop();
  });

  it('rebinds srcObject only when the underlying video track changes (no flicker)', () => {
    const c = new CanvasCompositor();
    const tA = track('a');
    const s1 = streamOf(tA);
    c.setSources([src('p', s1)]);
    const v = videos[0]!;
    expect(v.muted).toBe(true);
    expect(v.playsInline).toBe(true);
    expect(v.srcObject).toBe(s1);
    expect(v.play).toHaveBeenCalledTimes(1);

    // syncRoster hands a FRESH MediaStream wrapper around the SAME track ~1×/s:
    // the element must not reload (that made the self tile flicker).
    c.setSources([src('p', streamOf(tA))]);
    expect(v.srcObject).toBe(s1);
    expect(v.play).toHaveBeenCalledTimes(1);

    const s3 = streamOf(track('b')); // real track change (camera ↔ screen share)
    c.updateSource('p', s3);
    expect(v.srcObject).toBe(s3);
    expect(v.play).toHaveBeenCalledTimes(2);

    c.updateSource('p', null); // media dropped
    expect(v.srcObject).toBeNull();
    expect(v.play).toHaveBeenCalledTimes(2); // no play() on a null stream

    c.updateSource('ghost', s3); // unknown peer → no-op
    expect(videos.length).toBe(1);

    // A participant that never had media keeps a dormant element: no bind/play.
    c.setSources([src('p', s3), src('q', null)]);
    expect(videos[1]!.play).not.toHaveBeenCalled();
    expect(videos[1]!.srcObject).toBeNull();
  });

  it('drops the hidden video element when a participant leaves', () => {
    const c = new CanvasCompositor();
    const q = src('q', streamOf(track('b')));
    c.setSources([src('p', streamOf(track('a'))), q]);
    expect(videos.length).toBe(2);
    c.setSources([q]); // p left
    expect(videos[0]!.pause).toHaveBeenCalled();
    expect(videos[0]!.srcObject).toBeNull();
    expect(videos[1]!.pause).not.toHaveBeenCalled(); // q untouched
  });

  it('swallows autoplay rejections from the hidden elements', async () => {
    playRejects = true;
    const c = new CanvasCompositor();
    c.setSources([src('p', streamOf(track('a')))]);
    expect(videos[0]!.play).toHaveBeenCalled();
    // The .catch(() => {}) guard must absorb the rejection (an unhandled
    // rejection would fail this run).
    await Promise.resolve();
    await Promise.resolve();
  });

  it('setVideoOff flips a tile to the placeholder and back; unknown peers no-op', () => {
    const c = new CanvasCompositor();
    c.start([src('p', streamOf(track('a')), false, 'Pat')]);
    videos[0]!.videoWidth = 640;
    videos[0]!.videoHeight = 480;
    tickFns[0]!();
    expect(ctx2d.drawImage).toHaveBeenCalledTimes(1);

    c.setVideoOff('p', true);
    tickFns[0]!();
    expect(ctx2d.drawImage).toHaveBeenCalledTimes(1); // no new video draw
    expect(ctx2d.arc).toHaveBeenCalledTimes(1); // placeholder disc instead

    c.setVideoOff('ghost', true); // unknown → no-op
    c.setVideoOff('p', false);
    tickFns[0]!();
    expect(ctx2d.drawImage).toHaveBeenCalledTimes(2);
    c.stop();
  });

  it('stop cancels the loop and releases every hidden video', () => {
    const c = new CanvasCompositor();
    c.start([src('p', streamOf(track('a')))]);
    const v = videos[0]!;
    c.stop();
    expect(cancelRaf).toHaveBeenCalled();
    expect(v.pause).toHaveBeenCalled();
    expect(v.srcObject).toBeNull();
  });
});
