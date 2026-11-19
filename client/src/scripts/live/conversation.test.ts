// @vitest-environment jsdom
import { beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('../auth', () => ({ WS_BASE: 'wss://api.test', HTTP_BASE: 'https://api.test' }));
const setNetworkDegraded = vi.fn();
vi.mock('../net-status', () => ({ setNetworkDegraded: (...a: unknown[]) => setNetworkDegraded(...a) }));

import {
  HISTORY_LIMIT,
  LiveConversation,
  RECONNECT_MAX_ATTEMPTS,
  buildLiveUrl,
  outputSelectionSupported,
  type Captions,
  type LiveSocket,
} from './conversation';
import { STORAGE_KEY, HEADPHONES_KEY } from './session-store';
import type { LiveContext } from './session-machine';
import type { ApiResult, SessionInfo } from './api';

class FakeSocket implements LiveSocket {
  readyState = 1;
  sent: string[] = [];
  closed: number[] = [];
  onopen: (() => void) | null = null;
  onclose: ((ev: { code: number }) => void) | null = null;
  onerror: (() => void) | null = null;
  onmessage: ((ev: { data: unknown }) => void) | null = null;
  constructor(readonly url: string) {
    sockets.push(this);
  }
  send(d: string | ArrayBufferLike | Blob): void {
    this.sent.push(typeof d === 'string' ? d : '<binary>');
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
  controls(): unknown[] {
    return this.sent.filter((s) => s.startsWith('{')).map((s) => JSON.parse(s));
  }
}

class FakeCapture {
  started = 0;
  stopped = 0;
  muted: boolean[] = [];
  gated: boolean[] = [];
  constructor() {
    captures.push(this);
  }
  start(): void {
    this.started += 1;
  }
  stop(): void {
    this.stopped += 1;
  }
  setMuted(m: boolean): void {
    this.muted.push(m);
  }
  setGated(g: boolean): void {
    this.gated.push(g);
  }
}

class FakePlayback {
  enqueued: Array<[string, number, string]> = [];
  resets = 0;
  stops = 0;
  unlocks = 0;
  listener: ((playing: boolean) => void) | null = null;
  setPlayingListener(fn: ((p: boolean) => void) | null): void {
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
}

class FakeTrack {
  stop = vi.fn();
}
function fakeStream() {
  const tracks = [new FakeTrack()];
  const stream = { getTracks: () => tracks, getAudioTracks: () => tracks } as unknown as MediaStream;
  return { stream, tracks };
}

let sockets: FakeSocket[] = [];
let captures: FakeCapture[] = [];
let timers: Array<{ fn: () => void; ms: number; cleared: boolean }> = [];

const flush = () => new Promise((r) => setTimeout(r, 0));

const PARTNER_PEER = { peer_id: 'peer-b', user_name: 'Bea', lang: 'es' };
// The server's `room_joined.peers[]` is a `PeerInfo`: the id is `id` there (and `peer_id` in `peer_joined`).
const PARTNER_ROOM_PEER = { id: 'peer-b', user_name: 'Bea', lang: 'es' };

type Probe = ApiResult<SessionInfo>;
const activeProbe = (): Probe => ({
  ok: true,
  data: { session: { id: 's1', status: 'active', host_lang: 'it', created_at: '', expires_at: '', participants: [] }, you: { peer_id: 'peer-a', role: 'host' } },
});

function harness(over: Record<string, unknown> = {}) {
  const states: LiveContext[] = [];
  const captions: Captions[] = [];
  const notices: string[] = [];
  const balances: number[] = [];
  const lows: number[] = [];
  const same: string[] = [];
  const mutes: boolean[] = [];
  const playback = new FakePlayback();
  const wakeLock = { request: vi.fn(async () => {}), release: vi.fn(async () => {}) };
  const media = fakeStream();
  const probe = vi.fn(async (): Promise<Probe> => activeProbe());
  const endRest = vi.fn(async () => ({ ok: true as const, data: {} }));
  const convo = new LiveConversation({
    sessionId: 's1',
    token: 'LIVE-TOKEN',
    wsPath: '/ws/live',
    role: 'host',
    myLang: 'it',
    myPeerId: 'peer-a',
    onState: (c) => states.push(c),
    onCaptions: (c) => captions.push(c),
    onNotice: (c) => notices.push(c),
    onBalance: (b) => balances.push(b),
    onLowBalance: (b) => lows.push(b),
    onSameLanguage: (a, b) => same.push(`${a}/${b}`),
    onMuteChange: (m) => mutes.push(m),
    createSocket: (url: string) => new FakeSocket(url),
    createCapture: () => new FakeCapture(),
    getMedia: async () => media.stream,
    now: () => 0,
    random: () => 0.5,
    setTimeoutImpl: ((fn: () => void, ms: number) => {
      const t = { fn, ms, cleared: false };
      timers.push(t);
      return t as unknown as ReturnType<typeof setTimeout>;
    }) as unknown as typeof setTimeout,
    clearTimeoutImpl: ((t: { cleared: boolean }) => {
      if (t) t.cleared = true;
    }) as unknown as typeof clearTimeout,
    playback: playback as never,
    wakeLock,
    getSessionImpl: probe,
    endSessionImpl: endRest,
    ...over,
  });
  const last = () => states[states.length - 1];
  const lastCaptions = () => captions[captions.length - 1];
  return { convo, states, last, captions, lastCaptions, notices, balances, lows, same, mutes, playback, wakeLock, media, probe, endRest };
}

async function connect(over: Record<string, unknown> = {}) {
  const h = harness(over);
  await h.convo.start();
  const sock = sockets[sockets.length - 1];
  sock.open();
  return { ...h, sock };
}

async function withPartner(over: Record<string, unknown> = {}) {
  const h = await connect(over);
  h.sock.deliver({ type: 'room_joined', peer_id: 'peer-a', peers: [PARTNER_ROOM_PEER] });
  return h;
}

/** Fire the most recent pending (uncleared) timer. */
function fireTimer(): number {
  const t = [...timers].reverse().find((x) => !x.cleared);
  if (!t) throw new Error('no timer pending');
  t.cleared = true;
  t.fn();
  return t.ms;
}

beforeEach(() => {
  sockets = [];
  captures = [];
  timers = [];
  setNetworkDegraded.mockReset();
  sessionStorage.clear();
  localStorage.clear();
});

describe('buildLiveUrl', () => {
  it('puts the live token (only) in the query and no lang/engine parameters', () => {
    expect(buildLiveUrl('/ws/live', 'T 1')).toBe('wss://api.test/ws/live?token=T%201');
  });
});

describe('start', () => {
  it('walks idle -> requesting_mic -> connecting -> waiting_peer and unlocks playback in the gesture', async () => {
    const h = harness();
    expect(h.convo.state().phase).toBe('idle');
    const p = h.convo.start();
    expect(h.states[0].phase).toBe('requesting_mic');
    expect(h.playback.unlocks).toBe(1);
    await p;
    expect(h.last().phase).toBe('connecting');
    expect(sockets).toHaveLength(1);
    expect(sockets[0].url).toBe('wss://api.test/ws/live?token=LIVE-TOKEN');
    sockets[0].open();
    expect(h.last().phase).toBe('waiting_peer');
  });

  it('sends {"type":"start"} as soon as the socket opens, even with nobody else there', async () => {
    const { sock } = await connect();
    expect(sock.controls()).toContainEqual({ type: 'start' });
    expect(captures).toHaveLength(1);
    expect(captures[0].started).toBe(1);
  });

  it.each([
    ['NotAllowedError', 'mic_denied'],
    ['NotFoundError', 'mic_missing'],
    ['NotReadableError', 'mic_blocked'],
    ['Weird', 'unknown'],
  ])('maps a %s microphone rejection to %s and opens no socket', async (name, kind) => {
    const h = harness({
      getMedia: async () => {
        throw Object.assign(new Error('x'), { name });
      },
    });
    await h.convo.start();
    expect(h.last().phase).toBe('failed');
    expect(h.last().failure).toBe(kind);
    expect(sockets).toHaveLength(0);
  });

  it('keeps the stored session after a microphone failure so a retry can rejoin', async () => {
    const h = harness({
      getMedia: async () => {
        throw Object.assign(new Error('x'), { name: 'NotAllowedError' });
      },
    });
    await h.convo.start();
    expect(sessionStorage.getItem(STORAGE_KEY)).not.toBeNull();
  });

  it('is idempotent: a second start while running opens nothing new', async () => {
    const h = await connect();
    await h.convo.start();
    expect(sockets).toHaveLength(1);
  });

  it('requests the screen wake lock once the socket is up and releases it at the end', async () => {
    const h = await connect();
    expect(h.wakeLock.request).toHaveBeenCalledTimes(1);
    h.convo.release();
    expect(h.wakeLock.release).toHaveBeenCalled();
  });

  it('persists the session in sessionStorage for a reload to rejoin', async () => {
    await connect({ inviteUrl: 'https://x/lc/C' });
    expect(JSON.parse(sessionStorage.getItem(STORAGE_KEY) ?? '{}')).toEqual({
      sessionId: 's1',
      token: 'LIVE-TOKEN',
      role: 'host',
      myLang: 'it',
      myPeerId: 'peer-a',
      wsPath: '/ws/live',
      inviteUrl: 'https://x/lc/C',
    });
  });

  it('persists a staff seat as staff, so a reload rejoins it as the same seat', async () => {
    await connect({ role: 'staff' });
    expect(JSON.parse(sessionStorage.getItem(STORAGE_KEY) ?? '{}').role).toBe('staff');
  });
});

describe('partner presence', () => {
  it('room_joined seeds the partner and ignores my own entry', async () => {
    const h = await connect();
    h.sock.deliver({
      type: 'room_joined',
      peer_id: 'peer-a',
      peers: [{ id: 'peer-a', user_name: 'Me', lang: 'it' }, PARTNER_ROOM_PEER],
    });
    expect(h.last().phase).toBe('live');
    expect(h.last().partner).toEqual({ peerId: 'peer-b', name: 'Bea', lang: 'es', muted: false });
    expect(h.convo.direction()).toEqual({ myLang: 'it', partnerLang: 'es' });
  });

  it('peer_joined then peer_left toggles live and waiting_peer', async () => {
    const h = await connect();
    h.sock.deliver({ type: 'room_joined', peer_id: 'peer-a', peers: [] });
    expect(h.last().phase).toBe('waiting_peer');
    h.sock.deliver({ type: 'peer_joined', ...PARTNER_PEER });
    expect(h.last().phase).toBe('live');
    h.sock.deliver({ type: 'peer_left', peer_id: 'peer-b' });
    expect(h.last().phase).toBe('waiting_peer');
    expect(h.convo.direction().partnerLang).toBeNull();
  });

  it('re-sends start when the partner arrives (upstream sessions open lazily per language)', async () => {
    const h = await connect();
    h.sock.deliver({ type: 'peer_joined', ...PARTNER_PEER });
    expect(h.sock.controls().filter((c) => (c as { type: string }).type === 'start').length).toBeGreaterThanOrEqual(2);
  });

  it('tracks the partner mute state from peer_muted (audio only)', async () => {
    const h = await withPartner();
    h.sock.deliver({ type: 'peer_muted', peer_id: 'peer-b', kind: 'audio', muted: true });
    expect(h.last().partner?.muted).toBe(true);
    h.sock.deliver({ type: 'peer_muted', peer_id: 'peer-b', kind: 'video', muted: false });
    expect(h.last().partner?.muted).toBe(true);
    h.sock.deliver({ type: 'peer_muted', peer_id: 'peer-b', kind: 'audio', muted: false });
    expect(h.last().partner?.muted).toBe(false);
  });

  it('warns once when both people speak the same language', async () => {
    const h = await connect();
    h.sock.deliver({ type: 'peer_joined', peer_id: 'peer-b', user_name: 'Bea', lang: 'it' });
    expect(h.same).toEqual(['it/it']);
    const h2 = await withPartner();
    expect(h2.same).toEqual([]);
  });
});

describe('subtitles', () => {
  it("shows the partner's words in MY language, big, with their original", async () => {
    const h = await withPartner();
    h.sock.deliver({
      type: 'subtitle_interim',
      speaker_id: 'peer-b',
      speaker_name: 'Bea',
      text: 'Ciao come',
      lang: 'it',
      original: 'Hola como',
    });
    expect(h.lastCaptions().partner).toMatchObject({ text: 'Ciao come', original: 'Hola como', settled: false });
    h.sock.deliver({
      type: 'subtitle_final',
      speaker_id: 'peer-b',
      speaker_name: 'Bea',
      original: 'Hola, como estas?',
      lang: 'es',
      translations: { it: 'Ciao, come stai?', en: 'Hi' },
    });
    expect(h.lastCaptions().partner).toMatchObject({
      text: 'Ciao, come stai?',
      original: 'Hola, como estas?',
      lang: 'es',
      settled: true,
    });
    expect(h.lastCaptions().mine).toBeNull();
  });

  it('shows MY utterances small as what I said, never as a translation', async () => {
    const h = await withPartner();
    h.sock.deliver({ type: 'subtitle_interim', speaker_id: 'peer-a', speaker_name: 'Me', text: 'Hello', lang: 'es', original: 'Buongiorno a' });
    expect(h.lastCaptions().mine).toMatchObject({ text: 'Buongiorno a', settled: false });
    expect(h.lastCaptions().partner).toBeNull();
    h.sock.deliver({
      type: 'subtitle_final',
      speaker_id: 'peer-a',
      speaker_name: 'Me',
      original: 'Buongiorno a tutti',
      lang: 'it',
      translations: { es: 'Buenos dias a todos' },
    });
    expect(h.lastCaptions().mine).toMatchObject({ text: 'Buongiorno a tutti', settled: true });
  });

  it('falls back to the original when there is no translation into my language (same language)', async () => {
    const h = await withPartner();
    h.sock.deliver({ type: 'subtitle_final', speaker_id: 'peer-b', speaker_name: 'Bea', original: 'Ciao', lang: 'it', translations: {} });
    expect(h.lastCaptions().partner?.text).toBe('Ciao');
  });

  it('moves a settled sentence into the history only when the next one starts', async () => {
    const h = await withPartner();
    const fin = (n: number) =>
      h.sock.deliver({ type: 'subtitle_final', speaker_id: 'peer-b', speaker_name: 'Bea', original: `o${n}`, lang: 'es', translations: { it: `t${n}` } });
    fin(1);
    expect(h.lastCaptions().history).toHaveLength(0);
    h.sock.deliver({ type: 'subtitle_interim', speaker_id: 'peer-b', speaker_name: 'Bea', text: 'nuova', lang: 'it' });
    expect(h.lastCaptions().history).toEqual([
      expect.objectContaining({ who: 'partner', original: 'o1', translated: 't1', originalLang: 'es', translatedLang: 'it' }),
    ]);
    expect(h.lastCaptions().partner?.settled).toBe(false);
  });

  it('keeps the history bounded', async () => {
    const h = await withPartner();
    for (let i = 0; i < HISTORY_LIMIT + 12; i++) {
      h.sock.deliver({ type: 'subtitle_final', speaker_id: 'peer-b', speaker_name: 'B', original: `o${i}`, lang: 'es', translations: { it: `t${i}` } });
    }
    h.sock.deliver({ type: 'subtitle_interim', speaker_id: 'peer-b', speaker_name: 'B', text: '…', lang: 'it' });
    const hist = h.lastCaptions().history;
    expect(hist).toHaveLength(HISTORY_LIMIT);
    expect(HISTORY_LIMIT).toBe(30);
    expect(hist[hist.length - 1].original).toBe(`o${HISTORY_LIMIT + 11}`);
  });

  it('flushes the held sentences into the history when the conversation ends', async () => {
    const h = await withPartner();
    h.sock.deliver({ type: 'subtitle_final', speaker_id: 'peer-b', speaker_name: 'B', original: 'o', lang: 'es', translations: { it: 't' } });
    h.convo.end();
    expect(h.lastCaptions().history.map((x) => x.original)).toEqual(['o']);
  });

  it('ignores a malformed frame and an unknown type', async () => {
    const h = await withPartner();
    const before = h.captions.length;
    h.sock.onmessage?.({ data: '{not json' });
    h.sock.onmessage?.({ data: new ArrayBuffer(4) });
    h.sock.deliver({ type: 'something_new' });
    expect(h.captions.length).toBe(before);
    expect(h.last().phase).toBe('live');
  });
});

describe('audio', () => {
  it('plays translated audio keyed by speaker', async () => {
    const h = await withPartner();
    h.sock.deliver({ type: 'translated_audio', speaker_id: 'peer-b', lang: 'it', seq: 3, pcm16_b64: 'AAAA' });
    h.sock.deliver({ type: 'translated_audio', speaker_id: 'peer-b', lang: 'it', seq: 4, pcm16_b64: '' });
    expect(h.playback.enqueued).toEqual([['peer-b', 3, 'AAAA']]);
  });

  it('gates my capture while the partner translation plays, and releases it afterwards', async () => {
    const h = await withPartner();
    h.playback.listener?.(true);
    expect(captures[0].gated).toEqual([true]);
    h.playback.listener?.(false);
    expect(captures[0].gated).toEqual([true, false]);
  });

  it('keeps my microphone open while I am the one speaking (guard speech-active)', async () => {
    const h = await withPartner();
    h.sock.deliver({ type: 'subtitle_interim', speaker_id: 'peer-a', speaker_name: 'Me', text: 'x', lang: 'es', original: 'ciao' });
    h.playback.listener?.(true);
    expect(captures[0].gated).toEqual([]);
  });
});

describe('mute', () => {
  it('tells the server, and gates the local capture', async () => {
    const h = await withPartner();
    h.convo.setMuted(true);
    expect(h.sock.controls()).toContainEqual({ type: 'mute_audio', muted: true });
    expect(captures[0].muted).toEqual([true]);
    expect(h.convo.isMuted()).toBe(true);
    expect(h.mutes).toEqual([true]);
    h.convo.setMuted(false);
    expect(h.sock.controls()).toContainEqual({ type: 'mute_audio', muted: false });
    expect(captures[0].muted).toEqual([true, false]);
  });

  it('keeps me muted across a reconnect: no capture starts, mute is re-sent', async () => {
    const h = await withPartner();
    h.convo.setMuted(true);
    h.sock.drop();
    fireTimer();
    await flush();
    const next = sockets[1];
    next.open();
    expect(next.controls()).toContainEqual({ type: 'mute_audio', muted: true });
    expect(next.controls()).not.toContainEqual({ type: 'start' });
    expect(captures[1].started).toBe(0);
  });
});

describe('reconnect', () => {
  it('reuses the SAME token, with jittered capped backoff, and recovers', async () => {
    const h = await withPartner();
    h.sock.drop();
    expect(h.last().phase).toBe('reconnecting');
    expect(setNetworkDegraded).toHaveBeenLastCalledWith(true);
    const ms = fireTimer();
    expect(ms).toBe(750); // 1000 * (0.5 + 0.5 * 0.5)
    await flush();
    expect(h.probe).toHaveBeenCalledWith('s1', 'LIVE-TOKEN');
    expect(sockets).toHaveLength(2);
    expect(sockets[1].url).toBe(sockets[0].url);
    sockets[1].open();
    sockets[1].deliver({ type: 'room_joined', peer_id: 'peer-a', peers: [PARTNER_ROOM_PEER] });
    expect(h.last().phase).toBe('live');
    expect(setNetworkDegraded).toHaveBeenLastCalledWith(false);
  });

  it('doubles the delay on consecutive failures and gives up after the bound', async () => {
    const h = await connect();
    const delays: number[] = [];
    h.sock.drop();
    for (let i = 0; i < RECONNECT_MAX_ATTEMPTS; i++) {
      delays.push(fireTimer());
      await flush();
      sockets[sockets.length - 1].drop();
    }
    // the last step is capped at 30 s before jitter: 30000 * 0.75
    expect(delays).toEqual([750, 1500, 3000, 6000, 12000, 22500]);
    expect(h.last().phase).toBe('failed');
    expect(h.last().failure).toBe('network');
    expect(timers.filter((t) => !t.cleared)).toHaveLength(0);
    expect(sessionStorage.getItem(STORAGE_KEY)).not.toBeNull();
  });

  it('stops retrying and reports "ended" when the server says the session is over (410)', async () => {
    const probe = vi.fn(async (): Promise<Probe> => ({ ok: false, status: 410, code: 'session_ended' }));
    const h = await connect({ getSessionImpl: probe });
    h.sock.drop();
    fireTimer();
    await flush();
    expect(h.last().phase).toBe('ended');
    expect(sockets).toHaveLength(1);
    expect(timers.filter((t) => !t.cleared)).toHaveLength(0);
    expect(sessionStorage.getItem(STORAGE_KEY)).toBeNull();
  });

  it('reports ended when the probe finds the session already ended', async () => {
    const probe = vi.fn(async (): Promise<Probe> => {
      const p = activeProbe();
      if (p.ok) p.data.session.status = 'ended';
      return p;
    });
    const h = await connect({ getSessionImpl: probe });
    h.sock.drop();
    fireTimer();
    await flush();
    expect(h.last().phase).toBe('ended');
  });

  it('treats an expired token as the end, and any other refusal as a failure', async () => {
    const expired = await connect({ getSessionImpl: async (): Promise<Probe> => ({ ok: false, status: 401, code: 'token_expired' }) });
    expired.sock.drop();
    fireTimer();
    await flush();
    expect(expired.last().phase).toBe('ended');
    expect(expired.last().endReason).toBe('expired');

    const wrong = await connect({ getSessionImpl: async (): Promise<Probe> => ({ ok: false, status: 403, code: 'wrong_session' }) });
    wrong.sock.drop();
    fireTimer();
    await flush();
    expect(wrong.last().phase).toBe('failed');
    expect(wrong.last().failure).toBe('unknown');
  });

  it('keeps retrying when the probe itself fails on the network', async () => {
    const probe = vi.fn(async (): Promise<Probe> => ({ ok: false, status: 0, code: 'network' }));
    const h = await connect({ getSessionImpl: probe });
    h.sock.drop();
    fireTimer();
    await flush();
    expect(h.last().phase).toBe('reconnecting');
    expect(sockets).toHaveLength(2); // a transient probe failure does not stop the retry
    sockets[1].drop();
    expect(timers.filter((t) => !t.cleared)).toHaveLength(1); // the next backoff is armed
  });

  it('a user end during the probe wins: no socket is opened afterwards', async () => {
    let release: (v: Probe) => void = () => {};
    const probe = vi.fn(() => new Promise<Probe>((r) => (release = r)));
    const h = await connect({ getSessionImpl: probe });
    h.sock.drop();
    fireTimer();
    h.convo.end();
    release(activeProbe());
    await flush();
    expect(sockets).toHaveLength(1);
    expect(h.last().phase).toBe('ended');
  });
});

describe('default timers (real browsers)', () => {
  it('calls setTimeout/clearTimeout unbound: stored on the instance they would throw "Illegal invocation"', async () => {
    const realSet = globalThis.setTimeout;
    const realClear = globalThis.clearTimeout;
    const strict = (name: string, fn: (...a: never[]) => unknown) =>
      function (this: unknown, ...a: never[]) {
        // A browser rejects a timer function invoked with any receiver but the window.
        if (this !== undefined && this !== globalThis) throw new TypeError(`Illegal invocation: ${name}`);
        return fn(...a);
      };
    const pending: Array<() => void> = [];
    globalThis.setTimeout = strict('setTimeout', ((f: () => void) => pending.push(f)) as never) as never;
    globalThis.clearTimeout = strict('clearTimeout', (() => {}) as never) as never;
    try {
      const h = await connect({ setTimeoutImpl: undefined, clearTimeoutImpl: undefined });
      h.sock.drop();
      expect(h.last().phase).toBe('reconnecting');
      expect(pending.length).toBeGreaterThanOrEqual(1);
      h.convo.end(); // clears the pending reconnect through the default clearTimeout
      expect(h.last().phase).toBe('ended');
    } finally {
      globalThis.setTimeout = realSet;
      globalThis.clearTimeout = realClear;
    }
  });
});

describe('server verdicts', () => {
  it.each(['ended_by_host', 'ended_by_guest', 'ended_by_staff', 'balance_exhausted', 'max_duration', 'abandoned', 'billing_error', 'expired', 'desk_revoked', 'desk_regenerated'])(
    'session_state with reason %s ends the session with that reason and releases everything',
    async (reason) => {
      const h = await withPartner();
      // A desk session nobody answered is `expired` on the server, whatever closed it.
      h.sock.deliver({ type: 'session_state', status: reason === 'expired' || reason.startsWith('desk_') ? 'expired' : 'ended', reason });
      expect(h.last().phase).toBe('ended');
      expect(h.last().endReason).toBe(reason);
      expect(h.media.tracks[0].stop).toHaveBeenCalled();
      expect(h.sock.closed.length).toBeGreaterThan(0);
      expect(h.playback.stops).toBeGreaterThan(0);
      expect(sessionStorage.getItem(STORAGE_KEY)).toBeNull();
      // The close that follows must not schedule a reconnect.
      h.sock.drop();
      expect(timers.filter((t) => !t.cleared)).toHaveLength(0);
    },
  );

  it('session_state expired without a reason maps to expired', async () => {
    const h = await withPartner();
    h.sock.deliver({ type: 'session_state', status: 'expired' });
    expect(h.last().endReason).toBe('expired');
  });

  it('an unknown reason is still an ended session', async () => {
    const h = await withPartner();
    h.sock.deliver({ type: 'session_state', status: 'ended', reason: 'brand_new_reason' });
    expect(h.last().phase).toBe('ended');
    expect(h.last().endReason).toBe('abandoned');
  });

  it('balance_exhausted ends the session even before session_state arrives', async () => {
    const h = await withPartner();
    h.sock.deliver({ type: 'balance_exhausted' });
    expect(h.last().phase).toBe('ended');
    expect(h.last().endReason).toBe('balance_exhausted');
  });

  it('balance frames reach the callbacks', async () => {
    const h = await withPartner();
    h.sock.deliver({ type: 'balance_update', balance: 4.2 });
    h.sock.deliver({ type: 'low_balance', balance: 0.4 });
    expect(h.balances).toEqual([4.2]);
    expect(h.lows).toEqual([0.4]);
  });

  it.each([
    ['insufficient_balance', 'credits'],
    ['insufficient_credits', 'credits'],
    ['live_busy', 'busy'],
  ])('error %s fails the session as %s and tears down', async (code, kind) => {
    const h = await withPartner();
    h.sock.deliver({ type: 'error', code });
    expect(h.last().phase).toBe('failed');
    expect(h.last().failure).toBe(kind);
    expect(h.media.tracks[0].stop).toHaveBeenCalled();
  });

  it.each(['billing_unavailable', 'server_busy', 'something_else'])('error %s is a notice and the conversation continues', async (code) => {
    const h = await withPartner();
    h.sock.deliver({ type: 'error', code });
    expect(h.notices).toEqual([code]);
    expect(h.last().phase).toBe('live');
  });

  it.each(['audio_session_closed', 'engine_unavailable'])('error %s re-sends start after a delay, at most 3 times, then tells the user', async (code) => {
    const h = await withPartner();
    const starts = () => h.sock.controls().filter((c) => (c as { type: string }).type === 'start').length;
    const base = starts();
    for (let i = 1; i <= 3; i++) {
      h.sock.deliver({ type: 'error', code });
      expect(h.notices).toEqual([]);
      fireTimer();
      expect(starts()).toBe(base + i);
    }
    h.sock.deliver({ type: 'error', code });
    expect(h.notices).toEqual([code]);
    expect(timers.filter((t) => !t.cleared)).toHaveLength(0);
    expect(h.last().phase).toBe('live');
  });

  it('a working translation resets the start-retry budget', async () => {
    const h = await withPartner();
    for (let i = 0; i < 3; i++) {
      h.sock.deliver({ type: 'error', code: 'audio_session_closed' });
      fireTimer();
    }
    h.sock.deliver({ type: 'subtitle_final', speaker_id: 'peer-a', speaker_name: 'Me', original: 'ok', lang: 'it', translations: {} });
    h.sock.deliver({ type: 'error', code: 'audio_session_closed' });
    expect(h.notices).toEqual([]);
  });
});

describe('end and release', () => {
  it('end() asks the server to end it (best effort), releases every resource, clears storage', async () => {
    const h = await withPartner();
    h.convo.end();
    expect(h.endRest).toHaveBeenCalledWith('s1', 'LIVE-TOKEN');
    expect(h.last().phase).toBe('ended');
    expect(h.last().endReason).toBe('ended_by_me');
    expect(h.media.tracks[0].stop).toHaveBeenCalled();
    expect(captures[0].stopped).toBeGreaterThan(0);
    expect(h.sock.closed).toEqual([1000]);
    expect(h.playback.stops).toBe(1);
    expect(h.wakeLock.release).toHaveBeenCalled();
    expect(sessionStorage.getItem(STORAGE_KEY)).toBeNull();
    expect(setNetworkDegraded).toHaveBeenLastCalledWith(false);
  });

  it('end() still tears down when the REST call rejects', async () => {
    const h = await withPartner({ endSessionImpl: async () => Promise.reject(new Error('offline')) });
    h.convo.end();
    await flush();
    expect(h.last().phase).toBe('ended');
  });

  it('end() clears a pending reconnect timer', async () => {
    const h = await withPartner();
    h.sock.drop();
    h.convo.end();
    expect(timers.filter((t) => !t.cleared)).toHaveLength(0);
  });

  it('release() frees the microphone and socket but keeps the session for a reload to rejoin', async () => {
    const h = await withPartner();
    h.convo.release();
    expect(h.media.tracks[0].stop).toHaveBeenCalled();
    expect(h.sock.closed).toEqual([1000]);
    expect(h.endRest).not.toHaveBeenCalled();
    expect(sessionStorage.getItem(STORAGE_KEY)).not.toBeNull();
  });

  it('end() before the microphone was granted releases the stream when it arrives', async () => {
    let grant: (s: MediaStream) => void = () => {};
    const media = fakeStream();
    const h = harness({ getMedia: () => new Promise<MediaStream>((r) => (grant = r)) });
    const p = h.convo.start();
    h.convo.end();
    grant(media.stream);
    await p;
    expect(media.tracks[0].stop).toHaveBeenCalled();
    expect(sockets).toHaveLength(0);
    expect(h.last().phase).toBe('ended');
  });
});

describe('headphones and echo guard', () => {
  it('defaults to the guarded mode, switches to open with headphones, and persists', async () => {
    const h = await connect();
    expect(h.convo.headphones()).toBe(false);
    h.convo.setHeadphones(true);
    expect(h.convo.headphones()).toBe(true);
    expect(localStorage.getItem(HEADPHONES_KEY)).toBe('1');
    // Open mode never gates the microphone for the partner's playback.
    h.playback.listener?.(true);
    expect(captures[0].gated).toEqual([]);
    h.convo.setHeadphones(false);
    expect(localStorage.getItem(HEADPHONES_KEY)).toBe('0');
  });

  it('reads the saved preference at construction', async () => {
    localStorage.setItem(HEADPHONES_KEY, '1');
    const h = await connect();
    expect(h.convo.headphones()).toBe(true);
  });
});

describe('output device', () => {
  it('detects setSinkId on AudioContext only: the playback graph is Web Audio, so a media-element sink could not be applied to it', () => {
    expect(outputSelectionSupported({ AudioContext: { prototype: {} } } as never)).toBe(false);
    expect(outputSelectionSupported({ AudioContext: { prototype: { setSinkId() {} } } } as never)).toBe(true);
    expect(outputSelectionSupported({ HTMLMediaElement: { prototype: { setSinkId() {} } } } as never)).toBe(false);
    expect(outputSelectionSupported({} as never)).toBe(false);
  });

  it('refuses without support and never touches the sink', async () => {
    const setSink = vi.fn(async () => {});
    const h = await connect({ sinkSupported: () => false, setSink });
    expect(h.convo.canSelectOutput()).toBe(false);
    await expect(h.convo.setOutputDevice('dev-1')).resolves.toBe(false);
    expect(setSink).not.toHaveBeenCalled();
  });

  it('applies the chosen device when supported, and re-applies on the first playback edge', async () => {
    const setSink = vi.fn(async () => {});
    const h = await connect({ sinkSupported: () => true, setSink });
    expect(h.convo.canSelectOutput()).toBe(true);
    await expect(h.convo.setOutputDevice('dev-1')).resolves.toBe(true);
    expect(setSink).toHaveBeenCalledWith('dev-1');
    h.playback.listener?.(true);
    await flush();
    expect(setSink.mock.calls.length).toBeGreaterThanOrEqual(1);
  });

  it('reports failure (not an exception) when the browser rejects the device', async () => {
    const setSink = vi.fn(async () => {
      throw new Error('NotAllowed');
    });
    const h = await connect({ sinkSupported: () => true, setSink });
    await expect(h.convo.setOutputDevice('x')).resolves.toBe(false);
  });

  it('lists audio outputs best-effort and returns [] when enumeration fails', async () => {
    const devices = [
      { kind: 'audiooutput', deviceId: 'a', label: 'Speaker' },
      { kind: 'audioinput', deviceId: 'b', label: 'Mic' },
    ];
    const h = await connect({ enumerateDevices: async () => devices });
    await expect(h.convo.listOutputDevices()).resolves.toEqual([devices[0]]);
    const bad = await connect({ enumerateDevices: async () => Promise.reject(new Error('x')) });
    await expect(bad.convo.listOutputDevices()).resolves.toEqual([]);
  });
});
