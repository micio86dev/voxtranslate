// Webinar chat helper tests (webinar Feature ⑤). Pure helpers: renderChatText fallback,
// sendChatMessage URL/headers/credentials + HTTP-status → ChatError mapping, fetchChatHistory
// parsing, and the display-name localStorage helpers. All fetch is injected as a fake.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import {
  ChatError,
  CHAT_TEXT_MAX,
  fetchChatHistory,
  getStoredDisplayName,
  renderChatText,
  sendChatMessage,
  setStoredDisplayName,
  type ChatEvent,
} from './webinar-chat';

const evBase: ChatEvent = {
  id: 'm1',
  sender_kind: 'guest',
  display_name: 'Ada',
  original: 'hola',
  lang: 'es',
  translations: { es: 'hola', en: 'hi' },
  created_at: '2026-07-12T10:00:00Z',
};

// A fake `fetch` returning a canned Response-like object, capturing the call args.
function fakeFetch(
  status: number,
  body: unknown,
): { fn: typeof fetch; calls: Array<{ url: string; init?: RequestInit }> } {
  const calls: Array<{ url: string; init?: RequestInit }> = [];
  const fn = (async (url: string, init?: RequestInit) => {
    calls.push({ url, init });
    return {
      ok: status >= 200 && status < 300,
      status,
      json: async () => body,
    } as unknown as Response;
  }) as unknown as typeof fetch;
  return { fn, calls };
}

describe('renderChatText', () => {
  it('returns the translation for myLang when present', () => {
    expect(renderChatText(evBase, 'en')).toBe('hi');
    expect(renderChatText(evBase, 'es')).toBe('hola');
  });
  it('falls back to the source original when myLang has no translation', () => {
    expect(renderChatText(evBase, 'fr')).toBe('hola');
    expect(renderChatText({ ...evBase, translations: {} }, 'en')).toBe('hola');
  });
});

describe('sendChatMessage', () => {
  it('POSTs to the chat endpoint with credentials + trimmed body, returning the 200 result', async () => {
    const { fn, calls } = fakeFetch(200, { id: 'srv-1', created_at: '2026-07-12T10:01:00Z' });
    const res = await sendChatMessage(
      'https://api.voxtranslate.app',
      'ab12cd',
      { text: 'hey', display_name: 'Ada', lang: 'en' },
      null,
      fn,
    );
    expect(res).toEqual({ id: 'srv-1', created_at: '2026-07-12T10:01:00Z' });
    expect(calls).toHaveLength(1);
    expect(calls[0].url).toBe('https://api.voxtranslate.app/api/w/ab12cd/chat');
    const init = calls[0].init!;
    expect(init.method).toBe('POST');
    expect(init.credentials).toBe('include'); // sends the guest_id cookie
    const headers = init.headers as Record<string, string>;
    expect(headers['Content-Type']).toBe('application/json');
    expect(headers.Authorization).toBeUndefined(); // no token → guest send
    expect(JSON.parse(init.body as string)).toEqual({
      text: 'hey',
      display_name: 'Ada',
      lang: 'en',
    });
  });

  it('adds the Authorization header when a host token is supplied (→ sender_kind host)', async () => {
    const { fn, calls } = fakeFetch(200, { id: 'x', created_at: 't' });
    await sendChatMessage('https://h', 'code', { text: 'hi' }, 'tok-123', fn);
    const headers = calls[0].init!.headers as Record<string, string>;
    expect(headers.Authorization).toBe('Bearer tok-123');
    // Empty optional fields are omitted from the body.
    expect(JSON.parse(calls[0].init!.body as string)).toEqual({ text: 'hi' });
  });

  it('maps a 429 to a ChatError carrying the status (rate-limited)', async () => {
    const { fn } = fakeFetch(429, { error: 'slow down' });
    await expect(
      sendChatMessage('https://h', 'c', { text: 'x' }, null, fn),
    ).rejects.toMatchObject({ name: 'ChatError', status: 429, message: 'slow down' });
  });

  it('maps a 403 (chat disabled) to a ChatError', async () => {
    const { fn } = fakeFetch(403, {});
    await expect(
      sendChatMessage('https://h', 'c', { text: 'x' }, null, fn),
    ).rejects.toMatchObject({ name: 'ChatError', status: 403 });
  });

  it('maps a 422 (moderated) and 400 (too long) to a ChatError with the status', async () => {
    const moderated = fakeFetch(422, { error: 'blocked' });
    await expect(
      sendChatMessage('https://h', 'c', { text: 'x' }, null, moderated.fn),
    ).rejects.toMatchObject({ status: 422 });
    const tooLong = fakeFetch(400, {});
    await expect(
      sendChatMessage('https://h', 'c', { text: 'a'.repeat(CHAT_TEXT_MAX + 1) }, null, tooLong.fn),
    ).rejects.toMatchObject({ status: 400 });
  });

  it('wraps a network rejection as a ChatError with status 0', async () => {
    const fn = (async () => {
      throw new Error('offline');
    }) as unknown as typeof fetch;
    await expect(
      sendChatMessage('https://h', 'c', { text: 'x' }, null, fn),
    ).rejects.toBeInstanceOf(ChatError);
    await expect(
      sendChatMessage('https://h', 'c', { text: 'x' }, null, fn),
    ).rejects.toMatchObject({ status: 0 });
  });
});

describe('fetchChatHistory', () => {
  it('GETs the history endpoint with the limit and returns normalized messages', async () => {
    const rows = [
      {
        id: 'a',
        sender_kind: 'host',
        display_name: 'Host',
        original: 'welcome',
        lang: 'en',
        translations: { en: 'welcome', es: 'bienvenido' },
        created_at: 't1',
      },
      { garbage: true }, // dropped
      {
        id: 'b',
        sender_kind: 'guest',
        display_name: 'Bo',
        original: 'ciao',
        lang: 'it',
        translations: { it: 'ciao', en: 'hi', bad: 5 }, // non-string dropped
        created_at: 't2',
      },
    ];
    const { fn, calls } = fakeFetch(200, rows);
    const out = await fetchChatHistory('https://api.voxtranslate.app', 'ab12cd', 50, fn);
    expect(calls[0].url).toBe('https://api.voxtranslate.app/api/w/ab12cd/chat?limit=50');
    expect(out).toHaveLength(2);
    expect(out[0].id).toBe('a');
    expect(out[1].translations).toEqual({ it: 'ciao', en: 'hi' }); // bad:5 dropped
  });

  it('returns an empty list on a non-2xx, non-array, or network failure', async () => {
    const err = fakeFetch(500, {});
    expect(await fetchChatHistory('https://h', 'c', 10, err.fn)).toEqual([]);
    const notArray = fakeFetch(200, { nope: true });
    expect(await fetchChatHistory('https://h', 'c', 10, notArray.fn)).toEqual([]);
    const netFail = (async () => {
      throw new Error('down');
    }) as unknown as typeof fetch;
    expect(await fetchChatHistory('https://h', 'c', 10, netFail)).toEqual([]);
  });
});

describe('display-name storage', () => {
  beforeEach(() => {
    vi.stubGlobal('localStorage', undefined); // force the in-memory fallback (deterministic)
  });
  afterEach(() => {
    vi.unstubAllGlobals();
  });
  it('persists a trimmed, clamped name and reads it back', () => {
    setStoredDisplayName('  Ada Lovelace  ');
    expect(getStoredDisplayName()).toBe('Ada Lovelace');
  });
  it('ignores a blank name (keeps any previous value)', () => {
    setStoredDisplayName('Grace');
    setStoredDisplayName('   ');
    expect(getStoredDisplayName()).toBe('Grace');
  });
});
