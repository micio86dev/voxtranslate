// VoiceSelectionService — the user's TTS preferences, resolved and saved with a
// server-first / local-fallback strategy:
//   • engine preference (auto|browser|vox) + chosen VOX voice → synced server-side for
//     logged-in users (portable), cached in localStorage for guests/offline/first paint.
//   • chosen BROWSER voice → device-local ONLY (SpeechSynthesis voiceURIs are not
//     portable across OS/browser), so it never leaves this device.
// Resolution mirrors the has_voice_clone precedent (app.ts): prefer server truth, fall
// back to the local cache, then a safe default. The pure resolvers are unit-tested; the
// I/O wrappers are thin.

import { saveTtsPrefs } from '../api';
import { getUser, isLoggedIn, setTtsPrefs } from '../auth';
import { TTS_CONFIG } from './config';
import type { EnginePref } from './types';

/** A safe localStorage shim (no-ops when storage is blocked: private mode, SSR, tests).
 *  Same pattern as engines.ts / auth.ts. */
function store(): Pick<Storage, 'getItem' | 'setItem'> | null {
  try {
    if (typeof localStorage !== 'undefined') return localStorage;
  } catch {
    /* access blocked */
  }
  return null;
}

function readLocal(key: string): string | null {
  try {
    return store()?.getItem(key) ?? null;
  } catch {
    return null;
  }
}

function writeLocal(key: string, value: string): void {
  try {
    store()?.setItem(key, value);
  } catch {
    /* ignore */
  }
}

// ---- pure resolvers (unit-tested) ------------------------------------------

/** Coerce any stored/legacy value to a valid EnginePref, defaulting to `auto`. */
export function normalizePref(v: string | null | undefined): EnginePref {
  return v === 'browser' || v === 'vox' || v === 'auto' ? v : 'auto';
}

/** Engine preference with server-first precedence, then local, then `auto`. */
export function resolveEnginePref(
  server: string | null | undefined,
  local: string | null | undefined,
): EnginePref {
  return normalizePref(server ?? local ?? undefined);
}

/** A portable id (Vox voice) with server-first precedence, then local, then null. */
export function resolveVoice(
  server: string | null | undefined,
  local: string | null | undefined,
): string | null {
  return server ?? local ?? null;
}

// ---- I/O wrappers ----------------------------------------------------------

export function loadEnginePref(): EnginePref {
  return resolveEnginePref(getUser()?.tts_engine_pref, readLocal(TTS_CONFIG.ENGINE_PREF_KEY));
}

export function loadVoxVoice(): string | null {
  return resolveVoice(getUser()?.tts_voice_id, readLocal(TTS_CONFIG.VOX_VOICE_KEY));
}

/** Device-local only (browser voiceURIs aren't portable). */
export function loadBrowserVoice(): string | null {
  return readLocal(TTS_CONFIG.BROWSER_VOICE_KEY);
}

export function saveEnginePref(pref: EnginePref): void {
  writeLocal(TTS_CONFIG.ENGINE_PREF_KEY, pref);
  if (isLoggedIn()) {
    setTtsPrefs({ tts_engine_pref: pref });
    void saveTtsPrefs({ tts_engine_pref: pref });
  }
}

export function saveVoxVoice(id: string): void {
  writeLocal(TTS_CONFIG.VOX_VOICE_KEY, id);
  if (isLoggedIn()) {
    setTtsPrefs({ tts_voice_id: id });
    void saveTtsPrefs({ tts_voice_id: id });
  }
}

/** Persist the chosen browser voice locally (never server-side — not portable). */
export function saveBrowserVoice(id: string): void {
  writeLocal(TTS_CONFIG.BROWSER_VOICE_KEY, id);
}
