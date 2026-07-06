// Tests for the client-direct Enhanced pipeline (spec 0108, Cartesia). The pure transcript
// + URL helpers are deterministic; the manager lifecycle is exercised with minimal
// WebSocket / AudioContext / MediaStream mocks so the reconcile + flush + translate + TTS
// flow is covered without a live provider.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import {
  accumulate,
  CartesiaManager,
  type CartesiaManagerOptions,
  type CartesiaSession,
  IDLE_FLUSH_MS,
  liveLine,
  resolveSttModel,
  sttUrl,
  ttsUrl,
} from './cartesia';
import { pcmPlayback } from './pcm-playback';

const SESSION: CartesiaSession = {
  token: 'tok_abc',
  expiresAt: Math.floor(Date.now() / 1000) + 3600,
  cartesiaVersion: '2026-03-01',
  sttEndpoint: 'wss://api.cartesia.ai/stt/websocket',
  sttModel: 'ink-whisper',
  sttModelsByLang: { en: 'ink-2' },
  ttsEndpoint: 'wss://api.cartesia.ai/tts/websocket',
  ttsModel: 'sonic-3.5',
  voiceCloningEnabled: true,
  defaultVoiceId: 'voice-default',
};

describe('accumulate / liveLine', () => {
  it('replaces the interim tail and appends finals', () => {
    let s = { confirmed: '', interim: '' };
    s = accumulate(s, { type: 'transcript', is_final: false, text: 'ciao' });
    expect(liveLine(s)).toBe('ciao');
    s = accumulate(s, { type: 'transcript', is_final: false, text: 'ciao come' });
    expect(liveLine(s)).toBe('ciao come'); // interim replaces, not appends
    s = accumulate(s, { type: 'transcript', is_final: true, text: 'ciao come stai' });
    expect(s.confirmed).toBe('ciao come stai');
    expect(s.interim).toBe('');
    s = accumulate(s, { type: 'transcript', is_final: true, text: 'bene' });
    expect(s.confirmed).toBe('ciao come stai bene'); // finals accumulate with a space
  });

  it('ignores non-transcript messages', () => {
    const s = { confirmed: 'x', interim: '' };
    expect(accumulate(s, { type: 'flush_done' })).toBe(s);
  });

  it('treats a missing text as empty', () => {
    const s = accumulate({ confirmed: 'a', interim: 'b' }, { is_final: true });
    expect(s).toEqual({ confirmed: 'a', interim: '' }); // no dangling separator
  });
});

describe('sttUrl / ttsUrl', () => {
  it('builds the STT URL with auth + format query params', () => {
    const u = new URL(sttUrl(SESSION, 'it'));
    expect(u.origin + u.pathname).toBe('wss://api.cartesia.ai/stt/websocket');
    expect(u.searchParams.get('model')).toBe('ink-whisper'); // 'it' → multilingual default
    expect(u.searchParams.get('encoding')).toBe('pcm_s16le');
    expect(u.searchParams.get('sample_rate')).toBe('16000');
    expect(u.searchParams.get('cartesia_version')).toBe('2026-03-01');
    expect(u.searchParams.get('access_token')).toBe('tok_abc');
    expect(u.searchParams.get('language')).toBe('it');
  });

  it('routes each speaker to the fastest STT model for their language', () => {
    // English → the per-language override (ink-2, Cartesia's fastest, English-only).
    expect(new URL(sttUrl(SESSION, 'en')).searchParams.get('model')).toBe('ink-2');
    // Regional English variant resolves on the base code; language is normalized to `en`.
    const enUs = new URL(sttUrl(SESSION, 'en-US'));
    expect(enUs.searchParams.get('model')).toBe('ink-2');
    expect(enUs.searchParams.get('language')).toBe('en');
    // Any other language → the multilingual default.
    expect(new URL(sttUrl(SESSION, 'es')).searchParams.get('model')).toBe('ink-whisper');
    // No map / unknown language → the default model.
    expect(resolveSttModel({ ...SESSION, sttModelsByLang: undefined }, 'en')).toBe('ink-whisper');
    expect(resolveSttModel(SESSION, 'de')).toBe('ink-whisper');
  });

  it('omits the language param when the source language is unknown', () => {
    const u = new URL(sttUrl(SESSION, ''));
    expect(u.searchParams.get('language')).toBeNull();
    expect(u.searchParams.get('model')).toBe('ink-whisper');
  });

  it('builds the TTS URL with auth', () => {
    const u = new URL(ttsUrl(SESSION));
    expect(u.searchParams.get('cartesia_version')).toBe('2026-03-01');
    expect(u.searchParams.get('access_token')).toBe('tok_abc');
  });
});

// ---- Manager lifecycle (mocked transport) ---------------------------------

interface FakeSocket {
  url: string;
  readyState: number;
  sent: unknown[];
  onopen: (() => void) | null;
  onmessage: ((ev: { data: string | ArrayBuffer }) => void) | null;
  onerror: (() => void) | null;
  onclose: ((ev: { code: number; reason?: string }) => void) | null;
  close: () => void;
  send: (d: unknown) => void;
}

interface FakeWorkletNode {
  port: { onmessage: ((e: { data: ArrayBuffer }) => void) | null };
  connect(n: unknown): unknown;
  disconnect(): void;
}

const sockets: FakeSocket[] = [];
const workletNodes: FakeWorkletNode[] = [];
// Per-test knobs for the fakes below (reset by installMocks).
const mockCfg = {
  /** Throw from the WebSocket constructor when the URL contains this substring. */
  wsThrowOn: null as string | null,
  /** Override audioWorklet.addModule (e.g. a deferred promise). */
  addModule: null as (() => Promise<void>) | null,
  /** Make createMediaStreamSource throw (capture failure). */
  sourceThrows: false,
};

function installMocks(): void {
  sockets.length = 0;
  workletNodes.length = 0;
  mockCfg.wsThrowOn = null;
  mockCfg.addModule = null;
  mockCfg.sourceThrows = false;
  class WS {
    static OPEN = 1;
    url: string;
    readyState = 1;
    sent: unknown[] = [];
    onopen: (() => void) | null = null;
    onmessage: ((ev: { data: string | ArrayBuffer }) => void) | null = null;
    onerror: (() => void) | null = null;
    onclose: ((ev: { code: number; reason?: string }) => void) | null = null;
    binaryType = '';
    constructor(url: string) {
      if (mockCfg.wsThrowOn && url.includes(mockCfg.wsThrowOn)) throw new Error('ctor fail');
      this.url = url;
      sockets.push(this as unknown as FakeSocket);
    }
    send(d: unknown): void {
      this.sent.push(d);
    }
    close(): void {
      this.readyState = 3;
    }
  }
  (globalThis as unknown as { WebSocket: unknown }).WebSocket = WS;
  // Minimal AudioContext graph so startCapture() doesn't throw (its work is fire-and-forget).
  class Node {
    port: { onmessage: ((e: { data: ArrayBuffer }) => void) | null } = { onmessage: null };
    constructor() {
      workletNodes.push(this);
    }
    connect(n: unknown): unknown {
      return n;
    }
    disconnect(): void {}
  }
  class Src {
    connect(n: unknown): unknown {
      return n;
    }
    disconnect(): void {}
  }
  class Ctx {
    audioWorklet = {
      addModule: (): Promise<void> => (mockCfg.addModule ? mockCfg.addModule() : Promise.resolve()),
    };
    createMediaStreamSource(): Src {
      if (mockCfg.sourceThrows) throw new Error('source fail');
      return new Src();
    }
    createGain(): { gain: { value: number }; connect: (n: unknown) => unknown; disconnect: () => void } {
      return { gain: { value: 0 }, connect: (n) => n, disconnect() {} };
    }
    get destination(): unknown {
      return {};
    }
    // Rejects so the `.catch(() => {})` teardown guards are exercised too.
    close(): Promise<void> {
      return Promise.reject(new Error('close failed'));
    }
  }
  (globalThis as unknown as { AudioContext: unknown }).AudioContext = Ctx;
  // cartesia.ts reads `window.AudioContext` (browser global); expose it in the test env.
  (globalThis as unknown as { window: unknown }).window = { AudioContext: Ctx };
  (globalThis as unknown as { AudioWorkletNode: unknown }).AudioWorkletNode = Node;
  (globalThis as unknown as { MediaStream: unknown }).MediaStream = class {
    tracks: unknown[];
    constructor(tracks: unknown[] = [{}]) {
      this.tracks = tracks;
    }
    getAudioTracks(): unknown[] {
      return this.tracks;
    }
  };
}

interface FakeStream {
  tracks: unknown[];
  getAudioTracks(): unknown[];
}

function fakeStream(tracks: unknown[] = [{}]): MediaStream & FakeStream {
  return new (
    globalThis as unknown as { MediaStream: new (t?: unknown[]) => MediaStream & FakeStream }
  ).MediaStream(tracks);
}

describe('CartesiaManager', () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.spyOn(console, 'warn').mockImplementation(() => {});
    installMocks();
  });
  afterEach(() => {
    vi.useRealTimers();
    vi.restoreAllMocks();
  });

  function make(
    translated: string | null = 'hello',
    overrides: Partial<CartesiaManagerOptions> = {},
  ) {
    const onSubtitle = vi.fn();
    const onError = vi.fn();
    const played: Array<{ speakerId: string; seq: number }> = [];
    const m = new CartesiaManager({
      fetchSession: () => Promise.resolve(SESSION),
      translate: () => Promise.resolve(translated),
      onSubtitle,
      onError,
      playAudio: (speakerId, seq) => played.push({ speakerId, seq }),
      ttsEnabled: () => true,
      ...overrides,
    });
    return { m, onSubtitle, onError, played };
  }

  /** Register a peer, let the STT socket open, and complete the capture wiring. */
  async function openPipeline(m: CartesiaManager, peerId = 'p1', lang = 'it'): Promise<FakeSocket> {
    m.setPeerStream(peerId, fakeStream());
    m.setPeerLang(peerId, lang);
    await vi.runAllTimersAsync();
    const stt = sockets[sockets.length - 1];
    stt.onopen?.();
    await vi.runAllTimersAsync();
    return stt;
  }

  /** Deliver one finalized transcript and run the idle flush (translate + TTS). */
  async function utter(stt: FakeSocket, text: string): Promise<void> {
    stt.onmessage?.({ data: JSON.stringify({ type: 'transcript', is_final: true, text }) });
    await vi.runAllTimersAsync();
  }

  function ttsSockets(): FakeSocket[] {
    return sockets.filter((s) => s.url.includes('/tts/websocket'));
  }

  it('opens an STT socket only for a cross-language peer', async () => {
    const { m } = make();
    m.activate('en');
    m.setPeerStream('p1', fakeStream());
    m.setPeerLang('p1', 'en'); // same language → no pipeline
    await vi.runAllTimersAsync();
    expect(sockets.length).toBe(0);

    m.setPeerLang('p1', 'it'); // cross-language → STT socket
    await vi.runAllTimersAsync();
    expect(sockets.length).toBe(1);
    expect(sockets[0].url).toContain('/stt/websocket');
    expect(sockets[0].url).toContain('language=it');
  });

  it('translates a finalized segment and speaks it in the peer voice', async () => {
    const { m, onSubtitle, played } = make('hello there');
    m.activate('en');
    m.setPeerVoiceId('p1', 'voice-p1');
    m.setPeerStream('p1', fakeStream());
    m.setPeerLang('p1', 'it');
    await vi.runAllTimersAsync();
    const stt = sockets[0];
    stt.onopen?.();
    stt.onmessage?.({ data: JSON.stringify({ type: 'transcript', is_final: true, text: 'ciao' }) });
    // Idle-flush fires the translate → subtitle(final) → TTS path.
    await vi.runAllTimersAsync();
    expect(onSubtitle).toHaveBeenCalledWith('p1', 'hello there', false, 'ciao');
    // A TTS socket opened and a generation request carrying the peer's voice id was sent.
    const tts = sockets.find((s) => s.url.includes('/tts/websocket'));
    expect(tts).toBeTruthy();
    const gen = JSON.parse(tts!.sent[0] as string);
    expect(gen.voice).toEqual({ mode: 'id', id: 'voice-p1' });
    expect(gen.transcript).toBe('hello there');
    // A TTS chunk routes back to playback for that speaker.
    tts!.onmessage?.({ data: JSON.stringify({ type: 'chunk', context_id: gen.context_id, data: 'AAAA' }) });
    expect(played).toEqual([{ speakerId: 'p1', seq: 0 }]);
  });

  it('falls back to the default voice when a peer has no clone', async () => {
    const { m } = make('hi');
    m.activate('en');
    m.setPeerStream('p1', fakeStream());
    m.setPeerLang('p1', 'it');
    await vi.runAllTimersAsync();
    sockets[0].onopen?.();
    sockets[0].onmessage?.({ data: JSON.stringify({ type: 'transcript', is_final: true, text: 'ciao' }) });
    await vi.runAllTimersAsync();
    const tts = sockets.find((s) => s.url.includes('/tts/websocket'))!;
    expect(JSON.parse(tts.sent[0] as string).voice.id).toBe('voice-default');
  });

  it('tears down all sockets on deactivate', async () => {
    const { m } = make();
    m.activate('en');
    m.setPeerStream('p1', fakeStream());
    m.setPeerLang('p1', 'it');
    await vi.runAllTimersAsync();
    expect(sockets[0].readyState).toBe(1);
    m.deactivate();
    expect(sockets[0].readyState).toBe(3);
  });

  it('reports browser support from the required globals', () => {
    expect(CartesiaManager.supported).toBe(true);
    const g = globalThis as unknown as Record<string, unknown>;
    const saved = g.AudioContext;
    delete g.AudioContext;
    expect(CartesiaManager.supported).toBe(false);
    g.AudioContext = saved;
  });

  it('starts pipelines for peers that joined before activation', async () => {
    const { m } = make();
    m.setPeerStream('p1', fakeStream());
    m.setPeerLang('p1', 'it'); // not active yet → nothing starts
    await vi.runAllTimersAsync();
    expect(sockets.length).toBe(0);
    m.activate('en');
    await vi.runAllTimersAsync();
    expect(sockets.length).toBe(1);
  });

  it('setMyLang restarts pipelines toward the new target and stops collisions', async () => {
    const { m } = make();
    m.activate('en');
    m.setPeerStream('p1', fakeStream());
    m.setPeerLang('p1', 'it');
    await vi.runAllTimersAsync();
    expect(sockets.length).toBe(1);

    m.setMyLang('en'); // unchanged → no-op
    await vi.runAllTimersAsync();
    expect(sockets.length).toBe(1);

    m.setMyLang('fr'); // target changed → the pipeline restarts
    await vi.runAllTimersAsync();
    expect(sockets.length).toBe(2);
    expect(sockets[0].readyState).toBe(3);
    expect(sockets[1].readyState).toBe(1);

    m.setMyLang('it'); // now the same as the peer's language → stop translating them
    await vi.runAllTimersAsync();
    expect(sockets.length).toBe(2);
    expect(sockets[1].readyState).toBe(3);
  });

  it('restarts when the peer source language changes and ignores redundant updates', async () => {
    const { m } = make();
    m.activate('en');
    m.setPeerStream('p1', fakeStream());
    m.setPeerLang('p1', 'it');
    await vi.runAllTimersAsync();
    expect(sockets.length).toBe(1);

    m.setPeerLang('p1', 'it'); // unchanged lang → ignored
    m.setPeerStream('p1', fakeStream()); // running & unchanged → reconcile keeps it
    await vi.runAllTimersAsync();
    expect(sockets.length).toBe(1);

    m.setPeerLang('p1', 'es'); // source changed → restart with the new language
    await vi.runAllTimersAsync();
    expect(sockets.length).toBe(2);
    expect(sockets[0].readyState).toBe(3);
    expect(sockets[1].url).toContain('language=es');
  });

  it('ignores auto-detect peers and streams without an audio track', async () => {
    const { m } = make();
    m.activate('en');
    m.setPeerStream('p1', fakeStream());
    m.setPeerLang('p1', 'auto'); // undetected language → wait
    await vi.runAllTimersAsync();
    expect(sockets.length).toBe(0);

    m.setPeerStream('p2', fakeStream([])); // no audio track yet
    m.setPeerLang('p2', 'it');
    await vi.runAllTimersAsync();
    expect(sockets.length).toBe(0);

    m.setPeerStream('p2', fakeStream()); // the track arrived → start
    await vi.runAllTimersAsync();
    expect(sockets.length).toBe(1);
  });

  it('removePeer tears the pipeline down and later frames are ignored', async () => {
    const { m, onSubtitle } = make();
    m.activate('en');
    const stt = await openPipeline(m);
    m.removePeer('p1');
    expect(stt.readyState).toBe(3);
    stt.onmessage?.({ data: JSON.stringify({ type: 'transcript', is_final: false, text: 'ciao' }) });
    await vi.runAllTimersAsync();
    expect(onSubtitle).not.toHaveBeenCalled();
    m.removePeer('ghost'); // unknown peer → no-op
  });

  it('caches the minted session and refreshes near expiry', async () => {
    const now = Math.floor(Date.now() / 1000);
    const fetchSession = vi
      .fn<() => Promise<CartesiaSession | null>>()
      .mockResolvedValueOnce({ ...SESSION, expiresAt: now + 30 }) // about to expire
      .mockResolvedValue(SESSION);
    const { m } = make('x', { fetchSession });
    m.activate('en');
    m.setPeerStream('p1', fakeStream());
    m.setPeerLang('p1', 'it');
    await vi.runAllTimersAsync();
    expect(fetchSession).toHaveBeenCalledTimes(1);

    m.setPeerStream('p2', fakeStream());
    m.setPeerLang('p2', 'fr');
    await vi.runAllTimersAsync();
    expect(fetchSession).toHaveBeenCalledTimes(2); // stale token → re-minted

    m.setPeerStream('p3', fakeStream());
    m.setPeerLang('p3', 'es');
    await vi.runAllTimersAsync();
    expect(fetchSession).toHaveBeenCalledTimes(2); // fresh token → cached
    expect(sockets.length).toBe(3);
  });

  it('does not start a pipeline when the session mint fails', async () => {
    const { m } = make('x', { fetchSession: () => Promise.resolve(null) });
    m.activate('en');
    m.setPeerStream('p1', fakeStream());
    m.setPeerLang('p1', 'it');
    await vi.runAllTimersAsync();
    expect(sockets.length).toBe(0);
  });

  it('bails out of a mint that finishes after deactivate or a language change', async () => {
    const { m } = make();
    m.activate('en');
    m.setPeerStream('p1', fakeStream());
    m.setPeerLang('p1', 'it');
    m.deactivate(); // deactivated while minting → no socket
    await vi.runAllTimersAsync();
    expect(sockets.length).toBe(0);

    m.activate('en');
    m.setPeerStream('p1', fakeStream());
    m.setPeerLang('p1', 'it');
    m.setPeerLang('p1', 'es'); // changed while minting → only the es pipeline survives
    await vi.runAllTimersAsync();
    expect(sockets.length).toBe(1);
    expect(sockets[0].url).toContain('language=es');
  });

  it('does not double-start when reconciles race during the mint', async () => {
    const { m } = make();
    m.activate('en');
    m.setPeerStream('p1', fakeStream());
    m.setPeerLang('p1', 'it'); // start #1 (minting…)
    m.setPeerStream('p1', fakeStream()); // start #2 races the same peer
    await vi.runAllTimersAsync();
    expect(sockets.length).toBe(1);
  });

  it('gives up immediately when the STT socket cannot be constructed', async () => {
    mockCfg.wsThrowOn = '/stt/';
    const { m, onError } = make();
    m.activate('en');
    m.setPeerStream('p1', fakeStream());
    m.setPeerLang('p1', 'it');
    await vi.runAllTimersAsync();
    expect(sockets.length).toBe(0);
    expect(onError).toHaveBeenCalledTimes(1);
    expect(onError).toHaveBeenCalledWith('p1', 'websocket_error', expect.stringContaining('ctor fail'));
  });

  it('retries transient STT failures with backoff before giving up', async () => {
    const { m, onError } = make();
    m.activate('en');
    m.setPeerStream('p1', fakeStream());
    m.setPeerLang('p1', 'it');
    await vi.runAllTimersAsync();
    expect(sockets.length).toBe(1);

    sockets[0].onerror?.(); // attempt 0 fails → retry in 1 s
    expect(sockets[0].readyState).toBe(3);
    await vi.advanceTimersByTimeAsync(999);
    expect(sockets.length).toBe(1); // backoff not elapsed yet
    await vi.advanceTimersByTimeAsync(1);
    expect(sockets.length).toBe(2);

    // An unexpected close is transient too; the reason is surfaced in the message.
    sockets[1].onclose?.({ code: 1008, reason: 'Invalid language for model' });
    await vi.advanceTimersByTimeAsync(2000);
    expect(sockets.length).toBe(3);

    sockets[2].onclose?.({ code: 1006 }); // reasonless close
    await vi.advanceTimersByTimeAsync(4000);
    expect(sockets.length).toBe(4);
    expect(onError).not.toHaveBeenCalled();

    sockets[3].onerror?.(); // retries exhausted → give up + notify
    await vi.runAllTimersAsync();
    expect(sockets.length).toBe(4);
    expect(onError).toHaveBeenCalledTimes(1);
    expect(onError).toHaveBeenCalledWith('p1', 'websocket_error', 'stt socket error');

    // Gave-up peers are skipped until something changes…
    m.setPeerStream('p1', fakeStream());
    await vi.runAllTimersAsync();
    expect(sockets.length).toBe(4);
    // …but a source-language change re-arms the peer.
    m.setPeerLang('p1', 'es');
    await vi.runAllTimersAsync();
    expect(sockets.length).toBe(5);
  });

  it('a reconcile during the backoff window cancels the pending retry', async () => {
    const { m } = make();
    m.activate('en');
    m.setPeerStream('p1', fakeStream());
    m.setPeerLang('p1', 'it');
    await vi.runAllTimersAsync();
    sockets[0].onerror?.(); // schedules a retry in 1 s
    m.setPeerLang('p1', 'es'); // reconcile supersedes the retry with a fresh start
    await vi.advanceTimersByTimeAsync(0);
    expect(sockets.length).toBe(2);
    expect(sockets[1].url).toContain('language=es');
    await vi.runAllTimersAsync(); // the cancelled retry never fires a duplicate
    expect(sockets.length).toBe(2);
  });

  it('deactivate cancels a pending retry', async () => {
    const { m, onError } = make();
    m.activate('en');
    m.setPeerStream('p1', fakeStream());
    m.setPeerLang('p1', 'it');
    await vi.runAllTimersAsync();
    sockets[0].onerror?.();
    m.deactivate();
    await vi.runAllTimersAsync();
    expect(sockets.length).toBe(1);
    expect(onError).not.toHaveBeenCalled();
  });

  it('streams captured PCM frames only while the socket is open and the pipe live', async () => {
    const { m } = make();
    m.activate('en');
    const stt = await openPipeline(m);
    expect(workletNodes.length).toBe(1);
    const handler = workletNodes[0].port.onmessage!;
    const frame = new ArrayBuffer(4);
    handler({ data: frame });
    expect(stt.sent).toContain(frame);
    stt.readyState = 0; // socket no longer open → dropped
    handler({ data: new ArrayBuffer(2) });
    expect(stt.sent.length).toBe(1);
    stt.readyState = 1;
    m.removePeer('p1'); // stopped pipe → dropped even via a stale handler ref
    handler({ data: new ArrayBuffer(2) });
    expect(stt.sent.length).toBe(1);
  });

  it('abandons capture when the pipeline stops while the worklet loads', async () => {
    let release!: () => void;
    mockCfg.addModule = () =>
      new Promise<void>((r) => {
        release = r;
      });
    const { m } = make();
    m.activate('en');
    m.setPeerStream('p1', fakeStream());
    m.setPeerLang('p1', 'it');
    await vi.runAllTimersAsync();
    sockets[0].onopen?.();
    m.removePeer('p1'); // torn down mid-load
    release();
    await vi.runAllTimersAsync();
    expect(workletNodes.length).toBe(0);
  });

  it('abandons capture when the audio track vanished before the worklet loaded', async () => {
    const { m } = make();
    const stream = fakeStream();
    m.activate('en');
    m.setPeerStream('p1', stream);
    m.setPeerLang('p1', 'it');
    await vi.runAllTimersAsync();
    stream.tracks = []; // the track died between open and capture
    sockets[0].onopen?.();
    await vi.runAllTimersAsync();
    expect(workletNodes.length).toBe(0);
  });

  it('recovers from a capture failure via the retry path (media_error)', async () => {
    mockCfg.sourceThrows = true;
    const { m } = make();
    m.activate('en');
    await openPipeline(m);
    // Capture blew up → the pipeline was torn down and the backoff retry already ran.
    expect(sockets.length).toBe(2);
    expect(sockets[0].readyState).toBe(3);
    expect(sockets[1].readyState).toBe(1);
  });

  it('falls back to webkitAudioContext when window.AudioContext is missing', async () => {
    const w = (globalThis as unknown as { window: Record<string, unknown> }).window;
    w.webkitAudioContext = w.AudioContext;
    delete w.AudioContext;
    const { m } = make();
    m.activate('en');
    const stt = await openPipeline(m);
    expect(workletNodes.length).toBe(1); // capture wired through the prefixed ctor
    expect(stt.readyState).toBe(1);
  });

  it('resets the idle timer across interims and skips empty flushes', async () => {
    const { m, onSubtitle } = make();
    m.activate('en');
    const stt = await openPipeline(m);

    stt.onmessage?.({ data: 'not json' }); // keepalive frame → ignored
    stt.onmessage?.({ data: new ArrayBuffer(0) }); // binary frame → ignored
    stt.onmessage?.({ data: JSON.stringify({ type: 'transcript', is_final: false, text: '' }) });
    expect(onSubtitle).not.toHaveBeenCalled(); // nothing to show yet
    await vi.advanceTimersByTimeAsync(IDLE_FLUSH_MS + 10); // empty segment → no flush output
    expect(onSubtitle).not.toHaveBeenCalled();

    stt.onmessage?.({ data: JSON.stringify({ type: 'transcript', is_final: false, text: 'ciao' }) });
    expect(onSubtitle).toHaveBeenCalledWith('p1', 'ciao', true);
    await vi.advanceTimersByTimeAsync(500);
    stt.onmessage?.({ data: JSON.stringify({ type: 'transcript', is_final: false, text: 'ciao amico' }) });
    await vi.advanceTimersByTimeAsync(500); // 1 s after the first, 0.5 s after the second
    expect(onSubtitle).not.toHaveBeenCalledWith('p1', expect.anything(), false, expect.anything()); // timer was reset
    await vi.runAllTimersAsync(); // idle elapsed → interim-only text still flushes
    expect(onSubtitle).toHaveBeenCalledWith('p1', 'hello', false, 'ciao amico');
  });

  it('falls back to the source text when translation fails, honouring the TTS toggle', async () => {
    const { m, onSubtitle } = make(null, { ttsEnabled: () => false });
    m.activate('en');
    const stt = await openPipeline(m);
    await utter(stt, 'ciao');
    expect(onSubtitle).toHaveBeenCalledWith('p1', 'ciao', false, 'ciao'); // untranslated fallback
    expect(ttsSockets().length).toBe(0); // spoken translation off → subtitles only
  });

  it('skips rendering when the translation comes back blank', async () => {
    const { m, onSubtitle } = make('   ');
    m.activate('en');
    const stt = await openPipeline(m);
    await utter(stt, 'ciao');
    expect(onSubtitle).toHaveBeenCalledWith('p1', 'ciao', true);
    expect(onSubtitle).not.toHaveBeenCalledWith('p1', expect.anything(), false, expect.anything());
    expect(ttsSockets().length).toBe(0);
  });

  it('swallows a translate rejection', async () => {
    const { m, onSubtitle } = make('x', { translate: () => Promise.reject(new Error('groq down')) });
    m.activate('en');
    const stt = await openPipeline(m);
    await utter(stt, 'ciao');
    expect(onSubtitle).toHaveBeenCalledWith('p1', 'ciao', true);
    expect(onSubtitle).not.toHaveBeenCalledWith('p1', expect.anything(), false, expect.anything());
  });

  it('drops a translation that resolves after the pipeline restarted', async () => {
    let resolveT!: (t: string | null) => void;
    const { m, onSubtitle } = make('x', {
      translate: () =>
        new Promise<string | null>((r) => {
          resolveT = r;
        }),
    });
    m.activate('en');
    const stt = await openPipeline(m);
    stt.onmessage?.({ data: JSON.stringify({ type: 'transcript', is_final: true, text: 'ciao' }) });
    await vi.advanceTimersByTimeAsync(IDLE_FLUSH_MS); // flush → translation in flight
    m.setPeerLang('p1', 'es'); // restart replaces the pipeline object
    await vi.runAllTimersAsync();
    resolveT('too late');
    await vi.runAllTimersAsync();
    expect(onSubtitle).not.toHaveBeenCalledWith('p1', 'too late', false, expect.anything());
  });

  it('renders subtitles only when there is no clone and no default voice', async () => {
    const { m, onSubtitle } = make('hola', {
      fetchSession: () => Promise.resolve({ ...SESSION, defaultVoiceId: undefined }),
    });
    m.activate('en');
    const stt = await openPipeline(m);
    await utter(stt, 'ciao');
    expect(onSubtitle).toHaveBeenCalledWith('p1', 'hola', false, 'ciao');
    expect(ttsSockets().length).toBe(1);
    expect(ttsSockets()[0].sent.length).toBe(0); // socket opened, but nothing to speak with
  });

  it('setPeerVoiceId(null) clears the clone so the default voice is used', async () => {
    const { m } = make('hola');
    m.activate('en');
    m.setPeerVoiceId('p1', 'voice-p1');
    m.setPeerVoiceId('p1', null);
    const stt = await openPipeline(m);
    await utter(stt, 'ciao');
    const gen = JSON.parse(ttsSockets()[0].sent[0] as string) as { voice: { id: string } };
    expect(gen.voice.id).toBe('voice-default');
  });

  it('degrades to subtitles when the TTS socket cannot be constructed', async () => {
    mockCfg.wsThrowOn = '/tts/';
    const { m, onSubtitle } = make('hola');
    m.activate('en');
    const stt = await openPipeline(m);
    await utter(stt, 'ciao');
    expect(onSubtitle).toHaveBeenCalledWith('p1', 'hola', false, 'ciao');
    expect(ttsSockets().length).toBe(0);
  });

  it('routes TTS chunks per context and clears routing on done/error/close', async () => {
    const { m, played } = make('uno');
    m.activate('en');
    const stt = await openPipeline(m);
    await utter(stt, 'ciao');
    const tts = ttsSockets()[0];
    const gen1 = JSON.parse(tts.sent[0] as string) as {
      context_id: string;
      language: string;
      model_id: string;
    };
    expect(gen1.language).toBe('en'); // spoken in the LISTENER's language
    expect(gen1.model_id).toBe('sonic-3.5');

    tts.onmessage?.({ data: new ArrayBuffer(4) }); // binary frame → ignored
    tts.onmessage?.({ data: 'not json' }); // garbage → ignored
    tts.onmessage?.({ data: JSON.stringify({ type: 'chunk', context_id: 'unknown', data: 'AAAA' }) });
    tts.onerror?.(); // a TTS hiccup is silent
    expect(played.length).toBe(0);

    tts.onmessage?.({ data: JSON.stringify({ type: 'chunk', context_id: gen1.context_id, data: 'AAAA' }) });
    tts.onmessage?.({ data: JSON.stringify({ type: 'chunk', context_id: gen1.context_id, data: 'BBBB' }) });
    expect(played).toEqual([
      { speakerId: 'p1', seq: 0 },
      { speakerId: 'p1', seq: 1 },
    ]);

    tts.onmessage?.({ data: JSON.stringify({ type: 'done', context_id: gen1.context_id }) });
    tts.onmessage?.({ data: JSON.stringify({ type: 'chunk', context_id: gen1.context_id, data: 'CCCC' }) });
    expect(played.length).toBe(2); // routing was cleared by `done`

    // A second utterance reuses the same socket with a fresh context.
    await utter(stt, 'ancora');
    expect(ttsSockets().length).toBe(1);
    const gen2 = JSON.parse(tts.sent[1] as string) as { context_id: string };
    expect(gen2.context_id).not.toBe(gen1.context_id);
    tts.onmessage?.({ data: JSON.stringify({ type: 'error', context_id: gen2.context_id }) });
    tts.onmessage?.({ data: JSON.stringify({ type: 'chunk', context_id: gen2.context_id, data: 'DDDD' }) });
    expect(played.length).toBe(2); // routing was cleared by `error`

    // A socket close clears everything; the next utterance opens a fresh TTS socket.
    tts.onclose?.({ code: 1006 });
    await utter(stt, 'terzo');
    expect(ttsSockets().length).toBe(2);
  });

  it('holds speech while the TTS socket is connecting and replaces a dead one', async () => {
    const { m } = make('uno');
    m.activate('en');
    const stt = await openPipeline(m);
    await utter(stt, 'ciao');
    const tts = ttsSockets()[0];
    expect(tts.sent.length).toBe(1);

    tts.readyState = 0; // still connecting → this utterance is skipped (no queueing)
    await utter(stt, 'due');
    expect(tts.sent.length).toBe(1);

    tts.readyState = 3; // dead socket → a fresh one is opened for the next utterance
    await utter(stt, 'tre');
    expect(ttsSockets().length).toBe(2);
    expect(ttsSockets()[1].sent.length).toBe(1);
  });

  it('uses the shared pcmPlayback graph and TTS-on default when not injected', async () => {
    const enq = vi.spyOn(pcmPlayback, 'enqueue').mockImplementation(() => {});
    const onSubtitle = vi.fn();
    const m = new CartesiaManager({
      fetchSession: () => Promise.resolve(SESSION),
      translate: () => Promise.resolve('hi'),
      onSubtitle,
    });
    m.activate('en');
    const stt = await openPipeline(m);
    await utter(stt, 'ciao');
    const tts = ttsSockets()[0];
    const gen = JSON.parse(tts.sent[0] as string) as { context_id: string };
    tts.onmessage?.({ data: JSON.stringify({ type: 'chunk', context_id: gen.context_id, data: 'AAAA' }) });
    expect(enq).toHaveBeenCalledWith('p1', 0, 'AAAA');

    m.deactivate(); // also closes the shared TTS socket
    expect(tts.readyState).toBe(3);
    tts.onclose?.({ code: 1000 }); // stale close after teardown → ignored
  });
});
