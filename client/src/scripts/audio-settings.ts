// Audio Settings controller (Vox Voices) — lazy-loaded the first time the modal opens
// (app.ts owns the overlay show()/focus-trap; this wires the CONTENTS). It renders the
// engine status, drives install/update/remove with live progress + cancellation, runs the
// benchmark, and lets the user pick + preview a voice per engine. Everything degrades
// gracefully: Browser Voice always works, so Vox-specific blocks stay hidden until the
// feature is configured / installed.

import { t } from './i18n';
import { toast } from './toast';
import { TTS_CONFIG } from './tts/config';
import { runBenchmark, benchmarkVerdictKey } from './tts/benchmark';
import { isCancel, VoicePackInstaller } from './tts/installer';
import { type Manifest, compareVersions, selectPack } from './tts/manifest';
import { ttsManager } from './tts/manager';
import {
  loadBrowserVoice,
  loadEnginePref,
  loadVoxVoice,
  saveBrowserVoice,
  saveEnginePref,
  saveVoxVoice,
} from './tts/preferences';
import { activateVoxProvider, deactivateVoxProvider, getVoxProvider } from './tts/register';
import { packStorage, type InstallMeta } from './tts/storage';
import type { EnginePref, VoiceInfo } from './tts/types';

const el = <T extends HTMLElement = HTMLElement>(id: string): T =>
  document.getElementById(id) as T;
const toggle = (id: string, visible: boolean): void => {
  el(id).classList.toggle('hidden', !visible);
};

let inited = false;
let installer: VoicePackInstaller | null = null;
let manifestCache: Manifest | null = null;
let abort: AbortController | null = null;
let installing = false;

/** Human-readable size, e.g. `86.4 MB`. */
function formatBytes(n: number): string {
  if (!n) return '—';
  const mb = n / (1024 * 1024);
  return mb >= 1024 ? `${(mb / 1024).toFixed(1)} GB` : `${mb.toFixed(1)} MB`;
}

function formatDate(ts: number): string {
  try {
    return new Date(ts).toLocaleDateString();
  } catch {
    return '';
  }
}

/** Developer Mode: `?dev=1` (sticky) or the localStorage flag. */
function devModeOn(): boolean {
  try {
    const q = new URLSearchParams(location.search);
    if (q.get('dev') === '1') {
      localStorage.setItem(TTS_CONFIG.DEV_MODE_KEY, '1');
      return true;
    }
    return localStorage.getItem(TTS_CONFIG.DEV_MODE_KEY) === '1';
  } catch {
    return false;
  }
}

async function installedMeta(): Promise<InstallMeta | null> {
  const metas = await packStorage.listMeta().catch(() => [] as InstallMeta[]);
  return metas.find((m) => m.engine === 'kokoro') ?? null;
}

async function loadManifestOnce(): Promise<Manifest | null> {
  if (manifestCache) return manifestCache;
  try {
    manifestCache = await installer!.fetchManifest();
  } catch {
    manifestCache = null;
  }
  return manifestCache;
}

/** Wire the modal's controls once. */
export function initAudioSettings(): void {
  if (inited) return;
  inited = true;
  installer = new VoicePackInstaller(packStorage, TTS_CONFIG.MANIFEST_URL);

  el<HTMLSelectElement>('audio-engine-select').addEventListener('change', (e) => {
    const pref = (e.target as HTMLSelectElement).value as EnginePref;
    saveEnginePref(pref);
    ttsManager.setPreference(pref);
    void refresh();
  });
  el('audio-install-btn').addEventListener('click', () => void doInstall());
  el('audio-update-btn').addEventListener('click', () => void doInstall());
  el('audio-cancel-btn').addEventListener('click', () => abort?.abort());
  el('audio-remove-btn').addEventListener('click', () => void doRemove());
  el('audio-bench-btn').addEventListener('click', () => void doBenchmark());
}

/** Called on every open: reset the cached manifest (re-check for updates) and repaint. */
export async function openAudioSettings(): Promise<void> {
  initAudioSettings();
  manifestCache = null;
  await refresh();
}

async function refresh(): Promise<void> {
  const configured = !!TTS_CONFIG.MANIFEST_URL;
  const meta = await installedMeta();
  const installed = !!meta;
  const bench = meta ? await packStorage.getBench(meta.packId).catch(() => undefined) : undefined;

  // Status block.
  el('audio-current-engine').textContent = currentEngineLabel();
  el('audio-status-val').textContent = statusLabel(configured, installed);
  toggle('audio-version-row', installed);
  if (meta) el('audio-version-val').textContent = meta.version;
  toggle('audio-bench-status-row', installed);
  if (installed) {
    el('audio-bench-val').textContent = bench
      ? `${t(bench.result.passed ? 'audioBenchPassed' : 'audioBenchNeedsRun')} · ${formatDate(bench.ranAt)}`
      : t('audioBenchNeedsRun');
  }

  // Voice pack.
  toggle('audio-pack', configured);
  if (configured) {
    toggle('audio-install-card', !installed && !installing);
    toggle('audio-progress', installing);
    toggle('audio-installed', installed && !installing);
    if (!installed && !installing) await populateInstallCard();
    if (installed && !installing) toggle('audio-update-btn', await updateAvailable(meta!));
  }

  // Benchmark section (installed only).
  toggle('audio-bench-section', installed);
  if (installed) {
    el('audio-bench-verdict').textContent = bench
      ? t(benchmarkVerdictKey(bench.result.passed))
      : t('audioBenchNeedsRun');
  }

  // Engine picker + voice lists.
  populateEngineSelect(installed);
  await renderVoices(installed);

  // Developer mode.
  const dev = devModeOn();
  toggle('audio-dev', dev);
  if (dev) el('audio-dev-info').textContent = devInfo(meta, bench?.result);
}

function currentEngineLabel(): string {
  return ttsManager.chooseProvider('en', false).id === 'vox'
    ? t('audioEngineVox')
    : t('audioEngineBrowser');
}

function statusLabel(configured: boolean, installed: boolean): string {
  // Browser Voice is the permanent fallback and always available; only report
  // "unavailable / not installed" when Vox is actually the chosen engine.
  if (ttsManager.chooseProvider('en', false).id !== 'vox') return t('audioStatusReady');
  if (!configured) return t('audioStatusUnavailable');
  return installed ? t('audioStatusReady') : t('audioStatusNotInstalled');
}

function populateEngineSelect(installed: boolean): void {
  const sel = el<HTMLSelectElement>('audio-engine-select');
  const opts: [EnginePref, string][] = [
    ['auto', t('audioEngineAuto')],
    ['browser', t('audioEngineBrowser')],
    ['vox', t('audioEngineVox')],
  ];
  sel.textContent = '';
  for (const [value, label] of opts) {
    const o = document.createElement('option');
    o.value = value;
    o.textContent = label;
    if (value === 'vox' && !installed) o.disabled = true;
    sel.appendChild(o);
  }
  sel.value = loadEnginePref();
}

async function populateInstallCard(): Promise<void> {
  const manifest = await loadManifestOnce();
  const pack = manifest ? selectPack(manifest, 'kokoro') : null;
  el('audio-install-size').textContent = pack ? formatBytes(pack.totalBytes) : '—';
  el<HTMLButtonElement>('audio-install-btn').disabled = !pack;
}

async function updateAvailable(meta: InstallMeta): Promise<boolean> {
  const manifest = await loadManifestOnce();
  const pack = manifest ? selectPack(manifest, 'kokoro') : null;
  return !!pack && compareVersions(pack.version, meta.version) > 0;
}

function setPackStatus(msg: string): void {
  el('audio-pack-status').textContent = msg;
}

async function doInstall(): Promise<void> {
  const manifest = await loadManifestOnce();
  const pack = manifest ? selectPack(manifest, 'kokoro') : null;
  if (!pack || !installer) {
    setPackStatus(t('voxInstallFailed'));
    return;
  }
  installing = true;
  abort = new AbortController();
  setPackStatus('');
  await refresh(); // reveal the progress UI
  try {
    const meta = await installer.install(pack, {
      signal: abort.signal,
      onProgress: updateProgress,
    });
    installing = false;
    ttsManager.unlock(); // we're inside the install click → a valid gesture for iOS
    await activateVoxProvider(meta);
    setPackStatus('');
    toast(t('voxInstallDone'), 'ok');
    await refresh();
    await doBenchmark(); // auto-benchmark immediately after install (spec)
  } catch (err) {
    installing = false;
    if (isCancel(err)) {
      setPackStatus('');
    } else {
      setPackStatus(t('voxInstallFailed'));
      toast(t('voxInstallFailed'), 'err');
    }
    await refresh();
  }
}

function updateProgress(p: { fraction: number; receivedBytes: number; totalBytes: number }): void {
  const pct = Math.round(p.fraction * 100);
  el('audio-progress-fill').style.width = `${pct}%`;
  el('audio-progress-pct').textContent = `${pct}%`;
  el('audio-progress-bytes').textContent = `${formatBytes(p.receivedBytes)} / ${formatBytes(p.totalBytes)}`;
}

async function doBenchmark(): Promise<void> {
  const provider = getVoxProvider();
  const meta = await installedMeta();
  if (!provider || !meta) return;
  const btn = el<HTMLButtonElement>('audio-bench-btn');
  el('audio-bench-verdict').textContent = t('voxBenchRunning');
  btn.classList.add('btn-loading');
  btn.disabled = true;
  try {
    ttsManager.unlock();
    const result = await runBenchmark(provider);
    await packStorage.putBench({
      packId: meta.packId,
      version: meta.version,
      ranAt: Date.now(),
      result,
    });
    ttsManager.setBenchmarkPassed(result.passed);
  } catch {
    toast(t('voxBenchFailed'), 'err');
  } finally {
    btn.classList.remove('btn-loading');
    btn.disabled = false;
  }
  await refresh();
}

async function doRemove(): Promise<void> {
  const meta = await installedMeta();
  if (!meta || !installer) return;
  await installer.remove(meta.packId).catch(() => {});
  deactivateVoxProvider();
  saveEnginePref('browser'); // spec: removing auto-switches to Browser Voice
  ttsManager.setPreference('browser');
  manifestCache = null;
  toast(t('voxRemoved'), 'ok');
  await refresh();
}

async function renderVoices(installed: boolean): Promise<void> {
  const { browser, vox } = await ttsManager.listVoices();
  // Filter browser voices to show only those matching the user's selected language.
  const langSel = document.getElementById('lang') as HTMLSelectElement | null;
  const lang = langSel?.value;
  const filteredBrowser =
    lang && lang !== 'auto'
      ? browser.filter((v) => v.lang.toLowerCase().startsWith(lang.toLowerCase()))
      : browser;
  toggle('audio-vox-voices', installed && vox.length > 0);
  renderVoiceList(el('audio-vox-list'), vox, 'vox', loadVoxVoice());
  renderVoiceList(el('audio-browser-list'), filteredBrowser, 'browser', loadBrowserVoice());
}

function renderVoiceList(
  container: HTMLElement,
  voices: VoiceInfo[],
  providerId: 'vox' | 'browser',
  selectedId: string | null,
): void {
  container.textContent = '';
  if (!voices.length) {
    const empty = document.createElement('p');
    empty.className = 'audio-voice-empty';
    empty.textContent = t('audioNoVoices');
    container.appendChild(empty);
    return;
  }
  for (const v of voices) {
    const row = document.createElement('div');
    row.className = 'audio-voice-row';

    const pick = document.createElement('button');
    pick.type = 'button';
    pick.className = 'audio-voice-select';
    pick.setAttribute('role', 'radio');
    pick.setAttribute('aria-checked', String(v.id === selectedId));
    pick.textContent = `${v.name} · ${v.lang}`;
    pick.addEventListener('click', () => {
      if (providerId === 'vox') {
        saveVoxVoice(v.id);
        ttsManager.setVoxVoice(v.id);
      } else {
        saveBrowserVoice(v.id);
        ttsManager.setBrowserVoice(v.id);
      }
      container
        .querySelectorAll('.audio-voice-select')
        .forEach((b) => b.setAttribute('aria-checked', 'false'));
      pick.setAttribute('aria-checked', 'true');
    });

    const preview = document.createElement('button');
    preview.type = 'button';
    preview.className = 'audio-voice-preview';
    preview.title = t('audioPreview');
    preview.setAttribute('aria-label', `${t('audioPreview')}: ${v.name}`);
    preview.textContent = '▶';
    preview.addEventListener('click', () => {
      ttsManager.unlock();
      void ttsManager.preview(providerId, TTS_CONFIG.SAMPLE_SENTENCE, v.lang, v.id);
    });

    row.append(pick, preview);
    container.appendChild(row);
  }
}

function devInfo(meta: InstallMeta | null, bench: import('./tts/types').BenchmarkResult | undefined): string {
  const lines = [
    `selected provider: ${ttsManager.chooseProvider('en', false).id}`,
    `preference: ${loadEnginePref()}`,
    `degraded (session): ${ttsManager.isDegraded()}`,
    `pack: ${meta ? `${meta.packId}@${meta.version}` : '—'}`,
  ];
  if (bench) {
    lines.push(
      `benchmark: init=${Math.round(bench.initMs)}ms firstAudio=${Math.round(bench.firstAudioMs)}ms ` +
        `avgSynth=${Math.round(bench.avgSynthMs)}ms rtf=${bench.rtf?.toFixed(2) ?? '—'} ` +
        `webgpu=${bench.webgpu} mem=${bench.deviceMemoryGb ?? '—'}GB passed=${bench.passed}`,
    );
  }
  return lines.join('\n');
}
