// Auto-translated webinar chat (webinar Feature ⑤). An optional per-webinar chat where
// EVERYONE sends (the host + unauthenticated viewers, each with a cosmetic display name)
// and everyone reads each message auto-translated into their OWN language. Chat frames
// arrive over the SAME presence WS the viewer already opens (see webinar-presence.ts);
// this module holds the pure/testable HTTP + rendering glue the viewer + host studio share.
//
// Contract (server is done):
//   - Send:    POST /api/w/{code}/chat  (credentials:'include' → guest_id cookie; a host
//              also sends Authorization: Bearer). Body {text, display_name?, lang?}.
//              200 {id, created_at}; 400 empty/too-long; 403 disabled; 404 unknown;
//              422 moderated; 429 rate-limited (5/10s per sender).
//   - History: GET /api/w/{code}/chat?limit=N (public) → chronological messages.
//   - A recipient renders translations[myLang] ?? original (same rule as subtitles).

/** Max chat message length the server accepts (matches the server-side guard). */
export const CHAT_TEXT_MAX = 500;
/** Max cosmetic display-name length the server accepts. */
export const CHAT_NAME_MAX = 40;

/** A chat message as it lives on the client (from the WS frame or the history endpoint).
 *  `translations` always includes the source `lang`; a recipient shows `translations[myLang]`
 *  falling back to `original`. `sender_kind` distinguishes the host from a guest for styling. */
export interface ChatEvent {
  /** Server-assigned message id (used to de-dupe the WS echo of an optimistic send). */
  id: string;
  /** Who sent it — `host` (the broadcaster) or `guest` (a viewer). */
  sender_kind: 'host' | 'guest';
  /** The sender's cosmetic display name. */
  display_name: string;
  /** The sender's original words. */
  original: string;
  /** The source language code of `original`. */
  lang: string;
  /** Per-language translations (always includes `lang`). */
  translations: Record<string, string>;
  /** RFC3339 timestamp the server stamped on the message. */
  created_at: string;
  avatar_url?: string | null;
  attachment?: ChatAttachment | null;
}

export interface ChatAttachment {
  url: string;
  name: string;
  content_type: string;
  size: number;
}

/** Pick the text to show a recipient in their language: the translation for `myLang`,
 *  falling back to the source `original` (the exact rule live subtitles use). Pure. */
export function renderChatText(event: ChatEvent, myLang: string): string {
  return event.translations[myLang] ?? event.original;
}

/** Where the guest's chosen cosmetic display name is persisted (survives reloads so a
 *  returning viewer keeps their name), mirroring the guest_id key in hls-player.ts. */
export const DISPLAY_NAME_KEY = 'vox.display_name';

/** localStorage may be blocked (private mode, tests) — fall back to an in-memory map,
 *  mirroring hls-player.ts so display-name persistence never throws. */
const mem = new Map<string, string>();
function store(): Pick<Storage, 'getItem' | 'setItem'> {
  try {
    if (typeof localStorage !== 'undefined') return localStorage;
  } catch {
    /* blocked */
  }
  return {
    getItem: (k) => (mem.has(k) ? mem.get(k)! : null),
    setItem: (k, v) => void mem.set(k, v),
  };
}

/** The persisted cosmetic display name, or null if the guest hasn't chosen one yet. */
export function getStoredDisplayName(): string | null {
  return store().getItem(DISPLAY_NAME_KEY);
}

/** Persist the guest's chosen display name (trimmed + clamped to CHAT_NAME_MAX). Returns
 *  the effective stored value. A blank name is ignored (leaves any previous name intact). */
export function setStoredDisplayName(name: string): string {
  const clean = (name || '').trim().slice(0, CHAT_NAME_MAX);
  if (clean) store().setItem(DISPLAY_NAME_KEY, clean);
  return clean || getStoredDisplayName() || '';
}

/** A failed chat request. `status` is the HTTP status (0 on a network error) so the UI
 *  can distinguish rate-limit / moderation / disabled from a generic failure and show the
 *  right friendly notice. Mirrors WebinarError so callers get a typed, switchable error. */
export class ChatError extends Error {
  status: number;
  constructor(status: number, message: string) {
    super(message);
    this.name = 'ChatError';
    this.status = status;
  }
}

/** The payload a caller sends: the message text plus, optionally, the sender's cosmetic
 *  display name and their UI-language hint (so the server records the source language). */
export interface SendChatBody {
  text: string;
  display_name?: string;
  lang?: string;
  avatar_url?: string | null;
  attachment?: ChatAttachment | null;
}

/** The 200 response of a successful send. */
export interface SendChatResult {
  id: string;
  created_at: string;
}

/** POST a chat message to `POST /api/w/{code}/chat`. Always sends `credentials:'include'`
 *  so the guest_id cookie rides along; when a host `token` is supplied it also sends the
 *  `Authorization: Bearer` header (→ the server records `sender_kind:"host"`). Empty fields
 *  are omitted from the body. On a non-2xx it throws a typed `ChatError` carrying the HTTP
 *  status (403 disabled / 422 moderated / 429 rate-limited / 400 empty-or-too-long) so the
 *  UI can map it to a localized notice. The `fetchImpl` is injectable for tests. */
export async function sendChatMessage(
  httpBase: string,
  code: string,
  body: SendChatBody,
  token?: string | null,
  fetchImpl: typeof fetch = fetch,
): Promise<SendChatResult> {
  const headers: Record<string, string> = { 'Content-Type': 'application/json' };
  if (token) headers.Authorization = `Bearer ${token}`;
  const payload: SendChatBody = { text: body.text };
  if (body.display_name) payload.display_name = body.display_name;
  if (body.lang) payload.lang = body.lang;
  if (body.avatar_url) payload.avatar_url = body.avatar_url;
  if (body.attachment) payload.attachment = body.attachment;

  let res: Response;
  try {
    res = await fetchImpl(`${httpBase}/api/w/${encodeURIComponent(code)}/chat`, {
      method: 'POST',
      credentials: 'include',
      headers,
      body: JSON.stringify(payload),
    });
  } catch {
    throw new ChatError(0, 'network error');
  }
  if (!res.ok) {
    let message = `chat send failed (${res.status})`;
    try {
      const err = (await res.json()) as { error?: string; message?: string };
      message = err.error || err.message || message;
    } catch {
      /* non-JSON error body — keep the generic message */
    }
    throw new ChatError(res.status, message);
  }
  return (await res.json()) as SendChatResult;
}

/** GET recent chat history from `GET /api/w/{code}/chat?limit=N` (public — no auth), a
 *  chronological array of messages so a late joiner sees prior context in their language.
 *  A failed fetch resolves to an empty list (history is best-effort; live frames still
 *  arrive over the WS). The `fetchImpl` is injectable for tests. */
export async function fetchChatHistory(
  httpBase: string,
  code: string,
  limit: number,
  fetchImpl: typeof fetch = fetch,
): Promise<ChatEvent[]> {
  let res: Response;
  try {
    res = await fetchImpl(
      `${httpBase}/api/w/${encodeURIComponent(code)}/chat?limit=${encodeURIComponent(
        String(limit),
      )}`,
    );
  } catch {
    return [];
  }
  if (!res.ok) return [];
  try {
    const rows = (await res.json()) as unknown;
    if (!Array.isArray(rows)) return [];
    return rows.map(normalizeChat).filter((e): e is ChatEvent => e !== null);
  } catch {
    return [];
  }
}

function parseAttachment(raw: unknown): ChatAttachment | null {
  if (typeof raw !== 'object' || raw === null) return null;
  const a = raw as Record<string, unknown>;
  if (typeof a.url !== 'string' || typeof a.name !== 'string') return null;
  if (typeof a.content_type !== 'string' || typeof a.size !== 'number') return null;
  if (!a.url.startsWith('https://')) return null;
  return { url: a.url, name: a.name, content_type: a.content_type, size: a.size };
}

/** Coerce one raw history row into a `ChatEvent`, or null if it's malformed. History rows
 *  carry no `sender_id` (public view); the shape otherwise mirrors the WS frame. Pure. */
function normalizeChat(row: unknown): ChatEvent | null {
  if (typeof row !== 'object' || row === null) return null;
  const r = row as Record<string, unknown>;
  if (typeof r.id !== 'string') return null;
  if (r.sender_kind !== 'host' && r.sender_kind !== 'guest') return null;
  if (typeof r.display_name !== 'string') return null;
  if (typeof r.original !== 'string') return null;
  if (typeof r.lang !== 'string') return null;
  if (typeof r.created_at !== 'string') return null;
  const translations: Record<string, string> = {};
  if (typeof r.translations === 'object' && r.translations !== null) {
    for (const [k, v] of Object.entries(r.translations as Record<string, unknown>)) {
      if (typeof v === 'string') translations[k] = v;
    }
  }
  return {
    id: r.id,
    sender_kind: r.sender_kind,
    display_name: r.display_name,
    original: r.original,
    lang: r.lang,
    translations,
    created_at: r.created_at,
    avatar_url: typeof r.avatar_url === 'string' ? r.avatar_url : null,
    attachment: parseAttachment(r.attachment),
  };
}

// ---- Thin DOM controller shared by the viewer panel + the host studio -------------------
// The viewer (webinar-view.ts) and the host studio (app.ts) render the exact same chat panel.
// The only differences are injected: the sender's auth `token` (host → sender_kind:"host") and
// whether a display-name prompt is shown (guests only). Everything DOM here so the pure helpers
// above stay unit-testable in isolation.

/** Localized strings the panel needs, injected so this module never imports i18n directly
 *  (keeps the /w/ bundle lean, mirroring webinar-view.ts). */
export interface ChatPanelStrings {
  send: string;
  hostTag: string;
  empty: string;
  rateLimited: string;
  blocked: string;
  genericError: string;
}

/** The DOM handles + behavior a `ChatPanel` drives. */
export interface ChatPanelOptions {
  /** The scrolling message list (`aria-live="polite"`). */
  list: HTMLElement;
  /** The message text input or textarea. */
  input: HTMLInputElement | HTMLTextAreaElement;
  /** The send button (disabled while a send is in flight). */
  sendBtn: HTMLButtonElement;
  /** The transient notice line (rate-limit / moderation / error), shown then auto-hidden. */
  notice: HTMLElement;
  /** The app HTTP base, e.g. `https://api.voxtranslate.app`. */
  httpBase: string;
  /** The webinar's public code. */
  code: string;
  /** The recipient's UI language — messages render `translations[myLang] ?? original`. */
  myLang: () => string;
  /** The sender's UI-language hint sent to the server (records the source language). */
  senderLang: () => string;
  /** The sender's cosmetic display name, or null (guest hasn't chosen one → the caller
   *  gates sending behind the name prompt). Host studio returns a stable name. */
  displayName: () => string | null;
  /** The host bearer token, or null for a guest send. */
  token: () => string | null;
  /** Localized strings. */
  strings: ChatPanelStrings;
  /** Injectable fetch for tests. Defaults to the global `fetch`. */
  fetchImpl?: typeof fetch;
  /** The host's avatar URL (from `PublicWebinar.host_avatar_url`), used to show a profile
   *  picture instead of initials for host chat messages. Null for webinars without one. */
  hostAvatarUrl?: string | null;
  /** Returns the sender's own avatar URL (for the optimistic render of host-sent messages). */
  senderAvatarUrl?: () => string | null;
  /** Optional character counter element (shows `len/max`, gets `.is-over` when over). */
  counter?: HTMLElement | null;
}

/**
 * Renders + drives a webinar chat panel. Appends messages (de-duped by id so the WS echo of an
 * optimistically-rendered send doesn't double up), auto-scrolls to the newest, and POSTs sends
 * with an optimistic local render on 200. Maps 429/422/403 to friendly notices.
 */
export class ChatPanel {
  private readonly opts: ChatPanelOptions;
  private readonly fetchImpl: typeof fetch;
  /** Ids already rendered — so a WS frame echoing our own optimistic send is a no-op. */
  private readonly seen = new Set<string>();
  private noticeTimer: ReturnType<typeof setTimeout> | null = null;
  private empty: HTMLElement | null = null;
  private pendingAttachment: ChatAttachment | null = null;

  constructor(opts: ChatPanelOptions) {
    this.opts = opts;
    this.fetchImpl = opts.fetchImpl ?? fetch;
    this.showEmpty();
    // Enter sends via the form submit the caller wires; this keeps the button in sync.
    this.opts.input.addEventListener('input', () => this.updateCounter());
  }

  /** Show the "no messages yet" placeholder (until the first message arrives). */
  private showEmpty(): void {
    if (this.seen.size > 0) return;
    const doc = this.opts.list.ownerDocument;
    this.opts.list.innerHTML = '';
    const p = doc.createElement('p');
    p.className = 'wv-chat-empty';
    p.textContent = this.opts.strings.empty;
    this.opts.list.appendChild(p);
    this.empty = p;
  }

  /** Append one message to the list (idempotent by id) and scroll to the newest. */
  append(event: ChatEvent): void {
    if (this.seen.has(event.id)) return; // already rendered (e.g. our optimistic send)
    this.seen.add(event.id);
    if (this.empty) {
      this.empty.remove();
      this.empty = null;
    }
    const doc = this.opts.list.ownerDocument;
    const row = buildChatRow(doc, event, this.opts.myLang(), this.opts.strings.hostTag, this.opts.hostAvatarUrl);
    this.opts.list.appendChild(row);
    // Scroll to the bottom so the newest message is visible.
    this.opts.list.scrollTop = this.opts.list.scrollHeight;
  }

  /** Load recent history so a late joiner sees prior context in their language. */
  async loadHistory(limit = 50): Promise<void> {
    const rows = await fetchChatHistory(this.opts.httpBase, this.opts.code, limit, this.fetchImpl);
    for (const row of rows) this.append(row);
  }

  /** Stage an attachment to be sent with the next message. Pass null to clear. */
  setStagedAttachment(att: ChatAttachment | null): void {
    this.pendingAttachment = att;
  }

  /** Send the current input text (and any staged attachment). Optimistically renders the sent
   *  message on a 200 (so it appears instantly, not only when the WS echoes it) and clears
   *  the input. Maps a failure to a friendly notice. Returns whether the send succeeded. */
  async send(): Promise<boolean> {
    const text = this.opts.input.value.trim();
    const attachment = this.pendingAttachment;
    if (!text && !attachment) return false;
    const name = this.opts.displayName() || undefined;
    const avatarUrl = this.opts.senderAvatarUrl?.() ?? null;
    this.opts.sendBtn.disabled = true;
    try {
      const res = await sendChatMessage(
        this.opts.httpBase,
        this.opts.code,
        { text, display_name: name, lang: this.opts.senderLang(), avatar_url: avatarUrl ?? undefined, attachment: attachment ?? undefined },
        this.opts.token(),
        this.fetchImpl,
      );
      this.append({
        id: res.id,
        sender_kind: this.opts.token() ? 'host' : 'guest',
        display_name: name || '',
        original: text,
        lang: this.opts.senderLang(),
        translations: {},
        created_at: res.created_at,
        avatar_url: avatarUrl,
        attachment,
      });
      this.opts.input.value = '';
      this.pendingAttachment = null;
      this.updateCounter();
      this.hideNotice();
      return true;
    } catch (err) {
      this.showNotice(this.messageFor(err));
      return false;
    } finally {
      this.opts.sendBtn.disabled = false;
    }
  }

  /** Map a ChatError status to the localized notice text. */
  private messageFor(err: unknown): string {
    const status = err instanceof ChatError ? err.status : 0;
    if (status === 429) return this.opts.strings.rateLimited;
    if (status === 422) return this.opts.strings.blocked;
    if (status === 403) return this.opts.strings.blocked; // chat turned off mid-session
    return this.opts.strings.genericError;
  }

  private showNotice(text: string): void {
    this.opts.notice.textContent = text;
    this.opts.notice.classList.remove('hidden');
    if (this.noticeTimer != null) clearTimeout(this.noticeTimer);
    this.noticeTimer = setTimeout(() => this.hideNotice(), 5_000);
  }

  private hideNotice(): void {
    this.opts.notice.classList.add('hidden');
    this.opts.notice.textContent = '';
    if (this.noticeTimer != null) {
      clearTimeout(this.noticeTimer);
      this.noticeTimer = null;
    }
  }

  private updateCounter(): void {
    const counter = this.opts.counter;
    if (!counter) return;
    const len = this.opts.input.value.length;
    counter.textContent = `${len}/${CHAT_TEXT_MAX}`;
    counter.classList.toggle('is-over', len > CHAT_TEXT_MAX);
  }
}

/** Format a file size in bytes to a human-readable string. */
export function formatFileSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

/** Palette for guest avatar backgrounds — rotates by the sum of char codes in the display name. */
const AVATAR_PALETTE = ['#10b981', '#8b5cf6', '#f59e0b', '#06b6d4', '#ec4899', '#f97316'];

/** Pick a consistent avatar background color from the sender's display name. Pure. */
export function avatarColor(name: string): string {
  const code = [...(name || 'G')].reduce((a, c) => a + c.charCodeAt(0), 0);
  return AVATAR_PALETTE[code % AVATAR_PALETTE.length];
}

/** Normalize an avatar URL for display at `size` px — mirrors `avatarUrl()` in auth.ts
 *  (not imported here to keep the /w/ bundle lean). Resizes Google CDN URLs; returns
 *  other URLs unchanged; returns null for absent/empty values. */
function processAvatarUrl(url: string | null | undefined, size = 36): string | null {
  if (!url) return null;
  if (/=s\d+(-c)?$/.test(url)) return url.replace(/=s\d+(-c)?$/, `=s${size}`);
  if (url.includes('googleusercontent.com')) return `${url}=s${size}`;
  return url;
}

/** Build a single chat message row for `event`, rendered in `myLang`. A host message carries a
 *  small "HOST" tag. The avatar shows the host's profile picture (when `hostAvatarUrl` is set
 *  and the image loads), falling back to colored initials on error — matching Meet chat behavior.
 *  Guest messages always use colored initials. The body uses `translations[myLang] ?? original`.
 *  Pure over `doc`. */
export function buildChatRow(
  doc: Document,
  event: ChatEvent,
  myLang: string,
  hostTag: string,
  hostAvatarUrl?: string | null,
): HTMLElement {
  const row = doc.createElement('div');
  row.className = `wv-chat-msg${event.sender_kind === 'host' ? ' is-host' : ''}`;
  row.setAttribute('data-id', event.id);

  const avatar = doc.createElement('span');
  avatar.className = 'wv-chat-avatar';
  avatar.setAttribute('aria-hidden', 'true');

  // Prefer per-message avatar_url (any sender — logged-in members and host both carry one),
  // then fall back to the host-level avatar for host-tagged messages, then initials.
  const resolvedUrl =
    processAvatarUrl(event.avatar_url, 36) ??
    (event.sender_kind === 'host' ? processAvatarUrl(hostAvatarUrl, 36) : null);
  if (resolvedUrl) {
    const img = doc.createElement('img');
    img.src = resolvedUrl;
    img.referrerPolicy = 'no-referrer';
    img.alt = '';
    img.style.cssText = 'width:100%;height:100%;object-fit:cover;border-radius:50%';
    img.addEventListener('error', () => {
      img.remove();
      avatar.textContent = (event.display_name || '?')[0].toUpperCase();
      avatar.style.background = '#3b82f6';
    });
    avatar.appendChild(img);
  } else {
    avatar.textContent = (event.display_name || '?')[0].toUpperCase();
    avatar.style.background =
      event.sender_kind === 'host' ? '#3b82f6' : avatarColor(event.display_name);
  }

  const content = doc.createElement('div');
  content.className = 'wv-chat-content';

  const nameLine = doc.createElement('div');
  nameLine.className = 'wv-chat-name-line';
  nameLine.textContent = event.display_name;
  if (event.sender_kind === 'host') {
    const tag = doc.createElement('span');
    tag.className = 'wv-chat-host-tag';
    tag.textContent = hostTag;
    nameLine.appendChild(tag);
  }

  const body = doc.createElement('div');
  body.className = 'wv-chat-body';
  const bodyText = renderChatText(event, myLang);
  if (bodyText) body.textContent = bodyText;

  content.append(nameLine, body);

  // Attachment bubble (image preview or file link).
  if (event.attachment) {
    const att = event.attachment;
    const attEl = doc.createElement('div');
    attEl.className = 'wv-chat-attachment';
    if (att.content_type.startsWith('image/')) {
      const img = doc.createElement('img');
      img.src = att.url;
      img.alt = att.name;
      img.className = 'wv-chat-att-img';
      img.loading = 'lazy';
      img.referrerPolicy = 'no-referrer';
      attEl.appendChild(img);
    } else {
      const link = doc.createElement('a');
      link.href = att.url;
      link.target = '_blank';
      link.rel = 'noopener noreferrer';
      link.className = 'wv-chat-att-file';
      link.textContent = `📎 ${att.name} (${formatFileSize(att.size)})`;
      attEl.appendChild(link);
    }
    content.appendChild(attEl);
  }

  row.append(avatar, content);
  return row;
}

/** Upload a file for a webinar chat message. Returns attachment metadata on success,
 *  or null on failure. The caller embeds the result in the next `sendChatMessage` body. */
export async function uploadWebinarFile(
  httpBase: string,
  code: string,
  file: File,
  onProgress?: (fraction: number) => void,
  token?: string | null,
): Promise<ChatAttachment | null> {
  return new Promise((resolve) => {
    const form = new FormData();
    form.append('file', file, file.name);
    const xhr = new XMLHttpRequest();
    xhr.open('POST', `${httpBase}/api/w/${encodeURIComponent(code)}/files`);
    if (token) xhr.setRequestHeader('Authorization', `Bearer ${token}`);
    if (onProgress && xhr.upload) {
      xhr.upload.onprogress = (e) => {
        if (e.lengthComputable) onProgress(e.loaded / e.total);
      };
    }
    xhr.withCredentials = true;
    xhr.onload = () => {
      if (xhr.status >= 200 && xhr.status < 300) {
        try {
          const data = JSON.parse(xhr.responseText) as unknown;
          const att = parseAttachment(data);
          resolve(att);
        } catch {
          resolve(null);
        }
      } else {
        resolve(null);
      }
    };
    xhr.onerror = () => resolve(null);
    xhr.onabort = () => resolve(null);
    xhr.send(form);
  });
}
