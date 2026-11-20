// @vitest-environment jsdom
import { beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('../auth', () => ({ WS_BASE: 'wss://api.test', HTTP_BASE: 'https://api.test' }));
const setNetworkDegraded = vi.fn();
vi.mock('../net-status', () => ({ setNetworkDegraded: (...a: unknown[]) => setNetworkDegraded(...a) }));

import {
  HISTORY_LIMIT,
  MAX_SPEAKER_CAPTIONS,
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
  const spents: number[] = [];
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
    onSpent: (n) => spents.push(n),
    onSameLanguage: (a, b, p) => same.push(`${a}/${b}/${p.peerId}`),
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
  return { convo, states, last, captions, lastCaptions, notices, balances, lows, spents, same, mutes, playback, wakeLock, media, probe, endRest };
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
    expect(h.last().partners).toEqual([{ peerId: 'peer-b', name: 'Bea', lang: 'es', muted: false }]);
    expect(h.convo.direction()).toEqual({ myLang: 'it', partnerLangs: ['es'] });
  });

  it('peer_joined then peer_left toggles live and waiting_peer', async () => {
    const h = await connect();
    h.sock.deliver({ type: 'room_joined', peer_id: 'peer-a', peers: [] });
    expect(h.last().phase).toBe('waiting_peer');
    h.sock.deliver({ type: 'peer_joined', ...PARTNER_PEER });
    expect(h.last().phase).toBe('live');
    h.sock.deliver({ type: 'peer_left', peer_id: 'peer-b' });
    expect(h.last().phase).toBe('waiting_peer');
    expect(h.convo.direction().partnerLangs).toEqual([]);
  });

  it('re-sends start when the partner arrives (upstream sessions open lazily per language)', async () => {
    const h = await connect();
    h.sock.deliver({ type: 'peer_joined', ...PARTNER_PEER });
    expect(h.sock.controls().filter((c) => (c as { type: string }).type === 'start').length).toBeGreaterThanOrEqual(2);
  });

  it('tracks the partner mute state from peer_muted (audio only)', async () => {
    const h = await withPartner();
    h.sock.deliver({ type: 'peer_muted', peer_id: 'peer-b', kind: 'audio', muted: true });
    expect(h.last().partners[0]?.muted).toBe(true);
    h.sock.deliver({ type: 'peer_muted', peer_id: 'peer-b', kind: 'video', muted: false });
    expect(h.last().partners[0]?.muted).toBe(true);
    h.sock.deliver({ type: 'peer_muted', peer_id: 'peer-b', kind: 'audio', muted: false });
    expect(h.last().partners[0]?.muted).toBe(false);
  });

  it('warns once when both people speak the same language', async () => {
    const h = await connect();
    h.sock.deliver({ type: 'peer_joined', peer_id: 'peer-b', user_name: 'Bea', lang: 'it' });
    expect(h.same).toEqual(['it/it/peer-b']);
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
    expect(h.lastCaptions().others[0]).toMatchObject({ text: 'Ciao come', original: 'Hola como', settled: false });
    h.sock.deliver({
      type: 'subtitle_final',
      speaker_id: 'peer-b',
      speaker_name: 'Bea',
      original: 'Hola, como estas?',
      lang: 'es',
      translations: { it: 'Ciao, come stai?', en: 'Hi' },
    });
    expect(h.lastCaptions().others[0]).toMatchObject({
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
    expect(h.lastCaptions().others).toEqual([]);
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
    expect(h.lastCaptions().others[0]?.text).toBe('Ciao');
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
    expect(h.lastCaptions().others[0]?.settled).toBe(false);
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

// --- a host and up to three guests in one room (spec 0121 multi-guest) --------------------------
const ROOM_PEERS = [
  { id: 'peer-b', user_name: 'Bea', lang: 'es' },
  { id: 'peer-c', user_name: 'Carlo', lang: 'en' },
  { id: 'peer-d', user_name: 'Dina', lang: 'es' },
];

async function withThree(over: Record<string, unknown> = {}) {
  const h = await connect(over);
  h.sock.deliver({ type: 'room_joined', peer_id: 'peer-a', peers: [{ id: 'peer-a', user_name: 'Me', lang: 'it' }, ...ROOM_PEERS] });
  return h;
}

describe('several partners', () => {
  it('room_joined seeds up to three partners, in order, and direction lists each language once', async () => {
    const h = await withThree();
    expect(h.last().phase).toBe('live');
    expect(h.last().partners.map((p) => p.name)).toEqual(['Bea', 'Carlo', 'Dina']);
    expect(h.convo.direction()).toEqual({ myLang: 'it', partnerLangs: ['es', 'en'] });
  });

  it('peer_joined adds, peer_left removes one: still live; the last one leaving is waiting_peer, not ended', async () => {
    const h = await connect();
    h.sock.deliver({ type: 'room_joined', peer_id: 'peer-a', peers: [] });
    h.sock.deliver({ type: 'peer_joined', peer_id: 'peer-b', user_name: 'Bea', lang: 'es' });
    h.sock.deliver({ type: 'peer_joined', peer_id: 'peer-c', user_name: 'Carlo', lang: 'en' });
    expect(h.last().partners).toHaveLength(2);
    h.sock.deliver({ type: 'peer_left', peer_id: 'peer-b' });
    expect(h.last().phase).toBe('live');
    expect(h.last().partners.map((p) => p.peerId)).toEqual(['peer-c']);
    h.sock.deliver({ type: 'peer_left', peer_id: 'peer-c' });
    expect(h.last().phase).toBe('waiting_peer');
    expect(h.states.some((c) => c.phase === 'ended')).toBe(false);
  });

  it('keeps one mute flag per partner', async () => {
    const h = await withThree();
    h.sock.deliver({ type: 'peer_muted', peer_id: 'peer-c', kind: 'audio', muted: true });
    expect(h.last().partners.map((p) => p.muted)).toEqual([false, true, false]);
  });

  it('warns once per partner who shares my language, including one who arrives later', async () => {
    const h = await connect();
    h.sock.deliver({ type: 'room_joined', peer_id: 'peer-a', peers: [{ id: 'peer-b', user_name: 'Bea', lang: 'it' }, { id: 'peer-c', user_name: 'Carlo', lang: 'en' }] });
    expect(h.same).toEqual(['it/it/peer-b']);
    h.sock.deliver({ type: 'peer_joined', peer_id: 'peer-d', user_name: 'Dina', lang: 'it-IT' });
    expect(h.same).toEqual(['it/it/peer-b', 'it/it-IT/peer-d']);
    h.sock.deliver({ type: 'peer_joined', peer_id: 'peer-d', user_name: 'Dina', lang: 'it-IT' });
    expect(h.same).toHaveLength(2);
    // Gone and back (a new seat holder): warned again.
    h.sock.deliver({ type: 'peer_left', peer_id: 'peer-b' });
    h.sock.deliver({ type: 'peer_joined', peer_id: 'peer-b', user_name: 'Bea', lang: 'it' });
    expect(h.same).toHaveLength(3);
  });

  it('announces who joined and who left, without announcing a reconnect', async () => {
    const seen: string[] = [];
    const h = await connect({ onPeerChange: (c: string, p: { name: string }) => seen.push(`${c}:${p.name}`) });
    h.sock.deliver({ type: 'room_joined', peer_id: 'peer-a', peers: [ROOM_PEERS[0]] });
    h.sock.deliver({ type: 'peer_joined', peer_id: 'peer-c', user_name: 'Carlo', lang: 'en' });
    h.sock.deliver({ type: 'peer_left', peer_id: 'peer-b' });
    h.sock.deliver({ type: 'peer_left', peer_id: 'nobody' });
    expect(seen).toEqual(['joined:Bea', 'joined:Carlo', 'left:Bea']);
    h.sock.drop();
    fireTimer();
    await flush();
    sockets[sockets.length - 1].open();
    sockets[sockets.length - 1].deliver({ type: 'room_joined', peer_id: 'peer-a', peers: [{ id: 'peer-c', user_name: 'Carlo', lang: 'en' }] });
    expect(seen).toEqual(['joined:Bea', 'joined:Carlo', 'left:Bea']);
  });

  it('a reconnect room_joined drops whoever is no longer in the room, with their caption', async () => {
    const h = await withThree();
    h.sock.deliver({ type: 'subtitle_interim', speaker_id: 'peer-d', speaker_name: 'Dina', text: 'ciao', lang: 'it', original: 'hola' });
    h.sock.drop();
    fireTimer();
    await flush();
    const s2 = sockets[sockets.length - 1];
    s2.open();
    s2.deliver({ type: 'room_joined', peer_id: 'peer-a', peers: [ROOM_PEERS[0]] });
    expect(h.last().partners.map((p) => p.peerId)).toEqual(['peer-b']);
    expect(h.lastCaptions().others).toEqual([]);
  });
});

describe('captions per speaker', () => {
  const interim = (h: { sock: FakeSocket }, id: string, name: string, text: string) =>
    h.sock.deliver({ type: 'subtitle_interim', speaker_id: id, speaker_name: name, text, lang: 'it', original: `o-${text}` });
  const final = (h: { sock: FakeSocket }, id: string, name: string, n: string, tr: Record<string, string> = { it: `t${n}` }) =>
    h.sock.deliver({ type: 'subtitle_final', speaker_id: id, speaker_name: name, original: `o${n}`, lang: 'es', translations: tr });

  it('keeps each other speaker on their own line, newest activity first, with their name', async () => {
    const h = await withThree();
    interim(h, 'peer-b', 'Bea', 'uno');
    interim(h, 'peer-c', 'Carlo', 'due');
    expect(h.lastCaptions().others.map((c) => [c.speakerId, c.name, c.text])).toEqual([
      ['peer-c', 'Carlo', 'due'],
      ['peer-b', 'Bea', 'uno'],
    ]);
    interim(h, 'peer-b', 'Bea', 'uno e');
    expect(h.lastCaptions().others.map((c) => c.speakerId)).toEqual(['peer-b', 'peer-c']);
    expect(h.lastCaptions().mine).toBeNull();
  });

  it('shows a speaker in MY language from the translations map, falling back to the original', async () => {
    const h = await withThree();
    final(h, 'peer-c', 'Carlo', '1', { it: 'Ciao', es: 'Hola' });
    final(h, 'peer-b', 'Bea', '2', { en: 'only english' });
    const [bea, carlo] = h.lastCaptions().others;
    expect(carlo).toMatchObject({ speakerId: 'peer-c', text: 'Ciao', settled: true });
    expect(bea).toMatchObject({ speakerId: 'peer-b', text: 'o2' });
  });

  it('falls back to the partner name when the frame carries none', async () => {
    const h = await withThree();
    h.sock.deliver({ type: 'subtitle_interim', speaker_id: 'peer-d', text: 'x', lang: 'it' });
    expect(h.lastCaptions().others[0]).toMatchObject({ name: 'Dina', lang: 'es' });
  });

  it("moves only that speaker's settled sentence into the history when their next one starts", async () => {
    const h = await withThree();
    final(h, 'peer-b', 'Bea', '1');
    final(h, 'peer-c', 'Carlo', '2');
    interim(h, 'peer-b', 'Bea', 'nuova');
    const hist = h.lastCaptions().history;
    expect(hist.map((x) => [x.speakerId, x.speakerName, x.original])).toEqual([['peer-b', 'Bea', 'o1']]);
    // Carlo's sentence is still on screen, and only there.
    expect(h.lastCaptions().others.find((c) => c.speakerId === 'peer-c')?.settled).toBe(true);
  });

  it('clears a speaker caption when they leave and keeps their last sentence in the history', async () => {
    const h = await withThree();
    final(h, 'peer-b', 'Bea', '1');
    h.sock.deliver({ type: 'peer_left', peer_id: 'peer-b' });
    expect(h.lastCaptions().others).toEqual([]);
    expect(h.lastCaptions().history.map((x) => x.original)).toEqual(['o1']);
    // Their translation arriving late must not bring the caption back.
    final(h, 'peer-b', 'Bea', '2');
    expect(h.lastCaptions().others).toEqual([]);
    interim(h, 'peer-b', 'Bea', 'late');
    expect(h.lastCaptions().others).toEqual([]);
    expect(h.lastCaptions().history.map((x) => x.original)).toEqual(['o1', 'o2']);
  });

  it('is bounded to four speakers: the stalest caption makes room', async () => {
    const h = await withThree();
    for (const id of ['s1', 's2', 's3', 's4', 's5']) interim(h, id, id, id);
    expect(h.lastCaptions().others.map((c) => c.speakerId)).toEqual(['s5', 's4', 's3', 's2']);
    expect(MAX_SPEAKER_CAPTIONS).toBe(4);
  });

  it('my own sentence shows what the first translated language of my listeners heard', async () => {
    const h = await withThree();
    h.sock.deliver({ type: 'subtitle_final', speaker_id: 'peer-a', speaker_name: 'Me', original: 'Ciao a tutti', lang: 'it', translations: { en: 'Hello all', es: 'Hola' } });
    h.sock.deliver({ type: 'subtitle_interim', speaker_id: 'peer-a', speaker_name: 'Me', text: '', lang: 'es', original: 'ancora' });
    const mine = h.lastCaptions().history.find((x) => x.who === 'me');
    expect(mine).toMatchObject({ translated: 'Hola', translatedLang: 'es' });
    expect(h.lastCaptions().others).toEqual([]);
  });

  it('keeps the playback sequence guard per speaker (one stream each)', async () => {
    const h = await withThree();
    h.sock.deliver({ type: 'translated_audio', speaker_id: 'peer-b', lang: 'it', seq: 1, pcm16_b64: 'AA' });
    h.sock.deliver({ type: 'translated_audio', speaker_id: 'peer-c', lang: 'it', seq: 1, pcm16_b64: 'BB' });
    h.sock.deliver({ type: 'translated_audio', speaker_id: 'peer-d', lang: 'it', seq: 1, pcm16_b64: 'CC' });
    expect(h.playback.enqueued).toEqual([['peer-b', 1, 'AA'], ['peer-c', 1, 'BB'], ['peer-d', 1, 'CC']]);
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

  it('the running spend reaches its callback, and a malformed one is ignored', async () => {
    const h = await withPartner();
    h.sock.deliver({ type: 'session_spent', spent: 0.0123 });
    h.sock.deliver({ type: 'session_spent', spent: 0.0301 });
    h.sock.deliver({ type: 'session_spent', spent: 'x' });
    h.sock.deliver({ type: 'session_spent' });
    expect(h.spents).toEqual([0.0123, 0.0301]);
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

describe('engine_downgraded', () => {
  const frame = { type: 'engine_downgraded', peer_id: 'peer-a', from: 'gemini_live_translate', to: 'standard', reason: 'premium_at_capacity' };

  it('tells the page once and never ends or interrupts the conversation', async () => {
    const seen: Array<{ from: string; to: string; reason: string }> = [];
    const h = await withPartner({ engine: 'gemini_live_translate', onEngineDowngraded: (d: { from: string; to: string; reason: string }) => seen.push(d) });
    const before = h.states.length;
    h.sock.deliver(frame);
    expect(seen).toEqual([{ from: 'gemini_live_translate', to: 'standard', reason: 'premium_at_capacity' }]);
    expect(h.last().phase).toBe('live');
    expect(h.states.length).toBe(before);
    expect(h.sock.closed).toHaveLength(0);
    expect(h.notices).toEqual([]);
    expect(h.media.tracks[0].stop).not.toHaveBeenCalled();
    // The conversation goes on: a caption still arrives.
    h.sock.deliver({ type: 'subtitle_final', speaker_id: 'peer-b', text: 'hola', lang: 'es', seq: 1 });
    expect(h.last().phase).toBe('live');
  });

  it('is harmless when nobody listens and when the frame is incomplete', async () => {
    const seen: unknown[] = [];
    const h = await withPartner({ onEngineDowngraded: (d: unknown) => seen.push(d) });
    h.sock.deliver({ type: 'engine_downgraded' });
    expect(seen).toEqual([{ from: '', to: 'standard', reason: '' }]);
    const g = await withPartner();
    g.sock.deliver(frame);
    expect(g.last().phase).toBe('live');
  });

  it('keeps the stored session on the tier it now runs on, so a reload does not claim Premium', async () => {
    const h = await withPartner({ engine: 'gemini_live_translate' });
    expect(JSON.parse(sessionStorage.getItem(STORAGE_KEY) ?? '{}').engine).toBe('gemini_live_translate');
    h.sock.deliver(frame);
    expect(JSON.parse(sessionStorage.getItem(STORAGE_KEY) ?? '{}').engine).toBe('standard');
  });

  it('a session without an engine stores none (backward compatible)', async () => {
    await withPartner();
    expect(JSON.parse(sessionStorage.getItem(STORAGE_KEY) ?? '{}')).not.toHaveProperty('engine');
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

  it('a guest hanging up in a multi-guest conversation LEAVES it: left_by_me, storage cleared', async () => {
    const h = await withPartner({ role: 'guest', maxGuests: 3 });
    h.convo.end();
    expect(h.endRest).toHaveBeenCalledWith('s1', 'LIVE-TOKEN');
    expect(h.last()).toMatchObject({ phase: 'ended', endReason: 'left_by_me' });
    expect(sessionStorage.getItem(STORAGE_KEY)).toBeNull();
  });

  it('the host ending a multi-guest conversation is still an end, and so is a guest in a pair', async () => {
    const host = await withPartner({ role: 'host', maxGuests: 3 });
    host.convo.end();
    expect(host.last().endReason).toBe('ended_by_me');
    const pair = await withPartner({ role: 'guest', maxGuests: 1 });
    pair.convo.end();
    expect(pair.last().endReason).toBe('ended_by_me');
  });

  it('trusts the server when it answers {ended:false,left:true} for a session whose size we did not know', async () => {
    const h = await withPartner({
      role: 'guest',
      endSessionImpl: async () => ({ ok: true, data: { ended: false, left: true } }),
    });
    h.convo.end();
    expect(h.last().endReason).toBe('ended_by_me');
    await flush();
    expect(h.last().endReason).toBe('left_by_me');
  });

  it('remembers how many guests the invitation admits, so a reload keeps the right behaviour', async () => {
    await withPartner({ role: 'host', maxGuests: 2 });
    expect(JSON.parse(sessionStorage.getItem(STORAGE_KEY) ?? '{}').maxGuests).toBe(2);
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

// --- Enhanced (Cartesia on the phone, spec 0121 S5) -------------------------------------------

class FakeEnhanced {
  opened = 0;
  started = 0;
  stopped = 0;
  closed = 0;
  muted: boolean[] = [];
  gated: boolean[] = [];
  spoken: Array<{ speakerId: string; segmentId: string; text: string; voiceId: string | null }> = [];
  cancels = 0;
  openResult = true;
  constructor(readonly deps: import('./enhanced').EnhancedDeps) {
    enhancedEngines.push(this);
  }
  async open() {
    this.opened += 1;
    return this.openResult;
  }
  start() {
    this.started += 1;
  }
  stop() {
    this.stopped += 1;
  }
  setMuted(m: boolean) {
    this.muted.push(m);
  }
  setGated(g: boolean) {
    this.gated.push(g);
  }
  speak(item: { speakerId: string; segmentId: string; text: string; voiceId: string | null }) {
    this.spoken.push(item);
  }
  cancelSpeech() {
    this.cancels += 1;
  }
  close() {
    this.closed += 1;
  }
}
let enhancedEngines: FakeEnhanced[] = [];

function enhancedHarness(over: Record<string, unknown> = {}) {
  const downgrades: Array<{ from: string; to: string; reason: string }> = [];
  const speaking: boolean[] = [];
  const token = vi.fn(async () => ({ ok: false as const, status: 500, code: 'x' }));
  const h = harness({
    engine: 'cartesia',
    createEnhanced: (d: import('./enhanced').EnhancedDeps) => new FakeEnhanced(d),
    enhancedSessionImpl: token,
    isRestricted: async () => false,
    onEngineDowngraded: (c: { from: string; to: string; reason: string }) => downgrades.push(c),
    onSpeaking: (s: boolean) => speaking.push(s),
    ...over,
  });
  return { ...h, downgrades, speaking, token };
}

async function enhancedConnected(over: Record<string, unknown> = {}) {
  const h = enhancedHarness(over);
  await h.convo.start();
  const sock = sockets[sockets.length - 1];
  sock.open();
  await flush();
  return { ...h, sock, engine: enhancedEngines[enhancedEngines.length - 1] };
}

describe('Enhanced mode', () => {
  beforeEach(() => {
    enhancedEngines = [];
  });

  it('runs the Enhanced engine instead of PCM capture, and does not send `start` itself', async () => {
    const { sock, engine } = await enhancedConnected();
    expect(enhancedEngines).toHaveLength(1);
    expect(captures).toHaveLength(0);
    expect(engine.opened).toBe(1);
    expect(sock.controls()).not.toContainEqual({ type: 'start' });
    // Once the token is in, recognition starts (the engine sends `start` when its socket is up).
    expect(engine.started).toBe(1);
  });

  it('hands the engine the live token fetch, my language and the stream; its control frames go to the live socket', async () => {
    const { sock, engine, token, media } = await enhancedConnected();
    expect(engine.deps.stream).toBe(media.stream);
    expect(engine.deps.myLang).toBe('it');
    await engine.deps.fetchToken();
    expect(token).toHaveBeenCalledWith('s1', 'LIVE-TOKEN');
    engine.deps.control({ type: 'translate_text', segment_id: 'x', text: 'ciao', final: false });
    expect(sock.controls()).toContainEqual({ type: 'translate_text', segment_id: 'x', text: 'ciao', final: false });
  });

  it('does not start recognising while muted, and unmuting starts it', async () => {
    const h = enhancedHarness();
    h.convo.setMuted(true);
    await h.convo.start();
    sockets[sockets.length - 1].open();
    await flush();
    const engine = enhancedEngines[0];
    expect(engine.started).toBe(0);
    expect(sockets[0].controls()).toContainEqual({ type: 'mute_audio', muted: true });
    h.convo.setMuted(false);
    expect(engine.muted).toEqual([false]);
  });

  it('mute is told to the server and to the engine', async () => {
    const { convo, sock, engine } = await enhancedConnected();
    convo.setMuted(true);
    expect(sock.controls()).toContainEqual({ type: 'mute_audio', muted: true });
    expect(engine.muted).toEqual([true]);
  });

  it('shows my own words from my recogniser: interim, then a settled sentence that goes to the history next', async () => {
    const h = await enhancedConnected();
    h.engine.deps.onMine('Buon', false);
    expect(h.lastCaptions().mine).toMatchObject({ text: 'Buon', settled: false, lang: 'it' });
    h.engine.deps.onMine('Buongiorno a tutti', true);
    expect(h.lastCaptions().mine).toMatchObject({ text: 'Buongiorno a tutti', settled: true });
    h.engine.deps.onMine('Ancora', false);
    expect(h.lastCaptions().history.map((x) => [x.who, x.original, x.translated])).toEqual([['me', 'Buongiorno a tutti', '']]);
  });

  it('speaks what the server says to speak, with the voice it names (or none)', async () => {
    const { sock, engine } = await enhancedConnected();
    sock.deliver({ type: 'enhanced_speak', speaker_id: 'peer-b', segment_id: 'seg1', lang: 'it', text: 'Ciao a tutti', voice_id: 'HOST-VOICE' });
    sock.deliver({ type: 'enhanced_speak', speaker_id: 'peer-b', segment_id: 'seg2', lang: 'it', text: 'Seconda', voice_id: null });
    expect(engine.spoken).toEqual([
      { speakerId: 'peer-b', segmentId: 'seg1', text: 'Ciao a tutti', voiceId: 'HOST-VOICE' },
      { speakerId: 'peer-b', segmentId: 'seg2', text: 'Seconda', voiceId: null },
    ]);
  });

  it('ignores an enhanced_speak of my own, one without text, and one on a Standard conversation', async () => {
    const { sock, engine } = await enhancedConnected();
    sock.deliver({ type: 'enhanced_speak', speaker_id: 'peer-a', segment_id: 's', lang: 'it', text: 'io', voice_id: null });
    sock.deliver({ type: 'enhanced_speak', speaker_id: 'peer-b', segment_id: 's', lang: 'it', voice_id: null });
    expect(engine.spoken).toEqual([]);
    const std = await connect();
    std.sock.deliver({ type: 'enhanced_speak', speaker_id: 'peer-b', segment_id: 's', lang: 'it', text: 'x', voice_id: null });
    expect(enhancedEngines).toHaveLength(1);
  });

  it('the echo guard gates the recogniser while a translation plays, shows "speaking", and barge-in cancels the queue', async () => {
    const h = await enhancedConnected();
    h.playback.listener?.(true);
    expect(h.engine.gated).toEqual([true]);
    expect(h.speaking).toEqual([true]);
    h.playback.listener?.(false);
    expect(h.engine.gated).toEqual([true, false]);
    expect(h.speaking).toEqual([true, false]);
  });

  it('talking over the translation (barge-in) drops what is waiting to be spoken', async () => {
    let t = 0;
    const h = await enhancedConnected({ now: () => t });
    h.playback.listener?.(true);
    const guard = (h.convo as unknown as { guard: { onLevel: (l: number) => void } }).guard;
    guard.onLevel(0.5);
    t = 400;
    guard.onLevel(0.5);
    expect(h.engine.cancels).toBe(1);
    expect(h.playback.resets).toBe(1);
  });

  it('a gated guard before the engine exists is applied to it', async () => {
    const h = enhancedHarness();
    await h.convo.start();
    h.playback.listener?.(true);
    sockets[0].open();
    await flush();
    expect(enhancedEngines[0].gated).toContain(true);
  });

  it('"speaking" is Enhanced-only: a Standard conversation never reports it', async () => {
    const speaking: boolean[] = [];
    const h = await connect({ onSpeaking: (s: boolean) => speaking.push(s) });
    h.playback.listener?.(true);
    expect(speaking).toEqual([]);
  });

  describe('falling back', () => {
    it.each(['rate_limited', 'enhanced_unavailable', 'insufficient_balance', 'cartesia_unreachable', 'restricted_network'])(
      'sends enhanced_fallback{%s} and waits for the server before changing path',
      async (reason) => {
        const { sock, engine } = await enhancedConnected();
        engine.deps.onFailure({ kind: 'fallback', reason });
        expect(sock.controls()).toContainEqual({ type: 'enhanced_fallback', reason });
        expect(captures).toHaveLength(0); // still no PCM: that is the server's call
      },
    );

    it('engine_downgraded then closes Cartesia, starts PCM exactly like a Standard conversation and says so', async () => {
      const { sock, engine, downgrades, convo, speaking } = await enhancedConnected();
      sock.deliver({ type: 'engine_downgraded', peer_id: 'peer-a', from: 'cartesia', to: 'standard', reason: 'enhanced_unavailable' });
      expect(engine.closed).toBe(1);
      expect(downgrades).toEqual([{ from: 'cartesia', to: 'standard', reason: 'enhanced_unavailable' }]);
      expect(captures).toHaveLength(1);
      expect(captures[0].started).toBe(1);
      expect(sock.controls().filter((c) => (c as { type: string }).type === 'start')).toHaveLength(1);
      expect(speaking.at(-1)).toBe(false);
      expect(JSON.parse(sessionStorage.getItem(STORAGE_KEY) as string).engine).toBe('standard');
      // From here on enhanced_speak means nothing.
      sock.deliver({ type: 'enhanced_speak', speaker_id: 'peer-b', segment_id: 's', lang: 'it', text: 'x', voice_id: null });
      expect(enhancedEngines).toHaveLength(1);
      convo.setMuted(true);
      expect(captures[0].muted).toEqual([true]);
    });

    it('a downgrade that finds the engine already closed by its own failure still switches to PCM', async () => {
      const { sock, engine } = await enhancedConnected();
      engine.deps.onFailure({ kind: 'fallback', reason: 'rate_limited' });
      sock.deliver({ type: 'engine_downgraded', from: 'cartesia', to: 'standard', reason: 'enhanced_unavailable' });
      expect(captures).toHaveLength(1);
    });

    it('a muted phone does not start PCM streaming after the downgrade', async () => {
      const { sock, convo } = await enhancedConnected();
      convo.setMuted(true);
      sock.deliver({ type: 'engine_downgraded', from: 'cartesia', to: 'standard', reason: 'enhanced_unavailable' });
      expect(captures).toHaveLength(1);
      expect(captures[0].started).toBe(0);
      expect(sock.controls().filter((c) => (c as { type: string }).type === 'start')).toHaveLength(0);
    });

    it('not_enhanced (a downgrade this phone missed) continues as Standard at once', async () => {
      const { sock, engine, downgrades } = await enhancedConnected();
      engine.deps.onFailure({ kind: 'not_enhanced' });
      expect(captures).toHaveLength(1);
      expect(captures[0].started).toBe(1);
      expect(downgrades).toEqual([{ from: 'cartesia', to: 'standard', reason: 'not_enhanced' }]);
      expect(sock.controls().filter((c) => (c as { type: string }).type === 'enhanced_fallback')).toEqual([]);
    });

    it('a gone seat is a notice, and the live socket decides the rest', async () => {
      const { engine, notices, last } = await enhancedConnected();
      engine.deps.onFailure({ kind: 'gone', code: 'session_ended' });
      expect(notices).toEqual(['enhanced_auth']);
      expect(last().phase).not.toBe('failed');
    });

    it('a failure from an engine that has been replaced is ignored', async () => {
      const { sock, engine } = await enhancedConnected();
      sock.drop();
      engine.deps.onFailure({ kind: 'fallback', reason: 'rate_limited' });
      expect(sock.controls().filter((c) => (c as { type: string }).type === 'enhanced_fallback')).toEqual([]);
    });

    it('a Premium-at-capacity downgrade on a Standard conversation touches nothing in Enhanced terms', async () => {
      const h = await connect({ engine: 'gemini_live_translate' });
      h.sock.deliver({ type: 'engine_downgraded', from: 'gemini_live_translate', to: 'standard', reason: 'premium_at_capacity' });
      expect(captures).toHaveLength(1);
      expect(enhancedEngines).toHaveLength(0);
    });

    it('a token refused when the live socket is gone just waits for the reconnect', async () => {
      const { sock, engine } = await enhancedConnected();
      sock.readyState = 3;
      engine.deps.onFailure({ kind: 'not_enhanced' });
      expect(captures).toHaveLength(0); // PCM attaches on the next open
    });
  });

  it('enhanced_not_active re-opens the meter, but not more than once every few seconds, and never while muted', async () => {
    let t = 0;
    const { sock, convo } = await enhancedConnected({ now: () => t });
    sock.deliver({ type: 'error', code: 'enhanced_not_active' });
    sock.deliver({ type: 'error', code: 'enhanced_not_active' });
    expect(sock.controls().filter((c) => (c as { type: string }).type === 'start')).toHaveLength(1);
    t = 4000;
    sock.deliver({ type: 'error', code: 'enhanced_not_active' });
    expect(sock.controls().filter((c) => (c as { type: string }).type === 'start')).toHaveLength(2);
    convo.setMuted(true);
    t = 9000;
    sock.deliver({ type: 'error', code: 'enhanced_not_active' });
    expect(sock.controls().filter((c) => (c as { type: string }).type === 'start')).toHaveLength(2);
  });

  it('translation_unavailable and text_too_long reach the view as notices', async () => {
    const { sock, notices } = await enhancedConnected();
    sock.deliver({ type: 'error', code: 'translation_unavailable' });
    sock.deliver({ type: 'error', code: 'text_too_long' });
    expect(notices).toEqual(['translation_unavailable', 'text_too_long']);
    sock.deliver({ type: 'error', code: 'fallback_not_allowed' });
    expect(notices.at(-1)).toBe('fallback_not_allowed');
    expect(notices).toHaveLength(3); // a notice, not a fatal end: the conversation goes on
  });

  it('a peer joining does not re-send `start` (the engine owns the meter)', async () => {
    const { sock } = await enhancedConnected();
    sock.deliver({ type: 'peer_joined', ...PARTNER_PEER });
    expect(sock.controls().filter((c) => (c as { type: string }).type === 'start')).toHaveLength(0);
  });

  describe('cleanup', () => {
    it('end() closes the engine', async () => {
      const { convo, engine } = await enhancedConnected();
      convo.end();
      expect(engine.closed).toBeGreaterThanOrEqual(1);
      expect(engine.stopped).toBeGreaterThanOrEqual(1);
    });

    it('release() closes the engine and keeps the stored session', async () => {
      const { convo, engine } = await enhancedConnected();
      convo.release();
      expect(engine.closed).toBeGreaterThanOrEqual(1);
      expect(sessionStorage.getItem(STORAGE_KEY)).not.toBeNull();
    });

    it('a session that ends closes the engine', async () => {
      const { sock, engine } = await enhancedConnected();
      sock.deliver({ type: 'session_state', status: 'ended', reason: 'ended_by_host' });
      expect(engine.closed).toBeGreaterThanOrEqual(1);
    });

    it('a dropped socket closes the engine, and the reconnect builds a fresh one (a new token)', async () => {
      const { sock, engine, probe } = await enhancedConnected();
      sock.drop();
      expect(engine.closed).toBe(1);
      fireTimer();
      await flush();
      expect(probe).toHaveBeenCalled();
      sockets[sockets.length - 1].open();
      await flush();
      expect(enhancedEngines).toHaveLength(2);
      expect(enhancedEngines[1].opened).toBe(1);
    });

    it('a reconnect adopts the tier the server reports now (a fallback this phone missed)', async () => {
      const probe = vi.fn(async (): Promise<Probe> => ({
        ok: true,
        data: { session: { id: 's1', status: 'active', host_lang: 'it', created_at: '', expires_at: '', participants: [], engine: 'standard' }, you: { peer_id: 'peer-a', role: 'host' } },
      }));
      const { sock } = await enhancedConnected({ getSessionImpl: probe });
      sock.drop();
      fireTimer();
      await flush();
      sockets[sockets.length - 1].open();
      await flush();
      expect(enhancedEngines).toHaveLength(1); // no new Enhanced engine
      expect(captures).toHaveLength(1); // PCM instead
    });

    it('an engine whose token failed does not start recognising', async () => {
      const h = enhancedHarness();
      await h.convo.start();
      // The next engine to be made reports a failed open.
      const orig = h.convo as unknown as { opts: { createEnhanced: (d: import('./enhanced').EnhancedDeps) => FakeEnhanced } };
      orig.opts.createEnhanced = (d) => {
        const e = new FakeEnhanced(d);
        e.openResult = false;
        return e;
      };
      sockets[0].open();
      await flush();
      expect(enhancedEngines[0].started).toBe(0);
    });
  });

  it('the stored session keeps the voice-clone flag across the conversation\'s own saves', async () => {
    await enhancedConnected({ voiceClone: true });
    expect(JSON.parse(sessionStorage.getItem(STORAGE_KEY) as string)).toMatchObject({ engine: 'cartesia', voiceClone: true });
  });
});
