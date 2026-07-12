// Webinar presence client tests. A fake WebSocket (injected via `socketFactory`)
// drives the parser, onCount dispatch, reconnect-on-close backoff, and that close()
// stops reconnecting — no real socket / timers required beyond vitest fake timers.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import {
  PresenceClient,
  buildPresenceUrl,
  connectPresence,
  parsePresenceCount,
  type PresenceSocket,
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
