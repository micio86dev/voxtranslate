import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

// Mock the Soniox SDK: capture each constructed client + its start() options so the
// tests can drive callbacks and assert lifecycle, without a real WebSocket.
const constructed: Array<{ opts: Record<string, unknown>; start: ReturnType<typeof vi.fn>; cancel: ReturnType<typeof vi.fn>; startOpts?: Record<string, unknown> }> = [];
vi.mock('@soniox/speech-to-text-web', () => {
  class MockSonioxClient {
    static isSupported = true;
    private entry: (typeof constructed)[number];
    constructor(opts: Record<string, unknown>) {
      this.entry = { opts, start: vi.fn(), cancel: vi.fn() };
      this.entry.start.mockImplementation((startOpts: Record<string, unknown>) => {
        this.entry.startOpts = startOpts;
        return Promise.resolve();
      });
      constructed.push(this.entry);
    }
    start(o: Record<string, unknown>) {
      return this.entry.start(o);
    }
    cancel() {
      return this.entry.cancel();
    }
  }
  return { SonioxClient: MockSonioxClient };
});

import { extractTranslation, IDLE_FLUSH_MS, SonioxManager, sttMimeType } from './soniox';
import type { Token } from '@soniox/speech-to-text-web';

function tok(text: string, is_final: boolean, status?: Token['translation_status']): Token {
  return { text, is_final, confidence: 1, translation_status: status };
}

// Minimal MediaStream stand-in for the node test env.
class FakeStream {
  constructor(private tracks: unknown[]) {}
  getAudioTracks() {
    return this.tracks;
  }
}

describe('extractTranslation', () => {
  it('keeps only translation tokens, splitting final vs non-final', () => {
    const tokens = [
      tok('Ciao', true, 'original'), // source language → ignored
      tok('Hello', true, 'translation'),
      tok(' wor', false, 'translation'),
      tok('xx', false, 'none'), // not a translation → ignored
    ];
    expect(extractTranslation(tokens)).toEqual({ finalDelta: 'Hello', nonFinal: ' wor' });
  });
  it('is empty when there are no translation tokens', () => {
    expect(extractTranslation([tok('a', true, 'original'), tok('b', false)])).toEqual({
      finalDelta: '',
      nonFinal: '',
    });
  });
});

describe('sttMimeType', () => {
  afterEach(() => {
    delete (globalThis as { MediaRecorder?: unknown }).MediaRecorder;
  });
  it('returns undefined when MediaRecorder is unavailable (e.g. node env)', () => {
    expect(sttMimeType()).toBeUndefined();
  });
  it('prefers webm/opus when supported', () => {
    (globalThis as { MediaRecorder?: unknown }).MediaRecorder = { isTypeSupported: () => true };
    expect(sttMimeType()).toBe('audio/webm;codecs=opus');
  });
  it('falls back to audio/webm when opus is unsupported', () => {
    (globalThis as { MediaRecorder?: unknown }).MediaRecorder = { isTypeSupported: () => false };
    expect(sttMimeType()).toBe('audio/webm');
  });
});

describe('SonioxManager', () => {
  beforeEach(() => {
    constructed.length = 0;
    (globalThis as { MediaStream?: unknown }).MediaStream = FakeStream;
    vi.useFakeTimers();
  });
  afterEach(() => {
    vi.useRealTimers();
    delete (globalThis as { MediaStream?: unknown }).MediaStream;
  });

  const session = { sttApiKey: 'temp:k', sttEndpoint: 'wss://stt', model: 'stt-rt-v5' };

  function mgr() {
    const onSubtitle = vi.fn();
    const onUtterance = vi.fn();
    const fetchSession = vi.fn().mockResolvedValue(session);
    const m = new SonioxManager({ fetchSession, onSubtitle, onUtterance });
    return { m, onSubtitle, onUtterance, fetchSession };
  }

  it('starts a pipeline only for a cross-language speaker', async () => {
    const { m, fetchSession } = mgr();
    m.activate('en');
    m.setPeerLang('p1', 'it'); // cross-language → translate
    m.setPeerStream('p1', new FakeStream([{}]) as unknown as MediaStream);
    m.setPeerLang('p2', 'en'); // same language as me → no pipeline
    m.setPeerStream('p2', new FakeStream([{}]) as unknown as MediaStream);
    await vi.runAllTimersAsync();

    expect(fetchSession).toHaveBeenCalledTimes(1);
    expect(constructed).toHaveLength(1);
    const c = constructed[0];
    expect(c.opts.webSocketUri).toBe('wss://stt');
    expect(c.startOpts?.translation).toEqual({ type: 'one_way', target_language: 'en' });
    expect(c.startOpts?.languageHints).toEqual(['it', 'en']);
  });

  it('pins the recorder mimeType so Android captures a decodable container', async () => {
    (globalThis as { MediaRecorder?: unknown }).MediaRecorder = { isTypeSupported: () => true };
    try {
      const { m } = mgr();
      m.activate('en');
      m.setPeerLang('p1', 'it');
      m.setPeerStream('p1', new FakeStream([{}]) as unknown as MediaStream);
      await vi.runAllTimersAsync();
      expect(constructed[0].startOpts?.mediaRecorderOptions).toEqual({
        mimeType: 'audio/webm;codecs=opus',
      });
    } finally {
      delete (globalThis as { MediaRecorder?: unknown }).MediaRecorder;
    }
  });

  it('renders interim subtitles, then a final + utterance after the idle flush', async () => {
    const { m, onSubtitle, onUtterance } = mgr();
    m.activate('en');
    m.setPeerLang('p1', 'it');
    m.setPeerStream('p1', new FakeStream([{}]) as unknown as MediaStream);
    await vi.runOnlyPendingTimersAsync();

    const onPartial = constructed[0].startOpts?.onPartialResult as (r: {
      tokens: Token[];
    }) => void;
    onPartial({ tokens: [tok('Hello', true, 'translation'), tok(' there', false, 'translation')] });
    expect(onSubtitle).toHaveBeenLastCalledWith('p1', 'Hello there', true);

    vi.advanceTimersByTime(IDLE_FLUSH_MS);
    expect(onSubtitle).toHaveBeenLastCalledWith('p1', 'Hello', false); // only finalized text
    expect(onUtterance).toHaveBeenCalledWith('p1', 'Hello');
  });

  it('restarts the pipeline when the speaker language changes', async () => {
    const { m } = mgr();
    m.activate('en');
    m.setPeerLang('p1', 'it');
    m.setPeerStream('p1', new FakeStream([{}]) as unknown as MediaStream);
    await vi.runOnlyPendingTimersAsync();
    expect(constructed).toHaveLength(1);

    m.setPeerLang('p1', 'fr'); // language changed → old cancelled, new started
    await vi.runOnlyPendingTimersAsync();
    expect(constructed[0].cancel).toHaveBeenCalled();
    expect(constructed).toHaveLength(2);
    expect(constructed[1].startOpts?.languageHints).toEqual(['fr', 'en']);
  });

  it('cancels pipelines on removePeer and deactivate', async () => {
    const { m } = mgr();
    m.activate('en');
    m.setPeerLang('p1', 'it');
    m.setPeerStream('p1', new FakeStream([{}]) as unknown as MediaStream);
    await vi.runOnlyPendingTimersAsync();
    m.removePeer('p1');
    expect(constructed[0].cancel).toHaveBeenCalledTimes(1);

    // Deactivate cancels any remaining pipelines too.
    m.setPeerLang('p2', 'de');
    m.setPeerStream('p2', new FakeStream([{}]) as unknown as MediaStream);
    await vi.runOnlyPendingTimersAsync();
    m.deactivate();
    expect(constructed[1].cancel).toHaveBeenCalledTimes(1);
  });

  it('does not translate while inactive', async () => {
    const { m, fetchSession } = mgr();
    m.setPeerLang('p1', 'it'); // no activate() yet
    m.setPeerStream('p1', new FakeStream([{}]) as unknown as MediaStream);
    await vi.runAllTimersAsync();
    expect(fetchSession).not.toHaveBeenCalled();
    expect(constructed).toHaveLength(0);
  });
});
