import { describe, expect, it } from 'vitest';

import { KokoroProvider } from './kokoro';
import type { BenchRecord, InstallMeta, PackStorage } from '../storage';

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
