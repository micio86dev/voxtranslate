import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { EnhancedSessionResponse } from '../api';
import type { SttGraph } from '../cartesia';
import type { ApiResult } from './api';
import {
  EnhancedEngine,
  MAX_SPEECH_QUEUE,
  MAX_TEXT_CHARS,
  OPEN_TIMEOUT_MS,
  PARTIAL_MIN_INTERVAL_MS,
  REFRESH_LEAD_MS,
  STABLE_MS,
  STT_RETRIES,
  TTS_FAILURE_LIMIT,
  UTTERANCE_TIMEOUT_MS,
  chunkText,
  classifyTokenError,
  type EnhancedDeps,
  type EnhancedFailure,
} from './enhanced';

class FakeWs {
  readyState = 0;
  binaryType = '';
  sent: Array<string | ArrayBufferLike | Blob> = [];
  onopen: (() => void) | null = null;
  onmessage: ((ev: { data: unknown }) => void) | null = null;
  onerror: (() => void) | null = null;
  onclose: ((ev: { code?: number }) => void) | null = null;
  closed = false;
  constructor(readonly url: string) {}
  send(d: string | ArrayBufferLike | Blob): void {
    this.sent.push(d);
  }
  close(): void {
    this.closed = true;
    this.readyState = 3;
  }
  open(): void {
    this.readyState = 1;
    this.onopen?.();
  }
  say(msg: unknown): void {
    this.onmessage?.({ data: JSON.stringify(msg) });
  }
  drop(): void {
    this.readyState = 3;
    this.onclose?.({ code: 1006 });
  }
  json(): Array<Record<string, unknown>> {
    return this.sent.filter((s): s is string => typeof s === 'string').map((s) => JSON.parse(s));
  }
}

const token = (over: Partial<EnhancedSessionResponse> = {}, expiresIn = 3600): ApiResult<EnhancedSessionResponse> => ({
  ok: true,
  data: {
    token: 'TOK-1',
    expires_at: Math.floor(Date.now() / 1000) + expiresIn,
    cartesia_version: '2025-04-16',
    stt: { endpoint: 'wss://stt.test/stt/websocket', model: 'ink-whisper', models_by_lang: { en: 'ink-2' } },
    tts: { endpoint: 'wss://tts.test/tts/websocket', model: 'sonic-3' },
    voice_cloning_enabled: true,
    default_voice_id: 'DEFAULT-VOICE',
    ...over,
  },
});
const refused = (status: number, code: string): ApiResult<EnhancedSessionResponse> => ({ ok: false, status, code });

function setup(over: Partial<EnhancedDeps> = {}) {
  const sockets: FakeWs[] = [];
  const control: Array<Record<string, unknown>> = [];
  const mine: Array<[string, boolean]> = [];
  const failures: EnhancedFailure[] = [];
  const audio: Array<[string, number, string]> = [];
  const graphs: Array<{ onChunk: (b: ArrayBuffer) => void; graph: SttGraph; stopped: () => boolean }> = [];
  const fetchToken = vi.fn(async (): Promise<ApiResult<EnhancedSessionResponse>> => token());
  const engine = new EnhancedEngine({
    stream: {} as MediaStream,
    myLang: 'it',
    fetchToken,
    control: (m) => control.push(m),
    onMine: (t, f) => mine.push([t, f]),
    onFailure: (f) => failures.push(f),
    playAudio: (s, q, b) => audio.push([s, q, b]),
    createSocket: (url) => {
      const w = new FakeWs(url);
      sockets.push(w);
      return w;
    },
    openGraph: async (_stream, onChunk) => {
      let stopped = false;
      const graph = {
        ctx: { close: vi.fn(async () => { stopped = true; }) },
        node: { port: { onmessage: null }, disconnect: vi.fn() },
        src: { disconnect: vi.fn() },
      } as unknown as SttGraph;
      graphs.push({ onChunk, graph, stopped: () => stopped });
      return graph;
    },
    isRestricted: async () => false,
    ...over,
  });
  const stt = () => sockets.filter((s) => s.url.startsWith('wss://stt.test'));
  const tts = () => sockets.filter((s) => s.url.startsWith('wss://tts.test'));
  return { engine, sockets, stt, tts, control, mine, failures, audio, graphs, fetchToken };
}

const tick = (ms = 0) => vi.advanceTimersByTimeAsync(ms);
const frames = (h: ReturnType<typeof setup>) => h.control.filter((m) => m.type === 'translate_text');

/** Open the token, start recognising and connect the STT socket. */
async function listening(over: Partial<EnhancedDeps> = {}) {
  const h = setup(over);
  expect(await h.engine.open()).toBe(true);
  h.engine.start();
  h.stt()[0].open();
  await tick();
  return h;
}

beforeEach(() => {
  vi.useFakeTimers();
  vi.setSystemTime(new Date('2026-01-01T00:00:00Z'));
});
afterEach(() => {
  vi.useRealTimers();
});

describe('chunkText', () => {
  it('splits on word boundaries and never exceeds the limit', () => {
    const text = Array.from({ length: 400 }, (_, i) => `word${i}`).join(' ');
    const parts = chunkText(text);
    expect(parts.length).toBeGreaterThan(1);
    expect(parts.every((p) => p.length <= MAX_TEXT_CHARS)).toBe(true);
    expect(parts.join(' ')).toBe(text);
  });
  it('cuts a single huge word in the middle rather than loop', () => {
    expect(chunkText('x'.repeat(2500)).map((p) => p.length)).toEqual([1000, 1000, 500]);
  });
  it('leaves a short text alone and drops an empty one', () => {
    expect(chunkText('ciao')).toEqual(['ciao']);
    expect(chunkText('   ')).toEqual([]);
  });
});

describe('classifyTokenError', () => {
  it.each([
    [429, 'rate_limited', { kind: 'fallback', reason: 'rate_limited' }],
    [402, 'insufficient_balance', { kind: 'fallback', reason: 'insufficient_balance' }],
    [502, 'enhanced_unavailable', { kind: 'fallback', reason: 'enhanced_unavailable' }],
    [409, 'not_enhanced', { kind: 'not_enhanced' }],
    [401, 'unauthorized', { kind: 'gone', code: 'unauthorized' }],
    [403, 'wrong_session', { kind: 'gone', code: 'wrong_session' }],
    [410, 'session_ended', { kind: 'gone', code: 'session_ended' }],
    [0, 'network', { kind: 'fallback', reason: 'cartesia_unreachable' }],
    [500, 'server_error', { kind: 'fallback', reason: 'token_refused' }],
  ])('%i %s', (status, code, want) => {
    expect(classifyTokenError(status, code)).toEqual(want);
  });
});

describe('token and start', () => {
  it('fetches the token before anything is sent, and opens no socket until asked to listen', async () => {
    const h = setup();
    expect(await h.engine.open()).toBe(true);
    expect(h.fetchToken).toHaveBeenCalledTimes(1);
    expect(h.sockets).toEqual([]);
    expect(h.control).toEqual([]);
  });

  it('opens STT on my language with the token, sends `start` only once the recogniser is connected, and streams audio', async () => {
    const h = setup();
    await h.engine.open();
    h.engine.start();
    const ws = h.stt()[0];
    const url = new URL(ws.url);
    expect(url.searchParams.get('access_token')).toBe('TOK-1');
    expect(url.searchParams.get('language')).toBe('it');
    expect(url.searchParams.get('model')).toBe('ink-whisper');
    expect(ws.binaryType).toBe('arraybuffer');
    expect(h.control).toEqual([]); // not connected yet: nothing to meter
    ws.open();
    await tick();
    expect(h.control).toEqual([{ type: 'start' }]);
    const pcm = new ArrayBuffer(8);
    h.graphs[0].onChunk(pcm);
    expect(ws.sent).toEqual([pcm]);
  });

  it('start() before the token arrives waits for it', async () => {
    let release!: (r: ApiResult<EnhancedSessionResponse>) => void;
    const h = setup({ fetchToken: () => new Promise((r) => { release = r; }) });
    const opened = h.engine.open();
    h.engine.start();
    expect(h.sockets).toEqual([]);
    release(token());
    expect(await opened).toBe(true);
    expect(h.stt()).toHaveLength(1);
  });

  it('open() is shared: a second call does not fetch again', async () => {
    const h = setup();
    await Promise.all([h.engine.open(), h.engine.open()]);
    expect(h.fetchToken).toHaveBeenCalledTimes(1);
  });

  it('sends `start` once even if the recogniser is reconnected, and `stop` when it ends', async () => {
    const h = await listening();
    h.stt()[0].drop();
    await tick(1000);
    h.stt()[1].open();
    await tick();
    expect(h.control.filter((m) => m.type === 'start')).toHaveLength(1);
    h.engine.stop();
    expect(h.control.filter((m) => m.type === 'stop')).toHaveLength(1);
    h.engine.stop();
    expect(h.control.filter((m) => m.type === 'stop')).toHaveLength(1);
  });

  it('gating holds audio back from the recogniser and releasing it resumes', async () => {
    const h = await listening();
    const ws = h.stt()[0];
    h.engine.setGated(true);
    h.graphs[0].onChunk(new ArrayBuffer(2));
    expect(ws.sent).toEqual([]);
    h.engine.setGated(false);
    h.graphs[0].onChunk(new ArrayBuffer(4));
    expect(ws.sent).toHaveLength(1);
  });
});

describe('speaker side: transcripts to translate_text', () => {
  const say = (h: Awaited<ReturnType<typeof listening>>, text: string, is_final = false) =>
    h.stt()[0].say({ type: 'transcript', text, is_final });

  it('shows my own words locally and sends one stable segment_id for the partials and the final', async () => {
    const h = await listening();
    say(h, 'Buon');
    say(h, 'Buongiorno a tutti');
    await tick(PARTIAL_MIN_INTERVAL_MS);
    say(h, 'Buongiorno a tutti', true);
    await tick(900);
    const sent = frames(h);
    expect(sent.length).toBeGreaterThanOrEqual(2);
    const ids = new Set(sent.map((m) => m.segment_id));
    expect(ids.size).toBe(1);
    const id = [...ids][0] as string;
    expect(id.length).toBeGreaterThanOrEqual(1);
    expect(id.length).toBeLessThanOrEqual(64);
    expect(sent.at(-1)).toMatchObject({ type: 'translate_text', text: 'Buongiorno a tutti', final: true });
    expect(sent.slice(0, -1).every((m) => m.final === false)).toBe(true);
    expect(h.mine.at(-1)).toEqual(['Buongiorno a tutti', true]);
    expect(h.mine[0]).toEqual(['Buon', false]);
  });

  it('coalesces partials: the first goes out at once, the rest collapse to the latest text', async () => {
    const h = await listening();
    for (const t of ['a', 'a b', 'a b c', 'a b c d']) say(h, t);
    expect(frames(h).map((m) => m.text)).toEqual(['a']);
    await tick(PARTIAL_MIN_INTERVAL_MS);
    expect(frames(h).map((m) => m.text)).toEqual(['a', 'a b c d']);
    expect(frames(h).every((m) => m.final === false)).toBe(true);
  });

  it('never exceeds 8 frames a second however fast the recogniser talks', async () => {
    const h = await listening();
    for (let i = 0; i < 100; i++) {
      say(h, `word ${i}`);
      await tick(10);
    }
    expect(frames(h).length).toBeLessThanOrEqual(8);
  });

  it('the next sentence gets a new segment_id', async () => {
    const h = await listening();
    say(h, 'uno', true);
    await tick(900);
    say(h, 'due', true);
    await tick(900);
    const finals = frames(h).filter((m) => m.final === true);
    expect(finals.map((m) => m.text)).toEqual(['uno', 'due']);
    expect(finals[0].segment_id).not.toBe(finals[1].segment_id);
  });

  it('sends only the tail of an over-long partial and splits an over-long final into valid frames', async () => {
    const h = await listening();
    const long = Array.from({ length: 500 }, (_, i) => `w${i}`).join(' ');
    say(h, long);
    expect((frames(h)[0].text as string).length).toBeLessThanOrEqual(MAX_TEXT_CHARS);
    say(h, long, true);
    await tick(900);
    const finals = frames(h).filter((m) => m.final === true);
    expect(finals.length).toBeGreaterThan(1);
    expect(finals.every((m) => (m.text as string).length <= MAX_TEXT_CHARS && (m.segment_id as string).length <= 64)).toBe(true);
    expect(finals[0].segment_id).not.toBe(finals[1].segment_id);
  });

  it('ignores empty transcripts and keep-alive frames', async () => {
    const h = await listening();
    say(h, '');
    h.stt()[0].onmessage?.({ data: 'not json' });
    h.stt()[0].onmessage?.({ data: new ArrayBuffer(1) });
    await tick(2000);
    expect(frames(h)).toEqual([]);
    expect(h.mine).toEqual([]);
  });

  it('stop() drops a half-said sentence: nothing is sent afterwards', async () => {
    const h = await listening();
    say(h, 'mezza frase');
    const before = frames(h).length;
    h.engine.stop();
    await tick(2000);
    expect(frames(h).length).toBe(before);
    expect(h.stt()[0].closed).toBe(true);
    expect(h.graphs[0].stopped()).toBe(true);
  });

  it('mute stops the recogniser and unmute starts a new one (setMuted)', async () => {
    const h = await listening();
    h.engine.setMuted(true);
    expect(h.stt()[0].closed).toBe(true);
    expect(h.control.at(-1)).toEqual({ type: 'stop' });
    h.engine.setMuted(false);
    expect(h.stt()).toHaveLength(2);
  });
});

describe('failures end in one of three outcomes', () => {
  it.each([
    [429, 'rate_limited', { kind: 'fallback', reason: 'rate_limited' }],
    [502, 'enhanced_unavailable', { kind: 'fallback', reason: 'enhanced_unavailable' }],
    [402, 'insufficient_balance', { kind: 'fallback', reason: 'insufficient_balance' }],
    [409, 'not_enhanced', { kind: 'not_enhanced' }],
    [410, 'session_ended', { kind: 'gone', code: 'session_ended' }],
  ])('a refused token (%i) is reported once and nothing is opened', async (status, code, want) => {
    const h = setup({ fetchToken: async () => refused(status, code) });
    expect(await h.engine.open()).toBe(false);
    h.engine.start();
    expect(h.failures).toEqual([want]);
    expect(h.sockets).toEqual([]);
    expect(h.control).toEqual([]);
  });

  it('a dead network is retried once, then Cartesia counts as unreachable', async () => {
    const fetchToken = vi.fn(async () => refused(0, 'network'));
    const h = setup({ fetchToken });
    const opened = h.engine.open();
    await tick(1500);
    expect(await opened).toBe(false);
    expect(fetchToken).toHaveBeenCalledTimes(2);
    expect(h.failures).toEqual([{ kind: 'fallback', reason: 'cartesia_unreachable' }]);
  });

  it('a network blip on the first try recovers on the second', async () => {
    const fetchToken = vi.fn().mockResolvedValueOnce(refused(0, 'network')).mockResolvedValue(token());
    const h = setup({ fetchToken });
    const opened = h.engine.open();
    await tick(1500);
    expect(await opened).toBe(true);
    expect(h.failures).toEqual([]);
  });

  it('a restricted network falls back without touching Cartesia', async () => {
    const h = setup({ isRestricted: async () => true });
    expect(await h.engine.open()).toBe(false);
    h.engine.start();
    expect(h.failures).toEqual([{ kind: 'fallback', reason: 'restricted_network' }]);
    expect(h.sockets).toEqual([]);
  });

  it('a failing reachability probe counts as "not restricted"', async () => {
    const h = setup({ isRestricted: () => Promise.reject(new Error('probe')) });
    expect(await h.engine.open()).toBe(true);
  });

  it('closing while the token is in flight reports nothing', async () => {
    let release!: (r: ApiResult<EnhancedSessionResponse>) => void;
    const h = setup({ fetchToken: () => new Promise((r) => { release = r; }) });
    const opened = h.engine.open();
    h.engine.close();
    release(refused(429, 'rate_limited'));
    expect(await opened).toBe(false);
    expect(h.failures).toEqual([]);
  });

  it('an STT socket that drops is retried on the backoff, then Cartesia is declared unreachable', async () => {
    const h = await listening();
    h.stt()[0].drop();
    for (let i = 1; i <= STT_RETRIES; i++) {
      await tick(4000);
      expect(h.stt()).toHaveLength(i + 1);
      h.stt()[i].drop();
    }
    expect(h.failures).toEqual([{ kind: 'fallback', reason: 'cartesia_unreachable' }]);
    expect(h.control.at(-1)).toEqual({ type: 'stop' }); // the meter is released
    await tick(10_000);
    expect(h.stt()).toHaveLength(STT_RETRIES + 1);
  });

  it('a socket that never opens (a firewall hangs) times out into the same path', async () => {
    const h = setup();
    await h.engine.open();
    h.engine.start();
    for (let i = 0; i <= STT_RETRIES; i++) await tick(OPEN_TIMEOUT_MS + 4000);
    expect(h.failures).toEqual([{ kind: 'fallback', reason: 'cartesia_unreachable' }]);
  });

  it('a socket that stayed up long enough gives the retry budget back', async () => {
    const h = await listening();
    for (let round = 0; round < STT_RETRIES + 2; round++) {
      const ws = h.stt().at(-1) as FakeWs;
      if (round > 0) ws.open();
      await tick(STABLE_MS + 1);
      ws.drop();
      await tick(1000);
    }
    expect(h.failures).toEqual([]);
  });

  it('a socket that cannot even be constructed counts as a failure', async () => {
    let n = 0;
    const h = setup({
      createSocket: (url) => {
        if (n++ === 0) throw new Error('ctor');
        const w = new FakeWs(url);
        h.sockets.push(w);
        return w;
      },
    });
    await h.engine.open();
    h.engine.start();
    await tick(1000);
    expect(h.stt()).toHaveLength(1); // the retry got through
  });

  it('a capture graph that fails is the same as a dropped recogniser', async () => {
    const h = setup({ openGraph: async () => { throw new Error('worklet'); } });
    await h.engine.open();
    h.engine.start();
    h.stt()[0].open();
    await tick(1000);
    expect(h.stt()[0].closed).toBe(true);
    expect(h.stt()).toHaveLength(2);
  });

  it('a graph that finishes loading after the recogniser was replaced is closed, not leaked', async () => {
    let release!: (g: SttGraph | null) => void;
    const graph = { ctx: { close: vi.fn(async () => {}) }, node: { port: { onmessage: null }, disconnect: vi.fn() }, src: { disconnect: vi.fn() } } as unknown as SttGraph;
    const h = setup({ openGraph: () => new Promise((r) => { release = r; }) });
    await h.engine.open();
    h.engine.start();
    h.stt()[0].open();
    h.engine.stop();
    release(graph);
    await tick();
    expect((graph.ctx as unknown as { close: ReturnType<typeof vi.fn> }).close).toHaveBeenCalled();
  });

  it('a late "null" from the graph (cancelled) is harmless', async () => {
    const h = setup({ openGraph: async () => null });
    await h.engine.open();
    h.engine.start();
    h.stt()[0].open();
    await tick();
    expect(h.failures).toEqual([]);
  });
});

describe('token refresh', () => {
  it('refreshes before the token expires and cycles the recogniser onto the new one, sending what it holds first', async () => {
    const h = await listening();
    h.stt()[0].say({ type: 'transcript', text: 'mezza frase', is_final: false });
    h.fetchToken.mockResolvedValue(token({ token: 'TOK-2' }));
    await tick(3600_000 - REFRESH_LEAD_MS - 1000);
    expect(h.fetchToken).toHaveBeenCalledTimes(1);
    await tick(2000);
    expect(h.fetchToken).toHaveBeenCalledTimes(2);
    expect(frames(h).at(-1)).toMatchObject({ text: 'mezza frase', final: true });
    expect(h.stt()[0].closed).toBe(true);
    expect(new URL(h.stt()[1].url).searchParams.get('access_token')).toBe('TOK-2');
    // The meter stays open across the swap.
    expect(h.control.filter((m) => m.type === 'start')).toHaveLength(1);
    expect(h.control.filter((m) => m.type === 'stop')).toHaveLength(0);
  });

  it('keeps refreshing: a second refresh is scheduled from the new expiry', async () => {
    const h = await listening();
    await tick(3600_000 - REFRESH_LEAD_MS + 1000);
    expect(h.fetchToken).toHaveBeenCalledTimes(2);
    h.stt()[1].open(); // the recogniser cycled onto the new token
    await tick(3600_000 - REFRESH_LEAD_MS);
    expect(h.fetchToken).toHaveBeenCalledTimes(3);
  });

  it('a refresh that fails while the old token still works is retried, not escalated', async () => {
    const h = await listening();
    h.fetchToken.mockResolvedValue(refused(429, 'rate_limited'));
    await tick(3600_000 - REFRESH_LEAD_MS + 1000);
    expect(h.failures).toEqual([]);
    h.fetchToken.mockResolvedValue(token({ token: 'TOK-3' }));
    await tick(15_000);
    expect(new URL(h.stt().at(-1)!.url).searchParams.get('access_token')).toBe('TOK-3');
  });

  it('a refresh that fails once the token is about to die is reported', async () => {
    const h = setup({ fetchToken: vi.fn().mockResolvedValueOnce(token({}, 60)).mockResolvedValue(refused(502, 'enhanced_unavailable')) });
    await h.engine.open();
    await tick(40_000);
    expect(h.failures).toEqual([{ kind: 'fallback', reason: 'enhanced_unavailable' }]);
  });

  it('a "gone" answer on refresh is reported as gone', async () => {
    const h = await listening();
    h.fetchToken.mockResolvedValue(refused(410, 'session_ended'));
    await tick(3600_000);
    expect(h.failures).toEqual([{ kind: 'gone', code: 'session_ended' }]);
  });

  it('a short-lived token refreshes at half its life', async () => {
    const fetchToken = vi.fn(async () => token({}, 120));
    const h = setup({ fetchToken });
    await h.engine.open();
    await tick(59_000);
    expect(fetchToken).toHaveBeenCalledTimes(1);
    await tick(2000);
    expect(fetchToken).toHaveBeenCalledTimes(2);
  });

  it('the speech socket is replaced after a refresh, at the next utterance', async () => {
    const h = setup();
    await h.engine.open();
    h.engine.speak({ speakerId: 'peer-b', segmentId: 's1', text: 'Ciao', voiceId: null });
    await tick();
    h.tts()[0].open();
    await tick();
    h.tts()[0].say({ type: 'done', context_id: (h.tts()[0].json()[0].context_id as string) });
    await tick();
    h.fetchToken.mockResolvedValue(token({ token: 'TOK-2' }));
    await tick(3600_000 - REFRESH_LEAD_MS + 1000);
    expect(h.tts()[0].closed).toBe(true);
    h.engine.speak({ speakerId: 'peer-b', segmentId: 's2', text: 'Ancora', voiceId: null });
    await tick();
    expect(new URL(h.tts()[1].url).searchParams.get('access_token')).toBe('TOK-2');
  });

  it('a refresh during an utterance leaves its socket alone until it is done', async () => {
    const h = setup();
    await h.engine.open();
    await tick(3600_000 - REFRESH_LEAD_MS - 5000);
    h.engine.speak({ speakerId: 'peer-b', segmentId: 's1', text: 'Lunga frase', voiceId: null });
    await tick();
    h.tts()[0].open();
    await tick();
    h.fetchToken.mockResolvedValue(token({ token: 'TOK-2' }));
    await tick(6000); // the refresh lands while the sentence is still being synthesised
    expect(h.fetchToken).toHaveBeenCalledTimes(2);
    expect(h.tts()[0].closed).toBe(false);
  });
});

describe('listener side: speaking', () => {
  async function talker(over: Partial<EnhancedDeps> = {}) {
    const h = setup(over);
    await h.engine.open();
    return h;
  }
  /** Let the pump run up to the next request on the speech socket, opening it the first time. */
  async function pump(h: Awaited<ReturnType<typeof talker>>): Promise<Record<string, unknown>[]> {
    await tick();
    const ws = h.tts()[0];
    if (ws && ws.readyState === 0) {
      ws.open();
      await tick();
    }
    return ws ? ws.json() : [];
  }
  const done = async (h: Awaited<ReturnType<typeof talker>>, n: number) => {
    h.tts()[0].say({ type: 'done', context_id: h.tts()[0].json()[n].context_id });
    await tick();
  };

  it('speaks in the voice the server named, else the default voice, in my language', async () => {
    const h = await talker();
    h.engine.speak({ speakerId: 'peer-a', segmentId: 's1', text: 'Hello there', voiceId: 'HOST-CLONE' });
    let sent = await pump(h);
    expect(sent[0]).toMatchObject({
      model_id: 'sonic-3',
      transcript: 'Hello there',
      voice: { mode: 'id', id: 'HOST-CLONE' },
      language: 'it',
      continue: false,
      output_format: { container: 'raw', encoding: 'pcm_s16le', sample_rate: 24000 },
    });
    await done(h, 0);
    h.engine.speak({ speakerId: 'peer-a', segmentId: 's2', text: 'Second', voiceId: null });
    sent = await pump(h);
    expect(sent[1]).toMatchObject({ transcript: 'Second', voice: { mode: 'id', id: 'DEFAULT-VOICE' } });
    expect(new URL(h.tts()[0].url).searchParams.get('access_token')).toBe('TOK-1');
  });

  it('speaks one sentence at a time, in arrival order, and the next waits for `done`', async () => {
    const h = await talker();
    for (const text of ['uno', 'due', 'tre']) h.engine.speak({ speakerId: 'p', segmentId: text, text, voiceId: 'V' });
    let sent = await pump(h);
    expect(sent.map((m) => m.transcript)).toEqual(['uno']);
    await done(h, 0);
    sent = h.tts()[0].json();
    expect(sent.map((m) => m.transcript)).toEqual(['uno', 'due']);
    await done(h, 1);
    await done(h, 2);
    expect(h.tts()[0].json().map((m) => m.transcript)).toEqual(['uno', 'due', 'tre']);
    expect(new Set(h.tts()[0].json().map((m) => m.context_id)).size).toBe(3);
  });

  it('hands every chunk to playback keyed by speaker with a rising sequence across sentences', async () => {
    const h = await talker();
    h.engine.speak({ speakerId: 'peer-b', segmentId: 's1', text: 'uno', voiceId: 'V' });
    h.engine.speak({ speakerId: 'peer-c', segmentId: 's2', text: 'due', voiceId: 'V' });
    await pump(h);
    const ctx0 = h.tts()[0].json()[0].context_id;
    h.tts()[0].say({ type: 'chunk', context_id: ctx0, data: 'AAA' });
    h.tts()[0].say({ type: 'chunk', context_id: ctx0, data: 'BBB' });
    h.tts()[0].say({ type: 'chunk', context_id: 'someone-else', data: 'XXX' });
    await done(h, 0);
    h.tts()[0].say({ type: 'chunk', context_id: h.tts()[0].json()[1].context_id, data: 'CCC' });
    expect(h.audio).toEqual([
      ['peer-b', 0, 'AAA'],
      ['peer-b', 1, 'BBB'],
      ['peer-c', 2, 'CCC'],
    ]);
  });

  it('with no voice at all the caption is all there is', async () => {
    const h = await talker({ fetchToken: async () => token({ default_voice_id: null }) });
    h.engine.speak({ speakerId: 'p', segmentId: 's', text: 'Hello', voiceId: null });
    await tick();
    expect(h.tts()).toEqual([]);
    expect(h.failures).toEqual([]);
  });

  it('keeps the queue bounded: when the phone falls behind, the oldest go', async () => {
    const h = await talker();
    h.engine.speak({ speakerId: 'p', segmentId: 'first', text: 'first', voiceId: 'V' });
    await pump(h);
    for (let i = 0; i < MAX_SPEECH_QUEUE + 8; i++) h.engine.speak({ speakerId: 'p', segmentId: `q${i}`, text: `q${i}`, voiceId: 'V' });
    // Drain everything.
    for (let i = 0; i < MAX_SPEECH_QUEUE + 1; i++) await done(h, i);
    const spoken = h.tts()[0].json().map((m) => m.transcript);
    expect(spoken[0]).toBe('first');
    expect(spoken).toHaveLength(MAX_SPEECH_QUEUE + 1);
    expect(spoken.at(-1)).toBe(`q${MAX_SPEECH_QUEUE + 7}`);
    expect(spoken).not.toContain('q0');
  });

  it('ignores a blank sentence', async () => {
    const h = await talker();
    h.engine.speak({ speakerId: 'p', segmentId: 's', text: '   ', voiceId: 'V' });
    await tick();
    expect(h.tts()).toEqual([]);
  });

  it('cancelSpeech (barge-in) drops the queue and the tail of the sentence in flight', async () => {
    const h = await talker();
    h.engine.speak({ speakerId: 'p', segmentId: 'a', text: 'uno', voiceId: 'V' });
    h.engine.speak({ speakerId: 'p', segmentId: 'b', text: 'due', voiceId: 'V' });
    await pump(h);
    const ctx0 = h.tts()[0].json()[0].context_id;
    h.engine.cancelSpeech();
    await tick();
    h.tts()[0].say({ type: 'chunk', context_id: ctx0, data: 'LATE' });
    expect(h.audio).toEqual([]);
    expect(h.tts()[0].json().map((m) => m.transcript)).toEqual(['uno']);
  });

  it('a sentence that never reports `done` times out and the next one is spoken', async () => {
    const h = await talker();
    h.engine.speak({ speakerId: 'p', segmentId: 'a', text: 'uno', voiceId: 'V' });
    h.engine.speak({ speakerId: 'p', segmentId: 'b', text: 'due', voiceId: 'V' });
    await pump(h);
    await tick(UTTERANCE_TIMEOUT_MS + 1);
    expect(h.tts()[0].json().map((m) => m.transcript)).toEqual(['uno', 'due']);
  });

  it('a few refused sentences in a row declare Cartesia unusable; one success resets the count', async () => {
    const h = await talker();
    const fail = async (n: number) => {
      h.tts()[0].say({ type: 'error', context_id: h.tts()[0].json()[n].context_id });
      await tick();
    };
    h.engine.speak({ speakerId: 'p', segmentId: '0', text: 'a', voiceId: 'V' });
    await pump(h);
    await fail(0);
    h.engine.speak({ speakerId: 'p', segmentId: '1', text: 'b', voiceId: 'V' });
    await tick();
    await fail(1);
    h.engine.speak({ speakerId: 'p', segmentId: '2', text: 'c', voiceId: 'V' });
    await tick();
    await done(h, 2); // a success: the count starts over
    for (let i = 3; i < 3 + TTS_FAILURE_LIMIT; i++) {
      expect(h.failures).toEqual([]);
      h.engine.speak({ speakerId: 'p', segmentId: String(i), text: String(i), voiceId: 'V' });
      await tick();
      await fail(i);
    }
    expect(h.failures).toEqual([{ kind: 'fallback', reason: 'cartesia_unreachable' }]);
  });

  it('a speech socket that never opens counts against Cartesia too', async () => {
    const h = await talker();
    for (let i = 0; i < TTS_FAILURE_LIMIT; i++) {
      h.engine.speak({ speakerId: 'p', segmentId: String(i), text: `t${i}`, voiceId: 'V' });
      await tick(OPEN_TIMEOUT_MS + 1);
    }
    expect(h.failures).toEqual([{ kind: 'fallback', reason: 'cartesia_unreachable' }]);
  });

  it('a socket that dies mid-sentence ends that sentence; the next one reopens it', async () => {
    const h = await talker();
    h.engine.speak({ speakerId: 'p', segmentId: 'a', text: 'uno', voiceId: 'V' });
    h.engine.speak({ speakerId: 'p', segmentId: 'b', text: 'due', voiceId: 'V' });
    await pump(h);
    h.tts()[0].drop();
    await tick();
    h.tts()[1].open();
    await tick();
    expect(h.tts()[1].json().map((m) => m.transcript)).toEqual(['due']);
  });

  it('a speech socket constructor that throws is a failed utterance, not a crash', async () => {
    const h = await talker({
      createSocket: () => {
        throw new Error('no socket');
      },
    });
    h.engine.speak({ speakerId: 'p', segmentId: 'a', text: 'uno', voiceId: 'V' });
    await tick();
    expect(h.failures).toEqual([]);
  });

  it('speaking works while muted: it needs the token, not the recogniser', async () => {
    const h = await talker();
    h.engine.setMuted(true);
    h.engine.speak({ speakerId: 'p', segmentId: 'a', text: 'uno', voiceId: 'V' });
    const sent = await pump(h);
    expect(sent).toHaveLength(1);
  });

  it('speak() before open() fetches the token itself', async () => {
    const h = setup();
    h.engine.speak({ speakerId: 'p', segmentId: 'a', text: 'uno', voiceId: 'V' });
    await tick();
    expect(h.fetchToken).toHaveBeenCalledTimes(1);
    expect(h.tts()).toHaveLength(1);
  });
});

describe('close', () => {
  it('releases the recogniser, the speech socket, the graph and every timer; and nothing speaks afterwards', async () => {
    const h = await listening();
    h.stt()[0].say({ type: 'transcript', text: 'in corso', is_final: false });
    h.engine.speak({ speakerId: 'p', segmentId: 'a', text: 'uno', voiceId: 'V' });
    await tick();
    h.tts()[0].open();
    await tick();
    h.engine.close();
    const sentBefore = h.control.length;
    expect(h.stt()[0].closed).toBe(true);
    expect(h.tts()[0].closed).toBe(true);
    expect(h.graphs[0].stopped()).toBe(true);
    expect(h.control.at(-1)).toEqual({ type: 'stop' });
    h.engine.speak({ speakerId: 'p', segmentId: 'b', text: 'due', voiceId: 'V' });
    h.engine.start();
    await tick(3600_000 * 2);
    expect(h.control.length).toBe(sentBefore);
    expect(h.tts()).toHaveLength(1);
    expect(h.stt()).toHaveLength(1);
    expect(h.fetchToken).toHaveBeenCalledTimes(1); // no refresh after close
    expect(h.failures).toEqual([]);
    h.engine.close(); // idempotent
  });

  it('a failure closes the engine for good and is reported once', async () => {
    const h = await listening();
    h.stt()[0].drop();
    for (let i = 1; i <= STT_RETRIES; i++) {
      await tick(4000);
      h.stt()[i].drop();
    }
    expect(h.failures).toHaveLength(1);
    h.engine.start();
    await tick(10_000);
    expect(h.stt()).toHaveLength(STT_RETRIES + 1);
  });
});
