import { describe, it, expect, vi, afterEach } from 'vitest';
import { fitCanvas, pipRect, coverCrop, ScreenSharePip } from './screenshare-pip';

// Pure geometry helpers for the screen-share camera PiP compositor (spec 0053),
// plus the ScreenSharePip draw loop itself, driven through fake canvas/video/rAF
// stand-ins (same recipe as whiteboard.test.ts / virtual-background.test.ts).

describe('fitCanvas', () => {
  it('passes a 1080p screen through untouched (never upscales)', () => {
    expect(fitCanvas(1920, 1080)).toEqual({ w: 1920, h: 1080 });
  });

  it('caps a 4K screen to 1920×1080 keeping the 16:9 aspect', () => {
    expect(fitCanvas(3840, 2160)).toEqual({ w: 1920, h: 1080 });
  });

  it('caps a tall/portrait screen by height, preserving aspect', () => {
    // 1440×2560 → scale = min(1, 1920/1440, 1080/2560) = 1080/2560
    const out = fitCanvas(1440, 2560);
    expect(out.h).toBe(1080);
    expect(out.w).toBe(Math.round(1440 * (1080 / 2560)));
    expect(out.w / out.h).toBeCloseTo(1440 / 2560, 2);
  });

  it('returns a sane default while the source size is unknown', () => {
    expect(fitCanvas(0, 0)).toEqual({ w: 1280, h: 720 });
  });
});

describe('pipRect', () => {
  it('sits in the bottom-right corner inset by the margin', () => {
    const r = pipRect(1920, 1080, 1280, 720);
    expect(r.w).toBe(Math.round(1920 * 0.24));
    const margin = Math.round(1920 * 0.025);
    expect(r.x + r.w + margin).toBe(1920);
    expect(r.y + r.h + margin).toBe(1080);
  });

  it('preserves the camera aspect ratio in the overlay box', () => {
    const r = pipRect(1920, 1080, 1280, 720);
    expect(r.h / r.w).toBeCloseTo(720 / 1280, 2);
  });

  it('falls back to 16:9 when the camera size is unknown', () => {
    const r = pipRect(1920, 1080, 0, 0);
    expect(r.h / r.w).toBeCloseTo(9 / 16, 2);
  });
});

describe('coverCrop', () => {
  it('crops the sides when the source is wider than the box', () => {
    // 1920×1080 source into a 1:1 box → crop to a centred 1080×1080 square.
    const c = coverCrop(1920, 1080, 100, 100);
    expect(c).toEqual({ sx: Math.round((1920 - 1080) / 2), sy: 0, sw: 1080, sh: 1080 });
  });

  it('crops top/bottom when the source is taller than the box', () => {
    // 720×1280 source into a 16:9 box → crop height to 720/(16/9)=405.
    const c = coverCrop(720, 1280, 160, 90);
    expect(c.sw).toBe(720);
    expect(c.sh).toBe(Math.round(720 / (160 / 90)));
    expect(c.sy).toBe(Math.round((1280 - c.sh) / 2));
  });

  it('returns the full source for a matching aspect box', () => {
    const c = coverCrop(1280, 720, 320, 180);
    expect(c).toEqual({ sx: 0, sy: 0, sw: 1280, sh: 720 });
  });

  it('is a no-op for degenerate sizes', () => {
    expect(coverCrop(0, 0, 100, 100)).toEqual({ sx: 0, sy: 0, sw: 0, sh: 0 });
  });
});

// ---- ScreenSharePip compositor -------------------------------------------------

interface FakeVideo {
  muted: boolean;
  playsInline: boolean;
  srcObject: unknown;
  videoWidth: number;
  videoHeight: number;
  play: ReturnType<typeof vi.fn>;
  pause: ReturnType<typeof vi.fn>;
}

/** A recording 2D context; `roundRect` is optional to exercise the square-corner
 *  fallback path. */
function makeCtx(withRoundRect: boolean): any {
  const ctx: Record<string, unknown> = {
    fillStyle: '',
    strokeStyle: '',
    lineWidth: 0,
    shadowColor: '',
    shadowBlur: 0,
    shadowOffsetY: 0,
    fillRect: vi.fn(),
    drawImage: vi.fn(),
    save: vi.fn(),
    restore: vi.fn(),
    beginPath: vi.fn(),
    rect: vi.fn(),
    fill: vi.fn(),
    clip: vi.fn(),
    stroke: vi.fn(),
  };
  if (withRoundRect) ctx.roundRect = vi.fn();
  return ctx;
}

function installDom(opts: { roundRect?: boolean; noCtx?: boolean } = {}) {
  const ctx = makeCtx(opts.roundRect ?? true);
  const outTrack = { stop: vi.fn() };
  const output = { getTracks: () => [outTrack] };
  const canvas: any = {
    width: 0,
    height: 0,
    getContext: vi.fn(() => (opts.noCtx ? null : ctx)),
    captureStream: vi.fn(() => output),
  };
  const videos: FakeVideo[] = [];
  (globalThis as any).document = {
    createElement: (tag: string) => {
      if (tag === 'canvas') return canvas;
      const v: FakeVideo = {
        muted: false,
        playsInline: false,
        srcObject: null,
        videoWidth: 0,
        videoHeight: 0,
        // Rejecting exercises the play().catch(() => {}) guards.
        play: vi.fn(() => Promise.reject(new Error('no autoplay'))),
        pause: vi.fn(),
      };
      videos.push(v);
      return v;
    },
  };
  const rafCbs: FrameRequestCallback[] = [];
  (globalThis as any).requestAnimationFrame = (fn: FrameRequestCallback) => {
    rafCbs.push(fn);
    return rafCbs.length;
  };
  const canceled: number[] = [];
  (globalThis as any).cancelAnimationFrame = (id: number) => canceled.push(id);
  const intervals: Array<{ fn: () => void; ms: number }> = [];
  (globalThis as any).window = {
    setInterval: (fn: () => void, ms: number) => {
      intervals.push({ fn, ms });
      return 77;
    },
  };
  (globalThis as any).MediaStream = class {
    tracks: unknown[];
    constructor(tracks: unknown[]) {
      this.tracks = tracks;
    }
  };
  return { ctx, canvas, videos, output, outTrack, rafCbs, canceled, intervals };
}

/** A fake getDisplayMedia stream whose single video track reports `settings`. */
function screenStream(settings?: { width: number; height: number }): MediaStream {
  return {
    getVideoTracks: () => (settings ? [{ getSettings: () => settings }] : []),
  } as unknown as MediaStream;
}

afterEach(() => {
  delete (globalThis as any).document;
  delete (globalThis as any).window;
  delete (globalThis as any).requestAnimationFrame;
  delete (globalThis as any).cancelAnimationFrame;
  delete (globalThis as any).MediaStream;
  vi.restoreAllMocks();
});

describe('ScreenSharePip compositor (spec 0053)', () => {
  it('seeds the canvas from the screen track settings and wires the hidden <video>', () => {
    const env = installDom();
    const stream = screenStream({ width: 3840, height: 2160 });
    const pip = new ScreenSharePip(stream);
    expect(env.canvas.width).toBe(1920); // 4K capped, aspect kept
    expect(env.canvas.height).toBe(1080);
    const sv = env.videos[0];
    expect(sv.muted).toBe(true);
    expect(sv.playsInline).toBe(true);
    expect(sv.srcObject).toBe(stream);
    expect(sv.play).toHaveBeenCalled();
    expect(pip.stream).toBeNull(); // not started yet
  });

  it('defaults to 1280×720 while the source size is unknown', () => {
    const env = installDom();
    new ScreenSharePip(screenStream()); // no video track yet
    expect(env.canvas.width).toBe(1280);
    expect(env.canvas.height).toBe(720);
  });

  it('throws when a 2d context is unavailable', () => {
    installDom({ noCtx: true });
    expect(() => new ScreenSharePip(screenStream({ width: 100, height: 100 }))).toThrow(
      '2d canvas unavailable',
    );
  });

  it('start() paints, captures at 30fps and throttles the rAF loop to ~33ms', () => {
    const env = installDom();
    const pip = new ScreenSharePip(screenStream({ width: 1920, height: 1080 }));
    const out = pip.start();
    expect(out).toBe(env.output);
    expect(pip.stream).toBe(env.output);
    expect(env.canvas.captureStream).toHaveBeenCalledWith(30);
    expect(env.ctx.fillRect).toHaveBeenCalledTimes(1); // the pre-capture frame
    expect(env.rafCbs.length).toBe(1);
    env.rafCbs[0](10); // 10ms in: too soon → skipped, but the loop re-arms
    expect(env.ctx.fillRect).toHaveBeenCalledTimes(1);
    expect(env.rafCbs.length).toBe(2);
    env.rafCbs[1](40); // past FRAME_MS → a new frame
    expect(env.ctx.fillRect).toHaveBeenCalledTimes(2);
    expect(env.rafCbs.length).toBe(3);
    pip.stop();
    env.rafCbs[2](80); // the loop halts once stopped
    expect(env.ctx.fillRect).toHaveBeenCalledTimes(2);
    expect(env.rafCbs.length).toBe(3);
  });

  it('keeps frames flowing from a 1s tick in background tabs, tracking resolution', () => {
    const env = installDom();
    const pip = new ScreenSharePip(screenStream({ width: 1280, height: 720 }));
    pip.start();
    expect(env.intervals.length).toBe(1);
    expect(env.intervals[0].ms).toBe(1000);
    const sv = env.videos[0];
    sv.videoWidth = 4000; // the live share resolution becomes known / changes
    sv.videoHeight = 2000;
    env.intervals[0].fn(); // background tick draws without rAF
    expect(env.canvas.width).toBe(1920); // capped, aspect preserved
    expect(env.canvas.height).toBe(960);
    expect(env.ctx.drawImage).toHaveBeenCalledWith(sv, 0, 0, 1920, 960);
  });

  it('setCamera wires, reuses and releases the PiP <video>', () => {
    const env = installDom();
    const pip = new ScreenSharePip(screenStream({ width: 1920, height: 1080 }));
    const trackA = { id: 'a' } as unknown as MediaStreamTrack;
    const trackB = { id: 'b' } as unknown as MediaStreamTrack;
    pip.setCamera(trackA);
    expect(env.videos.length).toBe(2);
    const cam = env.videos[1];
    expect(cam.muted).toBe(true);
    expect(cam.playsInline).toBe(true);
    expect((cam.srcObject as { tracks: unknown[] }).tracks).toEqual([trackA]);
    expect(cam.play).toHaveBeenCalledTimes(1);
    pip.setCamera(trackA); // same track → no-op
    expect(cam.play).toHaveBeenCalledTimes(1);
    pip.setCamera(trackB); // swapped mid-share (e.g. blur toggled) → same element
    expect(env.videos.length).toBe(2);
    expect((cam.srcObject as { tracks: unknown[] }).tracks).toEqual([trackB]);
    pip.setCamera(null); // camera off → released
    expect(cam.pause).toHaveBeenCalled();
    expect(cam.srcObject).toBeNull();
    pip.setCamera(null); // already off → no-op
    expect(env.videos.length).toBe(2);
  });

  it('composites the camera as a rounded, centre-cropped card (roundRect path)', () => {
    const env = installDom({ roundRect: true });
    const pip = new ScreenSharePip(screenStream({ width: 1920, height: 1080 }));
    pip.setCamera({} as unknown as MediaStreamTrack);
    const cam = env.videos[1];
    cam.videoWidth = 1280;
    cam.videoHeight = 720;
    pip.start(); // the initial frame already includes the PiP
    expect(env.ctx.roundRect).toHaveBeenCalledTimes(3); // card fill, clip, border
    expect(env.ctx.clip).toHaveBeenCalledTimes(1);
    expect(env.ctx.stroke).toHaveBeenCalledTimes(1);
    const r = pipRect(1920, 1080, 1280, 720);
    const crop = coverCrop(1280, 720, r.w, r.h);
    expect(env.ctx.drawImage).toHaveBeenCalledWith(
      cam,
      crop.sx,
      crop.sy,
      crop.sw,
      crop.sh,
      r.x,
      r.y,
      r.w,
      r.h,
    );
  });

  it('falls back to square corners when ctx.roundRect is missing', () => {
    const env = installDom({ roundRect: false });
    const pip = new ScreenSharePip(screenStream({ width: 1920, height: 1080 }));
    pip.setCamera({} as unknown as MediaStreamTrack);
    const cam = env.videos[1];
    cam.videoWidth = 640;
    cam.videoHeight = 480;
    pip.start();
    expect(env.ctx.rect).toHaveBeenCalledTimes(3);
  });

  it('skips the PiP while the camera has no decoded frames yet', () => {
    const env = installDom();
    const pip = new ScreenSharePip(screenStream({ width: 1920, height: 1080 }));
    pip.setCamera({} as unknown as MediaStreamTrack); // videoWidth stays 0
    pip.start();
    expect(env.ctx.clip).not.toHaveBeenCalled();
  });

  it('stop() halts the loops, stops the output and releases both videos', () => {
    const env = installDom();
    const pip = new ScreenSharePip(screenStream({ width: 1920, height: 1080 }));
    pip.setCamera({} as unknown as MediaStreamTrack);
    pip.start();
    pip.stop();
    expect(env.canceled.length).toBe(1);
    expect(env.outTrack.stop).toHaveBeenCalledTimes(1);
    expect(pip.stream).toBeNull();
    const [sv, cam] = env.videos;
    expect(sv.pause).toHaveBeenCalled();
    expect(sv.srcObject).toBeNull();
    expect(cam.pause).toHaveBeenCalled();
    expect(cam.srcObject).toBeNull();
    // idempotent: a second stop has nothing left to release
    expect(() => pip.stop()).not.toThrow();
    expect(env.outTrack.stop).toHaveBeenCalledTimes(1);
  });
});
