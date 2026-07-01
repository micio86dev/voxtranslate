// Translation-engine selection (spec 0093). Pure / DOM-free helpers: fetch the
// available engines from the backend, persist the user's choice across sessions,
// and derive the language set the chosen engine supports. The fetch + rendering
// wiring lives in app.ts; everything here is unit-testable.

import { LANGUAGES, REGIONS, type LangMeta } from './langmap';

export interface EngineCapabilities {
  translated_audio: boolean;
  /** Cost scales with the number of distinct target languages in the room — the
   *  rate shown is per translation stream, and a group call costs more (spec 0093). */
  cost_scales_per_language: boolean;
  /** The browser connects DIRECTLY to the provider (spec 0108, Cartesia "Enhanced"):
   *  this listener translates the audio it receives in-browser via a server-minted
   *  access token, instead of the server running the engine. Drives the client-direct
   *  receive pipeline; the server never proxies this engine's audio. */
  client_direct: boolean;
  max_room_size: number;
}

/** One engine as returned by `GET /api/engines` (client-safe; no raw cost). */
export interface EngineInfo {
  id: string;
  display_name: string;
  tier: string;
  description: string;
  /** USD/minute the user is charged (cost × markup, computed server-side). */
  rate_per_minute: number;
  input_languages: string[];
  output_languages: string[];
  capabilities: EngineCapabilities;
}

/** Canonical default engine id — always present in any deployment. */
export const DEFAULT_ENGINE_ID = 'standard';

/** localStorage key for the persisted engine id (exact key, per spec 0093). */
export const ENGINE_PREF_KEY = 'voxtranslate_engine_preference';

/** A safe localStorage shim: returns `null` / no-ops when storage is blocked
 *  (private mode, SSR, tests). Mirrors the pattern in auth.ts. */
function store(): Pick<Storage, 'getItem' | 'setItem'> | null {
  try {
    if (typeof localStorage !== 'undefined') return localStorage;
  } catch {
    /* access blocked */
  }
  return null;
}

/** The persisted engine id, or `null` when unset/unavailable. */
export function loadEnginePref(): string | null {
  try {
    return store()?.getItem(ENGINE_PREF_KEY) ?? null;
  } catch {
    return null;
  }
}

/** Persist the chosen engine id (best-effort; silently ignores a blocked store). */
export function saveEnginePref(id: string): void {
  try {
    store()?.setItem(ENGINE_PREF_KEY, id);
  } catch {
    /* ignore */
  }
}

/** The default engine id for a list: prefer the canonical `standard`, else the
 *  first listed; empty string when there are no engines. */
export function defaultEngineId(engines: EngineInfo[]): string {
  if (engines.some((e) => e.id === DEFAULT_ENGINE_ID)) return DEFAULT_ENGINE_ID;
  return engines[0]?.id ?? '';
}

/** Resolve a (possibly stale or absent) stored preference against the available
 *  engines, falling back to the default when the stored id is gone (spec 0093). */
export function resolveEnginePref(stored: string | null, engines: EngineInfo[]): string {
  if (stored && engines.some((e) => e.id === stored)) return stored;
  return defaultEngineId(engines);
}

/** The languages to offer for `engineId`: the engine's output languages
 *  intersected with the app's displayable set (`known`), so we never offer a
 *  language we can't label. Falls back to `known` when the engine is unknown. */
export function engineLangs(engineId: string, engines: EngineInfo[], known: string[]): string[] {
  const engine = engines.find((e) => e.id === engineId);
  if (!engine) return known.slice();
  const out = new Set(engine.output_languages);
  return known.filter((l) => out.has(l));
}

/** Languages to offer in the picker regardless of which engine is selected: the
 *  INTERSECTION of every engine's output languages, intersected with the
 *  displayable set (`known`). A room can MIX engines, and any speaker (on any
 *  engine) must be able to translate INTO a listener's language — so a language is
 *  only safe to offer if every engine can produce it (spec 0094). Falls back to
 *  `known` when there are no engines. */
export function commonLangs(engines: EngineInfo[], known: string[]): string[] {
  if (engines.length === 0) return known.slice();
  return known.filter((l) => engines.every((e) => e.output_languages.includes(l)));
}

/** Format a per-minute rate for display, e.g. `$0.066/min`. Three decimals because
 *  the cheaper tiers land in the sub-cent-resolution range (e.g. $0.010 vs $0.066). */
export function formatRate(ratePerMinute: number): string {
  return `$${ratePerMinute.toFixed(3)}/min`;
}

/** Whether a speaker on `engineId` must capture raw PCM16 @ 24 kHz (the server-side
 *  speech-to-speech engines — OpenAI, Gemini) rather than WebM/Opus (Standard, which
 *  streams to Deepgram). Keyed on the `translated_audio` capability, NOT a hardcoded
 *  id: the Gemini engine's id is `gemini_live_translate`, so an `id === 'premium'`
 *  check silently sent it WebM/Opus, which its PCM-expecting session decoded as noise
 *  → no transcript, no translated voice. Unknown/absent engine → false (safe WebM
 *  default, used by Standard). */
export function engineNeedsPcm(engineId: string | undefined, engines: EngineInfo[]): boolean {
  return engines.find((e) => e.id === engineId)?.capabilities.translated_audio ?? false;
}

/** Whether a listener on `engineId` translates client-direct (spec 0108, Cartesia
 *  "Enhanced"): the browser connects straight to the provider with a server-minted
 *  access token and renders its own subtitles/voice, so the server neither runs the engine
 *  nor pushes this listener server-side translation. Keyed on the `client_direct`
 *  capability, never a hardcoded id. Unknown/absent engine → false. */
export function engineIsClientDirect(engineId: string | undefined, engines: EngineInfo[]): boolean {
  return engines.find((e) => e.id === engineId)?.capabilities.client_direct ?? false;
}

/** The engines a client on a Great-Firewall-restricted network may use: drops the
 *  client-direct tier(s) (e.g. Enhanced/Cartesia, whose browser connects straight to a
 *  GFW-blocked domain and can't reach it). `restricted=false` returns the list unchanged,
 *  so non-China clients are unaffected. Pure → unit-testable. */
export function selectableEngines(engines: EngineInfo[], restricted: boolean): EngineInfo[] {
  return restricted ? engines.filter((e) => !e.capabilities.client_direct) : engines;
}

/** The engine id a client should actually JOIN with on a restricted network: a
 *  client-direct tier (unreachable behind the GFW) is downgraded to the default
 *  server-proxied tier; anything else passes through unchanged. Deterministic backstop
 *  for the picker gate, applied once the reachability verdict is known. Pure. */
export function enforceEngineForNetwork(
  engineId: string,
  engines: EngineInfo[],
  restricted: boolean,
): string {
  return restricted && engineIsClientDirect(engineId, engines) ? DEFAULT_ENGINE_ID : engineId;
}

// ---- Language-first picker (spec 0102) -------------------------------------
// The picker flips the flow: pick a TARGET language from the union, then choose among
// the tiers that can output it. Languages and per-tier output lists come from the shared
// map (`langmap.ts` / `shared/languages.json`) — the same source the backend embeds.

/** The OUTPUT-language codes the picker may offer in THIS deployment: the union of the
 *  enabled engines' `output_languages`. A Standard-only backend offers only Standard's
 *  languages, never the whole map — we must never offer a language nothing can translate. */
export function offeredLanguageCodes(engines: EngineInfo[]): Set<string> {
  const codes = new Set<string>();
  for (const e of engines) for (const l of e.output_languages) codes.add(l);
  return codes;
}

/** A region heading plus its offered languages, for the grouped picker. */
export interface RegionGroup {
  region: string;
  languages: LangMeta[];
}

/** Offered languages grouped by region, in the map's region order; metadata in map order.
 *  Empty groups (no offered language in that region) are dropped so the picker has no
 *  empty headings. Only languages at least one enabled engine can output are included. */
export function languagesByRegion(engines: EngineInfo[]): RegionGroup[] {
  const offered = offeredLanguageCodes(engines);
  return REGIONS.map((region) => ({
    region,
    languages: LANGUAGES.filter((l) => l.region === region && offered.has(l.code)),
  })).filter((g) => g.languages.length > 0);
}

/** Case-insensitive search over the offered languages by native name, English name, or
 *  code — for the picker's search box. Returns flat results in map order. */
export function searchLanguages(query: string, engines: EngineInfo[]): LangMeta[] {
  const offered = offeredLanguageCodes(engines);
  const q = query.trim().toLowerCase();
  const pool = LANGUAGES.filter((l) => offered.has(l.code));
  if (!q) return pool;
  return pool.filter(
    (l) =>
      l.code.toLowerCase().includes(q) ||
      l.english.toLowerCase().includes(q) ||
      l.native.toLowerCase().includes(q),
  );
}

/** The enabled tiers that can output `langCode`, CHEAPEST FIRST (spec 0102). The picker
 *  shows exactly these as cards and pre-selects the first. Does not mutate `engines`. */
export function getAvailableTiers(langCode: string, engines: EngineInfo[]): EngineInfo[] {
  return engines
    .filter((e) => e.output_languages.includes(langCode))
    .sort((a, b) => a.rate_per_minute - b.rate_per_minute);
}

/** The cheapest tier that can output `langCode` (pre-selection), or `null` if none can. */
export function cheapestTier(langCode: string, engines: EngineInfo[]): EngineInfo | null {
  return getAvailableTiers(langCode, engines)[0] ?? null;
}

/** i18n key for an engine's user-facing description, by tier (#236). The server's
 *  `description` exposes provider/model jargon (Deepgram, Groq, GPT-Realtime),
 *  so the UI shows a localized, benefit-focused string instead. Returns null for
 *  an unknown/future tier so the caller falls back to the server description. */
export function engineDescKey(tier: string): string | null {
  if (tier === 'standard') return 'engineDescStandard';
  if (tier === 'enhanced') return 'engineDescEnhanced';
  if (tier === 'pro') return 'engineDescPro';
  if (tier === 'premium') return 'engineDescPremium';
  return null;
}
