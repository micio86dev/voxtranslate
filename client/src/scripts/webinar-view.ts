// Webinar participant view controller (webinar phase 1, F1-5). Thin DOM glue for the
// `/w/{code}` page: localizes the shell, mounts the HlsPlayer, and toggles the
// waiting / live / ended / error overlays + the tap-to-start button from player state.
// All playback + polling logic lives in hls-player.ts (pure/unit-tested); this file is
// intentionally DOM-only and runs solely in the browser.

import { HlsPlayer, getStoredGuestId, type PlayerState } from './hls-player';
import { applyI18n, detectLang, getUiLang, loadLocale, setUiLang, SUPPORTED, t } from './i18n';
import { PresenceClient, type SubtitleEvent } from './webinar-presence';
import { renderSubtitleInto } from './subtitle-render';
import { getPublicWebinar } from './webinar';
import {
  ChatPanel,
  getStoredDisplayName,
  setStoredDisplayName,
  type ChatPanelStrings,
} from './webinar-chat';
import { insertAt } from './chat-input';

// App WS base, mirroring auth.ts (this file never imports auth to keep the /w/ bundle
// lean and free of the accounts/billing surface).
const WS_HOST = import.meta.env.PUBLIC_WS_HOST || location.host;
const WS_PROTO = location.protocol === 'https:' ? 'wss:' : 'ws:';
const WS_BASE = `${WS_PROTO}//${WS_HOST}`;
// App HTTP base (derived from WS_BASE, mirroring auth.ts) for the chat REST calls.
const HTTP_BASE = WS_BASE.replace(/^ws/, 'http');

// The localStorage key where the main app persists the user's chosen UI language
// (mirrors LANG_CACHE_KEY in app.ts). On the /w/ page we read it first so a viewer
// who has already used VoxTranslate gets subtitles in THEIR language, not the browser
// default. Falls back to detectLang() (browser language → 'en') for first-time visitors.
const LANG_CACHE_KEY = 'voxtranslate_lang';

/** Resolve the viewer's preferred subtitle language.
 *  Priority: localStorage preference → vt_lang cookie (set by the main app) → browser language → 'en'.
 *  The cookie fallback covers cases where localStorage is unavailable (private/incognito mode, Safari ITP)
 *  or the participant opened the webinar link before visiting the main app on this browser. */
function resolveViewerLang(): string {
  try {
    const stored = typeof localStorage !== 'undefined' ? localStorage.getItem(LANG_CACHE_KEY) : null;
    if (stored && SUPPORTED.includes(stored)) return stored;
  } catch {
    /* private mode / blocked */
  }
  try {
    const m = document.cookie.match(/(?:^|;\s*)vt_lang=([^;]+)/);
    if (m) {
      const fromCookie = decodeURIComponent(m[1]);
      if (SUPPORTED.includes(fromCookie)) return fromCookie;
    }
  } catch {
    /* blocked */
  }
  return detectLang();
}

/** The localized strings the chat panel needs, read from the current locale. */
function chatStrings(): ChatPanelStrings {
  return {
    send: t('wvChatSend'),
    hostTag: t('wvChatHost'),
    empty: t('wvChatEmpty'),
    rateLimited: t('wvChatRateLimited'),
    blocked: t('wvChatBlocked'),
    genericError: t('wvChatBlocked'),
  };
}

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

/** Auto-clear timer: hides the subtitle overlay 5 s after the last final line. */
let subtitleClearTimer: ReturnType<typeof setTimeout> | null = null;

/**
 * Render a live-subtitle event into the overlay in the viewer's language. Finals show
 * `translations[myLang]` (falling back to the source `original`); interims show the raw
 * source `text` while it streams. No-op while captions are toggled off, or when the mode
 * has nothing to show (then the previous caption stays on screen). Exported for unit tests.
 *
 * Final subtitles also schedule an auto-clear of the overlay after 5 seconds so the
 * caption doesn't linger on screen between phrases.
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
    // Schedule auto-clear: remove the subtitle 5 s after the last final line.
    if (subtitleClearTimer) clearTimeout(subtitleClearTimer);
    subtitleClearTimer = setTimeout(() => {
      overlay.innerHTML = '';
      subtitleClearTimer = null;
    }, 5_000);
  } else {
    // Interim: cancel any pending clear (streaming is active) then render.
    if (subtitleClearTimer) {
      clearTimeout(subtitleClearTimer);
      subtitleClearTimer = null;
    }
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
  // Resolve the viewer's language: stored preference (from a prior app session) takes
  // priority over the browser default so subtitles arrive in the right language even
  // for first-time visitors on the /w/ page who haven't explicitly chosen a language here.
  const lang = resolveViewerLang();
  setUiLang(lang);
  applyI18n();
  void loadLocale(lang).then(applyI18n);

  const root = document.querySelector<HTMLElement>('.wv');
  const code = root?.dataset.code ?? '';
  // The not-found shell has no player to mount.
  if (!root || root.dataset.notfound === '1' || !code) return;

  // Members-only gate: if the webinar requires authentication and the user has no
  // stored JWT, show a "log in to join" overlay instead of starting the player.
  if (root.dataset.membersOnly === '1') {
    const token = (() => {
      try { return typeof localStorage !== 'undefined' ? localStorage.getItem('vox.token') : null; }
      catch { return null; }
    })();
    if (!token) {
      // Reveal the members-only overlay (hidden by default in the page markup).
      const membersOnlyOverlay = document.getElementById('wv-overlay-members-only');
      show(membersOnlyOverlay, true);
      // Hide the normal waiting overlay so only the gate is shown.
      const waitingOverlay = document.getElementById('wv-overlay-waiting');
      show(waitingOverlay, false);
      return;
    }
  }

  const video = document.getElementById('wv-video') as HTMLVideoElement | null;
  const tapBtn = document.getElementById('wv-tap') as HTMLButtonElement | null;
  if (!video) return;

  // Pre-webinar name gate: collect the guest's display name before they enter.
  // The gate covers the full screen and disappears once the name is submitted.
  // Returning visitors (name already stored) never see it.
  const nameGate = document.getElementById('wv-name-gate');
  const nameGateInput = document.getElementById('wv-name-gate-input') as HTMLInputElement | null;
  const nameGateSave = document.getElementById('wv-name-gate-save') as HTMLButtonElement | null;
  if (!getStoredDisplayName()) {
    show(nameGate, true);
    nameGateInput?.focus();
  }
  function submitNameGate(): void {
    const chosen = setStoredDisplayName(nameGateInput?.value ?? '');
    if (!chosen) { nameGateInput?.focus(); return; }
    show(nameGate, false);
  }
  nameGateSave?.addEventListener('click', submitNameGate);
  nameGateInput?.addEventListener('keydown', (e) => {
    if (e.key === 'Enter') submitNameGate();
  });

  const subtitleOverlay = document.getElementById('wv-subtitles');
  const muteBtn = document.getElementById('wv-mute') as HTMLButtonElement | null;
  const ccBtn = document.getElementById('wv-cc') as HTMLButtonElement | null;

  const player = new HlsPlayer({
    code,
    video,
    onState: renderState,
    onTapToStart: (needsTap) => show(tapBtn, needsTap),
    onInfo: (info) => {
      // Replace the waiting spinner with the host's avatar when one is available.
      // Falls back to the spinner for webinars without an avatar (pre-043 or no profile pic).
      if (info.host_avatar_url) {
        const avatarImg = document.getElementById('wv-host-avatar') as HTMLImageElement | null;
        const spinner = document.getElementById('wv-waiting-spinner');
        if (avatarImg) {
          avatarImg.src = info.host_avatar_url;
          show(avatarImg, true);
          show(spinner, false);
        }
      }
    },
  });

  tapBtn?.addEventListener('click', () => {
    void player.userStart();
  });

  // Audio mute toggle (wv-mute): aria-pressed=true when audio is ACTIVE (playing).
  function paintAudio(): void {
    const muted = player.isMuted();
    if (muteBtn) {
      muteBtn.textContent = t(muted ? 'wvUnmuteAudio' : 'wvMuteAudio');
      muteBtn.setAttribute('aria-pressed', muted ? 'false' : 'true');
    }
  }
  muteBtn?.addEventListener('click', () => {
    player.muteAudio(!player.isMuted());
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

  // Transcript history panel: accumulates final subtitle lines with timestamps.
  const transcriptToggleBtn = document.getElementById('wv-transcript-toggle') as HTMLButtonElement | null;
  const transcriptPanel = document.getElementById('wv-transcript');
  const transcriptList = document.getElementById('wv-transcript-list');
  const transcriptEmpty = document.getElementById('wv-transcript-empty');
  let transcriptOpen = false;
  let transcriptHistoryLoaded = false;

  function paintTranscript(): void {
    if (!transcriptToggleBtn) return;
    transcriptToggleBtn.textContent = t(transcriptOpen ? 'wvTranscriptHide' : 'wvTranscriptShow');
    transcriptToggleBtn.setAttribute('aria-pressed', transcriptOpen ? 'true' : 'false');
  }
  transcriptToggleBtn?.addEventListener('click', () => {
    transcriptOpen = !transcriptOpen;
    show(transcriptPanel, transcriptOpen);
    paintTranscript();
    if (transcriptOpen && transcriptList) {
      if (!transcriptHistoryLoaded) {
        transcriptHistoryLoaded = true;
        void loadTranscriptHistory(code);
      }
      transcriptList.scrollTop = transcriptList.scrollHeight;
    }
  });
  paintTranscript();

  /** Build one transcript entry element. Shared by history load and live append. */
  function buildTranscriptItem(text: string, time: Date): HTMLElement {
    const item = document.createElement('div');
    item.className = 'wv-tr-item';
    const timeEl = document.createElement('span');
    timeEl.className = 'wv-tr-time';
    timeEl.textContent = time.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
    const textEl = document.createElement('span');
    textEl.className = 'wv-tr-text';
    textEl.textContent = text;
    item.append(timeEl, textEl);
    return item;
  }

  /** Fetch transcript history and prepend it before any live entries already in the
   *  list. Only called once (gated by `transcriptHistoryLoaded`). Silently no-ops
   *  when recording is off or the fetch fails — the panel still shows live entries. */
  async function loadTranscriptHistory(wCode: string): Promise<void> {
    if (!transcriptList) return;
    let rows: Array<{ original: string; lang: string; translations: Record<string, string>; spoken_at: string }>;
    try {
      const res = await fetch(
        `${HTTP_BASE}/api/w/${encodeURIComponent(wCode)}/transcript?limit=200`,
      );
      if (!res.ok) return;
      const data = (await res.json()) as unknown;
      if (!Array.isArray(data)) return;
      rows = data as typeof rows;
    } catch {
      return;
    }
    if (!rows.length) return;
    const myLang = getUiLang();
    // Insert history entries BEFORE any live entries already appended.
    const firstLive = transcriptList.querySelector('.wv-tr-item');
    for (const e of rows) {
      const text = e.translations[myLang] ?? e.original;
      const item = buildTranscriptItem(text, new Date(e.spoken_at));
      if (firstLive) {
        transcriptList.insertBefore(item, firstLive);
      } else {
        transcriptList.appendChild(item);
      }
    }
    if (transcriptEmpty) transcriptEmpty.style.display = 'none';
    if (transcriptOpen) transcriptList.scrollTop = transcriptList.scrollHeight;
  }

  /** Append a confirmed final subtitle line (in the viewer's language) to the history panel. */
  function appendTranscriptEntry(translation: string): void {
    if (!transcriptList) return;
    const item = buildTranscriptItem(translation, new Date());
    transcriptList.appendChild(item);
    // Hide empty state and auto-scroll when the panel is open.
    if (transcriptEmpty) transcriptEmpty.style.display = 'none';
    if (transcriptOpen) transcriptList.scrollTop = transcriptList.scrollHeight;
  }

  let presence: PresenceClient | null = null;

  // Auto-translated chat (Feature ⑤). Gated on the webinar's `chat_enabled` flag, fetched
  // once below; when on, we reveal the show/hide toggle and mount the panel. The returned
  // handler is wired into the presence WS's `onChat` so live messages append into the list.
  const chatHandler = setupChat(code);

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
  // live-presence socket (counted audience) so the "N watching" indicator streams updates,
  // the live subtitle frames (same WS) render into the overlay, AND — when chat is enabled —
  // chat frames (same WS) append into the panel, all in the viewer's language.
  void player.start().then(async () => {
    const guestId = getStoredGuestId();
    if (!guestId) return; // no identity (fetch failed) — skip the count, playback still works
    const onChat = await chatHandler;
    presence = new PresenceClient({
      wsBase: WS_BASE,
      code,
      guestId,
      host: false,
      token: (() => { try { return localStorage.getItem('vox.token'); } catch { return null; } })(),
      lang: getUiLang(),
      onCount: renderWatching,
      onSubtitle: subtitleOverlay
        ? (event) => {
            renderSubtitle(subtitleOverlay, event);
            // Accumulate final lines in the transcript history panel.
            if (event.kind === 'final') {
              const myLang = getUiLang();
              appendTranscriptEntry(event.translations[myLang] ?? event.original);
            }
          }
        : undefined,
      onChat: onChat ?? undefined,
    });
  });
}

/**
 * Wire the viewer chat panel if the webinar has chat enabled. Fetches the public webinar to
 * read `chat_enabled`; when off (or on error), resolves to null and the panel stays hidden.
 * When on: reveals the show/hide toggle, mounts a `ChatPanel`, wires the send form + the
 * one-time guest display-name prompt, loads history on first open, and returns an `onChat`
 * handler that appends live WS messages into the list. All DOM lives here; the panel logic
 * (render, optimistic send, notice mapping) lives in webinar-chat.ts.
 */
async function setupChat(
  code: string,
): Promise<((event: import('./webinar-chat').ChatEvent) => void) | null> {
  const toggleBtn = document.getElementById('wv-chat-toggle') as HTMLButtonElement | null;
  const panel = document.getElementById('wv-chat');
  const list = document.getElementById('wv-chat-list');
  const input = document.getElementById('wv-chat-input') as HTMLTextAreaElement | null;
  const sendBtn = document.getElementById('wv-chat-send') as HTMLButtonElement | null;
  const notice = document.getElementById('wv-chat-notice');
  const form = document.getElementById('wv-chat-form') as HTMLFormElement | null;
  const nameBox = document.getElementById('wv-chat-name');
  const nameInput = document.getElementById('wv-chat-name-input') as HTMLInputElement | null;
  const nameSave = document.getElementById('wv-chat-name-save') as HTMLButtonElement | null;
  const emojiToggle = document.getElementById('wv-emoji-toggle') as HTMLButtonElement | null;
  const emojiPanel = document.getElementById('wv-emoji-panel');
  if (!toggleBtn || !panel || !list || !input || !sendBtn || !notice || !form) return null;

  // Read the chat flag. A failure (offline / SSR-skipped) just leaves chat hidden.
  let enabled = false;
  let hostAvatarUrl: string | null = null;
  try {
    const info = await getPublicWebinar(code);
    enabled = !!info.chat_enabled;
    hostAvatarUrl = info.host_avatar_url ?? null;
  } catch {
    return null;
  }
  if (!enabled) return null;

  // Reveal the toggle now that chat is confirmed on.
  show(toggleBtn, true);

  const panelCtl = new ChatPanel({
    list,
    input,
    sendBtn,
    notice,
    httpBase: HTTP_BASE,
    code,
    myLang: () => getUiLang(),
    senderLang: () => getUiLang(),
    // Guests send with their stored display name (or the prompt gates the first send below).
    displayName: () => getStoredDisplayName(),
    token: () => null, // viewers are unauthenticated guests → sender_kind:"guest"
    strings: chatStrings(),
    hostAvatarUrl,
  });

  let historyLoaded = false;
  let open = false;
  function paintToggle(): void {
    if (!toggleBtn) return;
    toggleBtn.textContent = t(open ? 'wvChatHide' : 'wvChatShow');
    toggleBtn.setAttribute('aria-pressed', open ? 'true' : 'false');
  }
  toggleBtn.addEventListener('click', () => {
    open = !open;
    show(panel, open);
    paintToggle();
    if (open) {
      maybePromptName();
      if (!historyLoaded) {
        historyLoaded = true;
        void panelCtl.loadHistory(); // late joiners get prior context in their language
      }
      input?.focus();
    }
  });
  paintToggle();

  // One-time display-name prompt: a guest picks a name before their first message. Once set,
  // the prompt hides and the input row is enabled.
  function maybePromptName(): void {
    const hasName = !!getStoredDisplayName();
    show(nameBox, !hasName);
    show(form, hasName);
  }
  nameSave?.addEventListener('click', () => {
    const chosen = setStoredDisplayName(nameInput?.value ?? '');
    if (!chosen) {
      nameInput?.focus();
      return;
    }
    maybePromptName();
    input.focus();
  });
  maybePromptName();

  form.addEventListener('submit', (e) => {
    e.preventDefault();
    if (!getStoredDisplayName()) {
      maybePromptName();
      nameInput?.focus();
      return;
    }
    void panelCtl.send().then((sent) => {
      // After a successful send, reset the textarea height (it may have grown).
      if (sent) {
        input.style.height = 'auto';
        input.style.overflowY = 'hidden';
      }
    });
  });

  // Auto-grow the textarea as the user types (matches the Meet chat composer).
  input.addEventListener('input', () => {
    input.style.height = 'auto';
    const next = Math.min(input.scrollHeight, 120);
    input.style.height = `${next}px`;
    input.style.overflowY = input.scrollHeight > 120 ? 'auto' : 'hidden';
  });

  // Emoji picker: same 20-emoji set as the Meet chat (chat.ts EMOJI_LIST). Clicking an
  // emoji inserts it at the textarea cursor via insertAt() from chat-input.ts, then closes
  // the panel. Clicking outside the panel also closes it.
  const EMOJI_LIST = ['👍','❤️','😂','😮','😢','👏','🎉','🔥','💯','✅','🤔','😍','🙌','💪','🤝','😊','🥳','😎','🤬','👎'];
  if (emojiPanel) {
    for (const em of EMOJI_LIST) {
      const btn = document.createElement('button');
      btn.type = 'button';
      btn.className = 'wv-emoji-btn';
      btn.textContent = em;
      btn.addEventListener('click', () => {
        const start = input.selectionStart ?? input.value.length;
        const end = input.selectionEnd ?? start;
        const result = insertAt(input.value, em, start, end);
        if (result) {
          input.value = result.value;
          input.focus();
          input.setSelectionRange(result.caret, result.caret);
          // Trigger resize in case the text grew.
          input.dispatchEvent(new Event('input'));
        }
        show(emojiPanel, false);
        emojiToggle?.setAttribute('aria-expanded', 'false');
      });
      emojiPanel.appendChild(btn);
    }
  }
  emojiToggle?.addEventListener('click', () => {
    const isOpen = !emojiPanel?.classList.contains('hidden');
    show(emojiPanel ?? null, !isOpen);
    emojiToggle.setAttribute('aria-expanded', String(!isOpen));
  });
  // Close the panel on click outside (capture so it fires before the toggle itself).
  document.addEventListener('click', (e) => {
    if (
      emojiPanel &&
      !emojiPanel.classList.contains('hidden') &&
      !emojiPanel.contains(e.target as Node) &&
      e.target !== emojiToggle
    ) {
      show(emojiPanel, false);
      emojiToggle?.setAttribute('aria-expanded', 'false');
    }
  }, { capture: true });

  return (event) => panelCtl.append(event);
}
