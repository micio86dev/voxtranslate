// Global connection-status banner (YouTube-style). A small pill slides up from
// the bottom: RED when the browser is offline, AMBER when the realtime transport
// (signaling WebSocket) is dropping/reconnecting, and a brief GREEN "back online"
// flash on recovery before it auto-hides. Created lazily on <body>, so it works
// on every screen (home, pre-join, in-call, session details).
import { t } from './i18n';

export type NetHealth = 'ok' | 'degraded' | 'offline';
export type BannerState = 'hidden' | 'offline' | 'degraded' | 'restored';

/**
 * Pure decision core (DOM-free, unit-tested): given the PREVIOUS health and the
 * current inputs, return the new health plus what the banner should show. The
 * green `restored` flash fires only when recovering from a real problem — never
 * on first paint while everything is already fine.
 */
export function nextBannerState(
  prev: NetHealth,
  online: boolean,
  degraded: boolean,
): { health: NetHealth; banner: BannerState } {
  const health: NetHealth = !online ? 'offline' : degraded ? 'degraded' : 'ok';
  let banner: BannerState;
  if (health === 'offline') banner = 'offline';
  else if (health === 'degraded') banner = 'degraded';
  else banner = prev === 'ok' ? 'hidden' : 'restored';
  return { health, banner };
}

// How long the green "back online" pill lingers before fading out.
const RESTORED_MS = 2600;

const CONFIG: Record<Exclude<BannerState, 'hidden'>, { cls: string; key: string }> = {
  offline: { cls: 'net-offline', key: 'netOffline' },
  degraded: { cls: 'net-degraded', key: 'netReconnecting' },
  restored: { cls: 'net-online', key: 'netBackOnline' },
};

let bannerEl: HTMLDivElement | null = null;
let labelEl: HTMLSpanElement | null = null;
let prevHealth: NetHealth = 'ok';
let degraded = false;
let hideTimer: ReturnType<typeof setTimeout> | null = null;

function ensureBanner(): void {
  if (bannerEl || typeof document === 'undefined') return;
  const el = document.createElement('div');
  el.className = 'net-status';
  el.setAttribute('role', 'status');
  const dot = document.createElement('span');
  dot.className = 'net-status-dot';
  dot.setAttribute('aria-hidden', 'true');
  labelEl = document.createElement('span');
  labelEl.className = 'net-status-label';
  el.append(dot, labelEl);
  document.body.appendChild(el);
  bannerEl = el;
}

function paint(state: BannerState): void {
  ensureBanner();
  if (!bannerEl || !labelEl) return;
  bannerEl.classList.remove('net-offline', 'net-degraded', 'net-online');
  if (state === 'hidden') {
    bannerEl.classList.remove('show');
    return;
  }
  const cfg = CONFIG[state];
  bannerEl.classList.add(cfg.cls, 'show');
  labelEl.textContent = t(cfg.key);
  // Offline is the only state worth interrupting a screen reader for.
  bannerEl.setAttribute('aria-live', state === 'offline' ? 'assertive' : 'polite');
}

function render(): void {
  const online = typeof navigator === 'undefined' ? true : navigator.onLine !== false;
  const { health, banner } = nextBannerState(prevHealth, online, degraded);
  if (hideTimer) {
    clearTimeout(hideTimer);
    hideTimer = null;
  }
  paint(banner);
  if (banner === 'restored') hideTimer = setTimeout(() => paint('hidden'), RESTORED_MS);
  prevHealth = health;
}

/** Wire the browser online/offline events and paint the initial (usually hidden) state. */
export function initNetStatus(): void {
  if (typeof window === 'undefined') return;
  window.addEventListener('online', render);
  window.addEventListener('offline', render);
  render();
}

/**
 * Report whether the realtime transport is currently struggling (amber). Called
 * by the socket layer: `true` when the signaling WS drops and starts retrying,
 * `false` once it reconnects (which surfaces the green "back online" flash).
 */
export function setNetworkDegraded(on: boolean): void {
  if (degraded === on) return;
  degraded = on;
  render();
}
