import { afterEach, describe, expect, it, vi } from 'vitest';

import {
  DEFAULT_ENGINE_ID,
  ENGINE_PREF_KEY,
  type EngineInfo,
  cheapestTier,
  commonLangs,
  defaultEngineId,
  engineDescKey,
  engineIsClientDirect,
  engineLangs,
  engineNeedsPcm,
  formatRate,
  getAvailableTiers,
  languagesByRegion,
  loadEnginePref,
  offeredLanguageCodes,
  resolveEnginePref,
  saveEnginePref,
  searchLanguages,
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
      client_direct: id === 'soniox',
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
    capabilities: {
      translated_audio: true,
      cost_scales_per_language: true,
      client_direct: false,
      max_room_size: 4,
    },
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

describe('engineIsClientDirect', () => {
  // Soniox "Enhanced" (spec 0101): browser ↔ provider directly; the listener
  // translates in-browser. Keyed on the capability, not the id.
  const SONIOX: EngineInfo = {
    ...engine('soniox', ['en', 'it']),
    tier: 'enhanced',
    capabilities: {
      translated_audio: false,
      cost_scales_per_language: true,
      client_direct: true,
      max_room_size: 4,
    },
  };
  const list = [STANDARD, SONIOX, PREMIUM];

  it('is true only for a client-direct engine', () => {
    expect(engineIsClientDirect('soniox', list)).toBe(true);
    expect(engineIsClientDirect('standard', list)).toBe(false);
    expect(engineIsClientDirect('premium', list)).toBe(false);
  });
  it('is false for an unknown or absent engine', () => {
    expect(engineIsClientDirect('nope', list)).toBe(false);
    expect(engineIsClientDirect(undefined, list)).toBe(false);
    expect(engineIsClientDirect('soniox', [])).toBe(false);
  });
  it('does not force PCM capture (Enhanced is receive-side, not translated_audio)', () => {
    // A Soniox listener must NOT flip the speaker to PCM16 — that is for server
    // speech-to-speech engines only.
    expect(engineNeedsPcm('soniox', list)).toBe(false);
  });
});

// ---- Language-first picker (spec 0102) ------------------------------------
describe('getAvailableTiers', () => {
  // STANDARD: it/en/es @0.01; PREMIUM: en/fr @0.45.
  it('returns only tiers that output the language, cheapest first', () => {
    expect(getAvailableTiers('en', [PREMIUM, STANDARD]).map((e) => e.id)).toEqual([
      'standard',
      'premium',
    ]);
    expect(getAvailableTiers('fr', [STANDARD, PREMIUM]).map((e) => e.id)).toEqual(['premium']);
    expect(getAvailableTiers('es', [STANDARD, PREMIUM]).map((e) => e.id)).toEqual(['standard']);
  });
  it('returns [] when no tier can output the language', () => {
    expect(getAvailableTiers('de', [STANDARD, PREMIUM])).toEqual([]);
  });
  it('does not mutate the input array', () => {
    const list = [PREMIUM, STANDARD];
    getAvailableTiers('en', list);
    expect(list.map((e) => e.id)).toEqual(['premium', 'standard']);
  });
});

describe('cheapestTier', () => {
  it('is the lowest-rate tier that supports the language', () => {
    expect(cheapestTier('en', [PREMIUM, STANDARD])?.id).toBe('standard');
    expect(cheapestTier('fr', [STANDARD, PREMIUM])?.id).toBe('premium');
  });
  it('is null when nothing supports the language', () => {
    expect(cheapestTier('de', [STANDARD, PREMIUM])).toBeNull();
  });
});

describe('offeredLanguageCodes', () => {
  it('is the union of the enabled engines output languages', () => {
    expect([...offeredLanguageCodes([STANDARD, PREMIUM])].sort()).toEqual(['en', 'es', 'fr', 'it']);
  });
  it('is empty with no engines', () => {
    expect(offeredLanguageCodes([]).size).toBe(0);
  });
});

describe('languagesByRegion', () => {
  it('groups offered languages by region and drops empty regions', () => {
    // STANDARD/PREMIUM offer it/en/es/fr — all European in the shared map.
    const groups = languagesByRegion([STANDARD, PREMIUM]);
    expect(groups.length).toBe(1);
    expect(groups[0].region).toBe('europe');
    expect(groups[0].languages.map((l) => l.code).sort()).toEqual(['en', 'es', 'fr', 'it']);
  });
  it('never offers a language no enabled engine can output', () => {
    const codes = new Set(languagesByRegion([STANDARD]).flatMap((g) => g.languages.map((l) => l.code)));
    expect(codes.has('fr')).toBe(false); // STANDARD has no fr
    expect(codes.has('it')).toBe(true);
  });
});

describe('searchLanguages', () => {
  const wide = engine('premium', ['es', 'fr', 'de', 'ja']);
  it('matches by english name, native name, or code (offered only)', () => {
    expect(searchLanguages('span', [wide]).map((l) => l.code)).toEqual(['es']);
    expect(searchLanguages('日本', [wide]).map((l) => l.code)).toEqual(['ja']);
    expect(searchLanguages('de', [wide]).some((l) => l.code === 'de')).toBe(true);
  });
  it('returns all offered languages for an empty query', () => {
    expect(searchLanguages('  ', [wide]).map((l) => l.code).sort()).toEqual(['de', 'es', 'fr', 'ja']);
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
    expect(engineDescKey('enhanced')).toBe('engineDescEnhanced'); // Soniox = the "Enhanced" tier
    expect(engineDescKey('pro')).toBe('engineDescPro'); // OpenAI = the "Pro" tier
    expect(engineDescKey('premium')).toBe('engineDescPremium'); // Gemini = the "Premium" tier
    expect(engineDescKey('enterprise')).toBeNull(); // unknown → caller falls back to server desc
  });
});
