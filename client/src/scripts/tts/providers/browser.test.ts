// @vitest-environment jsdom
// Browser SpeechSynthesis provider — jsdom has no speechSynthesis, so a scriptable
// fake stands in. The delay-first voice scoring (spec 0042), the advance-on-error
// contract (spec 0040) and the iOS silent-utterance unlock are the behaviours pinned.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { TTS_CONFIG } from '../config';
import type { VoiceInfo } from '../types';
import { BrowserSpeechProvider, curateVoices, defaultBrowserVoiceId } from './browser';

class FakeUtterance {
  text: string;
  voice: SpeechSynthesisVoice | null = null;
  lang = '';
  rate = 1;
  volume = 1;
  onend: (() => void) | null = null;
  onerror: (() => void) | null = null;
  constructor(text: string) {
    this.text = text;
  }
}

function voice(over: Partial<SpeechSynthesisVoice>): SpeechSynthesisVoice {
  return {
    voiceURI: 'uri:default',
    name: 'Voice',
    lang: 'en-US',
    localService: true,
    default: false,
    ...over,
  } as SpeechSynthesisVoice;
}

class FakeSynth {
  voices: SpeechSynthesisVoice[] = [];
  spoken: FakeUtterance[] = [];
  listeners: (() => void)[] = [];
  speakError: Error | null = null;
  cancelError: Error | null = null;
  getVoices = vi.fn((): SpeechSynthesisVoice[] => this.voices);
  speak = vi.fn((u: FakeUtterance): void => {
    if (this.speakError) throw this.speakError;
    this.spoken.push(u);
  });
  cancel = vi.fn((): void => {
    if (this.cancelError) throw this.cancelError;
  });
  addEventListener = vi.fn((_type: string, cb: () => void): void => {
    this.listeners.push(cb);
  });
}

let synth: FakeSynth;

beforeEach(() => {
  synth = new FakeSynth();
  vi.stubGlobal('speechSynthesis', synth);
  vi.stubGlobal('SpeechSynthesisUtterance', FakeUtterance);
});

afterEach(() => {
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

describe('availability', () => {
  it('is available (and thus claims any language) when speechSynthesis exists', () => {
    const p = new BrowserSpeechProvider();
    expect(p.id).toBe('browser');
    expect(p.isAvailable()).toBe(true);
    expect(p.supports('en')).toBe(true);
    expect(p.supports('zz-XX')).toBe(true); // fallback of last resort — never blocks
    expect(synth.getVoices).toHaveBeenCalledTimes(1); // warmed at construction
  });

  it('is unavailable without speechSynthesis, and construction stays safe', () => {
    vi.stubGlobal('speechSynthesis', undefined);
    const p = new BrowserSpeechProvider();
    expect(p.isAvailable()).toBe(false);
    expect(p.supports('en')).toBe(false);
  });

  it('survives a throwing speechSynthesis getter at construction', () => {
    Object.defineProperty(window, 'speechSynthesis', {
      configurable: true,
      get() {
        throw new Error('blocked');
      },
    });
    expect(() => new BrowserSpeechProvider()).not.toThrow();
  });
});

describe('listVoices', () => {
  it('returns [] when unavailable', async () => {
    vi.stubGlobal('speechSynthesis', undefined);
    expect(await new BrowserSpeechProvider().listVoices()).toEqual([]);
  });

  it('maps already-populated voices to VoiceInfo', async () => {
    synth.voices = [
      voice({ voiceURI: 'uri:a', name: 'Ada', lang: 'en-GB', localService: true }),
      voice({ voiceURI: 'uri:b', name: 'Bo', lang: 'it-IT', localService: false }),
    ];
    const out = await new BrowserSpeechProvider().listVoices();
    expect(out).toEqual([
      { id: 'uri:a', name: 'Ada', lang: 'en-GB', provider: 'browser', local: true },
      { id: 'uri:b', name: 'Bo', lang: 'it-IT', provider: 'browser', local: false },
    ]);
  });

  it('waits for voiceschanged when the list is initially empty', async () => {
    const p = new BrowserSpeechProvider();
    const pending = p.listVoices();
    expect(synth.listeners).toHaveLength(1);

    synth.voices = [voice({ voiceURI: 'uri:late', name: 'Late' })];
    synth.listeners[0](); // voiceschanged fires
    const out = await pending;
    expect(out.map((v) => v.id)).toEqual(['uri:late']);
  });

  it('resolves via the bounded timeout if voiceschanged never fires, exactly once', async () => {
    vi.useFakeTimers();
    const p = new BrowserSpeechProvider();
    const pending = p.listVoices();
    vi.advanceTimersByTime(500);
    const out = await pending;
    expect(out).toEqual([]);
    // A late event after the timeout must not double-resolve / crash.
    expect(() => synth.listeners[0]()).not.toThrow();
  });

  it('copes with a speechSynthesis lacking addEventListener (old WebKit)', async () => {
    vi.useFakeTimers();
    const bare = new FakeSynth() as FakeSynth & { addEventListener?: unknown };
    delete bare.addEventListener;
    vi.stubGlobal('speechSynthesis', bare);
    const pending = new BrowserSpeechProvider().listVoices();
    vi.advanceTimersByTime(500);
    expect(await pending).toEqual([]);
  });
});

describe('voice curation (Chrome → Google voices only, per language)', () => {
  const googleIt = voice({ voiceURI: 'g:it', name: 'Google italiano', lang: 'it-IT', localService: false });
  const localIt = voice({ voiceURI: 'l:it', name: 'Alice', lang: 'it-IT', localService: true });
  const googleEn = voice({ voiceURI: 'g:en', name: 'Google US English', lang: 'en-US', localService: false });
  const localEn = voice({ voiceURI: 'l:en', name: 'Samantha', lang: 'en-US', localService: true });
  const localDe = voice({ voiceURI: 'l:de', name: 'Anna', lang: 'de-DE', localService: true });

  it('drops non-Google voices for any language a Google voice covers', () => {
    const out = curateVoices([googleIt, localIt, googleEn, localEn]);
    expect(out.map((v) => v.voiceURI).sort()).toEqual(['g:en', 'g:it']);
  });

  it('keeps voices for languages that have no Google voice (nothing becomes unpickable)', () => {
    // German has only a local voice → it survives; Italian collapses to Google.
    const out = curateVoices([googleIt, localIt, localDe]);
    expect(out.map((v) => v.voiceURI).sort()).toEqual(['g:it', 'l:de']);
  });

  it('returns the list unchanged when there are no Google voices (Safari/Firefox)', () => {
    const list = [localIt, localEn, localDe];
    expect(curateVoices(list)).toEqual(list);
  });

  it('listVoices applies the curation', async () => {
    synth.voices = [googleIt, localIt];
    const out = await new BrowserSpeechProvider().listVoices();
    expect(out.map((v) => v.id)).toEqual(['g:it']);
  });

  it('auto-pick prefers a Google voice over a plain local one (quality over latency)', async () => {
    synth.voices = [localIt, googleIt];
    const p = new BrowserSpeechProvider();
    const done = p.speak('ciao', 'it');
    const u = synth.spoken.at(-1) as FakeUtterance;
    expect(u.voice?.voiceURI).toBe('g:it');
    u.onend?.();
    await done;
  });
});

describe('defaultBrowserVoiceId (Standard-tier pre-selection)', () => {
  const vi_ = (over: Partial<VoiceInfo>): VoiceInfo => ({
    id: 'id',
    name: 'Voice',
    lang: 'en-US',
    provider: 'browser',
    local: true,
    ...over,
  });
  const voices: VoiceInfo[] = [
    vi_({ id: 'l-it', name: 'Alice', lang: 'it-IT', local: true }),
    vi_({ id: 'g-it', name: 'Google italiano', lang: 'it-IT', local: false }),
    vi_({ id: 'l-en', name: 'Samantha', lang: 'en-US', local: true }),
    vi_({ id: 'g-en', name: 'Google US English', lang: 'en-US', local: false }),
  ];

  it('prefers the Google voice for the requested language', () => {
    expect(defaultBrowserVoiceId(voices, 'it')).toBe('g-it');
    expect(defaultBrowserVoiceId(voices, 'it-IT')).toBe('g-it');
    expect(defaultBrowserVoiceId(voices, 'en')).toBe('g-en');
  });

  it('falls back to a local voice when no Google voice serves the language', () => {
    const noGoogle = [vi_({ id: 'l-de', name: 'Anna', lang: 'de-DE', local: true })];
    expect(defaultBrowserVoiceId(noGoogle, 'de')).toBe('l-de');
  });

  it('returns null for a language with no matching voice (UI shows its empty state)', () => {
    expect(defaultBrowserVoiceId(voices, 'ja')).toBeNull();
    expect(defaultBrowserVoiceId([], 'en')).toBeNull();
  });

  it('ranks over all languages for auto/empty, still preferring Google', () => {
    expect(defaultBrowserVoiceId(voices, 'auto')).toMatch(/^g-/);
    expect(defaultBrowserVoiceId(voices, '')).toMatch(/^g-/);
  });
});

describe('speak', () => {
  it('speaks with the configured default rate and resolves on end', async () => {
    const p = new BrowserSpeechProvider();
    const done = p.speak('hello there', 'en-US');
    const u = synth.spoken[0];
    expect(u.text).toBe('hello there');
    expect(u.lang).toBe('en-US');
    expect(u.rate).toBe(TTS_CONFIG.RATE);
    u.onend?.();
    await done;
  });

  it('resolves on error too (advance-on-error, the queue never stalls)', async () => {
    const p = new BrowserSpeechProvider();
    const done = p.speak('x', 'en');
    synth.spoken[0].onerror?.();
    await done;
  });

  it('applies a caller-provided rate', async () => {
    const p = new BrowserSpeechProvider();
    const done = p.speak('x', 'en', { rate: 0.8 });
    expect(synth.spoken[0].rate).toBe(0.8);
    synth.spoken[0].onend?.();
    await done;
  });

  it('resolves immediately when unavailable (nothing spoken)', async () => {
    vi.stubGlobal('speechSynthesis', undefined);
    await new BrowserSpeechProvider().speak('x', 'en');
  });
});

describe('voice picking (spec 0042: minimal delay first)', () => {
  const local = voice({ voiceURI: 'uri:local', name: 'Plain Local', localService: true });
  const localPremium = voice({
    voiceURI: 'uri:premium',
    name: 'Enhanced Local',
    localService: true,
  });
  const networkPremium = voice({
    voiceURI: 'uri:net',
    name: 'Neural Cloud',
    localService: false,
  });
  const localDefault = voice({
    voiceURI: 'uri:default-local',
    name: 'Default Local',
    localService: true,
    default: true,
  });
  const italian = voice({ voiceURI: 'uri:it', name: 'Alice', lang: 'it-IT' });

  async function speakAndGetVoice(
    voices: SpeechSynthesisVoice[],
    lang: string,
    voiceId?: string,
  ): Promise<SpeechSynthesisVoice | null> {
    synth.voices = voices;
    const p = new BrowserSpeechProvider();
    const done = p.speak('x', lang, { voiceId });
    const u = synth.spoken.at(-1) as FakeUtterance;
    u.onend?.();
    await done;
    return u.voice;
  }

  it('an explicit user pick wins when it can serve the language', async () => {
    const v = await speakAndGetVoice([local, localPremium], 'en', 'uri:local');
    expect(v?.voiceURI).toBe('uri:local');
  });

  it('a stale/foreign pick degrades gracefully to auto-scoring', async () => {
    const v = await speakAndGetVoice([italian, local], 'en', 'uri:it');
    expect(v?.voiceURI).toBe('uri:local'); // the Italian pick can't serve English
  });

  it('local beats network even when the network voice is premium', async () => {
    const v = await speakAndGetVoice([networkPremium, local], 'en');
    expect(v?.voiceURI).toBe('uri:local');
  });

  it('among local voices, premium/enhanced wins at no latency cost', async () => {
    const v = await speakAndGetVoice([local, localPremium], 'en');
    expect(v?.voiceURI).toBe('uri:premium');
  });

  it('the browser default breaks ties between equal candidates', async () => {
    const v = await speakAndGetVoice([local, localDefault], 'en');
    expect(v?.voiceURI).toBe('uri:default-local');
  });

  it('no matching voice → utterance keeps the engine default (voice unset)', async () => {
    const v = await speakAndGetVoice([italian], 'en');
    expect(v).toBeNull();
  });
});

describe('unlock / stop (iOS priming)', () => {
  it('primes once with a silent utterance, then becomes a no-op', () => {
    const p = new BrowserSpeechProvider();
    p.unlock();
    expect(synth.spoken).toHaveLength(1);
    expect(synth.spoken[0].volume).toBe(0);
    expect(synth.spoken[0].text).toBe(' ');
    p.unlock(); // already unlocked
    expect(synth.spoken).toHaveLength(1);
  });

  it('a real speak() also counts as the unlock', async () => {
    const p = new BrowserSpeechProvider();
    const done = p.speak('x', 'en');
    synth.spoken[0].onend?.();
    await done;
    p.unlock();
    expect(synth.spoken).toHaveLength(1); // no extra silent utterance
  });

  it('does nothing when unavailable', () => {
    vi.stubGlobal('speechSynthesis', undefined);
    expect(() => new BrowserSpeechProvider().unlock()).not.toThrow();
  });

  it('a failed unlock stays best-effort and can be retried', () => {
    const p = new BrowserSpeechProvider();
    synth.speakError = new Error('not allowed');
    expect(() => p.unlock()).not.toThrow();
    synth.speakError = null;
    p.unlock(); // the failure did not latch `unlocked`
    expect(synth.spoken).toHaveLength(1);
  });

  it('stop cancels playback and re-arms the unlock', () => {
    const p = new BrowserSpeechProvider();
    p.unlock();
    p.stop();
    expect(synth.cancel).toHaveBeenCalledTimes(1);
    p.unlock(); // stop reset the latch → primes again
    expect(synth.spoken).toHaveLength(2);
  });

  it('stop swallows cancel errors and copes with no speechSynthesis', () => {
    const p = new BrowserSpeechProvider();
    synth.cancelError = new Error('boom');
    expect(() => p.stop()).not.toThrow();
    vi.stubGlobal('speechSynthesis', undefined);
    expect(() => p.stop()).not.toThrow();
  });
});
