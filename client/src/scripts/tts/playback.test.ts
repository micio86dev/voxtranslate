// @vitest-environment jsdom
// One-shot AudioBufferSourceNode playback, driven via a fake AudioContext (jsdom has
// no Web Audio). The "finished" contract — resolve exactly on onended — is what the
// manager's serial queue depends on.
import { beforeEach, describe, expect, it } from 'vitest';

import { PlaybackService } from './playback';

class FakeBuffer {
  data: Float32Array;
  constructor(
    public channels: number,
    public length: number,
    public sampleRate: number,
  ) {
    this.data = new Float32Array(length);
  }
  getChannelData(_c: number): Float32Array {
    return this.data;
  }
}

class FakeSource {
  buffer: FakeBuffer | null = null;
  onended: (() => void) | null = null;
  connectedTo: unknown = null;
  startCalls = 0;
  stopCalls = 0;
  startError: unknown = null;
  stopError: unknown = null;
  connect(dest: unknown): void {
    this.connectedTo = dest;
  }
  start(): void {
    this.startCalls++;
    if (this.startError) throw this.startError;
  }
  stop(): void {
    this.stopCalls++;
    if (this.stopError) throw this.stopError;
  }
}

class FakeAudioContext {
  static instances: FakeAudioContext[] = [];
  state = 'running';
  destination = { node: 'dest' };
  resumeCalls = 0;
  resumeImpl: () => Promise<void> = () => Promise.resolve();
  buffers: FakeBuffer[] = [];
  sources: FakeSource[] = [];
  constructor() {
    FakeAudioContext.instances.push(this);
  }
  resume(): Promise<void> {
    this.resumeCalls++;
    return this.resumeImpl();
  }
  createBuffer(channels: number, length: number, sampleRate: number): FakeBuffer {
    const b = new FakeBuffer(channels, length, sampleRate);
    this.buffers.push(b);
    return b;
  }
  createBufferSource(): FakeSource {
    const s = new FakeSource();
    this.sources.push(s);
    return s;
  }
}

type W = { AudioContext?: unknown; webkitAudioContext?: unknown };
const win = window as unknown as W;

describe('PlaybackService', () => {
  beforeEach(() => {
    FakeAudioContext.instances = [];
    win.AudioContext = FakeAudioContext;
    delete win.webkitAudioContext;
  });

  it('plays a 24 kHz mono buffer and resolves exactly on onended', async () => {
    const svc = new PlaybackService();
    const samples = new Float32Array([0.1, -0.2, 0.3]);
    const done = svc.play(samples);

    const ctx = FakeAudioContext.instances[0];
    const src = ctx.sources[0];
    expect(ctx.buffers[0].channels).toBe(1);
    expect(ctx.buffers[0].sampleRate).toBe(24000);
    expect(ctx.buffers[0].length).toBe(3);
    expect(Array.from(ctx.buffers[0].data)).toEqual([...samples].map((v) => Math.fround(v)));
    expect(src.buffer).toBe(ctx.buffers[0]);
    expect(src.connectedTo).toBe(ctx.destination);
    expect(src.startCalls).toBe(1);

    let resolved = false;
    void done.then(() => (resolved = true));
    await Promise.resolve();
    expect(resolved).toBe(false); // strictly waits for onended
    src.onended?.();
    await done;
  });

  it('reuses one AudioContext across plays', async () => {
    const svc = new PlaybackService();
    const p1 = svc.play(new Float32Array(1));
    FakeAudioContext.instances[0].sources[0].onended?.();
    await p1;
    const p2 = svc.play(new Float32Array(1));
    FakeAudioContext.instances[0].sources[1].onended?.();
    await p2;
    expect(FakeAudioContext.instances).toHaveLength(1);
  });

  it('resumes a suspended context before playing (autoplay unlock), swallowing failures', async () => {
    const svc = new PlaybackService();
    svc.unlock(); // creates the ctx
    const ctx = FakeAudioContext.instances[0];
    expect(ctx.resumeCalls).toBe(0); // running → no resume needed

    ctx.state = 'suspended';
    ctx.resumeImpl = () => Promise.reject(new Error('blocked'));
    svc.unlock();
    expect(ctx.resumeCalls).toBe(1);

    const p = svc.play(new Float32Array(1));
    expect(ctx.resumeCalls).toBe(2); // play also resumes
    ctx.sources[0].onended?.();
    await p; // the rejected resume() never surfaced
  });

  it('unlock is a silent no-op without Web Audio; play rejects instead', async () => {
    delete win.AudioContext;
    const svc = new PlaybackService();
    expect(() => svc.unlock()).not.toThrow();
    await expect(svc.play(new Float32Array(1))).rejects.toBeInstanceOf(Error);
  });

  it('falls back to webkitAudioContext (Safari)', async () => {
    delete win.AudioContext;
    win.webkitAudioContext = FakeAudioContext;
    const svc = new PlaybackService();
    const p = svc.play(new Float32Array(2));
    FakeAudioContext.instances[0].sources[0].onended?.();
    await p;
  });

  it('rejects with the original Error when start() throws one', async () => {
    const svc = new PlaybackService();
    const boom = new Error('device gone');
    const p = svc.play(new Float32Array(1));
    // First play establishes the ctx; make the NEXT source's start() throw.
    const ctx = FakeAudioContext.instances[0];
    ctx.sources[0].onended?.();
    await p;

    // Poison the next created source.
    const orig = ctx.createBufferSource.bind(ctx);
    ctx.createBufferSource = () => {
      const s = orig();
      s.startError = boom;
      return s;
    };
    await expect(svc.play(new Float32Array(1))).rejects.toBe(boom);
  });

  it('wraps a non-Error start() throw in Error("playback failed")', async () => {
    const svc = new PlaybackService();
    const ctx = new FakeAudioContext();
    // Seed the private ctx by unlocking first (instances[0] is the service's ctx).
    svc.unlock();
    const serviceCtx = FakeAudioContext.instances.find((c) => c !== ctx) ?? ctx;
    serviceCtx.createBufferSource = () => {
      const s = new FakeSource();
      s.startError = 'string failure';
      serviceCtx.sources.push(s);
      return s;
    };
    await expect(svc.play(new Float32Array(1))).rejects.toThrow('playback failed');
  });

  it('stop() halts the current utterance, detaches onended, and is idempotent', () => {
    const svc = new PlaybackService();
    void svc.play(new Float32Array(1));
    const src = FakeAudioContext.instances[0].sources[0];
    expect(src.onended).toBeTypeOf('function');

    svc.stop();
    expect(src.onended).toBeNull(); // resolve suppressed — the line was cancelled
    expect(src.stopCalls).toBe(1);

    svc.stop(); // no current → no-op
    expect(src.stopCalls).toBe(1);
  });

  it('stop() swallows an already-stopped source error', () => {
    const svc = new PlaybackService();
    void svc.play(new Float32Array(1));
    const src = FakeAudioContext.instances[0].sources[0];
    src.stopError = new Error('InvalidStateError');
    expect(() => svc.stop()).not.toThrow();
  });

  it("a stale utterance's onended does not clear the newer current one", async () => {
    const svc = new PlaybackService();
    const p1 = svc.play(new Float32Array(1));
    const ctx = FakeAudioContext.instances[0];
    const first = ctx.sources[0];
    void svc.play(new Float32Array(1)); // current is now the second source
    const second = ctx.sources[1];

    first.onended?.(); // stale end — must not null out `current`
    await p1;
    svc.stop(); // must stop the SECOND source
    expect(second.stopCalls).toBe(1);
    expect(first.stopCalls).toBe(0);
  });
});
