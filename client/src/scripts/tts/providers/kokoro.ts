// Vox Voices provider — a THIN TTSProvider over the heavy engine chunk. Capability
// checks (supports) and voice listing come straight from the installed pack's metadata
// (cheap, synchronous), so the manager can route/skip Vox without ever loading the
// engine. The multi-MB engine is pulled ONLY via a dynamic import() inside ensureEngine(),
// on first synth (or warm()) — never at app boot. `import type` below is erased at compile
// time and does NOT drag the engine into this chunk.

import { PlaybackService } from '../playback';
import type { InstallMeta, PackStorage } from '../storage';
import type { SpeakOptions, TTSProvider, VoiceInfo } from '../types';
import type { LoadedEngine, LoadOptions } from '../kokoro-engine';

function baseLang(lang: string): string {
  return lang.toLowerCase().split(/[-_]/)[0];
}

export class KokoroProvider implements TTSProvider {
  readonly id = 'vox';
  private engine: LoadedEngine | null = null;
  private loading: Promise<LoadedEngine> | null = null;
  private playback = new PlaybackService();
  private langs: Set<string>;
  private defaultVoice: string;

  /** Optional hook: synth latency (ms) per utterance — the health monitor consumes it. */
  onTiming: ((ms: number) => void) | null = null;

  constructor(
    private storage: PackStorage,
    private meta: InstallMeta,
    private loadOpts: LoadOptions = {},
  ) {
    this.langs = new Set(meta.languages.map(baseLang));
    this.defaultVoice = meta.voices[0]?.id ?? 'af_heart';
  }

  /** Registered on the manager only when the pack is installed & enabled, so the pack
   *  is present by construction; the engine itself loads lazily on first use. */
  isAvailable(): boolean {
    return true;
  }

  supports(lang: string): boolean {
    return this.langs.has(baseLang(lang));
  }

  async listVoices(): Promise<VoiceInfo[]> {
    return this.meta.voices.map((v) => ({
      id: v.id,
      name: v.name,
      lang: v.lang,
      provider: this.id,
      local: true,
    }));
  }

  /** Force the engine to load now (call on enable/unlock so the first real line isn't
   *  paying cold-load latency). Safe to call repeatedly — the load is memoised. */
  warm(): Promise<LoadedEngine> {
    return this.ensureEngine();
  }

  async speak(text: string, lang: string, opts?: SpeakOptions): Promise<void> {
    const engine = await this.ensureEngine();
    const t0 = performance.now();
    const samples = await engine.synth(text, this.voiceForLang(lang, opts?.voiceId));
    this.onTiming?.(performance.now() - t0);
    await this.playback.play(samples);
  }

  /** Pick a voice whose language matches the utterance. The saved preference wins only
   *  when it speaks the right language (the pack is now multilingual, so an English voice
   *  must not be used to read Italian) — otherwise fall back to the first matching voice. */
  private voiceForLang(lang: string, preferredId?: string): string {
    const base = baseLang(lang);
    const pref = this.meta.voices.find((v) => v.id === preferredId);
    if (pref && baseLang(pref.lang) === base) return pref.id;
    const match = this.meta.voices.find((v) => baseLang(v.lang) === base);
    return match?.id ?? preferredId ?? this.defaultVoice;
  }

  unlock(): void {
    this.playback.unlock();
    void this.ensureEngine().catch(() => {}); // background warm; failures surface at speak()
  }

  stop(): void {
    this.playback.stop();
  }

  /** The loaded engine (for the benchmark), loading it if needed. */
  engineHandle(): Promise<LoadedEngine> {
    return this.ensureEngine();
  }

  private ensureEngine(): Promise<LoadedEngine> {
    if (this.engine) return Promise.resolve(this.engine);
    if (this.loading) return this.loading;
    this.loading = import('../kokoro-engine')
      .then((m) => m.loadKokoro(this.storage, this.meta, this.loadOpts))
      .then((e) => {
        this.engine = e;
        return e;
      })
      .catch((err) => {
        this.loading = null; // allow a later retry
        throw err;
      });
    return this.loading;
  }
}
