// Central TTS tuning. Mostly constants; the one moving part is the PUBLIC_* env
// read that keeps the whole feature dormant when unset.
import { afterEach, describe, expect, it, vi } from 'vitest';

describe('TTS_CONFIG.MANIFEST_URL (build-time env)', () => {
  afterEach(() => {
    vi.unstubAllEnvs();
    vi.resetModules();
  });

  it('uses PUBLIC_VOX_MANIFEST_URL when set', async () => {
    vi.stubEnv('PUBLIC_VOX_MANIFEST_URL', 'https://cdn.test/vox/manifest.json');
    vi.resetModules();
    const { TTS_CONFIG } = await import('./config');
    expect(TTS_CONFIG.MANIFEST_URL).toBe('https://cdn.test/vox/manifest.json');
  });

  it('stays dormant (empty string) when the var is empty', async () => {
    vi.stubEnv('PUBLIC_VOX_MANIFEST_URL', '');
    vi.resetModules();
    const { TTS_CONFIG } = await import('./config');
    expect(TTS_CONFIG.MANIFEST_URL).toBe('');
  });
});

describe('TTS_CONFIG constants', () => {
  it('exposes the spec-mandated tuning values', async () => {
    const { TTS_CONFIG } = await import('./config');
    expect(TTS_CONFIG.QUEUE_CAP).toBe(8);
    expect(TTS_CONFIG.RATE).toBe(1.1);
    expect(TTS_CONFIG.HEALTH_MAX_STRIKES).toBe(3);
    expect(TTS_CONFIG.HEALTH_STARTUP_MS).toBeGreaterThan(0);
    expect(TTS_CONFIG.BENCH_FIRST_AUDIO_MS).toBeGreaterThan(0);
    expect(TTS_CONFIG.BENCH_AVG_SYNTH_MS).toBeGreaterThan(0);
    expect(TTS_CONFIG.BENCH_TIMEOUT_MS).toBeGreaterThan(TTS_CONFIG.BENCH_FIRST_AUDIO_MS);
    expect(TTS_CONFIG.MODEL_PATH_PREFIX).toBe('/vox-models/');
  });

  it('provides per-language preview lines for every shipped language', async () => {
    const { TTS_CONFIG } = await import('./config');
    expect(Object.keys(TTS_CONFIG.SAMPLE_BY_LANG).sort()).toEqual(['en', 'es', 'fr', 'it', 'pt']);
    expect(TTS_CONFIG.SAMPLE_BY_LANG.en).toBe(TTS_CONFIG.SAMPLE_SENTENCE);
    for (const line of Object.values(TTS_CONFIG.SAMPLE_BY_LANG)) {
      expect(line).toContain('VoxTranslate');
    }
  });

  it('namespaces the localStorage keys', async () => {
    const { TTS_CONFIG } = await import('./config');
    expect(TTS_CONFIG.ENGINE_PREF_KEY).toBe('voxtranslate_tts_engine');
    expect(TTS_CONFIG.VOX_VOICE_KEY).toBe('voxtranslate_tts_vox_voice');
    expect(TTS_CONFIG.BROWSER_VOICE_KEY).toBe('voxtranslate_tts_browser_voice');
    expect(TTS_CONFIG.DEV_MODE_KEY).toBe('vox_dev_mode');
  });
});
