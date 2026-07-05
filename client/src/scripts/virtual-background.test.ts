import { afterEach, describe, expect, it, vi } from 'vitest';

async function load() {
  vi.resetModules();
  return import('./virtual-background');
}

afterEach(() => {
  delete (globalThis as any).SelfieSegmentation;
  delete (globalThis as any).document;
  delete (globalThis as any).HTMLCanvasElement;
  delete (globalThis as any).MediaStream;
  delete (globalThis as any).requestAnimationFrame;
  delete (globalThis as any).cancelAnimationFrame;
  vi.useRealTimers();
  vi.restoreAllMocks();
});

/** Flush pending microtasks (the pump awaits the model between frames). */
const tick = () => new Promise<void>((resolve) => setImmediate(resolve));

/** A complete fake browser + MediaPipe environment for the canvas pipeline:
 *  recording 2D context, capturable canvas, hidden <video>, and a segmentation
 *  model whose onResults callback the test fires by hand. */
function installBrowserEnv(opts: { videoW?: number; videoH?: number } = {}) {
  const drawSnapshots: Array<{ gco: string; filter: string }> = [];
  const ctx: any = {
    globalCompositeOperation: 'source-over',
    filter: 'none',
    save: vi.fn(),
    restore: vi.fn(),
    clearRect: vi.fn(),
    drawImage: vi.fn(() => {
      drawSnapshots.push({ gco: ctx.globalCompositeOperation, filter: ctx.filter });
    }),
  };
  const outTrack = { kind: 'video', stop: vi.fn() };
  const output = { getVideoTracks: () => [outTrack], getTracks: () => [outTrack] };
  const canvas: any = {
    width: 0,
    height: 0,
    getContext: vi.fn(() => ctx),
    captureStream: vi.fn(() => output),
  };
  const video: any = {
    muted: false,
    playsInline: false,
    srcObject: null,
    videoWidth: opts.videoW ?? 320,
    videoHeight: opts.videoH ?? 240,
    readyState: 4,
    // Rejecting exercises the play().catch(() => {}) guard (autoplay block).
    play: vi.fn(() => Promise.reject(new Error('autoplay blocked'))),
  };
  (globalThis as any).document = {
    createElement: (tag: string) => (tag === 'canvas' ? canvas : video),
    hidden: false,
  };
  class FakeCanvasEl {}
  (FakeCanvasEl.prototype as any).captureStream = () => {};
  (globalThis as any).HTMLCanvasElement = FakeCanvasEl;
  (globalThis as any).MediaStream = class {
    tracks: unknown[];
    constructor(tracks: unknown[]) {
      this.tracks = tracks;
    }
  };
  const rafCbs: FrameRequestCallback[] = [];
  (globalThis as any).requestAnimationFrame = (fn: FrameRequestCallback) => {
    rafCbs.push(fn);
    return rafCbs.length;
  };
  const cancelRaf = vi.fn();
  (globalThis as any).cancelAnimationFrame = cancelRaf;
  const seg = {
    setOptions: vi.fn(),
    onResultsCb: null as ((r: { image: unknown; segmentationMask: unknown }) => void) | null,
    onResults(cb: (r: { image: unknown; segmentationMask: unknown }) => void) {
      this.onResultsCb = cb;
    },
    send: vi.fn(async () => {}),
    close: vi.fn(),
  };
  let locateFile: ((f: string) => string) | null = null;
  (globalThis as any).SelfieSegmentation = function (cfg: { locateFile: (f: string) => string }) {
    locateFile = cfg.locateFile;
    return seg;
  };
  return { ctx, drawSnapshots, canvas, video, seg, output, outTrack, rafCbs, cancelRaf, getLocate: () => locateFile };
}

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

  it('scheduleNext drives the pump via a timer when hidden, rAF when visible', async () => {
    const rafCalls: any[] = [];
    const timeoutDelays: number[] = [];
    (globalThis as any).requestAnimationFrame = (fn: any) => {
      rafCalls.push(fn);
      return 1;
    };
    const realSetTimeout = globalThis.setTimeout;
    (globalThis as any).setTimeout = ((fn: any, ms: number) => {
      timeoutDelays.push(ms);
      return 0 as any;
    }) as any;

    const { VirtualBackground } = await load();
    const vbg: any = new VirtualBackground();
    vbg.running = true;

    // Background tab: rAF is paused, so the composited track is driven by a timer
    // (this is what keeps the outgoing video from freezing with PiP closed).
    (globalThis as any).document = { hidden: true };
    vbg.scheduleNext();
    expect(timeoutDelays.length).toBe(1);
    expect(rafCalls.length).toBe(0);

    // Foreground tab: smooth rAF.
    (globalThis as any).document = { hidden: false };
    vbg.scheduleNext();
    expect(rafCalls.length).toBe(1);
    expect(timeoutDelays.length).toBe(1); // unchanged

    (globalThis as any).setTimeout = realSetTimeout;
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

  it('loadMediaPipe resolves false when the CDN script fails to load', async () => {
    let injected: any;
    (globalThis as any).document = {
      createElement: () => {
        injected = {};
        return injected;
      },
      head: { appendChild: () => {} },
    };
    const vb = await load();
    const p = vb.loadMediaPipe();
    injected.onerror();
    await expect(p).resolves.toBe(false);
  });

  it('loadMediaPipe resolves false when the script loads but exposes no constructor', async () => {
    let injected: any;
    (globalThis as any).document = {
      createElement: () => {
        injected = {};
        return injected;
      },
      head: { appendChild: () => {} },
    };
    const vb = await load();
    const p = vb.loadMediaPipe();
    injected.onload(); // loaded, but the UMD global never appeared
    await expect(p).resolves.toBe(false);
  });

  it('start() falls back to the raw track when canvas capture is unsupported', async () => {
    (globalThis as any).SelfieSegmentation = function () {
      return {};
    };
    (globalThis as any).document = { createElement: () => ({}) };
    // no HTMLCanvasElement.prototype.captureStream in this environment
    const { VirtualBackground } = await load();
    const vbg = new VirtualBackground();
    const track = { kind: 'video' } as any;
    const out = await vbg.start(track);
    expect(out).toBe(track);
    expect(vbg.active).toBe(false);
  });

  it('start() builds the canvas pipeline and composites frames (issue #6/#50)', async () => {
    const env = installBrowserEnv();
    const { VirtualBackground } = await load();
    const vbg = new VirtualBackground();
    const track = { kind: 'video', getSettings: () => ({ width: 640, height: 480 }) } as any;
    const out = await vbg.start(track);
    await tick();
    expect(out).toBe(env.outTrack); // the processed track, not the raw one
    expect(vbg.active).toBe(true);
    expect(vbg.source).toBe(track);
    // model wired up, assets located on the CDN
    expect(env.seg.setOptions).toHaveBeenCalledWith({ modelSelection: 1 });
    expect(env.getLocate()!('m.wasm')).toContain('@mediapipe/selfie_segmentation');
    // hidden <video> plays the raw track
    expect(env.video.muted).toBe(true);
    expect(env.video.playsInline).toBe(true);
    expect((env.video.srcObject as any).tracks).toEqual([track]);
    // canvas sized from the DECODED frame, not getSettings (#50)
    expect(env.canvas.width).toBe(320);
    expect(env.canvas.height).toBe(240);
    expect(env.canvas.captureStream).toHaveBeenCalledWith(24);
    // the pump sent the first frame and armed the next visible-tab rAF
    expect(env.seg.send).toHaveBeenCalledWith({ image: env.video });
    expect(env.rafCbs.length).toBe(1);

    // a segmentation result composites: mask → sharp subject → blurred backdrop
    env.seg.onResultsCb!({ image: {}, segmentationMask: {} });
    expect(env.drawSnapshots).toEqual([
      { gco: 'source-over', filter: 'none' }, // the mask
      { gco: 'source-in', filter: 'none' }, // sharp frame kept only on the subject
      { gco: 'destination-over', filter: 'blur(8px)' }, // blurred frame behind
    ]);

    // device rotation: the decoded size changes mid-call → the canvas follows (#50)
    env.video.videoWidth = 240;
    env.video.videoHeight = 320;
    env.seg.onResultsCb!({ image: {}, segmentationMask: {} });
    expect(env.canvas.width).toBe(240);
    expect(env.canvas.height).toBe(320);

    // stop() tears down: model closed, output tracks stopped, video released
    vbg.stop();
    expect(vbg.active).toBe(false);
    expect(env.cancelRaf).toHaveBeenCalled();
    expect(env.seg.close).toHaveBeenCalled();
    expect(env.outTrack.stop).toHaveBeenCalled();
    expect(env.video.srcObject).toBeNull();
    expect(vbg.source).toBeNull();
    // late results after stop are ignored (ctx is gone)
    expect(() => env.seg.onResultsCb!({ image: {}, segmentationMask: {} })).not.toThrow();
  });

  it('the pump survives model errors and skips frames until the video is ready', async () => {
    const env = installBrowserEnv();
    const { VirtualBackground } = await load();
    const vbg = new VirtualBackground();
    await vbg.start({ kind: 'video', getSettings: () => ({}) } as any);
    await tick();
    expect(env.seg.send).toHaveBeenCalledTimes(1);
    // a failing send drops the frame but keeps the loop alive
    env.seg.send.mockRejectedValueOnce(new Error('model hiccup'));
    env.rafCbs.pop()!(0);
    await tick();
    expect(env.seg.send).toHaveBeenCalledTimes(2);
    expect(env.rafCbs.length).toBe(1); // rescheduled
    // an unready video (readyState < 2) skips the send but keeps pumping
    env.video.readyState = 0;
    env.rafCbs.pop()!(0);
    await tick();
    expect(env.seg.send).toHaveBeenCalledTimes(2); // unchanged
    expect(env.rafCbs.length).toBe(1);
    // once stopped, a stray scheduled pump is a no-op
    vbg.stop();
    env.rafCbs.pop()!(0);
    await tick();
    expect(env.seg.send).toHaveBeenCalledTimes(2);
  });

  it('keeps pumping via a timer while the tab is hidden, and stop() clears it', async () => {
    const env = installBrowserEnv();
    const { VirtualBackground } = await load();
    const vbg = new VirtualBackground();
    await vbg.start({ kind: 'video', getSettings: () => ({}) } as any);
    await tick();
    expect(env.rafCbs.length).toBe(1);
    vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] });
    (globalThis as any).document.hidden = true;
    env.rafCbs.pop()!(0); // pump → hidden → arms the ~42ms fallback timer
    await tick();
    expect(env.seg.send).toHaveBeenCalledTimes(2);
    await vi.advanceTimersByTimeAsync(50); // the background timer fires a frame
    await tick();
    expect(env.seg.send).toHaveBeenCalledTimes(3);
    expect(env.rafCbs.length).toBe(0); // rAF never used while hidden
    vbg.stop(); // clears the armed timer
    await vi.advanceTimersByTimeAsync(500);
    expect(env.seg.send).toHaveBeenCalledTimes(3);
  });

  it('falls back to getSettings, then 640×480, when the decoded size is unknown', async () => {
    const env = installBrowserEnv({ videoW: 0, videoH: 0 });
    const { VirtualBackground } = await load();
    const a = new VirtualBackground();
    await a.start({ kind: 'video', getSettings: () => ({ width: 800, height: 600 }) } as any);
    expect(env.canvas.width).toBe(800);
    expect(env.canvas.height).toBe(600);
    a.stop();
    const b = new VirtualBackground();
    await b.start({ kind: 'video', getSettings: () => ({}) } as any);
    expect(env.canvas.width).toBe(640);
    expect(env.canvas.height).toBe(480);
    b.stop();
  });

  it('stop() swallows model teardown errors and is idempotent', async () => {
    const env = installBrowserEnv();
    const { VirtualBackground } = await load();
    const vbg = new VirtualBackground();
    await vbg.start({ kind: 'video', getSettings: () => ({}) } as any);
    await tick();
    env.seg.close.mockImplementation(() => {
      throw new Error('already closed');
    });
    expect(() => vbg.stop()).not.toThrow();
    expect(() => vbg.stop()).not.toThrow(); // second call: everything already null
    expect(env.outTrack.stop).toHaveBeenCalledTimes(1);
  });
});
