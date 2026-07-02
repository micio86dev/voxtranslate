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

  const device: KokoroDevice = opts.device ?? (webgpuAvailable() ? 'webgpu' : 'wasm');
  const tts = await KokoroTTS.from_pretrained(`${meta.packId}/${meta.version}`, {
    dtype: opts.dtype ?? 'q8',
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

/** Best-effort read of the tokenizer's phoneme vocabulary (symbol set). Returns undefined
 *  if the internal shape changes — normalization then skips the vocab-filter step. */
function extractVocab(tts: KokoroTTS): Set<string> | undefined {
  try {
    const vocab = (tts as unknown as { tokenizer: { model?: { vocab?: Record<string, number> } } })
      .tokenizer.model?.vocab;
    return vocab ? new Set(Object.keys(vocab)) : undefined;
  } catch {
    return undefined;
  }
}
