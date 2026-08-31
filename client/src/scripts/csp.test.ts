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

  it('scopes jsdelivr to the MediaPipe package, not the whole CDN', () => {
    // jsDelivr serves EVERY npm and GitHub package. Allow-listing the bare host
    // in script-src therefore means "any script on npm may run in this origin" —
    // which hands back most of what the CSP is for, since an injection can just
    // point at a package of its choosing. The origin holds the session JWT in
    // localStorage, so that is the whole prize.
    //
    // CSP host-sources match a path by prefix, so scoping costs nothing: the one
    // thing we actually load still loads.
    const MEDIAPIPE = 'https://cdn.jsdelivr.net/npm/@mediapipe/selfie_segmentation';
    for (const name of ['script-src', 'connect-src']) {
      const d = directive(name);
      expect(d).toContain(MEDIAPIPE);
      expect(
        d.split(/\s+/).includes('https://cdn.jsdelivr.net'),
        `${name} allows the whole of jsdelivr — scope it to ${MEDIAPIPE}`,
      ).toBe(false);
    }
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
    // Great-Firewall detection (restricted-net.ts) fetches these two hosts to probe
    // reachability. They MUST be in connect-src or the browser blocks the probe →
    // indistinguishable from a network failure → every user flagged restricted
    // (Enhanced hidden + forced relay). Keep in sync with RESTRICTED_PROBE_URLS.
    expect(directive('connect-src')).toContain('https://www.google.com');
    expect(directive('connect-src')).toContain('https://api.cartesia.ai');
    // MediaRecorder / recording playback uses blob: object URLs for media.
    expect(directive('media-src')).toContain('blob:');
    // Chat voice messages are served as signed Supabase storage URLs, so the
    // <audio> element loads media cross-origin from supabase.co (not blob:).
    expect(directive('media-src')).toContain('https://*.supabase.co');
    // Google Identity Services (sign-in) loads its script + frame.
    expect(directive('script-src')).toContain('https://accounts.google.com');
    expect(directive('frame-src')).toContain('https://accounts.google.com');
  });

  it('allows Vox Voices to fetch its manifest + pack from R2 storage', () => {
    // Vox Voices (on-device Kokoro TTS) fetches PUBLIC_VOX_MANIFEST_URL and then
    // downloads the pack files during install via fetch() — both cross-origin to the
    // R2 bucket, served over the branded custom domain voices.voxtranslate.app (NOT
    // the rate-limited *.r2.dev dev URL). Without it in connect-src the browser blocks
    // the manifest fetch → install button stays disabled. (Runtime model loading is
    // same-origin via the SW at /vox-models/, covered by 'self'.)
    expect(directive('connect-src')).toContain('https://voices.voxtranslate.app');
  });

  it('allows the webinar media path (WHIP ingest + HLS playback)', () => {
    // The presenter POSTs the WHIP offer + hls.js fetches HLS segments (connect-src);
    // the participant <video> plays the HLS stream cross-origin (media-src). Without
    // these the browser blocks going live / watching a webinar (F1-4/F1-5).
    for (const host of ['https://ingest.voxtranslate.app', 'https://hls.voxtranslate.app']) {
      expect(directive('connect-src')).toContain(host);
      expect(directive('media-src')).toContain(host);
    }
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

// ---- Meta pixel origins ------------------------------------------------------
//
// Symptom this pins against: the pixel shipped, installed its stub, queued
// ["init", <id>] and ["track","PageView"] — and sent nothing. `fbevents.js` was
// never fetched because `script-src` did not list connect.facebook.net, so the
// browser blocked it and the queue was never drained. Everything looked correct
// from the code's side, which is exactly why it needs a test.
describe('CSP allows the Meta pixel it ships', () => {
  const pixelShipped = readFileSync(
    new URL('./analytics.ts', import.meta.url),
    'utf8',
  ).includes('connect.facebook.net');

  it('ships a pixel loader at all (guards the assumption below)', () => {
    expect(pixelShipped).toBe(true);
  });

  it('allows the pixel script origin', () => {
    expect(directive('script-src')).toContain('https://connect.facebook.net');
  });

  it('allows the beacon origin the pixel posts events to', () => {
    // fbevents sends via fetch/sendBeacon (connect-src) and falls back to an
    // image beacon (img-src) — both to www.facebook.com.
    expect(directive('connect-src')).toContain('https://www.facebook.com');
    expect(directive('img-src')).toContain('https://www.facebook.com');
  });
});
