// Client-direct "Enhanced" translation pipeline (spec 0108, Cartesia).
//
// When a signed-in listener picks Enhanced in a listener-pays room, THIS listener
// translates the audio it receives in-browser: for every remote speaker whose language
// differs from ours, we open a Cartesia real-time STT (Ink-2) stream straight to Cartesia
// (server-minted access token — the raw key never reaches the client) over that peer's
// WebRTC audio. The server never proxies this audio, which is what removes the relay hop.
//
// Cartesia does STT + TTS but NOT translation, so each finalized source segment is sent to
// the backend for a Groq translation (the injected `translate` callback → a `translate_text`
// WS round-trip) and then spoken back via Cartesia TTS (Sonic-3.5) in the SPEAKER's cloned
// voice — text is the only thing that crosses the server; audio stays browser ↔ Cartesia.
//
// This module is DOM-free and injectable: app.ts supplies how to fetch a session, how to
// translate, how to play audio, and the spoken-voice toggle, so the transcript logic stays
// unit-testable without a live WebSocket / AudioContext.

import { pcmPlayback } from './pcm-playback';

/** A minted client-direct session: the scoped access token + the public endpoints. */
export interface CartesiaSession {
  /** Short-lived Cartesia access token (both STT + TTS grants), passed as the WS
   *  `access_token` query param. */
  token: string;
  /** Unix seconds at which `token` expires — we refresh a little before this. */
  expiresAt: number;
  cartesiaVersion: string;
  sttEndpoint: string;
  sttModel: string;
  ttsEndpoint: string;
  ttsModel: string;
  voiceCloningEnabled: boolean;
  /** Fallback Cartesia voice id (env-configured) for a speaker without a clone; absent →
   *  such a speaker is shown subtitles only. */
  defaultVoiceId?: string;
}

export interface CartesiaManagerOptions {
  /** Mint / refresh a Cartesia session from our backend (`POST /api/sessions/enhanced/session`). */
  fetchSession: () => Promise<CartesiaSession | null>;
  /** Translate one finalized source segment via the backend (Groq). Returns null on failure
   *  so the caller can fall back to the untranslated source. */
  translate: (text: string, source: string, target: string) => Promise<string | null>;
  /** Render a translated subtitle for `speakerId` (interim while still streaming). */
  onSubtitle: (speakerId: string, text: string, interim: boolean) => void;
  /** A pipeline gave up on `speakerId` (a permanent error, or a transient one that survived
   *  all retries — e.g. Cartesia's concurrency limit). The app uses this to fall the listener
   *  back to server-side Standard translation. */
  onError?: (speakerId: string, status: string, message: string) => void;
  /** Play one decoded TTS chunk (base64 PCM16 @ 24 kHz). Defaults to the shared
   *  `pcmPlayback` graph; injectable so tests can stub it. */
  playAudio?: (speakerId: string, seq: number, pcm16Base64: string) => void;
  /** Whether the listener has spoken translation on (the "translated voice" toggle). When
   *  false we render subtitles only (no TTS). */
  ttsEnabled?: () => boolean;
}

/** Idle gap after which the accumulated transcript is flushed (translated + spoken). Cartesia
 *  endpointing finalizes quickly; this is the segment delimiter (a pause), mirroring the
 *  server's speech-to-speech engines. */
export const IDLE_FLUSH_MS = 900;

/** STT capture sample rate (Hz). Cartesia Ink-2 ingests raw PCM16; an AudioContext forced to
 *  this rate makes the browser resample the peer's WebRTC audio for us. */
export const STT_SAMPLE_RATE = 16000;
/** TTS playback sample rate (Hz) — matches the shared `pcmPlayback` graph (spec 0093). */
export const TTS_SAMPLE_RATE = 24000;

/** The static, same-origin PCM capture worklet (CSP `worker-src 'self'`; no blob:). */
const CAPTURE_WORKLET_URL = '/pcm-capture-worklet.js';

/** Exponential backoff between retry attempts (ms). Length = max retries before giving up. */
export const RETRY_BACKOFF_MS = [1000, 2000, 4000] as const;

/** One Cartesia STT transcript message we model (the rest deserialize and are ignored). */
export interface CartesiaTranscript {
  type?: string;
  is_final?: boolean;
  text?: string;
}

/** Running transcript state for one peer pipeline: confirmed (finalized) text accumulated
 *  since the last flush, plus the current interim hypothesis. Pure → unit-tested. */
export interface TranscriptState {
  confirmed: string;
  interim: string;
}

/** Fold one Cartesia transcript message into the running state. A final message appends its
 *  delta to `confirmed` and clears the interim tail; an interim replaces the tail. Returns
 *  the new state (pure — no mutation of the input). */
export function accumulate(state: TranscriptState, msg: CartesiaTranscript): TranscriptState {
  if (msg.type && msg.type !== 'transcript') return state;
  const text = msg.text ?? '';
  if (msg.is_final) {
    const sep = state.confirmed && text ? ' ' : '';
    return { confirmed: (state.confirmed + sep + text).trim(), interim: '' };
  }
  return { confirmed: state.confirmed, interim: text };
}

/** The live (interim) line to show: confirmed text plus the current interim tail. */
export function liveLine(state: TranscriptState): string {
  const sep = state.confirmed && state.interim ? ' ' : '';
  return (state.confirmed + sep + state.interim).trim();
}

/** Build the Cartesia STT WebSocket URL with auth + format as query params. */
export function sttUrl(session: CartesiaSession, sourceLang: string): string {
  const url = new URL(session.sttEndpoint);
  url.searchParams.set('model', session.sttModel);
  url.searchParams.set('encoding', 'pcm_s16le');
  url.searchParams.set('sample_rate', String(STT_SAMPLE_RATE));
  url.searchParams.set('cartesia_version', session.cartesiaVersion);
  url.searchParams.set('access_token', session.token);
  if (sourceLang) url.searchParams.set('language', sourceLang);
  return url.toString();
}

/** Build the Cartesia TTS WebSocket URL with auth as a query param. */
export function ttsUrl(session: CartesiaSession): string {
  const url = new URL(session.ttsEndpoint);
  url.searchParams.set('cartesia_version', session.cartesiaVersion);
  url.searchParams.set('access_token', session.token);
  return url.toString();
}

interface Pipeline {
  ws: WebSocket;
  ctx: AudioContext | null;
  node: AudioWorkletNode | null;
  src: MediaStreamAudioSourceNode | null;
  sourceLang: string;
  state: TranscriptState;
  idleTimer: ReturnType<typeof setTimeout> | null;
  stopped: boolean;
  attempt: number;
}

/** Manages one Cartesia STT pipeline (+ translation + TTS) per remote speaker for THIS
 *  listener — the same lifecycle as the previous Enhanced engine, but Cartesia-native. */
export class CartesiaManager {
  private pipelines = new Map<string, Pipeline>();
  private streams = new Map<string, MediaStream>();
  private langs = new Map<string, string>();
  private voiceIds = new Map<string, string>();
  private retryTimers = new Map<string, ReturnType<typeof setTimeout>>();
  /** Peers whose pipeline gave up; `reconcile` skips these until something warrants a fresh
   *  attempt (source/target lang change, the peer leaves, or we deactivate). */
  private gaveUp = new Set<string>();
  private myLang = '';
  private active = false;
  /** Cached session; re-minted when missing or within 60 s of expiry. */
  private session: CartesiaSession | null = null;
  /** Lazy shared TTS connection + the per-context routing back to a speaker. */
  private tts: WebSocket | null = null;
  private ttsContexts = new Map<string, { speakerId: string; seq: number }>();
  private ttsCounter = 0;

  constructor(private opts: CartesiaManagerOptions) {}

  /** Whether the browser can run the pipeline at all (WebSocket + Web Audio). */
  static get supported(): boolean {
    return (
      typeof WebSocket !== 'undefined' &&
      typeof AudioContext !== 'undefined' &&
      typeof MediaStream !== 'undefined'
    );
  }

  /** Turn the receive pipeline on for a listener whose language is `myLang`. */
  activate(myLang: string): void {
    this.active = true;
    this.myLang = myLang;
    for (const peerId of this.peerIds()) this.reconcile(peerId);
  }

  /** Tear everything down (leaving the call / switching away from Enhanced). */
  deactivate(): void {
    this.active = false;
    for (const peerId of [...this.pipelines.keys()]) this.stopPipeline(peerId);
    for (const t of this.retryTimers.values()) clearTimeout(t);
    this.retryTimers.clear();
    this.gaveUp.clear();
    this.streams.clear();
    this.langs.clear();
    this.voiceIds.clear();
    this.closeTts();
  }

  /** The listener changed their own language — every pipeline's translation target moves,
   *  so restart them. */
  setMyLang(lang: string): void {
    if (lang === this.myLang) return;
    this.myLang = lang;
    this.gaveUp.clear();
    for (const peerId of this.peerIds()) this.reconcile(peerId, /* targetChanged */ true);
  }

  /** A remote peer's audio stream arrived (or changed). */
  setPeerStream(peerId: string, stream: MediaStream): void {
    this.streams.set(peerId, stream);
    this.reconcile(peerId);
  }

  /** A remote peer's spoken language is known / changed (join, auto-detect, set-lang). */
  setPeerLang(peerId: string, lang: string): void {
    if (this.langs.get(peerId) === lang) return;
    this.langs.set(peerId, lang);
    this.gaveUp.delete(peerId);
    this.reconcile(peerId);
  }

  /** A remote peer's cloned-voice id (from presence, spec 0108). Used for their TTS; absent
   *  → the default voice. No restart needed — it only affects the next spoken utterance. */
  setPeerVoiceId(peerId: string, voiceId: string | null | undefined): void {
    if (voiceId) this.voiceIds.set(peerId, voiceId);
    else this.voiceIds.delete(peerId);
  }

  /** A peer left — drop its pipeline and state. */
  removePeer(peerId: string): void {
    this.stopPipeline(peerId);
    this.clearRetry(peerId);
    this.gaveUp.delete(peerId);
    this.streams.delete(peerId);
    this.langs.delete(peerId);
    this.voiceIds.delete(peerId);
  }

  private peerIds(): string[] {
    return [...new Set([...this.streams.keys(), ...this.langs.keys()])];
  }

  /** Mint a session if we don't have a live one (token valid > 60 s). */
  private async ensureSession(): Promise<CartesiaSession | null> {
    const now = Math.floor(Date.now() / 1000);
    if (this.session && this.session.expiresAt - now > 60) return this.session;
    this.session = await this.opts.fetchSession();
    return this.session;
  }

  /** Start/stop/restart `peerId`'s pipeline to match the desired state. */
  private reconcile(peerId: string, targetChanged = false): void {
    if (this.gaveUp.has(peerId)) return;
    const lang = this.langs.get(peerId);
    const stream = this.streams.get(peerId);
    const want =
      this.active && !!lang && !!stream && lang !== 'auto' && lang !== this.myLang;
    const existing = this.pipelines.get(peerId);

    if (!want) {
      this.clearRetry(peerId);
      if (existing) this.stopPipeline(peerId);
      return;
    }
    const sourceChanged = existing && existing.sourceLang !== lang;
    if (existing && !sourceChanged && !targetChanged) return; // already running, unchanged
    this.clearRetry(peerId);
    if (existing) this.stopPipeline(peerId);
    void this.startPipeline(peerId, stream as MediaStream, lang as string);
  }

  private async startPipeline(
    peerId: string,
    stream: MediaStream,
    sourceLang: string,
    attempt = 0,
  ): Promise<void> {
    if (stream.getAudioTracks().length === 0) return; // no audio yet — retried on stream update

    const session = await this.ensureSession();
    // The peer may have left / changed language / we deactivated while minting — bail.
    if (!session || !this.active || this.langs.get(peerId) !== sourceLang) return;
    if (this.pipelines.has(peerId)) return; // a concurrent reconcile already started one

    let ws: WebSocket;
    try {
      ws = new WebSocket(sttUrl(session, sourceLang));
      ws.binaryType = 'arraybuffer';
    } catch (e) {
      console.warn('[cartesia] STT socket construction failed', e);
      this.handleError(peerId, 'websocket_error', String(e));
      return;
    }

    const pipe: Pipeline = {
      ws,
      ctx: null,
      node: null,
      src: null,
      sourceLang,
      state: { confirmed: '', interim: '' },
      idleTimer: null,
      stopped: false,
      attempt,
    };
    this.pipelines.set(peerId, pipe);

    ws.onopen = () => {
      if (pipe.stopped) return;
      void this.startCapture(peerId, pipe, stream);
    };
    ws.onmessage = (ev) => {
      if (pipe.stopped) return;
      try {
        const msg = JSON.parse(typeof ev.data === 'string' ? ev.data : '') as CartesiaTranscript;
        this.handleTranscript(peerId, msg);
      } catch {
        /* non-JSON / keepalive frame */
      }
    };
    ws.onerror = () => this.handleError(peerId, 'websocket_error', 'stt socket error');
    ws.onclose = (ev) => {
      // A clean close (we tore it down) is ignored; an unexpected one is a transient error.
      // Include the close REASON (Cartesia sends a human string, e.g. "Invalid language for
      // model" on a 1008) — without it a fallback is undebuggable from the console alone.
      if (!pipe.stopped) {
        const reason = ev.reason ? ` ${ev.reason}` : '';
        this.handleError(peerId, 'websocket_error', `closed ${ev.code}${reason}`);
      }
    };
  }

  /** Wire a Web Audio capture graph (16 kHz PCM16) for `stream` into the open STT socket. */
  private async startCapture(peerId: string, pipe: Pipeline, stream: MediaStream): Promise<void> {
    try {
      const Ctx =
        window.AudioContext ??
        (window as unknown as { webkitAudioContext: typeof AudioContext }).webkitAudioContext;
      const ctx = new Ctx({ sampleRate: STT_SAMPLE_RATE });
      await ctx.audioWorklet.addModule(CAPTURE_WORKLET_URL);
      if (pipe.stopped) {
        void ctx.close().catch(() => {});
        return;
      }
      const track = stream.getAudioTracks()[0];
      if (!track) {
        void ctx.close().catch(() => {});
        return;
      }
      const src = ctx.createMediaStreamSource(new MediaStream([track]));
      const node = new AudioWorkletNode(ctx, 'pcm-capture-processor');
      node.port.onmessage = (e: MessageEvent<ArrayBuffer>) => {
        if (!pipe.stopped && pipe.ws.readyState === WebSocket.OPEN) pipe.ws.send(e.data);
      };
      const sink = ctx.createGain();
      sink.gain.value = 0; // a node only runs when pulled by the graph; route to a muted sink
      src.connect(node).connect(sink).connect(ctx.destination);
      pipe.ctx = ctx;
      pipe.node = node;
      pipe.src = src;
    } catch (e) {
      console.warn('[cartesia] capture start failed', e);
      this.handleError(peerId, 'media_error', String(e));
    }
  }

  private handleTranscript(peerId: string, msg: CartesiaTranscript): void {
    const pipe = this.pipelines.get(peerId);
    if (!pipe || pipe.stopped || !this.active) return;
    pipe.state = accumulate(pipe.state, msg);
    const live = liveLine(pipe.state);
    if (live) this.opts.onSubtitle(peerId, live, true);
    if (pipe.idleTimer) clearTimeout(pipe.idleTimer);
    pipe.idleTimer = setTimeout(() => this.flush(peerId), IDLE_FLUSH_MS);
  }

  /** A segment ended (idle): translate the confirmed text, render it, and speak it. */
  private flush(peerId: string): void {
    const pipe = this.pipelines.get(peerId);
    if (!pipe || pipe.stopped) return;
    const source = (pipe.state.confirmed + ' ' + pipe.state.interim).trim();
    pipe.state = { confirmed: '', interim: '' };
    pipe.idleTimer = null;
    if (!source) return;
    const sourceLang = pipe.sourceLang;
    void this.opts
      .translate(source, sourceLang, this.myLang)
      .then((translated) => {
        // Bail if the peer/pipeline changed while we awaited the translation.
        if (this.pipelines.get(peerId) !== pipe || pipe.stopped || !this.active) return;
        const text = (translated ?? source).trim();
        if (!text) return;
        this.opts.onSubtitle(peerId, text, false);
        if (this.opts.ttsEnabled?.() ?? true) this.speak(peerId, text);
      })
      .catch(() => {});
  }

  // ---- TTS (Cartesia Sonic, speaker's cloned voice) -------------------------

  private speak(peerId: string, text: string): void {
    const session = this.session;
    if (!session) return;
    const ws = this.ensureTts(session);
    if (!ws || ws.readyState !== WebSocket.OPEN) return;
    const voiceId = this.voiceIds.get(peerId) ?? session.defaultVoiceId;
    if (!voiceId) return; // no clone and no default → subtitles only
    const contextId = `${peerId}:${this.ttsCounter++}`;
    this.ttsContexts.set(contextId, { speakerId: peerId, seq: 0 });
    ws.send(
      JSON.stringify({
        model_id: session.ttsModel,
        transcript: text,
        voice: { mode: 'id', id: voiceId },
        output_format: { container: 'raw', encoding: 'pcm_s16le', sample_rate: TTS_SAMPLE_RATE },
        language: this.myLang,
        context_id: contextId,
        continue: false,
      }),
    );
  }

  /** Open (once) the shared TTS socket and route its chunks back to the right speaker. */
  private ensureTts(session: CartesiaSession): WebSocket | null {
    if (this.tts && this.tts.readyState <= WebSocket.OPEN) return this.tts;
    let ws: WebSocket;
    try {
      ws = new WebSocket(ttsUrl(session));
    } catch {
      return null;
    }
    this.tts = ws;
    const play = this.opts.playAudio ?? ((s, seq, b64) => pcmPlayback.enqueue(s, seq, b64));
    ws.onmessage = (ev) => {
      if (typeof ev.data !== 'string') return;
      let msg: { type?: string; context_id?: string; data?: string };
      try {
        msg = JSON.parse(ev.data);
      } catch {
        return;
      }
      const route = msg.context_id ? this.ttsContexts.get(msg.context_id) : undefined;
      if (!route) return;
      if (msg.type === 'chunk' && msg.data) {
        play(route.speakerId, route.seq++, msg.data);
      } else if ((msg.type === 'done' || msg.type === 'error') && msg.context_id) {
        this.ttsContexts.delete(msg.context_id);
      }
    };
    ws.onclose = () => {
      if (this.tts === ws) this.tts = null;
      this.ttsContexts.clear();
    };
    ws.onerror = () => {
      /* a TTS hiccup degrades to subtitles only; STT/translation are unaffected */
    };
    return ws;
  }

  private closeTts(): void {
    this.ttsContexts.clear();
    if (this.tts) {
      try {
        this.tts.close();
      } catch {
        /* already closed */
      }
      this.tts = null;
    }
  }

  // ---- Pipeline teardown + retry -------------------------------------------

  private stopPipeline(peerId: string): void {
    const pipe = this.pipelines.get(peerId);
    if (!pipe) return;
    pipe.stopped = true;
    if (pipe.idleTimer) clearTimeout(pipe.idleTimer);
    this.pipelines.delete(peerId);
    if (pipe.node) {
      pipe.node.port.onmessage = null;
      try {
        pipe.node.disconnect();
      } catch {
        /* noop */
      }
    }
    if (pipe.src) {
      try {
        pipe.src.disconnect();
      } catch {
        /* noop */
      }
    }
    if (pipe.ctx) void pipe.ctx.close().catch(() => {});
    try {
      pipe.ws.onclose = null; // suppress the "unexpected close" path during teardown
      pipe.ws.close();
    } catch {
      /* already closed */
    }
  }

  private clearRetry(peerId: string): void {
    const t = this.retryTimers.get(peerId);
    if (t) {
      clearTimeout(t);
      this.retryTimers.delete(peerId);
    }
  }

  /** A pipeline errored. Tear it down, then either schedule a backoff retry (attempts left)
   *  or give up — mark the peer and notify the app so it can fall back to Standard. */
  private handleError(peerId: string, status: string, message: string): void {
    const pipe = this.pipelines.get(peerId);
    const attempt = pipe?.attempt ?? RETRY_BACKOFF_MS.length;
    if (pipe && !pipe.stopped) {
      console.warn(`[cartesia] ${peerId} ${status}: ${message}`);
      this.stopPipeline(peerId);
    }

    if (attempt < RETRY_BACKOFF_MS.length) {
      const timer = setTimeout(() => {
        this.retryTimers.delete(peerId);
        const lang = this.langs.get(peerId);
        const stream = this.streams.get(peerId);
        const want =
          this.active && !!lang && !!stream && lang !== 'auto' && lang !== this.myLang;
        if (!want || this.gaveUp.has(peerId) || this.pipelines.has(peerId)) return;
        void this.startPipeline(peerId, stream as MediaStream, lang as string, attempt + 1);
      }, RETRY_BACKOFF_MS[attempt]);
      this.retryTimers.set(peerId, timer);
      return;
    }

    // Retries exhausted: stop trying for this peer and let the app degrade gracefully.
    this.gaveUp.add(peerId);
    this.opts.onError?.(peerId, status, message);
  }
}
