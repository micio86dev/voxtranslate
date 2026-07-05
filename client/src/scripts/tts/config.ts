// Central tuning for the TTS subsystem — every threshold lives here so nothing
// downstream hardcodes a magic number (spec requirement). Numbers are safe defaults;
// benchmark/health can be tuned without touching logic.

/** Read a non-empty PUBLIC_* build var as a string, else undefined. */
function envStr(key: string): string | undefined {
  const v = (import.meta.env as Record<string, unknown>)[key];
  return typeof v === 'string' && v ? v : undefined;
}

/** Vox Voices master switch. OFF as of 2026-07-05: the on-device Kokoro model is too slow
 *  on the CPU/wasm path (no usable WebGPU-fp32 today) for LIVE translation — the browser's
 *  own voices are far lower-latency. While this is false the whole feature stays dormant
 *  REGARDLESS of `PUBLIC_VOX_MANIFEST_URL` (no engine option, no install/benchmark UI,
 *  installed packs unused — Browser Voice only). Flip to true (and set the manifest env
 *  var) to bring it back. */
export const VOX_ENABLED = false;

export const TTS_CONFIG = {
  /** Max utterances queued for playback; past this the OLDEST still-waiting lines
   *  are dropped so playback stays near real-time (spec 0040 no-cut + backlog cap). */
  QUEUE_CAP: 8,
  /** Playback rate for translated speech (unchanged from the original path). */
  RATE: 1.1,
  /** Fixed preview line so voices compare like-for-like across engines/voices. */
  SAMPLE_SENTENCE: 'Hello! This is how I sound in VoxTranslate.',
  /** Per-language preview lines, so a voice previews in the language it actually speaks
   *  (keyed by base lang code; falls back to SAMPLE_SENTENCE). */
  SAMPLE_BY_LANG: {
    en: 'Hello! This is how I sound in VoxTranslate.',
    es: '¡Hola! Así es como sueno en VoxTranslate.',
    fr: 'Bonjour ! Voici comment je sonne dans VoxTranslate.',
    it: 'Ciao! Ecco come suono su VoxTranslate.',
    pt: 'Olá! É assim que eu soo no VoxTranslate.',
  } as Record<string, string>,

  /** Runtime health: if Vox time-to-first-audio exceeds this many ms for
   *  HEALTH_MAX_STRIKES consecutive utterances, fall back to Browser for the
   *  SESSION only (retried next session; never permanently disabled). */
  HEALTH_STARTUP_MS: 1500,
  HEALTH_MAX_STRIKES: 3,

  /** Benchmark pass thresholds (ms): a device "passes" (Vox becomes the default)
   *  when first-audio and average synthesis both land under these. Measured on a WARM
   *  engine (a throwaway synth primes the ONNX graph first), so these reflect sustained
   *  performance, not the one-time cold start. The runtime health monitor (HEALTH_*) is
   *  the backstop that demotes to Browser Voice if real use turns out slow anyway. */
  BENCH_FIRST_AUDIO_MS: 1800,
  BENCH_AVG_SYNTH_MS: 1000,
  /** Hard cap for the whole benchmark (engine load + synth). Generous, because the FIRST
   *  WebGPU run pays a one-time shader-compilation cost (tens of seconds) that a warm run
   *  never repeats — cutting it too low would fail capable devices before they warm up.
   *  Still bounded so a truly stuck device (iOS Safari) falls back instead of hanging. */
  BENCH_TIMEOUT_MS: 60000,

  /** Voice-pack manifest (Supabase Storage, public-read). Set at build time via
   *  PUBLIC_VOX_MANIFEST_URL; an empty string keeps the whole feature dormant
   *  (no install UI, Browser Voice only) — the zero-config default. */
  MANIFEST_URL: VOX_ENABLED ? (envStr('PUBLIC_VOX_MANIFEST_URL') ?? '') : '',

  /** Same-origin path prefix the Service Worker serves installed model bytes from
   *  (keeps model loading within `connect-src 'self'` / `worker-src 'self'`). */
  MODEL_PATH_PREFIX: '/vox-models/',

  /** localStorage keys (namespaced like the rest of the app). */
  ENGINE_PREF_KEY: 'voxtranslate_tts_engine',
  VOX_VOICE_KEY: 'voxtranslate_tts_vox_voice',
  BROWSER_VOICE_KEY: 'voxtranslate_tts_browser_voice',
  DEV_MODE_KEY: 'vox_dev_mode',
} as const;
