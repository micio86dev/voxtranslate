// Live Connect — the Enhanced tier on the phone (spec 0121, S5).
//
// Enhanced runs on Cartesia, and Cartesia is reached DIRECTLY from the browser with a short-lived
// token (the server never proxies this audio). There is no WebRTC between the two phones in Live
// Connect, so the design is, per phone:
//
//   speaker side   Cartesia STT on MY microphone  ->  text  ->  `translate_text` over /ws/live
//   server         translates, and tells every OTHER seat what to say (`enhanced_speak`)
//   listener side  Cartesia TTS speaks that text, in the host's cloned voice when the host opted in
//
// This module owns exactly that and nothing else: the token (fetched BEFORE `start`, refreshed
// before it expires), the STT socket and its capture graph, the coalescing of partial transcripts,
// the TTS queue, and the ways it can fail. It plugs into `LiveConversation` as its capture
// (`start`/`stop`/`setMuted`/`setGated`), so the echo guard that gates the PCM path gates this one
// too: a phone's own TTS re-enters its own microphone, and the server only has a backstop.
//
// The Cartesia protocol itself (URLs, the transcript fold, the TTS request, the capture graph) is
// `cartesia.ts` — Meet's — and is not repeated here.
//
// Any failure ends in ONE of three outcomes, reported once through `onFailure` and then this
// engine is closed: move the whole conversation to Standard (`fallback`), carry on as Standard
// because the server already did (`not_enhanced`), or leave it to the live socket (`gone`: the
// session or the seat no longer exists).

import {
  IDLE_FLUSH_MS,
  RETRY_BACKOFF_MS,
  accumulate,
  closeSttGraph,
  liveLine,
  openSttGraph,
  parseTtsFrame,
  sttUrl,
  toCartesiaSession,
  ttsRequest,
  ttsUrl,
  type CartesiaSession,
  type CartesiaTranscript,
  type SttGraph,
  type TranscriptState,
} from '../cartesia';
import type { EnhancedSessionResponse } from '../api';
import type { ApiResult } from './api';

/** The server accepts 1-1000 characters per `translate_text`. */
export const MAX_TEXT_CHARS = 1000;
/** The server accepts at most 8 `translate_text` frames per second: partials stay well under. */
export const PARTIAL_MIN_INTERVAL_MS = 200;
/** Sentences waiting to be spoken. Past this the OLDEST goes: a late translation is worse than none. */
export const MAX_SPEECH_QUEUE = 12;
/** Refresh the token this long before it expires. */
export const REFRESH_LEAD_MS = 5 * 60_000;
/** A socket that has not opened by then is unreachable (a firewall hangs, it does not refuse). */
export const OPEN_TIMEOUT_MS = 5_000;
/** An utterance that never reports `done`. */
export const UTTERANCE_TIMEOUT_MS = 20_000;
/** STT connection attempts after the first one (Meet's schedule, shortened: everybody waits). */
export const STT_RETRIES = 2;
/** A socket that stayed up this long was healthy: the retry budget starts over. */
export const STABLE_MS = 15_000;
/** Consecutive refused utterances / sockets before Cartesia is declared unusable. */
export const TTS_FAILURE_LIMIT = 3;

export type EnhancedFailure =
  /** Move the whole conversation to Standard: send `enhanced_fallback{reason}`. */
  | { kind: 'fallback'; reason: string }
  /** The server says the conversation is not Enhanced (any more): carry on as Standard. */
  | { kind: 'not_enhanced' }
  /** 401/403/410: the session or seat is gone. The live socket owns what happens next. */
  | { kind: 'gone'; code: string };

/** One sentence to speak, as the server's `enhanced_speak` says it. */
export interface SpeakItem {
  speakerId: string;
  segmentId: string;
  text: string;
  /** The host's cloned voice when they opted in, else null (Cartesia's default voice). */
  voiceId: string | null;
}

/** The slice of a WebSocket this module uses; tests supply a fake. */
export interface CartesiaSocket {
  readyState: number;
  binaryType?: string;
  onopen: (() => void) | null;
  onmessage: ((ev: { data: unknown }) => void) | null;
  onerror: (() => void) | null;
  onclose: ((ev: { code?: number }) => void) | null;
  send: (data: string | ArrayBufferLike | Blob) => void;
  close: () => void;
}

export interface EnhancedDeps {
  stream: MediaStream;
  myLang: string;
  fetchToken: () => Promise<ApiResult<EnhancedSessionResponse>>;
  /** Send a JSON frame on the live socket (`start`, `stop`, `translate_text`). */
  control: (msg: Record<string, unknown>) => void;
  /** What I am saying, from MY OWN recognition: the server never sends me my own words. */
  onMine: (text: string, final: boolean) => void;
  onFailure: (failure: EnhancedFailure) => void;
  /** One chunk of synthesised speech (base64 PCM16, 24 kHz) for the shared playback graph. */
  playAudio: (speakerId: string, seq: number, pcm16Base64: string) => void;
  // --- injectable seams, all defaulted -------------------------------------
  createSocket?: (url: string) => CartesiaSocket;
  openGraph?: typeof openSttGraph;
  /** Great-Firewall reachability verdict (Meet's probe). Resolves true behind it. */
  isRestricted?: () => Promise<boolean>;
  now?: () => number;
  setTimeoutImpl?: typeof setTimeout;
  clearTimeoutImpl?: typeof clearTimeout;
}

/** What `LiveConversation` needs from the engine; tests supply a fake. */
export interface EnhancedLike {
  open(): Promise<boolean>;
  start(): void;
  stop(): void;
  setMuted(muted: boolean): void;
  setGated(gated: boolean): void;
  speak(item: SpeakItem): void;
  cancelSpeech(): void;
  close(): void;
}

interface SttState {
  ws: CartesiaSocket;
  graph: SttGraph | null;
  opened: boolean;
  openTimer: ReturnType<typeof setTimeout> | null;
  stableTimer: ReturnType<typeof setTimeout> | null;
}

/** Split `text` into pieces of at most `max` characters, at word boundaries where there are any. */
export function chunkText(text: string, max: number = MAX_TEXT_CHARS): string[] {
  const out: string[] = [];
  let rest = text.trim();
  while (rest.length > max) {
    let cut = rest.lastIndexOf(' ', max);
    if (cut <= 0) cut = max;
    out.push(rest.slice(0, cut).trim());
    rest = rest.slice(cut).trim();
  }
  if (rest) out.push(rest);
  return out;
}

/**
 * Mean absolute sample of a PCM16 little-endian chunk (0 to 32768): the unit the server meters
 * microphones in. Sign is ignored, an empty chunk is silence.
 */
export function meanAbsPcm16(buf: ArrayBuffer): number {
  const n = Math.floor(buf.byteLength / 2);
  if (n === 0) return 0;
  const view = new DataView(buf);
  let sum = 0;
  for (let i = 0; i < n; i++) sum += Math.abs(view.getInt16(i * 2, true));
  return sum / n;
}

/** How much of the last level survives one ~100 ms chunk: a loud word fades over about a second. */
const LEVEL_DECAY = 0.9;

export class EnhancedEngine implements EnhancedLike {
  private session: CartesiaSession | null = null;
  private refreshTimer: ReturnType<typeof setTimeout> | null = null;
  private closed = false;
  private opening: Promise<boolean> | null = null;

  /** STT is wanted: started and not muted. */
  private active = false;
  private gated = false;
  /** My microphone's level, fast up and slowly down: what `translate_text` reports (see `level`). */
  private micLevel = 0;
  /** `start` was sent and `stop` has not been: the seat's meter is open on the server. */
  private meterOpen = false;
  private stt: SttState | null = null;
  private sttAttempt = 0;
  private retryTimer: ReturnType<typeof setTimeout> | null = null;

  private transcript: TranscriptState = { confirmed: '', interim: '' };
  private idleTimer: ReturnType<typeof setTimeout> | null = null;
  private segmentId: string | null = null;
  private segments = 0;
  private readonly prefix: string;
  private lastSentAt = -Infinity;
  private pendingPartial: string | null = null;
  private partialTimer: ReturnType<typeof setTimeout> | null = null;

  private queue: SpeakItem[] = [];
  private pumping = false;
  private tts: CartesiaSocket | null = null;
  private ttsStale = false;
  private ttsFailures = 0;
  private audioSeq = 0;
  private utterances = 0;
  /** The utterance being synthesised: its context and how to wake the queue. */
  private current: { contextId: string; item: SpeakItem; settle: (ok: boolean) => void } | null = null;

  private readonly now: () => number;
  private readonly setT: typeof setTimeout;
  private readonly clearT: typeof clearTimeout;

  constructor(private readonly deps: EnhancedDeps) {
    this.now = deps.now ?? (() => Date.now());
    // Wrapped, never stored bare: a browser throws "Illegal invocation" when `setTimeout` is
    // called with the instance as its receiver.
    this.setT =
      deps.setTimeoutImpl ??
      (((fn: () => void, ms?: number) => globalThis.setTimeout(fn, ms)) as unknown as typeof setTimeout);
    this.clearT =
      deps.clearTimeoutImpl ??
      (((id?: ReturnType<typeof setTimeout>) => globalThis.clearTimeout(id)) as unknown as typeof clearTimeout);
    this.prefix = `e${this.now().toString(36)}`;
  }

  // --- token --------------------------------------------------------------

  /**
   * Fetch the token. Must resolve BEFORE `start` goes out: a seat that opened its meter and then
   * could not reach Cartesia would be billed for silence. False = the engine failed and closed
   * (`onFailure` has said how).
   */
  open(): Promise<boolean> {
    this.opening ??= this.doOpen();
    return this.opening;
  }

  private async doOpen(): Promise<boolean> {
    for (let attempt = 0; ; attempt++) {
      const restricted = this.deps.isRestricted ? this.deps.isRestricted().catch(() => false) : Promise.resolve(false);
      const [res, blocked] = await Promise.all([this.deps.fetchToken(), restricted]);
      if (this.closed) return false;
      if (blocked) {
        this.fail({ kind: 'fallback', reason: 'restricted_network' });
        return false;
      }
      if (res.ok) {
        this.session = toCartesiaSession(res.data);
        this.scheduleRefresh();
        if (this.active) this.openStt();
        return true;
      }
      if (res.status === 0 && attempt < 1) {
        // Offline for a moment: once more, then Cartesia is as good as unreachable.
        await this.sleep(RETRY_BACKOFF_MS[0]);
        if (this.closed) return false;
        continue;
      }
      this.fail(classifyTokenError(res.status, res.code));
      return false;
    }
  }

  private sleep(ms: number): Promise<void> {
    return new Promise((resolve) => {
      this.setT(resolve, ms);
    });
  }

  private scheduleRefresh(): void {
    this.clearTimer('refreshTimer');
    const s = this.session;
    if (!s || this.closed) return;
    const remaining = s.expiresAt * 1000 - this.now();
    const delay = Math.max(5_000, remaining > REFRESH_LEAD_MS * 2 ? remaining - REFRESH_LEAD_MS : Math.floor(remaining / 2));
    this.refreshTimer = this.setT(() => {
      this.refreshTimer = null;
      void this.refresh();
    }, delay);
  }

  private async refresh(): Promise<void> {
    if (this.closed) return;
    const res = await this.deps.fetchToken();
    if (this.closed) return;
    if (res.ok) {
      this.session = toCartesiaSession(res.data);
      this.scheduleRefresh();
      // Sockets are authorised when they open, so the next ones carry the new token. The speech
      // socket is replaced when idle; the recogniser is cycled now, after what it holds is sent.
      this.ttsStale = true;
      if (!this.current) this.closeTts();
      if (this.stt) {
        this.flush();
        this.teardownStt();
        this.openStt();
      }
      return;
    }
    const left = this.session ? this.session.expiresAt * 1000 - this.now() : 0;
    const failure = classifyTokenError(res.status, res.code);
    if (failure.kind === 'fallback' && left > 30_000) {
      // The old token still works: try again shortly instead of moving everybody off Enhanced.
      this.refreshTimer = this.setT(() => {
        this.refreshTimer = null;
        void this.refresh();
      }, 15_000);
      return;
    }
    this.fail(failure);
  }

  // --- the capture contract (LiveCapture) -----------------------------------

  /** Start recognising my speech (opens the seat's meter once the recogniser is connected). */
  start(): void {
    if (this.closed || this.active) return;
    this.active = true;
    if (this.session) this.openStt();
  }

  /** Stop recognising (mute, hang-up). Anything half said is dropped: the server ignores it too. */
  stop(): void {
    if (!this.active) return;
    this.active = false;
    this.discardSegment();
    this.teardownStt();
    this.clearTimer('retryTimer');
    if (this.meterOpen) {
      this.meterOpen = false;
      this.deps.control({ type: 'stop' });
    }
  }

  setMuted(muted: boolean): void {
    if (muted) this.stop();
    else this.start();
  }

  /** Hold my audio back from the recogniser while this phone is speaking (the guard decides when). */
  setGated(gated: boolean): void {
    this.gated = gated;
  }

  // --- speaker side: STT -----------------------------------------------------

  private openStt(): void {
    const session = this.session;
    if (!session || !this.active || this.closed || this.stt) return;
    let ws: CartesiaSocket;
    try {
      ws = this.makeSocket(sttUrl(session, this.deps.myLang));
      ws.binaryType = 'arraybuffer';
    } catch {
      this.sttFailed();
      return;
    }
    const stt: SttState = { ws, graph: null, opened: false, openTimer: null, stableTimer: null };
    this.stt = stt;
    stt.openTimer = this.setT(() => {
      if (this.stt === stt && !stt.opened) this.sttFailed();
    }, OPEN_TIMEOUT_MS);
    ws.onopen = () => {
      if (this.stt !== stt) return;
      stt.opened = true;
      this.clearHandle(stt.openTimer);
      stt.openTimer = null;
      stt.stableTimer = this.setT(() => {
        this.sttAttempt = 0;
      }, STABLE_MS);
      if (!this.meterOpen) {
        this.meterOpen = true;
        this.deps.control({ type: 'start' });
      }
      void this.startGraph(stt);
    };
    ws.onmessage = (ev) => {
      if (this.stt !== stt || typeof ev.data !== 'string') return;
      try {
        this.onTranscript(JSON.parse(ev.data) as CartesiaTranscript);
      } catch {
        // A keep-alive or non-JSON frame.
      }
    };
    // `onclose` always follows an error; handling both would count the failure twice.
    ws.onerror = () => {};
    ws.onclose = () => {
      if (this.stt === stt) this.sttFailed();
    };
  }

  private async startGraph(stt: SttState): Promise<void> {
    try {
      const graph = await (this.deps.openGraph ?? openSttGraph)(
        this.deps.stream,
        (pcm) => {
          // Measured even while the echo guard holds the microphone back: the level describes the
          // room, whatever is sent to the recogniser.
          this.micLevel = Math.max(meanAbsPcm16(pcm), this.micLevel * LEVEL_DECAY);
          if (this.stt === stt && !this.gated && stt.ws.readyState === 1) stt.ws.send(pcm);
        },
        () => this.stt !== stt,
      );
      if (!graph) return;
      if (this.stt !== stt) closeSttGraph(graph);
      else stt.graph = graph;
    } catch {
      if (this.stt === stt) this.sttFailed();
    }
  }

  private teardownStt(): void {
    const stt = this.stt;
    if (!stt) return;
    this.stt = null;
    this.clearHandle(stt.openTimer);
    this.clearHandle(stt.stableTimer);
    stt.ws.onopen = stt.ws.onmessage = stt.ws.onerror = stt.ws.onclose = null;
    try {
      stt.ws.close();
    } catch {
      // Already closed.
    }
    if (stt.graph) closeSttGraph(stt.graph);
  }

  /** The recogniser dropped or never connected: a short retry, then Cartesia is unusable. */
  private sttFailed(): void {
    this.teardownStt();
    if (this.closed || !this.active) return;
    // What was half said is sent as it stands: it is the last the server will hear of it.
    this.flush();
    if (this.sttAttempt >= STT_RETRIES) {
      this.fail({ kind: 'fallback', reason: 'cartesia_unreachable' });
      return;
    }
    const wait = RETRY_BACKOFF_MS[Math.min(this.sttAttempt, RETRY_BACKOFF_MS.length - 1)];
    this.sttAttempt += 1;
    this.clearTimer('retryTimer');
    this.retryTimer = this.setT(() => {
      this.retryTimer = null;
      this.openStt();
    }, wait);
  }

  private onTranscript(msg: CartesiaTranscript): void {
    this.transcript = accumulate(this.transcript, msg);
    const live = liveLine(this.transcript);
    if (!live) return;
    this.segmentId ??= `${this.prefix}-${++this.segments}`;
    this.deps.onMine(live, false);
    this.queuePartial(live);
    this.clearTimer('idleTimer');
    this.idleTimer = this.setT(() => this.flush(), IDLE_FLUSH_MS);
  }

  /** The sentence is over (a pause): send it as FINAL, show it as mine. */
  private flush(): void {
    this.clearTimer('idleTimer');
    const source = `${this.transcript.confirmed} ${this.transcript.interim}`.trim();
    const id = this.segmentId;
    this.transcript = { confirmed: '', interim: '' };
    this.segmentId = null;
    this.clearTimer('partialTimer');
    this.pendingPartial = null;
    if (!source || !id) return;
    this.deps.onMine(source, true);
    chunkText(source).forEach((piece, i) => this.sendText(i === 0 ? id : `${id}.${i}`, piece, true));
  }

  private discardSegment(): void {
    this.clearTimer('idleTimer');
    this.clearTimer('partialTimer');
    this.pendingPartial = null;
    this.transcript = { confirmed: '', interim: '' };
    this.segmentId = null;
  }

  private sendText(segmentId: string, text: string, final: boolean): void {
    this.lastSentAt = this.now();
    // `level` lets the server tell my voice from my voice heard by the OTHER phone's microphone:
    // whoever is louder in their own microphone is the one speaking (spec 0121, full duplex).
    this.deps.control({
      type: 'translate_text',
      segment_id: segmentId,
      text,
      final,
      level: Math.round(this.micLevel),
    });
  }

  /** Partials are coalesced: at most one per interval, always the LATEST text. */
  private queuePartial(text: string): void {
    this.pendingPartial = text.length > MAX_TEXT_CHARS ? text.slice(-MAX_TEXT_CHARS) : text;
    if (this.partialTimer !== null) return;
    const wait = Math.max(0, this.lastSentAt + PARTIAL_MIN_INTERVAL_MS - this.now());
    if (wait === 0) {
      this.sendPartial();
      return;
    }
    this.partialTimer = this.setT(() => {
      this.partialTimer = null;
      this.sendPartial();
    }, wait);
  }

  private sendPartial(): void {
    const text = this.pendingPartial;
    this.pendingPartial = null;
    if (text && this.segmentId && this.active) this.sendText(this.segmentId, text, false);
  }

  // --- listener side: TTS -----------------------------------------------------

  /** Queue a sentence to be spoken, in arrival order. */
  speak(item: SpeakItem): void {
    if (this.closed || !item.text.trim()) return;
    this.queue.push(item);
    while (this.queue.length > MAX_SPEECH_QUEUE) this.queue.shift();
    void this.pump();
  }

  /** I talked over the translation: drop what is waiting and let the one in flight die out. */
  cancelSpeech(): void {
    this.queue = [];
    this.current?.settle(true);
  }

  private async pump(): Promise<void> {
    if (this.pumping) return;
    this.pumping = true;
    try {
      while (!this.closed && this.queue.length) {
        const item = this.queue.shift() as SpeakItem;
        await this.speakOne(item);
      }
    } finally {
      this.pumping = false;
    }
  }

  private async speakOne(item: SpeakItem): Promise<void> {
    const session = this.session ?? ((await this.open()) ? this.session : null);
    if (!session || this.closed) return;
    // No clone and no default voice: the caption is all there is.
    const voice = item.voiceId ?? session.defaultVoiceId;
    if (!voice) return;
    const ws = await this.openTts();
    if (this.closed) return;
    if (!ws) {
      this.ttsProblem();
      return;
    }
    const contextId = `${this.prefix}:${++this.utterances}`;
    const ok = await new Promise<boolean>((resolve) => {
      let timer: ReturnType<typeof setTimeout> | null = null;
      const settle = (good: boolean): void => {
        if (this.current?.contextId !== contextId) return;
        this.current = null;
        if (timer !== null) this.clearT(timer);
        resolve(good);
      };
      this.current = { contextId, item, settle };
      timer = this.setT(() => settle(false), UTTERANCE_TIMEOUT_MS);
      try {
        ws.send(ttsRequest(session, voice, item.text, this.deps.myLang, contextId));
      } catch {
        settle(false);
      }
    });
    if (ok) this.ttsFailures = 0;
    else this.ttsProblem();
  }

  private ttsProblem(): void {
    this.ttsFailures += 1;
    if (this.ttsFailures >= TTS_FAILURE_LIMIT) this.fail({ kind: 'fallback', reason: 'cartesia_unreachable' });
  }

  /** The shared speech socket, opened on demand. Null when it cannot be opened. */
  private openTts(): Promise<CartesiaSocket | null> {
    const session = this.session;
    if (!session) return Promise.resolve(null);
    if (this.tts && this.ttsStale && !this.current) this.closeTts();
    if (this.tts && this.tts.readyState === 1) return Promise.resolve(this.tts);
    this.closeTts();
    this.ttsStale = false;
    return new Promise((resolve) => {
      let ws: CartesiaSocket;
      try {
        ws = this.makeSocket(ttsUrl(session));
      } catch {
        resolve(null);
        return;
      }
      this.tts = ws;
      let settled = false;
      const done = (v: CartesiaSocket | null): void => {
        if (settled) return;
        settled = true;
        this.clearHandle(timer);
        resolve(v);
      };
      const timer = this.setT(() => {
        if (this.tts === ws) this.closeTts();
        done(null);
      }, OPEN_TIMEOUT_MS);
      ws.onopen = () => done(ws);
      ws.onmessage = (ev) => this.onTtsFrame(ev.data);
      ws.onerror = () => {};
      ws.onclose = () => {
        if (this.tts === ws) this.tts = null;
        done(null);
        // An utterance in flight on a socket that died did not finish.
        this.current?.settle(false);
      };
    });
  }

  private onTtsFrame(raw: unknown): void {
    const frame = parseTtsFrame(raw);
    const cur = this.current;
    if (!frame || !cur || frame.contextId !== cur.contextId) return; // a cancelled utterance's tail
    if (frame.type === 'chunk' && frame.data) {
      this.deps.playAudio(cur.item.speakerId, this.audioSeq++, frame.data);
    } else if (frame.type === 'done') {
      cur.settle(true);
    } else if (frame.type === 'error') {
      cur.settle(false);
    }
  }

  private closeTts(): void {
    const ws = this.tts;
    if (!ws) return;
    this.tts = null;
    ws.onopen = ws.onmessage = ws.onerror = ws.onclose = null;
    try {
      ws.close();
    } catch {
      // Already closed.
    }
  }

  // --- teardown ---------------------------------------------------------------

  /** Everything off: sockets, graph, timers, queue. Idempotent. */
  close(): void {
    if (this.closed) return;
    this.stop();
    this.closed = true;
    this.clearTimer('refreshTimer');
    this.clearTimer('retryTimer');
    this.queue = [];
    this.current?.settle(false);
    this.closeTts();
  }

  private fail(failure: EnhancedFailure): void {
    if (this.closed) return;
    this.close();
    this.deps.onFailure(failure);
  }

  private makeSocket(url: string): CartesiaSocket {
    return (this.deps.createSocket ?? ((u: string) => new WebSocket(u) as unknown as CartesiaSocket))(url);
  }

  private clearHandle(h: ReturnType<typeof setTimeout> | null): void {
    if (h !== null) this.clearT(h);
  }

  private clearTimer(name: 'idleTimer' | 'partialTimer' | 'refreshTimer' | 'retryTimer'): void {
    this.clearHandle(this[name]);
    this[name] = null;
  }
}

/** What a refused token means, from the status and code the server answered with. */
export function classifyTokenError(status: number, code: string): EnhancedFailure {
  if (code === 'not_enhanced') return { kind: 'not_enhanced' };
  if (status === 401 || status === 403 || status === 410 || code === 'session_ended' || code === 'wrong_session') {
    return { kind: 'gone', code };
  }
  if (status === 402 || code === 'insufficient_balance') return { kind: 'fallback', reason: 'insufficient_balance' };
  if (status === 429 || code === 'rate_limited') return { kind: 'fallback', reason: 'rate_limited' };
  if (status === 502 || code === 'enhanced_unavailable') return { kind: 'fallback', reason: 'enhanced_unavailable' };
  if (status === 0 || code === 'network') return { kind: 'fallback', reason: 'cartesia_unreachable' };
  return { kind: 'fallback', reason: 'token_refused' };
}
