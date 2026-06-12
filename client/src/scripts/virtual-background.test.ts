import { afterEach, describe, expect, it, vi } from 'vitest';

async function load() {
  vi.resetModules();
  return import('./virtual-background');
}

afterEach(() => {
  delete (globalThis as any).SelfieSegmentation;
  delete (globalThis as any).document;
  vi.restoreAllMocks();
});

describe('virtual-background', () => {
  it('builds CDN asset URLs under the MediaPipe base', async () => {
    const vb = await load();
    expect(vb.mediaPipeAsset('selfie_segmentation.js')).toBe(
      `${vb.MEDIAPIPE_BASE}/selfie_segmentation.js`,
    );
    expect(vb.mediaPipeAsset('x.wasm')).toContain('@mediapipe/selfie_segmentation');
  });

  it('loadMediaPipe resolves true immediately when the global already exists', async () => {
    (globalThis as any).SelfieSegmentation = function () {};
    const vb = await load();
    await expect(vb.loadMediaPipe()).resolves.toBe(true);
  });

  it('loadMediaPipe resolves false in a non-browser env (no document)', async () => {
    // node test env: no global document, no SelfieSegmentation.
    const vb = await load();
    await expect(vb.loadMediaPipe()).resolves.toBe(false);
  });

  it('loadMediaPipe injects a script and caches the promise', async () => {
    const appended: any[] = [];
    let injected: any;
    (globalThis as any).document = {
      createElement: () => {
        injected = { set onload(fn: any) { this._l = fn; }, get onload() { return this._l; } };
        return injected;
      },
      head: { appendChild: (el: any) => appended.push(el) },
    };
    const vb = await load();
    const p = vb.loadMediaPipe();
    expect(appended.length).toBe(1);
    expect(injected.src).toContain('selfie_segmentation.js');
    // Same promise returned on a second call (no second injection).
    expect(vb.loadMediaPipe()).toBe(p);
    expect(appended.length).toBe(1);
    // Simulate the script loading and exposing the global.
    (globalThis as any).SelfieSegmentation = function () {};
    injected.onload();
    await expect(p).resolves.toBe(true);
  });

  it('VirtualBackground.start returns the raw track and stays inactive without a model', async () => {
    const { VirtualBackground } = await load();
    const vbg = new VirtualBackground();
    const track = { kind: 'video', getSettings: () => ({}) } as any;
    const out = await vbg.start(track);
    expect(out).toBe(track); // graceful fallback
    expect(vbg.active).toBe(false);
    expect(vbg.source).toBe(track);
    vbg.stop();
    expect(vbg.source).toBeNull();
  });
});
