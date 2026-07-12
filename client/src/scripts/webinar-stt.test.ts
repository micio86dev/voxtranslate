// Host STT bridge tests. A fake socket (injected via `socketFactory`) + a fake capture
// (injected via `makeCapture`) drive the URL builder, the open→bridge flow, the
// reconnect-on-drop backoff, and that stop() closes + cancels reconnects — no real
// WebSocket / MediaRecorder / timers required beyond vitest fake timers.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import {
  WebinarSttClient,
  buildSttUrl,
  type SttCapture,
  type SttSocket,
} from './webinar-stt';

// A fake ingest socket capturing its URL and letting a test fire lifecycle events.
class FakeSocket implements SttSocket {
  onopen: ((this: unknown, ev: unknown) => unknown) | null = null;
  onclose: ((this: unknown, ev: unknown) => unknown) | null = null;
  onerror: ((this: unknown, ev: unknown) => unknown) | null = null;
  readyState = 1;
  send = vi.fn();
  close = vi.fn(() => {
    this.readyState = 3;
  });
  constructor(public url: string) {}
  emitOpen() {
    this.onopen?.call(this, {});
  }
  emitClose() {
    this.onclose?.call(this, {});
  }
  emitError() {
    this.onerror?.call(this, {});
  }
}

// A fake AudioCapture recording start/stop calls.
class FakeCapture implements SttCapture {
  start = vi.fn();
  stop = vi.fn();
  constructor(public socket: SttSocket) {}
}

function makeFactory() {
  const sockets: FakeSocket[] = [];
  const factory = (url: string) => {
    const s = new FakeSocket(url);
    sockets.push(s);
    return s;
  };
  return { sockets, factory };
}

function makeCaptureFactory() {
  const captures: FakeCapture[] = [];
  const makeCapture = (socket: SttSocket) => {
    const c = new FakeCapture(socket);
    captures.push(c);
    return c;
  };
  return { captures, makeCapture };
}

const baseOpts = {
  wsBase: 'wss://api.voxtranslate.app',
  webinarId: 'web_123',
  token: 'jwt.abc.def',
};

beforeEach(() => {
  vi.useFakeTimers();
});
afterEach(() => {
  vi.useRealTimers();
});

describe('buildSttUrl', () => {
  it('builds the ingest URL with the token query param', () => {
    expect(buildSttUrl(baseOpts)).toBe(
      'wss://api.voxtranslate.app/api/webinars/web_123/stt?token=jwt.abc.def',
    );
  });
  it('url-encodes the webinar id and the token', () => {
    expect(
      buildSttUrl({ wsBase: 'wss://x.dev', webinarId: 'a b', token: 'a/b+c=' }),
    ).toBe('wss://x.dev/api/webinars/a%20b/stt?token=a%2Fb%2Bc%3D');
  });
  it('strips a trailing slash from the ws base', () => {
    expect(buildSttUrl({ ...baseOpts, wsBase: 'wss://x.dev/' })).toContain(
      'wss://x.dev/api/webinars/',
    );
  });
});

describe('WebinarSttClient', () => {
  it('opens the ingest socket with the built URL and bridges the mic on open', () => {
    const { sockets, factory } = makeFactory();
    const { captures, makeCapture } = makeCaptureFactory();
    const c = new WebinarSttClient({ ...baseOpts, makeCapture, socketFactory: factory });
    c.start();
    expect(sockets).toHaveLength(1);
    expect(sockets[0].url).toBe(
      'wss://api.voxtranslate.app/api/webinars/web_123/stt?token=jwt.abc.def',
    );
    // Capture is created immediately (bound to the socket) but only STARTS on open.
    expect(captures).toHaveLength(1);
    expect(captures[0].start).not.toHaveBeenCalled();
    sockets[0].emitOpen();
    expect(captures[0].start).toHaveBeenCalledTimes(1);
    expect(captures[0].socket).toBe(sockets[0]);
    c.stop();
  });

  it('start() is idempotent — a second call opens no extra socket', () => {
    const { sockets, factory } = makeFactory();
    const { makeCapture } = makeCaptureFactory();
    const c = new WebinarSttClient({ ...baseOpts, makeCapture, socketFactory: factory });
    c.start();
    c.start();
    expect(sockets).toHaveLength(1);
    c.stop();
  });

  it('reconnects with backoff after a silent drop and re-bridges a fresh capture', () => {
    const { sockets, factory } = makeFactory();
    const { captures, makeCapture } = makeCaptureFactory();
    const c = new WebinarSttClient({
      ...baseOpts,
      makeCapture,
      socketFactory: factory,
      reconnectBaseMs: 1_000,
    });
    c.start();
    sockets[0].emitOpen();
    expect(captures[0].start).toHaveBeenCalledTimes(1);

    // Silent drop while the broadcast is still live → stop the dead capture, reconnect.
    sockets[0].emitClose();
    expect(captures[0].stop).toHaveBeenCalledTimes(1);
    expect(sockets).toHaveLength(1); // not yet — waits the backoff
    vi.advanceTimersByTime(1_000);
    expect(sockets).toHaveLength(2); // reconnected
    expect(captures).toHaveLength(2); // a FRESH capture for the new socket
    expect(captures[1].socket).toBe(sockets[1]);

    // A second drop WITHOUT an open in between (backoff not reset) doubles to 2s.
    sockets[1].emitClose();
    vi.advanceTimersByTime(1_000);
    expect(sockets).toHaveLength(2); // not yet
    vi.advanceTimersByTime(1_000);
    expect(sockets).toHaveLength(3); // reconnected after 2s total
    sockets[2].emitOpen();
    expect(captures[2].start).toHaveBeenCalledTimes(1);
    c.stop();
  });

  it('resets the backoff after a successful open', () => {
    const { sockets, factory } = makeFactory();
    const { makeCapture } = makeCaptureFactory();
    const c = new WebinarSttClient({
      ...baseOpts,
      makeCapture,
      socketFactory: factory,
      reconnectBaseMs: 1_000,
    });
    c.start();
    sockets[0].emitClose(); // drop before ever opening → reconnect at 1s
    vi.advanceTimersByTime(1_000);
    expect(sockets).toHaveLength(2);
    sockets[1].emitOpen(); // a successful open resets the attempt counter
    sockets[1].emitClose();
    vi.advanceTimersByTime(1_000);
    expect(sockets).toHaveLength(3); // again only the base delay, not 2s
    c.stop();
  });

  it('an error closes the socket which then schedules a reconnect', () => {
    const { sockets, factory } = makeFactory();
    const { makeCapture } = makeCaptureFactory();
    const c = new WebinarSttClient({
      ...baseOpts,
      makeCapture,
      socketFactory: factory,
      reconnectBaseMs: 1_000,
    });
    c.start();
    sockets[0].emitError();
    expect(sockets[0].close).toHaveBeenCalled();
    // The fake's close() doesn't auto-fire onclose — drive it to model a real socket.
    sockets[0].emitClose();
    vi.advanceTimersByTime(1_000);
    expect(sockets).toHaveLength(2);
    c.stop();
  });

  it('retries when constructing the socket throws (offline at connect)', () => {
    let calls = 0;
    const good = new FakeSocket('ok');
    const factory = () => {
      calls += 1;
      if (calls === 1) throw new Error('offline');
      return good;
    };
    const { makeCapture } = makeCaptureFactory();
    const c = new WebinarSttClient({
      ...baseOpts,
      makeCapture,
      socketFactory: factory,
      reconnectBaseMs: 1_000,
    });
    c.start();
    expect(calls).toBe(1); // threw on the first attempt
    vi.advanceTimersByTime(1_000);
    expect(calls).toBe(2); // retried on backoff
    c.stop();
  });

  it('stop() closes the socket, stops the capture, and cancels a pending reconnect', () => {
    const { sockets, factory } = makeFactory();
    const { captures, makeCapture } = makeCaptureFactory();
    const c = new WebinarSttClient({ ...baseOpts, makeCapture, socketFactory: factory });
    c.start();
    sockets[0].emitOpen();
    c.stop();
    expect(captures[0].stop).toHaveBeenCalled();
    expect(sockets[0].close).toHaveBeenCalled();
    // A late onclose after stop() must NOT reconnect (handlers were detached).
    sockets[0].emitClose();
    vi.advanceTimersByTime(60_000);
    expect(sockets).toHaveLength(1);
  });

  it('stop() cancels a reconnect scheduled by an earlier drop', () => {
    const { sockets, factory } = makeFactory();
    const { makeCapture } = makeCaptureFactory();
    const c = new WebinarSttClient({
      ...baseOpts,
      makeCapture,
      socketFactory: factory,
      reconnectBaseMs: 1_000,
    });
    c.start();
    sockets[0].emitClose(); // schedules a reconnect
    c.stop(); // must cancel it
    vi.advanceTimersByTime(60_000);
    expect(sockets).toHaveLength(1); // never reconnected
  });
});
