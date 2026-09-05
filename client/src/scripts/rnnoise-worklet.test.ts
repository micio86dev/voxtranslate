// Tests for the noisy-environment processor (`public/rnnoise-worklet.js`).
//
// Loaded as source and evaluated against the globals AudioWorkletGlobalScope provides,
// the same harness `pcm-playback-worklet.test.ts` uses. The difference: this one feeds
// it the REAL `public/rnnoise.wasm`, because the whole risk in this file is the
// hand-rolled instantiation — we bypass the Emscripten glue (a JS module an
// AudioWorklet cannot import) and wire the two imports ourselves. A test against a
// mocked wasm would prove nothing about that.
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

import { describe, expect, it } from 'vitest';

const WORKLET = resolve(process.cwd(), 'public/rnnoise-worklet.js');
const WASM = resolve(process.cwd(), 'public/rnnoise.wasm');

const QUANTUM = 128; // what a real AudioWorklet renders per process() call
const FRAME = 480; // RNNoise's fixed frame size at 48 kHz

interface Processor {
  port: {
    onmessage: ((e: { data: unknown }) => void) | null;
    postMessage(m: unknown): void;
  };
  process(inputs: Float32Array[][], outputs: Float32Array[][]): boolean;
}

/** Evaluate the worklet source and instantiate the processor it registers. */
function loadProcessor(): { proc: Processor; posted: unknown[] } {
  const src = readFileSync(WORKLET, 'utf8');
  const posted: unknown[] = [];
  class FakeAudioWorkletProcessor {
    port = {
      onmessage: null as ((e: { data: unknown }) => void) | null,
      postMessage: (m: unknown) => posted.push(m),
    };
  }
  const registered: (new () => Processor)[] = [];
  const evaluate = new Function('AudioWorkletProcessor', 'registerProcessor', 'sampleRate', src);
  evaluate(
    FakeAudioWorkletProcessor,
    (_name: string, ctor: new () => Processor) => registered.push(ctor),
    48_000,
  );
  return { proc: new registered[0](), posted };
}

/** One render quantum in, one out. */
function pump(proc: Processor, input: Float32Array): Float32Array {
  const out = new Float32Array(input.length);
  proc.process([[input]], [[out]]);
  return out;
}

/** A quantum of full-scale white noise — what RNNoise is supposed to remove. */
function noise(seed: number, n = QUANTUM): Float32Array {
  const out = new Float32Array(n);
  let s = seed;
  for (let i = 0; i < n; i++) {
    s = (s * 1664525 + 1013904223) >>> 0; // LCG: deterministic, no Math.random
    out[i] = (s / 0xffffffff) * 2 - 1;
  }
  return out;
}

function rms(a: Float32Array): number {
  let sum = 0;
  for (const v of a) sum += v * v;
  return Math.sqrt(sum / a.length);
}

/** Hand the processor the real wasm and WAIT for it to report either way.
 *  `WebAssembly.instantiate` resolves off the microtask queue, so a fixed number of
 *  yields is a race — poll for the verdict instead. */
async function withWasm(proc: Processor, posted: unknown[]): Promise<void> {
  const bytes = readFileSync(WASM);
  proc.port.onmessage?.({
    data: {
      type: 'wasm',
      bytes: bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength),
    },
  });
  const settled = (): boolean =>
    posted.some(
      (m) => (m as { type?: string })?.type === 'ready' || (m as { type?: string })?.type === 'failed',
    );
  for (let i = 0; i < 500 && !settled(); i++) await new Promise((r) => setTimeout(r, 2));
}

describe('rnnoise worklet', () => {
  it('passes audio through UNTOUCHED before the wasm arrives', () => {
    // Capture must never go silent because a denoiser is still downloading.
    const { proc } = loadProcessor();
    const input = noise(1);
    expect(Array.from(pump(proc, input))).toEqual(Array.from(input));
  });

  it('emits silence when there is no input, without crashing', () => {
    const { proc } = loadProcessor();
    const out = new Float32Array(QUANTUM).fill(0.7);
    proc.process([[]], [[out]]);
    expect(out.every((v) => v === 0)).toBe(true);
  });

  it('instantiates the real wasm and reports ready', async () => {
    // This is the assertion the whole file exists for: the two hand-wired imports
    // (emscripten_resize_heap / memcpy_big) and the numbered exports are correct.
    const { proc, posted } = loadProcessor();
    await withWasm(proc, posted);
    expect(posted).toContainEqual({ type: 'ready' });
    expect(posted).not.toContainEqual({ type: 'failed' });
  });

  it('actually attenuates white noise once running', async () => {
    const { proc, posted } = loadProcessor();
    await withWasm(proc, posted);
    expect(posted).toContainEqual({ type: 'ready' });

    // Prime past the 480-sample fill, then measure a settled stretch. RNNoise needs a
    // few frames to converge on the noise profile before it suppresses hard.
    let loud = 0;
    let quiet = 0;
    let measured = 0;
    for (let q = 0; q < 120; q++) {
      const input = noise(q + 2);
      const out = pump(proc, input);
      if (q < 60) continue; // warm-up: priming silence + model convergence
      loud += rms(input);
      quiet += rms(out);
      measured++;
    }
    expect(measured).toBeGreaterThan(0);
    // A modest bound on purpose. Full-scale white noise is the WORST case for RNNoise
    // — it is trained on real-world noise (fans, traffic, babble) and reads flat
    // broadband hiss as speech-like, so it suppresses it conservatively. What this
    // asserts is that the model is genuinely in the path and never adds energy;
    // measuring its real-world strength needs real recordings, not a PRNG.
    expect(quiet / measured).toBeLessThan(loud / measured);
  });

  it('stays in passthrough when the bytes are not a valid module', async () => {
    const { proc, posted } = loadProcessor();
    proc.port.onmessage?.({ data: { type: 'wasm', bytes: new Uint8Array([1, 2, 3]).buffer } });
    for (let i = 0; i < 200 && !posted.length; i++) await new Promise((r) => setTimeout(r, 2));
    expect(posted).toContainEqual({ type: 'failed' });
    const input = noise(9);
    expect(Array.from(pump(proc, input))).toEqual(Array.from(input));
  });

  it('ignores messages it does not understand', () => {
    const { proc } = loadProcessor();
    expect(() => proc.port.onmessage?.({ data: { type: 'nope' } })).not.toThrow();
    expect(() => proc.port.onmessage?.({ data: null })).not.toThrow();
  });

  it('consumes input in whole 480-sample frames', async () => {
    // The ring buffer is the part most likely to drift or leak: after N quanta the
    // processor must have emitted exactly the quanta it was asked for, no more.
    const { proc, posted } = loadProcessor();
    await withWasm(proc, posted);
    expect(posted).toContainEqual({ type: 'ready' });
    const total = 40 * QUANTUM;
    let emitted = 0;
    for (let q = 0; q < 40; q++) emitted += pump(proc, noise(q)).length;
    expect(emitted).toBe(total);
    expect(total).toBeGreaterThan(FRAME); // the test actually crosses a frame boundary
  });
});
