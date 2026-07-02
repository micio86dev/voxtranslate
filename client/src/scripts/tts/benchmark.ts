// Benchmark: a one-time device probe run AFTER install, AFTER an update, or on explicit
// request — NEVER every launch. It times engine init + synthesis, checks WebGPU + memory,
// and produces a pass/fail verdict that drives the default-engine choice. Users see a
// friendly verdict; the raw numbers are Developer-Mode only. The verdict math is pure &
// unit-tested; the runner needs the real engine (browser-only).

import { TTS_CONFIG } from './config';
import type { KokoroProvider } from './providers/kokoro';
import type { BenchmarkResult } from './types';

const SAMPLE_RATE = 24000;

/** Fixed short lines — representative of real translated speech. */
export const BENCH_LINES = [
  'Hello there.',
  'How are you today?',
  'This is a quick test of the voice.',
];

/** Pure verdict: does the device comfortably run Vox as the DEFAULT engine? Passing
 *  needs both a fast first line and a fast average — a slow tail would lag live speech. */
export function benchmarkVerdict(
  m: { firstAudioMs: number; avgSynthMs: number },
  thresholds: { firstAudioMs: number; avgSynthMs: number } = {
    firstAudioMs: TTS_CONFIG.BENCH_FIRST_AUDIO_MS,
    avgSynthMs: TTS_CONFIG.BENCH_AVG_SYNTH_MS,
  },
): boolean {
  return m.firstAudioMs <= thresholds.firstAudioMs && m.avgSynthMs <= thresholds.avgSynthMs;
}

/** i18n key for the friendly, number-free verdict shown to users. */
export function benchmarkVerdictKey(passed: boolean): string {
  return passed ? 'voxBenchPass' : 'voxBenchUseBrowser';
}

/** Run the benchmark against an installed Vox provider. Loads the engine (timing init),
 *  synthesizes the sample lines (timing first-audio, average, real-time factor), and
 *  probes WebGPU + device memory. Browser-only. */
export async function runBenchmark(provider: KokoroProvider): Promise<BenchmarkResult> {
  const t0 = performance.now();
  const engine = await provider.engineHandle();
  const initMs = performance.now() - t0;

  const synthMs: number[] = [];
  let audioMs = 0;
  let firstAudioMs = 0;
  for (let i = 0; i < BENCH_LINES.length; i++) {
    const s0 = performance.now();
    const samples = await engine.synth(BENCH_LINES[i], ''); // '' → engine default voice
    const dt = performance.now() - s0;
    synthMs.push(dt);
    audioMs += (samples.length / SAMPLE_RATE) * 1000;
    if (i === 0) firstAudioMs = dt;
  }
  const totalSynth = synthMs.reduce((a, b) => a + b, 0);
  const avgSynthMs = totalSynth / synthMs.length;
  const deviceMemoryGb = (navigator as unknown as { deviceMemory?: number }).deviceMemory;

  return {
    engine: 'vox',
    initMs,
    firstAudioMs,
    avgSynthMs,
    rtf: audioMs > 0 ? totalSynth / audioMs : undefined,
    webgpu: engine.webgpu,
    deviceMemoryGb,
    passed: benchmarkVerdict({ firstAudioMs, avgSynthMs }),
  };
}
