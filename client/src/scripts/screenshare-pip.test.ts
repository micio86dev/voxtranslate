import { describe, it, expect } from 'vitest';
import { fitCanvas, pipRect, coverCrop } from './screenshare-pip';

// Pure geometry helpers for the screen-share camera PiP compositor (spec 0053).
// The canvas/RAF parts of ScreenSharePip need real browser APIs and are covered by
// the screenshare e2e + manual testing, mirroring recording/canvas-compositor.ts.

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
