import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { KokoroProvider } from './kokoro';
import type { BenchRecord, InstallMeta, PackStorage } from '../storage';
import type { LoadedEngine } from '../kokoro-engine';

// The heavy engine + Web Audio playback are mocked: what's under test here is the
// provider's orchestration (lazy-load memoisation, timing hook, voice routing,
// unlock background-warm, stop) — never the real Kokoro/ONNX engine.
const h = vi.hoisted(() => ({
  loadKokoro: vi.fn(),
  play: vi.fn(async () => {}),
  unlock: vi.fn(),
  stop: vi.fn(),
}));

vi.mock('../kokoro-engine', () => ({ loadKokoro: h.loadKokoro }));
vi.mock('../playback', () => ({
  PlaybackService: class {
    play = h.play;
    unlock = h.unlock;
    stop = h.stop;
  },
}));

// Minimal storage stub — supports()/listVoices() never touch it (they read the pack
// metadata), and we don't exercise the engine-loading path here (that needs a browser).
const stubStorage: PackStorage = {
  fileKey: (p, v, path) => `${p}/${v}/${path}`,
  putFile: async () => {},
  getFile: async () => undefined,
  deleteFiles: async () => {},
  putMeta: async () => {},
  getMeta: async () => undefined,
  listMeta: async () => [],
  deleteMeta: async () => {},
  putBench: async (_r: BenchRecord) => {},
  getBench: async () => undefined,
  deleteBench: async () => {},
  estimateUsage: async () => undefined,
};

const meta: InstallMeta = {
  packId: 'kokoro-en',
  version: '1.0.0',
  engine: 'kokoro',
  languages: ['en'],
  voices: [
    { id: 'af_heart', name: 'Heart', lang: 'en-US' },
    { id: 'bm_george', name: 'George', lang: 'en-GB' },
  ],
  files: [],
  totalBytes: 0,
  installedAt: 0,
};

describe('KokoroProvider (metadata-driven, no engine load)', () => {
  it('supports its installed languages by base code, region-insensitive', () => {
    const p = new KokoroProvider(stubStorage, meta);
    expect(p.supports('en')).toBe(true);
    expect(p.supports('en-US')).toBe(true);
    expect(p.supports('EN')).toBe(true);
    expect(p.supports('it')).toBe(false);
    expect(p.supports('es-ES')).toBe(false);
  });

  it('lists the pack voices as provider-tagged local VoiceInfo', async () => {
    const p = new KokoroProvider(stubStorage, meta);
    const vs = await p.listVoices();
    expect(vs.map((v) => v.id)).toEqual(['af_heart', 'bm_george']);
    expect(vs[0]).toEqual({
      id: 'af_heart',
      name: 'Heart',
      lang: 'en-US',
      provider: 'vox',
      local: true,
    });
  });

  it('is available once constructed (it is only registered when a pack is installed)', () => {
    expect(new KokoroProvider(stubStorage, meta).isAvailable()).toBe(true);
    expect(new KokoroProvider(stubStorage, meta).id).toBe('vox');
  });
});

describe('KokoroProvider engine orchestration (mocked engine + playback)', () => {
  const multiMeta: InstallMeta = {
    ...meta,
    languages: ['en', 'it'],
    voices: [
      { id: 'af_heart', name: 'Heart', lang: 'en-US' },
      { id: 'if_sara', name: 'Sara', lang: 'it-IT' },
    ],
  };

  function fakeEngine(): LoadedEngine & { synth: ReturnType<typeof vi.fn> } {
    return {
      device: 'wasm',
      webgpu: false,
      voices: () => [],
      supports: () => true,
      synth: vi.fn(async () => new Float32Array([0.5])),
    };
  }

  let engine: ReturnType<typeof fakeEngine>;

  beforeEach(() => {
    vi.clearAllMocks();
    engine = fakeEngine();
    h.loadKokoro.mockResolvedValue(engine);
    h.play.mockResolvedValue(undefined);
  });

  afterEach(() => {
    vi.restoreAllMocks();
  });

  it('warm() loads the engine exactly once and memoises it', async () => {
    const p = new KokoroProvider(stubStorage, multiMeta);
    const e1 = await p.warm();
    const e2 = await p.warm();
    const e3 = await p.engineHandle();
    expect(e1).toBe(engine);
    expect(e2).toBe(engine);
    expect(e3).toBe(engine);
    expect(h.loadKokoro).toHaveBeenCalledTimes(1);
    expect(h.loadKokoro).toHaveBeenCalledWith(stubStorage, multiMeta, {});
  });

  it('speak() synthesizes, reports timing, then plays the samples', async () => {
    const p = new KokoroProvider(stubStorage, multiMeta);
    const timings: number[] = [];
    p.onTiming = (ms) => timings.push(ms);
    vi.spyOn(performance, 'now').mockReturnValueOnce(1000).mockReturnValueOnce(1150);

    await p.speak('hello', 'en', { voiceId: 'af_heart' });

    expect(engine.synth).toHaveBeenCalledWith('hello', 'af_heart');
    expect(h.play).toHaveBeenCalledWith(new Float32Array([0.5]));
    expect(timings).toEqual([150]);
  });

  it('speak() works without an onTiming hook', async () => {
    const p = new KokoroProvider(stubStorage, multiMeta);
    await p.speak('ciao', 'it');
    expect(h.play).toHaveBeenCalledTimes(1);
  });

  describe('voice-for-language routing', () => {
    async function voiceUsedFor(lang: string, voiceId?: string): Promise<string> {
      const p = new KokoroProvider(stubStorage, multiMeta);
      await p.speak('x', lang, { voiceId });
      return engine.synth.mock.calls[0][1] as string;
    }

    it('uses the matching-language voice when the saved pick speaks another language', async () => {
      // Saved pick is English, but the line is Italian → must not read Italian in English.
      expect(await voiceUsedFor('it', 'af_heart')).toBe('if_sara');
    });

    it('honours the saved pick when it speaks the right language', async () => {
      expect(await voiceUsedFor('it-IT', 'if_sara')).toBe('if_sara');
    });

    it('falls back to the first matching voice when no pick is given', async () => {
      expect(await voiceUsedFor('en')).toBe('af_heart');
    });

    it('keeps the caller pick for an unsupported language rather than mis-reading it', async () => {
      expect(await voiceUsedFor('de', 'some_de_voice')).toBe('some_de_voice');
    });

    it('falls back to the default voice for an unsupported language with no pick', async () => {
      expect(await voiceUsedFor('de')).toBe('af_heart'); // first voice = default
    });
  });

  it('defaults to af_heart when the pack lists no voices', async () => {
    const p = new KokoroProvider(stubStorage, { ...multiMeta, voices: [] });
    await p.speak('x', 'en');
    expect(engine.synth).toHaveBeenCalledWith('x', 'af_heart');
  });

  it('unlock() primes playback and background-warms the engine, swallowing load errors', async () => {
    h.loadKokoro.mockRejectedValueOnce(new Error('load failed'));
    const p = new KokoroProvider(stubStorage, multiMeta);
    expect(() => p.unlock()).not.toThrow();
    expect(h.unlock).toHaveBeenCalledTimes(1);
    await new Promise((r) => setTimeout(r, 0)); // let the background warm settle
    expect(h.loadKokoro).toHaveBeenCalledTimes(1);
  });

  it('stop() stops playback', () => {
    const p = new KokoroProvider(stubStorage, multiMeta);
    p.stop();
    expect(h.stop).toHaveBeenCalledTimes(1);
  });

  it('a failed engine load is not memoised — a later call retries', async () => {
    h.loadKokoro.mockRejectedValueOnce(new Error('cold start failed'));
    const p = new KokoroProvider(stubStorage, multiMeta);
    await expect(p.warm()).rejects.toThrow('cold start failed');

    h.loadKokoro.mockResolvedValueOnce(engine);
    await expect(p.warm()).resolves.toBe(engine); // retried, not stuck on the failure
    expect(h.loadKokoro).toHaveBeenCalledTimes(2);
  });

  it('passes constructor load options through to the engine', async () => {
    const p = new KokoroProvider(stubStorage, multiMeta, { dtype: 'q8', device: 'webgpu' });
    await p.warm();
    expect(h.loadKokoro).toHaveBeenCalledWith(stubStorage, multiMeta, {
      dtype: 'q8',
      device: 'webgpu',
    });
  });
});
