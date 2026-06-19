import { afterEach, describe, expect, it, vi } from 'vitest';

import { SUPPORTED, detectLang, isRtl } from './i18n';

// Production ships only `en` eagerly and lazy-loads the rest (spec 0104), so the runtime
// `I18N` map is NOT fully populated at import time. The completeness suite still needs every
// dictionary, so it eager-globs them here — a TEST-ONLY import that never reaches the bundle.
const DICTS = import.meta.glob<Record<string, string>>('./i18n/*.json', {
  eager: true,
  import: 'default',
});
const codeOf = (p: string): string => p.replace(/.*\/([^/]+)\.json$/, '$1');
const dicts: Record<string, Record<string, string>> = {};
for (const [path, dict] of Object.entries(DICTS)) dicts[codeOf(path)] = dict;

// Placeholders like {n} / {balance} / {tier} must survive translation verbatim, or the
// runtime `.replace('{x}', …)` interpolation silently breaks.
const placeholders = (s: string): string =>
  [...new Set(s.match(/\{[a-zA-Z]+\}/g) ?? [])].sort().join(',');

const codes = Object.keys(dicts);
const enKeys = Object.keys(dicts.en).sort();

describe('i18n completeness (spec 0102)', () => {
  it('ships the full union of UI locales', () => {
    // 8 original + the language-first expansion. Guard against an accidental shrink.
    expect(codes.length).toBeGreaterThanOrEqual(80);
    expect(codes).toContain('en');
    expect(codes).toContain('ar'); // an RTL addition
    expect(codes).toContain('sw'); // a long-tail addition
  });

  it.each(codes)('locale "%s" has EXACTLY the en key set', (code) => {
    expect(Object.keys(dicts[code]).sort()).toEqual(enKeys);
  });

  it.each(codes)('locale "%s" preserves every placeholder', (code) => {
    const mismatched = enKeys.filter(
      (k) => placeholders(dicts[code][k]) !== placeholders(dicts.en[k]),
    );
    expect(mismatched).toEqual([]);
  });

  it.each(codes)('locale "%s" has no empty values', (code) => {
    const empty = enKeys.filter((k) => dicts[code][k].trim() === '');
    expect(empty).toEqual([]);
  });

  it('SUPPORTED equals the set of locales we actually ship', () => {
    expect([...SUPPORTED].sort()).toEqual([...codes].sort());
  });
});

describe('isRtl', () => {
  it('is true for right-to-left scripts', () => {
    for (const c of ['ar', 'he', 'fa', 'ur', 'ckb', 'ps']) expect(isRtl(c)).toBe(true);
  });
  it('is false for LTR and unknown codes', () => {
    for (const c of ['en', 'de', 'ja', 'zh', 'zzz']) expect(isRtl(c)).toBe(false);
  });
});

describe('detectLang', () => {
  afterEach(() => vi.unstubAllGlobals());

  it('maps a regional browser locale to a supported base code', () => {
    vi.stubGlobal('navigator', { language: 'ar-EG' });
    expect(detectLang()).toBe('ar');
    vi.stubGlobal('navigator', { language: 'ko-KR' });
    expect(detectLang()).toBe('ko');
  });
  it('falls back to en for an unsupported locale', () => {
    vi.stubGlobal('navigator', { language: 'xx-YY' });
    expect(detectLang()).toBe('en');
  });
});
