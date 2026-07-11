// @vitest-environment jsdom
// Webinar participant view controller (webinar phase 1, F1-5). DOM glue for the
// `/w/{code}` page: mounts the HlsPlayer and reflects its state onto the badge +
// overlays. HlsPlayer and i18n are mocked so this tests the wiring, not playback.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

// Capture the options the controller passes to HlsPlayer so we can drive its callbacks.
let lastOpts: any = null;
const start = vi.fn(async () => {});
const userStart = vi.fn(async () => true);
const destroy = vi.fn();
vi.mock('./hls-player', () => ({
  // A real (constructable) function so `new HlsPlayer(opts)` works; captures opts.
  HlsPlayer: vi.fn(function (this: any, opts: any) {
    lastOpts = opts;
    this.start = start;
    this.userStart = userStart;
    this.destroy = destroy;
  }),
}));

// i18n: identity `t` (returns the key) + no-op locale glue, so we assert on keys.
vi.mock('./i18n', () => ({
  t: (k: string) => k,
  detectLang: () => 'en',
  setUiLang: vi.fn(),
  applyI18n: vi.fn(),
  loadLocale: vi.fn(async () => {}),
}));

import { mountWebinarPlayer } from './webinar-view';

function buildDom(code = 'ab12cd', notfound = '0'): void {
  document.body.innerHTML = `
    <main class="wv" data-code="${code}" data-notfound="${notfound}">
      <span id="wv-status" class="wv-badge"></span>
      <video id="wv-video"></video>
      <button id="wv-tap" class="wv-tap hidden"></button>
      <div id="wv-overlay-waiting" class="wv-overlay"></div>
      <div id="wv-overlay-ended" class="wv-overlay hidden"></div>
      <div id="wv-overlay-error" class="wv-overlay hidden"></div>
    </main>`;
}

const el = (id: string) => document.getElementById(id)!;

beforeEach(() => {
  lastOpts = null;
  start.mockClear();
  userStart.mockClear();
  destroy.mockClear();
});
afterEach(() => {
  document.body.innerHTML = '';
});

describe('mountWebinarPlayer', () => {
  it('mounts an HlsPlayer with the page code and starts it', () => {
    buildDom('ab12cd');
    mountWebinarPlayer();
    expect(lastOpts.code).toBe('ab12cd');
    expect(lastOpts.video).toBe(el('wv-video'));
    expect(start).toHaveBeenCalled();
  });

  it('does not mount on the not-found shell', () => {
    buildDom('ab12cd', '1');
    mountWebinarPlayer();
    expect(start).not.toHaveBeenCalled();
  });

  it('does not mount when there is no code', () => {
    buildDom('');
    mountWebinarPlayer();
    expect(start).not.toHaveBeenCalled();
  });

  it('reflects live state: badge live class + waiting/ended/error hidden', () => {
    buildDom();
    mountWebinarPlayer();
    lastOpts.onState('live');
    expect(el('wv-status').classList.contains('is-live')).toBe(true);
    expect(el('wv-status').textContent).toBe('wvStateLive');
    expect(el('wv-overlay-waiting').classList.contains('hidden')).toBe(true);
    expect(el('wv-overlay-ended').classList.contains('hidden')).toBe(true);
    expect(el('wv-overlay-error').classList.contains('hidden')).toBe(true);
  });

  it('reflects waiting / ended / error overlays', () => {
    buildDom();
    mountWebinarPlayer();

    lastOpts.onState('waiting');
    expect(el('wv-overlay-waiting').classList.contains('hidden')).toBe(false);
    expect(el('wv-status').textContent).toBe('wvStateWaiting');

    lastOpts.onState('ended');
    expect(el('wv-overlay-ended').classList.contains('hidden')).toBe(false);
    expect(el('wv-status').classList.contains('is-live')).toBe(false);
    expect(el('wv-status').textContent).toBe('wvStateEnded');

    lastOpts.onState('error');
    expect(el('wv-overlay-error').classList.contains('hidden')).toBe(false);
    expect(el('wv-status').textContent).toBe('wvStateError');
  });

  it('shows the tap-to-start button only when autoplay is blocked', () => {
    buildDom();
    mountWebinarPlayer();
    lastOpts.onTapToStart(true);
    expect(el('wv-tap').classList.contains('hidden')).toBe(false);
    lastOpts.onTapToStart(false);
    expect(el('wv-tap').classList.contains('hidden')).toBe(true);
  });

  it('tapping the start button calls player.userStart()', () => {
    buildDom();
    mountWebinarPlayer();
    (el('wv-tap') as HTMLButtonElement).click();
    expect(userStart).toHaveBeenCalled();
  });

  it('destroys the player on pagehide', () => {
    buildDom();
    mountWebinarPlayer();
    dispatchEvent(new Event('pagehide'));
    expect(destroy).toHaveBeenCalled();
  });
});
