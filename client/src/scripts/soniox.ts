// Client-direct "Enhanced" translation pipeline (spec 0101, Soniox).
//
// When a signed-in listener picks Enhanced in a listener-pays room, THIS listener
// translates the audio it receives in-browser: for every remote speaker whose language
// differs from ours, we open a Soniox real-time STT + translation stream straight to
// Soniox (server-minted temp key — the raw key never reaches the client) and render the
// translated tokens as subtitles. The server never proxies this audio, which is what
// removes the relay hop and gets us sub-250 ms latency.
//
// Spoken translation reuses the app's existing on-device voice (the `onUtterance`
// callback → `speak()`), honoring the project rule "prefer local voices, minimal delay"
// and staying clear of Soniox's small TTS concurrency cap. The backend can still mint a
// Soniox TTS key (request `spoken: true`) for a future higher-fidelity voice.
//
// This module is DOM-free and SDK-thin: app.ts injects how to fetch a session and how
// to render a subtitle / speak an utterance, so the token logic stays unit-testable.

import { SonioxClient, type SpeechToTextAPIResponse, type Token } from '@soniox/speech-to-text-web';

/** A minted client-direct session: the scoped temp key + the (regional) endpoint. */
export interface SonioxSession {
  sttApiKey: string;
  sttEndpoint: string;
  model: string;
}

export interface SonioxManagerOptions {
  /** Mint a fresh Soniox session from our backend (keys are single-use). */
  fetchSession: () => Promise<SonioxSession | null>;
  /** Render a translated subtitle for `speakerId` (interim while still streaming). */
  onSubtitle: (speakerId: string, text: string, interim: boolean) => void;
  /** A finalized translated utterance for `speakerId` — for optional on-device TTS. */
  onUtterance: (speakerId: string, text: string) => void;
}

/** Idle gap after which the accumulated translation is flushed as a final line + spoken.
 *  Soniox endpoint detection finalizes tokens quickly; this is the segment delimiter (a
 *  pause), mirroring the server's speech-to-speech engines. */
export const IDLE_FLUSH_MS = 900;

/** Pull the TRANSLATED text out of one Soniox result: newly-final translation tokens
 *  (appended once, in order) and the current non-final translation tail (re-sent each
 *  update). Original-language and non-translation tokens are ignored — we only show the
 *  listener their own language. Pure → unit-tested. */
export function extractTranslation(tokens: Token[]): { finalDelta: string; nonFinal: string } {
  let finalDelta = '';
  let nonFinal = '';
  for (const t of tokens) {
    if (t.translation_status !== 'translation') continue;
    if (t.is_final) finalDelta += t.text;
    else nonFinal += t.text;
  }
  return { finalDelta, nonFinal };
}

/** Deduped, falsy-stripped language hints (source + my language). */
function langHints(source: string, mine: string): string[] {
  return [...new Set([source, mine].filter(Boolean))];
}

/** MediaRecorder mimeType to hand the Soniox SDK for capturing a peer's audio.
 *  The SDK records via MediaRecorder and left to the browser default produces a
 *  container Soniox's `audioFormat:'auto'` can't always decode (Enhanced was silent
 *  on Android). Pick the best container the device supports, in order:
 *    1. `audio/webm;codecs=opus` — Chrome/Android/Firefox (unchanged from #281),
 *    2. `audio/mp4` — Safari/WebKit, which CANNOT record WebM at all; a pinned WebM
 *       mimeType made the SDK throw → Enhanced silent on Safari (spec 0103 Phase 1),
 *    3. `audio/webm` — any browser that supports plain WebM but not the opus form,
 *  else `undefined` → omit the option and let the SDK default (also the node test
 *  env, where MediaRecorder is absent). Mirrors recording/utils.ts `pickMimeType`. */
export function sttMimeType(): string | undefined {
  if (typeof MediaRecorder === 'undefined') return undefined;
  return ['audio/webm;codecs=opus', 'audio/mp4', 'audio/webm'].find((t) =>
    MediaRecorder.isTypeSupported(t),
  );
}

interface Pipeline {
  client: SonioxClient;
  sourceLang: string;
  /** Confirmed translated text accumulated since the last flush. */
  finalText: string;
  idleTimer: ReturnType<typeof setTimeout> | null;
  /** Set once we've torn it down, so a late async start bails. */
  stopped: boolean;
}

/** Manages one Soniox STT+translation pipeline per remote speaker for THIS listener. */
export class SonioxManager {
  private pipelines = new Map<string, Pipeline>();
  private streams = new Map<string, MediaStream>();
  private langs = new Map<string, string>();
  private myLang = '';
  private active = false;

  constructor(private opts: SonioxManagerOptions) {}

  /** Whether the browser can run Soniox at all (WebSocket + getUserMedia). */
  static get supported(): boolean {
    return SonioxClient.isSupported;
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
    this.streams.clear();
    this.langs.clear();
  }

  /** The listener changed their own language (auto-detect / manual) — every pipeline's
   *  translation target moves, so restart them. */
  setMyLang(lang: string): void {
    if (lang === this.myLang) return;
    this.myLang = lang;
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
    this.reconcile(peerId);
  }

  /** A peer left — drop its pipeline and state. */
  removePeer(peerId: string): void {
    this.stopPipeline(peerId);
    this.streams.delete(peerId);
    this.langs.delete(peerId);
  }

  private peerIds(): string[] {
    return [...new Set([...this.streams.keys(), ...this.langs.keys()])];
  }

  /** Start/stop/restart `peerId`'s pipeline to match the desired state. */
  private reconcile(peerId: string, targetChanged = false): void {
    const lang = this.langs.get(peerId);
    const stream = this.streams.get(peerId);
    const want =
      this.active && !!lang && !!stream && lang !== 'auto' && lang !== this.myLang;
    const existing = this.pipelines.get(peerId);

    if (!want) {
      if (existing) this.stopPipeline(peerId);
      return;
    }
    // want === true → lang and stream are defined.
    const sourceChanged = existing && existing.sourceLang !== lang;
    if (existing && !sourceChanged && !targetChanged) return; // already running, unchanged
    if (existing) this.stopPipeline(peerId);
    void this.startPipeline(peerId, stream as MediaStream, lang as string);
  }

  private async startPipeline(
    peerId: string,
    stream: MediaStream,
    sourceLang: string,
  ): Promise<void> {
    const audioTracks = stream.getAudioTracks();
    if (audioTracks.length === 0) return; // no audio yet — retried when the stream updates

    const session = await this.opts.fetchSession();
    // The peer may have left, changed language, or we may have deactivated while the
    // single-use key was minting — bail rather than open a stale stream.
    if (!session || !this.active || this.langs.get(peerId) !== sourceLang) return;
    if (this.pipelines.has(peerId)) return; // a concurrent reconcile already started one

    let client: SonioxClient;
    try {
      client = new SonioxClient({
        apiKey: session.sttApiKey,
        webSocketUri: session.sttEndpoint || undefined,
        onError: (status, message) => {
          // Soft-fail: a refused/limited stream just means no subtitles for this speaker.
          console.warn(`[soniox] ${peerId} ${status}: ${message}`);
        },
      });
    } catch (e) {
      console.warn('[soniox] client construction failed', e);
      return;
    }

    const pipe: Pipeline = { client, sourceLang, finalText: '', idleTimer: null, stopped: false };
    this.pipelines.set(peerId, pipe);

    const mimeType = sttMimeType();
    try {
      void client.start({
        model: session.model,
        audioFormat: 'auto',
        // Pin the recorder codec so Android Chrome captures a container Soniox can
        // decode (spec 0101 fix): without this the SDK's default made Enhanced
        // silent on Android. No-op where MediaRecorder is unavailable.
        ...(mimeType ? { mediaRecorderOptions: { mimeType } } : {}),
        languageHints: langHints(sourceLang, this.myLang),
        enableEndpointDetection: true,
        translation: { type: 'one_way', target_language: this.myLang },
        stream: new MediaStream(audioTracks),
        onPartialResult: (result) => this.handleResult(peerId, result),
      });
    } catch (e) {
      console.warn('[soniox] start failed', e);
      this.stopPipeline(peerId);
    }
  }

  private handleResult(peerId: string, result: SpeechToTextAPIResponse): void {
    const pipe = this.pipelines.get(peerId);
    if (!pipe || pipe.stopped || !this.active) return;
    const { finalDelta, nonFinal } = extractTranslation(result.tokens ?? []);
    pipe.finalText += finalDelta;
    const live = (pipe.finalText + nonFinal).trim();
    if (live) this.opts.onSubtitle(peerId, live, true);
    if (pipe.idleTimer) clearTimeout(pipe.idleTimer);
    pipe.idleTimer = setTimeout(() => this.flush(peerId), IDLE_FLUSH_MS);
  }

  private flush(peerId: string): void {
    const pipe = this.pipelines.get(peerId);
    if (!pipe || pipe.stopped) return;
    const text = pipe.finalText.trim();
    pipe.finalText = '';
    pipe.idleTimer = null;
    if (text) {
      this.opts.onSubtitle(peerId, text, false);
      this.opts.onUtterance(peerId, text);
    }
  }

  private stopPipeline(peerId: string): void {
    const pipe = this.pipelines.get(peerId);
    if (!pipe) return;
    pipe.stopped = true;
    if (pipe.idleTimer) clearTimeout(pipe.idleTimer);
    this.pipelines.delete(peerId);
    try {
      pipe.client.cancel(); // immediate teardown (no need to drain final results)
    } catch {
      /* already closed */
    }
  }
}
