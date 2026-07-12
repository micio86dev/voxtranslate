// Webinar participant view controller (webinar phase 1, F1-5). Thin DOM glue for the
// `/w/{code}` page: localizes the shell, mounts the HlsPlayer, and toggles the
// waiting / live / ended / error overlays + the tap-to-start button from player state.
// All playback + polling logic lives in hls-player.ts (pure/unit-tested); this file is
// intentionally DOM-only and runs solely in the browser.

import { HlsPlayer, getStoredGuestId, type PlayerState } from './hls-player';
import { applyI18n, detectLang, getUiLang, loadLocale, setUiLang, t } from './i18n';
import { PresenceClient, type SubtitleEvent } from './webinar-presence';
import { renderSubtitleInto } from './subtitle-render';

// App WS base, mirroring auth.ts (this file never imports auth to keep the /w/ bundle
// lean and free of the accounts/billing surface).
const WS_HOST = import.meta.env.PUBLIC_WS_HOST || location.host;
const WS_PROTO = location.protocol === 'https:' ? 'wss:' : 'ws:';
const WS_BASE = `${WS_PROTO}//${WS_HOST}`;

/** Toggle an element's `.hidden` CLASS (never the HTML `hidden` attribute — the app's
 *  show() gotcha: elements are styled off via the class). No-op for a missing element. */
function show(el: HTMLElement | null, visible: boolean): void {
  el?.classList.toggle('hidden', !visible);
}

/** Render the live audience count into the "N watching" indicator, revealing it on the
 *  first frame. */
function renderWatching(count: number): void {
  const badge = document.getElementById('wv-watching');
  const text = document.getElementById('wv-watching-text');
  if (text) text.textContent = t('webinarWatching').replace('{n}', String(count));
  show(badge, true);
}

/** Reflect the player state onto the page: the status badge + the overlays. */
function renderState(state: PlayerState): void {
  const badge = document.getElementById('wv-status');
  const waiting = document.getElementById('wv-overlay-waiting');
  const ended = document.getElementById('wv-overlay-ended');
  const error = document.getElementById('wv-overlay-error');

  show(waiting, state === 'waiting');
  show(ended, state === 'ended');
  show(error, state === 'error');

  if (badge) {
    badge.classList.toggle('is-live', state === 'live');
    const key =
      state === 'live'
        ? 'wvStateLive'
        : state === 'ended'
          ? 'wvStateEnded'
          : state === 'error'
            ? 'wvStateError'
            : 'wvStateWaiting';
    badge.textContent = t(key);
  }
}

/** Whether captions are currently shown (toggled by the CC button). Starts on. */
let subtitlesOn = true;

/**
 * Render a live-subtitle event into the overlay in the viewer's language. Finals show
 * `translations[myLang]` (falling back to the source `original`); interims show the raw
 * source `text` while it streams. No-op while captions are toggled off, or when the mode
 * has nothing to show (then the previous caption stays on screen). Exported for unit tests.
 */
export function renderSubtitle(overlay: HTMLElement, event: SubtitleEvent): void {
  if (!subtitlesOn) return;
  const myLang = getUiLang();
  if (event.kind === 'final') {
    const translation = event.translations[myLang] ?? event.original;
    // 'both' shows the listener's translation prominently + the source beneath (when the
    // viewer isn't already watching in the source language).
    renderSubtitleInto(overlay, 'both', {
      translation,
      original: event.original,
      interim: false,
    });
  } else {
    // Interim: only the streaming source is known — show it as the main line.
    renderSubtitleInto(overlay, 'both', {
      original: event.text,
      interim: true,
    });
  }
}

/**
 * Mount the participant player. Reads the webinar code from the page's `data-code`,
 * localizes the static shell, and — unless the page is the 404/not-found state —
 * starts an HlsPlayer that plays the LL-HLS manifest and polls for live/ended.
 */
export function mountWebinarPlayer(): void {
  // Localize the shell in the visitor's browser language, then repaint once the
  // (lazy) locale dictionary has loaded.
  const lang = detectLang();
  setUiLang(lang);
  applyI18n();
  void loadLocale(lang).then(applyI18n);

  const root = document.querySelector<HTMLElement>('.wv');
  const code = root?.dataset.code ?? '';
  // The not-found shell has no player to mount.
  if (!root || root.dataset.notfound === '1' || !code) return;

  const video = document.getElementById('wv-video') as HTMLVideoElement | null;
  const tapBtn = document.getElementById('wv-tap') as HTMLButtonElement | null;
  if (!video) return;

  const subtitleOverlay = document.getElementById('wv-subtitles');
  const muteBtn = document.getElementById('wv-mute') as HTMLButtonElement | null;
  const listenBtn = document.getElementById('wv-listen') as HTMLButtonElement | null;
  const ccBtn = document.getElementById('wv-cc') as HTMLButtonElement | null;

  const player = new HlsPlayer({
    code,
    video,
    onState: renderState,
    onTapToStart: (needsTap) => show(tapBtn, needsTap),
  });

  tapBtn?.addEventListener('click', () => {
    void player.userStart();
  });

  // Audio controls (webinar Fase 2). The video starts muted (autoplay policy); the guest
  // opts into sound. Both buttons drive `player.muteAudio` — the mute button toggles, and
  // "listen in the original language" is the explicit unmute affordance (Fase 2 carries
  // only the host's original audio over HLS). `paintAudio` keeps their labels in sync.
  function paintAudio(): void {
    const muted = player.isMuted();
    if (muteBtn) {
      muteBtn.textContent = t(muted ? 'wvUnmuteAudio' : 'wvMuteAudio');
      muteBtn.setAttribute('aria-pressed', muted ? 'true' : 'false');
    }
    if (listenBtn) listenBtn.setAttribute('aria-pressed', muted ? 'false' : 'true');
  }
  muteBtn?.addEventListener('click', () => {
    player.muteAudio(!player.isMuted());
    paintAudio();
  });
  listenBtn?.addEventListener('click', () => {
    player.muteAudio(false); // ensure the host's original audio is unmuted + playing
    paintAudio();
  });
  paintAudio();

  // Captions on/off toggle. Off clears whatever is on screen so a stale line doesn't linger.
  function paintCc(): void {
    if (!ccBtn) return;
    ccBtn.textContent = t(subtitlesOn ? 'wvSubtitlesOn' : 'wvSubtitlesOff');
    ccBtn.setAttribute('aria-pressed', subtitlesOn ? 'true' : 'false');
  }
  ccBtn?.addEventListener('click', () => {
    subtitlesOn = !subtitlesOn;
    if (!subtitlesOn && subtitleOverlay) subtitleOverlay.innerHTML = '';
    paintCc();
  });
  paintCc();

  let presence: PresenceClient | null = null;

  // Free the poll timer + hls.js + presence WS when the guest navigates away.
  addEventListener(
    'pagehide',
    () => {
      player.destroy();
      presence?.close();
      presence = null;
    },
    { once: true },
  );

  renderState('waiting');
  // start() fetches the webinar and persists the guest_id; once it resolves we open the
  // live-presence socket (counted audience) so the "N watching" indicator streams updates
  // AND the live subtitle frames (same WS) render into the overlay in the viewer's language.
  void player.start().then(() => {
    const guestId = getStoredGuestId();
    if (!guestId) return; // no identity (fetch failed) — skip the count, playback still works
    presence = new PresenceClient({
      wsBase: WS_BASE,
      code,
      guestId,
      host: false,
      lang: getUiLang(),
      onCount: renderWatching,
      onSubtitle: subtitleOverlay
        ? (event) => renderSubtitle(subtitleOverlay, event)
        : undefined,
    });
  });
}
