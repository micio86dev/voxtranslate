import { describe, it, expect, vi, beforeEach } from 'vitest';
import { AudioMixer } from './audio-mixer';

// Fake Web Audio graph — records connect/disconnect wiring per node so the
// unity-gain source → gain → destination topology is asserted for real.
class FakeNode {
  mediaStream: unknown = null;
  connect = vi.fn();
  disconnect = vi.fn();
}
class FakeGainNode extends FakeNode {
  gain = { value: 0 };
}

let ctxs: FakeAudioContext[] = [];

class FakeAudioContext {
  dest = { stream: { id: 'mixed' } };
  sources: FakeNode[] = [];
  gains: FakeGainNode[] = [];
  closeRejects = false;
  closed = false;
  constructor() {
    ctxs.push(this);
  }
  createMediaStreamDestination() {
    return this.dest;
  }
  createMediaStreamSource(stream: unknown) {
    const n = new FakeNode();
    n.mediaStream = stream;
    this.sources.push(n);
    return n;
  }
  createGain() {
    const g = new FakeGainNode();
    this.gains.push(g);
    return g;
  }
  close() {
    this.closed = true;
    return this.closeRejects ? Promise.reject(new Error('already closed')) : Promise.resolve();
  }
}
(globalThis as any).AudioContext = FakeAudioContext;

const stream = (nAudio = 1) =>
  ({ getAudioTracks: () => Array.from({ length: nAudio }, (_, i) => ({ id: `t${i}` })) }) as any;

describe('AudioMixer', () => {
  beforeEach(() => {
    ctxs = [];
  });

  it('exposes the MediaStreamDestination stream for the recorder', () => {
    const m = new AudioMixer();
    expect(ctxs.length).toBe(1);
    expect(m.stream).toBe(ctxs[0]!.dest.stream);
  });

  it('wires each participant source → unity gain → destination', () => {
    const m = new AudioMixer();
    const ctx = ctxs[0]!;
    const s = stream();
    m.add('p1', s);
    expect(ctx.sources.length).toBe(1);
    expect(ctx.sources[0]!.mediaStream).toBe(s);
    expect(ctx.gains[0]!.gain.value).toBe(1); // unity gain — original voices, not the TTS mix
    expect(ctx.sources[0]!.connect).toHaveBeenCalledWith(ctx.gains[0]);
    expect(ctx.gains[0]!.connect).toHaveBeenCalledWith(ctx.dest);
  });

  it('skips null streams and audio-less streams, unwiring any previous entry', () => {
    const m = new AudioMixer();
    const ctx = ctxs[0]!;
    m.add('p1', stream());
    m.add('p1', null); // media lost → old node unwired, nothing new added
    expect(ctx.sources[0]!.disconnect).toHaveBeenCalledTimes(1);
    expect(ctx.gains[0]!.disconnect).toHaveBeenCalledTimes(1);
    expect(ctx.sources.length).toBe(1);
    m.add('p2', stream(0)); // video-only participant → skipped
    expect(ctx.sources.length).toBe(1);
  });

  it('re-adding a peer replaces its node instead of stacking a duplicate', () => {
    const m = new AudioMixer();
    const ctx = ctxs[0]!;
    m.add('p1', stream());
    m.add('p1', stream());
    expect(ctx.sources[0]!.disconnect).toHaveBeenCalledTimes(1); // old graph torn down
    expect(ctx.sources.length).toBe(2); // fresh source wired in
    expect(ctx.sources[1]!.connect).toHaveBeenCalledWith(ctx.gains[1]);
  });

  it('remove unwires the peer; unknown/repeat removes are no-ops', () => {
    const m = new AudioMixer();
    const ctx = ctxs[0]!;
    m.add('p1', stream());
    m.remove('nobody'); // never added → no-op
    m.remove('p1');
    expect(ctx.sources[0]!.disconnect).toHaveBeenCalledTimes(1);
    m.remove('p1'); // already gone → no-op, no double disconnect
    expect(ctx.sources[0]!.disconnect).toHaveBeenCalledTimes(1);
  });

  it('close unwires everyone and closes the context, swallowing close() rejections', async () => {
    const m = new AudioMixer();
    const ctx = ctxs[0]!;
    m.add('p1', stream());
    m.add('p2', stream());
    ctx.closeRejects = true; // e.g. context already closed by the browser
    m.close();
    expect(ctx.sources.every((s) => s.disconnect.mock.calls.length === 1)).toBe(true);
    expect(ctx.gains.every((g) => g.disconnect.mock.calls.length === 1)).toBe(true);
    expect(ctx.closed).toBe(true);
    // Let the rejected close() settle — the .catch(() => {}) guard must absorb
    // it (an unhandled rejection would fail this test run).
    await Promise.resolve();
    await Promise.resolve();
  });
});
