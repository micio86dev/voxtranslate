import { beforeEach, describe, expect, it, vi } from 'vitest';

import { TTS_CONFIG } from './config';
import { HealthMonitor } from './health';
import { TTSManager } from './manager';
import type { SpeakOptions, TTSProvider, VoiceInfo } from './types';

/** A fully synchronous fake provider whose speak() resolves immediately. */
class FakeProvider implements TTSProvider {
  readonly id: string;
  available = true;
  /** null → supports every language; a Set restricts it. */
  supportedLangs: Set<string> | null = null;
  fail = false;
  voices: VoiceInfo[] = [];
  spoken: { text: string; lang: string; voiceId?: string }[] = [];
  unlockCount = 0;
  stopCount = 0;

  constructor(id: string) {
    this.id = id;
  }
  isAvailable(): boolean {
    return this.available;
  }
  supports(lang: string): boolean {
    return this.supportedLangs ? this.supportedLangs.has(lang) : true;
  }
  async listVoices(): Promise<VoiceInfo[]> {
    return this.voices;
  }
  async speak(text: string, lang: string, opts?: SpeakOptions): Promise<void> {
    if (this.fail) throw new Error('synth failed');
    this.spoken.push({ text, lang, voiceId: opts?.voiceId });
  }
  unlock(): void {
    this.unlockCount++;
  }
  stop(): void {
    this.stopCount++;
  }
}

/** A provider whose speak() blocks until released — for queue/backpressure tests. */
class GatedProvider implements TTSProvider {
  readonly id = 'browser';
  spoken: string[] = [];
  private resolvers: (() => void)[] = [];
  isAvailable(): boolean {
    return true;
  }
  supports(): boolean {
    return true;
  }
  async listVoices(): Promise<VoiceInfo[]> {
    return [];
  }
  speak(text: string): Promise<void> {
    this.spoken.push(text);
    return new Promise<void>((res) => this.resolvers.push(res));
  }
  release(): void {
    const r = this.resolvers.shift();
    r?.();
  }
  unlock(): void {}
  stop(): void {
    this.resolvers.length = 0;
  }
}

const flush = async (): Promise<void> => {
  for (let i = 0; i < 6; i++) await new Promise((r) => setTimeout(r, 0));
};

describe('TTSManager.chooseProvider', () => {
  let browser: FakeProvider;
  let vox: FakeProvider;
  let mgr: TTSManager;

  beforeEach(() => {
    browser = new FakeProvider('browser');
    vox = new FakeProvider('vox');
    mgr = new TTSManager(browser);
  });

  it('routes system phrases to Browser even when Vox is registered and preferred', () => {
    mgr.setVoxProvider(vox);
    mgr.setPreference('vox');
    expect(mgr.chooseProvider('en', true)).toBe(browser);
  });

  it('uses Browser when no Vox provider is registered', () => {
    expect(mgr.chooseProvider('en', false)).toBe(browser);
  });

  it('uses Vox on auto when available, supported AND the benchmark passed', () => {
    mgr.setVoxProvider(vox);
    mgr.setBenchmarkPassed(true);
    expect(mgr.chooseProvider('en', false)).toBe(vox);
  });

  it('on auto, stays on Browser until the benchmark passes', () => {
    mgr.setVoxProvider(vox); // benchmark not passed (default)
    expect(mgr.chooseProvider('en', false)).toBe(browser);
  });

  it('uses Vox when explicitly pinned, even if the benchmark did not pass', () => {
    mgr.setVoxProvider(vox);
    mgr.setPreference('vox'); // benchmark still false
    expect(mgr.chooseProvider('en', false)).toBe(vox);
  });

  it('falls back to Browser when the preference pins Browser', () => {
    mgr.setVoxProvider(vox);
    mgr.setPreference('browser');
    expect(mgr.chooseProvider('en', false)).toBe(browser);
  });

  it('falls back to Browser when Vox is unavailable', () => {
    vox.available = false;
    mgr.setVoxProvider(vox);
    expect(mgr.chooseProvider('en', false)).toBe(browser);
  });

  it('falls back to Browser for a language Vox cannot serve (capability query)', () => {
    vox.supportedLangs = new Set(['en']);
    mgr.setVoxProvider(vox);
    mgr.setBenchmarkPassed(true);
    expect(mgr.chooseProvider('en', false)).toBe(vox);
    expect(mgr.chooseProvider('it', false)).toBe(browser);
  });

  it('falls back to Browser while degraded, and recovers when cleared', () => {
    mgr.setVoxProvider(vox);
    mgr.setBenchmarkPassed(true);
    mgr.setDegraded(true);
    expect(mgr.isDegraded()).toBe(true);
    expect(mgr.chooseProvider('en', false)).toBe(browser);
    mgr.setDegraded(false);
    expect(mgr.chooseProvider('en', false)).toBe(vox);
  });
});

describe('TTSManager playback', () => {
  it('applies the selected per-provider voice id', async () => {
    const browser = new FakeProvider('browser');
    const vox = new FakeProvider('vox');
    const mgr = new TTSManager(browser);
    mgr.setVoxProvider(vox);
    mgr.setBenchmarkPassed(true);
    mgr.setVoxVoice('af_heart');
    mgr.setBrowserVoice('Daniel');

    mgr.speak('hi', 'en'); // → vox (auto + benchmark passed)
    mgr.speakSystem('timer up', 'en'); // → browser (system)
    await flush();

    expect(vox.spoken).toEqual([{ text: 'hi', lang: 'en', voiceId: 'af_heart' }]);
    expect(browser.spoken).toEqual([{ text: 'timer up', lang: 'en', voiceId: 'Daniel' }]);
  });

  it('silently falls back to Browser when Vox throws, notifying once per session', async () => {
    const browser = new FakeProvider('browser');
    const vox = new FakeProvider('vox');
    vox.fail = true;
    const notice = vi.fn();
    const mgr = new TTSManager(browser);
    mgr.setVoxProvider(vox);
    mgr.setBenchmarkPassed(true); // so auto routes to Vox (which then throws)
    mgr.onFallback(notice);

    mgr.speak('one', 'en');
    mgr.speak('two', 'en');
    await flush();

    // Both lines still played — on Browser — and the queue never stalled.
    expect(browser.spoken.map((s) => s.text)).toEqual(['one', 'two']);
    expect(notice).toHaveBeenCalledTimes(1); // once per session, not per failure
  });

  it('caps the backlog, dropping the OLDEST waiting lines (no-cut, near real-time)', async () => {
    const browser = new GatedProvider();
    const mgr = new TTSManager(browser);

    const total = TTS_CONFIG.QUEUE_CAP + 4;
    for (let i = 0; i < total; i++) mgr.speak(`line${i}`, 'en');
    await flush();

    // line0 is in-flight; the remaining queue is capped at QUEUE_CAP, so the
    // oldest waiters (line1..) were dropped. Drain everything.
    for (let i = 0; i < total + 2; i++) {
      browser.release();
      await flush();
    }

    // Exactly the in-flight line plus the last QUEUE_CAP survivors played.
    expect(browser.spoken.length).toBe(TTS_CONFIG.QUEUE_CAP + 1);
    expect(browser.spoken[0]).toBe('line0');
    expect(browser.spoken).toContain(`line${total - 1}`);
    expect(browser.spoken).not.toContain('line1'); // an oldest waiter, dropped
  });
});

describe('TTSManager health integration', () => {
  it('degrades to Browser after sustained slow Vox utterances, notifying once', () => {
    const browser = new FakeProvider('browser');
    const vox = new FakeProvider('vox');
    const notice = vi.fn();
    const mgr = new TTSManager(browser, new HealthMonitor(100, 2));
    mgr.setVoxProvider(vox);
    mgr.setBenchmarkPassed(true);
    mgr.onFallback(notice);

    expect(mgr.chooseProvider('en', false)).toBe(vox); // healthy → Vox
    mgr.recordVoxTiming(50); // fast, no strike
    expect(mgr.chooseProvider('en', false)).toBe(vox);
    mgr.recordVoxTiming(200); // slow strike 1
    mgr.recordVoxTiming(200); // slow strike 2 → degrade
    expect(mgr.isDegraded()).toBe(true);
    expect(mgr.chooseProvider('en', false)).toBe(browser);
    expect(notice).toHaveBeenCalledTimes(1);

    mgr.recordVoxTiming(200); // already degraded → no repeat notice
    expect(notice).toHaveBeenCalledTimes(1);
  });

  it('setDegraded(false) resets health so Vox is retried', () => {
    const vox = new FakeProvider('vox');
    const mgr = new TTSManager(new FakeProvider('browser'));
    mgr.setVoxProvider(vox);
    mgr.setBenchmarkPassed(true);
    mgr.setDegraded(true);
    expect(mgr.chooseProvider('en', false).id).toBe('browser');
    mgr.setDegraded(false);
    expect(mgr.chooseProvider('en', false).id).toBe('vox');
  });
});

describe('TTSManager lifecycle', () => {
  it('unlock/stop reach both providers; stop clears the queue', () => {
    const browser = new FakeProvider('browser');
    const vox = new FakeProvider('vox');
    const mgr = new TTSManager(browser);
    mgr.setVoxProvider(vox);

    mgr.unlock();
    mgr.stop();
    expect(browser.unlockCount).toBe(1);
    expect(vox.unlockCount).toBe(1);
    expect(browser.stopCount).toBe(1);
    expect(vox.stopCount).toBe(1);
  });

  it('listVoices aggregates both providers and tolerates a missing Vox', async () => {
    const browser = new FakeProvider('browser');
    browser.voices = [{ id: 'b1', name: 'B', lang: 'en', provider: 'browser' }];
    const mgr = new TTSManager(browser);

    let out = await mgr.listVoices();
    expect(out.browser).toHaveLength(1);
    expect(out.vox).toHaveLength(0);

    const vox = new FakeProvider('vox');
    vox.voices = [{ id: 'af_heart', name: 'Heart', lang: 'en', provider: 'vox' }];
    mgr.setVoxProvider(vox);
    out = await mgr.listVoices();
    expect(out.vox).toHaveLength(1);
  });
});
