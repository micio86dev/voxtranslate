// Live webinar presence (audience count) over a WebSocket. The server exposes
// `GET /api/w/{code}/presence` and pushes JSON text frames `{"type":"count","count":N}`
// on every join/leave. Two consumers:
//   - the HOST studio (`host=true`) — watches the count but is NOT counted as audience;
//   - each PARTICIPANT (`host=false`, carrying its guest_id + optional lang) — counted.
//
// The socket carries no required client→server messages for now; we only read `count`
// frames and ignore anything else. On an unexpected drop we auto-reconnect with a
// capped exponential backoff, so a flaky network doesn't freeze the count. `close()`
// tears everything down and stops any pending reconnect.
//
// The WebSocket ctor is injectable so the parsing + reconnect logic unit-tests with a
// fake socket (mirroring hls-player.test.ts / webrtc.test.ts).

/** Parse a server presence frame. Returns the count for a well-formed
 *  `{type:"count",count:N}` text frame (N a finite number), else null — used to
 *  ignore malformed JSON, other frame types, or a non-numeric count. Pure. */
export function parsePresenceCount(data: string): number | null {
  let msg: unknown;
  try {
    msg = JSON.parse(data);
  } catch {
    return null; // not JSON
  }
  if (typeof msg !== "object" || msg === null) return null;
  const frame = msg as { type?: unknown; count?: unknown };
  if (frame.type !== "count") return null; // unknown/other frame type
  if (typeof frame.count !== "number" || !Number.isFinite(frame.count)) return null;
  return frame.count;
}

/**
 * A live-subtitle event pushed by the server over the SAME presence WS (webinar
 * Fase 2 translation). The server streams the host's speech transcribed + translated:
 *  - `final`   — a completed segment with the source `original`, its `lang`, and the
 *                per-language `translations` map (always incl. the source language).
 *  - `interim` — a still-streaming partial carrying only the source `text` + `lang`.
 * A viewer renders `translations[myLang]`, falling back to `original`.
 */
export type SubtitleEvent =
  | {
      kind: "final";
      /** The speaker's source words. */
      original: string;
      /** The detected source language code. */
      lang: string;
      /** Per-language translations (always includes `lang`). */
      translations: Record<string, string>;
    }
  | {
      kind: "interim";
      /** The still-streaming source words (shown as the interim caption). */
      text: string;
      /** The detected source language code. */
      lang: string;
    };

/** Parse a server subtitle frame. Returns a well-typed `SubtitleEvent` for a valid
 *  `{type:"subtitle",kind:"final"|"interim",…}` text frame, else null — so malformed
 *  JSON, other frame types (e.g. `count`), or a shape mismatch are ignored. Pure,
 *  mirroring `parsePresenceCount`. */
export function parseSubtitleFrame(data: string): SubtitleEvent | null {
  let msg: unknown;
  try {
    msg = JSON.parse(data);
  } catch {
    return null; // not JSON
  }
  if (typeof msg !== "object" || msg === null) return null;
  const frame = msg as {
    type?: unknown;
    kind?: unknown;
    original?: unknown;
    text?: unknown;
    lang?: unknown;
    translations?: unknown;
  };
  if (frame.type !== "subtitle") return null; // not a subtitle frame
  if (typeof frame.lang !== "string") return null;

  if (frame.kind === "final") {
    if (typeof frame.original !== "string") return null;
    const tr = frame.translations;
    // `translations` must be a plain object of string values (drop any non-string).
    if (typeof tr !== "object" || tr === null) return null;
    const translations: Record<string, string> = {};
    for (const [k, v] of Object.entries(tr as Record<string, unknown>)) {
      if (typeof v === "string") translations[k] = v;
    }
    return { kind: "final", original: frame.original, lang: frame.lang, translations };
  }

  if (frame.kind === "interim") {
    if (typeof frame.text !== "string") return null;
    return { kind: "interim", text: frame.text, lang: frame.lang };
  }

  return null; // unknown kind
}

/**
 * An auto-translated chat message pushed by the server over the SAME presence WS (webinar
 * Feature ⑤). Everyone in the room sends (host + guests); everyone reads each message
 * translated into their own language — a recipient renders `translations[myLang]`, falling
 * back to `original` (the exact rule live subtitles use). Mirrors `SubtitleEvent`.
 */
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
}

/** Parse a server chat frame. Returns a well-typed `ChatEvent` for a valid
 *  `{type:"chat",id,sender_kind,display_name,original,lang,translations,created_at}` text
 *  frame, else null — so malformed JSON, other frame types (`count`/`subtitle`), or a shape
 *  mismatch are ignored. Pure, mirroring `parseSubtitleFrame`. */
export function parseChatFrame(data: string): ChatEvent | null {
  let msg: unknown;
  try {
    msg = JSON.parse(data);
  } catch {
    return null; // not JSON
  }
  if (typeof msg !== "object" || msg === null) return null;
  const frame = msg as {
    type?: unknown;
    id?: unknown;
    sender_kind?: unknown;
    display_name?: unknown;
    original?: unknown;
    lang?: unknown;
    translations?: unknown;
    created_at?: unknown;
  };
  if (frame.type !== "chat") return null; // not a chat frame
  if (typeof frame.id !== "string") return null;
  if (frame.sender_kind !== "host" && frame.sender_kind !== "guest") return null;
  if (typeof frame.display_name !== "string") return null;
  if (typeof frame.original !== "string") return null;
  if (typeof frame.lang !== "string") return null;
  if (typeof frame.created_at !== "string") return null;
  const tr = frame.translations;
  // `translations` must be a plain object of string values (drop any non-string).
  if (typeof tr !== "object" || tr === null) return null;
  const translations: Record<string, string> = {};
  for (const [k, v] of Object.entries(tr as Record<string, unknown>)) {
    if (typeof v === "string") translations[k] = v;
  }
  return {
    id: frame.id,
    sender_kind: frame.sender_kind,
    display_name: frame.display_name,
    original: frame.original,
    lang: frame.lang,
    translations,
    created_at: frame.created_at,
  };
}

/** The subset of the WebSocket API we drive, so a fake can stand in for tests. */
export interface PresenceSocket {
  onopen: ((this: unknown, ev: unknown) => unknown) | null;
  onmessage: ((this: unknown, ev: { data: unknown }) => unknown) | null;
  onclose: ((this: unknown, ev: unknown) => unknown) | null;
  onerror: ((this: unknown, ev: unknown) => unknown) | null;
  close(): void;
}

/** A WebSocket constructor (the global `WebSocket`, or a fake in tests). */
export type PresenceSocketFactory = (url: string) => PresenceSocket;

export interface PresenceClientOptions {
  /** The app WS base, e.g. `wss://api.voxtranslate.app` (no trailing slash). */
  wsBase: string;
  /** The webinar's public code. */
  code: string;
  /** This client's stable anonymous id (UUID string). */
  guestId: string;
  /** True for the host studio (watches the count, not counted as audience). */
  host: boolean;
  /** Optional listener language (participant side, when known). */
  lang?: string | null;
  /** Called with the latest audience count on every `count` frame. */
  onCount: (count: number) => void;
  /** Called with each live-subtitle event (webinar Fase 2 translation) that arrives
   *  on the same presence WS. Optional — the host studio doesn't render captions. */
  onSubtitle?: (event: SubtitleEvent) => void;
  /** Called with each auto-translated chat message (webinar Feature ⑤) that arrives on
   *  the same presence WS. Optional — only wired when the webinar has chat enabled. */
  onChat?: (event: ChatEvent) => void;
  /** Injectable WebSocket ctor for tests. Defaults to the global `WebSocket`. */
  socketFactory?: PresenceSocketFactory;
  /** First reconnect delay in ms (doubles each attempt, capped). Default 1000. */
  reconnectBaseMs?: number;
  /** Reconnect delay cap in ms. Default 30000. */
  reconnectMaxMs?: number;
}

/** Build the presence WS URL from the app WS base + params. Pure/exported so the
 *  URL shape is testable without opening a socket. */
export function buildPresenceUrl(opts: {
  wsBase: string;
  code: string;
  guestId: string;
  host: boolean;
  lang?: string | null;
}): string {
  const base = opts.wsBase.replace(/\/+$/, "");
  const q = new URLSearchParams();
  q.set("guest_id", opts.guestId);
  q.set("host", opts.host ? "true" : "false");
  if (opts.lang) q.set("lang", opts.lang);
  return `${base}/api/w/${encodeURIComponent(opts.code)}/presence?${q.toString()}`;
}

/**
 * A live presence connection. Opens the WS on construction, invokes `onCount(n)`
 * for every valid `count` frame, and auto-reconnects with capped exponential
 * backoff if the socket drops. `close()` stops reconnects and closes the socket.
 */
export class PresenceClient {
  private readonly opts: Required<
    Pick<PresenceClientOptions, "reconnectBaseMs" | "reconnectMaxMs">
  > &
    PresenceClientOptions;
  private readonly factory: PresenceSocketFactory;
  private socket: PresenceSocket | null = null;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private attempt = 0;
  private closed = false;

  constructor(opts: PresenceClientOptions) {
    this.opts = {
      reconnectBaseMs: 1_000,
      reconnectMaxMs: 30_000,
      ...opts,
    };
    this.factory =
      opts.socketFactory ??
      ((url) => new WebSocket(url) as unknown as PresenceSocket);
    this.connect();
  }

  private url(): string {
    return buildPresenceUrl({
      wsBase: this.opts.wsBase,
      code: this.opts.code,
      guestId: this.opts.guestId,
      host: this.opts.host,
      lang: this.opts.lang,
    });
  }

  private connect(): void {
    if (this.closed) return;
    let sock: PresenceSocket;
    try {
      sock = this.factory(this.url());
    } catch {
      // Constructing the socket threw (bad URL / offline) — treat as a drop and
      // retry on backoff so the count self-heals when connectivity returns.
      this.scheduleReconnect();
      return;
    }
    this.socket = sock;
    sock.onopen = () => {
      this.attempt = 0; // a successful open resets the backoff
    };
    sock.onmessage = (ev) => {
      if (typeof ev.data !== "string") return; // ignore binary frames
      const n = parsePresenceCount(ev.data);
      if (n !== null) {
        this.opts.onCount(n);
        return; // a count frame is never also a subtitle frame
      }
      // Not a count frame — try the subtitle path (same WS carries both).
      const sub = this.opts.onSubtitle ? parseSubtitleFrame(ev.data) : null;
      if (sub) {
        this.opts.onSubtitle!(sub);
        return; // a subtitle frame is never also a chat frame
      }
      // Not a count or subtitle frame — try the chat path (same WS carries all three).
      const chat = this.opts.onChat ? parseChatFrame(ev.data) : null;
      if (chat) this.opts.onChat!(chat);
    };
    sock.onclose = () => {
      this.socket = null;
      this.scheduleReconnect();
    };
    sock.onerror = () => {
      // An error is usually followed by a close; closing here makes the drop
      // deterministic. onclose then schedules the reconnect.
      try {
        sock.close();
      } catch {
        /* already closing */
      }
    };
  }

  /** Schedule the next reconnect with exponential backoff, capped. No-op once
   *  closed or when a reconnect is already pending. */
  private scheduleReconnect(): void {
    if (this.closed || this.reconnectTimer != null) return;
    const delay = Math.min(
      this.opts.reconnectBaseMs * 2 ** this.attempt,
      this.opts.reconnectMaxMs,
    );
    this.attempt += 1;
    this.reconnectTimer = setTimeout(() => {
      this.reconnectTimer = null;
      this.connect();
    }, delay);
  }

  /** Close the socket and stop reconnecting. Idempotent. */
  close(): void {
    if (this.closed) return;
    this.closed = true;
    if (this.reconnectTimer != null) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    if (this.socket) {
      // Drop handlers first so the impending close() doesn't schedule a reconnect.
      this.socket.onopen = null;
      this.socket.onmessage = null;
      this.socket.onclose = null;
      this.socket.onerror = null;
      try {
        this.socket.close();
      } catch {
        /* best-effort */
      }
      this.socket = null;
    }
  }
}

/** Convenience factory mirroring the HlsPlayer construction style. */
export function connectPresence(opts: PresenceClientOptions): PresenceClient {
  return new PresenceClient(opts);
}
