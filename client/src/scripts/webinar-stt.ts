// Host STT bridge (webinar Fase 2 translation). While the host is broadcasting over
// WHIP (video/audio go P2P to the media server), the SAME mic track is ALSO streamed
// to the API's Qwen-Omni Realtime ingest for streaming STT → translation, whose subtitle frames
// fan back out to every viewer over the presence WS.
//
// Contract (server-defined):
//   GET /api/webinars/{id}/stt?token=<host JWT>   (query param — browsers can't set WS
//   headers, same as the presence WS). The host sends BINARY WebSocket frames, each a
//   PCM16 mono chunk @ 24 kHz (100ms). There are NO start/stop control frames — the
//   stream itself is the signal and CLOSING the socket flushes pending finals. Inbound
//   text frames are ignored by the server (AudioCapture's harmless start/stop text
//   frames included).
//
// Reliability: the reviewer flagged that a SILENT STT drop stops subtitles while the
// video keeps flowing. So if the socket drops while the broadcast is still live we
// reconnect with capped exponential backoff (mirroring PresenceClient) and re-bridge a
// fresh capture, so the new Qwen session starts on a clean PCM stream. `stop()` tears
// everything down and stops reconnects.
//
// The WebSocket ctor + the AudioCapture ctor are injectable so the open→bridge→reconnect
// logic unit-tests with fakes (mirroring webinar-presence.test.ts / audio-capture.test.ts).

/** The subset of the WebSocket API this bridge drives, so a fake can stand in. */
export interface SttSocket {
  onopen: ((this: unknown, ev: unknown) => unknown) | null;
  onclose: ((this: unknown, ev: unknown) => unknown) | null;
  onerror: ((this: unknown, ev: unknown) => unknown) | null;
  send(data: unknown): void;
  close(): void;
  readyState: number;
}

/** A WebSocket constructor (the global `WebSocket`, or a fake in tests). */
export type SttSocketFactory = (url: string) => SttSocket;

/** The subset of `AudioCapture` this bridge drives (start/stop/setStream). Injectable so
 *  the reconnect path can be tested without a real MediaRecorder. */
export interface SttCapture {
  start(): void;
  stop(): void;
}

/** Build the host STT ingest WS URL. Pure/exported so the URL shape (incl. the token
 *  query param + encoding) is testable without opening a socket. */
export function buildSttUrl(opts: { wsBase: string; webinarId: string; token: string }): string {
  const base = opts.wsBase.replace(/\/+$/, "");
  return `${base}/api/webinars/${encodeURIComponent(opts.webinarId)}/stt?token=${encodeURIComponent(
    opts.token,
  )}`;
}

export interface WebinarSttOptions {
  /** The app WS base, e.g. `wss://api.voxtranslate.app` (no trailing slash). */
  wsBase: string;
  /** The webinar id the host is broadcasting. */
  webinarId: string;
  /** The host's session JWT (carried in the `token` query param). */
  token: string;
  /** Build the AudioCapture that bridges the mic track into a socket. Called once per
   *  (re)connect so each fresh Qwen session starts on a clean PCM stream. */
  makeCapture: (socket: SttSocket) => SttCapture;
  /** Injectable WebSocket ctor for tests. Defaults to the global `WebSocket`. */
  socketFactory?: SttSocketFactory;
  /** First reconnect delay in ms (doubles each attempt, capped). Default 1000. */
  reconnectBaseMs?: number;
  /** Reconnect delay cap in ms. Default 30000. */
  reconnectMaxMs?: number;
}

/**
 * Streams the host's mic to the STT ingest WS for the life of a broadcast, reconnecting
 * with capped backoff if the socket drops while still live. Construct then `start()`;
 * `stop()` closes the socket (flushing finals) and cancels any pending reconnect.
 */
export class WebinarSttClient {
  private readonly opts: Required<
    Pick<WebinarSttOptions, "reconnectBaseMs" | "reconnectMaxMs">
  > &
    WebinarSttOptions;
  private readonly factory: SttSocketFactory;
  private socket: SttSocket | null = null;
  private capture: SttCapture | null = null;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private attempt = 0;
  private stopped = false;
  private started = false;

  constructor(opts: WebinarSttOptions) {
    this.opts = {
      reconnectBaseMs: 1_000,
      reconnectMaxMs: 30_000,
      ...opts,
    };
    this.factory =
      opts.socketFactory ?? ((url) => new WebSocket(url) as unknown as SttSocket);
  }

  /** Open the ingest socket and bridge the mic into it. Idempotent — a second call is a
   *  no-op (use one client per broadcast). */
  start(): void {
    if (this.started) return;
    this.started = true;
    this.connect();
  }

  private url(): string {
    return buildSttUrl({
      wsBase: this.opts.wsBase,
      webinarId: this.opts.webinarId,
      token: this.opts.token,
    });
  }

  private connect(): void {
    if (this.stopped) return;
    let sock: SttSocket;
    try {
      sock = this.factory(this.url());
    } catch {
      // Constructing the socket threw (bad URL / offline) — treat as a drop and retry so
      // STT self-heals when connectivity returns while the broadcast is still live.
      this.scheduleReconnect();
      return;
    }
    this.socket = sock;

    // Bridge the mic through a FRESH AudioCapture. A new MediaRecorder emits a
    // clean PCM stream for the new Qwen session.
    const cap = this.opts.makeCapture(sock);
    this.capture = cap;

    sock.onopen = () => {
      this.attempt = 0; // a successful open resets the backoff
      // Begin (or resume, after a reconnect) streaming audio into this socket.
      cap.start();
    };
    sock.onclose = () => {
      // The capture bound to the dead socket can't reach it anymore — stop it before
      // scheduling a reconnect so a new one binds to the new socket.
      this.stopCapture();
      this.socket = null;
      this.scheduleReconnect();
    };
    sock.onerror = () => {
      // An error is usually followed by a close; closing here makes the drop
      // deterministic. onclose then stops the capture + schedules the reconnect.
      try {
        sock.close();
      } catch {
        /* already closing */
      }
    };
  }

  /** Schedule the next reconnect with exponential backoff, capped. No-op once stopped
   *  or when a reconnect is already pending. */
  private scheduleReconnect(): void {
    if (this.stopped || this.reconnectTimer != null) return;
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

  private stopCapture(): void {
    if (this.capture) {
      try {
        this.capture.stop();
      } catch {
        /* best-effort */
      }
      this.capture = null;
    }
  }

  /** Close the ingest socket (which flushes pending finals server-side) and stop
   *  reconnecting. Idempotent — safe to call from the end-broadcast path. */
  stop(): void {
    if (this.stopped) return;
    this.stopped = true;
    if (this.reconnectTimer != null) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    this.stopCapture();
    if (this.socket) {
      // Drop handlers first so the impending close() doesn't schedule a reconnect.
      this.socket.onopen = null;
      this.socket.onclose = null;
      this.socket.onerror = null;
      try {
        this.socket.close();
      } catch {
        /* best-effort — the server also reaps the stream on ingest timeout */
      }
      this.socket = null;
    }
  }
}
