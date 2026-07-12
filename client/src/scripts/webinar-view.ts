// Webinar participant view controller (webinar phase 1, F1-5). Thin DOM glue for the
// `/w/{code}` page: localizes the shell, mounts the HlsPlayer, and toggles the
// waiting / live / ended / error overlays + the tap-to-start button from player state.
// All playback + polling logic lives in hls-player.ts (pure/unit-tested); this file is
// intentionally DOM-only and runs solely in the browser.

import { HlsPlayer, getStoredGuestId, type PlayerState } from './hls-player';
import { applyI18n, detectLang, getUiLang, loadLocale, setUiLang, t } from './i18n';
import { PresenceClient } from './webinar-presence';

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

  const player = new HlsPlayer({
    code,
    video,
    onState: renderState,
    onTapToStart: (needsTap) => show(tapBtn, needsTap),
  });

  tapBtn?.addEventListener('click', () => {
    void player.userStart();
  });

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
  // live-presence socket (counted audience) so the "N watching" indicator streams updates.
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
    });
  });
}
