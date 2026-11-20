// Live Connect — the conversation engine (spec 0121).
//
// Two phones, two microphones, two independent translation streams. This is the DOM-free
// half: it owns the socket, the microphone, the playback graph, the echo guard, the wake
// lock and the state machine, and reports everything through callbacks so `view.ts` can be
// replaced (or driven by a test) without touching any of it. It is modelled on
// `talk/conversation.ts`, which it reuses instead of copying wherever the behaviour is the
// same (backoff, mic error classification, capture, playback, the guard).
//
// What is different from Talk, and why:
//  - Identity comes from the SERVER. The URL carries only the live token — no `lang`, no
//    `engine` — so a client cannot claim to be someone else or pick a pricier tier.
//  - Two directions are live at once, by design. Subtitles are routed by `speaker_id`:
//    what the PARTNER says is shown in my language, big; what I say is shown small.
//  - Reconnects REUSE the token, and before every retry the session is probed over REST: a
//    refused WebSocket upgrade surfaces in a browser only as a bare close (no status), so
//    without the probe "this conversation is over" and "your signal dropped" look identical
//    and the client would retry a finished session until it gave up.
//  - Leaving the page is NOT ending the conversation: `release()` frees the microphone and
//    socket but keeps the stored session, so a reload rejoins.

import { buildAudioConstraints, loadNoisyEnv } from '../mic-constraints';
import { createDenoiser, type Denoiser } from '../denoise';
import { PcmCapture } from '../pcm-capture';
import { pcmPlayback } from '../pcm-playback';
import { setNetworkDegraded } from '../net-status';
import { releaseWakeLock, requestWakeLock } from '../wake-lock';
import { WS_BASE } from '../auth';
import { SelfAudioGuard, startLevelMonitor } from '../talk/self-audio-guard';
import {
  RECONNECT_MAX_ATTEMPTS,
  backoffDelay,
  classifyMediaError,
  browserSupported,
} from '../talk/conversation';
import { endSession, getSession, type ApiResult, type SessionInfo } from './api';
import {
  clearStoredSession,
  loadHeadphones,
  saveHeadphones,
  saveStoredSession,
} from './session-store';
import {
  initialContext,
  transition,
  wantsWakeLock,
  type EndReason,
  type FailureKind,
  type LiveContext,
  type LiveEvent,
  type Partner,
} from './session-machine';

export { RECONNECT_MAX_ATTEMPTS, backoffDelay, classifyMediaError, browserSupported };

/** Past exchanges kept for the history list. Beyond this it is scrollback, not context. */
export const HISTORY_LIMIT = 30;
/** `start` is re-sent at most this many times after the server drops the upstream session. */
export const START_RETRY_MAX = 3;
const START_RETRY_BASE_MS = 800;

/** One finished sentence, either direction. */
export interface Exchange {
  id: number;
  who: 'partner' | 'me';
  original: string;
  originalLang: string;
  /** The translation. Empty when there was none (same language, or the engine sent nothing). */
  translated: string;
  translatedLang: string;
}

/** The line currently on screen for one speaker. */
export interface CaptionLine {
  /** Partner: the text in MY language (the big line). Me: what I said. */
  text: string;
  /** Partner only: what they actually said, in their language. */
  original: string;
  /** The language `original` is in. */
  lang: string;
  /** Finished; stays on screen until the next utterance replaces it. */
  settled: boolean;
}

export interface Captions {
  partner: CaptionLine | null;
  mine: CaptionLine | null;
  history: Exchange[];
}

/** The minimum socket surface, so tests can supply a fake. */
export interface LiveSocket {
  send: (data: string | ArrayBufferLike | Blob) => void;
  close: (code?: number, reason?: string) => void;
  readyState: number;
  onopen: (() => void) | null;
  onclose: ((ev: { code: number }) => void) | null;
  onerror: (() => void) | null;
  onmessage: ((ev: { data: unknown }) => void) | null;
}

/** What the engine needs from a capture: `PcmCapture` satisfies it. */
export interface LiveCapture {
  start(): void;
  stop(): void;
  setMuted(muted: boolean): void;
  setGated(gated: boolean): void;
}

export interface LiveCallbacks {
  onState: (ctx: LiveContext) => void;
  onCaptions: (captions: Captions) => void;
  onBalance?: (balance: number) => void;
  onLowBalance?: (balance: number) => void;
  /** A recoverable, user-facing problem. `code` is stable; the view picks the copy. */
  onNotice?: (code: string) => void;
  /** Both people speak the same language, so nothing will be translated. */
  onSameLanguage?: (myLang: string, partnerLang: string) => void;
  onMuteChange?: (muted: boolean) => void;
  onBargeIn?: () => void;
}

export interface LiveConversationOptions extends LiveCallbacks {
  sessionId: string;
  /** The Live token. NOT the account JWT. */
  token: string;
  wsPath: string;
  role: 'host' | 'guest' | 'staff';
  myLang: string;
  myPeerId: string;
  /** Host only: persisted so a reload can show the invite again. */
  inviteUrl?: string;
  headphones?: boolean;
  // --- injectable seams, all defaulted -------------------------------------
  createSocket?: (url: string) => LiveSocket;
  createCapture?: (stream: MediaStream, socket: LiveSocket) => LiveCapture;
  getMedia?: (constraints: MediaStreamConstraints) => Promise<MediaStream>;
  makeDenoiser?: (stream: MediaStream) => Promise<Denoiser>;
  now?: () => number;
  random?: () => number;
  setTimeoutImpl?: typeof setTimeout;
  clearTimeoutImpl?: typeof clearTimeout;
  wakeLock?: { request: () => Promise<void>; release: () => Promise<void> };
  playback?: typeof pcmPlayback;
  getSessionImpl?: (sessionId: string, token: string) => Promise<ApiResult<SessionInfo>>;
  endSessionImpl?: (sessionId: string, token: string) => Promise<unknown>;
  sinkSupported?: () => boolean;
  setSink?: (deviceId: string) => Promise<void>;
  enumerateDevices?: () => Promise<MediaDeviceInfo[]>;
}

/** The `/ws/live` URL. The token is the ONLY parameter: identity comes from the server. */
export function buildLiveUrl(wsPath: string, token: string): string {
  return `${WS_BASE}${wsPath}?token=${encodeURIComponent(token)}`;
}

/**
 * Whether the browser can route Web Audio to a chosen output.
 *
 * `AudioContext.setSinkId` only (Chromium). `HTMLMediaElement.setSinkId` exists in more
 * browsers but cannot be applied here: translated speech plays through an AudioWorklet on
 * an AudioContext, not an <audio> element. iOS Safari has neither, so the control is hidden
 * there rather than shown and broken.
 */
export function outputSelectionSupported(
  win: Partial<typeof globalThis> = globalThis as Partial<typeof globalThis>,
): boolean {
  const proto = (win.AudioContext as { prototype?: Record<string, unknown> } | undefined)?.prototype;
  return typeof proto?.setSinkId === 'function';
}

/** Microphone constraints, resolved at call time so the noisy-environment toggle applies to the next conversation. */
function micConstraints(): MediaStreamConstraints {
  return { audio: buildAudioConstraints(), video: false };
}

/** Normalised primary language subtag, for "are these the same language". */
function primaryLang(code: string): string {
  return code.toLowerCase().split(/[-_]/)[0];
}

const KNOWN_END_REASONS: ReadonlySet<string> = new Set<EndReason>([
  'ended_by_host',
  'ended_by_guest',
  'ended_by_staff',
  'balance_exhausted',
  'max_duration',
  'abandoned',
  'billing_error',
  'expired',
  'desk_revoked',
  'desk_regenerated',
]);

function asEndReason(reason: unknown, status: unknown): EndReason {
  if (typeof reason === 'string' && KNOWN_END_REASONS.has(reason)) return reason as EndReason;
  // `expired` is its own status; an unrecognised reason on an `ended` session is still an
  // ending, and the least presumptuous label for it is the neutral one.
  return status === 'expired' ? 'expired' : 'abandoned';
}

/** `room_joined.peers[]` carries the id as `id` (the server's `PeerInfo`); `peer_joined` as `peer_id`. */
function peerIdOf(p: Record<string, unknown>): string {
  return String(p.id ?? p.peer_id ?? '');
}

export class LiveConversation {
  private ctx: LiveContext = initialContext();
  private socket: LiveSocket | null = null;
  private stream: MediaStream | null = null;
  /** The RAW device stream, when RNNoise sits in front of it. Stopping the denoised tracks
   *  does not release the microphone — that handle lives here. */
  private rawStream: MediaStream | null = null;
  private denoiser: Denoiser | null = null;
  private capture: LiveCapture | null = null;
  private readonly guard: SelfAudioGuard;
  private stopLevelMonitor: (() => void) | null = null;
  private levelCtx: AudioContext | null = null;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private startRetryTimer: ReturnType<typeof setTimeout> | null = null;
  private attempt = 0;
  private startRetries = 0;
  private muted = false;
  private wakeHeld = false;
  private headphonesOn: boolean;
  private warnedSameLangFor: string | null = null;

  private partnerLine: CaptionLine | null = null;
  private mineLine: CaptionLine | null = null;
  private history: Exchange[] = [];
  /** The last finished sentence per speaker, held back from the history until the NEXT one
   *  begins, so exactly one surface shows a given sentence at a time (same rule as Talk). */
  private pendingPartner: Exchange | null = null;
  private pendingMine: Exchange | null = null;
  private exchangeId = 0;

  private outputDeviceId: string | null = null;
  private appliedSink: string | null = null;

  private readonly now: () => number;
  private readonly random: () => number;
  private readonly setTimeoutImpl: typeof setTimeout;
  private readonly clearTimeoutImpl: typeof clearTimeout;
  private readonly playback: typeof pcmPlayback;

  constructor(private readonly opts: LiveConversationOptions) {
    this.now = opts.now ?? (() => Date.now());
    this.random = opts.random ?? Math.random;
    // Wrapped, never stored bare: a browser throws "Illegal invocation" when `setTimeout` is
    // called with the instance as its receiver (`this.setTimeoutImpl(...)`).
    this.setTimeoutImpl =
      opts.setTimeoutImpl ??
      (((fn: () => void, ms?: number) => globalThis.setTimeout(fn, ms)) as unknown as typeof setTimeout);
    this.clearTimeoutImpl =
      opts.clearTimeoutImpl ??
      (((id?: ReturnType<typeof setTimeout>) => globalThis.clearTimeout(id)) as unknown as typeof clearTimeout);
    this.playback = opts.playback ?? pcmPlayback;
    this.headphonesOn = opts.headphones ?? loadHeadphones();
    this.guard = new SelfAudioGuard({
      // Hold my microphone while the partner's translation is audible: on one device the
      // speaker feeds the mic. Barge-in reopens it when I talk over the translation.
      setGated: (gated) => this.capture?.setGated(gated),
      cancelPlayback: () => this.playback.reset(),
      onBargeIn: () => this.opts.onBargeIn?.(),
      now: this.now,
    });
    this.guard.setMode(this.headphonesOn ? 'open' : 'gate');
  }

  state(): LiveContext {
    return this.ctx;
  }

  isMuted(): boolean {
    return this.muted;
  }

  headphones(): boolean {
    return this.headphonesOn;
  }

  /** Which language I speak and which one the other person hears (and vice versa). */
  direction(): { myLang: string; partnerLang: string | null } {
    return { myLang: this.opts.myLang, partnerLang: this.ctx.partner?.lang ?? null };
  }

  /** Acquire the microphone, open the socket, and wait for the other person. */
  async start(): Promise<void> {
    if (!this.dispatch({ type: 'START_REQUESTED' })) return;
    this.persist();

    // Unlock playback inside the user gesture that led here — iOS/Safari will not start an
    // AudioContext later, and the first translation would be silent.
    this.playback.unlock();
    this.playback.setPlayingListener((playing) => this.onPlaybackChange(playing));

    const getMedia =
      this.opts.getMedia ?? ((c: MediaStreamConstraints) => navigator.mediaDevices.getUserMedia(c));
    let raw: MediaStream;
    try {
      raw = await getMedia(micConstraints());
    } catch (err) {
      this.finish({ type: 'MIC_DENIED', kind: classifyMediaError(err) as FailureKind });
      return;
    }
    this.rawStream = raw;
    if (loadNoisyEnv()) {
      // Noisy mode turned the BROWSER filter off in `micConstraints()`, so RNNoise takes its
      // place; otherwise the microphone would be completely unfiltered.
      const make = this.opts.makeDenoiser ?? ((st: MediaStream) => createDenoiser(st));
      this.denoiser = await make(raw);
      this.stream = this.denoiser.stream;
    } else {
      this.stream = raw;
    }
    // The user may have ended it while the permission prompt was up.
    if (this.ctx.phase !== 'requesting_mic') {
      this.releaseStream();
      return;
    }
    this.dispatch({ type: 'MIC_GRANTED' });
    this.startLevelWatch();
    this.connect();
  }

  /**
   * Mute my microphone. The server is told (so the partner sees it, and a muted phone sends
   * nothing to the engine) AND the local capture is released: stopping it also pauses the
   * upstream session, so a muted owner is not billed for silence.
   */
  setMuted(muted: boolean): void {
    if (this.muted === muted) return;
    this.muted = muted;
    this.sendControl({ type: 'mute_audio', muted });
    this.capture?.setMuted(muted);
    this.opts.onMuteChange?.(muted);
  }

  /** "I am wearing headphones": the guard stops gating my microphone. Persisted. */
  setHeadphones(on: boolean): void {
    this.headphonesOn = on;
    this.guard.setMode(on ? 'open' : 'gate');
    saveHeadphones(on);
  }

  canSelectOutput(): boolean {
    return (this.opts.sinkSupported ?? (() => outputSelectionSupported()))();
  }

  /** Route the translated speech to `deviceId`. False when unsupported or the browser refuses. */
  async setOutputDevice(deviceId: string): Promise<boolean> {
    if (!this.canSelectOutput()) return false;
    this.outputDeviceId = deviceId;
    return this.applySink();
  }

  /** Audio outputs, best effort: labels are empty until the user has granted the microphone. */
  async listOutputDevices(): Promise<MediaDeviceInfo[]> {
    try {
      const list =
        this.opts.enumerateDevices ?? (() => navigator.mediaDevices.enumerateDevices());
      return (await list()).filter((d) => d.kind === 'audiooutput');
    } catch {
      return [];
    }
  }

  /** End the conversation for both phones, and release everything. */
  end(): void {
    if (this.isTerminal()) return;
    // Best effort: the server also notices the socket closing, but this is what tells the
    // partner "ended by the other person" rather than "connection lost".
    void Promise.resolve(
      (this.opts.endSessionImpl ?? endSession)(this.opts.sessionId, this.opts.token),
    ).catch(() => {});
    this.finish({ type: 'STOP_REQUESTED' });
  }

  /**
   * Free the microphone, socket and timers WITHOUT ending the conversation. For
   * `pagehide`: the session stays stored, so a reload rejoins instead of stranding the
   * other person.
   */
  release(): void {
    this.teardown();
  }

  // --- internals ---------------------------------------------------------

  private isTerminal(): boolean {
    return this.ctx.phase === 'ended' || this.ctx.phase === 'failed';
  }

  private persist(): void {
    saveStoredSession({
      sessionId: this.opts.sessionId,
      token: this.opts.token,
      role: this.opts.role,
      myLang: this.opts.myLang,
      myPeerId: this.opts.myPeerId,
      wsPath: this.opts.wsPath,
      ...(this.opts.inviteUrl ? { inviteUrl: this.opts.inviteUrl } : {}),
    });
  }

  private dispatch(event: LiveEvent): boolean {
    const before = this.ctx;
    const { context, accepted } = transition(before, event);
    this.ctx = context;
    if (context !== before) {
      this.reconcileWakeLock();
      if ((before.phase === 'reconnecting') !== (context.phase === 'reconnecting')) {
        setNetworkDegraded(context.phase === 'reconnecting');
      }
      this.opts.onState(context);
    }
    return accepted;
  }

  /** The wake lock is a FUNCTION of the phase, reconciled in one place. */
  private reconcileWakeLock(): void {
    this.setWake(wantsWakeLock(this.ctx));
  }

  private setWake(want: boolean): void {
    if (want === this.wakeHeld) return;
    this.wakeHeld = want;
    const wl = this.opts.wakeLock;
    // Fire-and-forget: nothing waits on a screen lock, and a rejection is routine.
    void (want
      ? wl
        ? wl.request()
        : requestWakeLock()
      : wl
        ? wl.release()
        : releaseWakeLock());
  }

  /** Release everything, record what the user should see last, then report the new state. */
  private finish(event: LiveEvent): void {
    if (this.isTerminal()) return;
    this.flushPending();
    this.teardown();
    // A microphone problem or a dropped network says nothing about the SESSION, which the
    // server still holds: keep the stored credentials so "try again" (or a reload) can rejoin.
    if (event.type !== 'MIC_DENIED' && event.type !== 'RECONNECT_EXHAUSTED') clearStoredSession();
    this.dispatch(event);
  }

  private teardown(): void {
    this.clearReconnect();
    this.clearStartRetry();
    this.capture?.stop();
    this.capture = null;
    this.stopLevelWatch();
    this.releaseStream();
    const sock = this.socket;
    if (sock) {
      this.socket = null; // detach first, so onclose cannot schedule a reconnect
      sock.onopen = sock.onclose = sock.onerror = sock.onmessage = null;
      try {
        sock.close(1000, 'ended');
      } catch {
        // Already closing. Nothing to recover.
      }
    }
    this.playback.setPlayingListener(null);
    this.playback.stop();
    this.guard.reset();
    this.setWake(false);
    setNetworkDegraded(false);
  }

  private connect(): void {
    const url = buildLiveUrl(this.opts.wsPath, this.opts.token);
    let sock: LiveSocket;
    try {
      sock = (this.opts.createSocket ?? defaultSocket)(url);
    } catch {
      // Bad URL or offline. Treat exactly like a drop so the retry path is one path.
      this.scheduleReconnect();
      return;
    }
    this.socket = sock;

    sock.onopen = () => {
      const reconnected = this.ctx.phase === 'reconnecting';
      this.dispatch(reconnected ? { type: 'RECONNECT_SUCCEEDED' } : { type: 'SOCKET_OPEN' });
      this.attachCapture(sock);
      if (this.muted) this.sendControl({ type: 'mute_audio', muted: true });
      else this.sendStart();
    };
    sock.onmessage = (ev) => {
      if (typeof ev.data === 'string') this.handleFrame(ev.data);
    };
    sock.onerror = () => {
      // `onclose` always follows; handling both would double-count the attempt.
    };
    sock.onclose = () => {
      if (this.socket !== sock) return; // a socket we already replaced or detached
      this.socket = null;
      this.capture?.stop();
      this.capture = null;
      this.dispatch({ type: 'SOCKET_CLOSED' });
      this.scheduleReconnect();
    };
  }

  private attachCapture(sock: LiveSocket): void {
    if (!this.stream) return;
    this.capture?.stop();
    // Always PCM: the server sends `capture_format{pcm:true}` and feeding it WebM produces
    // silence with no error at all.
    const make =
      this.opts.createCapture ??
      ((s: MediaStream, w: LiveSocket) => new PcmCapture(s, w as unknown as WebSocket));
    this.capture = make(this.stream, sock);
    // Muted: keep the capture ready but do not start it; `setMuted(false)` starts it.
    if (!this.muted) this.capture.start();
    if (this.guard.isGated()) this.capture.setGated(true);
  }

  private sendControl(msg: Record<string, unknown>): void {
    const sock = this.socket;
    if (!sock || sock.readyState !== 1) return;
    try {
      sock.send(JSON.stringify(msg));
    } catch {
      // A socket that dies mid-send closes next; the reconnect path owns it.
    }
  }

  /** Idempotent on the server: it opens upstream sessions lazily per target language. */
  private sendStart(): void {
    if (this.muted) return;
    this.sendControl({ type: 'start' });
  }

  private scheduleReconnect(): void {
    if (this.isTerminal()) return;
    if (this.attempt >= RECONNECT_MAX_ATTEMPTS) {
      this.finish({ type: 'RECONNECT_EXHAUSTED' });
      return;
    }
    const delay = backoffDelay(this.attempt, this.random);
    this.attempt += 1;
    this.clearReconnect();
    this.reconnectTimer = this.setTimeoutImpl(() => {
      this.reconnectTimer = null;
      void this.reconnectNow();
    }, delay);
  }

  /** Ask the server whether the session still exists BEFORE reopening the socket. */
  private async reconnectNow(): Promise<void> {
    const probe = this.opts.getSessionImpl ?? getSession;
    const result = await probe(this.opts.sessionId, this.opts.token);
    if (this.isTerminal() || this.ctx.phase !== 'reconnecting') return; // ended while probing
    if (result.ok) {
      const status = result.data.session.status;
      if (status === 'ended' || status === 'expired') {
        this.finish({ type: 'SESSION_ENDED', reason: asEndReason(undefined, status) });
        return;
      }
    } else if (result.status === 410 || result.code === 'session_ended') {
      this.finish({ type: 'SESSION_ENDED', reason: 'abandoned' });
      return;
    } else if (result.code === 'token_expired') {
      this.finish({ type: 'SESSION_ENDED', reason: 'expired' });
      return;
    } else if (result.status === 401 || result.status === 403) {
      // The token is no good and will not become good by retrying.
      this.finish({ type: 'FATAL', kind: 'unknown' });
      return;
    }
    // Anything else (a network error on the probe itself, a 5xx) is transient: try the socket.
    this.connect();
  }

  private clearReconnect(): void {
    if (this.reconnectTimer !== null) {
      this.clearTimeoutImpl(this.reconnectTimer);
      this.reconnectTimer = null;
    }
  }

  private clearStartRetry(): void {
    if (this.startRetryTimer !== null) {
      this.clearTimeoutImpl(this.startRetryTimer);
      this.startRetryTimer = null;
    }
  }

  // --- frames ------------------------------------------------------------

  private handleFrame(raw: string): void {
    let msg: Record<string, unknown>;
    try {
      msg = JSON.parse(raw) as Record<string, unknown>;
    } catch {
      return; // malformed frames are ignored, never fatal
    }
    switch (msg.type) {
      case 'room_joined': {
        this.attempt = 0; // the connection is genuinely healthy now
        const peers = Array.isArray(msg.peers) ? (msg.peers as Array<Record<string, unknown>>) : [];
        const other = peers.find((p) => peerIdOf(p) !== this.opts.myPeerId);
        this.dispatch({ type: 'ROOM_JOINED', partner: other ? this.toPartner(other) : null });
        this.checkSameLanguage();
        break;
      }
      case 'peer_joined': {
        if (msg.peer_id === this.opts.myPeerId) break;
        this.dispatch({ type: 'PEER_JOINED', partner: this.toPartner(msg) });
        this.checkSameLanguage();
        // The partner's language is a new translation target.
        this.sendStart();
        break;
      }
      case 'peer_left':
        this.dispatch({ type: 'PEER_LEFT', peerId: String(msg.peer_id ?? '') });
        break;
      case 'peer_muted':
        if (msg.kind === 'audio') {
          this.dispatch({ type: 'PEER_MUTED', peerId: String(msg.peer_id ?? ''), muted: msg.muted === true });
        }
        break;
      case 'subtitle_interim':
        this.onInterim(msg);
        break;
      case 'subtitle_final':
        this.onFinal(msg);
        break;
      case 'translated_audio': {
        const b64 = typeof msg.pcm16_b64 === 'string' ? msg.pcm16_b64 : '';
        // One stream per speaker: each direction has a single speaker, so `speaker_id` alone
        // keys the playback buffer's monotonic-seq guard.
        if (b64) this.playback.enqueue(String(msg.speaker_id ?? ''), Number(msg.seq ?? 0), b64);
        this.startRetries = 0;
        break;
      }
      case 'balance_update':
        this.opts.onBalance?.(Number(msg.balance ?? 0));
        break;
      case 'low_balance':
        this.opts.onLowBalance?.(Number(msg.balance ?? 0));
        break;
      case 'balance_exhausted':
        // `session_state` follows, but the verdict is already known.
        this.finish({ type: 'SESSION_ENDED', reason: 'balance_exhausted' });
        break;
      case 'session_state':
        if (msg.status === 'ended' || msg.status === 'expired') {
          this.finish({ type: 'SESSION_ENDED', reason: asEndReason(msg.reason, msg.status) });
        }
        break;
      case 'error':
        this.onError(typeof msg.code === 'string' ? msg.code : 'unknown');
        break;
      default:
        // `capture_format` and every other meeting frame: nothing to do. Capture is always PCM.
        break;
    }
  }

  private onError(code: string): void {
    switch (code) {
      case 'insufficient_balance':
      case 'insufficient_credits':
        this.finish({ type: 'FATAL', kind: 'credits' });
        return;
      case 'live_busy':
        this.finish({ type: 'FATAL', kind: 'busy' });
        return;
      case 'audio_session_closed':
      case 'engine_unavailable':
        this.retryStart(code);
        return;
      default:
        // billing_unavailable, server_busy, anything new: say so and keep listening.
        this.opts.onNotice?.(code);
    }
  }

  /** The upstream session dropped: ask for a new one, a few times, then tell the user. */
  private retryStart(code: string): void {
    if (this.startRetries >= START_RETRY_MAX) {
      this.opts.onNotice?.(code);
      return;
    }
    this.startRetries += 1;
    this.clearStartRetry();
    this.startRetryTimer = this.setTimeoutImpl(() => {
      this.startRetryTimer = null;
      this.sendStart();
    }, START_RETRY_BASE_MS * this.startRetries);
  }

  private toPartner(p: Record<string, unknown>): Partner {
    return {
      peerId: peerIdOf(p),
      name: typeof p.user_name === 'string' ? p.user_name : '',
      lang: typeof p.lang === 'string' ? p.lang : '',
      muted: false,
    };
  }

  private checkSameLanguage(): void {
    const partner = this.ctx.partner;
    if (!partner || this.warnedSameLangFor === partner.peerId) return;
    if (partner.lang && primaryLang(partner.lang) === primaryLang(this.opts.myLang)) {
      this.warnedSameLangFor = partner.peerId;
      this.opts.onSameLanguage?.(this.opts.myLang, partner.lang);
    }
  }

  // --- subtitles ---------------------------------------------------------

  private isMine(msg: Record<string, unknown>): boolean {
    return msg.speaker_id === this.opts.myPeerId;
  }

  private onInterim(msg: Record<string, unknown>): void {
    const text = typeof msg.text === 'string' ? msg.text : '';
    const original = typeof msg.original === 'string' ? msg.original : '';
    this.startRetries = 0;
    if (this.isMine(msg)) {
      if (this.mineLine?.settled) this.flushPending();
      // What I am saying, in my own words — never the partner-language rendering of it.
      this.mineLine = { text: original || text, original: '', lang: this.opts.myLang, settled: false };
      this.guard.setSpeechActive(true);
    } else {
      if (this.partnerLine?.settled) this.flushPending();
      this.partnerLine = {
        text,
        original,
        lang: this.ctx.partner?.lang ?? '',
        settled: false,
      };
    }
    this.emitCaptions();
  }

  private onFinal(msg: Record<string, unknown>): void {
    const original = typeof msg.original === 'string' ? msg.original.trim() : '';
    const spokenLang = typeof msg.lang === 'string' ? msg.lang : '';
    const translations = (msg.translations ?? {}) as Record<string, unknown>;
    const pick = (lang: string): string => {
      const v = translations[lang];
      return typeof v === 'string' ? v.trim() : '';
    };
    this.startRetries = 0;

    if (this.isMine(msg)) {
      // My sentence is over: from here the only voice that could reach the microphone is
      // the partner's translation, so normal echo protection resumes.
      this.guard.setSpeechActive(false);
      const partnerLang = this.ctx.partner?.lang ?? '';
      this.flushPending('me');
      this.pendingMine = this.exchange('me', original, spokenLang || this.opts.myLang, pick(partnerLang), partnerLang);
      this.mineLine = { text: original, original: '', lang: spokenLang || this.opts.myLang, settled: true };
    } else {
      const translated = pick(this.opts.myLang);
      this.flushPending('partner');
      this.pendingPartner = this.exchange('partner', original, spokenLang, translated, this.opts.myLang);
      // No translation into my language (same language, or the engine sent nothing): the
      // original is the best thing to show rather than a blank card.
      this.partnerLine = { text: translated || original, original, lang: spokenLang, settled: true };
    }
    this.emitCaptions();
  }

  private exchange(
    who: Exchange['who'],
    original: string,
    originalLang: string,
    translated: string,
    translatedLang: string,
  ): Exchange {
    this.exchangeId += 1;
    return { id: this.exchangeId, who, original, originalLang, translated, translatedLang };
  }

  /** Move held sentences into the history. Idempotent; no argument flushes both. */
  private flushPending(only?: Exchange['who']): void {
    const out: Exchange[] = [];
    if (only !== 'me' && this.pendingPartner) {
      out.push(this.pendingPartner);
      this.pendingPartner = null;
    }
    if (only !== 'partner' && this.pendingMine) {
      out.push(this.pendingMine);
      this.pendingMine = null;
    }
    if (!out.length) return;
    out.sort((a, b) => a.id - b.id);
    this.history = [...this.history, ...out].slice(-HISTORY_LIMIT);
    this.emitCaptions();
  }

  private emitCaptions(): void {
    this.opts.onCaptions({
      partner: this.partnerLine,
      mine: this.mineLine,
      history: this.history,
    });
  }

  // --- audio plumbing ----------------------------------------------------

  private onPlaybackChange(playing: boolean): void {
    this.guard.onPlaybackChange(playing);
    // The first audible sample is the first moment the graph certainly exists.
    if (playing && this.outputDeviceId && this.appliedSink !== this.outputDeviceId) {
      void this.applySink();
    }
  }

  private async applySink(): Promise<boolean> {
    const id = this.outputDeviceId;
    if (!id) return false;
    try {
      await (this.opts.setSink ?? ((d: string) => defaultSetSink(this.playback, d)))(id);
      this.appliedSink = id;
      return true;
    } catch {
      return false;
    }
  }

  private startLevelWatch(): void {
    if (!this.stream) return;
    try {
      const Ctor =
        window.AudioContext ??
        (window as unknown as { webkitAudioContext: typeof AudioContext }).webkitAudioContext;
      const ctx = new Ctor();
      const analyser = ctx.createAnalyser();
      analyser.fftSize = 512;
      ctx.createMediaStreamSource(this.stream).connect(analyser);
      this.levelCtx = ctx;
      this.stopLevelMonitor = startLevelMonitor(analyser, (level) => this.guard.onLevel(level));
    } catch {
      // No analyser means no barge-in; the gate still opens on the playback edge.
      this.stopLevelMonitor = null;
    }
  }

  private stopLevelWatch(): void {
    this.stopLevelMonitor?.();
    this.stopLevelMonitor = null;
    if (this.levelCtx) {
      void this.levelCtx.close().catch(() => {});
      this.levelCtx = null;
    }
  }

  private releaseStream(): void {
    this.stream?.getTracks().forEach((t) => t.stop());
    // The denoised tracks come out of an AudioContext; stopping them leaves the device
    // open. Release the raw stream too, and close the graph behind it.
    this.rawStream?.getTracks().forEach((t) => t.stop());
    this.rawStream = null;
    void this.denoiser?.stop();
    this.denoiser = null;
    this.stream = null;
  }
}

function defaultSocket(url: string): LiveSocket {
  return new WebSocket(url) as unknown as LiveSocket;
}

/**
 * Apply `setSinkId` to the playback graph's AudioContext.
 *
 * `PcmPlayback` keeps its context private and exposes no sink API, and its file is outside
 * this feature's edit surface, so the context is reached by a narrow, typed cast. A proper
 * `setOutputDevice` on `PcmPlayback` would replace this.
 */
async function defaultSetSink(playback: typeof pcmPlayback, deviceId: string): Promise<void> {
  const ctx = (playback as unknown as { ctx?: { setSinkId?: (id: string) => Promise<void> } | null }).ctx;
  if (!ctx || typeof ctx.setSinkId !== 'function') throw new Error('output not available yet');
  await ctx.setSinkId(deviceId);
}
