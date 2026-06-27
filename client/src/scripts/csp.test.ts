// CSP regression guard (issue #237).
//
// #237 was a `blob:` AudioWorklet script blocked by the call CSP. Spec 0094 fixed
// it by serving the worklets as STATIC same-origin files (client/public/*.js),
// covered by `worker-src 'self'`. This test pins the Content-Security-Policy
// invariants that keep the call (worklets, WebRTC, MediaPipe blur, recording,
// Google auth, Supabase) working WITHOUT re-opening the blob hole — so a future
// edit that re-introduces a blob worklet, drops `wasm-unsafe-eval`, or weakens a
// directive fails here instead of silently breaking a call in production.

import { existsSync, readFileSync } from 'node:fs';
import { describe, expect, it } from 'vitest';

const vercel = JSON.parse(
  readFileSync(new URL('../../vercel.json', import.meta.url), 'utf8'),
) as { headers: Array<{ headers: Array<{ key: string; value: string }> }> };

/** The single CSP string shipped to the browser via vercel.json headers. */
function csp(): string {
  for (const entry of vercel.headers) {
    const h = entry.headers.find((x) => x.key === 'Content-Security-Policy');
    if (h) return h.value;
  }
  throw new Error('no Content-Security-Policy header in vercel.json');
}

/** Extract a single directive's source list (e.g. "script-src 'self' ...") . */
function directive(name: string): string {
  const policy = csp();
  const part = policy.split(';').map((s) => s.trim()).find((s) => s === name || s.startsWith(`${name} `));
  if (!part) throw new Error(`CSP directive ${name} missing`);
  return part;
}

describe('call CSP (issue #237 regression guard)', () => {
  it('serves AudioWorklets as static same-origin files, not blob: scripts', () => {
    // The blob: hole is closed iff the worklets exist as static assets AND the
    // code points at the static path (not a createObjectURL blob).
    expect(existsSync(new URL('../../public/pcm-capture-worklet.js', import.meta.url))).toBe(true);
    expect(existsSync(new URL('../../public/pcm-playback-worklet.js', import.meta.url))).toBe(true);
    expect(readFileSync(new URL('./pcm-capture.ts', import.meta.url), 'utf8')).toContain(
      "'/pcm-capture-worklet.js'",
    );
    expect(readFileSync(new URL('./pcm-playback.ts', import.meta.url), 'utf8')).toContain(
      "'/pcm-playback-worklet.js'",
    );
  });

  it('allows same-origin workers/worklets without re-opening blob:', () => {
    expect(directive('worker-src')).toContain("'self'");
    // Regression: a blob: worklet is exactly what #237 was. script-src must NOT
    // permit blob: (worklets are static; we never want a blob script again).
    expect(directive('script-src')).not.toContain('blob:');
    expect(directive('worker-src')).not.toContain('blob:');
  });

  it('keeps the directives the call features depend on', () => {
    // MediaPipe Selfie Segmentation (background blur): UMD from jsdelivr +
    // WebAssembly compilation + asset fetches.
    expect(directive('script-src')).toContain('https://cdn.jsdelivr.net');
    expect(directive('script-src')).toContain("'wasm-unsafe-eval'");
    expect(directive('connect-src')).toContain('https://cdn.jsdelivr.net');
    // WebRTC signalling + STT/translation stream to our API over WS.
    expect(directive('connect-src')).toContain('wss://api.voxtranslate.app');
    expect(directive('connect-src')).toContain('https://api.voxtranslate.app');
    // Enhanced tier (spec 0108): the browser connects DIRECTLY to Cartesia STT/TTS over WS.
    expect(directive('connect-src')).toContain('wss://api.cartesia.ai');
    // MediaRecorder / recording playback uses blob: object URLs for media.
    expect(directive('media-src')).toContain('blob:');
    // Google Identity Services (sign-in) loads its script + frame.
    expect(directive('script-src')).toContain('https://accounts.google.com');
    expect(directive('frame-src')).toContain('https://accounts.google.com');
  });

  it('allows GA4 (gtag.js) to load and send hits', () => {
    // gtag.js itself is fetched from googletagmanager.com.
    expect(directive('script-src')).toContain('https://www.googletagmanager.com');
    // GA4 sends collect beacons (sendBeacon/fetch) to the analytics endpoints,
    // including regional collectors (region1.google-analytics.com, *.analytics.google.com).
    expect(directive('connect-src')).toContain('https://www.googletagmanager.com');
    expect(directive('connect-src')).toContain('https://www.google-analytics.com');
    expect(directive('connect-src')).toContain('https://*.google-analytics.com');
    expect(directive('connect-src')).toContain('https://*.analytics.google.com');
    // Legacy image-pixel beacon fallback.
    expect(directive('img-src')).toContain('https://www.google-analytics.com');
  });
});
