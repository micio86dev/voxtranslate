// Provider-based TTS abstraction ("Vox Voices"). The app talks ONLY to TTSManager;
// concrete engines (Browser SpeechSynthesis, Vox/Kokoro, or any future local/cloud
// TTS) sit behind this interface, so the engine can be swapped without touching a
// single call site. Language coverage is a capability the manager QUERIES
// (`supports`), never a hardcoded list — new languages/providers need no app change.

/** User's engine preference. `auto` lets the manager choose (Vox when installed,
 *  capable & healthy, else Browser); `browser`/`vox` pin a provider — but a pinned
 *  provider that can't serve a given line still silently falls back to Browser. */
export type EnginePref = 'auto' | 'browser' | 'vox';

/** A selectable voice, provider-agnostic. `id` is the stable selector: for the
 *  browser provider it's the SpeechSynthesis `voiceURI` (device-local, not portable);
 *  for Vox it's the Kokoro voice id (portable). */
export interface VoiceInfo {
  id: string;
  name: string;
  /** BCP-47 tag, e.g. `en-US`. */
  lang: string;
  /** Owning provider id. */
  provider: string;
  /** True for on-device/offline voices (no network at speak time). */
  local?: boolean;
}

export interface SpeakOptions {
  /** Preferred voice id within the chosen provider; falls back to auto/default when
   *  absent or unavailable (device voices aren't portable across machines). */
  voiceId?: string;
  /** Playback rate; defaults to TTS_CONFIG.RATE. */
  rate?: number;
}

/** Device benchmark for the Vox engine — measured once after install/update (or on
 *  explicit request), stored locally, and used to pick the default engine. Numbers are
 *  developer-only; users see a friendly verdict. Defined here (not benchmark.ts) so
 *  storage can persist it without importing the benchmark runner (avoids a cycle). */
export interface BenchmarkResult {
  /** Engine benchmarked (e.g. `vox`). */
  engine: string;
  /** Model init / session-create latency (ms). */
  initMs: number;
  /** Time to first audible sample for a short line (ms). */
  firstAudioMs: number;
  /** Average synthesis time across the sample lines (ms). */
  avgSynthMs: number;
  /** Real-time factor: synth wall time ÷ produced audio duration (lower is better). */
  rtf?: number;
  /** WebGPU was available for inference. */
  webgpu: boolean;
  /** navigator.deviceMemory (GB), when the browser exposes it. */
  deviceMemoryGb?: number;
  /** Overall verdict: the device can comfortably run Vox as the default engine. */
  passed: boolean;
}

/** One synthesis backend. Implementations MUST NOT throw synchronously from
 *  `speak` — failures surface as a rejected promise so the manager can fall back. */
export interface TTSProvider {
  readonly id: string;
  /** Usable right now? (API present / pack installed & loaded.) */
  isAvailable(): boolean;
  /** Capability query — can this provider synthesize `lang`? Keyed on capability,
   *  never a hardcoded language. */
  supports(lang: string): boolean;
  /** Selectable voices (async: a lazy engine may need to load its voice list). */
  listVoices(): Promise<VoiceInfo[]>;
  /** Synthesize + play ONE utterance. Resolves when playback finishes; rejects on
   *  failure. The manager serializes calls (never concurrent). */
  speak(text: string, lang: string, opts?: SpeakOptions): Promise<void>;
  /** Prime audio inside a user gesture (iOS autoplay / SpeechSynthesis unlock). */
  unlock(): void;
  /** Stop playback and drop in-flight work. */
  stop(): void;
}
