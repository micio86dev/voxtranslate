// Tests for webinar participant TTS (spec 0108 voice cloning applied to webinars).
// The Enhanced tier must speak the translation with the HOST's cloned voice; the
// configured default voice is only a fallback for hosts without a clone.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { resolveWebinarVoiceId, WebinarTts, type WebinarTtsSession } from './webinar-tts';

const SESSION: WebinarTtsSession = {
  token: 'tok_abc',
  expires_at: Math.floor(Date.now() / 1000) + 3600,
  cartesia_version: '2026-03-01',
  tts: { endpoint: 'wss://api.cartesia.ai/tts/websocket', model: 'sonic-3.5' },
  default_voice_id: 'voice-default',
  host_voice_id: 'voice-host-clone',
};

class FakeWs {
  static OPEN = 1;
  url: string;
  readyState = 1;
  sent: string[] = [];
  onmessage: ((ev: { data: string }) => void) | null = null;
  onclose: (() => void) | null = null;
  private listeners = new Map<string, Set<() => void>>();

  constructor(url: string) {
    this.url = url;
    sockets.push(this);
    setTimeout(() => this.dispatch('open'), 0);
  }
  addEventListener(type: string, fn: () => void): void {
    if (!this.listeners.has(type)) this.listeners.set(type, new Set());
    this.listeners.get(type)!.add(fn);
  }
  removeEventListener(type: string, fn: () => void): void {
    this.listeners.get(type)?.delete(fn);
  }
  dispatch(type: string): void {
    for (const fn of this.listeners.get(type) ?? []) fn();
  }
  send(data: string): void {
    this.sent.push(data);
  }
  close(): void {
    this.readyState = 3;
    this.dispatch('close');
    this.onclose?.();
  }
}

let sockets: FakeWs[] = [];
let sessionPayload: WebinarTtsSession = SESSION;

function newTts(tier: 'standard' | 'enhanced' = 'enhanced'): WebinarTts {
  return new WebinarTts({ code: 'ABC123', tier, lang: 'it', httpBase: 'https://api.test' });
}

beforeEach(() => {
  sockets = [];
  sessionPayload = { ...SESSION };
  vi.stubGlobal('WebSocket', FakeWs);
  vi.stubGlobal(
    'fetch',
    vi.fn(async () => ({ ok: true, json: async () => sessionPayload })),
  );
});

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe('resolveWebinarVoiceId', () => {
  it('prefers the host cloned voice', () => {
    expect(resolveWebinarVoiceId(SESSION)).toBe('voice-host-clone');
  });

  it('falls back to the configured default when the host has no clone', () => {
    expect(resolveWebinarVoiceId({ ...SESSION, host_voice_id: null })).toBe('voice-default');
    expect(resolveWebinarVoiceId({ ...SESSION, host_voice_id: undefined })).toBe('voice-default');
  });

  it('returns null when neither a clone nor a default exists', () => {
    expect(resolveWebinarVoiceId({ ...SESSION, host_voice_id: null, default_voice_id: null })).toBe(
      null,
    );
  });
});

describe('WebinarTts enhanced tier', () => {
  it("speaks with the host's cloned voice", async () => {
    const tts = newTts();
    tts.setEnabled(true);
    tts.speak('ciao a tutti');

    await vi.waitFor(() => expect(sockets[0]?.sent.length).toBe(1));
    const msg = JSON.parse(sockets[0].sent[0]) as Record<string, unknown>;
    expect(msg.voice).toEqual({ mode: 'id', id: 'voice-host-clone' });
    expect(msg.transcript).toBe('ciao a tutti');
    expect(msg.model_id).toBe('sonic-3.5');
  });

  it('falls back to the default voice when the host has no clone', async () => {
    sessionPayload = { ...SESSION, host_voice_id: null };
    const tts = newTts();
    tts.setEnabled(true);
    tts.speak('ciao');

    await vi.waitFor(() => expect(sockets[0]?.sent.length).toBe(1));
    const msg = JSON.parse(sockets[0].sent[0]) as { voice: unknown };
    expect(msg.voice).toEqual({ mode: 'id', id: 'voice-default' });
  });

  it('stays silent (subtitles only) when no voice is available at all', async () => {
    sessionPayload = { ...SESSION, host_voice_id: null, default_voice_id: null };
    const tts = newTts();
    tts.setEnabled(true);
    tts.speak('ciao');

    await vi.waitFor(() => expect(fetch).toHaveBeenCalled());
    await new Promise((r) => setTimeout(r, 5));
    expect(sockets.length).toBe(0);
  });

  it('reuses the same socket across utterances with fresh context ids', async () => {
    const tts = newTts();
    tts.setEnabled(true);
    tts.speak('uno');
    await vi.waitFor(() => expect(sockets[0]?.sent.length).toBe(1));
    tts.speak('due');
    await vi.waitFor(() => expect(sockets[0]?.sent.length).toBe(2));

    expect(sockets.length).toBe(1);
    const ids = sockets[0].sent.map((s) => (JSON.parse(s) as { context_id: string }).context_id);
    expect(new Set(ids).size).toBe(2);
  });

  it('does nothing while disabled', async () => {
    const tts = newTts();
    tts.speak('ignored');
    await new Promise((r) => setTimeout(r, 5));
    expect(fetch).not.toHaveBeenCalled();
    expect(sockets.length).toBe(0);
  });
});

describe('WebinarTts standard tier', () => {
  it('uses browser speech synthesis and never opens a Cartesia socket', async () => {
    const speak = vi.fn();
    const cancel = vi.fn();
    class Utterance {
      lang = '';
      constructor(public text: string) {}
    }
    vi.stubGlobal('window', { speechSynthesis: { speak, cancel } });
    vi.stubGlobal('SpeechSynthesisUtterance', Utterance);

    const tts = newTts('standard');
    tts.setEnabled(true);
    tts.speak('ciao');

    expect(speak).toHaveBeenCalledTimes(1);
    expect((speak.mock.calls[0][0] as Utterance).lang).toBe('it');
    expect(fetch).not.toHaveBeenCalled();
    expect(sockets.length).toBe(0);
  });
});
