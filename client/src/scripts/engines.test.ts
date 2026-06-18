import { afterEach, describe, expect, it, vi } from 'vitest';

import {
  DEFAULT_ENGINE_ID,
  ENGINE_PREF_KEY,
  type EngineInfo,
  commonLangs,
  defaultEngineId,
  engineDescKey,
  engineLangs,
  engineNeedsPcm,
  formatRate,
  loadEnginePref,
  resolveEnginePref,
  saveEnginePref,
} from './engines';

function engine(id: string, langs: string[], rate = 0.01): EngineInfo {
  return {
    id,
    display_name: id,
    tier: id,
    description: '',
    rate_per_minute: rate,
    input_languages: langs,
    output_languages: langs,
    capabilities: {
      translated_audio: id === 'premium',
      cost_scales_per_language: id === 'premium',
      max_room_size: 4,
    },
  };
}

const STANDARD = engine('standard', ['it', 'en', 'es']);
const PREMIUM = engine('premium', ['en', 'fr'], 0.45);

afterEach(() => {
  delete (globalThis as { localStorage?: unknown }).localStorage;
  vi.restoreAllMocks();
});

describe('defaultEngineId', () => {
  it('prefers the canonical standard engine', () => {
    expect(defaultEngineId([PREMIUM, STANDARD])).toBe(DEFAULT_ENGINE_ID);
  });
  it('falls back to the first engine when standard is absent', () => {
    expect(defaultEngineId([PREMIUM])).toBe('premium');
  });
  it('returns empty string for no engines', () => {
    expect(defaultEngineId([])).toBe('');
  });
});

describe('resolveEnginePref', () => {
  it('keeps a stored id that still exists', () => {
    expect(resolveEnginePref('premium', [STANDARD, PREMIUM])).toBe('premium');
  });
  it('falls back to the default when the stored id was removed (spec 0093)', () => {
    expect(resolveEnginePref('gone', [STANDARD, PREMIUM])).toBe('standard');
  });
  it('falls back to the default when nothing is stored', () => {
    expect(resolveEnginePref(null, [STANDARD, PREMIUM])).toBe('standard');
  });
});

describe('engineLangs', () => {
  it('intersects the engine output langs with the displayable set', () => {
    // PREMIUM supports en, fr; known is the app's 8 → only en, fr survive, in
    // `known` order.
    expect(engineLangs('premium', [STANDARD, PREMIUM], ['it', 'en', 'fr', 'de'])).toEqual([
      'en',
      'fr',
    ]);
  });
  it('returns the known set unchanged for an unknown engine', () => {
    expect(engineLangs('nope', [STANDARD], ['it', 'en'])).toEqual(['it', 'en']);
  });
});

describe('commonLangs', () => {
  it('returns only languages every engine can output (mixed-engine safe)', () => {
    // STANDARD outputs it/en/es, PREMIUM outputs en/fr → common = en.
    expect(commonLangs([STANDARD, PREMIUM], ['it', 'en', 'es', 'fr'])).toEqual(['en']);
  });
  it('intersects with the displayable set', () => {
    const wide = engine('x', ['en', 'de', 'ja']);
    expect(commonLangs([wide], ['en', 'de'])).toEqual(['en', 'de']);
  });
  it('falls back to known when there are no engines', () => {
    expect(commonLangs([], ['it', 'en'])).toEqual(['it', 'en']);
  });
});

describe('engineNeedsPcm', () => {
  // The Gemini engine: speech-to-speech (translated_audio) like OpenAI, but its id
  // is NOT `premium` — the exact case the old `id === 'premium'` check missed.
  const GEMINI: EngineInfo = {
    ...engine('gemini_live_translate', ['en', 'it']),
    capabilities: { translated_audio: true, cost_scales_per_language: true, max_room_size: 4 },
  };
  const list = [STANDARD, PREMIUM, GEMINI];

  it('is true for every translated-audio engine — OpenAI AND Gemini', () => {
    expect(engineNeedsPcm('premium', list)).toBe(true);
    // Regression (#258/#260): Gemini needs PCM16 too, despite its non-`premium` id.
    expect(engineNeedsPcm('gemini_live_translate', list)).toBe(true);
  });
  it('is false for Standard (captures WebM/Opus for Deepgram)', () => {
    expect(engineNeedsPcm('standard', list)).toBe(false);
  });
  it('is false for an unknown or absent engine (safe WebM default)', () => {
    expect(engineNeedsPcm('nope', list)).toBe(false);
    expect(engineNeedsPcm(undefined, list)).toBe(false);
    expect(engineNeedsPcm('premium', [])).toBe(false);
  });
});

describe('formatRate', () => {
  it('formats a per-minute USD rate', () => {
    expect(formatRate(0.01)).toBe('$0.010/min');
    expect(formatRate(0.45)).toBe('$0.450/min');
  });
});

describe('preference persistence', () => {
  it('round-trips through localStorage', () => {
    const mem = new Map<string, string>();
    (globalThis as { localStorage?: unknown }).localStorage = {
      getItem: (k: string) => mem.get(k) ?? null,
      setItem: (k: string, v: string) => void mem.set(k, v),
    };
    expect(loadEnginePref()).toBeNull();
    saveEnginePref('premium');
    expect(mem.get(ENGINE_PREF_KEY)).toBe('premium');
    expect(loadEnginePref()).toBe('premium');
  });

  it('no-ops without throwing when storage is unavailable (e.g. private mode)', () => {
    // No global localStorage defined → store() returns null.
    expect(() => saveEnginePref('premium')).not.toThrow();
    expect(loadEnginePref()).toBeNull();
  });

  it('swallows a throwing storage backend', () => {
    (globalThis as { localStorage?: unknown }).localStorage = {
      getItem: () => {
        throw new Error('blocked');
      },
      setItem: () => {
        throw new Error('blocked');
      },
    };
    expect(loadEnginePref()).toBeNull();
    expect(() => saveEnginePref('x')).not.toThrow();
  });

  it('maps tiers to localized description keys, null for unknown (#236)', () => {
    expect(engineDescKey('standard')).toBe('engineDescStandard');
    expect(engineDescKey('pro')).toBe('engineDescPro'); // OpenAI = the "Pro" tier
    expect(engineDescKey('premium')).toBe('engineDescPremium'); // Gemini = the "Premium" tier
    expect(engineDescKey('enterprise')).toBeNull(); // unknown → caller falls back to server desc
  });
});
