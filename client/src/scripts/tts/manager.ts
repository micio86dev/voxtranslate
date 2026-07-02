// The single surface the app uses for translated-voice playback. Owns the utterance
// queue (no-cut, drop-oldest past cap — spec 0040), picks a provider per line by
// capability + health, and silently falls back to Browser Voice on ANY failure so
// playback never stalls or crashes. Every existing app call site — speak(),
// unlockTts(), stopTts() — delegates here; nothing above this layer knows which
// engine is active (future-proof: swap Vox for another engine with no app change).

import { TTS_CONFIG } from './config';
import { HealthMonitor } from './health';
import { BrowserSpeechProvider } from './providers/browser';
import type { EnginePref, TTSProvider, VoiceInfo } from './types';

interface Job {
  text: string;
  lang: string;
  /** System phrases (timer confirmations) are always Browser — never spin up Vox. */
  system: boolean;
}

export class TTSManager {
  private browser: TTSProvider;
  /** Optional high-quality provider (Vox/Kokoro). Null until installed & loaded
   *  (registered in a later phase); the app works fully without it. */
  private vox: TTSProvider | null = null;

  private pref: EnginePref = 'auto';
  private voxVoiceId: string | undefined;
  private browserVoiceId: string | undefined;

  /** Benchmark verdict: only gates the `auto` DEFAULT. An explicit `vox` pin ignores
   *  it (the user may enable Vox even when the device benchmarks better on Browser). */
  private benchmarkPassed = false;

  /** Session-only Vox health; degrades to Browser on a sustained slowdown. */
  private health: HealthMonitor;

  private queue: Job[] = [];
  private pumping = false;

  private notifiedFallback = false;
  private onFallbackNotice: (() => void) | null = null;

  constructor(
    browser: TTSProvider = new BrowserSpeechProvider(),
    health: HealthMonitor = new HealthMonitor(),
  ) {
    this.browser = browser;
    this.health = health;
  }

  /** Register (or clear) the Vox provider once installed/loaded. */
  setVoxProvider(p: TTSProvider | null): void {
    this.vox = p;
  }

  setPreference(pref: EnginePref): void {
    this.pref = pref;
  }
  setVoxVoice(id: string | undefined): void {
    this.voxVoiceId = id;
  }
  setBrowserVoice(id: string | undefined): void {
    this.browserVoiceId = id;
  }
  /** The stored benchmark verdict for the installed pack (gates the `auto` default). */
  setBenchmarkPassed(v: boolean): void {
    this.benchmarkPassed = v;
  }
  /** Session-only degrade override. `true` forces Browser (e.g. a failed benchmark);
   *  `false` clears it so Vox is retried (e.g. a new session). Never persisted. */
  setDegraded(v: boolean): void {
    if (v) this.health.forceDegrade();
    else this.health.reset();
  }
  isDegraded(): boolean {
    return this.health.isDegraded();
  }

  /** Report a Vox utterance's startup latency (ms). A sustained slowdown degrades to
   *  Browser for the session and fires the one-time fallback notice. */
  recordVoxTiming(ms: number): void {
    if (this.health.record(ms)) this.noticeFallback();
  }
  /** UI hook: fired once per session the first time we silently fall back to Browser. */
  onFallback(cb: () => void): void {
    this.onFallbackNotice = cb;
  }

  /** Translated speech — the manager picks the provider by capability/health. */
  speak(text: string, lang: string): void {
    this.enqueue({ text, lang, system: false });
  }

  /** System phrases (e.g. timer confirmations): always Browser Voice — instant, and
   *  never spins up the heavy Vox engine for a throwaway line. */
  speakSystem(text: string, lang: string): void {
    this.enqueue({ text, lang, system: true });
  }

  unlock(): void {
    this.browser.unlock();
    this.vox?.unlock();
  }

  /** Preview one voice on a specific engine, bypassing the routing/queue (Audio Settings).
   *  Never throws — a failed preview is a no-op. */
  preview(providerId: string, text: string, lang: string, voiceId?: string): Promise<void> {
    const p = providerId === 'vox' ? this.vox : this.browser;
    if (!p) return Promise.resolve();
    return p.speak(text, lang, { voiceId }).catch(() => {});
  }

  stop(): void {
    this.queue.length = 0;
    this.browser.stop();
    this.vox?.stop();
  }

  /** Which provider serves this line right now — pure decision, unit-tested.
   *  Order: system phrases and a session health-degrade always force Browser; then the
   *  preference; Vox only when it's registered, available and can serve the language;
   *  finally, on `auto`, the benchmark must have passed (an explicit `vox` pin skips
   *  that last gate). Anything else → Browser, the permanent fallback. */
  chooseProvider(lang: string, system: boolean): TTSProvider {
    if (system) return this.browser;
    if (this.health.isDegraded()) return this.browser;
    if (this.pref === 'browser') return this.browser;
    if (!this.vox || !this.vox.isAvailable() || !this.vox.supports(lang)) return this.browser;
    if (this.pref === 'vox') return this.vox; // explicit override, ignores benchmark
    return this.benchmarkPassed ? this.vox : this.browser; // pref === 'auto'
  }

  /** Selectable voices from every available provider (for the settings UI). */
  async listVoices(): Promise<{ browser: VoiceInfo[]; vox: VoiceInfo[] }> {
    const browser = await this.browser.listVoices().catch(() => [] as VoiceInfo[]);
    const vox = this.vox ? await this.vox.listVoices().catch(() => [] as VoiceInfo[]) : [];
    return { browser, vox };
  }

  private enqueue(job: Job): void {
    this.queue.push(job);
    if (this.queue.length > TTS_CONFIG.QUEUE_CAP)
      this.queue.splice(0, this.queue.length - TTS_CONFIG.QUEUE_CAP);
    void this.pump();
  }

  private async pump(): Promise<void> {
    if (this.pumping) return;
    this.pumping = true;
    try {
      let job: Job | undefined;
      while ((job = this.queue.shift())) {
        const provider = this.chooseProvider(job.lang, job.system);
        const voiceId = provider === this.vox ? this.voxVoiceId : this.browserVoiceId;
        try {
          await provider.speak(job.text, job.lang, { voiceId });
        } catch {
          // Silent fallback: retry this one line on Browser, notify once, keep going.
          if (provider !== this.browser) {
            this.noticeFallback();
            try {
              await this.browser.speak(job.text, job.lang, { voiceId: this.browserVoiceId });
            } catch {
              /* give up on this single line only */
            }
          }
        }
      }
    } finally {
      this.pumping = false;
    }
  }

  private noticeFallback(): void {
    if (this.notifiedFallback) return;
    this.notifiedFallback = true;
    try {
      this.onFallbackNotice?.();
    } catch {
      /* ignore */
    }
  }
}

/** Shared instance — the one TTS surface for the whole app. */
export const ttsManager = new TTSManager();
