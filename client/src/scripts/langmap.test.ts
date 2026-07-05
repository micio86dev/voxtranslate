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

  it('tiers grow monotonically from standard to premium', () => {
    expect(tierOutputLangs('standard').length).toBeLessThanOrEqual(
      tierOutputLangs('pro').length,
    );
    expect(tierOutputLangs('pro').length).toBeLessThanOrEqual(
      tierOutputLangs('enhanced').length,
    );
    // Premium (Gemini) covers the whole union.
    expect(tierOutputLangs('premium').length).toBe(LANGUAGES.length);
  });

  it('is [] for an unknown tier', () => {
    expect(tierOutputLangs('enterprise')).toEqual([]);
    expect(tierOutputLangs('')).toEqual([]);
  });
});
