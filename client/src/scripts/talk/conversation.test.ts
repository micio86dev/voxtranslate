// @vitest-environment jsdom
import { describe, it, expect, beforeEach, vi } from 'vitest';
import {
  TalkConversation,
  backoffDelay,
  buildTalkUrl,
  browserSupported,
  classifyMediaError,
  MIC_CONSTRAINTS,
  RECONNECT_BASE_MS,
  RECONNECT_MAX_ATTEMPTS,
  RECONNECT_MAX_MS,
  type Exchange,
  type LiveExchange,
  type TalkSocket,
} from './conversation';
import type { SessionContext } from './session-machine';

class FakeSocket implements TalkSocket {
  readyState = 1;
  sent: unknown[] = [];
  closed: number[] = [];
  onopen: (() => void) | null = null;
  onclose: ((ev: { code: number }) => void) | null = null;
  onerror: (() => void) | null = null;
  onmessage: ((ev: { data: unknown }) => void) | null = null;
  constructor(readonly url: string) {
    sockets.push(this);
  }
  send(d: unknown): void {
    this.sent.push(d);
  }
  close(code = 1000): void {
    this.closed.push(code);
  }
  open(): void {
    this.onopen?.();
  }
  deliver(msg: unknown): void {
    this.onmessage?.({ data: JSON.stringify(msg) });
  }
  drop(): void {
    this.onclose?.({ code: 1006 });
  }
}

class FakeTrack {
  stop = vi.fn();
}

function fakeStream(): MediaStream {
  const tracks = [new FakeTrack()];
  return { getTracks: () => tracks, getAudioTracks: () => tracks } as unknown as MediaStream;
}

class FakePlayback {
  enqueued: Array<[string, number, string]> = [];
  resets = 0;
  stops = 0;
  unlocks = 0;
  listener: ((playing: boolean) => void) | null = null;
  setPlayingListener(fn: ((playing: boolean) => void) | null): void {
    this.listener = fn;
  }
  enqueue(s: string, seq: number, b64: string): void {
    this.enqueued.push([s, seq, b64]);
  }
  reset(): void {
    this.resets += 1;
  }
  stop(): void {
    this.stops += 1;
  }
  unlock(): void {
    this.unlocks += 1;
  }
  isPlaying(): boolean {
    return false;
  }
}

let sockets: FakeSocket[] = [];
let timers: Array<{ fn: () => void; ms: number }> = [];
let clock = 0;

function harness(over: Record<string, unknown> = {}) {
  const states: SessionContext[] = [];
  const lives: LiveExchange[] = [];
  const exchanges: Exchange[] = [];
  const notices: string[] = [];
  const balances: number[] = [];
  const lowBalances: number[] = [];
  const ttts: number[] = [];
  const playback = new FakePlayback();
  const wakeLock = { request: vi.fn(async () => {}), release: vi.fn(async () => {}) };
  const convo = new TalkConversation({
    userLang: 'it',
    otherLang: 'es',
    engineId: 'standard',
    needsPcm: true,
    onState: (c) => states.push(c),
    onLive: (l) => lives.push(l),
    onExchange: (e) => exchanges.push(e),
    onNotice: (c) => notices.push(c),
    onBalance: (b) => balances.push(b),
    onLowBalance: (b) => lowBalances.push(b),
    onTimeToTranslatedSpeech: (ms) => ttts.push(ms),
    createSocket: (url) => new FakeSocket(url),
    getMedia: async () => fakeStream(),
    now: () => clock,
    random: () => 0.5,
    setTimeoutImpl: ((fn: () => void, ms: number) => {
      timers.push({ fn, ms });
      return timers.length as unknown as ReturnType<typeof setTimeout>;
    }) as unknown as typeof setTimeout,
    clearTimeoutImpl: (() => {}) as unknown as typeof clearTimeout,
    playback: playback as never,
    wakeLock,
    ...over,
  });
  return {
    convo, states, lives, exchanges, notices, ttts, playback, wakeLock,
    balances, lowBalances,
  };
}

/** Start and open the socket, leaving the conversation live. */
async function live(over: Record<string, unknown> = {}) {
  const h = harness(over);
  await h.convo.start();
  sockets[0].open();
  return h;
}

beforeEach(() => {
  sockets = [];
  timers = [];
  clock = 0;
});

describe('url and pure helpers', () => {
  it('targets /ws/talk with both languages and the tier', () => {
    const url = buildTalkUrl('it', 'es', 'standard');
    expect(url).toContain('/ws/talk?');
    expect(url).toContain('lang=it');
    expect(url).toContain('other=es');
    expect(url).toContain('engine=standard');
    // The room route must not be hit by accident — it speaks a different protocol.
    expect(url).not.toContain('/ws?');
  });

  it('asks for the three echo-cancellation flags', () => {
    // The first and cheapest layer of feedback-loop defence (brief §21). Losing these
    // turns a phone on a table into an infinite translation loop.
    const audio = MIC_CONSTRAINTS.audio as MediaTrackConstraints;
    expect(audio.echoCancellation).toBe(true);
    expect(audio.noiseSuppression).toBe(true);
    expect(audio.autoGainControl).toBe(true);
    expect(audio.channelCount).toBe(1);
  });

  it('classifies each permission failure distinctly', () => {
    expect(classifyMediaError({ name: 'NotAllowedError' })).toBe('mic_denied');
    expect(classifyMediaError({ name: 'SecurityError' })).toBe('mic_denied');
    expect(classifyMediaError({ name: 'NotFoundError' })).toBe('mic_missing');
    expect(classifyMediaError({ name: 'OverconstrainedError' })).toBe('mic_missing');
    expect(classifyMediaError({ name: 'NotReadableError' })).toBe('mic_blocked');
    expect(classifyMediaError({ name: 'AbortError' })).toBe('mic_blocked');
    expect(classifyMediaError(new Error('???'))).toBe('unknown');
    expect(classifyMediaError(null)).toBe('unknown');
  });

  it('grows the backoff, caps it, and jitters it', () => {
    const half = () => 0; // full jitter floor: exactly half the ceiling
    expect(backoffDelay(0, half)).toBe(RECONNECT_BASE_MS / 2);
    expect(backoffDelay(1, half)).toBe(RECONNECT_BASE_MS);
    expect(backoffDelay(99, half)).toBe(RECONNECT_MAX_MS / 2);
    // Jitter spreads the retry: without it every device in a café reconnects on the
    // same millisecond when a server restarts.
    expect(backoffDelay(3, () => 0)).not.toBe(backoffDelay(3, () => 1));
    expect(backoffDelay(3, () => 1)).toBeLessThanOrEqual(RECONNECT_BASE_MS * 8);
  });

  it('detects a browser that cannot run the conversation', () => {
    const full = {
      navigator: { mediaDevices: { getUserMedia: () => {} } },
      AudioContext: class {},
      AudioWorkletNode: class {},
      WebSocket: class {},
    } as unknown as Window & typeof globalThis;
    expect(browserSupported(full)).toBe(true);
    // No AudioWorklet means no capture and no playback — say so up front rather than
    // failing silently after the permission prompt.
    expect(browserSupported({ ...full, AudioWorkletNode: undefined } as never)).toBe(false);
    expect(browserSupported({ ...full, navigator: {} } as never)).toBe(false);
  });
});

describe('lifecycle', () => {
  it('walks start → live and unlocks playback inside the gesture', async () => {
    const h = await live();
    expect(h.convo.state().phase).toBe('live');
    // iOS will not start an AudioContext outside a user gesture; unlocking later means
    // the first translation is silent.
    expect(h.playback.unlocks).toBe(1);
    expect(h.wakeLock.request).toHaveBeenCalled();
  });

  it('reports a denied microphone without opening a socket', async () => {
    const h = harness({
      getMedia: async () => {
        throw { name: 'NotAllowedError' };
      },
    });
    await h.convo.start();
    expect(h.convo.state().phase).toBe('error');
    expect(h.convo.state().failure).toBe('mic_denied');
    expect(sockets.length).toBe(0);
  });

  it('releases a stream granted after the user gave up', async () => {
    // Tap Start, tap End while the permission prompt is up, then grant. The track must
    // not be left live — the browser's recording indicator would stay on.
    let release!: (s: MediaStream) => void;
    const stream = fakeStream();
    const h = harness({ getMedia: () => new Promise<MediaStream>((r) => (release = r)) });
    const pending = h.convo.start();
    h.convo.end();
    release(stream);
    await pending;
    expect(stream.getTracks()[0].stop).toHaveBeenCalled();
    expect(sockets.length).toBe(0);
  });

  it('releases everything on end', async () => {
    const h = await live();
    const sock = sockets[0];
    h.convo.end();

    expect(h.convo.state().phase).toBe('ended');
    expect(sock.closed).toEqual([1000]);
    expect(h.playback.stops).toBe(1);
    expect(h.playback.listener).toBeNull();
    expect(h.wakeLock.release).toHaveBeenCalled();
    // No leaked microphone track.
    expect(sock.onmessage).toBeNull();
  });

  it('does not reconnect after the user ended it', async () => {
    const h = await live();
    const sock = sockets[0];
    h.convo.end();
    sock.drop(); // a late close from the socket we already detached
    expect(timers.length).toBe(0);
    expect(sockets.length).toBe(1);
  });

  it('pauses and resumes without dropping the socket', async () => {
    const h = await live();
    h.convo.pause();
    expect(h.convo.state().phase).toBe('paused');
    // Queued translations are dropped: resuming into a sentence from before the pause
    // would be worse than silence.
    expect(h.playback.resets).toBeGreaterThan(0);
    expect(h.wakeLock.release).toHaveBeenCalled();

    h.convo.resume();
    expect(h.convo.state().phase).toBe('live');
    expect(sockets.length).toBe(1);
  });
});

describe('reconnect', () => {
  it('retries with backoff and resumes the session on success', async () => {
    const h = await live();
    sockets[0].drop();
    expect(h.convo.state().phase).toBe('reconnecting');
    expect(timers.length).toBe(1);

    timers[0].fn();
    sockets[1].open();
    expect(h.convo.state().phase).toBe('live');
  });

  it('gives up after a bounded number of attempts', async () => {
    // An unbounded retry loop hammers a server that is down and never tells the user
    // anything is wrong.
    const h = await live();
    for (let i = 0; i < RECONNECT_MAX_ATTEMPTS; i++) {
      sockets[sockets.length - 1].drop();
      timers[timers.length - 1].fn();
    }
    sockets[sockets.length - 1].drop();
    expect(h.convo.state().phase).toBe('error');
    expect(h.convo.state().failure).toBe('connection');
  });

  it('treats a socket constructor that throws as a drop', async () => {
    let first = true;
    const h = harness({
      createSocket: (url: string) => {
        if (first) {
          first = false;
          throw new Error('offline');
        }
        return new FakeSocket(url);
      },
    });
    await h.convo.start();
    expect(timers.length).toBe(1); // retried rather than died
  });
});

describe('frames', () => {
  it('renders a full exchange from direction → interim → final', async () => {
    const h = await live();
    const sock = sockets[0];

    sock.deliver({ type: 'talk_direction', spoken: 'it', target: 'es' });
    sock.deliver({
      type: 'subtitle_interim',
      text: 'Quiero ir a la',
      original: 'Vorrei andare alla',
    });
    sock.deliver({
      type: 'subtitle_final',
      original: 'Vorrei andare alla stazione',
      lang: 'auto',
      translations: { es: 'Quiero ir a la estación' },
    });

    expect(h.exchanges).toHaveLength(1);
    expect(h.exchanges[0]).toMatchObject({
      spokenLang: 'it',
      targetLang: 'es',
      originalText: 'Vorrei andare alla stazione',
      translatedText: 'Quiero ir a la estación',
    });
    // The finished translation STAYS in the big area, marked settled. Clearing it here
    // is what made it flash past before anyone could read it — the opposite of "the
    // newest translation remains prominent" (brief §12).
    const last = h.lives[h.lives.length - 1];
    expect(last.translatedText).toBe('Quiero ir a la estación');
    expect(last.settled).toBe(true);

    // ...and the NEXT sentence replaces it rather than appending to it.
    sock.deliver({ type: 'subtitle_interim', text: 'Está a cinco', original: 'Está a cinco' });
    const next = h.lives[h.lives.length - 1];
    expect(next.translatedText).toBe('Está a cinco');
    expect(next.settled).toBe(false);
  });

  it('falls back to the only translation when the direction arrived late', async () => {
    // `translations` always carries exactly one entry (standard.rs::flush_final), so an
    // empty card here would be a rendering bug, not missing data.
    const h = await live();
    sockets[0].deliver({
      type: 'subtitle_final',
      original: 'Está a cinco minutos',
      translations: { it: 'È a cinque minuti' },
    });
    expect(h.exchanges[0].translatedText).toBe('È a cinque minuti');
  });

  it('does not record an empty exchange', async () => {
    const h = await live();
    sockets[0].deliver({ type: 'subtitle_final', original: '   ', translations: { es: '' } });
    expect(h.exchanges).toHaveLength(0);
  });

  it('queues translated audio for playback', async () => {
    const h = await live();
    sockets[0].deliver({ type: 'translated_audio', speaker_id: 'src', seq: 3, pcm16_b64: 'AAA' });
    expect(h.playback.enqueued).toEqual([['src', 3, 'AAA']]);
    // A frame with no payload is not queued as silence.
    sockets[0].deliver({ type: 'translated_audio', speaker_id: 'src', seq: 4, pcm16_b64: '' });
    expect(h.playback.enqueued).toHaveLength(1);
  });

  it('measures speech seen → translated speech audible, once per utterance', async () => {
    const h = await live();
    sockets[0].deliver({ type: 'subtitle_interim', text: 'Quiero', original: 'Vorrei' });
    clock = 800;
    h.playback.listener?.(true);
    h.playback.listener?.(false);
    h.playback.listener?.(true);
    expect(h.ttts).toEqual([800]);
  });

  it('keeps the session alive on a recoverable error', async () => {
    const h = await live();
    sockets[0].deliver({ type: 'error', code: 'provider_unavailable', message: 'busy' });
    expect(h.notices).toEqual(['provider_unavailable']);
    expect(h.convo.state().phase).toBe('live');
  });

  it('reports the balance on every meter tick', async () => {
    // The server was already sending these; the client threw them away, so nothing on
    // screen moved while credits drained.
    const h = await live();
    sockets[0].deliver({ type: 'balance_update', balance: 12.5099 });
    sockets[0].deliver({ type: 'balance_update', balance: 12.5091 });
    expect(h.balances).toEqual([12.5099, 12.5091]);
  });

  it('flags a low balance separately from a routine tick', async () => {
    const h = await live();
    sockets[0].deliver({ type: 'low_balance', balance: 0.42 });
    expect(h.lowBalances).toEqual([0.42]);
    expect(h.convo.state().phase).toBe('live');
  });

  it('stops the conversation when credits run out, and says why', async () => {
    // Not just a banner: the server has already stopped billing and translating, so the
    // pipeline comes down and the state carries the reason the UI needs to offer a top-up.
    const h = await live();
    const sock = sockets[0];
    sock.deliver({ type: 'balance_exhausted' });
    expect(h.convo.state().failure).toBe('credits');
    expect(h.convo.state().phase).toBe('error');
    // Everything released — no microphone left live, no socket left open.
    expect(sock.closed).toEqual([1000]);
    expect(h.playback.stops).toBe(1);
    expect(h.wakeLock.release).toHaveBeenCalled();
  });

  it('ignores malformed and unknown frames', async () => {
    const h = await live();
    sockets[0].onmessage?.({ data: 'not json' });
    sockets[0].onmessage?.({ data: new ArrayBuffer(4) });
    sockets[0].deliver({ type: 'peer_joined' });
    expect(h.convo.state().phase).toBe('live');
    expect(h.exchanges).toHaveLength(0);
  });
});
