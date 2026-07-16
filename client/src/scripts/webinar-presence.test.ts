// Webinar presence client tests. A fake WebSocket (injected via `socketFactory`)
// drives the parser, onCount dispatch, reconnect-on-close backoff, and that close()
// stops reconnecting — no real socket / timers required beyond vitest fake timers.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import {
  PresenceClient,
  buildPresenceUrl,
  connectPresence,
  parseChatFrame,
  parsePresenceCount,
  parseSubtitleFrame,
  type ChatEvent,
  type PresenceSocket,
  type SubtitleEvent,
} from './webinar-presence';

// A fake WebSocket capturing its URL and letting a test fire lifecycle events.
class FakeSocket implements PresenceSocket {
  onopen: ((this: unknown, ev: unknown) => unknown) | null = null;
  onmessage: ((this: unknown, ev: { data: unknown }) => unknown) | null = null;
  onclose: ((this: unknown, ev: unknown) => unknown) | null = null;
  onerror: ((this: unknown, ev: unknown) => unknown) | null = null;
  closed = false;
  close = vi.fn(() => {
    this.closed = true;
  });
  constructor(public url: string) {}

  // Test helpers to simulate server/network events.
  emitOpen() {
    this.onopen?.call(this, {});
  }
  emitMessage(data: unknown) {
    this.onmessage?.call(this, { data });
  }
  emitClose() {
    this.onclose?.call(this, {});
  }
  emitError() {
    this.onerror?.call(this, {});
  }
}

// Records every socket the client constructs so a test can inspect reconnects.
function makeFactory() {
  const sockets: FakeSocket[] = [];
  const factory = (url: string) => {
    const s = new FakeSocket(url);
    sockets.push(s);
    return s;
  };
  return { sockets, factory };
}

const baseOpts = {
  wsBase: 'wss://api.voxtranslate.app',
  code: 'ab12cd',
  guestId: 'guest-1',
  host: false,
};

beforeEach(() => {
  vi.useFakeTimers();
});
afterEach(() => {
  vi.useRealTimers();
});

describe('parsePresenceCount', () => {
  it('returns the count for a valid count frame', () => {
    expect(parsePresenceCount('{"type":"count","count":7}')).toBe(7);
    expect(parsePresenceCount('{"type":"count","count":0}')).toBe(0);
  });
  it('returns null for a non-count / unknown frame type', () => {
    expect(parsePresenceCount('{"type":"hello"}')).toBeNull();
    expect(parsePresenceCount('{"type":"count"}')).toBeNull(); // no count field
  });
  it('returns null for a non-numeric or non-finite count', () => {
    expect(parsePresenceCount('{"type":"count","count":"3"}')).toBeNull();
    expect(parsePresenceCount('{"type":"count","count":null}')).toBeNull();
  });
  it('returns null for invalid JSON or non-object payloads', () => {
    expect(parsePresenceCount('not json')).toBeNull();
    expect(parsePresenceCount('42')).toBeNull();
    expect(parsePresenceCount('null')).toBeNull();
  });
});

describe('parseSubtitleFrame', () => {
  it('parses a final frame with the source + translations map', () => {
    const ev = parseSubtitleFrame(
      '{"type":"subtitle","kind":"final","original":"hola","lang":"es","translations":{"es":"hola","en":"hi"}}',
    );
    expect(ev).toEqual<SubtitleEvent>({
      kind: 'final',
      original: 'hola',
      lang: 'es',
      translations: { es: 'hola', en: 'hi' },
    });
  });
  it('parses an interim frame carrying only the source text', () => {
    const ev = parseSubtitleFrame(
      '{"type":"subtitle","kind":"interim","text":"hol","lang":"es"}',
    );
    expect(ev).toEqual<SubtitleEvent>({ kind: 'interim', text: 'hol', lang: 'es' });
  });
  it('drops non-string values from the translations map', () => {
    const ev = parseSubtitleFrame(
      '{"type":"subtitle","kind":"final","original":"ok","lang":"en","translations":{"en":"ok","xx":5}}',
    );
    expect(ev).toEqual<SubtitleEvent>({
      kind: 'final',
      original: 'ok',
      lang: 'en',
      translations: { en: 'ok' },
    });
  });
  it('returns null for the wrong frame type or a missing kind', () => {
    expect(parseSubtitleFrame('{"type":"count","count":3}')).toBeNull();
    expect(parseSubtitleFrame('{"type":"subtitle","lang":"en"}')).toBeNull(); // no kind
    expect(parseSubtitleFrame('{"type":"subtitle","kind":"other","lang":"en"}')).toBeNull();
  });
  it('returns null when required fields are missing or mistyped', () => {
    // final without original / translations
    expect(
      parseSubtitleFrame('{"type":"subtitle","kind":"final","lang":"en","translations":{}}'),
    ).toBeNull();
    expect(
      parseSubtitleFrame('{"type":"subtitle","kind":"final","original":"x","lang":"en"}'),
    ).toBeNull();
    // interim without text, and any frame without a string lang
    expect(parseSubtitleFrame('{"type":"subtitle","kind":"interim","lang":"en"}')).toBeNull();
    expect(
      parseSubtitleFrame('{"type":"subtitle","kind":"interim","text":"x","lang":5}'),
    ).toBeNull();
  });
  it('returns null for garbage / non-object JSON', () => {
    expect(parseSubtitleFrame('not json')).toBeNull();
    expect(parseSubtitleFrame('42')).toBeNull();
    expect(parseSubtitleFrame('null')).toBeNull();
  });
});

describe('parseChatFrame', () => {
  const valid =
    '{"type":"chat","id":"m1","sender_kind":"guest","display_name":"Ada","original":"hola","lang":"es","translations":{"es":"hola","en":"hi"},"created_at":"2026-07-12T10:00:00Z"}';
  it('parses a valid chat frame with the translations map', () => {
    expect(parseChatFrame(valid)).toEqual<ChatEvent>({
      id: 'm1',
      sender_kind: 'guest',
      display_name: 'Ada',
      original: 'hola',
      lang: 'es',
      translations: { es: 'hola', en: 'hi' },
      created_at: '2026-07-12T10:00:00Z',
    });
  });
  it('parses the avatar_url and file attachment carried on the frame', () => {
    const frame =
      '{"type":"chat","id":"m3","sender_kind":"guest","display_name":"Ada","original":"see file","lang":"es","translations":{"es":"see file"},"created_at":"t","avatar_url":"https://cdn/x=s96","attachment":{"url":"https://files/x.pdf","name":"x.pdf","content_type":"application/pdf","size":2048}}';
    const ev = parseChatFrame(frame);
    expect(ev?.avatar_url).toBe('https://cdn/x=s96');
    expect(ev?.attachment).toEqual({
      url: 'https://files/x.pdf',
      name: 'x.pdf',
      content_type: 'application/pdf',
      size: 2048,
    });
  });
  it('leaves attachment undefined when the frame carries a malformed one', () => {
    const frame =
      '{"type":"chat","id":"m4","sender_kind":"guest","display_name":"A","original":"x","lang":"en","translations":{},"created_at":"t","attachment":{"url":"u"}}';
    expect(parseChatFrame(frame)?.attachment).toBeUndefined();
  });
  it('accepts the host sender_kind and drops non-string translation values', () => {
    const ev = parseChatFrame(
      '{"type":"chat","id":"m2","sender_kind":"host","display_name":"H","original":"hi","lang":"en","translations":{"en":"hi","xx":5},"created_at":"t"}',
    );
    expect(ev).toEqual<ChatEvent>({
      id: 'm2',
      sender_kind: 'host',
      display_name: 'H',
      original: 'hi',
      lang: 'en',
      translations: { en: 'hi' },
      created_at: 't',
    });
  });
  it('returns null for the wrong frame type', () => {
    expect(parseChatFrame('{"type":"count","count":3}')).toBeNull();
    expect(
      parseChatFrame(
        '{"type":"subtitle","kind":"final","original":"x","lang":"en","translations":{}}',
      ),
    ).toBeNull();
  });
  it('returns null when a required field is missing or mistyped', () => {
    // bad sender_kind
    expect(
      parseChatFrame(
        '{"type":"chat","id":"m","sender_kind":"admin","display_name":"A","original":"x","lang":"en","translations":{},"created_at":"t"}',
      ),
    ).toBeNull();
    // missing id
    expect(
      parseChatFrame(
        '{"type":"chat","sender_kind":"guest","display_name":"A","original":"x","lang":"en","translations":{},"created_at":"t"}',
      ),
    ).toBeNull();
    // missing translations object
    expect(
      parseChatFrame(
        '{"type":"chat","id":"m","sender_kind":"guest","display_name":"A","original":"x","lang":"en","created_at":"t"}',
      ),
    ).toBeNull();
    // non-string lang
    expect(
      parseChatFrame(
        '{"type":"chat","id":"m","sender_kind":"guest","display_name":"A","original":"x","lang":5,"translations":{},"created_at":"t"}',
      ),
    ).toBeNull();
  });
  it('returns null for garbage / non-object JSON', () => {
    expect(parseChatFrame('not json')).toBeNull();
    expect(parseChatFrame('42')).toBeNull();
    expect(parseChatFrame('null')).toBeNull();
  });
});

describe('buildPresenceUrl', () => {
  it('builds the participant URL with guest_id + host=false', () => {
    expect(buildPresenceUrl({ ...baseOpts })).toBe(
      'wss://api.voxtranslate.app/api/w/ab12cd/presence?guest_id=guest-1&host=false',
    );
  });
  it('adds lang only when provided and marks the host', () => {
    expect(
      buildPresenceUrl({ ...baseOpts, host: true, lang: 'it' }),
    ).toBe(
      'wss://api.voxtranslate.app/api/w/ab12cd/presence?guest_id=guest-1&host=true&lang=it',
    );
  });
  it('strips a trailing slash from the ws base', () => {
    expect(buildPresenceUrl({ ...baseOpts, wsBase: 'wss://x.dev/' })).toContain(
      'wss://x.dev/api/w/',
    );
  });
  it('sends the token when provided, regardless of host flag', () => {
    // Hosts and authenticated viewers (members-only rooms) both send their JWT.
    expect(
      buildPresenceUrl({ ...baseOpts, host: true, token: 'jwt-123' }),
    ).toContain('&token=jwt-123');
    expect(
      buildPresenceUrl({ ...baseOpts, host: false, token: 'jwt-123' }),
    ).toContain('token=jwt-123');
    // No token param when none is provided.
    expect(
      buildPresenceUrl({ ...baseOpts, host: false }),
    ).not.toContain('token=');
  });
});

describe('PresenceClient', () => {
  it('opens the socket with the built URL on construction', () => {
    const { sockets, factory } = makeFactory();
    const client = new PresenceClient({
      ...baseOpts,
      onCount: () => {},
      socketFactory: factory,
    });
    expect(sockets).toHaveLength(1);
    expect(sockets[0].url).toBe(
      'wss://api.voxtranslate.app/api/w/ab12cd/presence?guest_id=guest-1&host=false',
    );
    client.close();
  });

  it('dispatches onCount for each valid frame and ignores others', () => {
    const { sockets, factory } = makeFactory();
    const counts: number[] = [];
    const client = new PresenceClient({
      ...baseOpts,
      onCount: (n) => counts.push(n),
      socketFactory: factory,
    });
    const s = sockets[0];
    s.emitMessage('{"type":"count","count":3}');
    s.emitMessage('{"type":"other"}'); // ignored
    s.emitMessage('garbage'); // ignored
    s.emitMessage({ notString: true }); // binary/non-string — ignored
    s.emitMessage('{"type":"count","count":5}');
    expect(counts).toEqual([3, 5]);
    client.close();
  });

  it('dispatches onSubtitle for subtitle frames and never confuses them with counts', () => {
    const { sockets, factory } = makeFactory();
    const counts: number[] = [];
    const subs: SubtitleEvent[] = [];
    const client = new PresenceClient({
      ...baseOpts,
      onCount: (n) => counts.push(n),
      onSubtitle: (s) => subs.push(s),
      socketFactory: factory,
    });
    const s = sockets[0];
    s.emitMessage('{"type":"count","count":2}'); // count only
    s.emitMessage(
      '{"type":"subtitle","kind":"final","original":"ciao","lang":"it","translations":{"it":"ciao","en":"hi"}}',
    );
    s.emitMessage('{"type":"subtitle","kind":"interim","text":"cia","lang":"it"}');
    s.emitMessage('garbage'); // ignored by both
    expect(counts).toEqual([2]);
    expect(subs).toEqual<SubtitleEvent[]>([
      { kind: 'final', original: 'ciao', lang: 'it', translations: { it: 'ciao', en: 'hi' } },
      { kind: 'interim', text: 'cia', lang: 'it' },
    ]);
    client.close();
  });

  it('dispatches onChat for chat frames and never confuses them with counts/subtitles', () => {
    const { sockets, factory } = makeFactory();
    const counts: number[] = [];
    const subs: SubtitleEvent[] = [];
    const chats: ChatEvent[] = [];
    const client = new PresenceClient({
      ...baseOpts,
      onCount: (n) => counts.push(n),
      onSubtitle: (s) => subs.push(s),
      onChat: (c) => chats.push(c),
      socketFactory: factory,
    });
    const s = sockets[0];
    s.emitMessage('{"type":"count","count":4}'); // count only
    s.emitMessage(
      '{"type":"subtitle","kind":"final","original":"ciao","lang":"it","translations":{"it":"ciao"}}',
    ); // subtitle only
    s.emitMessage(
      '{"type":"chat","id":"m1","sender_kind":"guest","display_name":"Ada","original":"hola","lang":"es","translations":{"es":"hola","en":"hi"},"created_at":"t"}',
    );
    s.emitMessage('garbage'); // ignored by all
    expect(counts).toEqual([4]);
    expect(subs).toHaveLength(1);
    expect(chats).toEqual<ChatEvent[]>([
      {
        id: 'm1',
        sender_kind: 'guest',
        display_name: 'Ada',
        original: 'hola',
        lang: 'es',
        translations: { es: 'hola', en: 'hi' },
        created_at: 't',
      },
    ]);
    client.close();
  });

  it('reconnects with backoff after an unexpected close', () => {
    const { sockets, factory } = makeFactory();
    const client = new PresenceClient({
      ...baseOpts,
      onCount: () => {},
      socketFactory: factory,
      reconnectBaseMs: 1_000,
    });
    expect(sockets).toHaveLength(1);

    // Socket drops → a reconnect is scheduled at base delay.
    sockets[0].emitClose();
    expect(sockets).toHaveLength(1);
    vi.advanceTimersByTime(1_000);
    expect(sockets).toHaveLength(2); // reconnected

    // A second drop (without an open) doubles the backoff to 2s.
    sockets[1].emitClose();
    vi.advanceTimersByTime(1_000);
    expect(sockets).toHaveLength(2); // not yet
    vi.advanceTimersByTime(1_000);
    expect(sockets).toHaveLength(3); // reconnected after 2s total

    client.close();
  });

  it('resets backoff after a successful open', () => {
    const { sockets, factory } = makeFactory();
    const client = new PresenceClient({
      ...baseOpts,
      onCount: () => {},
      socketFactory: factory,
      reconnectBaseMs: 1_000,
    });
    // First drop → reconnect at 1s.
    sockets[0].emitClose();
    vi.advanceTimersByTime(1_000);
    expect(sockets).toHaveLength(2);
    // The reconnect opens successfully, resetting the attempt counter.
    sockets[1].emitOpen();
    // Next drop should again wait only the base delay (not 2s).
    sockets[1].emitClose();
    vi.advanceTimersByTime(1_000);
    expect(sockets).toHaveLength(3);
    client.close();
  });

  it('close() stops any pending reconnect', () => {
    const { sockets, factory } = makeFactory();
    const client = new PresenceClient({
      ...baseOpts,
      onCount: () => {},
      socketFactory: factory,
      reconnectBaseMs: 1_000,
    });
    sockets[0].emitClose(); // schedules a reconnect
    client.close(); // must cancel it
    vi.advanceTimersByTime(60_000);
    expect(sockets).toHaveLength(1); // never reconnected
  });

  it('close() closes the live socket and no reconnect follows its close', () => {
    const { sockets, factory } = makeFactory();
    const client = new PresenceClient({
      ...baseOpts,
      onCount: () => {},
      socketFactory: factory,
    });
    const s = sockets[0];
    client.close();
    expect(s.close).toHaveBeenCalled();
    // The close() detached handlers, so even a late onclose can't reconnect.
    s.emitClose();
    vi.advanceTimersByTime(60_000);
    expect(sockets).toHaveLength(1);
  });

  it('an error triggers a close which schedules a reconnect', () => {
    const { sockets, factory } = makeFactory();
    const client = new PresenceClient({
      ...baseOpts,
      onCount: () => {},
      socketFactory: factory,
      reconnectBaseMs: 1_000,
    });
    // onerror closes the socket; the fake's close() does not auto-fire onclose,
    // so drive the close event to model a real socket's error→close sequence.
    sockets[0].emitError();
    expect(sockets[0].close).toHaveBeenCalled();
    sockets[0].emitClose();
    vi.advanceTimersByTime(1_000);
    expect(sockets).toHaveLength(2);
    client.close();
  });

  it('retries when constructing the socket throws', () => {
    let calls = 0;
    const good = new FakeSocket('ok');
    const factory = () => {
      calls += 1;
      if (calls === 1) throw new Error('offline');
      return good;
    };
    const client = new PresenceClient({
      ...baseOpts,
      onCount: () => {},
      socketFactory: factory,
      reconnectBaseMs: 1_000,
    });
    expect(calls).toBe(1); // threw on first attempt
    vi.advanceTimersByTime(1_000);
    expect(calls).toBe(2); // retried
    client.close();
  });

  it('connectPresence returns a working client', () => {
    const { sockets, factory } = makeFactory();
    const client = connectPresence({
      ...baseOpts,
      onCount: () => {},
      socketFactory: factory,
    });
    expect(client).toBeInstanceOf(PresenceClient);
    expect(sockets).toHaveLength(1);
    client.close();
  });
});
