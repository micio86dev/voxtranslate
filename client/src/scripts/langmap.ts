// Shared language↔tier map (spec 0102). Thin typed wrapper over the single source of
// truth `server/src/engine/languages.json` — the SAME file the Rust backend embeds
// (`server/src/engine/langmap.rs`, via `include_str!`). It lives under `server/` (not a
// repo-root `shared/`) because the server's Railway build only uploads `server/`; the
// client/Vercel build has the whole repo on disk, so this relative import resolves and
// Vite/rollup inlines the JSON at build time. Keeps the picker's language list, region
// grouping, RTL flags and per-tier filtering in lockstep with `/api/engines`.

import langMapJson from '../../../server/src/engine/languages.json';

export interface LangMeta {
  /** BCP-47-ish short code (e.g. `en`, `zh`, `pt`, `fil`). */
  code: string;
  /** Endonym — the language's name in itself (e.g. `Français`, `中文`). */
  native: string;
  /** English name, shown in parentheses after the endonym. */
  english: string;
  /** Picker grouping bucket — one of `regions`. */
  region: string;
  /** Right-to-left script (drives `document.documentElement.dir`). */
  rtl: boolean;
  /** Flag emoji (🌐 for stateless/cross-border languages — never a misleading flag). */
  flag: string;
}

interface LangMapFile {
  regions: string[];
  languages: LangMeta[];
  /** tier id → its OUTPUT-language codes. */
  tiers: Record<string, string[]>;
}

const MAP = langMapJson as unknown as LangMapFile;

/** Every language in the union, in the JSON's declared order. */
export const LANGUAGES: readonly LangMeta[] = MAP.languages;

/** Region grouping order used by the picker. */
export const REGIONS: readonly string[] = MAP.regions;

/** All union language codes (the master set the picker offers). */
export const ALL_LANG_CODES: readonly string[] = MAP.languages.map((l) => l.code);

const BY_CODE: ReadonlyMap<string, LangMeta> = new Map(MAP.languages.map((l) => [l.code, l]));

/** Metadata for a code, or `undefined` if it isn't in the union. */
export function langMeta(code: string): LangMeta | undefined {
  return BY_CODE.get(code);
}

/** Whether a language is written right-to-left. Unknown code → `false`. */
export function isRtlLang(code: string): boolean {
  return BY_CODE.get(code)?.rtl ?? false;
}

/** OUTPUT-language codes for a tier id (`standard|enhanced|pro|premium`); `[]` if unknown. */
export function tierOutputLangs(tier: string): readonly string[] {
  return MAP.tiers[tier] ?? [];
}
