// Browser SpeechSynthesis provider — the PERMANENT fallback. This is the original
// on-device TTS path (spec 0040 no-cut, spec 0042 delay-first voice pick), moved
// behind TTSProvider with NO behavioural change: local-first voice scoring, rate 1.1,
// and the iOS silent-utterance unlock. The manager owns the queue/no-cut policy now,
// so this speaks ONE utterance per call and resolves when it ends (or errors — it
// never rejects, matching the original pumpTts() "advance on error" behaviour).

import { TTS_CONFIG } from '../config';
import type { SpeakOptions, TTSProvider, VoiceInfo } from '../types';

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
    const voices = await this.voicesReady();
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
      (v.localService ? 100 : 0) + // local = instant; the dominant factor
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
