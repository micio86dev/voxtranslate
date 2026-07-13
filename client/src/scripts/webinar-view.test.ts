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
const muteAudio = vi.fn();
let mutedState = true; // the video starts muted (autoplay policy)
const isMuted = vi.fn(() => mutedState);
vi.mock('./hls-player', () => ({
  // A real (constructable) function so `new HlsPlayer(opts)` works; captures opts.
  HlsPlayer: vi.fn(function (this: any, opts: any) {
    lastOpts = opts;
    this.start = start;
    this.userStart = userStart;
    this.destroy = destroy;
    this.muteAudio = muteAudio;
    this.isMuted = isMuted;
  }),
  // No stored guest identity → the controller skips the presence connect, so these
  // tests exercise the player wiring only (not the viewer-count side effect).
  getStoredGuestId: vi.fn(() => null),
}));

// i18n: identity `t` (returns the key) + no-op locale glue, so we assert on keys.
// SUPPORTED is exported and used by resolveViewerLang to validate a stored language.
// setUiLangMock is hoisted so the vi.mock factory (which is hoisted) can reference it.
const { setUiLangMock } = vi.hoisted(() => ({ setUiLangMock: vi.fn() }));
vi.mock('./i18n', () => ({
  t: (k: string) => k,
  detectLang: () => 'en',
  getUiLang: () => 'en',
  setUiLang: setUiLangMock,
  applyI18n: vi.fn(),
  loadLocale: vi.fn(async () => {}),
  SUPPORTED: ['en', 'es', 'it', 'fr', 'de', 'pt', 'zh', 'ja', 'ko', 'ar', 'ru'],
}));

import { mountWebinarPlayer, renderSubtitle } from './webinar-view';
import type { SubtitleEvent } from './webinar-presence';

function buildDom(code = 'ab12cd', notfound = '0'): void {
  document.body.innerHTML = `
    <main class="wv" data-code="${code}" data-notfound="${notfound}">
      <span id="wv-status" class="wv-badge"></span>
      <video id="wv-video"></video>
      <div id="wv-subtitles"></div>
      <button id="wv-tap" class="wv-tap hidden"></button>
      <div id="wv-overlay-waiting" class="wv-overlay"></div>
      <div id="wv-overlay-ended" class="wv-overlay hidden"></div>
      <div id="wv-overlay-error" class="wv-overlay hidden"></div>
      <button id="wv-mute" aria-pressed="false"></button>
      <button id="wv-listen" aria-pressed="false"></button>
      <button id="wv-cc" aria-pressed="true"></button>
    </main>`;
}

const el = (id: string) => document.getElementById(id)!;

beforeEach(() => {
  lastOpts = null;
  start.mockClear();
  userStart.mockClear();
  destroy.mockClear();
  muteAudio.mockClear();
  isMuted.mockClear();
  setUiLangMock.mockClear();
  mutedState = true; // reset the fake player's mute flag between tests
  localStorage.clear(); // start each test without a stored language preference
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

  it('paints the audio buttons from the initial (muted) player state', () => {
    buildDom();
    mountWebinarPlayer();
    // Muted by default → the mute button offers "Unmute" and is NOT pressed (audio inactive).
    // The listen button's aria-pressed is not managed by paintAudio — it stays at its HTML default.
    expect(el('wv-mute').textContent).toBe('wvUnmuteAudio');
    expect(el('wv-mute').getAttribute('aria-pressed')).toBe('false');
    expect(el('wv-listen').getAttribute('aria-pressed')).toBe('false'); // HTML default, untouched
  });

  it('the mute button toggles the HLS audio and repaints', () => {
    buildDom();
    mountWebinarPlayer();
    // Click while muted → unmute (muteAudio(false)); fake reflects the new state.
    muteAudio.mockImplementation((m: boolean) => (mutedState = m));
    (el('wv-mute') as HTMLButtonElement).click();
    expect(muteAudio).toHaveBeenCalledWith(false);
    expect(el('wv-mute').textContent).toBe('wvMuteAudio'); // now offers "Mute"
    expect(el('wv-mute').getAttribute('aria-pressed')).toBe('true'); // audio is now active
    // listenBtn is decoupled — pressing mute must NOT change its visual state.
    expect(el('wv-listen').getAttribute('aria-pressed')).toBe('false');
    // Click again → mute.
    (el('wv-mute') as HTMLButtonElement).click();
    expect(muteAudio).toHaveBeenLastCalledWith(true);
    expect(el('wv-mute').textContent).toBe('wvUnmuteAudio');
    expect(el('wv-mute').getAttribute('aria-pressed')).toBe('false');
  });

  it('the listen-original button unmutes the HLS audio when muted', () => {
    buildDom();
    mountWebinarPlayer();
    muteAudio.mockImplementation((m: boolean) => (mutedState = m));
    (el('wv-listen') as HTMLButtonElement).click();
    expect(muteAudio).toHaveBeenCalledWith(false);
    // After unmute, mute toggle reflects active audio; listen button state is unchanged.
    expect(el('wv-mute').getAttribute('aria-pressed')).toBe('true');
    expect(el('wv-listen').getAttribute('aria-pressed')).toBe('false');
  });

  it('the listen-original button is a no-op when audio is already playing', () => {
    buildDom();
    mountWebinarPlayer();
    mutedState = false; // fake: already playing
    (el('wv-listen') as HTMLButtonElement).click();
    expect(muteAudio).not.toHaveBeenCalled(); // no state change when audio is already on
  });

  it('the CC button toggles captions and clears the overlay when turned off', () => {
    buildDom();
    mountWebinarPlayer();
    const overlay = el('wv-subtitles');
    overlay.innerHTML = '<div>stale caption</div>';
    // Starts on → first click turns it OFF and clears the overlay.
    (el('wv-cc') as HTMLButtonElement).click();
    expect(el('wv-cc').textContent).toBe('wvSubtitlesOff');
    expect(el('wv-cc').getAttribute('aria-pressed')).toBe('false');
    expect(overlay.innerHTML).toBe('');
    // Second click turns it back ON (restore shared module state for other tests).
    (el('wv-cc') as HTMLButtonElement).click();
    expect(el('wv-cc').textContent).toBe('wvSubtitlesOn');
    expect(el('wv-cc').getAttribute('aria-pressed')).toBe('true');
  });

  it('renderSubtitle renders a final frame as translations[myLang] into the overlay', () => {
    buildDom();
    const overlay = el('wv-subtitles');
    // Viewer language is mocked to 'en' → picks translations.en.
    const final: SubtitleEvent = {
      kind: 'final',
      original: 'hola',
      lang: 'es',
      translations: { es: 'hola', en: 'hi' },
    };
    renderSubtitle(overlay, final);
    expect(overlay.querySelector('.subtitle-translation')?.textContent).toBe('hi');
    // 'both' mode also renders the source line beneath a final with a distinct original.
    expect(overlay.querySelector('.subtitle-original')?.textContent).toBe('hola');
  });

  it('renderSubtitle falls back to the source when no translation for myLang', () => {
    buildDom();
    const overlay = el('wv-subtitles');
    renderSubtitle(overlay, {
      kind: 'final',
      original: 'hola',
      lang: 'es',
      translations: { es: 'hola' }, // no 'en'
    });
    expect(overlay.querySelector('.subtitle-translation')?.textContent).toBe('hola');
  });

  it('renderSubtitle renders an interim frame as the streaming source line', () => {
    buildDom();
    const overlay = el('wv-subtitles');
    renderSubtitle(overlay, { kind: 'interim', text: 'hol', lang: 'es' });
    const box = overlay.querySelector('.subtitle');
    expect(box?.classList.contains('subtitle-interim')).toBe(true);
    expect(overlay.querySelector('.subtitle-translation')?.textContent).toBe('hol');
  });

  it('uses a stored language preference from localStorage over the browser default', () => {
    localStorage.setItem('voxtranslate_lang', 'es');
    buildDom();
    mountWebinarPlayer();
    // setUiLang should have been called with the stored language, not the detectLang() default ('en')
    expect(setUiLangMock).toHaveBeenCalledWith('es');
  });

  it('falls back to detectLang() when localStorage has no stored language', () => {
    buildDom();
    mountWebinarPlayer();
    expect(setUiLangMock).toHaveBeenCalledWith('en');
  });

  it('falls back to detectLang() when the stored language is not supported', () => {
    localStorage.setItem('voxtranslate_lang', 'xx'); // 'xx' is not in SUPPORTED
    buildDom();
    mountWebinarPlayer();
    expect(setUiLangMock).toHaveBeenCalledWith('en');
  });
});
