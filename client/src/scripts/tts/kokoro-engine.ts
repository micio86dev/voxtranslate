// Vox Voices engine — the HEAVY chunk. It statically imports kokoro-js (which pulls
// @huggingface/transformers + onnxruntime-web + an inline-wasm phonemizer, ~several MB),
// so it MUST only ever be reached via a dynamic import() from providers/kokoro.ts — never
// from the app's boot path. Nothing here runs until the user actually installs & uses Vox.
//
// CSP / offline design (see the plan): everything loads SAME-ORIGIN.
//  • model + tokenizer  → transformers.js env.localModelPath → `/vox-models/<pack>/<ver>/`
//    which the Service Worker serves from the verified IndexedDB bytes.
//  • ORT wasm runtime   → env.backends.onnx.wasm.wasmPaths → `/vox-models/<pack>/<ver>/ort/`.
//  • voices             → kokoro-js hardcodes a huggingface.co URL but checks a
//    `kokoro-voices` Cache Storage FIRST, so we pre-seed that cache from IndexedDB and it
//    never hits the network (no HuggingFace, no CSP violation).

import { KokoroTTS } from 'kokoro-js';
import { env as tjEnv } from '@huggingface/transformers';

import { TTS_CONFIG } from './config';
import { espeakLangFor, espeakPhonemize, needsEspeak, normalizeForKokoro } from './espeak-phonemizer';
import type { InstallMeta, PackStorage } from './storage';
import type { VoiceInfo } from './types';

/** kokoro-js's hardcoded voice repo — the exact URL its cache-lookup keys on. */
const KOKORO_VOICE_REPO = 'https://huggingface.co/onnx-community/Kokoro-82M-v1.0-ONNX/resolve/main/voices';
const VOICE_CACHE = 'kokoro-voices';

export type KokoroDtype = 'fp32' | 'fp16' | 'q8' | 'q4' | 'q4f16';
export type KokoroDevice = 'wasm' | 'webgpu';

export interface LoadProgress {
  status: string;
  file?: string;
  progress?: number;
  loaded?: number;
  total?: number;
}

export interface LoadOptions {
  dtype?: KokoroDtype;
  device?: KokoroDevice;
  onProgress?: (p: LoadProgress) => void;
}

export interface LoadedEngine {
  readonly device: KokoroDevice;
  readonly webgpu: boolean;
  voices(): VoiceInfo[];
  supports(lang: string): boolean;
  /** Synthesize a whole line → Float32 PCM @ 24 kHz. */
  synth(text: string, voiceId: string): Promise<Float32Array>;
}

/** Base language code, e.g. `en-US` → `en`. */
function baseLang(lang: string): string {
  return lang.toLowerCase().split(/[-_]/)[0];
}

/** True when WebGPU is present; we still fall back to wasm on any load failure. */
function webgpuAvailable(): boolean {
  return typeof navigator !== 'undefined' && 'gpu' in navigator && !!navigator.gpu;
}

/** transformers.js dtype → the onnx/ filename it fetches (its suffix mapping). */
const MODEL_FILE: Record<KokoroDtype, string> = {
  fp32: 'model.onnx',
  fp16: 'model_fp16.onnx',
  q8: 'model_quantized.onnx',
  q4: 'model_q4.onnx',
  q4f16: 'model_q4f16.onnx',
};

/** Which model dtypes the installed pack actually ships (probed by onnx/ filename).
 *  Empty only for a legacy install whose meta predates file listing — callers then
 *  assume the old fp16-only build. */
function shippedDtypes(meta: InstallMeta): Set<KokoroDtype> {
  const paths = new Set(meta.files.map((f) => f.path));
  const out = new Set<KokoroDtype>();
  for (const dtype of Object.keys(MODEL_FILE) as KokoroDtype[])
    if (paths.has(`onnx/${MODEL_FILE[dtype]}`)) out.add(dtype);
  return out;
}

/** Pick a (device, dtype) that is BOTH shipped in the pack AND numerically sound.
 *  Kokoro's iSTFT vocoder comes out GARBLED under onnxruntime-web's fp16 WebGPU path
 *  (robotic ~half-second noise / distorted speech on Apple-Silicon Metal — kokoro-js's
 *  own README says "if using webgpu, we recommend dtype=fp32"), so we NEVER run fp16 on
 *  WebGPU:
 *    • WebGPU → fp32 only (GPU-resident and correct). q8's int8 ops silently fall back
 *      to CPU and fp16 distorts, so if the pack ships no fp32 we drop to the wasm path.
 *    • wasm/CPU → q8 (fast + correct), else fp16, else fp32.
 *  A fully-specified {device,dtype} override still wins verbatim (benchmark/tests). */
function chooseBackend(
  meta: InstallMeta,
  opts: LoadOptions,
): { device: KokoroDevice; dtype: KokoroDtype } {
  if (opts.device && opts.dtype) return { device: opts.device, dtype: opts.dtype };

  const shipped = shippedDtypes(meta);
  const has = (d: KokoroDtype): boolean => (shipped.size ? shipped.has(d) : d === 'fp16');
  const wantGpu = opts.device ? opts.device === 'webgpu' : webgpuAvailable();

  // fp32 is the only Kokoro dtype WebGPU renders correctly — never emit webgpu+fp16.
  if (wantGpu && has('fp32')) return { device: 'webgpu', dtype: 'fp32' };
  if (opts.dtype && has(opts.dtype)) return { device: 'wasm', dtype: opts.dtype };
  for (const d of ['q8', 'fp16', 'fp32'] as KokoroDtype[])
    if (has(d)) return { device: 'wasm', dtype: d };
  return { device: 'wasm', dtype: 'fp16' };
}

/** Point transformers.js at the SAME-ORIGIN, SW-served pack (never remote). */
function configureEnv(meta: InstallMeta): void {
  tjEnv.allowRemoteModels = false;
  tjEnv.allowLocalModels = true;
  // We own persistence (IndexedDB) — don't let transformers double-cache in Cache Storage.
  tjEnv.useBrowserCache = false;
  // pathJoin(localModelPath, modelId, file) → `/vox-models/<pack>/<ver>/<file>`.
  tjEnv.localModelPath = TTS_CONFIG.MODEL_PATH_PREFIX.replace(/\/$/, '');
  const wasm = tjEnv.backends?.onnx?.wasm;
  if (wasm) wasm.wasmPaths = `${TTS_CONFIG.MODEL_PATH_PREFIX}${meta.packId}/${meta.version}/ort/`;
}

/** Pre-seed kokoro-js's voice cache from our verified IndexedDB copies so its
 *  hardcoded HuggingFace fetch is never reached (offline + CSP-clean). */
async function seedVoiceCache(storage: PackStorage, meta: InstallMeta): Promise<void> {
  if (typeof caches === 'undefined') return;
  const cache = await caches.open(VOICE_CACHE);
  for (const voice of meta.voices) {
    const url = `${KOKORO_VOICE_REPO}/${voice.id}.bin`;
    if (await cache.match(url)) continue;
    const blob = await storage.getFile(
      storage.fileKey(meta.packId, meta.version, `voices/${voice.id}.bin`),
    );
    if (blob) await cache.put(url, new Response(blob));
  }
}

/** Load the engine for an installed pack. Resolves once the model is ready to synth. */
export async function loadKokoro(
  storage: PackStorage,
  meta: InstallMeta,
  opts: LoadOptions = {},
): Promise<LoadedEngine> {
  configureEnv(meta);
  await seedVoiceCache(storage, meta);

  // Device+dtype are chosen from what the pack actually ships, never landing on the
  // distorted webgpu+fp16 combination (see chooseBackend). WebGPU needs the fp32 model;
  // a pack that ships only fp16/q8 runs correctly (if slower) on the wasm backend.
  const { device, dtype } = chooseBackend(meta, opts);
  const tts = await KokoroTTS.from_pretrained(`${meta.packId}/${meta.version}`, {
    dtype,
    device,
    progress_callback: opts.onProgress as never,
  });

  const infos: VoiceInfo[] = meta.voices.map((v) => ({
    id: v.id,
    name: v.name,
    lang: v.lang,
    provider: 'vox',
    local: true,
  }));
  const defaultVoice = meta.voices[0]?.id ?? 'af_heart';
  const langs = new Set(meta.languages.map(baseLang));

  // Full eSpeak NG (shipped in the pack) — served SAME-ORIGIN by the SW, so non-English
  // phonemization stays CSP-clean and offline. Only fetched when a non-English voice runs.
  const packRoot = `${TTS_CONFIG.MODEL_PATH_PREFIX}${meta.packId}/${meta.version}/espeak/`;
  const espeakLoaderUrl = `${packRoot}espeak-ng.js`;
  const espeakWasmUrl = `${packRoot}espeak-ng.wasm`;
  // Kokoro's phoneme inventory, to strip glyphs it can't tokenize (best-effort).
  const vocab = extractVocab(tts);

  return {
    device,
    webgpu: device === 'webgpu',
    voices: () => infos,
    supports: (lang: string) => langs.has(baseLang(lang)),
    synth: async (text: string, voiceId: string): Promise<Float32Array> => {
      const voice = infos.some((v) => v.id === voiceId) ? voiceId : defaultVoice;
      // English (a*/b*) stays on kokoro-js's built-in phonemizer. Other languages run
      // through full eSpeak → normalize → feed phonemes straight to the model, skipping
      // kokoro-js's English-only phonemizer.
      const espeakLang = needsEspeak(voice) ? espeakLangFor(voice) : null;
      if (espeakLang) {
        const ipa = await espeakPhonemize(text, espeakLang, espeakLoaderUrl, espeakWasmUrl);
        const phonemes = normalizeForKokoro(ipa, vocab);
        const { input_ids } = tts.tokenizer(phonemes, { truncation: true });
        const audio = await tts.generate_from_ids(input_ids, { voice: voice as never });
        return audio.audio as Float32Array;
      }
      const audio = await tts.generate(text, { voice: voice as never });
      // RawAudio.audio is a Float32Array at RawAudio.sampling_rate (24 kHz for Kokoro).
      return audio.audio as Float32Array;
    },
  };
}

/** Best-effort read of the tokenizer's phoneme vocabulary (the SYMBOL set). Returns
 *  undefined if the internal shape is unrecognized — normalization then skips the
 *  vocab-filter step (safer than filtering against the wrong set).
 *
 *  CRITICAL: kokoro-js's tokenizer exposes `model.vocab` as an ARRAY of token strings
 *  (`["$", ";", "a", "ɑ", …]`), NOT a `{token: id}` map. The old code did
 *  `Object.keys(vocab)`, which on an array yields the INDICES `["0","1","2",…]` — so the
 *  filter kept only digits and STRIPPED EVERY PHONEME, leaving empty input → ~0.3 s of
 *  clipped noise for every non-English (eSpeak) voice. Handle the array shape explicitly. */
function extractVocab(tts: KokoroTTS): Set<string> | undefined {
  try {
    const vocab = (tts as unknown as { tokenizer: { model?: { vocab?: unknown } } })
      .tokenizer.model?.vocab;
    if (Array.isArray(vocab)) {
      const set = new Set<string>();
      for (const entry of vocab) {
        // Either a bare token string, or a `[token, score]` pair (Unigram models).
        const tok = Array.isArray(entry) ? entry[0] : entry;
        if (typeof tok === 'string') set.add(tok);
      }
      return set.size ? set : undefined;
    }
    // Legacy `{token: id}` map — the keys ARE the tokens.
    if (vocab && typeof vocab === 'object') return new Set(Object.keys(vocab as object));
    return undefined;
  } catch {
    return undefined;
  }
}
