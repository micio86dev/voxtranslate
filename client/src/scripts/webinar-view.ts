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
import { WebinarTts } from './webinar-tts';
import {
  ChatPanel,
  getStoredDisplayName,
  setStoredDisplayName,
  uploadWebinarFile,
  type ChatAttachment,
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

/** Resolve the viewer's preferred subtitle language.
 *  Priority: vt_lang cookie → browser language → 'en'. */
function resolveViewerLang(): string {
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

/** Paint a control button that is icon + label: writes the localized label into the
 *  `.wv-ctl-label` span (NOT `button.textContent`, which would wipe the icon), mirrors it
 *  onto the button's `aria-label` (so the icon-only mobile view stays accessible), and —
 *  when `icon` is provided — updates the `.wv-ctl-ico` glyph. Null-safe: no-ops for a
 *  missing button or absent spans (spans may not exist in unit-test fixtures). */
function setCtl(btn: HTMLElement | null, labelKey: string, icon?: string): void {
  if (!btn) return;
  const label = t(labelKey);
  const labelSpan = btn.querySelector('.wv-ctl-label');
  if (labelSpan) labelSpan.textContent = label;
  btn.setAttribute('aria-label', label);
  if (icon !== undefined) {
    const iconSpan = btn.querySelector('.wv-ctl-ico');
    if (iconSpan) iconSpan.textContent = icon;
  }
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
  const listenBtn = document.getElementById('wv-listen') as HTMLButtonElement | null;
  const ccBtn = document.getElementById('wv-cc') as HTMLButtonElement | null;
  const ctaBtn = document.getElementById('wv-cta') as HTMLButtonElement | null;

  let listenMode: 'original' | 'translated' = 'original';
  let webinarTts: WebinarTts | null = null;
  // Whether a listener-language translation is available (host speaks a different
  // language). Captured from onInfo and read by the CTA to decide the audio path.
  let translationAvailable = false;
  // Whether the viewer has taken the start gesture (via the CTA). Once started, the
  // CTA no longer reappears on later `live` state polls.
  let viewerStarted = false;

  const player = new HlsPlayer({
    code,
    video,
    // Reflect player state onto the shell, and reveal the start CTA the first time
    // the webinar is live and the viewer hasn't started yet (autoplay policy: the
    // stream only reliably plays after a user gesture — the CTA click IS that gesture).
    onState: (state) => {
      renderState(state);
      if (state === 'live' && !viewerStarted) show(ctaBtn, true);
    },
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
      if (info.source_language && info.source_language !== lang) {
        translationAvailable = true;
        show(listenBtn, true);
        webinarTts = new WebinarTts({
          code,
          tier: info.tier,
          lang,
          httpBase: HTTP_BASE,
        });
      }
    },
  });

  tapBtn?.addEventListener('click', () => {
    void player.userStart();
  });

  // Primary start CTA: the reliable gesture that starts playback (autoplay is blocked
  // until the user acts). Starts the video unmuted, then — when a translation is
  // available — switches the audio to the translated TTS track (silencing the original
  // HLS audio) so the listener hears the webinar in their own language from the first tap.
  ctaBtn?.addEventListener('click', () => {
    viewerStarted = true;
    void player.userStart();
    if (translationAvailable) {
      listenMode = 'translated';
      // userStart() unmuted the original HLS audio; mute it so the TTS is the only
      // audio, then enable the translated speech and repaint the listen toggle.
      player.muteAudio(true);
      paintAudio();
      webinarTts?.setEnabled(true);
      paintListen();
    }
    show(ctaBtn, false);
  });

  // Audio mute toggle (wv-mute): aria-pressed=true when audio is ACTIVE (playing).
  function paintAudio(): void {
    const muted = player.isMuted();
    if (muteBtn) {
      setCtl(muteBtn, muted ? 'wvUnmuteAudio' : 'wvMuteAudio', muted ? '🔇' : '🔊');
      muteBtn.setAttribute('aria-pressed', muted ? 'false' : 'true');
    }
  }
  muteBtn?.addEventListener('click', () => {
    player.muteAudio(!player.isMuted());
    paintAudio();
  });
  paintAudio();

  function paintListen(): void {
    if (!listenBtn) return;
    const key = listenMode === 'original' ? 'wvListenTranslated' : 'wvListenOriginal';
    // Keep the data-i18n on the label span so applyI18n() re-localizes it on locale change.
    listenBtn.querySelector('.wv-ctl-label')?.setAttribute('data-i18n', key);
    setCtl(listenBtn, key);
    listenBtn.setAttribute('aria-pressed', listenMode === 'translated' ? 'true' : 'false');
  }
  listenBtn?.addEventListener('click', () => {
    if (listenMode === 'original') {
      listenMode = 'translated';
      player.muteAudio(true);
      paintAudio();
      webinarTts?.setEnabled(true);
    } else {
      listenMode = 'original';
      player.muteAudio(false);
      paintAudio();
      webinarTts?.setEnabled(false);
      webinarTts?.stop();
    }
    paintListen();
  });
  paintListen();

  // Captions on/off toggle. Off clears whatever is on screen so a stale line doesn't linger.
  function paintCc(): void {
    if (!ccBtn) return;
    setCtl(ccBtn, subtitlesOn ? 'wvSubtitlesOn' : 'wvSubtitlesOff');
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
  // Transcript starts OPEN by default so live captions accumulate in view without a click.
  let transcriptOpen = true;
  let transcriptHistoryLoaded = false;

  function paintTranscript(): void {
    if (!transcriptToggleBtn) return;
    setCtl(transcriptToggleBtn, transcriptOpen ? 'wvTranscriptHide' : 'wvTranscriptShow');
    transcriptToggleBtn.setAttribute('aria-pressed', transcriptOpen ? 'true' : 'false');
  }
  // Reveal the panel + load history when the transcript is open (mount or manual toggle).
  // The history fetch is gated by `transcriptHistoryLoaded` so it runs at most once.
  function reflectTranscriptOpen(): void {
    show(transcriptPanel, transcriptOpen);
    paintTranscript();
    if (transcriptOpen && transcriptList) {
      if (!transcriptHistoryLoaded) {
        transcriptHistoryLoaded = true;
        void loadTranscriptHistory(code);
      }
      transcriptList.scrollTop = transcriptList.scrollHeight;
    }
  }
  transcriptToggleBtn?.addEventListener('click', () => {
    transcriptOpen = !transcriptOpen;
    reflectTranscriptOpen();
  });
  // Reflect the default-open state on mount (shows the panel + loads history once).
  reflectTranscriptOpen();

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
      webinarTts?.stop();
      webinarTts = null;
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
            if (event.kind === 'final') {
              const myLang = getUiLang();
              appendTranscriptEntry(event.translations[myLang] ?? event.original);
              if (listenMode === 'translated') {
                const text = event.translations[myLang] ?? event.original;
                webinarTts?.speak(text);
              }
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
  const attachBtn = document.getElementById('wv-chat-attach') as HTMLButtonElement | null;
  const fileInput = document.getElementById('wv-chat-file') as HTMLInputElement | null;
  const uploadBox = document.getElementById('wv-chat-upload') as HTMLElement | null;
  const uploadFill = document.getElementById('wv-chat-upload-fill') as HTMLElement | null;
  const counter = document.getElementById('wv-chat-counter') as HTMLElement | null;
  const attachPreview = document.getElementById('wv-attach-preview') as HTMLElement | null;
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
    // Logged-in viewers send with their token (stored at vox.token, same key the presence
    // WS already uses). A valid org-member token → sender_kind:"host"; otherwise still
    // "guest". Either way the avatar_url rides along for display.
    token: () => { try { return localStorage.getItem('vox.token'); } catch { return null; } },
    strings: chatStrings(),
    hostAvatarUrl,
    senderAvatarUrl: () => {
      try {
        const raw = localStorage.getItem('vox.user');
        if (!raw) return null;
        const u = JSON.parse(raw) as { avatar_url?: string | null };
        return u.avatar_url ?? null;
      } catch { return null; }
    },
    counter,
    attachPreview,
  });

  let historyLoaded = false;
  let open = false;
  function paintToggle(): void {
    if (!toggleBtn) return;
    setCtl(toggleBtn, open ? 'wvChatHide' : 'wvChatShow');
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

  // File attachment: clicking the attach button opens the hidden file input.
  attachBtn?.addEventListener('click', () => fileInput?.click());
  fileInput?.addEventListener('change', async () => {
    const file = fileInput?.files?.[0];
    if (!file || !uploadBox || !uploadFill) return;
    show(uploadBox, true);
    uploadFill.style.width = '0%';
    const att = await uploadWebinarFile(HTTP_BASE, code, file, (frac) => {
      uploadFill.style.width = `${Math.round(frac * 100)}%`;
    });
    show(uploadBox, false);
    if (fileInput) fileInput.value = '';
    if (att) {
      panelCtl.setStagedAttachment(att);
      input.focus();
    }
  });

  return (event) => panelCtl.append(event);
}
