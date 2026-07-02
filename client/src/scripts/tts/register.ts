// Vox provider lifecycle — the single owner of the KokoroProvider instance, shared by
// boot (registerVoxIfInstalled) and Audio Settings (activate on install, deactivate on
// remove). Keeping ONE instance avoids loading the multi-MB engine twice. The heavy
// provider is only ever reached via dynamic import(), so nothing here is on the boot path.

import { ttsManager } from './manager';
import { loadBrowserVoice, loadEnginePref, loadVoxVoice } from './preferences';
import type { KokoroProvider } from './providers/kokoro';
import type { InstallMeta } from './storage';
import { packStorage } from './storage';
import { TTS_CONFIG } from './config';

let voxProvider: KokoroProvider | null = null;

/** The active Vox provider (for preview/benchmark), or null when none is installed. */
export function getVoxProvider(): KokoroProvider | null {
  return voxProvider;
}

/** Build + register the Vox provider for an installed pack (idempotent per pack). Wires
 *  its per-utterance timing into the manager's health monitor. Returns the instance. */
export async function activateVoxProvider(meta: InstallMeta): Promise<KokoroProvider> {
  const { KokoroProvider } = await import('./providers/kokoro');
  const provider = new KokoroProvider(packStorage, meta);
  provider.onTiming = (ms) => ttsManager.recordVoxTiming(ms);
  voxProvider = provider;
  ttsManager.setVoxProvider(provider);
  ttsManager.setVoxVoice(loadVoxVoice() ?? undefined);
  return provider;
}

/** Remove the Vox provider (on uninstall): the manager falls back to Browser Voice. */
export function deactivateVoxProvider(): void {
  voxProvider?.stop();
  voxProvider = null;
  ttsManager.setVoxProvider(null);
  ttsManager.setBenchmarkPassed(false);
}

/** Boot hook: push the saved engine/voice prefs to the manager, and — if a pack is
 *  installed — activate its provider and apply the stored benchmark verdict. Fully
 *  dormant unless PUBLIC_VOX_MANIFEST_URL is set AND a pack is installed. */
export async function registerVoxIfInstalled(): Promise<void> {
  // The engine preference + browser-voice pick govern Browser Voice too, so apply them
  // unconditionally (even with no pack installed).
  ttsManager.setPreference(loadEnginePref());
  ttsManager.setBrowserVoice(loadBrowserVoice() ?? undefined);

  if (!TTS_CONFIG.MANIFEST_URL) return; // feature not configured → stay fully dormant
  try {
    const metas = await packStorage.listMeta();
    const meta = metas.find((m) => m.engine === 'kokoro');
    if (!meta) return; // nothing installed yet → Browser Voice only
    await activateVoxProvider(meta);
    // Gate the `auto` default on the stored benchmark verdict (an explicit `vox` pin
    // ignores it — see the manager). A missing benchmark → not passed → Browser default.
    const bench = await packStorage.getBench(meta.packId);
    ttsManager.setBenchmarkPassed(bench?.result.passed ?? false);
  } catch {
    /* storage/engine unavailable → silently stay on Browser Voice */
  }
}
