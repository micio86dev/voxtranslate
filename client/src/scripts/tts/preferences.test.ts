// @vitest-environment jsdom
// preferences.ts → api.ts → auth.ts reads `location` at module load, so this file needs
// a DOM. The pure resolvers are exercised directly; the I/O wrappers run against a real
// jsdom localStorage with the auth/api boundary mocked.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { TTS_CONFIG } from './config';
import {
  loadBrowserVoice,
  loadEnginePref,
  loadVoxVoice,
  normalizePref,
  resolveEnginePref,
  resolveVoice,
  saveBrowserVoice,
  saveEnginePref,
  saveVoxVoice,
} from './preferences';
import * as auth from '../auth';
import type { User } from '../auth';
import * as api from '../api';

/** Build a partial user record for the mock (only the TTS prefs matter here). */
const asUser = (u: Partial<User>): User => u as User;

vi.mock('../auth', () => ({
  getUser: vi.fn<() => unknown>(() => null),
  isLoggedIn: vi.fn<() => boolean>(() => false),
  setTtsPrefs: vi.fn(),
}));
vi.mock('../api', () => ({ saveTtsPrefs: vi.fn(async () => {}) }));

describe('normalizePref', () => {
  it('accepts valid prefs and defaults everything else to auto', () => {
    expect(normalizePref('auto')).toBe('auto');
    expect(normalizePref('browser')).toBe('browser');
    expect(normalizePref('vox')).toBe('vox');
    expect(normalizePref('nonsense')).toBe('auto');
    expect(normalizePref(null)).toBe('auto');
    expect(normalizePref(undefined)).toBe('auto');
  });
});

describe('resolveEnginePref (server-first, then local, then default)', () => {
  it('prefers the server value when present', () => {
    expect(resolveEnginePref('vox', 'browser')).toBe('vox');
  });
  it('falls back to local when there is no server value', () => {
    expect(resolveEnginePref(null, 'browser')).toBe('browser');
    expect(resolveEnginePref(undefined, 'vox')).toBe('vox');
  });
  it('defaults to auto when neither is set (or invalid)', () => {
    expect(resolveEnginePref(null, null)).toBe('auto');
    expect(resolveEnginePref('junk', null)).toBe('auto');
  });
});

describe('resolveVoice (portable ids, server-first)', () => {
  it('prefers server, then local, then null', () => {
    expect(resolveVoice('af_heart', 'am_michael')).toBe('af_heart');
    expect(resolveVoice(null, 'am_michael')).toBe('am_michael');
    expect(resolveVoice(undefined, undefined)).toBeNull();
  });
});

describe('preferences I/O wrappers', () => {
  const getUser = vi.mocked(auth.getUser);
  const isLoggedIn = vi.mocked(auth.isLoggedIn);
  const setTtsPrefs = vi.mocked(auth.setTtsPrefs);
  const saveTtsPrefs = vi.mocked(api.saveTtsPrefs);

  beforeEach(() => {
    localStorage.clear();
    vi.clearAllMocks();
    getUser.mockReturnValue(null);
    isLoggedIn.mockReturnValue(false);
  });

  afterEach(() => {
    localStorage.clear();
  });

  describe('load (server truth wins over local cache)', () => {
    it('loadEnginePref: server value beats the local cache', () => {
      getUser.mockReturnValue(asUser({ tts_engine_pref: 'vox' }));
      localStorage.setItem(TTS_CONFIG.ENGINE_PREF_KEY, 'browser');
      expect(loadEnginePref()).toBe('vox');
    });

    it('loadEnginePref: falls back to the local cache for a guest', () => {
      localStorage.setItem(TTS_CONFIG.ENGINE_PREF_KEY, 'browser');
      expect(loadEnginePref()).toBe('browser');
    });

    it('loadEnginePref: defaults to auto with neither set', () => {
      expect(loadEnginePref()).toBe('auto');
    });

    it('loadVoxVoice: server id wins, else local, else null', () => {
      getUser.mockReturnValue(asUser({ tts_voice_id: 'af_heart' }));
      localStorage.setItem(TTS_CONFIG.VOX_VOICE_KEY, 'am_michael');
      expect(loadVoxVoice()).toBe('af_heart');

      getUser.mockReturnValue(null);
      expect(loadVoxVoice()).toBe('am_michael');

      localStorage.removeItem(TTS_CONFIG.VOX_VOICE_KEY);
      expect(loadVoxVoice()).toBeNull();
    });

    it('loadBrowserVoice: device-local only (never consults the user record)', () => {
      getUser.mockReturnValue(asUser({ tts_voice_id: 'server_voice' }));
      expect(loadBrowserVoice()).toBeNull();
      localStorage.setItem(TTS_CONFIG.BROWSER_VOICE_KEY, 'Daniel');
      expect(loadBrowserVoice()).toBe('Daniel');
      expect(getUser).not.toHaveBeenCalled(); // browser voice is purely local
    });
  });

  describe('save (local cache always; server sync only when logged in)', () => {
    it('saveEnginePref: caches locally and syncs server-side when logged in', () => {
      isLoggedIn.mockReturnValue(true);
      saveEnginePref('vox');
      expect(localStorage.getItem(TTS_CONFIG.ENGINE_PREF_KEY)).toBe('vox');
      expect(setTtsPrefs).toHaveBeenCalledWith({ tts_engine_pref: 'vox' });
      expect(saveTtsPrefs).toHaveBeenCalledWith({ tts_engine_pref: 'vox' });
    });

    it('saveEnginePref: guest caches locally but never hits the server', () => {
      saveEnginePref('browser');
      expect(localStorage.getItem(TTS_CONFIG.ENGINE_PREF_KEY)).toBe('browser');
      expect(setTtsPrefs).not.toHaveBeenCalled();
      expect(saveTtsPrefs).not.toHaveBeenCalled();
    });

    it('saveVoxVoice: caches locally and syncs server-side when logged in', () => {
      isLoggedIn.mockReturnValue(true);
      saveVoxVoice('af_heart');
      expect(localStorage.getItem(TTS_CONFIG.VOX_VOICE_KEY)).toBe('af_heart');
      expect(setTtsPrefs).toHaveBeenCalledWith({ tts_voice_id: 'af_heart' });
      expect(saveTtsPrefs).toHaveBeenCalledWith({ tts_voice_id: 'af_heart' });
    });

    it('saveVoxVoice: guest stays local-only', () => {
      saveVoxVoice('af_heart');
      expect(localStorage.getItem(TTS_CONFIG.VOX_VOICE_KEY)).toBe('af_heart');
      expect(saveTtsPrefs).not.toHaveBeenCalled();
    });

    it('saveBrowserVoice: writes locally and never syncs (voiceURIs are not portable)', () => {
      isLoggedIn.mockReturnValue(true); // even logged in, browser voice stays local
      saveBrowserVoice('Daniel');
      expect(localStorage.getItem(TTS_CONFIG.BROWSER_VOICE_KEY)).toBe('Daniel');
      expect(setTtsPrefs).not.toHaveBeenCalled();
      expect(saveTtsPrefs).not.toHaveBeenCalled();
    });
  });

  describe('localStorage access is blocked (private mode / SSR)', () => {
    it('reads default to null and writes are swallowed', () => {
      const proto = Object.getPrototypeOf(localStorage);
      const getSpy = vi.spyOn(proto, 'getItem').mockImplementation(() => {
        throw new Error('access denied');
      });
      const setSpy = vi.spyOn(proto, 'setItem').mockImplementation(() => {
        throw new Error('access denied');
      });

      expect(loadEnginePref()).toBe('auto'); // read failed → default
      expect(loadBrowserVoice()).toBeNull();
      expect(() => saveBrowserVoice('X')).not.toThrow(); // write failed → swallowed

      getSpy.mockRestore();
      setSpy.mockRestore();
    });
  });
});
