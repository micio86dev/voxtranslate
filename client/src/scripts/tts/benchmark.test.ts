import { describe, expect, it } from 'vitest';

import { benchmarkVerdict, benchmarkVerdictKey } from './benchmark';

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
