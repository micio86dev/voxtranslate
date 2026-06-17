// Translation-engine selection (spec 0093). Pure / DOM-free helpers: fetch the
// available engines from the backend, persist the user's choice across sessions,
// and derive the language set the chosen engine supports. The fetch + rendering
// wiring lives in app.ts; everything here is unit-testable.

export interface EngineCapabilities {
  translated_audio: boolean;
  /** Cost scales with the number of distinct target languages in the room — the
   *  rate shown is per translation stream, and a group call costs more (spec 0093). */
  cost_scales_per_language: boolean;
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

/** Format a per-minute rate for display, e.g. `$0.45/min`. */
export function formatRate(ratePerMinute: number): string {
  return `$${ratePerMinute.toFixed(2)}/min`;
}
