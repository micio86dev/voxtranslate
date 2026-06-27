// Tests for the client-direct Enhanced pipeline (spec 0108, Cartesia). The pure transcript
// + URL helpers are deterministic; the manager lifecycle is exercised with minimal
// WebSocket / AudioContext / MediaStream mocks so the reconcile + flush + translate + TTS
// flow is covered without a live provider.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import {
  accumulate,
  CartesiaManager,
  type CartesiaSession,
  liveLine,
  sttUrl,
  ttsUrl,
} from './cartesia';

const SESSION: CartesiaSession = {
  token: 'tok_abc',
  expiresAt: Math.floor(Date.now() / 1000) + 3600,
  cartesiaVersion: '2026-03-01',
  sttEndpoint: 'wss://api.cartesia.ai/stt/websocket',
  sttModel: 'ink-2',
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
});

describe('sttUrl / ttsUrl', () => {
  it('builds the STT URL with auth + format query params', () => {
    const u = new URL(sttUrl(SESSION, 'it'));
    expect(u.origin + u.pathname).toBe('wss://api.cartesia.ai/stt/websocket');
    expect(u.searchParams.get('model')).toBe('ink-2');
    expect(u.searchParams.get('encoding')).toBe('pcm_s16le');
    expect(u.searchParams.get('sample_rate')).toBe('16000');
    expect(u.searchParams.get('cartesia_version')).toBe('2026-03-01');
    expect(u.searchParams.get('access_token')).toBe('tok_abc');
    expect(u.searchParams.get('language')).toBe('it');
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
  onclose: ((ev: { code: number }) => void) | null;
  close: () => void;
  send: (d: unknown) => void;
}

const sockets: FakeSocket[] = [];

function installMocks(): void {
  sockets.length = 0;
  class WS {
    static OPEN = 1;
    url: string;
    readyState = 1;
    sent: unknown[] = [];
    onopen: (() => void) | null = null;
    onmessage: ((ev: { data: string | ArrayBuffer }) => void) | null = null;
    onerror: (() => void) | null = null;
    onclose: ((ev: { code: number }) => void) | null = null;
    binaryType = '';
    constructor(url: string) {
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
    port = { onmessage: null as unknown };
    connect(n: unknown): unknown {
      return n;
    }
    disconnect(): void {}
  }
  class Ctx {
    audioWorklet = { addModule: () => Promise.resolve() };
    createMediaStreamSource(): Node {
      return new Node();
    }
    createGain(): { gain: { value: number }; connect: (n: unknown) => unknown; disconnect: () => void } {
      return { gain: { value: 0 }, connect: (n) => n, disconnect() {} };
    }
    get destination(): unknown {
      return {};
    }
    close(): Promise<void> {
      return Promise.resolve();
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

function fakeStream(): MediaStream {
  return new (globalThis as unknown as { MediaStream: new () => MediaStream }).MediaStream();
}

describe('CartesiaManager', () => {
  beforeEach(() => {
    vi.useFakeTimers();
    installMocks();
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  function make(translated = 'hello') {
    const onSubtitle = vi.fn();
    const played: Array<{ speakerId: string; seq: number }> = [];
    const m = new CartesiaManager({
      fetchSession: () => Promise.resolve(SESSION),
      translate: () => Promise.resolve(translated),
      onSubtitle,
      playAudio: (speakerId, seq) => played.push({ speakerId, seq }),
      ttsEnabled: () => true,
    });
    return { m, onSubtitle, played };
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
    expect(onSubtitle).toHaveBeenCalledWith('p1', 'hello there', false);
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
});
