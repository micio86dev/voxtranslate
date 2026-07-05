// Tests for the Premium PCM mic capture (spec 0093). The AudioWorklet graph is faked
// (node env, following cartesia.test.ts) so the start/stop/mute/restart lifecycle and
// the WS framing are covered without real audio APIs.

import { beforeEach, describe, expect, it, vi } from 'vitest';
import { PcmCapture } from './pcm-capture';

class FakeWorkletNode {
  ctx: unknown;
  name: string;
  port: { onmessage: ((e: { data: ArrayBuffer }) => void) | null } = { onmessage: null };
  connect = vi.fn((n: unknown) => n);
  disconnect = vi.fn();
  constructor(ctx: unknown, name: string) {
    this.ctx = ctx;
    this.name = name;
    nodes.push(this);
  }
}

class FakeSrc {
  connect = vi.fn((n: unknown) => n);
  disconnect = vi.fn();
}

class FakeGain {
  gain = { value: 1 };
  connect = vi.fn((n: unknown) => n);
}

class FakeCtx {
  opts: unknown;
  sources: Array<{ tracks: unknown[] }> = [];
  gains: FakeGain[] = [];
  destination = {};
  audioWorklet = {
    addModule: (url: string): Promise<void> => {
      addModuleUrls.push(url);
      return addModuleImpl();
    },
  };
  // Rejects so the fire-and-forget `.catch(() => {})` teardown guard is exercised.
  close = vi.fn((): Promise<void> => Promise.reject(new Error('close failed')));
  createMediaStreamSource = vi.fn((s: { tracks: unknown[] }) => {
    this.sources.push(s);
    const src = new FakeSrc();
    srcs.push(src);
    return src;
  });
  createGain = vi.fn(() => {
    const g = new FakeGain();
    this.gains.push(g);
    return g;
  });
  constructor(opts?: unknown) {
    this.opts = opts;
    ctxs.push(this);
  }
}

class FakeMediaStream {
  tracks: unknown[];
  constructor(tracks: unknown[] = []) {
    this.tracks = tracks;
  }
  getAudioTracks(): unknown[] {
    return this.tracks;
  }
}

let nodes: FakeWorkletNode[] = [];
let srcs: FakeSrc[] = [];
let ctxs: FakeCtx[] = [];
let addModuleUrls: string[] = [];
let addModuleImpl: () => Promise<void> = () => Promise.resolve();

/** Flush the fire-and-forget async start (`_start` awaits the worklet module). */
const tick = (): Promise<void> => new Promise((r) => setTimeout(r, 0));

interface WsFake {
  readyState: number;
  sent: unknown[];
  send(d: unknown): void;
}

function fakeWs(open = true): WebSocket & WsFake {
  const ws: WsFake = {
    readyState: open ? 1 : 0,
    sent: [],
    send(d: unknown) {
      this.sent.push(d);
    },
  };
  return ws as unknown as WebSocket & WsFake;
}

function fakeStream(tracks: unknown[] = [{ kind: 'audio' }]): MediaStream & { tracks: unknown[] } {
  return new FakeMediaStream(tracks) as unknown as MediaStream & { tracks: unknown[] };
}

const START = JSON.stringify({ type: 'start' });
const STOP = JSON.stringify({ type: 'stop' });

beforeEach(() => {
  nodes = [];
  srcs = [];
  ctxs = [];
  addModuleUrls = [];
  addModuleImpl = () => Promise.resolve();
  const g = globalThis as unknown as Record<string, unknown>;
  g.window = { AudioContext: FakeCtx };
  g.WebSocket = { OPEN: 1 };
  g.AudioWorkletNode = FakeWorkletNode;
  g.MediaStream = FakeMediaStream;
});

describe('PcmCapture', () => {
  it('wires a 24 kHz worklet graph, sends the start control, and streams chunks', async () => {
    const ws = fakeWs();
    const cap = new PcmCapture(fakeStream(), ws);
    cap.start();
    cap.start(); // second call while starting → ignored
    await tick();

    expect(ctxs.length).toBe(1);
    expect(ctxs[0].opts).toEqual({ sampleRate: 24000 });
    expect(addModuleUrls).toEqual(['/pcm-capture-worklet.js']); // same-origin, CSP-safe
    expect(nodes[0].name).toBe('pcm-capture-processor');
    expect(ctxs[0].gains[0].gain.value).toBe(0); // routed to a muted sink
    expect(ws.sent).toEqual([START]); // control frame before audio flows

    const buf = new ArrayBuffer(8);
    nodes[0].port.onmessage?.({ data: buf });
    expect(ws.sent).toContain(buf);

    ws.readyState = 0; // socket gone → chunks are dropped
    nodes[0].port.onmessage?.({ data: new ArrayBuffer(2) });
    expect(ws.sent.length).toBe(2);

    ws.readyState = 1;
    cap.start(); // already active → no-op
    await tick();
    expect(ctxs.length).toBe(1);
  });

  it('does nothing without an audio track', async () => {
    const ws = fakeWs();
    const cap = new PcmCapture(fakeStream([]), ws);
    cap.start();
    await tick();
    expect(ctxs.length).toBe(0);
    expect(ws.sent.length).toBe(0);
  });

  it('aborts cleanly when the track disappears while the worklet loads', async () => {
    let release!: () => void;
    addModuleImpl = () =>
      new Promise<void>((r) => {
        release = r;
      });
    const ws = fakeWs();
    const stream = fakeStream();
    const cap = new PcmCapture(stream, ws);
    cap.start();
    stream.tracks = []; // the device vanished mid-start
    release();
    await tick();
    expect(nodes.length).toBe(0);
    expect(ctxs[0].close).toHaveBeenCalled(); // stop() tore the context down
    expect(ws.sent.length).toBe(0); // never became active → no control frames

    // …and capture can start again once a track is back.
    stream.tracks = [{ kind: 'audio' }];
    addModuleImpl = () => Promise.resolve();
    cap.start();
    await tick();
    expect(nodes.length).toBe(1);
    expect(ws.sent).toEqual([START]);
  });

  it('swallows a worklet load failure (e.g. CSP-blocked module)', async () => {
    addModuleImpl = () => Promise.reject(new Error('csp blocked'));
    const ws = fakeWs();
    const cap = new PcmCapture(fakeStream(), ws);
    cap.start();
    await tick();
    expect(nodes.length).toBe(0);
    expect(ctxs[0].close).toHaveBeenCalled();
    expect(ws.sent.length).toBe(0);
  });

  it('stop tears the graph down and sends the stop control exactly once', async () => {
    const ws = fakeWs();
    const cap = new PcmCapture(fakeStream(), ws);
    cap.start();
    await tick();

    cap.stop();
    expect(nodes[0].port.onmessage).toBeNull();
    expect(nodes[0].disconnect).toHaveBeenCalled();
    expect(srcs[0].disconnect).toHaveBeenCalled();
    expect(ctxs[0].close).toHaveBeenCalled();
    expect(ws.sent).toEqual([START, STOP]);

    cap.stop(); // idempotent → no second stop frame
    expect(ws.sent).toEqual([START, STOP]);
  });

  it('restart() puts stop before start on the wire and rebuilds the graph', async () => {
    const ws = fakeWs();
    const cap = new PcmCapture(fakeStream(), ws);
    cap.start();
    await tick();
    cap.restart();
    await tick();
    expect(ws.sent).toEqual([START, STOP, START]);
    expect(ctxs.length).toBe(2); // a fresh AudioContext per capture run
  });

  it('setMuted toggles capture', async () => {
    const ws = fakeWs();
    const cap = new PcmCapture(fakeStream(), ws);
    cap.setMuted(false); // unmute = start
    await tick();
    expect(ws.sent).toEqual([START]);
    cap.setMuted(true); // mute = stop
    expect(ws.sent).toEqual([START, STOP]);
    expect(ctxs.length).toBe(1);
  });

  it('setStream repoints capture at the new device (single re-wrapped track)', async () => {
    const ws = fakeWs();
    const cap = new PcmCapture(fakeStream(), ws);
    cap.start();
    await tick();
    cap.stop();

    const track2 = { kind: 'audio', id: 't2' };
    cap.setStream(fakeStream([track2, { kind: 'audio', id: 't3' }]));
    cap.start();
    await tick();
    // Only the FIRST audio track is wrapped into a fresh single-track stream.
    expect(ctxs[1].sources[0].tracks).toEqual([track2]);
  });

  it('skips control frames when the socket is not open', async () => {
    const ws = fakeWs(false);
    const cap = new PcmCapture(fakeStream(), ws);
    cap.start();
    await tick();
    expect(ctxs.length).toBe(1); // the graph still runs; only the frames are gated
    expect(ws.sent.length).toBe(0);
    cap.stop();
    expect(ws.sent.length).toBe(0);
  });

  it('falls back to webkitAudioContext when AudioContext is missing', async () => {
    (globalThis as unknown as Record<string, unknown>).window = { webkitAudioContext: FakeCtx };
    const ws = fakeWs();
    const cap = new PcmCapture(fakeStream(), ws);
    cap.start();
    await tick();
    expect(ctxs.length).toBe(1);
    expect(ws.sent).toEqual([START]);
  });
});
