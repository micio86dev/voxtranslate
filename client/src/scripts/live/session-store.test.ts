// @vitest-environment jsdom
import { beforeEach, describe, expect, it, vi } from 'vitest';
import {
  STORAGE_KEY,
  HEADPHONES_KEY,
  DESK_KEY,
  STAFF_LANG_KEY,
  clearDeskInfo,
  clearStoredSession,
  loadDeskInfo,
  loadHeadphones,
  loadStaffLang,
  loadTierPref,
  loadStoredSession,
  saveDeskInfo,
  saveHeadphones,
  saveStaffLang,
  saveTierPref,
  TIER_PREF_KEY,
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

describe('stored session: the tier it runs on', () => {
  it('round-trips the engine id and treats an absent or malformed one as standard (absent)', () => {
    saveStoredSession({ ...S, engine: 'gemini_live_translate' });
    expect(loadStoredSession()?.engine).toBe('gemini_live_translate');
    for (const bad of ['', 7, null, {}]) {
      sessionStorage.setItem(STORAGE_KEY, JSON.stringify({ ...S, engine: bad }));
      expect(loadStoredSession()).not.toHaveProperty('engine');
    }
    // A session stored before tiers existed has none: still loads.
    saveStoredSession(S);
    expect(loadStoredSession()).toMatchObject({ sessionId: 's1' });
    expect(loadStoredSession()).not.toHaveProperty('engine');
  });
});

describe('remembered tier choice', () => {
  it('round-trips in localStorage', () => {
    expect(loadTierPref()).toBeNull();
    saveTierPref('gemini_live_translate');
    expect(localStorage.getItem(TIER_PREF_KEY)).toBe('gemini_live_translate');
    expect(loadTierPref()).toBe('gemini_live_translate');
  });

  it('survives storage that throws', () => {
    vi.spyOn(Storage.prototype, 'getItem').mockImplementation(() => {
      throw new Error('blocked');
    });
    vi.spyOn(Storage.prototype, 'setItem').mockImplementation(() => {
      throw new Error('blocked');
    });
    expect(loadTierPref()).toBeNull();
    expect(() => saveTierPref('x')).not.toThrow();
  });
});

describe('stored session: how many guests the invitation admits', () => {
  it('round-trips a valid size and drops anything else', () => {
    saveStoredSession({ ...S, maxGuests: 3 });
    expect(loadStoredSession()?.maxGuests).toBe(3);
    for (const bad of [0, 4, 1.5, '2', null]) {
      sessionStorage.setItem(STORAGE_KEY, JSON.stringify({ ...S, maxGuests: bad }));
      expect(loadStoredSession()).not.toHaveProperty('maxGuests');
    }
    // A session stored before multi-guest has no size at all.
    saveStoredSession(S);
    expect(loadStoredSession()).not.toHaveProperty('maxGuests');
  });
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

describe('the staff seat', () => {
  it('is a valid stored role, so a reload can rejoin the conversation', () => {
    const staff: StoredSession = { ...S, role: 'staff' };
    delete staff.inviteUrl;
    saveStoredSession(staff);
    expect(loadStoredSession()).toEqual(staff);
  });
});

describe('desk info (sessionStorage, key vox.live.desk)', () => {
  const D = { sessionId: 's1', handle: 'h1', name: 'Front desk', location: 'Terminal 2', until: 1_700_000_060_000 };

  it('round-trips for the same session', () => {
    expect(DESK_KEY).toBe('vox.live.desk');
    saveDeskInfo(D);
    expect(loadDeskInfo('s1')).toEqual(D);
  });

  it('is never returned for another session: a new scan must not inherit the old countdown', () => {
    saveDeskInfo(D);
    expect(loadDeskInfo('s2')).toBeNull();
  });

  it('keeps a desk without a location and rejects malformed payloads', () => {
    saveDeskInfo({ ...D, location: null });
    expect(loadDeskInfo('s1')).toEqual({ ...D, location: null });
    sessionStorage.setItem(DESK_KEY, '{nope');
    expect(loadDeskInfo('s1')).toBeNull();
    sessionStorage.setItem(DESK_KEY, JSON.stringify({ sessionId: 's1', handle: 'h1', name: 5, until: 'x' }));
    expect(loadDeskInfo('s1')).toBeNull();
  });

  it('clear removes it, and nothing throws when storage is unavailable', () => {
    saveDeskInfo(D);
    clearDeskInfo();
    expect(loadDeskInfo('s1')).toBeNull();
    vi.spyOn(Storage.prototype, 'setItem').mockImplementation(() => {
      throw new Error('quota');
    });
    vi.spyOn(Storage.prototype, 'getItem').mockImplementation(() => {
      throw new Error('blocked');
    });
    vi.spyOn(Storage.prototype, 'removeItem').mockImplementation(() => {
      throw new Error('blocked');
    });
    expect(() => saveDeskInfo(D)).not.toThrow();
    expect(loadDeskInfo('s1')).toBeNull();
    expect(() => clearDeskInfo()).not.toThrow();
  });
});

describe('staff language per desk (localStorage)', () => {
  it('remembers the language chosen at each desk separately', () => {
    expect(STAFF_LANG_KEY).toBe('vox.live.staffLang');
    expect(loadStaffLang('d1')).toBeNull();
    saveStaffLang('d1', 'it');
    saveStaffLang('d2', 'fr');
    expect(loadStaffLang('d1')).toBe('it');
    expect(loadStaffLang('d2')).toBe('fr');
  });

  it('ignores a corrupt value and never throws without storage', () => {
    localStorage.setItem(STAFF_LANG_KEY, '[1,2');
    expect(loadStaffLang('d1')).toBeNull();
    saveStaffLang('d1', 'de');
    expect(loadStaffLang('d1')).toBe('de');
    vi.spyOn(Storage.prototype, 'setItem').mockImplementation(() => {
      throw new Error('quota');
    });
    vi.spyOn(Storage.prototype, 'getItem').mockImplementation(() => {
      throw new Error('blocked');
    });
    expect(() => saveStaffLang('d1', 'it')).not.toThrow();
    expect(loadStaffLang('d1')).toBeNull();
  });
});

describe('stored session: the host voice clone flag', () => {
  it('round-trips true, and an absent or non-true value is simply absent', () => {
    const base = { sessionId: 's', token: 't', role: 'guest' as const, myLang: 'es', myPeerId: 'g', wsPath: '/ws/live' };
    saveStoredSession({ ...base, engine: 'cartesia', voiceClone: true });
    expect(loadStoredSession()).toMatchObject({ engine: 'cartesia', voiceClone: true });
    sessionStorage.setItem(STORAGE_KEY, JSON.stringify({ ...base, voiceClone: 'yes' }));
    expect(loadStoredSession()).not.toHaveProperty('voiceClone');
    sessionStorage.setItem(STORAGE_KEY, JSON.stringify(base));
    expect(loadStoredSession()).not.toHaveProperty('voiceClone');
  });
});
