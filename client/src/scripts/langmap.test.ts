import { describe, expect, it } from 'vitest';

import {
  ALL_LANG_CODES,
  LANGUAGES,
  REGIONS,
  isRtlLang,
  langMeta,
  tierOutputLangs,
} from './langmap';

describe('langmap (shared language↔tier map, spec 0102)', () => {
  it('exposes the full union in map order, English first', () => {
    expect(LANGUAGES.length).toBeGreaterThanOrEqual(80);
    expect(LANGUAGES[0].code).toBe('en');
    expect(ALL_LANG_CODES.length).toBe(LANGUAGES.length);
    expect(ALL_LANG_CODES[0]).toBe('en');
  });

  it('has unique codes', () => {
    expect(new Set(ALL_LANG_CODES).size).toBe(ALL_LANG_CODES.length);
  });

  it('declares the picker region buckets and every language uses one', () => {
    expect(REGIONS).toEqual(['europe', 'asia', 'mena', 'subsaharan', 'americas']);
    for (const l of LANGUAGES) expect(REGIONS).toContain(l.region);
  });

  it('every language carries complete metadata', () => {
    for (const l of LANGUAGES) {
      expect(l.code).not.toBe('');
      expect(l.native).not.toBe('');
      expect(l.english).not.toBe('');
      expect(l.flag).not.toBe('');
      expect(typeof l.rtl).toBe('boolean');
    }
  });
});

describe('langMeta', () => {
  it('returns the metadata for a known code', () => {
    const en = langMeta('en');
    expect(en?.english).toBe('English');
    expect(en?.native).toBe('English');
    expect(en?.region).toBe('europe');
    expect(langMeta('it')?.native).toBe('Italiano');
  });

  it('is undefined for a code outside the union', () => {
    expect(langMeta('zz')).toBeUndefined();
    expect(langMeta('')).toBeUndefined();
  });
});

describe('isRtlLang', () => {
  it('is true for right-to-left scripts', () => {
    for (const c of ['ar', 'he', 'fa', 'ur']) expect(isRtlLang(c), c).toBe(true);
  });

  it('is false for LTR codes and unknown codes (safe default)', () => {
    for (const c of ['en', 'it', 'ja', 'zz', '']) expect(isRtlLang(c), c).toBe(false);
  });
});

describe('tierOutputLangs', () => {
  it('returns each tier’s output set, always within the union', () => {
    for (const tier of ['standard', 'enhanced', 'pro', 'premium']) {
      const langs = tierOutputLangs(tier);
      expect(langs.length, tier).toBeGreaterThan(0);
      expect(langs, tier).toContain('en');
      for (const code of langs) expect(ALL_LANG_CODES, `${tier}:${code}`).toContain(code);
    }
  });

  // Coverage does NOT follow price. This used to assert standard <= pro <= enhanced,
  // which encoded a pricing ladder rather than a fact about the providers: the tiers are
  // different vendors, and a cheaper one can reach further. Standard (Qwen 3.5
  // LiveTranslate, 29 languages it can speak) now covers more than Pro (OpenAI
  // GPT-Realtime-Translate, 13). What actually has to hold is the floor and the ceiling.
  it('every tier carries the legacy 8, and premium covers the whole union', () => {
    const LEGACY_8 = ['it', 'en', 'es', 'fr', 'de', 'pt', 'ja', 'zh'];
    for (const tier of ['standard', 'pro', 'enhanced', 'premium']) {
      const langs = tierOutputLangs(tier);
      for (const code of LEGACY_8) expect(langs, `${tier}:${code}`).toContain(code);
    }
    // Premium (Gemini) is the universal fallback, so it must reach everything.
    expect(tierOutputLangs('premium').length).toBe(LANGUAGES.length);
  });

  it('standard offers only languages its engine can SPEAK', () => {
    // Qwen 3.5 LiveTranslate reaches 60 target languages but speaks 29; the rest are
    // text only. On a speech-to-speech tier a text-only target is subtitles and silence,
    // so the list is the audio-capable set — no wider, and no narrower than the model
    // actually supports, or we throw away reach we are already paying for.
    expect(tierOutputLangs('standard')).toHaveLength(29);
  });

  it('is [] for an unknown tier', () => {
    expect(tierOutputLangs('enterprise')).toEqual([]);
    expect(tierOutputLangs('')).toEqual([]);
  });
});
