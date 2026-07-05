// Tests for the Premium translated-audio playback graph (spec 0093). A fake
// AudioContext/AudioWorkletNode pair (node env, following cartesia.test.ts) covers the
// lazy graph build, per-speaker sequence gating, and the unlock/reset/stop lifecycle
// without producing real audio.

import { beforeEach, describe, expect, it, vi } from 'vitest';
import { PcmPlayback, pcmPlayback } from './pcm-playback';

class FakeWorkletNode {
  name: string;
  ctorOpts: unknown;
  posted: unknown[] = [];
  port = {
    postMessage: (msg: unknown, _transfer?: unknown): void => {
      this.posted.push(msg);
    },
  };
  connect = vi.fn((n: unknown) => n);
  disconnect = vi.fn();
  constructor(_ctx: unknown, name: string, opts?: unknown) {
    this.name = name;
    this.ctorOpts = opts;
    nodes.push(this);
  }
}

class FakeCtx {
  opts: unknown;
  state = 'running';
  destination = {};
  // Rejections must be swallowed by the module's `.catch(() => {})` guards.
  resume = vi.fn((): Promise<void> => Promise.reject(new Error('resume failed')));
  close = vi.fn((): Promise<void> => Promise.reject(new Error('close failed')));
  audioWorklet = {
    addModule: (url: string): Promise<void> => {
      addModuleUrls.push(url);
      return addModuleImpl();
    },
  };
  constructor(opts?: unknown) {
    this.opts = opts;
    ctxs.push(this);
  }
}

let nodes: FakeWorkletNode[] = [];
let ctxs: FakeCtx[] = [];
let addModuleUrls: string[] = [];
let addModuleImpl: () => Promise<void> = () => Promise.resolve();

const tick = (): Promise<void> => new Promise((r) => setTimeout(r, 0));

// One PCM16 sample, little-endian: 0x4000 = 16384 → 0.5 after normalization.
const HALF = btoa(String.fromCharCode(0x00, 0x40));

/** Payloads posted to the worklet, minus the 'flush' control messages. */
function postedSamples(node: FakeWorkletNode): unknown[] {
  return node.posted.filter((m) => m !== 'flush');
}

beforeEach(() => {
  nodes = [];
  ctxs = [];
  addModuleUrls = [];
  addModuleImpl = () => Promise.resolve();
  const g = globalThis as unknown as Record<string, unknown>;
  g.window = { AudioContext: FakeCtx };
  g.AudioWorkletNode = FakeWorkletNode;
});

describe('PcmPlayback', () => {
  it('lazily builds the 24 kHz graph once and posts decoded samples', async () => {
    const p = new PcmPlayback();
    p.enqueue('alice', 0, HALF);
    await tick();

    expect(ctxs.length).toBe(1);
    expect(ctxs[0].opts).toEqual({ sampleRate: 24000 });
    expect(addModuleUrls).toEqual(['/pcm-playback-worklet.js']); // same-origin, CSP-safe
    expect(nodes[0].name).toBe('pcm-playback-processor');
    expect(nodes[0].ctorOpts).toEqual({
      numberOfInputs: 0,
      numberOfOutputs: 1,
      outputChannelCount: [1],
    });
    expect(nodes[0].connect).toHaveBeenCalledWith(ctxs[0].destination);
    expect(nodes[0].posted.length).toBe(1);
    expect(Array.from(nodes[0].posted[0] as Float32Array)).toEqual([0.5]);
    expect(ctxs[0].resume).not.toHaveBeenCalled(); // running → no resume needed

    p.enqueue('alice', 1, HALF);
    await tick();
    expect(ctxs.length).toBe(1); // the graph is shared, not rebuilt
    expect(nodes[0].posted.length).toBe(2);
  });

  it('drops stale/duplicate frames per speaker but accepts a seq-0 restart', async () => {
    const p = new PcmPlayback();
    p.enqueue('a', 1, HALF);
    p.enqueue('a', 1, HALF); // duplicate → dropped
    p.enqueue('b', 1, HALF); // independent speaker → plays
    p.enqueue('a', 0, HALF); // an upstream reconnect restarts at 0 → plays
    p.enqueue('a', 1, HALF); // the new stream climbs again → plays
    p.enqueue('a', 1, HALF); // duplicate again → dropped
    await tick();
    expect(postedSamples(nodes[0]).length).toBe(4);
  });

  it('ignores an empty chunk without posting to the worklet', async () => {
    const p = new PcmPlayback();
    p.enqueue('a', 0, '');
    await tick();
    expect(ctxs.length).toBe(1); // the graph was still (lazily) built
    expect(nodes[0].posted.length).toBe(0);
  });

  it('resumes a suspended context on enqueue and unlock, swallowing failures', async () => {
    const p = new PcmPlayback();
    p.enqueue('a', 0, HALF);
    await tick();

    ctxs[0].state = 'suspended';
    p.enqueue('a', 1, HALF); // suspended → resume attempted (and its rejection swallowed)
    await tick();
    expect(ctxs[0].resume).toHaveBeenCalledTimes(1);

    p.unlock(); // user-gesture unlock path
    await tick();
    expect(ctxs[0].resume).toHaveBeenCalledTimes(2);

    ctxs[0].state = 'running';
    p.unlock(); // already running → nothing to resume
    await tick();
    expect(ctxs[0].resume).toHaveBeenCalledTimes(2);
  });

  it('unlock builds the graph on first use', async () => {
    const p = new PcmPlayback();
    p.unlock();
    await tick();
    expect(ctxs.length).toBe(1);
    expect(nodes[0].posted.length).toBe(0);
  });

  it('swallows a worklet load failure on both enqueue and unlock', async () => {
    addModuleImpl = () => Promise.reject(new Error('csp blocked'));
    const p = new PcmPlayback();
    p.enqueue('a', 0, HALF); // must not throw
    await tick();
    p.unlock(); // the cached rejected `ready` is swallowed here too
    await tick();
    expect(nodes.length).toBe(0);
  });

  it('reset flushes the ring buffer and restarts sequence tracking', async () => {
    const p = new PcmPlayback();
    p.enqueue('a', 5, HALF);
    await tick();
    expect(postedSamples(nodes[0]).length).toBe(1);

    p.reset();
    expect(nodes[0].posted).toContain('flush');

    p.enqueue('a', 3, HALF); // stale before the reset → accepted now
    await tick();
    expect(postedSamples(nodes[0]).length).toBe(2);

    new PcmPlayback().reset(); // reset before any graph exists is a no-op
  });

  it('stop tears everything down and a later enqueue rebuilds the graph', async () => {
    const p = new PcmPlayback();
    p.enqueue('a', 0, HALF);
    await tick();

    p.stop();
    expect(nodes[0].posted).toContain('flush'); // stop() flushes queued audio first
    expect(nodes[0].disconnect).toHaveBeenCalled();
    expect(ctxs[0].close).toHaveBeenCalled();

    p.enqueue('a', 0, HALF);
    await tick();
    expect(ctxs.length).toBe(2); // a fresh graph after teardown
    expect(postedSamples(nodes[1]).length).toBe(1);
  });

  it('discards a chunk that was in flight when stop() landed', async () => {
    const p = new PcmPlayback();
    p.enqueue('a', 0, HALF);
    await tick();
    p.enqueue('a', 1, HALF); // decode scheduled…
    p.stop(); // …but the graph is torn down before the microtask runs
    await tick();
    expect(postedSamples(nodes[0]).length).toBe(1); // only the first chunk played
    expect(ctxs.length).toBe(1); // and nothing rebuilt the graph behind stop()
  });

  it('falls back to webkitAudioContext when AudioContext is missing', async () => {
    (globalThis as unknown as Record<string, unknown>).window = { webkitAudioContext: FakeCtx };
    const p = new PcmPlayback();
    p.enqueue('a', 0, HALF);
    await tick();
    expect(ctxs.length).toBe(1);
    expect(nodes[0].posted.length).toBe(1);
  });

  it('exports one shared playback instance for the whole call', () => {
    expect(pcmPlayback).toBeInstanceOf(PcmPlayback);
  });
});
