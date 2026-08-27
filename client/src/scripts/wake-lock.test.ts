// @vitest-environment jsdom
import { describe, it, expect, beforeEach, vi } from 'vitest';
import {
  requestWakeLock,
  releaseWakeLock,
  wakeLockHeld,
  wakeLockSupported,
  type WakeLockLike,
} from './wake-lock';

class FakeSentinel implements WakeLockLike {
  released = false;
  release = vi.fn(async (): Promise<void> => {
    this.released = true;
  });
}

let sentinels: FakeSentinel[] = [];
let requestImpl: () => Promise<WakeLockLike>;

function installApi(): void {
  Object.defineProperty(navigator, 'wakeLock', {
    configurable: true,
    value: { request: vi.fn(() => requestImpl()) },
  });
}

function removeApi(): void {
  Object.defineProperty(navigator, 'wakeLock', { configurable: true, value: undefined });
}

function setVisibility(state: 'visible' | 'hidden'): void {
  Object.defineProperty(document, 'visibilityState', { configurable: true, value: state });
  document.dispatchEvent(new Event('visibilitychange'));
}

beforeEach(async () => {
  await releaseWakeLock();
  sentinels = [];
  requestImpl = async () => {
    const s = new FakeSentinel();
    sentinels.push(s);
    return s;
  };
  installApi();
  Object.defineProperty(document, 'visibilityState', { configurable: true, value: 'visible' });
});

describe('wake lock', () => {
  it('acquires and releases', async () => {
    await requestWakeLock();
    expect(wakeLockHeld()).toBe(true);

    await releaseWakeLock();
    expect(wakeLockHeld()).toBe(false);
    expect(sentinels[0].release).toHaveBeenCalled();
  });

  it('does not stack locks on repeated requests', async () => {
    await requestWakeLock();
    await requestWakeLock();
    await requestWakeLock();
    expect(sentinels.length).toBe(1);
  });

  it('re-acquires when the page becomes visible again', async () => {
    // The browser drops the lock whenever the page is hidden and does NOT restore it.
    // Without this the screen stops staying awake after the first swiped-away
    // notification — and the failure is invisible until the phone locks mid-sentence.
    await requestWakeLock();
    sentinels[0].released = true; // what the browser does on hide

    setVisibility('hidden');
    setVisibility('visible');
    await Promise.resolve();
    await Promise.resolve();

    expect(sentinels.length).toBe(2);
    expect(wakeLockHeld()).toBe(true);
    await releaseWakeLock();
  });

  it('stops re-acquiring once released', async () => {
    await requestWakeLock();
    await releaseWakeLock();
    setVisibility('hidden');
    setVisibility('visible');
    await Promise.resolve();
    expect(sentinels.length).toBe(1);
  });

  it('treats a rejected request as routine', async () => {
    // request() rejects when the document is hidden or the OS refuses. That is normal,
    // not an error to surface — the conversation continues, the screen may dim.
    requestImpl = () => Promise.reject(new Error('hidden'));
    await expect(requestWakeLock()).resolves.toBeUndefined();
    expect(wakeLockHeld()).toBe(false);
    await releaseWakeLock();
  });

  it('releases a lock that arrived after the caller gave up', async () => {
    // Tap Start then End immediately: the in-flight request must not leave the screen
    // pinned awake for the rest of the session.
    let resolve!: (s: WakeLockLike) => void;
    requestImpl = () => new Promise<WakeLockLike>((r) => (resolve = r));
    const pending = requestWakeLock();
    await releaseWakeLock();
    const late = new FakeSentinel();
    resolve(late);
    await pending;
    expect(late.release).toHaveBeenCalled();
    expect(wakeLockHeld()).toBe(false);
  });

  it('tolerates a release that throws', async () => {
    await requestWakeLock();
    sentinels[0].release.mockRejectedValueOnce(new Error('gone'));
    await expect(releaseWakeLock()).resolves.toBeUndefined();
    expect(wakeLockHeld()).toBe(false);
  });

  it('is a no-op where the API does not exist', async () => {
    // Older iOS Safari and some Android forks. Absence is normal; the feature degrades
    // to "the screen may dim", never to an error.
    removeApi();
    expect(wakeLockSupported()).toBe(false);
    await expect(requestWakeLock()).resolves.toBeUndefined();
    expect(wakeLockHeld()).toBe(false);
    await expect(releaseWakeLock()).resolves.toBeUndefined();
  });

  it('reports support when the API is present', () => {
    expect(wakeLockSupported()).toBe(true);
  });
});
