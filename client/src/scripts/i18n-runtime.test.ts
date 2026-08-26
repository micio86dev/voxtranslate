// @vitest-environment jsdom
//
// Runtime i18n behaviour that needs a DOM (applyI18n) + the mutable module state
// (uiLang). The sibling i18n.test.ts stays under the default node environment for
// the dictionary-completeness suite.
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import en from './i18n/en.json';
import {
  I18N,
  SUPPORTED,
  getUiLang,
  setUiLang,
  loadLocale,
  t,
  applyI18n,
  isRtl,
  detectLang,
  resolveStoredLang,
} from './i18n';

const FIRST_KEY = Object.keys(en)[0];

beforeEach(() => {
  setUiLang('en'); // deterministic start
  document.documentElement.lang = '';
  document.documentElement.dir = '';
  document.body.innerHTML = '';
});

afterEach(() => {
  setUiLang('en');
});

describe('setUiLang / getUiLang', () => {
  it('switches to a supported language', () => {
    expect(SUPPORTED).toContain('it');
    setUiLang('it');
    expect(getUiLang()).toBe('it');
  });

  it('ignores an unsupported language', () => {
    setUiLang('it');
    setUiLang('zzz');
    expect(getUiLang()).toBe('it');
  });
});

describe('t', () => {
  it('returns the English string for the active (en) language', () => {
    expect(t(FIRST_KEY)).toBe(en[FIRST_KEY as keyof typeof en]);
  });

  it('returns the key itself for an unknown key', () => {
    expect(t('definitely.missing.key')).toBe('definitely.missing.key');
  });

  it('falls back to English when the active locale dict is not loaded', () => {
    setUiLang('it'); // it.json not loaded yet → en fallback
    expect(t(FIRST_KEY)).toBe(en[FIRST_KEY as keyof typeof en]);
  });
});

describe('loadLocale', () => {
  it('no-ops for en (already present) and for an unknown code', async () => {
    await loadLocale('en');
    await loadLocale('zzz-not-real');
    expect(I18N['zzz-not-real']).toBeUndefined();
  });

  it('lazy-loads a locale dictionary so t() resolves in that language', async () => {
    await loadLocale('it');
    expect(I18N.it).toBeDefined();
    setUiLang('it');
    // the IT value for the first key should now come from the loaded dict
    expect(typeof t(FIRST_KEY)).toBe('string');
    expect(t(FIRST_KEY)).toBe(I18N.it[FIRST_KEY]);
    // second load is a no-op (already in memory)
    await loadLocale('it');
    expect(I18N.it).toBeDefined();
  });
});

describe('applyI18n', () => {
  it('translates text, placeholder, title/aria, and label attributes', () => {
    const span = document.createElement('span');
    span.dataset.i18n = FIRST_KEY;
    const input = document.createElement('input');
    input.dataset.i18nPh = FIRST_KEY;
    const btn = document.createElement('button');
    btn.dataset.i18nTitle = FIRST_KEY;
    const nav = document.createElement('nav');
    nav.dataset.i18nLabel = FIRST_KEY;
    document.body.append(span, input, btn, nav);

    applyI18n();

    const expected = en[FIRST_KEY as keyof typeof en];
    expect(span.textContent).toBe(expected);
    expect(input.getAttribute('placeholder')).toBe(expected);
    expect(btn.getAttribute('title')).toBe(expected);
    expect(btn.getAttribute('aria-label')).toBe(expected);
    expect(nav.getAttribute('aria-label')).toBe(expected);
    expect(document.documentElement.lang).toBe('en');
    expect(document.documentElement.dir).toBe('ltr');
  });

  it('sets dir=rtl for a right-to-left UI language', () => {
    expect(isRtl('ar')).toBe(true);
    setUiLang('ar');
    applyI18n();
    expect(document.documentElement.dir).toBe('rtl');
    expect(document.documentElement.lang).toBe('ar');
  });
});

describe('resolveStoredLang', () => {
  afterEach(() => {
    document.cookie = 'vt_lang=; max-age=0';
  });

  it('prefers a stored vt_lang preference over browser detection', () => {
    document.cookie = 'vt_lang=es';
    expect(resolveStoredLang()).toBe('es');
  });

  it('ignores a stored language we ship no UI translation for', () => {
    document.cookie = 'vt_lang=zzz';
    expect(resolveStoredLang()).toBe(detectLang());
  });

  it('falls back to browser detection when nothing is stored', () => {
    expect(resolveStoredLang()).toBe(detectLang());
  });
});
