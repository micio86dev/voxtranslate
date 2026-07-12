// @vitest-environment jsdom
// ChatPanel DOM controller tests (webinar Feature ⑤). Exercises the shared panel logic in
// jsdom: message-row rendering in myLang, de-dup by id (so a WS echo of our optimistic send
// doesn't double up), optimistic render on a 200 send, and notice mapping for 429/422.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { ChatPanel, buildChatRow, type ChatEvent } from './webinar-chat';

const strings = {
  send: 'Send',
  hostTag: 'HOST',
  empty: 'No messages yet',
  rateLimited: 'Too fast',
  blocked: 'Blocked',
  genericError: 'Error',
};

function dom() {
  document.body.innerHTML = `
    <div id="list"></div>
    <input id="input" />
    <button id="send"></button>
    <p id="notice" class="hidden"></p>`;
  return {
    list: document.getElementById('list')!,
    input: document.getElementById('input') as HTMLInputElement,
    sendBtn: document.getElementById('send') as HTMLButtonElement,
    notice: document.getElementById('notice')!,
  };
}

const ev = (over: Partial<ChatEvent> = {}): ChatEvent => ({
  id: 'm1',
  sender_kind: 'guest',
  display_name: 'Ada',
  original: 'hola',
  lang: 'es',
  translations: { es: 'hola', en: 'hi' },
  created_at: 't',
  ...over,
});

beforeEach(() => {
  vi.useFakeTimers();
});
afterEach(() => {
  vi.useRealTimers();
});

describe('buildChatRow', () => {
  it('renders the name + the myLang translation, tagging a host message', () => {
    const guest = buildChatRow(document, ev(), 'en', 'HOST');
    expect(guest.querySelector('.wv-chat-body')!.textContent).toBe('hi');
    expect(guest.classList.contains('is-host')).toBe(false);
    expect(guest.querySelector('.wv-chat-host-tag')).toBeNull();

    const host = buildChatRow(document, ev({ sender_kind: 'host', display_name: 'H' }), 'fr', 'HOST');
    expect(host.classList.contains('is-host')).toBe(true);
    expect(host.querySelector('.wv-chat-host-tag')!.textContent).toBe('HOST');
    // No 'fr' translation → falls back to the source original.
    expect(host.querySelector('.wv-chat-body')!.textContent).toBe('hola');
  });
});

describe('ChatPanel', () => {
  it('shows the empty state, then replaces it on the first appended message', () => {
    const d = dom();
    const panel = new ChatPanel({
      ...d,
      httpBase: 'https://h',
      code: 'c',
      myLang: () => 'en',
      senderLang: () => 'en',
      displayName: () => 'Ada',
      token: () => null,
      strings,
    });
    expect(d.list.querySelector('.wv-chat-empty')).not.toBeNull();
    panel.append(ev());
    expect(d.list.querySelector('.wv-chat-empty')).toBeNull();
    expect(d.list.querySelectorAll('.wv-chat-msg')).toHaveLength(1);
  });

  it('de-dupes by id so a WS echo of an already-rendered message is a no-op', () => {
    const d = dom();
    const panel = new ChatPanel({
      ...d,
      httpBase: 'https://h',
      code: 'c',
      myLang: () => 'en',
      senderLang: () => 'en',
      displayName: () => 'Ada',
      token: () => null,
      strings,
    });
    panel.append(ev({ id: 'x' }));
    panel.append(ev({ id: 'x' })); // duplicate id — ignored
    expect(d.list.querySelectorAll('.wv-chat-msg')).toHaveLength(1);
  });

  it('optimistically renders a sent message on a 200 and clears the input', async () => {
    const d = dom();
    d.input.value = 'hey there';
    const fetchImpl = (async () => ({
      ok: true,
      status: 200,
      json: async () => ({ id: 'srv-9', created_at: 't9' }),
    })) as unknown as typeof fetch;
    const panel = new ChatPanel({
      ...d,
      httpBase: 'https://h',
      code: 'c',
      myLang: () => 'en',
      senderLang: () => 'en',
      displayName: () => 'Ada',
      token: () => null,
      strings,
      fetchImpl,
    });
    const ok = await panel.send();
    expect(ok).toBe(true);
    expect(d.input.value).toBe(''); // cleared
    const rows = d.list.querySelectorAll('.wv-chat-msg');
    expect(rows).toHaveLength(1);
    expect(rows[0].querySelector('.wv-chat-body')!.textContent).toBe('hey there');
    // A later WS echo carrying the SAME server id must not duplicate the message.
    panel.append(ev({ id: 'srv-9', original: 'hey there' }));
    expect(d.list.querySelectorAll('.wv-chat-msg')).toHaveLength(1);
  });

  it('shows the rate-limit notice on a 429 and keeps the input intact', async () => {
    const d = dom();
    d.input.value = 'spam';
    const fetchImpl = (async () => ({
      ok: false,
      status: 429,
      json: async () => ({}),
    })) as unknown as typeof fetch;
    const panel = new ChatPanel({
      ...d,
      httpBase: 'https://h',
      code: 'c',
      myLang: () => 'en',
      senderLang: () => 'en',
      displayName: () => 'Ada',
      token: () => null,
      strings,
      fetchImpl,
    });
    const ok = await panel.send();
    expect(ok).toBe(false);
    expect(d.notice.textContent).toBe('Too fast');
    expect(d.notice.classList.contains('hidden')).toBe(false);
    expect(d.input.value).toBe('spam'); // not cleared — the user can retry
    expect(d.list.querySelectorAll('.wv-chat-msg')).toHaveLength(0);
  });

  it('maps a 422 to the blocked notice', async () => {
    const d = dom();
    d.input.value = 'bad words';
    const fetchImpl = (async () => ({
      ok: false,
      status: 422,
      json: async () => ({ error: 'moderated' }),
    })) as unknown as typeof fetch;
    const panel = new ChatPanel({
      ...d,
      httpBase: 'https://h',
      code: 'c',
      myLang: () => 'en',
      senderLang: () => 'en',
      displayName: () => 'Ada',
      token: () => null,
      strings,
      fetchImpl,
    });
    await panel.send();
    expect(d.notice.textContent).toBe('Blocked');
  });
});
