// Vox provider lifecycle. The manager / preferences / storage / config modules are
// mocked (each has its own tests); what's under test is the wiring: single provider
// instance, timing hook, boot-time activation, and the dormant/silent-failure paths.
// The KokoroProvider itself is the real class (its engine load stays untouched here).
import { beforeEach, describe, expect, it, vi } from 'vitest';

import type { InstallMeta } from './storage';

const h = vi.hoisted(() => ({
  manager: {
    setPreference: vi.fn(),
    setBrowserVoice: vi.fn(),
    setVoxProvider: vi.fn(),
    setVoxVoice: vi.fn(),
    setBenchmarkPassed: vi.fn(),
    recordVoxTiming: vi.fn(),
  },
  loadEnginePref: vi.fn<() => string>(() => 'auto'),
  loadBrowserVoice: vi.fn<() => string | null>(() => null),
  loadVoxVoice: vi.fn<() => string | null>(() => null),
  packStorage: {
    listMeta: vi.fn<() => Promise<InstallMeta[]>>(async () => []),
    getBench: vi.fn(async (): Promise<unknown> => undefined),
  },
  config: { MANIFEST_URL: '' },
}));

vi.mock('./manager', () => ({ ttsManager: h.manager }));
vi.mock('./preferences', () => ({
  loadEnginePref: h.loadEnginePref,
  loadBrowserVoice: h.loadBrowserVoice,
  loadVoxVoice: h.loadVoxVoice,
}));
vi.mock('./storage', () => ({ packStorage: h.packStorage }));
vi.mock('./config', () => ({ TTS_CONFIG: h.config }));

const kokoroMeta: InstallMeta = {
  packId: 'kokoro-multi',
  version: '1.0.0',
  engine: 'kokoro',
  languages: ['en'],
  voices: [{ id: 'af_heart', name: 'Heart', lang: 'en-US' }],
  files: [],
  totalBytes: 0,
  installedAt: 0,
};

async function freshRegister(): Promise<typeof import('./register')> {
  vi.resetModules(); // clears the module-level voxProvider singleton
  return import('./register');
}

beforeEach(() => {
  vi.clearAllMocks();
  h.loadEnginePref.mockReturnValue('auto');
  h.loadBrowserVoice.mockReturnValue(null);
  h.loadVoxVoice.mockReturnValue(null);
  h.packStorage.listMeta.mockResolvedValue([]);
  h.packStorage.getBench.mockResolvedValue(undefined);
  h.config.MANIFEST_URL = '';
});

describe('activateVoxProvider', () => {
  it('builds ONE provider, registers it, applies the saved voice and wires timing', async () => {
    h.loadVoxVoice.mockReturnValue('af_heart');
    const reg = await freshRegister();
    expect(reg.getVoxProvider()).toBeNull();

    const provider = await reg.activateVoxProvider(kokoroMeta);
    expect(provider.id).toBe('vox');
    expect(reg.getVoxProvider()).toBe(provider);
    expect(h.manager.setVoxProvider).toHaveBeenCalledWith(provider);
    expect(h.manager.setVoxVoice).toHaveBeenCalledWith('af_heart');

    // Per-utterance timing flows into the manager's health monitor.
    provider.onTiming?.(321);
    expect(h.manager.recordVoxTiming).toHaveBeenCalledWith(321);
  });

  it('passes undefined when no Vox voice is saved', async () => {
    const reg = await freshRegister();
    await reg.activateVoxProvider(kokoroMeta);
    expect(h.manager.setVoxVoice).toHaveBeenCalledWith(undefined);
  });
});

describe('deactivateVoxProvider', () => {
  it('stops + clears the provider and resets the benchmark gate', async () => {
    const reg = await freshRegister();
    const provider = await reg.activateVoxProvider(kokoroMeta);
    const stop = vi.spyOn(provider, 'stop');

    reg.deactivateVoxProvider();
    expect(stop).toHaveBeenCalledTimes(1);
    expect(reg.getVoxProvider()).toBeNull();
    expect(h.manager.setVoxProvider).toHaveBeenLastCalledWith(null);
    expect(h.manager.setBenchmarkPassed).toHaveBeenCalledWith(false);
  });

  it('is safe with no active provider', async () => {
    const reg = await freshRegister();
    expect(() => reg.deactivateVoxProvider()).not.toThrow();
    expect(h.manager.setVoxProvider).toHaveBeenCalledWith(null);
  });
});

describe('registerVoxIfInstalled', () => {
  it('always applies engine + browser-voice prefs, even with the feature dormant', async () => {
    h.loadEnginePref.mockReturnValue('browser');
    h.loadBrowserVoice.mockReturnValue('Daniel');
    const reg = await freshRegister();
    await reg.registerVoxIfInstalled();

    expect(h.manager.setPreference).toHaveBeenCalledWith('browser');
    expect(h.manager.setBrowserVoice).toHaveBeenCalledWith('Daniel');
    // No manifest URL → fully dormant, storage never touched.
    expect(h.packStorage.listMeta).not.toHaveBeenCalled();
    expect(h.manager.setVoxProvider).not.toHaveBeenCalled();
  });

  it('maps a missing saved browser voice to undefined', async () => {
    const reg = await freshRegister();
    await reg.registerVoxIfInstalled();
    expect(h.manager.setBrowserVoice).toHaveBeenCalledWith(undefined);
  });

  it('stays on Browser Voice when configured but nothing is installed', async () => {
    h.config.MANIFEST_URL = 'https://cdn.test/manifest.json';
    const reg = await freshRegister();
    await reg.registerVoxIfInstalled();
    expect(h.packStorage.listMeta).toHaveBeenCalledTimes(1);
    expect(h.manager.setVoxProvider).not.toHaveBeenCalled();
  });

  it('activates the installed kokoro pack and applies its stored benchmark verdict', async () => {
    h.config.MANIFEST_URL = 'https://cdn.test/manifest.json';
    h.packStorage.listMeta.mockResolvedValue([
      { ...kokoroMeta, packId: 'other', engine: 'future' },
      kokoroMeta,
    ]);
    h.packStorage.getBench.mockResolvedValue({
      packId: kokoroMeta.packId,
      version: '1.0.0',
      ranAt: 1,
      result: { engine: 'vox', initMs: 1, firstAudioMs: 1, avgSynthMs: 1, webgpu: true, passed: true },
    });

    const reg = await freshRegister();
    await reg.registerVoxIfInstalled();

    expect(reg.getVoxProvider()?.id).toBe('vox');
    expect(h.packStorage.getBench).toHaveBeenCalledWith('kokoro-multi');
    expect(h.manager.setBenchmarkPassed).toHaveBeenCalledWith(true);
  });

  it('a missing benchmark record counts as not passed (Browser default)', async () => {
    h.config.MANIFEST_URL = 'https://cdn.test/manifest.json';
    h.packStorage.listMeta.mockResolvedValue([kokoroMeta]);
    const reg = await freshRegister();
    await reg.registerVoxIfInstalled();
    expect(h.manager.setBenchmarkPassed).toHaveBeenCalledWith(false);
  });

  it('silently stays on Browser Voice when storage is unavailable', async () => {
    h.config.MANIFEST_URL = 'https://cdn.test/manifest.json';
    h.packStorage.listMeta.mockRejectedValue(new Error('idb blocked'));
    const reg = await freshRegister();
    await expect(reg.registerVoxIfInstalled()).resolves.toBeUndefined();
    expect(h.manager.setVoxProvider).not.toHaveBeenCalled();
  });
});
