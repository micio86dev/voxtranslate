// @vitest-environment jsdom
// The ONE place that decides how the microphone is opened. Before this module the
// same decision lived in five call sites and two of them got it wrong (the webinar
// publish path shipped no filtering at all), so these tests pin the contract rather
// than any one caller.
import { beforeEach, describe, expect, it, vi } from 'vitest';

import {
  buildAudioConstraints,
  loadNoisyEnv,
  noiseMixPercent,
  saveNoisyEnv,
} from './mic-constraints';

beforeEach(() => {
  localStorage.clear();
  vi.unstubAllGlobals();
});

describe('buildAudioConstraints', () => {
  it('keeps the browser filter on by default — the current behaviour, unchanged', () => {
    const c = buildAudioConstraints({});
    expect(c.noiseSuppression).toBe(true);
    expect(c.echoCancellation).toBe(true);
    expect(c.autoGainControl).toBe(true);
    expect(c.channelCount).toBe(1);
  });

  it('turns the BROWSER filter off in noisy-environment mode', () => {
    // Two suppressors in series fight each other: the browser leaves artifacts and
    // RNNoise then treats them as signal, which sounds worse than either alone. When
    // our own denoiser runs, the native one must step aside.
    const c = buildAudioConstraints({ noisyEnv: true });
    expect(c.noiseSuppression).toBe(false);
  });

  it('never touches echo cancellation or gain, in either mode', () => {
    // AEC is structural for Talk to Anyone on one device (see self-audio-guard.ts):
    // without it the translated speech re-enters the microphone. It is NOT part of
    // this toggle, and neither is AGC.
    for (const noisyEnv of [false, true]) {
      const c = buildAudioConstraints({ noisyEnv });
      expect(c.echoCancellation).toBe(true);
      expect(c.autoGainControl).toBe(true);
    }
  });

  it('pins a chosen device exactly, and omits the key when none is chosen', () => {
    expect(buildAudioConstraints({ deviceId: 'mic-1' }).deviceId).toEqual({ exact: 'mic-1' });
    expect('deviceId' in buildAudioConstraints({})).toBe(false);
    // An empty string is "no choice", not a device named "".
    expect('deviceId' in buildAudioConstraints({ deviceId: '' })).toBe(false);
  });
});

describe('noiseMixPercent', () => {
  it('defaults to 70 when unset or unparsable', () => {
    // Deliberately high: at 50 half the original noise survives, and a user who turns
    // the toggle on and still hears traffic concludes it does not work.
    expect(noiseMixPercent(undefined)).toBe(70);
    expect(noiseMixPercent('')).toBe(70);
    expect(noiseMixPercent('abc')).toBe(70);
    expect(noiseMixPercent(NaN)).toBe(70);
  });

  it('accepts a configured value', () => {
    expect(noiseMixPercent('40')).toBe(40);
    expect(noiseMixPercent(85)).toBe(85);
    expect(noiseMixPercent('0')).toBe(0);
  });

  it('clamps out-of-range values instead of producing a nonsense gain', () => {
    // The value drives a GainNode pair; >1 would amplify and <0 would invert phase.
    expect(noiseMixPercent('150')).toBe(100);
    expect(noiseMixPercent('-20')).toBe(0);
  });
});

describe('noisy-environment preference', () => {
  it('is off until the user turns it on, and survives a reload', () => {
    expect(loadNoisyEnv()).toBe(false);
    saveNoisyEnv(true);
    expect(loadNoisyEnv()).toBe(true);
    saveNoisyEnv(false);
    expect(loadNoisyEnv()).toBe(false);
  });

  it('degrades to off when storage is unavailable rather than throwing', () => {
    // Safari private mode throws on setItem; a mic toggle must not take the app down.
    vi.stubGlobal('localStorage', {
      getItem: () => {
        throw new Error('denied');
      },
      setItem: () => {
        throw new Error('denied');
      },
    });
    expect(() => saveNoisyEnv(true)).not.toThrow();
    expect(loadNoisyEnv()).toBe(false);
  });
});
