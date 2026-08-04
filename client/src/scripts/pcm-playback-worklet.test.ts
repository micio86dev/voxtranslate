// Tests for the translated-audio playback processor (`public/pcm-playback-worklet.js`).
//
// The processor is plain JS served as a static asset, so it is loaded as source and
// evaluated against the globals AudioWorkletGlobalScope provides (`AudioWorkletProcessor`,
// `registerProcessor`, `sampleRate`). The harness then drives `process()` one render
// quantum at a time against a scripted frame-arrival schedule — a deterministic stand-in
// for a jittery network, which is what the underrun bug needs to reproduce.
//
// Every test sample is `LEVEL`, so the output is self-describing: a 0 between the first
// and last audible sample is silence the processor injected, and the count of samples
// still exactly `LEVEL` is the audio it played back untouched.

import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { describe, expect, it } from 'vitest';

// Resolved from the Vitest root rather than `import.meta.url`: this file is mirrored
// into the extension repo, whose suite runs under happy-dom where `import.meta.url` is
// an http: URL that `readFileSync` refuses. Both repos keep the worklet in `public/`.
const WORKLET = resolve(process.cwd(), 'public/pcm-playback-worklet.js');

const RATE = 24_000;
const QUANTUM = 128; // what a real AudioWorklet renders per `process()` call
const MS_PER_QUANTUM = (QUANTUM / RATE) * 1000;
const LEVEL = 0.5;

interface Processor {
  port: { onmessage: ((e: { data: unknown }) => void) | null };
  process(inputs: unknown[], outputs: Float32Array[][]): boolean;
}

/** One frame handed to the processor: `samples` of test tone landing at `atMs`. */
interface Arrival {
  atMs: number;
  samples: number;
}

function loadProcessor(): Processor {
  const src = readFileSync(WORKLET, 'utf8');
  class FakeAudioWorkletProcessor {
    port = { onmessage: null as ((e: { data: unknown }) => void) | null, postMessage: () => {} };
  }
  const registered: (new () => Processor)[] = [];
  const evaluate = new Function('AudioWorkletProcessor', 'registerProcessor', 'sampleRate', src);
  evaluate(
    FakeAudioWorkletProcessor,
    (_name: string, cls: new () => Processor) => {
      registered.push(cls);
    },
    RATE,
  );
  const Ctor = registered[0];
  if (!Ctor) throw new Error('the worklet registered no processor');
  return new Ctor();
}

/** Render `durationMs` of output, delivering each arrival at its scheduled time. */
function render(proc: Processor, arrivals: Arrival[], durationMs: number): Float32Array {
  const out = new Float32Array(Math.ceil(durationMs / MS_PER_QUANTUM) * QUANTUM);
  const buf = new Float32Array(QUANTUM);
  let next = 0;
  for (let offset = 0; offset < out.length; offset += QUANTUM) {
    const nowMs = (offset / RATE) * 1000;
    for (let due = arrivals[next]; due && due.atMs <= nowMs; due = arrivals[next]) {
      proc.port.onmessage?.({ data: new Float32Array(due.samples).fill(LEVEL) });
      next++;
    }
    buf.fill(0);
    proc.process([], [[buf]]);
    out.set(buf, offset);
  }
  return out;
}

/** Samples of silence between the first and last audible sample — holes in the speech. */
function holes(out: Float32Array): number {
  const first = out.findIndex((v) => v !== 0);
  if (first === -1) return 0;
  let last = out.length - 1;
  while (out[last] === 0) last--;
  let count = 0;
  for (let i = first; i <= last; i++) if (out[i] === 0) count++;
  return count;
}

/** Stretches of silence between the first and last audible sample. */
function gapCount(out: Float32Array): number {
  const first = out.findIndex((v) => v !== 0);
  if (first === -1) return 0;
  let last = out.length - 1;
  while (out[last] === 0) last--;
  let gaps = 0;
  for (let i = first; i <= last; i++) {
    if (out[i] === 0 && out[i - 1] !== 0) gaps++;
  }
  return gaps;
}

/** Samples played back at full level — audio that survived the trip unaltered. */
function played(out: Float32Array): number {
  let count = 0;
  for (const v of out) if (v === LEVEL) count++;
  return count;
}

describe('pcm-playback-worklet', () => {
  it('plays a jittery stream without punching holes into the speech', () => {
    // 40 ms frames on a 40 ms cadence, smeared by up to 55 ms. A WebSocket delivers in
    // order, so a late frame holds back the ones behind it — hence the `max` with the
    // previous arrival rather than independent per-frame jitter.
    const JITTER_MS = [0, 12, 55, 4, 30, 0, 47, 18];
    const FRAME_MS = 40;
    const FRAME_SAMPLES = (FRAME_MS / 1000) * RATE;
    const FRAMES = 50; // 2 s of translated speech

    const arrivals: Arrival[] = [];
    let prev = 0;
    for (let i = 0; i < FRAMES; i++) {
      const atMs = Math.max(prev, i * FRAME_MS + (JITTER_MS[i % JITTER_MS.length] ?? 0));
      arrivals.push({ atMs, samples: FRAME_SAMPLES });
      prev = atMs;
    }

    const out = render(loadProcessor(), arrivals, 3200);

    expect(holes(out)).toBe(0);
    expect(played(out)).toBe(FRAMES * FRAME_SAMPLES);
  });

  it('survives a burst late enough to drain the whole cushion', () => {
    // One frame arrives 240 ms behind the cadence — just inside the cushion. The
    // speech either side of it must still join up.
    const FRAME_SAMPLES = 960;
    const arrivals: Arrival[] = [];
    for (let i = 0; i < 30; i++) {
      arrivals.push({ atMs: i * 40 + (i === 15 ? 240 : 0), samples: FRAME_SAMPLES });
    }

    const out = render(loadProcessor(), arrivals, 3000);

    expect(holes(out)).toBe(0);
    expect(played(out)).toBe(30 * FRAME_SAMPLES);
  });

  it('keeps each utterance whole across the silence between them', () => {
    // The real shape of a call: a burst of translated speech, a pause while nobody
    // talks, then another burst. The pause must not cost either utterance a sample.
    const FRAME_SAMPLES = 960;
    const arrivals: Arrival[] = [];
    for (let i = 0; i < 20; i++) arrivals.push({ atMs: i * 40, samples: FRAME_SAMPLES });
    for (let i = 0; i < 20; i++) arrivals.push({ atMs: 2000 + i * 40, samples: FRAME_SAMPLES });

    const out = render(loadProcessor(), arrivals, 4000);

    expect(played(out)).toBe(40 * FRAME_SAMPLES);
    // Two utterances → exactly one stretch of silence between them, not one per hiccup.
    expect(gapCount(out)).toBe(1);
  });

  it('plays out an utterance shorter than the cushion instead of holding it', () => {
    const SAMPLES = 2400; // 100 ms — less than the prebuffer, and nothing follows it
    const out = render(loadProcessor(), [{ atMs: 0, samples: SAMPLES }], 1000);

    expect(played(out)).toBe(SAMPLES);
    expect(holes(out)).toBe(0);
  });

  it('emits silence until audio arrives', () => {
    const out = render(loadProcessor(), [], 200);
    expect(out.some((v) => v !== 0)).toBe(false);
  });

  it('drops queued audio on flush', () => {
    const proc = loadProcessor();
    proc.port.onmessage?.({ data: new Float32Array(2400).fill(LEVEL) });
    proc.port.onmessage?.({ data: 'flush' });
    const out = render(proc, [], 1000);
    expect(out.some((v) => v !== 0)).toBe(false);
  });
});
