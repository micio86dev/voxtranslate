import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { BENCH_LINES, benchmarkVerdict, benchmarkVerdictKey, runBenchmark } from './benchmark';
import { TTS_CONFIG } from './config';
import type { LoadedEngine } from './kokoro-engine';
import type { KokoroProvider } from './providers/kokoro';

describe('benchmarkVerdict', () => {
  const th = { firstAudioMs: 1200, avgSynthMs: 800 };

  it('passes when both first-audio and average synthesis are under threshold', () => {
    expect(benchmarkVerdict({ firstAudioMs: 400, avgSynthMs: 300 }, th)).toBe(true);
    expect(benchmarkVerdict({ firstAudioMs: 1200, avgSynthMs: 800 }, th)).toBe(true); // at limit
  });

  it('fails when the first line is too slow', () => {
    expect(benchmarkVerdict({ firstAudioMs: 1500, avgSynthMs: 300 }, th)).toBe(false);
  });

  it('fails when the average is too slow (would lag live speech)', () => {
    expect(benchmarkVerdict({ firstAudioMs: 400, avgSynthMs: 1200 }, th)).toBe(false);
  });
});

describe('benchmarkVerdictKey', () => {
  it('maps to friendly, number-free i18n keys', () => {
    expect(benchmarkVerdictKey(true)).toBe('voxBenchPass');
    expect(benchmarkVerdictKey(false)).toBe('voxBenchUseBrowser');
  });
});

describe('runBenchmark', () => {
  const SAMPLE_RATE = 24000;

  /** A provider stub whose engine synthesizes deterministically. `synthMs` drives the
   *  timed durations via a scripted performance.now(); `samples` sizes the produced audio. */
  function providerWith(opts: {
    device?: string;
    webgpu?: boolean;
    sampleLengths?: number[];
  }): { provider: KokoroProvider; engine: LoadedEngine; synth: ReturnType<typeof vi.fn> } {
    const lengths = opts.sampleLengths ?? [];
    let call = 0;
    const synth = vi.fn(async () => {
      // First synth call is the (untimed) warm-up; the timed lines follow.
      const len = call === 0 ? 0 : (lengths[call - 1] ?? SAMPLE_RATE);
      call++;
      return new Float32Array(len);
    });
    const engine = {
      device: opts.device ?? 'webgpu',
      webgpu: opts.webgpu ?? true,
      voices: () => [],
      supports: () => true,
      synth,
    } as LoadedEngine;
    const provider = { engineHandle: vi.fn(async () => engine) } as unknown as KokoroProvider;
    return { provider, engine, synth };
  }

  let infoSpy: ReturnType<typeof vi.spyOn>;

  beforeEach(() => {
    infoSpy = vi.spyOn(console, 'info').mockImplementation(() => {});
    // Deterministic clock: init(2), warmup(2), then 2 marks per timed line.
    let t = 0;
    vi.spyOn(performance, 'now').mockImplementation(() => (t += 100));
    vi.stubGlobal('navigator', { deviceMemory: 8 });
  });

  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it('loads the engine, warms up, times every line and returns a full result', async () => {
    const { provider, synth } = providerWith({
      sampleLengths: [SAMPLE_RATE, SAMPLE_RATE, SAMPLE_RATE],
    });
    const res = await runBenchmark(provider);

    // 1 warm-up + one synth per BENCH_LINES entry, all on the engine default voice ('').
    expect(synth).toHaveBeenCalledTimes(BENCH_LINES.length + 1);
    expect(synth).toHaveBeenNthCalledWith(1, BENCH_LINES[0], '');
    expect(synth).toHaveBeenLastCalledWith(BENCH_LINES[BENCH_LINES.length - 1], '');

    expect(res.engine).toBe('vox');
    expect(res.webgpu).toBe(true);
    expect(res.deviceMemoryGb).toBe(8);
    expect(res.initMs).toBeGreaterThan(0);
    expect(res.firstAudioMs).toBeGreaterThan(0);
    expect(res.avgSynthMs).toBeGreaterThan(0);
    expect(res.rtf).toBeGreaterThan(0); // audio produced → real-time factor computed
    expect(typeof res.passed).toBe('boolean');
  });

  it('leaves rtf undefined when no audio is produced (zero-length samples)', async () => {
    const { provider } = providerWith({ sampleLengths: [0, 0, 0] });
    const res = await runBenchmark(provider);
    expect(res.rtf).toBeUndefined();
  });

  it('reports webgpu=false / no deviceMemory when the device lacks them', async () => {
    vi.stubGlobal('navigator', {});
    const { provider } = providerWith({ device: 'wasm', webgpu: false });
    const res = await runBenchmark(provider);
    expect(res.webgpu).toBe(false);
    expect(res.deviceMemoryGb).toBeUndefined();
  });

  it('passes the verdict when the scripted timings land under threshold', async () => {
    // 100ms-per-mark clock → each timed line ~100ms, well under the pass thresholds.
    const { provider } = providerWith({ sampleLengths: [SAMPLE_RATE, SAMPLE_RATE, SAMPLE_RATE] });
    const res = await runBenchmark(provider);
    expect(res.firstAudioMs).toBeLessThanOrEqual(TTS_CONFIG.BENCH_FIRST_AUDIO_MS);
    expect(res.avgSynthMs).toBeLessThanOrEqual(TTS_CONFIG.BENCH_AVG_SYNTH_MS);
    expect(res.passed).toBe(true);
  });
});
