// @vitest-environment jsdom
// preferences.ts → api.ts → auth.ts reads `location` at module load, so this file needs
// a DOM. The tests themselves only exercise the pure resolvers.
import { describe, expect, it } from 'vitest';

import { normalizePref, resolveEnginePref, resolveVoice } from './preferences';

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
