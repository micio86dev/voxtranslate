// @vitest-environment jsdom
import { beforeEach, describe, expect, it, vi } from 'vitest';
import {
  STORAGE_KEY,
  HEADPHONES_KEY,
  clearStoredSession,
  loadHeadphones,
  loadStoredSession,
  saveHeadphones,
  saveStoredSession,
  type StoredSession,
} from './session-store';

const S: StoredSession = {
  sessionId: 's1',
  token: 'tok',
  role: 'host',
  myLang: 'it',
  myPeerId: 'p1',
  wsPath: '/ws/live',
  inviteUrl: 'https://x/lc/C',
};

beforeEach(() => {
  sessionStorage.clear();
  localStorage.clear();
  vi.restoreAllMocks();
});

describe('stored session (sessionStorage, key vox.live.session)', () => {
  it('uses the documented key and round-trips', () => {
    expect(STORAGE_KEY).toBe('vox.live.session');
    saveStoredSession(S);
    expect(sessionStorage.getItem('vox.live.session')).toBeTruthy();
    expect(loadStoredSession()).toEqual(S);
  });

  it('clear removes it', () => {
    saveStoredSession(S);
    clearStoredSession();
    expect(loadStoredSession()).toBeNull();
  });

  it('rejects malformed or incomplete payloads instead of trusting them', () => {
    sessionStorage.setItem(STORAGE_KEY, '{nope');
    expect(loadStoredSession()).toBeNull();
    sessionStorage.setItem(STORAGE_KEY, JSON.stringify({ sessionId: 's', token: 't' }));
    expect(loadStoredSession()).toBeNull();
    sessionStorage.setItem(STORAGE_KEY, JSON.stringify({ ...S, role: 'admin' }));
    expect(loadStoredSession()).toBeNull();
  });

  it('defaults the websocket path and drops a non-string invite URL', () => {
    sessionStorage.setItem(STORAGE_KEY, JSON.stringify({ ...S, wsPath: undefined, inviteUrl: 5 }));
    const got = loadStoredSession();
    expect(got?.wsPath).toBe('/ws/live');
    expect(got?.inviteUrl).toBeUndefined();
  });

  it('never throws when storage is unavailable', () => {
    vi.spyOn(Storage.prototype, 'setItem').mockImplementation(() => {
      throw new Error('quota');
    });
    vi.spyOn(Storage.prototype, 'getItem').mockImplementation(() => {
      throw new Error('blocked');
    });
    vi.spyOn(Storage.prototype, 'removeItem').mockImplementation(() => {
      throw new Error('blocked');
    });
    expect(() => saveStoredSession(S)).not.toThrow();
    expect(loadStoredSession()).toBeNull();
    expect(() => clearStoredSession()).not.toThrow();
    expect(() => saveHeadphones(true)).not.toThrow();
    expect(loadHeadphones()).toBe(false);
  });
});

describe('headphones preference (localStorage)', () => {
  it('defaults off and persists', () => {
    expect(loadHeadphones()).toBe(false);
    saveHeadphones(true);
    expect(localStorage.getItem(HEADPHONES_KEY)).toBe('1');
    expect(loadHeadphones()).toBe(true);
    saveHeadphones(false);
    expect(loadHeadphones()).toBe(false);
  });
});
