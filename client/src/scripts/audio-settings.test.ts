// @vitest-environment jsdom
// Audio Settings modal controller (Vox Voices). Every collaborator is mocked
// (i18n/toast/config/benchmark/installer/manager/preferences/register/storage —
// manifest stays real, it's pure); the DOM is a minimal id-for-id skeleton of the
// modal markup. Module state (inited/installing/manifestCache) → fresh import per
// test via vi.resetModules().
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import type { Manifest, ManifestPack } from './tts/manifest';
import type { InstallMeta } from './tts/storage';
import type { BenchmarkResult } from './tts/types';

const h = vi.hoisted(() => ({
  config: {
    MANIFEST_URL: 'https://packs.test/manifest.json',
    DEV_MODE_KEY: 'vox_dev_mode',
    BENCH_TIMEOUT_MS: 50,
    SAMPLE_SENTENCE: 'sample-default',
    SAMPLE_BY_LANG: { en: 'sample-en' } as Record<string, string>,
  },
  manager: {
    setPreference: vi.fn(),
    setVoxVoice: vi.fn(),
    setBrowserVoice: vi.fn(),
    setBenchmarkPassed: vi.fn(),
    unlock: vi.fn(),
    isDegraded: vi.fn(),
    chooseProvider: vi.fn(),
    listVoices: vi.fn(),
    preview: vi.fn(),
  },
  storage: {
    listMeta: vi.fn(),
    getBench: vi.fn(),
    putBench: vi.fn(),
  },
  fetchManifest: vi.fn(),
  install: vi.fn(),
  remove: vi.fn(),
  runBenchmark: vi.fn(),
  getVoxProvider: vi.fn(),
  activateVoxProvider: vi.fn(),
  deactivateVoxProvider: vi.fn(),
  loadEnginePref: vi.fn(),
  loadVoxVoice: vi.fn(),
  loadBrowserVoice: vi.fn(),
  saveEnginePref: vi.fn(),
  saveVoxVoice: vi.fn(),
  saveBrowserVoice: vi.fn(),
  toast: vi.fn(),
}));

vi.mock('./i18n', () => ({ t: (k: string) => k }));
vi.mock('./toast', () => ({ toast: h.toast }));
vi.mock('./tts/config', () => ({ TTS_CONFIG: h.config }));
vi.mock('./tts/benchmark', () => ({
  runBenchmark: h.runBenchmark,
  benchmarkVerdictKey: (passed: boolean) => (passed ? 'verdict-pass' : 'verdict-fail'),
}));
vi.mock('./tts/installer', () => ({
  isCancel: (err: unknown) => err instanceof Error && err.message === 'cancelled',
  VoicePackInstaller: class {
    fetchManifest = h.fetchManifest;
    install = h.install;
    remove = h.remove;
  },
}));
vi.mock('./tts/manager', () => ({ ttsManager: h.manager }));
vi.mock('./tts/preferences', () => ({
  loadBrowserVoice: h.loadBrowserVoice,
  loadEnginePref: h.loadEnginePref,
  loadVoxVoice: h.loadVoxVoice,
  saveBrowserVoice: h.saveBrowserVoice,
  saveEnginePref: h.saveEnginePref,
  saveVoxVoice: h.saveVoxVoice,
}));
vi.mock('./tts/register', () => ({
  activateVoxProvider: h.activateVoxProvider,
  deactivateVoxProvider: h.deactivateVoxProvider,
  getVoxProvider: h.getVoxProvider,
}));
vi.mock('./tts/storage', () => ({ packStorage: h.storage }));

// ---- fixtures ---------------------------------------------------------------

const MB = 1024 * 1024;

function makePack(over: Partial<ManifestPack> = {}): ManifestPack {
  return {
    id: 'kokoro-en',
    engine: 'kokoro',
    version: '1.1.0',
    displayName: 'Kokoro English',
    languages: ['en'],
    totalBytes: 90 * MB,
    baseUrl: 'https://packs.test/kokoro-en/',
    files: [{ path: 'model.onnx', bytes: 90 * MB, sha256: 'a'.repeat(64) }],
    voices: [{ id: 'af_heart', name: 'Heart', lang: 'en-US' }],
    ...over,
  };
}

const manifestWith = (pack: ManifestPack): Manifest => ({ schemaVersion: 1, packs: [pack] });

const META: InstallMeta = {
  packId: 'kokoro-en',
  version: '1.0.0',
  engine: 'kokoro',
  languages: ['en'],
  voices: [{ id: 'af_heart', name: 'Heart', lang: 'en-US' }],
  files: [{ path: 'model.onnx', bytes: 90 * MB, sha256: 'a'.repeat(64) }],
  totalBytes: 90 * MB,
  installedAt: 1750000000000,
};

const PASS_RESULT: BenchmarkResult = {
  engine: 'vox',
  initMs: 120,
  firstAudioMs: 300,
  avgSynthMs: 200,
  rtf: 0.4,
  webgpu: true,
  passed: true,
};

// ---- DOM skeleton + helpers -------------------------------------------------

const el = <T extends HTMLElement = HTMLElement>(id: string): T =>
  document.getElementById(id) as T;
const hiddenEl = (id: string): boolean => el(id).classList.contains('hidden');
const text = (id: string): string => el(id).textContent ?? '';
const click = (id: string): void => {
  el(id).dispatchEvent(new MouseEvent('click'));
};

function buildDom(lang = 'auto'): void {
  document.body.innerHTML = `
    <select id="lang">
      <option value="auto">auto</option><option value="en">en</option><option value="it">it</option>
    </select>
    <span id="audio-current-engine"></span>
    <span id="audio-status-val"></span>
    <div id="audio-version-row"><span id="audio-version-val"></span></div>
    <div id="audio-bench-status-row"><span id="audio-bench-val"></span></div>
    <select id="audio-engine-select"></select>
    <section id="audio-pack">
      <div id="audio-install-card">
        <span id="audio-install-size"></span>
        <button id="audio-install-btn"></button>
      </div>
      <div id="audio-progress">
        <div id="audio-progress-fill"></div>
        <span id="audio-progress-pct"></span>
        <span id="audio-progress-bytes"></span>
        <button id="audio-cancel-btn"></button>
      </div>
      <div id="audio-installed">
        <button id="audio-update-btn"></button>
        <button id="audio-remove-btn"></button>
      </div>
      <p id="audio-pack-status"></p>
    </section>
    <section id="audio-bench-section">
      <span id="audio-bench-verdict"></span>
      <button id="audio-bench-btn"></button>
    </section>
    <div id="audio-vox-voices"><div id="audio-vox-list"></div></div>
    <div id="audio-browser-list"></div>
    <div id="audio-dev"><pre id="audio-dev-info"></pre></div>
  `;
  el<HTMLSelectElement>('lang').value = lang;
}

/** Fresh module instance (module-level inited/installing/manifestCache state). */
async function loadSettings() {
  vi.resetModules();
  return import('./audio-settings');
}

async function openSettings() {
  const mod = await loadSettings();
  await mod.openAudioSettings();
  return mod;
}

const settle = (): Promise<void> => new Promise((r) => setTimeout(r, 25));

beforeEach(() => {
  vi.clearAllMocks();
  localStorage.clear();
  h.config.MANIFEST_URL = 'https://packs.test/manifest.json';
  h.fetchManifest.mockResolvedValue(manifestWith(makePack()));
  h.install.mockResolvedValue(META);
  h.remove.mockResolvedValue(undefined);
  h.storage.listMeta.mockResolvedValue([]);
  h.storage.getBench.mockResolvedValue(undefined);
  h.storage.putBench.mockResolvedValue(undefined);
  h.manager.chooseProvider.mockReturnValue({ id: 'browser' });
  h.manager.listVoices.mockResolvedValue({ browser: [], vox: [] });
  h.manager.isDegraded.mockReturnValue(false);
  h.manager.preview.mockResolvedValue(undefined);
  h.getVoxProvider.mockReturnValue(null);
  h.activateVoxProvider.mockResolvedValue({});
  h.runBenchmark.mockResolvedValue(PASS_RESULT);
  h.loadEnginePref.mockReturnValue('auto');
  h.loadVoxVoice.mockReturnValue(null);
  h.loadBrowserVoice.mockReturnValue(null);
  buildDom();
});

afterEach(() => {
  delete (navigator as unknown as { gpu?: unknown }).gpu;
  window.history.replaceState({}, '', '/');
});

// ---- status / engine picker -------------------------------------------------

describe('openAudioSettings status painting', () => {
  it('not installed + Browser active: ready status, install CTA with the pack size', async () => {
    await openSettings();
    expect(text('audio-current-engine')).toBe('audioEngineBrowser');
    expect(text('audio-status-val')).toBe('audioStatusReady');
    expect(hiddenEl('audio-version-row')).toBe(true);
    expect(hiddenEl('audio-bench-section')).toBe(true);
    expect(hiddenEl('audio-pack')).toBe(false);
    expect(hiddenEl('audio-install-card')).toBe(false);
    expect(hiddenEl('audio-installed')).toBe(true);
    expect(text('audio-install-size')).toBe('90.0 MB');
    expect(el<HTMLButtonElement>('audio-install-btn').disabled).toBe(false);

    const opts = [...el<HTMLSelectElement>('audio-engine-select').options];
    expect(opts.map((o) => o.value)).toEqual(['auto', 'browser', 'vox']);
    expect(opts[2].disabled).toBe(true); // vox not installed → unpickable
    expect(el<HTMLSelectElement>('audio-engine-select').value).toBe('auto');
  });

  it('Vox disabled (no manifest): engine picker suppressed + pref pinned to browser, pack hidden', async () => {
    h.config.MANIFEST_URL = '';
    h.loadEnginePref.mockReturnValue('auto'); // a stale non-browser pref
    await openSettings();
    expect(hiddenEl('audio-pack')).toBe(true);
    // No engine to choose → the picker is left empty and the preference pinned to browser.
    expect(el<HTMLSelectElement>('audio-engine-select').options.length).toBe(0);
    expect(h.saveEnginePref).toHaveBeenCalledWith('browser');
    expect(h.manager.setPreference).toHaveBeenCalledWith('browser');
  });

  it('configured but not installed + vox chosen: "not installed" status', async () => {
    h.manager.chooseProvider.mockReturnValue({ id: 'vox' });
    await openSettings();
    expect(text('audio-status-val')).toBe('audioStatusNotInstalled');
  });

  it('installed: version + bench rows, update button only when the manifest is newer', async () => {
    h.storage.listMeta.mockResolvedValue([META]);
    h.storage.getBench.mockResolvedValue({
      packId: META.packId,
      version: META.version,
      ranAt: 1750000000000,
      result: PASS_RESULT,
    });
    h.manager.chooseProvider.mockReturnValue({ id: 'vox' });
    const mod = await openSettings();

    expect(text('audio-status-val')).toBe('audioStatusReady');
    expect(hiddenEl('audio-version-row')).toBe(false);
    expect(text('audio-version-val')).toBe('1.0.0');
    expect(hiddenEl('audio-bench-status-row')).toBe(false);
    expect(text('audio-bench-val')).toContain('audioBenchPassed');
    expect(text('audio-bench-verdict')).toBe('verdict-pass');
    expect(hiddenEl('audio-install-card')).toBe(true);
    expect(hiddenEl('audio-installed')).toBe(false);
    expect(hiddenEl('audio-update-btn')).toBe(false); // manifest 1.1.0 > installed 1.0.0
    const voxOpt = el<HTMLSelectElement>('audio-engine-select').options[2];
    expect(voxOpt.disabled).toBe(false);

    // Same manifest version → the update CTA disappears on reopen.
    h.fetchManifest.mockResolvedValue(manifestWith(makePack({ version: '1.0.0' })));
    await mod.openAudioSettings();
    expect(hiddenEl('audio-update-btn')).toBe(true);
  });

  it('shows "needs run" when no benchmark is stored yet', async () => {
    h.storage.listMeta.mockResolvedValue([META]);
    await openSettings();
    expect(text('audio-bench-val')).toBe('audioBenchNeedsRun');
    expect(text('audio-bench-verdict')).toBe('audioBenchNeedsRun');
  });

  it('formats GB-sized and zero-sized packs', async () => {
    h.fetchManifest.mockResolvedValue(
      manifestWith(makePack({ totalBytes: 2.5 * 1024 * MB })),
    );
    const mod = await openSettings();
    expect(text('audio-install-size')).toBe('2.5 GB');

    h.fetchManifest.mockResolvedValue(manifestWith(makePack({ totalBytes: 0 })));
    await mod.openAudioSettings(); // reopen re-fetches the manifest
    expect(text('audio-install-size')).toBe('—');
  });
});

describe('language gating', () => {
  it('hides the pack section when no pack serves the selected output language', async () => {
    buildDom('it'); // packs are English-only
    await openSettings();
    expect(hiddenEl('audio-pack')).toBe(true);
  });

  it('keeps an installed pack manageable even for an uncovered language', async () => {
    buildDom('it');
    h.storage.listMeta.mockResolvedValue([META]);
    await openSettings();
    expect(hiddenEl('audio-pack')).toBe(false);
    expect(hiddenEl('audio-install-card')).toBe(true); // …but no install CTA
  });

  it('fails open when the manifest cannot be loaded', async () => {
    buildDom('it');
    h.fetchManifest.mockRejectedValue(new Error('offline'));
    await openSettings();
    expect(hiddenEl('audio-pack')).toBe(false); // never hide on a fetch hiccup
    expect(text('audio-install-size')).toBe('—');
    expect(el<HTMLButtonElement>('audio-install-btn').disabled).toBe(true);
  });
});

describe('engine preference', () => {
  it('persists a change and pushes it to the manager (wired exactly once)', async () => {
    const mod = await openSettings();
    await mod.openAudioSettings(); // second open must not double-bind listeners
    const sel = el<HTMLSelectElement>('audio-engine-select');
    sel.value = 'browser';
    sel.dispatchEvent(new Event('change'));
    await settle();
    expect(h.saveEnginePref).toHaveBeenCalledTimes(1);
    expect(h.saveEnginePref).toHaveBeenCalledWith('browser');
    expect(h.manager.setPreference).toHaveBeenCalledWith('browser');
  });
});

// ---- install / cancel / remove ----------------------------------------------

describe('install flow', () => {
  it('installs, reports progress, activates the provider, and toasts success', async () => {
    h.install.mockImplementation(
      async (
        _pack: ManifestPack,
        opts: {
          signal?: AbortSignal;
          onProgress?: (p: { fraction: number; receivedBytes: number; totalBytes: number }) => void;
        },
      ) => {
        opts.onProgress?.({ fraction: 0.42, receivedBytes: 42 * MB, totalBytes: 90 * MB });
        h.storage.listMeta.mockResolvedValue([META]); // the pack is now installed
        return META;
      },
    );
    await openSettings();
    click('audio-install-btn');
    await vi.waitFor(() => expect(h.activateVoxProvider).toHaveBeenCalledWith(META));

    expect(el('audio-progress-fill').style.width).toBe('42%');
    expect(text('audio-progress-pct')).toBe('42%');
    expect(text('audio-progress-bytes')).toBe('42.0 MB / 90.0 MB');
    expect(h.manager.unlock).toHaveBeenCalled(); // install click = the iOS gesture
    expect(h.toast).toHaveBeenCalledWith('voxInstallDone', 'ok');
    await vi.waitFor(() => expect(hiddenEl('audio-installed')).toBe(false));
    expect(text('audio-pack-status')).toBe('');
  });

  it('surfaces a failed install and repaints the CTA', async () => {
    h.install.mockRejectedValue(new Error('quota exceeded'));
    await openSettings();
    click('audio-install-btn');
    await vi.waitFor(() => expect(text('audio-pack-status')).toBe('voxInstallFailed'));
    expect(h.toast).toHaveBeenCalledWith('voxInstallFailed', 'err');
    expect(h.activateVoxProvider).not.toHaveBeenCalled();
    await vi.waitFor(() => expect(hiddenEl('audio-install-card')).toBe(false));
  });

  it('cancel aborts quietly: no failure message, no toast, CTA restored', async () => {
    h.install.mockImplementation(
      (_pack: ManifestPack, opts: { signal?: AbortSignal }) =>
        new Promise((_res, reject) => {
          opts.signal?.addEventListener('abort', () => reject(new Error('cancelled')));
        }),
    );
    await openSettings();
    click('audio-install-btn');
    await vi.waitFor(() => expect(hiddenEl('audio-progress')).toBe(false));
    click('audio-cancel-btn');
    await vi.waitFor(() => expect(hiddenEl('audio-install-card')).toBe(false));
    expect(text('audio-pack-status')).toBe('');
    expect(h.toast).not.toHaveBeenCalled();
  });

  it('reports failure without attempting a download when no pack is obtainable', async () => {
    h.fetchManifest.mockRejectedValue(new Error('404'));
    await openSettings();
    click('audio-install-btn');
    await vi.waitFor(() => expect(text('audio-pack-status')).toBe('voxInstallFailed'));
    expect(h.install).not.toHaveBeenCalled();
  });
});

describe('remove flow', () => {
  it('removes the pack and auto-switches to Browser Voice (spec)', async () => {
    h.storage.listMeta.mockResolvedValue([META]);
    await openSettings();
    click('audio-remove-btn');
    await vi.waitFor(() => expect(h.remove).toHaveBeenCalledWith('kokoro-en'));
    expect(h.deactivateVoxProvider).toHaveBeenCalled();
    expect(h.saveEnginePref).toHaveBeenCalledWith('browser');
    expect(h.manager.setPreference).toHaveBeenCalledWith('browser');
    expect(h.toast).toHaveBeenCalledWith('voxRemoved', 'ok');
  });

  it('is a no-op with nothing installed', async () => {
    await openSettings();
    click('audio-remove-btn');
    await settle();
    expect(h.remove).not.toHaveBeenCalled();
    expect(h.deactivateVoxProvider).not.toHaveBeenCalled();
  });
});

// ---- benchmark ----------------------------------------------------------------

describe('benchmark', () => {
  const provider = {};
  beforeEach(() => {
    h.storage.listMeta.mockResolvedValue([META]);
    h.getVoxProvider.mockReturnValue(provider);
  });

  it('without WebGPU: records an instant failed verdict, never loads the model', async () => {
    await openSettings();
    click('audio-bench-btn');
    await vi.waitFor(() => expect(h.manager.setBenchmarkPassed).toHaveBeenCalledWith(false));
    expect(h.runBenchmark).not.toHaveBeenCalled();
    const rec = h.storage.putBench.mock.calls[0][0] as {
      packId: string;
      result: BenchmarkResult;
    };
    expect(rec.packId).toBe('kokoro-en');
    expect(rec.result.passed).toBe(false);
    expect(rec.result.webgpu).toBe(false); // the REAL capability, not hardcoded
  });

  it('with WebGPU: runs, stores the result, and applies the verdict', async () => {
    Object.defineProperty(navigator, 'gpu', { value: {}, configurable: true });
    let finish!: (r: BenchmarkResult) => void;
    h.runBenchmark.mockImplementation(
      () => new Promise<BenchmarkResult>((res) => (finish = res)),
    );
    await openSettings();
    click('audio-bench-btn');
    await vi.waitFor(() => expect(text('audio-bench-verdict')).toBe('voxBenchRunning'));
    expect(el<HTMLButtonElement>('audio-bench-btn').disabled).toBe(true);
    finish(PASS_RESULT);
    await vi.waitFor(() => expect(h.manager.setBenchmarkPassed).toHaveBeenCalledWith(true));
    expect(h.runBenchmark).toHaveBeenCalledWith(provider);
    const rec = h.storage.putBench.mock.calls[0][0] as { result: BenchmarkResult };
    expect(rec.result).toEqual(PASS_RESULT);
    const btn = el<HTMLButtonElement>('audio-bench-btn');
    expect(btn.disabled).toBe(false);
    expect(btn.classList.contains('btn-loading')).toBe(false);
  });

  it('a crashed/timed-out run falls back to Browser Voice and is remembered', async () => {
    Object.defineProperty(navigator, 'gpu', { value: {}, configurable: true });
    h.runBenchmark.mockRejectedValue(new Error('engine stalled'));
    await openSettings();
    click('audio-bench-btn');
    await vi.waitFor(() => expect(h.manager.setBenchmarkPassed).toHaveBeenCalledWith(false));
    const rec = h.storage.putBench.mock.calls[0][0] as { result: BenchmarkResult };
    expect(rec.result.passed).toBe(false);
    expect(rec.result.webgpu).toBe(true);
  });

  it('does nothing without an active provider', async () => {
    h.getVoxProvider.mockReturnValue(null);
    await openSettings();
    click('audio-bench-btn');
    await settle();
    expect(h.storage.putBench).not.toHaveBeenCalled();
    expect(h.manager.setBenchmarkPassed).not.toHaveBeenCalled();
  });
});

// ---- voice lists ----------------------------------------------------------------

describe('voice lists', () => {
  const VOICES = {
    browser: [
      { id: 'b-en', name: 'Browser EN', lang: 'en-US', provider: 'browser' },
      { id: 'b-it', name: 'Browser IT', lang: 'it-IT', provider: 'browser' },
    ],
    vox: [{ id: 'v-en', name: 'Vox EN', lang: 'en-US', provider: 'vox', local: true }],
  };

  it('filters to the selected output language and marks the saved voice', async () => {
    buildDom('en');
    h.storage.listMeta.mockResolvedValue([META]);
    h.manager.listVoices.mockResolvedValue(VOICES);
    h.loadVoxVoice.mockReturnValue('v-en');
    await openSettings();

    expect(hiddenEl('audio-vox-voices')).toBe(false);
    const voxPicks = el('audio-vox-list').querySelectorAll('.audio-voice-select');
    const browserPicks = el('audio-browser-list').querySelectorAll('.audio-voice-select');
    expect(voxPicks).toHaveLength(1);
    expect(browserPicks).toHaveLength(1); // the it-IT voice is filtered out
    expect(voxPicks[0].getAttribute('aria-checked')).toBe('true');
    expect(el('audio-browser-list').textContent).toContain('Browser EN');

    // Picking a vox voice saves + applies it and moves the radio state.
    (voxPicks[0] as HTMLElement).click();
    expect(h.saveVoxVoice).toHaveBeenCalledWith('v-en');
    expect(h.manager.setVoxVoice).toHaveBeenCalledWith('v-en');
    expect(voxPicks[0].getAttribute('aria-checked')).toBe('true');

    // Preview speaks the per-language sample through the owning provider.
    (el('audio-vox-list').querySelector('.audio-voice-preview') as HTMLElement).click();
    expect(h.manager.unlock).toHaveBeenCalled();
    expect(h.manager.preview).toHaveBeenCalledWith('vox', 'sample-en', 'en-US', 'v-en');
  });

  it('shows every voice on "auto" and falls back to the default sample line', async () => {
    h.manager.listVoices.mockResolvedValue(VOICES);
    await openSettings();

    expect(hiddenEl('audio-vox-voices')).toBe(true); // not installed
    const browserPicks = el('audio-browser-list').querySelectorAll('.audio-voice-select');
    expect(browserPicks).toHaveLength(2);

    (browserPicks[1] as HTMLElement).click(); // pick the Italian browser voice
    expect(h.saveBrowserVoice).toHaveBeenCalledWith('b-it');
    expect(h.manager.setBrowserVoice).toHaveBeenCalledWith('b-it');
    expect(browserPicks[1].getAttribute('aria-checked')).toBe('true');
    expect(browserPicks[0].getAttribute('aria-checked')).toBe('false');

    const previews = el('audio-browser-list').querySelectorAll('.audio-voice-preview');
    (previews[1] as HTMLElement).click(); // no it sample configured → default line
    expect(h.manager.preview).toHaveBeenCalledWith('browser', 'sample-default', 'it-IT', 'b-it');
  });

  it('pre-selects (and persists) the Google browser voice for the chosen language', async () => {
    buildDom('it');
    h.manager.listVoices.mockResolvedValue({
      browser: [
        { id: 'g-it', name: 'Google italiano', lang: 'it-IT', provider: 'browser' },
        { id: 'l-it', name: 'Alice', lang: 'it-IT', provider: 'browser', local: true },
      ],
      vox: [],
    });
    h.loadBrowserVoice.mockReturnValue(null); // no saved pick yet
    await openSettings();

    // Standard tier with no saved pick → default to the natural Google voice, persisted
    // and pushed to the manager so playback uses it, and shown as the selected radio.
    expect(h.saveBrowserVoice).toHaveBeenCalledWith('g-it');
    expect(h.manager.setBrowserVoice).toHaveBeenCalledWith('g-it');
    const picks = el('audio-browser-list').querySelectorAll('.audio-voice-select');
    const checked = Array.from(picks).find((p) => p.getAttribute('aria-checked') === 'true');
    expect(checked?.textContent).toContain('Google italiano');
  });

  it('honors a saved browser voice that still serves the language (no re-persist)', async () => {
    buildDom('it');
    h.manager.listVoices.mockResolvedValue({
      browser: [
        { id: 'g-it', name: 'Google italiano', lang: 'it-IT', provider: 'browser' },
        { id: 'l-it', name: 'Alice', lang: 'it-IT', provider: 'browser', local: true },
      ],
      vox: [],
    });
    h.loadBrowserVoice.mockReturnValue('l-it'); // user previously chose Alice
    await openSettings();

    expect(h.saveBrowserVoice).not.toHaveBeenCalled();
    const picks = el('audio-browser-list').querySelectorAll('.audio-voice-select');
    const checked = Array.from(picks).find((p) => p.getAttribute('aria-checked') === 'true');
    expect(checked?.textContent).toContain('Alice');
  });

  it('renders an empty-state line when a provider has no matching voices', async () => {
    await openSettings();
    expect(text('audio-browser-list')).toBe('audioNoVoices');
    expect(text('audio-vox-list')).toBe('audioNoVoices');
  });
});

// ---- developer mode --------------------------------------------------------------

describe('developer mode', () => {
  it('stays hidden by default', async () => {
    await openSettings();
    expect(hiddenEl('audio-dev')).toBe(true);
  });

  it('the sticky localStorage flag reveals the diagnostics block', async () => {
    localStorage.setItem('vox_dev_mode', '1');
    h.storage.listMeta.mockResolvedValue([META]);
    h.storage.getBench.mockResolvedValue({
      packId: META.packId,
      version: META.version,
      ranAt: 1750000000000,
      result: PASS_RESULT,
    });
    await openSettings();
    expect(hiddenEl('audio-dev')).toBe(false);
    const info = text('audio-dev-info');
    expect(info).toContain('selected provider: browser');
    expect(info).toContain('preference: auto');
    expect(info).toContain('pack: kokoro-en@1.0.0');
    expect(info).toContain('rtf=0.40');
    expect(info).toContain('passed=true');
  });

  it('?dev=1 turns dev mode on and makes it sticky', async () => {
    window.history.replaceState({}, '', '/?dev=1');
    await openSettings();
    expect(hiddenEl('audio-dev')).toBe(false);
    expect(localStorage.getItem('vox_dev_mode')).toBe('1');
    expect(text('audio-dev-info')).toContain('pack: —'); // nothing installed
  });
});
