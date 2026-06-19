// UI internationalization (spec 0102). The interface language follows the chosen "my
// language", defaulting from the browser (fallback: en).
//
// The full union of languages + their metadata (endonym, flag, RTL) comes from the shared
// map (`langmap.ts` → `shared/languages.json`, the SAME file the backend embeds). Each
// locale's UI strings live in `./i18n/<code>.json` and are glob-loaded at build time, so
// adding a language = dropping in one JSON file. `en` is the fallback for any missing key.

import { LANGUAGES, isRtlLang } from './langmap';

type Dict = Record<string, string>;

// `en` is the fallback for every missing key, so it must always be present AND synchronous:
// ship it eagerly (~16 KB) in the entry chunk. Every OTHER locale is lazy — its dictionary is
// a separate chunk fetched on demand the first time that UI language is selected. This stops
// the first load from pulling all 84 locales (~577 KB gzip) just to paint one language (the
// LCP killer, spec 0104).
import enDict from './i18n/en.json';
const localeLoaders = import.meta.glob<Dict>('./i18n/*.json', { import: 'default' });
const codeOf = (p: string): string => p.replace(/.*\/([^/]+)\.json$/, '$1');

// Dictionaries currently in memory (`en` always; others added by `loadLocale`). `t()` reads
// this synchronously and falls back to `en` for any locale not yet loaded.
export const I18N: Record<string, Dict> = { en: enDict as Dict };

// UI languages we actually ship translations for — derived from the glob keys (known at build
// time, so NO dictionary loading is needed and detection stays synchronous). DISTINCT from the
// TARGET languages the picker offers (those come from the engines/map and may exceed our UI
// set; any missing UI string falls back to English). `en` is guaranteed present.
export const SUPPORTED: string[] = Object.keys(localeLoaders).map(codeOf);

/**
 * Lazy-load one locale's UI dictionary into `I18N` (its chunk is fetched on first use).
 * No-op when the dictionary is already present (incl. `en`) or the code is unknown. Fails
 * safe: on a chunk-load error the `en` fallback simply remains. Callers re-run `applyI18n()`
 * once this resolves to repaint the DOM in the loaded language.
 */
export async function loadLocale(lang: string): Promise<void> {
  if (I18N[lang]) return;
  const loader = localeLoaders[`./i18n/${lang}.json`];
  if (!loader) return;
  try {
    I18N[lang] = await loader();
  } catch {
    /* keep the en fallback if the locale chunk fails to load */
  }
}

// Endonym + flag for EVERY union language (from the shared map), so a language label renders
// even for a target we don't yet have a UI translation for. `auto` = detection pending
// (spec 0012) — resolved server-side after ~3s.
export const ENDONYM: Record<string, string> = Object.fromEntries(
  LANGUAGES.map((l) => [l.code, l.native]),
);
export const FLAG: Record<string, string> = {
  ...Object.fromEntries(LANGUAGES.map((l) => [l.code, l.flag])),
  auto: '🌐',
};

/** Whether a language is written right-to-left (drives `<html dir>` and per-element dir). */
export const isRtl = (code: string): boolean => isRtlLang(code);

export function detectLang(): string {
  // Runs at module load, so guard `navigator` for non-browser environments
  // (the unit tests run under a node environment without a global navigator).
  const lang = typeof navigator !== 'undefined' ? navigator.language : '';
  const nav = (lang || 'en').slice(0, 2).toLowerCase();
  return SUPPORTED.includes(nav) ? nav : 'en';
}

let uiLang = detectLang();
export const getUiLang = (): string => uiLang;
export function setUiLang(l: string): void {
  if (SUPPORTED.includes(l)) uiLang = l;
}
export const t = (key: string): string => I18N[uiLang]?.[key] ?? I18N.en[key] ?? key;

export function applyI18n(): void {
  document.documentElement.lang = uiLang;
  // RTL: mirror the whole document for right-to-left UI languages (spec 0102).
  document.documentElement.dir = isRtl(uiLang) ? 'rtl' : 'ltr';
  document.querySelectorAll<HTMLElement>('[data-i18n]').forEach((el) => {
    el.textContent = t(el.dataset.i18n!);
  });
  document.querySelectorAll<HTMLElement>('[data-i18n-ph]').forEach((el) => {
    el.setAttribute('placeholder', t(el.dataset.i18nPh!));
  });
  document.querySelectorAll<HTMLElement>('[data-i18n-title]').forEach((el) => {
    el.setAttribute('title', t(el.dataset.i18nTitle!));
    el.setAttribute('aria-label', t(el.dataset.i18nTitle!));
  });
  // aria-label only (no tooltip) — for landmark/region names.
  document.querySelectorAll<HTMLElement>('[data-i18n-label]').forEach((el) => {
    el.setAttribute('aria-label', t(el.dataset.i18nLabel!));
  });
}
