// @vitest-environment jsdom
// nextBannerState is pure; the banner lifecycle (lazy <body> pill, online/offline
// events, the green auto-hide) needs a DOM, so the whole file runs under jsdom.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { nextBannerState } from './net-status';

// Keep labels deterministic (and skip the lazy locale loading).
vi.mock('./i18n', () => ({ t: (k: string) => k }));

describe('nextBannerState', () => {
  it('hides while everything is fine (no flash on first paint)', () => {
    expect(nextBannerState('ok', true, false)).toEqual({ health: 'ok', banner: 'hidden' });
  });

  it('shows the red offline pill when the browser is offline', () => {
    expect(nextBannerState('ok', false, false)).toEqual({ health: 'offline', banner: 'offline' });
  });

  it('offline wins even if the transport is also flagged degraded', () => {
    expect(nextBannerState('ok', false, true)).toEqual({ health: 'offline', banner: 'offline' });
  });

  it('shows the amber reconnecting pill when online but transport degraded', () => {
    expect(nextBannerState('ok', true, true)).toEqual({ health: 'degraded', banner: 'degraded' });
  });

  it('flashes green when recovering from offline', () => {
    expect(nextBannerState('offline', true, false)).toEqual({ health: 'ok', banner: 'restored' });
  });

  it('flashes green when recovering from a degraded transport', () => {
    expect(nextBannerState('degraded', true, false)).toEqual({ health: 'ok', banner: 'restored' });
  });

  it('does not re-flash green once already settled', () => {
    expect(nextBannerState('ok', true, false)).toEqual({ health: 'ok', banner: 'hidden' });
  });
});

// ---- banner lifecycle (DOM) -------------------------------------------------

function setOnline(v: boolean): void {
  Object.defineProperty(window.navigator, 'onLine', { value: v, configurable: true });
}

/** Fresh module per test — the banner element + health live in module state. */
async function loadNetStatus() {
  vi.resetModules();
  return import('./net-status');
}

function banner(): HTMLElement {
  const el = document.body.querySelector<HTMLElement>('.net-status');
  expect(el).not.toBeNull();
  return el!;
}

function label(): string {
  return banner().querySelector('.net-status-label')?.textContent ?? '';
}

describe('net-status banner (DOM)', () => {
  beforeEach(() => {
    document.body.innerHTML = '';
    setOnline(true);
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it('creates the pill lazily on <body> and starts hidden when all is well', async () => {
    const mod = await loadNetStatus();
    mod.initNetStatus();
    const el = banner();
    expect(el.getAttribute('role')).toBe('status');
    expect(el.classList.contains('show')).toBe(false);
    expect(document.body.querySelectorAll('.net-status').length).toBe(1);
  });

  it('paints red + assertive when the browser goes offline', async () => {
    const mod = await loadNetStatus();
    mod.initNetStatus();
    setOnline(false);
    window.dispatchEvent(new Event('offline'));
    const el = banner();
    expect(el.classList.contains('net-offline')).toBe(true);
    expect(el.classList.contains('show')).toBe(true);
    expect(el.getAttribute('aria-live')).toBe('assertive');
    expect(label()).toBe('netOffline');
  });

  it('paints amber while the transport is degraded, then flashes green and auto-hides', async () => {
    const mod = await loadNetStatus();
    mod.initNetStatus();

    mod.setNetworkDegraded(true);
    const el = banner();
    expect(el.classList.contains('net-degraded')).toBe(true);
    expect(el.classList.contains('show')).toBe(true);
    expect(el.getAttribute('aria-live')).toBe('polite');
    expect(label()).toBe('netReconnecting');

    mod.setNetworkDegraded(true); // repeated report → no state change, no crash
    expect(el.classList.contains('net-degraded')).toBe(true);

    mod.setNetworkDegraded(false); // recovered → green flash
    expect(el.classList.contains('net-online')).toBe(true);
    expect(label()).toBe('netBackOnline');

    vi.advanceTimersByTime(2600); // …which auto-hides
    expect(el.classList.contains('show')).toBe(false);
  });

  it('flashes green after coming back from offline', async () => {
    const mod = await loadNetStatus();
    mod.initNetStatus();
    setOnline(false);
    window.dispatchEvent(new Event('offline'));
    setOnline(true);
    window.dispatchEvent(new Event('online'));
    const el = banner();
    expect(el.classList.contains('net-online')).toBe(true);
    expect(el.classList.contains('show')).toBe(true);
  });

  it('going offline during the green flash cancels the pending auto-hide', async () => {
    const mod = await loadNetStatus();
    mod.initNetStatus();
    setOnline(false);
    window.dispatchEvent(new Event('offline'));
    setOnline(true);
    window.dispatchEvent(new Event('online')); // restored, hide timer armed
    setOnline(false);
    window.dispatchEvent(new Event('offline')); // red again before the flash ends
    vi.advanceTimersByTime(5000);
    const el = banner();
    expect(el.classList.contains('net-offline')).toBe(true);
    expect(el.classList.contains('show')).toBe(true); // the stale timer must not hide it
  });
});
