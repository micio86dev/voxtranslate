// Talk to Anyone — the conversation engine (spec 0110).
//
// Owns the socket, the microphone, the playback graph, the self-audio guard, the wake
// lock and the state machine. Deliberately DOM-free: every observable thing is a
// callback, so `view.ts` can be replaced (or driven by a test) without touching any of
// the logic below. Business logic outside presentation components, per the brief §42.
//
// Reconnect uses capped exponential backoff WITH JITTER, mirroring
// `webinar-presence.ts` — explicitly not the fixed 2 s retry in `app.ts`, which
// thunders when a server restarts.

import { buildWsUrl, getToken } from '../auth';
import { pcmPlayback } from '../pcm-playback';
import { PcmCapture } from '../pcm-capture';
import { AudioCapture } from '../audio-capture';
import { releaseWakeLock, requestWakeLock } from '../wake-lock';
import { SelfAudioGuard, startLevelMonitor, type GuardMode } from './self-audio-guard';
import {
  initialContext,
  transition,
  wantsWakeLock,
  type FailureKind,
  type SessionContext,
  type SessionEvent,
} from './session-machine';

/** Reconnect bounds, matching `PresenceClient`. */
export const RECONNECT_BASE_MS = 1_000;
export const RECONNECT_MAX_MS = 30_000;
/** Give up after this many consecutive failures — a bounded retry, never an endless one. */
export const RECONNECT_MAX_ATTEMPTS = 6;

/** One completed exchange, as the UI shows it. */
export interface Exchange {
  id: number;
  /** The language that was spoken. */
  spokenLang: string;
  /** What was said, in the language it was said in. */
  originalText: string;
  /** The language it came out in. */
  targetLang: string;
  /** The translation — the visually dominant half of the card. */
  translatedText: string;
}

/** The live, still-forming exchange. */
export interface LiveExchange {
  spokenLang: string | null;
  targetLang: string | null;
  originalText: string;
  translatedText: string;
}

/** The minimum socket surface, so tests can supply a fake. */
export interface TalkSocket {
  send: (data: string | ArrayBufferLike | Blob) => void;
  close: (code?: number, reason?: string) => void;
  readyState: number;
  onopen: (() => void) | null;
  onclose: ((ev: { code: number }) => void) | null;
  onerror: (() => void) | null;
  onmessage: ((ev: { data: unknown }) => void) | null;
}

export interface ConversationCallbacks {
  onState: (ctx: SessionContext) => void;
  onLive: (live: LiveExchange) => void;
  onExchange: (exchange: Exchange) => void;
  onBalance?: (balance: number) => void;
  onLowBalance?: (balance: number) => void;
  /** A recoverable, user-facing problem. `code` is stable; the view picks the copy. */
  onNotice?: (code: string) => void;
  /** The tier actually in use, when the server had to change it. */
  onEngineChanged?: (to: string, reason: string) => void;
  /** Latency instrumentation: speech seen → translated audio audible, in ms. */
  onTimeToTranslatedSpeech?: (ms: number) => void;
  onBargeIn?: () => void;
}

export interface ConversationOptions extends ConversationCallbacks {
  userLang: string;
  otherLang: string;
  engineId: string;
  /** From `engineNeedsPcm(engineId, engines)` — never an id comparison. */
  needsPcm: boolean;
  guardMode?: GuardMode;
  // --- injectable seams, all defaulted -------------------------------------
  createSocket?: (url: string) => TalkSocket;
  getMedia?: (constraints: MediaStreamConstraints) => Promise<MediaStream>;
  now?: () => number;
  random?: () => number;
  setTimeoutImpl?: typeof setTimeout;
  clearTimeoutImpl?: typeof clearTimeout;
  wakeLock?: { request: () => Promise<void>; release: () => Promise<void> };
  playback?: typeof pcmPlayback;
}

/**
 * Microphone constraints. The three processing flags are the first and cheapest layer of
 * feedback-loop defence (brief §21) and match what `app.ts` already asks for on calls;
 * `channelCount: 1` because every engine wants mono and asking for stereo only invites
 * a downmix we do not control.
 */
export const MIC_CONSTRAINTS: MediaStreamConstraints = {
  audio: {
    channelCount: 1,
    echoCancellation: true,
    noiseSuppression: true,
    autoGainControl: true,
  },
  video: false,
};

/** Map a getUserMedia rejection onto the copy the user should actually see. */
export function classifyMediaError(err: unknown): FailureKind {
  const name = (err as { name?: string } | null)?.name ?? '';
  switch (name) {
    // Chrome reports a permanently blocked site the same way as a fresh denial, so the
    // view offers the browser-settings path in both cases.
    case 'NotAllowedError':
    case 'SecurityError':
      return 'mic_denied';
    case 'NotFoundError':
    case 'OverconstrainedError':
      return 'mic_missing';
    // The device exists but something else holds it, or it vanished mid-request.
    case 'NotReadableError':
    case 'AbortError':
      return 'mic_blocked';
    default:
      return 'unknown';
  }
}

/** True when this browser has everything the conversation needs. */
export function browserSupported(win: Partial<Window & typeof globalThis> = window): boolean {
  const hasMedia = !!win.navigator?.mediaDevices?.getUserMedia;
  const hasAudio = !!(win.AudioContext ?? (win as { webkitAudioContext?: unknown }).webkitAudioContext);
  const hasWorklet = typeof win.AudioWorkletNode !== 'undefined';
  const hasSockets = typeof win.WebSocket !== 'undefined';
  return hasMedia && hasAudio && hasWorklet && hasSockets;
}

/**
 * Backoff delay for `attempt` (0-based), capped and jittered.
 *
 * Full jitter, like the extension's `nextBackoff`: without it every phone in a café
 * whose server just restarted retries on the same millisecond.
 */
export function backoffDelay(attempt: number, random: () => number = Math.random): number {
  const ceiling = Math.min(RECONNECT_MAX_MS, RECONNECT_BASE_MS * 2 ** attempt);
  return Math.round(ceiling * (0.5 + random() * 0.5));
}

/** Build the `/ws/talk` URL. Exported for the test and for the e2e harness. */
export function buildTalkUrl(userLang: string, otherLang: string, engineId: string): string {
  const params = new URLSearchParams({ lang: userLang, other: otherLang, engine: engineId });
  // `buildWsUrl` targets `/ws`; this route is a sibling and attaches the same token.
  return buildWsUrl(params).replace('/ws?', '/ws/talk?');
}

type Capture = PcmCapture | AudioCapture;

export class TalkConversation {
  private ctx: SessionContext = initialContext();
  private socket: TalkSocket | null = null;
  private stream: MediaStream | null = null;
  private capture: Capture | null = null;
  private guard: SelfAudioGuard;
  private stopLevelMonitor: (() => void) | null = null;
  private levelCtx: AudioContext | null = null;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private attempt = 0;
  private exchangeId = 0;
  private live: LiveExchange = blankLive();
  /** When the current utterance was first seen, for `timeToTranslatedSpeech`. */
  private utteranceStartedAt: number | null = null;
  private ttsReported = false;

  private readonly now: () => number;
  private readonly random: () => number;
  private readonly setTimeoutImpl: typeof setTimeout;
  private readonly clearTimeoutImpl: typeof clearTimeout;
  private readonly playback: typeof pcmPlayback;

  constructor(private readonly opts: ConversationOptions) {
    this.now = opts.now ?? (() => Date.now());
    this.random = opts.random ?? Math.random;
    this.setTimeoutImpl = opts.setTimeoutImpl ?? setTimeout;
    this.clearTimeoutImpl = opts.clearTimeoutImpl ?? clearTimeout;
    this.playback = opts.playback ?? pcmPlayback;
    this.guard = new SelfAudioGuard({
      setGated: (gated) => {
        if (this.capture instanceof PcmCapture) this.capture.setGated(gated);
        // AudioCapture (WebM/Opus, subtitle-only tiers) has no gate and needs none: it
        // produces no translated audio to feed back.
      },
      cancelPlayback: () => this.playback.reset(),
      onBargeIn: () => this.opts.onBargeIn?.(),
      now: this.now,
    });
    if (opts.guardMode) this.guard.setMode(opts.guardMode);
  }

  state(): SessionContext {
    return this.ctx;
  }

  /** Acquire the microphone, open the socket, and start listening. */
  async start(): Promise<void> {
    if (!this.dispatch({ type: 'START_REQUESTED' }, newSessionId(this.random))) return;

    // Unlock playback inside the user gesture that led here — iOS/Safari will not start
    // an AudioContext later, and the first translation would be silent.
    this.playback.unlock();
    this.playback.setPlayingListener((playing) => this.onPlaybackChange(playing));

    const getMedia =
      this.opts.getMedia ??
      ((c: MediaStreamConstraints) => navigator.mediaDevices.getUserMedia(c));
    try {
      this.stream = await getMedia(MIC_CONSTRAINTS);
    } catch (err) {
      this.dispatch({ type: 'MIC_DENIED', kind: classifyMediaError(err) });
      return;
    }
    // The user may have hit End while the permission prompt was up.
    if (this.ctx.phase !== 'requesting_mic') {
      this.releaseStream();
      return;
    }
    this.dispatch({ type: 'MIC_GRANTED' });
    this.startLevelWatch();
    this.connect();
  }

  /** Stop sending audio without releasing anything. Resume is instant. */
  pause(): void {
    if (!this.dispatch({ type: 'PAUSE_REQUESTED' })) return;
    this.setMuted(true);
    // Drop whatever is queued: resuming into a sentence from before the pause would be
    // more confusing than the silence.
    this.playback.reset();
    this.guard.reset();
  }

  resume(): void {
    if (!this.dispatch({ type: 'RESUME_REQUESTED' })) return;
    this.setMuted(false);
  }

  /** Mute the microphone mid-conversation. Distinct from pause: the session stays live. */
  setMuted(muted: boolean): void {
    this.capture?.setMuted(muted);
  }

  /**
   * End the conversation and release EVERYTHING: microphone tracks, the socket, the
   * playback graph, the level monitor, the wake lock and every timer. A leaked
   * microphone track leaves the browser's recording indicator on after the user thinks
   * they are done, which is a trust problem before it is a bug.
   */
  end(): void {
    this.dispatch({ type: 'STOP_REQUESTED' });
    this.clearReconnect();

    if (this.capture) {
      this.capture.stop();
      this.capture = null;
    }
    this.stopLevelWatch();
    this.releaseStream();

    if (this.socket) {
      const sock = this.socket;
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
    this.live = blankLive();
    // Synchronous on purpose: the screen has changed, the microphone is off, and the UI
    // must say so NOW rather than one promise later. `reconcileWakeLock` releases the
    // lock as a side effect of leaving the live phase.
    this.dispatch({ type: 'TEARDOWN_COMPLETE' });
  }

  // --- internals ---------------------------------------------------------

  private dispatch(event: SessionEvent, newId?: string): boolean {
    const before = this.ctx;
    const { context, accepted } = transition(before, event, newId);
    this.ctx = context;
    if (context !== before) {
      // The wake lock is a FUNCTION of the phase, reconciled here so there is exactly
      // one place that can get it wrong. Acquiring it at the call sites instead means
      // every new transition is a chance to leave a traveller's screen pinned awake —
      // or, worse, to let the phone lock mid-sentence.
      this.reconcileWakeLock(before);
      this.opts.onState(context);
    }
    return accepted;
  }

  private reconcileWakeLock(before: SessionContext): void {
    const was = wantsWakeLock(before);
    const now = wantsWakeLock(this.ctx);
    if (was === now) return;
    const wl = this.opts.wakeLock;
    // Fire-and-forget: nothing downstream waits on a screen lock, and a rejection here
    // is routine (hidden document, OS refusal).
    void (now
      ? wl
        ? wl.request()
        : requestWakeLock()
      : wl
        ? wl.release()
        : releaseWakeLock()
    );
  }

  private connect(): void {
    const url = buildTalkUrl(this.opts.userLang, this.opts.otherLang, this.opts.engineId);
    let sock: TalkSocket;
    try {
      sock = (this.opts.createSocket ?? defaultSocket)(url);
    } catch {
      // Bad URL or offline. Treat exactly like a drop so the retry path is one path.
      this.scheduleReconnect();
      return;
    }
    this.socket = sock;

    sock.onopen = () => {
      this.attempt = 0;
      const reconnected = this.ctx.phase === 'reconnecting';
      this.dispatch(reconnected ? { type: 'RECONNECT_SUCCEEDED' } : { type: 'SOCKET_OPEN' });
      this.attachCapture(sock);
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
      this.detachCapture();
      this.dispatch({ type: 'SOCKET_CLOSED', recoverable: true });
      this.scheduleReconnect();
    };
  }

  private attachCapture(sock: TalkSocket): void {
    if (!this.stream) return;
    this.detachCapture();
    // Capability-driven, never an engine-id comparison: a speech-to-speech tier needs
    // PCM16/24k, and feeding one WebM produces silence with no error at all.
    this.capture = this.opts.needsPcm
      ? new PcmCapture(this.stream, sock as unknown as WebSocket)
      : new AudioCapture(this.stream, sock as unknown as WebSocket);
    this.capture.start();
  }

  private detachCapture(): void {
    this.capture?.stop();
    this.capture = null;
  }

  private scheduleReconnect(): void {
    if (this.ctx.phase === 'stopping' || this.ctx.phase === 'ended') return;
    if (this.attempt >= RECONNECT_MAX_ATTEMPTS) {
      this.dispatch({ type: 'RECONNECT_EXHAUSTED' });
      return;
    }
    const delay = backoffDelay(this.attempt, this.random);
    this.attempt += 1;
    this.clearReconnect();
    this.reconnectTimer = this.setTimeoutImpl(() => {
      this.reconnectTimer = null;
      this.connect();
    }, delay);
  }

  private clearReconnect(): void {
    if (this.reconnectTimer !== null) {
      this.clearTimeoutImpl(this.reconnectTimer);
      this.reconnectTimer = null;
    }
  }

  private handleFrame(raw: string): void {
    let msg: Record<string, unknown>;
    try {
      msg = JSON.parse(raw) as Record<string, unknown>;
    } catch {
      return; // malformed frames are ignored, never fatal
    }
    switch (msg.type) {
      case 'talk_direction': {
        const spoken = String(msg.spoken ?? '');
        const target = String(msg.target ?? '');
        this.live = { ...this.live, spokenLang: spoken, targetLang: target };
        this.dispatch({ type: 'DIRECTION_RESOLVED' });
        this.opts.onLive(this.live);
        break;
      }
      case 'subtitle_interim': {
        this.noteUtteranceStart();
        const original = typeof msg.original === 'string' ? msg.original : '';
        const text = typeof msg.text === 'string' ? msg.text : '';
        // The server gates by direction, so an interim that reaches us is always the
        // real translation — `original` alongside it, when the engine sent one.
        this.live = {
          ...this.live,
          translatedText: text,
          originalText: original || this.live.originalText,
        };
        this.dispatch({ type: 'SPEECH_DETECTED' });
        this.opts.onLive(this.live);
        break;
      }
      case 'subtitle_final': {
        this.commitExchange(msg);
        break;
      }
      case 'translated_audio': {
        const seq = Number(msg.seq ?? 0);
        const b64 = typeof msg.pcm16_b64 === 'string' ? msg.pcm16_b64 : '';
        const speaker = String(msg.speaker_id ?? 'talk');
        if (b64) this.playback.enqueue(speaker, seq, b64);
        break;
      }
      case 'balance_update':
        this.opts.onBalance?.(Number(msg.balance ?? 0));
        break;
      case 'low_balance':
        this.opts.onLowBalance?.(Number(msg.balance ?? 0));
        break;
      case 'balance_exhausted':
        this.dispatch({ type: 'CREDITS_EXHAUSTED' });
        this.end();
        break;
      case 'engine_downgraded':
        this.opts.onEngineChanged?.(String(msg.to ?? ''), String(msg.reason ?? ''));
        break;
      case 'error': {
        const code = typeof msg.code === 'string' ? msg.code : 'unknown';
        if (code === 'invalid_token' || code === 'insufficient_balance' || code === 'banned') {
          this.dispatch({ type: 'FATAL', kind: code === 'invalid_token' ? 'unknown' : 'credits' });
          this.end();
          return;
        }
        // Everything else — a busy provider, a transient upstream failure — is
        // recoverable: say so and keep listening rather than tearing the session down.
        this.opts.onNotice?.(code);
        break;
      }
      default:
        break;
    }
  }

  private commitExchange(msg: Record<string, unknown>): void {
    const original = typeof msg.original === 'string' ? msg.original.trim() : '';
    const translations = (msg.translations ?? {}) as Record<string, string>;
    const target = this.live.targetLang;
    // `translations` carries exactly one entry — this session's target language (see
    // `standard.rs::flush_final`). Prefer the committed target, but fall back to the
    // single value rather than render an empty card if the direction arrived late.
    const translated = (
      (target && translations[target]) ||
      Object.values(translations)[0] ||
      ''
    ).trim();

    this.dispatch({ type: 'UTTERANCE_ENDED' });
    if (translated || original) {
      this.exchangeId += 1;
      this.opts.onExchange({
        id: this.exchangeId,
        spokenLang: this.live.spokenLang ?? '',
        originalText: original,
        targetLang: target ?? '',
        translatedText: translated,
      });
    }
    this.live = blankLive();
    this.utteranceStartedAt = null;
    this.ttsReported = false;
    this.opts.onLive(this.live);
  }

  private noteUtteranceStart(): void {
    if (this.utteranceStartedAt === null) this.utteranceStartedAt = this.now();
  }

  private onPlaybackChange(playing: boolean): void {
    this.guard.onPlaybackChange(playing);
    this.dispatch({ type: playing ? 'PLAYBACK_STARTED' : 'PLAYBACK_STOPPED' });
    if (playing && !this.ttsReported && this.utteranceStartedAt !== null) {
      // The headline product metric (brief §33): speech seen → translated speech
      // audible. Reported once per utterance, on the first audible sample.
      this.ttsReported = true;
      this.opts.onTimeToTranslatedSpeech?.(this.now() - this.utteranceStartedAt);
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
      // No analyser means no barge-in — the gate still opens on the playback edge, so
      // the conversation works, it just cannot be interrupted mid-translation.
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
    this.stream = null;
  }

}

function blankLive(): LiveExchange {
  return { spokenLang: null, targetLang: null, originalText: '', translatedText: '' };
}

function defaultSocket(url: string): TalkSocket {
  return new WebSocket(url) as unknown as TalkSocket;
}

function newSessionId(random: () => number): string {
  return `${Math.floor(random() * 1e9).toString(36)}-${Date.now().toString(36)}`;
}

/** Re-exported so the page can read the token without importing auth directly. */
export { getToken };
