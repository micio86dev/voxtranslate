// Live Connect — the translation tiers the host can choose from (spec 0121, S4).
//
// Pure helpers: the server's `GET /api/live/engines` answer is validated here, the stored
// preference is re-validated against it, and the names a room shows are derived. No DOM, no
// network: `tier-picker.ts` draws, `api.ts` fetches.

import { DEFAULT_ENGINE_ID, selectableEngines, type EngineInfo } from '../engines';

/** The Enhanced tier (Cartesia): the phone talks to Cartesia directly. */
export const ENHANCED_ENGINE_ID = 'cartesia';

/** One tier as `GET /api/live/engines` lists it. `rate_per_minute` is PER LANGUAGE STREAM. */
export interface LiveEngine {
  id: string;
  display_name: string;
  tier: string;
  description: string;
  rate_per_minute: number;
  /** The balance the host needs to start a conversation on this tier. */
  min_balance: number;
  /** A hint only (`reason: "at_capacity"`): the server decides when the session is created. */
  available: boolean;
  scales_per_language: boolean;
  client_direct: boolean;
  /** Enhanced only: whether the host can have the translations spoken in their cloned voice. */
  voice_clone?: VoiceCloneInfo;
}

/** `available` is true only when cloning is on AND the account already has a clone. */
export interface VoiceCloneInfo {
  available: boolean;
  cloning_enabled: boolean;
  has_clone: boolean;
}

export interface LiveEngines {
  default: string;
  engines: LiveEngine[];
}

/** The names shown in a room, where the picker's list is not at hand (a guest has no account). */
const LABELS: Record<string, string> = {
  [DEFAULT_ENGINE_ID]: 'Standard',
  [ENHANCED_ENGINE_ID]: 'Enhanced',
  gemini_live_translate: 'Premium',
};

export function tierLabel(id: string): string {
  return LABELS[id] ?? id;
}

/** Standard only: what the picker shows while the list loads and when it cannot be loaded. */
export function fallbackEngines(): LiveEngines {
  return {
    default: DEFAULT_ENGINE_ID,
    engines: [
      {
        id: DEFAULT_ENGINE_ID,
        display_name: 'Standard',
        tier: 'standard',
        description: '',
        rate_per_minute: 0,
        min_balance: 0,
        available: true,
        scales_per_language: false,
        client_direct: false,
      },
    ],
  };
}

function num(v: unknown): number {
  return typeof v === 'number' && Number.isFinite(v) && v >= 0 ? v : 0;
}

function parseVoiceClone(raw: unknown): { voice_clone?: VoiceCloneInfo } {
  if (!raw || typeof raw !== 'object') return {};
  const v = raw as Record<string, unknown>;
  return {
    voice_clone: {
      available: v.available === true,
      cloning_enabled: v.cloning_enabled === true,
      has_clone: v.has_clone === true,
    },
  };
}

/**
 * The tiers a phone can actually use. Same rule as Meet's picker (`selectableEngines`): behind
 * the Great Firewall a client-direct tier cannot reach its provider, so it is not offered.
 * The default is re-pointed at Standard if it was the one dropped.
 */
export function selectableTiers(list: LiveEngines, restricted: boolean): LiveEngines {
  if (!restricted) return list;
  const asEngines = list.engines.map(
    (e) => ({ ...e, capabilities: { client_direct: e.client_direct } }) as unknown as EngineInfo,
  );
  const keep = new Set(selectableEngines(asEngines, true).map((e) => e.id));
  const engines = list.engines.filter((e) => keep.has(e.id));
  return { default: engines.some((e) => e.id === list.default) ? list.default : DEFAULT_ENGINE_ID, engines };
}

/** A usable list, or null. A malformed entry is dropped rather than trusted. */
export function parseEngines(data: unknown): LiveEngines | null {
  if (!data || typeof data !== 'object') return null;
  const raw = (data as { engines?: unknown }).engines;
  if (!Array.isArray(raw)) return null;
  const engines: LiveEngine[] = [];
  for (const item of raw) {
    if (!item || typeof item !== 'object') continue;
    const e = item as Record<string, unknown>;
    if (typeof e.id !== 'string' || !e.id) continue;
    engines.push({
      id: e.id,
      display_name: typeof e.display_name === 'string' && e.display_name ? e.display_name : tierLabel(e.id),
      tier: typeof e.tier === 'string' ? e.tier : '',
      description: typeof e.description === 'string' ? e.description : '',
      rate_per_minute: num(e.rate_per_minute),
      min_balance: num(e.min_balance),
      available: e.available !== false,
      scales_per_language: e.scales_per_language === true,
      client_direct: e.client_direct === true,
      ...parseVoiceClone(e.voice_clone),
    });
  }
  if (engines.length === 0) return null;
  const wanted = (data as { default?: unknown }).default;
  const def = typeof wanted === 'string' && engines.some((e) => e.id === wanted) ? wanted : engines[0].id;
  return { default: def, engines };
}

/**
 * The host's cached balance is below what this tier needs to start. Never true for the default
 * tier (the server is the authority and a 402 leads to the buy-credits path) and never when the
 * balance is unknown.
 */
export function needsMoreBalance(e: LiveEngine, balance: number | undefined): boolean {
  return e.id !== DEFAULT_ENGINE_ID && typeof balance === 'number' && balance < e.min_balance;
}

/** A remembered choice, re-validated against what is offered NOW; otherwise the default. */
export function resolveTier(stored: string | null, list: LiveEngines, balance: number | undefined): string {
  const hit = stored ? list.engines.find((e) => e.id === stored) : undefined;
  return hit && !needsMoreBalance(hit, balance) ? hit.id : list.default;
}
