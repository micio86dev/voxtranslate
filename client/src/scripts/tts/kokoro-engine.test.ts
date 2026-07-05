// The heavy engine chunk, tested with kokoro-js / transformers.js fully mocked:
// what matters here is OUR glue — same-origin env configuration, voice-cache
// seeding from IndexedDB, device pick, and the English vs eSpeak synth routing.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { TTS_CONFIG } from './config';
import type { InstallMeta, PackStorage } from './storage';

const h = vi.hoisted(() => ({
  from_pretrained: vi.fn(),
  // Mutable stand-in for transformers.js `env` (reset per test).
  tjEnv: {} as Record<string, unknown>,
}));

vi.mock('kokoro-js', () => ({ KokoroTTS: { from_pretrained: h.from_pretrained } }));
vi.mock('@huggingface/transformers', () => ({ env: h.tjEnv }));
vi.mock('./espeak-phonemizer', () => ({
  needsEspeak: vi.fn((id: string) => id[0] !== 'a' && id[0] !== 'b'),
  espeakLangFor: vi.fn((id: string) => (id[0] === 'i' ? 'it' : null)),
  espeakPhonemize: vi.fn(async () => ' raw ipa '),
  normalizeForKokoro: vi.fn(() => 'normalized'),
}));

import { loadKokoro } from './kokoro-engine';
import * as espeak from './espeak-phonemizer';

interface FakeTts {
  tokenizer: unknown;
  generate: ReturnType<typeof vi.fn>;
  generate_from_ids: ReturnType<typeof vi.fn>;
}

function makeTts(vocab?: Record<string, number> | unknown[]): FakeTts {
  const tokenizer = Object.assign(vi.fn(() => ({ input_ids: [7, 8, 9] })), {
    model: vocab ? { vocab } : {},
  });
  return {
    tokenizer,
    generate: vi.fn(async () => ({ audio: new Float32Array([0.1, 0.2]) })),
    generate_from_ids: vi.fn(async () => ({ audio: new Float32Array([0.9]) })),
  };
}

function makeStorage(files: Record<string, Blob> = {}): PackStorage {
  return {
    fileKey: (p, v, path) => `${p}/${v}/${path}`,
    putFile: async () => {},
    getFile: vi.fn(async (key: string) => files[key]),
    deleteFiles: async () => {},
    putMeta: async () => {},
    getMeta: async () => undefined,
    listMeta: async () => [],
    deleteMeta: async () => {},
    putBench: async () => {},
    getBench: async () => undefined,
    deleteBench: async () => {},
    estimateUsage: async () => undefined,
  };
}

const meta: InstallMeta = {
  packId: 'kokoro-multi',
  version: '2.0.0',
  engine: 'kokoro',
  languages: ['en', 'it'],
  voices: [
    { id: 'af_heart', name: 'Heart', lang: 'en-US' },
    { id: 'if_sara', name: 'Sara', lang: 'it-IT' },
  ],
  files: [],
  totalBytes: 0,
  installedAt: 0,
};

describe('loadKokoro', () => {
  let tts: FakeTts;

  beforeEach(() => {
    tts = makeTts({ n: 0 });
    h.from_pretrained.mockReset().mockResolvedValue(tts);
    // Fresh transformers env each test (the module mutates it in place).
    for (const k of Object.keys(h.tjEnv)) delete h.tjEnv[k];
    h.tjEnv.backends = { onnx: { wasm: {} } };
    // No CacheStorage by default → seedVoiceCache's early return.
    vi.stubGlobal('caches', undefined);
    vi.stubGlobal('navigator', {}); // no WebGPU by default
    vi.mocked(espeak.needsEspeak).mockClear();
    vi.mocked(espeak.espeakPhonemize).mockClear();
    vi.mocked(espeak.normalizeForKokoro).mockClear();
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it('configures transformers.js for same-origin, SW-served loading', async () => {
    await loadKokoro(makeStorage(), meta);

    expect(h.tjEnv.allowRemoteModels).toBe(false);
    expect(h.tjEnv.allowLocalModels).toBe(true);
    expect(h.tjEnv.useBrowserCache).toBe(false);
    expect(h.tjEnv.localModelPath).toBe('/vox-models'); // trailing slash stripped
    const wasm = (h.tjEnv.backends as { onnx: { wasm: { wasmPaths?: string } } }).onnx.wasm;
    expect(wasm.wasmPaths).toBe('/vox-models/kokoro-multi/2.0.0/ort/');
  });

  it('tolerates a transformers build without the onnx wasm backend', async () => {
    h.tjEnv.backends = {};
    await expect(loadKokoro(makeStorage(), meta)).resolves.toBeTruthy();
  });

  it('loads a legacy (fp16-only) pack on wasm and forwards progress + overrides', async () => {
    const onProgress = (): void => {};
    // `meta.files` is empty (legacy install) → assume the old fp16-only build, which must
    // run on wasm (fp16-on-WebGPU distorts Kokoro), NOT webgpu.
    await loadKokoro(makeStorage(), meta, { onProgress });
    expect(h.from_pretrained).toHaveBeenCalledWith('kokoro-multi/2.0.0', {
      dtype: 'fp16',
      device: 'wasm',
      progress_callback: onProgress,
    });

    // A fully-specified override still wins verbatim (benchmark/tests).
    await loadKokoro(makeStorage(), meta, { dtype: 'q8', device: 'webgpu' });
    expect(h.from_pretrained).toHaveBeenLastCalledWith('kokoro-multi/2.0.0', {
      dtype: 'q8',
      device: 'webgpu',
      progress_callback: undefined,
    });
  });

  it('NEVER runs fp16 on WebGPU: an fp16-only pack stays on wasm even with navigator.gpu', async () => {
    const fp16Pack: InstallMeta = { ...meta, files: [{ path: 'onnx/model_fp16.onnx', bytes: 1, sha256: 'x' }] };
    expect((await loadKokoro(makeStorage(), fp16Pack)).webgpu).toBe(false);

    vi.stubGlobal('navigator', { gpu: {} }); // WebGPU present…
    const engine = await loadKokoro(makeStorage(), fp16Pack);
    expect(engine.device).toBe('wasm'); // …but fp16-only → wasm to avoid distortion
    expect(engine.webgpu).toBe(false);
    expect(h.from_pretrained).toHaveBeenLastCalledWith('kokoro-multi/2.0.0', {
      dtype: 'fp16',
      device: 'wasm',
      progress_callback: undefined,
    });
  });

  it('uses WebGPU + fp32 when the pack ships the fp32 model and navigator.gpu is present', async () => {
    const fp32Pack: InstallMeta = {
      ...meta,
      files: [
        { path: 'onnx/model.onnx', bytes: 1, sha256: 'a' },
        { path: 'onnx/model_quantized.onnx', bytes: 1, sha256: 'b' },
      ],
    };
    // No WebGPU → the q8 model on wasm (fast + correct on CPU).
    expect((await loadKokoro(makeStorage(), fp32Pack)).device).toBe('wasm');
    expect(h.from_pretrained).toHaveBeenLastCalledWith('kokoro-multi/2.0.0', {
      dtype: 'q8',
      device: 'wasm',
      progress_callback: undefined,
    });

    vi.stubGlobal('navigator', { gpu: {} });
    const engine = await loadKokoro(makeStorage(), fp32Pack);
    expect(engine.device).toBe('webgpu');
    expect(engine.webgpu).toBe(true);
    expect(h.from_pretrained).toHaveBeenLastCalledWith('kokoro-multi/2.0.0', {
      dtype: 'fp32',
      device: 'webgpu',
      progress_callback: undefined,
    });
  });

  it('exposes the pack voices and base-language capability', async () => {
    const engine = await loadKokoro(makeStorage(), meta);
    expect(engine.voices()).toEqual([
      { id: 'af_heart', name: 'Heart', lang: 'en-US', provider: 'vox', local: true },
      { id: 'if_sara', name: 'Sara', lang: 'it-IT', provider: 'vox', local: true },
    ]);
    expect(engine.supports('en')).toBe(true);
    expect(engine.supports('IT-it')).toBe(true);
    expect(engine.supports('fr')).toBe(false);
  });

  describe('voice cache seeding', () => {
    it('skips entirely when CacheStorage is unavailable (SSR/tests)', async () => {
      const storage = makeStorage();
      await loadKokoro(storage, meta);
      expect(storage.getFile).not.toHaveBeenCalled();
    });

    it('seeds only missing voices from IndexedDB, skipping stored-blob misses', async () => {
      const heartBlob = new Blob(['heart-bytes']);
      const storage = makeStorage({ 'kokoro-multi/2.0.0/voices/af_heart.bin': heartBlob });
      const put = vi.fn();
      const match = vi.fn(async (url: string) => (url.includes('if_sara') ? new Response('') : undefined));
      vi.stubGlobal('caches', { open: vi.fn(async () => ({ match, put })) });

      await loadKokoro(storage, meta);

      // if_sara already cached → untouched; af_heart seeded; no other puts.
      expect(put).toHaveBeenCalledTimes(1);
      const [url, resp] = put.mock.calls[0] as [string, Response];
      expect(url).toBe(
        'https://huggingface.co/onnx-community/Kokoro-82M-v1.0-ONNX/resolve/main/voices/af_heart.bin',
      );
      expect(await resp.text()).toBe('heart-bytes');
    });

    it('leaves a voice unseeded when its blob is missing from storage', async () => {
      const put = vi.fn();
      vi.stubGlobal('caches', {
        open: vi.fn(async () => ({ match: vi.fn(async () => undefined), put })),
      });
      await loadKokoro(makeStorage(), meta); // storage has no blobs
      expect(put).not.toHaveBeenCalled();
    });
  });

  describe('synth routing', () => {
    it('keeps English voices on the built-in kokoro-js path', async () => {
      const engine = await loadKokoro(makeStorage(), meta);
      const audio = await engine.synth('hello', 'af_heart');
      expect(tts.generate).toHaveBeenCalledWith('hello', { voice: 'af_heart' });
      expect(audio).toEqual(new Float32Array([0.1, 0.2]));
      expect(espeak.espeakPhonemize).not.toHaveBeenCalled();
    });

    it('falls back to the default (first) voice for an unknown voice id', async () => {
      const engine = await loadKokoro(makeStorage(), meta);
      await engine.synth('hello', 'zz_nope');
      expect(tts.generate).toHaveBeenCalledWith('hello', { voice: 'af_heart' });
    });

    it('routes non-English voices through eSpeak → normalize → generate_from_ids', async () => {
      const engine = await loadKokoro(makeStorage(), meta);
      const audio = await engine.synth('ciao', 'if_sara');

      expect(espeak.espeakPhonemize).toHaveBeenCalledWith(
        'ciao',
        'it',
        `${TTS_CONFIG.MODEL_PATH_PREFIX}kokoro-multi/2.0.0/espeak/espeak-ng.js`,
        `${TTS_CONFIG.MODEL_PATH_PREFIX}kokoro-multi/2.0.0/espeak/espeak-ng.wasm`,
      );
      // Vocab extracted from the tokenizer feeds normalization.
      expect(espeak.normalizeForKokoro).toHaveBeenCalledWith(' raw ipa ', new Set(['n']));
      expect(tts.tokenizer).toHaveBeenCalledWith('normalized', { truncation: true });
      expect(tts.generate_from_ids).toHaveBeenCalledWith([7, 8, 9], { voice: 'if_sara' });
      expect(audio).toEqual(new Float32Array([0.9]));
    });

    it('extracts phonemes from the tokenizer ARRAY vocab, not its indices (the eSpeak fix)', async () => {
      // kokoro-js exposes `model.vocab` as an ARRAY of token strings. The old code did
      // Object.keys() → ["0","1",…] (indices), so normalizeForKokoro stripped every
      // phoneme → empty input → ~0.3s of clipped noise for non-English voices. Assert the
      // real phoneme chars now reach normalizeForKokoro.
      h.from_pretrained.mockResolvedValue(makeTts(['a', 'ɾ', 'ʃ', 'n']));
      const engine = await loadKokoro(makeStorage(), meta);
      await engine.synth('ciao', 'if_sara');
      expect(espeak.normalizeForKokoro).toHaveBeenCalledWith(' raw ipa ', new Set(['a', 'ɾ', 'ʃ', 'n']));
    });

    it('passes an undefined vocab when the tokenizer exposes none', async () => {
      h.from_pretrained.mockResolvedValue(makeTts(undefined));
      const engine = await loadKokoro(makeStorage(), meta);
      await engine.synth('ciao', 'if_sara');
      expect(espeak.normalizeForKokoro).toHaveBeenCalledWith(' raw ipa ', undefined);
    });

    it('survives a tokenizer whose internals cannot be read (vocab extraction throws)', async () => {
      const broken = makeTts();
      (broken as { tokenizer: unknown }).tokenizer = undefined; // .model access throws
      h.from_pretrained.mockResolvedValue(broken);
      const engine = await loadKokoro(makeStorage(), meta);
      await engine.synth('hello', 'af_heart'); // English path never touches the tokenizer
      expect(broken.generate).toHaveBeenCalled();
    });
  });

  it('defaults to af_heart when the pack lists no voices', async () => {
    const empty: InstallMeta = { ...meta, voices: [] };
    const engine = await loadKokoro(makeStorage(), empty);
    await engine.synth('hello', 'anything');
    expect(tts.generate).toHaveBeenCalledWith('hello', { voice: 'af_heart' });
  });
});
