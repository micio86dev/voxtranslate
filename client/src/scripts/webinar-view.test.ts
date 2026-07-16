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

// WebinarTts is constructed in onInfo when host/viewer languages differ; mock it so the
// viewer wiring can toggle the translated-audio track without real speechSynthesis.
const ttsSetEnabled = vi.fn();
const ttsStop = vi.fn();
const ttsSpeak = vi.fn();
vi.mock('./webinar-tts', () => ({
  WebinarTts: vi.fn(function (this: any) {
    this.setEnabled = ttsSetEnabled;
    this.stop = ttsStop;
    this.speak = ttsSpeak;
  }),
}));

import { mountWebinarPlayer, renderSubtitle } from './webinar-view';
import type { SubtitleEvent } from './webinar-presence';

function buildDom(code = 'ab12cd', notfound = '0'): void {
  document.body.innerHTML = `
    <main class="wv" data-code="${code}" data-notfound="${notfound}">
      <button id="wv-timer" class="wv-timer hidden"></button>
      <span id="wv-status" class="wv-badge"></span>
      <video id="wv-video"></video>
      <div id="wv-subtitles"></div>
      <button id="wv-tap" class="wv-tap hidden"></button>
      <div id="wv-overlay-waiting" class="wv-overlay">
        <img id="wv-host-avatar" class="wv-host-avatar hidden" alt="" />
        <div id="wv-waiting-spinner" class="wv-spinner"></div>
      </div>
      <div id="wv-overlay-ended" class="wv-overlay hidden"></div>
      <div id="wv-overlay-error" class="wv-overlay hidden"></div>
      <button id="wv-mute" class="wv-ctl" aria-pressed="false">
        <span class="wv-ctl-ico" aria-hidden="true">🔊</span>
        <span class="wv-ctl-label">Mute audio</span>
      </button>
      <button id="wv-listen" class="wv-ctl hidden" aria-pressed="false">
        <span class="wv-ctl-ico" aria-hidden="true">🌐</span>
        <span class="wv-ctl-label">Listen translated</span>
      </button>
      <button id="wv-cc" class="wv-ctl" aria-pressed="true">
        <span class="wv-ctl-ico wv-cc-badge" aria-hidden="true">CC</span>
        <span class="wv-ctl-label">Subtitles on</span>
      </button>
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
  ttsSetEnabled.mockClear();
  ttsStop.mockClear();
  ttsSpeak.mockClear();
  setUiLangMock.mockClear();
  mutedState = true; // reset the fake player's mute flag between tests
  localStorage.clear(); // start each test without a stored language preference
  // Clear the vt_lang cookie between tests so language tests don't bleed into each other.
  document.cookie = 'vt_lang=; max-age=0';
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

  it('paints the mute button from the initial (muted) player state', () => {
    buildDom();
    mountWebinarPlayer();
    // Muted by default → mute button offers "Unmute" and is NOT pressed (audio inactive).
    expect(el('wv-mute').querySelector('.wv-ctl-label')?.textContent).toBe('wvUnmuteAudio');
    expect(el('wv-mute').getAttribute('aria-pressed')).toBe('false');
  });

  it('the mute button toggles the HLS audio and repaints', () => {
    buildDom();
    mountWebinarPlayer();
    muteAudio.mockImplementation((m: boolean) => (mutedState = m));
    // Click while muted → unmute.
    (el('wv-mute') as HTMLButtonElement).click();
    expect(muteAudio).toHaveBeenCalledWith(false);
    expect(el('wv-mute').querySelector('.wv-ctl-label')?.textContent).toBe('wvMuteAudio');
    expect(el('wv-mute').getAttribute('aria-pressed')).toBe('true');
    // Click again → mute.
    (el('wv-mute') as HTMLButtonElement).click();
    expect(muteAudio).toHaveBeenLastCalledWith(true);
    expect(el('wv-mute').getAttribute('aria-pressed')).toBe('false');
  });

  it('switching to translated audio does NOT flip the mute toggle', () => {
    buildDom();
    mountWebinarPlayer();
    muteAudio.mockImplementation((m: boolean) => (mutedState = m));
    // Host speaks a different language than the viewer (en) → the listen toggle appears.
    lastOpts.onInfo?.({ source_language: 'it', code: 'x', title: 'T', status: 'live',
      tier: 'standard', join_url: '', chat_enabled: false, playback_url: null, guest_id: 'g',
      host_avatar_url: null });
    expect(el('wv-listen').classList.contains('hidden')).toBe(false);
    // Turn audio ON so the mute control is in its "active" state.
    (el('wv-mute') as HTMLButtonElement).click();
    expect(el('wv-mute').getAttribute('aria-pressed')).toBe('true');
    const labelBefore = el('wv-mute').querySelector('.wv-ctl-label')?.textContent;
    // Choosing translated audio must re-route the SOURCE only, never touch the mute control.
    (el('wv-listen') as HTMLButtonElement).click();
    expect(el('wv-mute').getAttribute('aria-pressed')).toBe('true');
    expect(el('wv-mute').querySelector('.wv-ctl-label')?.textContent).toBe(labelBefore);
    // The listen toggle itself flips to translated and enables the TTS track.
    expect(el('wv-listen').getAttribute('aria-pressed')).toBe('true');
    expect(ttsSetEnabled).toHaveBeenLastCalledWith(true);
  });

  it('muting silences the translated TTS track, not just the original HLS', () => {
    buildDom();
    mountWebinarPlayer();
    muteAudio.mockImplementation((m: boolean) => (mutedState = m));
    lastOpts.onInfo?.({ source_language: 'it', code: 'x', title: 'T', status: 'live',
      tier: 'standard', join_url: '', chat_enabled: false, playback_url: null, guest_id: 'g',
      host_avatar_url: null });
    (el('wv-mute') as HTMLButtonElement).click(); // audio on (original)
    (el('wv-listen') as HTMLButtonElement).click(); // switch to translated (TTS on)
    expect(ttsSetEnabled).toHaveBeenLastCalledWith(true);
    (el('wv-mute') as HTMLButtonElement).click(); // mute → TTS must go silent too
    expect(el('wv-mute').getAttribute('aria-pressed')).toBe('false');
    expect(ttsSetEnabled).toHaveBeenLastCalledWith(false);
    expect(ttsStop).toHaveBeenCalled();
  });

  it('hides the listen toggle when the participant speaks the webinar language', () => {
    buildDom();
    mountWebinarPlayer(); // mocked viewer language is 'en'
    lastOpts.onInfo?.({ source_language: 'en', code: 'x', title: 'T', status: 'live',
      tier: 'standard', join_url: '', chat_enabled: false, playback_url: null, guest_id: 'g',
      host_avatar_url: null });
    expect(el('wv-listen').classList.contains('hidden')).toBe(true);
  });

  it('hides the listen toggle regardless of language case/region', () => {
    buildDom();
    mountWebinarPlayer(); // viewer 'en' vs webinar 'EN' / 'en-US' is still the same language
    lastOpts.onInfo?.({ source_language: 'EN', code: 'x', title: 'T', status: 'live',
      tier: 'standard', join_url: '', chat_enabled: false, playback_url: null, guest_id: 'g',
      host_avatar_url: null });
    expect(el('wv-listen').classList.contains('hidden')).toBe(true);
  });

  it('a different-language viewer defaults to the translated track on start', () => {
    buildDom();
    mountWebinarPlayer();
    muteAudio.mockImplementation((m: boolean) => (mutedState = m));
    lastOpts.onInfo?.({ source_language: 'it', code: 'x', title: 'T', status: 'live',
      tier: 'standard', join_url: '', chat_enabled: false, playback_url: null, guest_id: 'g',
      host_avatar_url: null });
    (el('wv-tap') as HTMLButtonElement).click(); // start via the autoplay-unblock gesture
    expect(el('wv-mute').getAttribute('aria-pressed')).toBe('true'); // audio active
    expect(el('wv-listen').getAttribute('aria-pressed')).toBe('true'); // translated (default)
    expect(ttsSetEnabled).toHaveBeenLastCalledWith(true); // TTS track carries the sound
    expect(muteAudio).toHaveBeenLastCalledWith(true); // original HLS silenced
  });

  it('a same-language viewer hears the original on start (no translation)', () => {
    buildDom();
    mountWebinarPlayer();
    muteAudio.mockImplementation((m: boolean) => (mutedState = m));
    lastOpts.onInfo?.({ source_language: 'en', code: 'x', title: 'T', status: 'live',
      tier: 'standard', join_url: '', chat_enabled: false, playback_url: null, guest_id: 'g',
      host_avatar_url: null });
    (el('wv-tap') as HTMLButtonElement).click();
    expect(el('wv-listen').classList.contains('hidden')).toBe(true); // no listen toggle
    expect(el('wv-mute').getAttribute('aria-pressed')).toBe('true'); // audio active
    expect(muteAudio).toHaveBeenLastCalledWith(false); // original HLS audible
    expect(ttsSetEnabled).not.toHaveBeenCalled(); // TTS never even constructed/enabled
  });

  it('shows the host avatar and hides the spinner when host_avatar_url is set', () => {
    buildDom();
    mountWebinarPlayer();
    lastOpts.onInfo?.({ source_language: 'en', code: 'x', title: 'T', status: 'scheduled',
      tier: 'standard', join_url: '', chat_enabled: false, playback_url: null, guest_id: 'g',
      host_avatar_url: 'https://cdn.example/avatar.jpg' });
    const avatar = el('wv-host-avatar') as HTMLImageElement;
    const spinner = el('wv-waiting-spinner');
    expect(avatar.classList.contains('hidden')).toBe(false);
    expect((avatar as HTMLImageElement).src).toBe('https://cdn.example/avatar.jpg');
    expect(spinner.classList.contains('hidden')).toBe(true);
  });

  it('keeps the spinner when host_avatar_url is null', () => {
    buildDom();
    mountWebinarPlayer();
    lastOpts.onInfo?.({ source_language: 'en', code: 'x', title: 'T', status: 'scheduled',
      tier: 'standard', join_url: '', chat_enabled: false, playback_url: null, guest_id: 'g',
      host_avatar_url: null });
    expect(el('wv-host-avatar').classList.contains('hidden')).toBe(true);
    expect(el('wv-waiting-spinner').classList.contains('hidden')).toBe(false);
  });

  it('the CC button toggles captions and clears the overlay when turned off', () => {
    buildDom();
    mountWebinarPlayer();
    const overlay = el('wv-subtitles');
    overlay.innerHTML = '<div>stale caption</div>';
    // Starts on → first click turns it OFF and clears the overlay.
    (el('wv-cc') as HTMLButtonElement).click();
    expect(el('wv-cc').querySelector('.wv-ctl-label')?.textContent).toBe('wvSubtitlesOff');
    expect(el('wv-cc').getAttribute('aria-pressed')).toBe('false');
    expect(overlay.innerHTML).toBe('');
    // Second click turns it back ON (restore shared module state for other tests).
    (el('wv-cc') as HTMLButtonElement).click();
    expect(el('wv-cc').querySelector('.wv-ctl-label')?.textContent).toBe('wvSubtitlesOn');
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

  it('uses a stored language preference from the vt_lang cookie over the browser default', () => {
    document.cookie = 'vt_lang=es';
    buildDom();
    mountWebinarPlayer();
    // setUiLang should have been called with the cookie language, not the detectLang() default ('en')
    expect(setUiLangMock).toHaveBeenCalledWith('es');
  });

  it('falls back to detectLang() when localStorage has no stored language', () => {
    buildDom();
    mountWebinarPlayer();
    expect(setUiLangMock).toHaveBeenCalledWith('en');
  });

  it('falls back to detectLang() when the vt_lang cookie holds an unsupported language', () => {
    document.cookie = 'vt_lang=xx'; // 'xx' is not in SUPPORTED
    buildDom();
    mountWebinarPlayer();
    expect(setUiLangMock).toHaveBeenCalledWith('en');
  });
});

describe('mountWebinarPlayer live timer', () => {
  // A base PublicWebinar-shaped info object; individual tests override the timer fields.
  const baseInfo = {
    code: 'x', title: 'T', status: 'live' as const, source_language: 'en',
    tier: 'standard' as const, join_url: '', chat_enabled: false,
    playback_url: null, guest_id: 'g', host_avatar_url: null, members_only: false,
  };

  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  it('shows the elapsed timer when live and actual_start is set', () => {
    vi.setSystemTime(new Date('2026-07-15T12:00:07Z')); // 7s after start
    buildDom();
    mountWebinarPlayer();
    lastOpts.onInfo({ ...baseInfo, actual_start: '2026-07-15T12:00:00Z', scheduled_end: null });
    lastOpts.onState('live');
    const timer = el('wv-timer');
    expect(timer.classList.contains('hidden')).toBe(false);
    expect(timer.textContent).toBe('00:00'); // 7s → still the 00 minute
    expect(timer.getAttribute('aria-label')).toBe('wvTimerElapsed');
  });

  it('ticks the elapsed count every second', () => {
    vi.setSystemTime(new Date('2026-07-15T12:00:00Z'));
    buildDom();
    mountWebinarPlayer();
    lastOpts.onInfo({ ...baseInfo, actual_start: '2026-07-15T12:00:00Z', scheduled_end: null });
    lastOpts.onState('live');
    // advanceTimersByTime also moves the mocked clock forward, so Date.now() in the
    // tick reflects the elapsed 65s → 00:01.
    vi.advanceTimersByTime(65_000);
    expect(el('wv-timer').textContent).toBe('00:01');
  });

  it('does not toggle to countdown when there is no scheduled end (stays elapsed)', () => {
    vi.setSystemTime(new Date('2026-07-15T12:00:00Z'));
    buildDom();
    mountWebinarPlayer();
    lastOpts.onInfo({ ...baseInfo, actual_start: '2026-07-15T12:00:00Z', scheduled_end: null });
    lastOpts.onState('live');
    (el('wv-timer') as HTMLButtonElement).click();
    expect(el('wv-timer').getAttribute('aria-label')).toBe('wvTimerElapsed');
  });

  it('toggles to countdown on click when a scheduled end is set', () => {
    vi.setSystemTime(new Date('2026-07-15T12:00:00Z'));
    buildDom();
    mountWebinarPlayer();
    lastOpts.onInfo({
      ...baseInfo,
      actual_start: '2026-07-15T12:00:00Z',
      scheduled_end: '2026-07-15T12:05:00Z', // 5 min out
    });
    lastOpts.onState('live');
    (el('wv-timer') as HTMLButtonElement).click();
    expect(el('wv-timer').getAttribute('aria-label')).toBe('wvTimerRemaining');
    expect(el('wv-timer').textContent).toBe('00:05'); // 5 min remaining
    // Click again → back to elapsed.
    (el('wv-timer') as HTMLButtonElement).click();
    expect(el('wv-timer').getAttribute('aria-label')).toBe('wvTimerElapsed');
  });

  it('remaining never goes negative past the scheduled end', () => {
    vi.setSystemTime(new Date('2026-07-15T12:10:00Z')); // past the 12:05 end
    buildDom();
    mountWebinarPlayer();
    lastOpts.onInfo({
      ...baseInfo,
      actual_start: '2026-07-15T12:00:00Z',
      scheduled_end: '2026-07-15T12:05:00Z',
    });
    lastOpts.onState('live');
    (el('wv-timer') as HTMLButtonElement).click(); // → remaining
    expect(el('wv-timer').textContent).toBe('00:00');
  });

  it('hides + stops the timer when the webinar ends', () => {
    vi.setSystemTime(new Date('2026-07-15T12:00:00Z'));
    buildDom();
    mountWebinarPlayer();
    lastOpts.onInfo({ ...baseInfo, actual_start: '2026-07-15T12:00:00Z', scheduled_end: null });
    lastOpts.onState('live');
    expect(el('wv-timer').classList.contains('hidden')).toBe(false);
    lastOpts.onState('ended');
    expect(el('wv-timer').classList.contains('hidden')).toBe(true);
  });

  it('does not start the timer without an actual_start', () => {
    buildDom();
    mountWebinarPlayer();
    lastOpts.onInfo({ ...baseInfo, actual_start: null, scheduled_end: null });
    lastOpts.onState('live');
    expect(el('wv-timer').classList.contains('hidden')).toBe(true);
  });
});
