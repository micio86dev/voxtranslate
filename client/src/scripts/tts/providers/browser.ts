// Browser SpeechSynthesis provider — the PERMANENT fallback. This is the original
// on-device TTS path (spec 0040 no-cut, spec 0042 delay-first voice pick), moved
// behind TTSProvider with NO behavioural change: local-first voice scoring, rate 1.1,
// and the iOS silent-utterance unlock. The manager owns the queue/no-cut policy now,
// so this speaks ONE utterance per call and resolves when it ends (or errors — it
// never rejects, matching the original pumpTts() "advance on error" behaviour).

import { TTS_CONFIG } from '../config';
import type { SpeakOptions, TTSProvider, VoiceInfo } from '../types';

/** Base language code, e.g. `it-IT` → `it`. */
function baseLang(lang: string): string {
  return lang.toLowerCase().split(/[-_]/)[0];
}

/** True for Chrome/Chromium's natural "Google …" voices (e.g. "Google italiano"). */
function isGoogleVoice(v: SpeechSynthesisVoice): boolean {
  return /^google\b/i.test(v.name.trim());
}

/** Trim the raw platform voice list to the ones worth offering. On Chrome/Chromium the
 *  "Google …" voices sound markedly more natural than the bundled local ones, so for any
 *  language a Google voice covers we drop the rest (Italian collapses to just "Google
 *  italiano"). Languages with NO Google voice keep theirs, so nothing becomes unpickable.
 *  On Safari/Firefox there are no Google voices, so the list is returned unchanged — those
 *  platforms already expose only a couple of (Siri/system) voices per language. */
export function curateVoices(voices: SpeechSynthesisVoice[]): SpeechSynthesisVoice[] {
  const googleLangs = new Set(voices.filter(isGoogleVoice).map((v) => baseLang(v.lang)));
  if (!googleLangs.size) return voices;
  return voices.filter((v) => isGoogleVoice(v) || !googleLangs.has(baseLang(v.lang)));
}

/** The best default browser-voice id for `lang` from a VoiceInfo list — Google-preferring,
 *  then local — mirroring the runtime pickVoice order. Audio Settings uses it so the
 *  Standard (Browser) tier ALWAYS has a concrete voice pre-selected for the chosen output
 *  language. `lang` may be a base code (`it`), a regional code (`it-IT`) or `auto`/empty
 *  (then the best voice across all languages wins). Returns null only for an empty list. */
export function defaultBrowserVoiceId(voices: VoiceInfo[], lang: string): string | null {
  const base = baseLang(lang);
  const wantLang = !!base && base !== 'auto';
  // A specific language with no matching voice → null (the UI shows its empty state
  // rather than pre-selecting a wrong-language voice); `auto`/empty ranks over all voices.
  const pool = wantLang ? voices.filter((v) => baseLang(v.lang) === base) : voices;
  if (!pool.length) return null;
  const score = (v: VoiceInfo): number =>
    (/^google\b/i.test(v.name.trim()) ? 1000 : 0) + (v.local ? 100 : 0);
  return pool.reduce((best, v) => (score(v) > score(best) ? v : best)).id;
}

export class BrowserSpeechProvider implements TTSProvider {
  readonly id = 'browser';
  private unlocked = false;

  constructor() {
    // Warm the (async-populated) voice list at construction — the original warmed it
    // at module load (`speechSynthesis.getVoices()`), so first speak() has voices.
    try {
      window.speechSynthesis?.getVoices();
    } catch {
      /* no speechSynthesis (SSR / tests) */
    }
  }

  isAvailable(): boolean {
    return typeof window !== 'undefined' && !!window.speechSynthesis;
  }

  /** The browser can attempt ANY language: SpeechSynthesis silently uses a default
   *  voice when no exact match exists, so we never block a line here — this is the
   *  fallback of last resort. */
  supports(_lang: string): boolean {
    return this.isAvailable();
  }

  async listVoices(): Promise<VoiceInfo[]> {
    if (!this.isAvailable()) return [];
    const voices = curateVoices(await this.voicesReady());
    return voices.map((v) => ({
      id: v.voiceURI,
      name: v.name,
      lang: v.lang,
      provider: this.id,
      local: v.localService,
    }));
  }

  speak(text: string, lang: string, opts?: SpeakOptions): Promise<void> {
    return new Promise((resolve) => {
      if (!this.isAvailable()) return resolve();
      const u = new SpeechSynthesisUtterance(text);
      const v = this.pickVoice(lang, opts?.voiceId);
      if (v) u.voice = v;
      u.lang = lang;
      u.rate = opts?.rate ?? TTS_CONFIG.RATE;
      // Advance on both end AND error, exactly like the original pumpTts().
      u.onend = () => resolve();
      u.onerror = () => resolve();
      this.unlocked = true;
      window.speechSynthesis.speak(u);
    });
  }

  unlock(): void {
    if (this.unlocked || !this.isAvailable()) return;
    try {
      const u = new SpeechSynthesisUtterance(' ');
      u.volume = 0;
      window.speechSynthesis.speak(u);
      this.unlocked = true;
    } catch {
      /* best-effort: a later real speak() will still attempt to unlock */
    }
  }

  stop(): void {
    this.unlocked = false; // re-prime on the next join / TTS-toggle gesture (iOS)
    try {
      window.speechSynthesis?.cancel();
    } catch {
      /* ignore */
    }
  }

  // --- internals (ported from app.ts) -------------------------------------

  /** Pick a voice for `lang`, optimised for the owner's hard priority: MINIMAL
   *  DELAY. Local/offline voices (localService) start instantly, so they win
   *  heavily; among those we prefer premium/enhanced ones AT NO LATENCY COST; the
   *  browser default breaks ties. Network voices are a last resort (spec 0042). An
   *  explicit `voiceId` (user pick) wins outright — but ONLY when it can serve this
   *  language, so a stale/foreign pick degrades gracefully to auto-scoring. */
  private pickVoice(lang: string, voiceId?: string): SpeechSynthesisVoice | undefined {
    const want = lang.toLowerCase();
    const all = window.speechSynthesis.getVoices();
    if (voiceId) {
      const chosen = all.find((v) => v.voiceURI === voiceId);
      if (chosen && chosen.lang.toLowerCase().startsWith(want)) return chosen;
    }
    const matches = all.filter((v) => v.lang.toLowerCase().startsWith(want));
    if (!matches.length) return undefined;
    const score = (v: SpeechSynthesisVoice): number =>
      // Chrome's "Google …" voices sound clearly more natural, so they win outright —
      // a deliberate quality-over-latency choice for the browser fallback (they are
      // network voices, so this trades a little first-audio delay for a better voice).
      (isGoogleVoice(v) ? 1000 : 0) +
      (v.localService ? 100 : 0) + // local = instant; the dominant factor on Safari/Firefox
      (/premium|enhanced|neural|natural|siri/i.test(`${v.name} ${v.voiceURI}`) ? 10 : 0) +
      (v.default ? 1 : 0);
    return matches.reduce((best, v) => (score(v) > score(best) ? v : best));
  }

  /** Resolve the voice list, which SpeechSynthesis fills asynchronously on some
   *  browsers (empty on first call → `voiceschanged`). Bounded so it never hangs. */
  private voicesReady(): Promise<SpeechSynthesisVoice[]> {
    const now = window.speechSynthesis.getVoices();
    if (now.length) return Promise.resolve(now);
    return new Promise((resolve) => {
      let done = false;
      const finish = (): void => {
        if (done) return;
        done = true;
        resolve(window.speechSynthesis.getVoices());
      };
      window.speechSynthesis.addEventListener?.('voiceschanged', finish, { once: true });
      setTimeout(finish, 500); // fallback if the event never fires
    });
  }
}
